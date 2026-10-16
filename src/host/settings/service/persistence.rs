use sha2::{Digest, Sha256};

use super::super::operations::{
    SettingsFileFingerprint, SettingsWriteIntent, SettingsWriteResolution,
};
use super::super::{HostSettings, MAX_SETTINGS_BYTES, SettingsRepository, read_settings};
use super::SettingsServiceError;

pub(super) struct PreparedSettingsFile {
    pub before: SettingsFileFingerprint,
    pub after: SettingsFileFingerprint,
    pub candidate: HostSettings,
}

pub(super) struct SettingsFileObservation {
    pub resolution: SettingsWriteResolution,
    pub settings: HostSettings,
}

impl SettingsRepository {
    pub(super) fn prepare_file(
        &self,
        candidate: HostSettings,
        expected_revision: u64,
    ) -> Result<PreparedSettingsFile, SettingsServiceError> {
        let current = self
            .current
            .lock()
            .map_err(|_| super::super::SettingsError::Poisoned)?;
        let actual = read_settings(&self.lease)?.ok_or(super::super::SettingsError::Conflict)?;
        actual.validate()?;
        let before = file_fingerprint(&actual)?;
        if before != file_fingerprint(&current)?
            || before.revision != expected_revision
            || candidate.revision != expected_revision
            || expected_revision >= i64::MAX as u64
        {
            return Err(SettingsServiceError::Conflict);
        }
        let mut post = candidate.clone();
        post.revision = expected_revision + 1;
        post.validate()?;
        let after = file_fingerprint(&post)?;
        Ok(PreparedSettingsFile {
            before,
            after,
            candidate,
        })
    }

    pub(super) fn observe_file(
        &self,
        intent: &SettingsWriteIntent,
    ) -> Result<Option<SettingsFileObservation>, SettingsServiceError> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| super::super::SettingsError::Poisoned)?;
        let actual = read_settings(&self.lease)?.ok_or(super::super::SettingsError::Conflict)?;
        actual.validate()?;
        let fingerprint = file_fingerprint(&actual)?;
        let resolution = if fingerprint == intent.before {
            SettingsWriteResolution::NotApplied
        } else if fingerprint == intent.after {
            SettingsWriteResolution::Applied
        } else {
            return Ok(None);
        };
        *current = actual.clone();
        Ok(Some(SettingsFileObservation {
            resolution,
            settings: actual,
        }))
    }

    pub(super) fn observed_snapshot(&self) -> Result<HostSettings, SettingsServiceError> {
        let current = self
            .current
            .lock()
            .map_err(|_| super::super::SettingsError::Poisoned)?;
        let actual = read_settings(&self.lease)?.ok_or(super::super::SettingsError::Conflict)?;
        actual.validate()?;
        if file_fingerprint(&actual)? != file_fingerprint(&current)? {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(actual)
    }
}

pub(super) fn file_fingerprint(
    settings: &HostSettings,
) -> Result<SettingsFileFingerprint, SettingsServiceError> {
    let bytes = serde_json::to_vec(settings).map_err(|_| SettingsServiceError::Encoding)?;
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsServiceError::Capacity);
    }
    let mut digest = Sha256::new();
    digest.update(b"everything.settings.file.v1\0");
    digest.update(&bytes);
    Ok(SettingsFileFingerprint {
        revision: settings.revision,
        sha256: format!("{:x}", digest.finalize()),
    })
}

impl super::SettingsService {
    pub async fn reconcile_write(
        &self,
        caller: super::SettingsCaller,
        operation_id: crate::protocol::CommandId,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<super::super::SettingsOperationInspection, SettingsServiceError> {
        let cleanup = match caller.source {
            super::super::authority::CallerSource::NativeUi(_) => true,
            super::super::authority::CallerSource::LocalOwner { .. } => false,
            super::super::authority::CallerSource::SettingsAgent { .. } => {
                return Err(SettingsServiceError::OutsideScope);
            }
        };
        self.core.authority.validate(&caller, cleanup)?;
        if !cleanup {
            self.core
                .authority
                .require_tool(&caller, super::super::contract::SettingsToolName::Get)?;
        }
        let mutation = tokio::select! {
            biased;
            _=cancellation.cancelled()=>return Err(SettingsServiceError::Cancelled),
            guard=std::sync::Arc::clone(&self.core.mutation).lock_owned()=>guard,
        };
        self.blocking(move |core| {
            let _mutation = mutation;
            core.authority.validate(&caller, cleanup)?;
            if cancellation.is_cancelled() {
                return Err(SettingsServiceError::Cancelled);
            }
            let record = core.repository.get(operation_id)?;
            core.check_principal(&caller, &record.identity)?;
            let write = record.write().ok_or(SettingsServiceError::Conflict)?;
            if write.resolution.is_some() {
                if write.resolution == Some(SettingsWriteResolution::NotApplied) {
                    core.restore_file_admission(operation_id)?;
                }
                core.release_file_resources(&record.identity, &write.intent)?;
                core.authority.validate(&caller, cleanup)?;
                return Ok(record.inspection());
            }
            core.repository
                .unresolved_write()?
                .ok_or(SettingsServiceError::CorruptJournal)?;
            let inspection = core.reconcile_file(&record.identity, &write.intent)?;
            core.authority.validate(&caller, cleanup)?;
            Ok(inspection)
        })
        .await
    }
}

impl super::ServiceCore {
    pub(super) fn reconcile_file(
        &self,
        identity: &super::super::operations::OperationIdentity,
        intent: &SettingsWriteIntent,
    ) -> Result<super::super::SettingsOperationInspection, SettingsServiceError> {
        use super::super::contract::{SettingsFailureReason, SettingsOperationState};
        let observed = self.settings.observe_file(intent);
        let (resolution, state) = match observed {
            Ok(Some(observation)) => {
                let state = match observation.resolution {
                    SettingsWriteResolution::NotApplied => SettingsOperationState::Failed {
                        reason: SettingsFailureReason::ApplyFailed,
                    },
                    SettingsWriteResolution::Applied
                        if intent.memory_changed || intent.selection_changed =>
                    {
                        SettingsOperationState::PendingApplication {
                            revision: observation.settings.revision,
                        }
                    }
                    SettingsWriteResolution::Applied if intent.restart_required => {
                        SettingsOperationState::RestartRequired {
                            revision: observation.settings.revision,
                        }
                    }
                    SettingsWriteResolution::Applied => SettingsOperationState::Applied {
                        revision: observation.settings.revision,
                    },
                };
                (Some(observation.resolution), state)
            }
            Ok(None) => (None, SettingsOperationState::Unknown),
            Err(cause) => {
                let repair = self.repository.finish_write(
                    identity,
                    intent,
                    None,
                    self.result(identity, SettingsOperationState::Unknown),
                );
                return Err(SettingsServiceError::OutcomeUnknown {
                    request_id: identity.binding.request_id,
                    cause: Box::new(cause),
                    repair_error: repair.err().map(Box::new),
                });
            }
        };
        let inspection = self
            .repository
            .finish_write(identity, intent, resolution, self.result(identity, state))
            .map_err(|cause| SettingsServiceError::OutcomeUnknown {
                request_id: identity.binding.request_id,
                cause: Box::new(cause),
                repair_error: None,
            })?;
        if resolution == Some(SettingsWriteResolution::NotApplied) {
            self.restore_file_admission(identity.binding.request_id)?;
        }
        if resolution.is_some() {
            self.release_file_resources(identity, intent)?;
            if !intent.memory_changed && !intent.selection_changed {
                self.applications
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?
                    .remove(&identity.binding.request_id);
            }
        }
        Ok(inspection)
    }
    fn release_file_resources(
        &self,
        identity: &super::super::operations::OperationIdentity,
        intent: &SettingsWriteIntent,
    ) -> Result<(), SettingsServiceError> {
        let cleanup = (|| {
            let claim = self
                .claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .get(&identity.binding.request_id)
                .cloned();
            if let Some(claim) = claim {
                if claim.identity != *identity
                    || Some(claim.native.native_request_id) != intent.native_request_id
                {
                    return Err(SettingsServiceError::Conflict);
                }
                claim.close()?;
                let session = claim
                    .session
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?
                    .take();
                drop(session);
                let removed = {
                    let mut claims = self
                        .claims
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?;
                    if claims
                        .get(&identity.binding.request_id)
                        .is_some_and(|current| std::sync::Arc::ptr_eq(current, &claim))
                    {
                        claims.remove(&identity.binding.request_id)
                    } else {
                        None
                    }
                };
                drop(removed);
            }
            let retained = {
                let mut active = self
                    .active_write
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?;
                if let Some(current) = active.as_ref()
                    && current.request_id == identity.binding.request_id
                {
                    if current.client_session_id != identity.binding.client_session_id {
                        return Err(SettingsServiceError::Conflict);
                    }
                    active.take()
                } else {
                    None
                }
            };
            drop(retained);
            Ok::<(), SettingsServiceError>(())
        })();
        cleanup.map_err(|cause| SettingsServiceError::OutcomeUnknown {
            request_id: identity.binding.request_id,
            cause: Box::new(cause),
            repair_error: None,
        })
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerSettingsWriteResult {
    pub operation: super::super::SettingsOperationInspection,
    pub settings: Option<Box<HostSettings>>,
    pub restart_required: bool,
}

impl super::ServiceCore {
    pub(super) fn owner_receipt(
        &self,
        record: &super::super::operations::OperationRecord,
    ) -> Result<OwnerSettingsWriteResult, SettingsServiceError> {
        let super::super::actions::SettingsActionTarget::OwnerReplacement(request) =
            &record.identity.target
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if !matches!(
            record.identity.origin,
            super::super::authority::SettingsOperationOrigin::LocalOwner { .. }
        ) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let settings = match record.write() {
            Some(write) if write.resolution == Some(SettingsWriteResolution::Applied) => {
                match self.settings.observed_snapshot().and_then(|current| {
                    Ok(if file_fingerprint(&current)? == write.intent.after {
                        Some(Box::new(current))
                    } else {
                        None
                    })
                }) {
                    Ok(current) => current,
                    Err(_) => {
                        tracing::warn!(operation_id=%record.identity.binding.request_id,"current settings snapshot unavailable for original retained receipt");
                        None
                    }
                }
            }
            _ => None,
        };
        Ok(OwnerSettingsWriteResult {
            operation: record.inspection(),
            settings,
            restart_required: request.restart_required
                && record.write().is_some_and(|write| {
                    write.resolution == Some(SettingsWriteResolution::Applied)
                }),
        })
    }
}
