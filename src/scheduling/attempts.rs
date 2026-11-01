use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::agent::LoopLimits;
use crate::context::ContextBudget;
use crate::host::agents::AgentDefinition;
use crate::protocol::{CommandId, ConversationId, TaskId, UnixMillis};
use crate::scheduler::{ExecutionPolicy, SCHEDULER_TOOL_NAMES};
use crate::tools::ToolRegistry;

use super::{
    OccurrenceId, RunState, ScheduleError, ScheduleRepository, ScheduleResult, ScheduledRun,
    timestamp,
};

pub(super) const MAX_POLICY_BYTES: usize = 32_768;
pub(super) const MAX_ATTEMPT_BYTES: i64 = 64 * 1_048_576;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledAttempt {
    pub occurrence_id: OccurrenceId,
    pub attempt_number: u32,
    pub task_id: TaskId,
    pub conversation_id: ConversationId,
    pub create_command_id: CommandId,
    pub input_command_id: CommandId,
    pub accepted_at: UnixMillis,
    pub deadline: UnixMillis,
    pub execution: ExecutionPolicy,
}

impl ScheduleRepository {
    pub fn first_attempt(
        &self,
        occurrence: OccurrenceId,
    ) -> ScheduleResult<Option<ScheduledAttempt>> {
        let connection = self.store.connection()?;
        read_attempt(&connection, occurrence)
    }

    pub fn reserve_first_attempt(
        &self,
        occurrence: OccurrenceId,
        execution: &ExecutionPolicy,
    ) -> ScheduleResult<ScheduledAttempt> {
        execution
            .validate()
            .map_err(|_| ScheduleError::Invalid("invalid scheduled native policy"))?;
        let encoded = serde_json::to_string(execution)?;
        if encoded.len() > MAX_POLICY_BYTES {
            return Err(ScheduleError::Capacity("attempt execution policy"));
        }
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(existing) = read_attempt(&transaction, occurrence)? {
            if serde_json::to_string(&existing.execution)? != encoded {
                return Err(ScheduleError::OccurrenceConflict(occurrence));
            }
            return Ok(existing);
        }
        let run = super::store::required_run(&transaction, occurrence)?;
        if run.state != RunState::Claimed {
            return Err(ScheduleError::RunStateConflict(occurrence));
        }
        let deadline = occurrence_deadline(&run)?;
        validate_policy(&run, execution)?;
        let used: i64 = transaction.query_row(
            "SELECT attempt_bytes FROM scheduling_usage WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if used < 0
            || used
                .checked_add(encoded.len() as i64)
                .is_none_or(|total| total > MAX_ATTEMPT_BYTES)
        {
            return Err(ScheduleError::Capacity("attempt execution policies"));
        }
        let attempt = ScheduledAttempt {
            occurrence_id: occurrence,
            attempt_number: 1,
            task_id: run.initial_task_id,
            conversation_id: ConversationId::new(),
            create_command_id: CommandId::new(),
            input_command_id: CommandId::new(),
            accepted_at: run.claimed_at,
            deadline,
            execution: execution.clone(),
        };
        transaction.execute("INSERT INTO scheduled_attempts(occurrence_id,attempt_number,task_id,conversation_id,create_command_id,input_command_id,accepted_at,deadline,execution) VALUES(?1,1,?2,?3,?4,?5,?6,?7,?8)",params![occurrence.to_string(),attempt.task_id.to_string(),attempt.conversation_id.to_string(),attempt.create_command_id.to_string(),attempt.input_command_id.to_string(),timestamp(attempt.accepted_at)?,timestamp(deadline)?,encoded])?;
        transaction.commit()?;
        Ok(attempt)
    }
}

pub fn occurrence_deadline(run: &ScheduledRun) -> ScheduleResult<UnixMillis> {
    let value = run
        .claimed_at
        .get()
        .checked_add(run.definition.target.budget.timeout_ms)
        .ok_or(ScheduleError::Invalid("occurrence deadline overflow"))?;
    let value = UnixMillis::try_from(value)
        .map_err(|_| ScheduleError::Invalid("occurrence deadline overflow"))?;
    timestamp(value)?;
    Ok(value)
}

pub(super) fn capture_policy(
    run: &ScheduledRun,
    registry: &ToolRegistry,
    agent: &AgentDefinition,
) -> ScheduleResult<ExecutionPolicy> {
    let target = &run.definition.target;
    let maximum = target.configuration.max_output_tokens;
    let requests = u32::from(target.budget.max_model_requests);
    let context = 128_000_u64.max(u64::from(maximum) + 2048);
    let allowed: BTreeSet<String> = target
        .permissions
        .allowed_tools
        .iter()
        .filter(|name| {
            !SCHEDULER_TOOL_NAMES.contains(&name.as_str())
                && !super::controls::SCHEDULE_CONTROL_NAMES.contains(&name.as_str())
        })
        .cloned()
        .collect();
    let tools = target.budget.max_tool_calls > 0 && !allowed.is_empty();
    let limits = LoopLimits {
        max_model_attempts: requests,
        max_tool_calls: u32::from(target.budget.max_tool_calls),
        max_calls_per_round: 32,
        max_parallel_calls: 8,
        max_selected_tools: 32,
        max_charged_tokens: context
            .checked_mul(u64::from(requests))
            .ok_or(ScheduleError::Invalid("attempt token ceiling overflow"))?,
        max_reserved_output_tokens: Some(u64::from(target.budget.max_output_tokens)),
        max_duration_ms: target.budget.timeout_ms,
        absolute_deadline: Some(occurrence_deadline(run)?),
    };
    let policy = ExecutionPolicy {
        workspace_root: target.workspace_root.clone(),
        mode: target.permissions.mode,
        tools_enabled: tools,
        allowed_tools: Some(allowed),
        budget: ContextBudget {
            max_context_tokens: context,
            reserved_output_tokens: maximum,
            safety_margin_tokens: 1024,
            max_request_bytes: 32 * 1_048_576,
            max_compaction_bytes: 16_384,
        },
        limits,
        agent: None,
    };
    let policy = agent
        .narrow(policy, registry)
        .capture(registry)
        .map_err(|_| ScheduleError::Invalid("scheduled native policy could not be captured"))?;
    validate_policy(run, &policy)?;
    Ok(policy)
}

pub(super) fn validate_policy(run: &ScheduledRun, policy: &ExecutionPolicy) -> ScheduleResult<()> {
    policy
        .validate()
        .map_err(|_| ScheduleError::Invalid("invalid scheduled native policy"))?;
    let target = &run.definition.target;
    let allowed = policy
        .allowed_tools
        .as_ref()
        .ok_or(ScheduleError::Invalid("scheduled tools must be captured"))?;
    let max_requests = if policy.limits.max_reserved_output_tokens.is_some() {
        u32::from(target.budget.max_model_requests)
    } else {
        u32::from(target.budget.max_model_requests)
            .min(target.budget.max_output_tokens / target.configuration.max_output_tokens)
    };
    if policy.workspace_root != target.workspace_root
        || policy.mode > target.permissions.mode
        || !allowed.is_subset(&target.permissions.allowed_tools)
        || allowed.iter().any(|name| {
            SCHEDULER_TOOL_NAMES.contains(&name.as_str())
                || super::controls::SCHEDULE_CONTROL_NAMES.contains(&name.as_str())
        })
        || policy
            .limits
            .max_reserved_output_tokens
            .is_some_and(|cap| cap > u64::from(target.budget.max_output_tokens))
        || policy.limits.max_model_attempts > max_requests
        || policy.limits.max_tool_calls > u32::from(target.budget.max_tool_calls)
        || policy.limits.max_duration_ms > target.budget.timeout_ms
        || policy.limits.absolute_deadline != Some(occurrence_deadline(run)?)
        || policy.budget.reserved_output_tokens != target.configuration.max_output_tokens
    {
        return Err(ScheduleError::Invalid(
            "scheduled native policy exceeds the captured authority or budget",
        ));
    }
    Ok(())
}

fn read_attempt(
    connection: &rusqlite::Connection,
    occurrence: OccurrenceId,
) -> ScheduleResult<Option<ScheduledAttempt>> {
    read_attempt_number(connection, occurrence, 1)
}

pub(super) fn read_attempt_number(
    connection: &rusqlite::Connection,
    occurrence: OccurrenceId,
    attempt_number: u32,
) -> ScheduleResult<Option<ScheduledAttempt>> {
    let value=connection.query_row("SELECT attempt_number,task_id,conversation_id,create_command_id,input_command_id,accepted_at,deadline,execution,length(CAST(execution AS BLOB)),length(CAST(task_id AS BLOB)),length(CAST(conversation_id AS BLOB)),length(CAST(create_command_id AS BLOB)),length(CAST(input_command_id AS BLOB)) FROM scheduled_attempts WHERE occurrence_id=?1 AND attempt_number=?2",rusqlite::params![occurrence.to_string(),attempt_number],|row| {
        let size:usize=row.get(8)?;
        if size>MAX_POLICY_BYTES {return Err(rusqlite::Error::InvalidQuery);}
        for column in 9..=12 {let size:usize=row.get(column)?;if size!=36 {return Err(rusqlite::Error::InvalidQuery);}}
        Ok((row.get::<_,u32>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,u64>(5)?,row.get::<_,u64>(6)?,row.get::<_,String>(7)?))
    }).optional()?;
    value
        .map(|row| {
            let attempt = ScheduledAttempt {
                occurrence_id: occurrence,
                attempt_number: row.0,
                task_id: row
                    .1
                    .parse()
                    .map_err(|_| ScheduleError::Invalid("invalid attempt task identity"))?,
                conversation_id: row
                    .2
                    .parse()
                    .map_err(|_| ScheduleError::Invalid("invalid attempt conversation identity"))?,
                create_command_id: row
                    .3
                    .parse()
                    .map_err(|_| ScheduleError::Invalid("invalid attempt create identity"))?,
                input_command_id: row
                    .4
                    .parse()
                    .map_err(|_| ScheduleError::Invalid("invalid attempt input identity"))?,
                accepted_at: UnixMillis::try_from(row.5)
                    .map_err(|_| ScheduleError::Invalid("invalid attempt time"))?,
                deadline: UnixMillis::try_from(row.6)
                    .map_err(|_| ScheduleError::Invalid("invalid attempt deadline"))?,
                execution: serde_json::from_str(&row.7)?,
            };
            let run = super::store::required_run(connection, occurrence)?;
            if !(1..=u32::from(run.definition.policies.retry.max_attempts))
                .contains(&attempt.attempt_number)
                || attempt.attempt_number == 1
                    && (attempt.task_id != run.initial_task_id
                        || attempt.accepted_at != run.claimed_at)
                || attempt.accepted_at < run.claimed_at
                || attempt.accepted_at > attempt.deadline
                || attempt.deadline != occurrence_deadline(&run)?
            {
                return Err(ScheduleError::Invalid(
                    "attempt identity differs from immutable occurrence",
                ));
            }
            validate_policy(&run, &attempt.execution)?;
            Ok(attempt)
        })
        .transpose()
}
