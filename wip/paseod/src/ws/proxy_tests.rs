//! Proxy tests against an in-process fake backend (tokio-tungstenite server).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::WebSocketStream;

use super::*;
use crate::error::EdgeError;
use crate::routing::{Domain, OwnedTypes};

const TOKEN: &str = "test-edge-token";
const SERVER_INFO: &str = r#"{"type":"session","message":{"type":"status","payload":{"status":"server_info","serverId":"s"}}}"#;

type Ws = WebSocketStream<TcpStream>;

struct Conn {
    ws: Ws,
    headers: HeaderMap,
}

#[derive(Default)]
struct BackendOpts {
    reject: Option<(u16, &'static str)>,
    select_protocol: bool,
}

async fn spawn_backend<F, Fut>(opts: BackendOpts, f: F) -> SocketAddr
where
    F: Fn(Conn) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let f = Arc::new(f);
    let opts = Arc::new(opts);
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let f = f.clone();
            let opts = opts.clone();
            tokio::spawn(async move {
                let captured = Arc::new(Mutex::new(HeaderMap::new()));
                let cap = captured.clone();
                let cb = move |req: &Request,
                               mut resp: Response|
                      -> std::result::Result<Response, ErrorResponse> {
                    *cap.lock().unwrap() = req.headers().clone();
                    if let Some((status, body)) = opts.reject {
                        let mut r = ErrorResponse::new(Some(body.to_owned()));
                        *r.status_mut() =
                            tokio_tungstenite::tungstenite::http::StatusCode::from_u16(status)
                                .unwrap();
                        return Err(r);
                    }
                    if opts.select_protocol {
                        if let Some(p) = req.headers().get("sec-websocket-protocol") {
                            resp.headers_mut()
                                .insert("sec-websocket-protocol", p.clone());
                        }
                    }
                    Ok(resp)
                };
                if let Ok(ws) = tokio_tungstenite::accept_hdr_async(tcp, cb).await {
                    let headers = captured.lock().unwrap().clone();
                    f(Conn { ws, headers }).await;
                }
            });
        }
    });
    addr
}

fn proxy_for(addr: SocketAddr, limits: Limits) -> Arc<WsProxy> {
    Arc::new(WsProxy::new_fixed(
        format!("ws://{addr}/ws"),
        TOKEN,
        Arc::new(OwnedTypes::empty()),
        limits,
    ))
}

async fn spawn_edge(proxy: Arc<WsProxy>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = ws_router(proxy).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

type Client = WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

async fn connect(addr: SocketAddr, headers: &[(&str, &str)]) -> Client {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

async fn echo(mut c: Conn) {
    while let Some(Ok(m)) = c.ws.next().await {
        if (m.is_text() || m.is_binary()) && c.ws.send(m).await.is_err() {
            return;
        }
    }
}

fn short_limits() -> Limits {
    Limits {
        lease: Duration::from_millis(400),
        lease_check_interval: Duration::from_millis(50),
        ..Limits::default()
    }
}

async fn next_frame(c: &mut Client) -> WsMsg {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c.next().await {
                Some(Ok(WsMsg::Ping(_) | WsMsg::Pong(_))) => {}
                Some(Ok(m)) => return m,
                other => panic!("stream ended: {other:?}"),
            }
        }
    })
    .await
    .expect("frame timeout")
}

#[tokio::test]
async fn passthrough_text_and_binary_is_byte_exact() {
    let backend = spawn_backend(BackendOpts::default(), echo).await;
    let edge = spawn_edge(proxy_for(backend, Limits::default())).await;
    let mut c = connect(edge, &[]).await;

    let texts = [
        r#"{ "type" : "hello" ,"clientId":"c","protocolVersion":1 ,  "auth":{"kind":"password","password":"p w"}}"#.to_owned(),
        "{\"type\":\"session\",\"message\":{\"type\":\"x\",\"v\":\"\u{1F600} \\u00e9 \u{0}\"}}".to_owned(),
        "not json at all \n\t".to_owned(),
        String::new(),
        "x".repeat(300_000),
    ];
    for t in &texts {
        c.send(WsMsg::Text(t.clone().into())).await.unwrap();
        match next_frame(&mut c).await {
            WsMsg::Text(got) => assert_eq!(got.as_str(), t),
            other => panic!("expected text, got {other:?}"),
        }
    }
    let mut big = vec![0u8; 2 * 1024 * 1024];
    for (i, b) in big.iter_mut().enumerate() {
        *b = (i * 31 % 256) as u8;
    }
    let bins: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x00],
        vec![0x01, 0xff, 0x00, 0x7f],
        vec![0x10; 70_000],
        big,
    ];
    for b in &bins {
        c.send(WsMsg::Binary(b.clone().into())).await.unwrap();
        match next_frame(&mut c).await {
            WsMsg::Binary(got) => assert_eq!(got.as_ref(), b.as_slice()),
            other => panic!("expected binary, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn forwards_identity_headers_and_selected_subprotocol() {
    let (tx, mut rx) = mpsc::unbounded_channel::<HeaderMap>();
    let backend = spawn_backend(
        BackendOpts {
            select_protocol: true,
            ..Default::default()
        },
        move |c: Conn| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(c.headers.clone());
                echo(c).await;
            }
        },
    )
    .await;
    let edge = spawn_edge(proxy_for(backend, Limits::default())).await;

    let mut req = format!("ws://{edge}/ws").into_client_request().unwrap();
    let h = req.headers_mut();
    h.insert("host", "daemon.example:6767".parse().unwrap());
    h.insert("origin", "http://localhost:6767".parse().unwrap());
    h.insert("authorization", "Bearer abc".parse().unwrap());
    h.insert(
        "sec-websocket-protocol",
        "paseo.bearer.abc".parse().unwrap(),
    );
    h.insert("x-paseo-edge-token", "forged".parse().unwrap());
    h.insert("x-paseo-edge-remote-addr", "6.6.6.6:1".parse().unwrap());
    h.insert("x-forwarded-for", "6.6.6.6".parse().unwrap());
    let (_c, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(
        resp.headers().get("sec-websocket-protocol").unwrap(),
        "paseo.bearer.abc"
    );

    let got = rx.recv().await.unwrap();
    let get = |n: &str| got.get(n).map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(get("host").as_deref(), Some("daemon.example:6767"));
    assert_eq!(get("origin").as_deref(), Some("http://localhost:6767"));
    assert_eq!(get("authorization").as_deref(), Some("Bearer abc"));
    assert_eq!(get("x-paseo-edge-token").as_deref(), Some(TOKEN));
    assert_eq!(get("x-paseo-edge-transport").as_deref(), Some("direct"));
    assert_eq!(get("x-forwarded-for").as_deref(), Some("127.0.0.1"));
    let remote = get("x-paseo-edge-remote-addr").unwrap();
    assert!(remote.starts_with("127.0.0.1:"), "{remote}");
}

#[tokio::test]
async fn backend_rejection_reaches_client_unchanged() {
    let backend = spawn_backend(
        BackendOpts {
            reject: Some((403, "Host not allowed")),
            ..Default::default()
        },
        echo,
    )
    .await;
    let edge = spawn_edge(proxy_for(backend, Limits::default())).await;
    let req = format!("ws://{edge}/ws").into_client_request().unwrap();
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 403);
            assert_eq!(resp.body().as_deref(), Some(&b"Host not allowed"[..]));
        }
        other => panic!("expected http rejection, got {:?}", other.map(|_| ())),
    }
}

#[tokio::test]
async fn unreachable_backend_is_502() {
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    let edge = spawn_edge(proxy_for(addr, Limits::default())).await;
    let req = format!("ws://{edge}/ws").into_client_request().unwrap();
    match tokio_tungstenite::connect_async(req).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 502);
        }
        other => panic!("expected 502, got {:?}", other.map(|_| ())),
    }
}

/// Backend that sends server_info, records every inbound text frame, and reports when it ends.
fn recording_backend(
    seen: mpsc::UnboundedSender<String>,
    ended: mpsc::UnboundedSender<Option<(u16, String)>>,
) -> impl Fn(Conn) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static {
    move |mut c: Conn| {
        let seen = seen.clone();
        let ended = ended.clone();
        Box::pin(async move {
            let _ = c.ws.send(WsMsg::Text(SERVER_INFO.into())).await;
            loop {
                match c.ws.next().await {
                    Some(Ok(WsMsg::Text(t))) => {
                        let _ = seen.send(t.as_str().to_owned());
                    }
                    Some(Ok(WsMsg::Close(f))) => {
                        let _ = ended
                            .send(f.map(|f| (u16::from(f.code), f.reason.as_str().to_owned())));
                        return;
                    }
                    Some(Ok(_)) => {}
                    _ => {
                        let _ = ended.send(None);
                        return;
                    }
                }
            }
        })
    }
}

async fn expect_terminated(c: &mut Client, within: Duration) {
    let r = tokio::time::timeout(within, async {
        loop {
            match c.next().await {
                None | Some(Err(_)) | Some(Ok(WsMsg::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(r.is_ok(), "socket was not terminated");
}

#[tokio::test]
async fn ping_is_answered_by_edge_and_lease_expires_without_traffic() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let (end_tx, mut end_rx) = mpsc::unbounded_channel();
    let backend = spawn_backend(BackendOpts::default(), recording_backend(seen_tx, end_tx)).await;
    let edge = spawn_edge(proxy_for(backend, short_limits())).await;
    let mut c = connect(edge, &[]).await;
    assert!(matches!(next_frame(&mut c).await, WsMsg::Text(t) if t.as_str() == SERVER_INFO));

    c.send(WsMsg::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    assert!(
        matches!(next_frame(&mut c).await, WsMsg::Text(t) if t.as_str() == r#"{"type":"pong"}"#)
    );
    c.send(WsMsg::Text(r#"{"type":"other"}"#.into()))
        .await
        .unwrap();
    assert_eq!(seen_rx.recv().await.unwrap(), r#"{"type":"other"}"#);
    assert!(
        seen_rx.try_recv().is_err(),
        "ping must not reach the backend"
    );

    expect_terminated(&mut c, Duration::from_secs(3)).await;
    let end = tokio::time::timeout(Duration::from_secs(3), end_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(end.is_none_or(|(code, _)| code == 1000));
}

#[tokio::test]
async fn any_inbound_frame_renews_the_lease() {
    let (seen_tx, _seen_rx) = mpsc::unbounded_channel();
    let (end_tx, mut end_rx) = mpsc::unbounded_channel();
    let backend = spawn_backend(BackendOpts::default(), recording_backend(seen_tx, end_tx)).await;
    let edge = spawn_edge(proxy_for(backend, short_limits())).await;
    let mut c = connect(edge, &[]).await;
    next_frame(&mut c).await;
    c.send(WsMsg::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    next_frame(&mut c).await;
    // 8 x 150ms = 1.2s of non-ping traffic, three times the lease.
    for i in 0..8u8 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        c.send(WsMsg::Binary(vec![0x02, i].into())).await.unwrap();
    }
    assert!(end_rx.try_recv().is_err(), "lease must have been renewed");
    expect_terminated(&mut c, Duration::from_secs(3)).await;
}

#[tokio::test]
async fn legacy_socket_that_never_pings_is_never_closed() {
    let backend = spawn_backend(BackendOpts::default(), echo).await;
    let edge = spawn_edge(proxy_for(backend, short_limits())).await;
    let mut c = connect(edge, &[]).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    c.send(WsMsg::Text("still here".into())).await.unwrap();
    assert!(matches!(next_frame(&mut c).await, WsMsg::Text(t) if t.as_str() == "still here"));
}

#[tokio::test]
async fn ping_before_server_info_is_forwarded() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let backend = spawn_backend(BackendOpts::default(), move |mut c: Conn| {
        let seen = seen_tx.clone();
        async move {
            while let Some(Ok(WsMsg::Text(t))) = c.ws.next().await {
                let _ = seen.send(t.as_str().to_owned());
            }
        }
    })
    .await;
    let edge = spawn_edge(proxy_for(backend, short_limits())).await;
    let mut c = connect(edge, &[]).await;
    c.send(WsMsg::Text(r#"{"type":"ping"}"#.into()))
        .await
        .unwrap();
    assert_eq!(seen_rx.recv().await.unwrap(), r#"{"type":"ping"}"#);
}

// ---- mock physical socket (relay-like) ----

struct MockSocket {
    rx: mpsc::UnboundedReceiver<Message>,
    tx: mpsc::UnboundedSender<Message>,
    stuck: bool,
    closed: Arc<Mutex<Option<(u16, String)>>>,
}

struct MockHandle {
    to_edge: mpsc::UnboundedSender<Message>,
    from_edge: mpsc::UnboundedReceiver<Message>,
    closed: Arc<Mutex<Option<(u16, String)>>>,
}

fn mock(stuck: bool) -> (MockSocket, MockHandle) {
    let (to_edge, rx) = mpsc::unbounded_channel();
    let (tx, from_edge) = mpsc::unbounded_channel();
    let closed = Arc::new(Mutex::new(None));
    (
        MockSocket {
            rx,
            tx,
            stuck,
            closed: closed.clone(),
        },
        MockHandle {
            to_edge,
            from_edge,
            closed,
        },
    )
}

impl PhysicalSocket for MockSocket {
    async fn recv(&mut self) -> Option<Result<Message>> {
        self.rx.recv().await.map(Ok)
    }
    async fn send(&mut self, msg: Message) -> Result<()> {
        if self.stuck {
            std::future::pending::<()>().await;
        }
        self.tx
            .send(msg)
            .map_err(|_| EdgeError::Relay("mock closed".into()))
    }
    async fn close(&mut self, code: u16, reason: String) {
        *self.closed.lock().unwrap() = Some((code, reason));
    }
}

fn relay_info() -> ClientInfo {
    ClientInfo {
        transport: Transport::Relay {
            connection_id: "conn-1".into(),
        },
        forwarded_headers: vec![],
    }
}

#[tokio::test]
async fn high_water_terminates_only_the_slow_socket() {
    // Backend streams 600-byte frames slowly; budget is 1000 bytes.
    let (end_tx, mut end_rx) = mpsc::unbounded_channel::<()>();
    let backend = spawn_backend(BackendOpts::default(), move |mut c: Conn| {
        let end_tx = end_tx.clone();
        async move {
            for _ in 0..6 {
                if c.ws
                    .send(WsMsg::Binary(vec![7u8; 600].into()))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            while let Some(Ok(_)) = c.ws.next().await {}
            let _ = end_tx.send(());
        }
    })
    .await;
    let limits = Limits {
        outbound_high_water_bytes: 1000,
        ..Limits::default()
    };
    let proxy = proxy_for(backend, limits);

    let (slow, slow_h) = mock(true);
    let (fast, mut fast_h) = mock(false);
    let p1 = proxy.clone();
    let slow_task = tokio::spawn(async move { p1.run(slow, relay_info()).await });
    let p2 = proxy.clone();
    let fast_task = tokio::spawn(async move { p2.run(fast, relay_info()).await });

    let outcome = tokio::time::timeout(Duration::from_secs(5), slow_task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(outcome, ProxyOutcome::HighWater { buffered } if buffered > 1000),
        "{outcome:?}"
    );
    assert!(
        slow_h.closed.lock().unwrap().is_none(),
        "terminated, not closed gracefully"
    );

    for _ in 0..6 {
        let m = tokio::time::timeout(Duration::from_secs(5), fast_h.from_edge.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.byte_len(), 600);
    }
    drop(fast_h.to_edge);
    let fast_outcome = tokio::time::timeout(Duration::from_secs(5), fast_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fast_outcome, ProxyOutcome::ClientClosed);
    // Both internal sockets end.
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(5), end_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn single_frame_over_high_water_is_rejected() {
    let backend = spawn_backend(BackendOpts::default(), |mut c: Conn| async move {
        let _ = c.ws.send(WsMsg::Binary(vec![1u8; 100].into())).await;
        while let Some(Ok(_)) = c.ws.next().await {}
    })
    .await;
    let limits = Limits {
        outbound_high_water_bytes: 99,
        ..Limits::default()
    };
    let (sock, _h) = mock(false);
    let out = proxy_for(backend, limits).run(sock, relay_info()).await;
    assert!(
        matches!(out, ProxyOutcome::HighWater { buffered: 100 }),
        "{out:?}"
    );
}

// ---- close propagation ----

async fn read_close(c: &mut Client) -> Option<(u16, String)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c.next().await {
                Some(Ok(WsMsg::Close(f))) => {
                    return f.map(|f| (u16::from(f.code), f.reason.as_str().to_owned()))
                }
                Some(Ok(_)) => {}
                _ => return None,
            }
        }
    })
    .await
    .expect("close timeout")
}

#[tokio::test]
async fn backend_close_code_and_reason_reach_client() {
    for code in [4001u16, 4002, 4003, 4401, 1001] {
        let backend = spawn_backend(BackendOpts::default(), move |mut c: Conn| async move {
            let _ =
                c.ws.close(Some(CloseFrame {
                    code: CloseCode::from(code),
                    reason: "because".into(),
                }))
                .await;
            while let Some(Ok(_)) = c.ws.next().await {}
        })
        .await;
        let edge = spawn_edge(proxy_for(backend, Limits::default())).await;
        let mut c = connect(edge, &[]).await;
        assert_eq!(read_close(&mut c).await, Some((code, "because".to_owned())));
    }
}

#[tokio::test]
async fn backend_drop_without_close_frame_becomes_1011() {
    let backend = spawn_backend(BackendOpts::default(), |c: Conn| async move {
        // Drop the TCP stream without a close handshake.
        drop(c);
    })
    .await;
    let edge = spawn_edge(proxy_for(backend, Limits::default())).await;
    let mut c = connect(edge, &[]).await;
    assert_eq!(read_close(&mut c).await.map(|(code, _)| code), Some(1011));
}

#[tokio::test]
async fn client_close_code_reaches_backend() {
    let (seen_tx, _seen_rx) = mpsc::unbounded_channel();
    let (end_tx, mut end_rx) = mpsc::unbounded_channel();
    let backend = spawn_backend(BackendOpts::default(), recording_backend(seen_tx, end_tx)).await;
    let edge = spawn_edge(proxy_for(backend, Limits::default())).await;
    let mut c = connect(edge, &[]).await;
    next_frame(&mut c).await;
    c.close(Some(CloseFrame {
        code: CloseCode::Away,
        reason: "bye".into(),
    }))
    .await
    .unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), end_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end, Some((1001, "bye".to_owned())));
}

#[tokio::test]
async fn client_vanishing_closes_backend_with_1000() {
    let (seen_tx, _seen_rx) = mpsc::unbounded_channel();
    let (end_tx, mut end_rx) = mpsc::unbounded_channel();
    let backend = spawn_backend(BackendOpts::default(), recording_backend(seen_tx, end_tx)).await;
    let (sock, h) = mock(false);
    let proxy = proxy_for(backend, Limits::default());
    let task = tokio::spawn(async move { proxy.run(sock, relay_info()).await });
    drop(h.to_edge);
    let end = tokio::time::timeout(Duration::from_secs(5), end_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end.map(|(c, _)| c), Some(1000));
    assert_eq!(task.await.unwrap(), ProxyOutcome::ClientClosed);
}

// ---- dispatch hook ----

#[derive(Debug)]
struct Handler;

impl Dispatcher for Handler {
    fn dispatch(&self, _d: Domain, msg: Message, reply: &Replier) -> Dispatch {
        match msg {
            Message::Text(t) if t.contains("keep_in_ts") => Dispatch::Forward(Message::Text(t)),
            _ => {
                reply.send(Message::Text("handled-in-rust".into()));
                Dispatch::Handled
            }
        }
    }
}

#[tokio::test]
async fn owned_types_dispatch_instead_of_forwarding() {
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let (end_tx, _end_rx) = mpsc::unbounded_channel();
    let backend = spawn_backend(BackendOpts::default(), recording_backend(seen_tx, end_tx)).await;
    let mut table = OwnedTypes::empty();
    table.claim_type("fetch_agents_request", Domain::Placeholder);
    table.claim_type("keep_in_ts", Domain::Placeholder);
    let proxy = WsProxy::new_fixed(
        format!("ws://{backend}/ws"),
        TOKEN,
        Arc::new(table),
        Limits::default(),
    )
    .with_dispatcher(Arc::new(Handler));
    let (sock, mut h) = mock(false);
    tokio::spawn(async move { proxy.run(sock, relay_info()).await });
    // server_info first so the client could ping; not needed here.
    assert!(matches!(h.from_edge.recv().await, Some(Message::Text(t)) if t == SERVER_INFO));

    let owned = r#"{"type":"session","message":{"type":"fetch_agents_request"}}"#;
    let kept = r#"{"type":"session","message":{"type":"keep_in_ts"}}"#;
    let other = r#"{"type":"session","message":{"type":"unclaimed"}}"#;
    for t in [owned, kept, other] {
        h.to_edge.send(Message::Text(t.into())).unwrap();
    }
    assert!(matches!(h.from_edge.recv().await, Some(Message::Text(t)) if t == "handled-in-rust"));
    assert_eq!(seen_rx.recv().await.unwrap(), kept);
    assert_eq!(seen_rx.recv().await.unwrap(), other);
    assert!(
        seen_rx.try_recv().is_err(),
        "owned frame must not reach the backend"
    );
}
