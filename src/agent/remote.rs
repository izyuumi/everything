use rusqlite::OptionalExtension;

use super::*;
use crate::protocol::{RemoteResultSource, RemoteTaskOutcome, RemoteTaskReport, TaskStatus};
use crate::scheduler::{RemoteResultCommit, RemoteTaskBinding};

pub struct VerifiedRemoteOutcome {
    binding: RemoteTaskBinding,
    report: RemoteTaskReport,
    verified_at: UnixMillis,
    _endpoint: Box<dyn Send + Sync>,
    host: Option<Box<dyn Send + Sync>>,
    owner: Agent,
}

impl VerifiedRemoteOutcome {
    pub(crate) fn from_verified_worker(
        owner: Agent,
        binding: RemoteTaskBinding,
        report: RemoteTaskReport,
        endpoint: Box<dyn Send + Sync>,
    ) -> Result<Self, AgentError> {
        if owner.state_directory().is_none() {
            return Err(AgentError::InvalidData(
                "remote outcome requires a leased Agent",
            ));
        }
        binding
            .validate()
            .map_err(|_| AgentError::InvalidData("invalid remote outcome binding"))?;
        validate_binding(&binding, &report)?;
        let verified_at = UnixMillis::now()?;
        Ok(Self {
            binding,
            report,
            verified_at,
            _endpoint: endpoint,
            host: None,
            owner,
        })
    }

    pub fn binding(&self) -> &RemoteTaskBinding {
        &self.binding
    }

    pub fn report(&self) -> &RemoteTaskReport {
        &self.report
    }

    pub(crate) fn retain_host_authority(
        &mut self,
        host: Box<dyn Send + Sync>,
    ) -> Result<(), AgentError> {
        if self.host.is_some() {
            return Err(AgentError::InvalidData(
                "remote outcome already retains host authority",
            ));
        }
        self.host = Some(host);
        Ok(())
    }

    pub(crate) fn validate_owner(&self, owner: &Agent) -> Result<(), AgentError> {
        if owner.state_directory().is_none() || !owner.store.shares_connection(&self.owner.store) {
            return Err(AgentError::InvalidData(
                "remote outcome belongs to another leased Agent store",
            ));
        }
        validate_binding(&self.binding, &self.report)
    }

    pub(crate) fn validate_fresh_owner(&self, owner: &Agent) -> Result<(), AgentError> {
        self.validate_owner(owner)?;
        if self.host.is_none() {
            return Err(AgentError::InvalidData(
                "remote outcome does not retain host authority",
            ));
        }
        Ok(())
    }

    pub(crate) fn verified_at(&self) -> UnixMillis {
        self.verified_at
    }

    pub(crate) fn same_source(&self, accepted: &RemoteTaskReport) -> bool {
        let mut report = self.report.clone();
        report.accepted_at = accepted.accepted_at;
        report == *accepted
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteResultInspection {
    pub report: RemoteTaskReport,
    pub receipt_command_id: CommandId,
    pub event_id: EventId,
    pub sequence: EventSequence,
    pub task_status: TaskStatus,
    pub cancellation_requested: bool,
}

impl RemoteResultInspection {
    pub(crate) fn validate(&self, conversation: ConversationId) -> Result<(), AgentError> {
        self.report.validate()?;
        let (command, event) = receipt_identity(conversation, &self.report)?;
        if self.receipt_command_id != command
            || self.event_id != event
            || self.task_status != result_status(&self.report)
        {
            return Err(AgentError::InvalidData(
                "remote receipt identity or status changed",
            ));
        }
        Ok(())
    }
}

pub struct RemoteResultReceipt {
    inspection: RemoteResultInspection,
    binding: RemoteTaskBinding,
    conversation: ConversationId,
    owner: Agent,
}

impl RemoteResultReceipt {
    pub fn inspection(&self) -> &RemoteResultInspection {
        &self.inspection
    }

    pub fn binding(&self) -> &RemoteTaskBinding {
        &self.binding
    }

    pub fn conversation_id(&self) -> ConversationId {
        self.conversation
    }

    pub(crate) fn validate_store(&self, agent: &Agent) -> Result<(), AgentError> {
        if !agent.store.shares_connection(&self.owner.store) || agent.state_directory().is_none() {
            return Err(AgentError::InvalidData(
                "remote receipt belongs to another leased Agent store",
            ));
        }
        validate_receipt(
            &agent.store,
            self.conversation,
            &self.binding,
            &self.inspection,
        )
    }
}

impl Agent {
    pub(crate) async fn commit_remote_result(
        &self,
        completion: RemoteResultCommit,
    ) -> Result<RemoteResultReceipt, AgentError> {
        completion.validate_owner(self)?;
        let conversation_id = completion.conversation_id();
        let admission = self.admit(conversation_id)?;
        let write = self.write_gate(conversation_id)?.lock_owned().await;
        let resources = (completion, admission, write, self.clone());
        blocking(move || {
            let (completion, _admission, _write, owner) = &resources;
            completion.validate_owner(owner)?;
            let mut conversation = load_conversation(&owner.store, conversation_id)?;
            let pending = conversation
                .checkpoint
                .pending
                .as_ref()
                .ok_or(AgentError::NoPendingTurn)?;
            if pending.command.command_id != completion.command_id()
                || pending.command.text != completion.instruction()
                || conversation.config != *completion.configuration()
                || conversation.checkpoint.native.is_some()
                || conversation.checkpoint.continuation.is_some()
                || !conversation.checkpoint.inputs.is_empty()
                || conversation.checkpoint.last_turn.is_some()
            {
                return Err(AgentError::InvalidData(
                    "remote result no longer owns the original unstarted turn",
                ));
            }
            let accepted_at = UnixMillis::now()?;
            if accepted_at < completion.not_before() {
                return Err(AgentError::InvalidData(
                    "remote result admission clock moved backwards",
                ));
            }
            let mut report = completion.report().clone();
            report.accepted_at = accepted_at;
            let (receipt_command_id, event_id) = receipt_identity(conversation_id, &report)?;
            let sequence =
                EventSequence::try_from(next_sequence(conversation.info.last_sequence)?)?;
            let inspection = RemoteResultInspection {
                task_status: result_status(&report),
                report,
                receipt_command_id,
                event_id,
                sequence,
                cancellation_requested: completion.cancellation_requested(),
            };
            inspection.validate(conversation_id)?;
            let event = EventEnvelope {
                schema_version: crate::protocol::SchemaVersion::CURRENT,
                event_id,
                conversation_id,
                sequence,
                occurred_at: accepted_at,
                causation_id: Some(receipt_command_id),
                payload: ConversationEvent::RemoteTaskFinished {
                    report: Box::new(inspection.report.clone()),
                },
            };
            encoded_len(&event, MAX_PUBLIC_EVENT_BYTES)?;
            conversation.checkpoint.pending = None;
            let update = completion
                .task_update(&inspection)
                .map_err(|_| AgentError::InvalidData("remote task result checkpoint is invalid"))?;
            owner.store.append_with_external_task(
                &AppendRequest {
                    conversation_id: conversation_id.to_string(),
                    command_id: receipt_command_id.to_string(),
                    expected_sequence: conversation.info.last_sequence,
                    events: vec![EventInput {
                        id: event_id.to_string(),
                        kind: "remote_task_finished".into(),
                        payload: serde_json::to_value(event).map_err(|_| AgentError::Worker)?,
                        occurred_at: timestamp(accepted_at)?,
                    }],
                },
                &ConversationStateUpdate {
                    expected_revision: conversation.info.checkpoint_revision,
                    payload: checkpoint_payload(&conversation.checkpoint)?,
                },
                &update,
            )?;
            validate_receipt(
                &owner.store,
                conversation_id,
                completion.binding(),
                &inspection,
            )?;
            Ok(RemoteResultReceipt {
                inspection,
                binding: completion.binding().clone(),
                conversation: conversation_id,
                owner: owner.clone(),
            })
        })
        .await
    }

    pub(crate) async fn restore_remote_result_receipt(
        &self,
        conversation: ConversationId,
        binding: RemoteTaskBinding,
        inspection: RemoteResultInspection,
    ) -> Result<RemoteResultReceipt, AgentError> {
        if self.state_directory().is_none() {
            return Err(AgentError::InvalidData(
                "remote receipt requires a leased Agent",
            ));
        }
        let owner = self.clone();
        blocking(move || {
            validate_receipt(&owner.store, conversation, &binding, &inspection)?;
            Ok(RemoteResultReceipt {
                inspection,
                binding,
                conversation,
                owner,
            })
        })
        .await
    }
}

fn validate_binding(
    binding: &RemoteTaskBinding,
    report: &RemoteTaskReport,
) -> Result<(), AgentError> {
    report.validate()?;
    if report.delegation_id != binding.delegation_id
        || report.attempt_id != binding.attempt_id
        || report.manifest_digest != binding.manifest_digest
        || report.executor != binding.executor.device_id
        || report.executor_fingerprint != binding.executor.fingerprint()
        || report.finished_at.is_some_and(|at| at < binding.created_at)
    {
        return Err(AgentError::InvalidData(
            "remote report differs from original worker binding",
        ));
    }
    Ok(())
}

fn receipt_identity(
    conversation: ConversationId,
    report: &RemoteTaskReport,
) -> Result<(CommandId, EventId), AgentError> {
    super::control_events::identities(&(
        "remote_task_finished",
        conversation,
        report.task_id,
        report.command_id,
        report.delegation_id,
        report.attempt_id,
        &report.manifest_digest,
        report.source_message_id,
        &report.envelope_digest,
        &report.packet_digest,
        &report.source,
    ))
}

fn result_status(report: &RemoteTaskReport) -> TaskStatus {
    match report.outcome {
        RemoteTaskOutcome::Succeeded => TaskStatus::Completed,
        RemoteTaskOutcome::Failed => TaskStatus::Failed,
        RemoteTaskOutcome::Cancelled => TaskStatus::Cancelled,
        RemoteTaskOutcome::Unknown => TaskStatus::WaitingForChildren,
        RemoteTaskOutcome::Rejected => match &report.source {
            RemoteResultSource::Rejected { reason } if reason == "cancelled_before_acceptance" => {
                TaskStatus::Cancelled
            }
            _ => TaskStatus::Failed,
        },
    }
}

fn validate_receipt(
    store: &Store,
    conversation: ConversationId,
    binding: &RemoteTaskBinding,
    inspection: &RemoteResultInspection,
) -> Result<(), AgentError> {
    validate_binding(binding, &inspection.report)?;
    inspection.validate(conversation)?;
    RemoteResultCommit::validate_record(store, conversation, binding, inspection).map_err(
        |_| AgentError::InvalidData("remote receipt differs from actual owned task checkpoint"),
    )?;
    let request = store
        .command_request(&inspection.receipt_command_id.to_string())?
        .ok_or(AgentError::InvalidData(
            "remote result has no command receipt",
        ))?;
    if request.conversation_id != conversation.to_string()
        || request.command_id != inspection.receipt_command_id.to_string()
        || request.events.len() != 1
        || request.expected_sequence.checked_add(1) != Some(inspection.sequence.get())
    {
        return Err(AgentError::InvalidData(
            "remote result command receipt changed",
        ));
    }
    let row = request.events.first().ok_or(AgentError::InvalidData(
        "remote result command has no event",
    ))?;
    let event: EventEnvelope = serde_json::from_value(row.payload.clone())
        .map_err(|_| AgentError::InvalidData("remote result envelope is invalid"))?;
    let expected = EventEnvelope {
        schema_version: crate::protocol::SchemaVersion::CURRENT,
        event_id: inspection.event_id,
        conversation_id: conversation,
        sequence: inspection.sequence,
        occurred_at: inspection.report.accepted_at,
        causation_id: Some(inspection.receipt_command_id),
        payload: ConversationEvent::RemoteTaskFinished {
            report: Box::new(inspection.report.clone()),
        },
    };
    if event != expected
        || row.id != inspection.event_id.to_string()
        || row.kind != "remote_task_finished"
        || row.occurred_at != timestamp(inspection.report.accepted_at)?
    {
        return Err(AgentError::InvalidData(
            "remote result attributed report changed",
        ));
    }
    let saved = store.events(&conversation.to_string(), inspection.sequence.get() - 1, 1)?;
    let saved = saved
        .first()
        .ok_or(AgentError::InvalidData("remote result event disappeared"))?;
    if decode_event(saved, conversation)? != expected
        || saved.kind != row.kind
        || saved.id != row.id
        || saved.occurred_at != row.occurred_at
    {
        return Err(AgentError::InvalidData(
            "remote result journal differs from receipt",
        ));
    }
    let digest: Option<String> = store
        .connection()?
        .query_row(
            "SELECT state_digest FROM commands WHERE id=?1",
            [inspection.receipt_command_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(StorageError::from)?
        .flatten();
    if digest.as_ref().is_none_or(|digest| {
        digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(AgentError::InvalidData(
            "remote result has no atomic state receipt",
        ));
    }
    Ok(())
}
