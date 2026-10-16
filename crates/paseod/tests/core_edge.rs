//! The edge with the Rust core behind it (`PASEO_CORE=rust`): a real client socket, a fake TS
//! backend, real core services in a temp dir.
//!
//! Covers routing (owned types answered in Rust, everything else forwarded byte for byte, frames
//! before `hello` left to TS), the delivery gate at the socket writer, session lifetime across
//! socket close, and `internal.delivery.*` over `GET /__paseo_edge/rpc` with a fake TS controller.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use paseo_core::mock::MockClient;
use paseo_core::session::{NoSessionHooks, SessionServices};
use paseo_core::time::SystemClock;
use paseo_core::{
    AgentManager, ManagerOptions, ProviderDefinition, ProviderRegistryUpdate, StorePersister,
};
use paseo_directory::DirectorySync;
use paseo_provider_api::AgentClient;
use paseo_store::workspace_registry::WorkspaceKind;
use paseo_store::{
    AgentStore, Nullable, ProjectKind, ProjectRecord, ProjectRegistry, StoredAgentRecord,
    WorkspaceRecord, WorkspaceRegistry,
};
use paseod::backend::Backend;
use paseod::config::{BackendConfig, EdgeConfig, Limits, ListenAddr, RelayConfig};
use paseod::core_runtime::CoreRuntime;
use paseod::http::{self, AppState};
use paseod::internal::HEADER_SOCKET_ID;
use paseod::internal_rpc::InternalRpc;
use paseod::ws::WsProxy;
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const TOKEN: &str = "edge-test-token";
const SERVER_INFO: &str = r#"{"type":"session","message":{"type":"status","payload":{"status":"server_info","serverId":"s"}}}"#;

/// What the fake TS backend saw.
#[derive(Default)]
struct Seen {
    frames: Mutex<Vec<String>>,
    socket_ids: Mutex<Vec<u64>>,
}

async fn fake_ts(seen: Arc<Seen>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let capture = seen.clone();
                let cb = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                    if let Some(id) = req
                        .headers()
                        .get(HEADER_SOCKET_ID)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                    {
                        capture.socket_ids.lock().unwrap().push(id);
                    }
                    Ok(resp)
                };
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await else {
                    return;
                };
                let _ = ws.send(WsMsg::Text(SERVER_INFO.into())).await;
                while let Some(Ok(message)) = ws.next().await {
                    if let WsMsg::Text(text) = message {
                        seen.frames.lock().unwrap().push(text.as_str().to_owned());
                    }
                }
            });
        }
    });
    addr
}

struct Edge {
    addr: SocketAddr,
    runtime: CoreRuntime,
    seen: Arc<Seen>,
    rpc: Arc<InternalRpc>,
    _dir: tempfile::TempDir,
}

fn edge_config() -> Arc<EdgeConfig> {
    Arc::new(EdgeConfig {
        listen: ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()),
        paseo_home: std::env::temp_dir(),
        web_ui: None,
        limits: Limits::default(),
        backend: BackendConfig::new(vec!["x".into()]),
        relay: RelayConfig {
            enabled: false,
            endpoint: "r:1".into(),
            use_tls: false,
        },
        trusted_proxies: Default::default(),
        watch_parent_stdin: false,
        attach: None,
    })
}

async fn start_edge() -> Edge {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(AgentStore::new(dir.path().join("agents")));
    let projects = Arc::new(ProjectRegistry::new(dir.path().join("projects.json")));
    let workspaces = Arc::new(WorkspaceRegistry::new(dir.path().join("workspaces.json")));
    let mut providers = ProviderRegistryUpdate::default();
    providers.clients.insert(
        "codex".into(),
        Arc::new(MockClient::new("codex")) as Arc<dyn AgentClient>,
    );
    providers
        .definitions
        .insert("codex".into(), ProviderDefinition::enabled());
    let manager = AgentManager::new(ManagerOptions {
        store: Some(store.clone()),
        persister: Some(Arc::new(StorePersister::new(store.clone()))),
        providers,
        ..ManagerOptions::default()
    });
    projects
        .upsert(ProjectRecord::new(
            "prj_1",
            "/work/repo",
            ProjectKind::Git,
            "repo",
            "2026-03-01T00:00:00.000Z",
            "2026-03-01T00:00:00.000Z",
        ))
        .await
        .unwrap();
    workspaces
        .upsert(
            WorkspaceRecord::new(
                "ws_1",
                "prj_1",
                "/work/repo",
                WorkspaceKind::LocalCheckout,
                "main",
                "2026-03-01T00:00:00.000Z",
                "2026-03-01T00:00:00.000Z",
            ),
            None,
        )
        .await
        .unwrap();
    let mut record =
        StoredAgentRecord::new("agent-a", "codex", "/work/repo", "2026-03-01T10:00:00.000Z");
    record.workspace_id = Some("ws_1".into());
    record.title = Nullable::Value("Agent a".into());
    store.upsert(record).await.unwrap();

    let runtime = CoreRuntime::from_services(SessionServices {
        manager,
        store,
        workspaces,
        projects,
        directory: Arc::new(DirectorySync::new("edge-generation")),
        hooks: Arc::new(NoSessionHooks),
        clock: Arc::new(SystemClock),
    });
    let seen = Arc::new(Seen::default());
    let backend_addr = fake_ts(seen.clone()).await;
    let backend = Backend::fixed(backend_addr, TOKEN);
    let ws = Arc::new(
        WsProxy::new_fixed(
            format!("ws://{backend_addr}/ws"),
            TOKEN,
            Arc::new(runtime.owned_types()),
            Limits::default(),
        )
        .with_dispatcher(runtime.dispatcher.clone()),
    );
    let rpc = InternalRpc::new(runtime.sessions.clone());
    let state = AppState::new(edge_config(), backend, ws)
        .with_internal_rpc(http::rpc::router(rpc.clone(), TOKEN.to_owned()));
    let listener = http::bind(&ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr: SocketAddr = listener.public_address().parse().unwrap();
    tokio::spawn(http::serve_on(state, listener, std::future::pending()));
    Edge {
        addr,
        runtime,
        seen,
        rpc,
        _dir: dir,
    }
}

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn connect(edge: &Edge) -> Client {
    let req = format!("ws://{}/ws", edge.addr)
        .into_client_request()
        .unwrap();
    let (mut client, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    // The first frame is TS's `server_info`, passed through untouched.
    match next_text(&mut client).await {
        text if text == SERVER_INFO => {}
        other => panic!("expected server_info, got {other}"),
    }
    client
}

async fn next_text(client: &mut Client) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.next().await {
                Some(Ok(WsMsg::Text(t))) => return t.as_str().to_owned(),
                Some(Ok(_)) => {}
                other => panic!("client socket ended: {other:?}"),
            }
        }
    })
    .await
    .expect("a frame within 5 s")
}

async fn next_message(client: &mut Client, ty: &str) -> Value {
    loop {
        let envelope: Value = serde_json::from_str(&next_text(client).await).unwrap();
        if envelope["type"] == "session" && envelope["message"]["type"] == ty {
            return envelope["message"].clone();
        }
    }
}

fn hello(client_id: &str, caps: Value) -> String {
    json!({
        "type": "hello",
        "clientId": client_id,
        "clientType": "cli",
        "protocolVersion": 1,
        "capabilities": caps,
    })
    .to_string()
}

fn session(message: Value) -> String {
    json!({ "type": "session", "message": message }).to_string()
}

async fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met within 5 s");
}

fn backend_saw(edge: &Edge, needle: &str) -> bool {
    edge.seen
        .frames
        .lock()
        .unwrap()
        .iter()
        .any(|f| f.contains(needle))
}

#[tokio::test]
async fn owned_requests_are_answered_by_the_core_and_everything_else_goes_to_ts() {
    let edge = start_edge().await;
    let mut client = connect(&edge).await;
    client
        .send(WsMsg::Text(
            hello("client-1", json!({ "owned_subscriptions": true })).into(),
        ))
        .await
        .unwrap();
    // `hello` is observed by the core and still forwarded: TS owns admission.
    wait_for(|| backend_saw(&edge, r#""type":"hello""#).then_some(())).await;

    client
        .send(WsMsg::Text(
            session(json!({
                "type": "fetch_agents_request",
                "requestId": "r1",
                "scope": "active",
                "subscribe": {},
            }))
            .into(),
        ))
        .await
        .unwrap();
    let response = next_message(&mut client, "fetch_agents_response").await;
    assert_eq!(response["payload"]["requestId"], "r1");
    assert!(response["payload"]["subscriptionId"].is_string());
    assert_eq!(response["payload"]["entries"][0]["agent"]["id"], "agent-a");
    assert_eq!(
        response["payload"]["entries"][0]["project"]["projectName"],
        "repo"
    );

    // Not owned: forwarded untouched. Owned frames never reached TS.
    let other = session(json!({ "type": "create_agent_request", "requestId": "r2" }));
    client
        .send(WsMsg::Text(other.clone().into()))
        .await
        .unwrap();
    wait_for(|| {
        edge.seen
            .frames
            .lock()
            .unwrap()
            .contains(&other)
            .then_some(())
    })
    .await;
    assert!(!backend_saw(&edge, "fetch_agents_request"));
    assert_eq!(edge.runtime.sessions.session_count(), 1);
}

#[tokio::test]
async fn frames_before_hello_and_malformed_hello_are_left_to_ts() {
    let edge = start_edge().await;
    let mut client = connect(&edge).await;
    let early = session(json!({ "type": "fetch_agents_request", "requestId": "early" }));
    client
        .send(WsMsg::Text(early.clone().into()))
        .await
        .unwrap();
    wait_for(|| {
        edge.seen
            .frames
            .lock()
            .unwrap()
            .contains(&early)
            .then_some(())
    })
    .await;
    // A hello without a client id is TS's to reject; the core does not register the socket.
    let bad = json!({ "type": "hello", "clientType": "cli", "protocolVersion": 1 }).to_string();
    client.send(WsMsg::Text(bad.clone().into())).await.unwrap();
    wait_for(|| {
        edge.seen
            .frames
            .lock()
            .unwrap()
            .contains(&bad)
            .then_some(())
    })
    .await;
    let later = session(json!({ "type": "fetch_agents_request", "requestId": "later" }));
    client
        .send(WsMsg::Text(later.clone().into()))
        .await
        .unwrap();
    wait_for(|| {
        edge.seen
            .frames
            .lock()
            .unwrap()
            .contains(&later)
            .then_some(())
    })
    .await;
    assert_eq!(edge.runtime.sessions.session_count(), 0);
}

#[tokio::test]
async fn the_writer_enforces_ownership_and_close_detaches_the_session() {
    let edge = start_edge().await;
    let baseline = edge.runtime.services.manager.subscription_count();
    let mut client = connect(&edge).await;
    client
        .send(WsMsg::Text(
            hello("client-1", json!({ "owned_subscriptions": true })).into(),
        ))
        .await
        .unwrap();
    client
        .send(WsMsg::Text(
            session(json!({ "type": "fetch_agents_request", "requestId": "r1", "subscribe": {} }))
                .into(),
        ))
        .await
        .unwrap();
    next_message(&mut client, "fetch_agents_response").await;
    let socket = paseod::ws::SocketId(edge.seen.socket_ids.lock().unwrap()[0]);
    let session_handle = edge
        .runtime
        .sessions
        .session_of(paseod::client_registry::ClientRegistry::source_of(socket))
        .expect("session");

    // A frame with no owner is refused at the writer for a modern socket.
    let source = paseod::client_registry::ClientRegistry::source_of(socket);
    session_handle.delivery().emit_unowned(
        source,
        json!({ "type": "activity_log", "payload": { "id": "x" } }),
    );
    wait_for(|| (edge.runtime.clients.denied(socket) == 1).then_some(())).await;

    // Closing the socket releases what it owned; the session waits out the grace.
    client.close(None).await.unwrap();
    wait_for(|| (edge.runtime.services.manager.subscription_count() == baseline).then_some(()))
        .await;
    wait_for(|| (!edge.runtime.clients.is_registered(socket)).then_some(())).await;
    assert_eq!(edge.runtime.sessions.session_count(), 1);
}

// ---- internal.delivery.* ----

async fn rpc_connect(
    edge: &Edge,
    token: Option<&str>,
) -> Result<Client, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{}/__paseo_edge/rpc", edge.addr)
        .into_client_request()
        .unwrap();
    if let Some(token) = token {
        req.headers_mut()
            .insert("x-paseo-edge-token", token.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(c, _)| c)
}

async fn next_json(client: &mut Client) -> Value {
    serde_json::from_str(&next_text(client).await).unwrap()
}

#[tokio::test]
async fn the_rpc_route_needs_the_edge_token() {
    let edge = start_edge().await;
    assert!(rpc_connect(&edge, None).await.is_err());
    assert!(rpc_connect(&edge, Some("forged")).await.is_err());
    assert!(rpc_connect(&edge, Some(TOKEN)).await.is_ok());
}

#[tokio::test]
async fn ts_controllers_register_owned_subscriptions_over_internal_delivery() {
    let edge = start_edge().await;
    let mut client = connect(&edge).await;
    client
        .send(WsMsg::Text(
            hello("client-1", json!({ "owned_subscriptions": true })).into(),
        ))
        .await
        .unwrap();
    wait_for(|| edge.runtime.sessions.session_count().eq(&1).then_some(())).await;
    let socket_id = edge.seen.socket_ids.lock().unwrap()[0];

    let mut ts = rpc_connect(&edge, Some(TOKEN)).await.unwrap();
    wait_for(|| edge.rpc.is_connected().then_some(())).await;

    // TS serves `subscribe_terminals_request` and asks the core to own its subscription.
    ts.send(WsMsg::Text(
        json!({
            "type": "internal.delivery.begin.request",
            "requestId": "b1",
            "sourceId": socket_id,
            "family": "terminals",
            "request": { "type": "subscribe_terminals_request", "requestId": "t1" },
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let begun = next_json(&mut ts).await;
    assert_eq!(begun["type"], "internal.delivery.begin.response");
    assert_eq!(begun["payload"]["requestId"], "b1");
    assert_eq!(begun["payload"]["error"], Value::Null);
    let subscription_id = begun["payload"]["subscriptionId"]
        .as_str()
        .unwrap()
        .to_owned();

    // Emissions are tagged and delivered to the client through the one writer.
    ts.send(WsMsg::Text(
        json!({
            "type": "internal.delivery.emit",
            "subscriptionId": subscription_id,
            "message": { "type": "terminals_changed", "payload": { "cwd": "/x", "terminals": [] } },
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let changed = next_message(&mut client, "terminals_changed").await;
    assert_eq!(changed["payload"]["subscriptionId"], subscription_id);

    // The client releases it: the core asks TS to stop the producer, TS answers, the client is told.
    client
        .send(WsMsg::Text(
            session(json!({
                "type": "subscription.release.request",
                "requestId": "rel1",
                "subscriptionId": subscription_id,
            }))
            .into(),
        ))
        .await
        .unwrap();
    let stop = next_json(&mut ts).await;
    assert_eq!(stop["type"], "internal.delivery.stop.request");
    assert_eq!(stop["subscriptionId"], subscription_id);
    ts.send(WsMsg::Text(
        json!({
            "type": "internal.delivery.stop.response",
            "payload": { "requestId": stop["requestId"], "error": null },
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let released = next_message(&mut client, "subscription.release.response").await;
    assert_eq!(released["payload"]["subscriptionId"], subscription_id);
    // Later emissions for the released subscription are ignored.
    assert!(!edge
        .runtime
        .sessions
        .remote_emit(&subscription_id, json!({"type": "x"})));
}

#[tokio::test]
async fn unknown_internal_requests_are_answered_with_an_error() {
    let edge = start_edge().await;
    let mut ts = rpc_connect(&edge, Some(TOKEN)).await.unwrap();
    ts.send(WsMsg::Text(
        json!({ "type": "internal.nothing.do.request", "requestId": "u1" })
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let reply = next_json(&mut ts).await;
    assert_eq!(reply["type"], "internal.nothing.do.response");
    assert_eq!(reply["payload"]["error"], "unknown internal rpc");
}
