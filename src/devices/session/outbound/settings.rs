use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use thiserror::Error;
use tokio::sync::OwnedMutexGuard;

use super::*;
use crate::host::settings::agent::PairedHandoffReadError;
use crate::storage::SettingsCiphertextLoadFailure;

pub(super) type GuardWait = Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(in crate::devices) enum SettingsPreparationStop {
    #[error("Settings preparation was cancelled")]
    Cancelled,
    #[error("Settings preparation connection closed")]
    ConnectionClosed,
    #[error("Settings preparation deadline expired")]
    Expired,
    #[error("Settings preparation intake closed")]
    Closed,
}

#[derive(Error)]
pub(in crate::devices) enum SettingsPreparationWorkerFailure {
    #[error("Settings ciphertext source is unavailable")]
    Owner(#[source] PairedHandoffReadError),
    #[error("Settings ciphertext loading failed")]
    Load(#[source] Box<SettingsCiphertextLoadFailure>),
    #[error("Settings ciphertext preparation stopped")]
    Stopped(#[source] SettingsPreparationStop),
    #[error("Settings ciphertext preparation association does not match")]
    Association,
}

pub(in crate::devices) struct SettingsPreparationStartFailure {
    cause: SettingsPreparationWorkerFailure,
    _ticket: PendingSettingsSend,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::devices) enum SettingsPreparationObserveFailure {
    Missing,
    Association,
    Worker,
    Stopped(SettingsPreparationStop),
}

pub(super) struct SettingsPreparationInput {
    deadline: Instant,
    cancellation: CancellationToken,
    connection_cancellation: CancellationToken,
    window: Arc<()>,
    slot: Arc<()>,
    ticket: PendingSettingsSend,
    source: Arc<SettingsSendSource>,
}

pub(super) struct SettingsPreparationOutput {
    outcome: Result<Box<RelayClientMessage>, SettingsPreparationWorkerFailure>,
    slot: Arc<()>,
    source: Arc<SettingsSendSource>,
}

enum WaitingResult {
    Guard(OwnedMutexGuard<()>),
    Stop(SettingsPreparationStop),
}

impl SettingsPreparationInput {
    fn stop(&self) -> Option<SettingsPreparationStop> {
        if self.cancellation.is_cancelled() {
            Some(SettingsPreparationStop::Cancelled)
        } else if self.connection_cancellation.is_cancelled() {
            Some(SettingsPreparationStop::ConnectionClosed)
        } else if Instant::now() >= self.deadline {
            Some(SettingsPreparationStop::Expired)
        } else {
            None
        }
    }
}

impl SettingsCustodyState {
    pub(super) fn source(&self) -> Option<&Arc<SettingsSendSource>> {
        match self {
            Self::Provisional => None,
            Self::LoanedUnstarted(source) => Some(source),
            Self::Waiting { input, .. }
            | Self::Preparing { input, .. }
            | Self::Observed { input, .. }
            | Self::Prepared { input, .. }
            | Self::Stopped { input, .. } => Some(&input.source),
        }
    }

    pub(super) fn stop_preparation(&mut self, cause: SettingsPreparationStop) {
        match self {
            Self::Waiting { .. } => {
                let state = std::mem::replace(self, Self::Provisional);
                match state {
                    Self::Waiting { waiting, input } => {
                        drop(waiting);
                        *self = Self::Stopped { cause, input };
                    }
                    state => *self = state,
                }
            }
            Self::Prepared { .. } => {
                let state = std::mem::replace(self, Self::Provisional);
                match state {
                    Self::Prepared { frame, input } => {
                        drop(frame);
                        *self = Self::Stopped { cause, input };
                    }
                    state => *self = state,
                }
            }
            Self::Preparing { stop, .. } | Self::Observed { stop, .. } => {
                if stop.is_none() {
                    *stop = Some(cause);
                }
            }
            _ => {}
        }
    }
}

impl RelaySendWindow {
    pub(in crate::devices) fn try_start_settings_preparation(
        &mut self,
        ticket: PendingSettingsSend,
        cancellation: CancellationToken,
    ) -> Result<MessageId, SettingsPreparationStartFailure> {
        let admitted = (|| {
            if self.finished || self.failure.is_some() || self.endpoint.outbound_is_quarantined() {
                return Err(SettingsPreparationWorkerFailure::Stopped(
                    SettingsPreparationStop::Closed,
                ));
            }
            if !self.socket.is_live() {
                return Err(SettingsPreparationWorkerFailure::Stopped(
                    SettingsPreparationStop::ConnectionClosed,
                ));
            }
            let Some(RelaySendEntry::Settings(cell)) = self.entries.get(&ticket.message_id())
            else {
                return Err(SettingsPreparationWorkerFailure::Association);
            };
            let SettingsCustodyState::LoanedUnstarted(source) = &cell.state else {
                return Err(SettingsPreparationWorkerFailure::Association);
            };
            if !cell.matches_window(&self.identity)
                || !ticket.matches_slot(&self.identity, &cell.slot.identity)
                || !ticket.shares_socket(&self.socket)
                || !ticket.matches_source(source)
            {
                return Err(SettingsPreparationWorkerFailure::Association);
            }
            let (mutation, deadline, connection_cancellation) = source.preparation_wait_inputs()?;
            if cancellation.is_cancelled() {
                return Err(SettingsPreparationWorkerFailure::Stopped(
                    SettingsPreparationStop::Cancelled,
                ));
            }
            if connection_cancellation.is_cancelled() {
                return Err(SettingsPreparationWorkerFailure::Stopped(
                    SettingsPreparationStop::ConnectionClosed,
                ));
            }
            if Instant::now() >= deadline {
                return Err(SettingsPreparationWorkerFailure::Stopped(
                    SettingsPreparationStop::Expired,
                ));
            }
            Ok((mutation, deadline, connection_cancellation))
        })();
        let (mutation, deadline, connection_cancellation) = match admitted {
            Ok(admitted) => admitted,
            Err(cause) => {
                return Err(SettingsPreparationStartFailure {
                    cause,
                    _ticket: ticket,
                });
            }
        };
        let id = ticket.message_id();
        let Some(RelaySendEntry::Settings(cell)) = self.entries.get_mut(&id) else {
            return Err(SettingsPreparationStartFailure {
                cause: SettingsPreparationWorkerFailure::Association,
                _ticket: ticket,
            });
        };
        let SettingsCustodyState::LoanedUnstarted(source) = &cell.state else {
            return Err(SettingsPreparationStartFailure {
                cause: SettingsPreparationWorkerFailure::Association,
                _ticket: ticket,
            });
        };
        let input = Arc::new(SettingsPreparationInput {
            deadline,
            cancellation,
            connection_cancellation,
            window: Arc::clone(&self.identity),
            slot: Arc::clone(&cell.slot.identity),
            ticket,
            source: Arc::clone(source),
        });
        cell.state = SettingsCustodyState::Waiting {
            waiting: Box::pin(mutation.lock_owned()),
            input,
        };
        Ok(id)
    }

    pub(in crate::devices) async fn observe_settings_preparation(
        &mut self,
        id: MessageId,
    ) -> Result<(), SettingsPreparationObserveFailure> {
        loop {
            let socket_live = self.socket.is_live();
            let closing =
                self.finished || self.failure.is_some() || self.endpoint.outbound_is_quarantined();
            let Some(RelaySendEntry::Settings(cell)) = self.entries.get_mut(&id) else {
                return Err(SettingsPreparationObserveFailure::Missing);
            };
            match &mut cell.state {
                SettingsCustodyState::Waiting { waiting, input } => {
                    let stopped = closing
                        .then_some(SettingsPreparationStop::Closed)
                        .or_else(|| input.stop())
                        .or_else(|| {
                            (!socket_live).then_some(SettingsPreparationStop::ConnectionClosed)
                        });
                    let result = if let Some(cause) = stopped {
                        WaitingResult::Stop(cause)
                    } else {
                        tokio::select! {
                            biased;
                            _ = input.cancellation.cancelled() => WaitingResult::Stop(SettingsPreparationStop::Cancelled),
                            _ = input.connection_cancellation.cancelled() => WaitingResult::Stop(SettingsPreparationStop::ConnectionClosed),
                            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(input.deadline)) => WaitingResult::Stop(SettingsPreparationStop::Expired),
                            guard = waiting.as_mut() => WaitingResult::Guard(guard),
                        }
                    };
                    let guard = match result {
                        WaitingResult::Stop(cause) => {
                            cell.state.stop_preparation(cause);
                            return Err(SettingsPreparationObserveFailure::Stopped(cause));
                        }
                        WaitingResult::Guard(guard) => guard,
                    };
                    let stopped = input.stop().or_else(|| {
                        (!self.socket.is_live())
                            .then_some(SettingsPreparationStop::ConnectionClosed)
                    });
                    if let Some(cause) = stopped {
                        drop(guard);
                        cell.state.stop_preparation(cause);
                        return Err(SettingsPreparationObserveFailure::Stopped(cause));
                    }
                    let retained = Arc::clone(input);
                    let worker = Arc::clone(input);
                    let handle =
                        tokio::task::spawn_blocking(move || run_preparation(worker, guard));
                    let task = handle.id();
                    cell.state = SettingsCustodyState::Preparing {
                        task,
                        handle,
                        input: retained,
                        stop: None,
                    };
                }
                SettingsCustodyState::Preparing {
                    task,
                    handle,
                    input,
                    stop,
                } => {
                    if stop.is_none() {
                        *stop = closing
                            .then_some(SettingsPreparationStop::Closed)
                            .or_else(|| input.stop())
                            .or_else(|| {
                                (!socket_live).then_some(SettingsPreparationStop::ConnectionClosed)
                            });
                    }
                    let joined = if stop.is_some() {
                        Some(handle.await)
                    } else {
                        tokio::select! {
                            biased;
                            _ = input.cancellation.cancelled() => {
                                *stop = Some(SettingsPreparationStop::Cancelled);
                                None
                            }
                            _ = input.connection_cancellation.cancelled() => {
                                *stop = Some(SettingsPreparationStop::ConnectionClosed);
                                None
                            }
                            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(input.deadline)) => {
                                *stop = Some(SettingsPreparationStop::Expired);
                                None
                            }
                            joined = handle => Some(joined),
                        }
                    };
                    if let Some(joined) = joined {
                        cell.state = SettingsCustodyState::Observed {
                            expected: *task,
                            joined,
                            input: Arc::clone(input),
                            stop: *stop,
                            issue: None,
                        };
                    }
                }
                SettingsCustodyState::Observed { .. } => {
                    return self.finish_settings_preparation(id);
                }
                SettingsCustodyState::Prepared { input, .. } => {
                    let stopped = closing
                        .then_some(SettingsPreparationStop::Closed)
                        .or_else(|| input.stop())
                        .or_else(|| {
                            (!socket_live).then_some(SettingsPreparationStop::ConnectionClosed)
                        });
                    if let Some(cause) = stopped {
                        cell.state.stop_preparation(cause);
                        return Err(SettingsPreparationObserveFailure::Stopped(cause));
                    }
                    return Ok(());
                }
                SettingsCustodyState::Stopped { cause, .. } => {
                    return Err(SettingsPreparationObserveFailure::Stopped(*cause));
                }
                _ => return Err(SettingsPreparationObserveFailure::Association),
            }
        }
    }

    fn finish_settings_preparation(
        &mut self,
        id: MessageId,
    ) -> Result<(), SettingsPreparationObserveFailure> {
        use SettingsPreparationObserveFailure as Failure;

        let closing =
            self.finished || self.failure.is_some() || self.endpoint.outbound_is_quarantined();
        let Some(RelaySendEntry::Settings(cell)) = self.entries.get_mut(&id) else {
            return Err(Failure::Missing);
        };
        let SettingsCustodyState::Observed {
            expected,
            joined,
            input,
            stop,
            issue,
        } = &mut cell.state
        else {
            return Err(Failure::Association);
        };
        if stop.is_none() {
            *stop = closing
                .then_some(SettingsPreparationStop::Closed)
                .or_else(|| input.stop())
                .or_else(|| {
                    (!self.socket.is_live()).then_some(SettingsPreparationStop::ConnectionClosed)
                });
        }
        if let Some(issue) = issue {
            return Err(*issue);
        }
        let valid = Arc::ptr_eq(&input.window, &self.identity)
            && Arc::ptr_eq(&input.slot, &cell.slot.identity)
            && input.ticket.message_id() == id
            && input
                .ticket
                .matches_slot(&self.identity, &cell.slot.identity)
            && input.ticket.shares_socket(&self.socket)
            && input.ticket.matches_source(&input.source)
            && cell
                .association
                .as_ref()
                .is_some_and(|association| input.source.matches_association(association))
            && match joined {
                Ok(output) => {
                    Arc::ptr_eq(&output.slot, &input.slot)
                        && Arc::ptr_eq(&output.source, &input.source)
                }
                Err(error) => error.id() == *expected,
            };
        if !valid {
            *issue = Some(Failure::Association);
            return Err(Failure::Association);
        }
        if let Some(cause) = *stop {
            return Err(Failure::Stopped(cause));
        }
        if !matches!(joined, Ok(output) if output.outcome.is_ok()) {
            *issue = Some(Failure::Worker);
            return Err(Failure::Worker);
        }
        let state = std::mem::replace(&mut cell.state, SettingsCustodyState::Provisional);
        match state {
            SettingsCustodyState::Observed {
                joined:
                    Ok(SettingsPreparationOutput {
                        outcome: Ok(frame), ..
                    }),
                input,
                ..
            } => {
                cell.state = SettingsCustodyState::Prepared { frame, input };
                Ok(())
            }
            state => {
                cell.state = state;
                Err(Failure::Association)
            }
        }
    }

    pub(super) fn stop_settings_waiters(&mut self) {
        for entry in self.entries.values_mut() {
            if let RelaySendEntry::Settings(cell) = entry {
                cell.state.stop_preparation(SettingsPreparationStop::Closed);
            }
        }
    }
}

fn run_preparation(
    input: Arc<SettingsPreparationInput>,
    guard: OwnedMutexGuard<()>,
) -> SettingsPreparationOutput {
    let outcome = match input.stop() {
        Some(cause) => Err(SettingsPreparationWorkerFailure::Stopped(cause)),
        None => input
            .source
            .load_preparation_ciphertext(&guard)
            .map(Box::new),
    };
    drop(guard);
    let slot = Arc::clone(&input.slot);
    let source = Arc::clone(&input.source);
    drop(input);
    SettingsPreparationOutput {
        outcome,
        slot,
        source,
    }
}

impl fmt::Debug for SettingsPreparationWorkerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsPreparationWorkerFailure([retained cause])")
    }
}

impl fmt::Debug for SettingsPreparationStartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsPreparationStartFailure([retained ticket])")
    }
}

impl fmt::Display for SettingsPreparationStartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Settings ciphertext preparation could not start")
    }
}

impl std::error::Error for SettingsPreparationStartFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

impl fmt::Debug for SettingsPreparationObserveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsPreparationObserveFailure([retained result])")
    }
}

impl fmt::Display for SettingsPreparationObserveFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Settings ciphertext preparation did not become ready")
    }
}

impl std::error::Error for SettingsPreparationObserveFailure {}
