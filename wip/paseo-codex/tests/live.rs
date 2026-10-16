//! Live smoke test against the installed, authenticated `codex`. Ignored by default: it spends a
//! few tokens. Run with `cargo test --test live -- --ignored --nocapture`.

use paseo_codex::agent::*;
use paseo_codex::{CodexClient, CodexOptions};

#[tokio::test]
#[ignore = "needs an authenticated codex CLI and spends tokens"]
async fn prompt_and_reply() {
    let client = CodexClient::new(CodexOptions::new());
    let catalog = client.fetch_catalog().await.expect("catalog");
    let model = std::env::var("PASEO_CODEX_LIVE_MODEL").ok().or_else(|| {
        catalog
            .models
            .iter()
            .find(|m| m.id.contains("luna"))
            .map(|m| m.id.clone())
    });
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let session = client
        .create_session(SessionConfig {
            cwd,
            model,
            ..Default::default()
        })
        .await
        .expect("session");
    let result = session
        .run(
            "Reply with exactly the word: pong. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .expect("run");
    println!(
        "final text: {:?}, usage: {:?}",
        result.final_text, result.usage
    );
    assert!(result.final_text.to_lowercase().contains("pong"));
    let handle = session.describe_persistence().expect("handle");
    session.close().await.expect("close");

    let resumed = client.resume_session(&handle, None).await.expect("resume");
    let again = resumed
        .run(
            "Reply with exactly the word: ping. No tools.".into(),
            RunOptions::default(),
        )
        .await
        .expect("run after resume");
    println!("after resume: {:?}", again.final_text);
    assert!(again.final_text.to_lowercase().contains("ping"));
    resumed.close().await.expect("close");
}
