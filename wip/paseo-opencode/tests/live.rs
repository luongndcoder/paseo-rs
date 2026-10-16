//! End-to-end against a real `opencode` binary. Ignored by default (needs the CLI and a model):
//!
//! `cargo test --test live -- --ignored --nocapture`
//!
//! `PASEO_OPENCODE_TEST_MODEL` overrides the model (default: a free OpenCode model).

use paseo_opencode::agent::*;
use paseo_opencode::OpenCodeOptions;

fn model() -> String {
    std::env::var("PASEO_OPENCODE_TEST_MODEL")
        .unwrap_or_else(|_| "opencode/glm-5.3-flash-free".into())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the opencode CLI and network access to a model"]
async fn real_server_prompt_permission_and_cleanup() {
    let home = tempdir("home");
    let cwd = tempdir("cwd");
    let client = OpenCodeAgentClient::new(OpenCodeOptions::new().home_dir(&home));
    assert!(client.is_available().await, "opencode is not on PATH");

    let catalog = client
        .fetch_catalog(FetchCatalogOptions {
            scope: FetchCatalogScope::Workspace { cwd: cwd.clone() },
            force: false,
        })
        .await
        .expect("catalog");
    assert!(
        catalog.models.iter().any(|m| m.id == model()),
        "model {} not offered",
        model()
    );
    assert!(catalog.modes.iter().any(|m| m.id == "build"));

    // Secured server (random basic-auth password) + prompt/reply.
    let config = AgentSessionConfig {
        cwd: cwd.clone(),
        model: Some(model()),
        mode_id: Some("build".into()),
        permission_rules: vec![paseo_opencode::protocol::PermissionRule {
            permission: "bash".into(),
            pattern: "*".into(),
            action: Some(paseo_opencode::protocol::PermissionAction::Ask),
            ..Default::default()
        }],
        ..Default::default()
    };
    let session = client
        .create_session(
            config,
            None,
            AgentCreateSessionOptions {
                persist_session: Some(false),
            },
        )
        .await
        .expect("create");
    let reply = session
        .run(
            text_prompt("Reply with exactly the word: pong. Do not use any tools."),
            Default::default(),
        )
        .await
        .expect("run");
    assert!(reply.error.is_none(), "{reply:?}");
    assert!(
        reply.final_text.to_lowercase().contains("pong"),
        "reply: {:?}",
        reply.final_text
    );

    // Permission round trip.
    let mut rx = session.subscribe();
    session
        .start_turn(
            text_prompt("Use the bash tool to run exactly: echo hi > out.txt   Then reply done."),
            Default::default(),
        )
        .await
        .unwrap();
    let mut answered = false;
    let end = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            match rx.recv().await.unwrap() {
                AgentEvent::PermissionRequested { request, .. } => {
                    session
                        .respond_to_permission(&request.id, AgentPermissionResponse::allow())
                        .await
                        .unwrap();
                    answered = true;
                }
                e if e.is_terminal() => return e,
                _ => {}
            }
        }
    })
    .await
    .expect("turn finished");
    assert!(matches!(end, AgentEvent::TurnCompleted { .. }), "{end:?}");
    assert!(answered, "model never asked for bash permission");
    assert_eq!(
        std::fs::read_to_string(format!("{cwd}/out.txt"))
            .unwrap()
            .trim(),
        "hi"
    );

    session.close().await.unwrap();
    client.shutdown().await;
}

fn tempdir(tag: &str) -> String {
    let dir =
        std::env::temp_dir().join(format!("paseo-opencode-live-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap().to_string_lossy().into_owned()
}
