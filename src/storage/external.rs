use super::*;

#[derive(Serialize)]
pub(crate) struct ExternalTaskStateUpdate {
    pub(crate) expected: TaskRow,
    pub(crate) next_state: TaskState,
    pub(crate) checkpoint: Value,
    pub(crate) updated_at: i64,
}

pub(super) struct PreparedTaskUpdate {
    input: String,
    previous_checkpoint: String,
    checkpoint: String,
}

pub(super) fn prepare(
    request: &AppendRequest,
    update: &ExternalTaskStateUpdate,
) -> StorageResult<PreparedTaskUpdate> {
    let expected = &update.expected;
    validate_id(&expected.id)?;
    validate_id(&expected.conversation_id)?;
    validate_optional_id(expected.parent_task_id.as_deref())?;
    validate_interval(expected.created_at, expected.updated_at)?;
    validate_interval(expected.updated_at, update.updated_at)?;
    if expected.parent_task_id.is_some() || expected.conversation_id != request.conversation_id {
        return invalid("external task append requires a root task in the same conversation");
    }
    if expected.state != TaskState::Waiting
        || !matches!(
            update.next_state,
            TaskState::Waiting | TaskState::Completed | TaskState::Failed | TaskState::Cancelled
        )
    {
        return Err(state_conflict("task", &expected.id));
    }
    if request
        .events
        .iter()
        .any(|event| event.occurred_at != update.updated_at)
    {
        return invalid("external task events must use the task update timestamp");
    }
    Ok(PreparedTaskUpdate {
        input: encode_json(&expected.input)?,
        previous_checkpoint: encode_json(&expected.checkpoint)?,
        checkpoint: encode_json(&update.checkpoint)?,
    })
}

pub(super) fn state_digest(
    state: &ConversationStateUpdate,
    task: &ExternalTaskStateUpdate,
) -> StorageResult<String> {
    let mut hash = Sha256::new();
    hash.update(b"everything.conversation.external-task-update.v1\0");
    hash.update(serde_json::to_vec(&(state, task))?);
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn apply(
    connection: &Connection,
    update: &ExternalTaskStateUpdate,
    prepared: &PreparedTaskUpdate,
) -> StorageResult<()> {
    let actual = bounded_task(connection, &update.expected.id)?;
    if actual != update.expected {
        return Err(state_conflict("task", &update.expected.id));
    }
    require_no_tools(connection, &update.expected)?;
    let changed = connection.execute(
        "UPDATE tasks SET state=?2,checkpoint=?3,updated_at=?4 WHERE id=?1 AND conversation_id=?5 AND parent_task_id IS NULL AND state=?6 AND input=?7 AND checkpoint=?8 AND created_at=?9 AND updated_at=?10",
        params![
            update.expected.id,
            update.next_state.as_str(),
            prepared.checkpoint,
            update.updated_at,
            update.expected.conversation_id,
            update.expected.state.as_str(),
            prepared.input,
            prepared.previous_checkpoint,
            update.expected.created_at,
            update.expected.updated_at
        ],
    )?;
    require_change(changed, "task", &update.expected.id)
}

pub(super) fn verify_committed(
    connection: &Connection,
    update: &ExternalTaskStateUpdate,
) -> StorageResult<()> {
    let actual = bounded_task(connection, &update.expected.id)?;
    if actual.id != update.expected.id
        || actual.conversation_id != update.expected.conversation_id
        || actual.parent_task_id != update.expected.parent_task_id
        || actual.state != update.next_state
        || actual.input != update.expected.input
        || actual.checkpoint != update.checkpoint
        || actual.created_at != update.expected.created_at
        || actual.updated_at != update.updated_at
    {
        return Err(state_conflict("task", &update.expected.id));
    }
    require_no_tools(connection, &update.expected)
}

fn bounded_task(connection: &Connection, id: &str) -> StorageResult<TaskRow> {
    let lengths: Option<(i64, Option<i64>, i64, i64)> = connection
        .query_row(
            "SELECT length(CAST(conversation_id AS BLOB)),length(CAST(parent_task_id AS BLOB)),length(CAST(input AS BLOB)),length(CAST(checkpoint AS BLOB)) FROM tasks WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let (conversation_bytes, parent_bytes, input_bytes, checkpoint_bytes) =
        lengths.ok_or_else(|| missing("task", id))?;
    if !(1..=256).contains(&conversation_bytes)
        || parent_bytes.is_some_and(|bytes| !(1..=256).contains(&bytes))
        || !(1..=MAX_JSON_BYTES as i64).contains(&input_bytes)
        || !(1..=MAX_JSON_BYTES as i64).contains(&checkpoint_bytes)
    {
        return invalid("external task row exceeds its bounded representation");
    }
    let task = connection.query_row(
        "SELECT id,conversation_id,parent_task_id,state,input,checkpoint,created_at,updated_at FROM tasks WHERE id=?1",
        [id],
        task_row,
    )?;
    Ok(task)
}

fn require_no_tools(connection: &Connection, task: &TaskRow) -> StorageResult<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1 OR conversation_id=?2)",
        params![task.id, task.conversation_id],
        |row| row.get(0),
    )?;
    if exists {
        return Err(state_conflict(
            "external task without local tools",
            &task.id,
        ));
    }
    Ok(())
}
