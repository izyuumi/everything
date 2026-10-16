use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::wire::{BindingGeneration, canonical_uuid, object};
use super::{BindRejection, VerifiedSettingsEnvelope, work::PrivateWitness};
use crate::agent::Agent;
use crate::crypto::DevicePublicKeys;
use crate::devices::{DeviceEndpoint, SettingsConnectionIdentity};
use crate::protocol::{CommandId, DeviceId, UnixMillis};
use crate::storage::settings_originals::RemoteDimensionSelection;
use crate::storage::{Store, TransportInboxMetadata};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsCommandIntent {
    pub(crate) host_id: DeviceId,
    pub(crate) parent: SettingsParentReference,
    pub(crate) request_id: CommandId,
    pub(crate) issued_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
    pub(crate) update: SettingsUpdateIntent,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsUpdateIntent {
    pub(crate) expected_settings_revision: u64,
    pub(crate) enabled: Option<bool>,
    pub(crate) dimensions: Option<RemoteDimensionSelection>,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsParentReference {
    #[serde(deserialize_with = "canonical_uuid")]
    pub(crate) parent_id: Uuid,
    #[serde(deserialize_with = "object")]
    pub(crate) generation: BindingGeneration,
}

pub(crate) struct AuthenticatedSettingsCommandSource {
    intent: SettingsCommandIntent,
    envelope: VerifiedSettingsEnvelope,
    invocation: Arc<()>,
}

pub(crate) struct OwnedVerifiedSettingsCommand {
    source: AuthenticatedSettingsCommandSource,
    _witness: PrivateWitness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum CommandRejection {
    #[error("original Settings command is expired")]
    Expired,
    #[error("Settings command is outside its current scope")]
    OutsideScope,
    #[error("Settings command source is unavailable")]
    Unavailable,
    #[error("Settings command source bookkeeping failed")]
    Bookkeeping,
}

pub(super) enum DecodedCommand {
    Update(SettingsCommandIntent),
    Unavailable,
    Invalid,
}

impl SettingsCommandIntent {
    fn valid(&self) -> bool {
        let generation = &self.parent.generation;
        !self.host_id.as_uuid().is_nil()
            && !self.request_id.as_uuid().is_nil()
            && !self.parent.parent_id.is_nil()
            && !generation.grant_id.is_nil()
            && !generation.host_epoch.is_nil()
            && !generation.connection_epoch.is_nil()
            && generation.grant_revision > 0
            && generation.grant_revision <= i64::MAX as u64
            && self.issued_at.get() > 0
            && self.expires_at > self.issued_at
            && self.expires_at.get() - self.issued_at.get() <= 30_000
            && self.update.expected_settings_revision > 0
            && self.update.expected_settings_revision <= i64::MAX as u64
            && (self.update.enabled.is_some() || self.update.dimensions.is_some())
            && !matches!(&self.update.dimensions, Some(RemoteDimensionSelection::Selected { dimensions }) if *dimensions == 0 || *dimensions > 65_536)
    }
}

impl AuthenticatedSettingsCommandSource {
    pub(crate) fn matches_context(
        &self,
        agent: &Agent,
        endpoint: &DeviceEndpoint,
        mutation: &Arc<Mutex<()>>,
    ) -> bool {
        self.envelope.matches_context(agent, endpoint, mutation)
    }

    pub(crate) fn connection(&self) -> &Arc<SettingsConnectionIdentity> {
        self.envelope.connection()
    }

    pub(crate) fn verified_peer_keys(&self) -> &DevicePublicKeys {
        self.envelope.verified_peer_keys()
    }

    pub(crate) fn intent(&self) -> &SettingsCommandIntent {
        &self.intent
    }

    pub(crate) fn invocation_identity(&self) -> &Arc<()> {
        &self.invocation
    }

    pub(crate) fn check_precommit(
        &self,
        store: &Store,
    ) -> Result<(UnixMillis, Instant), CommandRejection> {
        if !self.intent.valid() {
            return Err(CommandRejection::OutsideScope);
        }
        self.envelope
            .check_precommit(
                store,
                self.intent.host_id,
                &self.intent.parent.generation,
                self.intent.issued_at.get(),
                self.intent.expires_at.get(),
            )
            .map_err(CommandRejection::from)
    }

    pub(crate) fn receipt_metadata(&self) -> Result<TransportInboxMetadata<'_>, CommandRejection> {
        self.envelope
            .receipt_metadata()
            .map_err(CommandRejection::from)
    }

    pub(crate) fn pinned_identity(&self) -> &Value {
        self.envelope.pinned_identity()
    }
}

impl OwnedVerifiedSettingsCommand {
    pub(super) fn from_verified_update(
        envelope: VerifiedSettingsEnvelope,
        intent: SettingsCommandIntent,
        witness: PrivateWitness,
    ) -> Self {
        Self {
            source: AuthenticatedSettingsCommandSource {
                intent,
                envelope,
                invocation: Arc::new(()),
            },
            _witness: witness,
        }
    }

    pub(crate) fn authority_source(&self) -> &AuthenticatedSettingsCommandSource {
        &self.source
    }
}

impl From<BindRejection> for CommandRejection {
    fn from(value: BindRejection) -> Self {
        match value {
            BindRejection::Expired => Self::Expired,
            BindRejection::OutsideScope => Self::OutsideScope,
            BindRejection::Unavailable => Self::Unavailable,
            BindRejection::ExistingBinding | BindRejection::Conflict => Self::Bookkeeping,
        }
    }
}

impl fmt::Debug for AuthenticatedSettingsCommandSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatedSettingsCommandSource([owned verification])")
    }
}

impl fmt::Debug for OwnedVerifiedSettingsCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedVerifiedSettingsCommand([private])")
    }
}

pub(super) fn decode(bytes: &[u8]) -> DecodedCommand {
    if bytes.len() > super::wire::MAX_PRIVATE_REQUEST_BYTES {
        return DecodedCommand::Invalid;
    }
    let Ok(probe) = decode_object::<PacketProbe>(bytes) else {
        return DecodedCommand::Invalid;
    };
    if probe.schema_version != 1 || probe.payload.version != 1 {
        return DecodedCommand::Invalid;
    }
    match probe.payload.message.kind {
        CommandLane::Unavailable => return DecodedCommand::Unavailable,
        CommandLane::Unknown => return DecodedCommand::Invalid,
        CommandLane::Command => {}
    }
    match probe.payload.message.body.map(|body| body.kind) {
        Some(UpdateLane::Update) => {}
        Some(UpdateLane::Unavailable) => return DecodedCommand::Unavailable,
        Some(UpdateLane::Unknown) | None => return DecodedCommand::Invalid,
    }
    let Ok(packet) = decode_object::<Packet>(bytes) else {
        return DecodedCommand::Invalid;
    };
    let command = packet.payload.message;
    let Ok(host_id) = DeviceId::try_from(packet.payload.host_id) else {
        return DecodedCommand::Invalid;
    };
    let Ok(request_id) = CommandId::try_from(command.request_id) else {
        return DecodedCommand::Invalid;
    };
    let (Ok(issued_at), Ok(expires_at)) = (
        UnixMillis::try_from(command.time.issued_at),
        UnixMillis::try_from(command.time.expires_at),
    ) else {
        return DecodedCommand::Invalid;
    };
    let intent = SettingsCommandIntent {
        host_id,
        parent: command.parent,
        request_id,
        issued_at,
        expires_at,
        update: SettingsUpdateIntent {
            expected_settings_revision: command.body.expected_settings_revision,
            enabled: command.body.enabled,
            dimensions: command.body.dimensions,
        },
    };
    if packet.schema_version != 1 || packet.payload.version != 1 || !intent.valid() {
        return DecodedCommand::Invalid;
    }
    DecodedCommand::Update(intent)
}

fn decode_object<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = object(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
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
    message: Command,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    #[serde(rename = "kind", deserialize_with = "command_kind")]
    _kind: (),
    #[serde(deserialize_with = "object")]
    parent: SettingsParentReference,
    #[serde(deserialize_with = "canonical_uuid")]
    request_id: Uuid,
    #[serde(deserialize_with = "object")]
    time: RequestTime,
    #[serde(deserialize_with = "object")]
    body: Update,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestTime {
    issued_at: u64,
    expires_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    #[serde(rename = "kind", deserialize_with = "update_kind")]
    _kind: (),
    expected_settings_revision: u64,
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "optional_object")]
    dimensions: Option<RemoteDimensionSelection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PacketProbe {
    schema_version: u16,
    #[serde(deserialize_with = "object")]
    payload: PayloadProbe,
}

#[derive(Deserialize)]
struct PayloadProbe {
    #[serde(rename = "kind", deserialize_with = "settings_kind")]
    _kind: (),
    version: u16,
    #[serde(deserialize_with = "object")]
    message: MessageProbe,
}

#[derive(Deserialize)]
struct MessageProbe {
    kind: CommandLane,
    #[serde(default, deserialize_with = "optional_object")]
    body: Option<BodyProbe>,
}

#[derive(Deserialize)]
struct BodyProbe {
    kind: UpdateLane,
}

enum CommandLane {
    Command,
    Unavailable,
    Unknown,
}

enum UpdateLane {
    Update,
    Unavailable,
    Unknown,
}

impl<'de> Deserialize<'de> for CommandLane {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "command" => Self::Command,
            "bind" | "query" | "close" | "response" | "hint" => Self::Unavailable,
            _ => Self::Unknown,
        })
    }
}

impl<'de> Deserialize<'de> for UpdateLane {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "update" => Self::Update,
            "chat_open" | "send" => Self::Unavailable,
            _ => Self::Unknown,
        })
    }
}

fn settings_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "settings")
}

fn command_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "command")
}

fn update_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "update")
}

fn require_kind<'de, D: Deserializer<'de>>(
    deserializer: D,
    expected: &'static str,
) -> Result<(), D::Error> {
    let value = String::deserialize(deserializer)?;
    if value != expected {
        return Err(serde::de::Error::custom("unsupported Settings kind"));
    }
    Ok(())
}

fn optional_object<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    struct OptionalObject<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for OptionalObject<T> {
        type Value = Option<T>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an optional Settings object")
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            object(deserializer).map(Some)
        }
    }
    deserializer.deserialize_option(OptionalObject(std::marker::PhantomData))
}
