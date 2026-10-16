//! paseo-terminal-worker: JSONL-over-stdio terminal worker. See PROTOCOL.md.

mod activity;
mod coalescer;
mod emu;
#[cfg(not(feature = "stub-emulator"))]
mod emulator;
mod env;
mod input_mode;
mod manager;
mod out;
mod protocol;
mod scanner;
mod spawn;
#[cfg(feature = "stub-emulator")]
mod stub_emulator;
mod terminal;
mod titles;
mod utf8;

use std::io::BufRead;

fn main() {
    let filter = std::env::var("PASEO_TERMINAL_WORKER_LOG").unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .init();

    let (out, writer) = out::spawn_stdout_writer();
    let manager = manager::Manager::new(out.clone(), env::EnvConfig::from_process_env());

    let mut stdin = std::io::stdin().lock();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match stdin.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::error!("stdin read failed: {e}");
                break;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match protocol::parse_request(line) {
            Ok(request) => manager.dispatch(request),
            Err((request_id, message)) => {
                tracing::warn!("{message}");
                if let Some(id) = request_id {
                    out.send(&protocol::response_err(&id, &message));
                }
            }
        }
    }

    manager.shutdown();
    env::cleanup_zsh_runtime_dir();
    out.quit();
    let _ = writer.join();
}
