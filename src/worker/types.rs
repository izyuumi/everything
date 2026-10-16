use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::agent::{ConversationConfig, LoopLimits, NativeRunInfo};
use crate::context::{ContextBudget, ContextBuilder};
use crate::credentials::{CredentialError, CredentialKey};
use crate::protocol::{CommandId, ConversationId, DeviceId, TaskId, UnixMillis};
use crate::relay_protocol::RelayOrigin;

use super::WorkerError;

pub const MAX_WORKER_PACKET_BYTES: usize = 512 * 1024;
pub const MAX_WORKER_FILE_BYTES: usize = 256 * 1024;
pub const MAX_WORKER_FILES: usize = 64;
pub const WORKER_PURPOSE: &str = "everything.worker.v1";
const MAX_MANIFEST_BYTES: usize = 480 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    Coordinator,
    Executor,
}

impl WorkerRole {
    pub fn counterpart(self) -> Self {
        match self {
            Self::Coordinator => Self::Executor,
            Self::Executor => Self::Coordinator,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerModelBinding {
    pub configuration: ConversationConfig,
    pub credential_reference: Uuid,
}

impl WorkerModelBinding {
    pub fn validate(&self) -> Result<(), WorkerError> {
        self.configuration.validate()?;
        if self.credential_reference.is_nil() {
            return Err(WorkerError::Invalid(
                "worker credential reference must not be nil",
            ));
        }
        Ok(())
    }

    pub fn credential_key(&self, executor: DeviceId) -> Result<CredentialKey, CredentialError> {
        self.validate().map_err(|_| CredentialError::InvalidKey)?;
        let value =
            serde_json::to_vec(&(executor, self)).map_err(|_| CredentialError::InvalidKey)?;
        CredentialKey::new(
            "ai.everything.worker.models",
            &digest(b"everything.worker.credential.v1\0", &value),
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerJobIds {
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub create_command_id: CommandId,
    pub input_command_id: CommandId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerParentBinding {
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub command_id: CommandId,
    pub checkpoint_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFile {
    pub path: String,
    pub sha256: String,
    pub contents_base64: String,
}

impl WorkerFile {
    pub fn from_bytes(path: String, contents: &[u8]) -> Result<Self, WorkerError> {
        relative_path(&path)?;
        if contents.len() > MAX_WORKER_FILE_BYTES {
            return Err(WorkerError::Capacity("file bytes"));
        }
        Ok(Self {
            path,
            sha256: digest(b"", contents),
            contents_base64: STANDARD.encode(contents),
        })
    }

    pub fn decode(&self) -> Result<Vec<u8>, WorkerError> {
        relative_path(&self.path)?;
        valid_digest(&self.sha256)?;
        if self.contents_base64.len() > MAX_WORKER_FILE_BYTES.div_ceil(3) * 4 {
            return Err(WorkerError::Capacity("encoded file bytes"));
        }
        let contents = STANDARD
            .decode(&self.contents_base64)
            .map_err(|_| WorkerError::Invalid("file content is not canonical base64"))?;
        if contents.len() > MAX_WORKER_FILE_BYTES
            || STANDARD.encode(&contents) != self.contents_base64
        {
            return Err(WorkerError::Capacity("decoded file bytes or encoding"));
        }
        if digest(b"", &contents) != self.sha256 {
            return Err(WorkerError::Conflict("file content digest"));
        }
        Ok(contents)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerContext {
    pub label: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHardLimits {
    pub memory_bytes: Option<u64>,
    pub cpu_ms: Option<u64>,
    pub process_count: Option<u32>,
    pub disk_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerManifest {
    pub version: u16,
    pub delegation_id: Uuid,
    pub attempt_id: Uuid,
    pub coordinator: DeviceId,
    pub executor: DeviceId,
    pub relay_origin: RelayOrigin,
    pub created_at: UnixMillis,
    pub deadline: UnixMillis,
    pub ids: WorkerJobIds,
    pub parent: Option<WorkerParentBinding>,
    pub objective: String,
    pub constraints: Vec<String>,
    pub context: Vec<WorkerContext>,
    pub inputs: Vec<WorkerFile>,
    pub outputs: BTreeSet<String>,
    pub allowed_tools: BTreeSet<String>,
    pub mode: crate::permissions::ExecutionMode,
    pub model: WorkerModelBinding,
    pub budget: ContextBudget,
    pub limits: LoopLimits,
    pub hard_limits: Option<WorkerHardLimits>,
}

impl WorkerManifest {
    pub fn prepare(self) -> Result<PreparedDelegation, WorkerError> {
        let fingerprint = self.fingerprint()?;
        Ok(PreparedDelegation {
            manifest: self,
            fingerprint,
        })
    }

    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.version != 1
            || self.delegation_id.is_nil()
            || self.attempt_id.is_nil()
            || self.coordinator == self.executor
            || self.ids.create_command_id == self.ids.input_command_id
        {
            return Err(WorkerError::Invalid("worker manifest identity"));
        }
        if self.parent.is_some() {
            return Err(WorkerError::Unsupported(
                "active-parent delegation requires actual native scheduler accounting",
            ));
        }
        if self.hard_limits.is_some() {
            return Err(WorkerError::Unsupported(
                "hard OS resource guarantees are unavailable",
            ));
        }
        if self.mode == crate::permissions::ExecutionMode::FullHost
            || self.allowed_tools.iter().any(|tool| {
                !matches!(
                    tool.as_str(),
                    "read_file" | "write_file" | "search_files" | "search_tools"
                )
            })
            || self.allowed_tools.contains("write_file")
                && self.mode == crate::permissions::ExecutionMode::ReadOnly
        {
            return Err(WorkerError::Unsupported(
                "only explicit workspace file tools and offline discovery are supported",
            ));
        }
        self.model.validate()?;
        self.limits.validate()?;
        ContextBuilder::new(self.budget.clone())
            .map_err(|_| WorkerError::Invalid("worker context budget"))?;
        if self.limits.max_model_attempts > 32
            || self.limits.max_charged_tokens > 1_000_000
            || self.budget.max_context_tokens > 128_000
            || self.budget.max_request_bytes > 32 * 1024 * 1024
            || self.limits.max_tool_calls > 64
            || self.limits.absolute_deadline != Some(self.deadline)
            || self.deadline.get() <= self.created_at.get()
            || self.deadline.get() - self.created_at.get() > self.limits.max_duration_ms
            || self.limits.max_reserved_output_tokens.is_none_or(|tokens| {
                tokens < u64::from(self.model.configuration.max_output_tokens) || tokens > 1_000_000
            })
            || self.budget.reserved_output_tokens != self.model.configuration.max_output_tokens
        {
            return Err(WorkerError::Invalid("worker immutable execution bounds"));
        }
        bounded_text(&self.objective, 16 * 1024, false)?;
        if self.constraints.len() > 32 || self.context.len() > 32 {
            return Err(WorkerError::Capacity("task constraints or context entries"));
        }
        let mut context_bytes = 0_usize;
        for constraint in &self.constraints {
            bounded_text(constraint, 4096, false)?;
            context_bytes = context_bytes.saturating_add(constraint.len());
        }
        for context in &self.context {
            bounded_text(&context.label, 256, false)?;
            bounded_text(&context.text, 32 * 1024, false)?;
            context_bytes = context_bytes
                .saturating_add(context.label.len())
                .saturating_add(context.text.len());
        }
        if context_bytes > 64 * 1024 {
            return Err(WorkerError::Capacity("selected context bytes"));
        }
        validate_files(&self.inputs)?;
        if self.outputs.len() > MAX_WORKER_FILES {
            return Err(WorkerError::Capacity("output paths"));
        }
        for path in &self.outputs {
            relative_path(path)?;
        }
        self.prompt_bytes()?;
        encode_bounded(self, MAX_MANIFEST_BYTES)?;
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String, WorkerError> {
        self.validate()?;
        Ok(digest(
            b"everything.worker.manifest.v1\0",
            &encode_bounded(self, MAX_MANIFEST_BYTES)?,
        ))
    }

    pub fn instruction(&self) -> Result<String, WorkerError> {
        self.validate()?;
        String::from_utf8(self.prompt_bytes()?)
            .map_err(|_| WorkerError::Invalid("task text encoding"))
    }

    fn prompt_bytes(&self) -> Result<Vec<u8>, WorkerError> {
        #[derive(Serialize)]
        struct Input<'a> {
            objective: &'a str,
            constraints: &'a [String],
            selected_untrusted_context: &'a [WorkerContext],
            input_files: Vec<&'a str>,
            allowed_output_files: &'a BTreeSet<String>,
        }
        encode_bounded(
            &Input {
                objective: &self.objective,
                constraints: &self.constraints,
                selected_untrusted_context: &self.context,
                input_files: self.inputs.iter().map(|file| file.path.as_str()).collect(),
                allowed_output_files: &self.outputs,
            },
            120 * 1024,
        )
    }
}

pub struct PreparedDelegation {
    pub(super) manifest: WorkerManifest,
    pub(super) fingerprint: String,
}

impl PreparedDelegation {
    pub fn manifest(&self) -> &WorkerManifest {
        &self.manifest
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn approve(
        self,
        now: UnixMillis,
        lifetime: Duration,
    ) -> Result<ApprovedDelegation, WorkerError> {
        let lifetime = u64::try_from(lifetime.as_millis())
            .map_err(|_| WorkerError::Invalid("approval lifetime"))?;
        if lifetime == 0
            || lifetime > 300_000
            || now.get() < self.manifest.created_at.get()
            || now.get() >= self.manifest.deadline.get()
        {
            return Err(WorkerError::Expired);
        }
        let expires = now
            .get()
            .checked_add(lifetime)
            .ok_or(WorkerError::Expired)?
            .min(self.manifest.deadline.get());
        Ok(ApprovedDelegation {
            manifest: self.manifest,
            fingerprint: self.fingerprint,
            approved_at: now,
            expires_at: expires,
            deadline: tokio::time::Instant::now() + Duration::from_millis(expires - now.get()),
        })
    }
}

pub struct ApprovedDelegation {
    pub(super) manifest: WorkerManifest,
    pub(super) fingerprint: String,
    pub(super) approved_at: UnixMillis,
    pub(super) expires_at: u64,
    pub(super) deadline: tokio::time::Instant,
}

impl ApprovedDelegation {
    pub fn validate_for_submission(&self, now: UnixMillis) -> Result<(), WorkerError> {
        if now.get() < self.approved_at.get()
            || now.get() >= self.expires_at
            || tokio::time::Instant::now() >= self.deadline
        {
            return Err(WorkerError::Expired);
        }
        if self.manifest.fingerprint()? != self.fingerprint {
            return Err(WorkerError::Conflict("approved manifest"));
        }
        Ok(())
    }
}

pub struct StagedDelegation {
    pub(super) approved: ApprovedDelegation,
    pub(super) expires_at: UnixMillis,
}

impl StagedDelegation {
    pub fn manifest(&self) -> &WorkerManifest {
        &self.approved.manifest
    }

    pub fn binding(&self) -> WorkerJobBinding {
        WorkerJobBinding {
            delegation_id: self.approved.manifest.delegation_id,
            attempt_id: self.approved.manifest.attempt_id,
            manifest_digest: self.approved.fingerprint.clone(),
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.approved.fingerprint
    }

    pub fn approved_at(&self) -> UnixMillis {
        self.approved.approved_at
    }

    pub fn expires_at(&self) -> UnixMillis {
        self.expires_at
    }

    pub fn validate_for_admission(&self, now: UnixMillis) -> Result<(), WorkerError> {
        self.approved.validate_for_submission(now)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerResultStatus {
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerUsage {
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reserved_output_tokens: Option<u64>,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
}

impl From<&NativeRunInfo> for WorkerUsage {
    fn from(run: &NativeRunInfo) -> Self {
        Self {
            model_attempts: run.model_attempts,
            tool_calls: run.tool_calls,
            charged_tokens: run.charged_tokens,
            reserved_output_tokens: run.reserved_output_tokens,
            reported_input_tokens: run.reported_input_tokens,
            reported_output_tokens: run.reported_output_tokens,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResult {
    pub delegation_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: String,
    pub status: WorkerResultStatus,
    pub summary: String,
    pub artifacts: Vec<WorkerFile>,
    pub usage: WorkerUsage,
    pub finished_at: UnixMillis,
}

impl WorkerResult {
    pub fn validate(&self, manifest: &WorkerManifest) -> Result<(), WorkerError> {
        if self.delegation_id != manifest.delegation_id
            || self.attempt_id != manifest.attempt_id
            || self.manifest_digest != manifest.fingerprint()?
        {
            return Err(WorkerError::Conflict("result identity"));
        }
        if self.finished_at.get() < manifest.created_at.get() {
            return Err(WorkerError::Invalid("result predates the original job"));
        }
        bounded_text(&self.summary, 8192, true)?;
        validate_files(&self.artifacts)?;
        if self
            .artifacts
            .iter()
            .any(|file| !manifest.outputs.contains(&file.path))
        {
            return Err(WorkerError::Invalid(
                "result contains an unapproved artifact",
            ));
        }
        if self.usage.model_attempts > manifest.limits.max_model_attempts
            || self.usage.tool_calls > manifest.limits.max_tool_calls
        {
            return Err(WorkerError::Invalid(
                "worker result exceeds captured operation counts",
            ));
        }
        encode_bounded(self, MAX_MANIFEST_BYTES)?;
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<String, WorkerError> {
        Ok(digest(
            b"everything.worker.result.v1\0",
            &encode_bounded(self, MAX_MANIFEST_BYTES)?,
        ))
    }
}

pub(super) fn relative_path(value: &str) -> Result<(), WorkerError> {
    if value.is_empty()
        || value.len() > 1024
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err(WorkerError::Invalid(
            "worker path must be a bounded ordinary relative path",
        ));
    }
    let mut canonical = Vec::new();
    for component in Path::new(value).components() {
        let Component::Normal(name) = component else {
            return Err(WorkerError::Invalid("worker path contains traversal"));
        };
        let name = name
            .to_str()
            .ok_or(WorkerError::Invalid("worker path is not UTF-8"))?;
        if name.eq_ignore_ascii_case(".everything-agents")
            || name.eq_ignore_ascii_case(".everything-workspace.json")
        {
            return Err(WorkerError::Invalid(
                "worker path uses reserved control names",
            ));
        }
        canonical.push(name);
    }
    if canonical.len() > 16 || canonical.join("/") != value {
        return Err(WorkerError::Invalid("worker path is not normalized"));
    }
    Ok(())
}

pub(super) fn validate_files(files: &[WorkerFile]) -> Result<(), WorkerError> {
    if files.len() > MAX_WORKER_FILES {
        return Err(WorkerError::Capacity("file count"));
    }
    let mut bytes = 0_usize;
    let mut names = BTreeSet::new();
    for file in files {
        if !names.insert(&file.path) {
            return Err(WorkerError::Conflict("duplicate file path"));
        }
        bytes = bytes.saturating_add(file.decode()?.len());
        if bytes > MAX_WORKER_FILE_BYTES {
            return Err(WorkerError::Capacity("aggregate file bytes"));
        }
    }
    Ok(())
}

pub(super) fn bounded_text(value: &str, limit: usize, empty: bool) -> Result<(), WorkerError> {
    if value.len() > limit || !empty && value.trim().is_empty() || value.contains('\0') {
        return Err(WorkerError::Invalid(
            "worker text is empty, oversized or contains NUL",
        ));
    }
    Ok(())
}

pub(super) fn valid_digest(value: &str) -> Result<(), WorkerError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkerError::Invalid("invalid SHA-256 digest"));
    }
    Ok(())
}

pub(super) fn encode_bounded<T: Serialize>(
    value: &T,
    limit: usize,
) -> Result<Vec<u8>, WorkerError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > limit {
        return Err(WorkerError::Capacity("serialized payload"));
    }
    Ok(bytes)
}

pub(super) fn digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerJobBinding {
    pub delegation_id: Uuid,
    pub attempt_id: Uuid,
    pub manifest_digest: String,
}

impl WorkerJobBinding {
    pub fn from_manifest(manifest: &WorkerManifest) -> Result<Self, WorkerError> {
        Ok(Self {
            delegation_id: manifest.delegation_id,
            attempt_id: manifest.attempt_id,
            manifest_digest: manifest.fingerprint()?,
        })
    }
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.delegation_id.is_nil() || self.attempt_id.is_nil() {
            return Err(WorkerError::Invalid("job binding identifiers"));
        }
        valid_digest(&self.manifest_digest)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRejection {
    UnsupportedPolicy,
    CredentialUnavailable,
    Capacity,
    Expired,
    Revoked,
    Configuration,
    CancelledBeforeAcceptance,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerMessage {
    Offer {
        manifest: Box<WorkerManifest>,
    },
    Accepted {
        binding: WorkerJobBinding,
    },
    Rejected {
        binding: WorkerJobBinding,
        reason: WorkerRejection,
    },
    Cancel {
        binding: WorkerJobBinding,
        request_id: CommandId,
    },
    CancellationAccepted {
        binding: WorkerJobBinding,
        request_id: CommandId,
    },
    Result {
        result: Box<WorkerResult>,
    },
    ResultAcknowledged {
        binding: WorkerJobBinding,
        result_digest: String,
        receipt_id: CommandId,
    },
    Revoke {
        request_id: CommandId,
        revoked_at: UnixMillis,
    },
}

impl WorkerMessage {
    pub fn binding(&self) -> Result<Option<WorkerJobBinding>, WorkerError> {
        match self {
            Self::Offer { manifest } => WorkerJobBinding::from_manifest(manifest).map(Some),
            Self::Accepted { binding }
            | Self::Rejected { binding, .. }
            | Self::Cancel { binding, .. }
            | Self::CancellationAccepted { binding, .. }
            | Self::ResultAcknowledged { binding, .. } => Ok(Some(binding.clone())),
            Self::Result { result } => Ok(Some(WorkerJobBinding {
                delegation_id: result.delegation_id,
                attempt_id: result.attempt_id,
                manifest_digest: result.manifest_digest.clone(),
            })),
            Self::Revoke { .. } => Ok(None),
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Offer { .. } => "offer",
            Self::Accepted { .. } => "accepted",
            Self::Rejected { .. } => "rejected",
            Self::Cancel { .. } => "cancel",
            Self::CancellationAccepted { .. } => "cancellation_accepted",
            Self::Result { .. } => "result",
            Self::ResultAcknowledged { .. } => "result_acknowledged",
            Self::Revoke { .. } => "revoke",
        }
    }
    pub fn sender_role(&self) -> Option<WorkerRole> {
        match self {
            Self::Offer { .. } | Self::Cancel { .. } | Self::ResultAcknowledged { .. } => {
                Some(WorkerRole::Coordinator)
            }
            Self::Accepted { .. }
            | Self::Rejected { .. }
            | Self::CancellationAccepted { .. }
            | Self::Result { .. } => Some(WorkerRole::Executor),
            Self::Revoke { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPacket {
    pub version: u16,
    pub purpose: String,
    pub sender_role: WorkerRole,
    pub sender: DeviceId,
    pub recipient: DeviceId,
    pub body: WorkerMessage,
}

impl WorkerPacket {
    pub fn parse(bytes: &[u8]) -> Result<Self, WorkerError> {
        if bytes.is_empty() || bytes.len() > MAX_WORKER_PACKET_BYTES {
            return Err(WorkerError::Capacity("worker packet bytes"));
        }
        let packet: Self = serde_json::from_slice(bytes)
            .map_err(|_| WorkerError::Invalid("worker packet encoding"))?;
        packet.validate()?;
        Ok(packet)
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, WorkerError> {
        self.validate()?;
        encode_bounded(self, MAX_WORKER_PACKET_BYTES)
    }
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.version != 1
            || self.purpose != WORKER_PURPOSE
            || self.sender == self.recipient
            || self
                .body
                .sender_role()
                .is_some_and(|role| role != self.sender_role)
        {
            return Err(WorkerError::Invalid(
                "worker packet purpose, role or identities",
            ));
        }
        if let Some(binding) = self.body.binding()? {
            binding.validate()?;
        }
        match &self.body {
            WorkerMessage::Offer { manifest } => {
                manifest.validate()?;
                if manifest.coordinator != self.sender || manifest.executor != self.recipient {
                    return Err(WorkerError::Conflict("offer endpoints"));
                }
            }
            WorkerMessage::Result { result } => {
                bounded_text(&result.summary, 8192, true)?;
                validate_files(&result.artifacts)?;
            }
            WorkerMessage::ResultAcknowledged { result_digest, .. } => valid_digest(result_digest)?,
            _ => {}
        }
        Ok(())
    }
}
