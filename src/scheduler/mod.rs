mod actor;
mod inputs;
mod repository;
mod tools;
mod types;
mod worker;
mod workspace;

pub use actor::{Scheduler, ShutdownReport};
pub use tools::{SCHEDULER_TOOL_NAMES, SchedulerToolBinding, SchedulerTools};
pub use types::{
    DiscoveryServices, ExecutionPolicy, SchedulerError, SchedulerLimits, SchedulerServices,
    TaskProviderFactory, TaskSnapshot, TaskSubmission, WaitingReason,
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
