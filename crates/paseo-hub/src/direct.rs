//! `DirectHubRelationshipRemote`: the network implementation of [`HubRemote`] (reqwest for the
//! three HTTP calls, tokio-tungstenite for the daemon socket). Every HTTP call is bounded by
//! one request timeout (15 s) covering connect, headers and body.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::record::{is_valid_web_socket_url, web_socket_matches_hub_origin};
use crate::remote::{
    HubEnrollment, HubEnrollmentResult, HubPermissionUpdate, HubRemote, HubRevocation, HubSocket,
    HubSocketConnection, HubSocketCredentials, HubSocketEvents, RemoteError, SessionProtocol,
};

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// JS `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
const URI_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

pub type HubWebSocketStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The socket the network remote hands to the session layer; downcast with
/// `Box<dyn Any>::downcast::<DirectHubSocket>()`. A pump task owns the stream: the session
/// layer reads `incoming` and writes `outgoing`, and the pump reports the end of the
/// connection through the dial's [`HubSocketEvents`].
pub struct DirectHubSocket {
    pub incoming: mpsc::UnboundedReceiver<Message>,
    pub outgoing: mpsc::UnboundedSender<Message>,
    closer: Arc<Notify>,
}

struct PumpCloser(Arc<Notify>);

impl HubSocketConnection for PumpCloser {
    fn close(&self) {
        // `notify_one` stores a permit, so closing before the pump first polls still lands.
        self.0.notify_one();
    }
}

impl HubSocket for DirectHubSocket {
    fn close(self: Box<Self>) {
        self.closer.notify_one();
    }

    fn close_handle(&self) -> Box<dyn HubSocketConnection> {
        Box::new(PumpCloser(self.closer.clone()))
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

fn spawn_pump(mut stream: HubWebSocketStream, events: Arc<dyn HubSocketEvents>) -> DirectHubSocket {
    let (in_tx, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let closer = Arc::new(Notify::new());
    let wait_close = closer.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = wait_close.notified() => {
                    let _ = stream.close(None).await;
                    return;
                }
                out = out_rx.recv() => match out {
                    Some(message) => {
                        if let Err(error) = stream.send(message).await {
                            return events.failed(error.to_string());
                        }
                    }
                    // The session layer dropped its end: the session is over.
                    None => {
                        let _ = stream.close(None).await;
                        return events.closed(1006);
                    }
                },
                next = stream.next() => match next {
                    Some(Ok(Message::Close(frame))) => {
                        return events.closed(frame.map_or(1005, |f| u16::from(f.code)));
                    }
                    Some(Ok(message)) => {
                        let _ = in_tx.send(message);
                    }
                    Some(Err(error)) => return events.failed(error.to_string()),
                    None => return events.closed(1006),
                },
            }
        }
    });
    DirectHubSocket {
        incoming,
        outgoing,
        closer,
    }
}

pub struct DirectHubRelationshipRemote {
    client: reqwest::Client,
    request_timeout: Duration,
}

impl DirectHubRelationshipRemote {
    pub fn new() -> Self {
        Self::with_request_timeout(DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn with_request_timeout(request_timeout: Duration) -> Self {
        Self {
            client: reqwest::Client::new(),
            request_timeout,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<(u16, Vec<u8>), RemoteError> {
        let response = request
            .timeout(self.request_timeout)
            .send()
            .await
            .map_err(map_request_error)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(map_request_error)?;
        Ok((status, body.to_vec()))
    }
}

impl Default for DirectHubRelationshipRemote {
    fn default() -> Self {
        Self::new()
    }
}

fn map_request_error(error: reqwest::Error) -> RemoteError {
    if error.is_timeout() {
        return RemoteError::failed("Hub request timed out");
    }
    RemoteError::failed(error.to_string())
}

fn is_ok(status: u16) -> bool {
    (200..300).contains(&status)
}

fn daemon_url(hub_origin: &str, daemon_id: &str) -> String {
    format!(
        "{hub_origin}/api/daemons/{}",
        utf8_percent_encode(daemon_id, URI_COMPONENT)
    )
}

fn string_array(value: &Value, what: &str) -> Result<Vec<String>, RemoteError> {
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| RemoteError::failed(format!("{what}: expected an array of strings")))
}

/// `EnrollmentResultSchema` plus `ensureWebSocketMatchesHubOrigin`. The messages are the
/// TS refinement messages (TS wraps them in a zod error whose text also contains them).
fn parse_enrollment(hub_origin: &str, body: &[u8]) -> Result<HubEnrollmentResult, RemoteError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| RemoteError::failed(format!("Hub enrollment response was not JSON: {e}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| RemoteError::failed("Hub enrollment response must be an object"))?;
    let daemon_id = object
        .get("daemonId")
        .and_then(Value::as_str)
        .ok_or_else(|| RemoteError::failed("Hub enrollment daemonId must be a string"))?;
    let permissions = string_array(
        object.get("permissions").unwrap_or(&Value::Null),
        "Hub enrollment permissions",
    )?;
    let web_socket_url = object
        .get("webSocketUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| RemoteError::failed("Hub enrollment webSocketUrl must be a string"))?;
    let parsed = url::Url::parse(web_socket_url)
        .map_err(|_| RemoteError::failed("Hub enrollment webSocketUrl must be a URL"))?;
    if !matches!(parsed.scheme(), "ws" | "wss") {
        return Err(RemoteError::failed("Hub WebSocket URL must use ws or wss"));
    }
    if parsed.fragment().is_some_and(|f| !f.is_empty()) {
        return Err(RemoteError::failed(
            "Hub WebSocket URL cannot include a fragment",
        ));
    }
    debug_assert!(is_valid_web_socket_url(web_socket_url));
    if !web_socket_matches_hub_origin(hub_origin, web_socket_url) {
        return Err(RemoteError::failed(
            "Hub WebSocket URL must match the Hub origin",
        ));
    }
    Ok(HubEnrollmentResult {
        daemon_id: daemon_id.to_owned(),
        permissions,
        web_socket_url: web_socket_url.to_owned(),
    })
}

#[async_trait]
impl HubRemote for DirectHubRelationshipRemote {
    async fn enroll(&self, input: HubEnrollment) -> Result<HubEnrollmentResult, RemoteError> {
        let body = json!({
            "daemonId": input.daemon_id,
            "idempotencyKey": input.idempotency_key,
            "hostname": input.hostname,
            "serverId": input.server_id,
            "daemonPublicKey": input.daemon_public_key,
            "credentialVerifier": input.credential_verifier,
            "permissions": input.permissions,
        });
        let request = self
            .client
            .post(format!("{}/api/daemons/enroll", input.hub_origin))
            .header("content-type", "application/json")
            .bearer_auth(&input.token)
            .body(body.to_string());
        let (status, bytes) = self.send(request).await?;
        if !is_ok(status) {
            if status == 401 || status == 403 {
                return Err(RemoteError::EnrollmentRejected { status });
            }
            return Err(RemoteError::failed(format!(
                "Hub enrollment failed ({status})"
            )));
        }
        parse_enrollment(&input.hub_origin, &bytes)
    }

    async fn update_permissions(
        &self,
        input: HubPermissionUpdate,
    ) -> Result<Vec<String>, RemoteError> {
        let request = self
            .client
            .patch(daemon_url(&input.hub_origin, &input.daemon_id))
            .header("content-type", "application/json")
            .bearer_auth(&input.credential)
            .body(json!({ "permissions": input.permissions }).to_string());
        let (status, bytes) = self.send(request).await?;
        if !is_ok(status) {
            return Err(RemoteError::failed(format!(
                "Hub permission update failed ({status})"
            )));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            RemoteError::failed(format!("Hub permission response was not JSON: {e}"))
        })?;
        string_array(
            value.get("permissions").unwrap_or(&Value::Null),
            "Hub permission response permissions",
        )
    }

    async fn revoke(&self, input: HubRevocation) -> Result<(), RemoteError> {
        let request = self
            .client
            .delete(daemon_url(&input.hub_origin, &input.daemon_id))
            .bearer_auth(&input.credential);
        let (status, _) = self.send(request).await?;
        if !is_ok(status) && ![401, 403, 404].contains(&status) {
            return Err(RemoteError::failed(format!(
                "Hub revocation failed ({status})"
            )));
        }
        Ok(())
    }

    fn open_socket(
        &self,
        input: HubSocketCredentials,
        events: Arc<dyn HubSocketEvents>,
    ) -> Box<dyn HubSocketConnection> {
        let timeout = self.request_timeout;
        Box::new(Dial(tokio::spawn(dial(input, events, timeout))))
    }
}

struct Dial(tokio::task::JoinHandle<()>);

impl HubSocketConnection for Dial {
    fn close(&self) {
        // Aborting the task drops the half-open connection; after `connected` the task has
        // finished and the actor holds the socket's own close handle instead.
        self.0.abort();
    }
}

async fn dial(input: HubSocketCredentials, events: Arc<dyn HubSocketEvents>, timeout: Duration) {
    let request = match build_request(&input) {
        Ok(request) => request,
        Err(message) => return events.failed(message),
    };
    match tokio::time::timeout(timeout, connect_async(request)).await {
        Err(_) => events.failed("Opening handshake has timed out".into()),
        Ok(Ok((stream, response))) => {
            let protocol = match response.headers().get("x-paseo-session-protocol") {
                Some(value) if value.as_bytes() == b"1" => SessionProtocol::SessionV1,
                _ => SessionProtocol::Legacy,
            };
            events.connected(Box::new(spawn_pump(stream, events.clone())), protocol);
        }
        Ok(Err(WsError::Http(response))) => match response.status().as_u16() {
            status @ (401 | 403) => events.rejected(status),
            _ => events.closed(1006),
        },
        Ok(Err(error)) => events.failed(error.to_string()),
    }
}

fn build_request(
    input: &HubSocketCredentials,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    let mut request = input
        .web_socket_url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    let headers = request.headers_mut();
    let bearer = HeaderValue::from_str(&format!("Bearer {}", input.credential))
        .map_err(|e| e.to_string())?;
    headers.insert("authorization", bearer);
    headers.insert(
        "x-paseo-daemon-id",
        HeaderValue::from_str(&input.daemon_id).map_err(|e| e.to_string())?,
    );
    headers.insert("x-paseo-session-protocol", HeaderValue::from_static("1"));
    Ok(request)
}
