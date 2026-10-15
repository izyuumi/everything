mod actor;
mod inputs;
mod repository;
mod schedule_context;
mod tools;
mod types;
mod worker;
mod workspace;

pub use actor::{Scheduler, ShutdownReport};
pub use schedule_context::ScheduleCallerContext;
pub use tools::{SCHEDULER_TOOL_NAMES, SchedulerToolBinding, SchedulerTools};
pub use types::{
    DiscoveryServices, ExecutionPolicy, PreparedTaskInputs, SchedulerError, SchedulerLimits,
    SchedulerServices, TaskPreparation, TaskPreparationRequest, TaskProviderFactory, TaskSnapshot,
    TaskSubmission, WaitingReason,
};

pub fn task_snapshot(
    store: &crate::storage::Store,
    task: crate::protocol::TaskId,
) -> Result<TaskSnapshot, SchedulerError> {
    repository::load(store, task).map(|task| task.snapshot())
}

pub fn task_publication_records(
    store: &crate::storage::Store,
    task: crate::protocol::TaskId,
) -> Result<(crate::protocol::TaskRecord, crate::protocol::TaskRecord), SchedulerError> {
    let saved = repository::load(store, task)?;
    Ok((saved.definition.initial, saved.checkpoint.task))
}
