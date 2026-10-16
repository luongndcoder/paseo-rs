//! Proxy and static web UI tests against a fake backend. No TS involved.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use bytes::Bytes;
use http_body_util::BodyExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::backend::BackendState;
use crate::config::{BackendConfig, EdgeConfig, Limits, RelayConfig, WebUiConfig};
use crate::routing::OwnedTypes;

const TOKEN: &str = "test-token";

fn headers_json(headers: &HeaderMap) -> String {
    let map: std::collections::BTreeMap<String, String> = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("?").to_string()))
        .collect();
    serde_json::to_string(&map).unwrap()
}

async fn fake_backend() -> SocketAddr {
    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route(
            "/echo",
            any(|req: Request| async move { headers_json(req.headers()) }),
        )
        .route(
            "/body",
            any(|req: Request| async move {
                let bytes = req.into_body().collect().await.unwrap().to_bytes();
                format!("len={}", bytes.len())
            }),
        )
        .route(
            "/missing",
            any(|| async { (StatusCode::NOT_FOUND, [("x-custom", "kept")], "nope") }),
        )
        .route(
            "/redirect",
            any(|| async { (StatusCode::FOUND, [("location", "/elsewhere")], "") }),
        )
        .route(
            "/big",
            get(|| async { Body::from(vec![7u8; 5 * 1024 * 1024]) }),
        )
        .route(
            "/stream",
            get(|| async {
                let (tx, rx) =
                    tokio::sync::mpsc::channel::<std::result::Result<Bytes, std::io::Error>>(1);
                tokio::spawn(async move {
                    let _ = tx.send(Ok(Bytes::from("data: one\n\n"))).await;
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    let _ = tx.send(Ok(Bytes::from("data: two\n\n"))).await;
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(tokio_stream_from(rx)))
                    .unwrap()
            }),
        )
        .fallback(
            |req: Request| async move { format!("backend:{}", req.uri().path()).into_response() },
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn tokio_stream_from(
    mut rx: tokio::sync::mpsc::Receiver<std::result::Result<Bytes, std::io::Error>>,
) -> impl futures_util::Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

fn edge_config(web_ui: Option<WebUiConfig>) -> Arc<EdgeConfig> {
    Arc::new(EdgeConfig {
        listen: ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()),
        paseo_home: std::env::temp_dir(),
        web_ui,
        limits: Limits::default(),
        backend: BackendConfig::new(vec!["x".into()]),
        relay: RelayConfig {
            enabled: false,
            endpoint: "r:1".into(),
            use_tls: false,
        },
        attach: None,
    })
}

async fn start_edge(backend: Arc<Backend>, web_ui: Option<WebUiConfig>) -> SocketAddr {
    let ws = Arc::new(WsProxy::new(
        backend.clone(),
        Arc::new(OwnedTypes::empty()),
        Limits::default(),
    ));
    let state = AppState::new(edge_config(web_ui), backend, ws);
    let listener = bind(&ListenAddr::Tcp("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr: SocketAddr = listener.public_address().parse().unwrap();
    tokio::spawn(serve_on(state, listener, std::future::pending()));
    addr
}

async fn edge_with_fake() -> SocketAddr {
    let backend_addr = fake_backend().await;
    start_edge(Backend::fixed(backend_addr, TOKEN), None).await
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> Reply {
    let client = proxy::new_client();
    let mut builder = http::Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path}"));
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = client.request(builder.body(body).unwrap()).await.unwrap();
    let (parts, incoming) = response.into_parts();
    let body = incoming.collect().await.unwrap().to_bytes();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

async fn fetch(addr: SocketAddr, path: &str, headers: &[(&str, &str)]) -> Reply {
    request(addr, "GET", path, headers, Body::empty()).await
}

#[tokio::test]
async fn forwards_identity_and_strips_forgeries() {
    let edge = edge_with_fake().await;
    let reply = fetch(
        edge,
        "/echo?x=1",
        &[
            ("x-paseo-edge-token", "forged"),
            ("x-paseo-edge-remote-addr", "6.6.6.6:1"),
            ("x-paseo-edge-transport", "relay"),
            ("x-paseo-edge-connection-id", "forged"),
            ("x-forwarded-for", "9.9.9.9"),
            ("authorization", "Bearer abc"),
            ("origin", "http://evil.test"),
            ("x-keep", "yes"),
            ("connection", "x-drop-me"),
            ("x-drop-me", "1"),
        ],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let seen: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&reply.text()).unwrap();
    assert_eq!(seen["x-paseo-edge-token"], TOKEN);
    assert_eq!(seen["x-paseo-edge-transport"], "direct");
    assert!(seen["x-paseo-edge-remote-addr"].starts_with("127.0.0.1:"));
    assert!(!seen.contains_key("x-paseo-edge-connection-id"));
    assert_eq!(seen["x-forwarded-for"], "127.0.0.1");
    assert_eq!(seen["authorization"], "Bearer abc");
    assert_eq!(seen["origin"], "http://evil.test");
    assert_eq!(seen["x-keep"], "yes");
    assert!(!seen.contains_key("x-drop-me"));
    // Host is preserved as the client sent it (the edge's own address here).
    assert_eq!(seen["host"], edge.to_string());
}

#[tokio::test]
async fn host_header_is_preserved_verbatim() {
    let edge = edge_with_fake().await;
    let reply = fetch(edge, "/echo", &[("host", "svc.localhost:6767")]).await;
    let seen: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&reply.text()).unwrap();
    assert_eq!(seen["host"], "svc.localhost:6767");
}

#[tokio::test]
async fn relays_status_headers_and_does_not_follow_redirects() {
    let edge = edge_with_fake().await;
    let reply = fetch(edge, "/missing", &[]).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.header("x-custom"), Some("kept"));
    assert_eq!(reply.text(), "nope");

    let reply = fetch(edge, "/redirect", &[]).await;
    assert_eq!(reply.status, StatusCode::FOUND);
    assert_eq!(reply.header("location"), Some("/elsewhere"));
}

#[tokio::test]
async fn streams_request_and_response_bodies() {
    let edge = edge_with_fake().await;
    let reply = request(
        edge,
        "POST",
        "/body",
        &[],
        Body::from(vec![1u8; 3 * 1024 * 1024]),
    )
    .await;
    assert_eq!(reply.text(), format!("len={}", 3 * 1024 * 1024));

    let reply = fetch(edge, "/big", &[]).await;
    assert_eq!(reply.body.len(), 5 * 1024 * 1024);
    assert!(reply.body.iter().all(|b| *b == 7));
}

#[tokio::test]
async fn sse_chunks_are_not_buffered() {
    let edge = edge_with_fake().await;
    let client = proxy::new_client();
    let started = std::time::Instant::now();
    let response = client
        .request(
            http::Request::builder()
                .uri(format!("http://{edge}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&first[..], b"data: one\n\n");
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "first chunk waited for the whole stream: {:?}",
        started.elapsed()
    );
    let second = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(&second[..], b"data: two\n\n");
}

#[tokio::test]
async fn not_ready_backend_gives_503() {
    let backend = Backend::with_state(BackendState::Starting, TOKEN);
    let edge = start_edge(backend, None).await;
    let reply = fetch(edge, "/api/status", &[]).await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.text(), r#"{"error":"Server not ready"}"#);
    let reply = fetch(
        edge,
        "/ws",
        &[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ],
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn dead_backend_gives_502() {
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = dead.local_addr().unwrap();
    drop(dead);
    let edge = start_edge(Backend::fixed(addr, TOKEN), None).await;
    let reply = fetch(edge, "/anything", &[]).await;
    assert_eq!(reply.status, StatusCode::BAD_GATEWAY);
}

/// Raw backend: records the request head, answers `101`, then upper-cases what it receives.
async fn fake_upgrade_backend() -> (SocketAddr, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(String::new()));
    let seen_inner = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let seen = seen_inner.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                *seen.lock().unwrap() = String::from_utf8_lossy(&head).into_owned();
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: abc\r\nSec-WebSocket-Protocol: paseo.bearer.x\r\n\r\n",
                    )
                    .await;
                let mut buf = [0u8; 256];
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        return;
                    }
                    let upper = buf[..n].to_ascii_uppercase();
                    if sock.write_all(&upper).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, seen)
}

async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

#[tokio::test]
async fn upgrade_is_tunneled_as_raw_bytes() {
    for path in ["/svc/socket"] {
        let (backend_addr, seen) = fake_upgrade_backend().await;
        let edge = start_edge(Backend::fixed(backend_addr, TOKEN), None).await;
        let mut stream = TcpStream::connect(edge).await.unwrap();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: svc.localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: paseo.bearer.x\r\nOrigin: http://localhost\r\nx-paseo-edge-remote-addr: forged\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let head = read_head(&mut stream).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("sec-websocket-accept: abc"));
        assert!(lower.contains("sec-websocket-protocol: paseo.bearer.x"));
        assert!(lower.contains("upgrade: websocket"));

        stream.write_all(b"hello \x00\xff binary").await.unwrap();
        let mut echoed = [0u8; 15];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"HELLO \x00\xff BINARY");

        let seen = seen.lock().unwrap().to_ascii_lowercase();
        assert!(seen.contains(&format!("get {path} http/1.1")), "{seen}");
        assert!(seen.contains("host: svc.localhost"));
        assert!(seen.contains("sec-websocket-key: dghlihnhbxbszsbub25jzq=="));
        assert!(seen.contains("origin: http://localhost"));
        assert!(seen.contains("x-paseo-edge-token: test-token"));
        assert!(seen.contains("x-paseo-edge-remote-addr: 127.0.0.1:"));
        assert!(!seen.contains("forged"));
    }
}

struct Dist {
    _dir: tempfile::TempDir,
    cfg: WebUiConfig,
}

fn dist() -> Dist {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("index.html"),
        "<html><head><title>x</title></head><body>root</body></html>",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("assets/app-0123456789abcdef.js"), "plain-js").unwrap();
    std::fs::write(root.join("assets/app-0123456789abcdef.js.br"), "br-js").unwrap();
    std::fs::write(root.join("assets/app-0123456789abcdef.js.gz"), "gz-js").unwrap();
    std::fs::write(root.join("style.css"), "body{}").unwrap();
    std::fs::write(root.join("style.css.gz"), "gz-css").unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/index.html"), "<p>docs</p>").unwrap();
    let cfg = WebUiConfig {
        dist_dir: root.to_path_buf(),
        label: "my<box>".into(),
    };
    Dist { _dir: dir, cfg }
}

async fn edge_with_ui(dist: &Dist) -> SocketAddr {
    let backend_addr = fake_backend().await;
    start_edge(Backend::fixed(backend_addr, TOKEN), Some(dist.cfg.clone())).await
}

#[tokio::test]
async fn static_index_has_no_cache_and_connection_hint() {
    let d = dist();
    let edge = edge_with_ui(&d).await;
    let reply = fetch(edge, "/", &[("host", "daemon.local")]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("cache-control"),
        Some("no-store, no-cache, must-revalidate, proxy-revalidate")
    );
    assert_eq!(reply.header("pragma"), Some("no-cache"));
    assert_eq!(reply.header("expires"), Some("0"));
    assert_eq!(
        reply.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        reply.text(),
        "<html><head><title>x</title><script>window.__PASEO_INITIAL_DAEMON_CONNECTION__={\"listen\":\"daemon.local:80\",\"useTls\":false,\"label\":\"my\\u003Cbox\\u003E\"}</script></head><body>root</body></html>"
    );

    let reply = fetch(
        edge,
        "/",
        &[("host", "daemon.local"), ("x-forwarded-proto", "https")],
    )
    .await;
    assert!(reply
        .text()
        .contains("\"listen\":\"daemon.local:443\",\"useTls\":true"));
}

#[tokio::test]
async fn static_spa_fallback_and_directory_index() {
    let d = dist();
    let edge = edge_with_ui(&d).await;
    let reply = fetch(edge, "/some/client/route", &[]).await;
    assert!(reply.text().contains("<body>root</body>"));
    let reply = fetch(edge, "/docs/", &[]).await;
    assert!(reply.text().contains("<p>docs</p>"));
    // Traversal is clamped to the dist dir, so it falls back to index.html.
    let mut stream = TcpStream::connect(edge).await.unwrap();
    stream
        .write_all(b"GET /../../../etc/passwd HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(raw.contains("<body>root</body>"), "{raw}");
    assert!(!raw.contains("root:"));
}

#[tokio::test]
async fn static_cache_headers_and_precompressed_variants() {
    let d = dist();
    let edge = edge_with_ui(&d).await;

    let reply = fetch(edge, "/assets/app-0123456789abcdef.js", &[]).await;
    assert_eq!(reply.text(), "plain-js");
    assert_eq!(
        reply.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(
        reply.header("content-type"),
        Some("application/javascript; charset=utf-8")
    );
    assert_eq!(reply.header("content-encoding"), None);

    let reply = fetch(
        edge,
        "/assets/app-0123456789abcdef.js",
        &[("accept-encoding", "gzip, br")],
    )
    .await;
    assert_eq!(reply.text(), "br-js");
    assert_eq!(reply.header("content-encoding"), Some("br"));
    assert_eq!(reply.header("vary"), Some("Accept-Encoding"));
    assert_eq!(
        reply.header("content-type"),
        Some("application/javascript; charset=utf-8")
    );

    let reply = fetch(edge, "/style.css", &[("accept-encoding", "gzip")]).await;
    assert_eq!(reply.text(), "gz-css");
    assert_eq!(reply.header("content-encoding"), Some("gzip"));
    assert_eq!(reply.header("cache-control"), Some("no-cache"));
    assert_eq!(
        reply.header("content-type"),
        Some("text/css; charset=utf-8")
    );

    // No br sibling: falls back to the plain file even though br was requested.
    let reply = fetch(edge, "/style.css", &[("accept-encoding", "br")]).await;
    assert_eq!(reply.text(), "body{}");
    assert_eq!(reply.header("content-encoding"), None);
}

#[tokio::test]
async fn static_head_and_non_get_and_excluded_paths() {
    let d = dist();
    let edge = edge_with_ui(&d).await;

    let reply = request(edge, "HEAD", "/style.css", &[], Body::empty()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.is_empty());
    assert_eq!(reply.header("cache-control"), Some("no-cache"));

    for path in [
        "/api/status",
        "/mcp/agents",
        "/public/x.txt",
        "/api",
        "/mcp",
        "/public",
    ] {
        let reply = fetch(edge, path, &[]).await;
        assert_eq!(reply.text(), format!("backend:{path}"), "{path}");
    }
    let reply = request(edge, "POST", "/style.css", &[], Body::empty()).await;
    assert_eq!(reply.text(), "backend:/style.css");
}

#[tokio::test]
async fn static_missing_dist_index_is_404() {
    let dir = tempfile::tempdir().unwrap();
    let ui = WebUiConfig {
        dist_dir: dir.path().to_path_buf(),
        label: "l".into(),
    };
    let backend_addr = fake_backend().await;
    let edge = start_edge(Backend::fixed(backend_addr, TOKEN), Some(ui)).await;
    let reply = fetch(edge, "/anything", &[]).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_web_ui_config_everything_is_proxied() {
    let edge = edge_with_fake().await;
    let reply = fetch(edge, "/index.html", &[]).await;
    assert_eq!(reply.text(), "backend:/index.html");
}

#[cfg(unix)]
#[tokio::test]
async fn serves_on_a_unix_socket_and_unlinks_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edge.sock");
    std::fs::write(&path, "stale").unwrap();
    let backend_addr = fake_backend().await;
    let backend = Backend::fixed(backend_addr, TOKEN);
    let ws = Arc::new(WsProxy::new(
        backend.clone(),
        Arc::new(OwnedTypes::empty()),
        Limits::default(),
    ));
    let state = AppState::new(edge_config(None), backend, ws);
    let listener = bind(&ListenAddr::Unix(path.clone())).await.unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_on(state, listener, async {
        let _ = stop_rx.await;
    }));

    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"GET /hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).await.unwrap();
    assert!(raw.contains("backend:/hello"), "{raw}");

    let _ = stop_tx.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!path.exists());
}
