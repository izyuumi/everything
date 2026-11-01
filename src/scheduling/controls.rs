use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::permissions::{Authorizer, ExecutionAuthority, ExecutionMode, ToolVisibility};
use crate::protocol::{ConversationId, TaskId, UnixMillis};
use crate::scheduler::{SCHEDULER_TOOL_NAMES, ScheduleCallerContext, SchedulerError};
use crate::storage::{Store, ToolCallState};

use super::clock::next_after;
use super::engine::ScheduleEngine;
use super::execution::{AttemptUsage, ScheduledAttemptResult};
use super::{
    JobFilter, ManualClaim, NewScheduledJob, OccurrenceId, RunCursor, ScheduleDefinition,
    ScheduleError, ScheduleRepository, ScheduledJob, ScheduledJobEdit, ScheduledJobId,
    ScheduledRun, ScheduledTarget,
};

pub const SCHEDULE_CONTROL_NAMES: &[&str] = &[
    "schedule_create",
    "schedule_list",
    "schedule_show",
    "schedule_update",
    "schedule_disable",
    "schedule_delete",
    "schedule_run",
    "schedule_history",
];

pub const MAX_CONTROL_REPLY_BYTES: usize = 1024 * 1024;
const MAX_AGENT_REPLY_BYTES: usize = 28 * 1_024;
const MAX_CONCURRENT: u32 = 4;
const MAX_PAGE_SIZE: usize = 100;

#[derive(Debug, Error)]
pub enum ControlError {
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    Agents(#[from] crate::host::agents::AgentsError),
    #[error("invalid schedule control: {0}")]
    Invalid(&'static str),
    #[error("schedule control requires the actual leased host Agent")]
    LeaseRequired,
    #[error("schedule control is outside the caller's current authority")]
    OutsideScope,
    #[error("schedule control was cancelled before repository admission")]
    Cancelled,
    #[error("schedule control intake is stopped")]
    Stopped,
    #[error("schedule control admission is busy")]
    Busy,
    #[error("schedule control reply exceeds its bound; reduce the page size")]
    ReplyTooLarge,
    #[error("schedule control worker failed")]
    Worker,
    #[error("schedule mutation may have committed; inspect its stable identity: {0}")]
    OutcomeUnknown(String),
    #[error("schedule controls still own admitted work; retain host services and retry shutdown")]
    ShutdownPending,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleJobDraft {
    pub id: ScheduledJobId,
    pub name: String,
    pub definition: ScheduleDefinition,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleMutation {
    Create {
        job: Box<ScheduleJobDraft>,
    },
    Update {
        job: Box<ScheduleJobDraft>,
        expected_revision: u64,
    },
    Disable {
        job_id: ScheduledJobId,
        expected_revision: u64,
    },
    Delete {
        job_id: ScheduledJobId,
        expected_revision: u64,
    },
    Run {
        claim: ManualClaim,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleQuery {
    List {
        after_id: Option<ScheduledJobId>,
        enabled: Option<bool>,
        #[serde(default)]
        include_deleted: bool,
        limit: usize,
    },
    Show {
        job_id: ScheduledJobId,
    },
    History {
        job_id: ScheduledJobId,
        before: Option<RunCursor>,
        limit: usize,
    },
    Run {
        occurrence_id: OccurrenceId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleMutationResult {
    Job {
        job: Box<ScheduledJob>,
    },
    Run {
        run: Box<ScheduledRun>,
        replayed: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleAttemptInspection {
    pub attempt_number: u32,
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub accepted_at: UnixMillis,
    pub deadline: UnixMillis,
    pub result: Option<ScheduledAttemptResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleRunInspection {
    pub run: ScheduledRun,
    pub attempts: Vec<ScheduleAttemptInspection>,
    pub usage: AttemptUsage,
    pub outcome_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleQueryResult {
    Jobs {
        jobs: Vec<ScheduledJob>,
        next_after: Option<ScheduledJobId>,
    },
    Job {
        job: Box<ScheduledJob>,
    },
    History {
        runs: Vec<ScheduleRunInspection>,
        next_before: Option<RunCursor>,
    },
    Run {
        inspection: Box<ScheduleRunInspection>,
    },
}

pub struct ScheduleControls {
    repository: ScheduleRepository,
    engine: ScheduleEngine,
    authorizer: Arc<Authorizer>,
    admission: Arc<Semaphore>,
    closing: CancellationToken,
    agent: Agent,
}

enum Scope {
    Owner(Option<ExecutionAuthority>),
    Agent(Box<ScheduleCallerContext>),
}

impl ScheduleControls {
    pub fn new(
        agent: Agent,
        engine: ScheduleEngine,
        authorizer: Arc<Authorizer>,
    ) -> Result<Arc<Self>, ControlError> {
        if !leased_agent(&agent) {
            return Err(ControlError::LeaseRequired);
        }
        Ok(Arc::new(Self {
            repository: ScheduleRepository::new(agent.store()),
            agent,
            engine,
            authorizer,
            admission: Arc::new(Semaphore::new(MAX_CONCURRENT as usize)),
            closing: CancellationToken::new(),
        }))
    }

    pub fn close(&self) {
        self.closing.cancel();
    }

    pub async fn shutdown(&self) -> Result<(), ControlError> {
        self.close();
        let _admitted = tokio::time::timeout(
            Duration::from_secs(10),
            Arc::clone(&self.admission).acquire_many_owned(MAX_CONCURRENT),
        )
        .await
        .map_err(|_| ControlError::ShutdownPending)?
        .map_err(|_| ControlError::Stopped)?;
        Ok(())
    }

    pub async fn query_as_owner(
        &self,
        query: ScheduleQuery,
        cancellation: CancellationToken,
    ) -> Result<ScheduleQueryResult, ControlError> {
        self.query(query, Scope::Owner(None), cancellation).await
    }

    pub async fn mutate_as_owner(
        &self,
        mutation: ScheduleMutation,
        authority: ExecutionAuthority,
        cancellation: CancellationToken,
    ) -> Result<ScheduleMutationResult, ControlError> {
        self.mutate(mutation, Scope::Owner(Some(authority)), cancellation)
            .await
    }

    pub async fn query_as_agent(
        &self,
        query: ScheduleQuery,
        caller: ScheduleCallerContext,
        cancellation: CancellationToken,
    ) -> Result<ScheduleQueryResult, ControlError> {
        caller_tool(&caller, query_tool(&query))?;
        self.query(query, Scope::Agent(Box::new(caller)), cancellation)
            .await
    }

    pub async fn mutate_as_agent(
        &self,
        mutation: ScheduleMutation,
        caller: ScheduleCallerContext,
        cancellation: CancellationToken,
    ) -> Result<ScheduleMutationResult, ControlError> {
        caller_tool(&caller, mutation_tool(&mutation))?;
        if matches!(&mutation, ScheduleMutation::Create { job } if *job.id.as_uuid() != *caller.operation_id().as_uuid())
            || matches!(&mutation, ScheduleMutation::Run { claim } if claim.command_id != caller.operation_id() || *claim.occurrence_id.as_uuid() != *caller.operation_id().as_uuid())
        {
            return Err(ControlError::OutsideScope);
        }
        self.mutate(mutation, Scope::Agent(Box::new(caller)), cancellation)
            .await
    }

    async fn query(
        &self,
        query: ScheduleQuery,
        scope: Scope,
        cancellation: CancellationToken,
    ) -> Result<ScheduleQueryResult, ControlError> {
        self.run(
            scope,
            cancellation,
            false,
            move |repository, scope, authorizer, _| {
                let maximum = scope.reply_limit();
                let result = match query {
                    ScheduleQuery::List {
                        after_id,
                        enabled,
                        include_deleted,
                        limit,
                    } => {
                        validate_limit(limit)?;
                        let (page, requested) = job_page(
                            repository,
                            JobFilter {
                                after_id,
                                enabled,
                                include_deleted,
                            },
                            limit,
                        )?;
                        let full_page = page.len() == requested;
                        let mut after = after_id;
                        let mut more = full_page;
                        let mut bytes = 4096;
                        let mut jobs = Vec::new();
                        for job in page {
                            match scope.authorize(&job.definition.target, authorizer) {
                                Ok(()) => {
                                    let size = encoded_size(&job)? + 1;
                                    if size > maximum.saturating_sub(bytes) {
                                        if jobs.is_empty() {
                                            return Err(ControlError::ReplyTooLarge);
                                        }
                                        more = true;
                                        break;
                                    }
                                    bytes += size;
                                    after = Some(job.id);
                                    jobs.push(job);
                                }
                                Err(ControlError::OutsideScope) => {
                                    after = Some(job.id);
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        let next_after = if more { after } else { None };
                        ScheduleQueryResult::Jobs { jobs, next_after }
                    }
                    ScheduleQuery::Show { job_id } => {
                        let job = required_job(repository, job_id)?;
                        scope.authorize(&job.definition.target, authorizer)?;
                        ScheduleQueryResult::Job { job: Box::new(job) }
                    }
                    ScheduleQuery::History {
                        job_id,
                        before,
                        limit,
                    } => {
                        validate_limit(limit)?;
                        let job = required_job(repository, job_id)?;
                        scope.authorize(&job.definition.target, authorizer)?;
                        let (page, requested) = history_page(repository, job_id, before, limit)?;
                        let mut more = page.len() == requested;
                        let mut cursor = before;
                        let mut bytes = 4096;
                        let mut runs = Vec::new();
                        for run in page {
                            let next = Some(RunCursor {
                                claimed_at: run.claimed_at,
                                occurrence_id: run.occurrence_id,
                            });
                            match scope.authorize(&run.definition.target, authorizer) {
                                Ok(()) => {
                                    let inspection = inspect_run(repository, run)?;
                                    let size = encoded_size(&inspection)? + 1;
                                    if size > maximum.saturating_sub(bytes) {
                                        if runs.is_empty() {
                                            return Err(ControlError::ReplyTooLarge);
                                        }
                                        more = true;
                                        break;
                                    }
                                    bytes += size;
                                    cursor = next;
                                    runs.push(inspection);
                                }
                                Err(ControlError::OutsideScope) => {
                                    cursor = next;
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        let next_before = if more { cursor } else { None };
                        ScheduleQueryResult::History { runs, next_before }
                    }
                    ScheduleQuery::Run { occurrence_id } => {
                        let run = repository.run(occurrence_id)?.ok_or_else(|| {
                            ScheduleError::NotFound {
                                entity: "run",
                                id: occurrence_id.to_string(),
                            }
                        })?;
                        let job = required_job(repository, run.job_id)?;
                        scope.authorize(&job.definition.target, authorizer)?;
                        scope.authorize(&run.definition.target, authorizer)?;
                        ScheduleQueryResult::Run {
                            inspection: Box::new(inspect_run(repository, run)?),
                        }
                    }
                };
                check_reply(&result, maximum)?;
                Ok(result)
            },
        )
        .await
    }

    async fn mutate(
        &self,
        mutation: ScheduleMutation,
        scope: Scope,
        cancellation: CancellationToken,
    ) -> Result<ScheduleMutationResult, ControlError> {
        let agents = crate::host::agents::AgentRepository::for_agent(&self.agent)?;
        self.run(
            scope,
            cancellation,
            true,
            move |repository, scope, authorizer, token| {
                let now = UnixMillis::now().map_err(SchedulerError::from)?;
                let prepared = match mutation {
                    ScheduleMutation::Create { job } => {
                        validate_draft(&job)?;
                        validate_agent(&agents, &job)?;
                        scope.authorize(&job.definition.target, authorizer)?;
                        let next = next_run(&job, None, now, token)?;
                        PreparedMutation::Create(new_job(*job, next))
                    }
                    ScheduleMutation::Update {
                        mut job,
                        expected_revision,
                    } => {
                        validate_draft(&job)?;
                        let old = required_job(repository, job.id)?;
                        if matches!(scope, Scope::Agent(_)) && job.definition.target.agent.is_none()
                        {
                            job.definition.target.agent = old.definition.target.agent;
                        }
                        validate_agent(&agents, &job)?;
                        check_revision(old.revision, expected_revision)?;
                        scope.authorize_existing(&old.definition.target, authorizer)?;
                        scope.authorize(&job.definition.target, authorizer)?;
                        let next = next_run(&job, Some(&old), now, token)?;
                        PreparedMutation::Update(ScheduledJobEdit {
                            job: new_job(*job, next),
                            expected_revision,
                        })
                    }
                    ScheduleMutation::Disable {
                        job_id,
                        expected_revision,
                    } => {
                        let job = required_job(repository, job_id)?;
                        check_revision(job.revision, expected_revision)?;
                        scope.authorize_existing(&job.definition.target, authorizer)?;
                        PreparedMutation::Disable {
                            job,
                            expected_revision,
                        }
                    }
                    ScheduleMutation::Delete {
                        job_id,
                        expected_revision,
                    } => {
                        let job = required_job(repository, job_id)?;
                        check_revision(job.revision, expected_revision)?;
                        scope.authorize_existing(&job.definition.target, authorizer)?;
                        PreparedMutation::Delete {
                            job_id,
                            expected_revision,
                        }
                    }
                    ScheduleMutation::Run { claim } => {
                        let job = required_job(repository, claim.job_id)?;
                        if let Some(run) = repository.run(claim.occurrence_id)? {
                            scope.authorize_existing(&job.definition.target, authorizer)?;
                            scope.authorize_existing(&run.definition.target, authorizer)?;
                        } else {
                            check_revision(job.revision, claim.expected_revision)?;
                            scope.authorize(&job.definition.target, authorizer)?;
                        }
                        PreparedMutation::Run(claim)
                    }
                };
                scope.check_live(&repository.store)?;
                if token.is_cancelled() {
                    return Err(ControlError::Cancelled);
                }
                let authority = scope.authority().ok_or(ControlError::OutsideScope)?;
                let result = match prepared {
                    PreparedMutation::Create(job) => repository
                        .create(&job, authority, now)
                        .map(|job| ScheduleMutationResult::Job { job: Box::new(job) }),
                    PreparedMutation::Update(edit) => repository
                        .edit(&edit, authority, now)
                        .map(|job| ScheduleMutationResult::Job { job: Box::new(job) }),
                    PreparedMutation::Disable {
                        job,
                        expected_revision,
                    } => repository
                        .set_enabled(job.id, expected_revision, false, job.next_run_at, now)
                        .map(|job| ScheduleMutationResult::Job { job: Box::new(job) }),
                    PreparedMutation::Delete {
                        job_id,
                        expected_revision,
                    } => repository
                        .delete(job_id, expected_revision, now)
                        .map(|job| ScheduleMutationResult::Job { job: Box::new(job) }),
                    PreparedMutation::Run(claim) => repository
                        .claim_manual(&claim, authority, now)
                        .map(|outcome| ScheduleMutationResult::Run {
                            run: Box::new(outcome.run),
                            replayed: outcome.replayed,
                        }),
                }
                .map_err(mutation_error)?;
                check_reply(&result, MAX_CONTROL_REPLY_BYTES).map_err(|_| {
                    ControlError::OutcomeUnknown(
                        "mutation committed but its reply exceeded the bound".into(),
                    )
                })?;
                Ok(result)
            },
        )
        .await
    }

    async fn run<R: Send + 'static>(
        &self,
        scope: Scope,
        cancellation: CancellationToken,
        mutating: bool,
        operation: impl FnOnce(
            &ScheduleRepository,
            &Scope,
            &Authorizer,
            &CancellationToken,
        ) -> Result<R, ControlError>
        + Send
        + 'static,
    ) -> Result<R, ControlError> {
        let token = cancellation.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        let active = scope.cancellation();
        let permit = self.permit(&token, &active, mutating).await?;
        let agent = self.agent.clone();
        let repository = self.repository.clone();
        let engine = self.engine.clone();
        let authorizer = Arc::clone(&self.authorizer);
        let closing = self.closing.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let resources = (repository, authorizer, agent);
            if token.is_cancelled() || active.is_cancelled() {
                return Err(ControlError::Cancelled);
            }
            if mutating && closing.is_cancelled() {
                return Err(ControlError::Stopped);
            }
            scope.check_live(&resources.0.store)?;
            let result = operation(&resources.0, &scope, &resources.1, &token);
            if mutating && result.is_ok() {
                engine.wake();
            }
            result
        })
        .await
        .map_err(|_| {
            if mutating {
                ControlError::OutcomeUnknown(
                    "admitted worker stopped before returning its result".into(),
                )
            } else {
                ControlError::Worker
            }
        })?
    }

    async fn permit(
        &self,
        cancellation: &CancellationToken,
        active: &CancellationToken,
        mutating: bool,
    ) -> Result<OwnedSemaphorePermit, ControlError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ControlError::Cancelled),
            _ = active.cancelled() => Err(ControlError::Cancelled),
            _ = self.closing.cancelled(), if mutating => Err(ControlError::Stopped),
            result = tokio::time::timeout(Duration::from_secs(5), Arc::clone(&self.admission).acquire_owned()) => result.map_err(|_| ControlError::Busy)?.map_err(|_| ControlError::Stopped),
        }
    }
}

impl Drop for ScheduleControls {
    fn drop(&mut self) {
        self.closing.cancel();
    }
}

impl Scope {
    fn reply_limit(&self) -> usize {
        match self {
            Self::Owner(_) => MAX_CONTROL_REPLY_BYTES,
            Self::Agent(_) => MAX_AGENT_REPLY_BYTES,
        }
    }

    fn authority(&self) -> Option<&ExecutionAuthority> {
        match self {
            Self::Owner(authority) => authority.as_ref(),
            Self::Agent(caller) => Some(caller.authority()),
        }
    }

    fn cancellation(&self) -> CancellationToken {
        match self {
            Self::Owner(_) => CancellationToken::new(),
            Self::Agent(caller) => caller.cancellation(),
        }
    }

    fn check_live(&self, store: &Store) -> Result<(), ControlError> {
        let Self::Agent(caller) = self else {
            return Ok(());
        };
        if caller.cancellation().is_cancelled() {
            return Err(ControlError::Cancelled);
        }
        let row = store
            .tool_call(&caller.operation_id().to_string())
            .map_err(ScheduleError::from)?
            .ok_or(ControlError::OutsideScope)?;
        if row.state != ToolCallState::Running
            || row.task_id.as_deref() != Some(caller.authority().task_id())
            || row.conversation_id != caller.authority().conversation_id()
            || row.tool_name != caller.tool_name()
        {
            return Err(ControlError::OutsideScope);
        }
        if caller.cancellation().is_cancelled() {
            return Err(ControlError::Cancelled);
        }
        Ok(())
    }

    fn authorize_existing(
        &self,
        target: &ScheduledTarget,
        authorizer: &Authorizer,
    ) -> Result<(), ControlError> {
        match self {
            Self::Owner(_) => Ok(()),
            Self::Agent(_) => self.authorize(target, authorizer),
        }
    }

    fn authorize(
        &self,
        target: &ScheduledTarget,
        authorizer: &Authorizer,
    ) -> Result<(), ControlError> {
        let Some(authority) = self.authority() else {
            return Ok(());
        };
        if matches!(self, Self::Agent(caller) if target.configuration != *caller.configuration()) {
            return Err(ControlError::OutsideScope);
        }
        target
            .validate_authority(authority)
            .map_err(|_| ControlError::OutsideScope)?;
        let target_authority = ExecutionAuthority::new(
            authority.conversation_id(),
            authority.task_id(),
            target.workspace_root.clone(),
            target.permissions.mode,
        )
        .and_then(|authority| authority.restrict_tools(target.permissions.allowed_tools.clone()))
        .map_err(|_| ControlError::OutsideScope)?;
        for name in &target.permissions.allowed_tools {
            match authorizer.tool_visibility(&target_authority, name) {
                Ok(ToolVisibility::Available | ToolVisibility::RequiresApproval) => {}
                Ok(ToolVisibility::Denied { .. }) | Err(_) => {
                    return Err(ControlError::OutsideScope);
                }
            }
        }
        Ok(())
    }
}

enum PreparedMutation {
    Create(NewScheduledJob),
    Update(ScheduledJobEdit),
    Disable {
        job: ScheduledJob,
        expected_revision: u64,
    },
    Delete {
        job_id: ScheduledJobId,
        expected_revision: u64,
    },
    Run(ManualClaim),
}

fn caller_tool(caller: &ScheduleCallerContext, expected: &str) -> Result<(), ControlError> {
    if caller.tool_name() == expected
        && caller.authority().mode() == ExecutionMode::FullHost
        && caller.authority().permits_tool(expected)
    {
        Ok(())
    } else {
        Err(ControlError::OutsideScope)
    }
}

fn query_tool(query: &ScheduleQuery) -> &str {
    match query {
        ScheduleQuery::List { .. } => "schedule_list",
        ScheduleQuery::Show { .. } => "schedule_show",
        ScheduleQuery::History { .. } | ScheduleQuery::Run { .. } => "schedule_history",
    }
}

fn mutation_tool(mutation: &ScheduleMutation) -> &str {
    match mutation {
        ScheduleMutation::Create { .. } => "schedule_create",
        ScheduleMutation::Update { .. } => "schedule_update",
        ScheduleMutation::Disable { .. } => "schedule_disable",
        ScheduleMutation::Delete { .. } => "schedule_delete",
        ScheduleMutation::Run { .. } => "schedule_run",
    }
}

fn required_job(
    repository: &ScheduleRepository,
    id: ScheduledJobId,
) -> Result<ScheduledJob, ControlError> {
    repository.job(id)?.ok_or_else(|| {
        ScheduleError::NotFound {
            entity: "job",
            id: id.to_string(),
        }
        .into()
    })
}

fn check_revision(actual: u64, expected: u64) -> Result<(), ControlError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ScheduleError::RevisionConflict { expected, actual }.into())
    }
}

fn validate_limit(limit: usize) -> Result<(), ControlError> {
    if (1..=MAX_PAGE_SIZE).contains(&limit) {
        Ok(())
    } else {
        Err(ControlError::Invalid(
            "page limit must be between one and 100",
        ))
    }
}

fn validate_agent(
    agents: &crate::host::agents::AgentRepository,
    job: &ScheduleJobDraft,
) -> Result<(), ControlError> {
    if let Some(id) = job.definition.target.agent {
        agents.snapshot()?.resolve(Some(id))?;
    }
    Ok(())
}

fn validate_draft(job: &ScheduleJobDraft) -> Result<(), ControlError> {
    job.definition.validate()?;
    if job.name.trim().is_empty() || job.name.len() > 256 || job.name.chars().any(char::is_control)
    {
        return Err(ControlError::Invalid(
            "job name requires one to 256 bytes without controls",
        ));
    }
    if job
        .definition
        .target
        .permissions
        .allowed_tools
        .iter()
        .any(|name| {
            SCHEDULE_CONTROL_NAMES.contains(&name.as_str())
                || SCHEDULER_TOOL_NAMES.contains(&name.as_str())
        })
    {
        return Err(ControlError::Invalid(
            "native task and schedule controls are unavailable in scheduled profiles",
        ));
    }
    if serde_json::to_vec(&job.definition)
        .map_err(ScheduleError::from)?
        .len()
        > super::MAX_SCHEDULE_DEFINITION_BYTES
    {
        return Err(ScheduleError::Capacity("schedule definition bytes").into());
    }
    Ok(())
}

fn next_run(
    job: &ScheduleJobDraft,
    old: Option<&ScheduledJob>,
    now: UnixMillis,
    cancellation: &CancellationToken,
) -> Result<Option<UnixMillis>, ControlError> {
    if let Some(old) = old.filter(|old| {
        old.enabled == job.enabled && old.definition.schedule == job.definition.schedule
    }) {
        return Ok(old.next_run_at);
    }
    if !job.enabled {
        return Ok(None);
    }
    let after = old.map_or(now, |old| now.max(old.updated_at));
    match next_after(&job.definition.schedule, after, cancellation) {
        Ok(Some(next)) => Ok(Some(next)),
        Ok(None) => Err(ControlError::Invalid(
            "enabled schedule has no future occurrence",
        )),
        Err(super::clock::ClockError::Cancelled) => Err(ControlError::Cancelled),
        Err(
            super::clock::ClockError::Invalid(message) | super::clock::ClockError::Blocked(message),
        ) => Err(ControlError::Invalid(message)),
    }
}

fn new_job(job: ScheduleJobDraft, next_run_at: Option<UnixMillis>) -> NewScheduledJob {
    NewScheduledJob {
        id: job.id,
        name: job.name,
        definition: job.definition,
        enabled: job.enabled,
        next_run_at,
    }
}

fn inspect_run(
    repository: &ScheduleRepository,
    run: ScheduledRun,
) -> Result<ScheduleRunInspection, ControlError> {
    let attempts = repository
        .attempt_history(run.occurrence_id)?
        .into_iter()
        .map(|attempt| {
            Ok(ScheduleAttemptInspection {
                attempt_number: attempt.attempt_number,
                task_id: attempt.task_id,
                conversation_id: attempt.conversation_id,
                accepted_at: attempt.accepted_at,
                deadline: attempt.deadline,
                result: repository.attempt_result(run.occurrence_id, attempt.attempt_number)?,
            })
        })
        .collect::<Result<Vec<_>, ScheduleError>>()?;
    let usage = repository.cumulative_usage(run.occurrence_id)?;
    let outcome_digest = if run.outcome.is_some() {
        Some(repository.terminal_outcome_digest(run.occurrence_id)?)
    } else {
        None
    };
    Ok(ScheduleRunInspection {
        run,
        attempts,
        usage,
        outcome_digest,
    })
}

fn check_reply(value: &impl Serialize, maximum: usize) -> Result<(), ControlError> {
    if encoded_size(value)? > maximum {
        Err(ControlError::ReplyTooLarge)
    } else {
        Ok(())
    }
}

fn encoded_size(value: &impl Serialize) -> Result<usize, ControlError> {
    Ok(serde_json::to_vec(value)
        .map_err(ScheduleError::from)?
        .len())
}

fn job_page(
    repository: &ScheduleRepository,
    filter: JobFilter,
    mut limit: usize,
) -> Result<(Vec<ScheduledJob>, usize), ControlError> {
    loop {
        match repository.jobs(filter, limit) {
            Ok(page) => return Ok((page, limit)),
            Err(ScheduleError::ReadBudgetExceeded) if limit > 1 => limit /= 2,
            Err(error) => return Err(error.into()),
        }
    }
}

fn history_page(
    repository: &ScheduleRepository,
    job_id: ScheduledJobId,
    before: Option<RunCursor>,
    mut limit: usize,
) -> Result<(Vec<ScheduledRun>, usize), ControlError> {
    loop {
        match repository.history(job_id, before, limit) {
            Ok(page) => return Ok((page, limit)),
            Err(ScheduleError::ReadBudgetExceeded) if limit > 1 => limit /= 2,
            Err(error) => return Err(error.into()),
        }
    }
}

fn mutation_error(error: ScheduleError) -> ControlError {
    match error {
        ScheduleError::Storage(_) | ScheduleError::Sqlite(_) | ScheduleError::Json(_) => {
            let text = error.to_string();
            let mut end = text.len().min(2048);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            ControlError::OutcomeUnknown(text[..end].into())
        }
        error => error.into(),
    }
}

fn leased_agent(agent: &Agent) -> bool {
    #[cfg(unix)]
    {
        agent.state_directory().is_some()
    }
    #[cfg(not(unix))]
    {
        let _ = agent;
        false
    }
}
