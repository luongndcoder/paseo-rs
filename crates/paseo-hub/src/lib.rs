//! Hub relationship and executions (WP13): the Rust port of `packages/server/src/server/hub`.
//!
//! - [`HubRelationshipActor`] owns `hub-relationship.json` and the Hub socket: enrollment,
//!   reconnect with bounded jittered backoff, permission updates, revocation, disconnect.
//! - [`HubRemote`] is the Hub as the actor sees it; [`DirectHubRelationshipRemote`] is the
//!   network implementation. [`HubClock`] and [`RetryPolicy`] are injectable.
//! - [`DaemonExecutions`] is the idempotent, authority-fenced execution API over an
//!   [`ExecutionBackend`] (agent store + manager); [`HubExecutionController`] is its
//!   per-session `hub.execution.*` message handler.
//!
//! Behavior is the TS daemon's. Reproduced quirks are marked `PARITY-QUIRK(<name>)` and listed
//! in `docs/p4-core-spec.md` section 17.

pub mod actor;
pub mod backend;
pub mod clock;
pub mod controller;
pub mod direct;
pub mod executions;
mod pending;
pub mod record;
pub mod remote;
pub mod types;

pub use actor::{secret_fingerprint, HubHandle, HubRelationshipActor};
pub use backend::{
    CoreExecutionBackend, CreateProgress, CreatedExecution, CreatedWorktree, ExecError,
    ExecutionActions, ExecutionBackend, ExecutionCreateRequest, ExecutionWorktree, WorktreeAction,
};
pub use clock::{
    BoundedExponentialRetryPolicy, HubClock, RetryPolicy, ScheduledTask, SystemHubClock,
};
pub use controller::{
    ExecutionValidator, HubExecutionAgentCreateRequest, HubExecutionAgentValidateRequest,
    HubExecutionControlRequest, HubExecutionController, SendMessage, ValidateInput, WorktreeTarget,
};
pub use direct::{DirectHubRelationshipRemote, DirectHubSocket};
pub use executions::{
    ControlAction, DaemonExecutions, HubExecutionAgentCreateInput, HubExecutionAgents,
    HubExecutionControlInput, OwnedAgentEvent, OwnedAgentSnapshot, OwnedEventListener, Unsubscribe,
};
pub use record::{normalize_hub_url, Record, FILE_NAME};
pub use remote::{
    HubEnrollment, HubEnrollmentResult, HubPermissionUpdate, HubRemote, HubRevocation, HubSocket,
    HubSocketConnection, HubSocketCredentials, HubSocketEvents, RemoteError, SessionProtocol,
};
pub use types::{
    AttachOptions, Ceremony, ConnectInput, DisconnectOutcome, HubConnectionState, HubError,
    HubLogger, HubOptions, HubRelationshipStatus, HubSessionHost, LogLevel, PermissionChange,
    TracingLogger,
};
