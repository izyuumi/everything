use std::fmt;

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::host::settings::contract::{SettingKey, SettingsContractError, SettingsSnapshot};
use crate::host::settings::service::SettingsChangePage;
use crate::host::settings::{
    SettingsApplicationState, SettingsGrantConsent, SettingsGrantHead, SettingsGrantState,
    SettingsOperationInspection, SettingsOperationPhase, SettingsWriteInspection,
};
use crate::protocol::{CommandId, DeviceId, MessageId, SchemaVersion, UnixMillis};

use super::{BindingGeneration, SettingsHostAction};

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
    AccessStatus {
        summary: AccessSummary,
    },
    Snapshot {
        snapshot: SettingsSnapshot,
    },
    Changes {
        page: SettingsChangePage,
    },
    Update {
        request_id: CommandId,
        operation: SettingsRemoteOperation,
    },
    HostActionRequired {
        action: SettingsHostAction,
    },
    Denied {
        reason: SettingsRemoteDenial,
    },
}

#[derive(Clone)]
pub(crate) enum SettingsRemoteReply {
    Snapshot(SettingsSnapshot),
    Changes(SettingsChangePage),
    Update {
        request_id: CommandId,
        operation: SettingsRemoteOperation,
    },
    Denied(SettingsRemoteDenial),
}

#[derive(Clone, Serialize)]
pub(crate) struct SettingsRemoteOperation {
    phase: SettingsOperationPhase,
    updated_at: UnixMillis,
    write: Option<SettingsWriteInspection>,
    application: Option<SettingsApplicationState>,
}

impl SettingsRemoteOperation {
    pub(crate) fn from_inspection(inspection: &SettingsOperationInspection) -> Self {
        Self {
            phase: inspection.phase.clone(),
            updated_at: inspection.updated_at,
            write: inspection.write.clone(),
            application: inspection.application.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsRemoteDenial {
    GrantUnavailable,
    GrantChanged,
    OutsideScope,
    Conflict,
    WriteBlocked,
    Capacity,
    Invalid,
    OutcomeUnknown,
    Unavailable,
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
                reason: SettingsRemoteDenial::GrantUnavailable,
            },
        )
    }

    pub(crate) fn remote(
        host_id: DeviceId,
        in_reply_to: MessageId,
        query_id: Uuid,
        generation: BindingGeneration,
        reply: SettingsRemoteReply,
    ) -> Result<Self, SettingsAccessReplyContractError> {
        let (generation, body) = match reply {
            SettingsRemoteReply::Snapshot(snapshot) => {
                (Some(generation), AccessReplyBody::Snapshot { snapshot })
            }
            SettingsRemoteReply::Changes(page) => {
                (Some(generation), AccessReplyBody::Changes { page })
            }
            SettingsRemoteReply::Update {
                request_id,
                operation,
            } => (
                Some(generation),
                AccessReplyBody::Update {
                    request_id,
                    operation,
                },
            ),
            SettingsRemoteReply::Denied(reason) => (None, AccessReplyBody::Denied { reason }),
        };
        Self::new(host_id, in_reply_to, query_id, generation, body)
    }

    pub(crate) fn host_action_required(
        host_id: DeviceId,
        in_reply_to: MessageId,
        query_id: Uuid,
        generation: BindingGeneration,
        action: SettingsHostAction,
    ) -> Result<Self, SettingsAccessReplyContractError> {
        Self::new(
            host_id,
            in_reply_to,
            query_id,
            Some(generation),
            AccessReplyBody::HostActionRequired { action },
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
            AccessReplyBody::Snapshot { .. }
            | AccessReplyBody::Changes { .. }
            | AccessReplyBody::Update { .. }
            | AccessReplyBody::HostActionRequired { .. } => {
                let generation = generation
                    .as_ref()
                    .ok_or(SettingsAccessReplyContractError::Invalid)?;
                if generation.grant_id.is_nil()
                    || generation.host_epoch.is_nil()
                    || generation.connection_epoch.is_nil()
                    || generation.grant_revision == 0
                    || generation.grant_revision > i64::MAX as u64
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
