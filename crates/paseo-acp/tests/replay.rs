//! Golden-traffic tests. `fixtures/*.jsonl` are either captured from the TS adapter driving a
//! real agent through `tools/capture/tee-acp.mjs` (files without a `synthetic` header) or
//! written by hand from the TS source (`tools/synthetic/make_synthetic.py`).
//!
//! Two kinds of test:
//! - conformance: every recorded frame parses into the typed layer and serializes back to the
//!   same JSON, and nothing known lands in a `Lenient::Unknown`;
//! - behavior: the Rust client replays a fixture; each frame it sends must equal the frame the
//!   TS adapter sent (`messageId` aside).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use paseo_acp::agent::{
    CreateSessionOptions, PermissionResponse, PersistenceHandle, ResumeOverrides, RunOptions,
    SessionConfig,
};
use paseo_acp::client::AcpClient;
use paseo_acp::error::Error;
use paseo_acp::protocol::*;
use paseo_acp::providers::{copilot, cursor, generic, kimi, kiro};
use paseo_acp::replay::{Entry, Fixture, ReplayFactory};
use paseo_acp::{AcpSession, SessionEvent};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;

const CWD: &str = "/tmp/paseo-acp-fixture";
const PONG: &str = "Reply with exactly the word: pong. Do not use any tools.";
const SHELL: &str = "Use your shell tool to run exactly: echo hello > hello.txt . Then reply done.";
const MODE_AGENT: &str = "https://agentclientprotocol.com/protocol/session-modes#agent";
const MODE_PLAN: &str = "https://agentclientprotocol.com/protocol/session-modes#plan";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fx(name: &str) -> Fixture {
    Fixture::load(fixtures_dir().join(format!("{name}.jsonl")))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn all_fixtures() -> Vec<Fixture> {
    let mut paths: Vec<_> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    paths.sort();
    paths.iter().map(|p| Fixture::load(p).unwrap()).collect()
}

fn session_id_of(f: &Fixture) -> String {
    f.outbound()
        .find_map(|v| {
            v.pointer("/result/sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .expect("fixture has a session/new response")
}

fn copilot_replay(names: &[&str]) -> (AcpClient, Arc<ReplayFactory>) {
    let factory = ReplayFactory::new(names.iter().map(|n| fx(n)).collect());
    (
        copilot::client().with_transport_factory(factory.clone()),
        factory,
    )
}

fn cfg() -> SessionConfig {
    SessionConfig {
        cwd: CWD.into(),
        model: Some("gpt-4.1".into()),
        ..Default::default()
    }
}

async fn next_event(rx: &mut UnboundedReceiver<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for a session event")
        .expect("event stream closed")
}

/// Drive events until the turn ends; `on_event` may react (permissions, interrupt).
async fn until_turn_end<F: FnMut(&SessionEvent)>(
    rx: &mut UnboundedReceiver<SessionEvent>,
    mut on_event: F,
) -> Vec<SessionEvent> {
    let mut seen = Vec::new();
    loop {
        let e = next_event(rx).await;
        on_event(&e);
        let end = e.is_turn_end();
        seen.push(e);
        if end {
            return seen;
        }
    }
}

fn assert_clean(factory: &ReplayFactory) {
    for report in factory.reports() {
        assert!(
            report.mismatches().is_empty(),
            "{}",
            report.mismatches().join("\n")
        );
        assert_eq!(report.unconsumed(), 0, "client skipped recorded frames");
        assert!(report.finished());
    }
}

// ---------------------------------------------------------------- conformance

fn strip_nulls(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

/// parse -> serialize must give back the same JSON (an explicit `null` for an optional field
/// is not preserved; ACP treats it as absent).
fn roundtrip<T: serde::Serialize + serde::de::DeserializeOwned>(what: &str, v: &Value) -> T {
    let typed: T = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{what}: {e}\n{v}"));
    let back = serde_json::to_value(&typed).unwrap();
    assert_eq!(
        strip_nulls(&back),
        strip_nulls(v),
        "{what} did not round-trip"
    );
    typed
}

#[test]
fn every_fixture_frame_round_trips_through_the_typed_layer() {
    let mut checked = 0;
    let mut update_kinds: HashMap<String, usize> = HashMap::new();
    for f in all_fixtures() {
        let mut methods: HashMap<String, String> = HashMap::new();
        for entry in &f.entries {
            let (frame, outbound) = match entry {
                Entry::In(v) => (v, false),
                Entry::Out(v) => (v, true),
                Entry::Exit(_) => continue,
            };
            let what = f.name.clone();
            match classify(frame.clone()).unwrap_or_else(|v| panic!("{what}: not JSON-RPC: {v}")) {
                Frame::Request { id, method, params } => {
                    if !outbound {
                        methods.insert(id.to_string(), method.clone());
                        match method.as_str() {
                            "initialize" => drop(roundtrip::<InitializeParams>(&what, &params)),
                            "session/new" => drop(roundtrip::<NewSessionParams>(&what, &params)),
                            "session/load" | "session/resume" => {
                                drop(roundtrip::<LoadSessionParams>(&what, &params))
                            }
                            "session/prompt" => drop(roundtrip::<PromptParams>(&what, &params)),
                            "session/set_mode" => drop(roundtrip::<SetModeParams>(&what, &params)),
                            "session/set_model" => {
                                drop(roundtrip::<SetModelParams>(&what, &params))
                            }
                            "session/set_config_option" => {
                                drop(roundtrip::<SetConfigOptionParams>(&what, &params))
                            }
                            "session/list" => drop(roundtrip::<ListSessionsParams>(&what, &params)),
                            _ => {}
                        }
                    } else if method == "session/request_permission" {
                        let p: RequestPermissionRequest = roundtrip(&what, &params);
                        assert!(!p.options.is_empty());
                    }
                    checked += 1;
                }
                Frame::Notification { method, params } => {
                    if method == "session/update" {
                        let n: SessionNotification = roundtrip(&what, &params);
                        let kind = params["update"]["sessionUpdate"]
                            .as_str()
                            .unwrap()
                            .to_string();
                        assert!(
                            n.update.known().is_some(),
                            "{what}: {kind} fell back to Unknown"
                        );
                        *update_kinds.entry(kind).or_default() += 1;
                    }
                    checked += 1;
                }
                Frame::Response { id, result } => {
                    let Ok(result) = result else {
                        checked += 1;
                        continue;
                    };
                    if outbound {
                        match methods
                            .get(&normalize_response_id(id.clone()).to_string())
                            .map(String::as_str)
                        {
                            Some("initialize") => {
                                let r: InitializeResponse = roundtrip(&what, &result);
                                assert_eq!(r.protocol_version, 1);
                            }
                            Some("session/new" | "session/load" | "session/resume") => {
                                drop(roundtrip::<SessionState>(&what, &result))
                            }
                            Some("session/prompt") => {
                                drop(roundtrip::<PromptResponse>(&what, &result))
                            }
                            Some("session/set_config_option") => {
                                drop(roundtrip::<SetConfigOptionResponse>(&what, &result))
                            }
                            Some("session/list") => {
                                drop(roundtrip::<ListSessionsResponse>(&what, &result))
                            }
                            _ => {}
                        }
                    } else if result.get("outcome").is_some() {
                        drop(roundtrip::<RequestPermissionResponse>(&what, &result));
                    }
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 150, "only {checked} frames checked");
    for kind in [
        "agent_message_chunk",
        "tool_call",
        "tool_call_update",
        "config_option_update",
        "user_message_chunk",
        "available_commands_update",
    ] {
        assert!(
            update_kinds.contains_key(kind),
            "no {kind} frame in any fixture"
        );
    }
}

#[test]
fn fixtures_are_small_and_scrubbed() {
    let mut total = 0;
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        total += text.len();
        for needle in ["/Users/", "/private/tmp/", "ghp_", "github_pat_", "Bearer "] {
            assert!(
                !text.contains(needle),
                "{} contains {needle}",
                path.display()
            );
        }
    }
    assert!(total < 3 * 1024 * 1024, "fixtures are {total} bytes");
}

#[test]
fn unknown_shapes_survive_as_raw_values() {
    let raw =
        json!({"sessionId": "s", "update": {"sessionUpdate": "brand_new_update", "x": [1, 2]}});
    let n: SessionNotification = serde_json::from_value(raw.clone()).unwrap();
    assert!(n.update.known().is_none());
    assert_eq!(serde_json::to_value(&n).unwrap(), raw);

    let block: ContentBlock = serde_json::from_value(json!({"type": "hologram", "id": 7})).unwrap();
    assert!(block.known().is_none());

    let resp: PromptResponse =
        serde_json::from_value(json!({"stopReason": "new_reason", "extra": 1})).unwrap();
    assert!(resp.stop_reason.known().is_none() && !resp.is_cancelled());
    assert_eq!(resp.extra["extra"], 1);
}

// ---------------------------------------------------------------- Copilot, captured

#[tokio::test]
async fn prompt_and_reply() {
    let (client, factory) = copilot_replay(&["01-prompt-reply"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    assert_eq!(session.id().unwrap(), session_id_of(&fx("01-prompt-reply")));
    assert_eq!(session.runtime_info().model.as_deref(), Some("gpt-4.1"));
    let result = session.run(PONG, RunOptions::default()).await.unwrap();
    assert_eq!(result.final_text, "pong");
    assert!(!result.canceled);
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn permission_allow_sends_the_allow_option() {
    let (client, factory) = copilot_replay(&["02-permission-allow"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    let mut events = session.subscribe();
    session
        .start_turn(SHELL, RunOptions::default())
        .await
        .unwrap();

    let mut requested = None;
    let seen = {
        let mut seen = Vec::new();
        loop {
            let e = next_event(&mut events).await;
            if let SessionEvent::PermissionRequested {
                request_id,
                request,
                ..
            } = &e
            {
                assert_eq!(session.pending_permissions().len(), 1);
                requested = Some(request.clone());
                session
                    .respond_to_permission(request_id, PermissionResponse::allow())
                    .await
                    .unwrap();
            }
            let end = e.is_turn_end();
            seen.push(e);
            if end {
                break seen;
            }
        }
    };
    let request = requested.expect("permission requested");
    assert_eq!(request.tool_call.kind.as_deref(), Some("execute"));
    assert_eq!(
        request
            .options
            .iter()
            .map(|o| o.kind.as_str())
            .collect::<Vec<_>>(),
        ["allow_once", "allow_always", "reject_once"]
    );
    assert!(seen
        .iter()
        .any(|e| matches!(e, SessionEvent::PermissionResolved { .. })));
    assert!(matches!(
        seen.last(),
        Some(SessionEvent::TurnCompleted { .. })
    ));
    assert!(session.pending_permissions().is_empty());
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn permission_deny_sends_the_reject_option() {
    let (client, factory) = copilot_replay(&["03-permission-deny"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    let mut events = session.subscribe();
    session
        .start_turn(SHELL, RunOptions::default())
        .await
        .unwrap();
    let s = session.clone();
    let seen = until_turn_end(&mut events, move |e| {
        if let SessionEvent::PermissionRequested { request_id, .. } = e {
            let s = s.clone();
            let id = request_id.clone();
            tokio::spawn(async move {
                s.respond_to_permission(&id, PermissionResponse::deny())
                    .await
                    .unwrap()
            });
        }
    })
    .await;
    assert!(seen.iter().any(|e| matches!(
        e,
        SessionEvent::PermissionResolved {
            response: PermissionResponse::Deny { .. },
            ..
        }
    )));
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn interrupt_sends_session_cancel() {
    let (client, factory) = copilot_replay(&["04-interrupt"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    let mut events = session.subscribe();
    session.start_turn("Write the numbers from 1 to 400, one per line, with a short sentence after each. Do not use tools.", RunOptions::default()).await.unwrap();
    let mut chunks = 0;
    loop {
        let e = next_event(&mut events).await;
        if matches!(&e, SessionEvent::Update(n) if matches!(n.update.known(), Some(KnownSessionUpdate::AgentMessageChunk { .. })))
        {
            chunks += 1;
            if chunks == 1 {
                session.interrupt().await.unwrap();
            }
        }
        if e.is_turn_end() {
            // Copilot answers a cancelled turn with end_turn, so the turn completes.
            assert!(matches!(e, SessionEvent::TurnCompleted { .. }), "{e:?}");
            break;
        }
    }
    assert!(chunks > 10);
    assert!(factory.reports()[0]
        .received()
        .iter()
        .any(|f| f["method"] == "session/cancel" && f.get("id").is_none()));
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn resume_loads_history_then_continues() {
    let source = fx("05-resume");
    let handle = PersistenceHandle {
        provider: "copilot".into(),
        session_id: source
            .inbound()
            .find(|v| v["method"] == "session/load")
            .unwrap()["params"]["sessionId"]
            .as_str()
            .unwrap()
            .into(),
        native_handle: None,
        metadata: Some(json!({"cwd": CWD, "model": "gpt-4.1"})),
    };
    let (client, factory) = copilot_replay(&["05-resume"]);
    let session = client
        .resume_session(
            handle.clone(),
            ResumeOverrides::default(),
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.id().as_deref(), Some(handle.session_id.as_str()));
    let history = session.take_history();
    assert_eq!(
        history.len(),
        2,
        "user + assistant chunk replayed during session/load"
    );
    assert!(matches!(
        history[0].update.known(),
        Some(KnownSessionUpdate::UserMessageChunk { .. })
    ));
    assert!(session.take_history().is_empty(), "history drains once");
    let result = session
        .run(
            "What single word did you reply with earlier? Answer with just that word.",
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.final_text, "pong");
    let persisted = session.describe_persistence().unwrap();
    assert_eq!(persisted.metadata.as_ref().unwrap()["cwd"], CWD);
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn model_and_mode_switch() {
    let (client, factory) = copilot_replay(&["06-model-mode-switch"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    let modes = session.available_modes();
    assert_eq!(
        modes.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
        ["Agent", "Plan", "Allow All"],
        "autopilot is replaced by allow-all"
    );
    let mut events = session.subscribe();
    session.set_mode(MODE_PLAN).await.unwrap();
    assert_eq!(session.current_mode().as_deref(), Some(MODE_PLAN));
    session.set_mode(MODE_AGENT).await.unwrap();
    session.set_model(Some("gpt-5-mini")).await.unwrap();
    assert_eq!(session.runtime_info().model.as_deref(), Some("gpt-5-mini"));
    let result = session.run(PONG, RunOptions::default()).await.unwrap();
    assert!(!result.final_text.is_empty());

    let mut mode_changes = 0;
    let mut model_changes = 0;
    while let Ok(e) = events.try_recv() {
        match e {
            SessionEvent::ModeChanged { .. } => mode_changes += 1,
            SessionEvent::ModelChanged { .. } => model_changes += 1,
            _ => {}
        }
    }
    assert!(mode_changes >= 2 && model_changes >= 1);
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn unsupported_allow_all_surfaces_the_agent_error() {
    let (client, factory) = copilot_replay(&["07-allow-all-unsupported"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    let err = session.set_mode("allow-all").await.unwrap_err();
    assert!(err.is_invalid_params(), "{err:?}");
    assert_eq!(err.to_string(), "Unknown config option 'allow_all'.");
    assert_eq!(err.summarize().code.as_deref(), Some("-32602"));
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn catalog_probe_lists_models_and_modes() {
    let (client, factory) = copilot_replay(&["08-catalog"]);
    let catalog = client.fetch_catalog(Some(CWD)).await.unwrap();
    assert_eq!(
        catalog
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-5.4-mini", "gpt-5-mini", "gpt-4.1", "claude-haiku-4.5"]
    );
    let default: Vec<_> = catalog.models.iter().filter(|m| m.is_default).collect();
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].id, "gpt-5.4-mini");
    assert_eq!(
        default[0]
            .thinking_options
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>(),
        ["low", "medium", "high", "xhigh"]
    );
    assert_eq!(
        default[0].default_thinking_option_id.as_deref(),
        Some("medium")
    );
    assert_eq!(
        catalog
            .modes
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        [MODE_AGENT, MODE_PLAN, "allow-all"]
    );
    assert_clean(&factory);
}

#[tokio::test]
async fn copilot_without_command_updates_has_no_commands() {
    let (client, factory) = copilot_replay(&["09-commands"]);
    let session = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap();
    assert!(session.list_commands().await.is_empty());
    let features = session.features();
    assert!(
        matches!(&features[0], paseo_acp::agent::Feature::Toggle { id, value: false, .. } if id == "auto_accept")
    );
    session.close().await;
    assert_clean(&factory);
}

// ---------------------------------------------------------------- synthetic fixtures

#[tokio::test]
async fn copilot_allow_all_goes_through_the_allow_all_config_option() {
    let factory = ReplayFactory::new(vec![fx("10-copilot-allow-all")]);
    let client = copilot::client().with_transport_factory(factory.clone());
    let session = client
        .create_session(
            SessionConfig {
                cwd: CWD.into(),
                ..Default::default()
            },
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.current_mode().as_deref(), Some(MODE_AGENT));
    session.set_mode("allow-all").await.unwrap();
    assert_eq!(session.current_mode().as_deref(), Some("allow-all"));
    // Leaving allow-all first switches the config option off, then writes the mode.
    session.set_mode(MODE_PLAN).await.unwrap();
    assert_eq!(session.current_mode().as_deref(), Some(MODE_PLAN));
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn kiro_commands_come_from_the_extension_notification() {
    let factory = ReplayFactory::new(vec![fx("11-kiro-commands")]);
    let client =
        kiro::client(vec!["kiro-cli".into(), "acp".into()]).with_transport_factory(factory.clone());
    let session = client
        .create_session(
            SessionConfig {
                cwd: CWD.into(),
                ..Default::default()
            },
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    let commands = tokio::time::timeout(Duration::from_secs(5), session.list_commands())
        .await
        .unwrap();
    let got: Vec<_> = commands
        .iter()
        .map(|c| (c.name.as_str(), c.argument_hint.as_str(), c.kind))
        .collect();
    use paseo_acp::agent::SlashCommandKind::{Command, Skill};
    assert_eq!(
        got,
        [
            ("agent", "swap <name>", Command),
            ("agent-sync-doctor", "", Skill)
        ]
    );
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn cursor_catalog_uses_the_list_available_models_extension() {
    let factory = ReplayFactory::new(vec![fx("12-cursor-catalog")]);
    let client = cursor::client(vec!["cursor-agent".into(), "acp".into()])
        .with_transport_factory(factory.clone());
    let catalog = client.fetch_catalog(Some(CWD)).await.unwrap();
    let ids: Vec<_> = catalog
        .models
        .iter()
        .map(|m| (m.id.as_str(), m.is_default))
        .collect();
    assert_eq!(ids, [("claude-haiku-4-5", true), ("grok-4.6", false)]);
    assert_eq!(
        catalog.models[0]
            .thinking_options
            .iter()
            .map(|o| o.id.as_str())
            .collect::<Vec<_>>(),
        ["false", "true"]
    );
    assert_eq!(
        catalog.models[0].default_thinking_option_id.as_deref(),
        Some("true")
    );
    assert_eq!(catalog.models[1].thinking_options.len(), 4);
    assert_eq!(
        catalog.models[1].default_thinking_option_id.as_deref(),
        Some("xhigh")
    );
    assert_clean(&factory);
}

#[tokio::test]
async fn cursor_old_cli_asks_for_an_update() {
    let factory = ReplayFactory::new(vec![fx("12b-cursor-catalog-old-cli")]);
    let client = cursor::client(vec!["cursor-agent".into(), "acp".into()])
        .with_transport_factory(factory.clone());
    let err = client.fetch_catalog(Some(CWD)).await.unwrap_err();
    assert!(
        matches!(&err, Error::Unsupported(m) if m.starts_with("Update Cursor CLI")),
        "{err:?}"
    );
    assert_clean(&factory);
}

#[tokio::test]
async fn kimi_probes_each_model_for_its_thinking_options() {
    let factory = ReplayFactory::new(vec![fx("13-kimi-catalog")]);
    let client =
        kimi::client(vec!["kimi".into(), "acp".into()]).with_transport_factory(factory.clone());
    let catalog = client.fetch_catalog(Some(CWD)).await.unwrap();
    let ids = |m: &paseo_acp::agent::ModelDefinition| {
        m.thinking_options
            .iter()
            .map(|o| o.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&catalog.models[0]), ["off", "on"]);
    assert_eq!(
        catalog.models[0].default_thinking_option_id.as_deref(),
        Some("off")
    );
    assert_eq!(ids(&catalog.models[1]), ["off", "low", "medium", "high"]);
    assert_eq!(
        catalog.models[1].default_thinking_option_id.as_deref(),
        Some("medium")
    );
    assert_clean(&factory);
}

#[tokio::test]
async fn generic_agent_reads_available_commands_update() {
    let factory = ReplayFactory::new(vec![fx("14-generic-commands")]);
    let client = generic::generic_client(generic::GenericOptions::new(vec![
        "some-agent".into(),
        "--acp".into(),
    ]))
    .with_transport_factory(factory.clone());
    let session = client
        .create_session(
            SessionConfig {
                cwd: CWD.into(),
                ..Default::default()
            },
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    let commands = session.list_commands().await;
    assert_eq!(commands.len(), 2);
    assert_eq!(
        (
            commands[0].name.as_str(),
            commands[0].argument_hint.as_str()
        ),
        ("review", "query")
    );
    assert_eq!(commands[1].argument_hint, "");
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn stringified_numeric_response_ids_are_accepted() {
    let factory = ReplayFactory::new(vec![fx("15-string-numeric-ids")]);
    let client = generic::generic_client(generic::GenericOptions::new(vec!["deepseek-tui".into()]))
        .with_transport_factory(factory.clone());
    let session = client
        .create_session(
            SessionConfig {
                cwd: CWD.into(),
                ..Default::default()
            },
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.id().as_deref(), Some("sess-1"));
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn list_sessions_follows_next_cursor() {
    let factory = ReplayFactory::new(vec![fx("16-list-sessions")]);
    let client = generic::generic_client(generic::GenericOptions::new(vec!["some-agent".into()]))
        .with_transport_factory(factory.clone());
    let sessions = client.list_sessions(Some(CWD), None).await.unwrap();
    assert_eq!(
        sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect::<Vec<_>>(),
        ["s-1", "s-2", "s-3"]
    );
    assert_eq!(sessions[0].title.as_deref(), Some("First"));
    assert_eq!(sessions[2].title, None);
    assert_clean(&factory);
}

#[tokio::test]
async fn resume_falls_back_to_session_resume_and_closes_the_session() {
    let factory = ReplayFactory::new(vec![fx("17-resume-unstable")]);
    let client = generic::generic_client(generic::GenericOptions::new(vec!["some-agent".into()]))
        .with_transport_factory(factory.clone());
    let handle = PersistenceHandle {
        provider: "acp".into(),
        session_id: "sess-1".into(),
        native_handle: None,
        metadata: Some(json!({"cwd": CWD})),
    };
    let session: AcpSession = client
        .resume_session(
            handle,
            ResumeOverrides::default(),
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.current_mode().as_deref(), Some("default"));
    session.close().await;
    assert_clean(&factory);
}

#[tokio::test]
async fn a_client_that_deviates_from_the_fixture_is_reported() {
    let (client, factory) = copilot_replay(&["01-prompt-reply"]);
    // Asking for a different model changes the frames sent after `session/new`.
    let config = SessionConfig {
        model: Some("gpt-5-mini".into()),
        ..cfg()
    };
    let result = client
        .create_session(config, CreateSessionOptions::default())
        .await;
    assert!(result.is_err());
    let mismatches = factory.reports()[0].mismatches();
    assert_eq!(mismatches.len(), 1, "{mismatches:?}");
    assert!(
        mismatches[0].contains("gpt-5-mini") && mismatches[0].contains("gpt-4.1"),
        "{}",
        mismatches[0]
    );
}
