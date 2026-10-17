//! Agent record parity: the Rust manager and `paseo-store` write the same record files as the TS
//! daemon. `fixtures/p4/agent-records/records.json` is TS output
//! (`capture-agent-records.mts`); every scenario here replays the TS script step by step, with the
//! same manual clock, and compares the parsed files. Session ids are random UUIDs in the mock and
//! `session-N` in TS, so they are normalized to a placeholder and checked for consistency.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use indexmap::IndexMap;
use paseo_agent_types::{AgentStreamEvent, PermissionKind, PermissionRequest};
use paseo_core::mock::{boxed, MockClient, MockSession, ScriptedBehavior};
use paseo_core::time::ManualClock;
use paseo_core::{CreateAgentOptions, RunOpts};
use paseo_provider_api::{AgentSessionConfig, PromptInput, TurnHandle};
use serde_json::{json, Value};

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00.000Z
const CWD: &str = "/tmp/paseo-record-parity";
const PARENT_LABEL: &str = "paseo.parent-agent-id";

fn id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

#[derive(Clone, Copy)]
enum Mode {
    Complete,
    Fail,
    Permission,
    CompleteThenHang,
}

fn script_client(mode: Mode) -> MockClient {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        boxed(async move {
            let behavior = ScriptedBehavior::new().on_start(move |session, prompt, options| {
                boxed(async move {
                    let n = session.record_start(prompt, options);
                    let turn = format!("turn-{n}");
                    let (soon, turn_id) = (session.clone(), turn.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::ZERO).await;
                        soon.push_event(turn_started(&turn_id));
                        match mode {
                            Mode::Complete => {
                                soon.push_event(turn_completed(&turn_id));
                                soon.with_state(|s| s.runtime_model = Some("gpt-5.2-codex".into()));
                            }
                            Mode::CompleteThenHang if n == 1 => {
                                soon.push_event(turn_completed(&turn_id));
                                soon.with_state(|s| s.runtime_model = Some("gpt-5.2-codex".into()));
                            }
                            Mode::Fail => {
                                soon.push_event(turn_failed(Some(&turn_id), "scripted failure"))
                            }
                            Mode::Permission => {
                                soon.push_event(AgentStreamEvent::PermissionRequested {
                                    provider: "codex".into(),
                                    request: Box::new(PermissionRequest {
                                        id: "perm-1".into(),
                                        provider: "codex".into(),
                                        name: "Read file".into(),
                                        kind: PermissionKind::Tool,
                                        title: None,
                                        description: None,
                                        input: None,
                                        detail: None,
                                        suggestions: None,
                                        actions: None,
                                        metadata: None,
                                    }),
                                    turn_id: Some(turn_id.clone()),
                                })
                            }
                            Mode::CompleteThenHang => {}
                        }
                    });
                    Ok(TurnHandle { turn_id: turn })
                })
            });
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(behavior)
                .build())
        })
    });
    client
}

struct Run {
    lc: Lc,
    clock: Arc<ManualClock>,
}

impl Run {
    async fn new(mode: Mode) -> Self {
        let clock = Arc::new(ManualClock::new(T0));
        let tweak_clock = clock.clone();
        let lc = Lc::with(
            script_client(mode),
            LcOptions {
                tweak: Box::new(move |o| o.clock = tweak_clock),
                ..LcOptions::default()
            },
        )
        .await;
        Self { lc, clock }
    }

    fn step(&self, n: i64) {
        self.clock.set(T0 + n * 1000);
    }

    fn config(&self, title: &str) -> AgentSessionConfig {
        let mut config = AgentSessionConfig::new("codex", CWD);
        config.title = Some(title.into());
        config
    }

    async fn create(&self, n: u32, config: AgentSessionConfig, options: CreateAgentOptions) {
        // The manager rejects a missing cwd; fresh CI runners have no such directory.
        std::fs::create_dir_all(CWD).expect("create cwd");
        self.lc
            .manager
            .create_agent(config, Some(&id(n)), options)
            .await
            .expect("create_agent");
    }

    async fn run_turn(&self, n: u32) {
        let _ = self
            .lc
            .manager
            .run_agent(&id(n), PromptInput::from("hello"), RunOpts::default())
            .await;
        settle().await;
    }

    /// Flush every writer and read the record files back, like the TS `readRecords`.
    async fn records(&self) -> BTreeMap<String, (String, Value)> {
        self.lc.flush().await;
        settle().await;
        let mut out = BTreeMap::new();
        read_dir(&self.lc.dir.path().join("agents"), Path::new(""), &mut out);
        out
    }
}

fn read_dir(dir: &Path, rel: &Path, out: &mut BTreeMap<String, (String, Value)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = rel.join(entry.file_name());
        if path.is_dir() {
            read_dir(&path, &rel, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let agent_id = record["id"].as_str().unwrap().to_owned();
            out.insert(agent_id, (rel.to_string_lossy().into_owned(), record));
        }
    }
}

/// Mock sessions have random uuids: replace them with a placeholder, after checking that every
/// place that names the session agrees.
fn normalize(record: &mut Value) {
    let persisted = record["persistence"]["sessionId"].clone();
    let runtime = record["runtimeInfo"]["sessionId"].clone();
    assert_eq!(
        persisted, runtime,
        "persistence and runtimeInfo agree on the session"
    );
    for path in ["/persistence/sessionId", "/runtimeInfo/sessionId"] {
        if let Some(slot) = record.pointer_mut(path) {
            *slot = json!("<session>");
        }
    }
}

fn golden() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p4/agent-records/records.json");
    serde_json::from_slice(&std::fs::read(path).expect("records.json")).expect("json")
}

async fn assert_scenario(name: &str, run: &Run) {
    let golden = golden();
    let expected = golden["scenarios"][name]
        .as_object()
        .unwrap_or_else(|| panic!("no golden scenario {name}"));
    let actual = run.records().await;
    assert_eq!(
        actual.keys().cloned().collect::<Vec<_>>(),
        expected.keys().cloned().collect::<Vec<_>>(),
        "{name}: same agent files"
    );
    for (agent, (file, mut record)) in actual {
        let want = &expected[&agent];
        assert_eq!(
            want["file"].as_str(),
            Some(file.as_str()),
            "{name}: file path of {agent}"
        );
        let mut want_record = want["record"].clone();
        if want_record.get("persistence").is_some_and(|p| !p.is_null()) {
            normalize(&mut record);
            // TS persistence ids are `session-N`; compare the shape only.
            want_record["persistence"]["sessionId"] = json!("<session>");
            want_record["runtimeInfo"]["sessionId"] = json!("<session>");
        }
        assert_eq!(record, want_record, "{name}: record of {agent}");
    }
}

fn labels(pairs: &[(&str, String)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[tokio::test]
async fn create() {
    let run = Run::new(Mode::Complete).await;
    run.step(1);
    let mut config = run.config("Parity");
    config.mode_id = Some("plan".into());
    config.model = Some("gpt-5.4".into());
    run.create(
        1,
        config,
        CreateAgentOptions {
            labels: Some(labels(&[("team", "core".into())])),
            workspace_id: Some("wks_1".into()),
            ..CreateAgentOptions::default()
        },
    )
    .await;
    assert_scenario("create", &run).await;
}

#[tokio::test]
async fn run_finished() {
    let run = Run::new(Mode::Complete).await;
    run.step(1);
    run.create(1, run.config("Finished"), CreateAgentOptions::default())
        .await;
    run.step(2);
    run.run_turn(1).await;
    assert_scenario("run-finished", &run).await;
}

#[tokio::test]
async fn run_failed() {
    let run = Run::new(Mode::Fail).await;
    run.step(1);
    run.create(1, run.config("Failed"), CreateAgentOptions::default())
        .await;
    run.step(2);
    run.run_turn(1).await;
    assert_scenario("run-failed", &run).await;
}

#[tokio::test]
async fn archive_running_with_attention() {
    let run = Run::new(Mode::Permission).await;
    run.step(1);
    run.create(1, run.config("Busy"), CreateAgentOptions::default())
        .await;
    run.step(2);
    let mut stream = run
        .lc
        .manager
        .stream_agent(&id(1), PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap();
    tokio::spawn(async move { while let Ok(Some(_)) = stream.next().await {} });
    settle().await;
    run.step(3);
    run.lc.manager.archive_agent(&id(1)).await.unwrap();
    assert_scenario("archive-running-with-attention", &run).await;
}

#[tokio::test]
async fn close() {
    let run = Run::new(Mode::Complete).await;
    run.step(1);
    run.create(1, run.config("To close"), CreateAgentOptions::default())
        .await;
    run.step(2);
    run.run_turn(1).await;
    run.step(3);
    run.lc.manager.close_agent(&id(1)).await.unwrap();
    assert_scenario("close", &run).await;
}

#[tokio::test]
async fn labels_title_detach() {
    let run = Run::new(Mode::Complete).await;
    run.step(1);
    run.create(1, run.config("Parent"), CreateAgentOptions::default())
        .await;
    run.step(2);
    run.create(
        2,
        run.config("Child"),
        CreateAgentOptions {
            labels: Some(labels(&[
                (PARENT_LABEL, id(1)),
                ("paseo.open-tab.desktop-client", "true".into()),
            ])),
            ..CreateAgentOptions::default()
        },
    )
    .await;
    run.step(3);
    run.lc
        .manager
        .set_labels(&id(2), labels(&[("team", "core".into())]))
        .await
        .unwrap();
    run.step(4);
    run.lc
        .manager
        .set_title(&id(2), "Renamed child")
        .await
        .unwrap();
    run.step(5);
    run.lc
        .manager
        .update_agent_metadata(
            &id(2),
            paseo_core::MetadataUpdates {
                title: Some("Metadata title".into()),
                labels: Some(labels(&[("extra", "1".into())])),
            },
        )
        .await
        .unwrap();
    run.step(6);
    run.lc.manager.detach_agent(&id(2)).await.unwrap();
    assert_scenario("labels-title-detach", &run).await;
}

#[tokio::test]
async fn cascade_archive() {
    let run = Run::new(Mode::Complete).await;
    run.step(1);
    run.create(1, run.config("Parent"), CreateAgentOptions::default())
        .await;
    for (n, title, step) in [(2, "Child A", 2), (3, "Child B", 3)] {
        run.step(step);
        run.create(
            n,
            run.config(title),
            CreateAgentOptions {
                labels: Some(labels(&[(PARENT_LABEL, id(1))])),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    }
    run.step(4);
    run.lc.manager.archive_agent(&id(1)).await.unwrap();
    assert_scenario("cascade-archive", &run).await;
}

#[tokio::test]
async fn archive_snapshot_normalizes_running() {
    let run = Run::new(Mode::Complete).await;
    let record: paseo_store::StoredAgentRecord = serde_json::from_value(json!({
        "id": id(1),
        "provider": "codex",
        "cwd": CWD,
        "createdAt": "2026-01-01T00:00:00.000Z",
        "updatedAt": "2026-01-01T00:00:01.000Z",
        "title": "Stored running",
        "labels": {},
        "lastStatus": "running",
        "config": null,
        "requiresAttention": true,
        "attentionReason": "permission",
        "attentionTimestamp": "2026-01-01T00:00:01.000Z"
    }))
    .unwrap();
    run.lc.store.upsert(record).await.unwrap();
    run.step(5);
    run.lc
        .manager
        .archive_snapshot(&id(1), "2026-01-01T00:00:05.000Z")
        .await
        .unwrap();
    assert_scenario("archive-snapshot-normalizes-running", &run).await;
}

/// PARITY-QUIRK(archive-closed-snapshot-overwrite): the closed snapshot written after the archive
/// keeps the live agent's `lastStatus` (closed) and attention, and preserves only `archivedAt`.
#[tokio::test]
async fn archive_running_after_finished_keeps_attention_in_the_closed_snapshot() {
    let run = Run::new(Mode::CompleteThenHang).await;
    run.step(1);
    run.create(1, run.config("Second turn"), CreateAgentOptions::default())
        .await;
    run.step(2);
    run.run_turn(1).await;
    run.step(3);
    let mut stream = run
        .lc
        .manager
        .stream_agent(&id(1), PromptInput::from("second"), RunOpts::default())
        .await
        .unwrap();
    tokio::spawn(async move { while let Ok(Some(_)) = stream.next().await {} });
    settle().await;
    run.step(4);
    run.lc.manager.archive_agent(&id(1)).await.unwrap();
    assert_scenario("archive-running-after-finished", &run).await;
    let records = run.records().await;
    let record = &records[&id(1)].1;
    assert_eq!(record["lastStatus"], "closed");
    assert_eq!(record["requiresAttention"], true);
}
