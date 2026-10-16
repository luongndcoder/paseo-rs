//! Golden-traffic tests: fixtures recorded from the TS adapter driving a real `opencode serve`.
//!
//! 1. Every recorded SSE event and REST body survives `parse -> serialize` unchanged.
//! 2. The Rust agent client, run against the recorded server, sends the same requests as the TS
//!    adapter (method, path, body) and reaches the same outcomes.

use paseo_opencode::agent::*;
use paseo_opencode::protocol::*;
use paseo_opencode::replay::{json_equal, load_fixture, Record, ReplayServers, ReplayTransport};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

const CWD: &str = "/tmp/paseo-opencode-fixture";
const MODEL: &str = "opencode/glm-5.3-flash-free";
const MODEL_B: &str = "opencode/deepseek-v4.1-flash-free";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.jsonl"))
}

fn all_fixtures() -> Vec<PathBuf> {
    let mut v: Vec<_> =
        std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
    v.sort();
    v
}

// ---------- 1. protocol round trip ----------

#[test]
fn every_sse_event_round_trips() {
    let mut frames = 0;
    let mut by_type = std::collections::BTreeMap::<String, (usize, usize)>::new();
    for path in all_fixtures() {
        for record in load_fixture(&path).unwrap() {
            if let Record::Sse { event, .. } = record {
                let parsed = GlobalEvent::from_value(event.clone());
                let back = parsed.to_value();
                assert!(
                    json_equal(&event, &back),
                    "{}: {event}\n!=\n{back}",
                    path.display()
                );
                let entry = by_type
                    .entry(parsed.payload.event.type_name().to_owned())
                    .or_default();
                entry.0 += 1;
                if matches!(parsed.payload.event, Event::Unknown { .. }) {
                    entry.1 += 1;
                }
                frames += 1;
            }
        }
    }
    assert!(frames > 500, "fixtures look empty: {frames} frames");
    // Event types the adapter depends on must parse into typed variants, not the Unknown fallback.
    for ty in [
        "session.created",
        "session.updated",
        "session.status",
        "session.idle",
        "message.updated",
        "message.part.updated",
        "message.part.delta",
        "permission.asked",
        "permission.replied",
        "server.heartbeat",
    ] {
        let (total, unknown) = by_type.get(ty).copied().unwrap_or((0, 0));
        assert!(total > 0, "fixtures have no {ty}");
        assert_eq!(unknown, 0, "{ty}: {unknown}/{total} fell back to Unknown");
    }
}

#[test]
fn rest_bodies_round_trip_through_typed_structs() {
    let mut checked = 0;
    for path in all_fixtures() {
        let records = load_fixture(&path).unwrap();
        let mut paths = std::collections::HashMap::new();
        for r in &records {
            if let Record::Req {
                proc,
                id,
                method,
                path,
                ..
            } = r
            {
                paths.insert(
                    (*proc, *id),
                    format!("{method} {}", path.split('?').next().unwrap()),
                );
            }
        }
        for r in &records {
            let Record::Res {
                proc,
                id,
                status: 200,
                body,
            } = r
            else {
                continue;
            };
            let key = paths.get(&(*proc, *id)).cloned().unwrap_or_default();
            let back: Value = match key.as_str() {
                "POST /session" | "PATCH /session" => {
                    serde_json::to_value(serde_json::from_value::<Session>(body.clone()).unwrap())
                        .unwrap()
                }
                "GET /provider" => serde_json::to_value(
                    serde_json::from_value::<ProviderList>(body.clone()).unwrap(),
                )
                .unwrap(),
                "GET /agent" => serde_json::to_value(
                    serde_json::from_value::<Vec<AgentInfo>>(body.clone()).unwrap(),
                )
                .unwrap(),
                "GET /command" => serde_json::to_value(
                    serde_json::from_value::<Vec<CommandInfo>>(body.clone()).unwrap(),
                )
                .unwrap(),
                k if k.starts_with("GET /session/") && k.ends_with("/message") => {
                    serde_json::to_value(
                        serde_json::from_value::<Vec<MessageWithParts>>(body.clone()).unwrap(),
                    )
                    .unwrap()
                }
                k if k.starts_with("GET /session/") && !k.ends_with("/children") => {
                    serde_json::to_value(serde_json::from_value::<Session>(body.clone()).unwrap())
                        .unwrap()
                }
                _ => continue,
            };
            assert!(
                json_equal(body, &back),
                "{key} in {}:\n{body}\n!=\n{back}",
                path.display()
            );
            checked += 1;
        }
    }
    assert!(checked >= 10, "only {checked} bodies checked");
}

#[test]
fn unknown_and_malformed_events_never_error() {
    let unknown = serde_json::json!({"directory":"/x","payload":{"id":"e1","type":"brand.new","properties":{"a":1}}});
    let parsed = GlobalEvent::from_value(unknown.clone());
    assert!(matches!(&parsed.payload.event, Event::Unknown { ty, .. } if ty == "brand.new"));
    assert_eq!(parsed.to_value(), unknown);

    // known type, properties that do not fit the struct: falls back, keeps the data
    let odd = serde_json::json!({"payload":{"type":"session.idle","properties":{"sessionID":7}}});
    let parsed = GlobalEvent::from_value(odd.clone());
    assert!(matches!(parsed.payload.event, Event::Unknown { .. }));
    assert_eq!(parsed.to_value(), odd);

    let junk = serde_json::json!([1, 2, 3]);
    let parsed = GlobalEvent::from_value(junk.clone());
    assert!(matches!(parsed.payload.event, Event::Malformed(_)));
    assert_eq!(parsed.to_value(), junk);
}

// ---------- 2. agent client against recorded traffic ----------

struct Replayed {
    transport: ReplayTransport,
    client: OpenCodeAgentClient,
}

fn replay(name: &str) -> Replayed {
    let transport = ReplayTransport::from_fixture(fixture(name))
        .unwrap()
        // The TS adapter lists child sessions before every prompt; the Rust adapter does not.
        .skip_requests_containing("/children")
        .build();
    let servers = ReplayServers::new(transport.clone());
    let client = OpenCodeAgentClient::with_servers(servers, Default::default());
    Replayed { transport, client }
}

impl Replayed {
    /// The Rust client made exactly the recorded requests.
    fn assert_traffic_matches(&self) {
        assert_eq!(
            self.transport.unmatched(),
            Vec::<String>::new(),
            "requests not in the recording"
        );
        assert_eq!(
            self.transport.mismatches(),
            Vec::<String>::new(),
            "requests that differ from the recording"
        );
        assert_eq!(
            self.transport.unconsumed(),
            Vec::<String>::new(),
            "recorded requests never made"
        );
    }
}

fn config() -> AgentSessionConfig {
    AgentSessionConfig {
        cwd: CWD.into(),
        model: Some(MODEL.into()),
        mode_id: Some("build".into()),
        ..Default::default()
    }
}

fn ephemeral() -> AgentCreateSessionOptions {
    AgentCreateSessionOptions {
        persist_session: Some(false),
    }
}

async fn next_matching(
    rx: &mut broadcast::Receiver<AgentEvent>,
    mut f: impl FnMut(&AgentEvent) -> bool,
) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Ok(e) if f(&e) => return e,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("event channel closed: {e}"),
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn raw_types(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Provider { event, .. } => Some(event.payload.event.type_name().to_owned()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_and_reply() {
    let r = replay("01-prompt-reply");
    let session = r
        .client
        .create_session(config(), None, ephemeral())
        .await
        .unwrap();
    let result = session
        .run(
            text_prompt("Reply with exactly the word: pong. Do not use any tools."),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(result.error.is_none() && !result.canceled, "{result:?}");
    assert!(
        result.final_text.contains("pong"),
        "final text: {:?}",
        result.final_text
    );
    let types = raw_types(&result.events);
    assert!(
        types.contains(&"message.part.delta".to_owned()),
        "raw events are forwarded: {types:?}"
    );
    assert!(result
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::TurnStarted { .. })));
    session.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_permission_allow() {
    let r = replay("02-permission-allow");
    let mut cfg = config();
    cfg.permission_rules = vec![PermissionRule {
        permission: "bash".into(),
        pattern: "*".into(),
        action: Some(PermissionAction::Ask),
        ..Default::default()
    }];
    let session = r
        .client
        .create_session(cfg, None, ephemeral())
        .await
        .unwrap();
    let mut rx = session.subscribe();
    session
        .start_turn(
            text_prompt(
                "Use the bash tool to run exactly: echo captured > out.txt   Then reply done.",
            ),
            Default::default(),
        )
        .await
        .unwrap();
    let AgentEvent::PermissionRequested { request, .. } = next_matching(&mut rx, |e| {
        matches!(e, AgentEvent::PermissionRequested { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(request.name, "bash");
    assert_eq!(request.kind, PermissionKind::Tool);
    assert_eq!(
        request.input["metadata"]["command"],
        "echo captured > out.txt"
    );
    assert_eq!(session.get_pending_permissions().len(), 1);
    session
        .respond_to_permission(&request.id, AgentPermissionResponse::allow())
        .await
        .unwrap();
    assert!(session.get_pending_permissions().is_empty());
    let end = next_matching(&mut rx, AgentEvent::is_terminal).await;
    assert!(matches!(end, AgentEvent::TurnCompleted { .. }), "{end:?}");
    session.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_permission_deny() {
    let r = replay("03-permission-deny");
    let mut cfg = config();
    cfg.permission_rules = vec![PermissionRule {
        permission: "bash".into(),
        pattern: "*".into(),
        action: Some(PermissionAction::Ask),
        ..Default::default()
    }];
    let session = r
        .client
        .create_session(cfg, None, ephemeral())
        .await
        .unwrap();
    let mut rx = session.subscribe();
    session
        .start_turn(
            text_prompt(
                "Use the bash tool to run exactly: echo denied > out.txt   Then reply done.",
            ),
            Default::default(),
        )
        .await
        .unwrap();
    let AgentEvent::PermissionRequested { request, .. } = next_matching(&mut rx, |e| {
        matches!(e, AgentEvent::PermissionRequested { .. })
    })
    .await
    else {
        unreachable!()
    };
    session
        .respond_to_permission(&request.id, AgentPermissionResponse::deny("Denied by test"))
        .await
        .unwrap();
    let resolved = next_matching(&mut rx, |e| {
        matches!(e, AgentEvent::PermissionResolved { .. })
    })
    .await;
    assert!(
        matches!(&resolved, AgentEvent::PermissionResolved { reply: Some(r), .. } if r == "reject"),
        "{resolved:?}"
    );
    let end = next_matching(&mut rx, AgentEvent::is_terminal).await;
    assert!(
        matches!(
            end,
            AgentEvent::TurnCompleted { .. } | AgentEvent::TurnFailed { .. }
        ),
        "{end:?}"
    );
    assert!(matches!(
        session
            .respond_to_permission("per_unknown", AgentPermissionResponse::allow())
            .await,
        Err(paseo_opencode::Error::UnknownPermission(_))
    ));
    session.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupt_cancels_the_turn() {
    let r = replay("04-interrupt");
    let session = r
        .client
        .create_session(config(), None, ephemeral())
        .await
        .unwrap();
    let mut rx = session.subscribe();
    session
        .start_turn(
            text_prompt("Write a very long story of at least 2000 words about a lighthouse keeper. No tools."),
            Default::default(),
        )
        .await
        .unwrap();
    next_matching(&mut rx, |e| {
        matches!(e, AgentEvent::Provider { event, .. } if matches!(event.payload.event, Event::MessagePartUpdated(_) | Event::MessagePartDelta(_)))
    })
    .await;
    session.interrupt().await.unwrap();
    let end = next_matching(&mut rx, AgentEvent::is_terminal).await;
    assert!(
        matches!(&end, AgentEvent::TurnCanceled { reason, .. } if reason == "interrupted"),
        "{end:?}"
    );
    session.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_and_history() {
    let r = replay("05-resume");
    let session = r
        .client
        .create_session(
            config(),
            None,
            AgentCreateSessionOptions {
                persist_session: Some(true),
            },
        )
        .await
        .unwrap();
    let first = session
        .run(
            text_prompt("Remember the secret word: tangerine. Reply with exactly: ok."),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(first.error.is_none());
    let handle = session.describe_persistence().unwrap();
    assert_eq!(handle.provider, "opencode");
    assert_eq!(handle.metadata["cwd"], CWD);
    assert_eq!(handle.metadata["model"], MODEL);
    session.close().await.unwrap();

    let resumed = r.client.resume_session(&handle, None, None).await.unwrap();
    assert_eq!(resumed.id(), handle.session_id);
    let history = resumed.stream_history().await.unwrap();
    assert!(history.len() >= 2, "history: {history:?}");
    assert_eq!(history[0]["info"]["role"], "user");
    let second = resumed
        .run(
            text_prompt("What was the secret word? Reply with just the word."),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(second.error.is_none() && !second.canceled);
    resumed.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn model_and_mode_switch() {
    let r = replay("06-model-mode-switch");
    let session = r
        .client
        .create_session(config(), None, ephemeral())
        .await
        .unwrap();
    let modes = session.get_available_modes().await.unwrap();
    let ids: Vec<_> = modes.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        &ids[..2],
        ["build", "plan"],
        "build and plan sort first: {ids:?}"
    );
    assert_eq!(
        modes[1].description.as_deref(),
        Some("Plan mode. Disallows all edit tools.")
    );

    session.set_mode("plan").await.unwrap();
    assert_eq!(session.get_current_mode().await.as_deref(), Some("plan"));
    let one = session
        .run(
            text_prompt("Reply with exactly: one. No tools."),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(one.error.is_none());

    session.set_model(Some(MODEL_B)).await.unwrap();
    session.set_mode("build").await.unwrap();
    let info = session.get_runtime_info().await.unwrap();
    assert_eq!(
        (info.model.as_deref(), info.mode_id.as_deref()),
        (Some(MODEL_B), Some("build"))
    );
    let two = session
        .run(
            text_prompt("Reply with exactly: two. No tools."),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(two.error.is_none());
    session.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn catalog_models_modes_and_commands() {
    let r = replay("07-catalog");
    let catalog = r
        .client
        .fetch_catalog(FetchCatalogOptions {
            scope: FetchCatalogScope::Workspace { cwd: CWD.into() },
            force: false,
        })
        .await
        .unwrap();
    assert!(
        catalog.models.iter().any(|m| m.id == MODEL),
        "models: {:?}",
        catalog.models.iter().map(|m| &m.id).collect::<Vec<_>>()
    );
    let model = catalog.models.iter().find(|m| m.id == MODEL).unwrap();
    assert_eq!(model.provider_id, "opencode");
    assert!(model.context_window_max_tokens.is_some());
    assert!(catalog.modes.iter().any(|m| m.id == "build"));

    let commands = r.client.list_commands(&config()).await.unwrap();
    assert!(
        commands.iter().any(|c| c.name == "init"),
        "commands: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c.name == "compact"),
        "adapter-handled commands are always listed"
    );
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn image_prompt_is_sent_as_file_part() {
    let r = replay("08-image-prompt");
    let session = r
        .client
        .create_session(config(), None, ephemeral())
        .await
        .unwrap();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let result = session
        .run(
            vec![
                PromptPart::Text("Describe this image in five words. No tools.".into()),
                PromptPart::Image {
                    mime_type: "image/png".into(),
                    data: png.into(),
                },
            ],
            Default::default(),
        )
        .await
        .unwrap();
    assert!(result.error.is_none());
    session.close().await.unwrap();
    r.assert_traffic_matches();
}

#[tokio::test(flavor = "multi_thread")]
async fn second_turn_while_running_is_rejected() {
    let r = replay("01-prompt-reply");
    let session: Arc<dyn AgentSession> = r
        .client
        .create_session(config(), None, ephemeral())
        .await
        .unwrap();
    session
        .start_turn(
            text_prompt("Reply with exactly the word: pong. Do not use any tools."),
            Default::default(),
        )
        .await
        .unwrap();
    let again = session
        .start_turn(text_prompt("again"), Default::default())
        .await;
    assert!(matches!(again, Err(paseo_opencode::Error::TurnActive)));
    session.close().await.unwrap();
}
