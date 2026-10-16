//! End to end: real `RelayHandle` against an in-process fake relay server over loopback,
//! with a test client speaking the E2EE protocol.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use paseod::config::RelayConfig;
use paseod::relay::crypto::SharedKey;
use paseod::relay::{b64, DaemonKeyPair, RelayHandle, RelaySocket, SocketHandler};
use paseod::ws::{ClientInfo, Message, PhysicalSocket, Transport};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message as Ws;
use tokio_tungstenite::WebSocketStream;

type Server = WebSocketStream<TcpStream>;

const WAIT: Duration = Duration::from_secs(10);

fn pub_of(secret: [u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(secret, x25519_dalek::X25519_BASEPOINT_BYTES)
}

fn keypair() -> DaemonKeyPair {
    let secret: [u8; 32] = rand::random();
    let public = pub_of(secret);
    DaemonKeyPair {
        public_key: public,
        secret_key: secret,
    }
}

struct Accepted {
    query: String,
    ws: Server,
}

async fn accept_loop(listener: TcpListener, tx: mpsc::UnboundedSender<Accepted>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut query = String::new();
            let cb = |req: &Request, resp: Response| {
                query = req.uri().query().unwrap_or_default().to_owned();
                assert_eq!(req.uri().path(), "/ws");
                Ok(resp)
            };
            if let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, cb).await {
                let _ = tx.send(Accepted { query, ws });
            }
        });
    }
}

struct Echo {
    seen: mpsc::UnboundedSender<ClientInfo>,
}

impl SocketHandler for Echo {
    fn handle(
        &self,
        mut socket: RelaySocket,
        info: ClientInfo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let _ = self.seen.send(info);
        Box::pin(async move {
            while let Some(Ok(m)) = socket.recv().await {
                if socket.send(m).await.is_err() {
                    break;
                }
            }
        })
    }
}

struct Client {
    ws: Server,
    key: SharedKey,
    binary: bool,
}

impl Client {
    async fn handshake(mut ws: Server, daemon_public: [u8; 32], binary: bool) -> (Self, String) {
        let secret: [u8; 32] = rand::random();
        let public = pub_of(secret);
        let caps = if binary {
            r#","capabilities":{"binaryCiphertext":true}"#
        } else {
            ""
        };
        let hello = format!(
            r#"{{"type":"e2ee_hello","key":"{}"{caps}}}"#,
            b64::encode(&public)
        );
        ws.send(Ws::text(hello)).await.unwrap();
        let ready = match timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap() {
            Ws::Text(t) => t.as_str().to_owned(),
            other => panic!("expected ready text, got {other:?}"),
        };
        let key = SharedKey::derive(&secret, &daemon_public).unwrap();
        (Self { ws, key, binary }, ready)
    }

    async fn send(&mut self, m: Message) {
        let wire = match m {
            Message::Text(t) => Ws::text(b64::encode(&self.key.encrypt(t.as_bytes()))),
            Message::Binary(b) if self.binary => Ws::Binary(Bytes::from(self.key.encrypt(&b))),
            Message::Binary(b) => Ws::text(b64::encode(&self.key.encrypt(&b))),
        };
        self.ws.send(wire).await.unwrap();
    }

    async fn recv(&mut self) -> Message {
        loop {
            match timeout(WAIT, self.ws.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                Ws::Text(t) => {
                    let bundle = b64::decode_lenient(t.as_str()).unwrap();
                    return Message::Text(
                        String::from_utf8(self.key.decrypt(&bundle).unwrap()).unwrap(),
                    );
                }
                Ws::Binary(b) => {
                    return Message::Binary(Bytes::from(self.key.decrypt(&b).unwrap()))
                }
                Ws::Ping(_) | Ws::Pong(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}

async fn next_accept(rx: &mut mpsc::UnboundedReceiver<Accepted>) -> Accepted {
    timeout(WAIT, rx.recv())
        .await
        .expect("accept timeout")
        .unwrap()
}

async fn setup() -> (
    RelayHandle,
    DaemonKeyPair,
    mpsc::UnboundedReceiver<Accepted>,
    mpsc::UnboundedReceiver<ClientInfo>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(accept_loop(listener, tx));
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    let keys = keypair();
    let handle = RelayHandle::start_with_handler(
        RelayConfig {
            enabled: true,
            endpoint: format!("127.0.0.1:{port}"),
            use_tls: false,
        },
        keys.clone(),
        "srv-test".to_owned(),
        Arc::new(Echo { seen: seen_tx }),
        64 * 1024 * 1024,
    )
    .await
    .unwrap();
    (handle, keys, rx, seen_rx)
}

#[tokio::test]
async fn relay_serves_clients_end_to_end() {
    let (handle, keys, mut accepted, mut seen) = setup().await;

    // Control socket.
    let mut control = next_accept(&mut accepted).await;
    assert_eq!(control.query, "serverId=srv-test&role=server&v=2");
    // Daemon pings immediately (keepalive).
    match timeout(WAIT, control.ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    {
        Ws::Ping(_) => {}
        other => panic!("expected ping, got {other:?}"),
    }
    // JSON ping gets a pong with ts.
    control
        .ws
        .send(Ws::text(r#"{"type":"ping"}"#))
        .await
        .unwrap();
    match timeout(WAIT, control.ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    {
        Ws::Text(t) => {
            let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap();
            assert_eq!(v["type"], "pong");
            assert!(v["ts"].is_u64());
        }
        other => panic!("expected pong text, got {other:?}"),
    }

    // sync -> data socket c1.
    control
        .ws
        .send(Ws::text(r#"{"type":"sync","connectionIds":["c1"]}"#))
        .await
        .unwrap();
    let data = next_accept(&mut accepted).await;
    assert_eq!(
        data.query,
        "serverId=srv-test&role=server&v=2&connectionId=c1"
    );
    let (mut c1, ready) = Client::handshake(data.ws, keys.public_key, true).await;
    assert_eq!(
        ready,
        r#"{"type":"e2ee_ready","capabilities":{"binaryCiphertext":true}}"#
    );

    let info = timeout(WAIT, seen.recv()).await.unwrap().unwrap();
    assert_eq!(
        info.transport,
        Transport::Relay {
            connection_id: "c1".into()
        }
    );
    assert!(info.forwarded_headers.is_empty());

    // Echo text + binary through the encrypted channel.
    c1.send(Message::Text("héllo 🌍".into())).await;
    assert_eq!(c1.recv().await, Message::Text("héllo 🌍".into()));
    c1.send(Message::Binary(Bytes::from_static(&[1, 2, 3, 0, 255])))
        .await;
    assert_eq!(
        c1.recv().await,
        Message::Binary(Bytes::from_static(&[1, 2, 3, 0, 255]))
    );

    // `connected` for a second client; garbage hello -> 1011 "E2EE handshake failed".
    control
        .ws
        .send(Ws::text(r#"{"type":"connected","connectionId":"c2"}"#))
        .await
        .unwrap();
    let mut bad = next_accept(&mut accepted).await;
    assert!(bad.query.ends_with("connectionId=c2"));
    bad.ws.send(Ws::text("not a hello")).await.unwrap();
    match timeout(WAIT, bad.ws.next()).await.unwrap() {
        Some(Ok(Ws::Close(Some(f)))) => {
            assert_eq!(u16::from(f.code), 1011);
            assert_eq!(f.reason.as_str(), "E2EE handshake failed");
        }
        other => panic!("expected close 1011, got {other:?}"),
    }

    // `disconnected` closes c1 with 1001.
    control
        .ws
        .send(Ws::text(r#"{"type":"disconnected","connectionId":"c1"}"#))
        .await
        .unwrap();
    loop {
        match timeout(WAIT, c1.ws.next()).await.unwrap() {
            Some(Ok(Ws::Close(Some(f)))) => {
                assert_eq!(u16::from(f.code), 1001);
                assert_eq!(f.reason.as_str(), "Client disconnected");
                break;
            }
            Some(Ok(_)) => {}
            other => panic!("expected close 1001, got {other:?}"),
        }
    }

    handle.stop().await.unwrap();
    // Control socket is closed by stop.
    let end = timeout(WAIT, async {
        loop {
            match control.ws.next().await {
                Some(Ok(Ws::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(end.is_ok(), "control socket not closed on stop");
}

#[tokio::test]
async fn relay_rehello_and_key_rotation() {
    let (handle, keys, mut accepted, _seen) = setup().await;
    let mut control = next_accept(&mut accepted).await;
    control
        .ws
        .send(Ws::text(r#"{"type":"connected","connectionId":"r1"}"#))
        .await
        .unwrap();
    let data = next_accept(&mut accepted).await;

    // Legacy peer (no capabilities): base64 text frames both ways.
    let secret: [u8; 32] = rand::random();
    let public = pub_of(secret);
    let hello = format!(
        r#"{{"type":"e2ee_hello","key":"{}"}}"#,
        b64::encode(&public)
    );
    let mut ws = data.ws;
    ws.send(Ws::text(hello.clone())).await.unwrap();
    match timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap() {
        Ws::Text(t) => assert_eq!(t.as_str(), r#"{"type":"e2ee_ready"}"#),
        other => panic!("{other:?}"),
    }
    let key = SharedKey::derive(&secret, &keys.public_key).unwrap();
    ws.send(Ws::text(b64::encode(&key.encrypt(b"one"))))
        .await
        .unwrap();
    let echo = |m: Ws| match m {
        Ws::Text(t) => key
            .decrypt(&b64::decode_lenient(t.as_str()).unwrap())
            .unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        echo(timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap()),
        b"one"
    );

    // Same key re-hello: ready again, session unharmed.
    ws.send(Ws::text(hello)).await.unwrap();
    match timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap() {
        Ws::Text(t) => assert_eq!(t.as_str(), r#"{"type":"e2ee_ready"}"#),
        other => panic!("{other:?}"),
    }
    ws.send(Ws::text(b64::encode(&key.encrypt(b"two"))))
        .await
        .unwrap();
    assert_eq!(
        echo(timeout(WAIT, ws.next()).await.unwrap().unwrap().unwrap()),
        b"two"
    );

    // Different key: 1008.
    let other_pub = pub_of(rand::random::<[u8; 32]>());
    ws.send(Ws::text(format!(
        r#"{{"type":"e2ee_hello","key":"{}"}}"#,
        b64::encode(&other_pub)
    )))
    .await
    .unwrap();
    match timeout(WAIT, ws.next()).await.unwrap() {
        Some(Ok(Ws::Close(Some(f)))) => {
            assert_eq!(u16::from(f.code), 1008);
            assert_eq!(f.reason.as_str(), "E2EE re-handshake key mismatch");
        }
        other => panic!("expected 1008, got {other:?}"),
    }
    handle.stop().await.unwrap();
}

#[tokio::test]
async fn control_reconnects_after_drop() {
    let (handle, _keys, mut accepted, _seen) = setup().await;
    let mut control = next_accept(&mut accepted).await;
    // Become ready, then drop the socket: the transport must dial again (backoff 1 s).
    control
        .ws
        .send(Ws::text(r#"{"type":"ping"}"#))
        .await
        .unwrap();
    drop(control);
    let again = timeout(Duration::from_secs(5), accepted.recv())
        .await
        .expect("no reconnect")
        .unwrap();
    assert_eq!(again.query, "serverId=srv-test&role=server&v=2");
    handle.stop().await.unwrap();
}
