use rusqlite::{Connection, OptionalExtension, params};

use crate::agent::ScheduledResultCommand;
use crate::protocol::{
    CommandId, ConversationEvent, EventEnvelope, EventId, EventSequence, ScheduledRunNotification,
    ScheduledRunOutcomeKind, UnixMillis,
};
use crate::storage::AppendRequest;

use super::execution::{outcome_digest, preview};
use super::{
    OccurrenceId, ResultDelivery, RunOutcome, ScheduleError, ScheduleRepository, ScheduleResult,
    ScheduledRun, timestamp,
};

const MAX_NOTIFICATION_BYTES: usize = 32768;

#[derive(Clone, Debug)]
pub struct ScheduledDelivery {
    pub occurrence_id: OccurrenceId,
    pub command: ScheduledResultCommand,
}

pub(super) fn reserve_in(connection: &Connection, run: &ScheduledRun) -> ScheduleResult<()> {
    let ResultDelivery::Conversation { conversation_id } = run.definition.target.delivery else {
        return Ok(());
    };
    let existing:Option<String>=connection.query_row("SELECT conversation_id,length(conversation_id) FROM scheduled_deliveries WHERE occurrence_id=?1",[run.occurrence_id.to_string()],|row|{
        if row.get::<_,usize>(1)?!=36 {return Err(rusqlite::Error::InvalidQuery);}row.get(0)
    }).optional()?;
    if let Some(existing) = existing {
        return if existing == conversation_id.to_string() {
            Ok(())
        } else {
            Err(ScheduleError::OccurrenceConflict(run.occurrence_id))
        };
    }
    let slots: i64 = connection.query_row(
        "SELECT delivery_slots FROM scheduling_execution_usage WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if !(0..2048).contains(&slots) {
        return Err(ScheduleError::Capacity(
            "pending scheduled delivery reservations",
        ));
    }
    connection.execute("INSERT INTO scheduled_deliveries(occurrence_id,conversation_id,command_id,event_id) VALUES(?1,?2,?3,?4)",params![run.occurrence_id.to_string(),conversation_id.to_string(),CommandId::new().to_string(),EventId::new().to_string()])?;
    Ok(())
}

pub(super) fn ready_in(
    connection: &Connection,
    run: &ScheduledRun,
    now: UnixMillis,
) -> ScheduleResult<()> {
    if matches!(run.definition.target.delivery, ResultDelivery::Isolated) {
        return Ok(());
    }
    if run.outcome.is_none() {
        return Ok(());
    }
    reserve_in(connection, run)?;
    let ready: bool = connection.query_row(
        "SELECT accepted_at IS NOT NULL FROM scheduled_deliveries WHERE occurrence_id=?1",
        [run.occurrence_id.to_string()],
        |row| row.get(0),
    )?;
    if ready {
        return Ok(());
    }
    let notification = notification_in(connection, run)?;
    let digest = notification.outcome_digest.clone();
    let encoded = serde_json::to_string(&notification)?;
    if encoded.len() > MAX_NOTIFICATION_BYTES {
        return Err(ScheduleError::Capacity("scheduled notification bytes"));
    }
    connection.execute("UPDATE scheduled_deliveries SET accepted_at=?2,outcome_digest=?3,notification=?4 WHERE occurrence_id=?1 AND accepted_at IS NULL",params![run.occurrence_id.to_string(),timestamp(now)?,digest,encoded])?;
    Ok(())
}

fn notification_in(
    connection: &Connection,
    run: &ScheduledRun,
) -> ScheduleResult<ScheduledRunNotification> {
    let outcome = run
        .outcome
        .as_ref()
        .ok_or(ScheduleError::RunStateConflict(run.occurrence_id))?;
    let identity=connection.query_row("SELECT a.task_id,a.conversation_id,length(a.task_id),length(a.conversation_id) FROM scheduled_attempts a JOIN tasks t ON t.id=a.task_id AND t.conversation_id=a.conversation_id WHERE a.occurrence_id=?1 AND a.attempt_number=(SELECT max(attempt_number) FROM scheduled_attempts WHERE occurrence_id=?1) LIMIT 1",[run.occurrence_id.to_string()],|row|{
        if row.get::<_,usize>(2)?!=36 || row.get::<_,usize>(3)?!=36 {return Err(rusqlite::Error::InvalidQuery);}
        Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))
    }).optional()?;
    let (task_id, execution_conversation_id) =
        match identity {
            Some((task, conversation)) => (
                Some(
                    task.parse()
                        .map_err(|_| ScheduleError::Invalid("invalid delivered task identity"))?,
                ),
                Some(conversation.parse().map_err(|_| {
                    ScheduleError::Invalid("invalid delivered conversation identity")
                })?),
            ),
            None => (None, None),
        };
    let (kind, summary) = match outcome {
        RunOutcome::Succeeded { summary } => (ScheduledRunOutcomeKind::Succeeded, summary.as_str()),
        RunOutcome::Failed { message, .. } => (ScheduledRunOutcomeKind::Failed, message.as_str()),
        RunOutcome::Cancelled { reason } => (ScheduledRunOutcomeKind::Cancelled, reason.as_str()),
        RunOutcome::Unknown { reason } => (ScheduledRunOutcomeKind::Unknown, reason.as_str()),
        RunOutcome::Skipped { .. } => (
            ScheduledRunOutcomeKind::Skipped,
            "Scheduled occurrence skipped by its overlap policy.",
        ),
        RunOutcome::SkippedMissedRange { .. } => (
            ScheduledRunOutcomeKind::Skipped,
            "Missed scheduled occurrences were collapsed into one retained range.",
        ),
    };
    let digest = outcome_digest(run.occurrence_id, outcome)?;
    let notification = ScheduledRunNotification {
        job_id: (*run.job_id.as_uuid()),
        occurrence_id: (*run.occurrence_id.as_uuid()),
        generation: run.generation,
        scheduled_for: run.scheduled_for,
        finished_at: run.finished_at.ok_or(ScheduleError::Invalid(
            "terminal delivery has no completion time",
        ))?,
        task_id,
        execution_conversation_id,
        outcome: kind,
        summary: preview(summary, 4096),
        outcome_digest: digest,
    };
    notification
        .validate()
        .map_err(|_| ScheduleError::Invalid("invalid scheduled delivery notification"))?;
    Ok(notification)
}

impl ScheduleRepository {
    pub(super) fn reserve_run_delivery(&self, id: OccurrenceId) -> ScheduleResult<()> {
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let run = super::store::required_run(&transaction, id)?;
        reserve_in(&transaction, &run)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pending_delivery_count(&self) -> ScheduleResult<usize> {
        let value: i64 = self.store.connection()?.query_row(
            "SELECT delivery_slots FROM scheduling_execution_usage WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if !(0..=2048).contains(&value) {
            return Err(ScheduleError::Invalid("invalid pending delivery counter"));
        }
        Ok(value as usize)
    }
    pub fn prepare_terminal_deliveries(
        &self,
        limit: usize,
        now: UnixMillis,
    ) -> ScheduleResult<usize> {
        if !(1..=32).contains(&limit) {
            return Err(ScheduleError::Invalid("invalid delivery page size"));
        }
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let ids = {
            let mut statement=transaction.prepare("SELECT r.occurrence_id,length(r.occurrence_id) FROM scheduled_runs r LEFT JOIN scheduled_deliveries d ON d.occurrence_id=r.occurrence_id WHERE r.state NOT IN ('claimed','running') AND json_extract(r.definition,'$.target.delivery.kind')='conversation' AND (d.occurrence_id IS NULL OR d.accepted_at IS NULL) ORDER BY r.occurrence_id LIMIT ?1")?;
            statement
                .query_map([limit], |row| {
                    if row.get::<_, usize>(1)? != 36 {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    let id: String = row.get(0)?;
                    Ok(id)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for id in &ids {
            let id = id
                .parse()
                .map_err(|_| ScheduleError::Invalid("invalid pending delivery occurrence"))?;
            let run = super::store::required_run(&transaction, id)?;
            ready_in(&transaction, &run, now)?;
        }
        transaction.commit()?;
        Ok(ids.len())
    }

    pub fn pending_deliveries(
        &self,
        after: Option<OccurrenceId>,
        limit: usize,
    ) -> ScheduleResult<Vec<ScheduledDelivery>> {
        if !(1..=32).contains(&limit) {
            return Err(ScheduleError::Invalid("invalid delivery page size"));
        }
        let connection = self.store.connection()?;
        let mut statement=connection.prepare("SELECT occurrence_id,conversation_id,command_id,event_id,accepted_at,notification,length(CAST(notification AS BLOB)),length(occurrence_id),length(conversation_id),length(command_id),length(event_id) FROM scheduled_deliveries WHERE acknowledged_sequence IS NULL AND notification IS NOT NULL AND (?1 IS NULL OR occurrence_id>?1) ORDER BY occurrence_id LIMIT ?2")?;
        let values = statement
            .query_map(params![after.map(|id| id.to_string()), limit], |row| {
                if row.get::<_, usize>(6)? > MAX_NOTIFICATION_BYTES {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                for column in 7..=10 {
                    if row.get::<_, usize>(column)? != 36 {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                }
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|value| {
                let delivery=ScheduledDelivery {
                    occurrence_id: value
                        .0
                        .parse()
                        .map_err(|_| ScheduleError::Invalid("invalid delivery occurrence"))?,
                    command: ScheduledResultCommand {
                        conversation_id: value
                            .1
                            .parse()
                            .map_err(|_| ScheduleError::Invalid("invalid delivery destination"))?,
                        command_id: value
                            .2
                            .parse()
                            .map_err(|_| ScheduleError::Invalid("invalid delivery command"))?,
                        event_id: value
                            .3
                            .parse()
                            .map_err(|_| ScheduleError::Invalid("invalid delivery event"))?,
                        accepted_at: UnixMillis::try_from(value.4).map_err(|_| {
                            ScheduleError::Invalid("invalid delivery acceptance time")
                        })?,
                        notification: serde_json::from_str(&value.5)?,
                    },
                };
                let run=super::store::required_run(&connection,delivery.occurrence_id)?;
                if !matches!(run.definition.target.delivery,ResultDelivery::Conversation {conversation_id} if conversation_id==delivery.command.conversation_id)
                    || notification_in(&connection,&run)?!=delivery.command.notification
                {return Err(ScheduleError::OccurrenceConflict(delivery.occurrence_id));}
                Ok(delivery)
            })
            .collect()
    }

    pub fn acknowledge_delivery(
        &self,
        delivery: &ScheduledDelivery,
        sequence: EventSequence,
    ) -> ScheduleResult<()> {
        let mut connection = self.store.connection()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored=transaction.query_row("SELECT command_id,event_id,acknowledged_sequence,conversation_id,accepted_at,outcome_digest,notification,length(command_id),length(event_id),length(conversation_id),length(outcome_digest),length(CAST(notification AS BLOB)) FROM scheduled_deliveries WHERE occurrence_id=?1",[delivery.occurrence_id.to_string()],|row|{
            for column in 7..=9 {if row.get::<_,usize>(column)?!=36 {return Err(rusqlite::Error::InvalidQuery);}}
            if row.get::<_,usize>(10)?!=64 || row.get::<_,Option<usize>>(11)?.is_some_and(|size|size>MAX_NOTIFICATION_BYTES) {return Err(rusqlite::Error::InvalidQuery);}
            Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<u64>>(2)?,row.get::<_,String>(3)?,row.get::<_,u64>(4)?,row.get::<_,String>(5)?,row.get::<_,Option<String>>(6)?))
        })?;
        if stored.0 != delivery.command.command_id.to_string()
            || stored.1 != delivery.command.event_id.to_string()
            || stored.3 != delivery.command.conversation_id.to_string()
            || stored.4 != delivery.command.accepted_at.get()
            || stored.5 != delivery.command.notification.outcome_digest
            || stored.6.as_ref().is_some_and(|body| {
                !serde_json::from_str::<crate::protocol::ScheduledRunNotification>(body)
                    .is_ok_and(|notification| notification == delivery.command.notification)
            })
            || stored.2.is_some_and(|saved| saved != sequence.get())
        {
            return Err(ScheduleError::OccurrenceConflict(delivery.occurrence_id));
        }
        let request:String=transaction.query_row("SELECT request,length(CAST(request AS BLOB)) FROM commands WHERE id=?1 AND conversation_id=?2 AND first_sequence=?3 AND last_sequence=?3",params![stored.0,delivery.command.conversation_id.to_string(),sequence.get()],|row|{
            if row.get::<_,usize>(1)?>131072 {return Err(rusqlite::Error::InvalidQuery);}row.get(0)
        })?;
        let request: AppendRequest = serde_json::from_str(&request)?;
        let event = request
            .events
            .first()
            .filter(|_| request.events.len() == 1)
            .ok_or(ScheduleError::OccurrenceConflict(delivery.occurrence_id))?;
        let envelope: EventEnvelope = serde_json::from_value(event.payload.clone())?;
        if request.command_id != stored.0
            || request.conversation_id != delivery.command.conversation_id.to_string()
            || request.expected_sequence.checked_add(1) != Some(sequence.get())
            || event.id != delivery.command.event_id.to_string()
            || event.kind != "scheduled_run_finished"
            || u64::try_from(event.occurred_at).ok() != Some(delivery.command.accepted_at.get())
            || envelope.schema_version != crate::protocol::SchemaVersion::CURRENT
            || envelope.event_id != delivery.command.event_id
            || envelope.sequence != sequence
            || envelope.conversation_id != delivery.command.conversation_id
            || envelope.causation_id != Some(delivery.command.command_id)
            || envelope.occurred_at != delivery.command.accepted_at
            || envelope.payload
                != (ConversationEvent::ScheduledRunFinished {
                    notification: delivery.command.notification.clone(),
                })
        {
            return Err(ScheduleError::OccurrenceConflict(delivery.occurrence_id));
        }
        if stored.2.is_some() {
            return Ok(());
        }
        transaction.execute("UPDATE scheduled_deliveries SET acknowledged_sequence=?2,notification=NULL WHERE occurrence_id=?1 AND acknowledged_sequence IS NULL",params![delivery.occurrence_id.to_string(),sequence.get()])?;
        transaction.commit()?;
        Ok(())
    }
}
