use super::*;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

const JOB_COLUMNS: &str = "id,name,definition,generation,revision,enabled,next_run_at,last_run_at,created_at,updated_at,deleted_at";
const RUN_COLUMNS: &str = "occurrence_id,job_id,generation,definition,scheduled_for,trigger,manual_command_id,initial_task_id,state,revision,claimed_at,started_at,finished_at,updated_at,outcome";
const MAX_JOBS: i64 = 4_096;
const MAX_JOB_BYTES: i64 = 64 * 1_048_576;
const MAX_RUNS: i64 = 100_000;
const MAX_HISTORY_BYTES: i64 = 256 * 1_048_576;

#[derive(Clone)]
pub struct ScheduleRepository {
    pub(super) store: crate::storage::Store,
}

impl ScheduleRepository {
    pub fn new(store: crate::storage::Store) -> Self {
        Self { store }
    }

    pub fn create(
        &self,
        input: &NewScheduledJob,
        authority: &ExecutionAuthority,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledJob> {
        let definition = validate_input(input, authority)?;
        let now = timestamp(now)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read_job(&transaction, input.id)? {
            return Err(ScheduleError::RevisionConflict {
                expected: 0,
                actual: existing.revision,
            });
        }
        check_job_capacity(&transaction, 0, definition.len(), true)?;
        check_delivery(&transaction, &input.definition.target.delivery)?;
        transaction.execute(
            "INSERT INTO scheduled_jobs(id,name,definition,generation,revision,enabled,next_run_at,created_at,updated_at) VALUES (?1,?2,?3,1,1,?4,?5,?6,?6)",
            params![input.id.to_string(),input.name,definition,input.enabled,optional_time(input.next_run_at)?,now],
        )?;
        let created = required_job(&transaction, input.id)?;
        transaction.commit()?;
        Ok(created)
    }

    pub fn job(&self, id: ScheduledJobId) -> ScheduleResult<Option<ScheduledJob>> {
        let connection = self.store.connection()?;
        read_job(&connection, id)
    }

    pub fn edit(
        &self,
        edit: &ScheduledJobEdit,
        authority: &ExecutionAuthority,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledJob> {
        let definition = validate_input(&edit.job, authority)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = required_job(&transaction, edit.job.id)?;
        check_revision(existing.revision, edit.expected_revision)?;
        check_time(now, existing.updated_at)?;
        if existing.deleted_at.is_some() {
            return Err(ScheduleError::Invalid("deleted jobs cannot be edited"));
        }
        let generation = if existing.definition != edit.job.definition {
            increment(existing.generation)?
        } else {
            existing.generation
        };
        let original = encode(&existing.definition, MAX_SCHEDULE_DEFINITION_BYTES)?;
        check_job_capacity(&transaction, original.len(), definition.len(), false)?;
        check_delivery(&transaction, &edit.job.definition.target.delivery)?;
        transaction.execute(
            "UPDATE scheduled_jobs SET name=?2,definition=?3,generation=?4,revision=?5,enabled=?6,next_run_at=?7,updated_at=?8 WHERE id=?1",
            params![edit.job.id.to_string(),edit.job.name,definition,generation,increment(existing.revision)?,edit.job.enabled,optional_time(edit.job.next_run_at)?,timestamp(now)?],
        )?;
        let updated = required_job(&transaction, edit.job.id)?;
        transaction.commit()?;
        Ok(updated)
    }

    pub fn set_enabled(
        &self,
        id: ScheduledJobId,
        expected_revision: u64,
        enabled: bool,
        next_run_at: Option<UnixMillis>,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledJob> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = required_job(&transaction, id)?;
        check_revision(existing.revision, expected_revision)?;
        check_time(now, existing.updated_at)?;
        if existing.deleted_at.is_some() || (enabled && next_run_at.is_none()) {
            return Err(ScheduleError::Invalid(
                "enabling requires an undeleted job and a next run",
            ));
        }
        existing.definition.schedule.validate_next(next_run_at)?;
        transaction.execute(
            "UPDATE scheduled_jobs SET enabled=?2,next_run_at=?3,revision=?4,updated_at=?5 WHERE id=?1",
            params![id.to_string(),enabled,optional_time(next_run_at)?,increment(existing.revision)?,timestamp(now)?],
        )?;
        let updated = required_job(&transaction, id)?;
        transaction.commit()?;
        Ok(updated)
    }

    pub fn delete(
        &self,
        id: ScheduledJobId,
        expected_revision: u64,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledJob> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = required_job(&transaction, id)?;
        check_revision(existing.revision, expected_revision)?;
        check_time(now, existing.updated_at)?;
        if existing.deleted_at.is_some() {
            return Err(ScheduleError::Invalid("job is already deleted"));
        }
        transaction.execute(
            "UPDATE scheduled_jobs SET enabled=0,next_run_at=NULL,deleted_at=?2,updated_at=?2,revision=?3 WHERE id=?1",
            params![id.to_string(),timestamp(now)?,increment(existing.revision)?],
        )?;
        let deleted = required_job(&transaction, id)?;
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn jobs(&self, filter: JobFilter, limit: usize) -> ScheduleResult<Vec<ScheduledJob>> {
        validate_limit(limit)?;
        let connection = self.store.connection()?;
        let mut query = connection.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM scheduled_jobs WHERE (?1 IS NULL OR id>?1) AND (?2 IS NULL OR enabled=?2) AND (?3 OR deleted_at IS NULL) ORDER BY id LIMIT ?4"
        ))?;
        let rows = query.query_map(
            params![
                filter.after_id.map(|id| id.to_string()),
                filter.enabled,
                filter.include_deleted,
                limit as i64
            ],
            job_row,
        )?;
        collect(rows, validate_job)
    }

    pub fn due(&self, now: UnixMillis, limit: usize) -> ScheduleResult<Vec<ScheduledJob>> {
        validate_limit(limit)?;
        let connection = self.store.connection()?;
        let mut query = connection.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM scheduled_jobs WHERE enabled=1 AND deleted_at IS NULL AND next_run_at IS NOT NULL AND next_run_at<=?1 ORDER BY next_run_at,id LIMIT ?2"
        ))?;
        collect(
            query.query_map(params![timestamp(now)?, limit as i64], job_row)?,
            validate_job,
        )
    }

    pub(super) fn due_excluding(
        &self,
        now: UnixMillis,
        excluded: &[ScheduledJobId],
        limit: usize,
    ) -> ScheduleResult<Vec<ScheduledJob>> {
        validate_limit(limit)?;
        if excluded.len() > 4096 {
            return Err(ScheduleError::Capacity("blocked job identities"));
        }
        let connection = self.store.connection()?;
        let mut query=connection.prepare(&format!("SELECT {JOB_COLUMNS} FROM scheduled_jobs WHERE enabled=1 AND deleted_at IS NULL AND next_run_at IS NOT NULL AND next_run_at<=?1 AND id NOT IN (SELECT value FROM json_each(?2)) ORDER BY next_run_at,id LIMIT ?3"))?;
        collect(
            query.query_map(
                params![
                    timestamp(now)?,
                    serde_json::to_string(excluded)?,
                    limit as i64
                ],
                job_row,
            )?,
            validate_job,
        )
    }

    pub fn earliest_due(&self) -> ScheduleResult<Option<UnixMillis>> {
        let connection = self.store.connection()?;
        Ok(connection.query_row(
            "SELECT min(next_run_at) FROM scheduled_jobs WHERE enabled=1 AND deleted_at IS NULL AND next_run_at IS NOT NULL",
            [],
            |row| optional_millis(row, 0),
        )?)
    }

    pub fn claim_due(&self, claim: &DueClaim, now: UnixMillis) -> ScheduleResult<ClaimOutcome> {
        timestamp(claim.scheduled_for)?;
        let digest = claim_digest(b"everything.schedule.due-claim.v1\0", claim)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = replay_claim(&transaction, claim.occurrence_id, &digest)? {
            return Ok(outcome);
        }
        let job = required_job(&transaction, claim.job_id)?;
        check_revision(job.revision, claim.expected_revision)?;
        if !job.enabled
            || job.deleted_at.is_some()
            || job.next_run_at != Some(claim.scheduled_for)
            || claim.scheduled_for > now
        {
            return Err(ScheduleError::NotDue);
        }
        let recorded_at = now.max(job.updated_at);
        let forced_outcome = match claim.disposition {
            DueDisposition::Dispatch => None,
            DueDisposition::SkipMissedRange { through } => {
                timestamp(through)?;
                if through < claim.scheduled_for
                    || through > now
                    || claim.next_run_at.is_some_and(|next| next <= through)
                    || matches!(job.definition.schedule, ScheduleSpec::Once { .. })
                        && through != claim.scheduled_for
                {
                    return Err(ScheduleError::Invalid(
                        "missed range must cover only elapsed schedule time",
                    ));
                }
                Some(RunOutcome::SkippedMissedRange { through })
            }
        };
        job.definition.schedule.validate_next(claim.next_run_at)?;
        if claim
            .next_run_at
            .is_some_and(|next| next <= claim.scheduled_for)
            || matches!(job.definition.schedule, ScheduleSpec::Once { .. })
                && claim.next_run_at.is_some()
        {
            return Err(ScheduleError::Invalid(
                "claim must advance or exhaust the schedule",
            ));
        }
        let duplicate: Option<String> = transaction.query_row(
            "SELECT occurrence_id FROM scheduled_runs WHERE job_id=?1 AND generation=?2 AND scheduled_for=?3 AND trigger='scheduled'",
            params![job.id.to_string(),job.generation,timestamp(claim.scheduled_for)?],|row|row.get(0),
        ).optional()?;
        if let Some(id) = duplicate {
            return Err(ScheduleError::OccurrenceConflict(id.parse()?));
        }
        let run = insert_run(
            &transaction,
            &job,
            RunAdmission {
                occurrence_id: claim.occurrence_id,
                claimed_at: now,
                scheduled_for: claim.scheduled_for,
                trigger: RunTrigger::Scheduled,
                digest: &digest,
                forced_outcome,
            },
            recorded_at,
        )?;
        transaction.execute(
            "UPDATE scheduled_jobs SET next_run_at=?2,last_run_at=?3,updated_at=?3,revision=?4 WHERE id=?1",
            params![job.id.to_string(),optional_time(claim.next_run_at)?,timestamp(recorded_at)?,increment(job.revision)?],
        )?;
        transaction.commit()?;
        Ok(ClaimOutcome {
            replayed: false,
            run,
        })
    }

    pub fn claim_manual(
        &self,
        claim: &ManualClaim,
        authority: &ExecutionAuthority,
        now: UnixMillis,
    ) -> ScheduleResult<ClaimOutcome> {
        let digest = claim_digest(b"everything.schedule.manual-claim.v1\0", claim)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = replay_claim(&transaction, claim.occurrence_id, &digest)? {
            outcome
                .run
                .definition
                .target
                .validate_authority(authority)?;
            return Ok(outcome);
        }
        let job = required_job(&transaction, claim.job_id)?;
        check_revision(job.revision, claim.expected_revision)?;
        check_time(now, job.updated_at)?;
        job.definition.target.validate_authority(authority)?;
        if job.deleted_at.is_some() {
            return Err(ScheduleError::Invalid(
                "deleted jobs cannot be run manually",
            ));
        }
        let duplicate: Option<String> = transaction
            .query_row(
                "SELECT occurrence_id FROM scheduled_runs WHERE manual_command_id=?1",
                [claim.command_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = duplicate {
            return Err(ScheduleError::OccurrenceConflict(id.parse()?));
        }
        let run = insert_run(
            &transaction,
            &job,
            RunAdmission {
                occurrence_id: claim.occurrence_id,
                claimed_at: now,
                scheduled_for: now,
                trigger: RunTrigger::Manual {
                    command_id: claim.command_id,
                },
                digest: &digest,
                forced_outcome: None,
            },
            now,
        )?;
        transaction.execute(
            "UPDATE scheduled_jobs SET last_run_at=?2,updated_at=?2,revision=?3 WHERE id=?1",
            params![
                job.id.to_string(),
                timestamp(now)?,
                increment(job.revision)?
            ],
        )?;
        transaction.commit()?;
        Ok(ClaimOutcome {
            replayed: false,
            run,
        })
    }

    pub fn run(&self, id: OccurrenceId) -> ScheduleResult<Option<ScheduledRun>> {
        let connection = self.store.connection()?;
        read_run(&connection, id)
    }

    pub fn history(
        &self,
        job_id: ScheduledJobId,
        before: Option<RunCursor>,
        limit: usize,
    ) -> ScheduleResult<Vec<ScheduledRun>> {
        validate_limit(limit)?;
        let connection = self.store.connection()?;
        let mut query = connection.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM scheduled_runs WHERE job_id=?1 AND (?2 IS NULL OR claimed_at<?2 OR (claimed_at=?2 AND occurrence_id<?3)) ORDER BY claimed_at DESC,occurrence_id DESC LIMIT ?4"
        ))?;
        collect(
            query.query_map(
                params![
                    job_id.to_string(),
                    before
                        .map(|cursor| timestamp(cursor.claimed_at))
                        .transpose()?,
                    before.map(|cursor| cursor.occurrence_id.to_string()),
                    limit as i64
                ],
                run_row,
            )?,
            validate_run,
        )
    }

    pub fn unfinished_runs(
        &self,
        after: Option<OccurrenceId>,
        limit: usize,
    ) -> ScheduleResult<Vec<ScheduledRun>> {
        validate_limit(limit)?;
        let connection = self.store.connection()?;
        let mut query = connection.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM scheduled_runs WHERE state IN ('claimed','running') AND (?1 IS NULL OR occurrence_id>?1) ORDER BY occurrence_id LIMIT ?2"
        ))?;
        collect(
            query.query_map(
                params![after.map(|id| id.to_string()), limit as i64],
                run_row,
            )?,
            validate_run,
        )
    }

    pub fn start_run(
        &self,
        id: OccurrenceId,
        expected_revision: u64,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledRun> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = required_run(&transaction, id)?;
        check_revision(run.revision, expected_revision)?;
        check_time(now, run.updated_at)?;
        if run.state != RunState::Claimed {
            return Err(ScheduleError::RunStateConflict(id));
        }
        let task_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
            [run.initial_task_id.to_string()],
            |row| row.get(0),
        )?;
        if !task_exists {
            return Err(ScheduleError::Invalid(
                "reserved native task must exist before starting its scheduled run",
            ));
        }
        transaction.execute(
            "UPDATE scheduled_runs SET state='running',started_at=?2,updated_at=?2,revision=?3 WHERE occurrence_id=?1",
            params![id.to_string(),timestamp(now)?,increment(run.revision)?],
        )?;
        let updated = required_run(&transaction, id)?;
        transaction.commit()?;
        Ok(updated)
    }

    pub fn finish_run(
        &self,
        id: OccurrenceId,
        expected_revision: u64,
        outcome: &RunOutcome,
        now: UnixMillis,
    ) -> ScheduleResult<ScheduledRun> {
        outcome.validate()?;
        let encoded = encode(outcome, MAX_RUN_OUTCOME_BYTES)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = required_run(&transaction, id)?;
        check_revision(run.revision, expected_revision)?;
        check_time(now, run.updated_at)?;
        if run.state.is_terminal()
            || run.state == RunState::Claimed && outcome.state() == RunState::Succeeded
            || run.state == RunState::Running && outcome.state() == RunState::Skipped
        {
            return Err(ScheduleError::RunStateConflict(id));
        }
        transaction.execute(
            "UPDATE scheduled_runs SET state=?2,outcome=?3,finished_at=?4,updated_at=?4,revision=?5 WHERE occurrence_id=?1",
            params![id.to_string(),outcome.state().as_str(),encoded,timestamp(now)?,increment(run.revision)?],
        )?;
        let updated = required_run(&transaction, id)?;
        transaction.commit()?;
        Ok(updated)
    }
}

fn validate_input(
    input: &NewScheduledJob,
    authority: &ExecutionAuthority,
) -> ScheduleResult<String> {
    if input.name.trim().is_empty()
        || input.name.len() > 256
        || input.name.chars().any(char::is_control)
    {
        return Err(ScheduleError::Invalid(
            "job name requires 1 to 256 bytes without controls",
        ));
    }
    input.definition.validate()?;
    input.definition.target.validate_authority(authority)?;
    input.definition.schedule.validate_next(input.next_run_at)?;
    if input.enabled && input.next_run_at.is_none() {
        return Err(ScheduleError::Invalid(
            "enabled job creation or edit requires a next run",
        ));
    }
    encode(&input.definition, MAX_SCHEDULE_DEFINITION_BYTES)
}

fn check_delivery(connection: &Connection, delivery: &ResultDelivery) -> ScheduleResult<()> {
    if let ResultDelivery::Conversation { conversation_id } = delivery {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)",
            [conversation_id.to_string()],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(ScheduleError::Invalid(
                "result delivery conversation does not exist",
            ));
        }
    }
    Ok(())
}

fn check_job_capacity(
    connection: &Connection,
    old_bytes: usize,
    new_bytes: usize,
    inserting: bool,
) -> ScheduleResult<()> {
    let (jobs, bytes): (i64, i64) = connection.query_row(
        "SELECT jobs,job_bytes FROM scheduling_usage WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if jobs + i64::from(inserting) > MAX_JOBS
        || bytes - old_bytes as i64 + new_bytes as i64 > MAX_JOB_BYTES
    {
        return Err(ScheduleError::Capacity("job definitions"));
    }
    Ok(())
}

struct RunAdmission<'a> {
    occurrence_id: OccurrenceId,
    claimed_at: UnixMillis,
    scheduled_for: UnixMillis,
    trigger: RunTrigger,
    digest: &'a str,
    forced_outcome: Option<RunOutcome>,
}

fn insert_run(
    connection: &Connection,
    job: &ScheduledJob,
    admission: RunAdmission<'_>,
    now: UnixMillis,
) -> ScheduleResult<ScheduledRun> {
    let RunAdmission {
        occurrence_id,
        claimed_at,
        scheduled_for,
        trigger,
        digest,
        forced_outcome,
    } = admission;
    let active: i64 = connection.query_row(
        "SELECT count(*) FROM scheduled_runs WHERE job_id=?1 AND state IN ('claimed','running','unknown')",
        [job.id.to_string()],
        |row| row.get(0),
    )?;
    let overlap_limit = job.definition.policies.overlap.limit()?;
    let outcome = forced_outcome.or_else(|| {
        (active >= i64::from(overlap_limit)).then_some(RunOutcome::Skipped {
            reason: SkipReason::Overlap,
        })
    });
    let skipped = outcome.is_some();
    let outcome_json = outcome
        .as_ref()
        .map(|value| encode(value, MAX_RUN_OUTCOME_BYTES))
        .transpose()?;
    let definition = encode(&job.definition, MAX_SCHEDULE_DEFINITION_BYTES)?;
    let (runs, bytes, active_runs): (i64, i64, i64) = connection.query_row(
        "SELECT runs,history_bytes,active_runs FROM scheduling_usage WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let reserved = (active_runs + i64::from(!skipped)) * MAX_RUN_OUTCOME_BYTES as i64;
    if runs >= MAX_RUNS
        || bytes
            + definition.len() as i64
            + outcome_json.as_ref().map_or(0, |value| value.len()) as i64
            + reserved
            > MAX_HISTORY_BYTES
    {
        return Err(ScheduleError::Capacity("run history"));
    }
    let initial_task_id = TaskId::new();
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1)",
        [initial_task_id.to_string()],
        |row| row.get(0),
    )?;
    if exists {
        return Err(ScheduleError::Invalid(
            "reserved task identity already exists",
        ));
    }
    let (kind, manual_command) = match trigger {
        RunTrigger::Scheduled => ("scheduled", None),
        RunTrigger::Manual { command_id } => ("manual", Some(command_id.to_string())),
    };
    let state = if skipped {
        RunState::Skipped
    } else {
        RunState::Claimed
    };
    connection.execute(
        "INSERT INTO scheduled_runs(occurrence_id,job_id,generation,definition,scheduled_for,trigger,manual_command_id,initial_task_id,claim_digest,state,revision,claimed_at,finished_at,updated_at,outcome) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1,?11,?12,?14,?13)",
        params![occurrence_id.to_string(),job.id.to_string(),job.generation,definition,timestamp(scheduled_for)?,kind,manual_command,initial_task_id.to_string(),digest,state.as_str(),timestamp(claimed_at)?,if skipped { Some(timestamp(now)?) } else { None },outcome_json,timestamp(now)?],
    )?;
    required_run(connection, occurrence_id)
}

fn replay_claim(
    connection: &Connection,
    id: OccurrenceId,
    digest: &str,
) -> ScheduleResult<Option<ClaimOutcome>> {
    let existing: Option<String> = connection
        .query_row(
            "SELECT claim_digest FROM scheduled_runs WHERE occurrence_id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    if existing != digest {
        return Err(ScheduleError::OccurrenceConflict(id));
    }
    Ok(Some(ClaimOutcome {
        replayed: true,
        run: required_run(connection, id)?,
    }))
}

fn claim_digest(namespace: &[u8], claim: &impl Serialize) -> ScheduleResult<String> {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update(encode(claim, 4_096)?.as_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

fn read_job(connection: &Connection, id: ScheduledJobId) -> ScheduleResult<Option<ScheduledJob>> {
    let value = connection
        .query_row(
            &format!("SELECT {JOB_COLUMNS} FROM scheduled_jobs WHERE id=?1"),
            [id.to_string()],
            job_row,
        )
        .optional()?;
    value
        .map(|job| {
            validate_job(&job)?;
            Ok(job)
        })
        .transpose()
}

fn required_job(connection: &Connection, id: ScheduledJobId) -> ScheduleResult<ScheduledJob> {
    read_job(connection, id)?.ok_or_else(|| ScheduleError::NotFound {
        entity: "job",
        id: id.to_string(),
    })
}

fn read_run(connection: &Connection, id: OccurrenceId) -> ScheduleResult<Option<ScheduledRun>> {
    let value = connection
        .query_row(
            &format!("SELECT {RUN_COLUMNS} FROM scheduled_runs WHERE occurrence_id=?1"),
            [id.to_string()],
            run_row,
        )
        .optional()?;
    value
        .map(|run| {
            validate_run(&run)?;
            Ok(run)
        })
        .transpose()
}

pub(super) fn required_run(
    connection: &Connection,
    id: OccurrenceId,
) -> ScheduleResult<ScheduledRun> {
    read_run(connection, id)?.ok_or_else(|| ScheduleError::NotFound {
        entity: "run",
        id: id.to_string(),
    })
}

fn validate_job(job: &ScheduledJob) -> ScheduleResult<()> {
    job.definition.validate()?;
    job.definition.schedule.validate_next(job.next_run_at)?;
    timestamp(job.created_at)?;
    timestamp(job.updated_at)?;
    Ok(())
}

fn validate_run(run: &ScheduledRun) -> ScheduleResult<()> {
    run.definition.validate()?;
    timestamp(run.scheduled_for)?;
    timestamp(run.claimed_at)?;
    timestamp(run.updated_at)?;
    if let Some(outcome) = &run.outcome {
        outcome.validate()?;
        if let RunOutcome::SkippedMissedRange { through } = outcome
            && (*through < run.scheduled_for || *through > run.claimed_at)
        {
            return Err(ScheduleError::Invalid(
                "stored missed range is outside the occurrence bounds",
            ));
        }
        if outcome.state() != run.state {
            return Err(ScheduleError::Invalid(
                "stored run state and outcome disagree",
            ));
        }
    }
    Ok(())
}

fn collect<T: Serialize>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
    validate: impl Fn(&T) -> ScheduleResult<()>,
) -> ScheduleResult<Vec<T>> {
    let mut result = Vec::new();
    let mut bytes = 2_usize;
    for row in rows {
        let row = row?;
        validate(&row)?;
        bytes = bytes.saturating_add(serde_json::to_vec(&row)?.len() + 1);
        if bytes > MAX_SCHEDULE_PAGE_BYTES {
            return Err(ScheduleError::ReadBudgetExceeded);
        }
        result.push(row);
    }
    Ok(result)
}

fn encode(value: &impl Serialize, limit: usize) -> ScheduleResult<String> {
    let value = serde_json::to_string(value)?;
    if value.len() > limit {
        return Err(ScheduleError::Invalid(
            "serialized schedule data exceeds its bound",
        ));
    }
    Ok(value)
}

fn validate_limit(limit: usize) -> ScheduleResult<()> {
    if !(1..=256).contains(&limit) {
        return Err(ScheduleError::Invalid(
            "schedule page size must be between one and 256",
        ));
    }
    Ok(())
}

fn increment(value: u64) -> ScheduleResult<u64> {
    value
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(ScheduleError::Invalid(
            "schedule revision or generation overflow",
        ))
}

fn check_revision(actual: u64, expected: u64) -> ScheduleResult<()> {
    if actual != expected {
        return Err(ScheduleError::RevisionConflict { expected, actual });
    }
    Ok(())
}

fn check_time(now: UnixMillis, previous: UnixMillis) -> ScheduleResult<()> {
    timestamp(now)?;
    if now < previous {
        return Err(ScheduleError::Invalid(
            "schedule update time cannot move backward",
        ));
    }
    Ok(())
}

fn optional_time(value: Option<UnixMillis>) -> ScheduleResult<Option<i64>> {
    value.map(timestamp).transpose()
}

fn parsed_cell<T: DeserializeOwned>(row: &Row<'_>, column: usize) -> rusqlite::Result<T> {
    let raw: String = row.get(column)?;
    serde_json::from_value(serde_json::Value::String(raw)).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn json_cell<T: DeserializeOwned>(row: &Row<'_>, column: usize) -> rusqlite::Result<T> {
    let raw: String = row.get(column)?;
    serde_json::from_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn millis(row: &Row<'_>, column: usize) -> rusqlite::Result<UnixMillis> {
    let value: u64 = row.get(column)?;
    UnixMillis::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn optional_millis(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<UnixMillis>> {
    let value: Option<u64> = row.get(column)?;
    value
        .map(|value| {
            UnixMillis::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn job_row(row: &Row<'_>) -> rusqlite::Result<ScheduledJob> {
    Ok(ScheduledJob {
        id: parsed_cell(row, 0)?,
        name: row.get(1)?,
        definition: json_cell(row, 2)?,
        generation: row.get(3)?,
        revision: row.get(4)?,
        enabled: row.get(5)?,
        next_run_at: optional_millis(row, 6)?,
        last_run_at: optional_millis(row, 7)?,
        created_at: millis(row, 8)?,
        updated_at: millis(row, 9)?,
        deleted_at: optional_millis(row, 10)?,
    })
}

fn run_row(row: &Row<'_>) -> rusqlite::Result<ScheduledRun> {
    let trigger: String = row.get(5)?;
    let trigger = if trigger == "scheduled" {
        RunTrigger::Scheduled
    } else {
        RunTrigger::Manual {
            command_id: parsed_cell(row, 6)?,
        }
    };
    let outcome: Option<String> = row.get(14)?;
    let outcome = outcome
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()?;
    Ok(ScheduledRun {
        occurrence_id: parsed_cell(row, 0)?,
        job_id: parsed_cell(row, 1)?,
        generation: row.get(2)?,
        definition: json_cell(row, 3)?,
        scheduled_for: millis(row, 4)?,
        trigger,
        initial_task_id: parsed_cell(row, 7)?,
        state: parsed_cell(row, 8)?,
        revision: row.get(9)?,
        claimed_at: millis(row, 10)?,
        started_at: optional_millis(row, 11)?,
        finished_at: optional_millis(row, 12)?,
        updated_at: millis(row, 13)?,
        outcome,
    })
}
