use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::OwnedMutexGuard;
use tokio::task::{Id, JoinError};

use crate::devices::SettingsReplySealContext;
use crate::protocol::MessageId;
use crate::storage::{CommittedSettingsReply, SettingsReplyPublishFailure};

use super::{
    ReceiveContext, RegisteredJobOutput, RegisteredReplyBuild, ReplyBuildPermit,
    SettingsOwnerError, SettingsReceiveError, SettingsReceiveWork, SettingsReplyBuildRequest,
};

const MAX_QUEUED_REPLIES: usize = 32;

type GuardWait = Pin<Box<dyn Future<Output = OwnedMutexGuard<()>> + Send>>;
type JoinedReplyBuildJob = Result<(Id, ReplyBuildWorkerOutput), JoinError>;

pub(super) struct ReplyBuildJobInput {
    context: Arc<ReceiveContext>,
    record: RegisteredReplyBuild,
}

pub(super) enum ReplyBuildState {
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
    Retained {
        _cause: SettingsReceiveError,
        _joined: Option<JoinedReplyBuildJob>,
        _input: Arc<ReplyBuildJobInput>,
    },
}

pub(super) struct ReplyBuildWorkerOutput {
    outcome: Result<Box<CommittedSettingsReply>, Box<SettingsReplyPublishFailure>>,
    slot: Arc<()>,
}

pub(crate) struct OwnedQueuedSettingsReply {
    payload: Option<QueuedReplyPayload>,
}

struct QueuedReplyPayload {
    committed: Box<CommittedSettingsReply>,
    input: ReplyBuildJobInput,
}

struct AbandonedQueuedReply {
    _payload: QueuedReplyPayload,
}

impl OwnedQueuedSettingsReply {
    fn payload(&self) -> Result<&QueuedReplyPayload, SettingsOwnerError> {
        let payload = self.payload.as_ref().ok_or(SettingsOwnerError)?;
        let ledger = &payload.input.record.permit.ledger;
        ledger.check_admission().map_err(|_| SettingsOwnerError)?;
        if payload.input.record.permit.armed {
            ledger.fail();
            return Err(SettingsOwnerError);
        }
        Ok(payload)
    }

    pub(crate) fn committed(&self) -> Result<&CommittedSettingsReply, SettingsOwnerError> {
        Ok(&self.payload()?.committed)
    }

    pub(crate) fn request(&self) -> Result<&SettingsReplyBuildRequest, SettingsOwnerError> {
        Ok(&self.payload()?.input.record.request)
    }

    pub(in crate::devices) fn retire(mut self) {
        drop(self.payload.take());
    }
}

impl Drop for OwnedQueuedSettingsReply {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let ledger = Arc::clone(&payload.input.record.permit.ledger);
            ledger.fail();
            ledger.terminal.store(true, Ordering::Release);
            ledger.changed.notify_waiters();
            let _ = Box::leak(Box::new(AbandonedQueuedReply { _payload: payload }));
        }
    }
}

impl fmt::Debug for OwnedQueuedSettingsReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedQueuedSettingsReply([retained source])")
    }
}

impl SettingsReceiveWork {
    pub(super) fn begin_reply_build(
        &mut self,
        context: Arc<ReceiveContext>,
        record: RegisteredReplyBuild,
    ) {
        let waiting = Box::pin(Arc::clone(&context.mutation).lock_owned());
        self.build = Some(ReplyBuildState::Waiting {
            waiting,
            input: Arc::new(ReplyBuildJobInput { context, record }),
        });
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
        {
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            counts.jobs = counts
                .jobs
                .checked_add(1)
                .ok_or_else(|| self.ledger.fail())?;
        }
        let task = self.jobs.spawn_blocking(move || {
            RegisteredJobOutput::ReplyBuild(Box::new(run_reply_build(worker, guard)))
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

    pub(super) fn finish_reply_build_join(
        &mut self,
    ) -> Result<Option<MessageId>, SettingsReceiveError> {
        let accounted = (|| {
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            counts.jobs = counts
                .jobs
                .checked_sub(1)
                .ok_or_else(|| self.ledger.fail())?;
            Ok::<_, SettingsReceiveError>(())
        })();
        let Some(ReplyBuildState::Observed {
            expected,
            joined,
            input,
        }) = self.build.take()
        else {
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        };
        let matched = accounted.is_ok()
            && matches!(&joined, Ok((actual, output)) if *actual == expected && Arc::ptr_eq(&output.slot, &input.record.permit.slot))
            && Arc::ptr_eq(&input.record.permit.ledger, &self.ledger)
            && self
                .context
                .as_ref()
                .is_some_and(|context| Arc::ptr_eq(context, &input.context));
        let (outcome, input) = match (matched, joined) {
            (true, Ok((_, output))) => match Arc::try_unwrap(input) {
                Ok(input) => (output.outcome, input),
                Err(input) => {
                    return Err(self.retain_build(SettingsReceiveError::Bookkeeping, None, input));
                }
            },
            (_, joined) => {
                return Err(self.retain_build(SettingsReceiveError::Worker, Some(joined), input));
            }
        };
        match outcome {
            Ok(committed) => self.queue_reply(committed, input).map(Some),
            Err(failure) if failure.rolled_back() => {
                let mut input = input;
                self.release_build_slot(&mut input.record.permit)?;
                drop(input);
                Ok(None)
            }
            Err(failure) => {
                let mut input = input;
                input.record.permit.armed = false;
                let _ = Box::leak(Box::new((failure, input)));
                self.ledger.terminal.store(true, Ordering::Release);
                self.latch_failure();
                Err(SettingsReceiveError::Worker)
            }
        }
    }

    fn queue_reply(
        &mut self,
        committed: Box<CommittedSettingsReply>,
        mut input: ReplyBuildJobInput,
    ) -> Result<MessageId, SettingsReceiveError> {
        let header = committed.header();
        let message_id = header.message_id;
        let SettingsReplyBuildRequest::Access(request) = &input.record.request;
        let reply = request.reply_reservation();
        if message_id.as_uuid().is_nil()
            || header.sender != reply.local_device_id
            || header.recipient != reply.peer_device_id
            || header.created_at_ms < reply.created_at.get()
            || header.created_at_ms >= header.expires_at_ms
            || header.expires_at_ms != reply.expires_at.get()
            || self.queued.len() >= MAX_QUEUED_REPLIES
            || self.queued.contains_key(&message_id)
        {
            input.record.permit.armed = false;
            let _ = Box::leak(Box::new((committed, input)));
            self.ledger.terminal.store(true, Ordering::Release);
            self.latch_failure();
            return Err(SettingsReceiveError::Bookkeeping);
        }
        self.release_build_slot(&mut input.record.permit)?;
        self.queued.insert(
            message_id,
            OwnedQueuedSettingsReply {
                payload: Some(QueuedReplyPayload { committed, input }),
            },
        );
        self.ledger.changed.notify_waiters();
        Ok(message_id)
    }

    fn release_build_slot(
        &mut self,
        permit: &mut ReplyBuildPermit,
    ) -> Result<(), SettingsReceiveError> {
        let released = (|| {
            if !permit.armed || !Arc::ptr_eq(&permit.ledger, &self.ledger) {
                return Err(self.ledger.fail());
            }
            let mut counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
            if counts.build.as_ref().is_none_or(|slot| {
                !Arc::ptr_eq(&slot.identity, &permit.slot)
                    || !Arc::ptr_eq(&slot.association, &permit.association)
            }) {
                return Err(self.ledger.fail());
            }
            counts.build = None;
            permit.armed = false;
            Ok(())
        })();
        if let Err(error) = released {
            self.latch_failure();
            return Err(error);
        }
        self.ledger.changed.notify_waiters();
        Ok(())
    }

    fn retain_build(
        &mut self,
        cause: SettingsReceiveError,
        joined: Option<JoinedReplyBuildJob>,
        input: Arc<ReplyBuildJobInput>,
    ) -> SettingsReceiveError {
        self.build = Some(ReplyBuildState::Retained {
            _cause: cause.clone(),
            _joined: joined,
            _input: input,
        });
        self.ledger.fail();
        self.failure = Some(cause.clone());
        self.close_intake();
        cause
    }

    pub(super) fn withdraw_waiting_reply_build(&mut self) {
        if !matches!(self.build, Some(ReplyBuildState::Waiting { .. })) {
            return;
        }
        let Some(ReplyBuildState::Waiting { waiting, input }) = self.build.take() else {
            return;
        };
        drop(waiting);
        match Arc::try_unwrap(input) {
            Ok(mut input) => {
                if self.release_build_slot(&mut input.record.permit).is_ok() {
                    drop(input);
                } else {
                    let _ = Box::leak(Box::new(input));
                }
            }
            Err(input) => {
                self.retain_build(SettingsReceiveError::Bookkeeping, None, input);
            }
        }
    }
}

fn run_reply_build(
    input: Arc<ReplyBuildJobInput>,
    guard: OwnedMutexGuard<()>,
) -> ReplyBuildWorkerOutput {
    let slot = Arc::clone(&input.record.permit.slot);
    let SettingsReplyBuildRequest::Access(access) = &input.record.request;
    let store = input.context.agent.store();
    let outcome = store
        .publish_settings_reply(
            SettingsReplySealContext {
                agent: &input.context.agent,
                endpoint: &input.context.endpoint,
                store: &store,
                mutation: &guard,
                authority: access.verified_request().receipt_authority(),
                permit: &input.record.permit,
            },
            &input.record.request,
        )
        .map(Box::new);
    drop(guard);
    drop(input);
    ReplyBuildWorkerOutput { outcome, slot }
}
