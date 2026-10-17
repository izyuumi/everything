mod history;

use super::{
    OperationIdentity, SettingsOperationInspection, SettingsOperationPhase, SettingsWriteIntent,
    UpdateRecord, request_digest,
};
use crate::host::settings::actions::SettingsActionTarget;
use crate::host::settings::authority::SettingsOperationOrigin;
use crate::host::settings::contract::{
    SettingKey, SettingsContractError, SettingsOperationState, SettingsRequestBinding,
    SettingsScope,
};
use crate::host::settings::service::SettingsServiceError;

pub(in crate::host::settings) struct PairedReplyHistoryAnchor {
    sequence: u64,
}

impl PairedReplyHistoryAnchor {
    pub(in crate::host::settings) fn capture_on(
        connection: &rusqlite::Connection,
        expected: &super::OperationRecord,
    ) -> Result<Self, SettingsServiceError> {
        if expected.identity.version != 2
            || !matches!(
                expected.identity.origin,
                SettingsOperationOrigin::PairedSettings { .. }
            )
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let request_id = expected.identity.binding.request_id;
        let current =
            super::read(connection, request_id)?.ok_or(SettingsServiceError::CorruptJournal)?;
        let inspection = expected.inspection();
        if current.identity != expected.identity || current.inspection() != inspection {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let mut statement = connection.prepare(
            "SELECT sequence,record,occurred_at FROM settings_operation_updates WHERE request_id=?1 ORDER BY sequence DESC LIMIT 1",
        )?;
        let mut rows = statement.query([request_id.to_string()])?;
        let row = rows.next()?.ok_or(SettingsServiceError::CorruptJournal)?;
        let sequence = u64::try_from(row.get::<_, i64>(0)?)
            .map_err(|_| SettingsServiceError::CorruptJournal)?;
        if sequence == 0 {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let update: UpdateRecord = super::decode(
            super::bounded_text(row, 1, super::MAX_UPDATE_BYTES)?,
            super::MAX_UPDATE_BYTES,
        )?;
        let occurred_at = row.get::<_, i64>(2)?;
        drop(rows);
        drop(statement);
        super::validate_update(&current.identity, &update)?;
        super::validate_native_link(connection, &current.identity, &update)?;
        if update.updated_at.get() as i64 != occurred_at
            || (super::OperationRecord {
                identity: current.identity,
                update,
            })
            .inspection()
                != inspection
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        Ok(Self { sequence })
    }
}

pub(super) fn is_origin(origin: &SettingsOperationOrigin) -> bool {
    matches!(
        origin,
        SettingsOperationOrigin::PairedSettings { .. }
            | SettingsOperationOrigin::PairedSettingsAgent { .. }
    )
}

fn validate_origin(
    origin: &SettingsOperationOrigin,
    binding: &SettingsRequestBinding,
) -> Result<(), SettingsContractError> {
    binding.validate()?;
    let (actor, parent, grant, revision, host_epoch, connection_epoch) = match *origin {
        SettingsOperationOrigin::PairedSettings {
            actor_device_id,
            remote_parent_id,
            grant_id,
            grant_revision,
            host_epoch,
            connection_epoch,
        }
        | SettingsOperationOrigin::PairedSettingsAgent {
            actor_device_id,
            remote_parent_id,
            grant_id,
            grant_revision,
            host_epoch,
            connection_epoch,
            ..
        } => (
            actor_device_id,
            remote_parent_id,
            grant_id,
            grant_revision,
            host_epoch,
            connection_epoch,
        ),
        _ => return Err(SettingsContractError::Invalid("paired operation origin")),
    };
    if actor.as_uuid().is_nil()
        || parent.is_nil()
        || grant.is_nil()
        || revision == 0
        || revision > i64::MAX as u64
        || host_epoch.is_nil()
        || connection_epoch.is_nil()
        || binding.principal_id != *actor.as_uuid()
        || binding.client_session_id != parent
        || binding.request_id.as_uuid().is_nil()
        || binding.expected_revision.is_none()
        || binding.issued_at.get() == 0
        || binding.issued_at.get() > i64::MAX as u64
        || binding.expires_at.get() > i64::MAX as u64
        || !matches!(binding.scope, SettingsScope::Host { host_id } if !host_id.as_uuid().is_nil() && host_id != actor)
        || matches!(origin, SettingsOperationOrigin::PairedSettingsAgent { session_id, .. } if session_id.is_nil())
    {
        return Err(SettingsContractError::Invalid(
            "paired operation provenance",
        ));
    }
    Ok(())
}

pub(super) fn validate_identity(identity: &OperationIdentity) -> Result<(), SettingsServiceError> {
    validate_origin(&identity.origin, &identity.binding)?;
    let SettingsActionTarget::Patch(patch) = &identity.target else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    patch.validate()?;
    if patch.confirmation.is_some()
        || patch.changes.len() > 2
        || !patch
            .changes
            .windows(2)
            .all(|changes| changes[0].key < changes[1].key)
        || !patch.changes.iter().all(|change| memory_key(change.key))
        || identity.binding.request_id != patch.request_id
        || identity.binding.scope != patch.scope
        || identity.binding.expected_revision != Some(patch.expected_revision)
        || identity.binding.exact_request_sha256 != request_digest(&identity.target)?
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

fn memory_key(key: SettingKey) -> bool {
    matches!(
        key,
        SettingKey::EmbeddingsEnabled | SettingKey::EmbeddingsDimensions
    )
}

fn valid_keys(keys: &[SettingKey]) -> bool {
    !keys.is_empty()
        && keys.len() <= 2
        && keys.iter().copied().all(memory_key)
        && keys.windows(2).all(|keys| keys[0] < keys[1])
}

fn validate_phase(
    phase: &SettingsOperationPhase,
    has_write: bool,
) -> Result<(), SettingsContractError> {
    match phase {
        SettingsOperationPhase::Applying { .. } => Ok(()),
        SettingsOperationPhase::Finished { result } => match result.state {
            SettingsOperationState::Applied { .. }
            | SettingsOperationState::PendingApplication { .. }
            | SettingsOperationState::RestartRequired { .. }
                if !has_write =>
            {
                Err(SettingsContractError::Invalid(
                    "paired operation lacks its file write",
                ))
            }
            SettingsOperationState::Completed { .. }
            | SettingsOperationState::RequiresConfirmation { .. }
            | SettingsOperationState::RequiresUserAction { .. } => Err(
                SettingsContractError::Invalid("paired operation native outcome"),
            ),
            _ => Ok(()),
        },
        _ => Err(SettingsContractError::Invalid("paired operation phase")),
    }
}

pub(super) fn validate_inspection(
    inspection: &SettingsOperationInspection,
) -> Result<(), SettingsContractError> {
    validate_origin(&inspection.origin, &inspection.binding)?;
    validate_phase(&inspection.phase, inspection.write.is_some())?;
    if inspection
        .write
        .as_ref()
        .is_some_and(|write| !valid_keys(&write.changed_keys))
    {
        return Err(SettingsContractError::Invalid(
            "paired operation write keys",
        ));
    }
    Ok(())
}

pub(super) fn validate_update(update: &UpdateRecord) -> Result<(), SettingsServiceError> {
    validate_phase(&update.phase, update.write.is_some())?;
    if update.observation.is_some() || update.pairing.is_some() {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

pub(super) fn validate_write_intent(
    intent: &SettingsWriteIntent,
) -> Result<(), SettingsServiceError> {
    if intent.native_request_id.is_some()
        || intent.selection_changed
        || !valid_keys(&intent.changed_keys)
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

impl super::OperationRepository {
    pub(in crate::host::settings) fn begin_paired_write(
        &self,
        identity: OperationIdentity,
        intent: SettingsWriteIntent,
        admission: &crate::host::settings::service::PairedWriteAdmission<'_>,
    ) -> Result<(super::OperationRecord, bool), SettingsServiceError> {
        use crate::protocol::UnixMillis;
        use rusqlite::{TransactionBehavior, params};
        if identity.version != 2
            || !matches!(
                identity.origin,
                SettingsOperationOrigin::PairedSettings { .. }
            )
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        identity.validate()?;
        intent.validate(&identity)?;
        let encoded = super::encode(&identity, super::MAX_IDENTITY_BYTES)?;
        let request_id = identity.binding.request_id;
        let mut main = self.store.connection()?;
        let transaction = main.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prepared = (|| {
            admission.check_unlinked_on(&self.store, &transaction, &identity)?;
            if super::read(&transaction, request_id)?.is_some() {
                return Err(SettingsServiceError::Conflict);
            }
            if let Some(unresolved) = super::unresolved_write(&transaction)? {
                return Err(SettingsServiceError::WriteBlocked {
                    request_id: unresolved.identity.binding.request_id,
                });
            }
            let collision: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM settings_native_requests WHERE request_id=?1)",
                [request_id.to_string()],
                |row| row.get(0),
            )?;
            if collision {
                return Err(SettingsServiceError::Conflict);
            }
            if super::pending_count(&transaction)? >= super::MAX_PENDING {
                return Err(SettingsServiceError::Capacity);
            }
            super::reserve_capacity(&transaction, true, false, 3, Some(request_id))?;
            admission.check_unlinked_on(&self.store, &transaction, &identity)?;
            transaction.execute(
                "INSERT INTO settings_operations(request_id,identity,created_at,expires_at) VALUES (?1,?2,?3,?4)",
                params![request_id.to_string(),encoded,identity.binding.issued_at.get() as i64,identity.binding.expires_at.get() as i64],
            )?;
            admission.check_linked_on(&self.store, &transaction, &identity)?;
            let expected_revision = identity
                .binding
                .expected_revision
                .ok_or(SettingsServiceError::CorruptJournal)?;
            let revision = expected_revision
                .checked_add(1)
                .filter(|revision| *revision <= i64::MAX as u64)
                .ok_or(SettingsServiceError::Conflict)?;
            let update = UpdateRecord {
                application: None,
                write: Some(super::SettingsWriteRecord {
                    intent,
                    resolution: None,
                }),
                pairing: None,
                phase: SettingsOperationPhase::Applying {
                    expected_revision,
                    revision,
                },
                observation: None,
                updated_at: UnixMillis::now()?,
            };
            super::validate_update(&identity, &update)?;
            let sequence = super::append(&transaction, request_id, &update)?;
            admission.check_linked_on(&self.store, &transaction, &identity)?;
            Ok((super::OperationRecord { identity, update }, sequence))
        })();
        let (record, sequence) = match prepared {
            Ok(prepared) => prepared,
            Err(cause) => {
                return match transaction.rollback() {
                    Ok(()) => Err(cause),
                    Err(rollback) => Err(SettingsServiceError::OutcomeUnknown {
                        request_id,
                        cause: Box::new(cause),
                        repair_error: Some(Box::new(rollback.into())),
                    }),
                };
            }
        };
        transaction
            .commit()
            .map_err(|cause| SettingsServiceError::OutcomeUnknown {
                request_id,
                cause: Box::new(cause.into()),
                repair_error: None,
            })?;
        drop(main);
        self.notify_sequence(sequence);
        Ok((record, true))
    }
}

impl PairedReplyHistoryAnchor {
    pub(in crate::host::settings) fn capture_inspection_on(
        connection: &rusqlite::Connection,
        current: &super::OperationRecord,
        remaining: &mut usize,
    ) -> Result<Self, SettingsServiceError> {
        history::check_inspection_history_on(connection, current, None, remaining)?;
        let sequence: i64 = connection.query_row(
            "SELECT sequence FROM settings_operation_updates WHERE request_id=?1 ORDER BY sequence DESC LIMIT 1",
            [current.identity.binding.request_id.to_string()], |row| row.get(0),
        )?;
        let sequence = u64::try_from(sequence)
            .ok()
            .filter(|value| *value > 0)
            .ok_or(SettingsServiceError::CorruptJournal)?;
        Ok(Self { sequence })
    }

    pub(in crate::host::settings) fn check_inspection_current_on(
        &self,
        connection: &rusqlite::Connection,
        current: &super::OperationRecord,
        remaining: &mut usize,
    ) -> Result<(), SettingsServiceError> {
        history::check_inspection_history_on(connection, current, Some(self), remaining)
    }
}
