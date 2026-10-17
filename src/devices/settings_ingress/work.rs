mod build;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedMutexGuard};
use tokio::task::{Id, JoinSet};
use uuid::Uuid;

pub(crate) use build::OwnedQueuedSettingsReply;
use build::{ReplyBuildState, ReplyBuildWorkerOutput};

use super::access::{AccessReceiptOutput, AccessWorkState, OwnedCommittedSettingsAccess};
use super::{ReceiveContext, SettingsReceiveError, SettingsTransportReceipt, WorkCompletion};
use crate::crypto::EncryptedEnvelope;
use crate::devices::SettingsConnectionIdentity;
use crate::host::settings::service::{SettingsAccessReplyFamily, SettingsReceiptAuthority};
use crate::protocol::{DeviceId, MessageId, UnixMillis};
use crate::storage::StoredSettingsAccessReply;

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

#[derive(Clone, Copy, Debug)]
pub(crate) struct SettingsOwnerError;

pub(in crate::devices) enum SettingsReceiveWorkEvent {
    Received(Result<SettingsTransportReceipt, SettingsReceiveError>),
    ReplyQueued(MessageId),
}

pub(in crate::devices) struct SettingsReceiveWork {
    ledger: Arc<Ledger>,
    jobs: JoinSet<RegisteredJobOutput>,
    receive_tasks: BTreeSet<Id>,
    pending: Option<PendingInput>,
    build: Option<ReplyBuildState>,
    waiting: VecDeque<OwnedCommittedSettingsAccess>,
    queued: BTreeMap<MessageId, OwnedQueuedSettingsReply>,
    connection: Option<Arc<SettingsConnectionIdentity>>,
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
    waiting: Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>,
    frame: FrameWitness,
}

pub(super) struct WorkerInput {
    receipt_authority: Option<SettingsReceiptAuthority>,
    pub(super) envelope: EncryptedEnvelope,
    pub(super) context: Arc<ReceiveContext>,
    pub(super) connection: Arc<SettingsConnectionIdentity>,
    pub(super) ledger: Arc<Ledger>,
    pub(super) guard: GuardScope,
    frame: Option<FrameWitness>,
}

struct WorkerOutput {
    receipt_authority: Option<SettingsReceiptAuthority>,
    guard: GuardScope,
    context: Arc<ReceiveContext>,
    completion: Result<WorkCompletion, SettingsReceiveError>,
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
    Access(Box<OwnedCommittedSettingsAccess>),
}

struct RegisteredReplyBuild {
    request: SettingsReplyBuildRequest,
    permit: ReplyBuildPermit,
}

type TerminalQuarantine = (
    JoinSet<RegisteredJobOutput>,
    Option<PendingInput>,
    Option<ReplyBuildState>,
    VecDeque<OwnedCommittedSettingsAccess>,
    BTreeMap<MessageId, OwnedQueuedSettingsReply>,
    Option<Arc<ReceiveContext>>,
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
        let witness = request.reply_build_witness();
        self.ledger.check_admission()?;
        if self.ledger.closed.load(Ordering::Acquire) {
            return Err(SettingsReceiveError::Closed);
        }
        let joined = witness.build.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !self.armed
            || !Arc::ptr_eq(&self.ledger, &witness.ledger)
            || !Arc::ptr_eq(&self.association, &joined.association)
            || joined.claim != ReplyBuildClaim::Claimed
            || !self.association.matches(
                ReplyBuildFamily::access(reply),
                source.invocation_identity(),
                ReplyBuildStamp::access(reply),
            )
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
    fn access(&self) -> &OwnedCommittedSettingsAccess {
        let Self::Access(request) = self;
        request
    }

    fn matches_context(
        &self,
        context: &ReceiveContext,
        connection: &Arc<SettingsConnectionIdentity>,
    ) -> bool {
        let source = self.access().verified_request().authority_source();
        Arc::ptr_eq(source.connection(), connection)
            && source.matches_context(&context.agent, &context.endpoint, &context.mutation)
    }

    fn matches_association(&self, association: &ReplyBuildAssociation) -> bool {
        let request = self.access();
        association.matches(
            ReplyBuildFamily::access(request.reply_reservation()),
            request
                .verified_request()
                .authority_source()
                .invocation_identity(),
            ReplyBuildStamp::access(request.reply_reservation()),
        )
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
            receive_tasks: BTreeSet::new(),
            pending: None,
            build: None,
            waiting: VecDeque::new(),
            queued: BTreeMap::new(),
            connection: None,
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
        self.connection = Some(connection);
        self.context = Some(context);
        self.closed = false;
        self.ledger.closed.store(false, Ordering::Release);
        Ok(())
    }

    fn owns_connection(&self, connection: &Arc<SettingsConnectionIdentity>) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|owned| Arc::ptr_eq(owned, connection))
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
            || !self.owns_connection(connection)
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

    fn reserve_reply_build(
        &self,
        connection: &Arc<SettingsConnectionIdentity>,
        request: &mut SettingsReplyBuildRequest,
    ) -> Result<ReplyBuildPermit, SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.failure.is_some() {
            return Err(self.ledger.fail());
        }
        if self.closed || self.ledger.closed.load(Ordering::Acquire) || !connection.is_live() {
            return Err(SettingsReceiveError::Closed);
        }
        let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !self.owns_connection(connection) || !request.matches_context(context, connection) {
            return Err(self.ledger.fail());
        }
        let witness = request.access().reply_build_witness();
        let joined = witness.build.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !Arc::ptr_eq(&witness.ledger, &self.ledger)
            || witness.peer != joined.association.reply.peer_device_id
            || !request.matches_association(&joined.association)
            || joined.claim != ReplyBuildClaim::Unclaimed
        {
            return Err(self.ledger.fail());
        }
        let association = Arc::clone(&joined.association);
        let slot = Arc::new(());
        let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
        self.ledger.check_admission()?;
        if self.build.is_some() || counts.build.is_some() {
            return Err(self.ledger.fail());
        }
        let SettingsReplyBuildRequest::Access(owner) = request;
        let joined = owner
            .reply_build_witness_mut()
            .build
            .as_mut()
            .ok_or_else(|| self.ledger.fail())?;
        if !Arc::ptr_eq(&joined.association, &association)
            || joined.claim != ReplyBuildClaim::Unclaimed
        {
            return Err(self.ledger.fail());
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

    pub(in crate::devices) fn admit(
        &mut self,
        connection: Arc<SettingsConnectionIdentity>,
        envelope: EncryptedEnvelope,
        receipt_authority: SettingsReceiptAuthority,
    ) -> Result<(), SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.closed || self.failure.is_some() || !connection.is_live() {
            return Err(SettingsReceiveError::Closed);
        }
        if envelope.ciphertext.len() > crate::crypto::MAX_CIPHERTEXT_BYTES {
            return Err(SettingsReceiveError::Capacity);
        }
        let Some(context) = self.context.clone() else {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        };
        if !self.owns_connection(&connection) {
            return Err(SettingsReceiveError::Closed);
        }
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
        self.pending = Some(PendingInput {
            receipt_authority,
            envelope,
            context,
            connection,
            waiting,
            frame,
        });
        Ok(())
    }

    pub(in crate::devices) fn busy(&self) -> bool {
        !self.jobs.is_empty()
            || self.pending.is_some()
            || self.build.is_some()
            || (!self.waiting.is_empty() && !self.closed && self.failure.is_none())
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
            if let Err(error) = self.start_next_build() {
                self.failure = Some(error.clone());
                self.close_intake();
                return Err(error);
            }
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
                        return match self.finish_receive_join() {
                            Ok(receipt) => {
                                Ok(Some(SettingsReceiveWorkEvent::Received(Ok(receipt))))
                            }
                            Err(error) if self.failure.is_none() => {
                                Ok(Some(SettingsReceiveWorkEvent::Received(Err(error))))
                            }
                            Err(error) => Err(error),
                        };
                    }
                    RegisteredProgress::ReplyBuild => {
                        if let Some(message_id) = self.finish_reply_build_join()? {
                            return Ok(Some(SettingsReceiveWorkEvent::ReplyQueued(message_id)));
                        }
                    }
                },
            }
        }
    }

    fn start_next_build(&mut self) -> Result<(), SettingsReceiveError> {
        if self.build.is_some() || self.closed || self.failure.is_some() {
            return Ok(());
        }
        let Some(connection) = self.connection.clone() else {
            return Ok(());
        };
        let Some(owner) = self.waiting.pop_front() else {
            return Ok(());
        };
        let mut request = SettingsReplyBuildRequest::Access(Box::new(owner));
        let permit = match self.reserve_reply_build(&connection, &mut request) {
            Ok(permit) => permit,
            Err(SettingsReceiveError::Closed) => {
                drop(request);
                return Ok(());
            }
            Err(error) => {
                drop(request);
                return Err(error);
            }
        };
        let context = self.context.clone().ok_or_else(|| self.ledger.fail())?;
        self.begin_reply_build(context, RegisteredReplyBuild { request, permit });
        Ok(())
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
            waiting,
            frame,
        } = pending;
        drop(waiting);
        let input = WorkerInput {
            receipt_authority: Some(receipt_authority),
            envelope,
            context,
            connection,
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
        if !self.receive_tasks.insert(task.id()) {
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
            Ok((id, RegisteredJobOutput::Receive(output)))
                if self.retained_join.is_none() && self.receive_tasks.remove(&id) =>
            {
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
                if self.retained_join.is_none() && self.receive_tasks.remove(&error.id()) =>
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

    fn finish_receive_join(&mut self) -> Result<SettingsTransportReceipt, SettingsReceiveError> {
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
        match self.retained_join.as_ref() {
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                self.failure = Some(SettingsReceiveError::Worker);
                self.ledger.fail();
                self.close_intake();
                return Err(SettingsReceiveError::Worker);
            }
            None => {
                self.latch_failure();
                return Err(SettingsReceiveError::Bookkeeping);
            }
        }
        if matches!(self.retained_join.as_ref(), Some(Ok((_, output))) if output.access.is_some()) {
            return self.finish_access_join();
        }
        let output = match self.retained_join.take() {
            Some(Ok((_, output))) => output,
            retained => {
                self.retained_join = retained;
                self.latch_failure();
                return Err(SettingsReceiveError::Bookkeeping);
            }
        };
        let WorkerOutput {
            receipt_authority,
            completion,
            access,
            guard,
            context,
            frame,
        } = output;
        drop(access);
        drop(receipt_authority);
        drop(guard);
        drop(context);
        if frame.is_none() {
            self.failure = Some(self.ledger.fail());
            self.closed = true;
            return Err(SettingsReceiveError::Bookkeeping);
        }
        let result = match completion {
            Ok(WorkCompletion { receipt }) => Ok(receipt),
            Err(error) => {
                if error.quarantines() {
                    self.ledger.fail();
                    self.failure = Some(error.clone());
                    self.closed = true;
                }
                Err(error)
            }
        };
        drop(frame);
        result
    }

    fn access_join_disposition(&self) -> Result<AccessJoinedDisposition, SettingsReceiveError> {
        let Some(Ok((_, output))) = self.retained_join.as_ref() else {
            return Err(self.ledger.fail());
        };
        let Some(access) = output.access.as_ref() else {
            return Err(self.ledger.fail());
        };
        if output.frame.is_none() {
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
                }),
                AccessWorkState::Verified {
                    issue: Some(SettingsReceiveError::AccessAdmission(error)),
                    reply: None,
                    ..
                },
            ) if error.rejects_message() => Ok(AccessJoinedDisposition::Settled),
            (
                Ok(WorkCompletion {
                    receipt: SettingsTransportReceipt::PrivateRejected { .. },
                }),
                AccessWorkState::Rejected(SettingsReceiveError::AccessSource(_)),
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

    fn finish_access_join(&mut self) -> Result<SettingsTransportReceipt, SettingsReceiveError> {
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
                return Err(error);
            }
        };
        match (disposition, self.retained_join.take()) {
            (
                AccessJoinedDisposition::Fresh,
                Some(Ok((
                    _,
                    WorkerOutput {
                        completion: Ok(WorkCompletion { receipt }),
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
                self.waiting
                    .push_back(OwnedCommittedSettingsAccess::from_joined(
                        AccessReceiptOutput { reply, verified },
                    ));
                drop(frame);
                Ok(receipt)
            }
            (
                AccessJoinedDisposition::Settled,
                Some(Ok((
                    _,
                    WorkerOutput {
                        completion: Ok(WorkCompletion { receipt }),
                        access: Some(access),
                        frame: Some(frame),
                        ..
                    },
                ))),
            ) => {
                drop(access);
                drop(frame);
                Ok(receipt)
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
                drop(frame);
                Err(error)
            }
            (_, retained) => {
                self.retained_join = retained;
                self.latch_failure();
                Err(SettingsReceiveError::Bookkeeping)
            }
        }
    }

    pub(in crate::devices) fn take_queued_reply(
        &mut self,
        message_id: MessageId,
    ) -> Option<OwnedQueuedSettingsReply> {
        self.queued.remove(&message_id)
    }

    pub(in crate::devices) fn close_intake(&mut self) {
        self.closed = true;
        self.ledger.closed.store(true, Ordering::Release);
        drop(self.pending.take());
        self.waiting.clear();
        self.withdraw_waiting_reply_build();
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
                .receive_tasks
                .len()
                .checked_add(usize::from(self.reply_build_task().is_some()))
                == Some(self.jobs.len())
            && self.build.is_some() == report.reply_build_active
            && self.queued.len() + self.waiting.len() <= report.private_requests;
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
                Ok(Some(SettingsReceiveWorkEvent::ReplyQueued(_)) | None) | Err(_) => {}
                Ok(Some(SettingsReceiveWorkEvent::Received(_))) => {}
            }
        }
        self.waiting.clear();
        self.withdraw_waiting_reply_build();
        let queued = std::mem::take(&mut self.queued);
        if !queued.is_empty() {
            let store = self
                .context
                .as_ref()
                .map(|context| context.agent.store())
                .ok_or_else(|| self.ledger.fail())?;
            let retired = tokio::task::spawn_blocking(move || {
                for (_, reply) in queued {
                    store
                        .retire_private_settings_reply(&reply)
                        .map_err(|_| SettingsReceiveError::Worker)?;
                    reply.retire();
                }
                Ok::<_, SettingsReceiveError>(())
            })
            .await
            .map_err(|_| SettingsReceiveError::Worker)
            .and_then(|result| result);
            if let Err(error) = retired {
                self.ledger.fail();
                self.failure = Some(error.clone());
                return Err(error);
            }
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
                self.connection = None;
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
        self.withdraw_waiting_reply_build();
        if !self.report().complete() || self.failure.is_some() {
            self.ledger.terminal.store(true, Ordering::Release);
            self.ledger.changed.notify_waiters();
            let retained: TerminalQuarantine = (
                std::mem::replace(&mut self.jobs, JoinSet::new()),
                self.pending.take(),
                self.build.take(),
                std::mem::take(&mut self.waiting),
                std::mem::take(&mut self.queued),
                self.context.take(),
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
        access: None,
    };
    let completion =
        match std::panic::catch_unwind(AssertUnwindSafe(|| super::execute(&input, &mut command))) {
            Ok(result) => result,
            Err(_) => Err(SettingsReceiveError::Worker),
        };
    input.guard.active.store(false, Ordering::Release);
    drop(input.guard.guard.take());
    WorkerOutput {
        receipt_authority: command.authority.take(),
        completion,
        guard: GuardScope {
            guard: input.guard.guard.take(),
            active: Arc::clone(&input.guard.active),
        },
        context: Arc::clone(&input.context),
        access: command.access.take(),
        frame: input.frame.take(),
    }
}
