use serde::{Deserialize, Serialize};

use super::{Agent, AgentError, blocking, checkpoint_payload, load_conversation, next_sequence};
use crate::protocol::{
    CommandId, ConversationEvent, ConversationId, EventEnvelope, EventId, EventSequence,
    ScheduledRunNotification, SchemaVersion, UnixMillis,
};
use crate::storage::{AppendRequest, ConversationStateUpdate, EventInput};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledResultCommand {
    pub conversation_id: ConversationId,
    pub command_id: CommandId,
    pub event_id: EventId,
    pub accepted_at: UnixMillis,
    pub notification: ScheduledRunNotification,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScheduledResultReceipt {
    pub sequence: EventSequence,
}

impl Agent {
    pub async fn publish_scheduled_result(
        &self,
        command: ScheduledResultCommand,
    ) -> Result<ScheduledResultReceipt, AgentError> {
        command.notification.validate()?;
        let write = self.write_gate(command.conversation_id)?.lock_owned().await;
        let owner = self.clone();
        blocking(move || {
            let _write = write;
            let payload = ConversationEvent::ScheduledRunFinished {
                notification: command.notification,
            };
            if let Some(original) = owner
                .store
                .command_request(&command.command_id.to_string())?
            {
                let stored = original
                    .events
                    .first()
                    .filter(|_| original.events.len() == 1)
                    .ok_or(AgentError::CommandConflict(command.command_id))?;
                let event: EventEnvelope = serde_json::from_value(stored.payload.clone())
                    .map_err(|_| AgentError::CommandConflict(command.command_id))?;
                if original.conversation_id != command.conversation_id.to_string()
                    || original.expected_sequence.checked_add(1) != Some(event.sequence.get())
                    || event.schema_version != SchemaVersion::CURRENT
                    || event.conversation_id != command.conversation_id
                    || event.event_id != command.event_id
                    || event.causation_id != Some(command.command_id)
                    || event.occurred_at != command.accepted_at
                    || event.payload != payload
                    || stored.id != command.event_id.to_string()
                    || stored.kind != "scheduled_run_finished"
                    || u64::try_from(stored.occurred_at).ok() != Some(command.accepted_at.get())
                {
                    return Err(AgentError::CommandConflict(command.command_id));
                }
                return Ok(ScheduledResultReceipt {
                    sequence: event.sequence,
                });
            }
            let conversation = load_conversation(&owner.store, command.conversation_id)?;
            if let Some(pending) = conversation.info.pending_command_id {
                return Err(AgentError::PendingTurn(pending));
            }
            let sequence =
                EventSequence::try_from(next_sequence(conversation.info.last_sequence)?)?;
            let event = EventEnvelope {
                schema_version: SchemaVersion::CURRENT,
                event_id: command.event_id,
                conversation_id: command.conversation_id,
                sequence,
                occurred_at: command.accepted_at,
                causation_id: Some(command.command_id),
                payload,
            };
            owner.store.append_with_state(
                &AppendRequest {
                    conversation_id: command.conversation_id.to_string(),
                    command_id: command.command_id.to_string(),
                    expected_sequence: conversation.info.last_sequence,
                    events: vec![EventInput {
                        id: command.event_id.to_string(),
                        kind: "scheduled_run_finished".into(),
                        payload: serde_json::to_value(event).map_err(|_| {
                            AgentError::InvalidData("scheduled notification serialization failed")
                        })?,
                        occurred_at: i64::try_from(command.accepted_at.get()).map_err(|_| {
                            AgentError::InvalidData("scheduled notification timestamp overflow")
                        })?,
                    }],
                },
                &ConversationStateUpdate {
                    expected_revision: conversation.info.checkpoint_revision,
                    payload: checkpoint_payload(&conversation.checkpoint)?,
                },
            )?;
            Ok(ScheduledResultReceipt { sequence })
        })
        .await
    }
}
