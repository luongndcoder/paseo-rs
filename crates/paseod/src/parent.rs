//! Parent liveness: the TS supervisor holds paseod's stdin pipe (`PASEO_EDGE_PARENT_STDIN=1`).
//! EOF means the supervisor died, so the edge stops itself and, through the worker's own stdin
//! pipe, the worker. A `paseo:graceful-shutdown` line asks for a clean stop on platforms without
//! a catchable SIGTERM.

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentSignal {
    /// stdin closed or failed: the supervisor is gone.
    Lost,
    /// The supervisor asked for a graceful stop.
    GracefulShutdown,
}

/// Resolves when the parent is lost or asks for shutdown. Other lines are ignored.
pub async fn watch_parent<R: AsyncRead + Unpin>(reader: R) -> ParentSignal {
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let is_shutdown = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("type").and_then(|t| t.as_str().map(str::to_owned)))
                    .is_some_and(|t| t == "paseo:graceful-shutdown");
                if is_shutdown {
                    return ParentSignal::GracefulShutdown;
                }
            }
            Ok(None) | Err(_) => return ParentSignal::Lost,
        }
    }
}

/// Linux only: have the kernel SIGTERM us if the supervisor dies without closing the pipe
/// first (for example a stdin that was inherited into a forked child).
#[cfg(target_os = "linux")]
pub fn die_with_parent() {
    if let Err(err) = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM) {
        tracing::warn!(%err, "PR_SET_PDEATHSIG failed");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn die_with_parent() {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn eof_means_the_parent_is_lost() {
        let (mut w, r) = tokio::io::duplex(64);
        w.write_all(b"noise\n{\"type\":\"other\"}\n").await.unwrap();
        drop(w);
        assert_eq!(watch_parent(r).await, ParentSignal::Lost);
    }

    #[tokio::test]
    async fn shutdown_line_is_a_graceful_request() {
        let (mut w, r) = tokio::io::duplex(64);
        w.write_all(b"{\"type\":\"paseo:graceful-shutdown\",\"reason\":\"x\"}\n")
            .await
            .unwrap();
        assert_eq!(watch_parent(r).await, ParentSignal::GracefulShutdown);
    }

    #[tokio::test]
    async fn an_open_silent_pipe_keeps_waiting() {
        let (_w, r) = tokio::io::duplex(64);
        let pending = tokio::time::timeout(std::time::Duration::from_millis(100), watch_parent(r));
        assert!(pending.await.is_err());
    }
}
