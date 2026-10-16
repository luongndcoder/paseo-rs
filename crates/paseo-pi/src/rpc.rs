//! Request/response correlation and event fan-out over a [`Transport`].
//!
//! Mirrors `JsonlRpcProcess`: ids are `req_<n>`, a response nothing waits for is published like
//! any other frame, and `request_stop_work` treats an exited process as success.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::frame::{Decoded, FrameDecoder};
use crate::protocol::events::ProcessExit;
use crate::protocol::{Command, Event};
use crate::transport::Transport;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    /// The client's configured default.
    Default,
    /// Wait for a response, process death or `close()` only (e.g. LLM-backed `compact`).
    Never,
    After(Duration),
}

#[derive(Default)]
struct ReadyState {
    frame: Option<Value>,
    exit: Option<String>,
}

struct Inner {
    name: String,
    transport: Arc<dyn Transport>,
    default_timeout: Duration,
    pending: Mutex<HashMap<String, Sender<Result<Value>>>>,
    subscribers: Mutex<Vec<Sender<Event>>>,
    next_id: AtomicU64,
    disposed: AtomicBool,
    exited: AtomicBool,
    ready: Mutex<ReadyState>,
    ready_cv: Condvar,
    /// Frames that failed to decode, for diagnostics.
    problems: Mutex<Vec<String>>,
}

impl Inner {
    fn fail_all(&self, make: impl Fn() -> Error) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let drained: Vec<_> = self.pending.lock().unwrap().drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(make()));
        }
    }

    fn broadcast(&self, event: Event) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.send(event.clone()).is_ok());
    }

    fn dispatch_frame(&self, frame: Map<String, Value>) {
        let is_response = frame.get("type").and_then(Value::as_str) == Some("response");
        if is_response {
            if let Some(id) = frame.get("id").and_then(Value::as_str) {
                if let Some(tx) = self.pending.lock().unwrap().remove(id) {
                    let _ = tx.send(settle(&self.name, &frame));
                    return;
                }
            }
        }
        let value = Value::Object(frame);
        if value.get("type").and_then(Value::as_str) == Some("ready") {
            let mut ready = self.ready.lock().unwrap();
            if ready.frame.is_none() {
                ready.frame = Some(value.clone());
                self.ready_cv.notify_all();
            }
        }
        self.broadcast(Event::from_value(value));
    }

    fn on_eof(&self) {
        let info = self.transport.exit_info();
        let mut message = format!(
            "{} process exited with code {} and signal {}",
            self.name,
            info.code.map_or("null".to_string(), |c| c.to_string()),
            info.signal.map_or("null".to_string(), |s| s.to_string()),
        );
        if !info.stderr_tail.is_empty() {
            message.push('\n');
            message.push_str(&info.stderr_tail);
        }
        self.exited.store(true, Ordering::SeqCst);
        {
            let mut ready = self.ready.lock().unwrap();
            ready.exit = Some(message.clone());
            self.ready_cv.notify_all();
        }
        self.broadcast(Event::ProcessExit(ProcessExit {
            error: message.clone(),
        }));
        self.fail_all(|| Error::Closed(message.clone()));
    }

    fn stderr_tail(&self) -> String {
        // The transport only exposes stderr after exit; a live timeout reports none.
        String::new()
    }
}

fn settle(name: &str, frame: &Map<String, Value>) -> Result<Value> {
    let command = frame
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("request")
        .to_string();
    if frame.get("success").and_then(Value::as_bool) == Some(true) {
        return Ok(frame.get("data").cloned().unwrap_or(Value::Null));
    }
    let message = frame
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{name} {command} failed"));
    Err(Error::Rpc { command, message })
}

/// A request that has been written and awaits its response.
pub struct PendingRequest {
    id: String,
    command: String,
    rx: Receiver<Result<Value>>,
    timeout: Option<Duration>,
    started: Instant,
    inner: Arc<Inner>,
}

impl PendingRequest {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn wait(self) -> Result<Value> {
        let outcome = match self.timeout {
            None => self.rx.recv().map_err(|_| ()),
            Some(limit) => match self.rx.recv_timeout(limit) {
                Ok(v) => Ok(v),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.inner.pending.lock().unwrap().remove(&self.id);
                    return Err(Error::Timeout {
                        command: self.command,
                        elapsed_ms: self.started.elapsed().as_millis(),
                        timeout_ms: limit.as_millis(),
                        stderr: self.inner.stderr_tail(),
                    });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
            },
        };
        match outcome {
            Ok(result) => result,
            Err(()) => Err(Error::Closed(format!(
                "{} process is closed",
                self.inner.name
            ))),
        }
    }
}

#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}

impl RpcClient {
    /// Starts the reader thread immediately; subscribe before sending if you need every event.
    pub fn new(
        transport: Arc<dyn Transport>,
        diagnostic_name: impl Into<String>,
        default_timeout: Duration,
    ) -> Self {
        let inner = Arc::new(Inner {
            name: diagnostic_name.into(),
            transport,
            default_timeout,
            pending: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            disposed: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            ready: Mutex::new(ReadyState::default()),
            ready_cv: Condvar::new(),
            problems: Mutex::new(Vec::new()),
        });
        let reader = Arc::clone(&inner);
        thread::Builder::new()
            .name("paseo-pi-reader".into())
            .spawn(move || {
                let mut decoder = FrameDecoder::new();
                while let Some(line) = reader.transport.read_line() {
                    match decoder.consume_line(&line) {
                        Decoded::Nothing => {}
                        Decoded::Frame(frame) => reader.dispatch_frame(frame),
                        Decoded::Problem(problem) => {
                            reader.problems.lock().unwrap().push(format!("{problem:?}"));
                        }
                    }
                }
                reader.on_eof();
            })
            .expect("spawn reader thread");
        Self { inner }
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = mpsc::channel();
        self.inner.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Decode problems seen so far (invalid JSON, broken chunks).
    pub fn problems(&self) -> Vec<String> {
        self.inner.problems.lock().unwrap().clone()
    }

    pub fn has_exited(&self) -> bool {
        self.inner.exited.load(Ordering::SeqCst)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.disposed.load(Ordering::SeqCst)
    }

    pub fn start_request(&self, command: &Command, timeout: Timeout) -> Result<PendingRequest> {
        self.start_raw_request(command.to_value(), timeout)
    }

    /// Like `start_request` for a hand-built frame (must be a JSON object with a `type`).
    pub fn start_raw_request(
        &self,
        mut command: Value,
        timeout: Timeout,
    ) -> Result<PendingRequest> {
        let inner = &self.inner;
        if inner.disposed.load(Ordering::SeqCst) {
            return Err(Error::Closed(format!("{} process is closed", inner.name)));
        }
        let name = command
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Protocol("command has no `type`".into()))?
            .to_string();
        let id = format!("req_{}", inner.next_id.fetch_add(1, Ordering::SeqCst));
        command
            .as_object_mut()
            .ok_or_else(|| Error::Protocol("command is not an object".into()))?
            .insert("id".into(), Value::String(id.clone()));
        let (tx, rx) = mpsc::channel();
        inner.pending.lock().unwrap().insert(id.clone(), tx);
        if let Err(error) = self.write(&command) {
            inner.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        let timeout = match timeout {
            Timeout::Default => Some(inner.default_timeout),
            Timeout::Never => None,
            Timeout::After(d) => Some(d),
        };
        Ok(PendingRequest {
            id,
            command: name,
            rx,
            timeout,
            started: Instant::now(),
            inner: Arc::clone(inner),
        })
    }

    pub fn request(&self, command: &Command, timeout: Timeout) -> Result<Value> {
        self.start_request(command, timeout)?.wait()
    }

    pub fn request_raw(&self, command: Value, timeout: Timeout) -> Result<Value> {
        self.start_raw_request(command, timeout)?.wait()
    }

    /// For commands whose goal an exited process has already met (abort, clear_queue): returns
    /// `Ok` once the child is gone instead of failing.
    pub fn request_stop_work(&self, command: &Command, timeout: Timeout) -> Result<()> {
        if self.has_exited() {
            return Ok(());
        }
        match self.request(command, timeout) {
            Ok(_) => Ok(()),
            Err(error) => {
                // A dying child breaks its stdin pipe a moment before EOF reaches the reader.
                let deadline = Instant::now() + Duration::from_secs(2);
                while !self.has_exited() && Instant::now() < deadline {
                    if !matches!(error, Error::Closed(_) | Error::Io(_)) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                if self.has_exited() {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    /// Fire-and-forget frame (`extension_ui_response`, host tool results, `follow_up`).
    pub fn send(&self, frame: &Value) -> Result<()> {
        if self.inner.disposed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.write(frame)
    }

    fn write(&self, frame: &Value) -> Result<()> {
        let line = serde_json::to_string(frame)?;
        match self.inner.transport.write_line(&line) {
            Ok(()) => Ok(()),
            Err(error) => {
                // A child that died before we wrote surfaces as EPIPE on Linux before the reader
                // thread has recorded the exit. Prefer the exit message (code + stderr tail).
                if let Some(exit) = self.exit_after_broken_pipe(&error) {
                    self.close_with(&exit);
                    return Err(Error::Closed(exit));
                }
                let message = error.to_string();
                self.close_with(&format!(
                    "{} stdin write failed: {message}",
                    self.inner.name
                ));
                Err(error)
            }
        }
    }

    fn exit_after_broken_pipe(&self, error: &Error) -> Option<String> {
        let Error::Io(io) = error else {
            return None;
        };
        if io.kind() != std::io::ErrorKind::BrokenPipe {
            return None;
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(exit) = self.inner.ready.lock().unwrap().exit.clone() {
                return Some(exit);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// First `ready` frame, waiting up to `timeout`. Fails if the process dies first.
    pub fn wait_ready(&self, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let mut state = self.inner.ready.lock().unwrap();
        loop {
            if let Some(frame) = &state.frame {
                return Ok(frame.clone());
            }
            if let Some(exit) = &state.exit {
                return Err(Error::Closed(exit.clone()));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Protocol(format!(
                    "Timed out waiting for {} to become ready",
                    self.inner.name
                )));
            }
            state = self
                .inner
                .ready_cv
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }

    /// Fails pending requests and terminates the process.
    pub fn close(&self) {
        self.close_with(&format!("{} process is closed", self.inner.name));
    }

    pub fn close_with(&self, reason: &str) {
        if self.inner.disposed.load(Ordering::SeqCst) {
            return;
        }
        let reason = reason.to_string();
        self.inner.fail_all(|| Error::Closed(reason.clone()));
        self.inner.transport.close();
    }
}
