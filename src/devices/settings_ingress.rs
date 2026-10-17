mod access;
mod wire;
pub(super) mod work;

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use super::{
    DeviceEndpoint, DeviceError, DurableInbox, DurableInboxOutcome, PeerRegistration,
    SettingsConnectionIdentity,
};
use crate::agent::Agent;
use crate::crypto::{CryptoError, ReplayGuard, ReplayRecord};
use crate::protocol::{MessageId, UnixMillis};
use crate::storage::{
    SettingsAccessAdmissionError, SettingsAccessReceiptCommit, TransportInboxMetadata,
    TransportInboxRejection,
};

use access::AccessWorkState;
pub(crate) use access::{
    AccessStatusRejection, AuthenticatedSettingsAccessSource, OwnedCommittedSettingsAccess,
    OwnedVerifiedSettingsAccess, SettingsAccessStatusIntent, SettingsHostAction,
    SettingsRemoteRequest,
};
pub(crate) use wire::BindingGeneration;
pub(crate) use work::{SettingsReceiveDrain, SettingsReceiveWitness};

pub(crate) const SETTINGS_CLOCK_SKEW_MS: u64 = 5_000;

const STORAGE_FAILURE: &str = "owned_settings_receipt_failure";

#[derive(Clone, Error)]
pub(crate) enum SettingsReceiveError {
    #[error("private Settings receive capacity is exhausted")]
    Capacity,
    #[error("private Settings intake is closed")]
    Closed,
    #[error("private Settings receive bookkeeping failed")]
    Bookkeeping,
    #[error("private Settings receive worker failed")]
    Worker,
    #[error("private Settings device operation failed")]
    Device(#[source] Arc<DeviceError>),
    #[error("private Settings rejection storage failed")]
    RejectionStorage(#[source] Arc<crate::storage::StorageError>),
    #[error("private Settings access receipt admission failed")]
    AccessAdmission(#[source] Arc<SettingsAccessAdmissionError>),
    #[error("private Settings access source validation failed")]
    AccessSource(#[source] AccessStatusRejection),
}

impl fmt::Debug for SettingsReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReceiveError([retained cause])")
    }
}

impl SettingsReceiveError {
    fn quarantines(&self) -> bool {
        matches!(self, Self::Bookkeeping | Self::Worker)
            || matches!(self, Self::RejectionStorage(error) if !matches!(error.as_ref(), crate::storage::StorageError::TransportCapacity(_)))
            || matches!(self, Self::AccessAdmission(error) if error.quarantines())
    }
}

pub(super) struct ReceiveContext {
    pub(super) agent: Agent,
    pub(super) endpoint: DeviceEndpoint,
    pub(super) mutation: Arc<AsyncMutex<()>>,
}

struct VerifiedSettingsEnvelope {
    context: Arc<ReceiveContext>,
    connection: Arc<SettingsConnectionIdentity>,
    pin: PeerRegistration,
    pinned_identity: Value,
    message_id: String,
    sender: String,
    recipient: String,
    envelope_digest: String,
    outer_created_at: u64,
    outer_expires_at: u64,
    clock: ClockFloor,
    guard_active: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
pub(super) struct ClockFloor {
    wall: UnixMillis,
    monotonic: Instant,
}

impl ClockFloor {
    pub(super) fn capture() -> Result<Self, SettingsReceiveError> {
        Ok(Self {
            wall: UnixMillis::now()
                .map_err(|error| SettingsReceiveError::Device(Arc::new(error.into())))?,
            monotonic: Instant::now(),
        })
    }

    fn observe(&self) -> Result<(UnixMillis, Instant), AccessStatusRejection> {
        let wall = UnixMillis::now().map_err(|_| AccessStatusRejection::Unavailable)?;
        let monotonic = Instant::now();
        if wall < self.wall || monotonic < self.monotonic {
            return Err(AccessStatusRejection::OutsideScope);
        }
        Ok((wall, monotonic))
    }

    fn deadline(&self, expires_at: u64) -> Result<Instant, AccessStatusRejection> {
        let remaining = expires_at
            .checked_sub(self.wall.get())
            .ok_or(AccessStatusRejection::Expired)?;
        self.monotonic
            .checked_add(Duration::from_millis(remaining))
            .ok_or(AccessStatusRejection::Expired)
    }
}

impl VerifiedSettingsEnvelope {
    fn matches_context(
        &self,
        agent: &Agent,
        endpoint: &DeviceEndpoint,
        mutation: &Arc<AsyncMutex<()>>,
    ) -> bool {
        let leases_match = match (self.context.agent.state_lease(), agent.state_lease()) {
            (Some(original), Some(expected)) => Arc::ptr_eq(&original, &expected),
            _ => false,
        };
        leases_match
            && self.context.agent.store().shares_connection(&agent.store())
            && Arc::ptr_eq(&self.context.endpoint.inner, &endpoint.inner)
            && Arc::ptr_eq(&self.context.mutation, mutation)
    }

    fn connection(&self) -> &Arc<SettingsConnectionIdentity> {
        &self.connection
    }

    fn verified_peer_keys(&self) -> &crate::crypto::DevicePublicKeys {
        &self.pin.keys
    }

    fn pinned_identity(&self) -> &Value {
        &self.pinned_identity
    }

    fn receipt_metadata(&self) -> Result<TransportInboxMetadata<'_>, AccessStatusRejection> {
        Ok(TransportInboxMetadata {
            message_id: &self.message_id,
            sender: &self.sender,
            recipient: &self.recipient,
            envelope_digest: &self.envelope_digest,
            received_at: i64::try_from(self.clock.wall.get())
                .map_err(|_| AccessStatusRejection::Expired)?,
            expires_at: i64::try_from(self.outer_expires_at)
                .map_err(|_| AccessStatusRejection::Expired)?,
        })
    }
}

pub(crate) enum SettingsTransportReceipt {
    Ordinary { message_id: MessageId },
    PrivateRejected { message_id: MessageId },
    PrivateCommitted { message_id: MessageId },
    PrivateDuplicate { message_id: MessageId },
}

impl SettingsTransportReceipt {
    pub(crate) fn message_id(&self) -> MessageId {
        match self {
            Self::Ordinary { message_id }
            | Self::PrivateRejected { message_id }
            | Self::PrivateCommitted { message_id }
            | Self::PrivateDuplicate { message_id } => *message_id,
        }
    }
}

pub(super) struct WorkCompletion {
    pub(super) receipt: SettingsTransportReceipt,
}

enum ReplayOutcome {
    Unobserved,
    Legacy,
    Access,
    Duplicate,
    Failure(SettingsReceiveError),
}

struct OwnedReplay<'input> {
    input: &'input work::WorkerInput,
    command: &'input mut work::CommandWorkState,
    clock: ClockFloor,
    peer: PeerRegistration,
    pinned_identity: Value,
    digest: String,
    legacy: DurableInbox,
    outcome: ReplayOutcome,
}

impl ReplayGuard for OwnedReplay<'_> {
    fn record_if_new(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError> {
        if !matches!(self.outcome, ReplayOutcome::Unobserved) {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        }
        match super::routing::classify(plaintext) {
            super::routing::RoutingDecision::Ordinary => {
                self.outcome = ReplayOutcome::Legacy;
                self.legacy.record_ordinary(record, plaintext, now_ms)
            }
            super::routing::RoutingDecision::Rejected(
                TransportInboxRejection::SettingsUnavailable,
            ) => self.settings(record, plaintext, now_ms),
            super::routing::RoutingDecision::Rejected(reason) => {
                self.reject(record, now_ms, reason)
            }
        }
    }
}

impl OwnedReplay<'_> {
    fn fail(&mut self, failure: SettingsReceiveError) -> CryptoError {
        self.outcome = ReplayOutcome::Failure(failure);
        CryptoError::ReplayStorage(STORAGE_FAILURE.into())
    }

    fn reject(
        &mut self,
        record: ReplayRecord,
        now_ms: u64,
        reason: TransportInboxRejection,
    ) -> Result<bool, CryptoError> {
        self.outcome = ReplayOutcome::Legacy;
        self.legacy.record_rejected(record, now_ms, reason)
    }

    fn verified_envelope(
        &mut self,
        record: &ReplayRecord,
        now_ms: u64,
    ) -> Result<VerifiedSettingsEnvelope, CryptoError> {
        let envelope = &self.input.envelope;
        if record.message_id != envelope.header.message_id
            || record.sender != self.peer.keys.device_id
            || record.recipient != envelope.header.recipient
            || record.expires_at_ms != envelope.header.expires_at_ms
            || now_ms != self.clock.wall.get()
        {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        }
        Ok(VerifiedSettingsEnvelope {
            context: Arc::clone(&self.input.context),
            connection: Arc::clone(&self.input.connection),
            pin: self.peer.clone(),
            pinned_identity: self.pinned_identity.clone(),
            message_id: record.message_id.to_string(),
            sender: record.sender.to_string(),
            recipient: record.recipient.to_string(),
            envelope_digest: self.digest.clone(),
            outer_created_at: envelope.header.created_at_ms,
            outer_expires_at: record.expires_at_ms,
            clock: self.clock,
            guard_active: self.input.guard.active(),
        })
    }

    fn settings(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError> {
        if plaintext.len() > wire::MAX_PRIVATE_REQUEST_BYTES {
            return self.reject(record, now_ms, TransportInboxRejection::InvalidPacket);
        }
        match access::decode(plaintext) {
            access::DecodedAccessStatus::AccessStatus(intent) => {
                self.access(record, plaintext.len(), now_ms, intent)
            }
            access::DecodedAccessStatus::Invalid => {
                self.reject(record, now_ms, TransportInboxRejection::SettingsUnavailable)
            }
        }
    }

    fn access(
        &mut self,
        record: ReplayRecord,
        bytes: usize,
        now_ms: u64,
        intent: SettingsAccessStatusIntent,
    ) -> Result<bool, CryptoError> {
        if self.command.access.is_some() || self.command.authority.is_none() {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        }
        let envelope = self.verified_envelope(&record, now_ms)?;
        let Some(authority) = self.command.authority.take() else {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        };
        let witness = match self.input.ledger.private(self.peer.keys.device_id, bytes) {
            Ok(witness) => witness,
            Err(error) => {
                self.command.authority = Some(authority);
                self.command.access = Some(AccessWorkState::Rejected(error.clone()));
                return Err(self.fail(error));
            }
        };
        let verified = match OwnedVerifiedSettingsAccess::from_verified(
            envelope, intent, authority, witness,
        ) {
            Ok(verified) => verified,
            Err(error) => {
                self.command.access = Some(AccessWorkState::Rejected(
                    SettingsReceiveError::AccessSource(error),
                ));
                return self.reject(record, now_ms, TransportInboxRejection::SettingsUnavailable);
            }
        };
        self.command.access = Some(AccessWorkState::Verified {
            issue: None,
            reply: None,
            verified: Box::new(verified),
        });
        let Some(AccessWorkState::Verified {
            issue,
            reply,
            verified,
        }) = self.command.access.as_mut()
        else {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        };
        let admitted = self
            .input
            .context
            .agent
            .store()
            .admit_settings_access(verified);
        match admitted {
            Ok(SettingsAccessReceiptCommit::Duplicate) => {
                self.outcome = ReplayOutcome::Duplicate;
                Ok(false)
            }
            Ok(SettingsAccessReceiptCommit::Fresh { reply: committed }) => {
                *reply = Some(committed);
                self.outcome = ReplayOutcome::Access;
                Ok(true)
            }
            Err(error) => {
                let rejected = error.rejects_message();
                let failure = SettingsReceiveError::AccessAdmission(Arc::new(error));
                *issue = Some(failure.clone());
                if rejected {
                    self.reject(record, now_ms, TransportInboxRejection::SettingsUnavailable)
                } else {
                    Err(self.fail(failure))
                }
            }
        }
    }
}

fn execute(
    input: &work::WorkerInput,
    command: &mut work::CommandWorkState,
) -> Result<WorkCompletion, SettingsReceiveError> {
    let prepared = input
        .context
        .endpoint
        .prepare_envelope(&input.envelope)
        .map_err(|error| SettingsReceiveError::Device(Arc::new(error)))?;
    let clock = ClockFloor::capture()?;
    let mut replay = OwnedReplay {
        input,
        command,
        clock,
        peer: prepared.peer,
        pinned_identity: prepared.pinned_identity.clone(),
        digest: prepared.digest.clone(),
        legacy: DurableInbox {
            store: input.context.agent.store(),
            pinned_identity: prepared.pinned_identity,
            envelope_digest: prepared.digest,
            outcome: DurableInboxOutcome::Unobserved,
        },
        outcome: ReplayOutcome::Unobserved,
    };
    let opened = input.context.endpoint.inner.identity.open(
        &replay.peer.keys.clone(),
        &input.envelope,
        clock.wall.get(),
        &mut replay,
    );
    let id = input.envelope.header.message_id;
    match (opened, replay.outcome) {
        (Ok(raw), ReplayOutcome::Access) => {
            drop(raw);
            Ok(WorkCompletion {
                receipt: SettingsTransportReceipt::PrivateCommitted { message_id: id },
            })
        }
        (Err(CryptoError::Replay), ReplayOutcome::Duplicate) => Ok(WorkCompletion {
            receipt: SettingsTransportReceipt::PrivateDuplicate { message_id: id },
        }),
        (Err(CryptoError::ReplayStorage(marker)), ReplayOutcome::Failure(error))
            if marker == STORAGE_FAILURE =>
        {
            Err(error)
        }
        (opened, ReplayOutcome::Legacy) => {
            let rejected = matches!(
                &replay.legacy.outcome,
                DurableInboxOutcome::RejectedNew
                    | DurableInboxOutcome::RejectedDuplicate
                    | DurableInboxOutcome::RejectedFailure(_)
            );
            let outcome = match replay.legacy.outcome {
                DurableInboxOutcome::RejectedFailure(super::PrivateInboxFailure::Storage(
                    error,
                )) => {
                    return Err(SettingsReceiveError::RejectionStorage(Arc::new(error)));
                }
                outcome => outcome,
            };
            input
                .context
                .endpoint
                .finish_envelope(opened, outcome)
                .map_err(|error| SettingsReceiveError::Device(Arc::new(error)))?;
            Ok(WorkCompletion {
                receipt: if rejected {
                    SettingsTransportReceipt::PrivateRejected { message_id: id }
                } else {
                    SettingsTransportReceipt::Ordinary { message_id: id }
                },
            })
        }
        (Err(error), ReplayOutcome::Unobserved) => {
            Err(SettingsReceiveError::Device(Arc::new(error.into())))
        }
        _ => Err(SettingsReceiveError::Bookkeeping),
    }
}
