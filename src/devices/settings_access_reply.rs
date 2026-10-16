use std::fmt;

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::host::settings::contract::{SettingKey, SettingsContractError};
use crate::host::settings::{SettingsGrantConsent, SettingsGrantHead, SettingsGrantState};
use crate::protocol::{DeviceId, MessageId, SchemaVersion, UnixMillis};

use super::BindingGeneration;

const MAX_ACCESS_PACKET_BYTES: usize = 32_768;
const MAX_DENIAL_PACKET_BYTES: usize = 2048;

#[derive(Serialize)]
pub(crate) struct SettingsAccessReplyPacket {
    schema_version: SchemaVersion,
    payload: AccessReplyPayload,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AccessReplyPayload {
    Settings {
        version: u16,
        host_id: DeviceId,
        message: AccessReplyMessage,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AccessReplyMessage {
    Response {
        in_reply_to: MessageId,
        correlation: QueryCorrelation,
        generation: Option<BindingGeneration>,
        body: AccessReplyBody,
    },
}

#[derive(Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
enum QueryCorrelation {
    Query { query_id: Uuid },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AccessReplyBody {
    AccessStatus { summary: AccessSummary },
    Denied { reason: AccessDenialReason },
}

#[derive(Serialize)]
struct AccessSummary {
    generation: BindingGeneration,
    expires_at: UnixMillis,
    readable_keys: Vec<SettingKey>,
    writable_keys: Vec<SettingKey>,
    session_consent: SettingsGrantConsent,
    configured_settings_revision: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AccessDenialReason {
    GrantUnavailable,
}

#[derive(Error)]
pub(crate) enum SettingsAccessReplyContractError {
    #[error("invalid Settings access reply data")]
    Invalid,
    #[error("Settings access reply grant validation failed")]
    GrantValidation(#[source] SettingsContractError),
    #[error("Settings access reply exceeds its encoded byte limit")]
    TooLarge,
    #[error("Settings access reply serialization failed")]
    Encoding(#[source] serde_json::Error),
}

impl SettingsAccessReplyPacket {
    pub(crate) fn from_grant_head(
        host_id: DeviceId,
        in_reply_to: MessageId,
        query_id: Uuid,
        generation: BindingGeneration,
        head: &SettingsGrantHead,
    ) -> Result<Self, SettingsAccessReplyContractError> {
        head.validate()
            .map_err(SettingsAccessReplyContractError::GrantValidation)?;
        if head.host_revoked
            || head.peer_revoked
            || head.grant.host_id != host_id
            || head.grant.change.target.grant_id != generation.grant_id
            || head.grant.revision != generation.grant_revision
        {
            return Err(SettingsAccessReplyContractError::Invalid);
        }
        let SettingsGrantState::Active {
            readable_keys,
            writable_keys,
            session_consent,
        } = &head.grant.change.target.state
        else {
            return Err(SettingsAccessReplyContractError::Invalid);
        };
        let summary = AccessSummary {
            generation: generation.clone(),
            expires_at: head.grant.change.target.expires_at,
            readable_keys: readable_keys.clone(),
            writable_keys: writable_keys.clone(),
            session_consent: session_consent.clone(),
            configured_settings_revision: head.grant.change.configured_settings_revision,
        };
        Self::new(
            host_id,
            in_reply_to,
            query_id,
            Some(generation),
            AccessReplyBody::AccessStatus { summary },
        )
    }

    pub(crate) fn grant_unavailable(
        host_id: DeviceId,
        in_reply_to: MessageId,
        query_id: Uuid,
    ) -> Result<Self, SettingsAccessReplyContractError> {
        Self::new(
            host_id,
            in_reply_to,
            query_id,
            None,
            AccessReplyBody::Denied {
                reason: AccessDenialReason::GrantUnavailable,
            },
        )
    }

    fn new(
        host_id: DeviceId,
        in_reply_to: MessageId,
        query_id: Uuid,
        generation: Option<BindingGeneration>,
        body: AccessReplyBody,
    ) -> Result<Self, SettingsAccessReplyContractError> {
        let packet = Self {
            schema_version: SchemaVersion::CURRENT,
            payload: AccessReplyPayload::Settings {
                version: 1,
                host_id,
                message: AccessReplyMessage::Response {
                    in_reply_to,
                    correlation: QueryCorrelation::Query { query_id },
                    generation,
                    body,
                },
            },
        };
        packet.validate()?;
        Ok(packet)
    }

    pub(crate) fn encode_checked(
        &self,
    ) -> Result<Zeroizing<Vec<u8>>, SettingsAccessReplyContractError> {
        let maximum = self.validate()?;
        let mut encoded = Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *encoded, self)
            .map_err(SettingsAccessReplyContractError::Encoding)?;
        if encoded.len() > maximum {
            return Err(SettingsAccessReplyContractError::TooLarge);
        }
        Ok(encoded)
    }

    fn validate(&self) -> Result<usize, SettingsAccessReplyContractError> {
        let AccessReplyPayload::Settings {
            version,
            host_id,
            message:
                AccessReplyMessage::Response {
                    in_reply_to,
                    correlation: QueryCorrelation::Query { query_id },
                    generation,
                    body,
                },
        } = &self.payload;
        if self.schema_version != SchemaVersion::CURRENT
            || *version != 1
            || host_id.as_uuid().is_nil()
            || in_reply_to.as_uuid().is_nil()
            || query_id.is_nil()
        {
            return Err(SettingsAccessReplyContractError::Invalid);
        }
        match body {
            AccessReplyBody::AccessStatus { summary } => {
                let generation = generation
                    .as_ref()
                    .ok_or(SettingsAccessReplyContractError::Invalid)?;
                if generation != &summary.generation
                    || generation.grant_id.is_nil()
                    || generation.host_epoch.is_nil()
                    || generation.connection_epoch.is_nil()
                    || generation.grant_revision == 0
                    || generation.grant_revision > i64::MAX as u64
                    || summary.expires_at.get() == 0
                {
                    return Err(SettingsAccessReplyContractError::Invalid);
                }
                Ok(MAX_ACCESS_PACKET_BYTES)
            }
            AccessReplyBody::Denied { .. } => {
                if generation.is_some() {
                    return Err(SettingsAccessReplyContractError::Invalid);
                }
                Ok(MAX_DENIAL_PACKET_BYTES)
            }
        }
    }
}

impl fmt::Debug for SettingsAccessReplyPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAccessReplyPacket([redacted])")
    }
}

impl fmt::Debug for SettingsAccessReplyContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "SettingsAccessReplyContractError::Invalid",
            Self::GrantValidation(_) => {
                "SettingsAccessReplyContractError::GrantValidation([redacted])"
            }
            Self::TooLarge => "SettingsAccessReplyContractError::TooLarge",
            Self::Encoding(_) => "SettingsAccessReplyContractError::Encoding([redacted])",
        })
    }
}
