use rusqlite::{Connection, Row, Transaction, params, types::ValueRef};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::host::settings::actions::SettingsActionTarget;
use crate::host::settings::contract::{
    SettingKey, SettingValue, SettingsRequestBinding, SettingsScope,
};
use crate::protocol::{CommandId, ConversationId, DeviceId, UnixMillis};

use super::settings_agent::{SettingsSessionReservation, read_stored_session_on};
use super::{StorageError, StorageResult, invalid, missing};

const MAX_ORIGINAL_BASE_BYTES: usize = 1024;
const MAX_ORIGINAL_LINK_BYTES: usize = 512;
const MAX_ORIGINAL_CLOSE_BYTES: usize = 128;
const MAX_ORIGINAL_TERMINAL_BYTES: usize = 512;
const MAX_RESERVATION_BYTES: usize = 4096;
const MAX_OPERATION_IDENTITY_BYTES: usize = 18 * 1024;
const MAX_ORIGINALS: i64 = 16_384;
const MAX_ACTOR_ORIGINALS: i64 = 2_048;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteOriginalKey {
    pub host_id: DeviceId,
    pub actor_device_id: DeviceId,
    pub request_id: CommandId,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemoteDimensionSelection {
    BackendDefault,
    Selected { dimensions: u32 },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemotePublicCommand {
    ChatOpen,
    Send {
        chat_id: Uuid,
        provider_binding_sha256: String,
    },
    Update {
        expected_settings_revision: u64,
        enabled: Option<bool>,
        dimensions: Option<RemoteDimensionSelection>,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteAcceptedIdentity {
    pub version: u16,
    pub host_id: DeviceId,
    pub owner_principal_id: Uuid,
    pub peer_keys: DevicePublicKeys,
    pub request_id: CommandId,
    pub remote_parent_id: Uuid,
    pub grant_id: Uuid,
    pub grant_revision: u64,
    pub host_epoch: Uuid,
    pub connection_epoch: Uuid,
    pub admitted_at: UnixMillis,
    pub issued_at: UnixMillis,
    pub expires_at: UnixMillis,
    pub command: RemotePublicCommand,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSuppressedOpenIdentity {
    pub version: u16,
    pub host_id: DeviceId,
    pub owner_principal_id: Uuid,
    pub peer_keys: DevicePublicKeys,
    pub request_id: CommandId,
    pub remote_parent_id: Uuid,
    pub grant_id: Uuid,
    pub grant_revision: u64,
    pub host_epoch: Uuid,
    pub connection_epoch: Uuid,
    pub admitted_at: UnixMillis,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "receipt", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemoteOriginalIdentity {
    Accepted(RemoteAcceptedIdentity),
    SuppressedChatOpen(RemoteSuppressedOpenIdentity),
}

pub(crate) enum RemoteOriginalAdmission {
    Accepted(RemoteAcceptedIdentity),
    SuppressedOpen {
        identity: RemoteSuppressedOpenIdentity,
        closed_at: UnixMillis,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemoteOriginalLink {
    Chat {
        session_id: Uuid,
    },
    Turn {
        session_id: Uuid,
        command_id: CommandId,
    },
    Operation {
        operation_id: CommandId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteCloseReason {
    UserRequested,
    GrantChanged,
    ConnectionRetired,
    ParentExpired,
    HostShutdown,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteOriginalClose {
    pub requested_at: UnixMillis,
    pub reason: RemoteCloseReason,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemoteOriginalTerminal {
    Completed,
    Denied,
    Suppressed,
    RetiredLostRam,
    Unknown,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct RemoteOriginalRecord {
    pub key: RemoteOriginalKey,
    pub identity: RemoteOriginalIdentity,
    pub link: Option<RemoteOriginalLink>,
    pub close: Option<RemoteOriginalClose>,
    pub terminal: Option<RemoteOriginalTerminal>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PairedSettingsSessionReservation {
    pub version: u16,
    pub session_id: Uuid,
    pub host_id: DeviceId,
    pub owner_principal_id: Uuid,
    pub peer_keys: DevicePublicKeys,
    pub remote_parent_id: Uuid,
    pub grant_id: Uuid,
    pub grant_revision: u64,
    pub host_epoch: Uuid,
    pub connection_epoch: Uuid,
    pub open_request_id: CommandId,
    pub conversation_id: ConversationId,
    pub create_command_id: CommandId,
    pub provider_binding_sha256: String,
    pub tools_sha256: String,
    pub created_at: UnixMillis,
    pub expires_at: UnixMillis,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum StoredSettingsSessionReservation {
    Native(SettingsSessionReservation),
    Paired(PairedSettingsSessionReservation),
}

impl RemoteOriginalKey {
    fn validate(self) -> StorageResult<()> {
        if self.host_id.as_uuid().is_nil()
            || self.actor_device_id.as_uuid().is_nil()
            || self.request_id.as_uuid().is_nil()
            || self.host_id == self.actor_device_id
        {
            return invalid("invalid remote Settings original key");
        }
        Ok(())
    }
}

impl RemoteOriginalIdentity {
    pub(crate) fn key(&self) -> RemoteOriginalKey {
        match self {
            Self::Accepted(identity) => RemoteOriginalKey {
                host_id: identity.host_id,
                actor_device_id: identity.peer_keys.device_id,
                request_id: identity.request_id,
            },
            Self::SuppressedChatOpen(identity) => RemoteOriginalKey {
                host_id: identity.host_id,
                actor_device_id: identity.peer_keys.device_id,
                request_id: identity.request_id,
            },
        }
    }

    fn command_kind(&self) -> &'static str {
        match self {
            Self::Accepted(identity) => match &identity.command {
                RemotePublicCommand::ChatOpen => "chat_open",
                RemotePublicCommand::Send { .. } => "send",
                RemotePublicCommand::Update { .. } => "update",
            },
            Self::SuppressedChatOpen(_) => "chat_open",
        }
    }

    fn validate(&self) -> StorageResult<()> {
        self.key().validate()?;
        let (version, owner, peer, parent, grant, revision, host_epoch, connection_epoch, admitted) =
            match self {
                Self::Accepted(identity) => {
                    if identity.expires_at <= identity.issued_at
                        || identity.expires_at.get() - identity.issued_at.get() > 30_000
                        || identity.admitted_at < identity.issued_at
                        || identity.admitted_at >= identity.expires_at
                    {
                        return invalid("invalid remote Settings original interval");
                    }
                    identity.command.validate()?;
                    (
                        identity.version,
                        identity.owner_principal_id,
                        &identity.peer_keys,
                        identity.remote_parent_id,
                        identity.grant_id,
                        identity.grant_revision,
                        identity.host_epoch,
                        identity.connection_epoch,
                        identity.admitted_at,
                    )
                }
                Self::SuppressedChatOpen(identity) => (
                    identity.version,
                    identity.owner_principal_id,
                    &identity.peer_keys,
                    identity.remote_parent_id,
                    identity.grant_id,
                    identity.grant_revision,
                    identity.host_epoch,
                    identity.connection_epoch,
                    identity.admitted_at,
                ),
            };
        peer.validate()
            .map_err(|_| malformed("invalid remote Settings public keys"))?;
        if version != 1
            || owner != *self.key().host_id.as_uuid()
            || parent.is_nil()
            || grant.is_nil()
            || revision == 0
            || revision > i64::MAX as u64
            || host_epoch.is_nil()
            || connection_epoch.is_nil()
            || admitted.get() > i64::MAX as u64
        {
            return invalid("invalid remote Settings original provenance");
        }
        Ok(())
    }
}

impl RemotePublicCommand {
    fn validate(&self) -> StorageResult<()> {
        match self {
            Self::ChatOpen => Ok(()),
            Self::Send {
                chat_id,
                provider_binding_sha256,
            } if !chat_id.is_nil() && digest(provider_binding_sha256) => Ok(()),
            Self::Update {
                expected_settings_revision,
                enabled,
                dimensions,
            } if *expected_settings_revision > 0
                && *expected_settings_revision <= i64::MAX as u64
                && (enabled.is_some() || dimensions.is_some())
                && !matches!(dimensions, Some(RemoteDimensionSelection::Selected { dimensions }) if !(1..=65_536).contains(dimensions)) =>
            {
                Ok(())
            }
            _ => invalid("invalid remote Settings public command"),
        }
    }
}

impl PairedSettingsSessionReservation {
    pub(crate) fn validate(&self) -> StorageResult<()> {
        self.peer_keys
            .validate()
            .map_err(|_| malformed("invalid paired Settings public keys"))?;
        if self.version != 2
            || self.session_id.is_nil()
            || self.host_id.as_uuid().is_nil()
            || self.owner_principal_id != *self.host_id.as_uuid()
            || self.peer_keys.device_id == self.host_id
            || self.remote_parent_id.is_nil()
            || self.grant_id.is_nil()
            || self.grant_revision == 0
            || self.grant_revision > i64::MAX as u64
            || self.host_epoch.is_nil()
            || self.connection_epoch.is_nil()
            || self.open_request_id.as_uuid().is_nil()
            || self.conversation_id.as_uuid().is_nil()
            || self.create_command_id.as_uuid().is_nil()
            || self.expires_at <= self.created_at
            || self.expires_at.get() - self.created_at.get() > 300_000
            || self.expires_at.get() > i64::MAX as u64
            || !digest(&self.provider_binding_sha256)
            || !digest(&self.tools_sha256)
        {
            return invalid("invalid paired Settings session reservation");
        }
        encode(self, MAX_RESERVATION_BYTES)?;
        Ok(())
    }
}

impl StoredSettingsSessionReservation {
    pub(crate) fn session_id(&self) -> Uuid {
        match self {
            Self::Native(value) => value.session_id,
            Self::Paired(value) => value.session_id,
        }
    }

    pub(crate) fn conversation_id(&self) -> ConversationId {
        match self {
            Self::Native(value) => value.conversation_id,
            Self::Paired(value) => value.conversation_id,
        }
    }

    pub(crate) fn create_command_id(&self) -> CommandId {
        match self {
            Self::Native(value) => value.create_command_id,
            Self::Paired(value) => value.create_command_id,
        }
    }

    pub(crate) fn created_at(&self) -> UnixMillis {
        match self {
            Self::Native(value) => value.created_at,
            Self::Paired(value) => value.created_at,
        }
    }

    pub(crate) fn expires_at(&self) -> UnixMillis {
        match self {
            Self::Native(value) => value.expires_at,
            Self::Paired(value) => value.expires_at,
        }
    }

    pub(crate) fn owner_principal_id(&self) -> Uuid {
        match self {
            Self::Native(value) => value.principal_id,
            Self::Paired(value) => value.owner_principal_id,
        }
    }

    pub(crate) fn encode(&self) -> StorageResult<String> {
        match self {
            Self::Native(value) => encode(value, MAX_RESERVATION_BYTES),
            Self::Paired(value) => encode(value, MAX_RESERVATION_BYTES),
        }
    }
}

pub(crate) fn decode_stored_reservation(
    encoded: &str,
) -> StorageResult<StoredSettingsSessionReservation> {
    #[derive(Deserialize)]
    struct Version {
        version: u16,
    }
    if encoded.len() > MAX_RESERVATION_BYTES {
        return invalid("stored Settings reservation exceeds its bound");
    }
    let version: Version = serde_json::from_str(encoded)
        .map_err(|_| malformed("invalid stored Settings reservation version"))?;
    let value = match version.version {
        1 => StoredSettingsSessionReservation::Native(decode(encoded, MAX_RESERVATION_BYTES)?),
        2 => StoredSettingsSessionReservation::Paired(decode(encoded, MAX_RESERVATION_BYTES)?),
        _ => return invalid("unsupported stored Settings reservation version"),
    };
    match &value {
        StoredSettingsSessionReservation::Native(native) => {
            super::settings_agent::validate_session(native)?;
        }
        StoredSettingsSessionReservation::Paired(paired) => paired.validate()?,
    }
    if value.encode()? != encoded {
        return invalid("stored Settings reservation is not canonical");
    }
    Ok(value)
}

pub(crate) fn read_original_on(
    connection: &Connection,
    key: RemoteOriginalKey,
) -> StorageResult<Option<RemoteOriginalRecord>> {
    key.validate()?;
    let mut statement = connection.prepare("SELECT host_id,actor_device_id,request_id,command_kind,identity,link_kind,session_id,turn_command_id,operation_id,link,close,terminal FROM settings_remote_originals WHERE host_id=?1 AND actor_device_id=?2 AND request_id=?3")?;
    let mut rows = statement.query(params![
        key.host_id.to_string(),
        key.actor_device_id.to_string(),
        key.request_id.to_string()
    ])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    if text(row, 0, 36)? != key.host_id.to_string()
        || text(row, 1, 36)? != key.actor_device_id.to_string()
        || text(row, 2, 36)? != key.request_id.to_string()
    {
        return invalid("stored remote Settings original key differs");
    }
    let record = RemoteOriginalRecord {
        key,
        identity: decode(
            text(row, 4, MAX_ORIGINAL_BASE_BYTES)?,
            MAX_ORIGINAL_BASE_BYTES,
        )?,
        link: decode_optional(row, 9, MAX_ORIGINAL_LINK_BYTES)?,
        close: decode_optional(row, 10, MAX_ORIGINAL_CLOSE_BYTES)?,
        terminal: decode_optional(row, 11, MAX_ORIGINAL_TERMINAL_BYTES)?,
    };
    record.validate()?;
    if record.identity.command_kind() != text(row, 3, 9)? {
        return invalid("stored remote Settings original command differs");
    }
    let actual = (
        optional_text(row, 5, 9)?,
        optional_text(row, 6, 36)?,
        optional_text(row, 7, 36)?,
        optional_text(row, 8, 36)?,
    );
    let expected = match &record.link {
        None => (None, None, None, None),
        Some(RemoteOriginalLink::Chat { session_id }) => {
            (Some("chat"), Some(session_id.to_string()), None, None)
        }
        Some(RemoteOriginalLink::Turn {
            session_id,
            command_id,
        }) => (
            Some("turn"),
            Some(session_id.to_string()),
            Some(command_id.to_string()),
            None,
        ),
        Some(RemoteOriginalLink::Operation { operation_id }) => (
            Some("operation"),
            None,
            None,
            Some(operation_id.to_string()),
        ),
    };
    if actual
        != (
            expected.0,
            expected.1.as_deref(),
            expected.2.as_deref(),
            expected.3.as_deref(),
        )
    {
        return invalid("stored remote Settings original link scalar differs");
    }
    drop(rows);
    drop(statement);
    validate_linked_root(connection, &record)?;
    Ok(Some(record))
}

pub(crate) fn insert_original_in(
    transaction: &Transaction<'_>,
    admission: &RemoteOriginalAdmission,
) -> StorageResult<RemoteOriginalRecord> {
    let record = match admission {
        RemoteOriginalAdmission::Accepted(identity) => RemoteOriginalRecord {
            key: RemoteOriginalKey {
                host_id: identity.host_id,
                actor_device_id: identity.peer_keys.device_id,
                request_id: identity.request_id,
            },
            identity: RemoteOriginalIdentity::Accepted(identity.clone()),
            link: None,
            close: None,
            terminal: None,
        },
        RemoteOriginalAdmission::SuppressedOpen {
            identity,
            closed_at,
        } => RemoteOriginalRecord {
            key: RemoteOriginalKey {
                host_id: identity.host_id,
                actor_device_id: identity.peer_keys.device_id,
                request_id: identity.request_id,
            },
            identity: RemoteOriginalIdentity::SuppressedChatOpen(identity.clone()),
            link: None,
            close: Some(RemoteOriginalClose {
                requested_at: *closed_at,
                reason: RemoteCloseReason::UserRequested,
            }),
            terminal: Some(RemoteOriginalTerminal::Suppressed),
        },
    };
    record.validate()?;
    let (global, actor): (i64, i64) = transaction.query_row(
        "SELECT (SELECT count(*) FROM settings_remote_originals),(SELECT count(*) FROM settings_remote_originals WHERE actor_device_id=?1)",
        [record.key.actor_device_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if global >= MAX_ORIGINALS || actor >= MAX_ACTOR_ORIGINALS {
        return Err(StorageError::TransportCapacity("remote Settings originals"));
    }
    let identity = encode(&record.identity, MAX_ORIGINAL_BASE_BYTES)?;
    let close = record
        .close
        .as_ref()
        .map(|value| encode(value, MAX_ORIGINAL_CLOSE_BYTES))
        .transpose()?;
    let terminal = record
        .terminal
        .as_ref()
        .map(|value| encode(value, MAX_ORIGINAL_TERMINAL_BYTES))
        .transpose()?;
    transaction.execute("INSERT INTO settings_remote_originals(host_id,actor_device_id,request_id,command_kind,identity,close,terminal) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![record.key.host_id.to_string(),record.key.actor_device_id.to_string(),record.key.request_id.to_string(),record.identity.command_kind(),identity,close,terminal])?;
    let installed = required_original(transaction, record.key)?;
    if installed != record {
        return invalid("remote Settings original insertion differs");
    }
    Ok(installed)
}

pub(crate) fn close_original_in(
    transaction: &Transaction<'_>,
    key: RemoteOriginalKey,
    close: &RemoteOriginalClose,
) -> StorageResult<RemoteOriginalRecord> {
    let mut original = required_original(transaction, key)?;
    if let Some(previous) = &original.close {
        return if previous == close {
            Ok(original)
        } else {
            conflict(key)
        };
    }
    original.close = Some(close.clone());
    original.validate()?;
    let encoded = encode(close, MAX_ORIGINAL_CLOSE_BYTES)?;
    let changed = transaction.execute("UPDATE settings_remote_originals SET close=?4 WHERE host_id=?1 AND actor_device_id=?2 AND request_id=?3 AND close IS NULL",params![key.host_id.to_string(),key.actor_device_id.to_string(),key.request_id.to_string(),encoded])?;
    if changed != 1 {
        return conflict(key);
    }
    let installed = required_original(transaction, key)?;
    if installed != original {
        return invalid("remote Settings original close differs");
    }
    Ok(installed)
}

pub(crate) fn finish_original_in(
    transaction: &Transaction<'_>,
    key: RemoteOriginalKey,
    terminal: RemoteOriginalTerminal,
) -> StorageResult<RemoteOriginalRecord> {
    let mut original = required_original(transaction, key)?;
    if let Some(previous) = original.terminal {
        return if previous == terminal {
            Ok(original)
        } else {
            conflict(key)
        };
    }
    original.terminal = Some(terminal);
    original.validate()?;
    let encoded = encode(&terminal, MAX_ORIGINAL_TERMINAL_BYTES)?;
    let changed = transaction.execute("UPDATE settings_remote_originals SET terminal=?4 WHERE host_id=?1 AND actor_device_id=?2 AND request_id=?3 AND terminal IS NULL",params![key.host_id.to_string(),key.actor_device_id.to_string(),key.request_id.to_string(),encoded])?;
    if changed != 1 {
        return conflict(key);
    }
    let installed = required_original(transaction, key)?;
    if installed != original {
        return invalid("remote Settings original terminal differs");
    }
    Ok(installed)
}

impl RemoteOriginalRecord {
    fn validate(&self) -> StorageResult<()> {
        self.identity.validate()?;
        if self.key != self.identity.key() {
            return invalid("remote Settings original identity differs");
        }
        let base_bytes = encode(&self.identity, MAX_ORIGINAL_BASE_BYTES)?.len()
            + self.key.host_id.to_string().len()
            + self.key.actor_device_id.to_string().len()
            + self.key.request_id.to_string().len()
            + self.identity.command_kind().len();
        if base_bytes > MAX_ORIGINAL_BASE_BYTES {
            return invalid("remote Settings original base exceeds its reserve");
        }
        if let Some(close) = &self.close {
            if close.requested_at.get() > i64::MAX as u64 {
                return invalid("remote Settings original close exceeds storage range");
            }
            encode(close, MAX_ORIGINAL_CLOSE_BYTES)?;
        }
        if let Some(terminal) = &self.terminal {
            encode(terminal, MAX_ORIGINAL_TERMINAL_BYTES)?;
            if *terminal == RemoteOriginalTerminal::Suppressed
                && (self.close.is_none() || self.link.is_some())
            {
                return invalid("remote Settings suppression is not closed and unlinked");
            }
        }
        if matches!(
            &self.identity,
            RemoteOriginalIdentity::SuppressedChatOpen(_)
        ) && (self.link.is_some()
            || self.close.is_none()
            || self
                .close
                .as_ref()
                .is_some_and(|close| close.reason != RemoteCloseReason::UserRequested)
            || self.terminal != Some(RemoteOriginalTerminal::Suppressed))
        {
            return invalid("remote Settings suppressed original shape differs");
        }
        let link_bytes = match &self.link {
            None => 0,
            Some(link @ RemoteOriginalLink::Chat { session_id })
                if !session_id.is_nil() && self.identity.command_kind() == "chat_open" =>
            {
                encode(link, MAX_ORIGINAL_LINK_BYTES)?.len() + 4 + session_id.to_string().len()
            }
            Some(
                link @ RemoteOriginalLink::Turn {
                    session_id,
                    command_id,
                },
            ) if !session_id.is_nil()
                && !command_id.as_uuid().is_nil()
                && self.identity.command_kind() == "send" =>
            {
                encode(link, MAX_ORIGINAL_LINK_BYTES)?.len()
                    + 4
                    + session_id.to_string().len()
                    + command_id.to_string().len()
            }
            Some(link @ RemoteOriginalLink::Operation { operation_id })
                if !operation_id.as_uuid().is_nil() && self.identity.command_kind() == "update" =>
            {
                encode(link, MAX_ORIGINAL_LINK_BYTES)?.len() + 9 + operation_id.to_string().len()
            }
            _ => return invalid("remote Settings original link differs from command"),
        };
        if link_bytes > MAX_ORIGINAL_LINK_BYTES {
            return invalid("remote Settings original link exceeds its reserve");
        }
        Ok(())
    }
}

fn required_original(
    connection: &Connection,
    key: RemoteOriginalKey,
) -> StorageResult<RemoteOriginalRecord> {
    read_original_on(connection, key)?
        .ok_or_else(|| missing("remote Settings original", &key.request_id.to_string()))
}

fn validate_linked_root(
    connection: &Connection,
    record: &RemoteOriginalRecord,
) -> StorageResult<()> {
    if let RemoteOriginalIdentity::Accepted(identity) = &record.identity
        && let RemotePublicCommand::Send {
            chat_id,
            provider_binding_sha256,
        } = &identity.command
    {
        let reservation = paired_session(connection, *chat_id)?;
        if !identity.matches_session(&reservation)
            || *provider_binding_sha256 != reservation.provider_binding_sha256
            || identity.admitted_at < reservation.created_at
            || identity.expires_at > reservation.expires_at
        {
            return invalid("remote Settings Send original child differs");
        }
    }
    let Some(link) = &record.link else {
        return Ok(());
    };
    let RemoteOriginalIdentity::Accepted(identity) = &record.identity else {
        return invalid("suppressed remote Settings original has an execution link");
    };
    match link {
        RemoteOriginalLink::Chat { session_id } => {
            let reservation = paired_session(connection, *session_id)?;
            if !identity.matches_session(&reservation)
                || reservation.open_request_id != identity.request_id
                || !matches!(&identity.command, RemotePublicCommand::ChatOpen)
                || reservation.created_at < identity.admitted_at
                || reservation.created_at >= identity.expires_at
            {
                return invalid("remote Settings original chat relationship differs");
            }
        }
        RemoteOriginalLink::Turn {
            session_id,
            command_id,
        } => {
            let reservation = paired_session(connection, *session_id)?;
            let mut statement = connection.prepare("SELECT session_id,remote_original_request_id FROM settings_agent_turns WHERE command_id=?1")?;
            let mut rows = statement.query([command_id.to_string()])?;
            let original = match rows.next()? {
                Some(row) => Some((
                    text(row, 0, 36)?.to_owned(),
                    optional_text(row, 1, 36)?.map(str::to_owned),
                )),
                None => None,
            };
            let RemotePublicCommand::Send {
                chat_id,
                provider_binding_sha256,
            } = &identity.command
            else {
                return invalid("remote Settings original turn command differs");
            };
            if !identity.matches_session(&reservation)
                || *chat_id != *session_id
                || *provider_binding_sha256 != reservation.provider_binding_sha256
                || original
                    != Some((
                        session_id.to_string(),
                        Some(identity.request_id.to_string()),
                    ))
            {
                return invalid("remote Settings original turn relationship differs");
            }
        }
        RemoteOriginalLink::Operation { operation_id } => {
            let mut statement = connection
                .prepare("SELECT identity FROM settings_operations WHERE request_id=?1")?;
            let mut rows = statement.query([operation_id.to_string()])?;
            let row = rows
                .next()?
                .ok_or_else(|| missing("remote Settings operation", &operation_id.to_string()))?;
            let operation: PairedOperationIdentity = decode(
                text(row, 0, MAX_OPERATION_IDENTITY_BYTES)?,
                MAX_OPERATION_IDENTITY_BYTES,
            )?;
            operation.validate()?;
            if identity.request_id != *operation_id || !operation.matches_original(identity)? {
                return invalid("remote Settings original operation relationship differs");
            }
        }
    }
    Ok(())
}

fn paired_session(
    connection: &Connection,
    id: Uuid,
) -> StorageResult<PairedSettingsSessionReservation> {
    match read_stored_session_on(connection, id)? {
        Some(StoredSettingsSessionReservation::Paired(value)) => Ok(value),
        _ => invalid("remote Settings original paired session is missing"),
    }
}

impl RemoteAcceptedIdentity {
    pub(crate) fn matches_session(&self, session: &PairedSettingsSessionReservation) -> bool {
        self.host_id == session.host_id
            && self.owner_principal_id == session.owner_principal_id
            && self.peer_keys == session.peer_keys
            && self.remote_parent_id == session.remote_parent_id
            && self.grant_id == session.grant_id
            && self.grant_revision == session.grant_revision
            && self.host_epoch == session.host_epoch
            && self.connection_epoch == session.connection_epoch
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairedOperationIdentity {
    version: u16,
    binding: SettingsRequestBinding,
    target: SettingsActionTarget,
    origin: PairedOperationOrigin,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PairedOperationOrigin {
    PairedSettings {
        actor_device_id: DeviceId,
        remote_parent_id: Uuid,
        grant_id: Uuid,
        grant_revision: u64,
        host_epoch: Uuid,
        connection_epoch: Uuid,
    },
    PairedSettingsAgent {
        actor_device_id: DeviceId,
        remote_parent_id: Uuid,
        grant_id: Uuid,
        grant_revision: u64,
        host_epoch: Uuid,
        connection_epoch: Uuid,
        session_id: Uuid,
    },
}

impl PairedOperationIdentity {
    fn validate(&self) -> StorageResult<()> {
        self.binding
            .validate()
            .map_err(|_| malformed("invalid paired Settings operation binding"))?;
        let SettingsActionTarget::Patch(patch) = &self.target else {
            return invalid("paired Settings operation is outside memory preferences");
        };
        patch
            .validate()
            .map_err(|_| malformed("invalid paired Settings operation patch"))?;
        let (actor, parent, grant, revision, host_epoch, connection_epoch) = match self.origin {
            PairedOperationOrigin::PairedSettings {
                actor_device_id,
                remote_parent_id,
                grant_id,
                grant_revision,
                host_epoch,
                connection_epoch,
            }
            | PairedOperationOrigin::PairedSettingsAgent {
                actor_device_id,
                remote_parent_id,
                grant_id,
                grant_revision,
                host_epoch,
                connection_epoch,
                ..
            } => (
                actor_device_id,
                remote_parent_id,
                grant_id,
                grant_revision,
                host_epoch,
                connection_epoch,
            ),
        };
        if self.version != 2
            || self.binding.request_id != patch.request_id
            || self.binding.scope != patch.scope
            || self.binding.expected_revision != Some(patch.expected_revision)
            || self.binding.principal_id != *actor.as_uuid()
            || self.binding.client_session_id != parent
            || parent.is_nil()
            || grant.is_nil()
            || revision == 0
            || revision > i64::MAX as u64
            || host_epoch.is_nil()
            || connection_epoch.is_nil()
            || !matches!(patch.scope, SettingsScope::Host { host_id } if !host_id.as_uuid().is_nil() && host_id!=actor)
            || patch.confirmation.is_some()
            || patch.changes.len() > 2
            || patch.changes.iter().any(|change| {
                !matches!(
                    change.key,
                    SettingKey::EmbeddingsEnabled | SettingKey::EmbeddingsDimensions
                )
            })
            || !patch
                .changes
                .windows(2)
                .all(|changes| changes[0].key < changes[1].key)
            || matches!(self.origin, PairedOperationOrigin::PairedSettingsAgent { session_id,.. } if session_id.is_nil())
        {
            return invalid("invalid paired Settings operation provenance");
        }
        Ok(())
    }

    fn matches_original(&self, identity: &RemoteAcceptedIdentity) -> StorageResult<bool> {
        let RemotePublicCommand::Update {
            expected_settings_revision,
            enabled,
            dimensions,
        } = &identity.command
        else {
            return Ok(false);
        };
        let SettingsActionTarget::Patch(patch) = &self.target else {
            return Ok(false);
        };
        let PairedOperationOrigin::PairedSettings {
            actor_device_id,
            remote_parent_id,
            grant_id,
            grant_revision,
            host_epoch,
            connection_epoch,
        } = self.origin
        else {
            return Ok(false);
        };
        if self.binding.request_id != identity.request_id
            || self.binding.scope
                != (SettingsScope::Host {
                    host_id: identity.host_id,
                })
            || self.binding.principal_id != *identity.peer_keys.device_id.as_uuid()
            || actor_device_id != identity.peer_keys.device_id
            || remote_parent_id != identity.remote_parent_id
            || grant_id != identity.grant_id
            || grant_revision != identity.grant_revision
            || host_epoch != identity.host_epoch
            || connection_epoch != identity.connection_epoch
            || self.binding.issued_at != identity.issued_at
            || self.binding.expires_at != identity.expires_at
            || patch.expected_revision != *expected_settings_revision
            || patch.changes.len()
                != usize::from(enabled.is_some()) + usize::from(dimensions.is_some())
        {
            return Ok(false);
        }
        for change in &patch.changes {
            let matches = match (&change.key, &change.value) {
                (SettingKey::EmbeddingsEnabled, SettingValue::Boolean { value }) => {
                    enabled == &Some(*value)
                }
                (SettingKey::EmbeddingsDimensions, SettingValue::Dimensions { value }) => {
                    match dimensions {
                        Some(RemoteDimensionSelection::BackendDefault) => value.is_none(),
                        Some(RemoteDimensionSelection::Selected { dimensions }) => {
                            *value == Some(*dimensions)
                        }
                        None => false,
                    }
                }
                _ => false,
            };
            if !matches {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub(super) fn validate_paired_mutation_identity(
    encoded: &str,
    request_id: CommandId,
    session: &PairedSettingsSessionReservation,
) -> StorageResult<()> {
    let operation: PairedOperationIdentity = decode(encoded, MAX_OPERATION_IDENTITY_BYTES)?;
    operation.validate()?;
    let PairedOperationOrigin::PairedSettingsAgent {
        actor_device_id,
        remote_parent_id,
        grant_id,
        grant_revision,
        host_epoch,
        connection_epoch,
        session_id,
    } = operation.origin
    else {
        return invalid("Settings mutation has another paired origin");
    };
    if operation.binding.request_id != request_id
        || operation.binding.scope
            != (SettingsScope::Host {
                host_id: session.host_id,
            })
        || operation.binding.principal_id != *session.peer_keys.device_id.as_uuid()
        || actor_device_id != session.peer_keys.device_id
        || remote_parent_id != session.remote_parent_id
        || grant_id != session.grant_id
        || grant_revision != session.grant_revision
        || host_epoch != session.host_epoch
        || connection_epoch != session.connection_epoch
        || session_id != session.session_id
        || operation.binding.issued_at < session.created_at
        || operation.binding.expires_at > session.expires_at
    {
        return invalid("Settings mutation belongs to another paired session");
    }
    Ok(())
}

fn decode_optional<T: DeserializeOwned + Serialize>(
    row: &Row<'_>,
    index: usize,
    maximum: usize,
) -> StorageResult<Option<T>> {
    optional_text(row, index, maximum)?
        .map(|encoded| decode(encoded, maximum))
        .transpose()
}

fn optional_text<'a>(
    row: &'a Row<'_>,
    index: usize,
    maximum: usize,
) -> StorageResult<Option<&'a str>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        _ => text(row, index, maximum).map(Some),
    }
}

fn text<'a>(row: &'a Row<'_>, index: usize, maximum: usize) -> StorageResult<&'a str> {
    let ValueRef::Text(bytes) = row.get_ref(index)? else {
        return invalid("remote Settings metadata field has another SQL type");
    };
    if bytes.len() > maximum {
        return invalid("stored remote Settings metadata exceeds its field bound");
    }
    std::str::from_utf8(bytes).map_err(|_| malformed("invalid remote Settings metadata encoding"))
}

fn conflict<T>(key: RemoteOriginalKey) -> StorageResult<T> {
    Err(StorageError::StateConflict {
        entity: "remote Settings original",
        id: key.request_id.to_string(),
    })
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn encode<T: Serialize>(value: &T, maximum: usize) -> StorageResult<String> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| malformed("remote Settings metadata encoding failed"))?;
    if encoded.len() > maximum {
        return invalid("remote Settings metadata exceeds its bound");
    }
    Ok(encoded)
}

fn decode<T: DeserializeOwned + Serialize>(encoded: &str, maximum: usize) -> StorageResult<T> {
    if encoded.len() > maximum {
        return invalid("stored remote Settings metadata exceeds its bound");
    }
    let value: T = serde_json::from_str(encoded)
        .map_err(|_| malformed("invalid stored remote Settings metadata"))?;
    if encode(&value, maximum)? != encoded {
        return invalid("stored remote Settings metadata is not canonical");
    }
    Ok(value)
}

fn malformed(message: &'static str) -> StorageError {
    StorageError::InvalidInput(message.into())
}
