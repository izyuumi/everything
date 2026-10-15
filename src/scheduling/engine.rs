use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Notify, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, LoopStatus, NativeRunInfo, PauseReason, TextCommand};
use crate::protocol::{TaskStatus, UnixMillis};
use crate::scheduler::{Scheduler, SchedulerError, TaskSnapshot, TaskSubmission, WaitingReason};
use crate::tools::ToolRegistry;

use super::attempts::{ScheduledAttempt, capture_policy, occurrence_deadline};
use super::clock::{ClockError, latest_at_or_before, next_after};
use super::execution::{AttemptResultKind, AttemptUsage, native_usage};
use super::{
    DueClaim, DueDisposition, MissedRunPolicy, OccurrenceId, RunOutcome, ScheduleError,
    ScheduleRepository, ScheduledJob, ScheduledJobId, ScheduledRun,
};

const AUDIT: Duration = Duration::from_secs(1);
const NORMAL_LATENESS_MS: u64 = 5_000;
const BATCH: usize = 8;
const OUTCOME_PREVIEW_BYTES: usize = 4_096;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Clock(#[from] ClockError),
    #[error(transparent)]
    Native(#[from] SchedulerError),
    #[error(transparent)]
    Agent(#[from] crate::agent::AgentError),
    #[error("schedule engine operation cancelled")]
    Cancelled,
    #[error("schedule engine worker failed")]
    Worker,
    #[error("schedule engine is stopped")]
    Stopped,
    #[error(
        "schedule engine still owns an admission worker; retain host services and retry shutdown"
    )]
    ShutdownPending,
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockedSchedule {
    pub job_id: ScheduledJobId,
    pub diagnostic: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EngineStatus {
    pub active: bool,
    pub stopped: bool,
    pub blocked_jobs: usize,
    pub blocked: Vec<BlockedSchedule>,
    pub last_error: Option<String>,
    pub pending_deliveries: usize,
    pub delivery_error: Option<String>,
    pub delivery_preparation_error: Option<String>,
}

#[derive(Clone)]
pub struct ScheduleEngine {
    inner: Arc<EngineHandle>,
}

struct EngineHandle {
    stop: CancellationToken,
    wake: Arc<Notify>,
    activation: watch::Sender<bool>,
    status: watch::Receiver<EngineStatus>,
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop.cancel();
        self.wake.notify_one();
    }
}

struct Blocked {
    until: Instant,
    diagnostic: String,
}

struct Actor {
    agent: Agent,
    scheduler: Scheduler,
    registry: Arc<ToolRegistry>,
    repository: ScheduleRepository,
    stop: CancellationToken,
    wake: Arc<Notify>,
    activation: watch::Receiver<bool>,
    status: watch::Sender<EngineStatus>,
    admission: Arc<Semaphore>,
    blocked: BTreeMap<ScheduledJobId, Blocked>,
    recovery_cursor: Option<OccurrenceId>,
    delivery_cursor: Option<OccurrenceId>,
    delivery_poll: Option<Instant>,
}

impl ScheduleEngine {
    pub async fn start(
        agent: Agent,
        scheduler: Scheduler,
        registry: Arc<ToolRegistry>,
    ) -> Result<Self, EngineError> {
        let repository = ScheduleRepository::new(agent.store());
        let inspected = repository.clone();
        let lifetime = agent.clone();
        tokio::task::spawn_blocking(move || {
            let _lifetime = lifetime;
            inspected.execution_ready()
        })
        .await
        .map_err(|_| EngineError::Worker)??;
        let stop = CancellationToken::new();
        let wake = Arc::new(Notify::new());
        let (activation, activated) = watch::channel(false);
        let (status, observed) = watch::channel(EngineStatus::default());
        let actor = Actor {
            agent,
            scheduler,
            registry,
            repository,
            stop: stop.clone(),
            wake: Arc::clone(&wake),
            activation: activated,
            status,
            admission: Arc::new(Semaphore::new(1)),
            blocked: BTreeMap::new(),
            recovery_cursor: None,
            delivery_cursor: None,
            delivery_poll: None,
        };
        tokio::spawn(actor.run());
        Ok(Self {
            inner: Arc::new(EngineHandle {
                stop,
                wake,
                activation,
                status: observed,
            }),
        })
    }

    pub fn activate(&self) -> Result<(), EngineError> {
        if self.inner.stop.is_cancelled() || self.status().stopped {
            return Err(EngineError::Stopped);
        }
        self.inner
            .activation
            .send(true)
            .map_err(|_| EngineError::Stopped)?;
        self.wake();
        Ok(())
    }

    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }
    pub fn status(&self) -> EngineStatus {
        self.inner.status.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<EngineStatus> {
        self.inner.status.clone()
    }

    pub async fn shutdown(&self) -> Result<(), EngineError> {
        self.inner.stop.cancel();
        self.inner.wake.notify_one();
        let mut status = self.inner.status.clone();
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|state| state.stopped),
        )
        .await
        .map_err(|_| EngineError::ShutdownPending)?
        .map_err(|_| EngineError::Worker)?;
        Ok(())
    }
}

impl Actor {
    async fn run(mut self) {
        let mut changed = self.scheduler.subscribe();
        loop {
            if self.stop.is_cancelled() {
                break;
            }
            if !*self.activation.borrow() {
                tokio::select! {biased;
                    _=self.stop.cancelled()=>break,
                    result=self.activation.changed()=>if result.is_err(){break;},
                    _=self.wake.notified()=>{},
                }
                continue;
            }
            self.status.send_modify(|status| status.active = true);
            let delay = match self.cycle().await {
                Ok(delay) => delay,
                Err(EngineError::Cancelled) => break,
                Err(error) => {
                    self.error(&error);
                    AUDIT
                }
            };
            self.publish();
            tokio::select! {biased;
                _=self.stop.cancelled()=>break,
                _=self.wake.notified()=>self.blocked.clear(),
                result=changed.changed()=>if result.is_err(){self.status.send_modify(|status|status.last_error=Some("native scheduler change stream closed".into()));break;},
                _=tokio::time::sleep(delay)=>{},
            }
        }
        self.status.send_modify(|status| {
            status.active = false;
            status.stopped = true;
        });
    }

    async fn work<T: Send + 'static>(
        &self,
        operation: impl FnOnce(ScheduleRepository, CancellationToken) -> Result<T, EngineError>
        + Send
        + 'static,
    ) -> Result<T, EngineError> {
        if self.stop.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        let permit = Arc::clone(&self.admission)
            .acquire_owned()
            .await
            .map_err(|_| EngineError::Worker)?;
        let repository = self.repository.clone();
        let cancellation = self.stop.clone();
        let lifetime = self.agent.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _lifetime = lifetime;
            if cancellation.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            operation(repository, cancellation)
        })
        .await
        .map_err(|_| EngineError::Worker)?
    }

    async fn cycle(&mut self) -> Result<Duration, EngineError> {
        if self.delivery_poll.is_none_or(|at| at.elapsed() >= AUDIT) {
            self.delivery_poll = Some(Instant::now());
            if let Err(error) = self.process_deliveries().await {
                self.status.send_modify(|status| {
                    status.delivery_error = Some(error.to_string().chars().take(256).collect())
                });
            }
        }
        let cursor = self.recovery_cursor;
        let runs = self
            .work(move |repository, _| Ok(repository.unfinished_runs(cursor, BATCH)?))
            .await?;
        self.recovery_cursor = runs.last().map(|run| run.occurrence_id);
        for run in runs {
            if self.stop.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let id = run.job_id;
            if self
                .blocked
                .get(&id)
                .is_some_and(|blocked| blocked.until > Instant::now())
            {
                continue;
            }
            if let Err(error) = self.process_run(run).await {
                self.block(id, &error);
            }
        }
        self.blocked.retain(|_, value| value.until > Instant::now());
        let excluded = self.blocked.keys().copied().collect::<Vec<_>>();
        let now = wall_now()?;
        let jobs = self
            .work(move |repository, _| Ok(repository.due_excluding(now, &excluded, BATCH)?))
            .await?;
        let full = jobs.len() == BATCH;
        let mut progressed = false;
        for job in jobs {
            if self.stop.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let id = job.id;
            let result = self
                .work(move |repository, cancellation| {
                    let now = wall_now()?;
                    let claim = plan(&job, now, &cancellation)?;
                    Ok(repository.claim_due(&claim, now)?.run)
                })
                .await;
            match result {
                Ok(run) if !run.state.is_terminal() => {
                    progressed = true;
                    if let Err(error) = self.process_run(run).await {
                        self.block(id, &error);
                    }
                }
                Ok(_) => {
                    progressed = true;
                }
                Err(EngineError::Schedule(
                    ScheduleError::RevisionConflict { .. } | ScheduleError::NotDue,
                )) => {}
                Err(error) => self.block(id, &error),
            }
        }
        if full || progressed {
            return Ok(Duration::from_millis(20));
        }
        let next = self
            .work(|repository, _| Ok(repository.earliest_due()?))
            .await?;
        let now = wall_now()?;
        Ok(next.filter(|next| *next > now).map_or(AUDIT, |next| {
            Duration::from_millis(next.get() - now.get()).min(AUDIT)
        }))
    }

    async fn process_run(&self, run: ScheduledRun) -> Result<(), EngineError> {
        let occurrence = run.occurrence_id;
        self.work(move |repository, _| Ok(repository.reserve_run_delivery(occurrence)?))
            .await?;
        let existing = self
            .work(move |repository, _| Ok(repository.latest_attempt(occurrence)?))
            .await?;
        let deadline = occurrence_deadline(&run)?;
        let now = wall_now()?;
        let attempt = match existing {
            Some(attempt) => attempt,
            None if now >= deadline || now < run.claimed_at => {
                return self.finish(&run,RunOutcome::Cancelled {reason:"occurrence expired or wall clock moved backwards before native admission".into()}).await;
            }
            None => {
                let registry = Arc::clone(&self.registry);
                let captured = run.clone();
                self.work(move |repository, _| {
                    let policy = capture_policy(&captured, &registry)?;
                    Ok(repository.reserve_first_attempt(occurrence, &policy)?)
                })
                .await?
            }
        };
        let attempt_number = attempt.attempt_number;
        if let Some(result) = self
            .work(move |repository, _| Ok(repository.attempt_result(occurrence, attempt_number)?))
            .await?
        {
            if result.next_retry_at.is_none() {
                return Err(
                    ScheduleError::Invalid("settled attempt has no unfinished retry").into(),
                );
            }
            if now >= deadline || now < result.settled_at {
                return self
                    .finish(
                        &run,
                        RunOutcome::Cancelled {
                            reason: "scheduled retry expired or wall clock moved backwards".into(),
                        },
                    )
                    .await;
            }
            if result.next_retry_at.is_some_and(|at| now < at) {
                return Ok(());
            }
            let attempt = self
                .work(move |repository, _| Ok(repository.reserve_retry(occurrence, now)?))
                .await?;
            return self.admit_and_observe(run, attempt).await;
        }
        self.admit_and_observe(run, attempt).await
    }

    async fn admit_and_observe(
        &self,
        mut run: ScheduledRun,
        attempt: ScheduledAttempt,
    ) -> Result<(), EngineError> {
        let occurrence = run.occurrence_id;
        let deadline = attempt.deadline;
        let number = attempt.attempt_number;
        let admitted = self
            .work(move |repository, _| Ok(repository.attempt_admitted(occurrence, number)?))
            .await?;
        let mut snapshot = match self.scheduler.status(attempt.task_id).await {
            Ok(snapshot) => snapshot,
            Err(SchedulerError::NotFound(_)) => {
                if admitted {
                    return self.finish(&run,RunOutcome::Unknown {reason:"durably admitted native task is missing; owner reconciliation required".into()}).await;
                }
                let now = wall_now()?;
                if now >= deadline || now < run.claimed_at {
                    return self.finish(&run,RunOutcome::Cancelled {reason:"occurrence expired or wall clock moved backwards before native admission".into()}).await;
                }
                self.check_stop()?;
                self.agent
                    .create_conversation(
                        attempt.conversation_id,
                        attempt.create_command_id,
                        format!("Scheduled {}", run.job_id),
                        run.definition.target.configuration.clone(),
                    )
                    .await?;
                self.check_stop()?;
                self.agent
                    .accept_text(TextCommand {
                        conversation_id: attempt.conversation_id,
                        command_id: attempt.input_command_id,
                        text: run.definition.target.instruction.clone(),
                    })
                    .await?;
                self.check_stop()?;
                self.scheduler.enqueue(submission(&attempt)).await?
            }
            Err(error) => return Err(error.into()),
        };
        validate_snapshot(&attempt, &run, &snapshot)?;
        if !admitted {
            snapshot = self.scheduler.enqueue(submission(&attempt)).await?;
            validate_snapshot(&attempt, &run, &snapshot)?;
            let captured = attempt.clone();
            let recorded = wall_now()?;
            run = self
                .work(move |repository, _| {
                    Ok(repository.mark_attempt_admitted(&captured, recorded)?)
                })
                .await?;
        }
        let now = wall_now()?;
        if !snapshot.task.status.is_terminal() && (now >= deadline || now < run.claimed_at) {
            snapshot = self.scheduler.cancel(attempt.task_id).await?;
        }
        if matches!(snapshot.waiting, Some(WaitingReason::ProviderUnavailable))
            && matches!(
                snapshot.task.status,
                TaskStatus::WaitingForChildren | TaskStatus::Failed
            )
        {
            let id = attempt.occurrence_id;
            let number = attempt.attempt_number;
            let existing = self
                .work(move |repository, _| Ok(repository.factory_retirement(id, number)?))
                .await?;
            let intent = match existing {
                Some(intent) => Some(intent),
                None if snapshot.task.status == TaskStatus::WaitingForChildren
                    && now < deadline
                    && now >= run.claimed_at =>
                {
                    let captured = attempt.clone();
                    Some(
                        self.work(move |repository, _| {
                            Ok(repository.reserve_factory_retirement(&captured, now)?)
                        })
                        .await?,
                    )
                }
                None => None,
            };
            if let Some(intent) = intent {
                snapshot = self
                    .scheduler
                    .retire_provider_unavailable(
                        attempt.task_id,
                        attempt.input_command_id,
                        intent.command_id,
                    )
                    .await?;
                validate_snapshot(&attempt, &run, &snapshot)?;
                if snapshot.task.status != TaskStatus::Failed
                    || !matches!(snapshot.waiting, Some(WaitingReason::ProviderUnavailable))
                {
                    return Err(ScheduleError::Invalid(
                        "factory retirement returned an unexpected task boundary",
                    )
                    .into());
                }
                let settled = wall_now()?;
                let expired = settled >= deadline || settled < attempt.accepted_at;
                let outcome = if expired {
                    RunOutcome::Cancelled {
                        reason:
                            "scheduled execution deadline expired or wall clock moved backwards"
                                .into(),
                    }
                } else {
                    RunOutcome::Failed {
                        code: "provider_factory_unavailable".into(),
                        message: "Provider construction failed before native execution.".into(),
                    }
                };
                let kind = if expired {
                    AttemptResultKind::Terminal
                } else {
                    AttemptResultKind::FactoryFailure
                };
                self.work(move |repository, _| {
                    Ok(repository.settle_attempt(
                        &attempt,
                        kind,
                        AttemptUsage {
                            known: true,
                            ..AttemptUsage::default()
                        },
                        &outcome,
                        settled,
                    )?)
                })
                .await?;
                return Ok(());
            }
        }
        if snapshot.task.status.is_terminal() {
            let conversation = self.agent.conversation(attempt.conversation_id).await?;
            let native = conversation.native_run();
            let usage = native_usage(&attempt, native.as_ref())?;
            let task_id = attempt.task_id;
            let journal_unknown = self
                .work(move |repository, _| Ok(repository.terminal_journal_unknown(task_id)?))
                .await?;
            let outcome = if uncertain(snapshot.waiting.as_ref())
                || native.as_ref().is_some_and(native_outcome_unknown)
                || journal_unknown
            {
                RunOutcome::Unknown {
                    reason: "native scheduled task ended with uncertain effects; owner reconciliation required".into(),
                }
            } else {
                match snapshot.task.status {
                    TaskStatus::Completed => RunOutcome::Succeeded {
                        summary: outcome_preview(
                            snapshot.last_output.as_deref().unwrap_or_default(),
                        ),
                    },
                    TaskStatus::Cancelled => RunOutcome::Cancelled {
                        reason: "native scheduled task cancelled".into(),
                    },
                    TaskStatus::Failed => RunOutcome::Failed {
                        code: "native_task_failed".into(),
                        message: outcome_preview(
                            snapshot
                                .diagnostic
                                .as_deref()
                                .unwrap_or("native scheduled task failed"),
                        ),
                    },
                    _ => {
                        return Err(
                            ScheduleError::Invalid("unsupported terminal task status").into()
                        );
                    }
                }
            };
            self.work(move |repository, _| {
                Ok(repository.settle_attempt(
                    &attempt,
                    AttemptResultKind::Terminal,
                    usage,
                    &outcome,
                    now,
                )?)
            })
            .await?;
        }
        Ok(())
    }

    async fn process_deliveries(&mut self) -> Result<(), EngineError> {
        let now = wall_now()?;
        match self
            .work(move |repository, _| Ok(repository.prepare_terminal_deliveries(BATCH, now)?))
            .await
        {
            Ok(progress) if progress > 0 => self
                .status
                .send_modify(|status| status.delivery_preparation_error = None),
            Ok(_) => {}
            Err(EngineError::Cancelled) => return Err(EngineError::Cancelled),
            Err(error) => self.status.send_modify(|status| {
                status.delivery_preparation_error =
                    Some(error.to_string().chars().take(256).collect())
            }),
        }
        let cursor = self.delivery_cursor;
        let deliveries = self
            .work(move |repository, _| Ok(repository.pending_deliveries(cursor, BATCH)?))
            .await?;
        self.delivery_cursor = deliveries.last().map(|delivery| delivery.occurrence_id);
        self.status
            .send_modify(|status| status.delivery_error = None);
        for delivery in deliveries {
            self.check_stop()?;
            match self
                .agent
                .publish_scheduled_result(delivery.command.clone())
                .await
            {
                Ok(receipt) => {
                    self.work(move |repository, _| {
                        Ok(repository.acknowledge_delivery(&delivery, receipt.sequence)?)
                    })
                    .await?;
                }
                Err(error) => self.status.send_modify(|status| {
                    status.delivery_error = Some(error.to_string().chars().take(256).collect())
                }),
            }
        }
        let pending = self
            .work(|repository, _| Ok(repository.pending_delivery_count()?))
            .await?;
        self.status
            .send_modify(|status| status.pending_deliveries = pending);
        Ok(())
    }

    async fn finish(&self, run: &ScheduledRun, outcome: RunOutcome) -> Result<(), EngineError> {
        let id = run.occurrence_id;
        let revision = run.revision;
        let now = wall_now()?.max(run.updated_at);
        self.work(
            move |repository, _| match repository.finish_run(id, revision, &outcome, now) {
                Ok(_) => Ok(()),
                Err(
                    ScheduleError::RunStateConflict(_) | ScheduleError::RevisionConflict { .. },
                ) => {
                    let saved = repository
                        .run(id)?
                        .ok_or(ScheduleError::RunStateConflict(id))?;
                    if saved.outcome.as_ref() == Some(&outcome) {
                        Ok(())
                    } else {
                        Err(ScheduleError::RunStateConflict(id).into())
                    }
                }
                Err(error) => Err(error.into()),
            },
        )
        .await
    }

    fn check_stop(&self) -> Result<(), EngineError> {
        if self.stop.is_cancelled() {
            Err(EngineError::Cancelled)
        } else {
            Ok(())
        }
    }
    fn error(&self, error: &EngineError) {
        self.status.send_modify(|status| {
            status.last_error = Some(error.to_string().chars().take(256).collect())
        });
    }
    fn block(&mut self, id: ScheduledJobId, error: &EngineError) {
        self.error(error);
        self.blocked.insert(
            id,
            Blocked {
                until: Instant::now() + Duration::from_secs(5),
                diagnostic: error.to_string().chars().take(256).collect(),
            },
        );
    }
    fn publish(&self) {
        self.status.send_modify(|status| {
            status.blocked_jobs = self.blocked.len();
            status.blocked = self
                .blocked
                .iter()
                .take(64)
                .map(|(id, value)| BlockedSchedule {
                    job_id: *id,
                    diagnostic: value.diagnostic.clone(),
                })
                .collect();
        });
    }
}

fn submission(attempt: &ScheduledAttempt) -> TaskSubmission {
    TaskSubmission {
        task_id: attempt.task_id,
        conversation_id: attempt.conversation_id,
        command_id: attempt.input_command_id,
        accepted_at: attempt.accepted_at,
        execution: attempt.execution.clone(),
    }
}

fn validate_snapshot(
    attempt: &ScheduledAttempt,
    run: &ScheduledRun,
    snapshot: &TaskSnapshot,
) -> Result<(), EngineError> {
    if snapshot.task.id != attempt.task_id
        || snapshot.task.conversation_id != attempt.conversation_id
        || snapshot.task.originating_command_id != attempt.input_command_id
        || snapshot.task.parent_task_id.is_some()
        || snapshot.task.instruction != run.definition.target.instruction
        || snapshot.task.created_at != attempt.accepted_at
    {
        return Err(ScheduleError::Invalid(
            "native task differs from the immutable scheduled attempt",
        )
        .into());
    }
    Ok(())
}

fn outcome_preview(value: &str) -> String {
    let mut end = value.len().min(OUTCOME_PREVIEW_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].replace('\0', " ")
}

fn native_outcome_unknown(run: &NativeRunInfo) -> bool {
    matches!(
        run.status,
        LoopStatus::ProviderInFlight { .. }
            | LoopStatus::ExecutingTools
            | LoopStatus::Paused {
                reason: PauseReason::ReconciliationRequired { .. }
                    | PauseReason::BrokerRejected { .. }
                    | PauseReason::OperationInFlight { .. }
                    | PauseReason::ChildOutcomeUnknown { .. }
            }
    )
}

fn uncertain(reason: Option<&WaitingReason>) -> bool {
    matches!(
        reason,
        Some(
            WaitingReason::ProviderUncertain { .. }
                | WaitingReason::ChildOutcomeUnknown { .. }
                | WaitingReason::InFlight
                | WaitingReason::Reconciliation
        )
    )
}

fn plan(
    job: &ScheduledJob,
    now: UnixMillis,
    cancellation: &CancellationToken,
) -> Result<DueClaim, EngineError> {
    let due = job.next_run_at.ok_or(ScheduleError::NotDue)?;
    if due > now {
        return Err(ScheduleError::NotDue.into());
    }
    if latest_at_or_before(&job.definition.schedule, due, due, cancellation)? != Some(due) {
        return Err(ClockError::Blocked("stored due instant is not a matching occurrence").into());
    }
    let latest = latest_at_or_before(&job.definition.schedule, due, now, cancellation)?
        .ok_or(ClockError::Blocked("no matching latest due occurrence"))?;
    let lateness = now.get() - latest.get();
    let eligible = lateness <= NORMAL_LATENESS_MS
        || matches!(job.definition.policies.missed,MissedRunPolicy::Latest {within_ms} if lateness<=within_ms);
    let (disposition, next) = if !eligible {
        (
            DueDisposition::SkipMissedRange { through: latest },
            next_after(&job.definition.schedule, now, cancellation)?,
        )
    } else if latest > due {
        let before = UnixMillis::try_from(latest.get() - 1)
            .map_err(|_| ClockError::Invalid("latest predecessor underflow"))?;
        let through = latest_at_or_before(&job.definition.schedule, due, before, cancellation)?
            .ok_or(ClockError::Blocked(
                "missed prefix has no matching boundary",
            ))?;
        (DueDisposition::SkipMissedRange { through }, Some(latest))
    } else {
        (
            DueDisposition::Dispatch,
            next_after(&job.definition.schedule, latest, cancellation)?,
        )
    };
    Ok(DueClaim {
        job_id: job.id,
        expected_revision: job.revision,
        occurrence_id: OccurrenceId::new(),
        scheduled_for: due,
        next_run_at: next,
        disposition,
    })
}

fn wall_now() -> Result<UnixMillis, EngineError> {
    UnixMillis::now().map_err(|_| {
        ScheduleError::Invalid("system wall clock is outside scheduling bounds").into()
    })
}
