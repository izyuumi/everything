mod actor;
mod repository;
mod tools;
mod types;
mod worker;
mod workspace;

pub use actor::{Scheduler, ShutdownReport};
pub use tools::{SchedulerToolBinding, SchedulerTools};
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
