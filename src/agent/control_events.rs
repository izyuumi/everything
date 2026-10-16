use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use super::*;
use crate::permissions::PermissionDecisionPublication;
use crate::protocol::{OperationId, SchemaVersion, TaskId, TaskRecord, TaskStatus};
use crate::storage::TaskState;

const MAX_PAGE_SIZE: usize = 64;
pub(super) const MAX_CONCURRENT_EVENTS: u64 = 256;

#[derive(Clone, Debug, Serialize)]
pub struct TaskPublicationReceipt {
    pub task_id: TaskId,
    pub created_sequence: EventSequence,
    pub state_sequence: EventSequence,
    pub appended_events: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct PermissionPublicationReceipt {
    pub operation_id: OperationId,
    pub sequence: EventSequence,
    pub duplicate: bool,
}

impl Agent {
    pub async fn task_publication_ids(
        &self,
        after: Option<TaskId>,
        limit: usize,
    ) -> Result<Vec<TaskId>, AgentError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(AgentError::InvalidData("publication page limit is 1 to 64"));
        }
        let owner = self.clone();
        blocking(move || {
            let retained_agent = owner;
            let connection = retained_agent.store.connection()?;
            let mut statement = connection
                .prepare("SELECT CASE WHEN typeof(id)='text' AND length(CAST(id AS BLOB))=36 THEN id END FROM tasks WHERE id>coalesce(?1,'') ORDER BY id LIMIT ?2")
                .map_err(StorageError::from)?;
            let rows = statement
                .query_map(rusqlite::params![after.map(|id| id.to_string()), limit as i64], |row| row.get::<_, Option<String>>(0))
                .map_err(StorageError::from)?;
            rows.map(|row| {
                row.map_err(StorageError::from)?
                    .ok_or(AgentError::InvalidData("invalid task publication identity"))?
                    .parse()
                    .map_err(AgentError::from)
            }).collect()
        }).await
    }

    pub async fn publish_task_state(
        &self,
        task_id: TaskId,
    ) -> Result<TaskPublicationReceipt, AgentError> {
        let owner = self.clone();
        let conversation_id = blocking(move || {
            let retained_agent = owner;
            task_conversation(&retained_agent.store, task_id)
        })
        .await?;
        let write = self.write_gate(conversation_id)?.lock_owned().await;
        let owner = self.clone();
        blocking(move || {
            let retained_agent = owner;
            let _write = write;
            let (initial, current) = task_records(&retained_agent.store, task_id)?;
            if initial.conversation_id != conversation_id {
                return Err(AgentError::InvalidData(
                    "task publication conversation changed",
                ));
            }
            let creation = ConversationEvent::TaskCreated {
                task: initial.clone(),
            };
            let (command_id, event_id) = creation_identity(conversation_id, task_id)?;
            let state_payload = ConversationEvent::TaskStatusChanged {
                task_id,
                status: current.status,
                failure: current.failure.clone(),
            };
            if let Some(created) = existing_publication(
                &retained_agent.store,
                conversation_id,
                command_id,
                event_id,
                initial.created_at,
                &creation,
            )? {
                let previous =
                    latest_task_event(&retained_agent.store, conversation_id, task_id, None)?
                        .ok_or(AgentError::InvalidData(
                            "task publication lost its creation",
                        ))?;
                validate_task_creation(&initial, &previous)?;
                if task_state_matches(&previous.payload, &state_payload) {
                    return Ok(TaskPublicationReceipt {
                        task_id,
                        created_sequence: created.sequence,
                        state_sequence: previous.sequence,
                        appended_events: 0,
                    });
                }
            }
            let mut conversation = load_conversation(&retained_agent.store, conversation_id)?;
            let (created, duplicate) = publish(
                &retained_agent.store,
                &mut conversation,
                command_id,
                event_id,
                initial.created_at,
                creation,
            )?;
            let mut appended_events = u8::from(!duplicate);
            let previous =
                latest_task_event(&retained_agent.store, conversation_id, task_id, None)?.ok_or(
                    AgentError::InvalidData("task publication lost its creation"),
                )?;
            validate_task_creation(&initial, &previous)?;
            let payload = ConversationEvent::TaskStatusChanged {
                task_id,
                status: current.status,
                failure: current.failure,
            };
            let state_sequence = if task_state_matches(&previous.payload, &payload) {
                previous.sequence
            } else {
                let (command_id, event_id) = status_identity(conversation_id, &previous, &payload)?;
                let (event, duplicate) = publish(
                    &retained_agent.store,
                    &mut conversation,
                    command_id,
                    event_id,
                    current.updated_at,
                    payload,
                )?;
                appended_events += u8::from(!duplicate);
                event.sequence
            };
            Ok(TaskPublicationReceipt {
                task_id,
                created_sequence: created.sequence,
                state_sequence,
                appended_events,
            })
        })
        .await
    }

    pub async fn publish_permission_decision(
        &self,
        publication: PermissionDecisionPublication,
    ) -> Result<PermissionPublicationReceipt, AgentError> {
        let conversation_id = publication.conversation_id();
        let write = self.write_gate(conversation_id)?.lock_owned().await;
        let owner = self.clone();
        blocking(move || {
            let retained_agent = owner;
            let _write = write;
            publication.revalidate(&retained_agent.store).map_err(|_| {
                AgentError::InvalidData(
                    "permission publication differs from this host's stored resolution",
                )
            })?;
            let payload = ConversationEvent::PermissionDecided {
                operation_id: publication.operation_id(),
                decision: publication.decision(),
            };
            let expected_resolution = crate::permissions::publication::decision_publication_id(
                conversation_id,
                publication.operation_id(),
            )
            .map_err(|_| AgentError::InvalidData("permission publication identity is invalid"))?;
            if publication.resolution_id() != expected_resolution {
                return Err(AgentError::InvalidData(
                    "permission resolution identity changed",
                ));
            }
            let (command_id, event_id) = decision_identity(
                conversation_id,
                publication.operation_id(),
                publication.source_digest(),
            )?;
            if let Some(event) = existing_publication(
                &retained_agent.store,
                conversation_id,
                command_id,
                event_id,
                publication.resolved_at(),
                &payload,
            )? {
                return Ok(PermissionPublicationReceipt {
                    operation_id: publication.operation_id(),
                    sequence: event.sequence,
                    duplicate: true,
                });
            }
            let mut conversation = load_conversation(&retained_agent.store, conversation_id)?;
            let (event, duplicate) = publish(
                &retained_agent.store,
                &mut conversation,
                command_id,
                event_id,
                publication.resolved_at(),
                payload,
            )?;
            Ok(PermissionPublicationReceipt {
                operation_id: publication.operation_id(),
                sequence: event.sequence,
                duplicate,
            })
        })
        .await
    }
}

fn task_conversation(store: &Store, task_id: TaskId) -> Result<ConversationId, AgentError> {
    let value: Option<Option<String>> = store.connection()?.query_row(
        "SELECT CASE WHEN typeof(conversation_id)='text' AND length(CAST(conversation_id AS BLOB))=36 THEN conversation_id END FROM tasks WHERE id=?1",
        [task_id.to_string()], |row| row.get(0),
    ).optional().map_err(StorageError::from)?;
    value
        .flatten()
        .ok_or(AgentError::InvalidData(
            "task publication source is missing or invalid",
        ))?
        .parse()
        .map_err(AgentError::from)
}

fn task_records(store: &Store, task_id: TaskId) -> Result<(TaskRecord, TaskRecord), AgentError> {
    {
        let connection = store.connection()?;
        let bounds: Option<(i64, i64)> = connection.query_row(
            "SELECT length(CAST(input AS BLOB)),length(CAST(checkpoint AS BLOB)) FROM tasks WHERE id=?1",
            [task_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(StorageError::from)?;
        let Some((input, checkpoint)) = bounds else {
            return Err(AgentError::InvalidData(
                "task publication source is missing",
            ));
        };
        if input > 1_048_576 || checkpoint > 1_048_576 || input < 0 || checkpoint < 0 {
            return Err(AgentError::ResourceLimit("task publication source"));
        }
    }
    let row = store
        .task(&task_id.to_string())?
        .ok_or(AgentError::InvalidData(
            "task publication source is missing",
        ))?;
    if row.input.get("scheduler_version").is_some() {
        return crate::scheduler::task_publication_records(store, task_id)
            .map_err(|_| AgentError::InvalidData("scheduler publication source is invalid"));
    }
    let initial: TaskRecord = serde_json::from_value(row.input.clone())
        .map_err(|_| AgentError::InvalidData("foreground publication source is invalid"))?;
    let current: TaskRecord = serde_json::from_value(row.checkpoint.clone())
        .map_err(|_| AgentError::InvalidData("foreground publication checkpoint is invalid"))?;
    if initial.id != task_id
        || initial.id != current.id
        || initial.conversation_id != current.conversation_id
        || initial.conversation_id.to_string() != row.conversation_id
        || initial.parent_task_id != current.parent_task_id
        || initial.parent_task_id.map(|id| id.to_string()) != row.parent_task_id
        || initial.originating_command_id != current.originating_command_id
        || initial.instruction != current.instruction
        || initial.created_at != current.created_at
        || initial.created_at != initial.updated_at
        || initial.status != TaskStatus::Queued
        || initial.failure.is_some()
        || initial.schema_version != SchemaVersion::CURRENT
        || current.schema_version != SchemaVersion::CURRENT
        || timestamp(initial.created_at)? != row.created_at
        || timestamp(current.updated_at)? != row.updated_at
        || current.updated_at < current.created_at
        || (current.status == TaskStatus::Failed) != current.failure.is_some()
        || !matches!(
            (row.state, current.status),
            (TaskState::Queued, TaskStatus::Queued)
                | (TaskState::Running, TaskStatus::Running)
                | (
                    TaskState::Waiting,
                    TaskStatus::Queued
                        | TaskStatus::AwaitingPermission
                        | TaskStatus::WaitingForChildren
                )
                | (TaskState::Completed, TaskStatus::Completed)
                | (TaskState::Failed, TaskStatus::Failed)
                | (TaskState::Cancelled, TaskStatus::Cancelled)
        )
    {
        return Err(AgentError::InvalidData(
            "foreground task publication identity changed",
        ));
    }
    Ok((initial, current))
}

fn existing_publication(
    store: &Store,
    conversation_id: ConversationId,
    command_id: CommandId,
    event_id: EventId,
    occurred_at: UnixMillis,
    payload: &ConversationEvent,
) -> Result<Option<EventEnvelope>, AgentError> {
    let Some(original) = store.command_request(&command_id.to_string())? else {
        return Ok(None);
    };
    let event = publication_journal(store, &original, conversation_id)?;
    if event.event_id != event_id || event.occurred_at != occurred_at || event.payload != *payload {
        return Err(AgentError::CommandConflict(command_id));
    }
    Ok(Some(event))
}

fn publish(
    store: &Store,
    conversation: &mut Conversation,
    command_id: CommandId,
    event_id: EventId,
    occurred_at: UnixMillis,
    payload: ConversationEvent,
) -> Result<(EventEnvelope, bool), AgentError> {
    if let Some(event) = existing_publication(
        store,
        conversation.info.id,
        command_id,
        event_id,
        occurred_at,
        &payload,
    )? {
        return Ok((event, true));
    }
    let sequence = EventSequence::try_from(next_sequence(conversation.info.last_sequence)?)?;
    let event = EventEnvelope {
        schema_version: SchemaVersion::CURRENT,
        event_id,
        conversation_id: conversation.info.id,
        sequence,
        occurred_at,
        causation_id: Some(command_id),
        payload,
    };
    encoded_len(&event, MAX_PUBLIC_EVENT_BYTES)?;
    store.append_with_state(
        &AppendRequest {
            conversation_id: conversation.info.id.to_string(),
            command_id: command_id.to_string(),
            expected_sequence: conversation.info.last_sequence,
            events: vec![EventInput {
                id: event.event_id.to_string(),
                kind: event_kind(&event.payload).into(),
                payload: serde_json::to_value(&event).map_err(|_| AgentError::Worker)?,
                occurred_at: timestamp(event.occurred_at)?,
            }],
        },
        &ConversationStateUpdate {
            expected_revision: conversation.info.checkpoint_revision,
            payload: checkpoint_payload(&conversation.checkpoint)?,
        },
    )?;
    conversation.info.last_sequence = sequence.get();
    conversation.info.checkpoint_revision = conversation
        .info
        .checkpoint_revision
        .checked_add(1)
        .ok_or(AgentError::ResourceLimit("checkpoint revision"))?;
    Ok((event, false))
}

fn latest_task_event(
    store: &Store,
    conversation_id: ConversationId,
    task_id: TaskId,
    before: Option<u64>,
) -> Result<Option<EventEnvelope>, AgentError> {
    let encoded: Option<(String, u64, String, Option<String>, i64)> = store.connection()?.query_row(
        "SELECT id,sequence,kind,CASE WHEN length(CAST(payload AS BLOB))<=262144 THEN payload END,occurred_at FROM events WHERE conversation_id=?1 AND (?2 IS NULL OR sequence<?2) AND ((kind='task_created' AND json_extract(payload,'$.payload.task.id')=?3) OR (kind='task_status_changed' AND json_extract(payload,'$.payload.task_id')=?3)) ORDER BY sequence DESC LIMIT 1",
        rusqlite::params![conversation_id.to_string(), before, task_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).optional().map_err(StorageError::from)?;
    encoded
        .map(|(id, sequence, kind, value, occurred_at)| {
            let value = value.ok_or(AgentError::ResourceLimit("task publication envelope"))?;
            let row = EventRow {
                id,
                conversation_id: conversation_id.to_string(),
                sequence,
                kind,
                occurred_at,
                payload: serde_json::from_str(&value)
                    .map_err(|_| AgentError::InvalidData("task publication envelope is invalid"))?,
            };
            let event = decode_event(&row, conversation_id)?;
            let command = event.causation_id.ok_or(AgentError::InvalidData(
                "task publication has no journal identity",
            ))?;
            let request =
                store
                    .command_request(&command.to_string())?
                    .ok_or(AgentError::InvalidData(
                        "task publication has no command journal",
                    ))?;
            if publication_journal(store, &request, conversation_id)? != event {
                return Err(AgentError::InvalidData(
                    "task publication journal differs from event",
                ));
            }
            Ok(event)
        })
        .transpose()
}

fn validate_task_creation(initial: &TaskRecord, event: &EventEnvelope) -> Result<(), AgentError> {
    if let ConversationEvent::TaskCreated { task } = &event.payload
        && task != initial
    {
        return Err(AgentError::InvalidData("original published task changed"));
    }
    Ok(())
}

fn task_state_matches(previous: &ConversationEvent, proposed: &ConversationEvent) -> bool {
    match (previous, proposed) {
        (
            ConversationEvent::TaskCreated { task },
            ConversationEvent::TaskStatusChanged {
                task_id,
                status,
                failure,
            },
        ) => task.id == *task_id && task.status == *status && task.failure == *failure,
        _ => previous == proposed,
    }
}

fn creation_identity(
    conversation: ConversationId,
    task: TaskId,
) -> Result<(CommandId, EventId), AgentError> {
    identities(&("task_created", conversation, task))
}

fn status_identity(
    conversation: ConversationId,
    previous: &EventEnvelope,
    payload: &ConversationEvent,
) -> Result<(CommandId, EventId), AgentError> {
    identities(&(
        "task_status_changed",
        conversation,
        previous.event_id,
        payload,
    ))
}

fn decision_identity(
    conversation: ConversationId,
    operation: OperationId,
    source_digest: &str,
) -> Result<(CommandId, EventId), AgentError> {
    let (command, _) = identities(&("permission_decided", conversation, operation))?;
    let mut hash = Sha256::new();
    hash.update(b"everything.control-event.permission-envelope.v1\0");
    hash.update(command.as_uuid().as_bytes());
    hash.update(source_digest.as_bytes());
    Ok((
        command,
        EventId::try_from(derived_uuid(hash.finalize().into()))?,
    ))
}

fn identities(value: &impl Serialize) -> Result<(CommandId, EventId), AgentError> {
    let mut hash = Sha256::new();
    hash.update(b"everything.control-event.command.v1\0");
    hash.update(serde_json::to_vec(value).map_err(|_| AgentError::Worker)?);
    let command = derived_uuid(hash.finalize().into());
    let mut hash = Sha256::new();
    hash.update(b"everything.control-event.envelope.v1\0");
    hash.update(command.as_bytes());
    Ok((
        CommandId::try_from(command)?,
        EventId::try_from(derived_uuid(hash.finalize().into()))?,
    ))
}

fn derived_uuid(bytes: [u8; 32]) -> uuid::Uuid {
    let mut value = [0; 16];
    value.copy_from_slice(&bytes[..16]);
    value[6] = (value[6] & 0x0f) | 0x80;
    value[8] = (value[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(value)
}

fn publication_journal(
    store: &Store,
    request: &AppendRequest,
    conversation: ConversationId,
) -> Result<EventEnvelope, AgentError> {
    let stored = request
        .events
        .first()
        .filter(|_| request.events.len() == 1)
        .ok_or(AgentError::InvalidData(
            "publication journal is not a single event",
        ))?;
    let event: EventEnvelope = serde_json::from_value(stored.payload.clone())
        .map_err(|_| AgentError::InvalidData("publication journal envelope is invalid"))?;
    if request.conversation_id != conversation.to_string()
        || event.conversation_id != conversation
        || event.schema_version != SchemaVersion::CURRENT
        || event.causation_id.map(|id| id.to_string()).as_deref()
            != Some(request.command_id.as_str())
        || event.sequence.get() != next_sequence(request.expected_sequence)?
        || stored.id != event.event_id.to_string()
        || stored.kind != event_kind(&event.payload)
        || stored.occurred_at != timestamp(event.occurred_at)?
        || serde_json::to_value(&event).map_err(|_| AgentError::Worker)? != stored.payload
        || journal_state_digest(store, &request.command_id)?.is_none()
    {
        return Err(AgentError::InvalidData(
            "publication journal identity changed",
        ));
    }
    Ok(event)
}

fn journal_state_digest(store: &Store, command_id: &str) -> Result<Option<String>, AgentError> {
    let digest: Option<String> = store.connection()?.query_row(
        "SELECT CASE WHEN typeof(state_digest)='text' AND length(state_digest)=64 THEN state_digest END FROM commands WHERE id=?1",
        [command_id], |row| row.get(0),
    ).map_err(StorageError::from)?;
    Ok(digest)
}

pub(super) fn validate_metadata_event(
    store: &Store,
    event: &EventEnvelope,
) -> Result<(), AgentError> {
    let expected = match &event.payload {
        ConversationEvent::TaskCreated { task } => {
            creation_identity(event.conversation_id, task.id)?
        }
        ConversationEvent::TaskStatusChanged { task_id, .. } => {
            let previous = latest_task_event(
                store,
                event.conversation_id,
                *task_id,
                Some(event.sequence.get()),
            )?
            .ok_or(AgentError::InvalidData(
                "status publication has no previous task event",
            ))?;
            status_identity(event.conversation_id, &previous, &event.payload)?
        }
        ConversationEvent::PermissionDecided {
            operation_id,
            decision,
        } => {
            let source = crate::permissions::publication::stored_decision_publication(
                store,
                event.conversation_id,
                *operation_id,
            )
            .map_err(|_| {
                AgentError::InvalidData(
                    "control metadata has no matching original permission resolution",
                )
            })?;
            if source.decision() != *decision || source.resolved_at() != event.occurred_at {
                return Err(AgentError::InvalidData(
                    "permission metadata differs from its original resolution",
                ));
            }
            decision_identity(event.conversation_id, *operation_id, source.source_digest())?
        }
        _ => {
            return Err(AgentError::InvalidData(
                "concurrent event is not trusted control metadata",
            ));
        }
    };
    if event.causation_id != Some(expected.0) || event.event_id != expected.1 {
        return Err(AgentError::InvalidData(
            "control metadata identity is invalid",
        ));
    }
    Ok(())
}

pub(super) fn verify_append_state(
    store: &Store,
    event: &EventEnvelope,
    expected_revision: u64,
    checkpoint: &Checkpoint,
) -> Result<(), AgentError> {
    let command = event
        .causation_id
        .ok_or(AgentError::InvalidData("concurrent event has no command"))?;
    let request = store
        .command_request(&command.to_string())?
        .ok_or(AgentError::InvalidData(
            "concurrent event has no command journal",
        ))?;
    let recorded = publication_journal(store, &request, event.conversation_id)?;
    if recorded != *event {
        return Err(AgentError::InvalidData(
            "concurrent event differs from command journal",
        ));
    }
    let update = ConversationStateUpdate {
        expected_revision,
        payload: checkpoint_payload(checkpoint)?,
    };
    let mut hash = Sha256::new();
    hash.update(b"everything.conversation.state-update.v1\0");
    hash.update(serde_json::to_vec(&update).map_err(|_| AgentError::Worker)?);
    let expected = format!("{:x}", hash.finalize());
    if journal_state_digest(store, &command.to_string())?.as_deref() != Some(expected.as_str()) {
        return Err(AgentError::InvalidData(
            "concurrent append changed the private checkpoint",
        ));
    }
    Ok(())
}
