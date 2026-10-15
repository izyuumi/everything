use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub const CURRENT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("identifier must not be nil")]
    NilIdentifier,
    #[error("invalid UUID: {0}")]
    InvalidIdentifier(#[from] uuid::Error),
    #[error("unsupported schema version {received}; supported version is {supported}")]
    UnsupportedSchemaVersion { received: u16, supported: u16 },
    #[error("event sequence must start at one")]
    InvalidEventSequence,
    #[error("event sequence exhausted")]
    EventSequenceExhausted,
    #[error("system time is before the Unix epoch")]
    ClockBeforeEpoch,
    #[error("Unix timestamp exceeds the supported range")]
    TimestampOverflow,
    #[error("task cannot transition from {from:?} to {to:?}")]
    InvalidTaskTransition { from: TaskStatus, to: TaskStatus },
    #[error("task update timestamp precedes its previous update")]
    NonMonotonicTaskTimestamp,
    #[error("failed task requires a failure, and other task states must not contain one")]
    InvalidTaskFailure,
}

macro_rules! define_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(try_from = "Uuid", into = "Uuid")]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl TryFrom<Uuid> for $name {
            type Error = ProtocolError;

            fn try_from(value: Uuid) -> Result<Self, Self::Error> {
                if value.is_nil() {
                    return Err(ProtocolError::NilIdentifier);
                }
                Ok(Self(value))
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl FromStr for $name {
            type Err = ProtocolError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::try_from(Uuid::parse_str(value)?)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

define_id!(ConversationId);
define_id!(MessageId);
define_id!(TaskId);
define_id!(CommandId);
define_id!(EventId);
define_id!(ToolCallId);
define_id!(OperationId);
define_id!(DeviceId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct SchemaVersion(u16);

impl SchemaVersion {
    pub const CURRENT: Self = Self(CURRENT_SCHEMA_VERSION);

    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Default for SchemaVersion {
    fn default() -> Self {
        Self::CURRENT
    }
}

impl TryFrom<u16> for SchemaVersion {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        if value != CURRENT_SCHEMA_VERSION {
            return Err(ProtocolError::UnsupportedSchemaVersion {
                received: value,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        Ok(Self(value))
    }
}

impl From<SchemaVersion> for u16 {
    fn from(value: SchemaVersion) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct EventSequence(NonZeroU64);

impl EventSequence {
    pub const FIRST: Self = Self(NonZeroU64::MIN);

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub fn next(self) -> Result<Self, ProtocolError> {
        let next = self
            .get()
            .checked_add(1)
            .ok_or(ProtocolError::EventSequenceExhausted)?;
        Self::try_from(next)
    }
}

impl TryFrom<u64> for EventSequence {
    type Error = ProtocolError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(ProtocolError::InvalidEventSequence)
    }
}

impl From<EventSequence> for u64 {
    fn from(value: EventSequence) -> Self {
        value.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct UnixMillis(u64);

impl UnixMillis {
    pub fn now() -> Result<Self, ProtocolError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProtocolError::ClockBeforeEpoch)?;
        let millis =
            u64::try_from(elapsed.as_millis()).map_err(|_| ProtocolError::TimestampOverflow)?;
        Self::try_from(millis)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for UnixMillis {
    type Error = ProtocolError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > i64::MAX as u64 {
            return Err(ProtocolError::TimestampOverflow);
        }
        Ok(Self(value))
    }
}

impl From<UnixMillis> for u64 {
    fn from(value: UnixMillis) -> Self {
        value.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub provider_call_id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: ToolCallId,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MessageContent {
    Text { text: String },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: Vec<MessageContent>,
    pub created_at: UnixMillis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    AwaitingPermission,
    WaitingForChildren,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Failed | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Queued
                        | Self::AwaitingPermission
                        | Self::WaitingForChildren
                        | Self::Completed
                        | Self::Failed
                        | Self::Cancelled
                )
                | (
                    Self::AwaitingPermission | Self::WaitingForChildren,
                    Self::Queued | Self::Running | Self::Failed | Self::Cancelled
                )
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub schema_version: SchemaVersion,
    pub id: TaskId,
    pub conversation_id: ConversationId,
    pub parent_task_id: Option<TaskId>,
    pub originating_command_id: CommandId,
    pub instruction: String,
    pub status: TaskStatus,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub failure: Option<TaskFailure>,
}

impl TaskRecord {
    pub fn new(
        conversation_id: ConversationId,
        parent_task_id: Option<TaskId>,
        originating_command_id: CommandId,
        instruction: String,
        now: UnixMillis,
    ) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            id: TaskId::new(),
            conversation_id,
            parent_task_id,
            originating_command_id,
            instruction,
            status: TaskStatus::Queued,
            created_at: now,
            updated_at: now,
            failure: None,
        }
    }

    pub fn transition(
        &mut self,
        status: TaskStatus,
        now: UnixMillis,
        failure: Option<TaskFailure>,
    ) -> Result<(), ProtocolError> {
        if !self.status.can_transition_to(status) {
            return Err(ProtocolError::InvalidTaskTransition {
                from: self.status,
                to: status,
            });
        }
        if now < self.updated_at {
            return Err(ProtocolError::NonMonotonicTaskTimestamp);
        }
        if (status == TaskStatus::Failed) != failure.is_some() {
            return Err(ProtocolError::InvalidTaskFailure);
        }
        self.status = status;
        self.updated_at = now;
        self.failure = failure;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConversationEvent {
    ConversationCreated {
        title: Option<String>,
    },
    MessageAppended {
        message: ConversationMessage,
    },
    AssistantTextDelta {
        task_id: TaskId,
        message_id: MessageId,
        text: String,
    },
    TaskCreated {
        task: TaskRecord,
    },
    TaskStatusChanged {
        task_id: TaskId,
        status: TaskStatus,
        failure: Option<TaskFailure>,
    },
    ToolRequested {
        task_id: TaskId,
        call: ToolCall,
    },
    ToolCompleted {
        task_id: TaskId,
        result: ToolResult,
    },
    PermissionRequested {
        task_id: TaskId,
        operation_id: OperationId,
        tool_call_id: ToolCallId,
        tool_name: String,
    },
    PermissionDecided {
        operation_id: OperationId,
        decision: PermissionDecision,
    },
    SteeringReceived {
        task_id: TaskId,
        command_id: CommandId,
        instruction: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: SchemaVersion,
    pub event_id: EventId,
    pub conversation_id: ConversationId,
    pub sequence: EventSequence,
    pub occurred_at: UnixMillis,
    pub causation_id: Option<CommandId>,
    pub payload: ConversationEvent,
}

impl EventEnvelope {
    pub fn new(
        conversation_id: ConversationId,
        sequence: EventSequence,
        occurred_at: UnixMillis,
        causation_id: Option<CommandId>,
        payload: ConversationEvent,
    ) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            event_id: EventId::new(),
            conversation_id,
            sequence,
            occurred_at,
            causation_id,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    CreateConversation {
        title: Option<String>,
    },
    SendMessage {
        message: ConversationMessage,
    },
    CreateTask {
        instruction: String,
        parent_task_id: Option<TaskId>,
    },
    SteerTask {
        task_id: TaskId,
        instruction: String,
    },
    CancelTask {
        task_id: TaskId,
    },
    ResolvePermission {
        operation_id: OperationId,
        decision: PermissionDecision,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub schema_version: SchemaVersion,
    pub command_id: CommandId,
    pub conversation_id: ConversationId,
    pub issuing_device_id: Option<DeviceId>,
    pub issued_at: UnixMillis,
    pub payload: Command,
}

impl CommandEnvelope {
    pub fn new(
        conversation_id: ConversationId,
        issuing_device_id: Option<DeviceId>,
        issued_at: UnixMillis,
        payload: Command,
    ) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            command_id: CommandId::new(),
            conversation_id,
            issuing_device_id,
            issued_at,
            payload,
        }
    }
}
