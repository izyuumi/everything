use std::fmt;

use serde::Serialize;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::agent::ProviderKind;
use crate::host::settings::contract::{SettingKey, SettingState, SettingsUnavailableReason};
use crate::protocol::{CommandId, DeviceId, MessageId, SchemaVersion, UnixMillis};

use super::BindingGeneration;
use super::settings_ingress::read::{SettingsReadContractError, SettingsReadRequest};
use super::settings_reply::{OriginalKind, OriginalLink, OriginalView, ServiceLookup};

const MAX_SNAPSHOT_PACKET_BYTES: usize = 32_768;
const MAX_INSPECTION_PACKET_BYTES: usize = 65_536;
const MAX_DENIAL_PACKET_BYTES: usize = 2048;

#[derive(PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SettingsRemoteValue {
    Model {
        selection: Option<SettingsRemoteModelSelection>,
    },
    ToolSelection {
        selection: SettingsRemoteToolSelection,
    },
    Boolean {
        value: bool,
    },
    Dimensions {
        value: Option<u32>,
    },
    EmbeddingModel {
        model: Option<String>,
    },
}

#[derive(PartialEq, Eq, Serialize)]
pub(crate) struct SettingsRemoteModelSelection {
    pub(crate) provider: ProviderKind,
    pub(crate) model: String,
}

#[derive(PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum SettingsRemoteToolSelection {
    Local,
    SystemOne { model: String },
}

#[derive(Serialize)]
pub(crate) struct SettingsRemoteEntry {
    pub(crate) key: SettingKey,
    pub(crate) default: SettingsRemoteValue,
    pub(crate) desired: Option<SettingsRemoteValue>,
    pub(crate) effective: Option<SettingsRemoteValue>,
    pub(crate) state: SettingState,
    pub(crate) unavailable_reason: Option<SettingsUnavailableReason>,
}

#[derive(Serialize)]
pub(crate) struct SettingsHostSnapshot {
    pub(crate) revision: u64,
    pub(crate) observed_at: UnixMillis,
    pub(crate) stale: bool,
    pub(crate) entries: Vec<SettingsRemoteEntry>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum SettingsOriginalInspectionLookup {
    NotRecorded,
    Recorded {
        original_generation: BindingGeneration,
        view: Box<OriginalView>,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsReadDenialReason {
    GrantUnavailable,
    OutsideScope,
}

#[derive(Serialize)]
pub(crate) struct SettingsReadReplyPacket {
    schema_version: SchemaVersion,
    payload: ReadReplyPayload,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadReplyPayload {
    Settings {
        version: u16,
        host_id: DeviceId,
        message: ReadReplyMessage,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadReplyMessage {
    Response {
        in_reply_to: MessageId,
        correlation: ReadCorrelation,
        generation: Option<BindingGeneration>,
        body: ReadReplyBody,
    },
}

#[derive(Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
enum ReadCorrelation {
    Query { query_id: Uuid },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReadReplyBody {
    Snapshot {
        snapshot: SettingsHostSnapshot,
    },
    OriginalInspection {
        request_id: CommandId,
        operation_id: Option<CommandId>,
        observed_at: UnixMillis,
        lookup: SettingsOriginalInspectionLookup,
    },
    Denied {
        reason: SettingsReadDenialReason,
    },
}

impl SettingsRemoteValue {
    fn validate_for(&self, key: SettingKey) -> Result<(), SettingsReadContractError> {
        match (key, self) {
            (SettingKey::ModelSelection, Self::Model { selection }) => {
                if let Some(selection) = selection {
                    validate_text(&selection.model)?;
                }
            }
            (SettingKey::ToolSelection, Self::ToolSelection { selection }) => {
                if let SettingsRemoteToolSelection::SystemOne { model } = selection {
                    validate_text(model)?;
                }
            }
            (SettingKey::EmbeddingsEnabled, Self::Boolean { .. }) => {}
            (SettingKey::EmbeddingsDimensions, Self::Dimensions { value }) => {
                if value.is_some_and(|value| value == 0 || value > 65_536) {
                    return Err(SettingsReadContractError::Invalid);
                }
            }
            (SettingKey::EmbeddingsModel, Self::EmbeddingModel { model }) => {
                if let Some(model) = model {
                    validate_text(model)?;
                }
            }
            _ => return Err(SettingsReadContractError::Invalid),
        }
        Ok(())
    }
}

impl SettingsRemoteEntry {
    fn validate(&self) -> Result<(), SettingsReadContractError> {
        let valid_default = matches!(
            (self.key, &self.default),
            (
                SettingKey::ModelSelection,
                SettingsRemoteValue::Model { selection: None }
            ) | (
                SettingKey::ToolSelection,
                SettingsRemoteValue::ToolSelection {
                    selection: SettingsRemoteToolSelection::Local
                }
            ) | (
                SettingKey::EmbeddingsEnabled,
                SettingsRemoteValue::Boolean { value: false }
            ) | (
                SettingKey::EmbeddingsDimensions,
                SettingsRemoteValue::Dimensions { value: None }
            ) | (
                SettingKey::EmbeddingsModel,
                SettingsRemoteValue::EmbeddingModel { model: None }
            )
        );
        if !valid_default
            || (self.key == SettingKey::EmbeddingsModel && self.desired.is_some())
            || matches!(&self.state, SettingState::Unavailable) != self.unavailable_reason.is_some()
        {
            return Err(SettingsReadContractError::Invalid);
        }
        self.default.validate_for(self.key)?;
        if let Some(desired) = &self.desired {
            desired.validate_for(self.key)?;
        }
        if let Some(effective) = &self.effective {
            effective.validate_for(self.key)?;
        }
        if matches!(&self.state, SettingState::Effective)
            && (self.effective.is_none()
                || self
                    .desired
                    .as_ref()
                    .is_some_and(|desired| Some(desired) != self.effective.as_ref()))
        {
            return Err(SettingsReadContractError::Invalid);
        }
        Ok(())
    }
}

impl SettingsHostSnapshot {
    fn validate(&self) -> Result<(), SettingsReadContractError> {
        if !positive(self.revision)
            || !positive(self.observed_at.get())
            || self.entries.is_empty()
            || self.entries.len() > 5
            || !self
                .entries
                .windows(2)
                .all(|entries| entries[0].key < entries[1].key)
        {
            return Err(SettingsReadContractError::Invalid);
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        Ok(())
    }
}

impl SettingsOriginalInspectionLookup {
    fn validate(
        &self,
        request_id: CommandId,
        operation_id: Option<CommandId>,
    ) -> Result<(), SettingsReadContractError> {
        let Self::Recorded {
            original_generation,
            view,
        } = self
        else {
            return Ok(());
        };
        validate_generation(original_generation)?;
        drop(view.encode_checked()?);
        match view.original_kind {
            OriginalKind::ChatOpen if operation_id.is_none() => {}
            OriginalKind::Update if operation_id.is_none() => {
                if matches!(&view.link, OriginalLink::Operation { operation_id } if *operation_id != request_id)
                {
                    return Err(SettingsReadContractError::Invalid);
                }
            }
            OriginalKind::Send => match operation_id {
                None if matches!(&view.service, ServiceLookup::NotRequested) => {}
                Some(selected) if view.mutation_operation_ids.contains(&selected) => {
                    if !matches!(&view.service, ServiceLookup::NotRecorded)
                        && !matches!(&view.service, ServiceLookup::Recorded { receipt } if receipt.operation_id == selected)
                    {
                        return Err(SettingsReadContractError::Invalid);
                    }
                }
                _ => return Err(SettingsReadContractError::Invalid),
            },
            _ => return Err(SettingsReadContractError::Invalid),
        }
        Ok(())
    }
}

impl SettingsReadReplyPacket {
    pub(crate) fn snapshot(
        request: &SettingsReadRequest,
        in_reply_to: MessageId,
        snapshot: SettingsHostSnapshot,
    ) -> Result<Self, SettingsReadContractError> {
        Self::new(request, in_reply_to, ReadReplyBody::Snapshot { snapshot })
    }

    pub(crate) fn inspection(
        request: &SettingsReadRequest,
        in_reply_to: MessageId,
        observed_at: UnixMillis,
        lookup: SettingsOriginalInspectionLookup,
    ) -> Result<Self, SettingsReadContractError> {
        let SettingsReadRequest::InspectOriginal(intent) = request else {
            return Err(SettingsReadContractError::Invalid);
        };
        Self::new(
            request,
            in_reply_to,
            ReadReplyBody::OriginalInspection {
                request_id: intent.request_id,
                operation_id: intent.operation_id,
                observed_at,
                lookup,
            },
        )
    }

    pub(crate) fn denied(
        request: &SettingsReadRequest,
        in_reply_to: MessageId,
        reason: SettingsReadDenialReason,
    ) -> Result<Self, SettingsReadContractError> {
        Self::new(request, in_reply_to, ReadReplyBody::Denied { reason })
    }

    fn new(
        request: &SettingsReadRequest,
        in_reply_to: MessageId,
        body: ReadReplyBody,
    ) -> Result<Self, SettingsReadContractError> {
        request.validate()?;
        let (host_id, query_id, parent, issued_at, expires_at) = match request {
            SettingsReadRequest::Snapshot(intent) => (
                intent.host_id,
                intent.query_id,
                &intent.parent,
                intent.issued_at,
                intent.expires_at,
            ),
            SettingsReadRequest::InspectOriginal(intent) => (
                intent.host_id,
                intent.query_id,
                &intent.parent,
                intent.issued_at,
                intent.expires_at,
            ),
        };
        if in_reply_to.as_uuid().is_nil() || *in_reply_to.as_uuid() == query_id {
            return Err(SettingsReadContractError::Invalid);
        }
        let observed_at = match (request, &body) {
            (SettingsReadRequest::Snapshot(intent), ReadReplyBody::Snapshot { snapshot }) => {
                if !snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.key)
                    .eq(intent.keys.iter().copied())
                {
                    return Err(SettingsReadContractError::Invalid);
                }
                Some(snapshot.observed_at)
            }
            (
                SettingsReadRequest::InspectOriginal(intent),
                ReadReplyBody::OriginalInspection {
                    request_id,
                    operation_id,
                    observed_at,
                    ..
                },
            ) if *request_id == intent.request_id && *operation_id == intent.operation_id => {
                Some(*observed_at)
            }
            (_, ReadReplyBody::Denied { .. }) => None,
            _ => return Err(SettingsReadContractError::Invalid),
        };
        if observed_at.is_some_and(|observed| observed < issued_at || observed >= expires_at) {
            return Err(SettingsReadContractError::Invalid);
        }
        let generation = if matches!(&body, ReadReplyBody::Denied { .. }) {
            None
        } else {
            Some(parent.generation.clone())
        };
        let packet = Self {
            schema_version: SchemaVersion::CURRENT,
            payload: ReadReplyPayload::Settings {
                version: 1,
                host_id,
                message: ReadReplyMessage::Response {
                    in_reply_to,
                    correlation: ReadCorrelation::Query { query_id },
                    generation,
                    body,
                },
            },
        };
        packet.validate()?;
        Ok(packet)
    }

    fn validate(&self) -> Result<usize, SettingsReadContractError> {
        let ReadReplyPayload::Settings {
            version,
            host_id,
            message:
                ReadReplyMessage::Response {
                    in_reply_to,
                    correlation: ReadCorrelation::Query { query_id },
                    generation,
                    body,
                },
        } = &self.payload;
        if self.schema_version.get() != 1
            || *version != 1
            || host_id.as_uuid().is_nil()
            || in_reply_to.as_uuid().is_nil()
            || query_id.is_nil()
            || in_reply_to.as_uuid() == query_id
        {
            return Err(SettingsReadContractError::Invalid);
        }
        if let Some(generation) = generation {
            validate_generation(generation)?;
        }
        match body {
            ReadReplyBody::Snapshot { snapshot } if generation.is_some() => {
                snapshot.validate()?;
                Ok(MAX_SNAPSHOT_PACKET_BYTES)
            }
            ReadReplyBody::OriginalInspection {
                request_id,
                operation_id,
                observed_at,
                lookup,
            } if generation.is_some() => {
                if request_id.as_uuid().is_nil()
                    || request_id.as_uuid() == query_id
                    || operation_id.is_some_and(|id| id.as_uuid().is_nil())
                    || !positive(observed_at.get())
                {
                    return Err(SettingsReadContractError::Invalid);
                }
                lookup.validate(*request_id, *operation_id)?;
                Ok(MAX_INSPECTION_PACKET_BYTES)
            }
            ReadReplyBody::Denied { .. } if generation.is_none() => Ok(MAX_DENIAL_PACKET_BYTES),
            _ => Err(SettingsReadContractError::Invalid),
        }
    }

    pub(crate) fn encode_checked(&self) -> Result<Zeroizing<Vec<u8>>, SettingsReadContractError> {
        let maximum = self.validate()?;
        let mut encoded = Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *encoded, self).map_err(SettingsReadContractError::Encoding)?;
        if encoded.len() > maximum {
            return Err(SettingsReadContractError::TooLarge);
        }
        Ok(encoded)
    }
}

fn positive(value: u64) -> bool {
    value > 0 && value <= i64::MAX as u64
}

fn validate_text(value: &str) -> Result<(), SettingsReadContractError> {
    if value.len() > 256 || value.trim().is_empty() || value.contains('\0') {
        return Err(SettingsReadContractError::Invalid);
    }
    Ok(())
}

fn validate_generation(generation: &BindingGeneration) -> Result<(), SettingsReadContractError> {
    if generation.grant_id.is_nil()
        || generation.host_epoch.is_nil()
        || generation.connection_epoch.is_nil()
        || !positive(generation.grant_revision)
    {
        return Err(SettingsReadContractError::Invalid);
    }
    Ok(())
}

impl fmt::Debug for SettingsRemoteValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsRemoteValue([redacted])")
    }
}

impl fmt::Debug for SettingsRemoteModelSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsRemoteModelSelection([redacted])")
    }
}

impl fmt::Debug for SettingsRemoteToolSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsRemoteToolSelection([redacted])")
    }
}

impl fmt::Debug for SettingsRemoteEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsRemoteEntry([redacted])")
    }
}

impl fmt::Debug for SettingsHostSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsHostSnapshot([redacted])")
    }
}

impl fmt::Debug for SettingsOriginalInspectionLookup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsOriginalInspectionLookup([redacted])")
    }
}

impl fmt::Debug for SettingsReadReplyPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReadReplyPacket([redacted])")
    }
}
