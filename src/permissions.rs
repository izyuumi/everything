mod inspection;
pub use inspection::{ApprovalCursor, ApprovalPage, ApprovalSummary, MAX_APPROVAL_METADATA_BYTES};

use crate::storage::{PermissionRow, StorageError, Store};
use crate::tools::{ToolCapability, ToolDescriptor, ToolError, ToolRegistry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

const RECORD_VERSION: u32 = 1;
const SNAPSHOT_LIMIT: usize = 1_000;
const APPROVAL_LIFETIME_MS: i64 = 30 * 60 * 1_000;

#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("invalid approval inspection: {0}")]
    InvalidInspection(&'static str),
    #[error("permission storage failed: {0}")]
    Storage(#[from] StorageError),
    #[error("tool validation failed: {0}")]
    Tool(#[from] ToolError),
    #[error("permission serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid execution authority: {0}")]
    InvalidAuthority(String),
    #[error("invalid trusted tool scope: {0}")]
    InvalidToolScope(String),
    #[error("invalid filesystem target: {0}")]
    InvalidTarget(String),
    #[error("permission record {0} is malformed or unsupported")]
    MalformedRecord(String),
    #[error("approval {0} was not found")]
    ApprovalNotFound(String),
    #[error("approval {0} conflicts with the original operation or resolution")]
    ApprovalConflict(String),
    #[error("approval {0} is expired")]
    ApprovalExpired(String),
    #[error("grant {0} was not found")]
    GrantNotFound(String),
}

pub use crate::protocol::ExecutionMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionAuthority {
    conversation_id: String,
    task_id: String,
    workspace_root: PathBuf,
    mode: ExecutionMode,
    allowed_tools: Option<Arc<BTreeSet<String>>>,
}

impl ExecutionAuthority {
    pub fn new(
        conversation_id: impl Into<String>,
        task_id: impl Into<String>,
        workspace_root: PathBuf,
        mode: ExecutionMode,
    ) -> Result<Self, PermissionError> {
        let conversation_id = conversation_id.into();
        let task_id = task_id.into();
        validate_identifier(&conversation_id)?;
        validate_identifier(&task_id)?;
        if !workspace_root.is_absolute() {
            return Err(PermissionError::InvalidAuthority(
                "workspace must be absolute".into(),
            ));
        }
        let workspace_root = workspace_root.canonicalize().map_err(|error| {
            PermissionError::InvalidAuthority(format!("workspace cannot be resolved: {error}"))
        })?;
        if !workspace_root.is_dir() || workspace_root.to_str().is_none() {
            return Err(PermissionError::InvalidAuthority(
                "workspace must be a UTF-8 directory".into(),
            ));
        }
        Ok(Self {
            conversation_id,
            task_id,
            workspace_root,
            mode,
            allowed_tools: None,
        })
    }

    pub fn child(
        &self,
        task_id: impl Into<String>,
        mode: ExecutionMode,
    ) -> Result<Self, PermissionError> {
        self.child_in_conversation(self.conversation_id.clone(), task_id, mode)
    }

    pub fn child_in_conversation(
        &self,
        conversation_id: impl Into<String>,
        task_id: impl Into<String>,
        mode: ExecutionMode,
    ) -> Result<Self, PermissionError> {
        let conversation_id = conversation_id.into();
        let task_id = task_id.into();
        validate_identifier(&conversation_id)?;
        validate_identifier(&task_id)?;
        if mode > self.mode || task_id == self.task_id {
            return Err(PermissionError::InvalidAuthority(
                "child authority must have a distinct task and cannot increase execution mode"
                    .into(),
            ));
        }
        Ok(Self {
            conversation_id,
            task_id,
            workspace_root: self.workspace_root.clone(),
            mode,
            allowed_tools: self.allowed_tools.clone(),
        })
    }

    pub fn child_in_workspace(
        &self,
        conversation_id: impl Into<String>,
        task_id: impl Into<String>,
        workspace_root: PathBuf,
        mode: ExecutionMode,
    ) -> Result<Self, PermissionError> {
        let conversation_id = conversation_id.into();
        let task_id = task_id.into();
        validate_identifier(&conversation_id)?;
        validate_identifier(&task_id)?;
        if mode > self.mode || task_id == self.task_id || conversation_id == self.conversation_id {
            return Err(PermissionError::InvalidAuthority(
                "child workspace authority requires distinct task and conversation IDs and cannot increase execution mode"
                    .into(),
            ));
        }
        if !workspace_root.is_absolute() {
            return Err(PermissionError::InvalidAuthority(
                "child workspace must be absolute".into(),
            ));
        }
        let workspace_root = workspace_root.canonicalize().map_err(|error| {
            PermissionError::InvalidAuthority(format!(
                "child workspace cannot be resolved: {error}"
            ))
        })?;
        if !workspace_root.is_dir() || workspace_root.to_str().is_none() {
            return Err(PermissionError::InvalidAuthority(
                "child workspace must be a UTF-8 directory".into(),
            ));
        }
        if workspace_root == self.workspace_root
            || !workspace_root.starts_with(&self.workspace_root)
        {
            return Err(PermissionError::InvalidAuthority(
                "child workspace must be a strict physical descendant of the parent workspace"
                    .into(),
            ));
        }
        Ok(Self {
            conversation_id,
            task_id,
            workspace_root,
            mode,
            allowed_tools: self.allowed_tools.clone(),
        })
    }

    pub fn restrict_tools(
        mut self,
        allowed_tools: BTreeSet<String>,
    ) -> Result<Self, PermissionError> {
        if allowed_tools.len() > 1_000 {
            return Err(PermissionError::InvalidAuthority(
                "tool allowlist exceeds 1000 entries".into(),
            ));
        }
        for name in &allowed_tools {
            validate_identifier(name)?;
        }
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|inherited| !allowed_tools.is_subset(inherited))
        {
            return Err(PermissionError::InvalidAuthority(
                "tool allowlist cannot widen inherited authority".into(),
            ));
        }
        self.allowed_tools = Some(Arc::new(allowed_tools));
        Ok(self)
    }

    pub(crate) fn allowed_tools(&self) -> Option<&BTreeSet<String>> {
        self.allowed_tools.as_deref()
    }

    pub fn permits_tool(&self, name: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }
    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }
}

#[derive(Debug, Clone)]
pub enum ToolScopeRule {
    LocalReadOnly,
    RuntimeTask,
    Filesystem { path_pointers: Vec<String> },
    Host { service: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationScope {
    pub conversation_id: String,
    pub task_id: String,
    pub operation_id: String,
    pub workspace_root: String,
    pub mode: ExecutionMode,
    pub tool_name: String,
    pub capabilities: Vec<String>,
    pub targets: Vec<String>,
    pub arguments_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: String,
    pub scope: AuthorizationScope,
    pub arguments: Value,
    pub requested_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalChoice {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolution {
    pub choice: ApprovalChoice,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved { grant_id: String, expires_at: i64 },
    Denied,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalView {
    pub request: ApprovalRequest,
    pub status: ApprovalStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AuthorizationDecision {
    Allow { grant_id: Option<String> },
    Ask { approval: Box<ApprovalRequest> },
    Deny { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "visibility", rename_all = "snake_case")]
pub enum ToolVisibility {
    Available,
    RequiresApproval,
    Denied { reason: String },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionRecord {
    version: u32,
    record: StoredRecord,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredRecord {
    Approval {
        request: ApprovalRequest,
    },
    Resolution {
        approval_id: String,
        scope_digest: String,
        resolution: ApprovalResolution,
        resolved_at: i64,
    },
}

#[derive(Clone)]
pub struct Authorizer {
    store: Store,
    registry: Arc<ToolRegistry>,
    scope_rules: Arc<BTreeMap<String, ToolScopeRule>>,
}

impl Authorizer {
    pub fn new(
        store: Store,
        registry: Arc<ToolRegistry>,
        scope_rules: BTreeMap<String, ToolScopeRule>,
    ) -> Result<Self, PermissionError> {
        for (tool_name, rule) in &scope_rules {
            validate_rule(registry.descriptor(tool_name)?, rule)?;
        }
        Ok(Self {
            store,
            registry,
            scope_rules: Arc::new(scope_rules),
        })
    }

    pub fn tool_visibility(
        &self,
        authority: &ExecutionAuthority,
        tool_name: &str,
    ) -> Result<ToolVisibility, PermissionError> {
        let descriptor = self.registry.descriptor(tool_name)?;
        if !authority.permits_tool(tool_name) {
            return Ok(ToolVisibility::Denied {
                reason: "tool is outside the inherited allowlist".into(),
            });
        }
        let Some(rule) = self.scope_rules.get(tool_name) else {
            return Ok(ToolVisibility::Denied {
                reason: "tool has no trusted host scope registration".into(),
            });
        };
        if matches!(rule, ToolScopeRule::RuntimeTask) {
            return Ok(ToolVisibility::Available);
        }
        let privileged = descriptor.capabilities.iter().any(|capability| {
            matches!(
                capability,
                ToolCapability::Shell | ToolCapability::Network | ToolCapability::Mcp
            )
        }) || matches!(rule, ToolScopeRule::Host { .. });
        if privileged {
            return Ok(if authority.mode == ExecutionMode::FullHost {
                ToolVisibility::RequiresApproval
            } else {
                ToolVisibility::Denied {
                    reason: "tool requires FullHost authority and explicit approval".into(),
                }
            });
        }
        if descriptor
            .capabilities
            .contains(&ToolCapability::FilesystemWrite)
            && authority.mode == ExecutionMode::ReadOnly
        {
            return Ok(ToolVisibility::Denied {
                reason: "ReadOnly authority forbids filesystem mutation".into(),
            });
        }
        if !descriptor.read_only
            && !descriptor
                .capabilities
                .contains(&ToolCapability::FilesystemWrite)
        {
            return Ok(if authority.mode == ExecutionMode::FullHost {
                ToolVisibility::RequiresApproval
            } else {
                ToolVisibility::Denied {
                    reason: "tool does not declare a scoped implementation".into(),
                }
            });
        }
        Ok(ToolVisibility::Available)
    }

    pub fn authorize(
        &self,
        authority: &ExecutionAuthority,
        operation_id: &str,
        tool_name: &str,
        arguments: &Value,
        now: i64,
    ) -> Result<AuthorizationDecision, PermissionError> {
        validate_time(now)?;
        validate_identifier(operation_id)?;
        self.registry.validate(tool_name, arguments)?;
        self.validate_snapshot(authority.conversation_id(), now)?;
        let descriptor = self.registry.descriptor(tool_name)?;
        if !authority.permits_tool(tool_name) {
            return Ok(deny("tool is outside the inherited allowlist"));
        }
        let Some(rule) = self.scope_rules.get(tool_name) else {
            return Ok(deny("tool has no trusted host scope registration"));
        };
        let scope = derive_scope(authority, operation_id, descriptor, rule, arguments)?;
        let approval_id = approval_id(&scope)?;
        let existing_request = self
            .store
            .permission(&approval_id)?
            .as_ref()
            .map(decode_request)
            .transpose()?;
        if existing_request
            .as_ref()
            .is_some_and(|request| request.scope != scope)
        {
            return Err(PermissionError::ApprovalConflict(approval_id));
        }
        match evaluate_mode(authority, descriptor, rule, &scope) {
            ModeDecision::Allow => {
                return existing_request.as_ref().map_or_else(
                    || Ok(AuthorizationDecision::Allow { grant_id: None }),
                    |request| self.decision_for_request(request, now),
                );
            }
            ModeDecision::Deny(reason) => return Ok(deny(reason)),
            ModeDecision::Ask => {}
        }
        let request = if let Some(request) = existing_request {
            request
        } else {
            let expires_at = now
                .checked_add(APPROVAL_LIFETIME_MS)
                .ok_or_else(|| PermissionError::InvalidAuthority("timestamp overflow".into()))?;
            let request = ApprovalRequest {
                id: approval_id.clone(),
                scope,
                arguments: arguments.clone(),
                requested_at: now,
                expires_at,
            };
            let row = request_row(&request)?;
            if let Err(error) = self.store.create_permission(&row) {
                let Some(existing) = self.store.permission(&approval_id)? else {
                    return Err(error.into());
                };
                let existing = decode_request(&existing)?;
                if existing.scope != request.scope {
                    return Err(PermissionError::ApprovalConflict(approval_id));
                }
                return self.decision_for_request(&existing, now);
            }
            request
        };
        self.decision_for_request(&request, now)
    }

    pub fn approval(&self, approval_id: &str, now: i64) -> Result<ApprovalView, PermissionError> {
        validate_time(now)?;
        let request = self.load_request(approval_id)?;
        self.validate_snapshot(&request.scope.conversation_id, now)?;
        let status = self.status_for_request(&request, now)?;
        Ok(ApprovalView { request, status })
    }

    pub fn approval_for_operation(
        &self,
        conversation_id: &str,
        operation_id: &str,
        now: i64,
    ) -> Result<Option<ApprovalView>, PermissionError> {
        validate_identifier(conversation_id)?;
        validate_identifier(operation_id)?;
        validate_time(now)?;
        let id = approval_id_for_operation(conversation_id, operation_id)?;
        let Some(row) = self.store.permission(&id)? else {
            if let Some(orphan) = self.store.permission(&resolution_id(&id))? {
                return Err(PermissionError::MalformedRecord(orphan.id));
            }
            return Ok(None);
        };
        let request = decode_request(&row)?;
        if request.scope.conversation_id != conversation_id
            || request.scope.operation_id != operation_id
        {
            return Err(PermissionError::MalformedRecord(row.id));
        }
        let status = self.status_for_request(&request, now)?;
        Ok(Some(ApprovalView { request, status }))
    }

    pub fn approvals(
        &self,
        conversation_id: &str,
        now: i64,
    ) -> Result<Vec<ApprovalView>, PermissionError> {
        validate_identifier(conversation_id)?;
        validate_time(now)?;
        let rows = self
            .store
            .permission_history(conversation_id, SNAPSHOT_LIMIT)?;
        let index: BTreeMap<_, _> = rows.iter().map(|row| (row.id.as_str(), row)).collect();
        let mut requests = Vec::new();
        for row in &rows {
            match decode_record(row)?.record {
                StoredRecord::Approval { .. } => requests.push(decode_request(row)?),
                StoredRecord::Resolution { approval_id, .. } => {
                    let original = index
                        .get(approval_id.as_str())
                        .ok_or_else(|| PermissionError::MalformedRecord(row.id.clone()))?;
                    let request = decode_request(original)?;
                    decode_resolution(row, &request)?;
                }
            }
        }
        requests
            .into_iter()
            .map(|request| {
                let resolution = index.get(resolution_id(&request.id).as_str()).copied();
                let status = status_for_resolution(&request, resolution, now)?;
                Ok(ApprovalView { request, status })
            })
            .collect()
    }

    pub fn pending_approvals(
        &self,
        conversation_id: &str,
        now: i64,
    ) -> Result<Vec<ApprovalRequest>, PermissionError> {
        validate_identifier(conversation_id)?;
        validate_time(now)?;
        let rows = self.validate_snapshot(conversation_id, now)?;
        let mut pending = Vec::new();
        for row in rows {
            if let StoredRecord::Approval { request } = decode_record(&row)?.record
                && self.status_for_request(&request, now)? == ApprovalStatus::Pending
            {
                pending.push(request);
            }
        }
        Ok(pending)
    }

    pub fn resolve(
        &self,
        approval_id: &str,
        resolution: &ApprovalResolution,
        now: i64,
    ) -> Result<ApprovalView, PermissionError> {
        validate_time(now)?;
        let request = self.load_request(approval_id)?;
        self.validate_snapshot(&request.scope.conversation_id, now)?;
        let id = resolution_id(approval_id);
        if let Some(row) = self.store.permission(&id)? {
            let stored = decode_resolution(&row, &request)?;
            if stored != *resolution {
                return Err(PermissionError::ApprovalConflict(approval_id.into()));
            }
            return Ok(ApprovalView {
                status: self.status_for_request(&request, now)?,
                request,
            });
        }
        if now < request.requested_at || now >= request.expires_at {
            return Err(PermissionError::ApprovalExpired(approval_id.into()));
        }
        let expires_at = resolution.expires_at.unwrap_or(request.expires_at);
        if expires_at <= now || expires_at > request.expires_at {
            return Err(PermissionError::ApprovalConflict(approval_id.into()));
        }
        let payload = PermissionRecord {
            version: RECORD_VERSION,
            record: StoredRecord::Resolution {
                approval_id: approval_id.into(),
                scope_digest: digest(&request.scope)?,
                resolution: resolution.clone(),
                resolved_at: now,
            },
        };
        let row = PermissionRow {
            id: id.clone(),
            conversation_id: Some(request.scope.conversation_id.clone()),
            payload: serde_json::to_value(payload)?,
            created_at: now,
            expires_at: Some(expires_at),
            revoked_at: None,
        };
        if let Err(error) = self.store.create_permission(&row) {
            let Some(existing) = self.store.permission(&id)? else {
                return Err(error.into());
            };
            if decode_resolution(&existing, &request)? != *resolution {
                return Err(PermissionError::ApprovalConflict(approval_id.into()));
            }
        }
        Ok(ApprovalView {
            status: self.status_for_request(&request, now)?,
            request,
        })
    }

    pub fn revoke(&self, grant_id: &str, now: i64) -> Result<(), PermissionError> {
        validate_time(now)?;
        let row = self
            .store
            .permission(grant_id)?
            .ok_or_else(|| PermissionError::GrantNotFound(grant_id.into()))?;
        let record = decode_record(&row)?;
        let StoredRecord::Resolution {
            approval_id,
            resolution,
            ..
        } = record.record
        else {
            return Err(PermissionError::GrantNotFound(grant_id.into()));
        };
        let request = self.load_request(&approval_id)?;
        decode_resolution(&row, &request)?;
        if resolution.choice != ApprovalChoice::Allow {
            return Err(PermissionError::GrantNotFound(grant_id.into()));
        }
        if row.revoked_at.is_some() {
            return Ok(());
        }
        if let Err(error) = self.store.revoke_permission(grant_id, now)
            && self
                .store
                .permission(grant_id)?
                .is_none_or(|row| row.revoked_at.is_none())
        {
            return Err(error.into());
        }
        Ok(())
    }

    fn load_request(&self, id: &str) -> Result<ApprovalRequest, PermissionError> {
        validate_identifier(id)?;
        let row = self
            .store
            .permission(id)?
            .ok_or_else(|| PermissionError::ApprovalNotFound(id.into()))?;
        decode_request(&row)
    }

    fn validate_snapshot(
        &self,
        conversation_id: &str,
        now: i64,
    ) -> Result<Vec<PermissionRow>, PermissionError> {
        let rows = self
            .store
            .active_permissions(Some(conversation_id), now, SNAPSHOT_LIMIT)?;
        for row in &rows {
            match decode_record(row)?.record {
                StoredRecord::Approval { .. } => {
                    decode_request(row)?;
                }
                StoredRecord::Resolution { approval_id, .. } => {
                    let request = self.load_request(&approval_id)?;
                    decode_resolution(row, &request)?;
                }
            }
        }
        Ok(rows)
    }

    fn status_for_request(
        &self,
        request: &ApprovalRequest,
        now: i64,
    ) -> Result<ApprovalStatus, PermissionError> {
        let row = self.store.permission(&resolution_id(&request.id))?;
        status_for_resolution(request, row.as_ref(), now)
    }

    fn decision_for_request(
        &self,
        request: &ApprovalRequest,
        now: i64,
    ) -> Result<AuthorizationDecision, PermissionError> {
        Ok(match self.status_for_request(request, now)? {
            ApprovalStatus::Pending => AuthorizationDecision::Ask {
                approval: Box::new(request.clone()),
            },
            ApprovalStatus::Approved { grant_id, .. } => AuthorizationDecision::Allow {
                grant_id: Some(grant_id),
            },
            ApprovalStatus::Denied => deny("host denied this operation"),
            ApprovalStatus::Revoked => deny("host revoked the scoped grant"),
            ApprovalStatus::Expired => deny("approval or scoped grant expired"),
        })
    }
}

fn status_for_resolution(
    request: &ApprovalRequest,
    row: Option<&PermissionRow>,
    now: i64,
) -> Result<ApprovalStatus, PermissionError> {
    let Some(row) = row else {
        return Ok(if now >= request.expires_at {
            ApprovalStatus::Expired
        } else {
            ApprovalStatus::Pending
        });
    };
    let resolution = decode_resolution(row, request)?;
    if row.revoked_at.is_some() {
        return Ok(ApprovalStatus::Revoked);
    }
    if resolution.choice == ApprovalChoice::Deny {
        return Ok(ApprovalStatus::Denied);
    }
    let expires_at = resolution.expires_at.unwrap_or(request.expires_at);
    if now < row.created_at || now >= expires_at {
        return Ok(ApprovalStatus::Expired);
    }
    Ok(ApprovalStatus::Approved {
        grant_id: row.id.clone(),
        expires_at,
    })
}

enum ModeDecision {
    Allow,
    Ask,
    Deny(&'static str),
}

fn evaluate_mode(
    authority: &ExecutionAuthority,
    descriptor: &ToolDescriptor,
    rule: &ToolScopeRule,
    scope: &AuthorizationScope,
) -> ModeDecision {
    if matches!(rule, ToolScopeRule::RuntimeTask) {
        return ModeDecision::Allow;
    }
    let privileged = descriptor.capabilities.iter().any(|capability| {
        matches!(
            capability,
            ToolCapability::Shell | ToolCapability::Network | ToolCapability::Mcp
        )
    }) || matches!(rule, ToolScopeRule::Host { .. });
    if privileged {
        return if authority.mode == ExecutionMode::FullHost {
            ModeDecision::Ask
        } else {
            ModeDecision::Deny(
                "shell, network, MCP, and host-scoped tools require FullHost authority and explicit approval",
            )
        };
    }
    let writes = descriptor
        .capabilities
        .contains(&ToolCapability::FilesystemWrite);
    if writes && authority.mode == ExecutionMode::ReadOnly {
        return ModeDecision::Deny("ReadOnly authority forbids filesystem mutation");
    }
    if matches!(rule, ToolScopeRule::Filesystem { .. }) {
        let inside_workspace = scope
            .targets
            .iter()
            .all(|target| Path::new(target).starts_with(authority.workspace_root()));
        if !inside_workspace {
            return if authority.mode == ExecutionMode::FullHost {
                ModeDecision::Ask
            } else {
                ModeDecision::Deny("filesystem target is outside the inherited workspace")
            };
        }
        if !descriptor.read_only && !writes {
            return if authority.mode == ExecutionMode::FullHost {
                ModeDecision::Ask
            } else {
                ModeDecision::Deny(
                    "tool does not declare a read-only implementation or scoped filesystem writes",
                )
            };
        }
    }
    ModeDecision::Allow
}

fn validate_rule(descriptor: &ToolDescriptor, rule: &ToolScopeRule) -> Result<(), PermissionError> {
    let invalid = |message: &str| {
        PermissionError::InvalidToolScope(format!("{}: {message}", descriptor.name))
    };
    match rule {
        ToolScopeRule::LocalReadOnly => {
            if !descriptor.read_only || !descriptor.capabilities.is_empty() {
                return Err(invalid(
                    "local read-only tools must declare no external capabilities",
                ));
            }
        }
        ToolScopeRule::RuntimeTask => {
            if !descriptor.capabilities.is_empty() {
                return Err(invalid(
                    "native task controls must declare no external capabilities",
                ));
            }
        }
        ToolScopeRule::Filesystem { path_pointers } => {
            if descriptor.capabilities.is_empty()
                || descriptor.capabilities.iter().any(|capability| {
                    !matches!(
                        capability,
                        ToolCapability::FilesystemRead | ToolCapability::FilesystemWrite
                    )
                })
            {
                return Err(invalid(
                    "filesystem scopes require only filesystem capabilities",
                ));
            }
            if path_pointers.is_empty()
                || path_pointers.len() > 16
                || path_pointers
                    .iter()
                    .any(|pointer| !pointer.starts_with('/') || pointer.len() > 256)
            {
                return Err(invalid(
                    "filesystem scopes require 1–16 bounded JSON pointers",
                ));
            }
            if path_pointers.iter().collect::<BTreeSet<_>>().len() != path_pointers.len() {
                return Err(invalid("filesystem pointers must be unique"));
            }
        }
        ToolScopeRule::Host { service } => {
            validate_identifier(service)
                .map_err(|_| invalid("host service must be a valid identifier"))?;
        }
    }
    Ok(())
}

fn derive_scope(
    authority: &ExecutionAuthority,
    operation_id: &str,
    descriptor: &ToolDescriptor,
    rule: &ToolScopeRule,
    arguments: &Value,
) -> Result<AuthorizationScope, PermissionError> {
    let mut targets = Vec::new();
    match rule {
        ToolScopeRule::LocalReadOnly => {}
        ToolScopeRule::RuntimeTask => targets.push("native_tasks".into()),
        ToolScopeRule::Host { service } => targets.push(service.clone()),
        ToolScopeRule::Filesystem { path_pointers } => {
            for pointer in path_pointers {
                let target = arguments.pointer(pointer).ok_or_else(|| {
                    PermissionError::InvalidTarget(format!("missing path at {pointer}"))
                })?;
                let values: Vec<&Value> = if let Value::Array(values) = target {
                    values.iter().collect()
                } else {
                    vec![target]
                };
                if values.is_empty() || values.len() > 64 {
                    return Err(PermissionError::InvalidTarget(
                        "a path target requires 1–64 paths".into(),
                    ));
                }
                for value in values {
                    let path = value.as_str().ok_or_else(|| {
                        PermissionError::InvalidTarget("path values must be strings".into())
                    })?;
                    let target = resolve_path(
                        authority.workspace_root(),
                        path,
                        descriptor
                            .capabilities
                            .contains(&ToolCapability::FilesystemWrite),
                    )?;
                    targets.push(target);
                }
            }
        }
    }
    targets.sort();
    targets.dedup();
    let mut capabilities: Vec<_> = descriptor
        .capabilities
        .iter()
        .map(|capability| {
            match capability {
                ToolCapability::FilesystemRead => "filesystem_read",
                ToolCapability::FilesystemWrite => "filesystem_write",
                ToolCapability::Shell => "shell",
                ToolCapability::Network => "network",
                ToolCapability::Mcp => "mcp",
            }
            .to_owned()
        })
        .collect();
    capabilities.sort();
    capabilities.dedup();
    let workspace_root = authority
        .workspace_root
        .to_str()
        .ok_or_else(|| PermissionError::InvalidAuthority("workspace must be UTF-8".into()))?
        .to_owned();
    Ok(AuthorizationScope {
        conversation_id: authority.conversation_id.clone(),
        task_id: authority.task_id.clone(),
        operation_id: operation_id.into(),
        workspace_root,
        mode: authority.mode,
        tool_name: descriptor.name.clone(),
        capabilities,
        targets,
        arguments_digest: digest(arguments)?,
    })
}

fn resolve_path(root: &Path, value: &str, allow_missing: bool) -> Result<String, PermissionError> {
    if value.is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
        return Err(PermissionError::InvalidTarget(
            "paths require 1–4096 bytes without controls".into(),
        ));
    }
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let resolved = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            let mut ancestor = path.as_path();
            let mut tail = Vec::new();
            loop {
                if ancestor.symlink_metadata().is_ok() {
                    break;
                }
                let name = ancestor.file_name().ok_or_else(|| {
                    PermissionError::InvalidTarget("unresolvable target ancestor".into())
                })?;
                if ancestor
                    .components()
                    .next_back()
                    .is_none_or(|part| !matches!(part, Component::Normal(_)))
                {
                    return Err(PermissionError::InvalidTarget(
                        "unresolved target must use normal path components".into(),
                    ));
                }
                tail.push(name.to_owned());
                ancestor = ancestor.parent().ok_or_else(|| {
                    PermissionError::InvalidTarget("unresolvable target ancestor".into())
                })?;
            }
            let mut resolved = ancestor.canonicalize().map_err(|error| {
                PermissionError::InvalidTarget(format!(
                    "target ancestor cannot be resolved: {error}"
                ))
            })?;
            if !resolved.is_dir() {
                return Err(PermissionError::InvalidTarget(
                    "target ancestor is not a directory".into(),
                ));
            }
            for name in tail.into_iter().rev() {
                resolved.push(name);
            }
            resolved
        }
        Err(error) => {
            return Err(PermissionError::InvalidTarget(format!(
                "target cannot be resolved: {error}"
            )));
        }
    };
    resolved
        .into_os_string()
        .into_string()
        .map_err(|_| PermissionError::InvalidTarget("target must be UTF-8".into()))
}

fn decode_record(row: &PermissionRow) -> Result<PermissionRecord, PermissionError> {
    let record: PermissionRecord = serde_json::from_value(row.payload.clone())
        .map_err(|_| PermissionError::MalformedRecord(row.id.clone()))?;
    if record.version != RECORD_VERSION || row.conversation_id.is_none() {
        return Err(PermissionError::MalformedRecord(row.id.clone()));
    }
    Ok(record)
}

fn decode_request(row: &PermissionRow) -> Result<ApprovalRequest, PermissionError> {
    let malformed = || PermissionError::MalformedRecord(row.id.clone());
    let StoredRecord::Approval { request } = decode_record(row)?.record else {
        return Err(malformed());
    };
    if digest(&request.arguments)? != request.scope.arguments_digest
        || request
            .expires_at
            .checked_sub(request.requested_at)
            .is_none_or(|duration| duration > APPROVAL_LIFETIME_MS)
        || row.expires_at != Some(request.expires_at)
        || request.id != row.id
        || request.requested_at != row.created_at
        || request.expires_at <= request.requested_at
        || row.revoked_at.is_some()
        || row.conversation_id.as_deref() != Some(request.scope.conversation_id.as_str())
        || approval_id(&request.scope)? != row.id
    {
        return Err(malformed());
    }
    validate_stored_scope(&request.scope).map_err(|_| malformed())?;
    Ok(request)
}

fn decode_resolution(
    row: &PermissionRow,
    request: &ApprovalRequest,
) -> Result<ApprovalResolution, PermissionError> {
    let malformed = || PermissionError::MalformedRecord(row.id.clone());
    let StoredRecord::Resolution {
        approval_id,
        scope_digest,
        resolution,
        resolved_at,
    } = decode_record(row)?.record
    else {
        return Err(malformed());
    };
    let expires_at = resolution.expires_at.unwrap_or(request.expires_at);
    if approval_id != request.id
        || row.id != resolution_id(&request.id)
        || scope_digest != digest(&request.scope)?
        || row.expires_at != Some(expires_at)
        || row.created_at != resolved_at
        || row.created_at < request.requested_at
        || row.created_at >= request.expires_at
        || expires_at <= resolved_at
        || expires_at > request.expires_at
        || row.conversation_id.as_deref() != Some(request.scope.conversation_id.as_str())
    {
        return Err(malformed());
    }
    Ok(resolution)
}

fn validate_stored_scope(scope: &AuthorizationScope) -> Result<(), PermissionError> {
    validate_identifier(&scope.conversation_id)?;
    validate_identifier(&scope.task_id)?;
    validate_identifier(&scope.operation_id)?;
    validate_identifier(&scope.tool_name)?;
    if !Path::new(&scope.workspace_root).is_absolute()
        || scope.workspace_root.len() > 4_096
        || scope.arguments_digest.len() != 64
        || !scope
            .arguments_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || scope.targets.len() > 1_024
        || scope.targets.iter().any(|target| {
            target.is_empty() || target.len() > 4_096 || target.chars().any(char::is_control)
        })
        || scope.capabilities.iter().any(|capability| {
            !matches!(
                capability.as_str(),
                "filesystem_read" | "filesystem_write" | "shell" | "network" | "mcp"
            )
        })
    {
        return Err(PermissionError::InvalidAuthority(
            "stored authorization scope is invalid".into(),
        ));
    }
    Ok(())
}

fn request_row(request: &ApprovalRequest) -> Result<PermissionRow, PermissionError> {
    Ok(PermissionRow {
        id: request.id.clone(),
        conversation_id: Some(request.scope.conversation_id.clone()),
        payload: serde_json::to_value(PermissionRecord {
            version: RECORD_VERSION,
            record: StoredRecord::Approval {
                request: request.clone(),
            },
        })?,
        created_at: request.requested_at,
        expires_at: Some(request.expires_at),
        revoked_at: None,
    })
}

fn approval_id(scope: &AuthorizationScope) -> Result<String, PermissionError> {
    approval_id_for_operation(&scope.conversation_id, &scope.operation_id)
}

fn approval_id_for_operation(
    conversation_id: &str,
    operation_id: &str,
) -> Result<String, PermissionError> {
    Ok(format!(
        "approval:{}",
        digest(&(conversation_id, operation_id))?
    ))
}

fn resolution_id(approval_id: &str) -> String {
    format!("decision:{approval_id}")
}

fn digest(value: &impl Serialize) -> Result<String, PermissionError> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn deny(reason: &str) -> AuthorizationDecision {
    AuthorizationDecision::Deny {
        reason: reason.into(),
    }
}

fn validate_identifier(value: &str) -> Result<(), PermissionError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(PermissionError::InvalidAuthority(
            "identifiers require 1–256 bytes without controls".into(),
        ));
    }
    Ok(())
}

fn validate_time(now: i64) -> Result<(), PermissionError> {
    if now < 0 {
        return Err(PermissionError::InvalidAuthority(
            "time must be nonnegative Unix milliseconds".into(),
        ));
    }
    Ok(())
}
