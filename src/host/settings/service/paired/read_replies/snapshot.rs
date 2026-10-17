use rusqlite::Connection;

use crate::devices::{
    OwnedCommittedSettingsSnapshot, SettingsReadRequest, SettingsRemoteEntry,
    SettingsRemoteModelSelection, SettingsRemoteToolSelection, SettingsRemoteValue,
};
use crate::host::memory::{EmbeddingMetadata, EmbeddingStatus, MemoryWorkerStatus};
use crate::host::selection::SelectionSettings;
use crate::host::settings::HostSettings;
use crate::host::settings::contract::{SettingKey, SettingState, SettingsUnavailableReason};
use crate::host::settings::operations::snapshot_application_state_on;
use crate::host::settings::service::{ServiceCore, SettingsServiceError, persistence, updates};
use crate::selector::system_one::SelectionConfig;

pub(super) struct CapturedSnapshot {
    pub revision: u64,
    pub stale: bool,
    pub entries: Vec<SettingsRemoteEntry>,
}

pub(super) fn capture(
    core: &ServiceCore,
    connection: &Connection,
    request: &OwnedCommittedSettingsSnapshot,
) -> Result<CapturedSnapshot, SettingsServiceError> {
    let source = request.read_owner().verified_request().authority_source();
    let SettingsReadRequest::Snapshot(intent) = source.request() else {
        return Err(SettingsServiceError::OutsideScope);
    };
    let settings = core.settings.observed_snapshot()?;
    let fingerprint = persistence::file_fingerprint(&settings)?;
    let application = snapshot_application_state_on(connection, &fingerprint)?;
    let memory = if intent.keys.iter().any(|key| {
        matches!(
            key,
            SettingKey::EmbeddingsEnabled
                | SettingKey::EmbeddingsDimensions
                | SettingKey::EmbeddingsModel
        )
    }) {
        let composition = core.composition()?;
        Some((
            composition.memory.embedding_metadata()?,
            composition.memory.status().worker,
        ))
    } else {
        None
    };
    let mut entries = Vec::with_capacity(intent.keys.len());
    for key in &intent.keys {
        let entry = match key {
            SettingKey::ModelSelection => SettingsRemoteEntry {
                key: *key,
                default: SettingsRemoteValue::Model { selection: None },
                desired: Some(model(&settings)),
                effective: Some(model(&settings)),
                state: SettingState::Effective,
                unavailable_reason: None,
            },
            SettingKey::ToolSelection => {
                let composition = core.composition()?;
                let current = composition.selection.configuration_status()?;
                let desired = tools(
                    &settings.selection,
                    settings.preference_overrides.system_one_enabled,
                )?;
                let effective = tools(
                    &current.settings,
                    Some(current.remote_enabled && !current.suspended),
                )?;
                let equal = match (&desired, &effective) {
                    (SelectionConfig::Local, SelectionConfig::Local) => true,
                    (SelectionConfig::SystemOne(left), SelectionConfig::SystemOne(right)) => {
                        left.endpoint() == right.endpoint() && left.model() == right.model()
                    }
                    _ => false,
                };
                let mut entry = preference(
                    *key,
                    remote_tools(&desired),
                    remote_tools(&effective),
                    current.suspended,
                    equal,
                );
                if !application.pending_selection && entry.state != SettingState::Effective {
                    entry.state = SettingState::Unavailable;
                    entry.unavailable_reason = Some(SettingsUnavailableReason::ProviderUnavailable);
                }
                entry
            }
            SettingKey::EmbeddingsEnabled
            | SettingKey::EmbeddingsDimensions
            | SettingKey::EmbeddingsModel => {
                let (metadata, worker) =
                    memory.as_ref().ok_or(SettingsServiceError::OutsideScope)?;
                memory_entry(
                    *key,
                    &settings,
                    metadata,
                    worker,
                    application.pending_semantics,
                )?
            }
            _ => return Err(SettingsServiceError::OutsideScope),
        };
        entries.push(entry);
    }
    Ok(CapturedSnapshot {
        revision: settings.revision,
        stale: application.unresolved,
        entries,
    })
}

fn model(settings: &HostSettings) -> SettingsRemoteValue {
    SettingsRemoteValue::Model {
        selection: settings.default_conversation.as_ref().map(|config| {
            SettingsRemoteModelSelection {
                provider: config.provider,
                model: config.model.clone(),
            }
        }),
    }
}

fn tools(
    settings: &SelectionSettings,
    enabled: Option<bool>,
) -> Result<SelectionConfig, SettingsServiceError> {
    if !enabled.unwrap_or(matches!(settings, SelectionSettings::SystemOne { .. })) {
        return Ok(SelectionConfig::Local);
    }
    settings
        .configuration()
        .map_err(|_| SettingsServiceError::Invalid("configured System One selection"))
}

fn remote_tools(settings: &SelectionConfig) -> SettingsRemoteValue {
    SettingsRemoteValue::ToolSelection {
        selection: match settings {
            SelectionConfig::Local => SettingsRemoteToolSelection::Local,
            SelectionConfig::SystemOne(settings) => SettingsRemoteToolSelection::SystemOne {
                model: settings.model().into(),
            },
        },
    }
}

fn preference(
    key: SettingKey,
    desired: SettingsRemoteValue,
    effective: SettingsRemoteValue,
    pending: bool,
    equal: bool,
) -> SettingsRemoteEntry {
    SettingsRemoteEntry {
        key,
        default: match key {
            SettingKey::ToolSelection => SettingsRemoteValue::ToolSelection {
                selection: SettingsRemoteToolSelection::Local,
            },
            SettingKey::EmbeddingsEnabled => SettingsRemoteValue::Boolean { value: false },
            SettingKey::EmbeddingsDimensions => SettingsRemoteValue::Dimensions { value: None },
            _ => SettingsRemoteValue::EmbeddingModel { model: None },
        },
        desired: Some(desired),
        effective: Some(effective),
        state: if !pending && equal {
            SettingState::Effective
        } else {
            SettingState::PendingApplication
        },
        unavailable_reason: None,
    }
}

fn memory_entry(
    key: SettingKey,
    settings: &HostSettings,
    metadata: &EmbeddingMetadata,
    worker: &MemoryWorkerStatus,
    pending_semantics: bool,
) -> Result<SettingsRemoteEntry, SettingsServiceError> {
    let desired_enabled = updates::embeddings_enabled(settings);
    let current_enabled =
        matches!(metadata.status, EmbeddingStatus::Enabled { .. }) && metadata.accepting_semantics;
    let desired_dimensions = updates::embedding_dimensions(settings);
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
    match key {
        SettingKey::EmbeddingsEnabled => {
            let mut entry = preference(
                key,
                SettingsRemoteValue::Boolean {
                    value: desired_enabled,
                },
                SettingsRemoteValue::Boolean {
                    value: current_enabled,
                },
                pending,
                desired_enabled == current_enabled,
            );
            if terminal_failure || !pending && (unavailable || desired_enabled != current_enabled) {
                entry.state = SettingState::Unavailable;
                entry.unavailable_reason =
                    Some(SettingsUnavailableReason::EmbeddingBoundaryUnavailable);
            }
            Ok(entry)
        }
        SettingKey::EmbeddingsDimensions => {
            let Some(profile) = metadata
                .profile
                .as_ref()
                .filter(|_| current_enabled && !terminal_failure)
            else {
                return Ok(SettingsRemoteEntry {
                    key,
                    default: SettingsRemoteValue::Dimensions { value: None },
                    desired: Some(SettingsRemoteValue::Dimensions {
                        value: desired_dimensions,
                    }),
                    effective: None,
                    state: SettingState::Unavailable,
                    unavailable_reason: Some(
                        SettingsUnavailableReason::EmbeddingMetadataUnavailable,
                    ),
                });
            };
            let current = if desired_dimensions.is_none()
                && settings.memory.embeddings.backend_default_dimensions()
                    == Some(profile.dimensions())
            {
                None
            } else {
                Some(profile.dimensions() as u32)
            };
            let mut entry = preference(
                key,
                SettingsRemoteValue::Dimensions {
                    value: desired_dimensions,
                },
                SettingsRemoteValue::Dimensions { value: current },
                pending,
                desired_dimensions == current,
            );
            if !pending && desired_dimensions != current {
                entry.state = SettingState::Unavailable;
                entry.unavailable_reason =
                    Some(SettingsUnavailableReason::EmbeddingBoundaryUnavailable);
            }
            Ok(entry)
        }
        SettingKey::EmbeddingsModel => Ok(SettingsRemoteEntry {
            key,
            default: SettingsRemoteValue::EmbeddingModel { model: None },
            desired: None,
            effective: metadata.profile.as_ref().map(|profile| {
                SettingsRemoteValue::EmbeddingModel {
                    model: Some(profile.spec().model().into()),
                }
            }),
            state: if metadata.profile.is_some() {
                SettingState::Effective
            } else {
                SettingState::Unavailable
            },
            unavailable_reason: metadata
                .profile
                .is_none()
                .then_some(SettingsUnavailableReason::EmbeddingMetadataUnavailable),
        }),
        _ => Err(SettingsServiceError::OutsideScope),
    }
}
