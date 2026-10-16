//! The session layer end to end over a real manager, `AgentStore` and registries: modern and
//! legacy clients on one event stream, owned subscriptions and their release, authorization,
//! the reconnect grace and TS-resident producers registered over `internal.delivery.*`.

mod common;
mod session_support;

const A: &str = common::AGENT_IDS[0];
const B: &str = common::AGENT_IDS[1];
const C: &str = common::AGENT_IDS[2];

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use paseo_agent_types::AgentStreamEvent;
use paseo_authz::PermSet;
use paseo_core::delivery::RequestInfo;
use paseo_core::mock::MockSession;
use paseo_core::session::caps::cap;
use paseo_core::session::registry::{RemoteBegin, TsRpc};
use paseo_core::{ManagedAgentView, RegisterOptions};
use paseo_provider_api::AgentSessionConfig;
use paseo_store::Nullable;
use serde_json::{json, Value};
use session_support::*;

async fn live_agent(env: &Env, id: &str, session: &MockSession) -> Arc<ManagedAgentView> {
    env.manager
        .register_session(
            Box::new(session.clone()),
            AgentSessionConfig::new("codex", "/work/repo"),
            id,
            RegisterOptions {
                workspace_id: Some("ws_1".into()),
                history_primed: Some(true),
                ..RegisterOptions::default()
            },
        )
        .await
        .expect("register_session")
}

fn fetch(request_id: &str, extra: Value) -> Value {
    let mut request = json!({
        "type": "fetch_agents_request",
        "requestId": request_id,
    });
    for (k, v) in extra.as_object().unwrap() {
        request[k] = v.clone();
    }
    request
}

fn updates(socket: &Socket) -> Vec<Value> {
    socket.frames_of("agent_update")
}

fn entry_ids(response: &Value) -> Vec<String> {
    response["payload"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["agent"]["id"].as_str().unwrap().to_owned())
        .collect()
}

// ---- fetch_agents + live updates ----

#[tokio::test]
async fn a_modern_subscription_gets_the_snapshot_then_tagged_live_updates() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let session = MockSession::new("codex");
    live_agent(&env, A, &session).await;
    let socket = env.connect("client-1", &modern()).await;

    socket.send(fetch("r1", json!({ "subscribe": {} }))).await;
    let response = socket.response("fetch_agents_response", "r1").await;
    let subscription_id = response["payload"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(entry_ids(&response), vec![A]);
    assert_eq!(
        response["payload"]["entries"][0]["project"]["projectName"],
        "repo"
    );
    assert_eq!(response["payload"]["pageInfo"]["hasMore"], false);

    session.push_event(common::turn_started("turn-1"));
    let frames = wait_for(|| {
        let u = updates(&socket);
        u.iter()
            .any(|m| m["payload"]["agent"]["status"] == "running")
            .then_some(u)
    })
    .await;
    let running = frames
        .iter()
        .find(|m| m["payload"]["agent"]["status"] == "running")
        .unwrap();
    assert_eq!(running["payload"]["kind"], "upsert");
    assert_eq!(running["payload"]["subscriptionId"], subscription_id);
    assert_eq!(running["payload"]["agent"]["id"], A);
    assert_eq!(running["payload"]["project"]["workspaceName"], "main");
    // Streams only go to explicit timeline subscribers.
    assert!(socket.frames_of("agent_stream").is_empty());
    assert_eq!(socket.dropped(), 0);
}

#[tokio::test]
async fn an_event_during_bootstrap_is_never_lost() {
    // The subscription registers before the snapshot is read, so whichever side an event lands
    // on, the client ends up knowing the newest state.
    for _ in 0..10 {
        let env = Env::new().await;
        env.seed_workspace().await;
        let session = MockSession::new("codex");
        live_agent(&env, A, &session).await;
        let socket = env.connect("client-1", &modern()).await;
        socket.send(fetch("r1", json!({ "subscribe": {} }))).await;
        session.push_event(common::turn_started("turn-1"));
        let response = socket.response("fetch_agents_response", "r1").await;
        let mut known = response["payload"]["entries"][0]["agent"]["status"]
            .as_str()
            .unwrap()
            .to_owned();
        if known != "running" {
            let u = wait_for(|| {
                updates(&socket)
                    .into_iter()
                    .find(|m| m["payload"]["agent"]["status"] == "running")
            })
            .await;
            known = u["payload"]["agent"]["status"].as_str().unwrap().to_owned();
        }
        assert_eq!(known, "running");
    }
}

#[tokio::test]
async fn plain_listing_pages_sorts_and_rejects_bad_cursors() {
    let env = Env::new().await;
    env.seed_workspace().await;
    for (id, at) in [
        (A, "2026-03-01T10:00:00.000Z"),
        (B, "2026-03-01T11:00:00.000Z"),
        (C, "2026-03-01T12:00:00.000Z"),
    ] {
        env.put_agent(stored(id, at)).await;
    }
    let socket = env.connect("client-1", &modern()).await;
    socket
        .send(fetch(
            "p1",
            json!({ "scope": "active", "page": { "limit": 2 } }),
        ))
        .await;
    let first = socket.response("fetch_agents_response", "p1").await;
    assert_eq!(entry_ids(&first), vec![C, B]);
    assert_eq!(first["payload"]["pageInfo"]["hasMore"], true);
    assert!(first["payload"].get("sync").is_none());
    assert!(first["payload"].get("subscriptionId").is_none());
    let cursor = first["payload"]["pageInfo"]["nextCursor"]
        .as_str()
        .unwrap()
        .to_owned();

    socket
        .send(fetch(
            "p2",
            json!({ "page": { "limit": 2, "cursor": cursor } }),
        ))
        .await;
    let second = socket.response("fetch_agents_response", "p2").await;
    assert_eq!(entry_ids(&second), vec![A]);
    assert_eq!(second["payload"]["pageInfo"]["hasMore"], false);
    assert_eq!(second["payload"]["pageInfo"]["prevCursor"], cursor);

    socket
        .send(fetch(
            "p3",
            json!({ "page": { "limit": 2, "cursor": "not-a-cursor" } }),
        ))
        .await;
    let invalid = socket.response("rpc_error", "p3").await;
    assert_eq!(invalid["payload"]["code"], "invalid_cursor");
    assert_eq!(invalid["payload"]["requestType"], "fetch_agents_request");

    // The cursor belongs to its sort.
    socket
        .send(fetch(
            "p4",
            json!({
                "sort": [{ "key": "title", "direction": "asc" }],
                "page": { "limit": 2, "cursor": cursor },
            }),
        ))
        .await;
    let mismatch = socket.response("rpc_error", "p4").await;
    assert_eq!(
        mismatch["payload"]["error"],
        "fetch_agents cursor does not match current sort"
    );

    // Ascending by title.
    socket
        .send(fetch(
            "p5",
            json!({ "sort": [{ "key": "title", "direction": "asc" }] }),
        ))
        .await;
    let by_title = socket.response("fetch_agents_response", "p5").await;
    assert_eq!(entry_ids(&by_title), vec![A, B, C]);
}

#[tokio::test]
async fn filters_scope_and_archive_state_shape_the_listing() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let mut labelled = stored(A, "2026-03-01T10:00:00.000Z");
    labelled.labels.insert("team".into(), "a".into());
    env.put_agent(labelled).await;
    env.put_agent(stored(B, "2026-03-01T11:00:00.000Z")).await;
    let mut archived = stored(C, "2026-03-01T12:00:00.000Z");
    archived.archived_at = Nullable::Value("2026-03-02T00:00:00.000Z".into());
    env.put_agent(archived).await;
    let mut homeless = stored("agent-d", "2026-03-01T13:00:00.000Z");
    homeless.workspace_id = None;
    env.put_agent(homeless).await;
    let socket = env.connect("client-1", &modern()).await;

    let list = |id: &'static str, extra: Value| {
        let socket = &socket;
        async move {
            socket.send(fetch(id, extra)).await;
            entry_ids(&socket.response("fetch_agents_response", id).await)
        }
    };
    // Agents without a workspace have no placement and never list.
    assert_eq!(list("f1", json!({})).await, vec![B, A]);
    assert_eq!(
        list("f2", json!({ "filter": { "labels": { "team": "a" } } })).await,
        vec![A]
    );
    assert_eq!(
        list("f3", json!({ "filter": { "includeArchived": true } })).await,
        vec![C, B, A]
    );
    assert_eq!(
        list("f4", json!({ "filter": { "statuses": ["running"] } })).await,
        Vec::<String>::new()
    );
    assert_eq!(list("f5", json!({ "scope": "active" })).await, vec![B, A]);
}

#[tokio::test]
async fn sequenced_reads_require_scope_active_and_no_filter() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let socket = env.connect("client-1", &modern()).await;
    socket.send(fetch("s1", json!({ "sync": {} }))).await;
    let error = socket.response("rpc_error", "s1").await;
    assert_eq!(error["payload"]["code"], "invalid_request");
    assert_eq!(
        error["payload"]["error"],
        "Sequenced agent directory reads require scope=active and no filter."
    );
}

#[tokio::test]
async fn provider_visibility_follows_the_asking_sources_capabilities() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let mut record = stored("agent-pi", "2026-03-01T10:00:00.000Z");
    record.provider = "claude".into();
    env.put_agent(record).await;
    let mut record = stored("agent-codex", "2026-03-01T11:00:00.000Z");
    record.provider = "codex".into();
    env.put_agent(record).await;
    // `opencode` is a legacy id; an unknown provider is registered below.
    env.manager
        .register_client("pi", Arc::new(paseo_core::mock::MockClient::new("pi")));
    let mut record = stored("agent-newer", "2026-03-01T12:00:00.000Z");
    record.provider = "pi".into();
    env.put_agent(record).await;

    let old = env.connect_with("old", &[], Some("0.1.40")).await;
    let all = env.connect("all", &[cap::ALL_PROVIDERS]).await;
    let new = env.connect_with("new", &[], Some("0.1.50")).await;
    for (socket, expected) in [
        (&old, vec!["agent-codex", "agent-pi"]),
        (&all, vec!["agent-newer", "agent-codex", "agent-pi"]),
        (&new, vec!["agent-newer", "agent-codex", "agent-pi"]),
    ] {
        socket.send(fetch("v", json!({}))).await;
        assert_eq!(
            entry_ids(&socket.response("fetch_agents_response", "v").await),
            expected
        );
    }
}

// ---- timeline fan-out ----

#[tokio::test]
async fn timeline_subscriptions_deliver_streams_only_for_followed_agents() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let (a, b, c) = (
        MockSession::new("codex"),
        MockSession::new("codex"),
        MockSession::new("codex"),
    );
    live_agent(&env, A, &a).await;
    live_agent(&env, B, &b).await;
    live_agent(&env, C, &c).await;
    let socket = env.connect("client-1", &modern()).await;
    socket
        .send(json!({
            "type": "agent.timeline.set_subscription.request",
            "requestId": "t1",
            "agentIds": [B, A, B],
        }))
        .await;
    let response = socket
        .response("agent.timeline.set_subscription.response", "t1")
        .await;
    assert_eq!(response["payload"]["agentIds"], json!([A, B]));
    let subscription_id = response["payload"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    a.push_event(common::turn_started("ta"));
    c.push_event(common::turn_started("tc"));
    b.push_event(common::turn_started("tb"));
    socket.wait_for_type("agent_stream", 2).await;
    settle().await;
    let streams = socket.frames_of("agent_stream");
    let agents: Vec<&str> = streams
        .iter()
        .map(|m| m["payload"]["agentId"].as_str().unwrap())
        .collect();
    assert!(agents.contains(&A) && agents.contains(&B));
    assert!(!agents.contains(&C));
    for stream in &streams {
        assert_eq!(stream["payload"]["subscriptionId"], subscription_id);
        assert_eq!(stream["payload"]["event"]["type"], "turn_started");
    }
    drop(streams);
    // Releasing the subscription stops the stream.
    socket
        .send(json!({
            "type": "subscription.release.request",
            "requestId": "t2",
            "subscriptionId": subscription_id,
        }))
        .await;
    let released = socket.response("subscription.release.response", "t2").await;
    assert_eq!(released["payload"]["subscriptionId"], subscription_id);
    let before = socket.frames_of("agent_stream").len();
    a.push_event(common::assistant("late", "codex", Some("ta")));
    settle().await;
    assert_eq!(socket.frames_of("agent_stream").len(), before);
}

#[tokio::test]
async fn legacy_and_modern_clients_see_the_same_event_stream_differently() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let (a, b) = (MockSession::new("codex"), MockSession::new("codex"));
    live_agent(&env, A, &a).await;
    live_agent(&env, B, &b).await;
    // Two sockets of one logical session: a legacy one and a modern one.
    let legacy = env.connect("shared", &[]).await;
    let modern_socket = env.connect("shared", &modern()).await;
    assert_eq!(env.registry.session_count(), 1);
    modern_socket
        .send(json!({
            "type": "agent.timeline.set_subscription.request",
            "requestId": "t1",
            "agentIds": [A],
        }))
        .await;
    modern_socket
        .response("agent.timeline.set_subscription.response", "t1")
        .await;

    a.push_event(common::turn_started("ta"));
    b.push_event(common::turn_started("tb"));
    wait_for(|| (legacy.frames_of("agent_stream").len() >= 2).then_some(())).await;
    settle().await;

    // Legacy gets every agent's stream implicitly, untagged.
    let legacy_agents: Vec<String> = legacy
        .frames_of("agent_stream")
        .iter()
        .map(|m| m["payload"]["agentId"].as_str().unwrap().to_owned())
        .collect();
    assert!(legacy_agents.contains(&A.to_owned()));
    assert!(legacy_agents.contains(&B.to_owned()));
    for frame in legacy.frames_of("agent_stream") {
        assert!(frame["payload"].get("subscriptionId").is_none());
    }
    // Modern gets only what it subscribed to, tagged.
    let modern_streams = modern_socket.frames_of("agent_stream");
    assert_eq!(modern_streams.len(), 1);
    assert_eq!(modern_streams[0]["payload"]["agentId"], A);
    assert!(modern_streams[0]["payload"]["subscriptionId"].is_string());
    // Nothing was routed to the modern socket without an owner.
    assert_eq!(modern_socket.dropped(), 0);
    assert_eq!(legacy.dropped(), 0);
}

#[tokio::test]
async fn selective_legacy_clients_only_follow_their_subscription() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let (a, b) = (MockSession::new("codex"), MockSession::new("codex"));
    live_agent(&env, A, &a).await;
    live_agent(&env, B, &b).await;
    let socket = env
        .connect("selective", &[cap::SELECTIVE_AGENT_TIMELINE])
        .await;
    socket
        .send(json!({
            "type": "agent.timeline.set_subscription.request",
            "requestId": "t1",
            "agentIds": [B],
        }))
        .await;
    let response = socket
        .response("agent.timeline.set_subscription.response", "t1")
        .await;
    // Legacy sources keep the id the response carries (no client id was sent, so a host id).
    assert!(response["payload"]["subscriptionId"].is_string());
    a.push_event(common::turn_started("ta"));
    b.push_event(common::turn_started("tb"));
    socket.wait_for_type("agent_stream", 1).await;
    settle().await;
    let streams = socket.frames_of("agent_stream");
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0]["payload"]["agentId"], B);
}

#[tokio::test]
async fn timeline_items_a_client_cannot_parse_are_withheld() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let a = MockSession::new("codex");
    live_agent(&env, A, &a).await;
    let old = env.connect("old", &[]).await;
    let new = env.connect("new", &[cap::TIMELINE_NOTIFICATIONS]).await;
    let notification = paseo_agent_types::AgentTimelineItem::Notification {
        message: "heads up".into(),
        level: paseo_agent_types::NotificationLevel::Info,
    };
    a.push_event(common::timeline_event(notification, "codex", Some("ta")));
    a.push_event(common::assistant("visible", "codex", Some("ta")));
    wait_for(|| (new.frames_of("agent_stream").len() >= 2).then_some(())).await;
    settle().await;
    let kinds = |s: &Socket| -> Vec<String> {
        s.frames_of("agent_stream")
            .iter()
            .filter(|m| m["payload"]["event"]["type"] == "timeline")
            .map(|m| {
                m["payload"]["event"]["item"]["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(kinds(&old), vec!["assistant_message"]);
    assert_eq!(kinds(&new), vec!["notification", "assistant_message"]);
}

// ---- events ----

#[tokio::test]
async fn attention_goes_to_event_subscribers_and_stays_out_of_modern_streams() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let a = MockSession::new("codex");
    live_agent(&env, A, &a).await;
    let socket = env.connect("shared", &modern()).await;
    let legacy = env.connect("shared", &[]).await;
    socket
        .send(json!({
            "type": "agent.timeline.set_subscription.request",
            "requestId": "t1",
            "agentIds": [A],
        }))
        .await;
    socket
        .response("agent.timeline.set_subscription.response", "t1")
        .await;
    let mut event_subscriptions = Vec::new();
    for id in ["e1", "e2"] {
        socket
            .send(json!({
                "type": "session.events.set_subscription.request",
                "requestId": id,
                "events": ["agent_attention_required"],
                "notifications": true,
            }))
            .await;
        let response = socket
            .response("session.events.set_subscription.response", id)
            .await;
        event_subscriptions.push(response["payload"]["subscriptionId"].clone());
    }
    a.push_event(AgentStreamEvent::AttentionRequired {
        provider: "codex".into(),
        reason: paseo_agent_types::AttentionReason::Finished,
        timestamp: "2026-03-01T12:00:00.000Z".into(),
    });
    // Each event subscription of the modern source gets its own tagged message.
    let attention = socket.wait_for_type("agent_attention_required", 2).await;
    settle().await;
    let attention_ids: Vec<Value> = socket
        .frames_of("agent_attention_required")
        .iter()
        .map(|m| m["payload"]["subscriptionId"].clone())
        .collect();
    assert_eq!(attention_ids, event_subscriptions);
    assert_eq!(attention[0]["payload"]["agentId"], A);
    assert_eq!(attention[0]["payload"]["reason"], "finished");
    // The modern timeline subscriber never gets the stream-level attention event...
    for stream in socket.frames_of("agent_stream") {
        assert_ne!(stream["payload"]["event"]["type"], "attention_required");
    }
    // ...while a legacy socket of the same session gets both, untagged.
    let legacy_streams = legacy.wait_for_type("agent_stream", 1).await;
    assert_eq!(
        legacy_streams[0]["payload"]["event"]["type"],
        "attention_required"
    );
    legacy.wait_for_type("agent_attention_required", 1).await;
    for frame in legacy.frames() {
        assert!(frame["payload"].get("subscriptionId").is_none());
    }
}

#[tokio::test]
async fn project_updates_are_sequenced_and_reach_only_subscribers() {
    let env = Env::new().await;
    let subscribed = env.connect("one", &modern()).await;
    let quiet = env.connect("two", &modern()).await;
    subscribed
        .send(json!({
            "type": "session.events.set_subscription.request",
            "requestId": "e1",
            "events": ["project.update"],
        }))
        .await;
    subscribed
        .response("session.events.set_subscription.response", "e1")
        .await;
    let project = json!({ "projectId": "prj_1", "projectName": "repo" });
    subscribed
        .handle()
        .project_update(paseo_directory::ProjectUpdate::Upsert {
            project: project.clone(),
        })
        .await;
    quiet
        .handle()
        .project_update(paseo_directory::ProjectUpdate::Upsert { project })
        .await;
    let updates = subscribed.wait_for_type("project.update", 1).await;
    assert_eq!(updates[0]["payload"]["kind"], "upsert");
    assert_eq!(updates[0]["payload"]["generation"], GENERATION);
    assert!(updates[0]["payload"]["seq"].is_u64());
    assert!(updates[0]["payload"]["subscriptionId"].is_string());
    settle().await;
    assert!(quiet.frames_of("project.update").is_empty());
}

// ---- release, detach, shutdown ----

async fn wait_for_manager_subscribers(env: &Env, count: usize) {
    wait_for(|| (env.manager.subscription_count() == count).then_some(())).await;
}

#[tokio::test]
async fn the_manager_subscription_exists_only_while_something_wants_agent_events() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let baseline = env.manager.subscription_count();
    let socket = env.connect("client-1", &modern()).await;
    // A modern socket with no subscription is no demand.
    settle().await;
    assert_eq!(env.manager.subscription_count(), baseline);
    socket.send(fetch("r1", json!({ "subscribe": {} }))).await;
    let response = socket.response("fetch_agents_response", "r1").await;
    wait_for_manager_subscribers(&env, baseline + 1).await;
    socket
        .send(json!({
            "type": "subscription.release.request",
            "requestId": "r2",
            "subscriptionId": response["payload"]["subscriptionId"],
        }))
        .await;
    socket.response("subscription.release.response", "r2").await;
    wait_for_manager_subscribers(&env, baseline).await;
}

#[tokio::test]
async fn a_legacy_socket_is_demand_by_itself() {
    let env = Env::new().await;
    let baseline = env.manager.subscription_count();
    let legacy = env.connect("legacy", &[]).await;
    wait_for_manager_subscribers(&env, baseline + 1).await;
    legacy.detach().await;
    wait_for_manager_subscribers(&env, baseline).await;
}

#[tokio::test]
async fn disconnecting_releases_what_the_socket_owned() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let baseline = env.manager.subscription_count();
    let a = MockSession::new("codex");
    live_agent(&env, A, &a).await;
    let socket = env.connect("client-1", &modern()).await;
    socket.send(fetch("r1", json!({ "subscribe": {} }))).await;
    socket.response("fetch_agents_response", "r1").await;
    wait_for_manager_subscribers(&env, baseline + 1).await;
    let handle = socket.handle().clone();
    socket.detach().await;
    wait_for_manager_subscribers(&env, baseline).await;
    wait_for(|| (handle.delivery().registration_count() == 0).then_some(())).await;
    let before = socket.frames().len();
    a.push_event(common::turn_started("ta"));
    settle().await;
    assert_eq!(socket.frames().len(), before);
}

#[tokio::test]
async fn shutdown_closes_delivery_and_ends_the_sessions() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let baseline = env.manager.subscription_count();
    let socket = env.connect("client-1", &modern()).await;
    socket.send(fetch("r1", json!({ "subscribe": {} }))).await;
    socket.response("fetch_agents_response", "r1").await;
    env.registry.shutdown_all().await;
    assert_eq!(env.registry.session_count(), 0);
    wait_for_manager_subscribers(&env, baseline).await;
}

// ---- authorization, errors, ping ----

#[tokio::test]
async fn authorization_runs_inside_the_session() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let a = MockSession::new("codex");
    live_agent(&env, A, &a).await;
    let socket = env.connect("client-1", &modern()).await;
    socket.send(fetch("r1", json!({ "subscribe": {} }))).await;
    socket.response("fetch_agents_response", "r1").await;

    socket.handle().set_permissions(PermSet::EMPTY);
    // Inbound: denied with an access_denied error when the message has a requestId.
    socket.send(fetch("r2", json!({}))).await;
    let denied = socket.response("rpc_error", "r2").await;
    assert_eq!(denied["payload"]["code"], "access_denied");
    assert_eq!(
        denied["payload"]["error"],
        "Session is not authorized for fetch_agents_request"
    );
    assert_eq!(denied["payload"]["requestType"], "fetch_agents_request");
    // Outbound: live updates the session may no longer see are dropped silently.
    let before = socket.frames().len();
    a.push_event(common::turn_started("ta"));
    settle().await;
    assert_eq!(socket.frames().len(), before);
    // Permissions come back and delivery resumes.
    socket.handle().set_permissions(PermSet::OWNER);
    a.push_event(common::assistant("hello", "codex", Some("ta")));
    a.push_event(common::turn_completed("ta"));
    wait_for(|| (socket.frames().len() > before).then_some(())).await;
}

#[tokio::test]
async fn a_failing_handler_answers_handler_error() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let socket = env.connect("client-1", &modern()).await;
    // A modern client may not pick the subscription id.
    socket
        .send(fetch(
            "r1",
            json!({ "subscribe": { "subscriptionId": "mine" } }),
        ))
        .await;
    let error = socket.response("rpc_error", "r1").await;
    assert_eq!(error["payload"]["code"], "handler_error");
    assert_eq!(
        error["payload"]["error"],
        "Request failed: Subscription IDs are assigned by the host"
    );
}

#[tokio::test]
async fn legacy_clients_choose_their_own_subscription_id() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let socket = env.connect("legacy", &[]).await;
    socket
        .send(fetch(
            "r1",
            json!({ "subscribe": { "subscriptionId": "mine" } }),
        ))
        .await;
    let response = socket.response("fetch_agents_response", "r1").await;
    assert_eq!(response["payload"]["subscriptionId"], "mine");
}

#[tokio::test]
async fn ping_answers_with_a_pong() {
    let env = Env::new().await;
    let socket = env.connect("client-1", &modern()).await;
    socket
        .send(json!({ "type": "ping", "requestId": "p1", "clientSentAt": 5 }))
        .await;
    let pong = socket.response("pong", "p1").await;
    assert_eq!(pong["payload"]["clientSentAt"], 5);
    assert!(pong["payload"]["serverReceivedAt"].is_i64());
}

#[tokio::test]
async fn every_type_the_actor_claims_to_handle_answers() {
    let env = Env::new().await;
    env.seed_workspace().await;
    let socket = env.connect("client-1", &modern()).await;
    for (ty, extra, answer) in [
        ("fetch_agents_request", json!({}), "fetch_agents_response"),
        (
            "session.events.set_subscription.request",
            json!({ "events": [] }),
            "session.events.set_subscription.response",
        ),
        (
            "agent.timeline.set_subscription.request",
            json!({ "agentIds": [] }),
            "agent.timeline.set_subscription.response",
        ),
        ("ping", json!({ "clientSentAt": 1 }), "pong"),
    ] {
        let mut message = json!({ "type": ty, "requestId": ty });
        for (k, v) in extra.as_object().unwrap() {
            message[k] = v.clone();
        }
        socket.send(message).await;
        socket.response(answer, ty).await;
    }
    // The release request needs a subscription to release (it answers either way).
    socket
        .send(json!({
            "type": "subscription.release.request",
            "requestId": "rel",
            "subscriptionId": "nothing",
        }))
        .await;
    socket
        .response("subscription.release.response", "rel")
        .await;
    for ty in paseo_core::session::HANDLED_INBOUND {
        assert!(
            [
                "fetch_agents_request",
                "subscription.release.request",
                "session.events.set_subscription.request",
                "agent.timeline.set_subscription.request",
                "ping"
            ]
            .contains(ty),
            "{ty} has no coverage here"
        );
    }
}

#[tokio::test]
async fn inbound_from_an_unknown_socket_is_refused() {
    let env = Env::new().await;
    assert!(
        !env.registry
            .inbound(paseo_core::delivery::SourceId(999), json!({"type": "ping"}))
            .await
    );
}

// ---- reconnect grace ----

#[tokio::test(start_paused = true)]
async fn an_idle_session_lingers_for_the_grace_and_resumes_when_the_client_returns() {
    let env = Env::new().await;
    let first = env.connect("client-1", &modern()).await;
    first.detach().await;
    assert_eq!(env.registry.session_count(), 1);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(env.registry.session_count(), 1);

    // The client comes back within 90 s: same session, the timer is cancelled.
    let second = env.connect("client-1", &modern()).await;
    assert_eq!(second.handle().key(), first.handle().key());
    tokio::time::sleep(Duration::from_secs(200)).await;
    assert_eq!(env.registry.session_count(), 1);

    // Gone for good: the session ends after the grace.
    second.detach().await;
    tokio::time::sleep(Duration::from_secs(89)).await;
    assert_eq!(env.registry.session_count(), 1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(env.registry.session_count(), 0);

    // A new connection starts a fresh session.
    let third = env.connect("client-1", &modern()).await;
    assert_eq!(env.registry.session_count(), 1);
    third.detach().await;
}

#[tokio::test(start_paused = true)]
async fn sessions_of_different_clients_are_independent() {
    let env = Env::new().await;
    let one = env.connect("client-1", &modern()).await;
    let two = env.connect("client-2", &modern()).await;
    assert_eq!(env.registry.session_count(), 2);
    one.detach().await;
    tokio::time::sleep(Duration::from_secs(91)).await;
    assert_eq!(env.registry.session_count(), 1);
    two.detach().await;
}

// ---- internal.delivery.* ----

#[derive(Default)]
struct FakeRpc {
    calls: Mutex<Vec<(String, Value)>>,
    fail: Mutex<bool>,
}

#[async_trait]
impl TsRpc for FakeRpc {
    async fn call(&self, request_type: &str, payload: Value) -> Result<Value, String> {
        self.calls
            .lock()
            .unwrap()
            .push((request_type.to_owned(), payload));
        if *self.fail.lock().unwrap() {
            return Err("ts is gone".into());
        }
        Ok(json!({}))
    }
}

#[tokio::test]
async fn ts_resident_producers_own_subscriptions_through_the_core_registry() {
    let env = Env::new().await;
    let socket = env.connect("client-1", &modern()).await;
    let rpc = Arc::new(FakeRpc::default());
    let begun = env
        .registry
        .remote_begin(
            rpc.clone(),
            RemoteBegin {
                source: socket.source,
                family: "terminals".into(),
                requested_id: None,
                legacy_slot: None,
                request: RequestInfo::new("subscribe_terminals_request", Some("r1")),
            },
        )
        .await
        .unwrap();
    assert_eq!(begun.subscription_id, begun.response_id);

    assert!(env.registry.remote_emit(
        &begun.subscription_id,
        json!({ "type": "terminals_changed", "payload": { "cwd": "/x", "terminals": [] } }),
    ));
    let frame = socket.wait_for_type("terminals_changed", 1).await.remove(0);
    assert_eq!(frame["payload"]["subscriptionId"], begun.subscription_id);
    assert!(env
        .registry
        .remote_emit_binary(&begun.subscription_id, vec![1, 2, 3]));
    wait_for(|| (socket.wire.binary.lock().unwrap().len() == 1).then_some(())).await;
    assert!(!env.registry.remote_emit("unknown", json!({"type": "x"})));

    // A failing teardown leaves the subscription registered.
    *rpc.fail.lock().unwrap() = true;
    socket
        .send(json!({
            "type": "subscription.release.request",
            "requestId": "r2",
            "subscriptionId": begun.subscription_id,
        }))
        .await;
    let error = socket.response("rpc_error", "r2").await;
    assert_eq!(error["payload"]["error"], "Request failed: ts is gone");
    assert_eq!(
        env.registry.remote_release(&begun.subscription_id).await,
        Err("ts is gone".into())
    );

    // And a successful one is forgotten. The release is memoized, so a fresh subscription proves it.
    let again = env
        .registry
        .remote_begin(
            rpc.clone(),
            RemoteBegin {
                source: socket.source,
                family: "terminals".into(),
                requested_id: None,
                legacy_slot: None,
                request: RequestInfo::new("subscribe_terminals_request", Some("r3")),
            },
        )
        .await
        .unwrap();
    *rpc.fail.lock().unwrap() = false;
    socket
        .send(json!({
            "type": "subscription.release.request",
            "requestId": "r4",
            "subscriptionId": again.subscription_id,
        }))
        .await;
    socket.response("subscription.release.response", "r4").await;
    assert!(!env
        .registry
        .remote_emit(&again.subscription_id, json!({"type": "x"})));
    let stops: Vec<Value> = rpc
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(t, _)| t == "internal.delivery.stop.request")
        .map(|(_, p)| p["subscriptionId"].clone())
        .collect();
    assert!(stops.contains(&json!(again.subscription_id)));
}

#[tokio::test]
async fn remote_begin_needs_a_known_source_and_a_request_id() {
    let env = Env::new().await;
    let socket = env.connect("client-1", &modern()).await;
    let rpc = Arc::new(FakeRpc::default());
    let unknown = env
        .registry
        .remote_begin(
            rpc.clone(),
            RemoteBegin {
                source: paseo_core::delivery::SourceId(4242),
                family: "terminals".into(),
                requested_id: None,
                legacy_slot: None,
                request: RequestInfo::new("subscribe_terminals_request", Some("r1")),
            },
        )
        .await;
    assert!(unknown.is_err());
    let no_request_id = env
        .registry
        .remote_begin(
            rpc,
            RemoteBegin {
                source: socket.source,
                family: "terminals".into(),
                requested_id: None,
                legacy_slot: None,
                request: RequestInfo::new("subscribe_terminals_request", None),
            },
        )
        .await;
    assert_eq!(
        no_request_id,
        Err("Owned subscriptions require a requestId".to_owned())
    );
}

#[test]
fn agent_stream_event_type_names_match_the_wire() {
    // A guard for the helper events the tests above push.
    let event: AgentStreamEvent = common::turn_started("t");
    assert_eq!(
        paseo_agent_types::to_wire(&event).unwrap().type_name(),
        "turn_started"
    );
}
