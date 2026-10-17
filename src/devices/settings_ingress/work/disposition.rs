use super::*;
use crate::devices::session::outbound::{ReturnedSettingsSqlInput, SettingsStartedReturnFailure};
use crate::storage::{SettingsPrivateOutboxDisposition, SettingsPrivateOutboxFailure};
use thiserror::Error;
use tokio::task::JoinError;

pub(super) type JoinedDispositionJob = Result<(Id, ReturnedDispositionOutput), JoinError>;

pub(super) enum ReturnedDispositionState {
    Waiting {
        message_id: crate::protocol::MessageId,
        association: Arc<SettingsSendLoanAssociation>,
        waiting: Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>,
    },
    Running {
        message_id: crate::protocol::MessageId,
        association: Arc<SettingsSendLoanAssociation>,
        task: Id,
    },
    Observed {
        message_id: crate::protocol::MessageId,
        association: Arc<SettingsSendLoanAssociation>,
        expected: Id,
        joined: JoinedDispositionJob,
    },
    Stopped {
        _message_id: crate::protocol::MessageId,
        _association: Arc<SettingsSendLoanAssociation>,
    },
}

pub(super) struct ReturnedDispositionOutput {
    pub(super) outcome: Result<SettingsPrivateOutboxDisposition, ReturnedDispositionFailure>,
    pub(super) input: Arc<ReturnedSettingsSqlInput>,
}

#[derive(Error)]
pub(super) enum ReturnedDispositionFailure {
    #[error("Settings returned source does not match")]
    Source(#[source] PairedHandoffReadError),
    #[error("Settings local no-resend disposition failed")]
    Storage(#[source] Box<SettingsPrivateOutboxFailure>),
}

pub(super) enum RetainedDisposition {
    Observed {
        expected: Id,
        joined: JoinedDispositionJob,
    },
    Finished {
        _outcome: RetainedDispositionOutcome,
        _source_return: Result<(), SettingsStartedReturnFailure>,
    },
}

pub(super) enum RetainedDispositionOutcome {
    Worker {
        _outcome: Result<SettingsPrivateOutboxDisposition, ReturnedDispositionFailure>,
    },
    Join {
        _cause: JoinError,
    },
}

impl SettingsReceiveWork {
    pub(super) fn disposition_task(&self) -> Option<Id> {
        match self.disposition.as_ref()? {
            ReturnedDispositionState::Running { task, .. } => Some(*task),
            _ => None,
        }
    }

    pub(super) fn disposition_healthy(&self) -> bool {
        match self.disposition.as_ref() {
            None => true,
            Some(
                ReturnedDispositionState::Waiting {
                    message_id,
                    association,
                    ..
                }
                | ReturnedDispositionState::Running {
                    message_id,
                    association,
                    ..
                },
            ) => {
                self.queued.disposition_input(*message_id).is_some()
                    && self
                        .queued
                        .disposition_association(*message_id)
                        .is_some_and(|actual| Arc::ptr_eq(actual, association))
            }
            Some(
                ReturnedDispositionState::Observed { .. }
                | ReturnedDispositionState::Stopped { .. },
            ) => false,
        }
    }

    pub(super) fn disposition_waiting(&self) -> bool {
        matches!(
            self.disposition,
            Some(ReturnedDispositionState::Waiting { .. })
        )
    }

    pub(super) fn spawn_disposition(
        &mut self,
        guard: OwnedMutexGuard<()>,
    ) -> Result<(), SettingsReceiveError> {
        let Some(ReturnedDispositionState::Waiting {
            message_id,
            association,
            ..
        }) = self.disposition.as_ref()
        else {
            return Err(self.ledger.fail());
        };
        let message_id = *message_id;
        if self
            .queued
            .disposition_association(message_id)
            .is_none_or(|actual| !Arc::ptr_eq(actual, association))
        {
            return Err(self.ledger.fail());
        }
        let input = self
            .queued
            .disposition_input(message_id)
            .ok_or_else(|| self.ledger.fail())?;
        let mutation = self
            .queued
            .disposition_mutation(message_id)
            .ok_or_else(|| self.ledger.fail())?;
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(&guard), mutation) {
            return Err(self.ledger.fail());
        }
        let worker = Arc::clone(input);
        let association = Arc::clone(association);
        {
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            if !self.queued.healthy(&self.ledger, counts.private) {
                return Err(self.ledger.fail());
            }
            counts.jobs = counts
                .jobs
                .checked_add(1)
                .ok_or_else(|| self.ledger.fail())?;
        }
        let task = self.jobs.spawn_blocking(move || {
            RegisteredJobOutput::Disposition(Box::new(run_disposition(worker, guard)))
        });
        self.disposition = Some(ReturnedDispositionState::Running {
            message_id,
            association,
            task: task.id(),
        });
        Ok(())
    }

    pub(super) fn register_disposition_join(
        &mut self,
        joined: JoinedDispositionJob,
    ) -> Result<(), JoinedDispositionJob> {
        let Some(ReturnedDispositionState::Running {
            message_id,
            association,
            task,
        }) = self.disposition.as_ref()
        else {
            return Err(joined);
        };
        self.disposition = Some(ReturnedDispositionState::Observed {
            message_id: *message_id,
            association: Arc::clone(association),
            expected: *task,
            joined,
        });
        Ok(())
    }

    pub(super) fn finish_disposition_join(
        &mut self,
    ) -> Result<crate::protocol::MessageId, SettingsReceiveError> {
        let Some(ReturnedDispositionState::Observed {
            message_id,
            association,
            ..
        }) = self.disposition.as_ref()
        else {
            return Err(self.ledger.fail());
        };
        let message_id = *message_id;
        if self
            .queued
            .disposition_association(message_id)
            .is_none_or(|actual| !Arc::ptr_eq(actual, association))
        {
            return Err(self.ledger.fail());
        }
        let Some(state) = self.disposition.take() else {
            return Err(self.ledger.fail());
        };
        let ReturnedDispositionState::Observed {
            message_id,
            association,
            expected,
            joined,
        } = state
        else {
            self.disposition = Some(state);
            return Err(self.ledger.fail());
        };
        if let Err(joined) = self
            .queued
            .retain_disposition_join(message_id, expected, joined)
        {
            self.disposition = Some(ReturnedDispositionState::Observed {
                message_id,
                association,
                expected,
                joined,
            });
            return Err(self.ledger.fail());
        }
        self.queued
            .finish_disposition_join(&self.ledger, message_id)?;
        Ok(message_id)
    }

    pub(super) fn stop_disposition_waiter(&mut self) {
        if let Some(ReturnedDispositionState::Waiting {
            message_id,
            association,
            ..
        }) = self.disposition.as_ref()
        {
            let message_id = *message_id;
            let association = Arc::clone(association);
            let waiting = self.disposition.replace(ReturnedDispositionState::Stopped {
                _message_id: message_id,
                _association: association,
            });
            drop(waiting);
        }
    }
}

fn run_disposition(
    input: Arc<ReturnedSettingsSqlInput>,
    guard: OwnedMutexGuard<()>,
) -> ReturnedDispositionOutput {
    let outcome = match build::ReturnedSettingsReplySource::new(&input) {
        Ok(source) => match source.store() {
            Some(store) => store
                .dispose_returned_settings_reply(&source)
                .map_err(ReturnedDispositionFailure::Storage),
            None => Err(ReturnedDispositionFailure::Source(
                PairedHandoffReadError::Bookkeeping,
            )),
        },
        Err(cause) => Err(ReturnedDispositionFailure::Source(cause)),
    };
    drop(guard);
    ReturnedDispositionOutput { outcome, input }
}

impl fmt::Debug for ReturnedDispositionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReturnedDispositionFailure([retained cause])")
    }
}
