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
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.execution_mode == Some(ExecutionMode::SettingsRestricted) {
            return Err(
                "restricted Settings authority is unavailable through ordinary run options",
            );
        }
        Ok(())
    }

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
    WorkerCoordinator {
        query: crate::host::worker::WorkerCoordinatorQuery,
    },
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
    SettingsOperation {
        request_id: CommandId,
    },
    SettingsGrantCurrent {
        device_id: DeviceId,
    },
    SettingsGrantVersion {
        device_id: DeviceId,
        revision: u64,
    },
    SettingsGrantReceipt {
        request_id: CommandId,
    },
    Composition,
    Publication,
    ScheduleEngine,
    Schedule {
        query: crate::scheduling::controls::ScheduleQuery,
    },
    Worker {
        query: crate::worker::commands::WorkerQuery,
    },
    MemoryJob {
        job_id: String,
    },
    RetrieveMemory {
        question: String,
    },
    #[cfg(feature = "local-embeddings")]
    EmbeddingIndex,
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
        if let Self::SettingsGrantVersion { revision, .. } = self
            && (*revision == 0 || *revision > i64::MAX as u64)
        {
            return Err("invalid Settings grant revision");
        }
        if let Self::WorkerCoordinator { query } = self {
            query
                .validate()
                .map_err(|_| "invalid worker coordinator query")?;
        }
        if let Self::Worker { query } = self {
            query.validate()?;
        }
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

#[derive(Debug)]
pub struct JsonFieldError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for JsonFieldError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            formatter.write_str(&self.message)
        } else {
            write!(formatter, "field `{}`: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for JsonFieldError {}

pub fn decode_json<T: serde::de::DeserializeOwned>(
    value: &mut serde_json::Value,
) -> Result<T, JsonFieldError> {
    let message = match T::deserialize(&*value) {
        Ok(decoded) => return Ok(decoded),
        Err(error) => error.to_string(),
    };
    Err(JsonFieldError {
        path: json_error_path::<T>(value, &message),
        message,
    })
}

fn json_error_path<T: serde::de::DeserializeOwned>(
    value: &mut serde_json::Value,
    message: &str,
) -> String {
    use serde_json::Value;
    let mut pointer = String::new();
    let mut path = String::new();
    let mut budget = 256_usize;
    let variant = message
        .strip_prefix("unknown variant `")
        .and_then(|rest| rest.split_once('`'))
        .map(|(name, _)| name);
    loop {
        let (keys, array) = match value.pointer(&pointer) {
            Some(Value::Object(map)) => (map.keys().cloned().collect::<Vec<_>>(), false),
            Some(Value::Array(items)) => (
                (0..items.len()).map(|index| index.to_string()).collect(),
                true,
            ),
            _ => break,
        };
        let mut chosen = None;
        let mut fallback = None;
        for key in keys {
            if budget == 0 {
                return path;
            }
            budget -= 1;
            if !probe_json::<T>(value, &pointer, &key, None)
                .is_some_and(|error| error.as_deref() != Some(message))
            {
                continue;
            }
            if !probe_json::<T>(value, &pointer, &key, Some(Value::String("\u{1}".into())))
                .flatten()
                .is_some_and(|error| error.starts_with("unknown variant"))
            {
                chosen = Some(key);
                break;
            }
            if fallback.is_none()
                && variant.is_some_and(|name| {
                    value
                        .pointer(&format!("{pointer}/{}", escape_pointer(&key)))
                        .and_then(Value::as_str)
                        == Some(name)
                })
            {
                fallback = Some(key);
            }
        }
        let Some(key) = chosen.or(fallback) else {
            break;
        };
        pointer = format!("{pointer}/{}", escape_pointer(&key));
        if array {
            path = format!("{path}[{key}]");
        } else if path.is_empty() {
            path = key;
        } else {
            path = format!("{path}.{key}");
        }
    }
    path
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn probe_json<T: serde::de::DeserializeOwned>(
    value: &mut serde_json::Value,
    pointer: &str,
    key: &str,
    replacement: Option<serde_json::Value>,
) -> Option<Option<String>> {
    use serde_json::Value;
    let removal = replacement.is_none();
    let original = match (value.pointer_mut(pointer)?, replacement) {
        (Value::Object(map), None) => map.remove(key)?,
        (Value::Object(map), Some(replacement)) => {
            std::mem::replace(map.get_mut(key)?, replacement)
        }
        (Value::Array(items), None) => {
            let index = key
                .parse::<usize>()
                .ok()
                .filter(|index| *index < items.len())?;
            items.remove(index)
        }
        (Value::Array(items), Some(replacement)) => {
            std::mem::replace(items.get_mut(key.parse::<usize>().ok()?)?, replacement)
        }
        _ => return None,
    };
    let error = T::deserialize(&*value).err().map(|error| error.to_string());
    match value.pointer_mut(pointer)? {
        Value::Object(map) => {
            map.insert(key.to_owned(), original);
        }
        Value::Array(items) => {
            let index = key.parse::<usize>().ok()?;
            if removal {
                items.insert(index, original);
            } else {
                *items.get_mut(index)? = original;
            }
        }
        _ => return None,
    }
    Some(error)
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
    SetSettingsGrant {
        change: Box<crate::host::settings::SettingsGrantChange>,
    },
    WorkerCoordinator {
        operation: Box<crate::host::worker::WorkerCoordinatorOperation>,
    },
    RevokeWorkerPeer {
        device_id: DeviceId,
    },
    Schedule {
        mutation: crate::scheduling::controls::ScheduleMutation,
    },
    AcknowledgeScheduledUnknown {
        request: crate::scheduling::acknowledgements::UnknownAcknowledgementRequest,
    },
    PrepareSelection {
        command: serde_json::Value,
    },
    ApproveSelection {
        ticket: uuid::Uuid,
        command: serde_json::Value,
    },
    DiscardSelection {
        ticket: uuid::Uuid,
    },
    MemoryEnqueue {
        job_id: String,
        request: crate::memory::consolidate::MaintenanceRequest,
    },
    MemoryCancel {
        job_id: String,
    },
    MemoryRetry {
        job_id: String,
    },
    ApplyMemoryPlan {
        job_id: String,
        plan: crate::memory::consolidate::ConsolidationPlan,
    },
    #[cfg(feature = "local-embeddings")]
    RebuildEmbeddings {
        max_chunks: usize,
        max_elapsed_ms: u64,
        fresh: bool,
    },
    DiscardUnapplied {
        command_id: CommandId,
        expected_accepted_at: UnixMillis,
        operator: String,
        reason: String,
    },
    ReplaceSettings {
        expected_revision: u64,
        settings: Box<crate::host::settings::HostSettings>,
    },
    ReconcileSettings {
        operation_id: CommandId,
    },
    Credential {
        operation: crate::host::credentials::CredentialOperation,
    },
    #[serde(rename = "chatgpt")]
    ChatGpt {
        operation: crate::host::chatgpt::ChatGptOperation,
    },
    #[serde(rename = "chatgpt_models")]
    ChatGptModels {
        binding: crate::host::settings::ModelCredentialReference,
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
    AcknowledgeSettingsUncertainty {
        command_id: CommandId,
        exact_fence_sha256: String,
        lost_ram_uncertainty_accepted: bool,
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
            Self::SetSettingsGrant { change } => change
                .validate()
                .map_err(|_| "invalid Settings grant change"),
            Self::AcknowledgeSettingsUncertainty {
                exact_fence_sha256,
                lost_ram_uncertainty_accepted,
                ..
            } => {
                if !*lost_ram_uncertainty_accepted
                    || exact_fence_sha256.len() != 64
                    || !exact_fence_sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(
                        "Settings uncertainty acknowledgement requires the exact fence and explicit lost-RAM uncertainty acceptance",
                    );
                }
                Ok(())
            }
            Self::WorkerCoordinator { operation } => operation
                .validate()
                .map_err(|_| "invalid worker coordinator operation"),
            #[cfg(feature = "local-embeddings")]
            Self::RebuildEmbeddings {
                max_chunks,
                max_elapsed_ms,
                ..
            } if !(1..=1024).contains(max_chunks) || !(1..=300_000).contains(max_elapsed_ms) => {
                Err("invalid embedding rebuild budget")
            }
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
            Self::ChatGpt { operation } => operation.validate(),
            Self::ChatGptModels { binding } => crate::host::chatgpt::validate_binding(binding),
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
    SettingsGrantCurrent {
        head: Option<Box<crate::host::settings::SettingsGrantHead>>,
    },
    SettingsGrantVersion {
        grant: Option<Box<crate::host::settings::SettingsGrantVersion>>,
    },
    SettingsGrantReceipt {
        receipt: Option<crate::host::settings::SettingsGrantReceipt>,
    },
    SettingsOperation {
        operation: crate::host::settings::SettingsOperationInspection,
    },
    WorkerCoordinator {
        result: Box<crate::host::worker::WorkerCoordinatorQueryResult>,
    },
    Worker {
        result: crate::worker::commands::WorkerQueryResult,
    },
    Schedule {
        result: crate::scheduling::controls::ScheduleQueryResult,
    },
    ScheduleEngine {
        status: Box<crate::scheduling::engine::EngineStatus>,
    },
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
        settings: Box<crate::host::settings::HostSettings>,
    },
    Composition {
        status: Box<crate::host::composition::CompositionStatus>,
    },
    MemoryJob {
        job: crate::memory::consolidate::MaintenanceJob,
    },
    RetrievedMemory {
        excerpts: Vec<crate::context::MemoryExcerpt>,
        retrieval: crate::memory::search::RetrievalSummary,
        omitted_hits: usize,
    },
    #[cfg(feature = "local-embeddings")]
    EmbeddingIndex {
        status: Option<Box<crate::memory::vector::VectorStatus>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_use: Option<crate::agent::MemoryUse>,
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
            memory_use: information.memory_use,
        }
    }
}

impl OwnerQueryResult {
    pub fn validate_for(&self, query: &OwnerQuery) -> Result<(), &'static str> {
        let valid = match (query, self) {
            (
                OwnerQuery::SettingsGrantCurrent { device_id },
                Self::SettingsGrantCurrent { head },
            ) => head.as_ref().is_none_or(|head| {
                head.validate().is_ok() && head.grant.change.peer_keys.device_id == *device_id
            }),
            (
                OwnerQuery::SettingsGrantVersion {
                    device_id,
                    revision,
                },
                Self::SettingsGrantVersion { grant },
            ) => grant.as_ref().is_none_or(|grant| {
                grant.validate().is_ok()
                    && grant.change.peer_keys.device_id == *device_id
                    && grant.revision == *revision
            }),
            (
                OwnerQuery::SettingsGrantReceipt { request_id },
                Self::SettingsGrantReceipt { receipt },
            ) => receipt.as_ref().is_none_or(|receipt| {
                receipt.validate().is_ok() && receipt.owner_request_id == *request_id
            }),
            (
                OwnerQuery::SettingsOperation { request_id },
                Self::SettingsOperation { operation },
            ) => operation.binding.request_id == *request_id && operation.validate().is_ok(),
            (OwnerQuery::WorkerCoordinator { query }, Self::WorkerCoordinator { result }) => {
                result.validate_for(query).is_ok()
            }
            (OwnerQuery::Worker { query }, Self::Worker { result }) => {
                result.validate_for(query).is_ok()
            }
            (OwnerQuery::Schedule { query }, Self::Schedule { result }) => {
                schedule_query_matches(query, result)
            }
            (OwnerQuery::ScheduleEngine, Self::ScheduleEngine { .. }) => true,
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
            (OwnerQuery::Composition, Self::Composition { .. }) => true,
            (OwnerQuery::Publication, Self::Publication { .. }) => true,
            (OwnerQuery::MemoryJob { job_id }, Self::MemoryJob { job }) => job.id == *job_id,
            (OwnerQuery::RetrieveMemory { .. }, Self::RetrievedMemory { excerpts, .. }) => {
                excerpts.len() <= 16
            }
            #[cfg(feature = "local-embeddings")]
            (OwnerQuery::EmbeddingIndex, Self::EmbeddingIndex { .. }) => true,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_text: Option<crate::scheduler::LiveText>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OwnerOperationResult {
    SettingsGrantCommitted {
        receipt: crate::host::settings::SettingsGrantReceipt,
        replayed: bool,
    },
    SettingsUncertaintyAcknowledged {
        acknowledgement: crate::host::settings::agent::SettingsUncertaintyAcknowledgement,
    },
    WorkerCoordinator {
        result: Box<crate::host::worker::WorkerCoordinatorOperationResult>,
    },
    Schedule {
        result: crate::scheduling::controls::ScheduleMutationResult,
    },
    ScheduledUnknownAcknowledged {
        acknowledgement: crate::scheduling::acknowledgements::UnknownAcknowledgement,
    },
    SelectionPrepared {
        preparation: Box<crate::host::selection::PreparedSelectionTicket>,
    },
    SelectionApproved {
        ticket: uuid::Uuid,
        selection: crate::host::selection::SelectionSummary,
    },
    MemoryJob {
        job: crate::memory::consolidate::MaintenanceJob,
    },
    #[cfg(feature = "local-embeddings")]
    EmbeddingsRebuilt {
        report: crate::memory::search::BuildReport,
    },
    Command {
        receipt: CommandReceipt,
    },
    Applied,
    SettingsReceipt {
        operation: Box<crate::host::settings::SettingsOperationInspection>,
        settings: Option<Box<crate::host::settings::HostSettings>>,
        restart_required: bool,
    },
    SettingsReconciled {
        operation: Box<crate::host::settings::SettingsOperationInspection>,
    },
    Settings {
        settings: Box<crate::host::settings::HostSettings>,
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
    #[serde(rename = "chatgpt")]
    ChatGpt {
        status: Box<crate::host::chatgpt::ChatGptStatus>,
    },
    #[serde(rename = "chatgpt_models")]
    ChatGptModels {
        binding: crate::host::settings::ModelCredentialReference,
        models: Vec<crate::host::chatgpt::ChatGptModel>,
    },
}

impl OwnerOperationResult {
    pub fn validate_for_request(
        &self,
        operation: &OwnerOperation,
        request_id: CommandId,
    ) -> Result<(), &'static str> {
        if let OwnerOperation::SetSettingsGrant { change } = operation
            && change.request_id != request_id
        {
            return Err("Settings grant requires its original owner request ID");
        }
        if let Self::SettingsReceipt { operation, .. } = self
            && (operation.binding.request_id != request_id
                || !matches!(
                    operation.origin,
                    crate::host::settings::SettingsOperationOrigin::LocalOwner { request_id: original }
                        if original == request_id
                ))
        {
            return Err("settings receipt does not match its original owner request");
        }
        if let (
            OwnerOperation::WorkerCoordinator { operation },
            Self::WorkerCoordinator { result },
        ) = (operation, self)
        {
            return result
                .validate_for(operation, request_id)
                .map_err(|_| "worker coordinator result does not match its request");
        }
        self.validate_for(operation)
    }
    pub fn validate_for(&self, operation: &OwnerOperation) -> Result<(), &'static str> {
        let valid = match (operation, self) {
            (
                OwnerOperation::SetSettingsGrant { change },
                Self::SettingsGrantCommitted { receipt, .. },
            ) => receipt.matches_change(change),
            (
                OwnerOperation::ReconcileSettings { operation_id },
                Self::SettingsReconciled { operation },
            ) => {
                operation
                    .validate()
                    .map_err(|_| "invalid reconciled settings inspection")?;
                operation.binding.request_id == *operation_id && operation.write.is_some()
            }
            (
                OwnerOperation::ReplaceSettings {
                    expected_revision,
                    settings: submitted,
                },
                Self::SettingsReceipt {
                    operation,
                    settings,
                    restart_required,
                },
            ) => {
                operation
                    .validate()
                    .map_err(|_| "invalid settings receipt inspection")?;
                let applied = operation.write.as_ref().is_some_and(|write| {
                    write.resolution
                        == Some(crate::host::settings::SettingsWriteResolution::Applied)
                });
                let expected_restart = applied
                    && operation
                        .write
                        .as_ref()
                        .is_some_and(|write| write.restart_required);
                let snapshot_matches = if let Some(settings) = settings {
                    let mut expected = submitted.as_ref().clone();
                    expected.revision = expected_revision
                        .checked_add(1)
                        .ok_or("settings revision overflow")?;
                    applied
                        && serde_json::to_value(settings)
                            .map_err(|_| "invalid settings receipt snapshot")?
                            == serde_json::to_value(expected)
                                .map_err(|_| "invalid original settings snapshot")?
                } else {
                    true
                };
                operation.binding.expected_revision == Some(*expected_revision)
                    && submitted.revision == *expected_revision
                    && matches!(
                        operation.binding.scope,
                        crate::host::settings::contract::SettingsScope::Host { .. }
                    )
                    && matches!(
                        operation.origin,
                        crate::host::settings::SettingsOperationOrigin::LocalOwner { .. }
                    )
                    && matches!(
                        operation.phase,
                        crate::host::settings::SettingsOperationPhase::Applying { .. }
                            | crate::host::settings::SettingsOperationPhase::Finished { .. }
                    )
                    && *restart_required == expected_restart
                    && snapshot_matches
            }
            (OwnerOperation::Schedule { mutation }, Self::Schedule { result }) => {
                schedule_mutation_matches(mutation, result)
            }
            (
                OwnerOperation::AcknowledgeScheduledUnknown { request },
                Self::ScheduledUnknownAcknowledged { acknowledgement },
            ) => acknowledgement.request == *request,
            (
                OwnerOperation::PrepareSelection { command },
                Self::SelectionPrepared { preparation },
            ) => command
                .get("command_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| {
                    id.parse::<CommandId>()
                        .is_ok_and(|id| id == preparation.binding.command_id)
                }),
            (
                OwnerOperation::ApproveSelection { ticket, .. },
                Self::SelectionApproved {
                    ticket: returned, ..
                },
            ) => ticket == returned,
            (OwnerOperation::DiscardSelection { .. }, Self::Applied) => true,
            (
                OwnerOperation::MemoryEnqueue { job_id, .. }
                | OwnerOperation::MemoryCancel { job_id }
                | OwnerOperation::MemoryRetry { job_id }
                | OwnerOperation::ApplyMemoryPlan { job_id, .. },
                Self::MemoryJob { job },
            ) => job.id == *job_id,
            #[cfg(feature = "local-embeddings")]
            (OwnerOperation::RebuildEmbeddings { .. }, Self::EmbeddingsRebuilt { .. }) => true,
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
                OwnerOperation::AcknowledgeSettingsUncertainty {
                    command_id,
                    exact_fence_sha256,
                    lost_ram_uncertainty_accepted,
                },
                Self::SettingsUncertaintyAcknowledged { acknowledgement },
            ) => {
                *lost_ram_uncertainty_accepted
                    && acknowledgement.command_id == *command_id
                    && acknowledgement.exact_fence_sha256 == *exact_fence_sha256
                    && acknowledgement.lost_ram_uncertainty_accepted
                    && !acknowledgement.owner_principal_id.is_nil()
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
                | OwnerOperation::RevokeWorkerPeer { .. }
                | OwnerOperation::RevokeDevice { .. }
                | OwnerOperation::WakeTask { .. }
                | OwnerOperation::RetryProvider { .. },
                Self::Applied,
            ) => true,
            (OwnerOperation::ResumeInterrupted { .. }, Self::Applied) => true,
            (
                OwnerOperation::ChatGpt {
                    operation:
                        crate::host::chatgpt::ChatGptOperation::Status { binding }
                        | crate::host::chatgpt::ChatGptOperation::SignIn { binding, .. }
                        | crate::host::chatgpt::ChatGptOperation::SignOut { binding },
                },
                Self::ChatGpt { status },
            ) => status.binding == *binding,
            (OwnerOperation::ChatGpt { .. }, Self::ChatGpt { status }) => status.attempt.is_some(),
            (
                OwnerOperation::ChatGptModels { binding },
                Self::ChatGptModels {
                    binding: returned,
                    models,
                },
            ) => returned == binding && models.len() <= 1_024,
            _ => false,
        };
        if !valid {
            return Err("owner operation result does not match its request");
        }
        Ok(())
    }
}

fn schedule_query_matches(
    query: &crate::scheduling::controls::ScheduleQuery,
    result: &crate::scheduling::controls::ScheduleQueryResult,
) -> bool {
    use crate::scheduling::controls::{ScheduleQuery, ScheduleQueryResult};

    match (query, result) {
        (
            ScheduleQuery::List {
                after_id,
                enabled,
                include_deleted,
                limit,
            },
            ScheduleQueryResult::Jobs { jobs, next_after },
        ) => {
            jobs.len() <= *limit
                && jobs.windows(2).all(|pair| pair[0].id < pair[1].id)
                && jobs.iter().all(|job| {
                    after_id.is_none_or(|id| job.id > id)
                        && enabled.is_none_or(|value| job.enabled == value)
                        && (*include_deleted || job.deleted_at.is_none())
                })
                && next_after.is_none_or(|id| jobs.last().is_some_and(|job| job.id == id))
        }
        (ScheduleQuery::Show { job_id }, ScheduleQueryResult::Job { job }) => job.id == *job_id,
        (ScheduleQuery::Run { occurrence_id }, ScheduleQueryResult::Run { inspection }) => {
            inspection.run.occurrence_id == *occurrence_id
        }
        (
            ScheduleQuery::History {
                job_id,
                before,
                limit,
            },
            ScheduleQueryResult::History { runs, next_before },
        ) => {
            let key = |run: &crate::scheduling::controls::ScheduleRunInspection| {
                (run.run.claimed_at, run.run.occurrence_id)
            };
            runs.len() <= *limit
                && runs.windows(2).all(|pair| key(&pair[0]) > key(&pair[1]))
                && runs.iter().all(|run| {
                    run.run.job_id == *job_id
                        && before.is_none_or(|cursor| {
                            key(run) < (cursor.claimed_at, cursor.occurrence_id)
                        })
                })
                && next_before.is_none_or(|cursor| {
                    runs.last()
                        .is_some_and(|run| key(run) == (cursor.claimed_at, cursor.occurrence_id))
                })
        }
        _ => false,
    }
}

fn schedule_mutation_matches(
    mutation: &crate::scheduling::controls::ScheduleMutation,
    result: &crate::scheduling::controls::ScheduleMutationResult,
) -> bool {
    use crate::scheduling::controls::{ScheduleMutation, ScheduleMutationResult};

    match (mutation, result) {
        (ScheduleMutation::Create { job: requested }, ScheduleMutationResult::Job { job }) => {
            job.id == requested.id
                && job.name == requested.name
                && job.definition == requested.definition
                && job.enabled == requested.enabled
                && job.revision == 1
                && job.deleted_at.is_none()
        }
        (
            ScheduleMutation::Update {
                job: requested,
                expected_revision,
            },
            ScheduleMutationResult::Job { job },
        ) => {
            job.id == requested.id
                && job.name == requested.name
                && job.definition == requested.definition
                && job.enabled == requested.enabled
                && expected_revision.checked_add(1) == Some(job.revision)
                && job.deleted_at.is_none()
        }
        (
            ScheduleMutation::Disable {
                job_id,
                expected_revision,
            },
            ScheduleMutationResult::Job { job },
        ) => {
            job.id == *job_id
                && expected_revision.checked_add(1) == Some(job.revision)
                && !job.enabled
                && job.deleted_at.is_none()
        }
        (
            ScheduleMutation::Delete {
                job_id,
                expected_revision,
            },
            ScheduleMutationResult::Job { job },
        ) => {
            job.id == *job_id
                && expected_revision.checked_add(1) == Some(job.revision)
                && !job.enabled
                && job.deleted_at.is_some()
        }
        (ScheduleMutation::Run { claim }, ScheduleMutationResult::Run { run, .. }) => {
            run.job_id == claim.job_id
                && run.occurrence_id == claim.occurrence_id
                && run.trigger
                    == crate::scheduling::RunTrigger::Manual {
                        command_id: claim.command_id,
                    }
        }
        _ => false,
    }
}
