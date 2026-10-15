pub mod broker;
mod registry;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub use registry::ToolRegistry;

pub const MAX_TOOL_ARGUMENT_BYTES: usize = 1_048_576;
pub const MAX_TOOL_SCHEMA_BYTES: usize = 262_144;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    FilesystemRead,
    FilesystemWrite,
    Shell,
    Network,
    Mcp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolConcurrency {
    Concurrent,
    Exclusive,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub capabilities: Vec<ToolCapability>,
    pub read_only: bool,
    pub concurrency: ToolConcurrency,
}

#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    request_id: String,
    task_id: String,
    workspace_root: PathBuf,
    cancellation: CancellationToken,
}

impl ToolExecutionContext {
    pub fn new(
        request_id: impl Into<String>,
        task_id: impl Into<String>,
        workspace_root: PathBuf,
        cancellation: CancellationToken,
    ) -> Result<Self, ToolError> {
        let request_id = request_id.into();
        let task_id = task_id.into();
        if !valid_identifier(&request_id) || !valid_identifier(&task_id) {
            return Err(ToolError::InvalidContext {
                message:
                    "request and task identifiers must contain 1 to 256 bytes without control characters"
                        .into(),
            });
        }
        if !workspace_root.is_absolute() {
            return Err(ToolError::InvalidContext {
                message: "workspace root must be absolute".into(),
            });
        }
        Ok(Self {
            request_id,
            task_id,
            workspace_root,
            cancellation,
        })
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn check_cancelled(&self) -> Result<(), ToolError> {
        if self.cancellation.is_cancelled() {
            return Err(ToolError::Cancelled {
                request_id: self.request_id.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolOutput {
    pub content: Value,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArgumentViolation {
    pub instance_path: String,
    pub schema_path: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {name}")]
    UnknownTool { name: String },
    #[error("tool already registered: {name}")]
    DuplicateTool { name: String },
    #[error("invalid tool descriptor for {name}: {message}")]
    InvalidDescriptor { name: String, message: String },
    #[error("invalid argument schema for {name}: {message}")]
    InvalidSchema { name: String, message: String },
    #[error("invalid arguments for {name}")]
    InvalidArguments {
        name: String,
        violations: Vec<ArgumentViolation>,
    },
    #[error("{resource} exceeds the {limit}-byte limit")]
    ResourceLimit {
        resource: &'static str,
        limit: usize,
    },
    #[error("invalid execution context: {message}")]
    InvalidContext { message: String },
    #[error("tool request cancelled: {request_id}")]
    Cancelled { request_id: String },
    #[error("tool {name} failed: {message}")]
    Execution { name: String, message: String },
    #[error("tool {name} failed without side effects: {message}")]
    FailedWithoutSideEffects { name: String, message: String },
    #[error("tool {name} has unknown effects: {message}")]
    EffectsUnknown {
        name: String,
        message: String,
        cleanup_completed: bool,
    },
    #[error("tool validation worker failed: {message}")]
    ValidationWorker { message: String },
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError>;
}

fn valid_identifier(identifier: &str) -> bool {
    !identifier.trim().is_empty()
        && identifier.len() <= 256
        && !identifier.chars().any(char::is_control)
}
