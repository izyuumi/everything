mod build;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedMutexGuard, oneshot};
use tokio::task::{Id, JoinSet};
use uuid::Uuid;

pub(crate) use build::OwnedQueuedSettingsReply;
use build::{QueuedReplyRegistry, ReplyBuildState, ReplyBuildWorkerOutput};

use super::access::{AccessReceiptOutput, AccessWorkState, OwnedCommittedSettingsAccess};
use super::{
    BindingHistory, CommittedBindSource, CommittedSettingsCommandSource,
    OwnedCommittedSettingsBind, OwnedCommittedSettingsCommand, OwnedHistoricalSettingsCommand,
    OwnedVerifiedSettingsCommand, ReceiveContext, SettingsCommandOutcome, SettingsReceiveError,
    SettingsReceiveEvent, SettingsTransportReceipt, WorkCompletion,
};
use crate::crypto::EncryptedEnvelope;
use crate::devices::SettingsConnectionIdentity;
use crate::host::settings::agent::{
    PairedControlSendError, PairedHandoffReadError, PairedResultCorrelation, PairedResultHandoff,
    SettingsAgentRuntime,
};
use crate::host::settings::service::{
    PairedSettingsParent, SettingsAccessReplyFamily, SettingsReceiptAuthority,
};
use crate::protocol::{DeviceId, UnixMillis};
use crate::storage::{
    StoredSettingsAccessReply, StoredSettingsBindReply, StoredSettingsCommandReply,
};

#[derive(Default)]
struct Counts {
    frame: bool,
    jobs: usize,
    private: usize,
    bytes: usize,
    peers: BTreeMap<DeviceId, usize>,
    build: Option<ReplyBuildSlot>,
}

pub(super) struct Ledger {
    counts: Mutex<Counts>,
    failed: AtomicBool,
    terminal: AtomicBool,
    closed: AtomicBool,
    changed: Notify,
}

#[derive(Clone)]
pub(crate) struct SettingsReceiveWitness {
    ledger: Arc<Ledger>,
}

#[derive(Clone, Copy)]
pub(crate) struct SettingsReceiveDrain {
    pub(crate) intake_closed: bool,
    pub(crate) frame: bool,
    pub(crate) private_requests: usize,
    pub(crate) private_bytes: usize,
    pub(crate) original_jobs: usize,
    pub(crate) reply_build_active: bool,
    pub(crate) bookkeeping_ok: bool,
    pub(crate) terminal_quarantine: bool,
}

pub(crate) struct PendingSettingsReceipt {
    receiver: oneshot::Receiver<Result<SettingsTransportReceipt, SettingsReceiveError>>,
}

pub(in crate::devices) struct SettingsReceiveWork {
    ledger: Arc<Ledger>,
    jobs: JoinSet<RegisteredJobOutput>,
    receipts: BTreeMap<Id, oneshot::Sender<Result<SettingsTransportReceipt, SettingsReceiveError>>>,
    pending: Option<PendingInput>,
    build: Option<ReplyBuildState>,
    queued: QueuedReplyRegistry,
    history: Option<Arc<BindingHistory>>,
    context: Option<Arc<ReceiveContext>>,
    closed: bool,
    failure: Option<SettingsReceiveError>,
    retained_join: Option<JoinedReceiveWork>,
    unexpected_join: Option<JoinedRegisteredJob>,
}

struct PendingInput {
    receipt_authority: SettingsReceiptAuthority,
    envelope: EncryptedEnvelope,
    context: Arc<ReceiveContext>,
    connection: Arc<SettingsConnectionIdentity>,
    history: Arc<BindingHistory>,
    waiting: Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>,
    receipt: oneshot::Sender<Result<SettingsTransportReceipt, SettingsReceiveError>>,
    frame: FrameWitness,
}

pub(super) struct WorkerInput {
    receipt_authority: Option<SettingsReceiptAuthority>,
    pub(super) envelope: EncryptedEnvelope,
    pub(super) context: Arc<ReceiveContext>,
    pub(super) connection: Arc<SettingsConnectionIdentity>,
    pub(super) history: Arc<BindingHistory>,
    pub(super) ledger: Arc<Ledger>,
    pub(super) guard: GuardScope,
    frame: Option<FrameWitness>,
}

struct WorkerOutput {
    receipt_authority: Option<SettingsReceiptAuthority>,
    guard: GuardScope,
    context: Arc<ReceiveContext>,
    completion: Result<WorkCompletion, SettingsReceiveError>,
    retained_command: Option<OwnedVerifiedSettingsCommand>,
    command_outcome: Option<SettingsCommandOutcome>,
    access: Option<AccessWorkState>,
    frame: Option<FrameWitness>,
}

type JoinedReceiveWork = Result<(Id, WorkerOutput), tokio::task::JoinError>;
type JoinedRegisteredJob = Result<(Id, RegisteredJobOutput), tokio::task::JoinError>;

enum RegisteredJobOutput {
    Receive(Box<WorkerOutput>),
    ReplyBuild(Box<ReplyBuildWorkerOutput>),
}

enum RegisteredProgress {
    Receive,
    ReplyBuild,
}

enum ReadyWork {
    ReceiveGuard(OwnedMutexGuard<()>),
    ReplyBuildGuard(OwnedMutexGuard<()>),
    Joined(JoinedRegisteredJob),
}

pub(in crate::devices) enum SettingsReceiveWorkEvent {
    Receive(Box<SettingsReceiveEvent>),
    ReplyBuildJoined,
}

struct UnmatchedRegisteredJoin {
    _cause: SettingsReceiveError,
    _joined: JoinedRegisteredJob,
}

enum AccessJoinedDisposition {
    Fresh,
    Settled,
    Capacity,
}

pub(super) struct CommandWorkState {
    pub(super) authority: Option<SettingsReceiptAuthority>,
    pub(super) command: Option<OwnedVerifiedSettingsCommand>,
    pub(super) outcome: Option<SettingsCommandOutcome>,
    pub(super) access: Option<AccessWorkState>,
}

pub(super) struct GuardScope {
    guard: Option<OwnedMutexGuard<()>>,
    active: Arc<AtomicBool>,
}

struct FrameWitness {
    ledger: Arc<Ledger>,
}

pub(super) struct PrivateWitness {
    ledger: Arc<Ledger>,
    peer: DeviceId,
    bytes: usize,
    build: Option<JoinedReplyBuild>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyBuildFamily {
    BindDefault49152,
    CommandInspection90112,
    AccessDefault49152,
    AccessDenial4096,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReplyBuildStamp {
    reservation_id: Uuid,
    local_device_id: DeviceId,
    peer_device_id: DeviceId,
    created_at: UnixMillis,
    expires_at: UnixMillis,
}

struct ReplyBuildAssociation {
    family: ReplyBuildFamily,
    original: Arc<()>,
    reply: ReplyBuildStamp,
}

#[derive(PartialEq, Eq)]
enum ReplyBuildClaim {
    Unclaimed,
    Claimed,
}

struct JoinedReplyBuild {
    association: Arc<ReplyBuildAssociation>,
    claim: ReplyBuildClaim,
}

struct ReplyBuildSlot {
    identity: Arc<()>,
    association: Arc<ReplyBuildAssociation>,
}

pub(crate) struct ReplyBuildPermit {
    ledger: Arc<Ledger>,
    slot: Arc<()>,
    association: Arc<ReplyBuildAssociation>,
    armed: bool,
}

pub(crate) enum SettingsReplyBuildRequest {
    Bind {
        parent: PairedSettingsParent,
        request: Box<OwnedCommittedSettingsBind>,
    },
    Fresh(Box<OwnedCommittedSettingsCommand>),
    Historical(Box<OwnedHistoricalSettingsCommand>),
    Access(Box<OwnedCommittedSettingsAccess>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::devices) enum ReplyBuildAdmissionReason {
    Busy,
    Closed,
    AlreadyClaimed,
    Bookkeeping,
}

pub(in crate::devices) struct ReplyBuildAdmissionFailure {
    pub(in crate::devices) reason: ReplyBuildAdmissionReason,
    pub(in crate::devices) request: SettingsReplyBuildRequest,
}

pub(in crate::devices) struct PendingSettingsReplyBuild {
    slot: Arc<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::devices) enum PairedResultAdmissionReason {
    Receive(ReplyBuildAdmissionReason),
    RejectedBind,
}

pub(in crate::devices) struct PairedResultAdmissionFailure {
    pub(in crate::devices) reason: PairedResultAdmissionReason,
    pub(in crate::devices) handoff: PairedResultHandoff,
}

pub(crate) struct PairedWorkAdoption {
    correlation: PairedResultCorrelation,
}

enum RegisteredReplyBuildOwner {
    Direct(SettingsReplyBuildRequest),
    Supervised(Box<PairedResultHandoff>),
}

struct RegisteredReplyBuild {
    adoption: Option<PairedWorkAdoption>,
    adoption_sent: bool,
    request: RegisteredReplyBuildOwner,
    permit: ReplyBuildPermit,
}

type TerminalQuarantine = (
    JoinSet<RegisteredJobOutput>,
    Option<PendingInput>,
    Option<ReplyBuildState>,
    QueuedReplyRegistry,
    BTreeMap<Id, oneshot::Sender<Result<SettingsTransportReceipt, SettingsReceiveError>>>,
    Option<Arc<ReceiveContext>>,
    Option<Arc<BindingHistory>>,
    Arc<Ledger>,
    Option<SettingsReceiveError>,
    Option<JoinedReceiveWork>,
    Option<JoinedRegisteredJob>,
);

impl ReplyBuildFamily {
    fn access(reply: &StoredSettingsAccessReply) -> Self {
        match reply.family {
            SettingsAccessReplyFamily::Default => Self::AccessDefault49152,
            SettingsAccessReplyFamily::Denial => Self::AccessDenial4096,
        }
    }
}

impl ReplyBuildStamp {
    fn access(reply: &StoredSettingsAccessReply) -> Self {
        Self {
            reservation_id: reply.reservation_id,
            local_device_id: reply.local_device_id,
            peer_device_id: reply.peer_device_id,
            created_at: reply.created_at,
            expires_at: reply.expires_at,
        }
    }

    fn bind(reply: &StoredSettingsBindReply) -> Self {
        Self {
            reservation_id: reply.reservation_id,
            local_device_id: reply.local_device_id,
            peer_device_id: reply.peer_device_id,
            created_at: reply.created_at,
            expires_at: reply.expires_at,
        }
    }

    fn command(reply: &StoredSettingsCommandReply) -> Self {
        Self {
            reservation_id: reply.reservation_id,
            local_device_id: reply.local_device_id,
            peer_device_id: reply.peer_device_id,
            created_at: reply.created_at,
            expires_at: reply.expires_at,
        }
    }
}

impl ReplyBuildAssociation {
    fn matches(
        &self,
        family: ReplyBuildFamily,
        original: &Arc<()>,
        reply: ReplyBuildStamp,
    ) -> bool {
        self.family == family && Arc::ptr_eq(&self.original, original) && self.reply == reply
    }
}

impl ReplyBuildPermit {
    pub(crate) fn check_access(
        &self,
        request: &OwnedCommittedSettingsAccess,
    ) -> Result<(), SettingsReceiveError> {
        let source = request.verified_request().authority_source();
        let reply = request.reply_reservation();
        self.check(
            request.reply_build_witness(),
            ReplyBuildFamily::access(reply),
            source.invocation_identity(),
            ReplyBuildStamp::access(reply),
        )
    }

    pub(crate) fn check_bind(
        &self,
        source: &CommittedBindSource<'_>,
    ) -> Result<(), SettingsReceiveError> {
        self.check(
            &source.request.verified._witness,
            ReplyBuildFamily::BindDefault49152,
            source.creation_identity(),
            ReplyBuildStamp::bind(source.reply_reservation()),
        )
    }

    pub(crate) fn check_fresh(
        &self,
        source: &CommittedSettingsCommandSource<'_>,
    ) -> Result<(), SettingsReceiveError> {
        self.check(
            source.reply_build_witness(),
            ReplyBuildFamily::CommandInspection90112,
            source.authority_source().invocation_identity(),
            ReplyBuildStamp::command(source.reply_reservation()),
        )
    }

    pub(crate) fn check_historical(
        &self,
        request: &OwnedHistoricalSettingsCommand,
    ) -> Result<(), SettingsReceiveError> {
        self.check(
            request.reply_build_witness(),
            ReplyBuildFamily::CommandInspection90112,
            request.authority_source().invocation_identity(),
            ReplyBuildStamp::command(request.reply_reservation()),
        )
    }

    fn check(
        &self,
        witness: &PrivateWitness,
        family: ReplyBuildFamily,
        original: &Arc<()>,
        reply: ReplyBuildStamp,
    ) -> Result<(), SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.ledger.closed.load(Ordering::Acquire) {
            return Err(SettingsReceiveError::Closed);
        }
        let joined = witness.build.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !self.armed
            || !Arc::ptr_eq(&self.ledger, &witness.ledger)
            || !Arc::ptr_eq(&self.association, &joined.association)
            || joined.claim != ReplyBuildClaim::Claimed
            || !self.association.matches(family, original, reply)
        {
            return Err(self.ledger.fail());
        }
        let counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
        self.ledger.check_admission()?;
        if self.ledger.closed.load(Ordering::Acquire) {
            return Err(SettingsReceiveError::Closed);
        }
        if counts.build.as_ref().is_none_or(|slot| {
            !Arc::ptr_eq(&slot.identity, &self.slot)
                || !Arc::ptr_eq(&slot.association, &self.association)
        }) {
            return Err(self.ledger.fail());
        }
        Ok(())
    }
}

impl Drop for ReplyBuildPermit {
    fn drop(&mut self) {
        if self.armed {
            self.ledger.fail();
        }
    }
}

impl SettingsReplyBuildRequest {
    fn witness(&self) -> &PrivateWitness {
        match self {
            Self::Bind { request, .. } => &request.verified._witness,
            Self::Fresh(request) => request.reply_build_witness(),
            Self::Historical(request) => request.reply_build_witness(),
            Self::Access(request) => request.reply_build_witness(),
        }
    }

    fn witness_mut(&mut self) -> &mut PrivateWitness {
        match self {
            Self::Bind { request, .. } => &mut request.verified._witness,
            Self::Fresh(request) => request.reply_build_witness_mut(),
            Self::Historical(request) => request.reply_build_witness_mut(),
            Self::Access(request) => request.reply_build_witness_mut(),
        }
    }

    fn matches_context(
        &self,
        context: &ReceiveContext,
        connection: &Arc<SettingsConnectionIdentity>,
    ) -> bool {
        match self {
            Self::Bind { request, .. } => {
                let source = request.authority_source();
                Arc::ptr_eq(source.connection(), connection)
                    && source.matches_context(&context.agent, &context.endpoint, &context.mutation)
            }
            Self::Fresh(request) => {
                let committed = request.committed_source();
                let source = committed.authority_source();
                Arc::ptr_eq(source.connection(), connection)
                    && source.matches_context(&context.agent, &context.endpoint, &context.mutation)
            }
            Self::Historical(request) => {
                let source = request.authority_source();
                Arc::ptr_eq(source.connection(), connection)
                    && source.matches_context(&context.agent, &context.endpoint, &context.mutation)
            }
            Self::Access(request) => {
                let source = request.verified_request().authority_source();
                Arc::ptr_eq(source.connection(), connection)
                    && source.matches_context(&context.agent, &context.endpoint, &context.mutation)
            }
        }
    }

    fn matches_association(&self, association: &ReplyBuildAssociation) -> bool {
        match self {
            Self::Bind { request, .. } => {
                let source = request.committed_source();
                association.matches(
                    ReplyBuildFamily::BindDefault49152,
                    source.creation_identity(),
                    ReplyBuildStamp::bind(source.reply_reservation()),
                )
            }
            Self::Fresh(request) => {
                let source = request.committed_source();
                association.matches(
                    ReplyBuildFamily::CommandInspection90112,
                    source.authority_source().invocation_identity(),
                    ReplyBuildStamp::command(source.reply_reservation()),
                )
            }
            Self::Historical(request) => association.matches(
                ReplyBuildFamily::CommandInspection90112,
                request.authority_source().invocation_identity(),
                ReplyBuildStamp::command(request.reply_reservation()),
            ),
            Self::Access(request) => association.matches(
                ReplyBuildFamily::access(request.reply_reservation()),
                request
                    .verified_request()
                    .authority_source()
                    .invocation_identity(),
                ReplyBuildStamp::access(request.reply_reservation()),
            ),
        }
    }
}

impl fmt::Debug for ReplyBuildPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReplyBuildPermit([owned memory slot])")
    }
}

impl fmt::Debug for SettingsReplyBuildRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyBuildRequest([owned])")
    }
}

impl fmt::Debug for ReplyBuildAdmissionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReplyBuildAdmissionFailure([retained input])")
    }
}

impl fmt::Debug for PendingSettingsReplyBuild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingSettingsReplyBuild([retained work])")
    }
}

impl fmt::Debug for PairedResultAdmissionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedResultAdmissionFailure([retained handoff])")
    }
}

impl PairedWorkAdoption {
    pub(crate) fn correlation(&self) -> &PairedResultCorrelation {
        &self.correlation
    }
}

impl fmt::Debug for PairedWorkAdoption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedWorkAdoption([registered custody])")
    }
}

impl Ledger {
    fn check_admission(&self) -> Result<(), SettingsReceiveError> {
        if self.failed.load(Ordering::Acquire) || self.terminal.load(Ordering::Acquire) {
            return Err(self.fail());
        }
        Ok(())
    }

    pub(super) fn private(
        self: &Arc<Self>,
        peer: DeviceId,
        bytes: usize,
    ) -> Result<PrivateWitness, SettingsReceiveError> {
        self.check_admission()?;
        let mut counts = self.counts.lock().map_err(|_| self.fail())?;
        self.check_admission()?;
        let peer_count = counts.peers.get(&peer).copied().unwrap_or(0);
        let next = counts.bytes.checked_add(bytes).ok_or_else(|| self.fail())?;
        if bytes > super::wire::MAX_PRIVATE_REQUEST_BYTES
            || counts.private >= 32
            || peer_count >= 4
            || next > 1_048_576
        {
            return Err(SettingsReceiveError::Capacity);
        }
        counts.private += 1;
        counts.bytes = next;
        counts.peers.insert(peer, peer_count + 1);
        Ok(PrivateWitness {
            ledger: Arc::clone(self),
            peer,
            bytes,
            build: None,
        })
    }

    pub(super) fn fail(&self) -> SettingsReceiveError {
        self.failed.store(true, Ordering::Release);
        self.changed.notify_waiters();
        SettingsReceiveError::Bookkeeping
    }

    fn snapshot(&self) -> SettingsReceiveDrain {
        match self.counts.lock() {
            Ok(counts) => SettingsReceiveDrain {
                intake_closed: self.closed.load(Ordering::Acquire),
                frame: counts.frame,
                private_requests: counts.private,
                private_bytes: counts.bytes,
                original_jobs: counts.jobs,
                reply_build_active: counts.build.is_some(),
                bookkeeping_ok: !self.failed.load(Ordering::Acquire),
                terminal_quarantine: self.terminal.load(Ordering::Acquire),
            },
            Err(_) => SettingsReceiveDrain {
                intake_closed: self.closed.load(Ordering::Acquire),
                frame: true,
                private_requests: 32,
                private_bytes: 1_048_576,
                original_jobs: 1,
                reply_build_active: true,
                bookkeeping_ok: false,
                terminal_quarantine: self.terminal.load(Ordering::Acquire),
            },
        }
    }
}

impl SettingsReceiveDrain {
    pub(crate) fn complete(&self) -> bool {
        self.intake_closed
            && !self.frame
            && self.private_requests == 0
            && self.private_bytes == 0
            && self.original_jobs == 0
            && !self.reply_build_active
            && self.bookkeeping_ok
            && !self.terminal_quarantine
    }
}

impl SettingsReceiveWitness {
    pub(crate) fn snapshot(&self) -> SettingsReceiveDrain {
        self.ledger.snapshot()
    }
}

impl PendingSettingsReceipt {
    pub(in crate::devices) async fn joined_receipt(
        self,
    ) -> Result<SettingsTransportReceipt, SettingsReceiveError> {
        self.receiver
            .await
            .map_err(|_| SettingsReceiveError::Closed)?
    }
}

impl SettingsReceiveWork {
    pub(in crate::devices) fn new() -> Self {
        Self {
            ledger: Arc::new(Ledger {
                counts: Mutex::new(Counts::default()),
                failed: AtomicBool::new(false),
                terminal: AtomicBool::new(false),
                closed: AtomicBool::new(true),
                changed: Notify::new(),
            }),
            jobs: JoinSet::new(),
            receipts: BTreeMap::new(),
            pending: None,
            build: None,
            queued: QueuedReplyRegistry::default(),
            history: None,
            context: None,
            closed: true,
            failure: None,
            retained_join: None,
            unexpected_join: None,
        }
    }

    pub(in crate::devices) fn witness(&self) -> SettingsReceiveWitness {
        SettingsReceiveWitness {
            ledger: Arc::clone(&self.ledger),
        }
    }

    pub(in crate::devices) fn connect(
        &mut self,
        connection: Arc<SettingsConnectionIdentity>,
        context: Arc<ReceiveContext>,
    ) -> Result<(), SettingsReceiveError> {
        if !self.report().complete() || self.failure.is_some() {
            return Err(SettingsReceiveError::Bookkeeping);
        }
        self.history = Some(Arc::new(BindingHistory::new(
            connection,
            Arc::clone(&self.ledger),
        )));
        self.context = Some(context);
        self.closed = false;
        self.ledger.closed.store(false, Ordering::Release);
        Ok(())
    }

    pub(in crate::devices) fn context(&self) -> Result<Arc<ReceiveContext>, SettingsReceiveError> {
        self.context
            .as_ref()
            .cloned()
            .ok_or(SettingsReceiveError::Closed)
    }

    fn check_unjoined_build(
        &self,
        witness: &PrivateWitness,
        connection: &Arc<SettingsConnectionIdentity>,
        peer: DeviceId,
    ) -> Result<(), SettingsReceiveError> {
        self.ledger.check_admission()?;
        if !Arc::ptr_eq(&witness.ledger, &self.ledger)
            || witness.peer != peer
            || witness.build.is_some()
            || self
                .history
                .as_ref()
                .is_none_or(|history| !Arc::ptr_eq(&history.connection, connection))
        {
            return Err(self.ledger.fail());
        }
        Ok(())
    }

    fn seal_access_join(&mut self) -> Result<(), SettingsReceiveError> {
        let Some(Ok((_, output))) = self.retained_join.as_ref() else {
            return Err(self.ledger.fail());
        };
        let Some(
            access @ AccessWorkState::Verified {
                issue: None,
                reply: Some(reply),
                verified,
            },
        ) = output.access.as_ref()
        else {
            return Err(self.ledger.fail());
        };
        let source = verified.authority_source();
        let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !source.matches_context(&context.agent, &context.endpoint, &context.mutation) {
            return Err(self.ledger.fail());
        }
        self.check_unjoined_build(
            access.witness().ok_or_else(|| self.ledger.fail())?,
            source.connection(),
            source.verified_peer_keys().device_id,
        )?;
        let association = Arc::new(ReplyBuildAssociation {
            family: ReplyBuildFamily::access(reply),
            original: Arc::clone(source.invocation_identity()),
            reply: ReplyBuildStamp::access(reply),
        });
        let witness = self
            .retained_join
            .as_mut()
            .and_then(|joined| joined.as_mut().ok())
            .and_then(|(_, output)| output.access.as_mut())
            .and_then(AccessWorkState::witness_mut)
            .ok_or_else(|| self.ledger.fail())?;
        if witness.build.is_some() {
            return Err(self.ledger.fail());
        }
        witness.build = Some(JoinedReplyBuild {
            association,
            claim: ReplyBuildClaim::Unclaimed,
        });
        Ok(())
    }

    fn seal_bind_join(
        &self,
        request: &mut OwnedCommittedSettingsBind,
    ) -> Result<(), SettingsReceiveError> {
        let source = request.authority_source();
        let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !source.matches_context(&context.agent, &context.endpoint, &context.mutation) {
            return Err(self.ledger.fail());
        }
        self.check_unjoined_build(
            &request.verified._witness,
            source.connection(),
            source.verified_peer_keys().device_id,
        )?;
        let committed = request.committed_source();
        let association = Arc::new(ReplyBuildAssociation {
            family: ReplyBuildFamily::BindDefault49152,
            original: Arc::clone(committed.creation_identity()),
            reply: ReplyBuildStamp::bind(committed.reply_reservation()),
        });
        request.verified._witness.build = Some(JoinedReplyBuild {
            association,
            claim: ReplyBuildClaim::Unclaimed,
        });
        Ok(())
    }

    fn seal_fresh_join(
        &self,
        request: &mut OwnedCommittedSettingsCommand,
    ) -> Result<(), SettingsReceiveError> {
        let committed = request.committed_source();
        let source = committed.authority_source();
        let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !source.matches_context(&context.agent, &context.endpoint, &context.mutation) {
            return Err(self.ledger.fail());
        }
        self.check_unjoined_build(
            request.reply_build_witness(),
            source.connection(),
            source.verified_peer_keys().device_id,
        )?;
        let association = Arc::new(ReplyBuildAssociation {
            family: ReplyBuildFamily::CommandInspection90112,
            original: Arc::clone(source.invocation_identity()),
            reply: ReplyBuildStamp::command(committed.reply_reservation()),
        });
        request.reply_build_witness_mut().build = Some(JoinedReplyBuild {
            association,
            claim: ReplyBuildClaim::Unclaimed,
        });
        Ok(())
    }

    fn seal_historical_join(
        &self,
        request: &mut OwnedHistoricalSettingsCommand,
    ) -> Result<(), SettingsReceiveError> {
        let source = request.authority_source();
        let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !source.matches_context(&context.agent, &context.endpoint, &context.mutation) {
            return Err(self.ledger.fail());
        }
        self.check_unjoined_build(
            request.reply_build_witness(),
            source.connection(),
            source.verified_peer_keys().device_id,
        )?;
        let association = Arc::new(ReplyBuildAssociation {
            family: ReplyBuildFamily::CommandInspection90112,
            original: Arc::clone(source.invocation_identity()),
            reply: ReplyBuildStamp::command(request.reply_reservation()),
        });
        request.reply_build_witness_mut().build = Some(JoinedReplyBuild {
            association,
            claim: ReplyBuildClaim::Unclaimed,
        });
        Ok(())
    }

    pub(in crate::devices) fn try_register_reply_build(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        mut request: SettingsReplyBuildRequest,
    ) -> Result<PendingSettingsReplyBuild, ReplyBuildAdmissionFailure> {
        let permit = match self.reserve_reply_build(connection, &mut request) {
            Ok(permit) => permit,
            Err(reason) => return Err(ReplyBuildAdmissionFailure { reason, request }),
        };
        let slot = Arc::clone(&permit.slot);
        self.build = Some(ReplyBuildState::Unstarted(RegisteredReplyBuild {
            adoption: None,
            adoption_sent: true,
            request: RegisteredReplyBuildOwner::Direct(request),
            permit,
        }));
        Ok(PendingSettingsReplyBuild { slot })
    }

    pub(in crate::devices) fn try_adopt_paired_result(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        mut handoff: PairedResultHandoff,
    ) -> Result<PendingSettingsReplyBuild, Box<PairedResultAdmissionFailure>> {
        let request = match handoff.reply_request_mut() {
            Ok(request) => request,
            Err(error) => {
                let reason = match error {
                    PairedHandoffReadError::Closed => {
                        PairedResultAdmissionReason::Receive(ReplyBuildAdmissionReason::Closed)
                    }
                    PairedHandoffReadError::RejectedBind => {
                        PairedResultAdmissionReason::RejectedBind
                    }
                    PairedHandoffReadError::Bookkeeping => {
                        self.ledger.fail();
                        PairedResultAdmissionReason::Receive(ReplyBuildAdmissionReason::Bookkeeping)
                    }
                };
                return Err(Box::new(PairedResultAdmissionFailure { reason, handoff }));
            }
        };
        let permit = match self.reserve_reply_build(connection, request) {
            Ok(permit) => permit,
            Err(reason) => {
                return Err(Box::new(PairedResultAdmissionFailure {
                    reason: PairedResultAdmissionReason::Receive(reason),
                    handoff,
                }));
            }
        };
        let slot = Arc::clone(&permit.slot);
        self.build = Some(ReplyBuildState::Unstarted(RegisteredReplyBuild {
            adoption: None,
            adoption_sent: false,
            request: RegisteredReplyBuildOwner::Supervised(Box::new(handoff)),
            permit,
        }));
        Ok(PendingSettingsReplyBuild { slot })
    }

    pub(in crate::devices) fn try_ack_paired_adoption(
        &mut self,
        runtime: &SettingsAgentRuntime,
    ) -> Result<bool, SettingsReceiveError> {
        self.ledger.check_admission()?;
        let Some(state) = self.build.as_mut() else {
            return Ok(true);
        };
        let ReplyBuildState::Unstarted(record) = state else {
            return Ok(true);
        };
        if record.adoption_sent {
            return Ok(true);
        }
        let RegisteredReplyBuildOwner::Supervised(handoff) = &record.request else {
            return Err(self.ledger.fail());
        };
        let proof = match record.adoption.take() {
            Some(proof) => proof,
            None => {
                let correlation = handoff
                    .adoption_correlation()
                    .ok_or_else(|| self.ledger.fail())?;
                PairedWorkAdoption { correlation }
            }
        };
        match runtime.try_ack_paired_adoption(proof) {
            Ok(()) => {
                record.adoption_sent = true;
                Ok(true)
            }
            Err(PairedControlSendError::Full(proof)) => {
                record.adoption = Some(proof);
                Ok(false)
            }
            Err(PairedControlSendError::Closed(proof)) => {
                record.adoption = Some(proof);
                Err(SettingsReceiveError::Closed)
            }
        }
    }

    fn reserve_reply_build(
        &self,
        connection: &Arc<SettingsConnectionIdentity>,
        request: &mut SettingsReplyBuildRequest,
    ) -> Result<ReplyBuildPermit, ReplyBuildAdmissionReason> {
        let bookkeeping = || {
            self.ledger.fail();
            ReplyBuildAdmissionReason::Bookkeeping
        };
        self.ledger.check_admission().map_err(|_| bookkeeping())?;
        if self.failure.is_some() {
            return Err(bookkeeping());
        }
        if self.closed || self.ledger.closed.load(Ordering::Acquire) || !connection.is_live() {
            return Err(ReplyBuildAdmissionReason::Closed);
        }
        let context = self.context.as_ref().ok_or_else(bookkeeping)?;
        if self
            .history
            .as_ref()
            .is_none_or(|history| !Arc::ptr_eq(&history.connection, connection))
            || !request.matches_context(context, connection)
        {
            return Err(bookkeeping());
        }
        let witness = request.witness();
        let joined = witness.build.as_ref().ok_or_else(bookkeeping)?;
        if !Arc::ptr_eq(&witness.ledger, &self.ledger)
            || witness.peer != joined.association.reply.peer_device_id
            || !request.matches_association(&joined.association)
        {
            return Err(bookkeeping());
        }
        if joined.claim == ReplyBuildClaim::Claimed {
            return Err(ReplyBuildAdmissionReason::AlreadyClaimed);
        }
        let association = Arc::clone(&joined.association);
        let slot = Arc::new(());
        let mut counts = self.ledger.counts.lock().map_err(|_| bookkeeping())?;
        self.ledger.check_admission().map_err(|_| bookkeeping())?;
        if self.closed || self.ledger.closed.load(Ordering::Acquire) || !connection.is_live() {
            return Err(ReplyBuildAdmissionReason::Closed);
        }
        if self.build.is_some() != counts.build.is_some() {
            return Err(bookkeeping());
        }
        if counts.build.is_some() {
            return Err(ReplyBuildAdmissionReason::Busy);
        }
        let joined = request
            .witness_mut()
            .build
            .as_mut()
            .ok_or_else(bookkeeping)?;
        if !Arc::ptr_eq(&joined.association, &association)
            || joined.claim != ReplyBuildClaim::Unclaimed
        {
            return Err(bookkeeping());
        }
        joined.claim = ReplyBuildClaim::Claimed;
        counts.build = Some(ReplyBuildSlot {
            identity: Arc::clone(&slot),
            association: Arc::clone(&association),
        });
        Ok(ReplyBuildPermit {
            ledger: Arc::clone(&self.ledger),
            slot,
            association,
            armed: true,
        })
    }

    fn withdraw_unstarted_build(
        &mut self,
    ) -> Result<Option<SettingsReplyBuildRequest>, SettingsReceiveError> {
        let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
        self.ledger.check_admission()?;
        let Some(state) = self.build.as_ref() else {
            return if counts.build.is_none() {
                Ok(None)
            } else {
                Err(self.ledger.fail())
            };
        };
        let ReplyBuildState::Unstarted(record) = state else {
            return Err(SettingsReceiveError::Closed);
        };
        let RegisteredReplyBuildOwner::Direct(request) = &record.request else {
            return Err(self.ledger.fail());
        };
        let witness = request.witness();
        let joined = witness.build.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !record.permit.armed
            || !Arc::ptr_eq(&record.permit.ledger, &self.ledger)
            || !Arc::ptr_eq(&witness.ledger, &self.ledger)
            || !Arc::ptr_eq(&joined.association, &record.permit.association)
            || joined.claim != ReplyBuildClaim::Claimed
            || !request.matches_association(&joined.association)
            || counts.build.as_ref().is_none_or(|slot| {
                !Arc::ptr_eq(&slot.identity, &record.permit.slot)
                    || !Arc::ptr_eq(&slot.association, &record.permit.association)
            })
        {
            return Err(self.ledger.fail());
        }
        let record = self.build.take().ok_or_else(|| self.ledger.fail())?;
        match record {
            ReplyBuildState::Unstarted(RegisteredReplyBuild {
                request: RegisteredReplyBuildOwner::Direct(request),
                mut permit,
                ..
            }) => {
                counts.build = None;
                permit.armed = false;
                drop(counts);
                drop(permit);
                self.ledger.changed.notify_waiters();
                Ok(Some(request))
            }
            record => {
                self.build = Some(record);
                Err(self.ledger.fail())
            }
        }
    }

    pub(in crate::devices) fn admit(
        &mut self,
        context: Arc<ReceiveContext>,
        connection: Arc<SettingsConnectionIdentity>,
        envelope: EncryptedEnvelope,
        receipt_authority: SettingsReceiptAuthority,
    ) -> Result<PendingSettingsReceipt, SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.closed || self.failure.is_some() || !connection.is_live() {
            return Err(SettingsReceiveError::Closed);
        }
        if envelope.ciphertext.len() > crate::crypto::MAX_CIPHERTEXT_BYTES {
            return Err(SettingsReceiveError::Capacity);
        }
        if self
            .context
            .as_ref()
            .is_none_or(|original| !Arc::ptr_eq(original, &context))
        {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        }
        let history = self
            .history
            .as_ref()
            .filter(|history| Arc::ptr_eq(&history.connection, &connection))
            .cloned()
            .ok_or(SettingsReceiveError::Closed)?;
        let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
        self.ledger.check_admission()?;
        if counts.frame {
            return Err(SettingsReceiveError::Capacity);
        }
        counts.frame = true;
        drop(counts);
        let frame = FrameWitness {
            ledger: Arc::clone(&self.ledger),
        };
        let waiting = Box::pin(Arc::clone(&context.mutation).lock_owned());
        let (receipt, receiver) = oneshot::channel();
        self.pending = Some(PendingInput {
            receipt_authority,
            envelope,
            context,
            connection,
            history,
            waiting,
            receipt,
            frame,
        });
        Ok(PendingSettingsReceipt { receiver })
    }

    pub(in crate::devices) async fn next(
        &mut self,
    ) -> Result<Option<SettingsReceiveWorkEvent>, SettingsReceiveError> {
        if let Err(error) = self.ledger.check_admission()
            && self.failure.is_none()
        {
            self.failure = Some(error);
        }
        if self.failure.is_some() || self.retained_join.is_some() || self.unexpected_join.is_some()
        {
            self.close_intake();
        }
        loop {
            let build_waiting = matches!(self.build, Some(ReplyBuildState::Waiting { .. }));
            if self.jobs.is_empty() && self.pending.is_none() && !build_waiting {
                return match &self.failure {
                    Some(error) => Err(error.clone()),
                    None => Ok(None),
                };
            }
            let has_jobs = !self.jobs.is_empty();
            let receive = &mut self.pending;
            let build = &mut self.build;
            let receive_ready = async {
                match receive {
                    Some(pending) => pending.waiting.as_mut().await,
                    None => std::future::pending().await,
                }
            };
            let build_ready = async {
                match build {
                    Some(ReplyBuildState::Waiting { waiting, .. }) => waiting.as_mut().await,
                    _ => std::future::pending().await,
                }
            };
            let ready = tokio::select! {
                biased;
                joined = self.jobs.join_next_with_id(), if has_jobs => {
                    match joined {
                        Some(joined) => ReadyWork::Joined(joined),
                        None => return Err(self.ledger.fail()),
                    }
                }
                guard = receive_ready => ReadyWork::ReceiveGuard(guard),
                guard = build_ready => ReadyWork::ReplyBuildGuard(guard),
            };
            match ready {
                ReadyWork::ReceiveGuard(guard) => {
                    if let Err(error) = self.spawn_receive(guard) {
                        self.failure = Some(error.clone());
                        self.close_intake();
                        return Err(error);
                    }
                }
                ReadyWork::ReplyBuildGuard(guard) => {
                    if let Err(error) = self.spawn_reply_build(guard) {
                        self.failure = Some(error.clone());
                        self.close_intake();
                        return Err(error);
                    }
                }
                ReadyWork::Joined(joined) => match self.register_join(joined)? {
                    RegisteredProgress::Receive => {
                        return self.finish_receive_join().map(|event| {
                            event.map(|event| SettingsReceiveWorkEvent::Receive(Box::new(event)))
                        });
                    }
                    RegisteredProgress::ReplyBuild => {
                        self.finish_reply_build_join()?;
                        return Ok(Some(SettingsReceiveWorkEvent::ReplyBuildJoined));
                    }
                },
            }
        }
    }

    fn spawn_receive(&mut self, guard: OwnedMutexGuard<()>) -> Result<(), SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.closed {
            return Err(SettingsReceiveError::Closed);
        }
        let Some(pending) = self.pending.take() else {
            return Err(self.ledger.fail());
        };
        let PendingInput {
            receipt_authority,
            envelope,
            context,
            connection,
            history,
            waiting,
            receipt,
            frame,
        } = pending;
        drop(waiting);
        let input = WorkerInput {
            receipt_authority: Some(receipt_authority),
            envelope,
            context,
            connection,
            history,
            ledger: Arc::clone(&self.ledger),
            guard: GuardScope {
                guard: Some(guard),
                active: Arc::new(AtomicBool::new(true)),
            },
            frame: Some(frame),
        };
        {
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            self.ledger.check_admission()?;
            counts.jobs = counts
                .jobs
                .checked_add(1)
                .ok_or_else(|| self.ledger.fail())?;
        }
        let task = self
            .jobs
            .spawn_blocking(move || RegisteredJobOutput::Receive(Box::new(run(input))));
        if self.receipts.insert(task.id(), receipt).is_some() {
            self.failure = Some(self.ledger.fail());
            self.closed = true;
        }
        Ok(())
    }

    fn register_join(
        &mut self,
        joined: JoinedRegisteredJob,
    ) -> Result<RegisteredProgress, SettingsReceiveError> {
        let unexpected = match joined {
            Ok((id, RegisteredJobOutput::Receive(output))) if self.retained_join.is_none() => {
                self.retained_join = Some(Ok((id, *output)));
                return Ok(RegisteredProgress::Receive);
            }
            Ok((id, RegisteredJobOutput::ReplyBuild(output))) => {
                match self.register_reply_build_join(Ok((id, *output))) {
                    Ok(()) => return Ok(RegisteredProgress::ReplyBuild),
                    Err(Ok((id, output))) => {
                        Ok((id, RegisteredJobOutput::ReplyBuild(Box::new(output))))
                    }
                    Err(Err(error)) => Err(error),
                }
            }
            Err(error)
                if self
                    .reply_build_task()
                    .is_some_and(|task| task == error.id()) =>
            {
                match self.register_reply_build_join(Err(error)) {
                    Ok(()) => return Ok(RegisteredProgress::ReplyBuild),
                    Err(Ok((id, output))) => {
                        Ok((id, RegisteredJobOutput::ReplyBuild(Box::new(output))))
                    }
                    Err(Err(error)) => Err(error),
                }
            }
            Err(error)
                if self.retained_join.is_none() && self.receipts.contains_key(&error.id()) =>
            {
                self.retained_join = Some(Err(error));
                return Ok(RegisteredProgress::Receive);
            }
            joined => joined,
        };
        if self.unexpected_join.is_none() {
            self.unexpected_join = Some(unexpected);
            if let Some(Ok((_, RegisteredJobOutput::Receive(output)))) =
                self.unexpected_join.as_mut()
            {
                output.guard.active.store(false, Ordering::Release);
                drop(output.guard.guard.take());
            }
        } else {
            let mut retained = Box::new(UnmatchedRegisteredJoin {
                _cause: SettingsReceiveError::Bookkeeping,
                _joined: unexpected,
            });
            if let Ok((_, RegisteredJobOutput::Receive(output))) = &mut retained._joined {
                output.guard.active.store(false, Ordering::Release);
                drop(output.guard.guard.take());
            }
            self.ledger.terminal.store(true, Ordering::Release);
            let _ = Box::leak(retained);
        }
        self.latch_failure();
        Err(SettingsReceiveError::Bookkeeping)
    }

    fn finish_receive_join(
        &mut self,
    ) -> Result<Option<SettingsReceiveEvent>, SettingsReceiveError> {
        if let Some(Ok((_, output))) = self.retained_join.as_mut() {
            output.guard.active.store(false, Ordering::Release);
            drop(output.guard.guard.take());
        }
        let accounted = (|| {
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            counts.jobs = counts
                .jobs
                .checked_sub(1)
                .ok_or_else(|| self.ledger.fail())?;
            Ok::<_, SettingsReceiveError>(())
        })();
        if let Err(error) = accounted {
            self.failure = Some(error.clone());
            self.close_intake();
            return Err(error);
        }
        let id = match self.retained_join.as_ref() {
            Some(Ok((id, _))) => *id,
            Some(Err(error)) => {
                if let Some(receipt) = self.receipts.remove(&error.id()) {
                    let _ = receipt.send(Err(SettingsReceiveError::Worker));
                }
                self.failure = Some(SettingsReceiveError::Worker);
                self.ledger.fail();
                self.close_intake();
                return Err(SettingsReceiveError::Worker);
            }
            None => {
                self.latch_failure();
                return Err(SettingsReceiveError::Bookkeeping);
            }
        };
        let Some(receipt) = self.receipts.remove(&id) else {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        };
        if matches!(self.retained_join.as_ref(), Some(Ok((_, output))) if output.access.is_some()) {
            return self.finish_access_join(receipt);
        }
        let output = match self.retained_join.take() {
            Some(Ok((_, output))) => output,
            retained => {
                self.retained_join = retained;
                self.latch_failure();
                let _ = receipt.send(Err(SettingsReceiveError::Bookkeeping));
                return Err(SettingsReceiveError::Bookkeeping);
            }
        };
        let WorkerOutput {
            receipt_authority,
            completion,
            retained_command,
            command_outcome,
            access,
            guard,
            context,
            frame,
        } = output;
        drop(access);
        if frame.is_none() {
            self.failure = Some(self.ledger.fail());
            self.closed = true;
            let _ = receipt.send(Err(SettingsReceiveError::Bookkeeping));
            drop(receipt_authority);
            drop(guard);
            drop(context);
            drop(retained_command);
            drop(completion);
            drop(command_outcome);
            return Err(SettingsReceiveError::Bookkeeping);
        }
        drop(receipt_authority);
        drop(guard);
        drop(context);
        drop(retained_command);
        match completion {
            Ok(WorkCompletion {
                receipt: result,
                mut bind,
            }) => {
                let mut command_outcome = command_outcome;
                let sealed = match (&mut bind, &mut command_outcome) {
                    (Some(bind), None) => self.seal_bind_join(bind),
                    (None, Some(SettingsCommandOutcome::Fresh(command))) => {
                        self.seal_fresh_join(command)
                    }
                    (None, Some(SettingsCommandOutcome::Historical(command))) => {
                        self.seal_historical_join(command)
                    }
                    _ => Ok(()),
                };
                if let Err(error) = sealed {
                    let error = self.failure.clone().unwrap_or(error);
                    self.failure = Some(error.clone());
                    self.ledger.fail();
                    self.close_intake();
                    let _ = receipt.send(Err(error.clone()));
                    return Err(error);
                }
                let event = match (bind, command_outcome) {
                    (Some(bind), None) => SettingsReceiveEvent::CommittedBind(bind),
                    (None, Some(SettingsCommandOutcome::Fresh(command))) => {
                        SettingsReceiveEvent::CommittedCommand(command)
                    }
                    (None, Some(SettingsCommandOutcome::Historical(command))) => {
                        SettingsReceiveEvent::HistoricalCommand(command)
                    }
                    (None, None) => SettingsReceiveEvent::ReceiptSettled,
                    (Some(_), Some(_)) => {
                        self.failure = Some(self.ledger.fail());
                        self.closed = true;
                        return Err(SettingsReceiveError::Bookkeeping);
                    }
                };
                let _ = receipt.send(Ok(result));
                drop(frame);
                Ok(Some(event))
            }
            Err(error) => {
                drop(command_outcome);
                if error.quarantines() {
                    self.ledger.fail();
                    self.failure = Some(error.clone());
                    self.closed = true;
                }
                let _ = receipt.send(Err(error.clone()));
                drop(frame);
                Err(error)
            }
        }
    }

    fn access_join_disposition(&self) -> Result<AccessJoinedDisposition, SettingsReceiveError> {
        let Some(Ok((_, output))) = self.retained_join.as_ref() else {
            return Err(self.ledger.fail());
        };
        let Some(access) = output.access.as_ref() else {
            return Err(self.ledger.fail());
        };
        if output.frame.is_none()
            || output.retained_command.is_some()
            || output.command_outcome.is_some()
        {
            return Err(self.ledger.fail());
        }
        if let Some(verified) = access.verified() {
            let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
            let source = verified.authority_source();
            if output.receipt_authority.is_some()
                || !source.matches_context(&context.agent, &context.endpoint, &context.mutation)
            {
                return Err(self.ledger.fail());
            }
            self.check_unjoined_build(
                access.witness().ok_or_else(|| self.ledger.fail())?,
                source.connection(),
                source.verified_peer_keys().device_id,
            )?;
            if let Ok(completion) = &output.completion {
                let id = match &completion.receipt {
                    SettingsTransportReceipt::PrivateCommitted { message_id }
                    | SettingsTransportReceipt::PrivateDuplicate { message_id }
                    | SettingsTransportReceipt::PrivateRejected { message_id, .. } => message_id,
                    SettingsTransportReceipt::Ordinary { .. } => return Err(self.ledger.fail()),
                };
                if source
                    .receipt_metadata()
                    .map_err(SettingsReceiveError::AccessSource)?
                    .message_id
                    != id.to_string()
                {
                    return Err(self.ledger.fail());
                }
            }
        }
        match (&output.completion, access) {
            (
                Ok(WorkCompletion {
                    receipt: SettingsTransportReceipt::PrivateCommitted { .. },
                    bind: None,
                }),
                AccessWorkState::Verified {
                    issue: None,
                    reply: Some(reply),
                    verified,
                },
            ) => {
                let source = verified.authority_source();
                if reply.reservation_id.is_nil()
                    || reply.local_device_id != source.intent().host_id
                    || reply.peer_device_id != source.verified_peer_keys().device_id
                    || reply.created_at >= reply.expires_at
                    || reply.expires_at > source.intent().expires_at
                    || reply.provenance.len() > 2048
                {
                    return Err(self.ledger.fail());
                }
                Ok(AccessJoinedDisposition::Fresh)
            }
            (
                Ok(WorkCompletion {
                    receipt: SettingsTransportReceipt::PrivateDuplicate { .. },
                    bind: None,
                }),
                AccessWorkState::Verified {
                    issue: None,
                    reply: None,
                    ..
                },
            ) => Ok(AccessJoinedDisposition::Settled),
            (
                Ok(WorkCompletion {
                    receipt: SettingsTransportReceipt::PrivateRejected { .. },
                    bind: None,
                }),
                AccessWorkState::Verified {
                    issue: Some(SettingsReceiveError::AccessAdmission(error)),
                    reply: None,
                    ..
                },
            ) if error.is_policy_rejection() => Ok(AccessJoinedDisposition::Settled),
            (
                Ok(WorkCompletion {
                    receipt: SettingsTransportReceipt::PrivateRejected { .. },
                    bind: None,
                }),
                AccessWorkState::Rejected(SettingsReceiveError::AccessSource(
                    super::AccessStatusRejection::Expired
                    | super::AccessStatusRejection::OutsideScope,
                )),
            ) => Ok(AccessJoinedDisposition::Settled),
            (
                Err(SettingsReceiveError::Capacity),
                AccessWorkState::Rejected(SettingsReceiveError::Capacity),
            ) => Ok(AccessJoinedDisposition::Capacity),
            (
                Err(SettingsReceiveError::AccessAdmission(error)),
                AccessWorkState::Verified {
                    issue: Some(SettingsReceiveError::AccessAdmission(original)),
                    reply: None,
                    ..
                },
            ) if Arc::ptr_eq(error, original) && error.is_capacity() => {
                Ok(AccessJoinedDisposition::Capacity)
            }
            (Err(error), _) => Err(error.clone()),
            _ => Err(self.ledger.fail()),
        }
    }

    fn finish_access_join(
        &mut self,
        receipt: oneshot::Sender<Result<SettingsTransportReceipt, SettingsReceiveError>>,
    ) -> Result<Option<SettingsReceiveEvent>, SettingsReceiveError> {
        let disposition = self.access_join_disposition().and_then(|disposition| {
            if matches!(&disposition, AccessJoinedDisposition::Fresh) {
                self.seal_access_join()?;
            }
            Ok(disposition)
        });
        let disposition = match disposition {
            Ok(disposition) => disposition,
            Err(error) => {
                self.ledger.fail();
                self.failure = Some(error.clone());
                self.close_intake();
                let _ = receipt.send(Err(error.clone()));
                return Err(error);
            }
        };
        match (disposition, self.retained_join.take()) {
            (
                AccessJoinedDisposition::Fresh,
                Some(Ok((
                    _,
                    WorkerOutput {
                        completion:
                            Ok(WorkCompletion {
                                receipt: result,
                                bind: None,
                            }),
                        access:
                            Some(AccessWorkState::Verified {
                                issue: None,
                                reply: Some(reply),
                                verified,
                            }),
                        frame: Some(frame),
                        ..
                    },
                ))),
            ) => {
                let owner = OwnedCommittedSettingsAccess::from_joined(AccessReceiptOutput {
                    reply,
                    verified,
                });
                let _ = receipt.send(Ok(result));
                drop(frame);
                Ok(Some(SettingsReceiveEvent::CommittedAccess(owner)))
            }
            (
                AccessJoinedDisposition::Settled,
                Some(Ok((
                    _,
                    WorkerOutput {
                        completion:
                            Ok(WorkCompletion {
                                receipt: result,
                                bind: None,
                            }),
                        access: Some(access),
                        frame: Some(frame),
                        ..
                    },
                ))),
            ) => {
                drop(access);
                let _ = receipt.send(Ok(result));
                drop(frame);
                Ok(Some(SettingsReceiveEvent::ReceiptSettled))
            }
            (
                AccessJoinedDisposition::Capacity,
                Some(Ok((
                    _,
                    WorkerOutput {
                        completion: Err(error),
                        access: Some(access),
                        frame: Some(frame),
                        ..
                    },
                ))),
            ) => {
                drop(access);
                let _ = receipt.send(Err(error.clone()));
                drop(frame);
                Err(error)
            }
            (_, retained) => {
                self.retained_join = retained;
                self.latch_failure();
                let _ = receipt.send(Err(SettingsReceiveError::Bookkeeping));
                Err(SettingsReceiveError::Bookkeeping)
            }
        }
    }

    pub(in crate::devices) fn close_intake(&mut self) {
        self.closed = true;
        self.ledger.closed.store(true, Ordering::Release);
        if let Some(pending) = self.pending.take() {
            let _ = pending.receipt.send(Err(SettingsReceiveError::Closed));
        }
        self.stop_waiting_reply_build(SettingsReceiveError::Closed);
    }

    pub(in crate::devices) fn latch_failure(&mut self) {
        self.failure = Some(self.ledger.fail());
        self.close_intake();
    }

    pub(in crate::devices) fn report(&self) -> SettingsReceiveDrain {
        let mut report = self.ledger.snapshot();
        report.bookkeeping_ok &= self.failure.is_none()
            && self.retained_join.is_none()
            && self.unexpected_join.is_none()
            && self
                .receipts
                .len()
                .checked_add(usize::from(self.reply_build_task().is_some()))
                == Some(self.jobs.len())
            && self.build.is_some() == report.reply_build_active
            && self.queued.healthy(&self.ledger, report.private_requests)
            && self
                .history
                .as_ref()
                .is_none_or(|history| history.healthy());
        if !report.bookkeeping_ok {
            self.ledger.failed.store(true, Ordering::Release);
        }
        report
    }

    pub(in crate::devices) async fn close_and_wait(
        &mut self,
    ) -> Result<SettingsReceiveDrain, SettingsReceiveError> {
        self.close_intake();
        while !self.jobs.is_empty() {
            match self.next().await {
                Ok(event) => drop(event),
                Err(_) if !self.jobs.is_empty() => {}
                Err(_) => break,
            }
        }
        if self.build.is_some() {
            let request = self
                .withdraw_unstarted_build()
                .map_err(|error| self.failure.clone().unwrap_or(error))?;
            drop(request);
        }
        loop {
            let changed = self.ledger.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let report = self.report();
            if !report.bookkeeping_ok || report.terminal_quarantine {
                return Err(self
                    .failure
                    .clone()
                    .unwrap_or(SettingsReceiveError::Bookkeeping));
            }
            if report.complete() {
                self.context = None;
                return Ok(report);
            }
            changed.await;
        }
    }
}

impl Drop for SettingsReceiveWork {
    fn drop(&mut self) {
        self.closed = true;
        self.ledger.closed.store(true, Ordering::Release);
        self.stop_waiting_reply_build(SettingsReceiveError::Closed);
        if !self.report().complete() || self.failure.is_some() {
            self.ledger.terminal.store(true, Ordering::Release);
            self.ledger.changed.notify_waiters();
            let retained: TerminalQuarantine = (
                std::mem::replace(&mut self.jobs, JoinSet::new()),
                self.pending.take(),
                self.build.take(),
                std::mem::take(&mut self.queued),
                std::mem::take(&mut self.receipts),
                self.context.take(),
                self.history.take(),
                Arc::clone(&self.ledger),
                self.failure.take(),
                self.retained_join.take(),
                self.unexpected_join.take(),
            );
            let _ = Box::leak(Box::new(retained));
        }
    }
}

impl GuardScope {
    pub(super) fn active(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.active)
    }
}

impl Drop for GuardScope {
    fn drop(&mut self) {
        if self.guard.is_some() {
            self.active.store(false, Ordering::Release);
            drop(self.guard.take());
        }
    }
}

impl Drop for FrameWitness {
    fn drop(&mut self) {
        match self.ledger.counts.lock() {
            Ok(mut counts) if counts.frame => counts.frame = false,
            _ => {
                self.ledger.failed.store(true, Ordering::Release);
            }
        }
        self.ledger.changed.notify_waiters();
    }
}

impl Drop for PrivateWitness {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.ledger.counts.lock() {
            let peer = counts.peers.get(&self.peer).copied();
            match (
                counts.private.checked_sub(1),
                counts.bytes.checked_sub(self.bytes),
                peer.and_then(|value| value.checked_sub(1)),
            ) {
                (Some(private), Some(bytes), Some(peer)) => {
                    counts.private = private;
                    counts.bytes = bytes;
                    if peer == 0 {
                        counts.peers.remove(&self.peer);
                    } else {
                        counts.peers.insert(self.peer, peer);
                    }
                }
                _ => {
                    self.ledger.failed.store(true, Ordering::Release);
                }
            }
        } else {
            self.ledger.failed.store(true, Ordering::Release);
        }
        self.ledger.changed.notify_waiters();
    }
}

fn run(mut input: WorkerInput) -> WorkerOutput {
    let mut command = CommandWorkState {
        authority: input.receipt_authority.take(),
        command: None,
        outcome: None,
        access: None,
    };
    let completion =
        match std::panic::catch_unwind(AssertUnwindSafe(|| super::execute(&input, &mut command))) {
            Ok(result) => result,
            Err(_) => Err(SettingsReceiveError::Worker),
        };
    WorkerOutput {
        receipt_authority: command.authority.take(),
        completion,
        guard: GuardScope {
            guard: input.guard.guard.take(),
            active: Arc::clone(&input.guard.active),
        },
        context: Arc::clone(&input.context),
        retained_command: command.command.take(),
        command_outcome: command.outcome.take(),
        access: command.access.take(),
        frame: input.frame.take(),
    }
}
