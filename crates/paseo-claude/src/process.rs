//! Real transport: spawns `claude` with piped stdio in its own process group.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::error::{Error, Result};
use crate::transport::{ExitInfo, Transport, TransportEvent, TransportIo};

const STDERR_TAIL_BYTES: usize = 2048;
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);
const KILL_ESCALATION: Duration = Duration::from_secs(5);

/// Callback receiving stderr chunks as they arrive (the TS SDK's `Options.stderr`).
pub type StderrCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything needed to launch the process. Built by [`crate::ClaudeOptions`].
#[derive(Clone, Default)]
pub struct SpawnConfig {
    pub command: String,
    /// Arguments placed before the CLI flags (`spawnClaudeCodeProcess` argv prefix).
    pub args_prefix: Vec<String>,
    /// CLI flags.
    pub args: Vec<String>,
    pub args_suffix: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// `true`: start from an empty environment instead of inheriting.
    pub env_clear: bool,
    /// `Some(v)` sets, `None` removes.
    pub env: BTreeMap<String, Option<String>>,
    pub stderr: Option<StderrCallback>,
}

impl std::fmt::Debug for SpawnConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnConfig")
            .field("command", &self.command)
            .field("args_prefix", &self.args_prefix)
            .field("args", &self.args)
            .field("args_suffix", &self.args_suffix)
            .field("cwd", &self.cwd)
            .field("env_clear", &self.env_clear)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

pub struct ProcessTransport {
    config: SpawnConfig,
}

impl ProcessTransport {
    pub fn new(config: SpawnConfig) -> Self {
        Self { config }
    }
}

#[derive(Default)]
struct Tail(Mutex<Vec<u8>>);

impl Tail {
    fn push(&self, bytes: &[u8]) {
        let mut buf = match self.0.lock() {
            Ok(b) => b,
            Err(p) => p.into_inner(),
        };
        buf.extend_from_slice(bytes);
        if buf.len() > STDERR_TAIL_BYTES {
            let cut = buf.len() - STDERR_TAIL_BYTES;
            buf.drain(..cut);
        }
    }

    fn text(&self) -> String {
        let buf = match self.0.lock() {
            Ok(b) => b,
            Err(p) => p.into_inner(),
        };
        String::from_utf8_lossy(&buf).into_owned()
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) {
    // The child leads its own group (process_group(0)), so -pid reaches its descendants.
    // Errors (ESRCH after exit, EPERM) are irrelevant: the goal is "make sure it is gone".
    let Ok(pid) = i32::try_from(pid) else { return };
    // SAFETY: plain syscall with integer arguments.
    unsafe {
        libc::kill(-pid, sig);
    }
}

impl Transport for ProcessTransport {
    fn start(self: Box<Self>) -> Result<TransportIo> {
        let c = self.config;
        let mut cmd = Command::new(&c.command);
        cmd.args(&c.args_prefix)
            .args(&c.args)
            .args(&c.args_suffix)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(cwd) = &c.cwd {
            cmd.current_dir(cwd);
        }
        if c.env_clear {
            cmd.env_clear();
        }
        for (k, v) in &c.env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        let mut child = cmd.spawn().map_err(|source| Error::Spawn {
            command: c.command.clone(),
            source,
        })?;
        let pid = child.id();
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(Error::Protocol("child stdio was not piped".into()));
        };

        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let tail = Arc::new(Tail::default());
        let exited = Arc::new(AtomicBool::new(false));

        // stdin writer: ends (closing stdin) when every sender is dropped or a write fails.
        tokio::spawn(async move {
            while let Some(line) = line_rx.recv().await {
                let mut buf = line.into_bytes();
                buf.push(b'\n');
                if stdin.write_all(&buf).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });

        let out_tx = ev_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        while matches!(buf.last(), Some(b'\n' | b'\r')) {
                            buf.pop();
                        }
                        let line = String::from_utf8_lossy(&buf).into_owned();
                        if out_tx.send(TransportEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let tail2 = tail.clone();
        let cb = c.stderr.clone();
        let stderr_task = tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail2.push(&chunk[..n]);
                        if let Some(cb) = &cb {
                            cb(&String::from_utf8_lossy(&chunk[..n]));
                        }
                    }
                }
            }
        });

        let kill_notify = Arc::new(tokio::sync::Notify::new());
        let notify = kill_notify.clone();
        let exited2 = exited.clone();
        let tail3 = tail.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                () = notify.notified() => {
                    #[cfg(not(unix))]
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            exited2.store(true, Ordering::SeqCst);
            // Drain the pipes so `Exit` is the last event, but a descendant that inherited
            // them (MCP stdio server, background shell) must not hold `Exit` back forever.
            #[cfg(unix)]
            if let Some(pid) = pid {
                // The leader is gone; reap any descendants left in its group.
                signal_group(pid, libc::SIGTERM);
                tokio::spawn(async move {
                    tokio::time::sleep(KILL_ESCALATION).await;
                    signal_group(pid, libc::SIGKILL);
                });
            }
            // One shared deadline: draining stdout then stderr sequentially would double the grace.
            let deadline = tokio::time::Instant::now() + PIPE_DRAIN_GRACE;
            for task in [stdout_task, stderr_task] {
                let abort = task.abort_handle();
                if tokio::time::timeout_at(deadline, task).await.is_err() {
                    abort.abort();
                }
            }
            let mut info = ExitInfo {
                stderr_tail: tail3.text(),
                ..ExitInfo::default()
            };
            if let Ok(status) = status {
                info.code = status.code();
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    info.signal = status.signal();
                }
            }
            let _ = ev_tx.send(TransportEvent::Exit(info));
        });

        let kill = move || {
            if exited.load(Ordering::SeqCst) {
                return;
            }
            #[cfg(unix)]
            if let Some(pid) = pid {
                signal_group(pid, libc::SIGTERM);
                let exited = exited.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        tokio::time::sleep(KILL_ESCALATION).await;
                        if !exited.load(Ordering::SeqCst) {
                            signal_group(pid, libc::SIGKILL);
                        }
                    });
                } else {
                    signal_group(pid, libc::SIGKILL);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
                let _ = KILL_ESCALATION;
                kill_notify.notify_one();
            }
        };
        #[cfg(unix)]
        let _ = &kill_notify;

        Ok(TransportIo {
            stdin: line_tx,
            events: ev_rx,
            kill: Box::new(kill),
        })
    }
}
