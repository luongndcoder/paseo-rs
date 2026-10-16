//! Spawns and supervises one `opencode serve` process.

use crate::error::{Error, Result};
use crate::http::HttpTransport;
use crate::options::OpenCodeOptions;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch};

const STARTUP_BUFFER_CAP: usize = 8192;
const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(5);

/// HTTP basic-auth username OpenCode uses unless `OPENCODE_SERVER_USERNAME` overrides it.
pub const BASIC_AUTH_USER: &str = "opencode";

/// Finds `name` on `PATH`, or accepts a path that exists.
pub fn find_executable(name: &std::path::Path) -> Option<PathBuf> {
    if name.components().count() > 1 {
        return name.is_file().then(|| name.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn resolve_binary(opts: &OpenCodeOptions) -> Result<PathBuf> {
    let wanted = opts
        .binary_override()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("opencode"));
    find_executable(&wanted).ok_or_else(|| {
        Error::BinaryNotFound(format!(
            "'{}' not found. Install OpenCode and make sure it is on PATH.",
            wanted.display()
        ))
    })
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// A running `opencode serve`. Dropping it kills the process group.
pub struct ServerProcess {
    pub url: String,
    pub port: u16,
    password: Option<String>,
    pid: Option<u32>,
    exited: watch::Receiver<Option<String>>,
    kill_tx: Option<oneshot::Sender<()>>,
}

impl ServerProcess {
    pub async fn spawn(
        opts: &OpenCodeOptions,
        extra_env: &std::collections::HashMap<String, String>,
    ) -> Result<Self> {
        let binary = resolve_binary(opts)?;
        let port = free_port()?;
        std::fs::create_dir_all(opts.home_dir_ref())?;
        let password = opts.resolve_password();

        let mut cmd = Command::new(&binary);
        cmd.args(opts.prefix_args_ref())
            .args([
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .current_dir(opts.home_dir_ref())
            .envs(opts.env_ref())
            .envs(extra_env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(p) = &password {
            cmd.env("OPENCODE_SERVER_PASSWORD", p)
                .env("OPENCODE_PASSWORD", p);
        }
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Startup(format!("spawn {}: {e}", binary.display())))?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let (listening_tx, listening_rx) = oneshot::channel::<()>();
        let startup_out = Arc::new(std::sync::Mutex::new(String::new()));
        let startup_err = Arc::new(std::sync::Mutex::new(String::new()));
        spawn_stdout_reader(stdout, listening_tx, startup_out.clone());
        spawn_stderr_reader(stderr, startup_err.clone());

        let (exit_tx, exited) = watch::channel(None);
        let (kill_tx, kill_rx) = oneshot::channel();
        tokio::spawn(supervise(child, pid, exit_tx, kill_rx));

        let mut exit_watch = exited.clone();
        let headline = tokio::select! {
            r = listening_rx => match r {
                Ok(()) => None,
                Err(_) => Some("OpenCode server closed stdout before listening".to_owned()),
            },
            _ = exit_watch.wait_for(|v| v.is_some()) => {
                let why = exited.borrow().clone().unwrap_or_default();
                Some(format!("OpenCode server exited during startup ({why})"))
            }
            _ = tokio::time::sleep(opts.startup_timeout_value()) => Some("OpenCode server startup timeout".to_owned()),
        };
        let this = Self {
            url: format!("http://127.0.0.1:{port}"),
            port,
            password,
            pid,
            exited,
            kill_tx: Some(kill_tx),
        };
        if let Some(headline) = headline {
            let mut msg = headline;
            let err = startup_err.lock().unwrap().trim().to_owned();
            let out = startup_out.lock().unwrap().trim().to_owned();
            if !err.is_empty() {
                msg.push_str(&format!("\nstderr: {err}"));
            }
            if !out.is_empty() {
                msg.push_str(&format!("\nstdout: {out}"));
            }
            this.shutdown().await;
            return Err(Error::Startup(msg));
        }
        Ok(this)
    }

    /// Transport bound to this server (basic auth when a password is set).
    pub fn transport(&self) -> Result<HttpTransport> {
        let auth = self
            .password
            .clone()
            .map(|p| (BASIC_AUTH_USER.to_owned(), p));
        HttpTransport::new(self.url.clone(), auth)
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Resolves once the process has exited, with a description of how.
    pub fn exited(&self) -> watch::Receiver<Option<String>> {
        self.exited.clone()
    }

    pub fn is_running(&self) -> bool {
        self.exited.borrow().is_none()
    }

    /// SIGTERM the process group, SIGKILL after the grace period; resolves once it has exited.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.kill_tx.take() {
            let _ = tx.send(());
        }
        let mut rx = self.exited.clone();
        let _ = tokio::time::timeout(
            GRACEFUL_SHUTDOWN + Duration::from_secs(2),
            rx.wait_for(|v| v.is_some()),
        )
        .await;
    }
}

async fn supervise(
    mut child: Child,
    pid: Option<u32>,
    exit_tx: watch::Sender<Option<String>>,
    kill_rx: oneshot::Receiver<()>,
) {
    let describe = |status: std::io::Result<std::process::ExitStatus>| match status {
        Ok(s) => s.to_string(),
        Err(e) => format!("wait failed: {e}"),
    };
    tokio::select! {
        status = child.wait() => {
            let _ = exit_tx.send(Some(describe(status)));
        }
        res = kill_rx => {
            // `Err` means the owner was dropped: kill immediately (kill_on_drop also covers the leader).
            if res.is_ok() {
                signal_group(pid, false);
                if tokio::time::timeout(GRACEFUL_SHUTDOWN, child.wait()).await.is_err() {
                    signal_group(pid, true);
                }
            } else {
                signal_group(pid, true);
            }
            let status = child.wait().await;
            let _ = exit_tx.send(Some(describe(status)));
        }
    }
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, force: bool) {
    let Some(pid) = pid else { return };
    let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: plain syscall on the process group we created with `process_group(0)`.
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _force: bool) {}

fn spawn_stdout_reader(
    stdout: tokio::process::ChildStdout,
    listening: oneshot::Sender<()>,
    captured: Arc<std::sync::Mutex<String>>,
) {
    tokio::spawn(async move {
        let mut listening = Some(listening);
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            {
                let mut buf = captured.lock().unwrap();
                if buf.len() < STARTUP_BUFFER_CAP {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
            if line.contains("listening on") {
                if let Some(tx) = listening.take() {
                    let _ = tx.send(());
                }
            }
        }
    });
}

fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    captured: Arc<std::sync::Mutex<String>>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 2048];
        // Keep draining so the child never blocks on a full pipe; retain only the startup prefix.
        while let Ok(n) = reader.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let mut buf = captured.lock().unwrap();
            if buf.len() < STARTUP_BUFFER_CAP {
                buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            }
        }
    });
}
