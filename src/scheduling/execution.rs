use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::NativeRunInfo;
use crate::protocol::{CommandId, ConversationId, TaskId, UnixMillis};

use super::attempts::{
    ScheduledAttempt, occurrence_deadline, read_attempt_number, validate_policy,
};
use super::{
    OccurrenceId, RunOutcome, RunState, ScheduleError, ScheduleRepository, ScheduleResult,
    ScheduledRun, timestamp,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptUsage {
    pub model_requests: u32,
    pub tool_calls: u32,
    pub reserved_output_tokens: u64,
    pub known: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptResultKind {
    FactoryFailure,
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledAttemptResult {
    pub occurrence_id: OccurrenceId,
    pub attempt_number: u32,
    pub kind: AttemptResultKind,
    pub settled_at: UnixMillis,
    pub usage: AttemptUsage,
    pub diagnostic: String,
    pub outcome_digest: String,
    pub next_retry_at: Option<UnixMillis>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FactoryRetirementIntent {
    pub command_id: CommandId,
    pub accepted_at: UnixMillis,
}

pub(super) fn preview(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].replace('\0', " ")
}

pub(super) fn outcome_digest(id: OccurrenceId, outcome: &RunOutcome) -> ScheduleResult<String> {
    outcome.validate()?;
    let encoded = serde_json::to_vec(outcome)?;
    if encoded.len() > super::MAX_RUN_OUTCOME_BYTES {
        return Err(ScheduleError::Capacity("scheduled outcome bytes"));
    }
    let mut hash = Sha256::new();
    hash.update(b"everything.scheduling.outcome.v1\0");
    hash.update(id.as_uuid().as_bytes());
    hash.update(encoded);
    Ok(format!("{:x}", hash.finalize()))
}

fn latest_in(
    connection: &Connection,
    id: OccurrenceId,
) -> ScheduleResult<Option<ScheduledAttempt>> {
    let number: Option<u32> = connection.query_row(
        "SELECT max(attempt_number) FROM scheduled_attempts WHERE occurrence_id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    number
        .map(|number| {
            read_attempt_number(connection, id, number)?.ok_or(ScheduleError::RunStateConflict(id))
        })
        .transpose()
}

fn result_in(
    connection: &Connection,
    id: OccurrenceId,
    number: u32,
) -> ScheduleResult<Option<ScheduledAttemptResult>> {
    let row=connection.query_row("SELECT kind,settled_at,model_requests,tool_calls,reserved_output_tokens,usage_known,diagnostic,outcome_digest,next_retry_at,length(CAST(diagnostic AS BLOB)),length(outcome_digest),length(kind) FROM scheduled_attempt_results WHERE occurrence_id=?1 AND attempt_number=?2",params![id.to_string(),number],|row|{
        if row.get::<_,usize>(9)?>1024 || row.get::<_,usize>(10)?!=64 || row.get::<_,usize>(11)?>16 {return Err(rusqlite::Error::InvalidQuery);}
        Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?,row.get::<_,u32>(2)?,row.get::<_,u32>(3)?,row.get::<_,u64>(4)?,row.get::<_,u8>(5)?,row.get::<_,String>(6)?,row.get::<_,String>(7)?,row.get::<_,Option<u64>>(8)?))
    }).optional()?;
    row.map(|row| {
        let kind = match row.0.as_str() {
            "factory_failure" => AttemptResultKind::FactoryFailure,
            "terminal" => AttemptResultKind::Terminal,
            _ => return Err(ScheduleError::Invalid("invalid attempt result kind")),
        };
        if row.5 > 1
            || row.2 > 128
            || row.3 > 1024
            || row.4 > 262144
            || !row
                .7
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ScheduleError::Invalid("invalid attempt accounting"));
        }
        Ok(ScheduledAttemptResult {
            occurrence_id: id,
            attempt_number: number,
            kind,
            settled_at: UnixMillis::try_from(row.1)
                .map_err(|_| ScheduleError::Invalid("invalid attempt settlement time"))?,
            usage: AttemptUsage {
                model_requests: row.2,
                tool_calls: row.3,
                reserved_output_tokens: row.4,
                known: row.5 == 1,
            },
            diagnostic: row.6,
            outcome_digest: row.7,
            next_retry_at: row
                .8
                .map(UnixMillis::try_from)
                .transpose()
                .map_err(|_| ScheduleError::Invalid("invalid retry timestamp"))?,
        })
    })
    .transpose()
}

fn totals_in(connection: &Connection, id: OccurrenceId) -> ScheduleResult<(u32, AttemptUsage)> {
    let values:(u32,u32,u32,u64,u32)=connection.query_row("SELECT count(*),COALESCE(sum(model_requests),0),COALESCE(sum(tool_calls),0),COALESCE(sum(reserved_output_tokens),0),COALESCE(sum(usage_known=0),0) FROM scheduled_attempt_results WHERE occurrence_id=?1",[id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
    if values.0 > 8 {
        return Err(ScheduleError::Invalid("too many scheduled attempts"));
    }
    Ok((
        values.0,
        AttemptUsage {
            model_requests: values.1,
            tool_calls: values.2,
            reserved_output_tokens: values.3,
            known: values.4 == 0,
        },
    ))
}

fn ensure_budget(run: &ScheduledRun, usage: &AttemptUsage) -> ScheduleResult<()> {
    let budget = &run.definition.target.budget;
    if usage.model_requests > u32::from(budget.max_model_requests)
        || usage.tool_calls > u32::from(budget.max_tool_calls)
        || usage.reserved_output_tokens > u64::from(budget.max_output_tokens)
    {
        return Err(ScheduleError::Invalid(
            "cumulative scheduled accounting exceeds captured budget",
        ));
    }
    Ok(())
}

impl ScheduleRepository {
    pub fn latest_attempt(&self, id: OccurrenceId) -> ScheduleResult<Option<ScheduledAttempt>> {
        latest_in(&*self.store.connection()?, id)
    }
    pub fn attempt_result(
        &self,
        id: OccurrenceId,
        number: u32,
    ) -> ScheduleResult<Option<ScheduledAttemptResult>> {
        result_in(&*self.store.connection()?, id, number)
    }
    pub fn attempt_history(&self, id: OccurrenceId) -> ScheduleResult<Vec<ScheduledAttempt>> {
        let connection = self.store.connection()?;
        let latest = latest_in(&connection, id)?;
        let Some(latest) = latest else {
            return Ok(Vec::new());
        };
        (1..=latest.attempt_number)
            .map(|number| {
                read_attempt_number(&connection, id, number)?.ok_or(ScheduleError::Invalid(
                    "scheduled attempt history has a gap",
                ))
            })
            .collect()
    }
    pub fn cumulative_usage(&self, id: OccurrenceId) -> ScheduleResult<AttemptUsage> {
        let connection = self.store.connection()?;
        let run = super::store::required_run(&connection, id)?;
        let (_, mut usage) = totals_in(&connection, id)?;
        if let Some(latest) = latest_in(&connection, id)? {
            for number in 1..=latest.attempt_number {
                if result_in(&connection, id, number)?.is_none() {
                    let attempt = read_attempt_number(&connection, id, number)?
                        .ok_or(ScheduleError::Invalid("scheduled accounting has a gap"))?;
                    let unknown = native_usage(&attempt, None)?;
                    usage.model_requests = usage
                        .model_requests
                        .checked_add(unknown.model_requests)
                        .ok_or(ScheduleError::Invalid("model counter overflow"))?;
                    usage.tool_calls = usage
                        .tool_calls
                        .checked_add(unknown.tool_calls)
                        .ok_or(ScheduleError::Invalid("tool counter overflow"))?;
                    usage.reserved_output_tokens = usage
                        .reserved_output_tokens
                        .checked_add(unknown.reserved_output_tokens)
                        .ok_or(ScheduleError::Invalid("output counter overflow"))?;
                    usage.known = false;
                }
            }
        }
        ensure_budget(&run, &usage)?;
        Ok(usage)
    }
    pub(super) fn terminal_journal_unknown(&self, task: TaskId) -> ScheduleResult<bool> {
        Ok(self.store.connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE task_id=?1 AND state IN ('running','unknown'))",
            [task.to_string()],
            |row| row.get(0),
        )?)
    }
    pub(super) fn execution_ready(&self) -> ScheduleResult<()> {
        self.store.connection()?.query_row(
            "SELECT attempts FROM scheduling_execution_usage WHERE singleton=1",
            [],
            |row| row.get::<_, u32>(0),
        )?;
        Ok(())
    }
    pub(super) fn attempt_admitted(&self, id: OccurrenceId, number: u32) -> ScheduleResult<bool> {
        Ok(self.store.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM scheduled_attempt_admissions WHERE occurrence_id=?1 AND attempt_number=?2)",params![id.to_string(),number],|row|row.get(0))?)
    }
    pub(super) fn mark_attempt_admitted(
        &self,
        attempt: &ScheduledAttempt,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledRun> {
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let run = super::store::required_run(&transaction, attempt.occurrence_id)?;
        if run.state.is_terminal() {
            return Err(ScheduleError::RunStateConflict(run.occurrence_id));
        }
        let saved = read_attempt_number(&transaction, run.occurrence_id, attempt.attempt_number)?
            .ok_or(ScheduleError::RunStateConflict(run.occurrence_id))?;
        if serde_json::to_vec(&saved)? != serde_json::to_vec(attempt)? {
            return Err(ScheduleError::OccurrenceConflict(run.occurrence_id));
        }
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND conversation_id=?2)",
            params![
                attempt.task_id.to_string(),
                attempt.conversation_id.to_string()
            ],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(ScheduleError::RunStateConflict(run.occurrence_id));
        }
        let at = now.max(run.updated_at);
        transaction.execute("INSERT INTO scheduled_attempt_admissions(occurrence_id,attempt_number,admitted_at) VALUES(?1,?2,?3) ON CONFLICT(occurrence_id,attempt_number) DO NOTHING",params![run.occurrence_id.to_string(),attempt.attempt_number,timestamp(at)?])?;
        if run.state == RunState::Claimed {
            let changed=transaction.execute("UPDATE scheduled_runs SET state='running',started_at=?2,updated_at=?2,revision=revision+1 WHERE occurrence_id=?1 AND state='claimed' AND revision<9223372036854775807",params![run.occurrence_id.to_string(),timestamp(at)?])?;
            if changed != 1 {
                return Err(ScheduleError::Invalid("scheduled run revision exhausted"));
            }
        }
        let run = super::store::required_run(&transaction, run.occurrence_id)?;
        transaction.commit()?;
        Ok(run)
    }
    pub(super) fn factory_retirement(
        &self,
        id: OccurrenceId,
        number: u32,
    ) -> ScheduleResult<Option<FactoryRetirementIntent>> {
        self.store.connection()?.query_row("SELECT command_id,accepted_at,length(command_id) FROM scheduled_factory_retirements WHERE occurrence_id=?1 AND attempt_number=?2",params![id.to_string(),number],|row|{
            if row.get::<_,usize>(2)?!=36 {return Err(rusqlite::Error::InvalidQuery);}Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?))
        }).optional()?.map(|row|{
            let at=UnixMillis::try_from(row.1).map_err(|_|ScheduleError::Invalid("invalid factory intent timestamp"))?;timestamp(at)?;
            Ok(FactoryRetirementIntent {command_id:row.0.parse().map_err(|_|ScheduleError::Invalid("invalid factory intent identity"))?,accepted_at:at})
        }).transpose()
    }
    pub(super) fn reserve_factory_retirement(
        &self,
        attempt: &ScheduledAttempt,
        now: UnixMillis,
    ) -> ScheduleResult<FactoryRetirementIntent> {
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row=transaction.query_row("SELECT command_id,accepted_at,length(command_id) FROM scheduled_factory_retirements WHERE occurrence_id=?1 AND attempt_number=?2",params![attempt.occurrence_id.to_string(),attempt.attempt_number],|row|{
            if row.get::<_,usize>(2)?!=36 {return Err(rusqlite::Error::InvalidQuery);}Ok((row.get::<_,String>(0)?,row.get::<_,u64>(1)?))
        }).optional()?;
        if let Some(row) = row {
            return Ok(FactoryRetirementIntent {
                command_id: row
                    .0
                    .parse()
                    .map_err(|_| ScheduleError::Invalid("invalid retirement identity"))?,
                accepted_at: UnixMillis::try_from(row.1)
                    .map_err(|_| ScheduleError::Invalid("invalid retirement time"))?,
            });
        }
        let run = super::store::required_run(&transaction, attempt.occurrence_id)?;
        if run.state.is_terminal()
            || latest_in(&transaction, run.occurrence_id)?
                .is_none_or(|saved| saved.task_id != attempt.task_id)
            || result_in(&transaction, run.occurrence_id, attempt.attempt_number)?.is_some()
        {
            return Err(ScheduleError::RunStateConflict(run.occurrence_id));
        }
        let intent = FactoryRetirementIntent {
            command_id: CommandId::new(),
            accepted_at: now,
        };
        transaction.execute("INSERT INTO scheduled_factory_retirements(occurrence_id,attempt_number,command_id,accepted_at) VALUES(?1,?2,?3,?4)",params![attempt.occurrence_id.to_string(),attempt.attempt_number,intent.command_id.to_string(),timestamp(now)?])?;
        transaction.commit()?;
        Ok(intent)
    }
    pub(super) fn reserve_retry(
        &self,
        id: OccurrenceId,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledAttempt> {
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let run = super::store::required_run(&transaction, id)?;
        if run.state.is_terminal() {
            return Err(ScheduleError::RunStateConflict(id));
        }
        let latest = latest_in(&transaction, id)?.ok_or(ScheduleError::RunStateConflict(id))?;
        let Some(result) = result_in(&transaction, id, latest.attempt_number)? else {
            return Ok(latest);
        };
        let deadline = occurrence_deadline(&run)?;
        if result.kind != AttemptResultKind::FactoryFailure
            || result.next_retry_at.is_none_or(|at| now < at)
            || now < result.settled_at
            || now >= deadline
            || latest.attempt_number >= u32::from(run.definition.policies.retry.max_attempts)
        {
            return Err(ScheduleError::RunStateConflict(id));
        }
        let (count, usage) = totals_in(&transaction, id)?;
        if count != latest.attempt_number || !usage.known {
            return Err(ScheduleError::Invalid("incomplete retry accounting"));
        }
        ensure_budget(&run, &usage)?;
        let first =
            read_attempt_number(&transaction, id, 1)?.ok_or(ScheduleError::RunStateConflict(id))?;
        let mut policy = first.execution;
        let budget = &run.definition.target.budget;
        policy.limits.max_model_attempts =
            u32::from(budget.max_model_requests) - usage.model_requests;
        policy.limits.max_tool_calls = if policy.tools_enabled {
            policy
                .limits
                .max_tool_calls
                .min(u32::from(budget.max_tool_calls) - usage.tool_calls)
        } else {
            0
        };
        let output = u64::from(budget.max_output_tokens) - usage.reserved_output_tokens;
        if policy.limits.max_model_attempts == 0
            || output < u64::from(policy.budget.reserved_output_tokens)
        {
            return Err(ScheduleError::Capacity(
                "remaining scheduled inference budget",
            ));
        }
        policy.limits.max_reserved_output_tokens = Some(output);
        policy.limits.max_charged_tokens = policy
            .budget
            .max_context_tokens
            .checked_mul(u64::from(policy.limits.max_model_attempts))
            .ok_or(ScheduleError::Invalid("retry token ceiling overflow"))?;
        validate_policy(&run, &policy)?;
        let encoded = serde_json::to_string(&policy)?;
        let used: i64 = transaction.query_row(
            "SELECT attempt_bytes FROM scheduling_usage WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if encoded.len() > super::attempts::MAX_POLICY_BYTES
            || used < 0
            || used
                .checked_add(encoded.len() as i64)
                .is_none_or(|total| total > super::attempts::MAX_ATTEMPT_BYTES)
        {
            return Err(ScheduleError::Capacity("attempt execution policies"));
        }
        let attempt = ScheduledAttempt {
            occurrence_id: id,
            attempt_number: latest.attempt_number + 1,
            task_id: TaskId::new(),
            conversation_id: ConversationId::new(),
            create_command_id: CommandId::new(),
            input_command_id: CommandId::new(),
            accepted_at: now,
            deadline,
            execution: policy,
        };
        transaction.execute("INSERT INTO scheduled_attempts(occurrence_id,attempt_number,task_id,conversation_id,create_command_id,input_command_id,accepted_at,deadline,execution) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id.to_string(),attempt.attempt_number,attempt.task_id.to_string(),attempt.conversation_id.to_string(),attempt.create_command_id.to_string(),attempt.input_command_id.to_string(),timestamp(now)?,timestamp(deadline)?,encoded])?;
        transaction.commit()?;
        Ok(attempt)
    }

    pub(super) fn settle_attempt(
        &self,
        attempt: &ScheduledAttempt,
        kind: AttemptResultKind,
        usage: AttemptUsage,
        outcome: &RunOutcome,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledAttemptResult> {
        let digest = outcome_digest(attempt.occurrence_id, outcome)?;
        let diagnostic = match outcome {
            RunOutcome::Failed { message, .. } => preview(message, 1024),
            _ => String::new(),
        };
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(existing) =
            result_in(&transaction, attempt.occurrence_id, attempt.attempt_number)?
        {
            if existing.kind != kind
                || existing.usage != usage
                || existing.outcome_digest != digest
                || existing.diagnostic != diagnostic
            {
                return Err(ScheduleError::OccurrenceConflict(attempt.occurrence_id));
            }
            return Ok(existing);
        }
        let run = super::store::required_run(&transaction, attempt.occurrence_id)?;
        if run.state.is_terminal()
            || latest_in(&transaction, run.occurrence_id)?
                .is_none_or(|saved| saved.task_id != attempt.task_id)
        {
            return Err(ScheduleError::RunStateConflict(run.occurrence_id));
        }
        let (count, prior) = totals_in(&transaction, run.occurrence_id)?;
        if count + 1 != attempt.attempt_number {
            return Err(ScheduleError::Invalid("attempt settlement gap"));
        }
        let total = AttemptUsage {
            model_requests: prior
                .model_requests
                .checked_add(usage.model_requests)
                .ok_or(ScheduleError::Invalid("model counter overflow"))?,
            tool_calls: prior
                .tool_calls
                .checked_add(usage.tool_calls)
                .ok_or(ScheduleError::Invalid("tool counter overflow"))?,
            reserved_output_tokens: prior
                .reserved_output_tokens
                .checked_add(usage.reserved_output_tokens)
                .ok_or(ScheduleError::Invalid("output counter overflow"))?,
            known: prior.known && usage.known,
        };
        ensure_budget(&run, &total)?;
        if kind == AttemptResultKind::FactoryFailure
            && (usage
                != (AttemptUsage {
                    known: true,
                    ..AttemptUsage::default()
                })
                || !matches!(outcome, RunOutcome::Failed { .. }))
        {
            return Err(ScheduleError::Invalid(
                "factory retry requires proven zero native effects",
            ));
        }
        let retry = &run.definition.policies.retry;
        let delay = retry
            .initial_backoff_ms
            .checked_mul(1_u64 << attempt.attempt_number.saturating_sub(1))
            .ok_or(ScheduleError::Invalid("retry backoff overflow"))?
            .min(retry.max_backoff_ms);
        let next = now
            .get()
            .checked_add(delay)
            .filter(|at| {
                kind == AttemptResultKind::FactoryFailure
                    && attempt.attempt_number < u32::from(retry.max_attempts)
                    && now >= attempt.accepted_at
                    && *at < attempt.deadline.get()
            })
            .map(UnixMillis::try_from)
            .transpose()
            .map_err(|_| ScheduleError::Invalid("retry timestamp overflow"))?;
        let settled = now.max(attempt.accepted_at);
        transaction.execute("INSERT INTO scheduled_attempt_results(occurrence_id,attempt_number,kind,settled_at,model_requests,tool_calls,reserved_output_tokens,usage_known,diagnostic,outcome_digest,next_retry_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![attempt.occurrence_id.to_string(),attempt.attempt_number,if kind==AttemptResultKind::FactoryFailure {"factory_failure"} else {"terminal"},timestamp(settled)?,usage.model_requests,usage.tool_calls,usage.reserved_output_tokens,usage.known,diagnostic,digest,next.map(timestamp).transpose()?])?;
        if next.is_none() {
            let finished = super::store::finish_run_in(
                &transaction,
                run.occurrence_id,
                run.revision,
                outcome,
                now.max(run.updated_at),
            )?;
            super::delivery::ready_in(&transaction, &finished, now)?;
        }
        let result = result_in(&transaction, attempt.occurrence_id, attempt.attempt_number)?
            .ok_or(ScheduleError::RunStateConflict(attempt.occurrence_id))?;
        transaction.commit()?;
        Ok(result)
    }
}

pub(super) fn native_usage(
    attempt: &ScheduledAttempt,
    native: Option<&NativeRunInfo>,
) -> ScheduleResult<AttemptUsage> {
    let policy = &attempt.execution;
    let maximum_output = policy.limits.max_reserved_output_tokens.unwrap_or(
        u64::from(policy.limits.max_model_attempts)
            * u64::from(policy.budget.reserved_output_tokens),
    );
    let Some(native) = native else {
        return Ok(AttemptUsage {
            model_requests: policy.limits.max_model_attempts,
            tool_calls: policy.limits.max_tool_calls,
            reserved_output_tokens: maximum_output,
            known: false,
        });
    };
    if native.task_id != attempt.task_id
        || native.command_id != attempt.input_command_id
        || native.workspace_root != policy.workspace_root
        || native.mode != policy.mode
        || native
            .allowed_tools
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            != *policy
                .allowed_tools
                .as_ref()
                .ok_or(ScheduleError::Invalid("uncaptured scheduled allowlist"))?
        || native.deadline_ms > attempt.deadline.get()
        || native.model_attempts > policy.limits.max_model_attempts
        || native.tool_calls > policy.limits.max_tool_calls
        || native
            .reserved_output_tokens
            .is_some_and(|tokens| tokens > maximum_output)
    {
        return Err(ScheduleError::Invalid(
            "native attempt accounting differs from captured policy",
        ));
    }
    Ok(AttemptUsage {
        model_requests: native.model_attempts,
        tool_calls: native.tool_calls,
        reserved_output_tokens: native.reserved_output_tokens.unwrap_or(maximum_output),
        known: native.reserved_output_tokens.is_some(),
    })
}
