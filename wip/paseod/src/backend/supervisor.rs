//! Worker supervision loop: spawn, readiness, crash backoff, graceful stop.

use std::collections::VecDeque;
use std::io::Write;
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, watch};

use super::{BackendState, ReadyInfo};
use crate::config::{parse_listen, BackendConfig, ListenAddr};
use crate::internal::{
    parse_lifecycle_line, LifecycleEvent, ENV_EDGE_PUBLIC_LISTEN, ENV_EDGE_TOKEN, HEADER_TOKEN,
};

pub(super) struct Ctx {
    pub cfg: BackendConfig,
    pub public_listen: String,
    pub token: String,
    pub state_tx: watch::Sender<BackendState>,
    pub life_tx: broadcast::Sender<LifecycleEvent>,
    pub shutdown_rx: watch::Receiver<bool>,
}

enum Outcome {
    Shutdown,
    ReadyTimeout,
    /// Worker process ended. `requested` when it had announced `paseo:shutdown`/`paseo:restart`.
    Exited {
        reason: String,
        requested: bool,
        uptime: Duration,
    },
}

/// Origins a browser at the edge's public address will send. TS derives its fixed CORS
/// origins from the bound (internal) port, so the edge adds these via `PASEO_CORS_ORIGINS`.
pub fn public_origins(public_listen: &str) -> Vec<String> {
    let Ok(ListenAddr::Tcp(addr)) = parse_listen(public_listen) else {
        return Vec::new();
    };
    let port = addr.port();
    let mut out = vec![
        format!("http://localhost:{port}"),
        format!("http://127.0.0.1:{port}"),
    ];
    let ip = addr.ip();
    if !ip.is_unspecified() && !ip.is_loopback() {
        out.push(match ip {
            std::net::IpAddr::V4(v4) => format!("http://{v4}:{port}"),
            std::net::IpAddr::V6(v6) => format!("http://[{v6}]:{port}"),
        });
    }
    out
}

fn merged_cors(cfg: &BackendConfig, public_listen: &str) -> String {
    let user = cfg
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "PASEO_CORS_ORIGINS")
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var("PASEO_CORS_ORIGINS").ok())
        .unwrap_or_default();
    let mut parts: Vec<String> = user
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    for origin in public_origins(public_listen) {
        if !parts.contains(&origin) {
            parts.push(origin);
        }
    }
    parts.join(",")
}

async fn pick_free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    Ok(listener.local_addr()?.port())
}

pub(super) async fn supervise(mut ctx: Ctx) {
    let mut crashes: VecDeque<Instant> = VecDeque::new();
    let mut consecutive: u32 = 0;
    loop {
        if *ctx.shutdown_rx.borrow() {
            return;
        }
        let port = match ctx.cfg.internal_port {
            Some(port) => port,
            None => match pick_free_port().await {
                Ok(port) => port,
                Err(err) => {
                    fail(&ctx, format!("cannot pick an internal port: {err}"));
                    return;
                }
            },
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match run_once(&mut ctx, addr).await {
            Outcome::Shutdown => return,
            Outcome::ReadyTimeout => {
                fail(&ctx, "worker did not become ready in time".into());
                return;
            }
            Outcome::Exited {
                reason,
                requested: true,
                ..
            } => {
                // The worker asked to stop or restart: that decision belongs to our supervisor.
                tracing::info!(%reason, "worker exited on request; not respawning");
                fail(&ctx, reason);
                return;
            }
            Outcome::Exited { reason, uptime, .. } => {
                let now = Instant::now();
                let policy = &ctx.cfg.restart;
                if uptime >= policy.crash_window {
                    consecutive = 0;
                }
                consecutive += 1;
                crashes.push_back(now);
                while crashes
                    .front()
                    .is_some_and(|t| now.duration_since(*t) > policy.crash_window)
                {
                    crashes.pop_front();
                }
                if crashes.len() >= policy.max_crashes {
                    fail(
                        &ctx,
                        format!(
                            "worker crashed {} times within {:?}: {reason}",
                            crashes.len(),
                            policy.crash_window
                        ),
                    );
                    return;
                }
                let factor = 1u32 << (consecutive - 1).min(16);
                let backoff = policy
                    .base_backoff
                    .saturating_mul(factor)
                    .min(policy.max_backoff);
                tracing::warn!(%reason, ?backoff, "worker crashed; restarting");
                let _ = ctx.state_tx.send(BackendState::Restarting { reason });
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = ctx.shutdown_rx.changed() => return,
                }
            }
        }
    }
}

fn fail(ctx: &Ctx, reason: String) {
    tracing::error!(%reason, "backend failed");
    let _ = ctx.state_tx.send(BackendState::Failed { reason });
}

async fn run_once(ctx: &mut Ctx, addr: SocketAddr) -> Outcome {
    let cfg = &ctx.cfg;
    let mut cmd = Command::new(&cfg.command[0]);
    cmd.args(&cfg.command[1..])
        .envs(cfg.env.iter().cloned())
        .env("PASEO_LISTEN", addr.to_string())
        .env(ENV_EDGE_TOKEN, &ctx.token)
        .env(ENV_EDGE_PUBLIC_LISTEN, &ctx.public_listen)
        .env("PASEO_RELAY_ENABLED", "false")
        .env("PASEO_CORS_ORIGINS", merged_cors(cfg, &ctx.public_listen))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Outcome::Exited {
                reason: format!("spawn {:?} failed: {err}", cfg.command[0]),
                requested: false,
                uptime: Duration::ZERO,
            }
        }
    };
    tracing::info!(%addr, pid = ?child.id(), "worker spawned");
    // Held for the life of the worker: dropping it is the worker's "parent gone" signal.
    let mut stdin: Option<ChildStdin> = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<LifecycleEvent>();
    tokio::spawn(pump_stdout(stdout, ev_tx));

    let ready_deadline = tokio::time::Instant::now() + cfg.ready_timeout;
    let mut ready = false;
    let mut requested = false;
    let mut events_open = true;
    let mut probe: Option<tokio::task::JoinHandle<bool>> = None;
    let mut server_id = String::new();

    loop {
        tokio::select! {
            event = ev_rx.recv(), if events_open => match event {
                None => events_open = false,
                Some(LifecycleEvent::Ready { server_id: id, .. }) => {
                    server_id = id;
                    if probe.is_none() && !ready {
                        probe = Some(tokio::spawn(probe_until_healthy(
                            addr,
                            ctx.token.clone(),
                            ready_deadline,
                        )));
                    }
                }
                Some(other) => {
                    requested = true;
                    let _ = ctx.life_tx.send(other);
                }
            },
            healthy = async { probe.as_mut().expect("probe set").await }, if probe.is_some() => {
                probe = None;
                if healthy.unwrap_or(false) {
                    ready = true;
                    tracing::info!(%addr, "worker ready");
                    let _ = ctx.state_tx.send(BackendState::Ready(ReadyInfo {
                        internal_addr: addr,
                        server_id: server_id.clone(),
                    }));
                }
                // On failure the ready deadline branch below fires.
            }
            status = child.wait() => {
                if let Some(probe) = probe.take() { probe.abort(); }
                let reason = match status {
                    Ok(status) => format!("worker exited: {status}"),
                    Err(err) => format!("waiting for worker failed: {err}"),
                };
                return Outcome::Exited { reason, requested, uptime: started.elapsed() };
            }
            _ = ctx.shutdown_rx.changed() => {
                if let Some(probe) = probe.take() { probe.abort(); }
                terminate(&mut child, stdin.take(), cfg.shutdown_grace).await;
                return Outcome::Shutdown;
            }
            _ = tokio::time::sleep_until(ready_deadline), if !ready => {
                if let Some(probe) = probe.take() { probe.abort(); }
                terminate(&mut child, stdin.take(), Duration::from_secs(2)).await;
                return Outcome::ReadyTimeout;
            }
        }
    }
}

/// Reads worker stdout: lifecycle lines go to the channel, everything else is passed through
/// to the edge's stdout so the supervisor sees the same log stream it saw before.
async fn pump_stdout(
    stdout: tokio::process::ChildStdout,
    tx: mpsc::UnboundedSender<LifecycleEvent>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_lifecycle_line(&line) {
            Some(event) => {
                if tx.send(event).is_err() {
                    break;
                }
            }
            None => {
                let mut out = std::io::stdout().lock();
                let _ = writeln!(out, "{line}");
            }
        }
    }
}

async fn probe_until_healthy(
    addr: SocketAddr,
    token: String,
    deadline: tokio::time::Instant,
) -> bool {
    while tokio::time::Instant::now() < deadline {
        if probe_health(addr, &token).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// `GET /api/health` over a bare TCP connection (HTTP/1.0, no pooling). 200 means healthy.
pub(super) async fn probe_health(addr: SocketAddr, token: &str) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect(addr).await?;
        let request = format!(
            "GET /api/health HTTP/1.0\r\nHost: 127.0.0.1\r\n{HEADER_TOKEN}: {token}\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;
        let mut buf = [0u8; 32];
        let mut filled = 0;
        while filled < 12 {
            let n = stream.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok::<bool, std::io::Error>(
            buf[..filled].starts_with(b"HTTP/1.1 200")
                || buf[..filled].starts_with(b"HTTP/1.0 200"),
        )
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(2), attempt).await,
        Ok(Ok(true))
    )
}

/// SIGTERM on Unix (stdin shutdown line elsewhere), then SIGKILL after `grace`.
async fn terminate(child: &mut Child, stdin: Option<ChildStdin>, grace: Duration) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        drop(stdin);
    }
    #[cfg(not(unix))]
    {
        if let Some(mut stdin) = stdin {
            let _ = stdin
                .write_all(b"{\"type\":\"paseo:graceful-shutdown\"}\n")
                .await;
        }
    }
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        tracing::warn!("worker ignored graceful stop; killing");
        let _ = child.kill().await;
    }
}
