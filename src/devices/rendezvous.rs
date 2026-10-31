use super::*;
use crate::protocol::DeviceId;
use crate::relay_protocol::{RELAY_SCHEMA_VERSION, RelayClientMessage, RelayErrorCode};

const MAX_REPLIES: usize = 32;

pub(super) struct IncomingPairing {
    request: PairingRequest,
    device_name: String,
    code: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingPairingRequest {
    pub pairing_id: MessageId,
    pub device_id: DeviceId,
    pub device_name: String,
    pub fingerprint: String,
    pub code: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingDecision {
    pub device_id: DeviceId,
    pub label: String,
    pub approved: bool,
}

impl DeviceEndpoint {
    pub(crate) fn receive_pairing_request(
        &self,
        request: PairingRequest,
        device_name: String,
        name_signature: [u8; 64],
        now: UnixMillis,
    ) -> Result<(), DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        let pairing_id = request.pairing_id;
        let responder = request.responder.device_id;
        if request.verify_name(&device_name, &name_signature).is_err() {
            return self.reply(pairing_id, responder, RelayErrorCode::PairingRejected);
        }
        let code = {
            let pairings = self
                .inner
                .pairings
                .lock()
                .map_err(|_| DeviceError::Poisoned)?;
            let Some(pending) = pairings.get(&pairing_id) else {
                drop(pairings);
                return self.reply(pairing_id, responder, RelayErrorCode::PairingExpired);
            };
            if pending
                .check(&self.public_keys(), &request, now.get())
                .is_err()
            {
                drop(pairings);
                return self.reply(pairing_id, responder, RelayErrorCode::PairingRejected);
            }
            pending.code(&request)?
        };
        let mut incoming = self
            .inner
            .incoming
            .lock()
            .map_err(|_| DeviceError::Poisoned)?;
        incoming.retain(|_, entry| entry.request.expires_at_ms > now.get());
        if let Some(existing) = incoming.get(&pairing_id) {
            if existing.request.responder == request.responder {
                return Ok(());
            }
            drop(incoming);
            return self.reply(pairing_id, responder, RelayErrorCode::PairingInUse);
        }
        if incoming.len() >= MAX_PAIRINGS {
            drop(incoming);
            return self.reply(pairing_id, responder, RelayErrorCode::PairingInUse);
        }
        incoming.insert(
            pairing_id,
            IncomingPairing {
                request,
                device_name,
                code,
            },
        );
        Ok(())
    }

    pub fn pairing_requests(
        &self,
        pairing_id: Option<MessageId>,
        now: UnixMillis,
    ) -> Result<Vec<PendingPairingRequest>, DeviceError> {
        let mut incoming = self
            .inner
            .incoming
            .lock()
            .map_err(|_| DeviceError::Poisoned)?;
        incoming.retain(|_, entry| entry.request.expires_at_ms > now.get());
        Ok(incoming
            .values()
            .filter(|entry| pairing_id.is_none_or(|id| id == entry.request.pairing_id))
            .map(|entry| PendingPairingRequest {
                pairing_id: entry.request.pairing_id,
                device_id: entry.request.responder.device_id,
                device_name: entry.device_name.clone(),
                fingerprint: entry.request.responder.fingerprint(),
                code: format!("{:04} {:04}", entry.code / 10_000, entry.code % 10_000),
                expires_at_ms: entry.request.expires_at_ms,
            })
            .collect())
    }

    pub fn decide_pairing(
        &self,
        pairing_id: MessageId,
        device_id: DeviceId,
        approve: bool,
        access: DeviceAccess,
        now: UnixMillis,
    ) -> Result<PairingDecision, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        let entry = {
            let mut incoming = self
                .inner
                .incoming
                .lock()
                .map_err(|_| DeviceError::Poisoned)?;
            match incoming.get(&pairing_id) {
                Some(entry) if entry.request.responder.device_id == device_id => {
                    incoming.remove(&pairing_id)
                }
                _ => None,
            }
        }
        .ok_or(DeviceError::PairingNotFound)?;
        if !approve {
            self.cancel_pairing(pairing_id, now)?;
            self.reply(pairing_id, device_id, RelayErrorCode::PairingRejected)?;
            return Ok(PairingDecision {
                device_id,
                label: entry.device_name,
                approved: false,
            });
        }
        let approval = PairingApproval::new(
            entry.device_name.clone(),
            entry.request.responder.fingerprint(),
            access,
        )?;
        match self.approve_pairing(&entry.request, approval, now) {
            Ok(acceptance) => {
                self.queue_reply(RelayClientMessage::PairingDecision {
                    schema_version: RELAY_SCHEMA_VERSION,
                    pairing_id,
                    responder: device_id,
                    acceptance: Some(acceptance),
                    reason: None,
                })?;
                Ok(PairingDecision {
                    device_id,
                    label: entry.device_name,
                    approved: true,
                })
            }
            Err(error) => {
                self.cancel_pairing(pairing_id, now)?;
                self.reply(pairing_id, device_id, RelayErrorCode::PairingUnavailable)?;
                Err(error)
            }
        }
    }

    pub fn withdraw_pairing(
        &self,
        pairing_id: MessageId,
        now: UnixMillis,
    ) -> Result<(), DeviceError> {
        self.cancel_pairing(pairing_id, now)?;
        let entry = self
            .inner
            .incoming
            .lock()
            .map_err(|_| DeviceError::Poisoned)?
            .remove(&pairing_id);
        if let Some(entry) = entry {
            self.reply(
                pairing_id,
                entry.request.responder.device_id,
                RelayErrorCode::PairingExpired,
            )?;
        }
        Ok(())
    }

    pub(super) fn take_pairing_replies(&self) -> Result<Vec<RelayClientMessage>, DeviceError> {
        Ok(self
            .inner
            .pairing_replies
            .lock()
            .map_err(|_| DeviceError::Poisoned)?
            .drain(..)
            .collect())
    }

    fn reply(
        &self,
        pairing_id: MessageId,
        responder: DeviceId,
        reason: RelayErrorCode,
    ) -> Result<(), DeviceError> {
        let mut replies = self
            .inner
            .pairing_replies
            .lock()
            .map_err(|_| DeviceError::Poisoned)?;
        if replies.len() >= MAX_REPLIES {
            return Ok(());
        }
        replies.push_back(RelayClientMessage::PairingDecision {
            schema_version: RELAY_SCHEMA_VERSION,
            pairing_id,
            responder,
            acceptance: None,
            reason: Some(reason),
        });
        drop(replies);
        self.inner.outbox_ready.notify_one();
        Ok(())
    }

    fn queue_reply(&self, message: RelayClientMessage) -> Result<(), DeviceError> {
        let mut replies = self
            .inner
            .pairing_replies
            .lock()
            .map_err(|_| DeviceError::Poisoned)?;
        replies.push_back(message);
        drop(replies);
        self.inner.outbox_ready.notify_one();
        Ok(())
    }
}
