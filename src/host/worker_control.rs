use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::commands::EffectiveCommand;
use super::inbox::{AppliedReceipt, CommandOrigin, CommandPhase, HostCommandRecord, InboxInput};
use super::runtime::{HostError, HostInner, command_receipt};
use super::worker::service::WorkerCoordinator;
use super::worker::{
    WorkerCoordinatorOperation, WorkerCoordinatorOperationResult, WorkerCoordinatorQuery,
    WorkerCoordinatorQueryResult,
};
use crate::agent::TextCommand;
use crate::ipc::{OwnerCommand, OwnerOperationResult};
use crate::protocol::{
    CommandId, ConversationEvent, EventEnvelope, MessageContent, MessageRole, UnixMillis,
};
use crate::scheduler::{
    RemoteOwnerApproval, RemoteTaskSnapshot, RemoteTaskSubmission, TaskSubmission,
};
use crate::worker::{WorkerError, WorkerJobBinding, WorkerManifest};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkerAdmissionSnapshot {
    version: u16,
    ticket: Uuid,
    executor_fingerprint: String,
    submission: RemoteTaskSubmission,
}

impl WorkerAdmissionSnapshot {
    fn matches_operation(&self, operation: &WorkerCoordinatorOperation) -> bool {
        matches!(operation, WorkerCoordinatorOperation::Delegate {
            ticket, expected_manifest_digest, expected_executor_fingerprint,
        } if self.version == 1 && *ticket == self.ticket
            && *expected_manifest_digest == self.submission.binding.manifest_digest
            && *expected_executor_fingerprint == self.executor_fingerprint)
    }

    fn validate_record(&self, record: &HostCommandRecord) -> Result<(), HostError> {
        self.submission.binding.validate()?;
        self.submission.task.execution.validate()?;
        if self.version != 1
            || self.ticket.is_nil()
            || !matches!(record.origin, CommandOrigin::LocalOwner)
            || self.submission.approval.command_id != record.command_id
            || self.submission.task.conversation_id != record.conversation_id
            || self.submission.task.accepted_at != record.accepted_at
            || self.executor_fingerprint != self.submission.binding.executor.fingerprint()
            || self.submission.task.command_id == record.command_id
        {
            return Err(HostError::Invalid(
                "invalid retained worker admission mapping",
            ));
        }
        let Some(InboxInput::Owner(OwnerCommand::Initialize { .. })) = record.input()? else {
            return Err(HostError::Invalid(
                "worker admission requires its original owner initialization",
            ));
        };
        Ok(())
    }
}

impl HostInner {
    pub(super) fn worker_coordinator(&self) -> Result<Arc<WorkerCoordinator>, HostError> {
        self.worker.get().cloned().ok_or(HostError::Invalid(
            "worker coordination is disabled; configure its relay and restart the host",
        ))
    }

    pub(super) async fn worker_query(
        self: &Arc<Self>,
        query: WorkerCoordinatorQuery,
    ) -> Result<WorkerCoordinatorQueryResult, HostError> {
        query.validate()?;
        if let Some(coordinator) = self.worker.get() {
            return Ok(coordinator.query(query).await?);
        }
        if !matches!(query, WorkerCoordinatorQuery::Status) {
            return Err(HostError::Invalid(
                "worker coordination is disabled; configure its relay and restart the host",
            ));
        }
        let configured = self
            .blocking(|host| Ok(host.settings.snapshot()?.worker.is_some()))
            .await?;
        Ok(WorkerCoordinatorQueryResult::Status {
            status: super::worker::WorkerCoordinatorStatus {
                enabled: false,
                accepting: false,
                draining: self.draining.load(std::sync::atomic::Ordering::Acquire),
                relay: crate::ipc::RelayStatus {
                    origin: None,
                    connection: crate::ipc::RelayConnectionState::Disabled,
                },
                admission_blocked: false,
                preparations: 0,
                in_flight: 0,
                jobs: None,
                unresolved_remote_tasks: None,
                failure_code: configured.then(|| "worker_restart_required".to_owned()),
            },
        })
    }

    pub(super) async fn worker_operation(
        self: &Arc<Self>,
        operation: WorkerCoordinatorOperation,
        request_id: CommandId,
        cancellation: CancellationToken,
    ) -> Result<OwnerOperationResult, HostError> {
        operation.validate()?;
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled.into());
        }
        let coordinator = self.worker_coordinator()?;
        let permit = if matches!(
            operation,
            WorkerCoordinatorOperation::Cancel { .. }
                | WorkerCoordinatorOperation::RevokePeer { .. }
                | WorkerCoordinatorOperation::Discard { .. }
        ) {
            coordinator.begin_cleanup_operation().await?
        } else {
            coordinator.begin_operation().await?
        };
        let host = Arc::clone(self);
        let result = tokio::spawn(async move {
            let _permit = permit;
            let result = host
                .worker_operation_owned(&coordinator, &operation, request_id)
                .await?;
            result.validate_for(&operation, request_id)?;
            Ok::<_, HostError>(OwnerOperationResult::WorkerCoordinator {
                result: Box::new(result),
            })
        })
        .await
        .map_err(|_| HostError::Worker)??;
        Ok(result)
    }

    async fn worker_operation_owned(
        self: &Arc<Self>,
        coordinator: &Arc<WorkerCoordinator>,
        operation: &WorkerCoordinatorOperation,
        request_id: CommandId,
    ) -> Result<WorkerCoordinatorOperationResult, HostError> {
        match operation {
            WorkerCoordinatorOperation::Prepare { request } => {
                let preparation = coordinator
                    .prepare((**request).clone(), CancellationToken::new())
                    .await?;
                Ok(WorkerCoordinatorOperationResult::Prepared {
                    preparation: Box::new(preparation),
                })
            }
            WorkerCoordinatorOperation::Discard { ticket } => {
                coordinator.discard(*ticket).await?;
                Ok(WorkerCoordinatorOperationResult::Discarded { ticket: *ticket })
            }
            WorkerCoordinatorOperation::PinPeer {
                keys,
                expected_fingerprint,
            } => {
                coordinator.ensure_enabled()?;
                coordinator
                    .endpoint()
                    .pin_peer(keys.clone(), expected_fingerprint, UnixMillis::now()?)
                    .await?;
                let peer = coordinator
                    .endpoint()
                    .peers()
                    .await?
                    .into_iter()
                    .find(|peer| peer.keys.device_id == keys.device_id)
                    .ok_or(HostError::Invalid("pinned worker disappeared"))?;
                Ok(WorkerCoordinatorOperationResult::PeerPinned { peer })
            }
            WorkerCoordinatorOperation::Delegate { .. } => {
                self.worker_delegate(coordinator, operation, request_id)
                    .await
            }
            WorkerCoordinatorOperation::Cancel { task_id, binding } => {
                let mutation = Arc::clone(&self.mutation).lock_owned().await;
                let native = self.scheduler.remote_status(*task_id).await?;
                if worker_job_binding(&native) != *binding {
                    return Err(HostError::Invalid(
                        "worker cancellation binding differs from native reservation",
                    ));
                }
                drop(mutation);
                let job = coordinator
                    .endpoint()
                    .snapshot(binding.delegation_id)
                    .await?
                    .ok_or(HostError::Invalid(
                        "worker cancellation delegation is unknown",
                    ))?;
                if job.binding != *binding {
                    return Err(HostError::Invalid(
                        "worker cancellation binding differs from delegation",
                    ));
                }
                if !job.cancellation_requested {
                    coordinator
                        .endpoint()
                        .cancel(binding.clone(), request_id, UnixMillis::now()?)
                        .await?;
                }
                {
                    let _mutation = self.mutation.lock().await;
                    self.scheduler.cancel(*task_id).await?;
                }
                let WorkerCoordinatorQueryResult::Task { task } = coordinator
                    .query(WorkerCoordinatorQuery::Task { task_id: *task_id })
                    .await?
                else {
                    return Err(HostError::Invalid("worker task inspection changed kind"));
                };
                Ok(WorkerCoordinatorOperationResult::Cancellation {
                    binding: binding.clone(),
                    task: Box::new(task),
                })
            }
            WorkerCoordinatorOperation::RevokePeer { device_id } => {
                coordinator
                    .endpoint()
                    .revoke_peer(*device_id, request_id, UnixMillis::now()?)
                    .await?;
                let jobs = coordinator.endpoint().jobs().await?;
                for job in jobs.into_iter().filter(|job| job.peer == *device_id) {
                    let query = coordinator
                        .query(WorkerCoordinatorQuery::Delegation {
                            delegation_id: job.binding.delegation_id,
                        })
                        .await?;
                    if let WorkerCoordinatorQueryResult::Delegation { job: Some(job) } = query
                        && let Some(native) = job.native
                    {
                        let _mutation = self.mutation.lock().await;
                        self.scheduler.cancel(native.task_id).await?;
                    }
                }
                let peer = coordinator
                    .endpoint()
                    .peers()
                    .await?
                    .into_iter()
                    .find(|peer| peer.keys.device_id == *device_id)
                    .ok_or(HostError::Invalid("revoked worker disappeared"))?;
                Ok(WorkerCoordinatorOperationResult::PeerRevoked { peer })
            }
        }
    }

    async fn worker_delegate(
        self: &Arc<Self>,
        coordinator: &Arc<WorkerCoordinator>,
        operation: &WorkerCoordinatorOperation,
        request_id: CommandId,
    ) -> Result<WorkerCoordinatorOperationResult, HostError> {
        let existing = self
            .blocking(move |host| Ok(host.inbox.record(request_id)?))
            .await?;
        if let Some(record) = existing {
            let admission = retained_admission(&record)?;
            if !admission.matches_operation(operation) {
                return Err(HostError::Invalid(
                    "worker delegation request identity changed",
                ));
            }
            if matches!(record.phase, CommandPhase::Accepted | CommandPhase::Applied) {
                self.recover_worker_admission(record.clone(), admission.clone())
                    .await?;
            }
            return self
                .worker_admission_result(coordinator, request_id, &admission)
                .await;
        }
        coordinator.ensure_enabled()?;
        let WorkerCoordinatorOperation::Delegate {
            ticket,
            expected_manifest_digest,
            expected_executor_fingerprint,
        } = operation
        else {
            return Err(HostError::Invalid("worker delegation operation required"));
        };
        let prepared = coordinator
            .take_prepared(
                *ticket,
                expected_manifest_digest,
                expected_executor_fingerprint,
                request_id,
            )
            .await?;
        let (disclosure, prepared, budget_lease) = prepared.into_parts();
        let _budget_lease = budget_lease;
        let accepted_at = UnixMillis::now()?;
        let approved = prepared.approve(UnixMillis::now()?, Duration::from_secs(30))?;
        let staged = coordinator
            .endpoint()
            .stage_approved(approved, UnixMillis::now()?)
            .await?;
        let owned = coordinator
            .endpoint()
            .acquire_staged(staged, UnixMillis::now()?)
            .await?;
        let mutation = Arc::clone(&self.mutation).lock_owned().await;
        self.ensure_accepting()?;
        coordinator.ensure_enabled()?;
        let task = TaskSubmission {
            task_id: disclosure.local.task_id,
            conversation_id: disclosure.local.conversation_id,
            command_id: disclosure.local.input_command_id,
            accepted_at,
            execution: disclosure.local.execution.clone(),
        };
        let snapshot = WorkerAdmissionSnapshot {
            version: 1,
            ticket: disclosure.ticket,
            executor_fingerprint: disclosure.executor_fingerprint,
            submission: RemoteTaskSubmission {
                task: task.clone(),
                binding: owned.binding().clone(),
                approval: RemoteOwnerApproval {
                    command_id: request_id,
                    approved_at: owned.approved_at(),
                    expires_at: owned.expires_at(),
                },
            },
        };
        let owner = OwnerCommand::Initialize {
            conversation_id: disclosure.local.conversation_id,
            command_id: request_id,
            title: disclosure.local.title,
            configuration: disclosure.manifest.model.configuration.clone(),
        };
        let input = InboxInput::Owner(owner.clone());
        let original = serde_json::to_value(owner)?;
        let options = serde_json::to_value(EffectiveCommand::RemoteInitialize {
            admission: Box::new(snapshot.clone()),
        })?;
        let saved = self
            .blocking(move |host| {
                Ok(host.inbox.reserve(
                    &input,
                    &original,
                    &CommandOrigin::LocalOwner,
                    &options,
                    accepted_at,
                )?)
            })
            .await?;
        if saved.record.effective_options.as_ref()
            != Some(&serde_json::to_value(EffectiveCommand::RemoteInitialize {
                admission: Box::new(snapshot.clone()),
            })?)
        {
            return Err(HostError::Invalid(
                "worker initialization identity conflicts with its retained execution snapshot",
            ));
        }
        snapshot.validate_record(&saved.record)?;
        let instruction = disclosure.manifest.instruction()?;
        self.agent
            .create_conversation(
                disclosure.local.conversation_id,
                request_id,
                match saved.record.input()? {
                    Some(InboxInput::Owner(OwnerCommand::Initialize { title, .. })) => title,
                    _ => {
                        return Err(HostError::Invalid(
                            "retained worker creation is unavailable",
                        ));
                    }
                },
                disclosure.manifest.model.configuration,
            )
            .await?;
        let acceptance = self
            .agent
            .accept_text(TextCommand {
                conversation_id: disclosure.local.conversation_id,
                command_id: disclosure.local.input_command_id,
                text: instruction,
            })
            .await?;
        let message_id = match acceptance {
            crate::agent::TextAcceptance::Pending { message_id, .. } => message_id,
            crate::agent::TextAcceptance::Completed { .. } => {
                return Err(HostError::Invalid("new worker input already completed"));
            }
        };
        let authorization = owned.into_native_admission(task, request_id, Box::new(mutation))?;
        self.scheduler.admit_remote(authorization).await?;
        self.complete_worker_record(
            request_id,
            snapshot.submission.task.task_id,
            Some(message_id),
        )
        .await?;
        coordinator
            .endpoint()
            .offer_reserved(
                snapshot.submission.binding.clone(),
                coordinator.gate().as_ref(),
                UnixMillis::now()?,
            )
            .await?;
        self.worker_admission_result(coordinator, request_id, &snapshot)
            .await
    }

    pub(super) async fn recover_worker_admission(
        self: &Arc<Self>,
        record: HostCommandRecord,
        admission: WorkerAdmissionSnapshot,
    ) -> Result<AppliedReceipt, HostError> {
        admission.validate_record(&record)?;
        let coordinator = self
            .worker
            .get()
            .cloned()
            .ok_or(HostError::WorkerRecoveryUnavailable)?;
        let binding = admission.submission.binding.clone();
        let job = coordinator
            .endpoint()
            .snapshot(binding.delegation_id)
            .await?
            .ok_or(HostError::Invalid(
                "worker initialization has no retained manifest",
            ))?;
        let manifest = job.manifest.ok_or(HostError::Invalid(
            "worker initialization manifest is unavailable",
        ))?;
        if manifest.fingerprint()? != binding.manifest_digest {
            return Err(HostError::Invalid("worker initialization manifest changed"));
        }
        let _mutation = self.mutation.lock().await;
        let native = self
            .scheduler
            .remote_status(admission.submission.task.task_id)
            .await
            .map_err(|error| match error {
                crate::scheduler::SchedulerError::NotFound(_) => HostError::WorkerUnadmitted,
                error => HostError::from(error),
            })?;
        validate_native_mapping(&record, &admission, &manifest, &native)?;
        let inspected_record = record.clone();
        let inspected_admission = admission.clone();
        let message_id = self
            .blocking(move |host| {
                verify_original_history(
                    &host.agent.store(),
                    &inspected_record,
                    &inspected_admission,
                    &manifest,
                )
            })
            .await?;
        self.complete_worker_record(
            record.command_id,
            admission.submission.task.task_id,
            Some(message_id),
        )
        .await
    }

    async fn complete_worker_record(
        self: &Arc<Self>,
        command_id: CommandId,
        task_id: crate::protocol::TaskId,
        message_id: Option<crate::protocol::MessageId>,
    ) -> Result<AppliedReceipt, HostError> {
        let receipt = self
            .blocking(move |host| {
                let record = host
                    .inbox
                    .record(command_id)?
                    .ok_or(HostError::Invalid("worker command disappeared"))?;
                let receipt = AppliedReceipt {
                    conversation_id: record.conversation_id,
                    task_id: Some(task_id),
                    message_id,
                    operation_id: None,
                };
                host.inbox
                    .mark_applied(command_id, &receipt, UnixMillis::now()?)?;
                host.inbox.finish(
                    command_id,
                    false,
                    &json!({"applied": receipt}),
                    UnixMillis::now()?,
                )?;
                Ok(receipt)
            })
            .await?;
        self.receipt_changed();
        Ok(receipt)
    }

    async fn worker_admission_result(
        self: &Arc<Self>,
        coordinator: &Arc<WorkerCoordinator>,
        request_id: CommandId,
        admission: &WorkerAdmissionSnapshot,
    ) -> Result<WorkerCoordinatorOperationResult, HostError> {
        let receipt = self
            .blocking(move |host| {
                Ok(command_receipt(
                    host.inbox
                        .record(request_id)?
                        .ok_or(HostError::Invalid("worker command disappeared"))?,
                ))
            })
            .await?;
        let WorkerCoordinatorQueryResult::Delegation { job: Some(job) } = coordinator
            .query(WorkerCoordinatorQuery::Delegation {
                delegation_id: admission.submission.binding.delegation_id,
            })
            .await?
        else {
            return Err(HostError::Invalid("worker delegation disappeared"));
        };
        Ok(WorkerCoordinatorOperationResult::Admission {
            ticket: admission.ticket,
            executor_fingerprint: admission.executor_fingerprint.clone(),
            receipt,
            job: Box::new(job),
        })
    }
}

fn retained_admission(record: &HostCommandRecord) -> Result<WorkerAdmissionSnapshot, HostError> {
    let effective: EffectiveCommand =
        serde_json::from_value(record.effective_options.clone().ok_or(HostError::Invalid(
            "worker admission metadata has been archived",
        ))?)?;
    let EffectiveCommand::RemoteInitialize { admission } = effective else {
        return Err(HostError::Invalid(
            "request identity belongs to a different command",
        ));
    };
    admission.validate_record(record)?;
    Ok(*admission)
}

fn worker_job_binding(native: &RemoteTaskSnapshot) -> WorkerJobBinding {
    WorkerJobBinding {
        delegation_id: native.binding.delegation_id,
        attempt_id: native.binding.attempt_id,
        manifest_digest: native.binding.manifest_digest.clone(),
    }
}

fn validate_native_mapping(
    record: &HostCommandRecord,
    admission: &WorkerAdmissionSnapshot,
    manifest: &WorkerManifest,
    native: &RemoteTaskSnapshot,
) -> Result<(), HostError> {
    let Some(InboxInput::Owner(OwnerCommand::Initialize { configuration, .. })) = record.input()?
    else {
        return Err(HostError::Invalid(
            "worker original initialization is unavailable",
        ));
    };
    let expected = &admission.submission;
    if native.binding != expected.binding
        || native.approval != expected.approval
        || native.task.task.id != expected.task.task_id
        || native.task.task.conversation_id != expected.task.conversation_id
        || native.task.task.originating_command_id != expected.task.command_id
        || native.task.task.created_at != expected.task.accepted_at
        || native.task.task.parent_task_id.is_some()
        || native.task.task.instruction != manifest.instruction()?
        || native.configuration != configuration
        || native.configuration != manifest.model.configuration
        || serde_json::to_value(&native.execution)?
            != serde_json::to_value(&expected.task.execution)?
    {
        return Err(HostError::Invalid(
            "retained worker mapping differs from actual native reservation",
        ));
    }
    Ok(())
}

fn verify_original_history(
    store: &crate::storage::Store,
    record: &HostCommandRecord,
    admission: &WorkerAdmissionSnapshot,
    manifest: &WorkerManifest,
) -> Result<crate::protocol::MessageId, HostError> {
    let Some(InboxInput::Owner(OwnerCommand::Initialize { title, .. })) = record.input()? else {
        return Err(HostError::Invalid("worker creation history is unavailable"));
    };
    let created = store
        .command_request(&record.command_id.to_string())?
        .ok_or(HostError::Invalid("worker creation receipt is unavailable"))?;
    let input_id = admission.submission.task.command_id;
    let input = store
        .command_request(&input_id.to_string())?
        .ok_or(HostError::Invalid("worker input receipt is unavailable"))?;
    if created.conversation_id != record.conversation_id.to_string()
        || created.events.len() != 1
        || input.conversation_id != record.conversation_id.to_string()
        || input.events.len() != 1
    {
        return Err(HostError::Invalid(
            "worker original command receipts changed",
        ));
    }
    let creation: EventEnvelope = serde_json::from_value(created.events[0].payload.clone())?;
    let input: EventEnvelope = serde_json::from_value(input.events[0].payload.clone())?;
    if creation.conversation_id != record.conversation_id
        || creation.causation_id != Some(record.command_id)
        || !matches!(creation.payload, ConversationEvent::ConversationCreated { title: Some(original) } if original == title)
        || input.conversation_id != record.conversation_id
        || input.causation_id != Some(input_id)
    {
        return Err(HostError::Invalid("worker original event bindings changed"));
    }
    let ConversationEvent::MessageAppended { message } = input.payload else {
        return Err(HostError::Invalid("worker input was not a user message"));
    };
    if message.role != MessageRole::User
        || message.content
            != vec![MessageContent::Text {
                text: manifest.instruction()?,
            }]
    {
        return Err(HostError::Invalid("worker original instruction changed"));
    }
    Ok(message.id)
}
