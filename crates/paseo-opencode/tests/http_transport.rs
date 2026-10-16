//! `HttpTransport` against a tiny in-process HTTP server: JSON bodies, status mapping, basic auth,
//! directory encoding, and chunked SSE whose frames are split across TCP writes.

use futures_util::StreamExt;
use paseo_opencode::client::RawClient;
use paseo_opencode::http::HttpTransport;
use paseo_opencode::protocol::{Event, GlobalEvent};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn read_request(stream: &mut TcpStream) -> (String, String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "client closed early");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let length = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.unwrap();
        body.extend_from_slice(&chunk[..n]);
    }
    (head.lines().next().unwrap().to_owned(), head, body)
}

fn respond(status: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).into_bytes()
}

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (line, head, body) = read_request(&mut stream).await;
                if line.starts_with("GET /global/event") {
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                    stream.write_all(head.as_bytes()).await.unwrap();
                    let frames = [
                        "data: {\"payload\":{\"type\":\"server.connected\",\"properties\":{}}}\n\n",
                        "data: {\"directory\":\"/w\",\"payload\":{\"type\":\"session.idle\",\"properties\":{\"sessionID\":\"ses_1\"}}}\n\n",
                    ];
                    for frame in frames {
                        // split every frame in two TCP writes, each its own HTTP chunk
                        let (a, b) = frame.split_at(frame.len() / 2);
                        for part in [a, b] {
                            stream
                                .write_all(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes())
                                .await
                                .unwrap();
                            stream.flush().await.unwrap();
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    stream.write_all(b"0\r\n\r\n").await.unwrap();
                } else if line.starts_with("GET /session/missing") {
                    stream
                        .write_all(&respond("404 Not Found", "{\"name\":\"NotFoundError\"}"))
                        .await
                        .unwrap();
                } else if line.starts_with("POST /session/ses_1/prompt_async") {
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                        .await
                        .unwrap();
                } else {
                    // echo: request line, selected headers, body
                    let h = |name: &str| {
                        head.lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
                            })
                            .unwrap_or_default()
                    };
                    let echo = serde_json::json!({
                        "id": "ses_echo",
                        "title": line,
                        "authorization": h("authorization"),
                        "directory_header": h("x-opencode-directory"),
                        "body": String::from_utf8_lossy(&body),
                    });
                    stream
                        .write_all(&respond("200 OK", &echo.to_string()))
                        .await
                        .unwrap();
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn json_requests_carry_auth_directory_and_body() {
    let url = serve().await;
    let transport =
        Arc::new(HttpTransport::new(url, Some(("opencode".into(), "s3cret".into()))).unwrap());
    let client = RawClient::new(transport, Some("/tmp/my project".into()));
    let session = client.session_create(&Default::default()).await.unwrap();
    assert_eq!(session.id, "ses_echo");
    // `directory` is URI-encoded in the query and in the header, like the official SDK.
    assert_eq!(
        session.title.as_deref(),
        Some("POST /session?directory=%2Ftmp%2Fmy%20project HTTP/1.1")
    );
    assert_eq!(session.extra["directory_header"], "%2Ftmp%2Fmy%20project");
    // base64("opencode:s3cret")
    assert_eq!(session.extra["authorization"], "Basic b3BlbmNvZGU6czNjcmV0");
    assert_eq!(session.extra["body"], "");
}

#[tokio::test]
async fn status_codes_map_to_errors() {
    let url = serve().await;
    let client = RawClient::new(Arc::new(HttpTransport::new(url, None).unwrap()), None);
    let err = client.session_get("missing").await.unwrap_err();
    assert!(err.is_not_found(), "{err}");
    client
        .prompt_async(
            "ses_1",
            &paseo_opencode::protocol::PromptBody {
                parts: vec![],
                ..Default::default()
            },
        )
        .await
        .expect("204 is success");
}

#[tokio::test]
async fn sse_frames_split_across_chunks_are_reassembled() {
    let url = serve().await;
    let client = RawClient::new(Arc::new(HttpTransport::new(url, None).unwrap()), None);
    let mut stream = client.global_event().await.unwrap();
    let mut events = Vec::new();
    while let Some(frame) = stream.next().await {
        let frame = frame.unwrap();
        events.push(GlobalEvent::from_value(
            serde_json::from_str(&frame.data).unwrap(),
        ));
    }
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].payload.event, Event::ServerConnected));
    assert!(matches!(&events[1].payload.event, Event::SessionIdle(s) if s.session_id == "ses_1"));
    assert_eq!(events[1].directory.as_deref(), Some("/w"));
}
