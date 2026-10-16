use serde::{Deserialize, Serialize};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::agent::{
    Agent, AgentError, ConversationConfig, LoopLimits, RemoteResultInspection,
    VerifiedRemoteOutcome,
};
use crate::crypto::DevicePublicKeys;
use crate::permissions::ExecutionMode;
use crate::protocol::{
    CommandId, ConversationId, DeviceId, RemoteResultSource, RemoteTaskOutcome, TaskFailure,
    TaskId, TaskStatus, UnixMillis,
};
use crate::relay_protocol::RelayOrigin;

use super::types::{ExecutionPolicy, SchedulerError, StoredTask, TaskSnapshot, TaskSubmission};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTaskBinding {
    pub delegation_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: String,
    pub coordinator: DeviceId,
    pub executor: DevicePublicKeys,
    pub relay_origin: RelayOrigin,
    pub worker_task_id: TaskId,
    pub worker_conversation_id: ConversationId,
    pub worker_create_command_id: CommandId,
    pub worker_input_command_id: CommandId,
    pub created_at: UnixMillis,
    pub deadline: UnixMillis,
}

impl RemoteTaskBinding {
    pub fn validate(&self) -> Result<(), SchedulerError> {
        if self.delegation_id.is_nil()
            || self.attempt_id.is_nil()
            || self.coordinator == self.executor.device_id
            || self.worker_create_command_id == self.worker_input_command_id
            || self.manifest_digest.len() != 64
            || !self
                .manifest_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.deadline.get() <= self.created_at.get()
        {
            return Err(SchedulerError::Invalid("invalid remote task binding"));
        }
        self.executor
            .validate()
            .map_err(|_| SchedulerError::Invalid("invalid remote executor keys"))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteOwnerApproval {
    pub command_id: CommandId,
    pub approved_at: UnixMillis,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTaskSubmission {
    pub task: TaskSubmission,
    pub binding: RemoteTaskBinding,
    pub approval: RemoteOwnerApproval,
}

pub struct RemoteTaskAdmission {
    submission: RemoteTaskSubmission,
    configuration: ConversationConfig,
    instruction: String,
    approval_deadline: tokio::time::Instant,
    _endpoint: Box<dyn Send + Sync>,
    _host: Box<dyn Send + Sync>,
    owner: Agent,
}

impl RemoteTaskAdmission {
    pub(crate) fn from_staged(
        owner: Agent,
        submission: RemoteTaskSubmission,
        configuration: ConversationConfig,
        instruction: String,
        approval_deadline: tokio::time::Instant,
        endpoint: Box<dyn Send + Sync>,
        host: Box<dyn Send + Sync>,
    ) -> Result<Self, SchedulerError> {
        if owner.state_directory().is_none() {
            return Err(SchedulerError::Invalid(
                "remote admission requires a leased Agent",
            ));
        }
        submission.binding.validate()?;
        configuration.validate()?;
        if instruction.trim().is_empty()
            || instruction.len() > 131_072
            || instruction.contains('\0')
        {
            return Err(SchedulerError::Invalid(
                "invalid original remote instruction",
            ));
        }
        let admission = Self {
            submission,
            configuration,
            instruction,
            approval_deadline,
            _endpoint: endpoint,
            _host: host,
            owner,
        };
        admission.validate_fresh(&admission.owner.store())?;
        Ok(admission)
    }

    pub fn submission(&self) -> &RemoteTaskSubmission {
        &self.submission
    }

    pub(super) fn configuration(&self) -> &ConversationConfig {
        &self.configuration
    }

    pub(super) fn instruction(&self) -> &str {
        &self.instruction
    }

    pub(super) fn validate_owner(
        &self,
        store: &crate::storage::Store,
    ) -> Result<(), SchedulerError> {
        if self.owner.state_directory().is_none() || !store.shares_connection(&self.owner.store()) {
            return Err(SchedulerError::Invalid(
                "remote admission belongs to another leased Store",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_fresh(
        &self,
        store: &crate::storage::Store,
    ) -> Result<(), SchedulerError> {
        self.validate_owner(store)?;
        let now = UnixMillis::now()?;
        let approval = &self.submission.approval;
        if now < approval.approved_at
            || now >= approval.expires_at
            || now >= self.submission.binding.deadline
            || self.submission.task.accepted_at > approval.approved_at
            || tokio::time::Instant::now() >= self.approval_deadline
        {
            return Err(SchedulerError::OwnerDecision(self.submission.task.task_id));
        }
        Ok(())
    }

    pub(super) fn validate_task(&self, task: &StoredTask) -> Result<(), SchedulerError> {
        let initial = &task.definition.initial;
        let expected = &self.submission.task;
        let definition = task
            .definition
            .remote
            .as_ref()
            .ok_or(SchedulerError::Invalid("remote definition is missing"))?;
        if initial.id != expected.task_id
            || initial.conversation_id != expected.conversation_id
            || initial.originating_command_id != expected.command_id
            || initial.created_at != expected.accepted_at
            || initial.parent_task_id.is_some()
            || initial.instruction != self.instruction
            || task.definition.configuration != self.configuration
            || super::repository::encode(&task.definition.execution)?
                != super::repository::encode(&expected.execution)?
            || definition.binding != self.submission.binding
            || definition.approval != self.submission.approval
        {
            return Err(SchedulerError::Conflict(expected.task_id));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteTaskPhase {
    Reserved,
    CancellationRequested,
    OutcomeUnknown,
    Settled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteBudgetCharge {
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reserved_output_tokens: u64,
}

impl RemoteBudgetCharge {
    pub(super) fn from_limits(limits: &LoopLimits) -> Result<Self, SchedulerError> {
        Ok(Self {
            model_attempts: limits.max_model_attempts,
            tool_calls: limits.max_tool_calls,
            charged_tokens: limits.max_charged_tokens,
            reserved_output_tokens: limits.max_reserved_output_tokens.ok_or(
                SchedulerError::Invalid("remote output allowance is required"),
            )?,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTaskSnapshot {
    pub task: TaskSnapshot,
    pub binding: RemoteTaskBinding,
    pub approval: RemoteOwnerApproval,
    pub configuration: ConversationConfig,
    pub execution: ExecutionPolicy,
    pub charge: RemoteBudgetCharge,
    pub phase: RemoteTaskPhase,
    pub admitted_at: UnixMillis,
    pub last_dispatch_at: Option<UnixMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RemoteResultInspection>,
}

pub struct RemoteDispatchPermit {
    snapshot: RemoteTaskSnapshot,
    _admission: OwnedMutexGuard<()>,
    _native: Box<dyn Send + Sync>,
    _agent: Agent,
}

impl RemoteDispatchPermit {
    pub fn snapshot(&self) -> &RemoteTaskSnapshot {
        &self.snapshot
    }

    pub(super) fn new(
        snapshot: RemoteTaskSnapshot,
        admission: OwnedMutexGuard<()>,
        native: Box<dyn Send + Sync>,
        agent: Agent,
    ) -> Self {
        Self {
            snapshot,
            _admission: admission,
            _native: native,
            _agent: agent,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoteDefinition {
    pub version: u16,
    pub binding: RemoteTaskBinding,
    pub approval: RemoteOwnerApproval,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoteCheckpoint {
    pub charge: RemoteBudgetCharge,
    pub phase: RemoteTaskPhase,
    pub admitted_at: UnixMillis,
    pub last_dispatch_at: Option<UnixMillis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RemoteResultInspection>,
}

pub(super) fn validate_definition(
    remote: &RemoteDefinition,
    execution: &ExecutionPolicy,
    configuration: &ConversationConfig,
) -> Result<(), SchedulerError> {
    remote.binding.validate()?;
    execution.validate()?;
    configuration.validate()?;
    let binding = &remote.binding;
    let approval = &remote.approval;
    let allowed = execution
        .allowed_tools
        .as_ref()
        .ok_or(SchedulerError::Invalid("remote policy was not captured"))?;
    if remote.version != 1
        || execution.mode == ExecutionMode::FullHost
        || allowed.iter().any(|tool| {
            !matches!(
                tool.as_str(),
                "read_file" | "write_file" | "search_files" | "search_tools"
            )
        })
        || execution.mode == ExecutionMode::ReadOnly && allowed.contains("write_file")
        || !execution.tools_enabled && !allowed.is_empty()
        || execution.limits.max_model_attempts > 32
        || execution.limits.max_tool_calls > 64
        || execution.limits.max_charged_tokens > 1_000_000
        || execution
            .limits
            .max_reserved_output_tokens
            .is_none_or(|tokens| {
                tokens < u64::from(configuration.max_output_tokens) || tokens > 1_000_000
            })
        || execution.limits.absolute_deadline != Some(binding.deadline)
        || binding.deadline.get() - binding.created_at.get() > execution.limits.max_duration_ms
        || execution.budget.max_context_tokens > 128_000
        || execution.budget.max_request_bytes > 32 * 1024 * 1024
        || execution.budget.reserved_output_tokens != configuration.max_output_tokens
        || approval.approved_at.get() < binding.created_at.get()
        || approval.expires_at.get() <= approval.approved_at.get()
        || approval.expires_at.get() - approval.approved_at.get() > 300_000
        || approval.expires_at.get() > binding.deadline.get()
    {
        return Err(SchedulerError::Invalid(
            "unsupported remote root task policy",
        ));
    }
    Ok(())
}

pub(super) fn validate_task(task: &StoredTask) -> Result<(), SchedulerError> {
    match (&task.definition.remote, &task.checkpoint.remote) {
        (None, None) => Ok(()),
        (Some(remote), Some(checkpoint)) => {
            validate_definition(
                remote,
                &task.definition.execution,
                &task.definition.configuration,
            )?;
            if task.definition.initial.parent_task_id.is_some()
                || task.checkpoint.active_command != task.definition.initial.originating_command_id
                || task.definition.initial.created_at.get() > remote.approval.approved_at.get()
                || !task.checkpoint.queued.is_empty()
                || !task.checkpoint.resumes.is_empty()
                || task.checkpoint.child_wait.is_some()
                || task.checkpoint.provider_retirement.is_some()
                || checkpoint.charge
                    != RemoteBudgetCharge::from_limits(&task.definition.execution.limits)?
                || checkpoint.admitted_at.get() < remote.approval.approved_at.get()
                || checkpoint.admitted_at.get() >= remote.approval.expires_at.get()
                || checkpoint.last_dispatch_at.is_some_and(|at| {
                    at.get() < checkpoint.admitted_at.get()
                        || at.get() >= remote.binding.deadline.get()
                        || at > task.checkpoint.task.updated_at
                })
            {
                return Err(SchedulerError::Invalid("invalid remote task checkpoint"));
            }
            validate_result_state(task, remote, checkpoint)?;
            Ok(())
        }
        _ => Err(SchedulerError::Invalid("incomplete remote task ownership")),
    }
}

pub(super) fn snapshot(task: &StoredTask) -> Result<RemoteTaskSnapshot, SchedulerError> {
    validate_task(task)?;
    let definition = task
        .definition
        .remote
        .as_ref()
        .ok_or(SchedulerError::NotOwned(task.definition.initial.id))?;
    let checkpoint = task
        .checkpoint
        .remote
        .as_ref()
        .ok_or(SchedulerError::Invalid("remote checkpoint is missing"))?;
    Ok(RemoteTaskSnapshot {
        task: task.snapshot(),
        binding: definition.binding.clone(),
        approval: definition.approval.clone(),
        configuration: task.definition.configuration.clone(),
        execution: task.definition.execution.clone(),
        charge: checkpoint.charge.clone(),
        phase: checkpoint.phase,
        admitted_at: checkpoint.admitted_at,
        last_dispatch_at: checkpoint.last_dispatch_at,
        result: checkpoint.result.clone(),
    })
}

fn validate_result_state(
    task: &StoredTask,
    definition: &RemoteDefinition,
    checkpoint: &RemoteCheckpoint,
) -> Result<(), SchedulerError> {
    let waiting = matches!(task.checkpoint.waiting,
        Some(super::types::WaitingReason::Remote { phase }) if phase == checkpoint.phase);
    match &checkpoint.result {
        None => {
            if task.checkpoint.task.status != TaskStatus::WaitingForChildren
                || !waiting
                || !matches!(
                    checkpoint.phase,
                    RemoteTaskPhase::Reserved | RemoteTaskPhase::CancellationRequested
                )
                || task.checkpoint.cancel_requested
                    != (checkpoint.phase == RemoteTaskPhase::CancellationRequested)
                || task.checkpoint.last_output.is_some()
                || task.checkpoint.task.failure.is_some()
            {
                return Err(SchedulerError::Invalid(
                    "invalid unresolved remote reservation",
                ));
            }
        }
        Some(result) => {
            result.validate(task.definition.initial.conversation_id)?;
            validate_report(task, &definition.binding, &result.report)?;
            if result.report.accepted_at < checkpoint.admitted_at
                || result.report.accepted_at > task.checkpoint.task.updated_at
                || result.cancellation_requested && !task.checkpoint.cancel_requested
                || task.checkpoint.last_output.as_deref() != Some(result.report.summary.as_str())
                || result.task_status != task.checkpoint.task.status
                || task.checkpoint.task.failure != result_failure(result)
            {
                return Err(SchedulerError::Invalid(
                    "remote result receipt differs from task",
                ));
            }
            if result.report.outcome == RemoteTaskOutcome::Unknown {
                if checkpoint.phase != RemoteTaskPhase::OutcomeUnknown
                    || !waiting
                    || task.checkpoint.task.status != TaskStatus::WaitingForChildren
                {
                    return Err(SchedulerError::Invalid(
                        "unknown remote outcome lost its reservation",
                    ));
                }
            } else if checkpoint.phase != RemoteTaskPhase::Settled
                || !task.checkpoint.task.status.is_terminal()
                || task.checkpoint.waiting.is_some()
                || task.checkpoint.task.updated_at != result.report.accepted_at
                || task.checkpoint.cancel_requested != result.cancellation_requested
            {
                return Err(SchedulerError::Invalid("invalid settled remote task"));
            }
        }
    }
    Ok(())
}

fn validate_report(
    task: &StoredTask,
    binding: &RemoteTaskBinding,
    report: &crate::protocol::RemoteTaskReport,
) -> Result<(), SchedulerError> {
    report.validate()?;
    if report.task_id != task.definition.initial.id
        || report.command_id != task.checkpoint.active_command
        || report.delegation_id != binding.delegation_id
        || report.attempt_id != binding.attempt_id
        || report.manifest_digest != binding.manifest_digest
        || report.executor != binding.executor.device_id
        || report.executor_fingerprint != binding.executor.fingerprint()
        || report.finished_at.is_some_and(|at| at < binding.created_at)
        || report.usage.model_attempts > task.definition.execution.limits.max_model_attempts
        || report.usage.tool_calls > task.definition.execution.limits.max_tool_calls
        || matches!(
            report.outcome,
            RemoteTaskOutcome::Succeeded | RemoteTaskOutcome::Failed | RemoteTaskOutcome::Cancelled
        ) && !matches!(&report.source, RemoteResultSource::Result {result_digest} if report.quarantine_digest.as_ref() == Some(result_digest))
    {
        return Err(SchedulerError::Conflict(task.definition.initial.id));
    }
    Ok(())
}

fn result_failure(result: &RemoteResultInspection) -> Option<TaskFailure> {
    (result.task_status == TaskStatus::Failed).then(|| TaskFailure {
        code: if result.report.outcome == RemoteTaskOutcome::Rejected {
            "remote_rejected"
        } else {
            "remote_failed"
        }
        .into(),
        message: result.report.summary.clone(),
        retryable: false,
    })
}

pub(crate) struct RemoteResultCommit {
    task: StoredTask,
    expected: crate::storage::TaskRow,
    _admission: OwnedMutexGuard<()>,
    outcome: VerifiedRemoteOutcome,
}

impl RemoteResultCommit {
    pub(super) fn new(
        task: StoredTask,
        outcome: VerifiedRemoteOutcome,
        admission: OwnedMutexGuard<()>,
    ) -> Result<Self, SchedulerError> {
        validate_task(&task)?;
        let remote = task
            .definition
            .remote
            .as_ref()
            .ok_or(SchedulerError::NotOwned(task.definition.initial.id))?;
        if outcome.binding() != &remote.binding {
            return Err(SchedulerError::Conflict(task.definition.initial.id));
        }
        validate_report(&task, &remote.binding, outcome.report())?;
        let expected = super::repository::row(&task)?;
        Ok(Self {
            task,
            expected,
            outcome,
            _admission: admission,
        })
    }

    pub(crate) fn validate_owner(&self, owner: &Agent) -> Result<(), AgentError> {
        self.outcome.validate_fresh_owner(owner)
    }

    pub(crate) fn validate_record(
        store: &crate::storage::Store,
        conversation: ConversationId,
        binding: &RemoteTaskBinding,
        result: &RemoteResultInspection,
    ) -> Result<(), SchedulerError> {
        let task = super::repository::load(store, result.report.task_id)?;
        let saved = snapshot(&task)?;
        if task.definition.initial.conversation_id != conversation
            || saved.binding != *binding
            || saved.result.as_ref() != Some(result)
        {
            return Err(SchedulerError::Conflict(result.report.task_id));
        }
        Ok(())
    }

    pub(crate) fn conversation_id(&self) -> ConversationId {
        self.task.definition.initial.conversation_id
    }
    pub(crate) fn command_id(&self) -> CommandId {
        self.task.checkpoint.active_command
    }
    pub(crate) fn instruction(&self) -> &str {
        &self.task.definition.initial.instruction
    }
    pub(crate) fn configuration(&self) -> &ConversationConfig {
        &self.task.definition.configuration
    }
    pub(crate) fn report(&self) -> &crate::protocol::RemoteTaskReport {
        self.outcome.report()
    }
    pub(crate) fn binding(&self) -> &RemoteTaskBinding {
        self.outcome.binding()
    }
    pub(crate) fn not_before(&self) -> UnixMillis {
        self.task
            .checkpoint
            .task
            .updated_at
            .max(self.outcome.verified_at())
    }
    pub(crate) fn cancellation_requested(&self) -> bool {
        self.task.checkpoint.cancel_requested
    }

    pub(crate) fn task_update(
        &self,
        result: &RemoteResultInspection,
    ) -> Result<crate::storage::ExternalTaskStateUpdate, SchedulerError> {
        result.validate(self.conversation_id())?;
        let mut incoming = self.outcome.report().clone();
        incoming.accepted_at = result.report.accepted_at;
        if incoming != result.report
            || result.cancellation_requested != self.cancellation_requested()
        {
            return Err(SchedulerError::Conflict(self.task.definition.initial.id));
        }
        let mut next = self.task.clone();
        let at = result.report.accepted_at;
        if result.task_status == TaskStatus::Completed {
            next.checkpoint
                .task
                .transition(TaskStatus::Running, at, None)?;
        }
        if result.task_status != next.checkpoint.task.status {
            next.checkpoint
                .task
                .transition(result.task_status, at, result_failure(result))?;
        } else {
            next.checkpoint.task.updated_at = at;
        }
        let phase = if result.report.outcome == RemoteTaskOutcome::Unknown {
            RemoteTaskPhase::OutcomeUnknown
        } else {
            RemoteTaskPhase::Settled
        };
        let checkpoint = next
            .checkpoint
            .remote
            .as_mut()
            .ok_or(SchedulerError::Invalid("remote checkpoint is missing"))?;
        checkpoint.phase = phase;
        checkpoint.result = Some(result.clone());
        next.checkpoint.waiting = if phase == RemoteTaskPhase::OutcomeUnknown {
            Some(super::types::WaitingReason::Remote { phase })
        } else {
            None
        };
        next.checkpoint.last_output = Some(result.report.summary.clone());
        next.checkpoint.diagnostic = if phase == RemoteTaskPhase::OutcomeUnknown {
            Some("Remote execution outcome remains unknown; its primary slot and original allowance remain reserved and no automatic retry is permitted".into())
        } else {
            None
        };
        next.state = super::repository::state(result.task_status);
        validate_task(&next)?;
        Ok(crate::storage::ExternalTaskStateUpdate {
            expected: self.expected.clone(),
            next_state: next.state,
            checkpoint: super::repository::encode_checkpoint(&next.checkpoint)?,
            updated_at: super::types::timestamp(at)?,
        })
    }
}
