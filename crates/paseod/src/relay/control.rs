//! Control-channel pieces of `relay-transport.ts`: URL building, message parsing, backoff,
//! and the control socket session loop.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::time::{interval, sleep_until, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::dial::dial;
use super::Inner;
use crate::error::{EdgeError, Result};

/// `CURRENT_RELAY_PROTOCOL_VERSION`.
pub const RELAY_PROTOCOL_VERSION: &str = "2";
pub const CONTROL_PING_INTERVAL: Duration = Duration::from_millis(10_000);
pub const CONTROL_STALE_TIMEOUT: Duration = Duration::from_millis(30_000);
pub const CONTROL_READY_TIMEOUT: Duration = Duration::from_millis(8_000);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    Sync { connection_ids: Vec<String> },
    Connected { connection_id: String },
    Disconnected { connection_id: String },
    Ping,
    Pong,
}

/// `tryParseControlMessage`.
pub fn parse_control_message(raw: &[u8]) -> Option<ControlMessage> {
    let text = String::from_utf8_lossy(raw);
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let obj = parsed.as_object()?;
    let trimmed_id = |v: Option<&Value>| -> Option<String> {
        let s = v?.as_str()?.trim();
        (!s.is_empty()).then(|| s.to_owned())
    };
    match obj.get("type")?.as_str()? {
        "ping" => Some(ControlMessage::Ping),
        "pong" => Some(ControlMessage::Pong),
        "sync" => {
            let ids = obj.get("connectionIds")?.as_array()?;
            Some(ControlMessage::Sync {
                connection_ids: ids
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .collect(),
            })
        }
        "connected" => Some(ControlMessage::Connected {
            connection_id: trimmed_id(obj.get("connectionId"))?,
        }),
        "disconnected" => Some(ControlMessage::Disconnected {
            connection_id: trimmed_id(obj.get("connectionId"))?,
        }),
        _ => None,
    }
}

/// `Math.min(30000, 1000 * attempt)`.
pub fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_millis(u64::min(30_000, 1000 * u64::from(attempt)))
}

/// `parseHostPort`: `host:port` or `[ipv6]:port`.
fn parse_host_port(input: &str) -> Result<(String, u16, bool)> {
    let bad = |m: &str| EdgeError::Relay(m.to_owned());
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(bad("Host is required"));
    }
    let (host, port_str, ipv6) = if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, tail) = rest
            .split_once("]:")
            .ok_or_else(|| bad("Invalid host:port (expected [::1]:6767)"))?;
        (host, tail, true)
    } else {
        let (host, port) = trimmed
            .rsplit_once(':')
            .ok_or_else(|| bad("Invalid host:port (expected localhost:6767)"))?;
        (host, port, false)
    };
    let host = host.trim();
    if host.is_empty() {
        return Err(bad("Host is required"));
    }
    if port_str.is_empty() || port_str.len() > 5 || !port_str.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("Invalid host:port"));
    }
    let port: u16 = port_str
        .parse()
        .ok()
        .filter(|p| *p >= 1)
        .ok_or_else(|| bad("Invalid host:port"))?;
    Ok((host.to_owned(), port, ipv6))
}

/// `buildRelayWebSocketUrl` for `role=server`.
pub fn build_relay_url(
    endpoint: &str,
    use_tls: bool,
    server_id: &str,
    connection_id: Option<&str>,
) -> Result<String> {
    let (host, port, ipv6) = parse_host_port(endpoint)?;
    let scheme = if use_tls { "wss" } else { "ws" };
    let host_part = if ipv6 { format!("[{host}]") } else { host };
    let mut url = url::Url::parse(&format!("{scheme}://{host_part}:{port}/ws"))
        .map_err(|e| EdgeError::Relay(format!("invalid relay endpoint: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("serverId", server_id);
        q.append_pair("role", "server");
        q.append_pair("v", RELAY_PROTOCOL_VERSION);
        if let Some(id) = connection_id.filter(|s| !s.is_empty()) {
            q.append_pair("connectionId", id);
        }
    }
    Ok(url.to_string())
}

/// Reconnect loop (`connectControl` + `scheduleReconnect`). Runs until `Inner::stopped`.
pub(crate) async fn control_loop(inner: std::sync::Arc<Inner>) {
    let mut attempt: u32 = 0;
    let mut seq: u64 = 0;
    let mut stop = inner.stop_rx();
    loop {
        if inner.is_stopped() {
            return;
        }
        seq += 1;
        let url = match build_relay_url(
            &inner.cfg.endpoint,
            inner.cfg.use_tls,
            &inner.server_id,
            None,
        ) {
            Ok(u) => u,
            Err(e) => {
                tracing::error!(error = %e, "relay_control_bad_endpoint");
                return;
            }
        };
        match dial(&url).await {
            Ok(ws) => {
                let ready = run_session(&inner, ws, seq).await;
                if ready {
                    attempt = 0;
                }
            }
            Err(e) => tracing::warn!(connection = seq, error = %e, "relay_control_connect_failed"),
        }
        if inner.is_stopped() {
            return;
        }
        attempt += 1;
        let delay = backoff_delay(attempt);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.changed() => {}
        }
    }
}

/// One control connection. Returns whether it ever became ready.
async fn run_session(inner: &std::sync::Arc<Inner>, ws: super::dial::WsStream, seq: u64) -> bool {
    let (mut sink, mut stream) = ws.split();
    let mut stop = inner.stop_rx();
    // A stop that landed before `subscribe` is invisible to `changed()`; check the flag too.
    if inner.is_stopped() {
        let _ = sink.close().await;
        return false;
    }
    let opened = Instant::now();
    let mut last_seen = opened;
    let mut ready = false;
    let ready_deadline = opened + CONTROL_READY_TIMEOUT;
    let mut ticker = interval(CONTROL_PING_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tracing::debug!(connection = seq, "relay_control_open_waiting_for_ready");
    loop {
        tokio::select! {
            _ = stop.changed() => {
                let _ = sink.close().await;
                return ready;
            }
            _ = sleep_until(ready_deadline), if !ready => {
                tracing::warn!(connection = seq, waited_ms = CONTROL_READY_TIMEOUT.as_millis() as u64, "relay_control_ready_timeout_terminating");
                return ready;
            }
            _ = ticker.tick() => {
                if last_seen.elapsed() > CONTROL_STALE_TIMEOUT {
                    tracing::warn!(connection = seq, stale_ms = last_seen.elapsed().as_millis() as u64, "relay_control_stale_terminating");
                    return ready;
                }
                if sink.send(WsMessage::Ping(Default::default())).await.is_err() {
                    tracing::warn!(connection = seq, "relay_control_ping_send_failed");
                    return ready;
                }
            }
            frame = stream.next() => {
                let Some(Ok(frame)) = frame else {
                    tracing::warn!(connection = seq, "relay_control_disconnected");
                    return ready;
                };
                match frame {
                    WsMessage::Pong(_) => { last_seen = Instant::now(); }
                    WsMessage::Ping(_) => { let _ = sink.flush().await; }
                    WsMessage::Close(f) => {
                        tracing::warn!(connection = seq, code = f.as_ref().map(|c| u16::from(c.code)), "relay_control_disconnected");
                        return ready;
                    }
                    WsMessage::Text(t) => {
                        last_seen = Instant::now();
                        handle_control(inner, &mut sink, t.as_bytes(), &mut ready, seq).await;
                    }
                    WsMessage::Binary(b) => {
                        last_seen = Instant::now();
                        handle_control(inner, &mut sink, &b, &mut ready, seq).await;
                    }
                    WsMessage::Frame(_) => {}
                }
            }
        }
    }
}

async fn handle_control<S>(
    inner: &std::sync::Arc<Inner>,
    sink: &mut S,
    raw: &[u8],
    ready: &mut bool,
    seq: u64,
) where
    S: futures_util::Sink<WsMessage> + Unpin,
{
    let Some(msg) = parse_control_message(raw) else {
        return;
    };
    if !*ready {
        *ready = true;
        tracing::info!(connection = seq, "relay_control_connected");
    }
    match msg {
        ControlMessage::Ping => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = sink
                .send(WsMessage::text(format!(r#"{{"type":"pong","ts":{ts}}}"#)))
                .await;
        }
        ControlMessage::Pong => {}
        ControlMessage::Sync { connection_ids } => {
            for id in connection_ids {
                inner.ensure_client_data_socket(&id);
            }
        }
        ControlMessage::Connected { connection_id } => {
            inner.ensure_client_data_socket(&connection_id)
        }
        ControlMessage::Disconnected { connection_id } => inner.disconnect_client(&connection_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_control_messages() {
        assert_eq!(
            parse_control_message(br#"{"type":"ping"}"#),
            Some(ControlMessage::Ping)
        );
        assert_eq!(
            parse_control_message(br#"{"type":"pong","ts":1}"#),
            Some(ControlMessage::Pong)
        );
        assert_eq!(
            parse_control_message(br#"{"type":"sync","connectionIds":["a"," ","b",3]}"#),
            Some(ControlMessage::Sync {
                connection_ids: vec!["a".into(), "b".into()]
            })
        );
        assert_eq!(
            parse_control_message(br#"{"type":"connected","connectionId":"  c1 "}"#),
            Some(ControlMessage::Connected {
                connection_id: "c1".into()
            })
        );
        assert_eq!(
            parse_control_message(br#"{"type":"connected","connectionId":" "}"#),
            None
        );
        assert_eq!(parse_control_message(br#"{"type":"sync"}"#), None);
        assert_eq!(parse_control_message(b"nope"), None);
        assert_eq!(parse_control_message(br#"[1]"#), None);
    }

    #[test]
    fn backoff_is_linear_capped() {
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(7), Duration::from_secs(7));
        assert_eq!(backoff_delay(30), Duration::from_secs(30));
        assert_eq!(backoff_delay(500), Duration::from_secs(30));
    }

    #[test]
    fn rejects_bad_endpoints() {
        assert!(build_relay_url("nohost", true, "s", None).is_err());
        assert!(build_relay_url("host:0", true, "s", None).is_err());
        assert!(build_relay_url("host:70000", true, "s", None).is_err());
        assert!(build_relay_url("[::1]", true, "s", None).is_err());
    }
}
