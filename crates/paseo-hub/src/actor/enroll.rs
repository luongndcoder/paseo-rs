//! Start, connect, enrollment attempts and permission updates.

use paseo_authz::Permission;
use rand::RngCore;
use uuid::Uuid;

use super::{principal_id, secret_fingerprint, Actor, Event, StatusReply};
use crate::record::{
    normalize_hub_url, same_permissions, ActiveRecord, Credential, Identity, PendingRecord, Record,
    Relationship, Transport,
};
use crate::remote::{HubEnrollment, HubEnrollmentResult, HubPermissionUpdate, RemoteError};
use crate::types::{Ceremony, ConnectInput, HubConnectionState, HubError, PermissionChange};

impl Actor {
    pub(super) fn start(&mut self, reply: StatusReply) {
        if let Some(Record::Active(active)) = &self.record {
            let active = active.clone();
            self.open_socket(active, false);
        }
        if let Some(Record::Pending(pending)) = &self.record {
            let pending = pending.clone();
            let generation = self.begin_enrollment_attempt();
            self.try_enrollment(pending, generation, Some(reply));
            return;
        }
        let status = self.current_status();
        self.reply(reply, Ok(status));
    }

    /// `beginEnrollmentAttempt`: cancel the retry timer, reset the attempt counter and open
    /// a new enrollment generation (older attempts become stale).
    fn begin_enrollment_attempt(&mut self) -> u64 {
        if let Some(retry) = self.retry.take() {
            retry.cancel();
        }
        self.retry_attempt = 0;
        self.enrollment_generation += 1;
        self.enrollment_generation
    }

    pub(super) fn connect(&mut self, input: ConnectInput, reply: StatusReply) {
        let result = self.begin_connect(input);
        match result {
            Ok((pending, generation)) => self.try_enrollment(pending, generation, Some(reply)),
            Err(error) => self.reply(reply, Err(error)),
        }
    }

    fn begin_connect(&mut self, input: ConnectInput) -> Result<(PendingRecord, u64), HubError> {
        let permissions = Self::parse_permissions(&input.permissions)?;
        if let Some(Record::Pending(existing)) = &self.record {
            let origin = normalize_hub_url(&input.hub_url).map_err(HubError::message)?;
            if origin != existing.relationship.hub_origin {
                return Err(HubError::message(
                    "A pending Hub enrollment already exists for a different Hub",
                ));
            }
            // A fresh token replaces the one of the pending ceremony; the ceremony (daemon
            // id, idempotency key, secret, permissions) is kept.
            let mut pending = existing.clone();
            pending.enrollment_token = input.token;
            self.record = Some(Record::Pending(pending.clone()));
            let generation = self.begin_enrollment_attempt();
            self.persist(&Record::Pending(pending.clone()))?;
            self.state = HubConnectionState::Connecting;
            self.last_error = None;
            return Ok((pending, generation));
        }
        if self
            .record
            .as_ref()
            .is_some_and(|r| !matches!(r, Record::Revoked(_)))
        {
            return Err(HubError::message(
                "This daemon already has a Hub relationship",
            ));
        }
        let ceremony = self
            .create_ceremony
            .as_ref()
            .map_or_else(random_ceremony, |c| c());
        let pending = PendingRecord {
            relationship: Relationship {
                daemon_id: ceremony.daemon_id,
                idempotency_key: ceremony.idempotency_key,
                hub_origin: normalize_hub_url(&input.hub_url).map_err(HubError::message)?,
                created_at: self.clock.now_iso(),
                permissions,
            },
            credential: Credential {
                secret: ceremony.secret,
            },
            enrollment_token: input.token,
            identity: Identity {
                server_id: self.server_id.clone(),
                daemon_public_key: self.daemon_public_key.clone(),
            },
        };
        self.persist(&Record::Pending(pending.clone()))?;
        self.record = Some(Record::Pending(pending.clone()));
        self.state = HubConnectionState::Connecting;
        self.last_error = None;
        let generation = self.begin_enrollment_attempt();
        Ok((pending, generation))
    }

    /// `tryEnrollment`. The remote call runs detached; `enrollment_settled` finishes it.
    pub(super) fn try_enrollment(
        &mut self,
        pending: PendingRecord,
        generation: u64,
        reply: Option<StatusReply>,
    ) {
        if generation != self.enrollment_generation {
            if let Some(reply) = reply {
                let status = self.current_status();
                self.reply(reply, Ok(status));
            }
            return;
        }
        let request = HubEnrollment {
            daemon_id: pending.relationship.daemon_id.clone(),
            idempotency_key: pending.relationship.idempotency_key.clone(),
            hub_origin: pending.relationship.hub_origin.clone(),
            token: pending.enrollment_token.clone(),
            hostname: self.hostname.clone(),
            server_id: pending.identity.server_id.clone(),
            daemon_public_key: pending.identity.daemon_public_key.clone(),
            // The verifier is sent on every retry; the secret itself never leaves the daemon.
            credential_verifier: secret_fingerprint(&pending.credential.secret),
            permissions: pending
                .relationship
                .permissions
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect(),
        };
        let attempt = self.next_attempt;
        self.next_attempt += 1;
        self.in_flight.insert(attempt);
        if let Some(reply) = reply {
            self.attempt_replies.insert(attempt, reply);
        }
        let remote = self.remote.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = remote.enroll(request).await;
            let _ = tx.send(super::Msg::Event(Event::EnrollmentSettled {
                attempt,
                generation,
                pending,
                result,
            }));
        });
    }

    pub(super) fn enrollment_settled(
        &mut self,
        attempt: u64,
        generation: u64,
        pending: PendingRecord,
        result: Result<HubEnrollmentResult, RemoteError>,
    ) {
        let outcome = self.process_enrollment(pending, generation, result);
        self.in_flight.remove(&attempt);
        for (mut ids, waiter) in std::mem::take(&mut self.settle_waiters) {
            ids.remove(&attempt);
            if ids.is_empty() {
                let _ = waiter.send(());
            } else {
                self.settle_waiters.push((ids, waiter));
            }
        }
        if let Some(reply) = self.attempt_replies.remove(&attempt) {
            let status = self.current_status();
            self.reply(reply, outcome.map(|()| status));
        }
    }

    /// The part of `tryEnrollment` after `await request`.
    fn process_enrollment(
        &mut self,
        pending: PendingRecord,
        generation: u64,
        result: Result<HubEnrollmentResult, RemoteError>,
    ) -> Result<(), HubError> {
        if generation != self.enrollment_generation {
            return Ok(());
        }
        let failure = match result {
            Ok(enrollment) => match self.activate(&pending, enrollment) {
                Ok(()) => return Ok(()),
                Err(message) => message,
            },
            Err(RemoteError::EnrollmentRejected { status }) => {
                self.remove();
                return Err(HubError::EnrollmentRejected { status });
            }
            Err(RemoteError::Failed(message)) => message,
        };
        self.last_error = Some(failure);
        self.schedule_enrollment(pending, generation);
        Ok(())
    }

    fn activate(
        &mut self,
        pending: &PendingRecord,
        enrollment: HubEnrollmentResult,
    ) -> Result<(), String> {
        if enrollment.daemon_id != pending.relationship.daemon_id
            || !same_permissions(&enrollment.permissions, &pending.relationship.permissions)
        {
            return Err("Hub enrollment response did not match the pending relationship".into());
        }
        let active = ActiveRecord {
            relationship: pending.relationship.clone(),
            credential: pending.credential.clone(),
            transport: Transport {
                web_socket_url: enrollment.web_socket_url,
            },
        };
        self.persist(&Record::Active(active.clone()))
            .map_err(|e| e.to_string())?;
        self.record = Some(Record::Active(active.clone()));
        // `this.retry = null`: the timer that started this attempt has already fired.
        self.retry = None;
        self.retry_attempt = 0;
        self.open_socket(active, false);
        Ok(())
    }

    /// `scheduleEnrollment`.
    fn schedule_enrollment(&mut self, pending: PendingRecord, generation: u64) {
        if generation != self.enrollment_generation {
            return;
        }
        self.state = HubConnectionState::Reconnecting;
        if let Some(retry) = self.retry.take() {
            retry.cancel();
        }
        let delay = self.retry_policy.delay(self.retry_attempt);
        self.retry_attempt += 1;
        let tx = self.tx.clone();
        self.retry = Some(self.clock.schedule(
            delay,
            Box::new(move || {
                let _ = tx.send(super::Msg::Event(Event::EnrollmentRetry {
                    generation,
                    pending,
                }));
            }),
        ));
    }

    pub(super) fn update_permissions(&mut self, change: PermissionChange, reply: StatusReply) {
        let Some(Record::Active(active)) = &self.record else {
            return self.reply(
                reply,
                Err(HubError::message("This daemon is not connected to a Hub")),
            );
        };
        let computed = (|| {
            let grant = Self::parse_permissions(&change.grant)?;
            let revoke = Self::parse_permissions(&change.revoke)?;
            let mut permissions: Vec<Permission> = active
                .relationship
                .permissions
                .iter()
                .copied()
                .filter(|p| !revoke.contains(p))
                .collect();
            for permission in grant {
                if !permissions.contains(&permission) {
                    permissions.push(permission);
                }
            }
            Ok::<_, HubError>(permissions)
        })();
        let permissions = match computed {
            Ok(permissions) => permissions,
            Err(error) => return self.reply(reply, Err(error)),
        };
        let request = HubPermissionUpdate {
            daemon_id: active.relationship.daemon_id.clone(),
            hub_origin: active.relationship.hub_origin.clone(),
            credential: active.credential.secret.clone(),
            permissions: permissions.iter().map(|p| p.as_str().to_owned()).collect(),
        };
        let remote = self.remote.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = remote.update_permissions(request).await;
            let _ = tx.send(super::Msg::Event(Event::PermissionUpdateDone {
                reply,
                permissions,
                result,
            }));
        });
    }

    pub(super) fn permission_update_done(
        &mut self,
        reply: StatusReply,
        permissions: Vec<Permission>,
        result: Result<Vec<String>, RemoteError>,
    ) {
        let outcome = self.apply_permission_update(permissions, result);
        let status = self.current_status();
        self.reply(reply, outcome.map(|()| status));
    }

    fn apply_permission_update(
        &mut self,
        permissions: Vec<Permission>,
        result: Result<Vec<String>, RemoteError>,
    ) -> Result<(), HubError> {
        let granted = result.map_err(|e| HubError::message(e.to_string()))?;
        if !same_permissions(&granted, &permissions) {
            return Err(HubError::message(
                "Hub permission response did not match the local grant",
            ));
        }
        // PARITY-QUIRK(update-permissions-applies-to-current-record): the new set is written
        // onto whatever record exists after the remote call, in any state; TS throws a
        // `TypeError` when the record is gone, here it is a plain error.
        let Some(record) = self.record.as_mut() else {
            return Err(HubError::message("This daemon is not connected to a Hub"));
        };
        *record.permissions_mut() = permissions.clone();
        let snapshot = record.clone();
        self.persist(&snapshot)?;
        let Some(credential) = snapshot.credential() else {
            return Err(HubError::message("Hub relationship has no credential"));
        };
        self.host.update_attached_permissions(
            &principal_id(snapshot.daemon_id(), &credential.secret),
            &permissions,
        );
        Ok(())
    }
}

fn random_ceremony() -> Ceremony {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    Ceremony {
        daemon_id: Uuid::new_v4().to_string(),
        idempotency_key: Uuid::new_v4().to_string(),
        secret: URL_SAFE_NO_PAD.encode(bytes),
    }
}
