mod send;

use std::collections::{BTreeMap, btree_map::Entry};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::protocol::MessageId;
use crate::storage::CommittedSettingsReply;

use super::super::{
    Ledger, PairedHandoffReadError, PendingSettingsReplyBuild, ReplyBuildSlot,
    SettingsConnectionIdentity, SettingsReceiveError, SettingsReceiveWork,
    SettingsReplyBuildRequest,
};
use super::{ReplyBuildJobInput, ReplyBuildState, handoff_receive_error};

pub(in crate::devices) use send::SettingsStartedReturnAdoption;
pub(in crate::devices) use send::{
    PendingSettingsSend, SettingsSendAdmissionError, SettingsSendLoanAssociation,
    SettingsSendReturnFailure, SettingsSendReturnReason, SettingsSendSource,
};
use send::{ReturnedSettingsSend, StartedReturnedSettingsSend, WorkSettingsSendLoan};

const MAX_QUEUED_REPLIES: usize = 32;

#[derive(Default)]
pub(in crate::devices::settings_ingress::work) struct QueuedReplyRegistry {
    entries: BTreeMap<MessageId, QueuedReplyEntry>,
}

enum QueuedReplyEntry {
    StartedReturned(Box<StartedReturnedSettingsSend>),
    Owned {
        returned: Option<ReturnedSettingsSend>,
        reply: Option<OwnedQueuedSettingsReply>,
    },
    Loaned {
        witness: Option<WorkSettingsSendLoan>,
    },
}

pub(crate) struct OwnedQueuedSettingsReply {
    payload: Option<QueuedReplyPayload>,
}

struct QueuedReplyPayload {
    committed: Box<CommittedSettingsReply>,
    settlement: Option<ReplyBuildSlot>,
    input: Arc<ReplyBuildJobInput>,
}

struct AbandonedQueuedReply {
    _cause: QueuedReplyAbandoned,
    _payload: QueuedReplyPayload,
}

struct QueuedReplyAbandoned;

impl QueuedReplyPayload {
    fn matches_header(&self) -> bool {
        let header = self.committed.header();
        let reply = &self.input.record.permit.association.reply;
        !header.message_id.as_uuid().is_nil()
            && header.sender == reply.local_device_id
            && header.recipient == reply.peer_device_id
            && header.created_at_ms >= reply.created_at.get()
            && header.created_at_ms < header.expires_at_ms
            && header.expires_at_ms == reply.expires_at.get()
    }

    fn settled(&self) -> bool {
        let Some(slot) = self.settlement.as_ref() else {
            return false;
        };
        let permit = &self.input.record.permit;
        !permit.armed
            && Arc::ptr_eq(&slot.identity, &permit.slot)
            && Arc::ptr_eq(&slot.association, &permit.association)
            && self.matches_header()
    }
}

impl QueuedReplyRegistry {
    pub(in crate::devices::settings_ingress::work) fn healthy(
        &self,
        ledger: &Arc<Ledger>,
        private_requests: usize,
    ) -> bool {
        self.entries.len() <= MAX_QUEUED_REPLIES
            && self.entries.len() <= private_requests
            && self.entries.iter().all(|(message_id, entry)| match entry {
                QueuedReplyEntry::Owned {
                    returned,
                    reply: Some(reply),
                } => reply.payload.as_ref().is_some_and(|payload| {
                    Arc::ptr_eq(&payload.input.record.permit.ledger, ledger)
                        && payload.committed.header().message_id == *message_id
                        && payload.settled()
                        && returned.as_ref().is_none_or(|returned| {
                            returned.matches_payload(payload, ledger, *message_id)
                        })
                }),
                QueuedReplyEntry::StartedReturned(returned) => {
                    returned.healthy(ledger, *message_id)
                }
                QueuedReplyEntry::Loaned {
                    witness: Some(witness),
                } => witness.matches_registry(ledger, *message_id),
                _ => false,
            })
    }
}

impl OwnedQueuedSettingsReply {
    fn settled_payload(&self) -> Result<&QueuedReplyPayload, PairedHandoffReadError> {
        let payload = self
            .payload
            .as_ref()
            .ok_or(PairedHandoffReadError::Bookkeeping)?;
        let ledger = &payload.input.record.permit.ledger;
        ledger
            .check_admission()
            .map_err(|_| PairedHandoffReadError::Bookkeeping)?;
        if !payload.settled() {
            ledger.fail();
            return Err(PairedHandoffReadError::Bookkeeping);
        }
        Ok(payload)
    }

    pub(crate) fn committed(&self) -> Result<&CommittedSettingsReply, PairedHandoffReadError> {
        Ok(&self.settled_payload()?.committed)
    }

    pub(crate) fn request(&self) -> Result<&SettingsReplyBuildRequest, PairedHandoffReadError> {
        self.settled_payload()?.input.record.reply_request()
    }

    fn settle(&mut self, ledger: &Arc<Ledger>) -> Result<(), SettingsReceiveError> {
        let payload = self.payload.as_mut().ok_or_else(|| ledger.fail())?;
        if payload.settlement.is_some() || !payload.matches_header() {
            return Err(ledger.fail());
        }
        let input = Arc::get_mut(&mut payload.input).ok_or_else(|| ledger.fail())?;
        let permit = &mut input.record.permit;
        if !permit.armed || !Arc::ptr_eq(&permit.ledger, ledger) {
            return Err(ledger.fail());
        }
        ledger.check_admission()?;
        let mut counts = ledger.counts.lock().map_err(|_| ledger.fail())?;
        ledger.check_admission()?;
        if counts.build.as_ref().is_none_or(|slot| {
            !Arc::ptr_eq(&slot.identity, &permit.slot)
                || !Arc::ptr_eq(&slot.association, &permit.association)
        }) {
            return Err(ledger.fail());
        }
        let Some(slot) = counts.build.take() else {
            return Err(ledger.fail());
        };
        payload.settlement = Some(slot);
        permit.armed = false;
        drop(counts);
        Ok(())
    }
}

impl SettingsReceiveWork {
    pub(in crate::devices) fn try_queue_joined_reply(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        ticket: &PendingSettingsReplyBuild,
    ) -> Result<MessageId, SettingsReceiveError> {
        if let Err(error) = self.ledger.check_admission() {
            self.failure = Some(error.clone());
            self.close_intake();
            return Err(error);
        }
        if self.failure.is_some() {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        }
        let (message_id, matches) = {
            let Some(ReplyBuildState::JoinedReady {
                _committed: committed,
                _input: input,
            }) = self.build.as_ref()
            else {
                return Err(SettingsReceiveError::Capacity);
            };
            let permit = &input.record.permit;
            let header = committed.header();
            let reply = &permit.association.reply;
            (
                header.message_id,
                permit.armed
                    && Arc::ptr_eq(&permit.ledger, &self.ledger)
                    && Arc::ptr_eq(&permit.slot, &ticket.slot)
                    && self
                        .context
                        .as_ref()
                        .is_some_and(|context| Arc::ptr_eq(context, &input.context))
                    && self
                        .history
                        .as_ref()
                        .is_some_and(|history| Arc::ptr_eq(&history.connection, connection))
                    && !header.message_id.as_uuid().is_nil()
                    && header.sender == reply.local_device_id
                    && header.recipient == reply.peer_device_id
                    && header.created_at_ms >= reply.created_at.get()
                    && header.created_at_ms < header.expires_at_ms
                    && header.expires_at_ms == reply.expires_at.get(),
            )
        };
        if !matches || self.queued.entries.len() >= MAX_QUEUED_REPLIES {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        }
        let installed = match self.queued.entries.entry(message_id) {
            Entry::Vacant(entry) => match self.build.take() {
                Some(ReplyBuildState::JoinedReady {
                    _committed: committed,
                    _input: input,
                }) => {
                    entry.insert(QueuedReplyEntry::Owned {
                        returned: None,
                        reply: Some(OwnedQueuedSettingsReply {
                            payload: Some(QueuedReplyPayload {
                                committed,
                                settlement: None,
                                input,
                            }),
                        }),
                    });
                    true
                }
                state => {
                    self.build = state;
                    false
                }
            },
            Entry::Occupied(_) => false,
        };
        if !installed {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        }
        let settled = match self.queued.entries.get_mut(&message_id) {
            Some(QueuedReplyEntry::Owned {
                reply: Some(reply),
                returned: None,
            }) => reply.settle(&self.ledger),
            _ => Err(self.ledger.fail()),
        };
        if let Err(error) = settled {
            self.ledger.fail();
            self.failure = Some(error.clone());
            self.close_intake();
            return Err(error);
        }
        self.ledger.changed.notify_waiters();
        Ok(message_id)
    }

    pub(in crate::devices) fn queued_reply(
        &self,
        message_id: MessageId,
    ) -> Result<Option<&OwnedQueuedSettingsReply>, SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.failure.is_some() {
            return Err(self.ledger.fail());
        }
        let Some(entry) = self.queued.entries.get(&message_id) else {
            return Ok(None);
        };
        let reply = match entry {
            QueuedReplyEntry::Owned {
                reply: Some(reply), ..
            } => reply,
            QueuedReplyEntry::Loaned { witness: Some(_) }
            | QueuedReplyEntry::StartedReturned(_) => return Ok(None),
            _ => return Err(self.ledger.fail()),
        };
        let committed = reply.committed().map_err(handoff_receive_error)?;
        let payload = reply.payload.as_ref().ok_or_else(|| self.ledger.fail())?;
        if committed.header().message_id != message_id
            || !Arc::ptr_eq(&payload.input.record.permit.ledger, &self.ledger)
        {
            return Err(self.ledger.fail());
        }
        Ok(Some(reply))
    }
}

impl Drop for OwnedQueuedSettingsReply {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let ledger = &payload.input.record.permit.ledger;
            ledger.fail();
            ledger.terminal.store(true, Ordering::Release);
            ledger.changed.notify_waiters();
            let _ = Box::leak(Box::new(AbandonedQueuedReply {
                _cause: QueuedReplyAbandoned,
                _payload: payload,
            }));
        }
    }
}

impl fmt::Debug for OwnedQueuedSettingsReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedQueuedSettingsReply([retained source])")
    }
}
