use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer};
use uuid::Uuid;

use crate::protocol::DeviceId;

pub(crate) const MAX_PRIVATE_REQUEST_BYTES: usize = 32_768;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BindingIntent {
    pub(crate) host_id: DeviceId,
    pub(crate) generation: BindingGeneration,
    pub(crate) binding_request_id: Uuid,
    pub(crate) issued_at: u64,
    pub(crate) expires_at: u64,
    pub(crate) requested_parent_expires_at: u64,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BindingGeneration {
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) grant_id: Uuid,
    pub(crate) grant_revision: u64,
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) host_epoch: Uuid,
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) connection_epoch: Uuid,
}

pub(super) enum DecodedBind {
    Bind(BindingIntent),
    Unavailable,
    Invalid,
}

pub(super) fn decode(bytes: &[u8]) -> DecodedBind {
    if bytes.len() > MAX_PRIVATE_REQUEST_BYTES {
        return DecodedBind::Invalid;
    }
    let Ok(Object(prefix)) = serde_json::from_slice::<Object<Prefix>>(bytes) else {
        return DecodedBind::Invalid;
    };
    if prefix.schema_version != 1 || prefix.payload.version != 1 {
        return DecodedBind::Invalid;
    }
    match prefix.payload.message.kind {
        MessageKind::Unavailable => return DecodedBind::Unavailable,
        MessageKind::Unknown => return DecodedBind::Invalid,
        MessageKind::Bind => {}
    }
    let Ok(Object(packet)) = serde_json::from_slice::<Object<Packet>>(bytes) else {
        return DecodedBind::Invalid;
    };
    let bind = packet.payload.message;
    let generation = bind.generation;
    if packet.schema_version != 1
        || packet.payload.version != 1
        || generation.grant_revision == 0
        || generation.grant_revision > i64::MAX as u64
        || bind.time.issued_at == 0
        || bind.time.expires_at <= bind.time.issued_at
        || bind.time.expires_at - bind.time.issued_at > 30_000
        || bind.requested_parent_expires_at <= bind.time.issued_at
        || bind.requested_parent_expires_at > i64::MAX as u64
        || bind
            .time
            .issued_at
            .checked_add(300_000)
            .is_none_or(|maximum| bind.requested_parent_expires_at > maximum)
    {
        return DecodedBind::Invalid;
    }
    let Ok(host_id) = DeviceId::try_from(packet.payload.host_id) else {
        return DecodedBind::Invalid;
    };
    DecodedBind::Bind(BindingIntent {
        host_id,
        generation,
        binding_request_id: bind.binding_request_id,
        issued_at: bind.time.issued_at,
        expires_at: bind.time.expires_at,
        requested_parent_expires_at: bind.requested_parent_expires_at,
    })
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
    #[serde(rename = "kind")]
    _kind: SettingsKind,
    version: u16,
    #[serde(deserialize_with = "canonical_uuid")]
    host_id: Uuid,
    #[serde(deserialize_with = "object")]
    message: Bind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bind {
    #[serde(rename = "kind")]
    _kind: BindKind,
    #[serde(deserialize_with = "object")]
    generation: BindingGeneration,
    #[serde(deserialize_with = "canonical_uuid")]
    binding_request_id: Uuid,
    #[serde(deserialize_with = "object")]
    time: RequestTime,
    requested_parent_expires_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestTime {
    issued_at: u64,
    expires_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prefix {
    schema_version: u16,
    #[serde(deserialize_with = "object")]
    payload: PrefixPayload,
}

#[derive(Deserialize)]
struct PrefixPayload {
    #[serde(rename = "kind")]
    _kind: SettingsKind,
    version: u16,
    #[serde(deserialize_with = "object")]
    message: KindProbe,
}

#[derive(Deserialize)]
struct KindProbe {
    kind: MessageKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SettingsKind {
    Settings,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BindKind {
    Bind,
}

enum MessageKind {
    Bind,
    Unavailable,
    Unknown,
}

impl<'de> Deserialize<'de> for MessageKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KindVisitor;
        impl Visitor<'_> for KindVisitor {
            type Value = MessageKind;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Settings message kind")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(match value {
                    "bind" => MessageKind::Bind,
                    "command" | "query" | "close" | "response" | "hint" => MessageKind::Unavailable,
                    _ => MessageKind::Unknown,
                })
            }
        }
        deserializer.deserialize_str(KindVisitor)
    }
}

pub(super) fn canonical_uuid<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
    let encoded = String::deserialize(deserializer)?;
    let uuid = Uuid::parse_str(&encoded)
        .map_err(|_| serde::de::Error::custom("invalid Settings identifier"))?;
    if uuid.is_nil() || encoded.len() != 36 || encoded != uuid.to_string() {
        return Err(serde::de::Error::custom("invalid Settings identifier"));
    }
    Ok(uuid)
}

pub(super) fn object<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Object::deserialize(deserializer).map(|Object(value)| value)
}

struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = Object<T>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a Settings object")
            }
            fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(ObjectVisitor(PhantomData))
    }
}
