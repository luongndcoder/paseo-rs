//! Ports of `agent-stream-coalescer.test.ts` onto the pure state machine with a fake clock.

use std::time::{Duration, Instant};

use paseo_agent_types::AgentStreamEvent;
use paseo_timeline::coalescer::{is_coalescable, Coalescer, Flush};
use serde_json::{json, Value};

struct Sim {
    c: Coalescer,
    base: Instant,
    clock: u64,
    flushes: Vec<Flush>,
}

impl Sim {
    fn new() -> Self {
        Self::with_window(60)
    }
    fn with_window(ms: u64) -> Self {
        Sim {
            c: Coalescer::new(Duration::from_millis(ms)),
            base: Instant::now(),
            clock: 0,
            flushes: Vec::new(),
        }
    }
    fn now(&self) -> Instant {
        self.base + Duration::from_millis(self.clock)
    }
    fn advance(&mut self, ms: u64) {
        let target = self.clock + ms;
        while let Some(d) = self.c.next_deadline() {
            if d > self.base + Duration::from_millis(target) {
                break;
            }
            let out = self.c.tick(d);
            self.flushes.extend(out);
        }
        self.clock = target;
    }
    fn handle(&mut self, agent: &str, event: &Value) -> bool {
        let ev = AgentStreamEvent::from_value(event).unwrap();
        let handled = is_coalescable(&ev);
        let out = self.c.handle(agent, &ev, self.now());
        self.flushes.extend(out);
        handled
    }
    fn prime(&mut self) {
        self.handle("agent-1", &assistant("prime", None, None, "codex"));
        self.flushes.clear();
    }
    fn items(&self) -> Vec<Value> {
        self.flushes.iter().map(|f| f.item.to_value()).collect()
    }
    fn texts(&self) -> Vec<String> {
        self.items()
            .iter()
            .map(|i| i["text"].as_str().unwrap_or("").to_string())
            .collect()
    }
}

fn timeline(item: Value, provider: &str, turn: Option<&str>) -> Value {
    let mut v = json!({"type": "timeline", "item": item, "provider": provider});
    if let Some(t) = turn {
        v["turnId"] = json!(t);
    }
    v
}
fn assistant(text: &str, message_id: Option<&str>, turn: Option<&str>, provider: &str) -> Value {
    let mut item = json!({"type": "assistant_message", "text": text});
    if let Some(m) = message_id {
        item["messageId"] = json!(m);
    }
    timeline(item, provider, turn)
}
fn a(text: &str) -> Value {
    assistant(text, None, None, "codex")
}
fn reasoning(text: &str) -> Value {
    timeline(json!({"type": "reasoning", "text": text}), "codex", None)
}
fn tool(call: &str, status: &str, output: &str) -> Value {
    timeline(
        json!({"type": "tool_call", "callId": call, "name": "shell", "status": status,
               "error": if status == "failed" { json!("failed") } else { Value::Null },
               "detail": {"type": "shell", "command": "printf ok", "output": output,
                          "exitCode": if status == "completed" { json!(0) } else { Value::Null }}}),
        "codex",
        None,
    )
}

#[test]
fn leading_edge_then_trailing_window() {
    let mut s = Sim::new();
    assert!(s.handle("agent-1", &a("hel")));
    assert_eq!(s.texts(), ["hel"]);
    s.flushes.clear();

    s.handle("agent-1", &a("hel"));
    s.handle("agent-1", &a("lo"));
    assert!(s.flushes.is_empty());
    s.advance(59);
    assert!(s.flushes.is_empty());
    s.advance(1);
    assert_eq!(s.texts(), ["hello"]);
}

#[test]
fn leads_again_after_idle_window() {
    let mut s = Sim::new();
    s.handle("agent-1", &a("first"));
    assert_eq!(s.flushes.len(), 1);
    s.advance(60);
    s.handle("agent-1", &a("second"));
    assert_eq!(s.texts(), ["first", "second"]);
}

#[test]
fn constructor_window_is_used() {
    let mut s = Sim::with_window(10);
    s.prime();
    s.handle("agent-1", &a("fast"));
    s.advance(9);
    assert!(s.flushes.is_empty());
    s.advance(1);
    assert_eq!(s.texts(), ["fast"]);
    assert_eq!(
        paseo_timeline::coalescer::DEFAULT_WINDOW,
        Duration::from_millis(60)
    );
}

#[test]
fn flush_for_drains_pending_events() {
    let mut s = Sim::new();
    s.handle("agent-1", &a("before"));
    let out = s.c.flush_for("agent-1", s.now());
    s.flushes.extend(out);
    assert_eq!(s.texts(), ["before"]);
    assert!(s.handle("agent-1", &tool("tool-1", "running", "")));
}

#[test]
fn does_not_consume_non_chunkable_events() {
    let mut s = Sim::new();
    let events = [
        timeline(
            json!({"type": "todo", "items": [{"text": "ship it", "completed": false}]}),
            "codex",
            None,
        ),
        timeline(
            json!({"type": "user_message", "text": "hi", "messageId": "message-1"}),
            "codex",
            None,
        ),
        timeline(json!({"type": "error", "message": "boom"}), "codex", None),
        timeline(
            json!({"type": "compaction", "status": "loading", "trigger": "auto"}),
            "codex",
            None,
        ),
        json!({"type": "thread_started", "provider": "codex", "sessionId": "session-1"}),
        json!({"type": "turn_started", "provider": "codex", "turnId": "turn-1"}),
        json!({"type": "turn_completed", "provider": "codex", "turnId": "turn-1"}),
        json!({"type": "turn_failed", "provider": "codex", "error": "failed", "turnId": "turn-1"}),
        json!({"type": "turn_canceled", "provider": "codex", "reason": "canceled", "turnId": "turn-1"}),
        json!({"type": "usage_updated", "provider": "codex", "usage": {"inputTokens": 1}, "turnId": "turn-1"}),
        json!({"type": "attention_required", "provider": "codex", "reason": "permission",
               "timestamp": "2026-04-18T00:00:00.000Z"}),
    ];
    for e in &events {
        assert!(!s.handle("agent-1", e), "{e}");
    }
    assert!(s.flushes.is_empty());
}

#[test]
fn kind_provider_turn_and_message_id_boundaries() {
    let mut s = Sim::new();
    s.prime();
    for e in [a("a1"), reasoning("r1"), a("a2"), a("a3"), reasoning("r2")] {
        s.handle("agent-1", &e);
    }
    s.advance(60);
    assert_eq!(s.texts(), ["a1", "r1", "a2a3", "r2"]);

    let mut s = Sim::new();
    s.prime();
    s.handle("agent-1", &assistant("a", None, None, "codex"));
    s.handle("agent-1", &assistant("b", None, None, "claude"));
    s.handle("agent-1", &assistant("c", None, Some("t1"), "claude"));
    s.handle("agent-1", &assistant("d", None, Some("t2"), "claude"));
    s.handle("agent-1", &assistant("e", Some("m1"), Some("t2"), "claude"));
    s.handle("agent-1", &assistant("f", Some("m2"), Some("t2"), "claude"));
    s.handle("agent-1", &assistant("g", Some("m2"), Some("t2"), "claude"));
    s.advance(60);
    assert_eq!(s.texts(), ["a", "b", "c", "d", "e", "fg"]);
    let f = &s.flushes[5];
    assert_eq!(
        (f.provider.as_str(), f.turn_id.as_deref()),
        ("claude", Some("t2"))
    );
    assert_eq!(f.item.to_value()["messageId"], "m2");
}

#[test]
fn drops_empty_text_and_preserves_bytes() {
    let mut s = Sim::new();
    assert!(s.handle("agent-1", &a("")));
    assert!(s.flushes.is_empty());
    s.prime();
    for chunk in [" ", "\n", "x ", "\t", " y\r\n"] {
        s.handle("agent-1", &a(chunk));
    }
    s.advance(60);
    assert_eq!(s.texts(), [" \nx \t y\r\n"]);
}

#[test]
fn buffers_are_isolated_per_agent_and_flush_all_covers_each_once() {
    let mut s = Sim::new();
    s.handle("a", &a("lead-a"));
    s.handle("b", &a("lead-b"));
    s.flushes.clear();
    s.handle("a", &a("a1"));
    s.handle("b", &a("b1"));
    s.handle("a", &a("a2"));
    let out = s.c.flush_all(s.now());
    s.flushes.extend(out);
    let by: Vec<(&str, String)> = s
        .flushes
        .iter()
        .map(|f| {
            (
                f.agent_id.as_str(),
                f.item.to_value()["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(by, [("a", "a1a2".to_string()), ("b", "b1".to_string())]);
    // Idempotent: nothing left, no stale timer fires a duplicate.
    let again = s.c.flush_for("a", s.now());
    assert!(again.is_empty());
    s.advance(500);
    assert_eq!(s.flushes.len(), 2);
}

#[test]
fn flush_and_discard_clears_timer_and_forgets_agent() {
    let mut s = Sim::new();
    s.prime();
    s.handle("agent-1", &a("x"));
    assert_eq!(s.c.pending_timers(), 1);
    let out = s.c.flush_and_discard("agent-1", s.now());
    s.flushes.extend(out);
    assert_eq!(s.texts(), ["x"]);
    assert_eq!(s.c.pending_timers(), 0);
    assert!(s.c.next_deadline().is_none());
    // A reused agent id starts as a leading edge again.
    s.flushes.clear();
    s.handle("agent-1", &a("y"));
    assert_eq!(s.texts(), ["y"]);
}

#[test]
fn tool_updates_merge_by_call_id_latest_wins() {
    let mut s = Sim::new();
    s.prime();
    s.handle("agent-1", &tool("tool-1", "running", "first"));
    s.handle("agent-1", &tool("tool-1", "running", "second"));
    s.advance(60);
    assert_eq!(s.items().len(), 1);
    assert_eq!(s.items()[0]["detail"]["output"], "second");
    assert_eq!(s.items()[0]["status"], "running");
}

#[test]
fn interleaved_tool_calls_merge_independently() {
    let mut s = Sim::new();
    s.prime();
    s.handle("agent-1", &tool("tool-1", "running", "one-a"));
    s.handle("agent-1", &tool("tool-2", "running", "two-a"));
    s.handle("agent-1", &tool("tool-1", "running", "one-b"));
    s.handle("agent-1", &tool("tool-2", "running", "two-b"));
    s.advance(60);
    let outs: Vec<_> = s
        .items()
        .iter()
        .map(|i| i["detail"]["output"].clone())
        .collect();
    assert_eq!(outs, [json!("one-b"), json!("two-b")]);
}

#[test]
fn terminal_tool_status_flushes_immediately() {
    let mut s = Sim::new();
    s.prime();
    s.handle("agent-1", &tool("tool-1", "running", "running"));
    assert!(s.flushes.is_empty());
    assert!(s.handle("agent-1", &tool("tool-1", "completed", "done")));
    assert_eq!(s.items().len(), 1);
    assert_eq!(s.items()[0]["status"], "completed");
    assert_eq!(s.items()[0]["detail"]["exitCode"], 0);
    assert_eq!(s.c.pending_timers(), 0);
}

#[test]
fn known_detail_survives_a_later_unknown_update() {
    let mut s = Sim::new();
    s.prime();
    s.handle(
        "agent-1",
        &timeline(
            json!({"type": "tool_call", "callId": "agent-call", "name": "Task", "status": "running", "error": null,
                   "detail": {"type": "sub_agent", "subAgentType": "general-purpose",
                              "description": "List files", "log": "", "actions": []}}),
            "codex",
            None,
        ),
    );
    s.handle(
        "agent-1",
        &timeline(
            json!({"type": "tool_call", "callId": "agent-call", "name": "Agent", "status": "completed", "error": null,
                   "detail": {"type": "unknown", "input": {"description": "List files"}, "output": null}}),
            "codex",
            None,
        ),
    );
    assert_eq!(s.items().len(), 1);
    assert_eq!(s.items()[0]["name"], "Agent");
    assert_eq!(s.items()[0]["detail"]["type"], "sub_agent");
    assert_eq!(s.items()[0]["detail"]["description"], "List files");
}

#[test]
fn mixed_text_and_tool_arrival_order_is_kept() {
    let mut s = Sim::new();
    s.prime();
    s.handle("agent-1", &a("a"));
    s.handle("agent-1", &tool("tool-1", "running", "running"));
    s.handle("agent-1", &reasoning("r"));
    s.handle("agent-1", &tool("tool-1", "running", "latest"));
    s.handle("agent-1", &a("b"));
    s.advance(60);
    let kinds: Vec<_> = s
        .items()
        .iter()
        .map(|i| i["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        [
            "assistant_message",
            "tool_call",
            "reasoning",
            "assistant_message"
        ]
    );
    assert_eq!(s.items()[1]["detail"]["output"], "latest");
}
