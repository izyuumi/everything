use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;

use crate::{
    agent::Agent,
    crypto::DevicePublicKeys,
    protocol::{CommandId, UnixMillis},
    scheduler::{
        RemoteOwnerApproval, RemoteTaskAdmission, RemoteTaskBinding, RemoteTaskSubmission,
        TaskSubmission,
    },
};

use super::{
    DelegationState, StagedDelegation, WorkerEndpoint, WorkerError, WorkerJobBinding,
    WorkerManifest, WorkerRole,
};

pub struct OwnedStagedDelegation {
    staged: StagedDelegation,
    binding: RemoteTaskBinding,
    authority: OwnedMutexGuard<()>,
    endpoint: WorkerEndpoint,
    owner: Agent,
}

impl OwnedStagedDelegation {
    pub fn manifest(&self) -> &WorkerManifest {
        self.staged.manifest()
    }

    pub fn binding(&self) -> &RemoteTaskBinding {
        &self.binding
    }

    pub fn approved_at(&self) -> UnixMillis {
        self.staged.approved_at()
    }

    pub fn expires_at(&self) -> UnixMillis {
        self.staged.expires_at()
    }

    pub(crate) fn into_native_admission(
        self,
        task: TaskSubmission,
        original_owner_command: CommandId,
        host_guard: Box<dyn Send + Sync>,
    ) -> Result<RemoteTaskAdmission, WorkerError> {
        self.staged.validate_for_admission(UnixMillis::now()?)?;
        let manifest = self.staged.manifest();
        validate_policy(manifest, &task.execution)?;
        task.execution
            .authority(task.conversation_id, task.task_id)?;
        if task.task_id == manifest.ids.task_id
            || task.conversation_id == manifest.ids.conversation_id
            || task.command_id == manifest.ids.input_command_id
            || task.command_id == manifest.ids.create_command_id
        {
            return Err(WorkerError::Conflict(
                "local and executor identities must be separate",
            ));
        }
        let configuration = manifest.model.configuration.clone();
        let instruction = manifest.instruction()?;
        let submission = RemoteTaskSubmission {
            task,
            binding: self.binding.clone(),
            approval: RemoteOwnerApproval {
                command_id: original_owner_command,
                approved_at: self.staged.approved_at(),
                expires_at: self.staged.expires_at(),
            },
        };
        let approval_deadline = self.staged.approved.deadline;
        let Self {
            staged,
            authority,
            endpoint,
            owner,
            ..
        } = self;
        Ok(RemoteTaskAdmission::from_staged(
            owner,
            submission,
            configuration,
            instruction,
            approval_deadline,
            Box::new((staged, authority, endpoint)),
            host_guard,
        )?)
    }
}

impl WorkerEndpoint {
    pub async fn acquire_staged(
        &self,
        staged: StagedDelegation,
        now: UnixMillis,
    ) -> Result<OwnedStagedDelegation, WorkerError> {
        if self.role() != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let resources = (self.clone(), staged, authority, self.agent.clone());
        tokio::task::spawn_blocking(move || {
            let resources = resources;
            let fresh_now = UnixMillis::now()?;
            if fresh_now < now {
                return Err(WorkerError::Expired);
            }
            resources.1.validate_for_admission(fresh_now)?;
            let manifest = resources.1.manifest();
            resources.0.validate_manifest_endpoints(manifest)?;
            let peer = resources.0.repository.peer(manifest.executor)?;
            let binding = WorkerJobBinding::from_manifest(manifest)?;
            let saved = resources
                .0
                .repository
                .snapshot(binding.delegation_id)?
                .ok_or(WorkerError::Conflict("staged manifest disappeared"))?;
            if saved.binding != binding
                || saved.peer != peer.device_id
                || saved.cancellation_requested
                || saved.state != DelegationState::Offered
                || saved.result.is_some()
                || saved.rejection.is_some()
                || saved
                    .manifest
                    .as_ref()
                    .map(serde_json::to_vec)
                    .transpose()?
                    != Some(serde_json::to_vec(manifest)?)
            {
                return Err(WorkerError::Conflict(
                    "staged approval is no longer unstarted",
                ));
            }
            let native_binding = manifest_binding(manifest, peer)?;
            Ok(OwnedStagedDelegation {
                staged: resources.1,
                binding: native_binding,
                authority: resources.2,
                endpoint: resources.0,
                owner: resources.3,
            })
        })
        .await
        .map_err(|_| WorkerError::Worker)?
    }
}

pub(super) fn manifest_binding(
    manifest: &WorkerManifest,
    executor: DevicePublicKeys,
) -> Result<RemoteTaskBinding, WorkerError> {
    if executor.device_id != manifest.executor {
        return Err(WorkerError::UntrustedPeer);
    }
    Ok(RemoteTaskBinding {
        delegation_id: manifest.delegation_id,
        attempt_id: manifest.attempt_id,
        manifest_digest: manifest.fingerprint()?,
        coordinator: manifest.coordinator,
        executor,
        relay_origin: manifest.relay_origin.clone(),
        worker_task_id: manifest.ids.task_id,
        worker_conversation_id: manifest.ids.conversation_id,
        worker_create_command_id: manifest.ids.create_command_id,
        worker_input_command_id: manifest.ids.input_command_id,
        created_at: manifest.created_at,
        deadline: manifest.deadline,
    })
}

fn validate_policy(
    manifest: &WorkerManifest,
    execution: &crate::scheduler::ExecutionPolicy,
) -> Result<(), WorkerError> {
    manifest.validate()?;
    if manifest.allowed_tools.is_empty() && manifest.limits.max_tool_calls != 0
        || execution.mode != manifest.mode
        || execution.tools_enabled == manifest.allowed_tools.is_empty()
        || execution.allowed_tools.as_ref() != Some(&manifest.allowed_tools)
        || serde_json::to_vec(&execution.budget)? != serde_json::to_vec(&manifest.budget)?
        || serde_json::to_vec(&execution.limits)? != serde_json::to_vec(&manifest.limits)?
    {
        return Err(WorkerError::Conflict(
            "native policy differs from approved manifest",
        ));
    }
    Ok(())
}

impl WorkerEndpoint {
    pub async fn offer_reserved(
        &self,
        binding: RemoteTaskBinding,
        gate: &dyn super::WorkerOutboundGate,
        now: UnixMillis,
    ) -> Result<super::DelegationSnapshot, WorkerError> {
        if self.role() != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        binding.validate()?;
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let job = self
            .require_job(&WorkerJobBinding {
                delegation_id: binding.delegation_id,
                attempt_id: binding.attempt_id,
                manifest_digest: binding.manifest_digest.clone(),
            })
            .await?;
        let manifest = job
            .manifest
            .ok_or(WorkerError::Conflict("reserved manifest unavailable"))?;
        self.validate_manifest_endpoints(&manifest)?;
        let peer_id = manifest.executor;
        let peer = self
            .blocking(move |repository| repository.peer(peer_id))
            .await?;
        if manifest_binding(&manifest, peer.clone())? != binding
            || job.peer != peer.device_id
            || job.cancellation_requested
            || !matches!(
                job.state,
                DelegationState::Offered | DelegationState::Accepted | DelegationState::Running
            )
        {
            return Err(WorkerError::Conflict(
                "reserved offer is no longer dispatchable",
            ));
        }
        let fresh_now = UnixMillis::now()?;
        if fresh_now < now || fresh_now >= manifest.deadline {
            return Err(WorkerError::Expired);
        }
        let permit = gate.acquire(&manifest, &peer, fresh_now).await?;
        let id = binding.delegation_id;
        let retained = self
            .blocking(move |repository| repository.original_offer(id))
            .await?;
        if let Some(retained) = retained {
            validate_outbound(self, &retained, &peer)?;
            let super::WorkerMessage::Offer { manifest: original } = &retained.packet.body else {
                return Err(WorkerError::Conflict("original offer kind"));
            };
            if serde_json::to_vec(original.as_ref())? != serde_json::to_vec(&manifest)?
                || retained.envelope.header.expires_at_ms <= UnixMillis::now()?.get()
            {
                return Err(WorkerError::Conflict("original offer changed or expired"));
            }
            drop((permit, authority));
        } else {
            if job.state != DelegationState::Offered {
                return Err(WorkerError::Conflict("accepted job has no original offer"));
            }
            self.queue_owned(
                peer.device_id,
                super::WorkerMessage::Offer {
                    manifest: Box::new(manifest),
                },
                UnixMillis::now()?,
                authority,
                Some(permit),
            )
            .await?;
        }
        self.snapshot(binding.delegation_id)
            .await?
            .ok_or(WorkerError::Conflict("reserved offer disappeared"))
    }

    pub async fn verify_native_outcome(
        &self,
        native: crate::scheduler::RemoteTaskSnapshot,
    ) -> Result<crate::agent::VerifiedRemoteOutcome, WorkerError> {
        if self.role() != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let resources = (self.clone(), native, authority, self.agent.clone());
        tokio::task::spawn_blocking(move || {
            let resources = resources;
            let report =
                verified_report(&resources.0, NativeReportMapping::Reservation(&resources.1))?;
            Ok(crate::agent::VerifiedRemoteOutcome::from_verified_worker(
                resources.3,
                resources.1.binding,
                report,
                Box::new((resources.2, resources.0)),
            )?)
        })
        .await
        .map_err(|_| WorkerError::Worker)?
    }
}

fn validate_outbound(
    endpoint: &WorkerEndpoint,
    message: &super::repository::RetainedWorkerMessage,
    peer: &DevicePublicKeys,
) -> Result<(), WorkerError> {
    if message.direction != "outbound"
        || message.peer != peer.device_id
        || message.packet.sender != endpoint.public_keys().device_id
        || message.packet.recipient != peer.device_id
        || message.packet.sender_role != endpoint.role()
        || message.envelope.header.created_at_ms != message.recorded_at.get()
    {
        return Err(WorkerError::Conflict("original outbound message binding"));
    }
    message
        .envelope
        .verify_authenticated_sender(&endpoint.public_keys(), message.recorded_at.get())?;
    Ok(())
}

enum NativeReportMapping<'a> {
    Reservation(&'a crate::scheduler::RemoteTaskSnapshot),
    Accepted(&'a crate::agent::RemoteResultReceipt),
}

fn verified_report(
    endpoint: &WorkerEndpoint,
    mapping: NativeReportMapping<'_>,
) -> Result<crate::protocol::RemoteTaskReport, WorkerError> {
    use crate::protocol::{
        RemoteResultSource, RemoteTaskOutcome, RemoteTaskReport, RemoteTaskUsage,
    };
    let (binding, task_id, command_id) = match &mapping {
        NativeReportMapping::Reservation(native) => (
            &native.binding,
            native.task.task.id,
            native.task.active_command_id,
        ),
        NativeReportMapping::Accepted(receipt) => {
            receipt.validate_store(&endpoint.agent)?;
            (
                receipt.binding(),
                receipt.inspection().report.task_id,
                receipt.inspection().report.command_id,
            )
        }
    };
    binding.validate()?;
    let peer = endpoint.repository.peer(binding.executor.device_id)?;
    let job = endpoint
        .repository
        .snapshot(binding.delegation_id)?
        .ok_or(WorkerError::Conflict("native outcome has no worker job"))?;
    let manifest = job.manifest.as_ref().ok_or(WorkerError::Conflict(
        "native outcome has no immutable manifest",
    ))?;
    endpoint.validate_manifest_endpoints(manifest)?;
    if let NativeReportMapping::Reservation(native) = &mapping {
        validate_policy(manifest, &native.execution)?;
        if native.configuration != manifest.model.configuration
            || native.task.task.instruction != manifest.instruction()?
            || native.task.task.parent_task_id.is_some()
            || native.task.active_command_id != native.task.task.originating_command_id
        {
            return Err(WorkerError::Conflict(
                "native mapping differs from immutable worker scope",
            ));
        }
    }
    if manifest_binding(manifest, peer.clone())? != *binding || job.peer != peer.device_id {
        return Err(WorkerError::Conflict(
            "native binding differs from immutable worker scope",
        ));
    }
    let retained = endpoint.repository.terminal_source(binding.delegation_id)?;
    let now = UnixMillis::now()?;
    if now < retained.recorded_at
        || retained.direction != "inbound"
        || retained.peer != peer.device_id
        || retained.packet.sender_role != WorkerRole::Executor
        || retained.packet.sender != peer.device_id
        || retained.packet.recipient != endpoint.public_keys().device_id
        || retained.packet.body.binding()? != Some(job.binding.clone())
    {
        return Err(WorkerError::Conflict(
            "original authenticated terminal source binding",
        ));
    }
    let mut guard = RetainedSourceGuard {
        expected: &retained,
        verified: false,
    };
    match endpoint.identity.open(
        &peer,
        &retained.envelope,
        retained.recorded_at.get(),
        &mut guard,
    ) {
        Err(crate::crypto::CryptoError::Replay) if guard.verified => {}
        Err(error) => return Err(error.into()),
        Ok(_) => {
            return Err(WorkerError::Conflict(
                "historical source was not a retained replay",
            ));
        }
    }
    let (source, outcome, summary, usage, artifact_count, quarantine_digest, finished_at) =
        match &retained.packet.body {
            super::WorkerMessage::Result { result } => {
                result.validate(manifest)?;
                let digest = result.fingerprint()?;
                if job.rejection.is_some()
                    || job.result_digest.as_ref() != Some(&digest)
                    || job.result.as_ref().map(serde_json::to_vec).transpose()?
                        != Some(serde_json::to_vec(result.as_ref())?)
                {
                    return Err(WorkerError::Conflict(
                        "original result differs from worker ledger",
                    ));
                }
                let quarantine =
                    super::files::quarantine_result(&endpoint.agent, manifest, result)?;
                if quarantine.result_digest != digest
                    || quarantine.files.len() != result.artifacts.len()
                {
                    return Err(WorkerError::Conflict("private result quarantine differs"));
                }
                if !job.quarantined {
                    endpoint
                        .repository
                        .mark_quarantined(&job.binding, &digest, now)?;
                }
                let outcome = match result.status {
                    super::WorkerResultStatus::Succeeded => RemoteTaskOutcome::Succeeded,
                    super::WorkerResultStatus::Failed => RemoteTaskOutcome::Failed,
                    super::WorkerResultStatus::Cancelled => RemoteTaskOutcome::Cancelled,
                    super::WorkerResultStatus::Unknown => RemoteTaskOutcome::Unknown,
                };
                let usage = RemoteTaskUsage {
                    model_attempts: result.usage.model_attempts,
                    tool_calls: result.usage.tool_calls,
                    charged_tokens: result.usage.charged_tokens,
                    reserved_output_tokens: result.usage.reserved_output_tokens,
                    reported_input_tokens: result.usage.reported_input_tokens,
                    reported_output_tokens: result.usage.reported_output_tokens,
                };
                let quarantine_digest =
                    (outcome != RemoteTaskOutcome::Unknown).then_some(digest.clone());
                (
                    RemoteResultSource::Result {
                        result_digest: digest,
                    },
                    outcome,
                    result.summary.clone(),
                    usage,
                    u16::try_from(result.artifacts.len())
                        .map_err(|_| WorkerError::Capacity("artifact count"))?,
                    quarantine_digest,
                    Some(result.finished_at),
                )
            }
            super::WorkerMessage::Rejected { reason, .. } => {
                if job.result.is_some() || job.rejection != Some(*reason) {
                    return Err(WorkerError::Conflict(
                        "original rejection differs from worker ledger",
                    ));
                }
                let reason = serde_json::to_value(reason)?
                    .as_str()
                    .ok_or(WorkerError::Conflict("rejection encoding"))?
                    .to_owned();
                (
                    RemoteResultSource::Rejected {
                        reason: reason.clone(),
                    },
                    RemoteTaskOutcome::Rejected,
                    format!("Worker rejected the approved task before native execution: {reason}."),
                    RemoteTaskUsage::default(),
                    0,
                    None,
                    None,
                )
            }
            _ => return Err(WorkerError::Conflict("original source is not terminal")),
        };
    let report = RemoteTaskReport {
        task_id,
        command_id,
        delegation_id: binding.delegation_id,
        attempt_id: binding.attempt_id,
        manifest_digest: binding.manifest_digest.clone(),
        executor: peer.device_id,
        executor_fingerprint: peer.fingerprint(),
        source_message_id: retained.envelope.header.message_id,
        envelope_digest: super::types::digest(
            b"everything.worker.envelope.v1\0",
            &serde_json::to_vec(&retained.envelope)?,
        ),
        packet_digest: super::types::digest(
            b"everything.worker.packet.v1\0",
            &retained.packet.to_bytes()?,
        ),
        source,
        outcome,
        summary,
        usage,
        artifact_count,
        quarantine_digest,
        finished_at,
        accepted_at: now,
    };
    report.validate()?;
    Ok(report)
}

struct RetainedSourceGuard<'a> {
    expected: &'a super::repository::RetainedWorkerMessage,
    verified: bool,
}

impl crate::crypto::ReplayGuard for RetainedSourceGuard<'_> {
    fn record_if_new(
        &mut self,
        record: crate::crypto::ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, crate::crypto::CryptoError> {
        let expected = self.expected;
        if record.message_id != expected.envelope.header.message_id
            || record.sender != expected.packet.sender
            || record.recipient != expected.packet.recipient
            || record.expires_at_ms != expected.envelope.header.expires_at_ms
            || now_ms != expected.recorded_at.get()
            || expected.packet.to_bytes().map_err(|_| {
                crate::crypto::CryptoError::ReplayStorage("stored worker source encoding".into())
            })? != plaintext
        {
            return Err(crate::crypto::CryptoError::ReplayStorage(
                "retained worker source changed".into(),
            ));
        }
        self.verified = true;
        Ok(false)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerResultAcknowledgement {
    NotRequired,
    Queued {
        message_id: crate::protocol::MessageId,
        duplicate: bool,
    },
}

struct NativeReceiptPermit {
    _receipt: crate::agent::RemoteResultReceipt,
}

impl super::WorkerOutboundPermit for NativeReceiptPermit {}

impl WorkerEndpoint {
    pub async fn acknowledge_native_result(
        &self,
        receipt: crate::agent::RemoteResultReceipt,
        now: UnixMillis,
    ) -> Result<WorkerResultAcknowledgement, WorkerError> {
        if self.role() != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let resources = (self.clone(), receipt, authority, self.agent.clone());
        let (existing, resources) = tokio::task::spawn_blocking(move || {
            let resources = resources;
            let fresh_now = UnixMillis::now()?;
            if fresh_now < now {
                return Err(WorkerError::Expired);
            }
            let mut report =
                verified_report(&resources.0, NativeReportMapping::Accepted(&resources.1))?;
            report.accepted_at = resources.1.inspection().report.accepted_at;
            if report != resources.1.inspection().report {
                return Err(WorkerError::Conflict(
                    "native receipt differs from original worker source",
                ));
            }
            if matches!(
                report.source,
                crate::protocol::RemoteResultSource::Rejected { .. }
            ) {
                return Ok((Some(WorkerResultAcknowledgement::NotRequired), resources));
            }
            let existing = resources
                .0
                .repository
                .original_acknowledgement(report.delegation_id)?;
            if let Some(original) = existing {
                let peer = resources.0.repository.peer(report.executor)?;
                validate_outbound(&resources.0, &original, &peer)?;
                validate_acknowledgement(&original.packet.body, &resources.1)?;
                return Ok((
                    Some(WorkerResultAcknowledgement::Queued {
                        message_id: original.envelope.header.message_id,
                        duplicate: true,
                    }),
                    resources,
                ));
            }
            Ok::<_, WorkerError>((None, resources))
        })
        .await
        .map_err(|_| WorkerError::Worker)??;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let (endpoint, receipt, authority, owner) = resources;
        let report = &receipt.inspection().report;
        let crate::protocol::RemoteResultSource::Result { result_digest } = &report.source else {
            return Err(WorkerError::Conflict(
                "native acknowledgement has no result",
            ));
        };
        let delegation = report.delegation_id;
        let body = super::WorkerMessage::ResultAcknowledged {
            binding: WorkerJobBinding {
                delegation_id: report.delegation_id,
                attempt_id: report.attempt_id,
                manifest_digest: report.manifest_digest.clone(),
            },
            result_digest: result_digest.clone(),
            receipt_id: receipt.inspection().receipt_command_id,
        };
        endpoint
            .queue_owned(
                report.executor,
                body,
                UnixMillis::now()?,
                authority,
                Some(Box::new(NativeReceiptPermit { _receipt: receipt })),
            )
            .await?;
        let retained = endpoint
            .blocking(move |repository| repository.original_acknowledgement(delegation))
            .await?
            .ok_or(WorkerError::Conflict("native acknowledgement disappeared"))?;
        drop(owner);
        Ok(WorkerResultAcknowledgement::Queued {
            message_id: retained.envelope.header.message_id,
            duplicate: false,
        })
    }
}

fn validate_acknowledgement(
    body: &super::WorkerMessage,
    receipt: &crate::agent::RemoteResultReceipt,
) -> Result<(), WorkerError> {
    let expected = &receipt.inspection().report;
    let (
        super::WorkerMessage::ResultAcknowledged {
            binding,
            result_digest,
            receipt_id,
        },
        crate::protocol::RemoteResultSource::Result {
            result_digest: expected_digest,
        },
    ) = (body, &expected.source)
    else {
        return Err(WorkerError::Conflict("native acknowledgement kind"));
    };
    if binding.delegation_id != expected.delegation_id
        || binding.attempt_id != expected.attempt_id
        || binding.manifest_digest != expected.manifest_digest
        || result_digest != expected_digest
        || *receipt_id != receipt.inspection().receipt_command_id
    {
        return Err(WorkerError::Conflict(
            "native acknowledgement changed original acceptance",
        ));
    }
    Ok(())
}
