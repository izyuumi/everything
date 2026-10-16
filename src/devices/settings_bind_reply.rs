use std::fmt;

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::protocol::{DeviceId, MessageId, SchemaVersion, UnixMillis};

use super::{BindingGeneration, SettingsParentReference};

const MAX_BIND_PACKET_BYTES: usize = 32_768;

#[derive(Serialize)]
pub(crate) struct SettingsBindReplyPacket {
    schema_version: SchemaVersion,
    payload: BindReplyPayload,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BindReplyPayload {
    Settings {
        version: u16,
        host_id: DeviceId,
        message: BindReplyMessage,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BindReplyMessage {
    Response {
        in_reply_to: MessageId,
        correlation: BindCorrelation,
        generation: BindingGeneration,
        body: BoundReplyBody,
    },
}

#[derive(Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
enum BindCorrelation {
    Bind { binding_request_id: Uuid },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BoundReplyBody {
    Bound {
        binding_request_id: Uuid,
        parent: SettingsParentReference,
        expires_at: UnixMillis,
    },
}

#[derive(Error)]
pub(crate) enum SettingsBindReplyContractError {
    #[error("invalid Settings Bind reply data")]
    Invalid,
    #[error("Settings Bind reply exceeds its encoded byte limit")]
    TooLarge,
    #[error("Settings Bind reply serialization failed")]
    Encoding(#[source] serde_json::Error),
}

impl SettingsBindReplyPacket {
    pub(crate) fn new(
        host_id: DeviceId,
        in_reply_to: MessageId,
        binding_request_id: Uuid,
        parent: SettingsParentReference,
        expires_at: UnixMillis,
    ) -> Result<Self, SettingsBindReplyContractError> {
        let packet = Self {
            schema_version: SchemaVersion::CURRENT,
            payload: BindReplyPayload::Settings {
                version: 1,
                host_id,
                message: BindReplyMessage::Response {
                    in_reply_to,
                    correlation: BindCorrelation::Bind { binding_request_id },
                    generation: parent.generation.clone(),
                    body: BoundReplyBody::Bound {
                        binding_request_id,
                        parent,
                        expires_at,
                    },
                },
            },
        };
        packet.validate()?;
        Ok(packet)
    }

    pub(crate) fn encode_checked(
        &self,
    ) -> Result<Zeroizing<Vec<u8>>, SettingsBindReplyContractError> {
        self.validate()?;
        let mut encoded = Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *encoded, self)
            .map_err(SettingsBindReplyContractError::Encoding)?;
        if encoded.len() > MAX_BIND_PACKET_BYTES {
            return Err(SettingsBindReplyContractError::TooLarge);
        }
        Ok(encoded)
    }

    fn validate(&self) -> Result<(), SettingsBindReplyContractError> {
        let BindReplyPayload::Settings {
            version,
            host_id,
            message:
                BindReplyMessage::Response {
                    in_reply_to,
                    correlation: BindCorrelation::Bind { binding_request_id },
                    generation,
                    body:
                        BoundReplyBody::Bound {
                            binding_request_id: body_binding_request_id,
                            parent,
                            expires_at,
                        },
                },
        } = &self.payload;
        if self.schema_version != SchemaVersion::CURRENT
            || *version != 1
            || host_id.as_uuid().is_nil()
            || in_reply_to.as_uuid().is_nil()
            || binding_request_id.is_nil()
            || binding_request_id != body_binding_request_id
            || parent.parent_id.is_nil()
            || generation != &parent.generation
            || generation.grant_id.is_nil()
            || generation.host_epoch.is_nil()
            || generation.connection_epoch.is_nil()
            || generation.grant_revision == 0
            || generation.grant_revision > i64::MAX as u64
            || expires_at.get() == 0
        {
            return Err(SettingsBindReplyContractError::Invalid);
        }
        Ok(())
    }
}

impl fmt::Debug for SettingsBindReplyPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsBindReplyPacket([redacted])")
    }
}

impl fmt::Debug for SettingsBindReplyContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "SettingsBindReplyContractError::Invalid",
            Self::TooLarge => "SettingsBindReplyContractError::TooLarge",
            Self::Encoding(_) => "SettingsBindReplyContractError::Encoding([redacted])",
        })
    }
}
