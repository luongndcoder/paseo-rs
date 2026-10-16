use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem};
use paseo_provider_map::opencode::{
    build_session_timeline, diagnostic_error_message, filter_reverted_messages,
    normalize_replay_timestamp, tool_part_to_item, translate_event, TranslationState,
};
use paseo_provider_map::testing::{load_mapper_vectors, load_provider_events, replay};
use paseo_provider_map::{mapper_for, CounterEnv, MapEnv, ProviderMapper};
use serde_json::{json, Value};

/// All opencode provider-event fixtures are `partial` (update-translation seam only) or `failed`
/// (catalog-only, no session): compare every `partial` one, and require that `failed` ones map to
/// nothing.
#[test]
fn opencode_goldens() {
    let mut compared = 0;
    for g in load_provider_events("opencode") {
        let mut mapper = mapper_for("opencode").unwrap();
        let actual = replay(mapper.as_mut(), &g.raw);
        match g.status.as_str() {
            "ok" | "partial" => {
                assert_eq!(actual, g.expected, "opencode/{} events differ", g.fixture);
                compared += 1;
            }
            _ => assert!(
                actual.is_empty(),
                "opencode/{}: failed fixture mapped to events",
                g.fixture
            ),
        }
    }
    assert!(
        compared >= 7,
        "expected >= 7 comparable fixtures, got {compared}"
    );
}

#[test]
fn opencode_tool_call_vectors() {
    use paseo_tool_detail::detail::opencode::{map_opencode_tool_call, OpencodeToolCallParams};
    let vectors = load_mapper_vectors("opencode");
    assert!(vectors.len() >= 22);
    for v in vectors {
        assert_eq!(v.func, "mapOpencodeToolCall");
        let i = &v.input;
        let key = |k: &str| i.get(k);
        // `...Undefined` keys mark arguments that were `undefined`; absent keys already are.
        let metadata = key("metadata").and_then(Value::as_object);
        let item = map_opencode_tool_call(&OpencodeToolCallParams {
            tool_name: i["toolName"].as_str().unwrap_or(""),
            call_id: key("callId").and_then(Value::as_str),
            status: key("status"),
            input: key("input"),
            output: key("output"),
            error: key("error"),
            metadata,
        });
        let actual = item.map(|t| t.to_value()).unwrap_or(Value::Null);
        assert_eq!(actual, v.output, "vector {}", v.name);
    }
}

/// The same vectors through the part-shaped entry point the live path uses.
#[test]
fn tool_part_parsing_matches_the_zod_union() {
    let part = |v: Value| tool_part_to_item(&v).map(|t| t.call_id);
    // callID wins over id; trimmed.
    assert_eq!(
        part(json!({"tool": "bash", "callID": " c1 ", "id": "p1", "state": {}})),
        Some("c1".into())
    );
    // blank callID falls back to id.
    assert_eq!(
        part(json!({"tool": "bash", "callID": "  ", "id": "p1"})),
        Some("p1".into())
    );
    // no ids -> parses but maps to nothing.
    assert_eq!(part(json!({"tool": "bash"})), None);
    // non-object state, null status and blank tool all fail the schema.
    assert_eq!(part(json!({"tool": "bash", "id": "p", "state": 3})), None);
    assert_eq!(
        part(json!({"tool": "bash", "id": "p", "state": {"status": null}})),
        None
    );
    assert_eq!(part(json!({"tool": "  ", "id": "p"})), None);
}

fn feed_all(events: &[Value]) -> Vec<Value> {
    let mut m = paseo_provider_map::opencode::OpencodeMapper::with_session("ses_p");
    let mut env = CounterEnv::new();
    let out: Vec<AgentStreamEvent> = events.iter().flat_map(|e| m.feed(e, &mut env)).collect();
    out.iter().map(AgentStreamEvent::to_value).collect()
}

#[test]
fn bare_global_events_are_accepted_and_other_sessions_ignored() {
    let out = feed_all(&[
        json!({"payload": {"type": "session.idle", "properties": {"sessionID": "ses_p"}}}),
        json!({"type": "session.idle", "properties": {"sessionID": "ses_other"}}),
        json!({"payload": {"type": "server.heartbeat", "properties": {}}}),
    ]);
    assert_eq!(
        out,
        vec![json!({"type": "turn_completed", "provider": "opencode"})]
    );
}

#[test]
fn deltas_then_completed_part_emit_only_the_unseen_suffix() {
    let out = feed_all(&[
        json!({"type":"message.updated","properties":{"info":{"id":"m1","sessionID":"ses_p","role":"assistant"}}}),
        json!({"type":"message.part.delta","properties":{"sessionID":"ses_p","messageID":"m1","partID":"p1","field":"text","delta":"po"}}),
        json!({"type":"message.part.updated","properties":{"part":{"id":"p1","sessionID":"ses_p","messageID":"m1","type":"text","text":"pong","time":{"end":5}}}}),
        // closed: further deltas and updates are dropped
        json!({"type":"message.part.delta","properties":{"sessionID":"ses_p","messageID":"m1","partID":"p1","field":"text","delta":"x"}}),
    ]);
    let texts: Vec<&str> = out
        .iter()
        .map(|e| e["item"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["po", "ng"]);
}

#[test]
fn task_tool_call_links_its_child_session() {
    let out = feed_all(&[
        json!({"type":"message.part.updated","properties":{"part":{
            "id":"p1","sessionID":"ses_p","messageID":"m1","type":"tool","tool":"task","callID":"c1",
            "state":{"status":"running","input":{"description":"look","subagent_type":"general"}}}}}),
        json!({"type":"session.created","properties":{"info":{
            "id":"ses_child","parentID":"ses_p","title":"look","agent":"general","directory":"/w"}}}),
    ]);
    let types: Vec<String> = out
        .iter()
        .map(|e| {
            format!(
                "{}:{}",
                e["type"].as_str().unwrap(),
                e["event"]["type"]
                    .as_str()
                    .or(e["item"]["type"].as_str())
                    .unwrap_or("")
            )
        })
        .collect();
    // task item, child detected, link presentation, re-emitted task item with childSessionId.
    assert_eq!(
        types,
        [
            "timeline:tool_call",
            "provider_subagent:upsert",
            "provider_subagent:upsert",
            "timeline:tool_call"
        ]
    );
    assert_eq!(out[1]["event"]["parentSubagentId"], Value::Null);
    assert_eq!(out[1]["event"]["status"], "running");
    assert_eq!(out[2]["event"]["toolCallId"], "c1");
    assert_eq!(out[2]["event"]["title"], "general");
    assert_eq!(out[3]["item"]["detail"]["childSessionId"], "ses_child");
    // The deletion of a tracked child removes it.
    let mut st = TranslationState::new("ses_p");
    let out = translate_event(
        &json!({"type":"session.deleted","properties":{"sessionID":"ses_p"}}),
        &mut st,
    );
    assert_eq!(out[0].to_value()["event"]["type"], "remove");
}

#[test]
fn session_error_variants() {
    let mut st = TranslationState::new("ses_p");
    let abort = translate_event(
        &json!({"type":"session.error","properties":{"sessionID":"ses_p","error":{"name":"MessageAbortedError"}}}),
        &mut st,
    );
    assert_eq!(abort[0].to_value()["reason"], "interrupted");
    let failed = translate_event(
        &json!({"type":"session.error","properties":{"sessionID":"ses_p","error":{"name":"X","data":{"message":"m"}}}}),
        &mut st,
    );
    assert_eq!(
        failed[0].to_value()["error"],
        r#"{"name":"X","data":{"message":"m"}}"#
    );
    assert_eq!(diagnostic_error_message(Some(&json!({}))), "Unknown error");
    assert_eq!(
        diagnostic_error_message(Some(&json!("  "))),
        "Unknown error"
    );
    assert_eq!(diagnostic_error_message(None), "Unknown error");
}

#[test]
fn replay_timestamps() {
    assert_eq!(
        normalize_replay_timestamp(Some(&json!(1_779_621_718_649u64))).as_deref(),
        Some("2026-05-24T11:21:58.649Z")
    );
    // seconds are scaled
    assert_eq!(
        normalize_replay_timestamp(Some(&json!(86_400))).as_deref(),
        Some("1970-01-02T00:00:00.000Z")
    );
    assert_eq!(normalize_replay_timestamp(Some(&json!("garbage"))), None);
    assert_eq!(normalize_replay_timestamp(Some(&json!(null))), None);
}

#[test]
fn history_conversion() {
    let messages = vec![
        json!({"info":{"id":"u1","role":"user","time":{"created":1_779_621_718_649u64}},
               "parts":[{"type":"text","text":"hi "},{"type":"text","text":"hidden","synthetic":true},{"type":"text","text":"there"}]}),
        json!({"info":{"id":"a1","role":"assistant","time":{"created":1_779_621_719_000u64}},
               "parts":[
                 {"type":"reasoning","text":"think","time":{"start":1_779_621_719_100u64}},
                 {"type":"tool","tool":"todowrite","callID":"t","state":{"input":{"todos":[{"content":"a","status":"in_progress"},{"content":"b"}]}}},
                 {"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls"},"output":"x"}},
                 {"type":"text","text":"done"}]}),
        // compaction: user message with a compaction part hides the next assistant message
        json!({"info":{"id":"u2","role":"user"},"parts":[{"type":"compaction","auto":true}]}),
        json!({"info":{"id":"a2","role":"assistant","summary":true},"parts":[{"type":"text","text":"summary"}]}),
        json!({"info":{"id":"a3","role":"assistant"},"parts":[{"type":"text","text":"after"}]}),
        // structured fallback
        json!({"info":{"id":"a4","role":"assistant","structured":{"k":1}},"parts":[]}),
    ];
    let items: Vec<Value> = build_session_timeline(&messages)
        .iter()
        .map(AgentTimelineItem::to_value)
        .collect();
    let kinds: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "user_message",
            "reasoning",
            "todo",
            "tool_call",
            "assistant_message",
            "compaction",
            "assistant_message",
            "assistant_message"
        ]
    );
    assert_eq!(items[0]["text"], "hi there");
    assert_eq!(items[2]["items"][0]["status"], "in_progress");
    assert_eq!(items[2]["items"][1]["status"], "pending");
    assert_eq!(items[5]["trigger"], "auto");
    assert_eq!(items[6]["text"], "after");
    assert_eq!(items[7]["text"], r#"{"k":1}"#);
    let a4 = paseo_provider_map::opencode::replay_timeline_events(&messages[5]);
    assert_eq!(a4[0].to_value()["item"]["text"], r#"{"k":1}"#);
}

#[test]
fn replay_events_carry_timestamps() {
    let m = json!({"info":{"id":"a","role":"assistant","time":{"created":1_779_621_718_649u64}},
                   "parts":[{"type":"text","text":"x","time":{"start":1_779_621_719_000u64}}]});
    let ev = paseo_provider_map::opencode::replay_timeline_events(&m);
    assert_eq!(ev[0].to_value()["timestamp"], "2026-05-24T11:21:59.000Z");
}

#[test]
fn reverted_messages_are_cut_at_the_revert_point() {
    let ms: Vec<Value> = ["a", "b", "c"]
        .iter()
        .map(|id| json!({"info":{"id":id}}))
        .collect();
    assert_eq!(
        filter_reverted_messages(&ms, Some(&json!({"messageID":"b"}))).len(),
        1
    );
    assert_eq!(
        filter_reverted_messages(&ms, Some(&json!({"messageID":"b","partID":"p"}))).len(),
        3
    );
    assert_eq!(filter_reverted_messages(&ms, None).len(), 3);
}

/// History conversion over the recorded resume fixture's `GET .../message` response must not
/// panic and must produce a non-empty timeline.
#[test]
fn history_from_recorded_resume_fixture() {
    let g = load_provider_events("opencode")
        .into_iter()
        .find(|g| g.fixture == "05-resume")
        .unwrap();
    let messages = g
        .raw
        .iter()
        .filter(|r| r["dir"] == "res")
        .filter_map(|r| r["body"].as_array())
        .find(|a| {
            a.first()
                .is_some_and(|m| m.get("info").is_some() && m.get("parts").is_some())
        });
    if let Some(messages) = messages {
        assert!(!build_session_timeline(messages).is_empty());
    }
    let _ = (
        CounterEnv::new().uuid(),
        mapper_for("opencode").map(|m| m.provider()),
    );
}

#[test]
fn replay_timestamp_extremes_match_date() {
    let ts = |v: Value| normalize_replay_timestamp(Some(&v));
    assert_eq!(ts(json!(2e14)).as_deref(), Some("8307-10-01T19:33:20.000Z"));
    assert_eq!(
        ts(json!(8.64e15)).as_deref(),
        Some("+275760-09-13T00:00:00.000Z")
    );
    assert_eq!(
        ts(json!(-1e11)).as_deref(),
        Some("-001199-02-15T14:13:20.000Z")
    );
    assert_eq!(ts(json!(8.65e15)), None);
}
