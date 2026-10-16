//! Transport seam. The connection only sees channels of JSON values; where they come from
//! (child process stdio, a replayed fixture, an in-memory fake agent) is the transport's job.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch, Notify};

use crate::error::{Error, Result};
use crate::launch::LaunchSpec;
use crate::tee::Tee;
use crate::util::BoxFuture;

#[derive(Debug, Clone, PartialEq)]
pub enum WireEvent {
    /// One decoded line.
    Frame(Value),
    /// A stdout line that was not JSON; the TS adapter logs and ignores these.
    Malformed(String),
    /// The inbound stream ended.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Process-level controls a transport may offer. Non-process transports return no-ops.
pub trait ProcessControl: Send + Sync {
    /// Stderr collected so far (capped).
    fn stderr(&self) -> String;
    fn exit(&self) -> Option<ExitInfo>;
    /// Wait for the process to exit, up to `timeout`.
    fn wait_exit(&self, timeout: Duration) -> BoxFuture<'_, Option<ExitInfo>>;
    /// Close stdin, wait `graceful`, then kill and wait `force`.
    fn terminate(&self, graceful: Duration, force: Duration) -> BoxFuture<'_, ()>;
    /// Synchronous, non-blocking kill request used by drop guards. Default: nothing to kill.
    fn kill(&self) {}
}

pub struct TransportHandle {
    pub outgoing: mpsc::UnboundedSender<Value>,
    pub incoming: mpsc::UnboundedReceiver<WireEvent>,
    pub process: Arc<dyn ProcessControl>,
}

pub trait Transport: Send {
    /// Start moving frames. Must be called inside a tokio runtime.
    fn start(self: Box<Self>) -> Result<TransportHandle>;
}

/// Creates a transport per agent process; swapped out in tests.
pub trait TransportFactory: Send + Sync {
    fn open(&self, spec: &LaunchSpec) -> Result<Box<dyn Transport>>;

    /// Whether the launcher must exist on PATH. Fake transports say no.
    fn requires_binary(&self) -> bool {
        true
    }
}

#[derive(Default)]
pub struct StdioTransportFactory {
    /// When set, every process records its traffic to `<dir>/<n>-<command>.jsonl`.
    pub tee_dir: Option<std::path::PathBuf>,
    counter: std::sync::atomic::AtomicUsize,
}

impl StdioTransportFactory {
    pub fn with_tee_dir(dir: impl Into<std::path::PathBuf>) -> Self {
        StdioTransportFactory {
            tee_dir: Some(dir.into()),
            counter: Default::default(),
        }
    }
}

impl TransportFactory for StdioTransportFactory {
    fn open(&self, spec: &LaunchSpec) -> Result<Box<dyn Transport>> {
        let mut t = StdioTransport::new(spec.clone());
        if let Some(dir) = &self.tee_dir {
            let n = self
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let base = std::path::Path::new(&spec.command)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "agent".into());
            let path = dir.join(format!("{n:02}-{base}.jsonl"));
            let mut argv = vec![spec.command.clone()];
            argv.extend(spec.args.iter().cloned());
            let keys: Vec<String> = spec.env.keys().cloned().collect();
            t = t.with_tee(Tee::create(path, &argv, &keys)?);
        }
        Ok(Box::new(t))
    }
}

const STDERR_CAP: usize = 64 * 1024;

pub struct StdioTransport {
    spec: LaunchSpec,
    tee: Option<Tee>,
}

impl StdioTransport {
    pub fn new(spec: LaunchSpec) -> Self {
        StdioTransport { spec, tee: None }
    }

    pub fn with_tee(mut self, tee: Tee) -> Self {
        self.tee = Some(tee);
        self
    }
}

struct ChildControl {
    stderr: Arc<Mutex<String>>,
    exit_rx: watch::Receiver<Option<ExitInfo>>,
    close_stdin: Arc<Notify>,
    kill: Arc<Notify>,
}

impl ProcessControl for ChildControl {
    fn stderr(&self) -> String {
        self.stderr.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn exit(&self) -> Option<ExitInfo> {
        *self.exit_rx.borrow()
    }

    fn wait_exit(&self, timeout: Duration) -> BoxFuture<'_, Option<ExitInfo>> {
        let mut rx = self.exit_rx.clone();
        Box::pin(async move {
            let wait = async {
                loop {
                    if let Some(info) = *rx.borrow_and_update() {
                        return Some(info);
                    }
                    if rx.changed().await.is_err() {
                        return *rx.borrow();
                    }
                }
            };
            tokio::time::timeout(timeout, wait).await.unwrap_or(None)
        })
    }

    fn kill(&self) {
        if self.exit().is_none() {
            self.close_stdin.notify_one();
            self.kill.notify_one();
        }
    }

    fn terminate(&self, graceful: Duration, force: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if self.exit().is_some() {
                return;
            }
            self.close_stdin.notify_one();
            if self.wait_exit(graceful).await.is_some() {
                return;
            }
            self.kill.notify_one();
            let _ = self.wait_exit(force).await;
        })
    }
}

impl Transport for StdioTransport {
    fn start(self: Box<Self>) -> Result<TransportHandle> {
        let spec = self.spec;
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        for key in &spec.env_remove {
            cmd.env_remove(key);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("no stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("no stdout pipe".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Protocol("no stderr pipe".into()))?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<WireEvent>();
        let tee = self.tee;

        // Writer: one JSON object per line. Dropping stdin on exit signals EOF to the agent.
        let close_stdin = Arc::new(Notify::new());
        let writer_tee = tee.clone();
        let writer_close = close_stdin.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            loop {
                tokio::select! {
                    msg = out_rx.recv() => {
                        let Some(frame) = msg else { break };
                        if let Some(t) = &writer_tee { t.log_in(&frame); }
                        let mut line = frame.to_string();
                        line.push('\n');
                        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                            break;
                        }
                    }
                    _ = writer_close.notified() => break,
                }
            }
        });

        // Reader.
        let reader_tee = tee.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&buf);
                        let line = text.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(line) {
                            Ok(v) => {
                                if let Some(t) = &reader_tee {
                                    t.log_out(&v);
                                }
                                if in_tx.send(WireEvent::Frame(v)).is_err() {
                                    return;
                                }
                            }
                            Err(_) => {
                                let _ = in_tx
                                    .send(WireEvent::Malformed(line.chars().take(200).collect()));
                            }
                        }
                    }
                }
            }
            let _ = in_tx.send(WireEvent::Closed);
        });

        // Stderr collector.
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_sink = stderr_buf.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut chunk = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                if let Ok(mut s) = stderr_sink.lock() {
                    if s.len() < STDERR_CAP {
                        s.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    }
                }
            }
        });

        // Waiter owns the child.
        let (exit_tx, exit_rx) = watch::channel(None);
        let kill = Arc::new(Notify::new());
        let waiter_kill = kill.clone();
        let waiter_tee = tee;
        tokio::spawn(async move {
            let status = tokio::select! {
                s = child.wait() => s,
                _ = waiter_kill.notified() => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let info = match status {
                Ok(s) => ExitInfo {
                    code: s.code(),
                    signal: signal_of(&s),
                },
                Err(_) => ExitInfo {
                    code: None,
                    signal: None,
                },
            };
            if let Some(t) = &waiter_tee {
                t.log_exit(info.code, info.signal);
            }
            let _ = exit_tx.send(Some(info));
        });

        Ok(TransportHandle {
            outgoing: out_tx,
            incoming: in_rx,
            process: Arc::new(ChildControl {
                stderr: stderr_buf,
                exit_rx,
                close_stdin,
                kill,
            }),
        })
    }
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Process control for transports without a process.
pub struct NoProcess;

impl ProcessControl for NoProcess {
    fn stderr(&self) -> String {
        String::new()
    }
    fn exit(&self) -> Option<ExitInfo> {
        None
    }
    fn wait_exit(&self, _timeout: Duration) -> BoxFuture<'_, Option<ExitInfo>> {
        Box::pin(async { None })
    }
    fn terminate(&self, _graceful: Duration, _force: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub fn env_keys(env: &BTreeMap<String, String>) -> Vec<String> {
    env.keys().cloned().collect()
}
