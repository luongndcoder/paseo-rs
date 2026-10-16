//! Public types of the relationship actor: status, command inputs, errors, the host seam and
//! the options.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use paseo_authz::Permission;
use serde::Serialize;

use crate::clock::{SharedClock, SharedRetryPolicy};
use crate::executions::HubExecutionAgents;
use crate::remote::{HubRemote, HubSocket, SessionProtocol};

/// `HubConnectionState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HubConnectionState {
    NotConnected,
    Connecting,
    Connected,
    Reconnecting,
    Disconnecting,
    Revoked,
}

/// `HubRelationshipStatus`. Serializes to the TS wire shape (camelCase keys, permission names).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubRelationshipStatus {
    pub state: HubConnectionState,
    pub daemon_id: Option<String>,
    pub hub_origin: Option<String>,
    #[serde(serialize_with = "serialize_permissions")]
    pub permissions: Vec<Permission>,
    pub connected_at: Option<String>,
    pub last_error: Option<String>,
}

fn serialize_permissions<S: serde::Serializer>(
    permissions: &[Permission],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(permissions.iter().map(|p| p.as_str()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectInput {
    pub hub_url: String,
    pub token: String,
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChange {
    pub grant: Vec<String>,
    pub revoke: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisconnectOutcome {
    pub status: HubRelationshipStatus,
    pub warning: Option<String>,
}

/// Failure of a relationship command. The messages are the TS error messages.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    #[error("{0}")]
    Message(String),
    /// The Hub rejected the enrollment (401/403): the pending ceremony was discarded.
    #[error("Hub enrollment failed ({status})")]
    EnrollmentRejected { status: u16 },
}

impl HubError {
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// What the actor needs from the session layer (TS `attachSocket`,
/// `updateAttachedPermissions`, `createExecutionAgents`).
#[async_trait]
pub trait HubSessionHost: Send + Sync + 'static {
    /// Hands an open daemon socket to the session layer. Runs detached from the actor.
    async fn attach_socket(
        &self,
        socket: Box<dyn HubSocket>,
        options: AttachOptions,
    ) -> Result<(), String>;

    /// Replaces the permission set of the session attached for `principal_id`.
    fn update_attached_permissions(&self, principal_id: &str, permissions: &[Permission]);

    /// One execution-agents instance per daemon id; the actor caches it until authority ends.
    fn create_execution_agents(&self, daemon_id: &str) -> Arc<dyn HubExecutionAgents>;
}

pub struct AttachOptions {
    pub daemon_id: String,
    /// `hub:<daemonId>:<base64url(sha256(credential))>`.
    pub principal_id: String,
    pub permissions: Vec<Permission>,
    pub agents: Arc<dyn HubExecutionAgents>,
    pub session_protocol: SessionProtocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Warn,
    Error,
}

/// The slice of the TS pino logger the actor uses. Never receives secrets.
pub trait HubLogger: Send + Sync + 'static {
    fn log(&self, level: LogLevel, message: &str, detail: Option<&str>);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TracingLogger;

impl HubLogger for TracingLogger {
    fn log(&self, level: LogLevel, message: &str, detail: Option<&str>) {
        let detail = detail.unwrap_or_default();
        match level {
            LogLevel::Warn => tracing::warn!(detail, "{message}"),
            LogLevel::Error => tracing::error!(detail, "{message}"),
        }
    }
}

/// The random identity of one enrollment ceremony.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ceremony {
    pub daemon_id: String,
    pub idempotency_key: String,
    /// base64url, 32 random bytes in production.
    pub secret: String,
}

pub struct HubOptions {
    pub paseo_home: PathBuf,
    pub hostname: String,
    pub server_id: String,
    pub daemon_public_key: String,
    pub logger: Arc<dyn HubLogger>,
    pub remote: Arc<dyn HubRemote>,
    pub clock: SharedClock,
    pub retry_policy: SharedRetryPolicy,
    /// Source of the ceremony identity. Production draws random values; tests pin them.
    pub create_ceremony: Option<Box<dyn Fn() -> Ceremony + Send + Sync>>,
    pub host: Arc<dyn HubSessionHost>,
}
