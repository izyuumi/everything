use std::fmt;

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::host::settings::contract::{
    SettingKey, SettingsFailureReason, SettingsUnavailableReason,
};
use crate::host::settings::{
    SettingsApplicationState, SettingsWriteInspection, SettingsWriteResolution,
};
use crate::protocol::{CommandId, DeviceId, MessageId, SchemaVersion, UnixMillis};
use crate::storage::settings_originals::RemoteOriginalTerminal;

use super::BindingGeneration;

const MAX_SERVICE_RECEIPT_BYTES: usize = 2048;
const MAX_ORIGINAL_VIEW_BYTES: usize = 4096;
const MAX_ORIGINAL_PACKET_BYTES: usize = 65_536;
const MAX_MUTATION_OPERATIONS: usize = 8;

#[derive(Serialize)]
pub(crate) struct OriginalView {
    pub(crate) original_kind: OriginalKind,
    pub(crate) terminal: Option<RemoteOriginalTerminal>,
    pub(crate) link: OriginalLink,
    pub(crate) mutation_operation_ids: Vec<CommandId>,
    pub(crate) service: ServiceLookup,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OriginalKind {
    ChatOpen,
    Send,
    Update,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum OriginalLink {
    Unlinked,
    Chat { chat_id: Uuid },
    Turn { chat_id: Uuid, turn_id: CommandId },
    Operation { operation_id: CommandId },
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ServiceLookup {
    NotRequested,
    NotRecorded,
    Recorded { receipt: RemoteServiceReceipt },
}

#[derive(Serialize)]
pub(crate) struct RemoteServiceReceipt {
    pub(crate) operation_id: CommandId,
    pub(crate) expected_revision: Option<u64>,
    pub(crate) updated_at: UnixMillis,
    pub(crate) phase: RemoteOperationPhase,
    pub(crate) write: Option<SettingsWriteInspection>,
    pub(crate) application: Option<SettingsApplicationState>,
}

#[derive(Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub(crate) enum RemoteOperationPhase {
    Applying {
        expected_revision: u64,
        revision: u64,
    },
    Finished {
        state: RemoteOperationState,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RemoteOperationState {
    Applied { revision: u64 },
    PendingApplication { revision: u64 },
    RestartRequired { revision: u64 },
    Conflict { observed_revision: u64 },
    HostActionRequired,
    Unavailable { reason: SettingsUnavailableReason },
    Failed { reason: SettingsFailureReason },
    Unknown,
    Cancelled,
}

#[derive(Serialize)]
pub(crate) struct SettingsOriginalReplyPacket {
    schema_version: SchemaVersion,
    payload: ReplyPayload,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplyPayload {
    Settings {
        version: u16,
        host_id: DeviceId,
        message: ReplyMessage,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReplyMessage {
    Response {
        in_reply_to: MessageId,
        correlation: CommandCorrelation,
        generation: BindingGeneration,
        body: OriginalBody,
    },
}

#[derive(Serialize)]
#[serde(tag = "family", rename_all = "snake_case")]
enum CommandCorrelation {
    Command { request_id: CommandId },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum OriginalBody {
    Original {
        request_id: CommandId,
        view: OriginalView,
    },
}

#[derive(Error)]
pub(crate) enum SettingsReplyContractError {
    #[error("invalid Settings Original reply data")]
    Invalid,
    #[error("Settings Original reply exceeds its encoded byte limit")]
    TooLarge,
    #[error("Settings Original reply serialization failed")]
    Encoding(#[source] serde_json::Error),
}

impl OriginalView {
    pub(super) fn encode_checked(&self) -> Result<Zeroizing<Vec<u8>>, SettingsReplyContractError> {
        self.validate()?;
        if let ServiceLookup::Recorded { receipt } = &self.service {
            drop(encode_bounded(receipt, MAX_SERVICE_RECEIPT_BYTES)?);
        }
        encode_bounded(self, MAX_ORIGINAL_VIEW_BYTES)
    }

    fn validate(&self) -> Result<(), SettingsReplyContractError> {
        if self.mutation_operation_ids.len() > MAX_MUTATION_OPERATIONS
            || self
                .mutation_operation_ids
                .iter()
                .any(|operation_id| operation_id.as_uuid().is_nil())
            || !self
                .mutation_operation_ids
                .windows(2)
                .all(|ids| ids[0] < ids[1])
        {
            return Err(SettingsReplyContractError::Invalid);
        }
        match (&self.original_kind, &self.link) {
            (OriginalKind::ChatOpen, OriginalLink::Unlinked | OriginalLink::Chat { .. })
            | (OriginalKind::Send, OriginalLink::Unlinked | OriginalLink::Turn { .. })
            | (OriginalKind::Update, OriginalLink::Unlinked | OriginalLink::Operation { .. }) => {}
            _ => return Err(SettingsReplyContractError::Invalid),
        }
        match &self.link {
            OriginalLink::Unlinked => {
                if !self.mutation_operation_ids.is_empty()
                    || !matches!(self.service, ServiceLookup::NotRequested)
                {
                    return Err(SettingsReplyContractError::Invalid);
                }
            }
            OriginalLink::Chat { chat_id } => {
                if chat_id.is_nil()
                    || !self.mutation_operation_ids.is_empty()
                    || !matches!(self.service, ServiceLookup::NotRequested)
                {
                    return Err(SettingsReplyContractError::Invalid);
                }
            }
            OriginalLink::Turn { chat_id, turn_id } => {
                if chat_id.is_nil() || turn_id.as_uuid().is_nil() {
                    return Err(SettingsReplyContractError::Invalid);
                }
                match &self.service {
                    ServiceLookup::NotRequested => {}
                    ServiceLookup::NotRecorded if self.mutation_operation_ids.is_empty() => {
                        return Err(SettingsReplyContractError::Invalid);
                    }
                    ServiceLookup::NotRecorded => {}
                    ServiceLookup::Recorded { receipt }
                        if !self.mutation_operation_ids.contains(&receipt.operation_id) =>
                    {
                        return Err(SettingsReplyContractError::Invalid);
                    }
                    ServiceLookup::Recorded { .. } => {}
                }
            }
            OriginalLink::Operation { operation_id } => {
                if operation_id.as_uuid().is_nil() || !self.mutation_operation_ids.is_empty() {
                    return Err(SettingsReplyContractError::Invalid);
                }
                match &self.service {
                    ServiceLookup::NotRequested => return Err(SettingsReplyContractError::Invalid),
                    ServiceLookup::Recorded { receipt }
                        if receipt.operation_id != *operation_id =>
                    {
                        return Err(SettingsReplyContractError::Invalid);
                    }
                    ServiceLookup::NotRecorded | ServiceLookup::Recorded { .. } => {}
                }
            }
        }
        if let ServiceLookup::Recorded { receipt } = &self.service {
            receipt.validate()?;
        }
        Ok(())
    }
}

impl RemoteServiceReceipt {
    fn validate(&self) -> Result<(), SettingsReplyContractError> {
        if self.operation_id.as_uuid().is_nil() || self.updated_at.get() == 0 {
            return Err(SettingsReplyContractError::Invalid);
        }
        if let Some(expected_revision) = self.expected_revision {
            validate_revision(expected_revision)?;
        }
        match &self.phase {
            RemoteOperationPhase::Applying {
                expected_revision,
                revision,
            } => {
                if self.write.as_ref().is_none_or(|write| {
                    write.resolution.is_some()
                        || write.expected_revision != *expected_revision
                        || write.revision != *revision
                }) {
                    return Err(SettingsReplyContractError::Invalid);
                }
            }
            RemoteOperationPhase::Finished { state } => {
                state.validate()?;
                if self.write.is_none()
                    && matches!(
                        state,
                        RemoteOperationState::Applied { .. }
                            | RemoteOperationState::PendingApplication { .. }
                            | RemoteOperationState::RestartRequired { .. }
                    )
                {
                    return Err(SettingsReplyContractError::Invalid);
                }
            }
        }
        if let Some(write) = &self.write {
            if write.expected_revision == 0
                || write.expected_revision >= i64::MAX as u64
                || write.expected_revision.checked_add(1) != Some(write.revision)
                || self.expected_revision != Some(write.expected_revision)
                || write.changed_keys.is_empty()
                || write.changed_keys.len() > 2
                || !write.changed_keys.windows(2).all(|keys| keys[0] < keys[1])
                || !write.changed_keys.iter().all(|key| {
                    matches!(
                        key,
                        SettingKey::EmbeddingsEnabled | SettingKey::EmbeddingsDimensions
                    )
                })
            {
                return Err(SettingsReplyContractError::Invalid);
            }
            let valid_resolution = match (&self.phase, write.resolution) {
                (RemoteOperationPhase::Applying { .. }, None) => true,
                (
                    RemoteOperationPhase::Finished {
                        state: RemoteOperationState::Unknown,
                    },
                    None,
                ) => true,
                (
                    RemoteOperationPhase::Finished {
                        state:
                            RemoteOperationState::Applied { revision }
                            | RemoteOperationState::PendingApplication { revision }
                            | RemoteOperationState::RestartRequired { revision },
                    },
                    Some(SettingsWriteResolution::Applied),
                ) => *revision == write.revision,
                (
                    RemoteOperationPhase::Finished {
                        state:
                            RemoteOperationState::Failed { .. }
                            | RemoteOperationState::Unavailable { .. }
                            | RemoteOperationState::Cancelled
                            | RemoteOperationState::Conflict { .. },
                    },
                    Some(SettingsWriteResolution::NotApplied),
                ) => true,
                _ => false,
            };
            if !valid_resolution {
                return Err(SettingsReplyContractError::Invalid);
            }
        }
        if self.application.is_some()
            && (!matches!(
                self.phase,
                RemoteOperationPhase::Finished {
                    state: RemoteOperationState::PendingApplication { .. },
                }
            ) || self
                .write
                .as_ref()
                .is_none_or(|write| write.resolution != Some(SettingsWriteResolution::Applied)))
        {
            return Err(SettingsReplyContractError::Invalid);
        }
        Ok(())
    }
}

impl RemoteOperationState {
    fn validate(&self) -> Result<(), SettingsReplyContractError> {
        match *self {
            Self::Applied { revision }
            | Self::PendingApplication { revision }
            | Self::RestartRequired { revision } => validate_revision(revision),
            Self::Conflict { observed_revision } => validate_revision(observed_revision),
            _ => Ok(()),
        }
    }
}

impl SettingsOriginalReplyPacket {
    pub(crate) fn new(
        host_id: DeviceId,
        in_reply_to: MessageId,
        request_id: CommandId,
        generation: BindingGeneration,
        view: OriginalView,
    ) -> Result<Self, SettingsReplyContractError> {
        if host_id.as_uuid().is_nil()
            || in_reply_to.as_uuid().is_nil()
            || request_id.as_uuid().is_nil()
            || generation.grant_id.is_nil()
            || generation.host_epoch.is_nil()
            || generation.connection_epoch.is_nil()
        {
            return Err(SettingsReplyContractError::Invalid);
        }
        validate_revision(generation.grant_revision)?;
        view.validate()?;
        Ok(Self {
            schema_version: SchemaVersion::CURRENT,
            payload: ReplyPayload::Settings {
                version: 1,
                host_id,
                message: ReplyMessage::Response {
                    in_reply_to,
                    correlation: CommandCorrelation::Command { request_id },
                    generation,
                    body: OriginalBody::Original { request_id, view },
                },
            },
        })
    }

    pub(crate) fn encode_checked(&self) -> Result<Zeroizing<Vec<u8>>, SettingsReplyContractError> {
        let ReplyPayload::Settings {
            message:
                ReplyMessage::Response {
                    body: OriginalBody::Original { view, .. },
                    ..
                },
            ..
        } = &self.payload;
        drop(view.encode_checked()?);
        encode_bounded(self, MAX_ORIGINAL_PACKET_BYTES)
    }
}

fn validate_revision(revision: u64) -> Result<(), SettingsReplyContractError> {
    if revision == 0 || revision > i64::MAX as u64 {
        return Err(SettingsReplyContractError::Invalid);
    }
    Ok(())
}

fn encode_bounded(
    value: &impl Serialize,
    maximum: usize,
) -> Result<Zeroizing<Vec<u8>>, SettingsReplyContractError> {
    let mut encoded = Zeroizing::new(Vec::new());
    serde_json::to_writer(&mut *encoded, value).map_err(SettingsReplyContractError::Encoding)?;
    if encoded.len() > maximum {
        return Err(SettingsReplyContractError::TooLarge);
    }
    Ok(encoded)
}

impl fmt::Debug for OriginalView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OriginalView([redacted])")
    }
}

impl fmt::Debug for OriginalLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OriginalLink([redacted])")
    }
}

impl fmt::Debug for ServiceLookup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ServiceLookup([redacted])")
    }
}

impl fmt::Debug for RemoteServiceReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteServiceReceipt([redacted])")
    }
}

impl fmt::Debug for RemoteOperationPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteOperationPhase([redacted])")
    }
}

impl fmt::Debug for RemoteOperationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RemoteOperationState([redacted])")
    }
}

impl fmt::Debug for SettingsOriginalReplyPacket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsOriginalReplyPacket([redacted])")
    }
}

impl fmt::Debug for SettingsReplyContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Invalid => "SettingsReplyContractError::Invalid",
            Self::TooLarge => "SettingsReplyContractError::TooLarge",
            Self::Encoding(_) => "SettingsReplyContractError::Encoding([redacted])",
        })
    }
}
