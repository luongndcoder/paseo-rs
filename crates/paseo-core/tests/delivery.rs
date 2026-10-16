//! Port of `owned-subscriptions/index.test.ts` and the delivery-level cases of
//! `owned-subscriptions.e2e.test.ts` (source retention, close with a paused request, failed
//! teardown, ownership of replies and emissions, legacy versus modern sources).
//!
//! `AsyncLocalStorage` cases ("concurrent requests keep their source", "an asynchronous projection
//! keeps its source after close", the memory-retention fixture) have no Rust equivalent: the
//! request context is an explicit `ReqCtx`, so the source cannot leak between tasks. The first test
//! covers the observable half (two interleaved requests reply to their own sources).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use paseo_core::delivery::{
    conventional_response_type, is_reply, stop_async, stop_sync, Delivery, DeliveryError,
    DeliveryHooks, Frame, Outbound, OutboundSink, ProtocolFailure, ProtocolFailureCode,
    RequestInfo, SourceGate, SourceId,
};
use serde_json::{json, Value};
use tokio::sync::Notify;

/// The edge's writer in miniature: enforces the gate, records what a socket would send.
#[derive(Default)]
struct Wire {
    gates: Mutex<HashMap<SourceId, Arc<SourceGate>>>,
    sent: Mutex<Vec<(SourceId, Value)>>,
    binary: Mutex<Vec<(SourceId, Vec<u8>)>>,
    dropped: AtomicUsize,
}

impl Wire {
    fn register(&self, gate: Arc<SourceGate>) {
        self.gates.lock().unwrap().insert(gate.id(), gate);
    }

    fn messages(&self, source: SourceId) -> Vec<Value> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(s, _)| *s == source)
            .map(|(_, m)| m.clone())
            .collect()
    }

    fn types(&self, source: SourceId) -> Vec<String> {
        self.messages(source)
            .iter()
            .map(|m| m["type"].as_str().unwrap().to_owned())
            .collect()
    }

    fn all_types(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m)| m["type"].as_str().unwrap().to_owned())
            .collect()
    }
}

impl OutboundSink for Wire {
    fn send(&self, out: Outbound) {
        let gate = self.gates.lock().unwrap().get(&out.to).cloned();
        let Some(gate) = gate else {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            return;
        };
        if !gate.permits(&out) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            return;
        }
        match &out.body {
            Frame::Text(text) => {
                let envelope: Value = serde_json::from_str(text).unwrap();
                assert_eq!(envelope["type"], "session");
                self.sent
                    .lock()
                    .unwrap()
                    .push((out.to, envelope["message"].clone()));
            }
            Frame::Binary(bytes) => self.binary.lock().unwrap().push((out.to, bytes.to_vec())),
        }
    }
}

fn setup() -> (Delivery, Arc<Wire>) {
    let wire = Arc::new(Wire::default());
    (Delivery::new(wire.clone()), wire)
}

fn attach(delivery: &Delivery, wire: &Wire, id: u64, modern: bool) -> SourceId {
    let source = SourceId(id);
    wire.register(delivery.attach(source, modern).unwrap());
    source
}

fn ping(request_id: &str) -> RequestInfo {
    RequestInfo::new("ping", Some(request_id))
}

fn pong(request_id: &str) -> Value {
    json!({
        "type": "pong",
        "payload": { "requestId": request_id, "clientSentAt": 1, "serverReceivedAt": 1, "serverSentAt": 1 },
    })
}

#[tokio::test]
async fn interleaved_requests_reply_to_their_own_sources() {
    let (delivery, wire) = setup();
    let a = attach(&delivery, &wire, 1, true);
    let b = attach(&delivery, &wire, 2, true);
    let mut tasks = Vec::new();
    for (source, id) in [(a, "0"), (b, "1")] {
        let delivery = delivery.clone();
        tasks.push(tokio::spawn(async move {
            delivery
                .request(Some(source), ping(id), |ctx| async move {
                    tokio::task::yield_now().await;
                    assert_eq!(ctx.source(), source);
                    assert!(ctx.reply(pong(id)));
                })
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(wire.messages(a).len(), 1);
    assert_eq!(wire.messages(a)[0]["payload"]["requestId"], "0");
    assert_eq!(wire.messages(b)[0]["payload"]["requestId"], "1");
    delivery.close().await.unwrap();
}

#[tokio::test]
async fn close_finishes_with_a_paused_request_and_late_work_sees_cancellation_without_leaking_a_reply(
) {
    let wire = Arc::new(Wire::default());
    let delivery = Delivery::new(wire.clone());
    let other_wire = Arc::new(Wire::default());
    let other = Delivery::new(other_wire.clone());
    let source = attach(&delivery, &wire, 1, true);
    let other_source = attach(&other, &other_wire, 2, true);
    let resume = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let request = {
        let delivery = delivery.clone();
        let resume = resume.clone();
        let stopped = stopped.clone();
        let entered = Arc::new(Notify::new());
        let entered_signal = entered.clone();
        let task = tokio::spawn(async move {
            delivery
                .request(Some(source), ping("pending"), |ctx| async move {
                    ctx.begin("test", None, None, move |_id| async move {
                        stopped.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap();
                    entered_signal.notify_one();
                    resume.notified().await;
                    assert!(ctx.signal().is_aborted());
                    // The request is still a reply target, but nothing reaches the wire.
                    assert!(ctx.reply(pong("pending")));
                })
                .await
                .unwrap();
        });
        entered.notified().await;
        task
    };
    // Close must not wait for the paused request.
    tokio::time::timeout(Duration::from_secs(2), delivery.close())
        .await
        .expect("close is not blocked by the paused request")
        .unwrap();
    resume.notify_one();
    request.await.unwrap();
    assert!(stopped.load(Ordering::SeqCst));
    assert_eq!(delivery.registration_count(), 0);
    other
        .request(Some(other_source), ping("other"), |ctx| async move {
            assert!(ctx.reply(pong("other")));
        })
        .await
        .unwrap();
    assert_eq!(wire.all_types(), Vec::<String>::new());
    assert_eq!(other_wire.all_types(), vec!["pong"]);
    other.close().await.unwrap();
}

#[tokio::test]
async fn a_failed_in_flight_request_does_not_fail_delivery_teardown() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let resume = Arc::new(Notify::new());
    let entered = Arc::new(Notify::new());
    let request = {
        let delivery = delivery.clone();
        let resume = resume.clone();
        let entered = entered.clone();
        tokio::spawn(async move {
            delivery
                .request(Some(source), ping("failed"), |_ctx| async move {
                    entered.notify_one();
                    resume.notified().await;
                    Err::<(), String>("request canceled".into())
                })
                .await
        })
    };
    entered.notified().await;
    let closing = delivery.close();
    resume.notify_one();
    let outcome = request.await.unwrap().unwrap();
    assert_eq!(outcome, Err("request canceled".to_owned()));
    assert_eq!(closing.await, Ok(()));
}

#[tokio::test]
async fn failed_teardown_still_closes_the_delivery_and_rejects_late_work() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    delivery
        .request(Some(source), ping("subscribe"), |ctx| async move {
            ctx.begin("test", None, None, |_id| async move {
                Err("teardown failed".to_owned())
            })
            .unwrap();
        })
        .await
        .unwrap();
    let expected = Err(DeliveryError::Teardown("teardown failed".into()));
    assert_eq!(delivery.close().await, expected);
    assert_eq!(delivery.close().await, expected);
    assert_eq!(
        delivery.attach(SourceId(9), true).err(),
        Some(DeliveryError::Closed)
    );
    assert_eq!(
        delivery.open_request(Some(source), ping("late")).err(),
        Some(DeliveryError::Closed)
    );
    assert_eq!(
        delivery
            .request(Some(source), ping("late"), |_| async {})
            .await
            .err(),
        Some(DeliveryError::Closed)
    );
}

#[tokio::test]
async fn a_failed_teardown_stays_registered_and_is_reported_again() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let sub = delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("r1")),
            |ctx| async move {
                ctx.begin("agents", None, None, |_| async { Err("boom".to_owned()) })
                    .unwrap()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        sub.release().await,
        Err(DeliveryError::Teardown("boom".into()))
    );
    // Memoized: the same error again, and the registration is still there for `detach`.
    assert_eq!(
        sub.release().await,
        Err(DeliveryError::Teardown("boom".into()))
    );
    assert_eq!(delivery.source_registration_count(source), 1);
    assert_eq!(
        delivery.detach(source).await,
        Err(DeliveryError::Teardown("boom".into()))
    );
}

#[tokio::test]
async fn release_stops_once_and_forgets_on_success() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let stops = Arc::new(AtomicUsize::new(0));
    let sub = {
        let stops = stops.clone();
        delivery
            .request(
                Some(source),
                RequestInfo::new("fetch_agents_request", Some("r1")),
                |ctx| async move {
                    ctx.begin("agents", None, None, move |_| async move {
                        stops.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                    .unwrap()
                },
            )
            .await
            .unwrap()
    };
    assert!(delivery.has_demand("agents"));
    let (first, second) = tokio::join!(sub.release(), sub.release());
    assert_eq!((first, second), (Ok(()), Ok(())));
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(delivery.registration_count(), 0);
    assert!(!delivery.has_demand("agents"));
    // Emissions after release go nowhere.
    sub.emit(json!({"type": "agent_update", "payload": {}}));
    assert!(wire.messages(source).is_empty());
}

#[tokio::test]
async fn identical_observations_get_server_ids_and_release_independently() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let (one, two) = delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("r1")),
            |ctx| async move {
                let one = ctx
                    .begin("agents", None, None, |_| async { Ok(()) })
                    .unwrap();
                let two = ctx
                    .begin("agents", None, None, |_| async { Ok(()) })
                    .unwrap();
                (one, two)
            },
        )
        .await
        .unwrap();
    assert_ne!(one.id(), two.id());
    assert_eq!(one.id(), one.response_id());
    one.emit(json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": "a"}}));
    two.emit(json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": "a"}}));
    let ids: Vec<String> = wire
        .messages(source)
        .iter()
        .map(|m| m["payload"]["subscriptionId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec![one.id().to_owned(), two.id().to_owned()]);
    one.release().await.unwrap();
    two.emit(json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": "b"}}));
    one.emit(json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": "c"}}));
    assert_eq!(
        wire.messages(source).len(),
        3,
        "only the surviving observer emits"
    );
    assert_eq!(
        delivery.subscription_ids(source, "agents"),
        vec![two.id().to_owned()]
    );
}

#[tokio::test]
async fn modern_sources_get_host_ids_and_need_a_request_id() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("r1")),
            |ctx| async move {
                assert_eq!(
                    ctx.begin("agents", Some("client-id"), None, |_| async { Ok(()) })
                        .err(),
                    Some(DeliveryError::HostAssignedId)
                );
            },
        )
        .await
        .unwrap();
    delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", None),
            |ctx| async move {
                assert_eq!(
                    ctx.begin("agents", None, None, |_| async { Ok(()) }).err(),
                    Some(DeliveryError::RequestIdRequired)
                );
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_subscription_cannot_open_from_an_ended_request() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let ctx = delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("r1")),
            |ctx| async move { ctx },
        )
        .await
        .unwrap();
    assert!(!ctx.is_active());
    assert_eq!(
        ctx.begin("agents", None, None, |_| async { Ok(()) }).err(),
        Some(DeliveryError::SubscriptionSourceClosed)
    );
    // And an ended request no longer owns replies.
    ctx.reply(pong("r1"));
    assert!(wire.messages(source).is_empty());
}

#[tokio::test]
async fn legacy_sources_echo_their_own_id_and_replace_within_a_slot() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, false);
    let first = delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("r1")),
            |ctx| async move {
                ctx.begin("agents", Some(" legacy-1 "), None, |_| async { Ok(()) })
                    .unwrap()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.response_id(), "legacy-1");
    assert_ne!(first.id(), "legacy-1");
    let second = delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("r2")),
            |ctx| async move {
                ctx.begin("agents", Some("legacy-2"), None, |_| async { Ok(()) })
                    .unwrap()
            },
        )
        .await
        .unwrap();
    // The new subscription released the previous one in the same slot.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        delivery.subscription_ids(source, "agents"),
        vec![second.id().to_owned()]
    );
    // Legacy sources release by their own id.
    delivery
        .request(
            Some(source),
            RequestInfo::new("subscription.release.request", Some("r3")),
            |ctx| async move { ctx.release("legacy-2").await.unwrap() },
        )
        .await
        .unwrap();
    assert!(delivery.subscription_ids(source, "agents").is_empty());
}

#[tokio::test]
async fn legacy_emissions_drop_the_subscription_id_except_for_file_and_diff_updates() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, false);
    let sub = delivery
        .request(
            Some(source),
            RequestInfo::new("subscribe_terminal_request", Some("r1")),
            |ctx| async move {
                ctx.begin("terminal", Some("t1"), None, |_| async { Ok(()) })
                    .unwrap()
            },
        )
        .await
        .unwrap();
    sub.emit(json!({
        "type": "terminals_changed",
        "payload": { "cwd": "/x", "workspaceId": "w", "terminals": [] },
    }));
    sub.emit(json!({
        "type": "subscribe_terminal_response",
        "payload": { "requestId": "r1", "subscriptionId": "t1", "terminalId": "x" },
    }));
    sub.emit(json!({"type": "fs.file.update", "payload": { "path": "a" }}));
    sub.emit(json!({"type": "checkout_diff_update", "payload": { "cwd": "a" }}));
    sub.emit(json!({"type": "agent_update", "payload": { "kind": "remove", "agentId": "a" }}));
    let messages = wire.messages(source);
    assert!(messages[0]["payload"].get("workspaceId").is_none());
    assert!(messages[1]["payload"].get("subscriptionId").is_none());
    assert_eq!(messages[2]["payload"]["subscriptionId"], "t1");
    assert_eq!(messages[3]["payload"]["subscriptionId"], "t1");
    assert!(messages[4]["payload"].get("subscriptionId").is_none());
}

#[tokio::test]
async fn a_legacy_socket_cannot_borrow_a_modern_siblings_observation() {
    let (delivery, wire) = setup();
    let modern = attach(&delivery, &wire, 1, true);
    let legacy = attach(&delivery, &wire, 2, false);
    let sub = delivery
        .request(
            Some(modern),
            RequestInfo::new("fetch_agents_request", Some("r1")),
            |ctx| async move {
                ctx.begin("agents", None, None, |_| async { Ok(()) })
                    .unwrap()
            },
        )
        .await
        .unwrap();
    sub.emit(json!({"type": "agent_update", "payload": {"kind": "remove", "agentId": "a"}}));
    assert_eq!(wire.types(modern), vec!["agent_update"]);
    assert!(wire.messages(legacy).is_empty());
    // An unowned broadcast reaches the legacy socket and is denied to the modern one.
    let broadcast = json!({"type": "activity_log", "payload": {"id": "1"}});
    delivery.emit_unowned(modern, broadcast.clone());
    delivery.emit_unowned(legacy, broadcast);
    assert_eq!(wire.types(modern), vec!["agent_update"]);
    assert_eq!(wire.types(legacy), vec!["activity_log"]);
    assert!(delivery.has_legacy_sources());
}

#[tokio::test]
async fn a_pure_reply_belongs_only_to_its_source_in_a_shared_session() {
    let (delivery, wire) = setup();
    let a = attach(&delivery, &wire, 1, true);
    let b = attach(&delivery, &wire, 2, true);
    delivery
        .request(
            Some(a),
            RequestInfo::new("workspace.label.list.request", Some("pure")),
            |ctx| async move {
                assert!(ctx.reply(json!({
                    "type": "workspace.label.list.response",
                    "payload": { "requestId": "pure", "labels": [] },
                })));
            },
        )
        .await
        .unwrap();
    assert_eq!(wire.types(a), vec!["workspace.label.list.response"]);
    assert!(wire.messages(b).is_empty());
}

#[tokio::test]
async fn a_modern_source_drops_unproven_frames_and_one_server_info() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let server_info =
        json!({"type": "status", "payload": {"status": "server_info", "serverId": "s"}});
    delivery.emit_unowned(source, server_info.clone());
    delivery.emit_unowned(source, server_info);
    // The handshake bypass is one-shot.
    assert_eq!(wire.types(source), vec!["status"]);
    assert_eq!(wire.dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn status_shaped_replies_use_the_requests_provenance() {
    let wire = Arc::new(Wire::default());
    let unexpected = Arc::new(Mutex::new(Vec::new()));
    let hook = {
        let unexpected = unexpected.clone();
        Arc::new(move |request: &RequestInfo, message: &Value| {
            unexpected.lock().unwrap().push((
                request.ty.clone(),
                message["type"].as_str().unwrap().to_owned(),
            ));
        })
    };
    let delivery = Delivery::with_hooks(
        wire.clone(),
        DeliveryHooks {
            unexpected_reply: Some(hook),
            ..DeliveryHooks::default()
        },
    );
    let source = attach(&delivery, &wire, 1, true);
    let created = json!({
        "type": "status",
        "payload": { "status": "agent_created", "requestId": "c1", "agentId": "a" },
    });
    delivery
        .request(
            Some(source),
            RequestInfo::new("create_agent_request", Some("c1")),
            |ctx| {
                let created = created.clone();
                async move { assert!(ctx.reply(created)) }
            },
        )
        .await
        .unwrap();
    // The same status under a request that cannot produce it is not a reply.
    delivery
        .request(
            Some(source),
            RequestInfo::new("fetch_agents_request", Some("c1")),
            |ctx| {
                let created = created.clone();
                async move { assert!(!ctx.reply(created)) }
            },
        )
        .await
        .unwrap();
    assert_eq!(wire.types(source), vec!["status"]);
    assert_eq!(
        unexpected.lock().unwrap().clone(),
        vec![("fetch_agents_request".to_owned(), "status".to_owned())]
    );
}

#[tokio::test]
async fn archive_and_delete_replies_keep_their_historical_names() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    for (request, reply) in [
        ("archive_agent_request", "agent_archived"),
        ("delete_agent_request", "agent_deleted"),
    ] {
        delivery
            .request(
                Some(source),
                RequestInfo::new(request, Some("r")),
                |ctx| async move {
                    assert!(ctx.reply(json!({
                        "type": reply,
                        "payload": { "requestId": "r", "agentId": "a" },
                    })));
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(wire.types(source), vec!["agent_archived", "agent_deleted"]);
}

#[tokio::test]
async fn protocol_failure_is_a_source_only_control_reply() {
    let (delivery, wire) = setup();
    let a = attach(&delivery, &wire, 1, true);
    let b = attach(&delivery, &wire, 2, true);
    delivery.protocol_failure(
        a,
        &ProtocolFailure {
            request_id: Some("bad".into()),
            request_type: Some("fetch_agents_request".into()),
            error: "Invalid message".into(),
            code: ProtocolFailureCode::InvalidMessage,
        },
    );
    delivery.protocol_failure(
        a,
        &ProtocolFailure {
            request_id: None,
            request_type: None,
            error: "Unknown schema".into(),
            code: ProtocolFailureCode::UnknownSchema,
        },
    );
    let messages = wire.messages(a);
    assert_eq!(messages[0]["type"], "rpc_error");
    assert_eq!(messages[0]["payload"]["code"], "invalid_message");
    assert_eq!(messages[1]["type"], "status");
    assert_eq!(messages[1]["payload"]["message"], "Unknown schema");
    assert!(wire.messages(b).is_empty());
}

#[tokio::test]
async fn detach_cancels_and_releases_everything_a_source_owns() {
    let (delivery, wire) = setup();
    let source = attach(&delivery, &wire, 1, true);
    let stops = Arc::new(AtomicUsize::new(0));
    let (sub, op) = {
        let stops_sub = stops.clone();
        let stops_op = stops.clone();
        delivery
            .request(
                Some(source),
                RequestInfo::new("file.upload.request", Some("u1")),
                |ctx| async move {
                    let sub = ctx
                        .begin("files", None, None, move |_| async move {
                            stops_sub.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        })
                        .unwrap();
                    let op = ctx
                        .operation(
                            |m| m["type"] == "file.upload.response",
                            stop_sync(move || {
                                stops_op.fetch_add(1, Ordering::SeqCst);
                            }),
                        )
                        .unwrap();
                    (sub, op)
                },
            )
            .await
            .unwrap()
    };
    // An operation only emits what it accepts.
    op.emit(json!({"type": "file.upload.response", "payload": {"requestId": "u1"}}));
    op.emit(json!({"type": "activity_log", "payload": {}}));
    assert_eq!(wire.types(source), vec!["file.upload.response"]);
    assert_eq!(delivery.source_registration_count(source), 2);
    delivery.detach(source).await.unwrap();
    assert_eq!(stops.load(Ordering::SeqCst), 2);
    assert!(sub.signal().is_aborted() && op.signal().is_aborted());
    assert_eq!(delivery.registration_count(), 0);
    assert!(!delivery.has_source(source));
    sub.emit(json!({"type": "agent_update", "payload": {}}));
    assert_eq!(wire.types(source), vec!["file.upload.response"]);
}

#[tokio::test]
async fn binary_frames_need_a_proven_owner_on_modern_sources() {
    let (delivery, wire) = setup();
    let modern = attach(&delivery, &wire, 1, true);
    let legacy = attach(&delivery, &wire, 2, false);
    let sub = delivery
        .request(
            Some(modern),
            RequestInfo::new("subscribe_terminal_request", Some("r1")),
            |ctx| async move {
                ctx.begin("terminal", None, None, |_| async { Ok(()) })
                    .unwrap()
            },
        )
        .await
        .unwrap();
    sub.emit_binary(vec![1u8, 2, 3]);
    // A file explorer reply carries the request's proof; any other request does not.
    let writer = wire.clone();
    delivery
        .request(
            Some(modern),
            RequestInfo::new("file_explorer_request", Some("f1")),
            |ctx| async move {
                let owner = ctx.file_reply_owner().expect("file explorer request");
                writer.send(Outbound::binary(ctx.source(), Some(owner), vec![9u8]));
            },
        )
        .await
        .unwrap();
    delivery
        .request(
            Some(modern),
            RequestInfo::new("fetch_agents_request", Some("f2")),
            |ctx| async move { assert!(ctx.file_reply_owner().is_none()) },
        )
        .await
        .unwrap();
    // A legacy socket takes unowned binary frames; a modern one drops them.
    wire.send(Outbound::binary(legacy, None, vec![7u8]));
    wire.send(Outbound::binary(modern, None, vec![8u8]));
    let bytes: Vec<(u64, Vec<u8>)> = wire
        .binary
        .lock()
        .unwrap()
        .iter()
        .map(|(s, b)| (s.0, b.clone()))
        .collect();
    assert_eq!(bytes, vec![(1, vec![1, 2, 3]), (1, vec![9]), (2, vec![7])]);
}

// ---- isReply ----

#[test]
fn every_inbound_request_type_has_a_classified_reply() {
    // Naming is a convention, not proof: every inbound request type either has a conventional
    // response in the protocol or is listed in the exception tables.
    let outbound: std::collections::HashSet<&str> = paseo_authz::outbound_table()
        .iter()
        .map(|(ty, _)| *ty)
        .collect();
    let mut checked = 0;
    let mut without_conventional = Vec::new();
    for (ty, _) in paseo_authz::inbound_table() {
        let correlated = ty.ends_with(".request") || ty.ends_with("_request") || ty.contains('/');
        if !correlated {
            continue;
        }
        let request = RequestInfo::new(*ty, Some("r"));
        let response = conventional_response_type(ty);
        if outbound.contains(response.as_str()) {
            let reply = json!({"type": response, "payload": {"requestId": "r"}});
            assert!(is_reply(&request, &reply), "{ty} -> {response}");
            // A reply for another request id never matches.
            let other = json!({"type": response, "payload": {"requestId": "x"}});
            assert!(!is_reply(&request, &other));
            // The failure channel always answers its own request type only.
            let error =
                json!({"type": "rpc_error", "payload": {"requestId": "r", "requestType": ty}});
            assert!(is_reply(&request, &error));
            checked += 1;
        } else {
            without_conventional.push(*ty);
        }
    }
    assert!(checked > 100, "only {checked} request types were checked");
    // Requests whose answers use an exception name or a status envelope.
    let known_exceptions: &[&str] = &[
        "archive_agent_request",
        "delete_agent_request",
        "agent_permission_response",
        "subscribe_terminals_request",
        "usage.list_reports.request",
        "daemon.update.request",
        "create_agent_request",
        "import_agent_request",
        "resume_agent_request",
        "refresh_agent_request",
        "restart_server_request",
        "shutdown_server_request",
        // Fire-and-forget: the protocol has no response message for these.
        "abort_request",
        "unsubscribe_checkout_diff_request",
        "unsubscribe_terminal_request",
        "unsubscribe_terminals_request",
    ];
    let unexplained: Vec<&&str> = without_conventional
        .iter()
        .filter(|ty| !known_exceptions.contains(*ty))
        .collect();
    assert!(
        unexplained.is_empty(),
        "request types with neither a conventional response nor a declared exception: {unexplained:?}"
    );
}

#[test]
fn is_reply_follows_the_exception_tables() {
    let ping = RequestInfo::new("ping", Some("r"));
    assert!(is_reply(&ping, &pong("r")));
    assert!(!is_reply(
        &RequestInfo::new("fetch_agents_request", Some("r")),
        &pong("r")
    ));
    // No request id on the request: nothing correlates.
    assert!(!is_reply(&RequestInfo::new("ping", None), &pong("r")));
    // Voice transcription and checkout markers belong to no request.
    let transcription = json!({"type": "transcription_result", "payload": {"requestId": "r"}});
    assert!(!is_reply(&ping, &transcription));
    // Status replies are declared per status.
    let status = |s: &str| json!({"type": "status", "payload": {"status": s, "requestId": "r"}});
    assert!(is_reply(
        &RequestInfo::new("import_agent_request", Some("r")),
        &status("agent_resumed")
    ));
    assert!(is_reply(
        &RequestInfo::new("import_agent_request", Some("r")),
        &status("agent_create_failed")
    ));
    assert!(!is_reply(
        &RequestInfo::new("import_agent_request", Some("r")),
        &status("agent_created")
    ));
    assert!(!is_reply(
        &RequestInfo::new("ping", Some("r")),
        &status("unknown_status")
    ));
    // Slash-style request types answer `<type>/response`.
    assert!(is_reply(
        &RequestInfo::new("schedule/list", Some("r")),
        &json!({"type": "schedule/list/response", "payload": {"requestId": "r"}})
    ));
}

#[allow(dead_code)]
fn keep_imports_used() {
    let _ = stop_async(|| async { Ok::<(), String>(()) });
}
