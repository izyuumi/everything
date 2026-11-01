use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use crate::agent::{Agent, LoopStatus, PauseReason, TaskInput, TaskInputCommand};
use crate::protocol::{CommandId, TaskId};
use crate::storage::Store;

use super::repository;
use super::types::{
    ChildDelivery, ChildWaitSet, SchedulerError, StoredTask, WaitChild, WaitingReason,
};

pub(super) fn validate_wait(wait: &ChildWaitSet) -> Result<(), SchedulerError> {
    if wait.children.is_empty()
        || wait.children.len() > 32
        || wait
            .children
            .iter()
            .map(|child| child.task_id)
            .collect::<BTreeSet<_>>()
            .len()
            != wait.children.len()
    {
        return Err(SchedulerError::Invalid(
            "wait requires 1 to 32 distinct descendants",
        ));
    }
    for child in &wait.children {
        if let Some(delivery) = &child.delivery
            && (!delivery.status.is_terminal()
                || delivery.output.len() > 8192
                || delivery.delivery_id != delivery_id(wait.wait_id, child.task_id)?)
        {
            return Err(SchedulerError::Invalid("invalid saved child completion"));
        }
    }
    Ok(())
}

pub(super) fn adopt(
    store: &Store,
    task: &mut StoredTask,
    conversation: &crate::agent::Conversation,
    wait_id: CommandId,
) -> Result<(), SchedulerError> {
    let state = conversation.task_input_state();
    if state.wait_id != Some(wait_id) {
        return Err(SchedulerError::Invalid(
            "native child wait is not in the task inbox",
        ));
    }
    if task
        .checkpoint
        .child_wait
        .as_ref()
        .is_some_and(|wait| wait.children.iter().any(|child| child.delivery.is_none()))
    {
        return Err(SchedulerError::Capacity(
            "task already has an unresolved child wait",
        ));
    }
    let original = store
        .command_request(&wait_id.to_string())?
        .ok_or(SchedulerError::Invalid(
            "native child wait was not recorded",
        ))?;
    let event = original
        .events
        .first()
        .filter(|_| original.events.len() == 1)
        .ok_or(SchedulerError::Invalid(
            "native child wait was not recorded",
        ))?;
    let event: crate::protocol::EventEnvelope = serde_json::from_value(event.payload.clone())?;
    for child in &state.outstanding_children {
        repository::authorize_target(store, task.definition.initial.id, *child, false)?;
    }
    let wait = ChildWaitSet {
        wait_id,
        active_command_id: task.checkpoint.active_command,
        accepted_at: event.occurred_at,
        children: state
            .outstanding_children
            .into_iter()
            .map(|task_id| WaitChild {
                task_id,
                delivery: None,
            })
            .collect(),
    };
    validate_wait(&wait)?;
    let previous = task.checkpoint.child_wait.replace(wait);
    if let Err(error) = repository::encode_checkpoint(&task.checkpoint) {
        task.checkpoint.child_wait = previous;
        return Err(error);
    }
    Ok(())
}

pub(super) async fn repair(
    agent: &Agent,
    store: &Store,
    task: &mut StoredTask,
) -> Result<bool, SchedulerError> {
    let Some(wait) = task.checkpoint.child_wait.clone() else {
        return Ok(false);
    };
    if task.checkpoint.task.status.is_terminal()
        || repository::cancelled_ancestor(store, task.definition.initial.id)?
    {
        return Ok(false);
    }
    validate_wait(&wait)?;
    if wait.active_command_id != task.checkpoint.active_command {
        return Err(SchedulerError::Conflict(task.definition.initial.id));
    }
    let conversation = agent
        .conversation(task.definition.initial.conversation_id)
        .await?;
    if conversation.config != task.definition.configuration {
        return Err(SchedulerError::Conflict(task.definition.initial.id));
    }
    let acceptance = agent
        .accept_task_input(TaskInputCommand {
            conversation_id: task.definition.initial.conversation_id,
            task_id: task.definition.initial.id,
            active_command_id: wait.active_command_id,
            delivery_id: wait.wait_id,
            accepted_at: wait.accepted_at,
            input: TaskInput::WaitForChildren {
                children: wait.children.iter().map(|child| child.task_id).collect(),
            },
        })
        .await;
    match acceptance {
        Ok(_) => {}
        Err(error) if inbox_full(&error) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    for (index, child) in wait.children.iter().enumerate() {
        repository::authorize_target(store, task.definition.initial.id, child.task_id, false)?;
        let terminal = repository::load(store, child.task_id)?;
        if !terminal.checkpoint.task.status.is_terminal() {
            if child.delivery.is_some() {
                return Err(SchedulerError::Conflict(child.task_id));
            }
            continue;
        }
        let current = terminal_delivery(agent, store, wait.wait_id, &terminal).await?;
        let delivery = match &child.delivery {
            Some(saved) if same_delivery(saved, &current) => saved.clone(),
            Some(_) => return Err(SchedulerError::Conflict(child.task_id)),
            None => {
                let previous = task.checkpoint.clone();
                let mut metadata = current.clone();
                metadata.output.clear();
                if serde_json::to_vec(&metadata)?.len() > repository::CHILD_DELIVERY_METADATA_BYTES
                {
                    return Err(SchedulerError::Invalid(
                        "child delivery metadata exceeds its reserved budget",
                    ));
                }
                let mut captured = current;
                loop {
                    task.checkpoint
                        .child_wait
                        .as_mut()
                        .ok_or(SchedulerError::Invalid("child wait disappeared"))?
                        .children[index]
                        .delivery = Some(captured.clone());
                    let status = task.checkpoint.task.status;
                    match repository::save(store, task, status) {
                        Ok(()) => break captured,
                        Err(SchedulerError::Capacity("task checkpoint bytes"))
                            if !captured.output.is_empty() =>
                        {
                            let mut length = captured.output.len() / 2;
                            while !captured.output.is_char_boundary(length) {
                                length -= 1;
                            }
                            captured.output.truncate(length);
                        }
                        Err(SchedulerError::Capacity("task checkpoint bytes")) => {
                            task.checkpoint = previous;
                            tracing::warn!(task_id=%task.definition.initial.id, child_task_id=%child.task_id, "child completion deferred: legacy scheduler checkpoint cannot fit reserved delivery metadata; owner inspection is required");
                            return Ok(true);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        };
        let acceptance = agent
            .accept_task_input(TaskInputCommand {
                conversation_id: task.definition.initial.conversation_id,
                task_id: task.definition.initial.id,
                active_command_id: wait.active_command_id,
                delivery_id: delivery.delivery_id,
                accepted_at: delivery.accepted_at,
                input: TaskInput::ChildFinished {
                    wait_id: wait.wait_id,
                    child_task_id: child.task_id,
                    status: delivery.status,
                    output: delivery.output,
                    outcome_unknown: delivery.outcome_unknown,
                    agent: delivery.agent.clone(),
                    blocked: delivery.blocked,
                },
            })
            .await;
        match acceptance {
            Ok(_) => {}
            Err(error) if inbox_full(&error) => {
                tracing::warn!(task_id=%task.definition.initial.id, child_task_id=%child.task_id, "child completion delivery deferred until native input capacity is available");
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(true)
}

pub(super) async fn retire_consumed(
    agent: &Agent,
    store: &Store,
    task: &mut StoredTask,
) -> Result<(), SchedulerError> {
    let Some(wait) = task.checkpoint.child_wait.clone() else {
        return Ok(());
    };
    if wait.active_command_id != task.checkpoint.active_command
        || wait.children.iter().any(|child| child.delivery.is_none())
    {
        return Ok(());
    }
    let conversation = agent
        .conversation(task.definition.initial.conversation_id)
        .await?;
    if conversation.config != task.definition.configuration {
        return Err(SchedulerError::Conflict(task.definition.initial.id));
    }
    let state = conversation.task_input_state();
    if state.wait_id.is_some()
        || state.pending != 0
        || !repository::command_exists(store, wait.wait_id)?
    {
        return Ok(());
    }
    let receipt = agent
        .accept_task_input(TaskInputCommand {
            conversation_id: task.definition.initial.conversation_id,
            task_id: task.definition.initial.id,
            active_command_id: wait.active_command_id,
            delivery_id: wait.wait_id,
            accepted_at: wait.accepted_at,
            input: TaskInput::WaitForChildren {
                children: wait.children.iter().map(|child| child.task_id).collect(),
            },
        })
        .await?;
    if receipt.sequence > state.consumed_sequence {
        return Ok(());
    }
    for child in &wait.children {
        let delivery = child
            .delivery
            .as_ref()
            .ok_or(SchedulerError::Invalid("terminal delivery missing"))?;
        if !repository::command_exists(store, delivery.delivery_id)? {
            return Ok(());
        }
        let receipt = agent
            .accept_task_input(TaskInputCommand {
                conversation_id: task.definition.initial.conversation_id,
                task_id: task.definition.initial.id,
                active_command_id: wait.active_command_id,
                delivery_id: delivery.delivery_id,
                accepted_at: delivery.accepted_at,
                input: TaskInput::ChildFinished {
                    wait_id: wait.wait_id,
                    child_task_id: child.task_id,
                    status: delivery.status,
                    output: delivery.output.clone(),
                    outcome_unknown: delivery.outcome_unknown,
                    agent: delivery.agent.clone(),
                    blocked: delivery.blocked,
                },
            })
            .await?;
        if receipt.sequence > state.consumed_sequence {
            return Ok(());
        }
    }
    task.checkpoint.child_wait = None;
    Ok(())
}

fn same_delivery(saved: &ChildDelivery, current: &ChildDelivery) -> bool {
    saved.delivery_id == current.delivery_id
        && saved.accepted_at == current.accepted_at
        && saved.command_id == current.command_id
        && saved.status == current.status
        && saved.outcome_unknown == current.outcome_unknown
        && saved.agent == current.agent
        && saved.blocked == current.blocked
        && current.output.starts_with(&saved.output)
}

fn inbox_full(error: &crate::agent::AgentError) -> bool {
    matches!(
        error,
        crate::agent::AgentError::ResourceLimit(
            "pending task inputs" | "serialized conversation data"
        )
    )
}

async fn terminal_delivery(
    agent: &Agent,
    store: &Store,
    wait: CommandId,
    task: &StoredTask,
) -> Result<ChildDelivery, SchedulerError> {
    let conversation = agent
        .conversation(task.definition.initial.conversation_id)
        .await?;
    if conversation.config != task.definition.configuration {
        return Err(SchedulerError::Conflict(task.definition.initial.id));
    }
    let native_unknown = conversation.native_run().is_some_and(|run| {
        run.task_id == task.definition.initial.id
            && run.command_id == task.checkpoint.active_command
            && matches!(
                run.status,
                LoopStatus::ProviderInFlight { .. }
                    | LoopStatus::ExecutingTools
                    | LoopStatus::Paused {
                        reason: PauseReason::ReconciliationRequired { .. }
                            | PauseReason::BrokerRejected { .. }
                            | PauseReason::OperationInFlight { .. }
                            | PauseReason::ChildOutcomeUnknown { .. }
                    }
            )
    });
    let journal_unknown: bool = store.connection()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1 AND state IN ('running','unknown'))",
        [task.definition.initial.id.to_string()], |row| row.get(0),
    )?;
    let waiting_unknown = matches!(
        task.checkpoint.waiting,
        Some(
            WaitingReason::ProviderUncertain { .. }
                | WaitingReason::Reconciliation
                | WaitingReason::InFlight
                | WaitingReason::ChildOutcomeUnknown { .. }
        )
    );
    let blocked: bool = store.connection()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1 AND state='denied' AND json_extract(result,'$.result.code')=?2)",
        [
            task.definition.initial.id.to_string(),
            crate::permissions::CHILD_APPROVAL_CODE.to_owned(),
        ],
        |row| row.get(0),
    )?;
    let output = task
        .checkpoint
        .last_output
        .as_deref()
        .or(task.checkpoint.diagnostic.as_deref())
        .unwrap_or("");
    let mut end = output.len().min(8192);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    Ok(ChildDelivery {
        delivery_id: delivery_id(wait, task.definition.initial.id)?,
        accepted_at: task.checkpoint.task.updated_at,
        command_id: task.checkpoint.active_command,
        status: task.checkpoint.task.status,
        output: output[..end].to_owned(),
        outcome_unknown: native_unknown || journal_unknown || waiting_unknown,
        agent: task
            .definition
            .delegation
            .as_ref()
            .map(|delegation| delegation.name.as_str().to_owned()),
        blocked,
    })
}

fn delivery_id(wait: CommandId, child: TaskId) -> Result<CommandId, SchedulerError> {
    let mut hash = Sha256::new();
    hash.update(b"everything.scheduler.child-finished.v1\0");
    hash.update(wait.as_uuid().as_bytes());
    hash.update(child.as_uuid().as_bytes());
    let hash = hash.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(CommandId::try_from(uuid::Uuid::from_bytes(bytes))?)
}
