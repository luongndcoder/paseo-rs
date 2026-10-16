//! Replay of the WP0 coalescer traces (fake clock, timestamps = `at`).

mod common;

use std::time::{Duration, Instant};

use common::load_json;
use paseo_agent_types::AgentStreamEvent;
use paseo_timeline::coalescer::{is_coalescable, Coalescer, Flush};
use serde_json::{json, Value};

struct Harness {
    c: Coalescer,
    base: Instant,
    flushes: Vec<Value>,
}

impl Harness {
    fn at(&self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms)
    }

    fn record(&mut self, at_ms: u64, out: Vec<Flush>) {
        for f in out {
            let mut v = json!({
                "at": at_ms,
                "agentId": f.agent_id,
                "item": f.item.to_value(),
                "provider": f.provider,
            });
            if let Some(t) = f.turn_id {
                v["turnId"] = json!(t);
            }
            self.flushes.push(v);
        }
    }

    /// Advance the clock to `ms`, firing due timers earliest first.
    fn advance(&mut self, ms: u64) {
        let target = self.at(ms);
        while let Some(deadline) = self.c.next_deadline() {
            if deadline > target {
                break;
            }
            let at_ms = deadline.duration_since(self.base).as_millis() as u64;
            let out = self.c.tick(deadline);
            self.record(at_ms, out);
        }
    }
}

#[test]
fn coalescer_traces_match_ts() {
    let doc = load_json("coalescer-traces.json");
    let traces = doc["traces"].as_array().unwrap();
    assert_eq!(traces.len(), 12);
    for trace in traces {
        let name = trace["name"].as_str().unwrap();
        let mut h = Harness {
            c: Coalescer::new(Duration::from_millis(trace["windowMs"].as_u64().unwrap())),
            base: Instant::now(),
            flushes: Vec::new(),
        };
        for (i, op) in trace["ops"].as_array().unwrap().iter().enumerate() {
            let at = op["at"].as_u64().unwrap();
            h.advance(at);
            let now = h.at(at);
            match op["op"].as_str().unwrap() {
                "handle" => {
                    let event = AgentStreamEvent::from_value(&op["event"]).unwrap();
                    assert_eq!(
                        Value::Bool(is_coalescable(&event)),
                        op["handled"],
                        "{name} op {i} handled"
                    );
                    let out = h.c.handle(op["agent"].as_str().unwrap(), &event, now);
                    h.record(at, out);
                }
                "advance" => {}
                "flushFor" => {
                    let out = h.c.flush_for(op["agent"].as_str().unwrap(), now);
                    h.record(at, out);
                }
                "flushAll" => {
                    let out = h.c.flush_all(now);
                    h.record(at, out);
                }
                "flushAndDiscard" => {
                    let out = h.c.flush_and_discard(op["agent"].as_str().unwrap(), now);
                    h.record(at, out);
                }
                other => panic!("unknown op {other}"),
            }
        }
        assert_eq!(Value::Array(h.flushes), trace["flushes"], "{name} flushes");
        assert_eq!(
            json!(h.c.pending_timers()),
            trace["pendingTimersAtEnd"],
            "{name} pending timers"
        );
    }
}
