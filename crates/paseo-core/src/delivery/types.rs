//! Envelope types of the delivery layer: who an outbound frame is for, who proves it may go
//! there, and the sink that finally writes it.
//!
//! TS proves ownership with an `AsyncLocalStorage` request context and a `WeakMap` keyed by the
//! message object. Rust carries the proof in the envelope instead: every [`Outbound`] names its
//! target [`SourceId`] and, for a modern source, the [`OwnerId`] that is allowed to speak to it.
//! The only place that enforces it is the socket writer ([`crate::delivery::SourceGate::permits`]).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

/// One physical socket. Allocated by the edge (a new id per socket, never reused).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u64);

impl SourceId {
    /// The source TS falls back to when a request has no socket (`defaultSource`).
    pub const DEFAULT: SourceId = SourceId(0);
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "source-{}", self.0)
    }
}

#[derive(Debug)]
pub(crate) struct OwnerState {
    pub(crate) id: u64,
    pub(crate) source: SourceId,
    pub(crate) active: AtomicBool,
}

/// Proof that a frame may reach a modern source: an owner (a request, a subscription or an
/// operation) of that source that is still active when the frame is written.
#[derive(Debug, Clone)]
pub struct OwnerId(pub(crate) Arc<OwnerState>);

impl OwnerId {
    pub fn source(&self) -> SourceId {
        self.0.source
    }

    pub fn is_active(&self) -> bool {
        self.0.active.load(Ordering::SeqCst)
    }

    pub(crate) fn deactivate(&self) {
        self.0.active.store(false, Ordering::SeqCst);
    }
}

impl PartialEq for OwnerId {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

/// An outbound WebSocket data frame. Text is stringified once and shared (`Arc<str>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(Arc<str>),
    Binary(Arc<[u8]>),
}

impl Frame {
    pub fn byte_len(&self) -> usize {
        match self {
            Frame::Text(t) => t.len(),
            Frame::Binary(b) => b.len(),
        }
    }
}

/// What the socket writer receives.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub to: SourceId,
    /// `None` is an unowned broadcast: only a legacy source accepts it.
    pub owner: Option<OwnerId>,
    pub body: Frame,
    /// The frame is the session `server_info` status. A modern source accepts exactly one such
    /// frame without proof, during the handshake.
    pub server_info: bool,
}

impl Outbound {
    /// Wraps a session message into the `{"type":"session","message":...}` envelope and
    /// stringifies it (lone-surrogate carriers are written as `\udXXX` escapes, Q4).
    pub fn session_message(to: SourceId, owner: Option<OwnerId>, message: &Value) -> Self {
        let server_info = is_server_info_message(message);
        let envelope = serde_json::json!({ "type": "session", "message": message });
        Self {
            to,
            owner,
            body: Frame::Text(Arc::from(paseo_agent_types::js_str::to_js_json_string(
                &envelope,
            ))),
            server_info,
        }
    }

    pub fn binary(to: SourceId, owner: Option<OwnerId>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            to,
            owner,
            body: Frame::Binary(bytes.into()),
            server_info: false,
        }
    }
}

pub(crate) fn is_server_info_message(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("status")
        && message
            .get("payload")
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            == Some("server_info")
}

/// Writes frames to physical sockets. Implemented by the edge: its writer is the sole
/// enforcement point of [`crate::delivery::SourceGate::permits`] and must evaluate it when
/// `send` is called (not later), because request proofs expire when the request ends.
pub trait OutboundSink: Send + Sync + 'static {
    fn send(&self, out: Outbound);
}

/// The request a handler runs for: type and `requestId`, what `isReply` correlates against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestInfo {
    pub ty: String,
    pub request_id: Option<String>,
}

impl RequestInfo {
    pub fn new(ty: impl Into<String>, request_id: Option<&str>) -> Self {
        Self {
            ty: ty.into(),
            request_id: request_id.map(str::to_owned),
        }
    }

    /// Reads `type` and `requestId` of an inbound session message.
    pub fn from_message(message: &Value) -> Self {
        Self {
            ty: message
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            request_id: message
                .get("requestId")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }
}

/// Failures of the delivery API. The text is the TS `Error` message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    #[error("Session delivery is closed")]
    Closed,
    #[error("Cannot renegotiate subscription ownership on an active source")]
    Renegotiate,
    #[error("Subscription source is closed")]
    SubscriptionSourceClosed,
    #[error("Operation source is closed")]
    OperationSourceClosed,
    #[error("Owned subscriptions require a requestId")]
    RequestIdRequired,
    #[error("Subscription IDs are assigned by the host")]
    HostAssignedId,
    #[error("Release subscriptions using the server-assigned subscription ID")]
    ReleaseByServerId,
    /// A teardown (`stop`) failed. Memoized: later releases report the same error.
    #[error("{0}")]
    Teardown(String),
}
