//! Replay of recorded traffic.
//!
//! A fixture is the JSONL log written by `tools/capture/tee-pi.mjs`: a header line, then
//! `{dir:"in"|"out", line}` entries (`in` = client to provider), then `{exit}`.
//!
//! [`ReplayTransport`] plays the provider side. An `out` frame is released once every `in` frame
//! logged before it has been written by the client. A written frame matches the first unconsumed
//! `in` frame equal to it, ignoring the request `id`; the client's own id is substituted into the
//! matching response, so a client that skips requests the recording contains (the TS adapter
//! polls `get_session_stats` constantly) still replays.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use serde_json::Value;

use crate::agent::Launcher;
use crate::error::{Error, Result};
use crate::launch::Options;
use crate::session::Session;
use crate::transport::{ExitInfo, Transport};

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Client to provider.
    In(Value),
    /// Provider to client.
    Out(Value),
    Exit(ExitInfo),
}

#[derive(Debug, Clone, Default)]
pub struct Fixture {
    pub header: Option<Value>,
    pub steps: Vec<Step>,
}

/// Frames that answer a provider request by their own `id` (never a client request id).
const ID_IS_PAYLOAD: [&str; 3] = [
    "extension_ui_response",
    "host_tool_result",
    "host_tool_update",
];

impl Fixture {
    pub fn parse(text: &str) -> Result<Self> {
        let mut fixture = Fixture::default();
        for (n, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
            let value: Value = serde_json::from_str(line)?;
            if let Some(dir) = value.get("dir").and_then(Value::as_str) {
                let frame = value.get("line").cloned().unwrap_or(Value::Null);
                fixture.steps.push(match dir {
                    "in" => Step::In(frame),
                    "out" => Step::Out(frame),
                    other => return Err(Error::Protocol(format!("line {n}: unknown dir {other}"))),
                });
            } else if let Some(exit) = value.get("exit") {
                fixture.steps.push(Step::Exit(ExitInfo {
                    code: exit.get("code").and_then(Value::as_i64).map(|c| c as i32),
                    signal: None,
                    stderr_tail: String::new(),
                }));
            } else if n == 0 {
                fixture.header = Some(value);
            } else {
                return Err(Error::Protocol(format!(
                    "line {n}: unrecognized fixture entry"
                )));
            }
        }
        Ok(fixture)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// argv the recorded process was started with (without the binary).
    pub fn argv(&self) -> Vec<String> {
        self.header
            .as_ref()
            .and_then(|h| h.get("argv"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every provider-to-client frame, in order.
    pub fn out_frames(&self) -> impl Iterator<Item = &Value> {
        self.steps.iter().filter_map(|s| match s {
            Step::Out(v) => Some(v),
            _ => None,
        })
    }

    /// Every client-to-provider frame, in order.
    pub fn in_frames(&self) -> impl Iterator<Item = &Value> {
        self.steps.iter().filter_map(|s| match s {
            Step::In(v) => Some(v),
            _ => None,
        })
    }

    /// Drops requests of the given command types and the responses that answered them.
    pub fn without_requests(mut self, types: &[&str]) -> Self {
        let mut dropped_ids = Vec::new();
        self.steps.retain(|step| match step {
            Step::In(frame) => {
                let ty = frame
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if types.contains(&ty) {
                    if let Some(id) = frame.get("id").and_then(Value::as_str) {
                        dropped_ids.push(id.to_string());
                    }
                    false
                } else {
                    true
                }
            }
            _ => true,
        });
        self.steps.retain(|step| match step {
            Step::Out(frame) => {
                let is_response = frame.get("type").and_then(Value::as_str) == Some("response");
                let id = frame.get("id").and_then(Value::as_str);
                !(is_response && id.is_some_and(|id| dropped_ids.iter().any(|d| d == id)))
            }
            _ => true,
        });
        self
    }
}

struct Slot {
    step: Step,
    consumed: bool,
}

struct State {
    slots: Vec<Slot>,
    /// Script request id -> the id this client used.
    id_map: HashMap<String, String>,
    closed: bool,
    has_exit: bool,
    mismatches: Vec<String>,
    writes: Vec<Value>,
}

pub struct ReplayTransport {
    state: Mutex<State>,
    changed: Condvar,
}

fn comparable(frame: &Value) -> Value {
    let ty = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut frame = frame.clone();
    if !ID_IS_PAYLOAD.contains(&ty) {
        if let Some(obj) = frame.as_object_mut() {
            obj.remove("id");
        }
    }
    frame
}

impl ReplayTransport {
    pub fn new(fixture: &Fixture) -> Self {
        Self {
            state: Mutex::new(State {
                slots: fixture
                    .steps
                    .iter()
                    .cloned()
                    .map(|step| Slot {
                        step,
                        consumed: false,
                    })
                    .collect(),
                id_map: HashMap::new(),
                closed: false,
                has_exit: fixture.steps.iter().any(|s| matches!(s, Step::Exit(_))),
                mismatches: Vec::new(),
                writes: Vec::new(),
            }),
            changed: Condvar::new(),
        }
    }

    /// Frames the client wrote that matched nothing in the recording.
    pub fn mismatches(&self) -> Vec<String> {
        self.state.lock().unwrap().mismatches.clone()
    }

    /// Every frame the client wrote, in order.
    pub fn writes(&self) -> Vec<Value> {
        self.state.lock().unwrap().writes.clone()
    }

    /// `in` frames the client never wrote.
    pub fn unsent(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .slots
            .iter()
            .filter(|s| !s.consumed)
            .filter_map(|s| match &s.step {
                Step::In(v) => Some(v.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Transport for ReplayTransport {
    fn write_line(&self, line: &str) -> Result<()> {
        let frame: Value = serde_json::from_str(line)?;
        let mut state = self.state.lock().unwrap();
        state.writes.push(frame.clone());
        let wanted = comparable(&frame);
        let found = state.slots.iter().position(|slot| {
            !slot.consumed && matches!(&slot.step, Step::In(v) if comparable(v) == wanted)
        });
        let result = match found {
            Some(i) => {
                if let Step::In(script) = &state.slots[i].step {
                    let script_id = script.get("id").and_then(Value::as_str).map(str::to_owned);
                    let own_id = frame.get("id").and_then(Value::as_str).map(str::to_owned);
                    let ty = frame
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if let (Some(script_id), Some(own_id)) = (script_id, own_id) {
                        if !ID_IS_PAYLOAD.contains(&ty) {
                            state.id_map.insert(script_id, own_id);
                        }
                    }
                }
                state.slots[i].consumed = true;
                Ok(())
            }
            None => {
                let message = format!("unexpected frame: {line}");
                state.mismatches.push(message.clone());
                Err(Error::Protocol(message))
            }
        };
        self.changed.notify_all();
        result
    }

    fn read_line(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.closed {
                return None;
            }
            let Some(i) = state.slots.iter().position(|s| !s.consumed) else {
                if state.has_exit {
                    return None;
                }
                // A recording without an exit models a process that stays alive and silent.
                state = self.changed.wait(state).unwrap();
                continue;
            };
            match state.slots[i].step.clone() {
                Step::Exit(_) => {
                    state.slots[i].consumed = true;
                    return None;
                }
                Step::Out(mut frame) => {
                    state.slots[i].consumed = true;
                    if frame.get("type").and_then(Value::as_str) == Some("response") {
                        let mapped = frame
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|id| state.id_map.get(id).cloned());
                        if let Some(own) = mapped {
                            frame["id"] = Value::String(own);
                        }
                    }
                    return Some(frame.to_string());
                }
                Step::In(_) => {
                    state = self.changed.wait(state).unwrap();
                }
            }
        }
    }

    fn exit_info(&self) -> ExitInfo {
        let state = self.state.lock().unwrap();
        state
            .slots
            .iter()
            .find_map(|s| match &s.step {
                Step::Exit(info) => Some(info.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

/// A [`Launcher`] that serves one fixture per launch, in order.
pub struct ReplayLauncher {
    fixtures: Mutex<VecDeque<Fixture>>,
    launches: Mutex<Vec<Vec<String>>>,
    transports: Mutex<Vec<Arc<ReplayTransport>>>,
}

impl ReplayLauncher {
    pub fn new(fixtures: impl IntoIterator<Item = Fixture>) -> Arc<Self> {
        Arc::new(Self {
            fixtures: Mutex::new(fixtures.into_iter().collect()),
            launches: Mutex::new(Vec::new()),
            transports: Mutex::new(Vec::new()),
        })
    }

    /// argv the client built for each launch.
    pub fn launches(&self) -> Vec<Vec<String>> {
        self.launches.lock().unwrap().clone()
    }

    pub fn transports(&self) -> Vec<Arc<ReplayTransport>> {
        self.transports.lock().unwrap().clone()
    }
}

impl Launcher for ReplayLauncher {
    fn start(&self, options: &Options) -> Result<Session> {
        self.launches
            .lock()
            .unwrap()
            .push(options.build_launch().argv);
        let fixture = self
            .fixtures
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Protocol("no replay fixture left for this launch".into()))?;
        let transport = Arc::new(ReplayTransport::new(&fixture));
        self.transports.lock().unwrap().push(Arc::clone(&transport));
        Session::from_transport(options, transport)
    }
}
