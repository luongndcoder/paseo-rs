//! Outbound WebSocket dial to the relay (`RELAY_WEBSOCKET_OPTIONS`: 10 s handshake timeout,
//! no per-message deflate).

use std::time::Duration;

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{connect_async_with_config, MaybeTlsStream, WebSocketStream};

use crate::config::MAX_RELAY_WEBSOCKET_MESSAGE_BYTES;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub const DIAL_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(10_000);

/// rustls 0.23 panics at `ClientConfig::builder()` without a process-wide provider, and
/// tokio-tungstenite pulls rustls without one. Install `ring` (no C toolchain needed) once.
fn ensure_tls_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // Err means another component already installed a provider; that one wins.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub(crate) async fn dial(url: &str) -> Result<WsStream, String> {
    ensure_tls_provider();
    let cfg = WebSocketConfig::default()
        .max_message_size(Some(MAX_RELAY_WEBSOCKET_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_RELAY_WEBSOCKET_MESSAGE_BYTES));
    match tokio::time::timeout(
        DIAL_HANDSHAKE_TIMEOUT,
        connect_async_with_config(url, Some(cfg), false),
    )
    .await
    {
        Ok(Ok((ws, _response))) => Ok(ws),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("handshake timeout".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `wss://` must reach the TLS handshake (and fail there) instead of panicking on a
    /// missing crypto provider.
    #[tokio::test]
    async fn wss_dial_does_not_panic_without_provider() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                drop(sock);
            }
        });
        let result = dial(&format!("wss://localhost:{port}/ws")).await;
        assert!(result.is_err());
    }

    /// Manual: `cargo test relay::dial -- --ignored` (needs internet). Proves webpki roots and
    /// the ring provider complete a real TLS + WS handshake.
    #[tokio::test]
    #[ignore]
    async fn live_wss_handshake() {
        let url = "wss://relay.paseo.sh/ws?serverId=paseod-probe&role=server&v=2";
        dial(url).await.expect("wss handshake with relay.paseo.sh");
    }
}
