use crate::host::memory::{EmbeddingMetadata, EmbeddingStatus, MemoryWorkerStatus};
use crate::protocol::UnixMillis;

use super::super::HostSettings;
use super::super::authority::SettingsCaller;
use super::super::contract::{
    SETTINGS_CONTRACT_VERSION, SettingEntry, SettingKey, SettingRegistry, SettingState,
    SettingValue, SettingsScope, SettingsSnapshot, SettingsToolName, SettingsUnavailableReason,
};
use super::{ServiceCore, SettingsService, SettingsServiceError, updates};

impl SettingsService {
    pub async fn describe(
        &self,
        caller: SettingsCaller,
    ) -> Result<SettingRegistry, SettingsServiceError> {
        let _work = self.invocation_guard(&caller, SettingsToolName::List, None)?;
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::List)?;
        self.blocking(move |core| {
            core.authority
                .require_tool(&caller, SettingsToolName::List)?;
            let metadata = core.composition()?.memory.embedding_metadata()?;
            let registry = SettingRegistry::from_profile(metadata.profile.as_ref());
            core.authority
                .require_tool(&caller, SettingsToolName::List)?;
            Ok(registry)
        })
        .await
    }

    pub async fn get(
        &self,
        caller: SettingsCaller,
        scope: SettingsScope,
    ) -> Result<SettingsSnapshot, SettingsServiceError> {
        let _work = self.invocation_guard(&caller, SettingsToolName::Get, None)?;
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::Get)?;
        self.core.check_scope(&scope)?;
        if !matches!(scope, SettingsScope::Host { .. }) {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.blocking(move |core| {
            core.authority
                .require_tool(&caller, SettingsToolName::Get)?;
            let settings = core.settings.observed_snapshot()?;
            let composition = core.composition()?;
            let metadata = composition.memory.embedding_metadata()?;
            let selection = composition.selection.configuration_status()?;
            let mut effective_selection = settings.clone();
            effective_selection.selection = selection.settings;
            effective_selection.preference_overrides.system_one_enabled =
                Some(selection.remote_enabled && !selection.suspended);
            let desired_selection = updates::tool_selection(&settings)?;
            let current_selection = updates::tool_selection(&effective_selection)?;
            let model = SettingValue::Model {
                selection: updates::model_selection(&settings),
            };
            let mut entries = vec![
                effective(SettingKey::ModelSelection, Some(model.clone()), model),
                preference(
                    SettingKey::ToolSelection,
                    SettingValue::ToolSelection {
                        selection: desired_selection,
                    },
                    SettingValue::ToolSelection {
                        selection: current_selection,
                    },
                    selection.suspended,
                ),
            ];
            let fingerprint = super::persistence::file_fingerprint(&settings)?;
            let (pending_semantics, pending_selection) = core
                .repository
                .pending_applications()?
                .into_iter()
                .filter_map(|record| {
                    record
                        .write()
                        .filter(|write| write.intent.after == fingerprint)
                        .map(|write| (write.intent.memory_changed, write.intent.selection_changed))
                })
                .fold((false, false), |current, pending| {
                    (current.0 || pending.0, current.1 || pending.1)
                });
            if !pending_selection && entries[1].state != SettingState::Effective {
                entries[1].state = SettingState::Unavailable;
                entries[1].unavailable_reason =
                    Some(SettingsUnavailableReason::ProviderUnavailable);
            }
            entries.extend(memory_entries(
                &settings,
                &metadata,
                &composition.memory.status().worker,
                pending_semantics,
            ));
            let snapshot = SettingsSnapshot {
                contract_version: SETTINGS_CONTRACT_VERSION,
                scope,
                revision: settings.revision,
                observed_at: UnixMillis::now()?,
                stale: core.repository.unresolved_write()?.is_some(),
                entries,
            };
            snapshot.validate()?;
            core.authority
                .require_tool(&caller, SettingsToolName::Get)?;
            Ok(snapshot)
        })
        .await
    }
}

impl ServiceCore {
    pub(super) fn composition(
        &self,
    ) -> Result<std::sync::Arc<crate::host::composition::Composition>, SettingsServiceError> {
        self.composition
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .clone()
            .ok_or(SettingsServiceError::Invalid(
                "settings composition is not bound",
            ))
    }
}

fn memory_entries(
    settings: &HostSettings,
    metadata: &EmbeddingMetadata,
    worker: &MemoryWorkerStatus,
    pending_semantics: bool,
) -> Vec<SettingEntry> {
    let desired_enabled = updates::embeddings_enabled(settings);
    let current_enabled =
        matches!(metadata.status, EmbeddingStatus::Enabled { .. }) && metadata.accepting_semantics;
    let desired_dimensions = SettingValue::Dimensions {
        value: updates::embedding_dimensions(settings),
    };
    let unavailable = matches!(metadata.status, EmbeddingStatus::Unavailable { .. });
    let current_application = metadata
        .application
        .as_ref()
        .filter(|application| application.generation == metadata.generation);
    let pending = pending_semantics
        || current_application.is_some_and(|application| {
            matches!(
                application.phase,
                crate::host::memory::EmbeddingApplyPhase::Pending { .. }
            )
        });
    let terminal_failure = current_application.is_some_and(|application| {
        matches!(
            application.phase,
            crate::host::memory::EmbeddingApplyPhase::Unavailable { .. }
                | crate::host::memory::EmbeddingApplyPhase::Failed { .. }
                | crate::host::memory::EmbeddingApplyPhase::Stopped
                | crate::host::memory::EmbeddingApplyPhase::Superseded
        )
    }) || matches!(worker, MemoryWorkerStatus::Stopped { .. });
    let mut enabled = preference(
        SettingKey::EmbeddingsEnabled,
        SettingValue::Boolean {
            value: desired_enabled,
        },
        SettingValue::Boolean {
            value: current_enabled,
        },
        pending,
    );
    if terminal_failure || !pending && (unavailable || desired_enabled != current_enabled) {
        enabled.state = SettingState::Unavailable;
        enabled.unavailable_reason = Some(SettingsUnavailableReason::EmbeddingBoundaryUnavailable);
    }
    let dimension = match metadata.profile.as_ref() {
        Some(profile) if current_enabled && !terminal_failure => {
            let selected = profile.dimensions() as u32;
            let value = if updates::embedding_dimensions(settings).is_none()
                && settings.memory.embeddings.backend_default_dimensions()
                    == Some(profile.dimensions())
            {
                None
            } else {
                Some(selected)
            };
            let current = SettingValue::Dimensions { value };
            if !pending && desired_dimensions != current {
                SettingEntry {
                    key: SettingKey::EmbeddingsDimensions,
                    desired: Some(desired_dimensions),
                    effective: Some(current),
                    state: SettingState::Unavailable,
                    unavailable_reason: Some(
                        SettingsUnavailableReason::EmbeddingBoundaryUnavailable,
                    ),
                }
            } else {
                preference(
                    SettingKey::EmbeddingsDimensions,
                    desired_dimensions,
                    current,
                    pending,
                )
            }
        }
        _ => SettingEntry {
            key: SettingKey::EmbeddingsDimensions,
            desired: Some(desired_dimensions),
            effective: None,
            state: SettingState::Unavailable,
            unavailable_reason: Some(SettingsUnavailableReason::EmbeddingMetadataUnavailable),
        },
    };
    let model = match metadata.profile.as_ref() {
        Some(profile) => effective(
            SettingKey::EmbeddingsModel,
            None,
            SettingValue::EmbeddingModel {
                model: Some(profile.spec().model().into()),
            },
        ),
        None => SettingEntry {
            key: SettingKey::EmbeddingsModel,
            desired: None,
            effective: None,
            state: SettingState::Unavailable,
            unavailable_reason: Some(SettingsUnavailableReason::EmbeddingMetadataUnavailable),
        },
    };
    vec![enabled, dimension, model]
}

fn effective(key: SettingKey, desired: Option<SettingValue>, value: SettingValue) -> SettingEntry {
    SettingEntry {
        key,
        desired,
        effective: Some(value),
        state: SettingState::Effective,
        unavailable_reason: None,
    }
}

fn preference(
    key: SettingKey,
    desired: SettingValue,
    value: SettingValue,
    pending: bool,
) -> SettingEntry {
    let state = if !pending && desired == value {
        SettingState::Effective
    } else {
        SettingState::PendingApplication
    };
    SettingEntry {
        key,
        desired: Some(desired),
        effective: Some(value),
        state,
        unavailable_reason: None,
    }
}
