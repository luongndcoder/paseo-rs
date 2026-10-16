//! One prompt against the real `claude` CLI.
//!
//! ```text
//! cargo run --example simple -- "Say hi in five words"
//! ```
//! Env: `CLAUDE_BIN` (executable, default `claude`), `CLAUDE_MODEL` (default `haiku`).

use paseo_claude::protocol::{ContentBlock, PermissionMode};
use paseo_claude::{ClaudeOptions, SdkMessage, Session};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Say hi in five words.".into());
    let options = ClaudeOptions::new()
        .executable(std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()))
        .model(std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "haiku".into()))
        .permission_mode(PermissionMode::Default)
        .setting_sources(Vec::<String>::new())
        .max_turns(1)
        .on_stderr(|s| eprint!("[stderr] {s}"));

    let mut session = Session::start(options)?;
    session.send_user_message(prompt.as_str())?;

    while let Some(msg) = session.next_message().await {
        match msg? {
            SdkMessage::System(_) => eprintln!("[system message]"),
            SdkMessage::Assistant(a) => {
                for block in &a.message.content {
                    if let ContentBlock::Text(t) = block {
                        println!("{}", t.text);
                    }
                }
            }
            SdkMessage::Result(r) => {
                eprintln!(
                    "[result] {} cost={:?} turns={:?}",
                    r.subtype, r.total_cost_usd, r.num_turns
                );
                break;
            }
            _ => {}
        }
    }

    let commands = session.supported_commands().await?;
    eprintln!("[{} slash commands advertised]", commands.len());
    session.close().await;
    Ok(())
}
