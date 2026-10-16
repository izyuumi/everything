use crate::protocol::CURRENT_SCHEMA_VERSION;
use crate::storage::TransportInboxRejection;
use serde::de::{MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::marker::PhantomData;

pub(super) enum RoutingDecision {
    Ordinary,
    Rejected(TransportInboxRejection),
}

pub(super) fn classify(plaintext: &[u8]) -> RoutingDecision {
    let Ok(encoded) = std::str::from_utf8(plaintext) else {
        return RoutingDecision::Rejected(TransportInboxRejection::InvalidPacket);
    };
    match serde_json::from_str::<RoutingObject<RoutingProbe>>(encoded) {
        Ok(RoutingObject(probe)) if probe.schema_version != CURRENT_SCHEMA_VERSION => {
            RoutingDecision::Rejected(TransportInboxRejection::UnsupportedVersion)
        }
        Ok(RoutingObject(probe)) => match probe.payload.0.kind {
            PayloadKind::Ordinary => RoutingDecision::Ordinary,
            PayloadKind::Settings => {
                RoutingDecision::Rejected(TransportInboxRejection::SettingsUnavailable)
            }
            PayloadKind::Unknown => {
                RoutingDecision::Rejected(TransportInboxRejection::InvalidPacket)
            }
        },
        Err(_) => RoutingDecision::Rejected(TransportInboxRejection::InvalidPacket),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoutingProbe {
    schema_version: u16,
    payload: RoutingObject<PayloadProbe>,
}

#[derive(Deserialize)]
struct PayloadProbe {
    kind: PayloadKind,
}

struct RoutingObject<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for RoutingObject<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(RoutingObjectVisitor::<T>(PhantomData))
    }
}

struct RoutingObjectVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for RoutingObjectVisitor<T> {
    type Value = RoutingObject<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a routing object")
    }

    fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        T::deserialize(MapAccessDeserializer::new(map)).map(RoutingObject)
    }
}

enum PayloadKind {
    Ordinary,
    Settings,
    Unknown,
}

impl<'de> Deserialize<'de> for PayloadKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(PayloadKindVisitor)
    }
}

struct PayloadKindVisitor;

impl<'de> Visitor<'de> for PayloadKindVisitor {
    type Value = PayloadKind;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a routing kind string")
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(match value {
            "command"
            | "command_result"
            | "sync_request"
            | "sync_batch"
            | "sync_acknowledgement"
            | "revoked" => PayloadKind::Ordinary,
            "settings" => PayloadKind::Settings,
            _ => PayloadKind::Unknown,
        })
    }
}
