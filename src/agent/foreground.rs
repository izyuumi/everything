use super::*;
use crate::protocol::{TaskId, TaskRecord, TaskStatus};
use crate::storage::{TaskRow, TaskState};

impl Agent {
    pub async fn prepare_foreground_task(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
    ) -> Result<TaskRecord, AgentError> {
        let admission = self.admit(conversation_id)?;
        let owner = self.clone();
        blocking(move || {
            let _admission = admission;
            let conversation = load_conversation(&owner.store, conversation_id)?;
            let pending = conversation
                .checkpoint
                .pending
                .as_ref()
                .ok_or(AgentError::NoPendingTurn)?;
            if pending.command.command_id != command_id {
                return Err(AgentError::CommandConflict(command_id));
            }
            let accepted = conversation
                .messages
                .iter()
                .find(|message| message.message.id == pending.message_id)
                .ok_or(AgentError::InvalidData("accepted user message is missing"))?;
            let mut initial = TaskRecord::new(
                conversation_id,
                None,
                command_id,
                pending.command.text.clone(),
                accepted.message.created_at,
            );
            initial.id = TaskId::try_from(*command_id.as_uuid())?;
            let input = serde_json::to_value(&initial)
                .map_err(|_| AgentError::InvalidData("task serialization failed"))?;
            let row = match owner.store.task(&initial.id.to_string())? {
                Some(row) => row,
                None => {
                    let row = TaskRow {
                        id: initial.id.to_string(),
                        conversation_id: conversation_id.to_string(),
                        parent_task_id: None,
                        state: TaskState::Queued,
                        input: input.clone(),
                        checkpoint: input.clone(),
                        created_at: i64::try_from(initial.created_at.get())
                            .map_err(|_| AgentError::InvalidData("task time overflow"))?,
                        updated_at: i64::try_from(initial.updated_at.get())
                            .map_err(|_| AgentError::InvalidData("task time overflow"))?,
                    };
                    owner.store.create_task(&row)?;
                    row
                }
            };
            if row.input != input
                || row.id != initial.id.to_string()
                || row.conversation_id != conversation_id.to_string()
                || row.parent_task_id.is_some()
                || row.created_at
                    != i64::try_from(initial.created_at.get())
                        .map_err(|_| AgentError::InvalidData("task time overflow"))?
            {
                return Err(AgentError::ConfigurationMismatch);
            }
            let mut record: TaskRecord = serde_json::from_value(row.checkpoint.clone())
                .map_err(|_| AgentError::InvalidData("foreground task checkpoint is invalid"))?;
            validate_record(&initial, &record, &row)?;
            if row.state == TaskState::Running {
                if record.status != TaskStatus::Running {
                    return Err(AgentError::InvalidData("foreground task state mismatch"));
                }
                return Ok(record);
            }
            if !matches!(row.state, TaskState::Queued | TaskState::Waiting) {
                return Err(AgentError::InvalidData("foreground task is terminal"));
            }
            let now = UnixMillis::now()?;
            record.transition(TaskStatus::Running, now, None)?;
            let checkpoint = serde_json::to_value(&record)
                .map_err(|_| AgentError::InvalidData("task serialization failed"))?;
            owner.store.transition_task(
                &row.id,
                row.state,
                TaskState::Running,
                &checkpoint,
                i64::try_from(now.get())
                    .map_err(|_| AgentError::InvalidData("task time overflow"))?,
            )?;
            Ok(record)
        })
        .await
    }

    pub async fn reconcile_foreground_task(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
    ) -> Result<Option<TaskRecord>, AgentError> {
        let owner = self.clone();
        let exists = blocking(move || {
            Ok(owner
                .store
                .task(&TaskId::try_from(*command_id.as_uuid())?.to_string())?
                .is_some())
        })
        .await?;
        if exists {
            self.settle_foreground_task(conversation_id, command_id)
                .await
                .map(Some)
        } else {
            Ok(None)
        }
    }

    pub async fn settle_foreground_task(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
    ) -> Result<TaskRecord, AgentError> {
        let admission = self.admit(conversation_id)?;
        let owner = self.clone();
        blocking(move || {
            let _admission = admission;
            let conversation = load_conversation(&owner.store, conversation_id)?;
            let id = TaskId::try_from(*command_id.as_uuid())?;
            let mut row = owner
                .store
                .task(&id.to_string())?
                .ok_or(AgentError::InvalidData("foreground task is missing"))?;
            let initial: TaskRecord = serde_json::from_value(row.input.clone())
                .map_err(|_| AgentError::InvalidData("foreground task identity is invalid"))?;
            if initial.id != id
                || initial.conversation_id != conversation_id
                || initial.originating_command_id != command_id
            {
                return Err(AgentError::ConfigurationMismatch);
            }
            let mut record: TaskRecord = serde_json::from_value(row.checkpoint.clone())
                .map_err(|_| AgentError::InvalidData("foreground task checkpoint is invalid"))?;
            validate_record(&initial, &record, &row)?;
            let accepted = conversation
                .messages
                .iter()
                .find(|entry| {
                    entry.causation_id == Some(command_id)
                        && entry.message.role == MessageRole::User
                })
                .ok_or(AgentError::InvalidData(
                    "foreground accepted message is missing",
                ))?;
            if accepted.message.created_at != initial.created_at
                || accepted.message.content
                    != vec![MessageContent::Text {
                        text: initial.instruction.clone(),
                    }]
            {
                return Err(AgentError::ConfigurationMismatch);
            }
            let completed = conversation.info.pending_command_id != Some(command_id)
                && conversation
                    .messages
                    .iter()
                    .rev()
                    .find(|entry| {
                        entry.causation_id == Some(command_id)
                            && entry.message.role == MessageRole::Assistant
                    })
                    .is_some_and(|entry| {
                        entry
                            .message
                            .content
                            .iter()
                            .all(|block| !matches!(block, MessageContent::ToolCall(_)))
                    });
            if row.state == TaskState::Completed {
                if !completed || record.status != TaskStatus::Completed {
                    return Err(AgentError::InvalidData("foreground completion mismatch"));
                }
                return Ok(record);
            }
            if completed && matches!(row.state, TaskState::Queued | TaskState::Waiting) {
                let now = UnixMillis::now()?;
                record.transition(TaskStatus::Running, now, None)?;
                owner.store.transition_task(
                    &row.id,
                    row.state,
                    TaskState::Running,
                    &serde_json::to_value(&record)
                        .map_err(|_| AgentError::InvalidData("task serialization failed"))?,
                    i64::try_from(now.get())
                        .map_err(|_| AgentError::InvalidData("task time overflow"))?,
                )?;
                row.state = TaskState::Running;
            }
            if row.state != TaskState::Running {
                return Ok(record);
            }
            let saved = conversation
                .native_run()
                .filter(|run| run.command_id == command_id);
            let (state, status) = if completed {
                (TaskState::Completed, TaskStatus::Completed)
            } else if saved
                .as_ref()
                .is_some_and(|run| matches!(run.status, LoopStatus::AwaitingPermission))
            {
                (TaskState::Waiting, TaskStatus::AwaitingPermission)
            } else {
                (TaskState::Waiting, TaskStatus::Queued)
            };
            let now = UnixMillis::now()?;
            record.transition(status, now, None)?;
            owner.store.transition_task(
                &row.id,
                row.state,
                state,
                &serde_json::to_value(&record)
                    .map_err(|_| AgentError::InvalidData("task serialization failed"))?,
                i64::try_from(now.get())
                    .map_err(|_| AgentError::InvalidData("task time overflow"))?,
            )?;
            Ok(record)
        })
        .await
    }
}

fn validate_record(
    initial: &TaskRecord,
    record: &TaskRecord,
    row: &TaskRow,
) -> Result<(), AgentError> {
    if record.id != initial.id
        || record.schema_version != initial.schema_version
        || record.conversation_id != initial.conversation_id
        || record.parent_task_id != initial.parent_task_id
        || record.originating_command_id != initial.originating_command_id
        || record.instruction != initial.instruction
        || record.created_at != initial.created_at
        || i64::try_from(record.updated_at.get()).ok() != Some(row.updated_at)
        || record.failure.is_some()
    {
        return Err(AgentError::InvalidData("foreground task identity changed"));
    }
    Ok(())
}
