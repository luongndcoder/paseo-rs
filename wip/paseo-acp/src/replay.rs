//! Golden-traffic replay: a fake ACP agent driven by a recorded JSONL fixture
//! (`tools/capture/tee-acp.mjs` format). Used by `tests/replay.rs`, public so downstream
//! crates can replay the same fixtures.
//!
//! The agent is strictly sequential. After each client frame matches the next recorded `in`
//! entry, every following `out` entry is delivered up to the next `in` entry. A client frame
//! that does not match is recorded in the [`ReplayReport`] and the stream closes.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, watch, Notify};

use crate::error::{Error, Result};
use crate::launch::LaunchSpec;
use crate::transport::{
    ExitInfo, ProcessControl, Transport, TransportFactory, TransportHandle, WireEvent,
};
use crate::util::BoxFuture;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// Client -> agent.
    In(Value),
    /// Agent -> client.
    Out(Value),
    Exit(ExitInfo),
}

#[derive(Debug, Clone, Default)]
pub struct Fixture {
    pub name: String,
    pub header: Option<Value>,
    pub entries: Vec<Entry>,
    /// True for fixtures written by hand from TS source/tests rather than captured.
    pub synthetic: bool,
}

impl Fixture {
    pub fn from_jsonl(name: &str, text: &str) -> Result<Fixture> {
        let mut fixture = Fixture {
            name: name.to_string(),
            ..Fixture::default()
        };
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .map_err(|e| Error::Protocol(format!("{name}:{}: {e}", i + 1)))?;
            let Some(obj) = v.as_object() else {
                return Err(Error::Protocol(format!("{name}:{}: not an object", i + 1)));
            };
            if obj.get("synthetic").and_then(Value::as_bool) == Some(true) {
                fixture.synthetic = true;
                fixture.header = Some(v.clone());
            } else if obj.contains_key("argv") {
                fixture.header = Some(v.clone());
            } else if let Some(exit) = obj.get("exit") {
                fixture.entries.push(Entry::Exit(ExitInfo {
                    code: exit.get("code").and_then(Value::as_i64).map(|c| c as i32),
                    signal: exit.get("signal").and_then(Value::as_i64).map(|c| c as i32),
                }));
            } else {
                let dir = obj.get("dir").and_then(Value::as_str).unwrap_or("");
                let line = obj.get("line").cloned().unwrap_or(Value::Null);
                match dir {
                    "in" => fixture.entries.push(Entry::In(line)),
                    "out" => fixture.entries.push(Entry::Out(line)),
                    other => {
                        return Err(Error::Protocol(format!(
                            "{name}:{}: bad dir {other:?}",
                            i + 1
                        )))
                    }
                }
            }
        }
        Ok(fixture)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Fixture> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self::from_jsonl(&name, &text)
    }

    pub fn inbound(&self) -> impl Iterator<Item = &Value> {
        self.entries
            .iter()
            .filter_map(|e| if let Entry::In(v) = e { Some(v) } else { None })
    }

    pub fn outbound(&self) -> impl Iterator<Item = &Value> {
        self.entries
            .iter()
            .filter_map(|e| if let Entry::Out(v) = e { Some(v) } else { None })
    }
}

/// What happened during a replay; assert on it after the session closes.
#[derive(Debug, Default)]
pub struct ReplayReport {
    inner: Mutex<ReportInner>,
}

#[derive(Debug, Default)]
struct ReportInner {
    mismatches: Vec<String>,
    /// Client frames received, in order.
    received: Vec<Value>,
    unconsumed: usize,
    finished: bool,
}

impl ReplayReport {
    pub fn mismatches(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|i| i.mismatches.clone())
            .unwrap_or_default()
    }

    pub fn received(&self) -> Vec<Value> {
        self.inner
            .lock()
            .map(|i| i.received.clone())
            .unwrap_or_default()
    }

    /// Recorded client frames the client never sent (after the replay finished).
    pub fn unconsumed(&self) -> usize {
        self.inner.lock().map(|i| i.unconsumed).unwrap_or(0)
    }

    pub fn finished(&self) -> bool {
        self.inner.lock().map(|i| i.finished).unwrap_or(false)
    }
}

/// Keys whose values are random per run (client-generated ids) and compare by presence only.
pub const DEFAULT_MASKED_KEYS: &[&str] = &["messageId"];

pub fn frames_match(expected: &Value, actual: &Value, masked: &[String]) -> bool {
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            e.len() == a.len()
                && e.iter().all(|(k, ev)| match a.get(k) {
                    None => false,
                    Some(av) => masked.iter().any(|m| m == k) || frames_match(ev, av, masked),
                })
        }
        (Value::Array(e), Value::Array(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(x, y)| frames_match(x, y, masked))
        }
        (e, a) => e == a,
    }
}

pub struct ReplayTransport {
    fixture: Fixture,
    masked: Vec<String>,
    report: Arc<ReplayReport>,
}

impl ReplayTransport {
    pub fn new(fixture: Fixture) -> (Self, Arc<ReplayReport>) {
        let report = Arc::new(ReplayReport::default());
        let t = ReplayTransport {
            fixture,
            masked: DEFAULT_MASKED_KEYS.iter().map(|s| s.to_string()).collect(),
            report: report.clone(),
        };
        (t, report)
    }

    pub fn with_masked_keys(mut self, keys: &[&str]) -> Self {
        self.masked = keys.iter().map(|s| s.to_string()).collect();
        self
    }
}

struct ReplayControl {
    shutdown: Notify,
    exit_rx: watch::Receiver<Option<ExitInfo>>,
}

impl ProcessControl for ReplayControl {
    fn stderr(&self) -> String {
        String::new()
    }
    fn exit(&self) -> Option<ExitInfo> {
        *self.exit_rx.borrow()
    }
    fn wait_exit(&self, timeout: Duration) -> BoxFuture<'_, Option<ExitInfo>> {
        let mut rx = self.exit_rx.clone();
        Box::pin(async move {
            let wait = async {
                loop {
                    if let Some(i) = *rx.borrow_and_update() {
                        return Some(i);
                    }
                    if rx.changed().await.is_err() {
                        return *rx.borrow();
                    }
                }
            };
            tokio::time::timeout(timeout, wait).await.unwrap_or(None)
        })
    }
    fn terminate(&self, _graceful: Duration, _force: Duration) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.shutdown.notify_one();
            let _ = self.wait_exit(Duration::from_secs(1)).await;
        })
    }
}

impl Transport for ReplayTransport {
    fn start(self: Box<Self>) -> Result<TransportHandle> {
        let ReplayTransport {
            fixture,
            masked,
            report,
        } = *self;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<WireEvent>();
        let (exit_tx, exit_rx) = watch::channel(None);
        let control = Arc::new(ReplayControl {
            shutdown: Notify::new(),
            exit_rx,
        });
        let task_control = control.clone();
        let entries = fixture.entries;
        let name = fixture.name;

        tokio::spawn(async move {
            let mut cursor = 0usize;
            let mut recorded_exit = ExitInfo {
                code: Some(0),
                signal: None,
            };
            'outer: loop {
                // Deliver agent output until the next recorded client frame.
                while let Some(entry) = entries.get(cursor) {
                    match entry {
                        Entry::Out(v) => {
                            if in_tx.send(WireEvent::Frame(v.clone())).is_err() {
                                break 'outer;
                            }
                            cursor += 1;
                        }
                        Entry::Exit(info) => {
                            recorded_exit = *info;
                            cursor += 1;
                        }
                        Entry::In(_) => break,
                    }
                }
                tokio::select! {
                    frame = out_rx.recv() => {
                        let Some(frame) = frame else { break };
                        if let Ok(mut r) = report.inner.lock() {
                            r.received.push(frame.clone());
                        }
                        match entries.get(cursor) {
                            Some(Entry::In(expected)) if frames_match(expected, &frame, &masked) => cursor += 1,
                            other => {
                                let msg = format!(
                                    "{name}: client frame #{cursor} did not match.\n  got:      {frame}\n  expected: {}",
                                    match other {
                                        Some(Entry::In(e)) => e.to_string(),
                                        Some(_) => "<agent output>".into(),
                                        None => "<end of fixture>".into(),
                                    }
                                );
                                if let Ok(mut r) = report.inner.lock() {
                                    r.mismatches.push(msg);
                                }
                                break;
                            }
                        }
                    }
                    _ = task_control.shutdown.notified() => break,
                }
            }
            if let Ok(mut r) = report.inner.lock() {
                r.unconsumed = entries[cursor.min(entries.len())..]
                    .iter()
                    .filter(|e| matches!(e, Entry::In(_)))
                    .count();
                r.finished = true;
            }
            let _ = in_tx.send(WireEvent::Closed);
            let _ = exit_tx.send(Some(recorded_exit));
        });

        Ok(TransportHandle {
            outgoing: out_tx,
            incoming: in_rx,
            process: control,
        })
    }
}

/// Hands out one fixture per process the client spawns, in order.
pub struct ReplayFactory {
    queue: Mutex<std::collections::VecDeque<Fixture>>,
    reports: Mutex<Vec<Arc<ReplayReport>>>,
}

impl ReplayFactory {
    pub fn new(fixtures: Vec<Fixture>) -> Arc<Self> {
        Arc::new(ReplayFactory {
            queue: Mutex::new(fixtures.into()),
            reports: Mutex::new(Vec::new()),
        })
    }

    pub fn reports(&self) -> Vec<Arc<ReplayReport>> {
        self.reports.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl TransportFactory for ReplayFactory {
    fn open(&self, _spec: &LaunchSpec) -> Result<Box<dyn Transport>> {
        let fixture = self
            .queue
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
            .ok_or_else(|| {
                Error::Unsupported("replay: no fixture left for another agent process".into())
            })?;
        let (t, report) = ReplayTransport::new(fixture);
        if let Ok(mut r) = self.reports.lock() {
            r.push(report);
        }
        Ok(Box::new(t))
    }

    fn requires_binary(&self) -> bool {
        false
    }
}
