use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::permissions::ExecutionAuthority;
use crate::protocol::{TaskId, TaskStatus, UnixMillis};
use crate::storage::{Store, TaskRow, TaskState};

use super::types::*;

const MAX_TASK_BYTES: usize = 786_432;
const MAX_RETAINED_TASKS: i64 = 4096;
pub(super) const CHILD_DELIVERY_METADATA_BYTES: usize = 1024;

pub(super) fn check_capacity(store: &Store, max_pending: usize) -> Result<(), SchedulerError> {
    let connection = store.connection()?;
    let (retained,pending):(i64,i64)=connection.query_row("SELECT count(*),coalesce(sum(state IN ('queued','running','waiting')),0) FROM tasks WHERE json_extract(input,'$.scheduler_version')=1",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    if retained >= MAX_RETAINED_TASKS || pending >= max_pending as i64 {
        return Err(SchedulerError::Capacity("retained or pending tasks"));
    }
    Ok(())
}

pub(super) fn has_running_operations(store: &Store, id: TaskId) -> Result<bool, SchedulerError> {
    Ok(store.connection()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1 AND state='running')",
        [id.to_string()],
        |row| row.get(0),
    )?)
}

pub(super) fn check_remote_identity(
    store: &Store,
    candidate: &StoredTask,
) -> Result<(), SchedulerError> {
    let proposed = candidate
        .definition
        .remote
        .as_ref()
        .ok_or(SchedulerError::Invalid("remote definition is missing"))?;
    let connection = store.connection()?;
    let mut statement = connection.prepare("SELECT id FROM tasks WHERE json_extract(input,'$.scheduler_version')=1 AND json_type(input,'$.remote')='object' ORDER BY id LIMIT ?1")?;
    let ids = statement
        .query_map([MAX_RETAINED_TASKS + 1], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    drop(connection);
    if ids.len() as i64 > MAX_RETAINED_TASKS {
        return Err(SchedulerError::Capacity("retained remote identities"));
    }
    for id in ids {
        let existing = load(store, id.parse()?)?;
        let original = existing
            .definition
            .remote
            .as_ref()
            .ok_or(SchedulerError::Invalid("remote definition is missing"))?;
        let original_commands = [
            original.approval.command_id,
            existing.definition.initial.originating_command_id,
        ];
        let proposed_commands = [
            proposed.approval.command_id,
            candidate.definition.initial.originating_command_id,
        ];
        let binding = &original.binding;
        let proposed_binding = &proposed.binding;
        if existing.definition.initial.conversation_id
            == candidate.definition.initial.conversation_id
            || original_commands
                .iter()
                .any(|command| proposed_commands.contains(command))
            || binding.delegation_id == proposed_binding.delegation_id
            || binding.attempt_id == proposed_binding.attempt_id
            || binding.worker_task_id == proposed_binding.worker_task_id
            || binding.worker_conversation_id == proposed_binding.worker_conversation_id
            || [
                binding.worker_create_command_id,
                binding.worker_input_command_id,
            ]
            .iter()
            .any(|command| {
                [
                    proposed_binding.worker_create_command_id,
                    proposed_binding.worker_input_command_id,
                ]
                .contains(command)
            })
        {
            return Err(SchedulerError::Conflict(candidate.definition.initial.id));
        }
    }
    Ok(())
}

pub(super) fn has_unknown_operations(store: &Store, id: TaskId) -> Result<bool, SchedulerError> {
    Ok(store.connection()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1 AND state='unknown')",
        [id.to_string()],
        |row| row.get(0),
    )?)
}

pub(super) fn encode(value: &impl Serialize) -> Result<Value, SchedulerError> {
    let encoded = serde_json::to_vec(value)?;
    if encoded.len() > MAX_TASK_BYTES {
        return Err(SchedulerError::Capacity("task checkpoint bytes"));
    }
    Ok(serde_json::from_slice(&encoded)?)
}

pub(super) fn encode_checkpoint(checkpoint: &TaskCheckpoint) -> Result<Value, SchedulerError> {
    if let Some(evidence) = &checkpoint.terminal_evidence {
        evidence.validate_size()?;
        if !checkpoint.task.status.is_terminal() {
            return Err(SchedulerError::Invalid("live task has terminal evidence"));
        }
    }
    let bytes = serde_json::to_vec(checkpoint)?;
    let remaining = checkpoint
        .child_wait
        .as_ref()
        .filter(|_| !checkpoint.cancel_requested)
        .map_or(0, |wait| {
            wait.children
                .iter()
                .filter(|child| child.delivery.is_none())
                .count()
        });
    let delivery_reserve = remaining
        .checked_mul(CHILD_DELIVERY_METADATA_BYTES)
        .ok_or(SchedulerError::Capacity("task checkpoint bytes"))?;
    let status_bytes = serde_json::to_vec(&checkpoint.last_output)?
        .len()
        .saturating_add(serde_json::to_vec(&checkpoint.diagnostic)?.len())
        .saturating_add(serde_json::to_vec(&checkpoint.waiting)?.len())
        .saturating_add(serde_json::to_vec(&checkpoint.task.failure)?.len())
        .saturating_add(serde_json::to_vec(&checkpoint.task.status)?.len())
        .saturating_add(serde_json::to_vec(&checkpoint.task.updated_at)?.len());
    let status_reserve = if checkpoint.task.status.is_terminal() {
        0
    } else {
        81_920_usize.saturating_sub(status_bytes)
    };
    let reserve = delivery_reserve
        .checked_add(status_reserve)
        .and_then(|reserve| {
            reserve.checked_add(if checkpoint.task.status.is_terminal() {
                0
            } else {
                super::evidence::MAX_TERMINAL_EVIDENCE_BYTES
            })
        })
        .ok_or(SchedulerError::Capacity("task checkpoint bytes"))?;
    if bytes
        .len()
        .checked_add(reserve)
        .is_none_or(|size| size > MAX_TASK_BYTES)
    {
        return Err(SchedulerError::Capacity("task checkpoint bytes"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn check_child_depth(store: &Store, id: TaskId) -> Result<(), SchedulerError> {
    let mut task = load(store, id)?;
    for _ in 1..16 {
        let Some(parent) = task.definition.initial.parent_task_id else {
            return Ok(());
        };
        task = load(store, parent)?;
    }
    Err(SchedulerError::Capacity("subagent nesting"))
}

pub(super) fn load(store: &Store, id: TaskId) -> Result<StoredTask, SchedulerError> {
    let sizes: Option<(i64, i64)> = store.connection()?.query_row(
        "SELECT length(CAST(input AS BLOB)),length(CAST(checkpoint AS BLOB)) FROM tasks WHERE id=?1",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    if sizes.is_some_and(|(input, checkpoint)| {
        input > MAX_TASK_BYTES as i64 || checkpoint > MAX_TASK_BYTES as i64
    }) {
        return Err(SchedulerError::Capacity("stored task bytes"));
    }
    let row = store
        .task(&id.to_string())?
        .ok_or(SchedulerError::NotFound(id))?;
    decode(row)
}

pub(super) fn row(task: &StoredTask) -> Result<TaskRow, SchedulerError> {
    let initial = &task.definition.initial;
    Ok(TaskRow {
        id: initial.id.to_string(),
        conversation_id: initial.conversation_id.to_string(),
        parent_task_id: initial.parent_task_id.map(|id| id.to_string()),
        state: task.state,
        input: encode(&task.definition)?,
        checkpoint: encode_checkpoint(&task.checkpoint)?,
        created_at: timestamp(initial.created_at)?,
        updated_at: timestamp(task.checkpoint.task.updated_at)?,
    })
}

pub(super) fn remote_ids(store: &Store) -> Result<Vec<TaskId>, SchedulerError> {
    let connection = store.connection()?;
    let mut statement = connection.prepare("SELECT id FROM tasks WHERE json_extract(input,'$.scheduler_version')=1 AND json_type(input,'$.remote')='object' ORDER BY id LIMIT ?1")?;
    let ids = statement
        .query_map([MAX_RETAINED_TASKS + 1], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.len() as i64 > MAX_RETAINED_TASKS {
        return Err(SchedulerError::Capacity("retained remote tasks"));
    }
    ids.into_iter()
        .map(|id| id.parse().map_err(SchedulerError::from))
        .collect()
}

pub(super) fn remote_id_by_delegation(
    store: &Store,
    delegation: uuid::Uuid,
) -> Result<TaskId, SchedulerError> {
    let connection = store.connection()?;
    let mut statement = connection.prepare("SELECT length(CAST(id AS BLOB)),id FROM tasks WHERE json_extract(input,'$.scheduler_version')=1 AND json_type(input,'$.remote')='object' AND json_extract(input,'$.remote.binding.delegation_id') COLLATE NOCASE IN (?1,?2,?3,?4) LIMIT 2")?;
    let mut rows = statement.query([
        delegation.hyphenated().to_string(),
        delegation.simple().to_string(),
        delegation.braced().to_string(),
        delegation.urn().to_string(),
    ])?;
    let mut found = None;
    while let Some(row) = rows.next()? {
        if row.get::<_, i64>(0)? != 36 {
            return Err(SchedulerError::Invalid(
                "stored remote task identifier length",
            ));
        }
        let id: TaskId = row.get::<_, String>(1)?.parse()?;
        if found.is_some() {
            return Err(SchedulerError::Conflict(id));
        }
        found = Some(id);
    }
    found.ok_or(SchedulerError::RemoteBindingNotFound(delegation))
}

pub(super) fn active_in_conversation(
    store: &Store,
    conversation: crate::protocol::ConversationId,
) -> Result<Option<TaskSnapshot>, SchedulerError> {
    let connection = store.connection()?;
    let mut statement=connection.prepare("SELECT id FROM tasks WHERE conversation_id=?1 AND state IN ('queued','running','waiting') ORDER BY id LIMIT 2")?;
    let ids = statement
        .query_map([conversation.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    drop(connection);
    if ids.len() > 1 {
        return Err(SchedulerError::Invalid(
            "multiple active tasks own the conversation",
        ));
    }
    ids.first()
        .map(|id| load(store, id.parse()?).map(|task| task.snapshot()))
        .transpose()
}

fn decode(row: TaskRow) -> Result<StoredTask, SchedulerError> {
    if row.input.get("scheduler_version").is_none() {
        return Err(SchedulerError::NotOwned(row.id.parse()?));
    }
    let definition: Definition = serde_json::from_value(row.input)?;
    let checkpoint: TaskCheckpoint = serde_json::from_value(row.checkpoint)?;
    definition.configuration.validate()?;
    definition.execution.validate()?;
    if let Some(evidence) = &checkpoint.terminal_evidence {
        if !checkpoint.task.status.is_terminal() {
            return Err(SchedulerError::Invalid("live task has terminal evidence"));
        }
        evidence.validate(&definition, checkpoint.active_command)?;
    }
    if let Some(wait) = &checkpoint.child_wait {
        super::inputs::validate_wait(wait)?;
        if wait.active_command_id != checkpoint.active_command {
            return Err(SchedulerError::Invalid(
                "child wait targets a different command",
            ));
        }
    }
    if let Some(retired) = &checkpoint.provider_retirement
        && (checkpoint.task.status != TaskStatus::Failed
            || checkpoint.cancel_requested
            || checkpoint.active_command != retired.command_id
            || checkpoint.task.updated_at != retired.retired_at
            || !matches!(checkpoint.waiting, Some(WaitingReason::ProviderUnavailable))
            || checkpoint.task.failure.as_ref().is_none_or(|failure| {
                failure.code != "provider_factory_unavailable" || failure.retryable
            }))
    {
        return Err(SchedulerError::Invalid("invalid provider retirement audit"));
    }
    let initial = &definition.initial;
    let current = &checkpoint.task;
    if definition.scheduler_version != 1
        || checkpoint.scheduler_version != 1
        || initial.id.to_string() != row.id
        || initial.conversation_id.to_string() != row.conversation_id
        || initial.parent_task_id.map(|id| id.to_string()) != row.parent_task_id
        || initial.status != TaskStatus::Queued
        || current.id != initial.id
        || current.conversation_id != initial.conversation_id
        || current.parent_task_id != initial.parent_task_id
        || current.originating_command_id != initial.originating_command_id
        || current.instruction != initial.instruction
        || current.created_at != initial.created_at
        || timestamp(current.updated_at)? != row.updated_at
        || timestamp(initial.created_at)? != row.created_at
        || state(current.status) != row.state
        || checkpoint.queued.len() > 32
        || checkpoint.resumes.len() > 32
        || definition.execution.allowed_tools.is_none()
    {
        return Err(SchedulerError::Invalid(
            "task checkpoint identity or bounds are invalid",
        ));
    }
    let task = StoredTask {
        definition,
        checkpoint,
        state: row.state,
    };
    super::remote::validate_task(&task)?;
    Ok(task)
}

pub(super) fn insert(
    store: &Store,
    task: &StoredTask,
    max_pending: usize,
) -> Result<bool, SchedulerError> {
    insert_task(store, task, max_pending, None)
}

pub(super) fn insert_remote(
    store: &Store,
    task: &StoredTask,
    max_pending: usize,
    authorization: &super::remote::RemoteTaskAdmission,
) -> Result<bool, SchedulerError> {
    authorization.validate_task(task)?;
    insert_task(store, task, max_pending, Some(authorization))
}

fn insert_task(
    store: &Store,
    task: &StoredTask,
    max_pending: usize,
    authorization: Option<&super::remote::RemoteTaskAdmission>,
) -> Result<bool, SchedulerError> {
    let input = encode(&task.definition)?;
    let checkpoint = encode_checkpoint(&task.checkpoint)?;
    let mut connection = store.connection()?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(authorization) = authorization {
        authorization.validate_fresh(store)?;
    }
    let existing: Option<String> = transaction
        .query_row(
            "SELECT input FROM tasks WHERE id=?1",
            [task.definition.initial.id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if serde_json::from_str::<Value>(&existing)? != input {
            return Err(SchedulerError::Conflict(task.definition.initial.id));
        }
        return Ok(false);
    }
    let collision:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE conversation_id=?1 AND (state IN ('queued','running','waiting') OR json_extract(input,'$.initial.originating_command_id')=?2))",params![task.definition.initial.conversation_id.to_string(),task.definition.initial.originating_command_id.to_string()],|row|row.get(0))?;
    if collision {
        return Err(SchedulerError::Conflict(task.definition.initial.id));
    }
    let (retained,pending):(i64,i64)=transaction.query_row("SELECT count(*),coalesce(sum(state IN ('queued','running','waiting')),0) FROM tasks WHERE json_extract(input,'$.scheduler_version')=1",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    if retained >= MAX_RETAINED_TASKS || pending >= max_pending as i64 {
        return Err(SchedulerError::Capacity("retained or pending tasks"));
    }
    let initial = &task.definition.initial;
    transaction.execute("INSERT INTO tasks(id,conversation_id,parent_task_id,state,input,checkpoint,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![initial.id.to_string(),initial.conversation_id.to_string(),initial.parent_task_id.map(|id|id.to_string()),task.state.as_str(),serde_json::to_string(&input)?,serde_json::to_string(&checkpoint)?,timestamp(initial.created_at)?,timestamp(task.checkpoint.task.updated_at)?])?;
    if let Some(authorization) = authorization {
        authorization.validate_fresh(store)?;
    }
    transaction.commit()?;
    Ok(true)
}

pub(super) fn save(
    store: &Store,
    task: &mut StoredTask,
    status: TaskStatus,
) -> Result<(), SchedulerError> {
    let now = UnixMillis::now()?;
    if status != task.checkpoint.task.status {
        task.checkpoint.task.transition(
            status,
            now,
            if status == TaskStatus::Failed {
                Some(crate::protocol::TaskFailure {
                    code: if task.checkpoint.cancel_requested {
                        "cancelled_outcome_uncertain"
                    } else {
                        "native_task_failed"
                    }
                    .into(),
                    message: task
                        .checkpoint
                        .diagnostic
                        .clone()
                        .unwrap_or_else(|| "Native task failed".into()),
                    retryable: false,
                })
            } else {
                None
            },
        )?;
    } else {
        task.checkpoint.task.updated_at = now;
    }
    let next = state(status);
    let payload = encode_checkpoint(&task.checkpoint)?;
    if task.state == next {
        store.checkpoint_task(
            &task.definition.initial.id.to_string(),
            task.state,
            &payload,
            timestamp(now)?,
        )?;
    } else {
        store.transition_task(
            &task.definition.initial.id.to_string(),
            task.state,
            next,
            &payload,
            timestamp(now)?,
        )?;
    }
    task.state = next;
    Ok(())
}

pub(super) fn retire_provider_unavailable(
    store: &Store,
    task: &mut StoredTask,
    request: crate::protocol::CommandId,
) -> Result<(), SchedulerError> {
    let id = task.definition.initial.id;
    let expected = serde_json::to_value(&task.checkpoint)?;
    let now = UnixMillis::try_from(
        UnixMillis::now()?
            .get()
            .max(task.checkpoint.task.updated_at.get()),
    )?;
    let mut next = task.checkpoint.clone();
    next.task.transition(
        TaskStatus::Failed,
        now,
        Some(crate::protocol::TaskFailure {
            code: "provider_factory_unavailable".into(),
            message: "Provider construction failed before native execution".into(),
            retryable: false,
        }),
    )?;
    next.provider_retirement = Some(ProviderRetirement {
        request_id: request,
        command_id: next.active_command,
        retired_at: now,
    });
    next.terminal_evidence = Some(super::evidence::TerminalTaskEvidence::no_attempt(
        &task.definition,
        next.active_command,
    )?);
    let payload = encode_checkpoint(&next)?;
    let mut connection = store.connection()?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (current, state): (String, String) = transaction
        .query_row(
            "SELECT checkpoint,state FROM tasks WHERE id=?1 AND typeof(checkpoint)='text' AND length(CAST(checkpoint AS BLOB))<=?2 AND state='waiting'",
            params![id.to_string(), MAX_TASK_BYTES],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(SchedulerError::Conflict(id))?;
    let current: TaskCheckpoint = serde_json::from_str(&current)?;
    if serde_json::to_value(current)? != expected || state != task.state.as_str() {
        return Err(SchedulerError::Conflict(id));
    }
    let has_operations: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1)",
        [id.to_string()],
        |row| row.get(0),
    )?;
    if has_operations {
        return Err(SchedulerError::OwnerDecision(id));
    }
    let changed = transaction.execute(
        "UPDATE tasks SET state='failed',checkpoint=?2,updated_at=?3 WHERE id=?1 AND state=?4 AND updated_at<=?3",
        params![id.to_string(), serde_json::to_string(&payload)?, timestamp(now)?, task.state.as_str()],
    )?;
    if changed != 1 {
        return Err(SchedulerError::Conflict(id));
    }
    transaction.commit()?;
    task.checkpoint = next;
    task.state = TaskState::Failed;
    Ok(())
}

pub(super) fn state(status: TaskStatus) -> TaskState {
    match status {
        TaskStatus::Queued => TaskState::Queued,
        TaskStatus::Running => TaskState::Running,
        TaskStatus::AwaitingPermission | TaskStatus::WaitingForChildren => TaskState::Waiting,
        TaskStatus::Completed => TaskState::Completed,
        TaskStatus::Failed => TaskState::Failed,
        TaskStatus::Cancelled => TaskState::Cancelled,
    }
}

pub(super) fn unfinished(store: &Store, maximum: usize) -> Result<Vec<StoredTask>, SchedulerError> {
    let connection = store.connection()?;
    let mut statement=connection.prepare("SELECT id FROM tasks WHERE json_extract(input,'$.scheduler_version')=1 AND state IN ('queued','running','waiting') ORDER BY created_at,id LIMIT ?1")?;
    let ids = statement
        .query_map([maximum as i64 + 1], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(crate::storage::StorageError::from)?;
    drop(statement);
    drop(connection);
    if ids.len() > maximum {
        return Err(SchedulerError::Capacity("recovered pending tasks"));
    }
    ids.into_iter().map(|id| load(store, id.parse()?)).collect()
}

pub(super) fn descendants(
    store: &Store,
    parent: TaskId,
    maximum: usize,
) -> Result<Vec<TaskId>, SchedulerError> {
    let connection = store.connection()?;
    let mut statement=connection.prepare("WITH RECURSIVE children(id) AS (SELECT id FROM tasks WHERE parent_task_id=?1 UNION SELECT tasks.id FROM tasks JOIN children ON tasks.parent_task_id=children.id) SELECT id FROM children LIMIT ?2")?;
    let ids = statement
        .query_map(params![parent.to_string(), maximum as i64 + 1], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(crate::storage::StorageError::from)?;
    if ids.len() > maximum {
        return Err(SchedulerError::Capacity("descendant task count"));
    }
    ids.into_iter()
        .map(|id| id.parse().map_err(SchedulerError::from))
        .collect()
}

pub(super) fn authorize_target(
    store: &Store,
    caller: TaskId,
    target: TaskId,
    allow_self: bool,
) -> Result<(), SchedulerError> {
    if caller == target {
        return if allow_self {
            Ok(())
        } else {
            Err(SchedulerError::OutsideScope)
        };
    }
    let mut current = load(store, target)?;
    for _ in 0..16 {
        match current.definition.initial.parent_task_id {
            Some(parent) if parent == caller => return Ok(()),
            Some(parent) => current = load(store, parent)?,
            None => break,
        }
    }
    Err(SchedulerError::OutsideScope)
}

pub(super) fn authority(store: &Store, id: TaskId) -> Result<ExecutionAuthority, SchedulerError> {
    let mut chain = vec![load(store, id)?];
    while let Some(parent) = chain
        .last()
        .and_then(|task| task.definition.initial.parent_task_id)
    {
        if chain.len() >= 16 {
            return Err(SchedulerError::Capacity("subagent nesting"));
        }
        if chain
            .iter()
            .any(|task| task.definition.initial.id == parent)
        {
            return Err(SchedulerError::Invalid("cyclic task ancestry"));
        }
        chain.push(load(store, parent)?);
    }
    chain.reverse();
    let root = chain.first().ok_or(SchedulerError::NotFound(id))?;
    let mut authority = root.definition.execution.authority(
        root.definition.initial.conversation_id,
        root.definition.initial.id,
    )?;
    for child in chain.iter().skip(1) {
        let policy = &child.definition.execution;
        super::workspace::validate(
            &policy.workspace_root,
            child
                .definition
                .initial
                .parent_task_id
                .ok_or(SchedulerError::Invalid("child parent is missing"))?,
            child.definition.initial.id,
        )?;
        let allowed = policy
            .allowed_tools
            .clone()
            .ok_or(SchedulerError::Invalid("child policy was not captured"))?;
        authority = if child.definition.delegation.is_some() {
            delegation_authority(root)?.delegate(
                child.definition.initial.conversation_id.to_string(),
                child.definition.initial.id.to_string(),
                policy.workspace_root.clone(),
                policy.mode,
                allowed,
            )?
        } else {
            authority
                .child_in_workspace(
                    child.definition.initial.conversation_id.to_string(),
                    child.definition.initial.id.to_string(),
                    policy.workspace_root.clone(),
                    policy.mode,
                )?
                .restrict_tools(allowed)?
        };
    }
    Ok(authority)
}

pub(super) fn delegation_authority(
    root: &StoredTask,
) -> Result<ExecutionAuthority, SchedulerError> {
    let ceiling = root
        .definition
        .execution
        .delegation_ceiling
        .as_ref()
        .ok_or(SchedulerError::Delegation {
            code: "agent_messaging_unavailable",
            message: "the root turn did not capture an agent messaging ceiling",
        })?;
    let authority = ExecutionAuthority::new(
        root.definition.initial.conversation_id.to_string(),
        root.definition.initial.id.to_string(),
        root.definition.execution.workspace_root.clone(),
        ceiling.mode,
    )?;
    Ok(match &ceiling.allowed_tools {
        Some(allowed) => authority.restrict_tools(allowed.clone())?,
        None => authority,
    })
}

pub(super) fn lineage(store: &Store, id: TaskId) -> Result<Vec<StoredTask>, SchedulerError> {
    let mut chain = vec![load(store, id)?];
    while let Some(parent) = chain
        .last()
        .and_then(|task| task.definition.initial.parent_task_id)
    {
        if chain.len() >= 16 {
            return Err(SchedulerError::Capacity("subagent nesting"));
        }
        chain.push(load(store, parent)?);
    }
    Ok(chain)
}

pub(super) fn delegated_children(store: &Store, caller: TaskId) -> Result<usize, SchedulerError> {
    let count: i64 = store.connection()?.query_row(
        "SELECT count(*) FROM tasks WHERE parent_task_id=?1 AND json_type(input,'$.delegation')='object'",
        [caller.to_string()],
        |row| row.get(0),
    )?;
    usize::try_from(count).map_err(|_| SchedulerError::Invalid("negative task count"))
}

pub(super) fn active_delegated_grants(
    store: &Store,
    caller: TaskId,
) -> Result<u64, SchedulerError> {
    let total: i64 = store.connection()?.query_row(
        "SELECT coalesce(sum(json_extract(input,'$.execution.limits.max_charged_tokens')),0) FROM tasks WHERE parent_task_id=?1 AND json_type(input,'$.delegation')='object' AND coalesce(json_extract(checkpoint,'$.task.status'),'') NOT IN ('completed','failed','cancelled')",
        [caller.to_string()],
        |row| row.get(0),
    )?;
    u64::try_from(total).map_err(|_| SchedulerError::Invalid("negative delegated token grants"))
}

pub(super) fn agent_log(
    store: &Store,
    agent: Option<crate::host::agents::AgentId>,
    conversation: Option<crate::protocol::ConversationId>,
    limit: usize,
) -> Result<Vec<super::types::AgentHop>, SchedulerError> {
    if !(1..=100).contains(&limit) {
        return Err(SchedulerError::Invalid("agent log limit must be 1 to 100"));
    }
    let ids: Vec<String> = {
        let connection = store.connection()?;
        let mut statement = connection.prepare(
            "SELECT t.id FROM tasks t LEFT JOIN tasks p ON p.id=t.parent_task_id \
             WHERE json_type(t.input,'$.delegation')='object' \
             AND (?1 IS NULL OR json_extract(t.input,'$.delegation.from')=?1 OR json_extract(t.input,'$.delegation.to')=?1) \
             AND (?2 IS NULL OR t.conversation_id=?2 OR p.conversation_id=?2) \
             ORDER BY t.created_at DESC, t.id LIMIT ?3",
        )?;
        statement
            .query_map(
                rusqlite::params![
                    agent.map(|id| id.to_string()),
                    conversation.map(|id| id.to_string()),
                    i64::try_from(limit).map_err(|_| SchedulerError::Invalid("limit"))?
                ],
                |row| row.get(0),
            )?
            .collect::<Result<_, _>>()?
    };
    let mut hops = Vec::with_capacity(ids.len());
    for id in ids {
        let task = load(store, id.parse()?)?;
        let delegation = task
            .definition
            .delegation
            .as_ref()
            .ok_or(SchedulerError::Invalid(
                "delegated task lost its delegation",
            ))?;
        let parent_task_id = task
            .definition
            .initial
            .parent_task_id
            .ok_or(SchedulerError::Invalid("delegated task has no parent"))?;
        let parent = load(store, parent_task_id)?;
        let line = task
            .definition
            .initial
            .instruction
            .lines()
            .next()
            .unwrap_or("");
        let mut end = line.len().min(200);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        hops.push(super::types::AgentHop {
            task_id: task.definition.initial.id,
            parent_task_id,
            caller_conversation_id: parent.definition.initial.conversation_id,
            conversation_id: task.definition.initial.conversation_id,
            from: delegation.from,
            to: delegation.to,
            status: task.checkpoint.task.status,
            created_at: task.definition.initial.created_at,
            updated_at: task.checkpoint.task.updated_at,
            text: line[..end].to_owned(),
        });
    }
    Ok(hops)
}

pub(super) fn command_exists(
    store: &Store,
    id: crate::protocol::CommandId,
) -> Result<bool, SchedulerError> {
    let connection = store.connection()?;
    Ok(connection
        .query_row(
            "SELECT 1 FROM commands WHERE id=?1",
            [id.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

pub(super) fn cancelled_ancestor(store: &Store, id: TaskId) -> Result<bool, SchedulerError> {
    let mut task = load(store, id)?;
    for _ in 0..16 {
        if task.checkpoint.cancel_requested || task.checkpoint.task.status == TaskStatus::Cancelled
        {
            return Ok(true);
        }
        let Some(parent) = task.definition.initial.parent_task_id else {
            return Ok(false);
        };
        task = load(store, parent)?;
    }
    Err(SchedulerError::Capacity("task ancestry"))
}
