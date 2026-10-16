use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::crypto::DevicePublicKeys;
use crate::protocol::{CommandId, DeviceId, UnixMillis};

use super::contract::{SettingKey, SettingsContractError};

pub(crate) const MAX_SETTINGS_GRANT_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub(crate) const MAX_SETTINGS_GRANT_BYTES: usize = 8192;
pub(crate) const MAX_SETTINGS_GRANT_RECEIPT_BYTES: usize = 512;
pub(crate) const SETTINGS_GRANT_REVOKE_RESERVE_BYTES: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SettingsGrantOwner {
    pub host_id: DeviceId,
    pub principal_id: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGrantReference {
    pub grant_id: Uuid,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsGrantConsent {
    None,
    ConfiguredProvider {
        provider: ProviderKind,
        model: String,
        recipient: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsGrantState {
    Revoked,
    Active {
        readable_keys: Vec<SettingKey>,
        writable_keys: Vec<SettingKey>,
        session_consent: SettingsGrantConsent,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGrantTarget {
    pub grant_id: Uuid,
    pub issued_at: UnixMillis,
    pub expires_at: UnixMillis,
    pub state: SettingsGrantState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGrantChange {
    pub version: u16,
    pub request_id: CommandId,
    pub peer_keys: DevicePublicKeys,
    pub expected: Option<SettingsGrantReference>,
    pub rotate: bool,
    pub configured_settings_revision: Option<u64>,
    pub target: SettingsGrantTarget,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGrantVersion {
    pub version: u16,
    pub host_id: DeviceId,
    pub owner_principal_id: Uuid,
    pub revision: u64,
    pub change: SettingsGrantChange,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGrantReceipt {
    pub version: u16,
    pub host_id: DeviceId,
    pub device_id: DeviceId,
    pub owner_request_id: CommandId,
    pub grant_id: Uuid,
    pub revision: u64,
    pub request_sha256: String,
    pub committed_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SettingsGrantCommit {
    pub receipt: SettingsGrantReceipt,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGrantHead {
    pub grant: SettingsGrantVersion,
    pub peer_revoked: bool,
    pub host_revoked: bool,
}

impl SettingsGrantOwner {
    pub(crate) fn validate(self) -> Result<(), SettingsContractError> {
        if self.principal_id.is_nil() || self.principal_id != *self.host_id.as_uuid() {
            return Err(SettingsContractError::Invalid("Settings grant owner"));
        }
        Ok(())
    }
}

impl SettingsGrantReference {
    pub(crate) fn validate(self) -> Result<(), SettingsContractError> {
        if self.grant_id.is_nil() || self.revision == 0 || self.revision > i64::MAX as u64 {
            return Err(SettingsContractError::Invalid("Settings grant reference"));
        }
        Ok(())
    }
}

impl SettingsGrantChange {
    pub(crate) fn validate(&self) -> Result<(), SettingsContractError> {
        if self.version != 1 || self.target.grant_id.is_nil() {
            return Err(SettingsContractError::Invalid("Settings grant identity"));
        }
        self.peer_keys
            .validate()
            .map_err(|_| SettingsContractError::Invalid("Settings grant peer keys"))?;
        if let Some(expected) = self.expected {
            expected.validate()?;
            if self.rotate != (self.target.grant_id != expected.grant_id) {
                return Err(SettingsContractError::Invalid("Settings grant rotation"));
            }
        } else if self.rotate {
            return Err(SettingsContractError::Invalid(
                "Settings grant creation rotation",
            ));
        }
        let issued = self.target.issued_at.get();
        let expires = self.target.expires_at.get();
        if issued == 0 || expires <= issued || expires - issued > MAX_SETTINGS_GRANT_LIFETIME_MS {
            return Err(SettingsContractError::Invalid("Settings grant lifetime"));
        }
        if let SettingsGrantState::Active {
            readable_keys,
            writable_keys,
            session_consent,
        } = &self.target.state
        {
            if readable_keys.len() > 5
                || writable_keys.len() > 2
                || !readable_keys.windows(2).all(|pair| pair[0] < pair[1])
                || !writable_keys.windows(2).all(|pair| pair[0] < pair[1])
                || readable_keys.iter().any(|key| !readable_key(*key))
                || writable_keys.iter().any(|key| {
                    !matches!(
                        key,
                        SettingKey::EmbeddingsEnabled | SettingKey::EmbeddingsDimensions
                    ) || !readable_keys.contains(key)
                })
            {
                return Err(SettingsContractError::Invalid("Settings grant keys"));
            }
            session_consent.validate()?;
        }
        let configured_consent = matches!(
            self.target.state,
            SettingsGrantState::Active {
                session_consent: SettingsGrantConsent::ConfiguredProvider { .. },
                ..
            }
        );
        match (self.configured_settings_revision, configured_consent) {
            (Some(revision), true) if revision > 0 && revision <= i64::MAX as u64 => {}
            (None, false) => {}
            _ => {
                return Err(SettingsContractError::Invalid(
                    "Settings grant configured settings revision",
                ));
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| SettingsContractError::Invalid("Settings grant encoding"))?
            .len()
            > MAX_SETTINGS_GRANT_BYTES
        {
            return Err(SettingsContractError::TooLarge);
        }
        Ok(())
    }
}

impl SettingsGrantVersion {
    pub(crate) fn validate(&self) -> Result<(), SettingsContractError> {
        self.change.validate()?;
        SettingsGrantOwner {
            host_id: self.host_id,
            principal_id: self.owner_principal_id,
        }
        .validate()?;
        if self.version != 1 || self.host_id == self.change.peer_keys.device_id {
            return Err(SettingsContractError::Invalid("Settings grant Host"));
        }
        let expected_revision = match self.change.expected {
            None => Some(1),
            Some(expected) => expected.revision.checked_add(1),
        };
        if expected_revision != Some(self.revision)
            || self.revision > i64::MAX as u64
            || (self.revision == i64::MAX as u64
                && matches!(self.change.target.state, SettingsGrantState::Active { .. }))
        {
            return Err(SettingsContractError::Invalid("Settings grant revision"));
        }
        if serde_json::to_vec(self)
            .map_err(|_| SettingsContractError::Invalid("Settings grant version encoding"))?
            .len()
            > MAX_SETTINGS_GRANT_BYTES
        {
            return Err(SettingsContractError::TooLarge);
        }
        Ok(())
    }
}

impl SettingsGrantReceipt {
    pub(crate) fn validate(&self) -> Result<(), SettingsContractError> {
        if self.version != 1
            || self.host_id == self.device_id
            || self.grant_id.is_nil()
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.committed_at.get() == 0
            || self.request_sha256.len() != 64
            || !self
                .request_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || serde_json::to_vec(self)
                .map_err(|_| SettingsContractError::Invalid("Settings grant receipt encoding"))?
                .len()
                > MAX_SETTINGS_GRANT_RECEIPT_BYTES
        {
            return Err(SettingsContractError::Invalid("Settings grant receipt"));
        }
        Ok(())
    }

    pub(crate) fn matches_change(&self, change: &SettingsGrantChange) -> bool {
        if self.validate().is_err() || change.validate().is_err() {
            return false;
        }
        let revision = change
            .expected
            .map_or(Some(1), |expected| expected.revision.checked_add(1));
        let Ok(canonical) = serde_json::to_string(change) else {
            return false;
        };
        self.owner_request_id == change.request_id
            && self.device_id == change.peer_keys.device_id
            && self.grant_id == change.target.grant_id
            && Some(self.revision) == revision
            && self.request_sha256 == change_digest(&canonical)
            && self.committed_at >= change.target.issued_at
            && self.committed_at < change.target.expires_at
    }
}

impl SettingsGrantHead {
    pub(crate) fn validate(&self) -> Result<(), SettingsContractError> {
        self.grant.validate()
    }
}

impl SettingsGrantConsent {
    fn validate(&self) -> Result<(), SettingsContractError> {
        let Self::ConfiguredProvider {
            provider,
            model,
            recipient,
        } = self
        else {
            return Ok(());
        };
        if model.trim().is_empty()
            || model.len() > 256
            || model.contains('\0')
            || recipient.len() > 4096
            || recipient.contains('\0')
        {
            return Err(SettingsContractError::Invalid(
                "Settings grant provider consent",
            ));
        }
        let official = match provider {
            ProviderKind::OpenAi => "https://api.openai.com/v1/responses",
            ProviderKind::Anthropic => "https://api.anthropic.com/v1/messages",
        };
        let endpoint = if recipient == official {
            ProviderEndpoint::Official
        } else {
            let endpoint = ProviderEndpoint::loopback(recipient)
                .map_err(|_| SettingsContractError::Invalid("Settings grant recipient"))?;
            if endpoint != ProviderEndpoint::Loopback(recipient.clone()) {
                return Err(SettingsContractError::Invalid(
                    "Settings grant canonical recipient",
                ));
            }
            endpoint
        };
        ConversationConfig {
            provider: *provider,
            endpoint,
            model: model.clone(),
            instructions: "Validate Settings grant consent.".into(),
            max_output_tokens: 4096,
            temperature: None,
        }
        .validate()
        .map_err(|_| SettingsContractError::Invalid("Settings grant model"))
    }
}

fn readable_key(key: SettingKey) -> bool {
    matches!(
        key,
        SettingKey::ModelSelection
            | SettingKey::ToolSelection
            | SettingKey::EmbeddingsEnabled
            | SettingKey::EmbeddingsDimensions
            | SettingKey::EmbeddingsModel
    )
}

pub(crate) fn change_digest(canonical: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"everything.settings-grant.owner-change.v1\0");
    digest.update(canonical.as_bytes());
    format!("{:x}", digest.finalize())
}
