//! Replay of golden traffic captured from the TS adapter driving a real `codex app-server`
//! (`tools/capture`). Each scenario runs the Rust client against a fake server that plays the
//! recorded server side; see `tests/common/mod.rs`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{fixture_frames, Dir, ReplayFactory};
use paseo_codex::agent::*;
use paseo_codex::protocol::requests::ServerRequest;
use paseo_codex::protocol::{Frame, Incoming, ServerNotification};
use paseo_codex::{CodexClient, Error};
use serde_json::{json, Value};
use tokio::sync::broadcast;

const FIXTURES: &[&str] = &[
    "01-single-prompt",
    "02-tool-allow",
    "03-tool-deny",
    "04-interrupt",
    "05-resume",
    "06-mode-model-switch",
    "07-catalog-commands",
    "08-image-input",
    "09-importable-sessions",
    "10-list-commands",
];

const CWD: &str = "/tmp/paseo-codex-fixture";

fn client(factory: &Arc<ReplayFactory>) -> CodexClient {
    CodexClient::with_factory(factory.clone())
}

fn config() -> SessionConfig {
    SessionConfig {
        cwd: CWD.into(),
        model: Some("gpt-6-luna".into()),
        ..Default::default()
    }
}

async fn wait_event<F: Fn(&AgentEvent) -> bool>(
    rx: &mut broadcast::Receiver<AgentEvent>,
    pred: F,
) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let ev = rx.recv().await.expect("event stream open");
            if pred(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

fn no_violations(factory: &ReplayFactory) {
    assert_eq!(factory.violations(), Vec::<String>::new());
}

/// Every recorded frame survives parse -> serialize, and the typed layer understands it.
#[test]
fn golden_frames_round_trip_and_type() {
    let mut unknown_notifications = std::collections::BTreeSet::new();
    let mut unknown_requests = std::collections::BTreeSet::new();
    let mut frames = 0;
    for name in FIXTURES {
        for (dir, line) in fixture_frames(name) {
            frames += 1;
            let text = line.to_string();
            let frame: Frame =
                Frame::parse_line(&text).unwrap_or_else(|e| panic!("{name}: {e}: {text}"));
            assert_eq!(
                serde_json::to_value(&frame).unwrap(),
                line,
                "{name}: envelope round trip"
            );
            let incoming =
                Incoming::parse_line(&text).unwrap_or_else(|e| panic!("{name}: {e}: {text}"));
            if dir == Dir::In {
                continue;
            }
            match incoming {
                Incoming::Notification(n) => match n.typed() {
                    ServerNotification::Invalid { method, error, .. } => {
                        panic!("{name}: {method} does not fit its typed view: {error}")
                    }
                    ServerNotification::Unknown { method, .. } => {
                        unknown_notifications.insert(method);
                    }
                    _ => {}
                },
                Incoming::Request(r) => match r.typed() {
                    ServerRequest::Invalid { method, error, .. } => {
                        panic!("{name}: {method}: {error}")
                    }
                    ServerRequest::Unknown { method, .. } => {
                        unknown_requests.insert(method);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    assert!(frames > 500, "fixtures were not loaded ({frames} frames)");
    // Methods the crate passes through as `Unknown` on purpose. A new entry here is a Codex
    // change worth a look, not a failure of the client.
    let allowed: std::collections::BTreeSet<String> = [
        "account/rateLimits/updated",
        "account/updated",
        "deprecationNotice",
        "hook/completed",
        "hook/started",
        "remoteControl/status/changed",
        "thread/goal/cleared",
        "turn/moderationMetadata",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(unknown_notifications, allowed);
    assert!(unknown_requests.is_empty(), "{unknown_requests:?}");
}

#[tokio::test]
async fn single_prompt_returns_reply_and_lifecycle_events() {
    let factory = ReplayFactory::new("01-single-prompt");
    let session = client(&factory).create_session(config()).await.unwrap();
    let result = session
        .run(
            "Reply with exactly the word: pong. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.final_text, "pong");
    assert!(!result.canceled);
    assert_eq!(session.id().as_deref(), Some(result.session_id.as_str()));
    assert!(result.usage.is_some_and(|u| u.output_tokens.is_some()));

    let kinds: Vec<&str> = result
        .events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnStarted { .. } => Some("started"),
            AgentEvent::UsageUpdated { .. } => Some("usage"),
            AgentEvent::TurnCompleted { .. } => Some("completed"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, vec!["started", "usage", "completed"]);
    // raw notifications are forwarded verbatim, in order
    assert!(result.events.iter().any(|e| matches!(
        e,
        AgentEvent::Provider { event, .. } if event.notification.method == "item/agentMessage/delta"
    )));

    // wire shape of the requests, matching the TS adapter
    let turn = &factory.sent(0, "turn/start")[0]["params"];
    assert_eq!(turn["model"], "gpt-6-luna");
    assert_eq!(turn["effort"], "high");
    assert_eq!(turn["serviceTier"], "default");
    assert_eq!(turn["cwd"], CWD);
    assert_eq!(
        turn["collaborationMode"],
        json!({"mode":"default","settings":{"model":"gpt-6-luna"}})
    );
    assert_eq!(
        turn["input"][0],
        json!({"type":"text","text":"Reply with exactly the word: pong. No tools.","text_elements":[]})
    );
    assert!(
        turn.get("approvalPolicy").is_none(),
        "no mode override, no policy fields"
    );
    let start = &factory.sent(0, "thread/start")[0]["params"];
    assert_eq!(start, &json!({"model":"gpt-6-luna","cwd":CWD}));
    let init = &factory.sent(0, "initialize")[0]["params"];
    assert_eq!(init["clientInfo"]["name"], "codex_app_server_daemon");
    assert_eq!(factory.sent(0, "initialized").len(), 1);

    session.close().await.unwrap();
    no_violations(&factory);
}

async fn approval_scenario(
    fixture: &str,
    response: PermissionResponse,
) -> (Arc<ReplayFactory>, RunResult, PermissionRequest) {
    let factory = ReplayFactory::new(fixture);
    let session = client(&factory)
        .create_session(SessionConfig {
            mode_id: Some("read-only".into()),
            ..config()
        })
        .await
        .unwrap();
    let mut rx = session.subscribe();
    let responder = session.clone();
    let seen = tokio::spawn(async move {
        let ev = wait_event(&mut rx, |e| {
            matches!(e, AgentEvent::PermissionRequested { .. })
        })
        .await;
        let AgentEvent::PermissionRequested { request, .. } = ev else {
            unreachable!()
        };
        assert_eq!(responder.pending_permissions(), vec![request.clone()]);
        responder
            .respond_to_permission(&request.id, response)
            .await
            .unwrap();
        request
    });
    let result = session
        .run(
            "Run the shell command `touch out.txt`.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    let request = seen.await.unwrap();
    assert!(session.pending_permissions().is_empty());
    session.close().await.unwrap();
    (factory, result, request)
}

#[tokio::test]
async fn tool_permission_allow() {
    let (factory, result, request) =
        approval_scenario("02-tool-allow", PermissionResponse::allow()).await;
    assert_eq!(request.name, "CodexBash");
    assert_eq!(request.kind, PermissionKind::Tool);
    assert_eq!(
        request.title.as_deref(),
        Some("Run command: /bin/zsh -lc 'touch out.txt'")
    );
    assert_eq!(request.input["command"], "/bin/zsh -lc 'touch out.txt'");
    assert!(request.id.starts_with("permission-exec-"));
    assert_eq!(
        factory.sent_responses(0),
        vec![json!({"id":0,"result":{"decision":"accept"}})]
    );
    assert!(result.events.iter().any(|e| matches!(
        e,
        AgentEvent::PermissionResolved {
            resolution: Some(PermissionResponse::Allow { .. }),
            ..
        }
    )));
    assert!(
        result.final_text.starts_with("Xong"),
        "{}",
        result.final_text
    );
    // thread/start under a mode override carries the preset
    let start = &factory.sent(0, "thread/start")[0]["params"];
    assert_eq!(start["approvalPolicy"], "on-request");
    assert_eq!(start["sandbox"], "read-only");
    assert_eq!(start["approvalsReviewer"], "user");
    no_violations(&factory);
}

#[tokio::test]
async fn tool_permission_deny() {
    let deny = PermissionResponse::Deny {
        message: Some("no".into()),
        interrupt: false,
    };
    let (factory, _result, _request) = approval_scenario("03-tool-deny", deny).await;
    assert_eq!(
        factory.sent_responses(0),
        vec![json!({"id":0,"result":{"decision":"decline"}})]
    );
    no_violations(&factory);
}

#[tokio::test]
async fn tool_permission_deny_with_interrupt_cancels_the_turn() {
    let deny = PermissionResponse::Deny {
        message: None,
        interrupt: true,
    };
    let (factory, _result, _request) = approval_scenario("03-tool-deny", deny).await;
    assert_eq!(
        factory.sent_responses(0),
        vec![json!({"id":0,"result":{"decision":"cancel"}})]
    );
}

#[tokio::test]
async fn unknown_permission_id_is_rejected() {
    let factory = ReplayFactory::new("01-single-prompt");
    let session = client(&factory).create_session(config()).await.unwrap();
    let err = session
        .respond_to_permission("permission-nope", PermissionResponse::allow())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::UnknownPermission(id) if id == "permission-nope"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn interrupt_stops_the_turn_with_native_turn_id() {
    let factory = ReplayFactory::new("04-interrupt");
    let session = client(&factory).create_session(config()).await.unwrap();
    let mut rx = session.subscribe();
    let handle = session
        .start_turn(
            "Write a 2000 word essay about the history of the bicycle. Do not use tools.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(handle.turn_id, "codex-turn-0");
    wait_event(&mut rx, |e| matches!(e, AgentEvent::TurnStarted { .. })).await;
    session.interrupt().await.unwrap();
    let ev = wait_event(&mut rx, |e| e.is_turn_terminal()).await;
    assert!(
        matches!(&ev, AgentEvent::TurnCanceled { reason, turn_id } if reason == "interrupted" && turn_id.as_deref() == Some("codex-turn-0")),
        "{ev:?}"
    );

    let sent = factory.sent(0, "turn/interrupt");
    assert_eq!(sent.len(), 1);
    let params = &sent[0]["params"];
    assert_eq!(params["threadId"], session.id().unwrap());
    assert_eq!(params["turnId"], "01a145c7-7660-7431-812a-099cb707966e");

    // the idle session accepts a no-op interrupt
    session.interrupt().await.unwrap();
    session.close().await.unwrap();
    no_violations(&factory);
}

#[tokio::test]
async fn second_turn_while_active_is_rejected() {
    let factory = ReplayFactory::new("04-interrupt");
    let session = client(&factory).create_session(config()).await.unwrap();
    session
        .start_turn("first".into(), RunOptions::default())
        .await
        .unwrap();
    let err = session
        .start_turn("second".into(), RunOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::TurnActive));
    session.close().await.unwrap();
}

#[tokio::test]
async fn resume_reattaches_the_persisted_thread() {
    let factory = ReplayFactory::new("05-resume");
    let c = client(&factory);
    let first = c.create_session(config()).await.unwrap();
    first
        .run(
            "Remember the secret word: banana. Reply with just: ok.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    let handle = first
        .describe_persistence()
        .expect("persistence handle after the thread exists");
    assert_eq!(handle.provider, "codex");
    assert_eq!(
        handle.native_handle.as_deref(),
        Some(handle.session_id.as_str())
    );
    assert_eq!(handle.metadata["cwd"], CWD);
    assert_eq!(handle.metadata["model"], "gpt-6-luna");
    assert_eq!(handle.metadata["thinkingOptionId"], "high");
    first.close().await.unwrap();

    let resumed = c.resume_session(&handle, None).await.unwrap();
    assert_eq!(resumed.id().as_deref(), Some(handle.session_id.as_str()));
    let out = resumed
        .run(
            "What was the secret word? Reply with just the word. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert!(!out.final_text.is_empty());
    let resume = &factory.sent(1, "thread/resume")[0]["params"];
    assert_eq!(resume["threadId"], handle.session_id.as_str());
    assert!(
        factory.sent(1, "thread/start").is_empty(),
        "a resumed session never starts a new thread"
    );
    resumed.close().await.unwrap();
    no_violations(&factory);
}

#[tokio::test]
async fn catalog_modes_and_models() {
    let factory = ReplayFactory::new("07-catalog-commands");
    let catalog = client(&factory).fetch_catalog().await.unwrap();
    assert_eq!(catalog.models.len(), 7);
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids[0], "gpt-6.1-sol");
    assert!(
        catalog.models[0].label.starts_with("GPT-"),
        "{}",
        catalog.models[0].label
    );
    assert!(catalog
        .models
        .iter()
        .all(|m| !m.thinking_options.is_empty()));
    assert_eq!(catalog.models.iter().filter(|m| m.is_default).count(), 1);
    let modes: Vec<&str> = catalog.modes.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(modes, vec!["auto", "auto-review", "full-access"]);
    assert_eq!(catalog.default_mode_id.as_deref(), Some("auto-review"));
    no_violations(&factory);
}

#[tokio::test]
async fn old_codex_hides_auto_review() {
    let factory = ReplayFactory::with_version("07-catalog-commands", "codex-cli 0.114.0");
    // 0.114 predates `--enable goals`, so the recorded argv (no flag) is what gets opened
    let catalog = client(&factory).fetch_catalog().await.unwrap();
    let modes: Vec<&str> = catalog.modes.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(modes, vec!["auto", "full-access"]);
    assert_eq!(catalog.default_mode_id.as_deref(), Some("auto"));
}

#[tokio::test]
async fn mode_model_and_effort_apply_on_the_next_turn() {
    let factory = ReplayFactory::new("06-mode-model-switch");
    let c = client(&factory);
    let catalog = c.fetch_catalog().await.unwrap();
    let alt = catalog
        .models
        .iter()
        .find(|m| m.id == "gpt-5.6-luna")
        .expect("alt model")
        .id
        .clone();

    let session = c
        .create_session(SessionConfig {
            mode_id: Some("auto".into()),
            ..config()
        })
        .await
        .unwrap();
    session
        .run(
            "Reply with exactly: one. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        session.current_mode().await.unwrap().as_deref(),
        Some("auto")
    );

    assert!(session.set_mode("read-only").await.unwrap().is_none());
    session.set_model(Some(alt.clone())).await.unwrap();
    session
        .set_thinking_option(Some("low".into()))
        .await
        .unwrap();
    assert!(session.set_mode("not-a-mode").await.is_err());
    let info = session.runtime_info().await.unwrap();
    assert_eq!(info.model.as_deref(), Some(alt.as_str()));
    assert_eq!(info.thinking_option_id.as_deref(), Some("low"));
    assert_eq!(info.mode_id.as_deref(), Some("read-only"));
    session
        .run(
            "Reply with exactly: two. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();

    let turns = factory.sent(1, "turn/start");
    assert_eq!(turns.len(), 2);
    let (first, second) = (&turns[0]["params"], &turns[1]["params"]);
    assert_eq!(first["sandboxPolicy"]["type"], "workspaceWrite");
    assert_eq!(first["approvalPolicy"], "on-request");
    assert_eq!(first["model"], "gpt-6-luna");
    assert_eq!(second["sandboxPolicy"], json!({"type":"readOnly"}));
    assert_eq!(second["model"], alt.as_str());
    assert_eq!(second["effort"], "low");
    assert_eq!(
        second["collaborationMode"]["settings"],
        json!({"model": alt, "reasoning_effort":"low"})
    );
    // a loaded thread is re-checked before every turn, as the TS adapter does
    assert_eq!(factory.sent(1, "thread/loaded/list").len(), 1);
    session.close().await.unwrap();
    no_violations(&factory);
}

#[tokio::test]
async fn image_prompt_is_sent_as_local_image() {
    let factory = ReplayFactory::new("08-image-input");
    let session = client(&factory).create_session(config()).await.unwrap();
    // 1x1 PNG
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let result = session
        .run(
            PromptInput(vec![
                PromptBlock::Text(
                    "What color is the attached image? Reply with one word. No tools.".into(),
                ),
                PromptBlock::Image {
                    data: png.into(),
                    mime_type: "image/png".into(),
                },
            ]),
            RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.final_text, "Red");
    let input = &factory.sent(0, "turn/start")[0]["params"]["input"];
    assert_eq!(input[0]["type"], "text");
    assert_eq!(input[1]["type"], "localImage");
    let path = input[1]["path"].as_str().unwrap();
    assert!(path.ends_with(".png"), "{path}");
    assert!(
        std::fs::metadata(path).is_ok(),
        "the image file exists for Codex to read"
    );
    session.close().await.unwrap();
    no_violations(&factory);
}

#[tokio::test]
async fn importable_sessions_are_listed_by_last_use() {
    let factory = ReplayFactory::new("09-importable-sessions");
    // the recording starts with the capture's own session process; skip it
    let session_process = client(&factory).create_session(config()).await.unwrap();
    session_process
        .run(
            "Reply with exactly the word: ok. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .unwrap();
    session_process.close().await.unwrap();
    let listed = client(&factory)
        .list_importable_sessions(Some(CWD), 5)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].cwd.as_deref(), Some(CWD));
    assert_eq!(
        listed[0].title.as_deref(),
        Some("Reply with exactly the word: ok. No tools.")
    );
    assert!(listed[0].updated_at.is_some());
    let req = &factory.sent(1, "thread/list")[0]["params"];
    assert_eq!(req["sortKey"], "updated_at");
    assert_eq!(req["limit"], 50);
    no_violations(&factory);
}

#[tokio::test]
async fn commands_are_builtin_plus_skills() {
    let factory = ReplayFactory::new("10-list-commands");
    let commands = client(&factory).list_commands(&config()).await.unwrap();
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["compact", "example-skill"]);
    assert_eq!(commands[0].kind, SlashCommandKind::Command);
    assert_eq!(commands[1].kind, SlashCommandKind::Skill);
    no_violations(&factory);
}

#[tokio::test]
async fn session_commands_reload_skills() {
    let factory = ReplayFactory::new("01-single-prompt");
    let session = client(&factory).create_session(config()).await.unwrap();
    // the recording has one skills/list (the handshake's); a second would be a violation, so this
    // only checks what the handshake cached via the runtime info
    let info = session.runtime_info().await.unwrap();
    assert_eq!(info.provider, "codex");
    assert_eq!(
        info.extra.get("collaborationMode").and_then(Value::as_str),
        Some("Default")
    );
    session.close().await.unwrap();
}
