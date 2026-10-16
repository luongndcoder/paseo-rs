//! Dialing the Hub socket, reacting to its events, and permanent revocation.

use std::sync::Arc;

use tokio::sync::mpsc;

use super::{principal_id, Actor, Event, Msg};
use crate::record::{ActiveRecord, Record, RevokedRecord, SanitizedRelationship};
use crate::remote::{HubSocket, HubSocketCredentials, HubSocketEvents, SessionProtocol};
use crate::types::{AttachOptions, HubConnectionState, LogLevel};

/// Posts the dial's events into the actor mailbox, tagged with the generation (and the record)
/// the dial was opened with.
struct ActorSocketEvents {
    tx: mpsc::UnboundedSender<Msg>,
    generation: u64,
    record: Arc<ActiveRecord>,
}

impl ActorSocketEvents {
    fn post(&self, event: Event) {
        let _ = self.tx.send(Msg::Event(event));
    }
}

impl HubSocketEvents for ActorSocketEvents {
    fn connected(&self, socket: Box<dyn HubSocket>, protocol: SessionProtocol) {
        self.post(Event::SocketConnected {
            generation: self.generation,
            record: self.record.clone(),
            socket,
            protocol,
        });
    }

    fn rejected(&self, status: u16) {
        self.post(Event::SocketRejected {
            generation: self.generation,
            status,
        });
    }

    fn closed(&self, code: u16) {
        self.post(Event::SocketClosed {
            generation: self.generation,
            record: self.record.clone(),
            code,
        });
    }

    fn failed(&self, message: String) {
        self.post(Event::SocketFailed {
            generation: self.generation,
            record: self.record.clone(),
            message,
        });
    }
}

impl Actor {
    /// `openSocket`.
    pub(super) fn open_socket(&mut self, record: ActiveRecord, reconnecting: bool) {
        self.generation += 1;
        let generation = self.generation;
        self.state = if reconnecting {
            HubConnectionState::Reconnecting
        } else {
            HubConnectionState::Connecting
        };
        let credentials = HubSocketCredentials {
            daemon_id: record.relationship.daemon_id.clone(),
            web_socket_url: record.transport.web_socket_url.clone(),
            credential: record.credential.secret.clone(),
        };
        let events = Arc::new(ActorSocketEvents {
            tx: self.tx.clone(),
            generation,
            record: Arc::new(record),
        });
        self.socket = Some(self.remote.open_socket(credentials, events));
    }

    pub(super) fn socket_connected(
        &mut self,
        generation: u64,
        record: &ActiveRecord,
        socket: Box<dyn HubSocket>,
        protocol: SessionProtocol,
    ) {
        if generation != self.generation {
            socket.close();
            return;
        }
        // The dial task has finished; keep a handle that closes the live socket.
        self.socket = Some(socket.close_handle());
        self.retry_attempt = 0;
        self.state = HubConnectionState::Connected;
        self.connected_at = Some(self.clock.now_iso());
        self.last_error = None;
        // PARITY-QUIRK(reconnect-uses-captured-record): the permissions handed to the session
        // are those of the record the dial was opened with. A later `update_permissions`
        // replaces `self.record` but not the captured one, so after a reconnect a revoked
        // permission is attached again until the daemon restarts.
        let options = AttachOptions {
            daemon_id: record.relationship.daemon_id.clone(),
            principal_id: principal_id(&record.relationship.daemon_id, &record.credential.secret),
            permissions: record.relationship.permissions.clone(),
            agents: self.execution_agents_for(&record.relationship.daemon_id),
            session_protocol: protocol,
        };
        let host = self.host.clone();
        let logger = self.logger.clone();
        tokio::spawn(async move {
            if let Err(error) = host.attach_socket(socket, options).await {
                logger.log(LogLevel::Error, "Hub socket attach failed", Some(&error));
            }
        });
    }

    pub(super) fn socket_rejected(&mut self, generation: u64, status: u16) {
        if generation != self.generation {
            return;
        }
        self.revoke(format!("Hub rejected socket authentication ({status})"));
    }

    pub(super) fn socket_closed(&mut self, generation: u64, record: Arc<ActiveRecord>, code: u16) {
        if generation != self.generation {
            return;
        }
        if code == 4403 {
            self.revoke("Hub revoked this relationship".into());
            return;
        }
        if matches!(self.record, Some(Record::Active(_))) {
            self.schedule_socket(record);
        }
    }

    pub(super) fn socket_failed(
        &mut self,
        generation: u64,
        record: Arc<ActiveRecord>,
        message: String,
    ) {
        if generation != self.generation {
            return;
        }
        self.last_error = Some(message);
        if matches!(self.record, Some(Record::Active(_))) {
            self.schedule_socket(record);
        }
    }

    fn schedule_socket(&mut self, record: Arc<ActiveRecord>) {
        self.state = HubConnectionState::Reconnecting;
        if let Some(retry) = self.retry.take() {
            retry.cancel();
        }
        let generation = self.generation;
        let delay = self.retry_policy.delay(self.retry_attempt);
        self.retry_attempt += 1;
        let tx = self.tx.clone();
        self.retry = Some(self.clock.schedule(
            delay,
            Box::new(move || {
                let _ = tx.send(Msg::Event(Event::SocketRetry { generation, record }));
            }),
        ));
    }

    /// `revoke`: the Hub said this relationship is over. Keeps the relationship (without the
    /// credential and idempotency key) and the transport, records the reason, never redials.
    fn revoke(&mut self, reason: String) {
        drop(self.retire_execution_agents());
        self.cancel_lifecycle();
        let Some(record) = &self.record else {
            return;
        };
        let revoked = Record::Revoked(RevokedRecord {
            relationship: SanitizedRelationship {
                daemon_id: record.daemon_id().to_owned(),
                hub_origin: record.hub_origin().to_owned(),
                created_at: record.created_at().to_owned(),
                permissions: record.permissions().to_vec(),
            },
            transport: record.transport().cloned(),
            reason: Some(reason.clone()),
        });
        // A failed write leaves the record as it was; in TS the throw escapes the event
        // handler, here it is logged.
        if let Err(error) = self.persist(&revoked) {
            self.log(
                LogLevel::Error,
                "Failed to persist the revoked Hub relationship",
                Some(&error.to_string()),
            );
            return;
        }
        self.record = Some(revoked);
        self.state = HubConnectionState::Revoked;
        self.last_error = Some(reason);
    }
}
