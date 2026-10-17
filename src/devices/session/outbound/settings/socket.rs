use std::any::Any;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{OwnedMutexGuard, oneshot};
use tokio::task::{Id, JoinError, JoinHandle};
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::host::settings::service::SettingsQueuedReplyCheckError;
use crate::protocol::DeviceId;
use crate::relay_protocol::RelayProtocolError;
use crate::storage::SettingsSocketCheckFailure;
use crate::transport::remote::{SettingsSocketWriteOutcome, SettingsSocketWriteReturn};

type StopWait = Pin<Box<dyn Future<Output = ()> + Send>>;

enum FinalPhase {
    Unstarted,
    Waiting(GuardWait),
    Running {
        task: Id,
        handle: JoinHandle<FinalOutput>,
    },
    Observed {
        expected: Id,
        joined: Result<FinalOutput, JoinError>,
    },
    Stopped,
}

enum FinalWorkerOutcome {
    Checked(Result<(), Box<SettingsSocketCheckFailure>>),
    GuardDeliveryFailed,
    Stopped(SettingsPreparationStop),
}

struct FinalOutput {
    outcome: FinalWorkerOutcome,
    slot: Arc<()>,
    source: Arc<SettingsSendSource>,
}

enum SocketStartFailure {
    Frame(RelayProtocolError),
    Panicked(Box<dyn Any + Send>),
    Preparation(SettingsPreparationWorkerFailure),
}

pub(in crate::devices::session::outbound) struct SettingsSocketAttempt {
    start_failure: Option<SocketStartFailure>,
    phase: FinalPhase,
    frame: Option<Box<RelayClientMessage>>,
    binary: Option<Message>,
    outcome: Option<SettingsSocketWriteOutcome>,
    stop: Option<SettingsPreparationStop>,
    issue: Option<SettingsSocketObserveFailure>,
    loan_abandoned: bool,
    delivery_failure: Option<oneshot::error::TryRecvError>,
    input: Arc<SettingsPreparationInput>,
}

#[derive(Clone, Copy)]
pub(crate) struct SettingsSocketObserveFailure {
    cause: SocketObserveCause,
}

#[derive(Clone, Copy)]
enum SocketObserveCause {
    Missing,
    Association,
    Stopped(SettingsPreparationStop),
    Worker,
    Write,
}

pub(crate) struct SettingsSocketAdmissionFailure {
    cause: SocketAdmissionCause,
}

enum SocketAdmissionCause {
    Current(SettingsQueuedReplyCheckError),
    Stopped(SettingsPreparationStop),
    TimedOut,
    Association,
}

impl SettingsSocketObserveFailure {
    const MISSING: Self = Self {
        cause: SocketObserveCause::Missing,
    };
    const ASSOCIATION: Self = Self {
        cause: SocketObserveCause::Association,
    };
    const WORKER: Self = Self {
        cause: SocketObserveCause::Worker,
    };
    pub(crate) const WRITE: Self = Self {
        cause: SocketObserveCause::Write,
    };

    fn stopped(cause: SettingsPreparationStop) -> Self {
        Self {
            cause: SocketObserveCause::Stopped(cause),
        }
    }
}

impl SettingsSocketAdmissionFailure {
    const ASSOCIATION: Self = Self {
        cause: SocketAdmissionCause::Association,
    };
    const TIMED_OUT: Self = Self {
        cause: SocketAdmissionCause::TimedOut,
    };

    fn current(cause: SettingsQueuedReplyCheckError) -> Self {
        Self {
            cause: SocketAdmissionCause::Current(cause),
        }
    }

    fn stopped(cause: SettingsPreparationStop) -> Self {
        Self {
            cause: SocketAdmissionCause::Stopped(cause),
        }
    }
}

enum LoanPhase {
    Unchecked,
    Admitted,
    Released,
}

pub(crate) struct SettingsSocketSendLoan<'window> {
    guard: Option<OwnedMutexGuard<()>>,
    cancellation: StopWait,
    connection_cancellation: StopWait,
    deadline: Pin<Box<tokio::time::Sleep>>,
    write_deadline: Instant,
    write_budget: Pin<Box<tokio::time::Sleep>>,
    phase: LoanPhase,
    attempt: &'window mut SettingsSocketAttempt,
    endpoint: &'window DeviceEndpoint,
    socket: &'window Arc<RelaySocketIdentity>,
    failure: &'window mut Option<OrdinaryFailureBundle>,
}

struct FinalDriver<'window> {
    receiver: Option<oneshot::Receiver<OwnedMutexGuard<()>>>,
    completed: bool,
    attempt: &'window mut SettingsSocketAttempt,
    window: &'window Arc<()>,
    slot: &'window Arc<()>,
    association: &'window Option<Arc<SettingsSendLoanAssociation>>,
    endpoint: &'window DeviceEndpoint,
    socket: &'window Arc<RelaySocketIdentity>,
    failure: &'window mut Option<OrdinaryFailureBundle>,
}

impl SettingsSocketAttempt {
    pub(super) fn source(&self) -> &Arc<SettingsSendSource> {
        &self.input.source
    }

    pub(in crate::devices::session::outbound) fn is_running(&self) -> bool {
        matches!(self.phase, FinalPhase::Running { .. })
    }

    pub(super) fn stop(&mut self, cause: SettingsPreparationStop) {
        if self.stop.is_none() {
            self.stop = Some(cause);
        }
        if matches!(self.phase, FinalPhase::Waiting(_)) {
            let waiting = std::mem::replace(&mut self.phase, FinalPhase::Stopped);
            drop(waiting);
        }
        drop(self.frame.take());
        drop(self.binary.take());
    }

    fn joined_matches(&self) -> bool {
        match &self.phase {
            FinalPhase::Observed { expected, joined } => match joined {
                Ok(output) => {
                    Arc::ptr_eq(&output.slot, &self.input.slot)
                        && Arc::ptr_eq(&output.source, &self.input.source)
                }
                Err(error) => error.id() == *expected,
            },
            _ => false,
        }
    }

    fn joined_success(&self) -> bool {
        matches!(
            &self.phase,
            FinalPhase::Observed {
                joined: Ok(FinalOutput {
                    outcome: FinalWorkerOutcome::Checked(Ok(())),
                    ..
                }),
                ..
            }
        )
    }
}

impl RelaySendWindow {
    pub(in crate::devices) async fn finalize_and_send_settings(
        &mut self,
        id: MessageId,
        connection: &mut RelayConnection,
    ) -> Result<(), SettingsSocketObserveFailure> {
        if self.finished || self.failure.is_some() || self.endpoint.outbound_is_quarantined() {
            return Err(SettingsSocketObserveFailure::stopped(
                SettingsPreparationStop::Closed,
            ));
        }
        let Some(RelaySendEntry::Settings(cell)) = self.entries.get_mut(&id) else {
            return Err(SettingsSocketObserveFailure::MISSING);
        };
        if !cell.matches_window(&self.identity) {
            return Err(SettingsSocketObserveFailure::ASSOCIATION);
        }
        if !matches!(cell.state, SettingsCustodyState::Prepared { .. }) {
            return Err(SettingsSocketObserveFailure::ASSOCIATION);
        }
        let state = std::mem::replace(&mut cell.state, SettingsCustodyState::Provisional);
        let SettingsCustodyState::Prepared { frame, input } = state else {
            cell.state = state;
            return Err(SettingsSocketObserveFailure::ASSOCIATION);
        };
        let attempt = Box::new(SettingsSocketAttempt {
            start_failure: None,
            phase: FinalPhase::Unstarted,
            frame: Some(frame),
            binary: None,
            outcome: None,
            stop: None,
            issue: None,
            loan_abandoned: false,
            delivery_failure: None,
            input,
        });
        cell.state = SettingsCustodyState::Socket(attempt);
        let SettingsCustodyState::Socket(attempt) = &mut cell.state else {
            return Err(SettingsSocketObserveFailure::ASSOCIATION);
        };
        let mut driver = FinalDriver {
            receiver: None,
            completed: false,
            attempt,
            window: &self.identity,
            slot: &cell.slot.identity,
            association: &cell.association,
            endpoint: &self.endpoint,
            socket: &self.socket,
            failure: &mut self.failure,
        };
        let result = driver.run(connection).await;
        driver.completed = true;
        result
    }

    pub(in crate::devices::session::outbound) async fn join_settings_socket_on_close(
        &mut self,
        id: MessageId,
    ) -> Result<(), SettingsSocketObserveFailure> {
        let Some(RelaySendEntry::Settings(cell)) = self.entries.get_mut(&id) else {
            return Err(SettingsSocketObserveFailure::MISSING);
        };
        let SettingsCustodyState::Socket(attempt) = &mut cell.state else {
            return Err(SettingsSocketObserveFailure::ASSOCIATION);
        };
        attempt.stop(SettingsPreparationStop::Closed);
        if let FinalPhase::Running { task, handle } = &mut attempt.phase {
            let expected = *task;
            let joined = handle.await;
            attempt.phase = FinalPhase::Observed { expected, joined };
        }
        if !attempt.joined_matches() {
            attempt.issue = Some(SettingsSocketObserveFailure::ASSOCIATION);
            return Err(SettingsSocketObserveFailure::ASSOCIATION);
        }
        Err(SettingsSocketObserveFailure::stopped(
            SettingsPreparationStop::Closed,
        ))
    }
}

impl FinalDriver<'_> {
    fn association_matches(&self) -> bool {
        let input = &self.attempt.input;
        Arc::ptr_eq(&input.window, self.window)
            && Arc::ptr_eq(&input.slot, self.slot)
            && input.ticket.matches_slot(self.window, self.slot)
            && input.ticket.shares_socket(self.socket)
            && input.ticket.matches_source(&input.source)
            && self
                .association
                .as_ref()
                .is_some_and(|association| input.source.matches_association(association))
    }

    fn stopped(&self) -> Option<SettingsPreparationStop> {
        self.attempt
            .stop
            .or_else(|| {
                (self.failure.is_some() || self.endpoint.outbound_is_quarantined())
                    .then_some(SettingsPreparationStop::Closed)
            })
            .or_else(|| self.attempt.input.stop())
            .or_else(|| {
                (!self.socket.is_live()).then_some(SettingsPreparationStop::ConnectionClosed)
            })
    }

    async fn run(
        &mut self,
        connection: &mut RelayConnection,
    ) -> Result<(), SettingsSocketObserveFailure> {
        use SettingsSocketObserveFailure as Failure;

        if !self.association_matches() {
            return self.fail(Failure::ASSOCIATION);
        }
        let Some(frame) = self.attempt.frame.as_ref() else {
            return self.fail(Failure::ASSOCIATION);
        };
        match catch_unwind(AssertUnwindSafe(|| frame.to_bytes())) {
            Ok(Ok(bytes)) => self.attempt.binary = Some(Message::Binary(bytes.into())),
            Ok(Err(cause)) => self.attempt.start_failure = Some(SocketStartFailure::Frame(cause)),
            Err(payload) => {
                self.attempt.start_failure = Some(SocketStartFailure::Panicked(payload))
            }
        }
        drop(self.attempt.frame.take());
        if self.attempt.start_failure.is_some() {
            return self.fail(Failure::WORKER);
        }
        let mutation = match self.attempt.input.source.preparation_wait_inputs() {
            Ok((mutation, deadline, _)) if deadline == self.attempt.input.deadline => mutation,
            Ok(_) => return self.fail(Failure::ASSOCIATION),
            Err(cause) => {
                self.attempt.start_failure = Some(SocketStartFailure::Preparation(cause));
                return self.fail(Failure::WORKER);
            }
        };
        self.attempt.phase = FinalPhase::Waiting(Box::pin(mutation.lock_owned()));
        loop {
            if let Some(cause) = self.stopped() {
                self.attempt.stop(cause);
            }
            match &mut self.attempt.phase {
                FinalPhase::Waiting(waiting) => {
                    let input = &self.attempt.input;
                    let result = tokio::select! {
                        biased;
                        _ = input.cancellation.cancelled() => WaitingResult::Stop(SettingsPreparationStop::Cancelled),
                        _ = input.connection_cancellation.cancelled() => WaitingResult::Stop(SettingsPreparationStop::ConnectionClosed),
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(input.deadline)) => WaitingResult::Stop(SettingsPreparationStop::Expired),
                        guard = waiting.as_mut() => WaitingResult::Guard(guard),
                    };
                    match result {
                        WaitingResult::Stop(cause) => self.attempt.stop(cause),
                        WaitingResult::Guard(guard) => {
                            if let Some(cause) = self.stopped() {
                                drop(guard);
                                self.attempt.stop(cause);
                                continue;
                            }
                            let (sender, receiver) = oneshot::channel();
                            self.receiver = Some(receiver);
                            let worker = Arc::clone(&self.attempt.input);
                            let handle = tokio::task::spawn_blocking(move || {
                                run_final_check(worker, guard, sender)
                            });
                            let task = handle.id();
                            self.attempt.phase = FinalPhase::Running { task, handle };
                        }
                    }
                }
                FinalPhase::Running { task, handle } => {
                    let expected = *task;
                    let input = &self.attempt.input;
                    let joined = if self.attempt.stop.is_some() {
                        Some(handle.await)
                    } else {
                        tokio::select! {
                            biased;
                            _ = input.cancellation.cancelled() => {
                                self.attempt.stop = Some(SettingsPreparationStop::Cancelled);
                                None
                            }
                            _ = input.connection_cancellation.cancelled() => {
                                self.attempt.stop = Some(SettingsPreparationStop::ConnectionClosed);
                                None
                            }
                            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(input.deadline)) => {
                                self.attempt.stop = Some(SettingsPreparationStop::Expired);
                                None
                            }
                            joined = handle => Some(joined),
                        }
                    };
                    if let Some(joined) = joined {
                        self.attempt.phase = FinalPhase::Observed { expected, joined };
                    }
                }
                FinalPhase::Observed { .. } => break,
                FinalPhase::Stopped => {
                    return self.fail(Failure::stopped(
                        self.attempt.stop.unwrap_or(SettingsPreparationStop::Closed),
                    ));
                }
                FinalPhase::Unstarted => return self.fail(Failure::ASSOCIATION),
            }
        }
        if !self.association_matches() || !self.attempt.joined_matches() {
            return self.fail(Failure::ASSOCIATION);
        }
        if let Some(cause) = self.stopped() {
            return self.fail(Failure::stopped(cause));
        }
        if !self.attempt.joined_success() {
            return self.fail(Failure::WORKER);
        }
        let Some(mut receiver) = self.receiver.take() else {
            return self.fail(Failure::ASSOCIATION);
        };
        let received = receiver.try_recv();
        drop(receiver);
        let guard = match received {
            Ok(guard) => guard,
            Err(cause) => {
                self.attempt.delivery_failure = Some(cause);
                return self.fail(Failure::WORKER);
            }
        };
        if let Some(cause) = self.stopped() {
            drop(guard);
            return self.fail(Failure::stopped(cause));
        }
        let input = &self.attempt.input;
        let write_deadline = Instant::now() + std::time::Duration::from_secs(5);
        let write_budget = Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
            write_deadline,
        )));
        let loan = SettingsSocketSendLoan {
            guard: Some(guard),
            cancellation: Box::pin(input.cancellation.clone().cancelled_owned()),
            connection_cancellation: Box::pin(
                input.connection_cancellation.clone().cancelled_owned(),
            ),
            deadline: Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                input.deadline,
            ))),
            write_deadline,
            write_budget,
            phase: LoanPhase::Unchecked,
            attempt: self.attempt,
            endpoint: self.endpoint,
            socket: self.socket,
            failure: self.failure,
        };
        let returned: SettingsSocketWriteReturn<'_> = connection.send_settings(loan).await;
        returned.register();
        match self.attempt.outcome.as_ref() {
            Some(outcome) => outcome.observation(),
            None => self.fail(Failure::ASSOCIATION),
        }
    }

    fn fail<T>(
        &mut self,
        cause: SettingsSocketObserveFailure,
    ) -> Result<T, SettingsSocketObserveFailure> {
        drop(self.receiver.take());
        self.attempt.issue = Some(cause);
        self.attempt.stop(SettingsPreparationStop::Closed);
        latch_socket_failure(self.failure, self.endpoint, &self.attempt.input.source);
        Err(cause)
    }
}

impl Drop for FinalDriver<'_> {
    fn drop(&mut self) {
        drop(self.receiver.take());
        if !self.completed {
            self.attempt.stop(SettingsPreparationStop::Closed);
            self.attempt.issue = Some(SettingsSocketObserveFailure::stopped(
                SettingsPreparationStop::Closed,
            ));
            latch_socket_failure(self.failure, self.endpoint, &self.attempt.input.source);
        }
    }
}

fn run_final_check(
    input: Arc<SettingsPreparationInput>,
    guard: OwnedMutexGuard<()>,
    sender: oneshot::Sender<OwnedMutexGuard<()>>,
) -> FinalOutput {
    let outcome = match input.stop() {
        Some(cause) => {
            drop(guard);
            FinalWorkerOutcome::Stopped(cause)
        }
        None => match input.source.recheck_socket_sql(&guard) {
            Ok(()) => match sender.send(guard) {
                Ok(()) => FinalWorkerOutcome::Checked(Ok(())),
                Err(guard) => {
                    drop(guard);
                    FinalWorkerOutcome::GuardDeliveryFailed
                }
            },
            Err(cause) => {
                drop(guard);
                FinalWorkerOutcome::Checked(Err(cause))
            }
        },
    };
    let slot = Arc::clone(&input.slot);
    let source = Arc::clone(&input.source);
    drop(input);
    FinalOutput {
        outcome,
        slot,
        source,
    }
}

impl SettingsSocketSendLoan<'_> {
    pub(crate) fn matches_socket(
        &self,
        socket: &Arc<RelaySocketIdentity>,
        local_device: DeviceId,
    ) -> bool {
        Arc::ptr_eq(self.socket, socket)
            && self.endpoint.public_keys().device_id == local_device
            && self.attempt.input.ticket.shares_socket(socket)
    }

    pub(crate) fn poll_stop(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<SettingsSocketAdmissionFailure> {
        if self.write_budget.as_mut().poll(cx).is_ready() {
            return Poll::Ready(SettingsSocketAdmissionFailure::TIMED_OUT);
        }
        let stop = if self.cancellation.as_mut().poll(cx).is_ready() {
            Some(SettingsPreparationStop::Cancelled)
        } else if self.connection_cancellation.as_mut().poll(cx).is_ready()
            || !self.socket.is_live()
        {
            Some(SettingsPreparationStop::ConnectionClosed)
        } else if self.deadline.as_mut().poll(cx).is_ready() {
            Some(SettingsPreparationStop::Expired)
        } else if self.failure.is_some() || self.endpoint.outbound_is_quarantined() {
            Some(SettingsPreparationStop::Closed)
        } else {
            self.attempt.stop.or_else(|| self.attempt.input.stop())
        };
        match stop {
            Some(cause) => Poll::Ready(SettingsSocketAdmissionFailure::stopped(cause)),
            None => Poll::Pending,
        }
    }

    pub(crate) fn admit_current_then_release_guard(
        &mut self,
        socket: &Arc<RelaySocketIdentity>,
        local_device: DeviceId,
    ) -> Result<(), SettingsSocketAdmissionFailure> {
        let checked = (|| {
            if !matches!(self.phase, LoanPhase::Unchecked)
                || !self.matches_socket(socket, local_device)
                || !self.attempt.joined_matches()
                || !self.attempt.joined_success()
                || self.attempt.outcome.is_some()
                || self.attempt.binary.is_none()
                || self.attempt.issue.is_some()
            {
                return Err(SettingsSocketAdmissionFailure::ASSOCIATION);
            }
            if let Some(cause) = self.attempt.stop.or_else(|| self.attempt.input.stop()) {
                return Err(SettingsSocketAdmissionFailure::stopped(cause));
            }
            if self.failure.is_some() || self.endpoint.outbound_is_quarantined() {
                return Err(SettingsSocketAdmissionFailure::stopped(
                    SettingsPreparationStop::Closed,
                ));
            }
            let guard = self
                .guard
                .as_ref()
                .ok_or(SettingsSocketAdmissionFailure::ASSOCIATION)?;
            self.attempt
                .input
                .source
                .recheck_socket_lifetime(guard)
                .map_err(SettingsSocketAdmissionFailure::current)?;
            if let Some(cause) = self.attempt.stop.or_else(|| self.attempt.input.stop()) {
                return Err(SettingsSocketAdmissionFailure::stopped(cause));
            }
            if Instant::now() >= self.write_deadline {
                return Err(SettingsSocketAdmissionFailure::TIMED_OUT);
            }
            if !socket.is_live() {
                return Err(SettingsSocketAdmissionFailure::stopped(
                    SettingsPreparationStop::ConnectionClosed,
                ));
            }
            if self.failure.is_some() || self.endpoint.outbound_is_quarantined() {
                return Err(SettingsSocketAdmissionFailure::stopped(
                    SettingsPreparationStop::Closed,
                ));
            }
            Ok(())
        })();
        self.phase = if checked.is_ok() {
            LoanPhase::Admitted
        } else {
            LoanPhase::Released
        };
        drop(self.guard.take());
        checked
    }

    pub(crate) fn take_preencoded_for_start_send(
        &mut self,
    ) -> Result<Message, SettingsSocketAdmissionFailure> {
        if !matches!(self.phase, LoanPhase::Admitted) {
            return Err(SettingsSocketAdmissionFailure::ASSOCIATION);
        }
        self.attempt
            .binary
            .take()
            .ok_or(SettingsSocketAdmissionFailure::ASSOCIATION)
    }

    pub(crate) fn release_guard_on_drop(&mut self) {
        drop(self.guard.take());
        self.phase = LoanPhase::Released;
    }

    pub(crate) fn record_actual_write(mut self, outcome: SettingsSocketWriteOutcome) {
        self.attempt.outcome = Some(outcome);
        self.release_guard_on_drop();
        drop(self.attempt.binary.take());
        if let Some(outcome) = self.attempt.outcome.as_ref() {
            outcome.fence_abandoned_return();
        }
        if self
            .attempt
            .outcome
            .as_ref()
            .is_some_and(|outcome| outcome.observation().is_err())
        {
            latch_socket_failure(self.failure, self.endpoint, &self.attempt.input.source);
        }
    }
}

impl Drop for SettingsSocketSendLoan<'_> {
    fn drop(&mut self) {
        self.release_guard_on_drop();
        drop(self.attempt.binary.take());
        if self.attempt.outcome.is_none() {
            self.attempt.loan_abandoned = true;
            self.attempt.stop(SettingsPreparationStop::Closed);
            latch_socket_failure(self.failure, self.endpoint, &self.attempt.input.source);
        }
    }
}

fn latch_socket_failure(
    failure: &mut Option<OrdinaryFailureBundle>,
    endpoint: &DeviceEndpoint,
    source: &SettingsSendSource,
) {
    if failure.is_none() {
        *failure = Some(OrdinaryFailureBundle {
            _primary: None,
            _cleanup: OrdinaryIssue::PrivateCustodyOutstanding,
        });
    }
    source.quarantine_socket_owner();
    endpoint.quarantine_outbound();
}

impl fmt::Debug for SettingsSocketAdmissionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsSocketAdmissionFailure([retained cause])")
    }
}

impl fmt::Debug for SettingsSocketObserveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsSocketObserveFailure([retained outcome])")
    }
}

impl fmt::Display for SettingsSocketObserveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Settings socket attempt did not complete cleanly")
    }
}

impl std::error::Error for SettingsSocketObserveFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            SocketObserveCause::Stopped(cause) => Some(cause),
            _ => None,
        }
    }
}

impl fmt::Display for SettingsSocketAdmissionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Settings socket admission failed")
    }
}

impl std::error::Error for SettingsSocketAdmissionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            SocketAdmissionCause::Current(cause) => Some(cause),
            SocketAdmissionCause::Stopped(cause) => Some(cause),
            _ => None,
        }
    }
}

pub(super) struct ReturnedSocketParts {
    pub(super) input: Arc<SettingsPreparationInput>,
    pub(super) evidence: ReturnedSocketEvidence,
}

pub(super) struct ReturnedSocketEvidence {
    _start_failure: Option<SocketStartFailure>,
    _final_phase: ReturnedFinalPhase,
    _outcome: Option<SettingsSocketWriteOutcome>,
    _stop: Option<SettingsPreparationStop>,
    _issue: Option<SettingsSocketObserveFailure>,
    _loan_abandoned: bool,
    _delivery_failure: Option<oneshot::error::TryRecvError>,
}

enum ReturnedFinalPhase {
    NotSpawned,
    Joined {
        _expected: Id,
        _outcome: Result<ReturnedFinalOutput, JoinError>,
    },
}

struct ReturnedFinalOutput {
    _outcome: FinalWorkerOutcome,
    _slot: Arc<()>,
}

impl SettingsSocketAttempt {
    pub(super) fn return_identity_matches(
        &self,
        window: &Arc<()>,
        slot: &Arc<()>,
        socket: &Arc<RelaySocketIdentity>,
    ) -> bool {
        Arc::ptr_eq(&self.input.window, window)
            && Arc::ptr_eq(&self.input.slot, slot)
            && self.input.ticket.matches_slot(window, slot)
            && self.input.ticket.shares_socket(socket)
            && self.input.ticket.matches_source(&self.input.source)
    }

    pub(super) fn return_ready(&self) -> bool {
        self.frame.is_none()
            && self.binary.is_none()
            && match &self.phase {
                FinalPhase::Unstarted | FinalPhase::Stopped => {
                    self.outcome.is_none() && self.stop.is_some()
                }
                FinalPhase::Observed { .. } => self.joined_matches(),
                FinalPhase::Waiting(_) | FinalPhase::Running { .. } => false,
            }
    }

    pub(super) fn return_parts(self: Box<Self>) -> Result<ReturnedSocketParts, Box<Self>> {
        if !self.return_ready() {
            return Err(self);
        }
        let Self {
            start_failure,
            phase,
            frame,
            binary,
            outcome,
            stop,
            issue,
            loan_abandoned,
            delivery_failure,
            input,
        } = *self;
        drop(frame);
        drop(binary);
        let final_phase = match phase {
            FinalPhase::Observed { expected, joined } => ReturnedFinalPhase::Joined {
                _expected: expected,
                _outcome: joined.map(|output| {
                    let FinalOutput {
                        outcome,
                        slot,
                        source,
                    } = output;
                    drop(source);
                    ReturnedFinalOutput {
                        _outcome: outcome,
                        _slot: slot,
                    }
                }),
            },
            _ => ReturnedFinalPhase::NotSpawned,
        };
        Ok(ReturnedSocketParts {
            input,
            evidence: ReturnedSocketEvidence {
                _start_failure: start_failure,
                _final_phase: final_phase,
                _outcome: outcome,
                _stop: stop,
                _issue: issue,
                _loan_abandoned: loan_abandoned,
                _delivery_failure: delivery_failure,
            },
        })
    }
}
