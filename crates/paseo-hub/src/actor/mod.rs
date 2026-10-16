//! `HubRelationshipActor` (TS `relationship-controller.ts`): the single owner of
//! `hub-relationship.json` and of the Hub socket.
//!
//! TS runs every method to its first `await` atomically and lets the awaited calls (enroll,
//! permission update, revoke) interleave with other commands. Here one task owns all state;
//! each remote call runs in a spawned task and reports back as an event, and every event that
//! can be stale carries the `generation` / `enrollment_generation` it was started under.
//! Stale events no-op, exactly as the TS generation checks do.
//!
//! The status is published into an `ArcSwap` after every handled message and *before* a
//! reply is sent, so `HubHandle::status()` is synchronous like the TS `status()` and never
//! older than the caller's own awaited command.

mod disconnect;
mod enroll;
mod socket;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use paseo_authz::{parse_daemon_permissions, Permission};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use crate::clock::{HubClock, RetryPolicy, ScheduledTask};
use crate::executions::HubExecutionAgents;
use crate::record::{self, ActiveRecord, Loaded, PendingRecord, Record, FILE_NAME};
use crate::remote::{HubRemote, HubSocket, HubSocketConnection, RemoteError, SessionProtocol};
use crate::types::{
    Ceremony, ConnectInput, DisconnectOutcome, HubConnectionState, HubError, HubLogger, HubOptions,
    HubRelationshipStatus, HubSessionHost, LogLevel, PermissionChange,
};

type Reply<T> = oneshot::Sender<T>;
pub(crate) type StatusReply = Reply<Result<HubRelationshipStatus, HubError>>;
type DisconnectReply = Reply<Result<DisconnectOutcome, HubError>>;

// Messages are moved once through the mailbox; boxing the large events would only add a
// allocation per event.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Msg {
    Command(Command),
    Event(Event),
    /// The last handle was dropped.
    Exit,
}

pub(crate) enum Command {
    Start(StatusReply),
    Stop(Reply<()>),
    Connect(ConnectInput, StatusReply),
    UpdatePermissions(PermissionChange, StatusReply),
    Disconnect { force: bool, reply: DisconnectReply },
}

pub(crate) enum Event {
    EnrollmentSettled {
        attempt: u64,
        generation: u64,
        pending: PendingRecord,
        result: Result<crate::remote::HubEnrollmentResult, RemoteError>,
    },
    EnrollmentRetry {
        generation: u64,
        pending: PendingRecord,
    },
    SocketRetry {
        generation: u64,
        record: Arc<ActiveRecord>,
    },
    SocketConnected {
        generation: u64,
        record: Arc<ActiveRecord>,
        socket: Box<dyn HubSocket>,
        protocol: SessionProtocol,
    },
    SocketRejected {
        generation: u64,
        status: u16,
    },
    SocketClosed {
        generation: u64,
        record: Arc<ActiveRecord>,
        code: u16,
    },
    SocketFailed {
        generation: u64,
        record: Arc<ActiveRecord>,
        message: String,
    },
    PermissionUpdateDone {
        reply: StatusReply,
        permissions: Vec<Permission>,
        result: Result<Vec<String>, RemoteError>,
    },
    DisconnectRemoteDone {
        reply: DisconnectReply,
        cleanup: Option<futures_util::future::BoxFuture<'static, ()>>,
        warning: Option<String>,
    },
}

pub(crate) struct Actor {
    pub(super) hostname: String,
    pub(super) server_id: String,
    pub(super) daemon_public_key: String,
    pub(super) logger: Arc<dyn HubLogger>,
    pub(super) remote: Arc<dyn HubRemote>,
    pub(super) clock: Arc<dyn HubClock>,
    pub(super) retry_policy: Arc<dyn RetryPolicy>,
    pub(super) create_ceremony: Option<Box<dyn Fn() -> Ceremony + Send + Sync>>,
    pub(super) host: Arc<dyn HubSessionHost>,
    pub(super) file_path: PathBuf,
    pub(super) tx: mpsc::UnboundedSender<Msg>,
    pub(super) status: Arc<ArcSwap<HubRelationshipStatus>>,

    pub(super) record: Option<Record>,
    pub(super) state: HubConnectionState,
    pub(super) connected_at: Option<String>,
    pub(super) last_error: Option<String>,
    pub(super) socket: Option<Box<dyn HubSocketConnection>>,
    pub(super) retry: Option<Box<dyn ScheduledTask>>,
    pub(super) generation: u64,
    pub(super) enrollment_generation: u64,
    pub(super) retry_attempt: u32,
    pub(super) next_attempt: u64,
    pub(super) in_flight: HashSet<u64>,
    pub(super) attempt_replies: HashMap<u64, StatusReply>,
    pub(super) settle_waiters: Vec<(HashSet<u64>, Reply<()>)>,
    pub(super) execution_agents: Option<(String, Arc<dyn HubExecutionAgents>)>,
}

/// Starts the relationship actor. Loads (and if needed quarantines or migrates) the
/// persisted record synchronously, like the TS constructor.
pub struct HubRelationshipActor;

impl HubRelationshipActor {
    pub fn spawn(options: HubOptions) -> Result<HubHandle, HubError> {
        let file_path = options.paseo_home.join(FILE_NAME);
        let io = |e: std::io::Error| HubError::message(e.to_string());
        let mut record = match record::load(&file_path, options.clock.now_ms()).map_err(io)? {
            Loaded::Missing => None,
            Loaded::Record(record) => Some(record),
            Loaded::Quarantined {
                quarantine_path,
                error,
            } => {
                options.logger.log(
                    LogLevel::Error,
                    "Quarantined invalid Hub relationship authority",
                    Some(&format!("{error} ({})", quarantine_path.display())),
                );
                None
            }
        };
        if let Some(Record::Disconnecting(_)) = &record {
            // COMPAT(hubUnilateralDisconnect): added in v0.4.0, remove after 2027-02-13 once
            // legacy records have aged out.
            options.logger.log(
                LogLevel::Warn,
                "Removed legacy disconnecting Hub relationship during startup",
                record.as_ref().map(Record::daemon_id),
            );
            match std::fs::remove_file(&file_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(e)),
            }
            record = None;
        }
        let state = match &record {
            Some(Record::Revoked(_)) => HubConnectionState::Revoked,
            Some(_) => HubConnectionState::Connecting,
            None => HubConnectionState::NotConnected,
        };
        let last_error = match &record {
            Some(Record::Revoked(r)) => r.reason.clone(),
            _ => None,
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor {
            hostname: options.hostname,
            server_id: options.server_id,
            daemon_public_key: options.daemon_public_key,
            logger: options.logger.clone(),
            remote: options.remote,
            clock: options.clock,
            retry_policy: options.retry_policy,
            create_ceremony: options.create_ceremony,
            host: options.host,
            file_path,
            tx: tx.clone(),
            status: Arc::new(ArcSwap::from_pointee(HubRelationshipStatus {
                state: HubConnectionState::NotConnected,
                daemon_id: None,
                hub_origin: None,
                permissions: Vec::new(),
                connected_at: None,
                last_error: None,
            })),
            record,
            state,
            connected_at: None,
            last_error,
            socket: None,
            retry: None,
            generation: 0,
            enrollment_generation: 0,
            retry_attempt: 0,
            next_attempt: 0,
            in_flight: HashSet::new(),
            attempt_replies: HashMap::new(),
            settle_waiters: Vec::new(),
            execution_agents: None,
        };
        actor.publish_status();
        let status = actor.status.clone();
        tokio::spawn(actor.run(rx));
        Ok(HubHandle {
            inner: Arc::new(HandleInner {
                tx,
                status,
                logger: options.logger,
            }),
        })
    }
}

struct HandleInner {
    tx: mpsc::UnboundedSender<Msg>,
    status: Arc<ArcSwap<HubRelationshipStatus>>,
    logger: Arc<dyn HubLogger>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Exit);
    }
}

/// Handle to the relationship actor.
#[derive(Clone)]
pub struct HubHandle {
    inner: Arc<HandleInner>,
}

fn stopped() -> HubError {
    HubError::message("Hub relationship actor stopped")
}

impl HubHandle {
    async fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, HubError> {
        let (reply, rx) = oneshot::channel();
        self.inner
            .tx
            .send(Msg::Command(command(reply)))
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())
    }

    /// `HubRelationshipController.start`: resumes a persisted `active` (dial) or `pending`
    /// (enroll) relationship. A rejected pending enrollment is discarded with a warning.
    pub async fn start(&self) -> Result<(), HubError> {
        match self.call(Command::Start).await? {
            Ok(_) => Ok(()),
            Err(HubError::EnrollmentRejected { status }) => {
                self.inner.logger.log(
                    LogLevel::Warn,
                    "Discarded rejected pending Hub enrollment during startup",
                    Some(&status.to_string()),
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Fences execution authority, cancels timers and closes the socket. Waits for the
    /// execution agents to drain.
    pub async fn stop(&self) -> Result<(), HubError> {
        self.call(Command::Stop).await
    }

    pub fn status(&self) -> HubRelationshipStatus {
        HubRelationshipStatus::clone(&self.inner.status.load())
    }

    pub async fn connect(&self, input: ConnectInput) -> Result<HubRelationshipStatus, HubError> {
        self.call(|reply| Command::Connect(input, reply)).await?
    }

    pub async fn update_permissions(
        &self,
        change: PermissionChange,
    ) -> Result<HubRelationshipStatus, HubError> {
        self.call(|reply| Command::UpdatePermissions(change, reply))
            .await?
    }

    pub async fn disconnect(&self, force: bool) -> Result<DisconnectOutcome, HubError> {
        self.call(|reply| Command::Disconnect { force, reply })
            .await?
    }
}

pub(crate) fn principal_id(daemon_id: &str, secret: &str) -> String {
    format!("hub:{daemon_id}:{}", secret_fingerprint(secret))
}

/// `base64url(sha256(secret))`: the credential verifier and the principal fingerprint.
pub fn secret_fingerprint(secret: &str) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(message) = rx.recv().await {
            match message {
                Msg::Exit => {
                    self.cancel_lifecycle();
                    if let Some(socket) = self.socket.take() {
                        socket.close();
                    }
                    drop(self.retire_execution_agents());
                    break;
                }
                Msg::Command(command) => self.on_command(command),
                Msg::Event(event) => self.on_event(event),
            }
            self.publish_status();
        }
    }

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Start(reply) => self.start(reply),
            Command::Stop(reply) => self.stop(reply),
            Command::Connect(input, reply) => self.connect(input, reply),
            Command::UpdatePermissions(change, reply) => self.update_permissions(change, reply),
            Command::Disconnect { force, reply } => self.disconnect(force, reply),
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::EnrollmentSettled {
                attempt,
                generation,
                pending,
                result,
            } => self.enrollment_settled(attempt, generation, pending, result),
            Event::EnrollmentRetry {
                generation,
                pending,
            } => {
                if generation == self.enrollment_generation {
                    self.try_enrollment(pending, generation, None);
                }
            }
            Event::SocketRetry { generation, record } => {
                if generation == self.generation {
                    self.open_socket((*record).clone(), true);
                }
            }
            Event::SocketConnected {
                generation,
                record,
                socket,
                protocol,
            } => self.socket_connected(generation, &record, socket, protocol),
            Event::SocketRejected { generation, status } => {
                self.socket_rejected(generation, status)
            }
            Event::SocketClosed {
                generation,
                record,
                code,
            } => self.socket_closed(generation, record, code),
            Event::SocketFailed {
                generation,
                record,
                message,
            } => self.socket_failed(generation, record, message),
            Event::PermissionUpdateDone {
                reply,
                permissions,
                result,
            } => self.permission_update_done(reply, permissions, result),
            Event::DisconnectRemoteDone {
                reply,
                cleanup,
                warning,
            } => self.disconnect_remote_done(reply, cleanup, warning),
        }
    }

    pub(super) fn current_status(&self) -> HubRelationshipStatus {
        HubRelationshipStatus {
            state: self.state,
            daemon_id: self.record.as_ref().map(|r| r.daemon_id().to_owned()),
            hub_origin: self.record.as_ref().map(|r| r.hub_origin().to_owned()),
            permissions: self
                .record
                .as_ref()
                .map(|r| r.permissions().to_vec())
                .unwrap_or_default(),
            connected_at: self.connected_at.clone(),
            last_error: self.last_error.clone(),
        }
    }

    pub(super) fn publish_status(&self) {
        self.status.store(Arc::new(self.current_status()));
    }

    /// Publishes the status, then answers (a reply must never be older than the state it
    /// reports).
    pub(super) fn reply<T>(&self, reply: Reply<T>, value: T) {
        self.publish_status();
        let _ = reply.send(value);
    }

    pub(super) fn persist(&self, record: &Record) -> Result<(), HubError> {
        record::persist(&self.file_path, record).map_err(|e| HubError::message(e.to_string()))
    }

    pub(super) fn log(&self, level: LogLevel, message: &str, detail: Option<&str>) {
        self.logger.log(level, message, detail);
    }

    pub(super) fn parse_permissions(values: &[String]) -> Result<Vec<Permission>, HubError> {
        parse_daemon_permissions(values).map_err(|e| HubError::message(e.to_string()))
    }

    /// `cancelLifecycle`.
    pub(super) fn cancel_lifecycle(&mut self) {
        self.generation += 1;
        self.enrollment_generation += 1;
        if let Some(retry) = self.retry.take() {
            retry.cancel();
        }
    }

    /// `remove`: delete the file and forget the relationship. Does not close the socket
    /// (callers that need that did it first) and keeps `retry_attempt`, as TS does.
    pub(super) fn remove(&mut self) {
        drop(self.retire_execution_agents());
        self.cancel_lifecycle();
        match std::fs::remove_file(&self.file_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => self.log(
                LogLevel::Error,
                "Failed to remove the Hub relationship file",
                Some(&e.to_string()),
            ),
        }
        self.record = None;
        self.state = HubConnectionState::NotConnected;
        self.connected_at = None;
        self.last_error = None;
    }

    /// `retireExecutionAgents`: the authority fence is raised now; the returned future drains.
    pub(super) fn retire_execution_agents(
        &mut self,
    ) -> Option<futures_util::future::BoxFuture<'static, ()>> {
        self.execution_agents
            .take()
            .map(|(_, agents)| agents.invalidate_authority())
    }

    /// `executionAgentsFor`.
    pub(super) fn execution_agents_for(&mut self, daemon_id: &str) -> Arc<dyn HubExecutionAgents> {
        if let Some((id, agents)) = &self.execution_agents {
            if id == daemon_id {
                return agents.clone();
            }
        }
        let agents = self.host.create_execution_agents(daemon_id);
        self.execution_agents = Some((daemon_id.to_owned(), agents.clone()));
        agents
    }

    pub(super) fn stop(&mut self, reply: Reply<()>) {
        let cleanup = self.retire_execution_agents();
        self.cancel_lifecycle();
        if let Some(socket) = self.socket.take() {
            socket.close();
        }
        self.publish_status();
        tokio::spawn(async move {
            if let Some(cleanup) = cleanup {
                cleanup.await;
            }
            let _ = reply.send(());
        });
    }
}
