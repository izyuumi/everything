use rusqlite::{Connection, params};

use super::super::{
    MAX_UPDATE_BYTES, MAX_UPDATES, OperationRecord, SettingsOperationPhase,
    SettingsWriteResolution, UpdateRecord, bounded_text, decode, read, validate_native_link,
    validate_update,
};
use super::PairedReplyHistoryAnchor;
use crate::host::settings::authority::SettingsOperationOrigin;
use crate::host::settings::contract::SettingsOperationState;
use crate::host::settings::service::SettingsServiceError;

impl PairedReplyHistoryAnchor {
    pub(in crate::host::settings) fn check_current_on(
        &self,
        connection: &Connection,
        current: &OperationRecord,
    ) -> Result<(), SettingsServiceError> {
        check_history_on(connection, current, Some(self))
    }
}

impl OperationRecord {
    pub(in crate::host::settings) fn check_new_paired_reply_history_on(
        &self,
        connection: &Connection,
    ) -> Result<(), SettingsServiceError> {
        check_history_on(connection, self, None)
    }
}

fn check_history_on(
    connection: &Connection,
    current: &OperationRecord,
    anchor: Option<&PairedReplyHistoryAnchor>,
) -> Result<(), SettingsServiceError> {
    if current.identity.version != 2
        || !matches!(
            current.identity.origin,
            SettingsOperationOrigin::PairedSettings { .. }
        )
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    {
        let observed = read(connection, current.identity.binding.request_id)?
            .ok_or(SettingsServiceError::CorruptJournal)?;
        if observed.identity != current.identity || observed.update != current.update {
            return Err(SettingsServiceError::CorruptJournal);
        }
    }
    let baseline = anchor
        .map(|anchor| {
            i64::try_from(anchor.sequence)
                .ok()
                .filter(|sequence| *sequence > 0)
                .ok_or(SettingsServiceError::CorruptJournal)
        })
        .transpose()?;
    let mut baseline_found = baseline.is_none();
    let request_id = current.identity.binding.request_id.to_string();
    let mut statement = connection.prepare(
        "SELECT sequence,request_id,record,occurred_at FROM settings_operation_updates WHERE request_id=?1 ORDER BY sequence LIMIT ?2",
    )?;
    let mut rows = statement.query(params![request_id, MAX_UPDATES + 1])?;
    let mut previous_sequence = 0;
    let mut previous = None;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        if count >= MAX_UPDATES {
            return Err(SettingsServiceError::CorruptJournal);
        }
        count += 1;
        let sequence = row.get::<_, i64>(0)?;
        if sequence <= previous_sequence || bounded_text(row, 1, 36)? != request_id {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let update: UpdateRecord =
            decode(bounded_text(row, 2, MAX_UPDATE_BYTES)?, MAX_UPDATE_BYTES)?;
        validate_update(&current.identity, &update)?;
        validate_native_link(connection, &current.identity, &update)?;
        let occurred_at = i64::try_from(update.updated_at.get())
            .map_err(|_| SettingsServiceError::CorruptJournal)?;
        if occurred_at != row.get::<_, i64>(3)? {
            return Err(SettingsServiceError::CorruptJournal);
        }
        match &previous {
            Some(previous) => check_transition(previous, &update)?,
            None => check_initial(&update)?,
        }
        if baseline == Some(sequence) {
            baseline_found = true;
        }
        previous_sequence = sequence;
        previous = Some(update);
    }
    if !baseline_found || previous.as_ref() != Some(&current.update) {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

fn check_initial(update: &UpdateRecord) -> Result<(), SettingsServiceError> {
    if !matches!(update.phase, SettingsOperationPhase::Applying { .. })
        || update
            .write
            .as_ref()
            .is_none_or(|write| write.resolution.is_some())
        || update.application.is_some()
        || update.observation.is_some()
        || update.pairing.is_some()
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

fn check_transition(
    previous: &UpdateRecord,
    next: &UpdateRecord,
) -> Result<(), SettingsServiceError> {
    let previous_write = previous
        .write
        .as_ref()
        .ok_or(SettingsServiceError::CorruptJournal)?;
    let next_write = next
        .write
        .as_ref()
        .ok_or(SettingsServiceError::CorruptJournal)?;
    if previous_write.intent != next_write.intent
        || previous.application.is_some()
        || previous.observation.is_some()
        || previous.pairing.is_some()
        || next.observation.is_some()
        || next.pairing.is_some()
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    let valid = match (&previous.phase, &next.phase) {
        (SettingsOperationPhase::Applying { .. }, SettingsOperationPhase::Finished { .. }) => {
            previous_write.resolution.is_none() && next.application.is_none()
        }
        (SettingsOperationPhase::Finished { result }, SettingsOperationPhase::Finished { .. }) => {
            match previous_write.resolution {
                None => {
                    result.state == SettingsOperationState::Unknown
                        && next_write.resolution.is_some()
                        && next.application.is_none()
                }
                Some(SettingsWriteResolution::Applied) => {
                    matches!(
                        result.state,
                        SettingsOperationState::PendingApplication { .. }
                    ) && previous.phase == next.phase
                        && previous_write == next_write
                        && next.application.is_some()
                }
                Some(SettingsWriteResolution::NotApplied) => false,
            }
        }
        _ => false,
    };
    if !valid {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}
