mod queued;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::OwnedMutexGuard;
use tokio::task::{Id, JoinError};

use crate::devices::SettingsReplySealContext;
use crate::storage::{CommittedSettingsReply, SettingsReplyPublishFailure};

pub(crate) use queued::OwnedQueuedSettingsReply;
pub(super) use queued::QueuedReplyRegistry;
pub(in crate::devices) use queued::{
    PendingSettingsSend, SettingsSendAdmissionError, SettingsSendLoanAssociation,
    SettingsSendReturnFailure, SettingsSendReturnReason, SettingsSendSource,
};

use super::{
    PairedHandoffReadError, PendingSettingsReplyBuild, ReceiveContext, RegisteredReplyBuild,
    RegisteredReplyBuildOwner, SettingsConnectionIdentity, SettingsReceiptAuthority,
    SettingsReceiveError, SettingsReceiveWork, SettingsReplyBuildRequest,
};

type GuardWait = Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>;
type JoinedReplyBuildJob = Result<(Id, ReplyBuildWorkerOutput), JoinError>;

pub(super) struct ReplyBuildJobInput {
    context: Arc<ReceiveContext>,
    authority: SettingsReceiptAuthority,
    record: RegisteredReplyBuild,
}

pub(super) enum ReplyBuildState {
    Unstarted(RegisteredReplyBuild),
    Waiting {
        waiting: GuardWait,
        input: Arc<ReplyBuildJobInput>,
    },
    Running {
        task: Id,
        input: Arc<ReplyBuildJobInput>,
    },
    Observed {
        expected: Id,
        joined: JoinedReplyBuildJob,
        input: Arc<ReplyBuildJobInput>,
    },
    JoinedReady {
        _committed: Box<CommittedSettingsReply>,
        _input: Arc<ReplyBuildJobInput>,
    },
    Retained {
        _issue: RetainedReplyBuildIssue,
        _input: Arc<ReplyBuildJobInput>,
    },
}

pub(super) enum RetainedReplyBuildIssue {
    Stopped(SettingsReceiveError),
    Observed {
        _cause: SettingsReceiveError,
        _joined: JoinedReplyBuildJob,
    },
}

pub(super) struct ReplyBuildWorkerOutput {
    outcome: ReplyBuildWorkerOutcome,
    slot: Arc<()>,
}

enum ReplyBuildWorkerOutcome {
    Publication(Result<Box<CommittedSettingsReply>, Box<SettingsReplyPublishFailure>>),
    HandoffRead(PairedHandoffReadError),
}

impl RegisteredReplyBuild {
    fn reply_request(&self) -> Result<&SettingsReplyBuildRequest, PairedHandoffReadError> {
        match &self.request {
            RegisteredReplyBuildOwner::Direct(request) => Ok(request),
            RegisteredReplyBuildOwner::Supervised(handoff) => handoff.reply_request(),
        }
    }
}

impl SettingsReceiveWork {
    pub(in crate::devices) fn try_start_reply_build(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        ticket: &PendingSettingsReplyBuild,
        authority: SettingsReceiptAuthority,
    ) -> Result<(), SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.closed || self.failure.is_some() || !connection.is_live() {
            return Err(SettingsReceiveError::Closed);
        }
        let context = self.context.as_ref().ok_or_else(|| self.ledger.fail())?;
        let Some(ReplyBuildState::Unstarted(record)) = self.build.as_ref() else {
            return Err(SettingsReceiveError::Capacity);
        };
        if !record.adoption_sent || record.adoption.is_some() {
            return Err(SettingsReceiveError::Capacity);
        }
        let request = record.reply_request().map_err(handoff_receive_error)?;
        let witness = request.witness();
        let joined = witness.build.as_ref().ok_or_else(|| self.ledger.fail())?;
        if !Arc::ptr_eq(&witness.ledger, &self.ledger)
            || !Arc::ptr_eq(&joined.association, &record.permit.association)
            || joined.claim != super::ReplyBuildClaim::Claimed
            || witness.peer != record.permit.association.reply.peer_device_id
            || !Arc::ptr_eq(&ticket.slot, &record.permit.slot)
            || !record.permit.armed
            || !Arc::ptr_eq(&record.permit.ledger, &self.ledger)
            || self
                .history
                .as_ref()
                .is_none_or(|history| !Arc::ptr_eq(&history.connection, connection))
            || !request.matches_context(context, connection)
            || !request.matches_association(&record.permit.association)
        {
            return Err(self.ledger.fail());
        }
        {
            let counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            self.ledger.check_admission()?;
            if self.ledger.closed.load(Ordering::Acquire) || !connection.is_live() {
                return Err(SettingsReceiveError::Closed);
            }
            if counts.build.as_ref().is_none_or(|slot| {
                !Arc::ptr_eq(&slot.identity, &record.permit.slot)
                    || !Arc::ptr_eq(&slot.association, &record.permit.association)
            }) {
                return Err(self.ledger.fail());
            }
        }
        let context = Arc::clone(context);
        let waiting = Box::pin(Arc::clone(&context.mutation).lock_owned());
        let Some(state) = self.build.take() else {
            return Err(self.ledger.fail());
        };
        match state {
            ReplyBuildState::Unstarted(record) => {
                let input = Arc::new(ReplyBuildJobInput {
                    context,
                    authority,
                    record,
                });
                self.build = Some(ReplyBuildState::Waiting { waiting, input });
                Ok(())
            }
            state => {
                self.build = Some(state);
                Err(self.ledger.fail())
            }
        }
    }

    pub(super) fn spawn_reply_build(
        &mut self,
        guard: OwnedMutexGuard<()>,
    ) -> Result<(), SettingsReceiveError> {
        self.ledger.check_admission()?;
        if self.closed {
            return Err(SettingsReceiveError::Closed);
        }
        let Some(ReplyBuildState::Waiting { input, .. }) = self.build.as_ref() else {
            return Err(self.ledger.fail());
        };
        let worker = Arc::clone(input);
        let retained = Arc::clone(input);
        let task = self.jobs.spawn_blocking(move || {
            super::RegisteredJobOutput::ReplyBuild(Box::new(run_reply_build(worker, guard)))
        });
        self.build = Some(ReplyBuildState::Running {
            task: task.id(),
            input: retained,
        });
        Ok(())
    }

    pub(super) fn reply_build_task(&self) -> Option<Id> {
        match self.build.as_ref() {
            Some(ReplyBuildState::Running { task, .. }) => Some(*task),
            _ => None,
        }
    }

    pub(super) fn register_reply_build_join(
        &mut self,
        joined: JoinedReplyBuildJob,
    ) -> Result<(), JoinedReplyBuildJob> {
        let Some(ReplyBuildState::Running { task, input }) = self.build.as_ref() else {
            return Err(joined);
        };
        let expected = *task;
        let input = Arc::clone(input);
        self.build = Some(ReplyBuildState::Observed {
            expected,
            joined,
            input,
        });
        Ok(())
    }

    pub(super) fn finish_reply_build_join(&mut self) -> Result<(), SettingsReceiveError> {
        let status = self.check_reply_build_join();
        let Some(state) = self.build.take() else {
            return Err(self.ledger.fail());
        };
        match (status, state) {
            (
                Ok(()),
                ReplyBuildState::Observed {
                    joined:
                        Ok((
                            _,
                            ReplyBuildWorkerOutput {
                                outcome: ReplyBuildWorkerOutcome::Publication(Ok(committed)),
                                ..
                            },
                        )),
                    input,
                    ..
                },
            ) => {
                self.build = Some(ReplyBuildState::JoinedReady {
                    _committed: committed,
                    _input: input,
                });
                Ok(())
            }
            (status, ReplyBuildState::Observed { joined, input, .. }) => {
                let cause = status.err().unwrap_or(SettingsReceiveError::Bookkeeping);
                self.build = Some(ReplyBuildState::Retained {
                    _issue: RetainedReplyBuildIssue::Observed {
                        _cause: cause.clone(),
                        _joined: joined,
                    },
                    _input: input,
                });
                self.ledger.fail();
                self.failure = Some(cause.clone());
                self.close_intake();
                Err(cause)
            }
            (_, state) => {
                self.build = Some(state);
                self.latch_failure();
                Err(SettingsReceiveError::Bookkeeping)
            }
        }
    }

    fn check_reply_build_join(&self) -> Result<(), SettingsReceiveError> {
        let Some(ReplyBuildState::Observed {
            expected,
            joined,
            input,
        }) = self.build.as_ref()
        else {
            return Err(self.ledger.fail());
        };
        let (actual, output) = joined.as_ref().map_err(|_| SettingsReceiveError::Worker)?;
        if actual != expected
            || !Arc::ptr_eq(&output.slot, &input.record.permit.slot)
            || !Arc::ptr_eq(&input.record.permit.ledger, &self.ledger)
            || !input.record.permit.armed
            || self
                .context
                .as_ref()
                .is_none_or(|context| !Arc::ptr_eq(context, &input.context))
        {
            return Err(self.ledger.fail());
        }
        self.ledger.check_admission()?;
        match &output.outcome {
            ReplyBuildWorkerOutcome::Publication(Ok(_)) => Ok(()),
            ReplyBuildWorkerOutcome::Publication(Err(_)) => Err(SettingsReceiveError::Worker),
            ReplyBuildWorkerOutcome::HandoffRead(error) => Err(match error {
                PairedHandoffReadError::Closed => SettingsReceiveError::Closed,
                PairedHandoffReadError::RejectedBind | PairedHandoffReadError::Bookkeeping => {
                    SettingsReceiveError::Bookkeeping
                }
            }),
        }
    }

    pub(super) fn stop_waiting_reply_build(&mut self, cause: SettingsReceiveError) {
        if let Some(ReplyBuildState::Waiting { input, .. }) = self.build.as_ref() {
            let input = Arc::clone(input);
            self.build = Some(ReplyBuildState::Retained {
                _issue: RetainedReplyBuildIssue::Stopped(cause),
                _input: input,
            });
        }
    }
}

fn run_reply_build(
    input: Arc<ReplyBuildJobInput>,
    guard: OwnedMutexGuard<()>,
) -> ReplyBuildWorkerOutput {
    let slot = Arc::clone(&input.record.permit.slot);
    let outcome = match input.record.reply_request() {
        Ok(request) => {
            let result = match request {
                SettingsReplyBuildRequest::Fresh(original) => {
                    let source = original.committed_source();
                    publish_request(&input, &guard, request, source.receipt_authority())
                }
                SettingsReplyBuildRequest::Historical(original) => {
                    publish_request(&input, &guard, request, original.receipt_authority())
                }
                SettingsReplyBuildRequest::Bind { .. } => {
                    publish_request(&input, &guard, request, &input.authority)
                }
                SettingsReplyBuildRequest::Access(access) => publish_request(
                    &input,
                    &guard,
                    request,
                    access.verified_request().receipt_authority(),
                ),
            };
            ReplyBuildWorkerOutcome::Publication(result.map(Box::new))
        }
        Err(error) => ReplyBuildWorkerOutcome::HandoffRead(error),
    };
    drop(guard);
    drop(input);
    ReplyBuildWorkerOutput { outcome, slot }
}

fn publish_request(
    input: &ReplyBuildJobInput,
    guard: &OwnedMutexGuard<()>,
    request: &SettingsReplyBuildRequest,
    authority: &SettingsReceiptAuthority,
) -> Result<CommittedSettingsReply, Box<SettingsReplyPublishFailure>> {
    let store = input.context.agent.store();
    store.publish_settings_reply(
        SettingsReplySealContext {
            agent: &input.context.agent,
            endpoint: &input.context.endpoint,
            store: &store,
            mutation: guard,
            authority,
            permit: &input.record.permit,
        },
        request,
    )
}

fn handoff_receive_error(error: PairedHandoffReadError) -> SettingsReceiveError {
    match error {
        PairedHandoffReadError::Closed => SettingsReceiveError::Closed,
        PairedHandoffReadError::RejectedBind | PairedHandoffReadError::Bookkeeping => {
            SettingsReceiveError::Bookkeeping
        }
    }
}
