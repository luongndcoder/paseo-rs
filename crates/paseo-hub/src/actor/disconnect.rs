//! `disconnect`: persist the terminal intent, tell the Hub, delete the file.

use std::collections::HashSet;

use futures_util::future::BoxFuture;
use tokio::sync::oneshot;

use super::{Actor, DisconnectReply, Event, Msg};
use crate::record::{DisconnectingRecord, Record};
use crate::remote::HubRevocation;
use crate::types::{DisconnectOutcome, HubConnectionState, HubError, LogLevel};

const WARNING: &str =
    "Hub could not be reached; local relationship removed, but server-side revocation may remain pending.";

impl Actor {
    pub(super) fn disconnect(&mut self, force: bool, reply: DisconnectReply) {
        let wait_for_enrollment = matches!(self.record, Some(Record::Pending(_)));
        let cleanup = self.retire_execution_agents();
        self.cancel_lifecycle();
        if let Some(socket) = self.socket.take() {
            socket.close();
        }
        let disconnecting = match &self.record {
            None | Some(Record::Revoked(_)) => {
                self.remove();
                self.finish_after(cleanup, reply, None);
                return;
            }
            Some(record) => {
                // `relationship` and `credential` exist on every non-revoked state; the
                // transport only survives an `active` record.
                let relationship = match record {
                    Record::Pending(r) => r.relationship.clone(),
                    Record::Active(r) => r.relationship.clone(),
                    Record::Disconnecting(r) => r.relationship.clone(),
                    Record::Revoked(_) => unreachable!("handled above"),
                };
                DisconnectingRecord {
                    relationship,
                    credential: record
                        .credential()
                        .cloned()
                        .expect("non-revoked records carry a credential"),
                    transport: match record {
                        Record::Active(r) => Some(r.transport.clone()),
                        _ => None,
                    },
                }
            }
        };
        let persisted = Record::Disconnecting(disconnecting.clone());
        if let Err(error) = self.persist(&persisted) {
            // TS rejects here and leaves the pending-create cleanup running unobserved.
            self.reply(reply, Err(error));
            return;
        }
        self.record = Some(persisted);
        self.state = HubConnectionState::Disconnecting;

        let wait = (wait_for_enrollment && !force).then(|| {
            let (tx, rx) = oneshot::channel();
            if self.in_flight.is_empty() {
                let _ = tx.send(());
            } else {
                let ids: HashSet<u64> = self.in_flight.clone();
                self.settle_waiters.push((ids, tx));
            }
            rx
        });
        let remote = self.remote.clone();
        let logger = self.logger.clone();
        let events = self.tx.clone();
        let revocation = HubRevocation {
            daemon_id: disconnecting.relationship.daemon_id.clone(),
            hub_origin: disconnecting.relationship.hub_origin.clone(),
            credential: disconnecting.credential.secret.clone(),
        };
        self.publish_status();
        tokio::spawn(async move {
            if let Some(wait) = wait {
                let _ = wait.await;
            }
            let mut warning = None;
            if !force {
                if let Err(error) = remote.revoke(revocation.clone()).await {
                    logger.log(
                        LogLevel::Warn,
                        "Failed to notify Hub before removing local relationship",
                        Some(&format!("{error} (daemon {})", revocation.daemon_id)),
                    );
                    warning = Some(WARNING.to_owned());
                }
            }
            let _ = events.send(Msg::Event(Event::DisconnectRemoteDone {
                reply,
                cleanup,
                warning,
            }));
        });
    }

    pub(super) fn disconnect_remote_done(
        &mut self,
        reply: DisconnectReply,
        cleanup: Option<BoxFuture<'static, ()>>,
        warning: Option<String>,
    ) {
        self.remove();
        self.finish_after(cleanup, reply, warning);
    }

    /// Answers once the execution agents have drained, with the status as it is then.
    fn finish_after(
        &self,
        cleanup: Option<BoxFuture<'static, ()>>,
        reply: DisconnectReply,
        warning: Option<String>,
    ) {
        self.publish_status();
        let status = self.status.clone();
        tokio::spawn(async move {
            if let Some(cleanup) = cleanup {
                cleanup.await;
            }
            let outcome: Result<DisconnectOutcome, HubError> = Ok(DisconnectOutcome {
                status: (**status.load()).clone(),
                warning,
            });
            let _ = reply.send(outcome);
        });
    }
}
