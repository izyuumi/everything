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
    },
    Settings,
    Identity,
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
            Self::Conversations { limit } | Self::Tasks { limit, .. } | Self::Devices { limit }
                if !(1..=100).contains(limit)
        ) {
            return Err("query limit must be between one and one hundred");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerOperation {
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
}

impl OwnerOperation {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
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
    },
    Approvals {
        approvals: Vec<crate::permissions::ApprovalRequest>,
    },
    Settings {
        settings: crate::host::settings::HostSettings,
    },
    Identity {
        keys: crate::crypto::DevicePublicKeys,
        fingerprint: String,
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
            (OwnerQuery::Task { task_id }, Self::Task { task }) => task.id == *task_id,
            (OwnerQuery::Approvals { conversation_id }, Self::Approvals { approvals }) => {
                approvals.len() <= 256
                    && approvals.iter().all(|approval| {
                        approval.scope.conversation_id == conversation_id.to_string()
                    })
            }
            (OwnerQuery::Settings, Self::Settings { .. }) => true,
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
        Ok(())
    }
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerOperationResult {
    Applied,
    Settings {
        settings: crate::host::settings::HostSettings,
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
}

impl OwnerOperationResult {
    pub fn validate_for(&self, operation: &OwnerOperation) -> Result<(), &'static str> {
        let valid = match (operation, self) {
            (
                OwnerOperation::ReplaceSettings {
                    expected_revision, ..
                },
                Self::Settings { settings },
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
            _ => false,
        };
        if !valid {
            return Err("owner operation result does not match its request");
        }
        Ok(())
    }
}
