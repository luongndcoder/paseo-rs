//! Steps 2-3 of the direct flow: dial the internal WS with the client's headers.

use std::time::Duration;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::{BackendLink, ClientInfo, Transport, UpgradeRejection, WsProxy};
use crate::internal::{
    HEADER_CONNECTION_ID, HEADER_REMOTE_ADDR, HEADER_TOKEN, HEADER_TRANSPORT,
    STRIPPED_CLIENT_HEADERS,
};

pub type BackendStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Headers owned by the WS handshake itself or by the edge. Never copied from the client.
fn is_blocked_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "connection"
            | "upgrade"
            | "content-length"
            | "transfer-encoding"
            | "keep-alive"
            | "te"
            | "trailer"
            | "proxy-authorization"
            | "proxy-connection"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-extensions"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-forwarded-port"
            | "x-forwarded-prefix"
            | "forwarded"
    ) || STRIPPED_CLIENT_HEADERS.contains(&n.as_str())
}

fn reject(status: u16, body: &str) -> UpgradeRejection {
    UpgradeRejection {
        status,
        body: body.to_owned(),
    }
}

pub(super) async fn dial(
    proxy: &WsProxy,
    info: &ClientInfo,
) -> Result<BackendLink, UpgradeRejection> {
    let Some((url, token)) = proxy.upstream.target() else {
        return Err(reject(503, "Server not ready"));
    };
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|_| reject(502, "invalid backend url"))?;
    let headers = request.headers_mut();
    let mut seen = std::collections::HashSet::new();
    for (name, value) in &info.forwarded_headers {
        if is_blocked_header(name) {
            continue;
        }
        let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            continue;
        };
        // First occurrence replaces the handshake default (Host), repeats are appended.
        if seen.insert(n.clone()) {
            headers.insert(n, v);
        } else {
            headers.append(n, v);
        }
    }
    let mut set = |name: &'static str, value: &str| {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(HeaderName::from_static(name), v);
        }
    };
    set(HEADER_TOKEN, &token);
    match &info.transport {
        Transport::Direct { remote } => {
            set(HEADER_TRANSPORT, "direct");
            if let Some(addr) = remote {
                set(HEADER_REMOTE_ADDR, &addr.to_string());
                set("x-forwarded-for", &addr.ip().to_string());
            }
        }
        Transport::Relay { connection_id } => {
            set(HEADER_TRANSPORT, "relay");
            set(HEADER_CONNECTION_ID, connection_id);
        }
    }

    let max = proxy.limits.max_message_bytes;
    let config = WebSocketConfig::default()
        .max_message_size(Some(max))
        .max_frame_size(Some(max));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    match tokio::time::timeout(DIAL_TIMEOUT, connect).await {
        Err(_) => Err(reject(504, "backend dial timed out")),
        Ok(Ok((stream, response))) => {
            let selected_protocol = response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            Ok(BackendLink {
                selected_protocol,
                stream,
            })
        }
        Ok(Err(WsError::Http(resp))) => {
            let body = resp
                .body()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            Err(UpgradeRejection {
                status: resp.status().as_u16(),
                body,
            })
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "internal ws dial failed");
            Err(reject(502, "backend unavailable"))
        }
    }
}
