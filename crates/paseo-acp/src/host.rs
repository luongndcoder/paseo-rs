//! Client-side capabilities the agent may call: `fs/*` and `terminal/*`.
//! Behavior follows the `ACPAgentSession` handlers in `acp-agent.ts`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{watch, Notify};

use crate::error::{Error, Result};
use crate::protocol::{
    CreateTerminalParams, ReadTextFileParams, RpcError, TerminalExitStatus, TerminalOutputResponse,
    TerminalRef, WriteTextFileParams,
};
use crate::util::random_uuid;

pub async fn read_text_file(params: &ReadTextFileParams) -> Result<Value> {
    let raw = tokio::fs::read_to_string(&params.path).await?;
    if params.line.unwrap_or(0) == 0 && params.limit.unwrap_or(0) == 0 {
        return Ok(json!({ "content": raw }));
    }
    let lines: Vec<&str> = raw
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let start = params.line.unwrap_or(1).saturating_sub(1) as usize;
    let end = match params.limit {
        Some(l) if l > 0 => (start + l as usize).min(lines.len()),
        _ => lines.len(),
    };
    let slice = if start >= lines.len() {
        &[][..]
    } else {
        &lines[start..end.max(start)]
    };
    Ok(json!({ "content": slice.join("\n") }))
}

pub async fn write_text_file(params: &WriteTextFileParams) -> Result<Value> {
    if let Some(parent) = Path::new(&params.path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&params.path, &params.content).await?;
    Ok(json!({}))
}

struct Output {
    text: String,
    truncated: bool,
    limit: Option<usize>,
}

impl Output {
    fn append(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if let Some(limit) = self.limit.filter(|l| *l > 0) {
            // Drop from the front, whole characters at a time, until the byte limit holds.
            while self.text.len() > limit && !self.text.is_empty() {
                let first = self.text.chars().next().map(char::len_utf8).unwrap_or(1);
                self.text.drain(..first);
                self.truncated = true;
            }
        }
    }
}

struct Entry {
    output: Arc<Mutex<Output>>,
    exit: watch::Receiver<Option<TerminalExitStatus>>,
    kill: Arc<Notify>,
}

pub struct TerminalHost {
    entries: Mutex<HashMap<String, Arc<Entry>>>,
    default_cwd: String,
    launch_env: BTreeMap<String, String>,
}

/// Plain command with no args and whitespace in it is a shell string.
pub fn resolve_terminal_command(command: &str, args: Option<&[String]>) -> (String, Vec<String>) {
    if args.is_some_and(|a| !a.is_empty()) || !command.trim().contains(char::is_whitespace) {
        return (
            command.to_string(),
            args.map(<[String]>::to_vec).unwrap_or_default(),
        );
    }
    if cfg!(windows) {
        ("cmd".into(), vec!["/C".into(), command.to_string()])
    } else {
        ("sh".into(), vec!["-c".into(), command.to_string()])
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        15 => "SIGTERM".into(),
        n => format!("SIG{n}"),
    }
}

impl TerminalHost {
    pub fn new(default_cwd: impl Into<String>, launch_env: BTreeMap<String, String>) -> Self {
        TerminalHost {
            entries: Mutex::new(HashMap::new()),
            default_cwd: default_cwd.into(),
            launch_env,
        }
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry>> {
        self.entries
            .lock()
            .ok()
            .and_then(|e| e.get(id).cloned())
            .ok_or_else(|| Error::NotFound(format!("Unknown terminal '{id}'")))
    }

    pub fn create(&self, params: &CreateTerminalParams) -> Result<Value> {
        let id = random_uuid();
        let (command, args) = resolve_terminal_command(&params.command, params.args.as_deref());
        let mut cmd = Command::new(&command);
        cmd.args(&args)
            .current_dir(params.cwd.as_deref().unwrap_or(&self.default_cwd))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in crate::launch::PARENT_SESSION_ENV_VARS {
            cmd.env_remove(key);
        }
        cmd.envs(&self.launch_env);
        for kv in params.env.iter().flatten() {
            cmd.env(&kv.name, &kv.value);
        }

        let output = Arc::new(Mutex::new(Output {
            text: String::new(),
            truncated: false,
            limit: params.output_byte_limit.map(|l| l as usize),
        }));
        let (exit_tx, exit_rx) = watch::channel(None);
        let kill = Arc::new(Notify::new());

        match cmd.spawn() {
            Err(e) => {
                if let Ok(mut o) = output.lock() {
                    o.append(&format!("{e}\n"));
                }
                let _ = exit_tx.send(Some(TerminalExitStatus {
                    exit_code: None,
                    signal: None,
                }));
            }
            Ok(mut child) => {
                for stream in [
                    child
                        .stdout
                        .take()
                        .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                    child
                        .stderr
                        .take()
                        .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                ]
                .into_iter()
                .flatten()
                {
                    let sink = output.clone();
                    tokio::spawn(async move {
                        let mut stream = stream;
                        let mut buf = [0u8; 4096];
                        while let Ok(n) = stream.read(&mut buf).await {
                            if n == 0 {
                                break;
                            }
                            if let Ok(mut o) = sink.lock() {
                                o.append(&String::from_utf8_lossy(&buf[..n]));
                            }
                        }
                    });
                }
                let waiter_kill = kill.clone();
                tokio::spawn(async move {
                    let status = tokio::select! {
                        s = child.wait() => s,
                        _ = waiter_kill.notified() => {
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    };
                    let exit = match status {
                        Ok(s) => TerminalExitStatus {
                            exit_code: s.code().map(i64::from),
                            signal: exit_signal(&s).map(signal_name),
                        },
                        Err(_) => TerminalExitStatus {
                            exit_code: None,
                            signal: None,
                        },
                    };
                    let _ = exit_tx.send(Some(exit));
                });
            }
        }

        if let Ok(mut e) = self.entries.lock() {
            e.insert(
                id.clone(),
                Arc::new(Entry {
                    output,
                    exit: exit_rx,
                    kill,
                }),
            );
        }
        Ok(json!({ "terminalId": id }))
    }

    pub fn output(&self, params: &TerminalRef) -> Result<Value> {
        let e = self.entry(&params.terminal_id)?;
        let o = e.output.lock().map_err(|_| Error::Closed)?;
        let resp = TerminalOutputResponse {
            output: o.text.clone(),
            truncated: o.truncated,
            exit_status: e.exit.borrow().clone(),
        };
        Ok(serde_json::to_value(resp)?)
    }

    pub async fn wait_for_exit(&self, params: &TerminalRef) -> Result<Value> {
        let e = self.entry(&params.terminal_id)?;
        let mut rx = e.exit.clone();
        loop {
            if let Some(status) = rx.borrow_and_update().clone() {
                return Ok(serde_json::to_value(status)?);
            }
            if rx.changed().await.is_err() {
                return Err(Error::Closed);
            }
        }
    }

    async fn stop(&self, e: &Entry) {
        if e.exit.borrow().is_none() {
            e.kill.notify_one();
            let mut rx = e.exit.clone();
            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                while rx.borrow_and_update().is_none() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
    }

    pub async fn release(&self, params: &TerminalRef) -> Result<Value> {
        let e = self.entry(&params.terminal_id)?;
        self.stop(&e).await;
        if let Ok(mut m) = self.entries.lock() {
            m.remove(&params.terminal_id);
        }
        Ok(json!({}))
    }

    pub async fn kill(&self, params: &TerminalRef) -> Result<Value> {
        let e = self.entry(&params.terminal_id)?;
        self.stop(&e).await;
        Ok(json!({}))
    }

    pub async fn kill_all(&self) {
        let all: Vec<Arc<Entry>> = self
            .entries
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        for e in all {
            self.stop(&e).await;
        }
        if let Ok(mut m) = self.entries.lock() {
            m.clear();
        }
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Map a host error to the JSON-RPC error an ACP agent receives.
pub fn host_error(err: Error) -> RpcError {
    match err {
        Error::Rpc(e) => e,
        Error::NotFound(m) => RpcError {
            code: -32603,
            message: "Internal error".into(),
            data: Some(json!({ "details": m })),
        },
        other => RpcError::internal(other.to_string()),
    }
}
