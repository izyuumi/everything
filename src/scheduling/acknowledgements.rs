use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::protocol::{CommandId, UnixMillis};

use super::execution::outcome_digest;
use super::{OccurrenceId, RunState, ScheduleError, ScheduleRepository, ScheduleResult, timestamp};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnknownAcknowledgementRequest {
    pub occurrence_id: OccurrenceId,
    pub command_id: CommandId,
    pub outcome_digest: String,
    pub note: String,
    pub accepted_at: UnixMillis,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownAcknowledgement {
    pub request: UnknownAcknowledgementRequest,
    pub operator_uid: u32,
}

impl ScheduleRepository {
    pub fn terminal_outcome_digest(&self, id: OccurrenceId) -> ScheduleResult<String> {
        let connection = self.store.connection()?;
        let run = super::store::required_run(&connection, id)?;
        let outcome = run
            .outcome
            .as_ref()
            .ok_or(ScheduleError::RunStateConflict(id))?;
        outcome_digest(id, outcome)
    }

    pub fn acknowledge_unknown_as_owner(
        &self,
        request: &UnknownAcknowledgementRequest,
    ) -> ScheduleResult<UnknownAcknowledgement> {
        if request.note.is_empty()
            || request.note.len() > 1024
            || request.note.contains('\0')
            || request.outcome_digest.len() != 64
            || !request
                .outcome_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ScheduleError::Invalid(
                "invalid uncertainty acknowledgement",
            ));
        }
        timestamp(request.accepted_at)?;
        let operator_uid = rustix::process::geteuid().as_raw();
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing=transaction.query_row("SELECT occurrence_id,command_id,outcome_digest,operator_uid,note,acknowledged_at,length(CAST(note AS BLOB)),length(occurrence_id),length(command_id),length(outcome_digest) FROM scheduled_unknown_acknowledgements WHERE occurrence_id=?1 OR command_id=?2",params![request.occurrence_id.to_string(),request.command_id.to_string()],|row|{
            let size:usize=row.get(6)?;
            if size>1024 || row.get::<_,usize>(7)?!=36 || row.get::<_,usize>(8)?!=36 || row.get::<_,usize>(9)?!=64 {return Err(rusqlite::Error::InvalidQuery);}
            Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,u32>(3)?,row.get::<_,String>(4)?,row.get::<_,u64>(5)?))
        }).optional()?;
        if let Some(existing) = existing {
            if existing.0 != request.occurrence_id.to_string()
                || existing.1 != request.command_id.to_string()
                || existing.2 != request.outcome_digest
                || existing.3 != operator_uid
                || existing.4 != request.note
                || existing.5 != request.accepted_at.get()
            {
                return Err(ScheduleError::OccurrenceConflict(request.occurrence_id));
            }
            return Ok(UnknownAcknowledgement {
                request: request.clone(),
                operator_uid,
            });
        }
        let run = super::store::required_run(&transaction, request.occurrence_id)?;
        if run.state != RunState::Unknown
            || run
                .outcome
                .as_ref()
                .map(|outcome| outcome_digest(run.occurrence_id, outcome))
                .transpose()?
                .as_deref()
                != Some(request.outcome_digest.as_str())
        {
            return Err(ScheduleError::RunStateConflict(request.occurrence_id));
        }
        let used: i64 = transaction.query_row(
            "SELECT acknowledgement_bytes FROM scheduling_execution_usage WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if used < 0
            || used
                .checked_add(request.note.len() as i64 + 192)
                .is_none_or(|total| total > 16 * 1_048_576)
        {
            return Err(ScheduleError::Capacity(
                "uncertainty acknowledgement metadata",
            ));
        }
        transaction.execute("INSERT INTO scheduled_unknown_acknowledgements(occurrence_id,command_id,outcome_digest,operator_uid,note,acknowledged_at) VALUES(?1,?2,?3,?4,?5,?6)",params![request.occurrence_id.to_string(),request.command_id.to_string(),request.outcome_digest,operator_uid,request.note,timestamp(request.accepted_at)?])?;
        transaction.commit()?;
        Ok(UnknownAcknowledgement {
            request: request.clone(),
            operator_uid,
        })
    }
}
