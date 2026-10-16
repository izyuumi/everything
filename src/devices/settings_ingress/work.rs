use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedMutexGuard, oneshot};
use tokio::task::{Id, JoinSet};

use super::{
    BindingHistory, OwnedVerifiedSettingsCommand, ReceiveContext, SettingsCommandOutcome,
    SettingsReceiveError, SettingsReceiveEvent, SettingsTransportReceipt, WorkCompletion,
};
use crate::crypto::EncryptedEnvelope;
use crate::devices::SettingsConnectionIdentity;
use crate::host::settings::service::SettingsReceiptAuthority;
use crate::protocol::DeviceId;

#[derive(Default)]
struct Counts {
    frame: bool,
    jobs: usize,
    private: usize,
    bytes: usize,
    peers: BTreeMap<DeviceId, usize>,
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
    pub(crate) bookkeeping_ok: bool,
    pub(crate) terminal_quarantine: bool,
}

pub(crate) struct PendingSettingsReceipt {
    receiver: oneshot::Receiver<Result<SettingsTransportReceipt, SettingsReceiveError>>,
}

pub(in crate::devices) struct SettingsReceiveWork {
    ledger: Arc<Ledger>,
    jobs: JoinSet<WorkerOutput>,
    receipts: BTreeMap<Id, oneshot::Sender<Result<SettingsTransportReceipt, SettingsReceiveError>>>,
    pending: Option<PendingInput>,
    history: Option<Arc<BindingHistory>>,
    context: Option<Arc<ReceiveContext>>,
    closed: bool,
    failure: Option<SettingsReceiveError>,
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
    frame: Option<FrameWitness>,
}

pub(super) struct CommandWorkState {
    pub(super) authority: Option<SettingsReceiptAuthority>,
    pub(super) command: Option<OwnedVerifiedSettingsCommand>,
    pub(super) outcome: Option<SettingsCommandOutcome>,
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
}

type TerminalQuarantine = (
    JoinSet<WorkerOutput>,
    Option<PendingInput>,
    BTreeMap<Id, oneshot::Sender<Result<SettingsTransportReceipt, SettingsReceiveError>>>,
    Option<Arc<ReceiveContext>>,
    Option<Arc<BindingHistory>>,
    Arc<Ledger>,
    Option<SettingsReceiveError>,
);

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
                bookkeeping_ok: !self.failed.load(Ordering::Acquire),
                terminal_quarantine: self.terminal.load(Ordering::Acquire),
            },
            Err(_) => SettingsReceiveDrain {
                intake_closed: self.closed.load(Ordering::Acquire),
                frame: true,
                private_requests: 32,
                private_bytes: 1_048_576,
                original_jobs: 1,
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
            history: None,
            context: None,
            closed: true,
            failure: None,
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
    ) -> Result<Option<SettingsReceiveEvent>, SettingsReceiveError> {
        if let Some(pending) = self.pending.as_mut() {
            let guard = pending.waiting.as_mut().await;
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
                counts.jobs = counts
                    .jobs
                    .checked_add(1)
                    .ok_or_else(|| self.ledger.fail())?;
            }
            let task = self.jobs.spawn_blocking(move || run(input));
            if self.receipts.insert(task.id(), receipt).is_some() {
                self.failure = Some(self.ledger.fail());
                self.closed = true;
            }
        }
        let Some(joined) = self.jobs.join_next_with_id().await else {
            return Ok(None);
        };
        {
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            counts.jobs = counts
                .jobs
                .checked_sub(1)
                .ok_or_else(|| self.ledger.fail())?;
        }
        let (id, output) = match joined {
            Ok(value) => value,
            Err(error) => {
                if let Some(receipt) = self.receipts.remove(&error.id()) {
                    let _ = receipt.send(Err(SettingsReceiveError::Worker));
                }
                self.failure = Some(SettingsReceiveError::Worker);
                self.ledger.fail();
                self.closed = true;
                return Err(SettingsReceiveError::Worker);
            }
        };
        let Some(receipt) = self.receipts.remove(&id) else {
            self.failure = Some(self.ledger.fail());
            self.closed = true;
            drop(output);
            return Err(SettingsReceiveError::Bookkeeping);
        };
        let WorkerOutput {
            receipt_authority,
            completion,
            retained_command,
            command_outcome,
            guard,
            context,
            frame,
        } = output;
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
                bind,
            }) => {
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

    pub(in crate::devices) fn close_intake(&mut self) {
        self.closed = true;
        self.ledger.closed.store(true, Ordering::Release);
        if let Some(pending) = self.pending.take() {
            let _ = pending.receipt.send(Err(SettingsReceiveError::Closed));
        }
    }

    pub(in crate::devices) fn latch_failure(&mut self) {
        self.failure = Some(self.ledger.fail());
        self.close_intake();
    }

    pub(in crate::devices) fn report(&self) -> SettingsReceiveDrain {
        let mut report = self.ledger.snapshot();
        report.bookkeeping_ok &= self.failure.is_none()
            && self.receipts.len() == self.jobs.len()
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
        if !self.report().complete() || self.failure.is_some() {
            self.ledger.terminal.store(true, Ordering::Release);
            self.ledger.changed.notify_waiters();
            let retained: TerminalQuarantine = (
                std::mem::replace(&mut self.jobs, JoinSet::new()),
                self.pending.take(),
                std::mem::take(&mut self.receipts),
                self.context.take(),
                self.history.take(),
                Arc::clone(&self.ledger),
                self.failure.take(),
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
        frame: input.frame.take(),
    }
}
