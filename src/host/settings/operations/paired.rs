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
