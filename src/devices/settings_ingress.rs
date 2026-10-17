mod access;
mod command;
pub(super) mod read;
mod read_source;
mod wire;
pub(super) mod work;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use super::{
    DeviceEndpoint, DeviceError, DurableInbox, DurableInboxOutcome, EndpointRole,
    EnvelopeAcceptance, PeerRegistration, SettingsConnectionIdentity,
};
use crate::agent::Agent;
use crate::crypto::{CryptoError, DevicePublicKeys, ReplayGuard, ReplayRecord};
use crate::host::settings::SettingsGrantHead;
use crate::protocol::{DeviceId, MessageId, UnixMillis};
use crate::storage::{
    SettingsAccessAdmissionError, SettingsAccessReceiptCommit, SettingsBindAdmission,
    SettingsBindAdmissionError, SettingsBindReceiptCommit, SettingsCommandAdmission,
    SettingsCommandAdmissionError, SettingsCommandReceiptCommit, Store, StoredSettingsBindReply,
    TransportInboxMetadata, TransportInboxRejection,
};

use access::AccessWorkState;
pub(crate) use access::{
    AccessStatusRejection, AuthenticatedSettingsAccessSource, OwnedCommittedSettingsAccess,
    OwnedVerifiedSettingsAccess, SettingsAccessStatusIntent,
};
use command::SettingsCommandOutcome;
pub(crate) use command::{
    AuthenticatedSettingsCommandSource, CommandRejection, CommittedSettingsCommandSource,
    OwnedCommittedSettingsCommand, OwnedHistoricalSettingsCommand, OwnedVerifiedSettingsCommand,
    SettingsCommandIntent, SettingsParentReference,
};
use read_source::ReadWorkState;
pub(crate) use read_source::{
    AuthenticatedSettingsReadSource, OwnedCommittedSettingsRead, OwnedVerifiedSettingsRead,
    SettingsReadSourceError,
};
pub(crate) use wire::{BindingGeneration, BindingIntent};
pub(crate) use work::{PendingSettingsReceipt, SettingsReceiveDrain, SettingsReceiveWitness};

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
    #[error("private Settings receipt admission failed")]
    Admission(#[source] Arc<SettingsBindAdmissionError>),
    #[error("private Settings command receipt admission failed")]
    CommandAdmission(#[source] Arc<SettingsCommandAdmissionError>),
    #[error("private Settings access receipt admission failed")]
    AccessAdmission(#[source] Arc<SettingsAccessAdmissionError>),
    #[error("private Settings access source validation failed")]
    AccessSource(#[source] AccessStatusRejection),
    #[error("private Settings read receipt admission failed")]
    ReadAdmission(#[source] Arc<crate::storage::SettingsReadAdmissionError>),
    #[error("private Settings read source validation failed")]
    ReadSource(#[source] Arc<SettingsReadSourceError>),
    #[error("private Settings read envelope encoding failed")]
    ReadEnvelopeEncoding(#[source] Arc<serde_json::Error>),
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
            || matches!(self, Self::Admission(error) if !error.is_capacity())
            || matches!(self, Self::CommandAdmission(error) if !error.is_capacity())
            || matches!(self, Self::AccessAdmission(error) if !error.is_capacity())
            || matches!(self, Self::AccessSource(AccessStatusRejection::Unavailable))
            || matches!(self, Self::ReadAdmission(error) if !error.is_capacity())
            || matches!(self, Self::ReadEnvelopeEncoding(_))
            || matches!(self, Self::ReadSource(error) if !matches!(error.as_ref(), SettingsReadSourceError::Source(BindRejection::Expired | BindRejection::OutsideScope)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum BindRejection {
    #[error("original Bind is expired")]
    Expired,
    #[error("Bind is outside its current scope")]
    OutsideScope,
    #[error("original Bind is already retained")]
    ExistingBinding,
    #[error("Bind conflicts with its retained original")]
    Conflict,
    #[error("Bind source is unavailable")]
    Unavailable,
}

pub(super) struct ReceiveContext {
    pub(super) agent: Agent,
    pub(super) endpoint: DeviceEndpoint,
    pub(super) mutation: Arc<AsyncMutex<()>>,
}

pub(crate) struct AuthenticatedSettingsSource {
    intent: BindingIntent,
    envelope: VerifiedSettingsEnvelope,
    history: Arc<BindingHistory>,
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

pub(crate) struct OwnedVerifiedSettingsPacket {
    intent: BindingIntent,
    source: AuthenticatedSettingsSource,
    _witness: work::PrivateWitness,
}

pub(crate) struct OwnedCommittedSettingsBind {
    commit: BindCommit,
    verified: OwnedVerifiedSettingsPacket,
}

struct BindCommit {
    reservation: StoredSettingsBindReply,
    grant: SettingsGrantHead,
    lifetime: BindingLifetime,
    creator: BindingCreator,
}

pub(crate) struct CommittedBindSource<'request> {
    request: &'request OwnedCommittedSettingsBind,
}

#[derive(Clone, Copy)]
pub(crate) struct BindingLifetime {
    pub(crate) expires_at: UnixMillis,
    pub(crate) deadline: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingHistoryState {
    Preparing,
    Committed,
    Unknown,
    Closed,
    Suppressed,
}

pub(crate) struct BindingHistorySnapshot {
    pub(crate) intent: BindingIntent,
    pub(crate) state: BindingHistoryState,
    pub(crate) lifetime: Option<BindingLifetime>,
}

pub(super) struct BindingHistory {
    connection: Arc<SettingsConnectionIdentity>,
    ledger: Arc<work::Ledger>,
    cells: Mutex<BTreeMap<(DeviceId, Uuid), Arc<BindingCell>>>,
}

struct BindingCell {
    intent: BindingIntent,
    ledger: Arc<work::Ledger>,
    creation: Arc<()>,
    state: Mutex<(BindingHistoryState, Option<BindingLifetime>)>,
}

pub(crate) struct BindingCreator {
    cell: Arc<BindingCell>,
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

    fn observe(&self) -> Result<(UnixMillis, Instant), BindRejection> {
        let wall = UnixMillis::now().map_err(|_| BindRejection::Unavailable)?;
        let monotonic = Instant::now();
        if wall < self.wall || monotonic < self.monotonic {
            return Err(BindRejection::Unavailable);
        }
        Ok((wall, monotonic))
    }

    fn deadline(&self, expires_at: u64) -> Result<Instant, BindRejection> {
        let remaining = expires_at
            .checked_sub(self.wall.get())
            .ok_or(BindRejection::Expired)?;
        self.monotonic
            .checked_add(Duration::from_millis(remaining))
            .ok_or(BindRejection::Expired)
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

    fn verified_peer_keys(&self) -> &DevicePublicKeys {
        &self.pin.keys
    }

    fn check_precommit(
        &self,
        store: &Store,
        host_id: DeviceId,
        generation: &BindingGeneration,
        issued_at: u64,
        expires_at: u64,
    ) -> Result<(UnixMillis, Instant), BindRejection> {
        if !self.guard_active.load(Ordering::Acquire)
            || !self.connection.is_live()
            || !self.context.endpoint.shares_store(store)
            || !self.context.agent.store().shares_connection(store)
            || self.context.endpoint.role() != &EndpointRole::Host
            || host_id != self.connection.host_identity().host_id()
            || generation.host_epoch != self.connection.host_identity().host_epoch()
            || generation.connection_epoch != self.connection.connection_epoch()
            || self.outer_expires_at <= self.outer_created_at
            || self.outer_expires_at - self.outer_created_at > 30_000
            || self.outer_expires_at > expires_at
        {
            return Err(BindRejection::OutsideScope);
        }
        let observed = self.clock.observe()?;
        if observed.0.get() < issued_at
            || observed.0.get() >= expires_at
            || observed.0.get() >= self.outer_expires_at
            || observed.1 >= self.clock.deadline(expires_at)?
            || observed.1 >= self.clock.deadline(self.outer_expires_at)?
        {
            return Err(BindRejection::Expired);
        }
        Ok(observed)
    }

    fn receipt_metadata(&self) -> Result<TransportInboxMetadata<'_>, BindRejection> {
        Ok(TransportInboxMetadata {
            message_id: &self.message_id,
            sender: &self.sender,
            recipient: &self.recipient,
            envelope_digest: &self.envelope_digest,
            received_at: i64::try_from(self.clock.wall.get())
                .map_err(|_| BindRejection::Expired)?,
            expires_at: i64::try_from(self.outer_expires_at).map_err(|_| BindRejection::Expired)?,
        })
    }

    fn pinned_identity(&self) -> &Value {
        &self.pinned_identity
    }
}

impl AuthenticatedSettingsSource {
    pub(crate) fn matches_context(
        &self,
        agent: &Agent,
        endpoint: &DeviceEndpoint,
        mutation: &Arc<AsyncMutex<()>>,
    ) -> bool {
        self.envelope.matches_context(agent, endpoint, mutation)
    }
    pub(crate) fn connection(&self) -> &Arc<SettingsConnectionIdentity> {
        self.envelope.connection()
    }
    pub(crate) fn verified_peer_keys(&self) -> &DevicePublicKeys {
        self.envelope.verified_peer_keys()
    }
    pub(crate) fn receipt_metadata(&self) -> Result<TransportInboxMetadata<'_>, BindRejection> {
        self.envelope.receipt_metadata()
    }
    pub(crate) fn pinned_identity(&self) -> &Value {
        self.envelope.pinned_identity()
    }

    pub(crate) fn check_precommit(
        &self,
        store: &Store,
        intent: &BindingIntent,
    ) -> Result<(UnixMillis, Instant), BindRejection> {
        if intent != &self.intent {
            return Err(BindRejection::OutsideScope);
        }
        self.envelope.check_precommit(
            store,
            intent.host_id,
            &intent.generation,
            intent.issued_at,
            intent.expires_at,
        )
    }

    pub(crate) fn parent_lifetime(
        &self,
        intent: &BindingIntent,
        grant_expires_at: UnixMillis,
    ) -> Result<BindingLifetime, BindRejection> {
        let expires = intent
            .requested_parent_expires_at
            .min(
                intent
                    .issued_at
                    .checked_add(300_000)
                    .ok_or(BindRejection::Expired)?,
            )
            .min(grant_expires_at.get());
        let (wall, monotonic) = self.envelope.clock.observe()?;
        let deadline = self.envelope.clock.deadline(expires)?;
        if wall.get() >= expires || monotonic >= deadline {
            return Err(BindRejection::Expired);
        }
        Ok(BindingLifetime {
            expires_at: UnixMillis::try_from(expires).map_err(|_| BindRejection::Expired)?,
            deadline,
        })
    }

    pub(crate) fn reserve_binding(
        &self,
        intent: &BindingIntent,
    ) -> Result<BindingCreator, SettingsBindAdmissionError> {
        if intent != &self.intent {
            return Err(SettingsBindAdmissionError::Rejected(
                BindRejection::OutsideScope,
            ));
        }
        if !Arc::ptr_eq(&self.history.connection, &self.envelope.connection) {
            self.history.ledger.fail();
            return Err(SettingsBindAdmissionError::Bookkeeping);
        }
        let mut cells = self.history.cells.lock().map_err(|_| {
            self.history.ledger.fail();
            SettingsBindAdmissionError::Bookkeeping
        })?;
        let key = (self.envelope.pin.keys.device_id, intent.binding_request_id);
        if let Some(original) = cells.get(&key) {
            return Err(SettingsBindAdmissionError::Rejected(
                if original.intent == *intent {
                    BindRejection::ExistingBinding
                } else {
                    BindRejection::Conflict
                },
            ));
        }
        if cells.len() >= 32
            || cells
                .keys()
                .filter(|(peer, _)| *peer == self.envelope.pin.keys.device_id)
                .count()
                >= 4
        {
            return Err(SettingsBindAdmissionError::HistoryCapacity);
        }
        let cell = Arc::new(BindingCell {
            intent: intent.clone(),
            ledger: Arc::clone(&self.history.ledger),
            creation: Arc::new(()),
            state: Mutex::new((BindingHistoryState::Preparing, None)),
        });
        cells.insert(key, Arc::clone(&cell));
        Ok(BindingCreator { cell })
    }
}

impl BindingHistory {
    fn new(connection: Arc<SettingsConnectionIdentity>, ledger: Arc<work::Ledger>) -> Self {
        Self {
            connection,
            ledger,
            cells: Mutex::new(BTreeMap::new()),
        }
    }

    fn healthy(&self) -> bool {
        match self.cells.lock() {
            Ok(cells) => cells.values().all(|cell| !cell.state.is_poisoned()),
            Err(_) => false,
        }
    }
}

impl BindingCreator {
    pub(crate) fn committed(
        &self,
        lifetime: BindingLifetime,
    ) -> Result<(), SettingsBindAdmissionError> {
        let mut state = self.cell.state.lock().map_err(|_| {
            self.cell.ledger.fail();
            SettingsBindAdmissionError::Bookkeeping
        })?;
        if state.0 != BindingHistoryState::Preparing {
            self.cell.ledger.fail();
            return Err(SettingsBindAdmissionError::Bookkeeping);
        }
        *state = (BindingHistoryState::Committed, Some(lifetime));
        Ok(())
    }
}

impl Drop for BindingCreator {
    fn drop(&mut self) {
        match self.cell.state.lock() {
            Ok(mut state) => {
                if matches!(
                    state.0,
                    BindingHistoryState::Preparing | BindingHistoryState::Committed
                ) {
                    state.0 = BindingHistoryState::Unknown;
                }
            }
            Err(_) => {
                self.cell.ledger.fail();
            }
        }
    }
}

impl OwnedVerifiedSettingsPacket {
    pub(crate) fn authority_source(&self) -> &AuthenticatedSettingsSource {
        &self.source
    }
    pub(crate) fn binding_intent(&self) -> &BindingIntent {
        &self.intent
    }
}

impl OwnedCommittedSettingsBind {
    pub(crate) fn authority_source(&self) -> &AuthenticatedSettingsSource {
        &self.verified.source
    }
    pub(crate) fn committed_source(&self) -> CommittedBindSource<'_> {
        CommittedBindSource { request: self }
    }
    pub(crate) fn binding_intent(&self) -> &BindingIntent {
        &self.verified.intent
    }
    pub(crate) fn binding_snapshot(&self) -> Result<BindingHistorySnapshot, SettingsReceiveError> {
        let state = self
            .commit
            .creator
            .cell
            .state
            .lock()
            .map_err(|_| self.commit.creator.cell.ledger.fail())?;
        Ok(BindingHistorySnapshot {
            intent: self.verified.intent.clone(),
            state: state.0,
            lifetime: state.1,
        })
    }
}

impl CommittedBindSource<'_> {
    pub(crate) fn authority_source(&self) -> &AuthenticatedSettingsSource {
        self.request.authority_source()
    }
    pub(crate) fn lifetime(&self) -> BindingLifetime {
        self.request.commit.lifetime
    }
    pub(crate) fn grant(&self) -> &SettingsGrantHead {
        &self.request.commit.grant
    }
    pub(crate) fn reply_reservation(&self) -> &StoredSettingsBindReply {
        &self.request.commit.reservation
    }
    pub(crate) fn creation_identity(&self) -> &Arc<()> {
        &self.request.commit.creator.cell.creation
    }
}

pub(crate) enum SettingsTransportReceipt {
    Ordinary {
        message_id: MessageId,
        acceptance: EnvelopeAcceptance,
    },
    PrivateRejected {
        message_id: MessageId,
        acceptance: EnvelopeAcceptance,
    },
    PrivateCommitted {
        message_id: MessageId,
    },
    PrivateDuplicate {
        message_id: MessageId,
    },
}

pub(crate) enum SettingsReceiveEvent {
    CommittedBind(OwnedCommittedSettingsBind),
    CommittedCommand(OwnedCommittedSettingsCommand),
    HistoricalCommand(OwnedHistoricalSettingsCommand),
    CommittedAccess(OwnedCommittedSettingsAccess),
    CommittedRead(OwnedCommittedSettingsRead),
    ReceiptSettled,
}

pub(super) struct WorkCompletion {
    pub(super) receipt: SettingsTransportReceipt,
    pub(super) bind: Option<OwnedCommittedSettingsBind>,
}

enum ReplayOutcome {
    Unobserved,
    Legacy,
    Committed(Box<OwnedCommittedSettingsBind>),
    Command,
    Access,
    Read,
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
            ) => self.bind(record, plaintext, now_ms),
            super::routing::RoutingDecision::Rejected(reason) => {
                self.outcome = ReplayOutcome::Legacy;
                self.legacy.record_rejected(record, now_ms, reason)
            }
        }
    }
}

impl OwnedReplay<'_> {
    fn fail(&mut self, failure: SettingsReceiveError) -> CryptoError {
        self.outcome = ReplayOutcome::Failure(failure);
        CryptoError::ReplayStorage(STORAGE_FAILURE.into())
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

    fn bind(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError> {
        if plaintext.len() > wire::MAX_PRIVATE_REQUEST_BYTES {
            return Err(self.fail(SettingsReceiveError::Capacity));
        }
        if let command::DecodedCommand::Update(intent) = command::decode(plaintext) {
            return self.update(record, plaintext.len(), now_ms, intent);
        }
        if let access::DecodedAccessStatus::AccessStatus(intent) = access::decode(plaintext) {
            return self.access(record, plaintext.len(), now_ms, intent);
        }
        if let Ok(request) = read::decode(plaintext) {
            return self.read(record, plaintext.len(), now_ms, request);
        }
        let reason =
            match wire::decode(plaintext) {
                wire::DecodedBind::Bind(intent) => {
                    let witness = match self
                        .input
                        .ledger
                        .private(self.peer.keys.device_id, plaintext.len())
                    {
                        Ok(witness) => witness,
                        Err(error) => return Err(self.fail(error)),
                    };
                    let envelope = self.verified_envelope(&record, now_ms)?;
                    let source_intent = intent.clone();
                    let verified = OwnedVerifiedSettingsPacket {
                        intent,
                        source: AuthenticatedSettingsSource {
                            intent: source_intent,
                            envelope,
                            history: Arc::clone(&self.input.history),
                        },
                        _witness: witness,
                    };
                    let admitted = self.input.context.agent.store().admit_settings_bind(
                        &SettingsBindAdmission {
                            source: verified.authority_source(),
                            intent: verified.binding_intent(),
                        },
                    );
                    match admitted {
                        Ok(SettingsBindReceiptCommit::Duplicate) => {
                            self.outcome = ReplayOutcome::Duplicate;
                            return Ok(false);
                        }
                        Ok(SettingsBindReceiptCommit::Fresh {
                            reservation,
                            grant,
                            lifetime,
                            creator,
                        }) => {
                            self.outcome =
                                ReplayOutcome::Committed(Box::new(OwnedCommittedSettingsBind {
                                    verified,
                                    commit: BindCommit {
                                        reservation,
                                        grant: *grant,
                                        lifetime,
                                        creator,
                                    },
                                }));
                            return Ok(true);
                        }
                        Err(SettingsBindAdmissionError::Rejected(_)) => {
                            TransportInboxRejection::SettingsUnavailable
                        }
                        Err(error) => {
                            return Err(self.fail(SettingsReceiveError::Admission(Arc::new(error))));
                        }
                    }
                }
                wire::DecodedBind::Unavailable => TransportInboxRejection::SettingsUnavailable,
                wire::DecodedBind::Invalid => TransportInboxRejection::InvalidPacket,
            };
        self.outcome = ReplayOutcome::Legacy;
        self.legacy.record_rejected(record, now_ms, reason)
    }

    fn access(
        &mut self,
        record: ReplayRecord,
        bytes: usize,
        now_ms: u64,
        intent: SettingsAccessStatusIntent,
    ) -> Result<bool, CryptoError> {
        if self.command.command.is_some()
            || self.command.outcome.is_some()
            || self.command.access.is_some()
            || self.command.read.is_some()
            || self.command.authority.is_none()
        {
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
                let failure = SettingsReceiveError::AccessSource(error);
                self.command.access = Some(AccessWorkState::Rejected(failure.clone()));
                if matches!(
                    error,
                    AccessStatusRejection::Expired | AccessStatusRejection::OutsideScope
                ) {
                    self.outcome = ReplayOutcome::Legacy;
                    return self.legacy.record_rejected(
                        record,
                        now_ms,
                        TransportInboxRejection::SettingsUnavailable,
                    );
                }
                return Err(self.fail(failure));
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
                let policy = error.is_policy_rejection();
                let failure = SettingsReceiveError::AccessAdmission(Arc::new(error));
                *issue = Some(failure.clone());
                if policy {
                    self.outcome = ReplayOutcome::Legacy;
                    self.legacy.record_rejected(
                        record,
                        now_ms,
                        TransportInboxRejection::SettingsUnavailable,
                    )
                } else {
                    Err(self.fail(failure))
                }
            }
        }
    }

    fn read(
        &mut self,
        record: ReplayRecord,
        bytes: usize,
        now_ms: u64,
        request: read::SettingsReadRequest,
    ) -> Result<bool, CryptoError> {
        use crate::storage::SettingsReadReceiptCommit;

        if self.command.command.is_some()
            || self.command.outcome.is_some()
            || self.command.access.is_some()
            || self.command.read.is_some()
            || self.command.authority.is_none()
        {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        }
        let encoded = match serde_json::to_vec(&self.input.envelope) {
            Ok(encoded) => encoded,
            Err(error) => {
                let failure = SettingsReceiveError::ReadEnvelopeEncoding(Arc::new(error));
                self.command.read = Some(ReadWorkState::Rejected(failure.clone()));
                return Err(self.fail(failure));
            }
        };
        let oversized = encoded.len() > 49_152;
        drop(encoded);
        if oversized {
            self.command.read = Some(ReadWorkState::Rejected(SettingsReceiveError::Capacity));
            return Err(self.fail(SettingsReceiveError::Capacity));
        }
        let envelope = self.verified_envelope(&record, now_ms)?;
        let Some(authority) = self.command.authority.take() else {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        };
        let witness = match self.input.ledger.private(self.peer.keys.device_id, bytes) {
            Ok(witness) => witness,
            Err(error) => {
                self.command.authority = Some(authority);
                self.command.read = Some(ReadWorkState::Rejected(error.clone()));
                return Err(self.fail(error));
            }
        };
        let verified =
            match OwnedVerifiedSettingsRead::from_verified(envelope, request, authority, witness) {
                Ok(verified) => verified,
                Err(error) => {
                    let policy = matches!(
                        &error,
                        SettingsReadSourceError::Source(
                            BindRejection::Expired | BindRejection::OutsideScope
                        )
                    );
                    let failure = SettingsReceiveError::ReadSource(Arc::new(error));
                    self.command.read = Some(ReadWorkState::Rejected(failure.clone()));
                    if policy {
                        self.outcome = ReplayOutcome::Legacy;
                        return self.legacy.record_rejected(
                            record,
                            now_ms,
                            TransportInboxRejection::SettingsUnavailable,
                        );
                    }
                    return Err(self.fail(failure));
                }
            };
        self.command.read = Some(ReadWorkState::Verified {
            result: None,
            verified: Box::new(verified),
        });
        let Some(ReadWorkState::Verified { result, verified }) = self.command.read.as_mut() else {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        };
        *result = Some(
            self.input
                .context
                .agent
                .store()
                .admit_settings_read(verified)
                .map_err(Arc::new),
        );
        match result.as_ref() {
            Some(Ok(SettingsReadReceiptCommit::Duplicate)) => {
                self.outcome = ReplayOutcome::Duplicate;
                Ok(false)
            }
            Some(Ok(
                SettingsReadReceiptCommit::Known | SettingsReadReceiptCommit::Fresh { .. },
            )) => {
                self.outcome = ReplayOutcome::Read;
                Ok(true)
            }
            Some(Err(error)) => {
                let policy = error.is_policy_rejection();
                let failure = SettingsReceiveError::ReadAdmission(Arc::clone(error));
                if policy {
                    self.outcome = ReplayOutcome::Legacy;
                    self.legacy.record_rejected(
                        record,
                        now_ms,
                        TransportInboxRejection::SettingsUnavailable,
                    )
                } else {
                    Err(self.fail(failure))
                }
            }
            None => Err(self.fail(SettingsReceiveError::Bookkeeping)),
        }
    }

    fn update(
        &mut self,
        record: ReplayRecord,
        bytes: usize,
        now_ms: u64,
        intent: SettingsCommandIntent,
    ) -> Result<bool, CryptoError> {
        if self.command.command.is_some()
            || self.command.outcome.is_some()
            || self.command.authority.is_none()
        {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        }
        let envelope = self.verified_envelope(&record, now_ms)?;
        let deadline = match envelope.clock.deadline(intent.expires_at.get()) {
            Ok(deadline) => deadline,
            Err(BindRejection::Expired) => {
                self.outcome = ReplayOutcome::Legacy;
                return self.legacy.record_rejected(
                    record,
                    now_ms,
                    TransportInboxRejection::SettingsUnavailable,
                );
            }
            Err(_) => return Err(self.fail(SettingsReceiveError::Bookkeeping)),
        };
        let Some(authority) = self.command.authority.take() else {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        };
        let witness = match self.input.ledger.private(self.peer.keys.device_id, bytes) {
            Ok(witness) => witness,
            Err(error) => {
                self.command.authority = Some(authority);
                return Err(self.fail(error));
            }
        };
        self.command.command = Some(OwnedVerifiedSettingsCommand::from_verified_update(
            envelope, intent, authority, deadline, witness,
        ));
        let Some(verified) = self.command.command.as_ref() else {
            return Err(self.fail(SettingsReceiveError::Bookkeeping));
        };
        let admitted =
            self.input
                .context
                .agent
                .store()
                .admit_settings_command(&SettingsCommandAdmission {
                    source: verified.authority_source(),
                    authority: verified.receipt_authority(),
                });
        match admitted {
            Ok(SettingsCommandReceiptCommit::Duplicate) => {
                self.outcome = ReplayOutcome::Duplicate;
                Ok(false)
            }
            Ok(SettingsCommandReceiptCommit::Fresh { original, reply }) => {
                let Some(verified) = self.command.command.take() else {
                    return Err(self.fail(SettingsReceiveError::Bookkeeping));
                };
                self.command.outcome = Some(SettingsCommandOutcome::Fresh(
                    OwnedCommittedSettingsCommand::from_commit(verified, original, reply),
                ));
                self.outcome = ReplayOutcome::Command;
                Ok(true)
            }
            Ok(SettingsCommandReceiptCommit::Known { original, reply }) => {
                let Some(verified) = self.command.command.take() else {
                    return Err(self.fail(SettingsReceiveError::Bookkeeping));
                };
                self.command.outcome = Some(SettingsCommandOutcome::Historical(
                    OwnedHistoricalSettingsCommand::from_commit(verified, original, reply),
                ));
                self.outcome = ReplayOutcome::Command;
                Ok(true)
            }
            Err(error) if error.is_policy_rejection() => {
                self.outcome = ReplayOutcome::Legacy;
                self.legacy.record_rejected(
                    record,
                    now_ms,
                    TransportInboxRejection::SettingsUnavailable,
                )
            }
            Err(error) => Err(self.fail(SettingsReceiveError::CommandAdmission(Arc::new(error)))),
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
        (Ok(raw), ReplayOutcome::Committed(bind)) => {
            drop(raw);
            Ok(WorkCompletion {
                receipt: SettingsTransportReceipt::PrivateCommitted { message_id: id },
                bind: Some(*bind),
            })
        }
        (Ok(raw), ReplayOutcome::Command) => {
            drop(raw);
            Ok(WorkCompletion {
                receipt: SettingsTransportReceipt::PrivateCommitted { message_id: id },
                bind: None,
            })
        }
        (Ok(raw), ReplayOutcome::Access) => {
            drop(raw);
            Ok(WorkCompletion {
                receipt: SettingsTransportReceipt::PrivateCommitted { message_id: id },
                bind: None,
            })
        }
        (Ok(raw), ReplayOutcome::Read) => {
            drop(raw);
            Ok(WorkCompletion {
                receipt: SettingsTransportReceipt::PrivateCommitted { message_id: id },
                bind: None,
            })
        }
        (Err(CryptoError::Replay), ReplayOutcome::Duplicate) => Ok(WorkCompletion {
            receipt: SettingsTransportReceipt::PrivateDuplicate { message_id: id },
            bind: None,
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
            let acceptance = input
                .context
                .endpoint
                .finish_envelope(opened, outcome)
                .map_err(|error| SettingsReceiveError::Device(Arc::new(error)))?;
            Ok(WorkCompletion {
                receipt: if rejected {
                    SettingsTransportReceipt::PrivateRejected {
                        message_id: id,
                        acceptance,
                    }
                } else {
                    SettingsTransportReceipt::Ordinary {
                        message_id: id,
                        acceptance,
                    }
                },
                bind: None,
            })
        }
        (Err(error), ReplayOutcome::Unobserved) => {
            Err(SettingsReceiveError::Device(Arc::new(error.into())))
        }
        _ => Err(SettingsReceiveError::Bookkeeping),
    }
}

impl fmt::Debug for AuthenticatedSettingsSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatedSettingsSource([owned verification])")
    }
}

impl fmt::Debug for OwnedVerifiedSettingsPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedVerifiedSettingsPacket([private])")
    }
}

impl fmt::Debug for OwnedCommittedSettingsBind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedCommittedSettingsBind([private])")
    }
}
