pub(super) mod service;

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write};
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::agent::{LoopLimits, RemoteResultInspection};
use crate::context::{ContextBudget, ContextBuilder};
use crate::crypto::DevicePublicKeys;
use crate::ipc::{CommandReceipt, RelayStatus};
use crate::permissions::ExecutionMode;
use crate::protocol::{
    CommandId, ConversationId, DeviceId, RemoteResultSource, RemoteTaskOutcome, TaskId, TaskStatus,
    UnixMillis,
};
use crate::relay_protocol::RelayOrigin;
use crate::scheduler::{ExecutionPolicy, RemoteBudgetCharge, RemoteTaskPhase};
use crate::worker::{
    DelegationState, MAX_WORKER_FILES, WorkerContext, WorkerError, WorkerJobBinding,
    WorkerManifest, WorkerModelBinding, WorkerPeerState, WorkerRejection,
};

pub const MAX_WORKER_COORDINATOR_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_WORKER_COORDINATOR_DISCLOSURE_BYTES: usize = 768 * 1024;
pub const MAX_WORKER_COORDINATOR_INSPECTION_BYTES: usize = 256 * 1024;
const MAX_LOCAL_EXECUTION_BYTES: usize = 32 * 1024;
const MAX_TICKET_LIFETIME_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCoordinatorSettings {
    pub relay_origin: RelayOrigin,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPreparationRequest {
    pub executor: DeviceId,
    pub workspace_root: PathBuf,
    pub objective: String,
    pub constraints: Vec<String>,
    pub context: Vec<WorkerContext>,
    pub input_paths: Vec<String>,
    pub outputs: BTreeSet<String>,
    pub mode: ExecutionMode,
    pub model: WorkerModelBinding,
    #[serde(deserialize_with = "deserialize_request_budget")]
    pub budget: ContextBudget,
    pub limits: LoopLimits,
    pub deadline: UnixMillis,
    pub allowed_tools: BTreeSet<String>,
}

impl fmt::Debug for WorkerPreparationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerPreparationRequest")
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl WorkerPreparationRequest {
    pub fn validate(&self) -> Result<(), WorkerError> {
        workspace_shape(&self.workspace_root)?;
        text_shape(&self.objective, 16 * 1024)?;
        if self.constraints.len() > 32
            || self.context.len() > 32
            || self.input_paths.len() > MAX_WORKER_FILES
            || self.outputs.len() > MAX_WORKER_FILES
            || self.allowed_tools.len() > 128
        {
            return Err(WorkerError::Capacity("worker preparation entries"));
        }
        for constraint in &self.constraints {
            text_shape(constraint, 4096)?;
        }
        for context in &self.context {
            text_shape(&context.label, 256)?;
            text_shape(&context.text, 32 * 1024)?;
        }
        let mut inputs = BTreeSet::new();
        for path in &self.input_paths {
            text_shape(path, 4096)?;
            if !inputs.insert(path) {
                return Err(WorkerError::Conflict("duplicate preparation input path"));
            }
        }
        for path in &self.outputs {
            text_shape(path, 4096)?;
        }
        for tool in &self.allowed_tools {
            if tool.is_empty()
                || tool.len() > 128
                || !tool
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            {
                return Err(WorkerError::Invalid("worker preparation tool name"));
            }
        }
        self.model.validate()?;
        self.limits.validate()?;
        ContextBuilder::new(self.budget.clone())
            .map_err(|_| WorkerError::Invalid("worker preparation context budget"))?;
        if self.limits.absolute_deadline != Some(self.deadline) {
            return Err(WorkerError::Conflict("worker preparation deadline"));
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_REQUEST_BYTES)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBudget {
    max_context_tokens: u64,
    reserved_output_tokens: u32,
    safety_margin_tokens: u64,
    max_request_bytes: usize,
    max_compaction_bytes: usize,
}

fn deserialize_request_budget<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<ContextBudget, D::Error> {
    let value = RequestBudget::deserialize(deserializer)?;
    Ok(ContextBudget {
        max_context_tokens: value.max_context_tokens,
        reserved_output_tokens: value.reserved_output_tokens,
        safety_margin_tokens: value.safety_margin_tokens,
        max_request_bytes: value.max_request_bytes,
        max_compaction_bytes: value.max_compaction_bytes,
    })
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLocalPlan {
    pub conversation_id: ConversationId,
    pub create_command_id: CommandId,
    pub input_command_id: CommandId,
    pub task_id: TaskId,
    pub title: String,
    pub execution: ExecutionPolicy,
}

impl fmt::Debug for WorkerLocalPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerLocalPlan")
            .field("conversation_id", &self.conversation_id)
            .field("task_id", &self.task_id)
            .finish_non_exhaustive()
    }
}

impl WorkerLocalPlan {
    pub fn validate(&self) -> Result<(), WorkerError> {
        text_shape(&self.title, 4096)?;
        self.execution.validate()?;
        let ids = [
            *self.conversation_id.as_uuid(),
            *self.create_command_id.as_uuid(),
            *self.input_command_id.as_uuid(),
            *self.task_id.as_uuid(),
        ];
        if ids.into_iter().collect::<BTreeSet<_>>().len() != ids.len() {
            return Err(WorkerError::Conflict(
                "worker local identities must be distinct",
            ));
        }
        encoded_bound(&self.execution, MAX_LOCAL_EXECUTION_BYTES)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerPreparation {
    pub ticket: Uuid,
    pub prepared_at: UnixMillis,
    pub expires_at: UnixMillis,
    pub manifest_digest: String,
    pub executor_keys: DevicePublicKeys,
    pub executor_fingerprint: String,
    pub manifest: WorkerManifest,
    pub local: WorkerLocalPlan,
}

impl fmt::Debug for WorkerPreparation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerPreparation")
            .field("ticket", &self.ticket)
            .field("manifest_digest", &self.manifest_digest)
            .finish_non_exhaustive()
    }
}

impl WorkerPreparation {
    pub fn validate(&self) -> Result<(), WorkerError> {
        non_nil(self.ticket)?;
        self.executor_keys.validate()?;
        self.local.validate()?;
        let manifest = &self.manifest;
        if self.manifest_digest != manifest.fingerprint()?
            || self.executor_keys.device_id != manifest.executor
            || self.executor_fingerprint != self.executor_keys.fingerprint()
        {
            return Err(WorkerError::Conflict(
                "worker preparation manifest or executor",
            ));
        }
        if self.prepared_at < manifest.created_at
            || self.expires_at <= self.prepared_at
            || self.expires_at.get() - self.prepared_at.get() > MAX_TICKET_LIFETIME_MS
            || self.expires_at > manifest.deadline
        {
            return Err(WorkerError::Invalid("worker preparation ticket interval"));
        }
        let local = &self.local.execution;
        if local.mode != manifest.mode
            || local.allowed_tools.as_ref() != Some(&manifest.allowed_tools)
            || local.tools_enabled == manifest.allowed_tools.is_empty()
            || local.limits != manifest.limits
            || !same_budget(&local.budget, &manifest.budget)
            || manifest.allowed_tools.is_empty() && manifest.limits.max_tool_calls != 0
        {
            return Err(WorkerError::Conflict("worker local and manifest policy"));
        }
        let ids = [
            *self.local.conversation_id.as_uuid(),
            *self.local.create_command_id.as_uuid(),
            *self.local.input_command_id.as_uuid(),
            *self.local.task_id.as_uuid(),
            *manifest.ids.conversation_id.as_uuid(),
            *manifest.ids.create_command_id.as_uuid(),
            *manifest.ids.input_command_id.as_uuid(),
            *manifest.ids.task_id.as_uuid(),
        ];
        if ids.into_iter().collect::<BTreeSet<_>>().len() != ids.len() {
            return Err(WorkerError::Conflict(
                "local and worker identities must be separate",
            ));
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_DISCLOSURE_BYTES)
    }

    pub fn validate_for(&self, request: &WorkerPreparationRequest) -> Result<(), WorkerError> {
        request.validate()?;
        self.validate()?;
        let manifest = &self.manifest;
        let same_context = manifest.context.len() == request.context.len()
            && manifest
                .context
                .iter()
                .zip(&request.context)
                .all(|(left, right)| left.label == right.label && left.text == right.text);
        if manifest.executor != request.executor
            || self.local.execution.workspace_root != request.workspace_root
            || manifest.objective != request.objective
            || manifest.constraints != request.constraints
            || !same_context
            || !manifest
                .inputs
                .iter()
                .map(|file| &file.path)
                .eq(request.input_paths.iter())
            || manifest.outputs != request.outputs
            || manifest.mode != request.mode
            || manifest.model != request.model
            || !same_budget(&manifest.budget, &request.budget)
            || manifest.limits != request.limits
            || manifest.deadline != request.deadline
            || manifest.allowed_tools != request.allowed_tools
        {
            return Err(WorkerError::Conflict(
                "worker disclosure differs from preparation request",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerNativeInspection {
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub command_id: CommandId,
    pub status: TaskStatus,
    pub phase: RemoteTaskPhase,
    pub charge: RemoteBudgetCharge,
    pub admitted_at: UnixMillis,
    pub last_dispatch_at: Option<UnixMillis>,
    pub result: Option<RemoteResultInspection>,
}

impl WorkerNativeInspection {
    pub fn validate(&self) -> Result<(), WorkerError> {
        if !(1..=32).contains(&self.charge.model_attempts)
            || self.charge.tool_calls > 64
            || !(1..=1_000_000).contains(&self.charge.charged_tokens)
            || !(1..=1_000_000).contains(&self.charge.reserved_output_tokens)
            || self
                .last_dispatch_at
                .is_some_and(|at| at < self.admitted_at)
        {
            return Err(WorkerError::Invalid(
                "worker native inspection charge or time",
            ));
        }
        match &self.result {
            None if self.status == TaskStatus::WaitingForChildren
                && matches!(
                    self.phase,
                    RemoteTaskPhase::Reserved | RemoteTaskPhase::CancellationRequested
                ) => {}
            Some(result) => {
                result.validate(self.conversation_id)?;
                let report = &result.report;
                if report.task_id != self.task_id
                    || report.command_id != self.command_id
                    || result.task_status != self.status
                    || report.accepted_at < self.admitted_at
                    || report.usage.model_attempts > self.charge.model_attempts
                    || report.usage.tool_calls > self.charge.tool_calls
                {
                    return Err(WorkerError::Conflict("worker native result inspection"));
                }
                let expected = if report.outcome == RemoteTaskOutcome::Unknown {
                    RemoteTaskPhase::OutcomeUnknown
                } else {
                    RemoteTaskPhase::Settled
                };
                if self.phase != expected {
                    return Err(WorkerError::Conflict("worker native result phase"));
                }
            }
            None => return Err(WorkerError::Conflict("worker native unresolved phase")),
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_INSPECTION_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerJobInspection {
    pub binding: WorkerJobBinding,
    pub peer: DeviceId,
    pub state: DelegationState,
    pub cancellation_requested: bool,
    pub cancellation_acknowledged: bool,
    pub rejection: Option<WorkerRejection>,
    pub result_digest: Option<String>,
    pub quarantined: bool,
    pub receipt_id: Option<CommandId>,
    pub native: Option<WorkerNativeInspection>,
}

impl WorkerJobInspection {
    pub fn validate(&self) -> Result<(), WorkerError> {
        self.binding.validate()?;
        cancellation_shape(
            self.state,
            self.cancellation_requested,
            self.cancellation_acknowledged,
        )?;
        if let Some(digest) = &self.result_digest {
            digest_shape(digest)?;
        }
        let valid_state = match self.state {
            DelegationState::Offered
            | DelegationState::Accepted
            | DelegationState::Running
            | DelegationState::CancelRequested => {
                self.rejection.is_none()
                    && self.result_digest.is_none()
                    && self.receipt_id.is_none()
            }
            DelegationState::ResultReady => {
                self.rejection.is_none()
                    && self.result_digest.is_some()
                    && self.receipt_id.is_none()
            }
            DelegationState::Acknowledged => {
                self.rejection.is_none()
                    && self.result_digest.is_some()
                    && self.receipt_id.is_some()
            }
            DelegationState::Rejected => {
                self.rejection.is_some()
                    && self.result_digest.is_none()
                    && self.receipt_id.is_none()
            }
            DelegationState::Revoked => {
                !(self.rejection.is_some() && self.result_digest.is_some())
                    && self.receipt_id.is_none()
            }
        };
        if !valid_state || self.quarantined && self.result_digest.is_none() {
            return Err(WorkerError::Conflict("worker job terminal metadata"));
        }
        if let Some(native) = &self.native {
            native.validate()?;
            if let Some(result) = &native.result {
                let report = &result.report;
                if report.delegation_id != self.binding.delegation_id
                    || report.attempt_id != self.binding.attempt_id
                    || report.manifest_digest != self.binding.manifest_digest
                    || report.executor != self.peer
                {
                    return Err(WorkerError::Conflict("worker report job identity"));
                }
                match &report.source {
                    RemoteResultSource::Result { result_digest }
                        if self.result_digest.as_ref() == Some(result_digest) => {}
                    RemoteResultSource::Rejected { reason } => {
                        let rejection = self
                            .rejection
                            .as_ref()
                            .ok_or(WorkerError::Conflict("worker accepted rejection source"))?;
                        if serde_json::to_value(rejection)?.as_str() != Some(reason.as_str()) {
                            return Err(WorkerError::Conflict("worker accepted rejection reason"));
                        }
                    }
                    _ => return Err(WorkerError::Conflict("worker accepted report source")),
                }
                if self
                    .receipt_id
                    .is_some_and(|receipt| receipt != result.receipt_command_id)
                {
                    return Err(WorkerError::Conflict("worker result receipt identity"));
                }
            } else if self.receipt_id.is_some() {
                return Err(WorkerError::Conflict(
                    "worker acknowledgement lacks native result",
                ));
            }
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_INSPECTION_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerJobSummary {
    pub binding: WorkerJobBinding,
    pub peer: DeviceId,
    pub state: DelegationState,
    pub cancellation_requested: bool,
    pub cancellation_acknowledged: bool,
    pub local_task_id: Option<TaskId>,
    pub local_phase: Option<RemoteTaskPhase>,
}

impl WorkerJobSummary {
    pub fn validate(&self) -> Result<(), WorkerError> {
        self.binding.validate()?;
        cancellation_shape(
            self.state,
            self.cancellation_requested,
            self.cancellation_acknowledged,
        )?;
        if self.local_task_id.is_some() != self.local_phase.is_some() {
            return Err(WorkerError::Conflict("worker local summary identity"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCoordinatorStatus {
    pub enabled: bool,
    pub accepting: bool,
    pub draining: bool,
    pub relay: RelayStatus,
    pub admission_blocked: bool,
    pub preparations: usize,
    pub in_flight: usize,
    pub jobs: Option<usize>,
    pub unresolved_remote_tasks: Option<usize>,
    pub failure_code: Option<String>,
}

impl WorkerCoordinatorStatus {
    pub fn validate(&self) -> Result<(), WorkerError> {
        if self.preparations > 4
            || self.in_flight > 4
            || self.jobs.is_some_and(|jobs| jobs > 256)
            || self
                .unresolved_remote_tasks
                .is_some_and(|tasks| tasks > 4096)
            || self
                .jobs
                .zip(self.unresolved_remote_tasks)
                .is_some_and(|(jobs, tasks)| tasks > jobs)
            || self.jobs.is_some() != self.unresolved_remote_tasks.is_some()
            || self.accepting && self.jobs.is_none()
            || self.accepting && (!self.enabled || self.draining)
            || self.enabled && self.relay.origin.is_none()
            || self.failure_code.as_ref().is_some_and(|code| {
                code.is_empty()
                    || code.len() > 128
                    || !code
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            })
        {
            return Err(WorkerError::Invalid("worker coordinator status"));
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_INSPECTION_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCoordinatorQuery {
    Status,
    Identity,
    Peers,
    Jobs {
        after_delegation_id: Option<Uuid>,
        limit: usize,
    },
    Delegation {
        delegation_id: Uuid,
    },
    Task {
        task_id: TaskId,
    },
}

impl WorkerCoordinatorQuery {
    pub fn validate(&self) -> Result<(), WorkerError> {
        match self {
            Self::Jobs {
                after_delegation_id,
                limit,
            } => {
                if !(1..=32).contains(limit) {
                    return Err(WorkerError::Invalid("worker page size"));
                }
                if let Some(id) = after_delegation_id {
                    non_nil(*id)?;
                }
            }
            Self::Delegation { delegation_id } => non_nil(*delegation_id)?,
            _ => {}
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_REQUEST_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCoordinatorOperation {
    PinPeer {
        keys: DevicePublicKeys,
        expected_fingerprint: String,
    },
    Prepare {
        request: Box<WorkerPreparationRequest>,
    },
    Discard {
        ticket: Uuid,
    },
    Delegate {
        ticket: Uuid,
        expected_manifest_digest: String,
        expected_executor_fingerprint: String,
    },
    Cancel {
        task_id: TaskId,
        binding: WorkerJobBinding,
    },
    RevokePeer {
        device_id: DeviceId,
    },
}

impl WorkerCoordinatorOperation {
    pub fn validate(&self) -> Result<(), WorkerError> {
        match self {
            Self::PinPeer {
                keys,
                expected_fingerprint,
            } => {
                keys.validate()?;
                if *expected_fingerprint != keys.fingerprint() {
                    return Err(WorkerError::Conflict("worker pin fingerprint"));
                }
            }
            Self::Prepare { request } => request.validate()?,
            Self::Discard { ticket } => non_nil(*ticket)?,
            Self::Delegate {
                ticket,
                expected_manifest_digest,
                expected_executor_fingerprint,
            } => {
                non_nil(*ticket)?;
                digest_shape(expected_manifest_digest)?;
                fingerprint_shape(expected_executor_fingerprint)?;
            }
            Self::Cancel { binding, .. } => binding.validate()?,
            Self::RevokePeer { .. } => {}
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_REQUEST_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCoordinatorQueryResult {
    Status {
        status: WorkerCoordinatorStatus,
    },
    Identity {
        keys: DevicePublicKeys,
        fingerprint: String,
    },
    Peers {
        peers: Vec<WorkerPeerState>,
    },
    Jobs {
        jobs: Vec<WorkerJobSummary>,
        next_after: Option<Uuid>,
    },
    Delegation {
        job: Option<WorkerJobInspection>,
    },
    Task {
        task: WorkerNativeInspection,
    },
}

impl WorkerCoordinatorQueryResult {
    pub fn validate_for(&self, query: &WorkerCoordinatorQuery) -> Result<(), WorkerError> {
        query.validate()?;
        match (query, self) {
            (WorkerCoordinatorQuery::Status, Self::Status { status }) => status.validate()?,
            (WorkerCoordinatorQuery::Identity, Self::Identity { keys, fingerprint }) => {
                keys.validate()?;
                if *fingerprint != keys.fingerprint() {
                    return Err(WorkerError::Conflict("worker identity fingerprint"));
                }
            }
            (WorkerCoordinatorQuery::Peers, Self::Peers { peers }) => {
                if peers.len() > 16 {
                    return Err(WorkerError::Capacity("worker peers"));
                }
                let mut ids = BTreeSet::new();
                for peer in peers {
                    peer_shape(peer)?;
                    if !ids.insert(peer.keys.device_id) {
                        return Err(WorkerError::Conflict("duplicate worker peer"));
                    }
                }
            }
            (
                WorkerCoordinatorQuery::Jobs {
                    after_delegation_id,
                    limit,
                },
                Self::Jobs { jobs, next_after },
            ) => {
                if jobs.len() > *limit {
                    return Err(WorkerError::Capacity("worker job page"));
                }
                let mut cursor = *after_delegation_id;
                for job in jobs {
                    job.validate()?;
                    if cursor.is_some_and(|previous| job.binding.delegation_id <= previous) {
                        return Err(WorkerError::Conflict("worker job page order"));
                    }
                    cursor = Some(job.binding.delegation_id);
                }
                if next_after.is_some() && (jobs.is_empty() || *next_after != cursor) {
                    return Err(WorkerError::Conflict("worker job continuation"));
                }
            }
            (WorkerCoordinatorQuery::Delegation { delegation_id }, Self::Delegation { job }) => {
                if let Some(job) = job {
                    job.validate()?;
                    if job.binding.delegation_id != *delegation_id {
                        return Err(WorkerError::Conflict("worker delegation query identity"));
                    }
                }
            }
            (WorkerCoordinatorQuery::Task { task_id }, Self::Task { task }) => {
                task.validate()?;
                if task.task_id != *task_id {
                    return Err(WorkerError::Conflict("worker task query identity"));
                }
            }
            _ => return Err(WorkerError::Conflict("worker response kind")),
        }
        encoded_bound(self, MAX_WORKER_COORDINATOR_INSPECTION_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerCoordinatorOperationResult {
    PeerPinned {
        peer: WorkerPeerState,
    },
    Prepared {
        preparation: Box<WorkerPreparation>,
    },
    Discarded {
        ticket: Uuid,
    },
    Admission {
        ticket: Uuid,
        executor_fingerprint: String,
        receipt: CommandReceipt,
        job: Box<WorkerJobInspection>,
    },
    Cancellation {
        binding: WorkerJobBinding,
        task: Box<WorkerNativeInspection>,
    },
    PeerRevoked {
        peer: WorkerPeerState,
    },
}

impl WorkerCoordinatorOperationResult {
    pub fn validate_for(
        &self,
        operation: &WorkerCoordinatorOperation,
        request_id: CommandId,
    ) -> Result<(), WorkerError> {
        operation.validate()?;
        match (operation, self) {
            (WorkerCoordinatorOperation::PinPeer { keys, .. }, Self::PeerPinned { peer }) => {
                peer_shape(peer)?;
                if peer.keys != *keys || peer.revoked_at.is_some() {
                    return Err(WorkerError::Conflict("worker pin response"));
                }
            }
            (WorkerCoordinatorOperation::Prepare { request }, Self::Prepared { preparation }) => {
                preparation.validate_for(request)?
            }
            (
                WorkerCoordinatorOperation::Discard { ticket: requested },
                Self::Discarded { ticket },
            ) if ticket == requested => {}
            (
                WorkerCoordinatorOperation::Delegate {
                    ticket: requested,
                    expected_manifest_digest,
                    expected_executor_fingerprint,
                },
                Self::Admission {
                    ticket,
                    executor_fingerprint,
                    receipt,
                    job,
                },
            ) => {
                job.validate()?;
                if ticket != requested
                    || executor_fingerprint != expected_executor_fingerprint
                    || job.binding.manifest_digest != *expected_manifest_digest
                    || receipt.command_id != request_id
                {
                    return Err(WorkerError::Conflict("worker admission response identity"));
                }
                receipt_shape(receipt, job.native.as_ref())?;
            }
            (
                WorkerCoordinatorOperation::Cancel {
                    task_id,
                    binding: requested,
                },
                Self::Cancellation { binding, task },
            ) => {
                task.validate()?;
                if binding != requested
                    || task.task_id != *task_id
                    || task.phase == RemoteTaskPhase::Reserved
                {
                    return Err(WorkerError::Conflict("worker cancellation response"));
                }
                if let Some(result) = &task.result {
                    report_binding(result, binding)?;
                }
            }
            (WorkerCoordinatorOperation::RevokePeer { device_id }, Self::PeerRevoked { peer }) => {
                peer_shape(peer)?;
                if peer.keys.device_id != *device_id || peer.revoked_at.is_none() {
                    return Err(WorkerError::Conflict("worker revoke response"));
                }
            }
            _ => return Err(WorkerError::Conflict("worker operation response kind")),
        }
        encoded_bound(
            self,
            if matches!(self, Self::Prepared { .. }) {
                MAX_WORKER_COORDINATOR_DISCLOSURE_BYTES
            } else {
                MAX_WORKER_COORDINATOR_INSPECTION_BYTES
            },
        )
    }
}

fn receipt_shape(
    receipt: &CommandReceipt,
    native: Option<&WorkerNativeInspection>,
) -> Result<(), WorkerError> {
    use super::inbox::CommandPhase;
    let valid = match receipt.phase {
        CommandPhase::Accepted => {
            receipt.applied.is_none() && receipt.result.is_none() && !receipt.archived
        }
        CommandPhase::Applied => {
            receipt.applied.is_some() && receipt.result.is_none() && !receipt.archived
        }
        CommandPhase::Completed | CommandPhase::Rejected => {
            receipt.result.is_some() || receipt.archived
        }
    };
    if !valid
        || receipt
            .applied
            .as_ref()
            .is_some_and(|applied| applied.conversation_id != receipt.conversation_id)
    {
        return Err(WorkerError::Conflict("worker command receipt phase"));
    }
    if let Some(native) = native
        && (receipt.conversation_id != native.conversation_id
            || receipt.accepted_at > native.admitted_at
            || receipt
                .applied
                .as_ref()
                .is_some_and(|applied| applied.task_id.is_some_and(|task| task != native.task_id)))
    {
        return Err(WorkerError::Conflict("worker command and native identity"));
    }
    Ok(())
}

fn report_binding(
    result: &RemoteResultInspection,
    binding: &WorkerJobBinding,
) -> Result<(), WorkerError> {
    if result.report.delegation_id != binding.delegation_id
        || result.report.attempt_id != binding.attempt_id
        || result.report.manifest_digest != binding.manifest_digest
    {
        return Err(WorkerError::Conflict("worker report binding"));
    }
    Ok(())
}

fn cancellation_shape(
    state: DelegationState,
    requested: bool,
    acknowledged: bool,
) -> Result<(), WorkerError> {
    if (acknowledged || state == DelegationState::CancelRequested) && !requested {
        return Err(WorkerError::Conflict("worker cancellation metadata"));
    }
    Ok(())
}

fn peer_shape(peer: &WorkerPeerState) -> Result<(), WorkerError> {
    peer.keys.validate()?;
    if peer.revoked_at.is_some_and(|at| at < peer.pinned_at) {
        return Err(WorkerError::Conflict("worker peer chronology"));
    }
    Ok(())
}

fn workspace_shape(path: &std::path::Path) -> Result<(), WorkerError> {
    if !path.is_absolute()
        || path.to_str().is_none_or(|text| {
            text.is_empty() || text.len() > 4096 || text.chars().any(char::is_control)
        })
    {
        return Err(WorkerError::Invalid("worker workspace shape"));
    }
    Ok(())
}

fn text_shape(text: &str, max: usize) -> Result<(), WorkerError> {
    if text.trim().is_empty() || text.len() > max || text.contains('\0') {
        return Err(WorkerError::Invalid("worker metadata text"));
    }
    Ok(())
}

fn non_nil(id: Uuid) -> Result<(), WorkerError> {
    if id.is_nil() {
        Err(WorkerError::Invalid("nil worker coordinator identity"))
    } else {
        Ok(())
    }
}

fn digest_shape(value: &str) -> Result<(), WorkerError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkerError::Invalid("worker digest encoding"));
    }
    Ok(())
}

fn fingerprint_shape(value: &str) -> Result<(), WorkerError> {
    if value.len() != 71
        || value.bytes().enumerate().any(|(index, byte)| {
            if index % 9 == 8 {
                byte != b'-'
            } else {
                !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte)
            }
        })
    {
        return Err(WorkerError::Invalid("worker fingerprint encoding"));
    }
    Ok(())
}

fn same_budget(left: &ContextBudget, right: &ContextBudget) -> bool {
    left.max_context_tokens == right.max_context_tokens
        && left.reserved_output_tokens == right.reserved_output_tokens
        && left.safety_margin_tokens == right.safety_margin_tokens
        && left.max_request_bytes == right.max_request_bytes
        && left.max_compaction_bytes == right.max_compaction_bytes
}

fn encoded_bound(value: &impl Serialize, limit: usize) -> Result<(), WorkerError> {
    struct Budget {
        remaining: usize,
        exceeded: bool,
    }
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.remaining {
                self.exceeded = true;
                return Err(io::Error::other("serialized worker metadata exceeds limit"));
            }
            self.remaining -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut budget = Budget {
        remaining: limit,
        exceeded: false,
    };
    match serde_json::to_writer(&mut budget, value) {
        Ok(()) => Ok(()),
        Err(_) if budget.exceeded => {
            Err(WorkerError::Capacity("encoded worker coordinator metadata"))
        }
        Err(error) => Err(error.into()),
    }
}
