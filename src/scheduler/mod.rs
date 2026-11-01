mod actor;
pub(crate) mod evidence;
mod inputs;
mod remote;
mod repository;
mod schedule_context;
mod tools;
mod types;
mod worker;
mod workspace;

pub use actor::{Scheduler, ShutdownReport};
pub(crate) use remote::RemoteResultCommit;
pub use remote::{
    RemoteBudgetCharge, RemoteDispatchPermit, RemoteOwnerApproval, RemoteRelease,
    RemoteTaskAdmission, RemoteTaskBinding, RemoteTaskPhase, RemoteTaskSnapshot,
    RemoteTaskSubmission,
};
pub use schedule_context::ScheduleCallerContext;
pub use tools::{MESSAGE_AGENT, SCHEDULER_TOOL_NAMES, SchedulerToolBinding, SchedulerTools};
pub use types::{
    AgentHop, DelegationCeiling, DiscoveryServices, ExecutionPolicy, LiveText, PreparedTaskInputs,
    SchedulerError, SchedulerLimits, SchedulerServices, TaskPreparation, TaskPreparationRequest,
    TaskProviderFactory, TaskSnapshot, TaskSubmission, WaitingReason,
};

pub fn agent_log(
    store: &crate::storage::Store,
    agent: Option<crate::host::agents::AgentId>,
    conversation: Option<crate::protocol::ConversationId>,
    limit: usize,
) -> Result<Vec<AgentHop>, SchedulerError> {
    repository::agent_log(store, agent, conversation, limit)
}

pub fn task_snapshot(
    store: &crate::storage::Store,
    task: crate::protocol::TaskId,
) -> Result<TaskSnapshot, SchedulerError> {
    task_snapshot_with_evidence(store, task).map(|(snapshot, _)| snapshot)
}

pub(crate) fn task_snapshot_with_evidence(
    store: &crate::storage::Store,
    task: crate::protocol::TaskId,
) -> Result<(TaskSnapshot, Option<evidence::TerminalTaskEvidence>), SchedulerError> {
    let task = repository::load(store, task)?;
    Ok((task.snapshot(), task.checkpoint.terminal_evidence))
}

pub(crate) fn task_execution(
    store: &crate::storage::Store,
    task: crate::protocol::TaskId,
) -> Result<ExecutionPolicy, SchedulerError> {
    Ok(repository::load(store, task)?.definition.execution)
}

pub fn task_publication_records(
    store: &crate::storage::Store,
    task: crate::protocol::TaskId,
) -> Result<(crate::protocol::TaskRecord, crate::protocol::TaskRecord), SchedulerError> {
    let saved = repository::load(store, task)?;
    Ok((saved.definition.initial, saved.checkpoint.task))
}
