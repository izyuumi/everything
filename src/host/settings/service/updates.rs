use crate::agent::ConversationConfig;
use crate::host::selection::SelectionSettings;
use crate::memory::embedding::EmbeddingProfile;
use crate::selector::system_one::SelectionConfig;

use super::super::contract::{
    ModelSelection, SettingKey, SettingValue, SettingsPatch, ToolSelectionPreference,
};
use super::super::{EmbeddingDimensionsOverride, HostSettings};
use super::SettingsServiceError;

pub(super) fn patch_candidate(
    current: &HostSettings,
    patch: &SettingsPatch,
    profile: Option<&EmbeddingProfile>,
) -> Result<HostSettings, SettingsServiceError> {
    patch.validate()?;
    if patch.confirmation.is_some() || current.revision != patch.expected_revision {
        return Err(SettingsServiceError::Conflict);
    }
    let mut candidate = current.clone();
    for change in &patch.changes {
        match (&change.key, &change.value) {
            (SettingKey::ModelSelection, SettingValue::Model { selection }) => {
                candidate.default_conversation = match selection {
                    None => None,
                    Some(selection) => {
                        let mut configuration = current
                            .default_conversation
                            .clone()
                            .unwrap_or_else(|| ConversationConfig {
                                provider: selection.provider,
                                endpoint: selection.endpoint.as_native(),
                                model: selection.model.clone(),
                                instructions: "You are Vorra, a helpful assistant.".into(),
                                max_output_tokens: 4096,
                                temperature: None,
                            });
                        configuration.provider = selection.provider;
                        configuration.endpoint = selection.endpoint.as_native();
                        configuration.model = selection.model.clone();
                        Some(configuration)
                    }
                };
            }
            (SettingKey::ToolSelection, SettingValue::ToolSelection { selection }) => {
                match selection {
                    ToolSelectionPreference::Local => {
                        candidate.preference_overrides.system_one_enabled = Some(false);
                    }
                    ToolSelectionPreference::SystemOne { endpoint, model } => {
                        let SelectionConfig::SystemOne(selected) =
                            SelectionConfig::system_one(endpoint, model, None).map_err(|_| {
                                SettingsServiceError::Invalid("System One selection")
                            })?
                        else {
                            return Err(SettingsServiceError::Invalid("System One selection"));
                        };
                        let credential_reference =
                            match current.selection.configuration().map_err(|_| {
                                SettingsServiceError::Invalid("configured System One selection")
                            })? {
                                SelectionConfig::SystemOne(original)
                                    if original.endpoint() == selected.endpoint() =>
                                {
                                    match &current.selection {
                                        SelectionSettings::SystemOne {
                                            credential_reference,
                                            ..
                                        } => *credential_reference,
                                        SelectionSettings::Local => None,
                                    }
                                }
                                _ => None,
                            };
                        candidate.selection = SelectionSettings::SystemOne {
                            endpoint: selected.endpoint().into(),
                            model: selected.model().into(),
                            credential_reference,
                        };
                        candidate.preference_overrides.system_one_enabled = Some(true);
                    }
                }
            }
            (SettingKey::EmbeddingsEnabled, SettingValue::Boolean { value }) => {
                candidate.preference_overrides.embeddings_enabled = Some(*value);
            }
            (SettingKey::EmbeddingsDimensions, SettingValue::Dimensions { value }) => {
                let profile = profile.ok_or(SettingsServiceError::EmbeddingMetadataUnavailable)?;
                candidate.preference_overrides.embedding_dimensions = match value {
                    None => EmbeddingDimensionsOverride::BackendDefault,
                    Some(dimensions)
                        if profile
                            .spec()
                            .supported_dimensions()
                            .contains(&(*dimensions as usize)) =>
                    {
                        EmbeddingDimensionsOverride::Selected {
                            dimensions: *dimensions,
                        }
                    }
                    Some(_) => {
                        return Err(SettingsServiceError::Invalid(
                            "unsupported embedding dimension",
                        ));
                    }
                };
            }
            _ => return Err(SettingsServiceError::OutsideScope),
        }
    }
    candidate.validate()?;
    Ok(candidate)
}

pub(super) fn model_selection(settings: &HostSettings) -> Option<ModelSelection> {
    settings
        .default_conversation
        .as_ref()
        .map(|configuration| ModelSelection {
            provider: configuration.provider,
            endpoint: (&configuration.endpoint).into(),
            model: configuration.model.clone(),
        })
}

pub(super) fn tool_selection(
    settings: &HostSettings,
) -> Result<ToolSelectionPreference, SettingsServiceError> {
    if !settings
        .preference_overrides
        .system_one_enabled
        .unwrap_or(matches!(
            settings.selection,
            SelectionSettings::SystemOne { .. }
        ))
    {
        return Ok(ToolSelectionPreference::Local);
    }
    match settings
        .selection
        .configuration()
        .map_err(|_| SettingsServiceError::Invalid("configured System One selection"))?
    {
        SelectionConfig::Local => Ok(ToolSelectionPreference::Local),
        SelectionConfig::SystemOne(settings) => Ok(ToolSelectionPreference::SystemOne {
            endpoint: settings.endpoint().into(),
            model: settings.model().into(),
        }),
    }
}

pub(super) struct OwnerDifference {
    pub changed_keys: Vec<SettingKey>,
    pub memory_changed: bool,
    pub selection_changed: bool,
    pub protected: bool,
    pub restart_required: bool,
}

pub(super) fn embeddings_enabled(settings: &HostSettings) -> bool {
    settings
        .preference_overrides
        .embeddings_enabled
        .unwrap_or(!matches!(
            settings.memory.embeddings,
            crate::host::memory::EmbeddingSettings::Disabled
        ))
}

pub(super) fn embedding_dimensions(settings: &HostSettings) -> Option<u32> {
    match settings.preference_overrides.embedding_dimensions {
        EmbeddingDimensionsOverride::BackendDefault => None,
        EmbeddingDimensionsOverride::Selected { dimensions } => Some(dimensions),
        EmbeddingDimensionsOverride::Inherit => match &settings.memory.embeddings {
            crate::host::memory::EmbeddingSettings::Disabled => None,
            crate::host::memory::EmbeddingSettings::EmbeddingGemma2(configuration) => {
                Some(configuration.dimensions as u32)
            }
        },
    }
}

pub(super) fn owner_difference(
    before: &HostSettings,
    after: &HostSettings,
) -> Result<OwnerDifference, SettingsServiceError> {
    let model_changed = model_selection(before) != model_selection(after);
    let tool_changed = tool_selection(before)? != tool_selection(after)?;
    let mut changed_keys = Vec::new();
    if model_changed {
        changed_keys.push(SettingKey::ModelSelection);
    }
    if tool_changed {
        changed_keys.push(SettingKey::ToolSelection);
    }
    if embeddings_enabled(before) != embeddings_enabled(after) {
        changed_keys.push(SettingKey::EmbeddingsEnabled);
    }
    if embedding_dimensions(before) != embedding_dimensions(after) {
        changed_keys.push(SettingKey::EmbeddingsDimensions);
    }
    let memory_changed = !same_value(&before.memory.embeddings, &after.memory.embeddings)?
        || before.preference_overrides.embeddings_enabled
            != after.preference_overrides.embeddings_enabled
        || before.preference_overrides.embedding_dimensions
            != after.preference_overrides.embedding_dimensions;
    let selection_changed = before.selection != after.selection
        || before.preference_overrides.system_one_enabled
            != after.preference_overrides.system_one_enabled;
    let restart_required = before
        .memory
        .requires_restart(&after.memory)
        .map_err(|_| SettingsServiceError::Encoding)?
        || !same_value(&before.mcp, &after.mcp)?
        || before.worker != after.worker
        || before.relay_origin != after.relay_origin;
    let configured_selection_before = configured_selection(before)?;
    let configured_selection_after = configured_selection(after)?;
    Ok(OwnerDifference {
        changed_keys,
        memory_changed,
        selection_changed,
        protected: model_changed
            || tool_changed
            || configured_selection_before != configured_selection_after,
        restart_required,
    })
}

fn configured_selection(
    settings: &HostSettings,
) -> Result<ToolSelectionPreference, SettingsServiceError> {
    match settings
        .selection
        .configuration()
        .map_err(|_| SettingsServiceError::Invalid("configured System One selection"))?
    {
        SelectionConfig::Local => Ok(ToolSelectionPreference::Local),
        SelectionConfig::SystemOne(settings) => Ok(ToolSelectionPreference::SystemOne {
            endpoint: settings.endpoint().into(),
            model: settings.model().into(),
        }),
    }
}

fn same_value(
    left: &impl serde::Serialize,
    right: &impl serde::Serialize,
) -> Result<bool, SettingsServiceError> {
    let left = serde_json::to_value(left).map_err(|_| SettingsServiceError::Encoding)?;
    let right = serde_json::to_value(right).map_err(|_| SettingsServiceError::Encoding)?;
    Ok(left == right)
}

pub(super) fn embedding_preferences(
    settings: &HostSettings,
) -> crate::host::memory::EmbeddingPreferences {
    crate::host::memory::EmbeddingPreferences {
        configured: settings.memory.embeddings.clone(),
        enabled: embeddings_enabled(settings),
        dimensions: match settings.preference_overrides.embedding_dimensions {
            EmbeddingDimensionsOverride::Inherit => {
                crate::host::memory::EmbeddingDimensionChoice::Inherit
            }
            EmbeddingDimensionsOverride::BackendDefault => {
                crate::host::memory::EmbeddingDimensionChoice::BackendDefault
            }
            EmbeddingDimensionsOverride::Selected { dimensions } => {
                crate::host::memory::EmbeddingDimensionChoice::Selected { dimensions }
            }
        },
    }
}

pub(super) fn selection_enabled(settings: &HostSettings) -> bool {
    settings
        .preference_overrides
        .system_one_enabled
        .unwrap_or(matches!(
            settings.selection,
            SelectionSettings::SystemOne { .. }
        ))
}
