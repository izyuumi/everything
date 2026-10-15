use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentError, ConversationConfig, LoopLimits, LoopStatus};
use crate::context::{ContextBudget, ContextBuilder};
use crate::model::ModelProvider;
use crate::permissions::{ExecutionAuthority, ExecutionMode, PermissionError};
use crate::protocol::{CommandId, ConversationId, TaskId, TaskRecord, UnixMillis};
use crate::selector::{SearchTools, SelectorError, ToolSelector};
use crate::storage::{StorageError, Store};
use crate::tools::{ToolError, ToolRegistry, broker::ToolBroker};

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Permission(#[from] PermissionError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Selector(#[from] SelectorError),
    #[error(transparent)]
    Context(#[from] crate::context::ContextError),
    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid scheduler input: {0}")]
    Invalid(&'static str),
    #[error("task identity or immutable execution policy conflicts: {0}")]
    Conflict(TaskId),
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("task is terminal: {0}")]
    Terminal(TaskId),
    #[error("task is outside the caller's descendant scope")]
    OutsideScope,
    #[error("scheduler capacity exhausted: {0}")]
    Capacity(&'static str),
    #[error("scheduler is stopped")]
    Stopped,
    #[error("task needs an explicit owner decision: {0}")]
    OwnerDecision(TaskId),
    #[error("provider construction failed: {0}")]
    Provider(String),
    #[error("scheduler worker failed")]
    Worker,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub workspace_root: PathBuf,
    pub mode: ExecutionMode,
    pub tools_enabled: bool,
    pub allowed_tools: Option<BTreeSet<String>>,
    pub budget: ContextBudget,
    pub limits: LoopLimits,
}

impl ExecutionPolicy {
    pub fn validate(&self) -> Result<(), SchedulerError> {
        self.limits.validate()?;
        ContextBuilder::new(self.budget.clone())?;
        if !self.workspace_root.is_absolute()
            || self.workspace_root.to_str().is_none_or(|path| path.len() > 4096 || path.chars().any(char::is_control))
            || self.allowed_tools.as_ref().is_some_and(|names| names.len() > 1000 || names.iter().any(|name| name.is_empty() || name.len() > 128 || !name.bytes().all(|byte|byte.is_ascii_alphanumeric() || matches!(byte,b'_'|b'-'|b'.'))))
        {
            return Err(SchedulerError::Invalid("invalid execution workspace or tool allowlist"));
        }
        Ok(())
    }

    pub fn capture(&self, registry: &ToolRegistry) -> Result<Self, SchedulerError> {
        self.validate()?;
        let workspace_root = self.workspace_root.canonicalize()?;
        if !workspace_root.is_dir() {
            return Err(SchedulerError::Invalid("workspace is not a directory"));
        }
        let mut captured = self.clone();
        captured.workspace_root = workspace_root;
        let allowed = if self.tools_enabled {
            registry.descriptors().filter(|tool|self.allowed_tools.as_ref().is_none_or(|names|names.contains(&tool.name))).map(|tool|tool.name.clone()).collect()
        } else {
            BTreeSet::new()
        };
        captured.allowed_tools = Some(allowed);
        if !self.tools_enabled { captured.limits.max_tool_calls = 0; }
        captured.validate()?;
        Ok(captured)
    }

    pub(super) fn authority(&self, conversation_id: ConversationId, task_id: TaskId) -> Result<ExecutionAuthority, SchedulerError> {
        self.validate()?;
        let allowed = self.allowed_tools.clone().ok_or(SchedulerError::Invalid("execution policy was not captured"))?;
        let authority = ExecutionAuthority::new(conversation_id.to_string(),task_id.to_string(),self.workspace_root.clone(),self.mode)?.restrict_tools(allowed)?;
        if authority.workspace_root() != self.workspace_root { return Err(SchedulerError::Conflict(task_id)); }
        Ok(authority)
    }
}

#[async_trait]
pub trait TaskProviderFactory: Send + Sync {
    async fn create(&self, configuration: &ConversationConfig, cancellation: CancellationToken) -> Result<Arc<dyn ModelProvider>, SchedulerError>;
}

#[derive(Clone)]
pub struct DiscoveryServices {
    pub selector: ToolSelector,
    pub search: Arc<SearchTools>,
}

#[derive(Clone)]
pub struct SchedulerServices {
    pub agent: Agent,
    pub registry: Arc<ToolRegistry>,
    pub broker: ToolBroker,
    pub providers: Arc<dyn TaskProviderFactory>,
    pub discovery: Option<DiscoveryServices>,
}

#[derive(Clone, Debug)]
pub struct SchedulerLimits {
    pub max_primary_tasks: usize,
    pub max_subagents: usize,
    pub max_pending_tasks: usize,
    pub shutdown_grace: Duration,
}

impl Default for SchedulerLimits {
    fn default() -> Self {
        Self { max_primary_tasks:1,max_subagents:3,max_pending_tasks:128,shutdown_grace:Duration::from_secs(10) }
    }
}

impl SchedulerLimits {
    pub(super) fn validate(&self) -> Result<(), SchedulerError> {
        if !(1..=4).contains(&self.max_primary_tasks) || !(1..=3).contains(&self.max_subagents) || !(1..=256).contains(&self.max_pending_tasks) || !(Duration::from_millis(1)..=Duration::from_secs(60)).contains(&self.shutdown_grace) {
            return Err(SchedulerError::Invalid("invalid scheduler limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSubmission {
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub command_id: CommandId,
    pub accepted_at: UnixMillis,
    pub execution: ExecutionPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag="reason",rename_all="snake_case")]
pub enum WaitingReason {
    Approval,
    ProviderUncertain { attempt_id: CommandId },
    InFlight,
    Reconciliation,
    Interrupted,
    Limit,
    ProviderUnavailable,
    Stopped,
    NativePaused { status: LoopStatus },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub task: TaskRecord,
    pub active_command_id: CommandId,
    pub queued_messages: usize,
    pub cancel_requested: bool,
    pub waiting: Option<WaitingReason>,
    pub last_output: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Definition {
    pub scheduler_version: u16,
    pub initial: TaskRecord,
    pub configuration: ConversationConfig,
    pub execution: ExecutionPolicy,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QueuedMessage {
    pub command_id: CommandId,
    pub instruction: String,
    pub accepted_at: UnixMillis,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResumeAudit {
    pub request_id: CommandId,
    pub expected_attempt: CommandId,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskCheckpoint {
    pub scheduler_version: u16,
    pub task: TaskRecord,
    pub active_command: CommandId,
    pub queued: Vec<QueuedMessage>,
    pub waiting: Option<WaitingReason>,
    pub cancel_requested: bool,
    pub last_output: Option<String>,
    pub diagnostic: Option<String>,
    pub resumes: Vec<ResumeAudit>,
}

#[derive(Clone)]
pub(super) struct StoredTask {
    pub definition: Definition,
    pub checkpoint: TaskCheckpoint,
    pub state: crate::storage::TaskState,
}

impl StoredTask {
    pub fn snapshot(&self) -> TaskSnapshot {
        TaskSnapshot { task:self.checkpoint.task.clone(),active_command_id:self.checkpoint.active_command,queued_messages:self.checkpoint.queued.len(),cancel_requested:self.checkpoint.cancel_requested,waiting:self.checkpoint.waiting.clone(),last_output:self.checkpoint.last_output.clone(),diagnostic:self.checkpoint.diagnostic.clone() }
    }
}

pub(super) fn timestamp(now: UnixMillis) -> Result<i64,SchedulerError> {
    i64::try_from(now.get()).map_err(|_|SchedulerError::Invalid("timestamp overflow"))
}

pub(super) fn store(services: &SchedulerServices) -> Store { services.agent.store() }
