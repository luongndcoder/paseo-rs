//! Per-client data socket (`ensureClientDataSocket` + `attachEncryptedSocket`).

use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::SplitStream;
use futures_util::StreamExt;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::channel::{accept_hello, PROTOCOL_ERROR_CODE};
use super::control::build_relay_url;
use super::dial::{dial, WsStream};
use super::socket::{RelaySocket, Shutdown, Writer};
use super::Inner;
use crate::ws::{ClientInfo, Message, Transport};

/// Socket must be open within this long (`openTimeout`).
pub const DATA_OPEN_TIMEOUT: Duration = Duration::from_millis(15_000);
/// E2EE handshake must finish within this long (`handshakeTimeout`).
pub const E2EE_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(15_000);

pub(crate) async fn run_data_socket(
    inner: Arc<Inner>,
    connection_id: String,
    entry_id: u64,
    mut shutdown: watch::Receiver<Shutdown>,
) {
    run(&inner, &connection_id, &mut shutdown).await;
    inner.forget_data_socket(&connection_id, entry_id);
}

async fn run(inner: &Arc<Inner>, connection_id: &str, shutdown: &mut watch::Receiver<Shutdown>) {
    let url = match build_relay_url(
        &inner.cfg.endpoint,
        inner.cfg.use_tls,
        &inner.server_id,
        Some(connection_id),
    ) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(connection_id, error = %e, "relay_data_bad_endpoint");
            return;
        }
    };
    let ws = tokio::select! {
        r = tokio::time::timeout(DATA_OPEN_TIMEOUT, dial(&url)) => match r {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                tracing::warn!(connection_id, error = %e, "relay_data_error");
                return;
            }
            Err(_) => {
                tracing::warn!(connection_id, "relay_data_open_timeout_terminating");
                return;
            }
        },
        _ = shutdown.changed() => return,
    };
    tracing::info!(connection_id, "relay_data_connected");

    let (sink, mut stream) = ws.split();
    let writer = Writer::spawn(sink);

    let handshake = tokio::time::timeout(
        E2EE_HANDSHAKE_TIMEOUT,
        handshake(&mut stream, &writer, inner, shutdown),
    )
    .await;
    let channel = match handshake {
        Ok(Ok(channel)) => channel,
        Ok(Err(reason)) => {
            tracing::warn!(connection_id, reason, "relay_e2ee_handshake_failed");
            writer.close(Some((
                PROTOCOL_ERROR_CODE,
                "E2EE handshake failed".to_owned(),
            )));
            return;
        }
        Err(_) => {
            tracing::warn!(connection_id, "relay_e2ee_handshake_timeout_terminating");
            writer.abort();
            return;
        }
    };

    let socket = RelaySocket::new(
        stream,
        writer,
        channel,
        shutdown.clone(),
        inner.max_buffered,
    );
    let info = ClientInfo {
        transport: Transport::Relay {
            connection_id: connection_id.to_owned(),
        },
        forwarded_headers: Vec::new(),
    };
    inner.handler.handle(socket, info).await;
    tracing::info!(connection_id, "relay_data_disconnected");
}

/// First text frame must be `e2ee_hello`; answers `e2ee_ready`.
async fn handshake(
    stream: &mut SplitStream<WsStream>,
    writer: &Writer,
    inner: &Inner,
    shutdown: &mut watch::Receiver<Shutdown>,
) -> Result<super::channel::DaemonChannel, String> {
    loop {
        let frame = tokio::select! {
            f = stream.next() => f,
            _ = shutdown.changed() => return Err("closed before handshake".to_owned()),
        };
        let first = match frame {
            Some(Ok(WsMessage::Text(t))) => Message::Text(t.as_str().to_owned()),
            Some(Ok(WsMessage::Binary(b))) => Message::Binary(b),
            Some(Ok(WsMessage::Ping(_))) => {
                writer.flush();
                continue;
            }
            Some(Ok(WsMessage::Pong(_) | WsMessage::Frame(_))) => continue,
            Some(Ok(WsMessage::Close(f))) => {
                return Err(format!(
                    "Connection closed during handshake: {}",
                    f.map(|c| u16::from(c.code)).unwrap_or(1005)
                ))
            }
            Some(Err(e)) => return Err(e.to_string()),
            None => return Err("Connection closed during handshake".to_owned()),
        };
        let (channel, ready) = accept_hello(&inner.keys, &first).map_err(|e| e.0)?;
        writer.send_data(ready).map_err(|e| e.to_string())?;
        return Ok(channel);
    }
}
