//! Raw TCP hubs for the network remote tests: a minimal HTTP/1.1 server whose behavior the
//! test scripts per request, and an upgrade-rejecting hub (TS `UpgradeRejectingHub`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

#[derive(Debug, Clone)]
pub struct RawRequest {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

pub enum Reply {
    Status(u16),
    Json(u16, String),
    Stall,
}

pub struct RawServer {
    pub addr: std::net::SocketAddr,
    pub requests: Arc<Mutex<Vec<RawRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RawServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RawServer {
    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub fn requests(&self) -> Vec<RawRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub async fn start(handler: impl Fn(&RawRequest) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<RawRequest>>> = Arc::default();
        let log = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, handler) = (log.clone(), handler.clone());
                tokio::spawn(async move {
                    let Some(request) = read_request(stream, |s| async move { s }).await else {
                        return;
                    };
                    let (mut stream, request) = request;
                    log.lock().unwrap().push(request.clone());
                    match handler(&request) {
                        Reply::Status(status) => respond(&mut stream, status, "").await,
                        Reply::Json(status, body) => respond(&mut stream, status, &body).await,
                        Reply::Stall => {
                            let mut sink = [0u8; 64];
                            while stream.read(&mut sink).await.is_ok_and(|n| n > 0) {}
                        }
                    }
                });
            }
        });
        Self {
            addr,
            requests,
            task,
        }
    }
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let text = format!(
        "HTTP/1.1 {status} Reply\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn read_request<F, Fut>(mut stream: TcpStream, _wrap: F) -> Option<(TcpStream, RawRequest)>
where
    F: FnOnce(TcpStream) -> Fut,
    Fut: std::future::Future<Output = TcpStream>,
{
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let path = first.next()?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buffer[header_end..]).to_string();
    Some((
        stream,
        RawRequest {
            method,
            path,
            headers,
            body,
        },
    ))
}

/// What the upgrade endpoint does on its Nth attempt.
#[derive(Clone, Copy)]
pub enum UpgradeBehavior {
    Status(u16),
    /// Close the TCP connection without answering.
    Destroy,
    Stall,
}

/// Enrolls any daemon and answers every WebSocket upgrade with a scripted failure.
pub struct UpgradeRejectingHub {
    pub addr: std::net::SocketAddr,
    pub attempts: Arc<AtomicUsize>,
    pub open: Arc<AtomicUsize>,
    released: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for UpgradeRejectingHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl UpgradeRejectingHub {
    pub async fn start(behaviors: Vec<UpgradeBehavior>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(Notify::new());
        let (a, o, r) = (attempts.clone(), open.clone(), released.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (attempts, open, released) = (a.clone(), o.clone(), r.clone());
                let behaviors = behaviors.clone();
                tokio::spawn(async move {
                    let Some((mut stream, request)) =
                        read_request(stream, |s| async move { s }).await
                    else {
                        return;
                    };
                    if request.method == "POST" {
                        let enrollment: serde_json::Value =
                            serde_json::from_str(&request.body).unwrap();
                        let body = serde_json::json!({
                            "daemonId": enrollment["daemonId"],
                            "permissions": ["hub.execute"],
                            "webSocketUrl": format!("ws://127.0.0.1:{}/daemon", addr.port()),
                        })
                        .to_string();
                        respond(&mut stream, 200, &body).await;
                        return;
                    }
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    open.fetch_add(1, Ordering::SeqCst);
                    let behavior = behaviors
                        .get(attempt - 1)
                        .or(behaviors.last())
                        .copied()
                        .unwrap_or(UpgradeBehavior::Status(503));
                    match behavior {
                        UpgradeBehavior::Stall => {
                            let mut sink = [0u8; 64];
                            while stream.read(&mut sink).await.is_ok_and(|n| n > 0) {}
                        }
                        UpgradeBehavior::Destroy => drop(stream),
                        UpgradeBehavior::Status(status) => {
                            let text = format!(
                                "HTTP/1.1 {status} Rejected\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndeny"
                            );
                            let _ = stream.write_all(text.as_bytes()).await;
                            let _ = stream.shutdown().await;
                            // Wait for the client to hang up, so "released" means released.
                            let mut sink = [0u8; 64];
                            while stream.read(&mut sink).await.is_ok_and(|n| n > 0) {}
                        }
                    }
                    open.fetch_sub(1, Ordering::SeqCst);
                    released.notify_waiters();
                });
            }
        });
        Self {
            addr,
            attempts,
            open,
            released,
            task,
        }
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }

    pub fn web_socket_url(&self) -> String {
        format!("ws://127.0.0.1:{}/daemon", self.addr.port())
    }

    pub fn attempt_count(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// Waits until attempt `n` arrived and every connection is closed again.
    pub async fn expect_attempt_released(&self, n: usize) {
        for _ in 0..4000 {
            if self.attempt_count() >= n && self.open_connections() == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("Hub socket attempt {n} remained open");
    }
}
