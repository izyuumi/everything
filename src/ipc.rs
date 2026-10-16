use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::{ConversationConfig, TextCommand};
use crate::permissions::ExecutionMode;
use crate::protocol::{CommandId, ConversationId, DeviceId, TaskId, UnixMillis};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestedRunOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_mode: Option<ExecutionMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_model_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_charged_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_duration_seconds: Option<u64>,
}

impl RequestedRunOptions {
    pub fn is_empty(&self) -> bool {
        self.workspace.is_none()
            && self.execution_mode.is_none()
            && self.tools.is_none()
            && self.context_tokens.is_none()
            && self.max_model_attempts.is_none()
            && self.max_tool_calls.is_none()
            && self.max_charged_tokens.is_none()
            && self.max_duration_seconds.is_none()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerCommand {
    Text {
        command: TextCommand,
        #[serde(default)]
        options: RequestedRunOptions,
    },
    Initialize {
        conversation_id: ConversationId,
        command_id: CommandId,
        title: String,
        configuration: ConversationConfig,
    },
}

impl OwnerCommand {
    pub fn command_id(&self) -> CommandId {
        match self {
            Self::Text { command, .. } => command.command_id,
            Self::Initialize { command_id, .. } => *command_id,
        }
    }

    pub fn conversation_id(&self) -> ConversationId {
        match self {
            Self::Text { command, .. } => command.conversation_id,
            Self::Initialize {
                conversation_id, ..
            } => *conversation_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerQuery {
    Conversations {
        limit: usize,
    },
    Conversation {
        conversation_id: ConversationId,
    },
    Tasks {
        conversation_id: Option<ConversationId>,
        limit: usize,
    },
    Task {
        task_id: TaskId,
    },
    Approvals {
        conversation_id: ConversationId,
        #[serde(default = "default_approval_page_limit")]
        limit: usize,
        cursor: Option<crate::permissions::ApprovalCursor>,
    },
    Approval {
        conversation_id: ConversationId,
        lookup: ApprovalLookup,
    },
    Settings,
    Publication,
    Identity,
    Runtime,
    ToolRecovery,
    Transport,
    Devices {
        limit: usize,
    },
    Command {
        command_id: CommandId,
    },
}

impl OwnerQuery {
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(
            self,
            Self::Conversations { limit } | Self::Tasks { limit, .. } | Self::Devices { limit } | Self::Approvals { limit, .. }
                if !(1..=100).contains(limit)
        ) {
            return Err("query limit must be between one and one hundred");
        }
        if let Self::Approvals {
            cursor: Some(cursor),
            ..
        } = self
        {
            cursor
                .validate()
                .map_err(|_| "invalid approval page cursor")?;
        }
        if let Self::Approval { lookup, .. } = self {
            let identifier = match lookup {
                ApprovalLookup::Approval(identifier) | ApprovalLookup::Operation(identifier) => {
                    identifier
                }
            };
            if identifier.trim().is_empty()
                || identifier.len() > 256
                || identifier.chars().any(char::is_control)
            {
                return Err("invalid approval lookup identifier");
            }
        }
        Ok(())
    }
}

fn default_approval_page_limit() -> usize {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "by",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ApprovalLookup {
    Approval(String),
    Operation(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerOperation {
    DiscardUnapplied {
        command_id: CommandId,
        expected_accepted_at: UnixMillis,
        operator: String,
        reason: String,
    },
    ReplaceSettings {
        expected_revision: u64,
        settings: crate::host::settings::HostSettings,
    },
    Credential {
        operation: crate::host::credentials::CredentialOperation,
    },
    BeginPairing {
        lifetime_ms: u64,
    },
    ApprovePairing {
        request: crate::crypto::PairingRequest,
        label: String,
        expected_fingerprint: String,
        access: crate::devices::DeviceAccess,
    },
    RevokeDevice {
        device_id: DeviceId,
    },
    WakeTask {
        task_id: TaskId,
    },
    RetryProvider {
        task_id: TaskId,
        expected_attempt: CommandId,
    },
    ResumeInterrupted {
        task_id: TaskId,
        expected_command: CommandId,
        expected_revision: u64,
        expected_status: crate::agent::LoopStatus,
    },
    AcknowledgeCleanup {
        operation_id: String,
        original_result_digest: String,
        operator: String,
        note: String,
    },
    ReconcileDrained,
}

impl OwnerOperation {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::DiscardUnapplied {
                expected_accepted_at,
                operator,
                reason,
                ..
            } => crate::host::inbox::UnappliedDiscard::validate_metadata(
                *expected_accepted_at,
                operator,
                reason,
            )
            .map_err(|_| "invalid unapplied command discard"),
            Self::ResumeInterrupted {
                expected_revision,
                expected_status,
                ..
            } if *expected_revision == 0
                || *expected_revision > i64::MAX as u64
                || !matches!(
                    expected_status,
                    crate::agent::LoopStatus::Paused {
                        reason: crate::agent::PauseReason::Cancellation
                            | crate::agent::PauseReason::InterruptedTool,
                    }
                ) =>
            {
                Err(
                    "known interruption resume requires its exact supported phase and checkpoint revision",
                )
            }
            Self::BeginPairing { lifetime_ms }
                if *lifetime_ms == 0 || *lifetime_ms > crate::crypto::MAX_PAIRING_LIFETIME_MS =>
            {
                Err("invalid pairing lifetime")
            }
            Self::ApprovePairing {
                label,
                expected_fingerprint,
                access,
                ..
            } => crate::devices::PairingApproval::new(
                label.clone(),
                expected_fingerprint.clone(),
                access.clone(),
            )
            .map(|_| ())
            .map_err(|_| "invalid pairing approval"),
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationWait {
    pub conversation_id: ConversationId,
    pub after_sequence: u64,
    pub timeout_ms: u64,
}

impl ConversationWait {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.timeout_ms == 0 || self.timeout_ms > 30_000 {
            return Err("conversation wait must be between one and thirty thousand milliseconds");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerQueryResult {
    Publication {
        status: crate::host::publication::PublicationStatus,
    },
    Conversations {
        conversations: Vec<ConversationSummary>,
    },
    Conversation {
        information: ConversationSummary,
        configuration: ConversationConfig,
    },
    Tasks {
        tasks: Vec<crate::protocol::TaskRecord>,
    },
    Task {
        task: crate::protocol::TaskRecord,
        execution: TaskExecutionMetadata,
    },
    LegacyForeground {
        task: crate::protocol::TaskRecord,
    },
    Approvals {
        approvals: Vec<crate::permissions::ApprovalSummary>,
        next_cursor: Option<crate::permissions::ApprovalCursor>,
    },
    Approval {
        approval: Option<crate::permissions::ApprovalView>,
    },
    Settings {
        settings: crate::host::settings::HostSettings,
    },
    Identity {
        keys: crate::crypto::DevicePublicKeys,
        fingerprint: String,
    },
    Transport {
        relay: RelayStatus,
    },
    Runtime {
        state: HostRuntimeState,
    },
    ToolRecovery {
        report: crate::tools::broker::RecoveryReport,
        running_operations: Vec<String>,
        more_running: bool,
    },
    Devices {
        devices: Vec<crate::storage::DeviceRow>,
    },
    Command {
        receipt: Option<CommandReceipt>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub id: ConversationId,
    pub title: String,
    pub provider: crate::agent::ProviderKind,
    pub model: String,
    pub endpoint: crate::agent::ProviderEndpoint,
    pub last_sequence: u64,
    pub checkpoint_revision: u64,
    pub message_count: usize,
    pub pending_command_id: Option<CommandId>,
    pub native_status: Option<crate::agent::LoopStatus>,
}

impl From<crate::agent::ConversationInfo> for ConversationSummary {
    fn from(information: crate::agent::ConversationInfo) -> Self {
        Self {
            id: information.id,
            title: information.title,
            provider: information.provider,
            model: information.model,
            endpoint: information.endpoint,
            last_sequence: information.last_sequence,
            checkpoint_revision: information.checkpoint_revision,
            message_count: information.message_count,
            pending_command_id: information.pending_command_id,
            native_status: information.native_status,
        }
    }
}

impl OwnerQueryResult {
    pub fn validate_for(&self, query: &OwnerQuery) -> Result<(), &'static str> {
        let valid = match (query, self) {
            (OwnerQuery::Conversations { limit }, Self::Conversations { conversations }) => {
                conversations.len() <= *limit
            }
            (
                OwnerQuery::Conversation { conversation_id },
                Self::Conversation { information, .. },
            ) => information.id == *conversation_id,
            (
                OwnerQuery::Tasks {
                    conversation_id,
                    limit,
                },
                Self::Tasks { tasks },
            ) => {
                tasks.len() <= *limit
                    && tasks
                        .iter()
                        .all(|task| conversation_id.is_none_or(|id| task.conversation_id == id))
            }
            (
                OwnerQuery::Task { task_id },
                Self::Task { task, .. } | Self::LegacyForeground { task },
            ) => task.id == *task_id,
            (
                OwnerQuery::Approvals {
                    conversation_id,
                    limit,
                    cursor,
                },
                Self::Approvals {
                    approvals,
                    next_cursor,
                },
            ) => {
                let ordered = approvals.windows(2).all(|pair| {
                    (pair[0].requested_at, &pair[0].id) < (pair[1].requested_at, &pair[1].id)
                });
                let after = cursor.as_ref().is_none_or(|cursor| {
                    approvals.first().is_none_or(|first| {
                        (first.requested_at, &first.id) > (cursor.requested_at, &cursor.approval_id)
                    })
                });
                let continuation = next_cursor.as_ref().is_none_or(|cursor| {
                    approvals.last().is_some_and(|last| {
                        last.requested_at == cursor.requested_at && last.id == cursor.approval_id
                    })
                });
                approvals.len() <= *limit
                    && ordered
                    && after
                    && continuation
                    && approvals.iter().all(|approval| {
                        approval.conversation_id == conversation_id.to_string()
                            && approval.status == crate::permissions::ApprovalStatus::Pending
                    })
            }
            (
                OwnerQuery::Approval {
                    conversation_id,
                    lookup,
                },
                Self::Approval { approval },
            ) => approval.as_ref().is_none_or(|approval| {
                approval.request.scope.conversation_id == conversation_id.to_string()
                    && match lookup {
                        ApprovalLookup::Approval(id) => approval.request.id == *id,
                        ApprovalLookup::Operation(id) => approval.request.scope.operation_id == *id,
                    }
            }),
            (OwnerQuery::Settings, Self::Settings { .. }) => true,
            (OwnerQuery::Publication, Self::Publication { .. }) => true,
            (OwnerQuery::Transport, Self::Transport { .. }) => true,
            (OwnerQuery::Runtime, Self::Runtime { .. }) => true,
            (
                OwnerQuery::ToolRecovery,
                Self::ToolRecovery {
                    running_operations, ..
                },
            ) => running_operations.len() <= 128,
            (OwnerQuery::Identity, Self::Identity { keys, fingerprint }) => {
                keys.validate().is_ok() && keys.fingerprint() == *fingerprint
            }
            (OwnerQuery::Devices { limit }, Self::Devices { devices }) => devices.len() <= *limit,
            (OwnerQuery::Command { command_id }, Self::Command { receipt }) => receipt
                .as_ref()
                .is_none_or(|receipt| receipt.command_id == *command_id),
            _ => false,
        };
        if !valid {
            return Err("owner query result does not match its request");
        }
        if matches!(self, Self::Approvals { .. })
            && serde_json::to_vec(self)
                .map_err(|_| "invalid approval metadata response")?
                .len()
                > crate::permissions::MAX_APPROVAL_METADATA_BYTES
        {
            return Err("approval metadata response exceeds its byte limit");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayStatus {
    pub origin: Option<crate::relay_protocol::RelayOrigin>,
    pub connection: RelayConnectionState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostRuntimeState {
    pub draining: bool,
    pub workers_drained: bool,
    pub broker_quarantined: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayConnectionState {
    Disabled,
    Stopped,
    Connecting,
    Connected,
    Retrying,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub command_id: CommandId,
    pub conversation_id: ConversationId,
    pub phase: crate::host::inbox::CommandPhase,
    pub accepted_at: UnixMillis,
    pub applied: Option<crate::host::inbox::AppliedReceipt>,
    pub result: Option<serde_json::Value>,
    pub archived: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskExecutionMetadata {
    pub active_command_id: CommandId,
    pub queued_messages: usize,
    pub cancel_requested: bool,
    pub waiting: Option<crate::scheduler::WaitingReason>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerOperationResult {
    Command {
        receipt: CommandReceipt,
    },
    Applied,
    Settings {
        settings: crate::host::settings::HostSettings,
        restart_required: bool,
    },
    PairingInvitation {
        pairing_id: crate::protocol::MessageId,
        expires_at_ms: u64,
        fingerprint: String,
        invitation: crate::host::credentials::SecretTransfer,
    },
    PairingApproved {
        acceptance: crate::crypto::PairingAcceptance,
    },
    ToolRecovery {
        report: crate::tools::broker::RecoveryReport,
    },
}

impl OwnerOperationResult {
    pub fn validate_for(&self, operation: &OwnerOperation) -> Result<(), &'static str> {
        let valid = match (operation, self) {
            (
                OwnerOperation::DiscardUnapplied {
                    command_id,
                    expected_accepted_at,
                    ..
                },
                Self::Command { receipt },
            ) => {
                receipt.command_id == *command_id
                    && receipt.accepted_at == *expected_accepted_at
                    && receipt.phase == crate::host::inbox::CommandPhase::Rejected
            }
            (
                OwnerOperation::AcknowledgeCleanup { .. } | OwnerOperation::ReconcileDrained,
                Self::ToolRecovery { .. },
            ) => true,
            (
                OwnerOperation::ReplaceSettings {
                    expected_revision, ..
                },
                Self::Settings { settings, .. },
            ) => expected_revision.checked_add(1) == Some(settings.revision),
            (OwnerOperation::BeginPairing { .. }, Self::PairingInvitation { fingerprint, .. }) => {
                fingerprint.len() == 71
            }
            (
                OwnerOperation::ApprovePairing { request, .. },
                Self::PairingApproved { acceptance },
            ) => {
                acceptance.pairing_id == request.pairing_id
                    && acceptance.responder == request.responder
            }
            (
                OwnerOperation::Credential { .. }
                | OwnerOperation::RevokeDevice { .. }
                | OwnerOperation::WakeTask { .. }
                | OwnerOperation::RetryProvider { .. },
                Self::Applied,
            ) => true,
            (OwnerOperation::ResumeInterrupted { .. }, Self::Applied) => true,
            _ => false,
        };
        if !valid {
            return Err("owner operation result does not match its request");
        }
        Ok(())
    }
}
