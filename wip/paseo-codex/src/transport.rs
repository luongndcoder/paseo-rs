//! Transport seam between the JSON-RPC client and the thing it talks to.
//!
//! The production transport spawns `codex app-server` and speaks line-delimited JSON over its
//! stdio. Tests (and the golden-traffic replay) swap in another [`Transport`] through a
//! [`TransportFactory`], so no layer above this module knows whether a process exists.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch};

use crate::error::{Error, Result};
use crate::jsonl::LineDecoder;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(2);
const FORCE_SHUTDOWN: Duration = Duration::from_secs(1);
const TERM_GRACE: Duration = Duration::from_millis(500);
const READER_DRAIN: Duration = Duration::from_secs(1);

/// Signal the process group led by `pid` (`process_group(0)` makes pgid == pid).
#[cfg(unix)]
fn signal_group(pid: Option<u32>, force: bool) {
    if let Some(pid) = pid {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: killpg takes plain integers; a stale pgid yields ESRCH, which is ignored.
        unsafe {
            libc::killpg(pid as libc::pid_t, sig);
        }
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _force: bool) {}

struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        signal_group(self.0, true);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitInfo {
    /// Exit status text (`exit status: 0`, `signal: 9 (SIGKILL)`, ...); empty for a replay.
    pub status: String,
    /// Last bytes the process wrote to stderr.
    pub stderr_tail: String,
}

impl ExitInfo {
    pub fn describe(&self) -> String {
        format!("{}\n{}", self.status, self.stderr_tail)
            .trim()
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// One complete line from the server, without its terminator.
    Line(String),
    /// The server is gone. Always the last event, delivered after every line it wrote.
    Exited(ExitInfo),
}

pub trait TransportControl: Send + Sync + 'static {
    /// Stop the server: close its stdin, wait for a graceful exit, then force it.
    fn terminate(&self) -> BoxFuture<()>;
}

pub struct TransportIo {
    /// Lines to write to the server (a newline is appended). Dropping every sender closes stdin.
    pub outgoing: mpsc::UnboundedSender<String>,
    pub incoming: mpsc::UnboundedReceiver<TransportEvent>,
    pub control: Arc<dyn TransportControl>,
}

pub trait Transport: Send {
    fn start(self: Box<Self>) -> Result<TransportIo>;
}

/// What a session needs from the outside world: a way to open a transport and to ask the binary
/// for its version (gates features by Codex version).
pub trait TransportFactory: Send + Sync + 'static {
    /// `args` are the arguments after the binary, e.g. `["app-server"]`.
    fn open(&self, args: &[String]) -> Result<Box<dyn Transport>>;
    /// Output of `codex --version`.
    fn version(&self) -> BoxFuture<Result<String>>;
}

/// Spawns the real Codex binary.
#[derive(Debug, Clone)]
pub struct ProcessFactory {
    pub command: PathBuf,
    /// Arguments placed before `app-server` (e.g. a wrapper script's own flags).
    pub prefix_args: Vec<String>,
    /// Added to the inherited environment.
    pub env: BTreeMap<String, String>,
}

impl Default for ProcessFactory {
    fn default() -> Self {
        ProcessFactory {
            command: PathBuf::from("codex"),
            prefix_args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

impl TransportFactory for ProcessFactory {
    fn open(&self, args: &[String]) -> Result<Box<dyn Transport>> {
        let mut all = self.prefix_args.clone();
        all.extend_from_slice(args);
        Ok(Box::new(ProcessTransport {
            command: self.command.clone(),
            args: all,
            env: self.env.clone(),
        }))
    }

    fn version(&self) -> BoxFuture<Result<String>> {
        let command = self.command.clone();
        let mut args = self.prefix_args.clone();
        args.push("--version".into());
        let env = self.env.clone();
        Box::pin(async move {
            let out = Command::new(command)
                .args(args)
                .envs(env)
                .stdin(Stdio::null())
                .output()
                .await?;
            if !out.status.success() {
                return Err(Error::Invalid(format!(
                    "codex --version failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
    }
}

pub struct ProcessTransport {
    command: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

struct ProcessControl {
    close_stdin: Mutex<Option<oneshot::Sender<()>>>,
    kill: Mutex<Option<oneshot::Sender<()>>>,
    exited: watch::Receiver<bool>,
}

impl ProcessControl {
    async fn wait_exited(&self, limit: Duration) -> bool {
        let mut rx = self.exited.clone();
        tokio::time::timeout(limit, async move {
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .is_ok()
    }
}

impl TransportControl for ProcessControl {
    fn terminate(&self) -> BoxFuture<()> {
        if let Some(tx) = self.close_stdin.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(());
        }
        let kill = self.kill.lock().ok().and_then(|mut g| g.take());
        let this = ProcessControl {
            close_stdin: Mutex::new(None),
            kill: Mutex::new(None),
            exited: self.exited.clone(),
        };
        Box::pin(async move {
            if this.wait_exited(GRACEFUL_SHUTDOWN).await {
                return;
            }
            if let Some(tx) = kill {
                let _ = tx.send(());
            }
            this.wait_exited(FORCE_SHUTDOWN).await;
        })
    }
}

impl Transport for ProcessTransport {
    fn start(self: Box<Self>) -> Result<TransportIo> {
        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args)
            .envs(&self.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Invalid("no stdin".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Invalid("no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Invalid("no stderr".into()))?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (close_tx, mut close_rx) = oneshot::channel::<()>();
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let (exited_tx, exited_rx) = watch::channel(false);
        let stderr_tail = Arc::new(Mutex::new(String::new()));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    line = out_rx.recv() => {
                        let Some(line) = line else { break };
                        let mut bytes = line.into_bytes();
                        bytes.push(b'\n');
                        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                            break;
                        }
                    }
                    _ = &mut close_rx => break,
                }
            }
            // dropping stdin sends EOF
        });

        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                if let Ok(mut t) = tail.lock() {
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > STDERR_TAIL_BYTES {
                        let mut cut = t.len() - STDERR_TAIL_BYTES;
                        while !t.is_char_boundary(cut) {
                            cut += 1;
                        }
                        t.drain(..cut);
                    }
                }
            }
        });

        let lines_tx = in_tx.clone();
        let reader = tokio::spawn(async move {
            let mut decoder = LineDecoder::new();
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = stdout.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                for line in decoder.push(&buf[..n]) {
                    if lines_tx.send(TransportEvent::Line(line)).is_err() {
                        return;
                    }
                }
            }
            if let Some(line) = decoder.finish() {
                let _ = lines_tx.send(TransportEvent::Line(line));
            }
        });

        let tail = stderr_tail;
        let pgid = child.id();
        tokio::spawn(async move {
            let mut kill_rx = kill_rx;
            // Drop guard: if this task is cancelled (runtime shutdown) or finishes, kill the
            // whole process group so codex's children and MCP servers do not survive.
            let mut group = GroupKill(pgid);
            let status = tokio::select! {
                s = child.wait() => s,
                _ = &mut kill_rx => {
                    signal_group(pgid, false);
                    match tokio::time::timeout(TERM_GRACE, child.wait()).await {
                        Ok(s) => s,
                        Err(_) => {
                            signal_group(pgid, true);
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    }
                }
            };
            // Anything left in the group (orphans holding stdout) dies with the leader.
            signal_group(pgid, true);
            group.0 = None;
            // every stdout line is delivered before the exit event, but an orphan holding the
            // pipe must not block the exit event forever
            let mut reader = reader;
            if tokio::time::timeout(READER_DRAIN, &mut reader)
                .await
                .is_err()
            {
                reader.abort();
            }
            let status = match status {
                Ok(s) => s.to_string(),
                Err(e) => format!("wait failed: {e}"),
            };
            let stderr_tail = tail.lock().map(|t| t.clone()).unwrap_or_default();
            let _ = in_tx.send(TransportEvent::Exited(ExitInfo {
                status,
                stderr_tail,
            }));
            let _ = exited_tx.send(true);
        });

        Ok(TransportIo {
            outgoing: out_tx,
            incoming: in_rx,
            control: Arc::new(ProcessControl {
                close_stdin: Mutex::new(Some(close_tx)),
                kill: Mutex::new(Some(kill_tx)),
                exited: exited_rx,
            }),
        })
    }
}
