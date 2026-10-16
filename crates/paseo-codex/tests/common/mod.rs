//! Golden-traffic replay: a fake `codex app-server` driven by a recorded fixture.
//!
//! A fixture is the JSONL the tee wrapper wrote: per app-server process one `{argv}` header, then
//! `{dir:"in"|"out", line}` records, then `{exit}`. The replay plays the *server* side:
//!
//! - It emits consecutive recorded `out` lines until it reaches an `in` line, then waits for the
//!   client.
//! - A client message is matched to the next recorded `in` line with the same shape (request
//!   method / notification method / response). Recorded `in` lines the client never sends (the TS
//!   adapter probes `getUserSavedConfig`, which current Codex rejects) are skipped together with
//!   their responses, so the Rust client may issue a subset of the recorded traffic, in order.
//! - Recorded response ids are rewritten to the id the client actually used.
//! - Anything the client sends that has no recorded counterpart is a *violation*; tests assert
//!   there are none.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use paseo_codex::error::{Error, Result};
use paseo_codex::transport::{
    BoxFuture, ExitInfo, Transport, TransportControl, TransportEvent, TransportFactory, TransportIo,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq)]
pub enum Dir {
    In,
    Out,
}

#[derive(Debug, Clone)]
pub struct Rec {
    pub dir: Dir,
    pub line: Value,
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub argv: Vec<String>,
    pub recs: Vec<Rec>,
}

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.jsonl"))
}

pub fn load_fixture(name: &str) -> Vec<Proc> {
    let text = std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let mut procs: Vec<Proc> = Vec::new();
    for raw in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(raw).expect("fixture line is JSON");
        if let Some(argv) = v.get("argv") {
            procs.push(Proc {
                argv: serde_json::from_value(argv.clone()).expect("argv"),
                recs: Vec::new(),
            });
        } else if v.get("exit").is_some() {
            // end-of-process marker
        } else if let (Some(dir), Some(line)) =
            (v.get("dir").and_then(Value::as_str), v.get("line"))
        {
            let dir = if dir == "in" { Dir::In } else { Dir::Out };
            procs
                .last_mut()
                .expect("record before header")
                .recs
                .push(Rec {
                    dir,
                    line: line.clone(),
                });
        }
    }
    procs
}

/// Every recorded JSON frame of a fixture, for conformance tests.
pub fn fixture_frames(name: &str) -> Vec<(Dir, Value)> {
    load_fixture(name)
        .into_iter()
        .flat_map(|p| p.recs.into_iter().map(|r| (r.dir, r.line)))
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Request(String),
    Notification(String),
    Response,
}

fn shape(v: &Value) -> Option<Shape> {
    let has_id = v.get("id").is_some();
    match v.get("method").and_then(Value::as_str) {
        Some(m) if has_id => Some(Shape::Request(m.to_string())),
        Some(m) => Some(Shape::Notification(m.to_string())),
        None if has_id && (v.get("result").is_some() || v.get("error").is_some()) => {
            Some(Shape::Response)
        }
        None => None,
    }
}

#[derive(Default)]
pub struct Observed {
    /// Every line the client sent, per process, in order.
    pub sent: Vec<Vec<Value>>,
    pub violations: Vec<String>,
    pub opened_args: Vec<Vec<String>>,
}

pub struct ReplayFactory {
    procs: Mutex<VecDeque<Proc>>,
    version: String,
    pub observed: Arc<Mutex<Observed>>,
}

impl ReplayFactory {
    pub fn new(fixture: &str) -> Arc<ReplayFactory> {
        Self::with_version(fixture, "codex-cli 0.160.0")
    }

    pub fn with_version(fixture: &str, version: &str) -> Arc<ReplayFactory> {
        Arc::new(ReplayFactory {
            procs: Mutex::new(load_fixture(fixture).into()),
            version: version.to_string(),
            observed: Arc::new(Mutex::new(Observed::default())),
        })
    }

    pub fn violations(&self) -> Vec<String> {
        self.observed.lock().unwrap().violations.clone()
    }

    /// Lines the client sent to process `n` whose `method` is `method`.
    pub fn sent(&self, process: usize, method: &str) -> Vec<Value> {
        self.observed
            .lock()
            .unwrap()
            .sent
            .get(process)
            .map(|lines| {
                lines
                    .iter()
                    .filter(|l| l.get("method").and_then(Value::as_str) == Some(method))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Response lines (answers to server requests) the client sent to process `n`.
    pub fn sent_responses(&self, process: usize) -> Vec<Value> {
        self.observed
            .lock()
            .unwrap()
            .sent
            .get(process)
            .map(|lines| {
                lines
                    .iter()
                    .filter(|l| l.get("method").is_none())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl TransportFactory for ReplayFactory {
    fn open(&self, args: &[String]) -> Result<Box<dyn Transport>> {
        let proc = self
            .procs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| Error::Invalid("replay: no recorded process left".into()))?;
        let mut obs = self.observed.lock().unwrap();
        let index = obs.sent.len();
        obs.sent.push(Vec::new());
        obs.opened_args.push(args.to_vec());
        if proc.argv != args {
            obs.violations.push(format!(
                "process {index}: opened with {args:?}, recorded {:?}",
                proc.argv
            ));
        }
        Ok(Box::new(ReplayTransport {
            proc,
            index,
            observed: self.observed.clone(),
        }))
    }

    fn version(&self) -> BoxFuture<Result<String>> {
        let v = self.version.clone();
        Box::pin(async move { Ok(v) })
    }
}

struct ReplayTransport {
    proc: Proc,
    index: usize,
    observed: Arc<Mutex<Observed>>,
}

struct ReplayControl {
    stop: Mutex<Option<oneshot::Sender<()>>>,
}

impl TransportControl for ReplayControl {
    fn terminate(&self) -> BoxFuture<()> {
        if let Some(tx) = self.stop.lock().unwrap().take() {
            let _ = tx.send(());
        }
        Box::pin(async {})
    }
}

impl Transport for ReplayTransport {
    fn start(self: Box<Self>) -> Result<TransportIo> {
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<TransportEvent>();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let ReplayTransport {
            proc,
            index,
            observed,
        } = *self;

        tokio::spawn(async move {
            let recs = proc.recs;
            let mut cursor = 0usize;
            let mut id_map: HashMap<i64, i64> = HashMap::new();
            let emit = |v: &Value| {
                let _ = in_tx.send(TransportEvent::Line(v.to_string()));
            };
            let pump = |cursor: &mut usize, id_map: &HashMap<i64, i64>| {
                while *cursor < recs.len() && recs[*cursor].dir == Dir::Out {
                    let mut line = recs[*cursor].line.clone();
                    *cursor += 1;
                    if shape(&line) == Some(Shape::Response) {
                        let recorded = line.get("id").and_then(Value::as_i64);
                        match recorded.and_then(|r| id_map.get(&r)) {
                            Some(mine) => line["id"] = json!(mine),
                            None => continue, // answer to a request the client never made
                        }
                    }
                    emit(&line);
                }
            };
            pump(&mut cursor, &id_map);
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    msg = out_rx.recv() => {
                        let Some(msg) = msg else { break };
                        let v: Value = match serde_json::from_str(&msg) {
                            Ok(v) => v,
                            Err(e) => {
                                observed.lock().unwrap().violations.push(format!("client sent non-JSON: {e}: {msg}"));
                                continue;
                            }
                        };
                        observed.lock().unwrap().sent[index].push(v.clone());
                        let Some(want) = shape(&v) else {
                            observed.lock().unwrap().violations.push(format!("client sent unclassifiable frame: {v}"));
                            continue;
                        };
                        let found = (cursor..recs.len()).find(|&j| recs[j].dir == Dir::In && shape(&recs[j].line).as_ref() == Some(&want));
                        match found {
                            None => {
                                observed.lock().unwrap().violations.push(format!("no recorded counterpart for client frame: {v}"));
                                if let Shape::Request(_) = want {
                                    emit(&json!({"id": v["id"], "error": {"code": -32601, "message": "replay: unrecorded request"}}));
                                }
                            }
                            Some(j) => {
                                // contemporaneous notifications between the cursor and the match
                                for rec in &recs[cursor..j] {
                                    if rec.dir == Dir::Out && matches!(shape(&rec.line), Some(Shape::Notification(_))) {
                                        emit(&rec.line);
                                    }
                                }
                                if let Shape::Request(_) = want {
                                    if let (Some(rec_id), Some(my_id)) = (recs[j].line.get("id").and_then(Value::as_i64), v.get("id").and_then(Value::as_i64)) {
                                        id_map.insert(rec_id, my_id);
                                    }
                                }
                                cursor = j + 1;
                                pump(&mut cursor, &id_map);
                            }
                        }
                    }
                }
            }
            let _ = in_tx.send(TransportEvent::Exited(ExitInfo {
                status: "replay finished".into(),
                stderr_tail: String::new(),
            }));
        });

        Ok(TransportIo {
            outgoing: out_tx,
            incoming: in_rx,
            control: Arc::new(ReplayControl {
                stop: Mutex::new(Some(stop_tx)),
            }),
        })
    }
}
