use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::memory::embedding::{Embedder, EmbeddingProfile};
use crate::protocol::{CommandId, DeviceId, UnixMillis};
use crate::selector::system_one::SelectionConfig;

use super::{CredentialStorage, ModelCredentialReference};

pub const SETTINGS_CONTRACT_VERSION: u16 = 1;
pub const MAX_SETTINGS_PATCH_BYTES: usize = 16 * 1024;
pub const MAX_SETTINGS_SNAPSHOT_BYTES: usize = 32 * 1024;
pub const MAX_SETTINGS_DEVICE_PAGE: usize = 100;
pub const MAX_SETTINGS_ACTION_LIFETIME_MS: u64 = 300_000;
const MAX_MODEL_BYTES: usize = 256;

#[derive(Debug, Error)]
pub enum SettingsContractError {
    #[error("unsupported settings contract version")]
    Version,
    #[error("invalid settings contract: {0}")]
    Invalid(&'static str),
    #[error("settings contract exceeds its byte bound")]
    TooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SettingKey {
    #[serde(rename = "models.selection")]
    ModelSelection,
    #[serde(rename = "tools.selection")]
    ToolSelection,
    #[serde(rename = "memory.embeddings.enabled")]
    EmbeddingsEnabled,
    #[serde(rename = "memory.embeddings.dimensions")]
    EmbeddingsDimensions,
    #[serde(rename = "memory.embeddings.model")]
    EmbeddingsModel,
    #[serde(rename = "notifications.completion")]
    NotificationsCompletion,
    #[serde(rename = "notifications.attention")]
    NotificationsAttention,
    #[serde(rename = "notifications.authorization")]
    NotificationAuthorization,
}

impl SettingKey {
    pub const ALL: [Self; 8] = [
        Self::ModelSelection,
        Self::ToolSelection,
        Self::EmbeddingsEnabled,
        Self::EmbeddingsDimensions,
        Self::EmbeddingsModel,
        Self::NotificationsCompletion,
        Self::NotificationsAttention,
        Self::NotificationAuthorization,
    ];

    pub fn scope(self) -> SettingScopeKind {
        match self {
            Self::NotificationsCompletion | Self::NotificationsAttention => {
                SettingScopeKind::DevicePreferences
            }
            Self::NotificationAuthorization => SettingScopeKind::ObservedOsState,
            _ => SettingScopeKind::Host,
        }
    }

    pub fn agent_mutability(self) -> AgentMutability {
        match self {
            Self::ModelSelection | Self::ToolSelection => {
                AgentMutability::RequiresNativeConfirmation
            }
            Self::EmbeddingsModel | Self::NotificationAuthorization => AgentMutability::ReadOnly,
            _ => AgentMutability::AllowlistedPreference,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingScopeKind {
    Host,
    DevicePreferences,
    ObservedOsState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsScope {
    Host { host_id: DeviceId },
    DevicePreferences { device_id: DeviceId },
    ObservedOsState { device_id: DeviceId },
}

impl SettingsScope {
    pub fn kind(&self) -> SettingScopeKind {
        match self {
            Self::Host { .. } => SettingScopeKind::Host,
            Self::DevicePreferences { .. } => SettingScopeKind::DevicePreferences,
            Self::ObservedOsState { .. } => SettingScopeKind::ObservedOsState,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMutability {
    AllowlistedPreference,
    RequiresNativeConfirmation,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingSensitivity {
    NonSecretPreference,
    RecipientMetadata,
    ObservedDeviceState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingApplyBehavior {
    SubsequentRun,
    SubsequentSelection,
    StopNewWorkThenApply,
    RebuildRequired,
    ThisDevice,
    ReadOnly,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "url",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsModelEndpoint {
    Official,
    Loopback(String),
}

impl SettingsModelEndpoint {
    pub fn as_native(&self) -> ProviderEndpoint {
        match self {
            Self::Official => ProviderEndpoint::Official,
            Self::Loopback(endpoint) => ProviderEndpoint::Loopback(endpoint.clone()),
        }
    }
}

impl From<&ProviderEndpoint> for SettingsModelEndpoint {
    fn from(endpoint: &ProviderEndpoint) -> Self {
        match endpoint {
            ProviderEndpoint::Official => Self::Official,
            ProviderEndpoint::Loopback(endpoint) => Self::Loopback(endpoint.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: ProviderKind,
    pub endpoint: SettingsModelEndpoint,
    pub model: String,
}

impl ModelSelection {
    fn validate(&self) -> Result<(), SettingsContractError> {
        bounded_text(&self.model, MAX_MODEL_BYTES)?;
        ConversationConfig {
            provider: self.provider,
            endpoint: self.endpoint.as_native(),
            model: self.model.clone(),
            instructions: "Validate settings model selection.".into(),
            max_output_tokens: 4096,
            temperature: None,
        }
        .validate()
        .map_err(|_| SettingsContractError::Invalid("model selection"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolSelectionPreference {
    Local,
    SystemOne { endpoint: String, model: String },
}

impl ToolSelectionPreference {
    fn validate(&self) -> Result<(), SettingsContractError> {
        if let Self::SystemOne { endpoint, model } = self {
            bounded_text(endpoint, 4096)?;
            bounded_text(model, MAX_MODEL_BYTES)?;
            SelectionConfig::system_one(endpoint, model, None)
                .map_err(|_| SettingsContractError::Invalid("System One configuration"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAuthorization {
    NotDetermined,
    Denied,
    Authorized,
    Provisional,
    Ephemeral,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingValue {
    Model { selection: Option<ModelSelection> },
    ToolSelection { selection: ToolSelectionPreference },
    Boolean { value: bool },
    Dimensions { value: Option<u32> },
    EmbeddingModel { model: Option<String> },
    NotificationAuthorization { state: NotificationAuthorization },
}

fn validate_value(key: SettingKey, value: &SettingValue) -> Result<(), SettingsContractError> {
    match (key, value) {
        (SettingKey::ModelSelection, SettingValue::Model { selection }) => {
            if let Some(selection) = selection {
                selection.validate()?;
            }
        }
        (SettingKey::ToolSelection, SettingValue::ToolSelection { selection }) => {
            selection.validate()?;
        }
        (
            SettingKey::EmbeddingsEnabled
            | SettingKey::NotificationsCompletion
            | SettingKey::NotificationsAttention,
            SettingValue::Boolean { .. },
        ) => {}
        (SettingKey::EmbeddingsDimensions, SettingValue::Dimensions { value }) => {
            if value.is_some_and(|dimension| !(1..=65_536).contains(&dimension)) {
                return Err(SettingsContractError::Invalid("dimension range"));
            }
        }
        (SettingKey::EmbeddingsModel, SettingValue::EmbeddingModel { model }) => {
            if let Some(model) = model {
                bounded_text(model, 256)?;
            }
        }
        (SettingKey::NotificationAuthorization, SettingValue::NotificationAuthorization { .. }) => {
        }
        _ => {
            return Err(SettingsContractError::Invalid(
                "value type does not match key",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingConstraints {
    Model {
        max_model_bytes: usize,
    },
    ToolSelection {
        max_endpoint_bytes: usize,
        max_model_bytes: usize,
    },
    Boolean,
    OptionalDimensions {
        choices: Option<Vec<u32>>,
    },
    EmbeddingModel,
    NotificationAuthorization,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingDescriptor {
    pub key: SettingKey,
    pub scope: SettingScopeKind,
    pub constraints: SettingConstraints,
    pub default: SettingValue,
    pub sensitivity: SettingSensitivity,
    pub agent_mutability: AgentMutability,
    pub apply_behavior: SettingApplyBehavior,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingRegistry {
    pub contract_version: u16,
    pub descriptors: Vec<SettingDescriptor>,
}

impl SettingRegistry {
    pub fn from_active_embedder(embedder: Option<&dyn Embedder>) -> Self {
        Self::from_profile(embedder.map(|embedder| embedder.profile()))
    }

    pub fn from_profile(profile: Option<&EmbeddingProfile>) -> Self {
        let choices = profile.map(|profile| {
            profile
                .spec()
                .supported_dimensions()
                .iter()
                .map(|value| *value as u32)
                .collect()
        });
        let descriptors = SettingKey::ALL
            .into_iter()
            .map(|key| {
                let (constraints, default, sensitivity, apply_behavior) = match key {
                    SettingKey::ModelSelection => (
                        SettingConstraints::Model {
                            max_model_bytes: MAX_MODEL_BYTES,
                        },
                        SettingValue::Model { selection: None },
                        SettingSensitivity::RecipientMetadata,
                        SettingApplyBehavior::SubsequentRun,
                    ),
                    SettingKey::ToolSelection => (
                        SettingConstraints::ToolSelection {
                            max_endpoint_bytes: 4096,
                            max_model_bytes: MAX_MODEL_BYTES,
                        },
                        SettingValue::ToolSelection {
                            selection: ToolSelectionPreference::Local,
                        },
                        SettingSensitivity::RecipientMetadata,
                        SettingApplyBehavior::SubsequentSelection,
                    ),
                    SettingKey::EmbeddingsEnabled => (
                        SettingConstraints::Boolean,
                        SettingValue::Boolean { value: false },
                        SettingSensitivity::NonSecretPreference,
                        SettingApplyBehavior::StopNewWorkThenApply,
                    ),
                    SettingKey::EmbeddingsDimensions => (
                        SettingConstraints::OptionalDimensions {
                            choices: choices.clone(),
                        },
                        SettingValue::Dimensions { value: None },
                        SettingSensitivity::NonSecretPreference,
                        SettingApplyBehavior::RebuildRequired,
                    ),
                    SettingKey::EmbeddingsModel => (
                        SettingConstraints::EmbeddingModel,
                        SettingValue::EmbeddingModel { model: None },
                        SettingSensitivity::NonSecretPreference,
                        SettingApplyBehavior::ReadOnly,
                    ),
                    SettingKey::NotificationsCompletion | SettingKey::NotificationsAttention => (
                        SettingConstraints::Boolean,
                        SettingValue::Boolean { value: false },
                        SettingSensitivity::NonSecretPreference,
                        SettingApplyBehavior::ThisDevice,
                    ),
                    SettingKey::NotificationAuthorization => (
                        SettingConstraints::NotificationAuthorization,
                        SettingValue::NotificationAuthorization {
                            state: NotificationAuthorization::Unavailable,
                        },
                        SettingSensitivity::ObservedDeviceState,
                        SettingApplyBehavior::ReadOnly,
                    ),
                };
                SettingDescriptor {
                    key,
                    scope: key.scope(),
                    constraints,
                    default,
                    sensitivity,
                    agent_mutability: key.agent_mutability(),
                    apply_behavior,
                }
            })
            .collect();
        Self {
            contract_version: SETTINGS_CONTRACT_VERSION,
            descriptors,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingChange {
    pub key: SettingKey,
    pub value: SettingValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub contract_version: u16,
    pub request_id: CommandId,
    pub scope: SettingsScope,
    pub expected_revision: u64,
    pub changes: Vec<SettingChange>,
    pub confirmation: Option<SettingsConfirmationReference>,
}

impl SettingsPatch {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        version(self.contract_version)?;
        revision(self.expected_revision)?;
        if self.changes.is_empty() || self.changes.len() > SettingKey::ALL.len() {
            return Err(SettingsContractError::Invalid("patch count"));
        }
        let mut keys = BTreeSet::new();
        for change in &self.changes {
            if !keys.insert(change.key) || change.key.scope() != self.scope.kind() {
                return Err(SettingsContractError::Invalid(
                    "duplicate or cross-scope key",
                ));
            }
            if change.key.agent_mutability() == AgentMutability::ReadOnly {
                return Err(SettingsContractError::Invalid("read-only key"));
            }
            validate_value(change.key, &change.value)?;
        }
        if let Some(reference) = &self.confirmation {
            non_nil(reference.id)?;
        }
        byte_bound(self, MAX_SETTINGS_PATCH_BYTES)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingEntry {
    pub key: SettingKey,
    pub desired: Option<SettingValue>,
    pub effective: Option<SettingValue>,
    pub state: SettingState,
    pub unavailable_reason: Option<SettingsUnavailableReason>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingState {
    Effective,
    PendingApplication,
    RestartRequired,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    pub contract_version: u16,
    pub scope: SettingsScope,
    pub revision: u64,
    pub observed_at: UnixMillis,
    pub stale: bool,
    pub entries: Vec<SettingEntry>,
}

impl SettingsSnapshot {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        version(self.contract_version)?;
        revision(self.revision)?;
        if self.entries.len() > SettingKey::ALL.len() {
            return Err(SettingsContractError::Invalid("snapshot count"));
        }
        let mut keys = BTreeSet::new();
        for entry in &self.entries {
            if !keys.insert(entry.key) || entry.key.scope() != self.scope.kind() {
                return Err(SettingsContractError::Invalid(
                    "duplicate or cross-scope key",
                ));
            }
            for value in [&entry.desired, &entry.effective].into_iter().flatten() {
                validate_value(entry.key, value)?;
            }
            if entry.key.agent_mutability() == AgentMutability::ReadOnly && entry.desired.is_some()
            {
                return Err(SettingsContractError::Invalid(
                    "read-only state cannot have a desired value",
                ));
            }
            if entry.state == SettingState::Effective
                && (entry.effective.is_none()
                    || entry
                        .desired
                        .as_ref()
                        .is_some_and(|desired| entry.effective.as_ref() != Some(desired)))
            {
                return Err(SettingsContractError::Invalid(
                    "inconsistent effective state",
                ));
            }
            if (entry.state == SettingState::Unavailable) != entry.unavailable_reason.is_some() {
                return Err(SettingsContractError::Invalid("unavailable state reason"));
            }
        }
        byte_bound(self, MAX_SETTINGS_SNAPSHOT_BYTES)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingsToolName {
    #[serde(rename = "settings.list")]
    List,
    #[serde(rename = "settings.get")]
    Get,
    #[serde(rename = "settings.update")]
    Update,
    #[serde(rename = "settings.request_action")]
    RequestAction,
    #[serde(rename = "devices.list")]
    DevicesList,
    #[serde(rename = "devices.pair")]
    DevicesPair,
    #[serde(rename = "devices.revoke")]
    DevicesRevoke,
}

impl SettingsToolName {
    pub const ALL: [Self; 7] = [
        Self::List,
        Self::Get,
        Self::Update,
        Self::RequestAction,
        Self::DevicesList,
        Self::DevicesPair,
        Self::DevicesRevoke,
    ];

    pub fn logical_name(self) -> &'static str {
        match self {
            Self::List => "settings.list",
            Self::Get => "settings.get",
            Self::Update => "settings.update",
            Self::RequestAction => "settings.request_action",
            Self::DevicesList => "devices.list",
            Self::DevicesPair => "devices.pair",
            Self::DevicesRevoke => "devices.revoke",
        }
    }

    pub fn provider_name(self) -> &'static str {
        match self {
            Self::List => "settings_list",
            Self::Get => "settings_get",
            Self::Update => "settings_update",
            Self::RequestAction => "settings_request_action",
            Self::DevicesList => "devices_list",
            Self::DevicesPair => "devices_pair",
            Self::DevicesRevoke => "devices_revoke",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsConfirmationReference {
    pub id: Uuid,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsActionReference {
    pub id: Uuid,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsRequestBinding {
    pub request_id: CommandId,
    pub principal_id: Uuid,
    pub client_session_id: Uuid,
    pub scope: SettingsScope,
    pub expected_revision: Option<u64>,
    pub exact_request_sha256: String,
    pub issued_at: UnixMillis,
    pub expires_at: UnixMillis,
}

impl SettingsRequestBinding {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        non_nil(self.principal_id)?;
        non_nil(self.client_session_id)?;
        if let Some(value) = self.expected_revision {
            revision(value)?;
        }
        if self.expires_at.get() <= self.issued_at.get()
            || self.expires_at.get() - self.issued_at.get() > MAX_SETTINGS_ACTION_LIFETIME_MS
            || self.exact_request_sha256.len() != 64
            || !self
                .exact_request_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(SettingsContractError::Invalid("request binding"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsModelCredentialBinding {
    pub provider: ProviderKind,
    pub endpoint: SettingsModelEndpoint,
    pub reference: Uuid,
    pub storage: CredentialStorage,
}

impl SettingsModelCredentialBinding {
    pub fn as_native(&self) -> ModelCredentialReference {
        ModelCredentialReference {
            provider: self.provider,
            endpoint: self.endpoint.as_native(),
            reference: self.reference,
            storage: self.storage,
        }
    }
}

impl From<&ModelCredentialReference> for SettingsModelCredentialBinding {
    fn from(binding: &ModelCredentialReference) -> Self {
        Self {
            provider: binding.provider,
            endpoint: (&binding.endpoint).into(),
            reference: binding.reference,
            storage: binding.storage,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsCredentialBinding {
    Model {
        binding: SettingsModelCredentialBinding,
    },
    SystemOne {
        endpoint: String,
        reference: Uuid,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSection {
    General,
    Models,
    Memory,
    ToolSelection,
    Notifications,
    Devices,
    OwnerSecurity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeSettingsAction {
    CredentialEntry { binding: SettingsCredentialBinding },
    NotificationAuthorization,
    OpenSection { section: SettingsSection },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsActionRequest {
    pub contract_version: u16,
    pub request_id: CommandId,
    pub scope: SettingsScope,
    pub expected_revision: u64,
    pub action: NativeSettingsAction,
}

impl SettingsActionRequest {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        version(self.contract_version)?;
        revision(self.expected_revision)?;
        match &self.action {
            NativeSettingsAction::CredentialEntry { binding } => {
                if self.scope.kind() != SettingScopeKind::Host {
                    return Err(SettingsContractError::Invalid("credential scope"));
                }
                match binding {
                    SettingsCredentialBinding::Model { binding } => binding
                        .as_native()
                        .validate()
                        .map_err(|_| SettingsContractError::Invalid("model credential binding"))?,
                    SettingsCredentialBinding::SystemOne {
                        endpoint,
                        reference,
                    } => {
                        non_nil(*reference)?;
                        bounded_text(endpoint, 4096)?;
                        SelectionConfig::system_one(
                            endpoint,
                            "credential-binding",
                            Some(*reference),
                        )
                        .map_err(|_| {
                            SettingsContractError::Invalid("System One credential binding")
                        })?;
                    }
                }
            }
            NativeSettingsAction::NotificationAuthorization => {
                if self.scope.kind() != SettingScopeKind::DevicePreferences {
                    return Err(SettingsContractError::Invalid("notification action scope"));
                }
            }
            NativeSettingsAction::OpenSection { .. } => {}
        }
        byte_bound(self, MAX_SETTINGS_PATCH_BYTES)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsDeviceAction {
    Pair,
    Revoke {
        device_id: DeviceId,
        public_keys: crate::crypto::DevicePublicKeys,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsDeviceRequest {
    pub contract_version: u16,
    pub request_id: CommandId,
    pub scope: SettingsScope,
    pub action: SettingsDeviceAction,
}

impl SettingsDeviceRequest {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        version(self.contract_version)?;
        if self.scope.kind() != SettingScopeKind::Host {
            return Err(SettingsContractError::Invalid("device management scope"));
        }
        if let SettingsDeviceAction::Revoke {
            device_id,
            public_keys,
        } = &self.action
        {
            if *device_id != public_keys.device_id {
                return Err(SettingsContractError::Invalid("revocation identity"));
            }
            public_keys
                .validate()
                .map_err(|_| SettingsContractError::Invalid("device public keys"))?;
        }
        byte_bound(self, MAX_SETTINGS_PATCH_BYTES)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsDeviceSummary {
    pub device_id: DeviceId,
    pub label: String,
    pub public_keys: crate::crypto::DevicePublicKeys,
    pub revoked: bool,
    pub this_device: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsDevicePage {
    pub contract_version: u16,
    pub host_id: DeviceId,
    pub observed_at: UnixMillis,
    pub devices: Vec<SettingsDeviceSummary>,
    pub next_after: Option<DeviceId>,
}

impl SettingsDevicePage {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        version(self.contract_version)?;
        if self.devices.len() > MAX_SETTINGS_DEVICE_PAGE
            || !self
                .devices
                .windows(2)
                .all(|pair| pair[0].device_id < pair[1].device_id)
            || self.next_after.is_some_and(|id| {
                self.devices
                    .last()
                    .is_none_or(|device| device.device_id != id)
            })
        {
            return Err(SettingsContractError::Invalid("device page or cursor"));
        }
        for device in &self.devices {
            bounded_text(&device.label, 256)?;
            if device.device_id != device.public_keys.device_id {
                return Err(SettingsContractError::Invalid("device identity"));
            }
            device
                .public_keys
                .validate()
                .map_err(|_| SettingsContractError::Invalid("device public keys"))?;
        }
        byte_bound(self, MAX_SETTINGS_SNAPSHOT_BYTES)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsOperationState {
    Applied {
        revision: u64,
    },
    Completed {
        receipt_id: Uuid,
    },
    PendingApplication {
        revision: u64,
    },
    RequiresConfirmation {
        confirmation: SettingsConfirmationReference,
    },
    RequiresUserAction {
        action: SettingsActionReference,
    },
    HostActionRequired,
    RestartRequired {
        revision: u64,
    },
    Conflict {
        observed_revision: u64,
    },
    Unavailable {
        reason: SettingsUnavailableReason,
    },
    Failed {
        reason: SettingsFailureReason,
    },
    Unknown,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsUnavailableReason {
    HostOffline,
    ProviderUnavailable,
    EmbeddingMetadataUnavailable,
    EmbeddingBoundaryUnavailable,
    NativeActionUnavailable,
    PermissionDenied,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsFailureReason {
    InvalidRequest,
    Storage,
    ApplyFailed,
    NativeActionFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsOperationResult {
    pub contract_version: u16,
    pub request_id: CommandId,
    pub scope: SettingsScope,
    pub state: SettingsOperationState,
}

impl SettingsOperationResult {
    pub fn validate(&self) -> Result<(), SettingsContractError> {
        version(self.contract_version)?;
        match &self.state {
            SettingsOperationState::Applied { revision: value }
            | SettingsOperationState::PendingApplication { revision: value }
            | SettingsOperationState::RestartRequired { revision: value }
            | SettingsOperationState::Conflict {
                observed_revision: value,
            } => revision(*value)?,
            SettingsOperationState::Completed { receipt_id } => non_nil(*receipt_id)?,
            SettingsOperationState::RequiresConfirmation { confirmation } => {
                non_nil(confirmation.id)?
            }
            SettingsOperationState::RequiresUserAction { action } => non_nil(action.id)?,
            _ => {}
        }
        byte_bound(self, MAX_SETTINGS_PATCH_BYTES)
    }
}

fn version(value: u16) -> Result<(), SettingsContractError> {
    if value != SETTINGS_CONTRACT_VERSION {
        Err(SettingsContractError::Version)
    } else {
        Ok(())
    }
}

fn revision(value: u64) -> Result<(), SettingsContractError> {
    if value == 0 || value > i64::MAX as u64 {
        Err(SettingsContractError::Invalid("revision"))
    } else {
        Ok(())
    }
}

fn non_nil(value: Uuid) -> Result<(), SettingsContractError> {
    if value.is_nil() {
        Err(SettingsContractError::Invalid("nil reference"))
    } else {
        Ok(())
    }
}

fn bounded_text(value: &str, maximum: usize) -> Result<(), SettingsContractError> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(SettingsContractError::Invalid("text bound"))
    } else {
        Ok(())
    }
}

fn byte_bound(value: &impl Serialize, maximum: usize) -> Result<(), SettingsContractError> {
    let encoded =
        serde_json::to_vec(value).map_err(|_| SettingsContractError::Invalid("encoding"))?;
    if encoded.len() > maximum {
        Err(SettingsContractError::TooLarge)
    } else {
        Ok(())
    }
}
