//! Behavior that needs a scripted agent: permissions, host requests, failures.

mod common;

use std::time::Duration;

use common::{AgentIo, MemoryFactory};
use paseo_acp::agent::{
    CreateSessionOptions, PermissionResponse, PromptBlock, PromptInput, ResumeOverrides,
    ResumePurpose, RunOptions, SessionConfig,
};
use paseo_acp::client::AcpClient;
use paseo_acp::error::Error;
use paseo_acp::providers::generic::{generic_client, GenericOptions};
use paseo_acp::{AcpSession, SessionEvent};
use serde_json::{json, Value};

fn client_with(factory: std::sync::Arc<MemoryFactory>) -> AcpClient {
    generic_client(GenericOptions::new(vec!["fake-agent".into()])).with_transport_factory(factory)
}

async fn session(client: &AcpClient, config: SessionConfig) -> AcpSession {
    client
        .create_session(config, CreateSessionOptions::default())
        .await
        .unwrap()
}

fn cfg() -> SessionConfig {
    SessionConfig {
        cwd: "/tmp".into(),
        ..Default::default()
    }
}

fn permission_params(options: Value) -> Value {
    json!({"sessionId": "s1", "toolCall": {"toolCallId": "t1", "title": "Run it", "kind": "execute"}, "options": options})
}

async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timeout")
        .expect("closed")
}

#[tokio::test]
async fn auto_accept_picks_the_allow_option_without_asking() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let prompt = io.expect("session/prompt").await;
        io.request(
            0,
            "session/request_permission",
            permission_params(json!([
                {"optionId": "no", "kind": "reject_once", "name": "No"},
                {"optionId": "yes", "kind": "allow_once", "name": "Yes"},
            ])),
        );
        let answer = io.recv().await;
        assert_eq!(
            answer,
            json!({"jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "selected", "optionId": "yes"}}})
        );
        io.reply(&prompt, json!({"stopReason": "end_turn"}));
    });
    let mut config = cfg();
    config
        .feature_values
        .insert("auto_accept".into(), json!(true));
    let s = session(&client_with(factory), config).await;
    let mut events = s.subscribe();
    s.start_turn("go", RunOptions::default()).await.unwrap();
    loop {
        let e = next(&mut events).await;
        assert!(
            !matches!(e, SessionEvent::PermissionRequested { .. }),
            "auto-accept must not ask"
        );
        if e.is_turn_end() {
            break;
        }
    }
}

#[tokio::test]
async fn chooser_requests_are_never_auto_accepted() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let prompt = io.expect("session/prompt").await;
        io.request(7, "session/request_permission", permission_params(json!([
            {"optionId": "impl", "kind": "allow_once", "name": "Implement"},
            {"optionId": "impl-clean", "kind": "allow_once", "name": "Implement, clear context"},
            {"optionId": "keep", "kind": "reject_once", "name": "Keep planning"},
        ])));
        let answer = io.recv().await;
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["outcome"]["optionId"], "impl-clean");
        io.reply(&prompt, json!({"stopReason": "end_turn"}));
    });
    let mut config = cfg();
    config
        .feature_values
        .insert("auto_accept".into(), json!(true));
    let s = session(&client_with(factory), config).await;
    let mut events = s.subscribe();
    s.start_turn("go", RunOptions::default()).await.unwrap();
    loop {
        let e = next(&mut events).await;
        if let SessionEvent::PermissionRequested { request_id, .. } = &e {
            // The wrong behavior for the chosen action id is rejected and leaves the request open.
            let bad = s
                .respond_to_permission(
                    request_id,
                    PermissionResponse::Allow {
                        selected_action_id: Some("keep".into()),
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(
                bad.to_string(),
                "ACP permission action 'keep' does not exist or does not match 'allow' behavior"
            );
            assert_eq!(s.pending_permissions().len(), 1);
            s.respond_to_permission(
                request_id,
                PermissionResponse::Allow {
                    selected_action_id: Some("impl-clean".into()),
                },
            )
            .await
            .unwrap();
            let unknown = s
                .respond_to_permission("nope", PermissionResponse::allow())
                .await
                .unwrap_err();
            assert_eq!(
                unknown.to_string(),
                "No pending permission request with id 'nope'"
            );
        }
        if e.is_turn_end() {
            break;
        }
    }
}

#[tokio::test]
async fn deny_with_interrupt_cancels_the_turn() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let prompt = io.expect("session/prompt").await;
        io.request(
            0,
            "session/request_permission",
            permission_params(json!([
                {"optionId": "no", "kind": "reject_once", "name": "No"},
            ])),
        );
        let answer = io.recv().await;
        assert_eq!(answer["result"]["outcome"]["optionId"], "no");
        let cancel = io.expect("session/cancel").await;
        assert_eq!(cancel["params"]["sessionId"], "s1");
        assert!(cancel.get("id").is_none());
        io.reply(&prompt, json!({"stopReason": "cancelled"}));
    });
    let s = session(&client_with(factory), cfg()).await;
    let mut events = s.subscribe();
    s.start_turn("go", RunOptions::default()).await.unwrap();
    loop {
        let e = next(&mut events).await;
        if let SessionEvent::PermissionRequested { request_id, .. } = &e {
            s.respond_to_permission(
                request_id,
                PermissionResponse::Deny {
                    selected_action_id: None,
                    interrupt: true,
                },
            )
            .await
            .unwrap();
        }
        if e.is_turn_end() {
            assert!(
                matches!(e, SessionEvent::TurnCanceled { ref reason, .. } if reason == "Interrupted"),
                "{e:?}"
            );
            break;
        }
    }
}

#[tokio::test]
async fn open_permission_requests_are_cancelled_when_the_turn_is_interrupted() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let prompt = io.expect("session/prompt").await;
        io.request(
            3,
            "session/request_permission",
            permission_params(json!([
                {"optionId": "yes", "kind": "allow_once", "name": "Yes"},
            ])),
        );
        let answer = io.recv().await;
        assert_eq!(
            answer["result"],
            json!({"outcome": {"outcome": "cancelled"}})
        );
        io.expect("session/cancel").await;
        io.reply(&prompt, json!({"stopReason": "cancelled"}));
    });
    let s = session(&client_with(factory), cfg()).await;
    let mut events = s.subscribe();
    s.start_turn("go", RunOptions::default()).await.unwrap();
    loop {
        let e = next(&mut events).await;
        if matches!(e, SessionEvent::PermissionRequested { .. }) {
            s.interrupt().await.unwrap();
            assert!(s.pending_permissions().is_empty());
        }
        if e.is_turn_end() {
            break;
        }
    }
}

#[tokio::test]
async fn agent_exit_mid_turn_fails_the_turn() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        io.expect("session/prompt").await;
        io.exit();
    });
    let s = session(&client_with(factory), cfg()).await;
    let mut events = s.subscribe();
    s.start_turn("go", RunOptions::default()).await.unwrap();
    loop {
        let e = next(&mut events).await;
        if let SessionEvent::TurnFailed { error, .. } = &e {
            assert!(
                error.starts_with("ACP agent exited unexpectedly"),
                "{error}"
            );
            break;
        }
        assert!(!e.is_turn_end(), "{e:?}");
    }
    // The turn slot is free again.
    assert!(s.run("again", RunOptions::default()).await.is_err());
}

#[tokio::test]
async fn prompt_errors_become_turn_failed_with_code_and_detail() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let prompt = io.expect("session/prompt").await;
        io.reply_err(
            &prompt,
            -32000,
            "Authentication required",
            json!({"message": "Run 'agent login' first"}),
        );
    });
    let s = session(&client_with(factory), cfg()).await;
    let err = s.run("go", RunOptions::default()).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Authentication required: Run 'agent login' first"
    );
}

#[tokio::test]
async fn second_turn_is_rejected_while_one_is_active() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        io.expect("session/prompt").await;
        // never answers
        let _ = io.recv().await;
    });
    let s = session(&client_with(factory), cfg()).await;
    s.start_turn("one", RunOptions::default()).await.unwrap();
    let err = s
        .start_turn("two", RunOptions::default())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "A foreground turn is already active");
    s.close().await;
    let closed = s
        .start_turn("three", RunOptions::default())
        .await
        .unwrap_err();
    assert_eq!(closed.to_string(), "acp session is closed");
}

#[tokio::test]
async fn images_go_out_as_image_blocks() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let prompt = io.expect("session/prompt").await;
        assert_eq!(
            prompt["params"]["prompt"],
            json!([{"type": "text", "text": "what is this?"}, {"type": "image", "data": "QUJD", "mimeType": "image/png"}])
        );
        assert_eq!(prompt["params"]["messageId"], "client-msg-1");
        io.notify("session/update", json!({"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "a "}}}));
        io.notify("session/update", json!({"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "cat"}}}));
        io.notify("session/update", json!({"sessionId": "other", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "IGNORED"}}}));
        io.reply(&prompt, json!({"stopReason": "end_turn", "usage": {"inputTokens": 10, "outputTokens": 2, "cachedReadTokens": 4}}));
    });
    let s = session(&client_with(factory), cfg()).await;
    let prompt = PromptInput::Blocks(vec![
        PromptBlock::Text("what is this?".into()),
        PromptBlock::Image {
            data: "QUJD".into(),
            mime_type: "image/png".into(),
        },
    ]);
    let result = s
        .run(
            prompt,
            RunOptions {
                client_message_id: Some("client-msg-1".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.final_text, "a cat");
    let usage = result.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens
        ),
        (Some(10), Some(2), Some(4))
    );
}

#[tokio::test]
async fn host_requests_fs_unknown_methods_and_extensions() {
    let dir = std::env::temp_dir().join(format!("paseo-acp-flows-{}", std::process::id()));
    let file = dir.join("nested").join("a.txt");
    let (file_s, file_for_agent) = (
        file.to_string_lossy().into_owned(),
        file.to_string_lossy().into_owned(),
    );
    let factory = MemoryFactory::new(move |mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        io.request(0, "fs/write_text_file", json!({"sessionId": "s1", "path": file_for_agent, "content": "one\r\ntwo\nthree\nfour"}));
        assert_eq!(
            io.recv().await,
            json!({"jsonrpc": "2.0", "id": 0, "result": {}})
        );
        io.request(
            1,
            "fs/read_text_file",
            json!({"sessionId": "s1", "path": file_for_agent}),
        );
        assert_eq!(
            io.recv().await["result"],
            json!({"content": "one\r\ntwo\nthree\nfour"})
        );
        io.request(
            2,
            "fs/read_text_file",
            json!({"sessionId": "s1", "path": file_for_agent, "line": 2, "limit": 2}),
        );
        assert_eq!(io.recv().await["result"], json!({"content": "two\nthree"}));
        io.request(
            3,
            "fs/read_text_file",
            json!({"sessionId": "s1", "path": "/definitely/not/here"}),
        );
        let missing = io.recv().await;
        assert_eq!(missing["error"]["code"], -32603);
        io.request(4, "vendor/unknown", json!({}));
        assert_eq!(
            io.recv().await["error"],
            json!({"code": -32601, "message": "\"Method not found\": vendor/unknown", "data": {"method": "vendor/unknown"}})
        );
        io.request(
            5,
            "terminal/output",
            json!({"sessionId": "s1", "terminalId": "nope"}),
        );
        assert_eq!(io.recv().await["error"]["code"], -32603);
        io.notify("_vendor/hello", json!({"sessionId": "s1", "n": 1}));
        io.expect("session/prompt").await;
    });
    let s = session(&client_with(factory), cfg()).await;
    let mut events = s.subscribe();
    loop {
        if let SessionEvent::ExtNotification { method, params } = next(&mut events).await {
            assert_eq!(method, "_vendor/hello");
            assert_eq!(params["n"], 1);
            break;
        }
    }
    assert_eq!(
        std::fs::read_to_string(&file_s).unwrap(),
        "one\r\ntwo\nthree\nfour"
    );
    let _ = std::fs::remove_dir_all(dir);
    s.start_turn("x", RunOptions::default()).await.unwrap();
    s.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn terminals_run_commands_for_the_agent() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        io.request(0, "terminal/create", json!({"sessionId": "s1", "command": "printf", "args": ["hello"], "outputByteLimit": 3}));
        let created = io.recv().await;
        let tid = created["result"]["terminalId"]
            .as_str()
            .unwrap()
            .to_string();
        let t = json!({"sessionId": "s1", "terminalId": tid});
        io.request(1, "terminal/wait_for_exit", t.clone());
        assert_eq!(
            io.recv().await["result"],
            json!({"exitCode": 0, "signal": null})
        );
        io.request(2, "terminal/output", t.clone());
        let out = io.recv().await;
        // Truncation drops from the front to honor the byte limit.
        assert_eq!(
            out["result"],
            json!({"output": "llo", "truncated": true, "exitStatus": {"exitCode": 0, "signal": null}})
        );
        io.request(3, "terminal/release", t.clone());
        assert_eq!(io.recv().await["result"], json!({}));
        io.request(4, "terminal/output", t);
        assert_eq!(io.recv().await["error"]["code"], -32603);

        // A command string with spaces runs through the shell; a long runner can be killed.
        io.request(
            5,
            "terminal/create",
            json!({"sessionId": "s1", "command": "sleep 30"}),
        );
        let sleeper = io.recv().await["result"]["terminalId"]
            .as_str()
            .unwrap()
            .to_string();
        let st = json!({"sessionId": "s1", "terminalId": sleeper});
        io.request(6, "terminal/kill", st.clone());
        assert_eq!(io.recv().await["result"], json!({}));
        io.request(7, "terminal/output", st);
        let killed = io.recv().await;
        assert_eq!(killed["result"]["exitStatus"]["signal"], "SIGKILL");
        io.notify("_done", json!({}));
        let _ = io.recv().await;
    });
    let s = session(&client_with(factory), cfg()).await;
    let mut events = s.subscribe();
    loop {
        if matches!(
            next(&mut events).await,
            SessionEvent::ExtNotification { .. }
        ) {
            break;
        }
    }
    s.close().await;
}

#[tokio::test]
async fn unknown_models_are_ignored_with_a_warning() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake(
            "s1",
            json!({"models": {"availableModels": [{"modelId": "fast", "name": "Fast"}], "currentModelId": "fast"}}),
        )
        .await;
        // Must not see a set_model for "nope"; the next thing is the valid switch.
        let set = io.expect("session/set_model").await;
        assert_eq!(set["params"]["modelId"], "fast-2");
        io.reply(&set, json!({}));
        let _ = io.recv().await;
    });
    let client = client_with(factory);
    let mut config = cfg();
    config.model = Some("nope".into());
    let s = session(&client, config).await;
    let mut events = s.subscribe();
    s.set_model(Some("nope")).await.unwrap();
    loop {
        if let SessionEvent::Warning { message } = next(&mut events).await {
            assert!(
                message.contains("nope is not a valid acp model"),
                "{message}"
            );
            break;
        }
    }
    assert_eq!(s.runtime_info().model.as_deref(), Some("fast"));
}

#[tokio::test]
async fn configured_model_ids_are_applied_even_when_not_advertised() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake(
            "s1",
            json!({"models": {"availableModels": [{"modelId": "fast", "name": "Fast"}], "currentModelId": "fast"}}),
        )
        .await;
        let set = io.expect("session/set_model").await;
        assert_eq!(set["params"]["modelId"], "custom/model");
        io.reply(&set, json!({}));
        let _ = io.recv().await;
    });
    let mut config = cfg();
    config.model = Some("custom/model".into());
    let options = CreateSessionOptions {
        configured_model_ids: vec!["custom/model".into()],
        ..Default::default()
    };
    let s = client_with(factory)
        .create_session(config, options)
        .await
        .unwrap();
    assert_eq!(s.runtime_info().model.as_deref(), Some("custom/model"));
}

#[tokio::test]
async fn agents_without_model_selection_keep_their_default() {
    let factory = MemoryFactory::new(|mut io: AgentIo| async move {
        io.handshake("s1", json!({})).await;
        let _ = io.recv().await;
    });
    let mut config = cfg();
    config.model = Some("deepseek/v4".into());
    let client = client_with(factory);
    let s = client
        .create_session(config, CreateSessionOptions::default())
        .await
        .unwrap();
    assert_eq!(
        s.runtime_info().model,
        None,
        "the session reports what the agent runs, not what was asked for"
    );
}

#[tokio::test]
async fn history_resume_releases_the_process() {
    let fixture = paseo_acp::replay::Fixture::load(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/05-resume.jsonl"),
    )
    .unwrap();
    let sid = fixture
        .inbound()
        .find(|v| v["method"] == "session/load")
        .unwrap()["params"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let factory = paseo_acp::replay::ReplayFactory::new(vec![fixture]);
    let client = paseo_acp::providers::copilot::client().with_transport_factory(factory.clone());
    let handle = paseo_acp::agent::PersistenceHandle {
        provider: "copilot".into(),
        session_id: sid,
        native_handle: None,
        metadata: Some(json!({"cwd": "/tmp/paseo-acp-fixture"})),
    };
    let options = CreateSessionOptions {
        resume_purpose: ResumePurpose::History,
        ..Default::default()
    };
    let s = client
        .resume_session(handle, ResumeOverrides::default(), options)
        .await
        .unwrap();
    assert_eq!(s.take_history().len(), 2);
    let err = s.start_turn("hi", RunOptions::default()).await.unwrap_err();
    assert!(matches!(err, Error::Unsupported(m) if m.ends_with("session is not initialized")));
    let report = &factory.reports()[0];
    assert!(report.mismatches().is_empty());
    assert!(
        report.finished(),
        "the agent process was shut down after replay"
    );
}

#[tokio::test]
async fn resume_rejects_foreign_handles_and_missing_cwd() {
    let client = generic_client(GenericOptions::new(vec!["x".into()]));
    let foreign = paseo_acp::agent::PersistenceHandle {
        provider: "copilot".into(),
        session_id: "s".into(),
        native_handle: None,
        metadata: None,
    };
    let err = client
        .resume_session(
            foreign,
            ResumeOverrides::default(),
            CreateSessionOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot resume copilot handle with acp provider"
    );
    let no_cwd = paseo_acp::agent::PersistenceHandle {
        provider: "acp".into(),
        session_id: "s".into(),
        native_handle: None,
        metadata: None,
    };
    let err = client
        .resume_session(
            no_cwd,
            ResumeOverrides::default(),
            CreateSessionOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "acp resume requires the original working directory"
    );
}

#[tokio::test]
async fn missing_launcher_binary_is_reported_before_spawning() {
    let client = generic_client(GenericOptions::new(vec![
        "definitely-not-a-real-binary-xyz".into(),
        "acp".into(),
    ]));
    assert!(!client.is_available().await);
    let err = client
        .create_session(cfg(), CreateSessionOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::NotFound(m) if m == "acp command 'definitely-not-a-real-binary-xyz' not found")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chunks_sent_before_the_prompt_reply_precede_turn_completion() {
    for _ in 0..100 {
        let factory = MemoryFactory::new(|mut io: AgentIo| async move {
            io.handshake("s1", json!({})).await;
            let p = io.expect("session/prompt").await;
            for _ in 0..20 {
                io.notify("session/update", json!({"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x"}}}));
            }
            io.reply(&p, json!({"stopReason": "end_turn"}));
            let _ = io.recv().await;
        });
        let s = session(&client_with(factory), cfg()).await;
        let r = s.run("hi", RunOptions::default()).await.unwrap();
        assert_eq!(r.final_text.len(), 20);
        s.close().await;
    }
}
