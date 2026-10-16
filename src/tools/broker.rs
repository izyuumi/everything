use std::io::{self, Write};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::permissions::{
    ApprovalRequest, AuthorizationDecision, Authorizer, ExecutionAuthority, ExecutionMode,
    PermissionError,
};
use crate::storage::{StorageError, Store, ToolCallRow, ToolCallState, ToolCleanupAttestation};

use super::{
    RetainedToolOwner, ToolConcurrency, ToolError, ToolExecutionContext, ToolOutput, ToolRegistry,
};

const RECORD_VERSION: u16 = 1;
const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_DIAGNOSTIC_CHARACTERS: usize = 2_048;
const MAX_RECOVERY_REPORT: usize = 128;

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("invalid broker configuration: {0}")]
    InvalidConfiguration(String),
    #[error("invalid tool invocation: {0}")]
    InvalidInvocation(String),
    #[error("tool validation failed: {0}")]
    Tool(#[from] ToolError),
    #[error("permission evaluation failed: {0}")]
    Permission(#[from] PermissionError),
    #[error("operation journal failed: {0}")]
    Storage(#[from] StorageError),
    #[error("operation record serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("tool broker admission limit reached")]
    Busy,
    #[error("tool broker is quarantined; reconcile interrupted work before restarting the host")]
    Quarantined,
    #[error("cleanup attestation conflicts with the reviewed outcome for operation {operation_id}")]
    CleanupAttestationConflict { operation_id: String },
    #[error("active operations prevented acquiring the cleanup reconciliation gate")]
    RecoveryBusy,
    #[error("broker worker failed: {0}")]
    Worker(String),
    #[error("host clock cannot produce a supported Unix timestamp")]
    Clock,
    #[error("operation {operation_id} has an invalid or unsupported journal record")]
    CorruptJournal { operation_id: String },
    #[error("operation {operation_id} has an unknown outcome ({reason:?}); stop and reconcile")]
    OutcomeUnknown {
        operation_id: String,
        reason: UnknownReason,
        cleanup_completed: bool,
        journal_error: Option<String>,
    },
}

impl BrokerError {
    pub fn requires_reconciliation(&self) -> bool {
        matches!(
            self,
            Self::Quarantined | Self::CorruptJournal { .. } | Self::OutcomeUnknown { .. }
        )
    }
}

#[derive(Clone, Debug)]
pub struct BrokerLimits {
    pub max_concurrent: usize,
    pub max_in_flight: usize,
    pub queue_timeout: Duration,
    pub execution_timeout: Duration,
    pub cancellation_grace: Duration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub operation_id: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interruption {
    CancellationRequested,
    QueueDeadline,
    ExecutionDeadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    InterruptedByHostRestart,
    ToolFailureAfterInvocation,
    CancellationAfterInvocation,
    CleanupDeadline,
    ToolPanicked,
    ResultNotPersistable,
    JournalWriteFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OperationResult {
    Succeeded {
        output: ToolOutput,
    },
    Failed {
        code: String,
        message: String,
        #[serde(default)]
        output: Option<ToolOutput>,
    },
    Cancelled {
        reason: Interruption,
    },
    Denied {
        reason: String,
    },
    Unknown {
        reason: UnknownReason,
        cleanup_completed: bool,
        diagnostic: Option<String>,
    },
}

impl OperationResult {
    pub fn state(&self) -> ToolCallState {
        match self {
            Self::Succeeded { .. } => ToolCallState::Succeeded,
            Self::Failed { .. } => ToolCallState::Failed,
            Self::Cancelled { .. } => ToolCallState::Cancelled,
            Self::Denied { .. } => ToolCallState::Denied,
            Self::Unknown { .. } => ToolCallState::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub version: u16,
    pub result: OperationResult,
    pub interruption: Option<Interruption>,
}

impl ExecutionRecord {
    pub fn requires_stop(&self) -> bool {
        self.interruption.is_some()
            || matches!(
                self.result,
                OperationResult::Cancelled { .. } | OperationResult::Unknown { .. }
            )
    }
}

#[derive(Clone, Debug)]
pub enum BrokerOutcome {
    Completed {
        operation_id: String,
        record: ExecutionRecord,
        replayed: bool,
    },
    ApprovalRequired {
        operation_id: String,
        approval: Box<ApprovalRequest>,
    },
    InFlight {
        operation_id: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupRisk {
    IncompleteCleanup,
    InvalidJournalRecord,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupRequirement {
    pub operation_id: String,
    pub original_result_digest: String,
    pub risk: CleanupRisk,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub recovered_operations: usize,
    pub unresolved_count: usize,
    pub requirements: Vec<CleanupRequirement>,
    pub restart_required: bool,
    pub quarantined: bool,
}

#[derive(Clone)]
pub struct ToolBroker {
    inner: Arc<BrokerInner>,
}

#[derive(Clone)]
pub struct TaskToolBroker {
    broker: ToolBroker,
    authority: ExecutionAuthority,
}

struct BrokerInner {
    store: Store,
    registry: Arc<ToolRegistry>,
    authorizer: Arc<Authorizer>,
    limits: BrokerLimits,
    admission: Arc<Semaphore>,
    execution: Arc<Semaphore>,
    exclusion: Arc<RwLock<()>>,
    quarantined: AtomicBool,
    restart_required: AtomicBool,
    recovered_operations: usize,
}

#[derive(Serialize)]
struct JournalRequest<'a> {
    version: u16,
    arguments: &'a Value,
    workspace_root: &'a str,
    mode: ExecutionMode,
    descriptor_digest: String,
    execution_timeout: Duration,
    cancellation_grace: Duration,
}

enum ExclusionGuard {
    Concurrent { _guard: OwnedRwLockReadGuard<()> },
    Exclusive { _guard: OwnedRwLockWriteGuard<()> },
}

enum ExecutionClaim {
    Acquired,
    Competing,
    NotAllowed(AuthorizationDecision),
    Cancelled(Interruption),
}

struct ExecutionLease {
    inner: Arc<BrokerInner>,
    armed: bool,
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        if self.armed {
            self.inner.quarantined.store(true, Ordering::Release);
            self.inner.restart_required.store(true, Ordering::Release);
        }
    }
}

impl ToolBroker {
    pub async fn start(
        store: Store,
        registry: Arc<ToolRegistry>,
        authorizer: Arc<Authorizer>,
        limits: BrokerLimits,
    ) -> Result<Self, BrokerError> {
        Self::start_retained(store, registry, authorizer, limits, None).await
    }

    pub async fn start_with_owner(
        store: Store,
        registry: Arc<ToolRegistry>,
        authorizer: Arc<Authorizer>,
        limits: BrokerLimits,
        owner: impl Send + Sync + 'static,
    ) -> Result<Self, BrokerError> {
        Self::start_retained(
            store,
            registry,
            authorizer,
            limits,
            Some(RetainedToolOwner::new(owner)),
        )
        .await
    }

    async fn start_retained(
        store: Store,
        registry: Arc<ToolRegistry>,
        authorizer: Arc<Authorizer>,
        limits: BrokerLimits,
        owner: Option<RetainedToolOwner>,
    ) -> Result<Self, BrokerError> {
        validate_limits(&limits)?;
        let recovery = encode_record(&record(
            OperationResult::Unknown {
                reason: UnknownReason::InterruptedByHostRestart,
                cleanup_completed: false,
                diagnostic: None,
            },
            None,
        ))?;
        let recovery_store = store.clone();
        let (recovered_operations, recovery_report) = blocking(owner, move || {
            let recovered = recovery_store.mark_running_tool_calls_unknown(&recovery, now()?)?;
            Ok((recovered, inspect_cleanup(&recovery_store, recovered)?))
        })
        .await?;
        Ok(Self {
            inner: Arc::new(BrokerInner {
                store,
                registry,
                authorizer,
                admission: Arc::new(Semaphore::new(limits.max_in_flight)),
                execution: Arc::new(Semaphore::new(limits.max_concurrent)),
                exclusion: Arc::new(RwLock::new(())),
                limits,
                quarantined: AtomicBool::new(recovery_report.quarantined),
                restart_required: AtomicBool::new(false),
                recovered_operations,
            }),
        })
    }

    pub fn scoped(&self, authority: ExecutionAuthority) -> TaskToolBroker {
        TaskToolBroker {
            broker: self.clone(),
            authority,
        }
    }

    pub fn recovered_operations(&self) -> usize {
        self.inner.recovered_operations
    }

    pub fn is_quarantined(&self) -> bool {
        self.inner.quarantined.load(Ordering::Acquire)
    }

    pub async fn recovery_report(&self) -> Result<RecoveryReport, BrokerError> {
        let inner = Arc::clone(&self.inner);
        let mut report = blocking(None, move || {
            inspect_cleanup(&inner.store, inner.recovered_operations)
        })
        .await?;
        report.restart_required = self.inner.restart_required.load(Ordering::Acquire);
        report.quarantined |= self.is_quarantined() || report.restart_required;
        Ok(report)
    }

    pub async fn acknowledge_cleanup(
        &self,
        operation_id: &str,
        expected_result_digest: &str,
        operator: &str,
        note: &str,
    ) -> Result<RecoveryReport, BrokerError> {
        validate_identifier(operation_id)?;
        validate_identifier(operator)?;
        if expected_result_digest.len() != 64
            || !expected_result_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || note.trim().is_empty()
            || note.len() > 4096
            || note.chars().any(char::is_control)
        {
            return Err(BrokerError::InvalidInvocation(
                "cleanup requires an exact digest and a nonempty bounded note".into(),
            ));
        }
        let _gate = tokio::time::timeout(
            self.inner.limits.queue_timeout,
            Arc::clone(&self.inner.exclusion).write_owned(),
        )
        .await
        .map_err(|_| BrokerError::RecoveryBusy)?;
        let inner = Arc::clone(&self.inner);
        let operation_id = operation_id.to_owned();
        let expected_result_digest = expected_result_digest.to_owned();
        let operator = operator.to_owned();
        let note = note.to_owned();
        let mut report = blocking(None, move || {
            let status = inner.store.tool_cleanup_status(&operation_id)?;
            if status.original_result_digest != expected_result_digest {
                return Err(BrokerError::CleanupAttestationConflict { operation_id });
            }
            let attestation = match status.attestation {
                Some(existing) => {
                    if existing.operator != operator
                        || existing.note != note
                        || existing.original_result_digest != expected_result_digest
                    {
                        return Err(BrokerError::CleanupAttestationConflict { operation_id });
                    }
                    existing
                }
                None => {
                    let call = inner.store.tool_call(&operation_id)?.ok_or_else(|| {
                        BrokerError::CorruptJournal {
                            operation_id: operation_id.clone(),
                        }
                    })?;
                    ToolCleanupAttestation {
                        operation_id,
                        original_result_digest: expected_result_digest,
                        operator,
                        note,
                        attested_at: now()?.max(call.updated_at),
                    }
                }
            };
            inner.store.attest_tool_cleanup(&attestation)?;
            inspect_cleanup(&inner.store, inner.recovered_operations)
        })
        .await?;
        report.restart_required = self.inner.restart_required.load(Ordering::Acquire);
        report.quarantined |= report.restart_required;
        self.inner
            .quarantined
            .store(report.quarantined, Ordering::Release);
        Ok(report)
    }

    pub async fn execute(
        &self,
        authority: &ExecutionAuthority,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
    ) -> Result<BrokerOutcome, BrokerError> {
        self.execute_retained(authority, invocation, cancellation, None)
            .await
    }

    async fn execute_retained(
        &self,
        authority: &ExecutionAuthority,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
        owner: Option<RetainedToolOwner>,
    ) -> Result<BrokerOutcome, BrokerError> {
        if self.is_quarantined() {
            return Err(BrokerError::Quarantined);
        }
        validate_identifier(&invocation.operation_id)?;
        let admission = Arc::clone(&self.inner.admission)
            .try_acquire_owned()
            .map_err(|_| BrokerError::Busy)?;
        let operation_id = invocation.operation_id.clone();
        let authority = authority.clone();
        let inner = Arc::clone(&self.inner);
        let cancellation = cancellation.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let supervised = tokio::spawn(async move {
            let _admission = admission;
            inner.run(authority, invocation, cancellation, owner).await
        });
        supervised.await.map_err(|error| {
            self.inner.quarantined.store(true, Ordering::Release);
            self.inner.restart_required.store(true, Ordering::Release);
            BrokerError::OutcomeUnknown {
                operation_id,
                reason: UnknownReason::ToolPanicked,
                cleanup_completed: false,
                journal_error: Some(diagnostic(&error)),
            }
        })?
    }
}

impl TaskToolBroker {
    pub fn authority(&self) -> &ExecutionAuthority {
        &self.authority
    }

    pub async fn execute(
        &self,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
    ) -> Result<BrokerOutcome, BrokerError> {
        self.broker
            .execute(&self.authority, invocation, cancellation)
            .await
    }

    pub(crate) async fn execute_retained(
        &self,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
        owner: RetainedToolOwner,
    ) -> Result<BrokerOutcome, BrokerError> {
        self.broker
            .execute_retained(&self.authority, invocation, cancellation, Some(owner))
            .await
    }
}

impl BrokerInner {
    async fn run(
        self: Arc<Self>,
        authority: ExecutionAuthority,
        invocation: ToolInvocation,
        cancellation: CancellationToken,
        owner: Option<RetainedToolOwner>,
    ) -> Result<BrokerOutcome, BrokerError> {
        let deadline = Instant::now() + self.limits.queue_timeout;
        let inner = Arc::clone(&self);
        let prepare_authority = authority.clone();
        let prepare_invocation = invocation.clone();
        let (call, decision, concurrency) = blocking(owner.clone(), move || {
            inner
                .registry
                .validate(&prepare_invocation.tool_name, &prepare_invocation.arguments)?;
            let descriptor = inner.registry.descriptor(&prepare_invocation.tool_name)?;
            let decision = inner.authorizer.authorize(
                &prepare_authority,
                &prepare_invocation.operation_id,
                &prepare_invocation.tool_name,
                &prepare_invocation.arguments,
                now()?,
            )?;
            let workspace_root = prepare_authority
                .workspace_root()
                .to_str()
                .ok_or_else(|| BrokerError::InvalidInvocation("workspace must be UTF-8".into()))?;
            let descriptor_digest =
                format!("{:x}", Sha256::digest(serde_json::to_vec(descriptor)?));
            let request = encode_value(&JournalRequest {
                version: RECORD_VERSION,
                arguments: &prepare_invocation.arguments,
                workspace_root,
                mode: prepare_authority.mode(),
                descriptor_digest,
                execution_timeout: inner.limits.execution_timeout,
                cancellation_grace: inner.limits.cancellation_grace,
            })?;
            let timestamp = now()?;
            let row = ToolCallRow {
                id: prepare_invocation.operation_id,
                conversation_id: prepare_authority.conversation_id().into(),
                task_id: Some(prepare_authority.task_id().into()),
                tool_name: prepare_invocation.tool_name,
                state: ToolCallState::Pending,
                request,
                result: None,
                created_at: timestamp,
                updated_at: timestamp,
            };
            Ok((
                inner.store.reserve_tool_call(&row)?.call,
                decision,
                descriptor.concurrency,
            ))
        })
        .await?;
        if let Some(outcome) = replay(&call)? {
            return Ok(outcome);
        }
        if cancellation.is_cancelled() {
            return self
                .finish_pending(
                    &call,
                    OperationResult::Cancelled {
                        reason: Interruption::CancellationRequested,
                    },
                    owner.clone(),
                )
                .await;
        }
        if let Some(outcome) = self
            .enforce_decision(&call, decision, owner.clone())
            .await?
        {
            return Ok(outcome);
        }
        let admission = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return self.finish_pending(&call, OperationResult::Cancelled { reason: Interruption::CancellationRequested }, owner.clone()).await,
            _ = tokio::time::sleep_until(deadline) => return self.finish_pending(&call, OperationResult::Cancelled { reason: Interruption::QueueDeadline }, owner.clone()).await,
            permit = Arc::clone(&self.execution).acquire_owned() => permit.map_err(|error| BrokerError::Worker(error.to_string()))?,
        };
        let gate = async {
            match concurrency {
                ToolConcurrency::Concurrent => ExclusionGuard::Concurrent {
                    _guard: Arc::clone(&self.exclusion).read_owned().await,
                },
                ToolConcurrency::Exclusive => ExclusionGuard::Exclusive {
                    _guard: Arc::clone(&self.exclusion).write_owned().await,
                },
            }
        };
        let _gate = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return self.finish_pending(&call, OperationResult::Cancelled { reason: Interruption::CancellationRequested }, owner.clone()).await,
            _ = tokio::time::sleep_until(deadline) => return self.finish_pending(&call, OperationResult::Cancelled { reason: Interruption::QueueDeadline }, owner.clone()).await,
            gate = gate => gate,
        };
        let _admission = admission;
        if self.quarantined.load(Ordering::Acquire) {
            return Err(BrokerError::Quarantined);
        }
        let context = ToolExecutionContext::new(
            &invocation.operation_id,
            authority.task_id(),
            authority.workspace_root().to_path_buf(),
            cancellation.clone(),
        )?
        .with_retained_owner(owner.clone());
        let arguments = invocation_arguments(&call)?;
        let inner = Arc::clone(&self);
        let claim_call = call.clone();
        let claim_cancellation = cancellation.clone();
        let claim = blocking(owner.clone(), move || {
            let decision = inner.authorizer.authorize(
                &authority,
                &invocation.operation_id,
                &invocation.tool_name,
                &invocation.arguments,
                now()?,
            )?;
            if !matches!(decision, AuthorizationDecision::Allow { .. }) {
                return Ok(ExecutionClaim::NotAllowed(decision));
            }
            if claim_cancellation.is_cancelled() {
                return Ok(ExecutionClaim::Cancelled(
                    Interruption::CancellationRequested,
                ));
            }
            if Instant::now() >= deadline {
                return Ok(ExecutionClaim::Cancelled(Interruption::QueueDeadline));
            }
            if inner.quarantined.load(Ordering::Acquire) {
                return Err(BrokerError::Quarantined);
            }
            match inner.store.transition_tool_call(
                &claim_call.id,
                ToolCallState::Pending,
                ToolCallState::Running,
                None,
                now()?.max(claim_call.updated_at),
            ) {
                Ok(()) => Ok(ExecutionClaim::Acquired),
                Err(StorageError::StateConflict { .. }) => Ok(ExecutionClaim::Competing),
                Err(error) => Err(error.into()),
            }
        })
        .await?;
        match claim {
            ExecutionClaim::Acquired => {}
            ExecutionClaim::Competing => return self.refresh(&call.id, owner.clone()).await,
            ExecutionClaim::NotAllowed(decision) => {
                return self
                    .enforce_decision(&call, decision, owner.clone())
                    .await?
                    .ok_or_else(|| BrokerError::Worker("invalid execution claim".into()));
            }
            ExecutionClaim::Cancelled(reason) => {
                return self
                    .finish_pending(&call, OperationResult::Cancelled { reason }, owner.clone())
                    .await;
            }
        }
        let mut lease = ExecutionLease {
            inner: Arc::clone(&self),
            armed: true,
        };
        let mut execution = Box::pin(
            AssertUnwindSafe(self.registry.execute(&call.tool_name, &context, arguments))
                .catch_unwind(),
        );
        let execution_deadline = Instant::now() + self.limits.execution_timeout;
        let (returned, interruption) = tokio::select! {
            result = &mut execution => (Some(result), None),
            _ = cancellation.cancelled() => (None, Some(Interruption::CancellationRequested)),
            _ = tokio::time::sleep_until(execution_deadline) => (None, Some(Interruption::ExecutionDeadline)),
        };
        let returned = if returned.is_some() {
            returned
        } else {
            cancellation.cancel();
            tokio::time::timeout(self.limits.cancellation_grace, &mut execution)
                .await
                .ok()
        };
        let interruption = interruption.or_else(|| {
            if cancellation.is_cancelled() {
                Some(Interruption::CancellationRequested)
            } else if Instant::now() >= execution_deadline {
                Some(Interruption::ExecutionDeadline)
            } else {
                None
            }
        });
        let result = match returned {
            Some(Ok(Ok(output))) if output.is_error => OperationResult::Failed {
                code: "completed_tool_error".into(),
                message: "Tool completed with an error result".into(),
                output: Some(output),
            },
            Some(Ok(Ok(output))) => OperationResult::Succeeded { output },
            Some(Ok(Err(ToolError::FailedWithoutSideEffects { message, .. }))) => {
                OperationResult::Failed {
                    code: "failed_without_side_effects".into(),
                    message: diagnostic(&message),
                    output: None,
                }
            }
            Some(Ok(Err(ToolError::EffectsUnknown {
                message,
                cleanup_completed,
                ..
            }))) => OperationResult::Unknown {
                reason: if interruption.is_some() {
                    UnknownReason::CancellationAfterInvocation
                } else {
                    UnknownReason::ToolFailureAfterInvocation
                },
                cleanup_completed,
                diagnostic: Some(diagnostic(&message)),
            },
            Some(Ok(Err(error))) => OperationResult::Unknown {
                reason: if matches!(error, ToolError::Cancelled { .. }) {
                    UnknownReason::CancellationAfterInvocation
                } else {
                    UnknownReason::ToolFailureAfterInvocation
                },
                cleanup_completed: false,
                diagnostic: Some(diagnostic(&error)),
            },
            Some(Err(_)) => OperationResult::Unknown {
                reason: UnknownReason::ToolPanicked,
                cleanup_completed: false,
                diagnostic: None,
            },
            None => OperationResult::Unknown {
                reason: UnknownReason::CleanupDeadline,
                cleanup_completed: false,
                diagnostic: None,
            },
        };
        drop(execution);
        let mut record = record(result, interruption);
        let encoded = match encode_record(&record) {
            Ok(encoded) => encoded,
            Err(error) => {
                record = record_with_unknown(
                    UnknownReason::ResultNotPersistable,
                    true,
                    Some(diagnostic(&error)),
                    interruption,
                );
                encode_record(&record).map_err(|error| BrokerError::OutcomeUnknown {
                    operation_id: call.id.clone(),
                    reason: UnknownReason::ResultNotPersistable,
                    cleanup_completed: true,
                    journal_error: Some(diagnostic(&error)),
                })?
            }
        };
        let store = self.store.clone();
        let operation_id = call.id.clone();
        let next = record.result.state();
        let timestamp = now()
            .map_err(|error| BrokerError::OutcomeUnknown {
                operation_id: call.id.clone(),
                reason: UnknownReason::JournalWriteFailed,
                cleanup_completed: !matches!(
                    record.result,
                    OperationResult::Unknown {
                        cleanup_completed: false,
                        ..
                    }
                ),
                journal_error: Some(diagnostic(&error)),
            })?
            .max(call.updated_at);
        let cleanup_completed = !matches!(
            record.result,
            OperationResult::Unknown {
                cleanup_completed: false,
                ..
            }
        );
        let persisted = blocking(owner.clone(), move || {
            Ok(store.transition_tool_call(
                &operation_id,
                ToolCallState::Running,
                next,
                Some(&encoded),
                timestamp,
            )?)
        })
        .await;
        if let Err(error) = persisted {
            let unknown = record_with_unknown(
                UnknownReason::JournalWriteFailed,
                cleanup_completed,
                Some(diagnostic(&error)),
                interruption,
            );
            let store = self.store.clone();
            let operation_id = call.id.clone();
            let encoded = encode_record(&unknown).map_err(|error| BrokerError::OutcomeUnknown {
                operation_id: call.id.clone(),
                reason: UnknownReason::JournalWriteFailed,
                cleanup_completed,
                journal_error: Some(diagnostic(&error)),
            })?;
            let attempt = blocking(owner.clone(), move || {
                Ok(store.transition_tool_call(
                    &operation_id,
                    ToolCallState::Running,
                    ToolCallState::Unknown,
                    Some(&encoded),
                    timestamp,
                )?)
            })
            .await;
            return Err(BrokerError::OutcomeUnknown {
                operation_id: call.id,
                reason: UnknownReason::JournalWriteFailed,
                cleanup_completed,
                journal_error: Some(format!(
                    "{}; recovery: {}",
                    diagnostic(&error),
                    attempt
                        .err()
                        .map_or_else(|| "unknown recorded".into(), |error| diagnostic(&error))
                )),
            });
        }
        if matches!(
            record.result,
            OperationResult::Unknown {
                cleanup_completed: false,
                ..
            }
        ) {
            self.quarantined.store(true, Ordering::Release);
        }
        if matches!(
            record.result,
            OperationResult::Unknown {
                reason: UnknownReason::ToolPanicked,
                ..
            }
        ) {
            self.restart_required.store(true, Ordering::Release);
        }
        lease.armed = false;
        completed(call.id, record, false)
    }

    async fn enforce_decision(
        &self,
        call: &ToolCallRow,
        decision: AuthorizationDecision,
        owner: Option<RetainedToolOwner>,
    ) -> Result<Option<BrokerOutcome>, BrokerError> {
        match decision {
            AuthorizationDecision::Allow { .. } => Ok(None),
            AuthorizationDecision::Ask { approval } => Ok(Some(BrokerOutcome::ApprovalRequired {
                operation_id: call.id.clone(),
                approval,
            })),
            AuthorizationDecision::Deny { reason } => Ok(Some(
                self.finish_pending(call, OperationResult::Denied { reason }, owner)
                    .await?,
            )),
        }
    }

    async fn finish_pending(
        &self,
        call: &ToolCallRow,
        result: OperationResult,
        owner: Option<RetainedToolOwner>,
    ) -> Result<BrokerOutcome, BrokerError> {
        let record = record(result, None);
        let encoded = encode_record(&record)?;
        let store = self.store.clone();
        let operation_id = call.id.clone();
        let next = record.result.state();
        let timestamp = now()?.max(call.updated_at);
        let changed = blocking(owner.clone(), move || {
            match store.transition_tool_call(
                &operation_id,
                ToolCallState::Pending,
                next,
                Some(&encoded),
                timestamp,
            ) {
                Ok(()) => Ok(true),
                Err(StorageError::StateConflict { .. }) => Ok(false),
                Err(error) => Err(error.into()),
            }
        })
        .await?;
        if changed {
            completed(call.id.clone(), record, false)
        } else {
            self.refresh(&call.id, owner).await
        }
    }

    async fn refresh(
        &self,
        operation_id: &str,
        owner: Option<RetainedToolOwner>,
    ) -> Result<BrokerOutcome, BrokerError> {
        let store = self.store.clone();
        let operation_id = operation_id.to_owned();
        let call = blocking(owner, move || {
            store
                .tool_call(&operation_id)?
                .ok_or(BrokerError::CorruptJournal { operation_id })
        })
        .await?;
        replay(&call)?.ok_or(BrokerError::CorruptJournal {
            operation_id: call.id,
        })
    }
}

fn inspect_cleanup(
    store: &Store,
    recovered_operations: usize,
) -> Result<RecoveryReport, BrokerError> {
    let mut after_id = None;
    let mut requirements = Vec::new();
    let mut unresolved_count = 0_usize;
    let mut page_size = 8;
    loop {
        let page =
            match store.tool_calls_after_id(ToolCallState::Unknown, after_id.as_deref(), page_size)
            {
                Ok(page) => page,
                Err(StorageError::ReadBudgetExceeded) if page_size > 1 => {
                    page_size /= 2;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
        if page.is_empty() {
            break;
        }
        for call in page {
            after_id = Some(call.id.clone());
            let risk = match call
                .result
                .as_ref()
                .and_then(|value| serde_json::from_value::<ExecutionRecord>(value.clone()).ok())
            {
                Some(record) if record.version == RECORD_VERSION => match record.result {
                    OperationResult::Unknown {
                        cleanup_completed: true,
                        ..
                    } => continue,
                    OperationResult::Unknown {
                        cleanup_completed: false,
                        ..
                    } => CleanupRisk::IncompleteCleanup,
                    _ => CleanupRisk::InvalidJournalRecord,
                },
                _ => CleanupRisk::InvalidJournalRecord,
            };
            let status = store.tool_cleanup_status(&call.id)?;
            if status.attestation.as_ref().is_some_and(|attestation| {
                attestation.original_result_digest == status.original_result_digest
            }) {
                continue;
            }
            unresolved_count = unresolved_count.saturating_add(1);
            if requirements.len() < MAX_RECOVERY_REPORT {
                requirements.push(CleanupRequirement {
                    operation_id: call.id,
                    original_result_digest: status.original_result_digest,
                    risk,
                });
            }
        }
    }
    Ok(RecoveryReport {
        recovered_operations,
        unresolved_count,
        requirements,
        restart_required: false,
        quarantined: unresolved_count != 0,
    })
}

fn invocation_arguments(call: &ToolCallRow) -> Result<Value, BrokerError> {
    call.request
        .get("arguments")
        .cloned()
        .ok_or_else(|| BrokerError::CorruptJournal {
            operation_id: call.id.clone(),
        })
}

fn replay(call: &ToolCallRow) -> Result<Option<BrokerOutcome>, BrokerError> {
    match call.state {
        ToolCallState::Pending => Ok(None),
        ToolCallState::Running => Ok(Some(BrokerOutcome::InFlight {
            operation_id: call.id.clone(),
        })),
        _ => {
            let value = call
                .result
                .as_ref()
                .ok_or_else(|| BrokerError::CorruptJournal {
                    operation_id: call.id.clone(),
                })?;
            let record: ExecutionRecord =
                serde_json::from_value(value.clone()).map_err(|_| BrokerError::CorruptJournal {
                    operation_id: call.id.clone(),
                })?;
            if record.version != RECORD_VERSION || record.result.state() != call.state {
                return Err(BrokerError::CorruptJournal {
                    operation_id: call.id.clone(),
                });
            }
            completed(call.id.clone(), record, true).map(Some)
        }
    }
}

fn completed(
    operation_id: String,
    record: ExecutionRecord,
    replayed: bool,
) -> Result<BrokerOutcome, BrokerError> {
    if let OperationResult::Unknown {
        reason,
        cleanup_completed,
        ..
    } = &record.result
    {
        return Err(BrokerError::OutcomeUnknown {
            operation_id,
            reason: *reason,
            cleanup_completed: *cleanup_completed,
            journal_error: None,
        });
    }
    Ok(BrokerOutcome::Completed {
        operation_id,
        record,
        replayed,
    })
}

fn record(result: OperationResult, interruption: Option<Interruption>) -> ExecutionRecord {
    ExecutionRecord {
        version: RECORD_VERSION,
        result,
        interruption,
    }
}

fn record_with_unknown(
    reason: UnknownReason,
    cleanup_completed: bool,
    diagnostic: Option<String>,
    interruption: Option<Interruption>,
) -> ExecutionRecord {
    record(
        OperationResult::Unknown {
            reason,
            cleanup_completed,
            diagnostic,
        },
        interruption,
    )
}

fn encode_record(record: &ExecutionRecord) -> Result<Value, BrokerError> {
    encode_value(record)
}

fn encode_value(value: &impl Serialize) -> Result<Value, BrokerError> {
    let mut writer = RecordWriter { bytes: Vec::new() };
    serde_json::to_writer(&mut writer, value)?;
    Ok(serde_json::from_slice(&writer.bytes)?)
}

struct RecordWriter {
    bytes: Vec<u8>,
}

impl Write for RecordWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_RECORD_BYTES {
            return Err(io::Error::other("operation record exceeds 1 MiB"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn blocking<T: Send + 'static>(
    owner: Option<RetainedToolOwner>,
    operation: impl FnOnce() -> Result<T, BrokerError> + Send + 'static,
) -> Result<T, BrokerError> {
    tokio::task::spawn_blocking(move || {
        let _retained_owner = owner;
        operation()
    })
    .await
    .map_err(|error| BrokerError::Worker(diagnostic(&error)))?
}

fn now() -> Result<i64, BrokerError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BrokerError::Clock)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| BrokerError::Clock)
}

fn diagnostic(value: &impl std::fmt::Display) -> String {
    value
        .to_string()
        .chars()
        .take(MAX_DIAGNOSTIC_CHARACTERS)
        .collect()
}

fn validate_identifier(value: &str) -> Result<(), BrokerError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(BrokerError::InvalidInvocation(
            "operation identifier must contain 1 to 256 bytes without controls".into(),
        ));
    }
    Ok(())
}

fn validate_limits(limits: &BrokerLimits) -> Result<(), BrokerError> {
    let durations = [
        limits.queue_timeout,
        limits.execution_timeout,
        limits.cancellation_grace,
    ];
    if !(1..=64).contains(&limits.max_concurrent)
        || !(limits.max_concurrent..=1_024).contains(&limits.max_in_flight)
        || durations.iter().any(|duration| {
            *duration < Duration::from_millis(1) || *duration > Duration::from_secs(86_400)
        })
    {
        return Err(BrokerError::InvalidConfiguration(
            "concurrency must be 1..64, in-flight between concurrency and 1024, durations 1ms..24h"
                .into(),
        ));
    }
    Ok(())
}
