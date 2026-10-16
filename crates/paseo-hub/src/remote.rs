//! The Hub as the relationship actor sees it (TS `relationship-remote.ts` interfaces): HTTP
//! enrollment, permission update and revocation, and the daemon WebSocket. Implemented over
//! the network by [`crate::direct::DirectHubRelationshipRemote`]; tests use fakes.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;

/// `HubEnrollment`. `permissions` are the wire names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubEnrollment {
    pub daemon_id: String,
    pub idempotency_key: String,
    pub hub_origin: String,
    pub token: String,
    pub hostname: String,
    pub server_id: String,
    pub daemon_public_key: String,
    pub credential_verifier: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubEnrollmentResult {
    pub daemon_id: String,
    pub permissions: Vec<String>,
    pub web_socket_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPermissionUpdate {
    pub daemon_id: String,
    pub hub_origin: String,
    pub credential: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubRevocation {
    pub daemon_id: String,
    pub hub_origin: String,
    pub credential: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubSocketCredentials {
    pub daemon_id: String,
    pub web_socket_url: String,
    pub credential: String,
}

/// Failure of a Hub HTTP call. `EnrollmentRejected` (401/403 on enroll) is terminal; every
/// other error is retryable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    #[error("Hub enrollment failed ({status})")]
    EnrollmentRejected { status: u16 },
    #[error("{0}")]
    Failed(String),
}

impl RemoteError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionProtocol {
    Legacy,
    SessionV1,
}

/// An open daemon WebSocket handed to the session layer. `into_any` lets the host downcast
/// to the concrete transport (the network remote hands over a tungstenite stream).
pub trait HubSocket: Send + 'static {
    fn close(self: Box<Self>);
    /// A handle that still closes the socket after the session layer took it over. The
    /// actor keeps it so `disconnect`, `stop` and exit end the live session (TS
    /// `this.socket?.close()`).
    fn close_handle(&self) -> Box<dyn HubSocketConnection>;
    fn into_any(self: Box<Self>) -> Box<dyn Any + Send>;
}

/// `HubSocketEvents`. Exactly one of `rejected` / `closed` / `failed` ends a dial that never
/// connected. After `connected` the socket itself keeps reporting: the first `closed(code)`
/// or `failed(message)` after the connection ends (a close the actor requested through the
/// close handle reports nothing).
pub trait HubSocketEvents: Send + Sync + 'static {
    fn connected(&self, socket: Box<dyn HubSocket>, protocol: SessionProtocol);
    fn rejected(&self, status: u16);
    fn closed(&self, code: u16);
    fn failed(&self, message: String);
}

/// The dial in progress or the open connection; `close` aborts the dial or closes the socket.
pub trait HubSocketConnection: Send {
    fn close(&self);
}

#[async_trait]
pub trait HubRemote: Send + Sync + 'static {
    async fn enroll(&self, input: HubEnrollment) -> Result<HubEnrollmentResult, RemoteError>;
    /// Returns the permission names the Hub now holds for this daemon.
    async fn update_permissions(
        &self,
        input: HubPermissionUpdate,
    ) -> Result<Vec<String>, RemoteError>;
    async fn revoke(&self, input: HubRevocation) -> Result<(), RemoteError>;
    fn open_socket(
        &self,
        input: HubSocketCredentials,
        events: Arc<dyn HubSocketEvents>,
    ) -> Box<dyn HubSocketConnection>;
}
