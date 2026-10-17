use std::fmt;

use serde::de::{Visitor, value::StrDeserializer};
use serde::{Deserialize, Deserializer};
use thiserror::Error;
use uuid::Uuid;

use crate::host::settings::contract::SettingKey;
use crate::protocol::{CommandId, DeviceId, UnixMillis};

use super::super::settings_reply::SettingsReplyContractError;
use super::command::SettingsParentReference;
use super::wire::{MAX_PRIVATE_REQUEST_BYTES, canonical_uuid, object};

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SettingsReadRequest {
    Snapshot(SettingsSnapshotReadIntent),
    InspectOriginal(SettingsOriginalInspectionIntent),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsSnapshotReadIntent {
    pub(crate) host_id: DeviceId,
    pub(crate) query_id: Uuid,
    pub(crate) parent: SettingsParentReference,
    pub(crate) keys: Vec<SettingKey>,
    pub(crate) issued_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsOriginalInspectionIntent {
    pub(crate) host_id: DeviceId,
    pub(crate) query_id: Uuid,
    pub(crate) parent: SettingsParentReference,
    pub(crate) request_id: CommandId,
    pub(crate) operation_id: Option<CommandId>,
    pub(crate) issued_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

#[derive(Error)]
pub(crate) enum SettingsReadContractError {
    #[error("invalid Settings read data")]
    Invalid,
    #[error("Settings read data exceeds its encoded byte limit")]
    TooLarge,
    #[error("Settings read request decoding failed")]
    Decoding(#[source] serde_json::Error),
    #[error("Settings read reply encoding failed")]
    Encoding(#[source] serde_json::Error),
    #[error("Settings read Original data validation failed")]
    OriginalData(#[from] SettingsReplyContractError),
}

impl SettingsReadRequest {
    pub(crate) fn validate(&self) -> Result<(), SettingsReadContractError> {
        let (host, query, parent, issued, expires) = match self {
            Self::Snapshot(intent) => {
                if intent.keys.is_empty()
                    || intent.keys.len() > 5
                    || intent.keys.iter().any(|key| {
                        !matches!(
                            key,
                            SettingKey::ModelSelection
                                | SettingKey::ToolSelection
                                | SettingKey::EmbeddingsEnabled
                                | SettingKey::EmbeddingsDimensions
                                | SettingKey::EmbeddingsModel
                        )
                    })
                    || !intent.keys.windows(2).all(|keys| keys[0] < keys[1])
                {
                    return Err(SettingsReadContractError::Invalid);
                }
                (
                    intent.host_id,
                    intent.query_id,
                    &intent.parent,
                    intent.issued_at,
                    intent.expires_at,
                )
            }
            Self::InspectOriginal(intent) => {
                if intent.request_id.as_uuid().is_nil()
                    || intent.query_id == *intent.request_id.as_uuid()
                    || intent.operation_id.is_some_and(|id| id.as_uuid().is_nil())
                {
                    return Err(SettingsReadContractError::Invalid);
                }
                (
                    intent.host_id,
                    intent.query_id,
                    &intent.parent,
                    intent.issued_at,
                    intent.expires_at,
                )
            }
        };
        let generation = &parent.generation;
        if host.as_uuid().is_nil()
            || query.is_nil()
            || parent.parent_id.is_nil()
            || generation.grant_id.is_nil()
            || generation.host_epoch.is_nil()
            || generation.connection_epoch.is_nil()
            || generation.grant_revision == 0
            || generation.grant_revision > i64::MAX as u64
            || issued.get() == 0
            || expires <= issued
            || expires.get() - issued.get() > 30_000
        {
            return Err(SettingsReadContractError::Invalid);
        }
        Ok(())
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<SettingsReadRequest, SettingsReadContractError> {
    if bytes.len() > MAX_PRIVATE_REQUEST_BYTES {
        return Err(SettingsReadContractError::TooLarge);
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let packet = object::<Packet, _>(&mut decoder).map_err(SettingsReadContractError::Decoding)?;
    decoder.end().map_err(SettingsReadContractError::Decoding)?;
    if packet.schema_version != 1 || packet.payload.version != 1 {
        return Err(SettingsReadContractError::Invalid);
    }
    let host_id = DeviceId::try_from(packet.payload.host_id)
        .map_err(|_| SettingsReadContractError::Invalid)?;
    let query_id = packet.payload.message.query_id;
    let request = match packet.payload.message.body {
        ReadBody::Snapshot { parent, keys, time } => {
            SettingsReadRequest::Snapshot(SettingsSnapshotReadIntent {
                host_id,
                query_id,
                parent,
                keys,
                issued_at: UnixMillis::try_from(time.issued_at)
                    .map_err(|_| SettingsReadContractError::Invalid)?,
                expires_at: UnixMillis::try_from(time.expires_at)
                    .map_err(|_| SettingsReadContractError::Invalid)?,
            })
        }
        ReadBody::InspectOriginal {
            parent,
            request_id,
            operation_id,
            time,
        } => SettingsReadRequest::InspectOriginal(SettingsOriginalInspectionIntent {
            host_id,
            query_id,
            parent,
            request_id: CommandId::try_from(request_id)
                .map_err(|_| SettingsReadContractError::Invalid)?,
            operation_id: operation_id
                .0
                .map(CommandId::try_from)
                .transpose()
                .map_err(|_| SettingsReadContractError::Invalid)?,
            issued_at: UnixMillis::try_from(time.issued_at)
                .map_err(|_| SettingsReadContractError::Invalid)?,
            expires_at: UnixMillis::try_from(time.expires_at)
                .map_err(|_| SettingsReadContractError::Invalid)?,
        }),
    };
    request.validate()?;
    Ok(request)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    schema_version: u16,
    #[serde(deserialize_with = "object")]
    payload: Payload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    #[serde(rename = "kind", deserialize_with = "settings_kind")]
    _kind: (),
    version: u16,
    #[serde(deserialize_with = "canonical_uuid")]
    host_id: Uuid,
    #[serde(deserialize_with = "object")]
    message: Query,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    #[serde(rename = "kind", deserialize_with = "query_kind")]
    _kind: (),
    #[serde(deserialize_with = "canonical_uuid")]
    query_id: Uuid,
    #[serde(deserialize_with = "object")]
    body: ReadBody,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ReadBody {
    Snapshot {
        #[serde(deserialize_with = "object")]
        parent: SettingsParentReference,
        #[serde(deserialize_with = "read_keys")]
        keys: Vec<SettingKey>,
        #[serde(deserialize_with = "object")]
        time: RequestTime,
    },
    InspectOriginal {
        #[serde(deserialize_with = "object")]
        parent: SettingsParentReference,
        #[serde(deserialize_with = "canonical_uuid")]
        request_id: Uuid,
        operation_id: NullableOperationId,
        #[serde(deserialize_with = "object")]
        time: RequestTime,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestTime {
    issued_at: u64,
    expires_at: u64,
}

fn settings_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "settings")
}

fn query_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "query")
}

fn require_kind<'de, D: Deserializer<'de>>(
    deserializer: D,
    expected: &'static str,
) -> Result<(), D::Error> {
    if String::deserialize(deserializer)? != expected {
        return Err(serde::de::Error::custom("invalid Settings read kind"));
    }
    Ok(())
}

fn read_keys<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<SettingKey>, D::Error> {
    Vec::<String>::deserialize(deserializer)?
        .into_iter()
        .map(|key| match key.as_str() {
            "models.selection" => Ok(SettingKey::ModelSelection),
            "tools.selection" => Ok(SettingKey::ToolSelection),
            "memory.embeddings.enabled" => Ok(SettingKey::EmbeddingsEnabled),
            "memory.embeddings.dimensions" => Ok(SettingKey::EmbeddingsDimensions),
            "memory.embeddings.model" => Ok(SettingKey::EmbeddingsModel),
            _ => Err(serde::de::Error::custom("invalid Settings read key")),
        })
        .collect()
}

struct NullableOperationId(Option<Uuid>);

impl<'de> Deserialize<'de> for NullableOperationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdentifierVisitor;
        impl Visitor<'_> for IdentifierVisitor {
            type Value = NullableOperationId;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a null or canonical Settings operation identifier")
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(NullableOperationId(None))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                canonical_uuid(StrDeserializer::<E>::new(value))
                    .map(|id| NullableOperationId(Some(id)))
            }
        }
        deserializer.deserialize_any(IdentifierVisitor)
    }
}

impl fmt::Debug for SettingsReadRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReadRequest([data])")
    }
}

impl fmt::Debug for SettingsSnapshotReadIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsSnapshotReadIntent([data])")
    }
}

impl fmt::Debug for SettingsOriginalInspectionIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsOriginalInspectionIntent([data])")
    }
}

impl fmt::Debug for SettingsReadContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "SettingsReadContractError::Invalid",
            Self::TooLarge => "SettingsReadContractError::TooLarge",
            Self::Decoding(_) => "SettingsReadContractError::Decoding([redacted])",
            Self::Encoding(_) => "SettingsReadContractError::Encoding([redacted])",
            Self::OriginalData(_) => "SettingsReadContractError::OriginalData([redacted])",
        })
    }
}
