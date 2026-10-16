use std::any::Any;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paseo_hub::{
    HubEnrollment, HubEnrollmentResult, HubPermissionUpdate, HubRemote, HubRevocation, HubSocket,
    HubSocketConnection, HubSocketCredentials, HubSocketEvents, RemoteError, SessionProtocol,
};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollOutcome {
    Complete,
    /// The Hub recorded the enrollment but the response never arrived.
    Lose,
    Reject(u16),
}

pub struct SocketAttempt {
    pub input: HubSocketCredentials,
    pub events: Arc<dyn HubSocketEvents>,
    /// Closed through the dial (`HubSocketConnection`).
    pub closed: Arc<AtomicBool>,
    /// Closed through the handed-over socket (`HubSocket::close` or its close handle).
    pub socket_closed: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    enroll_attempts: Vec<HubEnrollment>,
    enrolled: HashSet<(String, String)>,
    hold_next: bool,
    held: Vec<(usize, oneshot::Sender<EnrollOutcome>)>,
    next_override: Option<EnrollOutcome>,
    enroll_failure: Option<String>,
    enrollment_permissions: Option<Vec<String>>,
    revocations: Vec<HubRevocation>,
    fail_revocations: usize,
    hold_revocation: bool,
    held_revocations: Vec<oneshot::Sender<()>>,
    sockets: Vec<SocketAttempt>,
    updates: Vec<HubPermissionUpdate>,
    update_response: Option<Vec<String>>,
}

/// The Hub: scripted enrollment, revocation, permission updates and sockets.
pub struct FakeRemote {
    state: Mutex<State>,
}

pub struct FakeSocket {
    pub closed: Arc<AtomicBool>,
}

impl HubSocket for FakeSocket {
    fn close(self: Box<Self>) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn close_handle(&self) -> Box<dyn HubSocketConnection> {
        Box::new(FakeConnection(self.closed.clone()))
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

struct FakeConnection(Arc<AtomicBool>);

impl HubSocketConnection for FakeConnection {
    fn close(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl FakeRemote {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    // ---- enrollment ----
    /// The next enrollment attempt waits until the test completes, loses or rejects it.
    pub fn hold_enrollment(&self) {
        self.state().hold_next = true;
    }

    pub fn reject_next_enrollment(&self, status: u16) {
        self.state().next_override = Some(EnrollOutcome::Reject(status));
    }

    /// Every enrollment fails with a retryable error carrying `message` (the Hub is down).
    pub fn fail_enrollments(&self, message: &str) {
        self.state().enroll_failure = Some(message.to_owned());
    }

    pub fn return_enrollment_permissions(&self, permissions: &[&str]) {
        self.state().enrollment_permissions =
            Some(permissions.iter().map(|p| (*p).to_owned()).collect());
    }

    pub fn enrollment_attempts(&self) -> Vec<HubEnrollment> {
        self.state().enroll_attempts.clone()
    }

    pub fn enrolled_relationships(&self) -> usize {
        self.state().enrolled.len()
    }

    fn settle_held(&self, index: Option<usize>, outcome: EnrollOutcome) {
        let mut state = self.state();
        let mut keep = Vec::new();
        for (i, tx) in std::mem::take(&mut state.held) {
            if index.is_none_or(|wanted| wanted == i) {
                let _ = tx.send(outcome);
            } else {
                keep.push((i, tx));
            }
        }
        state.held = keep;
    }

    pub fn complete_enrollment(&self) {
        self.settle_held(None, EnrollOutcome::Complete);
    }

    pub fn lose_enrollment_response(&self) {
        self.settle_held(None, EnrollOutcome::Lose);
    }

    pub fn reject_enrollment(&self, index: usize, status: u16) {
        self.settle_held(Some(index), EnrollOutcome::Reject(status));
    }

    // ---- revocation ----
    pub fn fail_revocations(&self, count: usize) {
        self.state().fail_revocations = count;
    }

    pub fn hold_revocation(&self) {
        self.state().hold_revocation = true;
    }

    pub fn complete_revocation(&self) {
        for tx in self.state().held_revocations.drain(..) {
            let _ = tx.send(());
        }
    }

    pub fn revocation_attempts(&self) -> usize {
        self.state().revocations.len()
    }

    pub fn latest_revocation(&self) -> Option<HubRevocation> {
        self.state().revocations.last().cloned()
    }

    // ---- permission updates ----
    pub fn respond_to_updates_with(&self, permissions: &[&str]) {
        self.state().update_response = Some(permissions.iter().map(|p| (*p).to_owned()).collect());
    }

    pub fn permission_updates(&self) -> Vec<HubPermissionUpdate> {
        self.state().updates.clone()
    }

    // ---- sockets ----
    pub fn socket_attempts(&self) -> usize {
        self.state().sockets.len()
    }

    fn socket(&self, index: usize) -> (Arc<dyn HubSocketEvents>, Arc<AtomicBool>) {
        let state = self.state();
        let attempt = &state.sockets[index];
        (attempt.events.clone(), attempt.closed.clone())
    }

    pub fn socket_input(&self, index: usize) -> HubSocketCredentials {
        self.state().sockets[index].input.clone()
    }

    pub fn socket_closed(&self, index: usize) -> bool {
        self.state().sockets[index].closed.load(Ordering::SeqCst)
    }

    /// Whether the socket handed over by `connect_socket` was closed.
    pub fn handed_over_closed(&self, index: usize) -> bool {
        self.state().sockets[index]
            .socket_closed
            .load(Ordering::SeqCst)
    }

    /// Returns the handed-over socket's closed flag.
    pub fn connect_socket(&self, index: usize) -> Arc<AtomicBool> {
        let (events, _) = self.socket(index);
        let closed = self.state().sockets[index].socket_closed.clone();
        events.connected(
            Box::new(FakeSocket {
                closed: closed.clone(),
            }),
            SessionProtocol::SessionV1,
        );
        closed
    }

    pub fn connect_latest_socket(&self) -> Arc<AtomicBool> {
        let index = self.socket_attempts() - 1;
        self.connect_socket(index)
    }

    pub fn close_socket(&self, index: usize, code: u16) {
        self.socket(index).0.closed(code);
    }

    pub fn close_latest_socket(&self, code: u16) {
        self.close_socket(self.socket_attempts() - 1, code);
    }

    pub fn reject_socket(&self, index: usize, status: u16) {
        self.socket(index).0.rejected(status);
    }

    pub fn fail_socket(&self, index: usize, message: &str) {
        self.socket(index).0.failed(message.to_owned());
    }
}

#[async_trait]
impl HubRemote for FakeRemote {
    async fn enroll(&self, input: HubEnrollment) -> Result<HubEnrollmentResult, RemoteError> {
        let (outcome_rx, immediate) = {
            let mut state = self.state();
            let index = state.enroll_attempts.len();
            state.enroll_attempts.push(input.clone());
            if state.hold_next {
                state.hold_next = false;
                let (tx, rx) = oneshot::channel();
                state.held.push((index, tx));
                (Some(rx), EnrollOutcome::Complete)
            } else {
                let outcome = state
                    .next_override
                    .take()
                    .unwrap_or(EnrollOutcome::Complete);
                (None, outcome)
            }
        };
        let outcome = match outcome_rx {
            Some(rx) => rx.await.unwrap_or(EnrollOutcome::Lose),
            None => immediate,
        };
        let mut state = self.state();
        if let Some(message) = state.enroll_failure.clone() {
            return Err(RemoteError::failed(message));
        }
        match outcome {
            EnrollOutcome::Reject(status) => Err(RemoteError::EnrollmentRejected { status }),
            EnrollOutcome::Lose => {
                state
                    .enrolled
                    .insert((input.daemon_id.clone(), input.idempotency_key.clone()));
                Err(RemoteError::failed("connection lost"))
            }
            EnrollOutcome::Complete => {
                state
                    .enrolled
                    .insert((input.daemon_id.clone(), input.idempotency_key.clone()));
                Ok(HubEnrollmentResult {
                    daemon_id: input.daemon_id,
                    permissions: state
                        .enrollment_permissions
                        .clone()
                        .unwrap_or(input.permissions),
                    web_socket_url: "wss://hub.test/daemon".into(),
                })
            }
        }
    }

    async fn update_permissions(
        &self,
        input: HubPermissionUpdate,
    ) -> Result<Vec<String>, RemoteError> {
        let mut state = self.state();
        state.updates.push(input.clone());
        Ok(state.update_response.clone().unwrap_or(input.permissions))
    }

    async fn revoke(&self, input: HubRevocation) -> Result<(), RemoteError> {
        let hold = {
            let mut state = self.state();
            state.revocations.push(input);
            if state.hold_revocation {
                state.hold_revocation = false;
                let (tx, rx) = oneshot::channel();
                state.held_revocations.push(tx);
                Some(rx)
            } else {
                None
            }
        };
        if let Some(rx) = hold {
            let _ = rx.await;
        }
        let mut state = self.state();
        if state.fail_revocations > 0 {
            state.fail_revocations -= 1;
            return Err(RemoteError::failed("Hub unreachable"));
        }
        Ok(())
    }

    fn open_socket(
        &self,
        input: HubSocketCredentials,
        events: Arc<dyn HubSocketEvents>,
    ) -> Box<dyn HubSocketConnection> {
        let closed = Arc::new(AtomicBool::new(false));
        self.state().sockets.push(SocketAttempt {
            input,
            events,
            closed: closed.clone(),
            socket_closed: Arc::new(AtomicBool::new(false)),
        });
        Box::new(FakeConnection(closed))
    }
}
