use rusqlite::{Connection, Row};
#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use uuid::Uuid;

#[cfg(unix)]
use crate::devices::{
    AuthenticatedSettingsCommandSource, AuthenticatedSettingsSource, BindRejection, BindingCreator,
    BindingIntent, BindingLifetime,
};
#[cfg(unix)]
use crate::host::settings::grants::SettingsGrantOwner;
#[cfg(unix)]
use crate::host::settings::service::{SettingsReceiptAuthority, SettingsServiceError};
#[cfg(unix)]
use crate::host::settings::{SettingsGrantHead, SettingsGrantReference, SettingsGrantState};
#[cfg(unix)]
use crate::protocol::{DeviceId, UnixMillis};

use super::{StorageError, StorageResult};

#[cfg(unix)]
use super::settings_originals::{
    RemoteAcceptedIdentity, RemoteOriginalIdentity, RemoteOriginalRecord,
};

#[cfg(unix)]
pub(crate) struct SettingsCommandAdmission<'source> {
    pub(crate) source: &'source AuthenticatedSettingsCommandSource,
    pub(crate) authority: &'source SettingsReceiptAuthority,
}

#[cfg(unix)]
#[derive(Error)]
pub(crate) enum SettingsCommandAdmissionError {
    #[error("Settings command authority rejected receipt admission")]
    Authority(#[source] SettingsServiceError),
    #[error("Settings command replay receipt capacity is exhausted")]
    ReplayCapacity(#[source] StorageError),
    #[error("Settings command Original capacity is exhausted")]
    OriginalCapacity(#[source] StorageError),
    #[error("Settings command reply capacity is exhausted")]
    ReplyCapacity(#[source] StorageError),
    #[error("Settings command receipt storage failed")]
    Storage(#[from] StorageError),
    #[error("Settings command receipt bookkeeping failed")]
    Bookkeeping,
    #[error("Settings command receipt rollback failed")]
    Rollback {
        #[source]
        cause: Box<SettingsCommandAdmissionError>,
        rollback: StorageError,
    },
}

#[cfg(unix)]
impl std::fmt::Debug for SettingsCommandAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SettingsCommandAdmissionError([retained cause])")
    }
}

#[cfg(unix)]
impl SettingsCommandAdmissionError {
    pub(crate) fn is_capacity(&self) -> bool {
        matches!(
            self,
            Self::ReplayCapacity(_) | Self::OriginalCapacity(_) | Self::ReplyCapacity(_)
        )
    }

    pub(crate) fn is_policy_rejection(&self) -> bool {
        matches!(
            self,
            Self::Authority(SettingsServiceError::Expired | SettingsServiceError::OutsideScope)
        )
    }
}

#[cfg(unix)]
pub(crate) enum SettingsCommandReceiptCommit {
    Duplicate,
    Fresh {
        original: RemoteOriginalRecord,
        reply: StoredSettingsCommandReply,
    },
    Known {
        original: RemoteOriginalRecord,
        reply: StoredSettingsCommandReply,
    },
}

#[cfg(unix)]
pub(crate) struct StoredSettingsCommandReply {
    pub(crate) reservation_id: Uuid,
    pub(crate) local_device_id: DeviceId,
    pub(crate) peer_device_id: DeviceId,
    pub(crate) provenance: String,
    pub(crate) created_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

#[cfg(unix)]
pub(super) fn matches_command_original(
    original: &RemoteOriginalRecord,
    current: &RemoteAcceptedIdentity,
) -> bool {
    let RemoteOriginalIdentity::Accepted(previous) = &original.identity else {
        return false;
    };
    let mut expected = current.clone();
    expected.admitted_at = previous.admitted_at;
    previous == &expected
}

#[cfg(unix)]
pub(super) fn reserve_command_reply_in(
    connection: &Connection,
    identity: &RemoteAcceptedIdentity,
) -> Result<StoredSettingsCommandReply, SettingsCommandAdmissionError> {
    let peer = identity.peer_keys.device_id;
    match check_settings_reply_capacity(connection, &peer.to_string(), 90_112) {
        Err(error @ StorageError::TransportCapacity(_)) => {
            return Err(SettingsCommandAdmissionError::ReplyCapacity(error));
        }
        result => result?,
    }
    let provenance = serde_json::to_string(&HostBindProvenance {
        side: "host",
        version: 1,
        local_id: identity.host_id,
        peer_keys: &identity.peer_keys,
        host_epoch: identity.host_epoch,
        connection_epoch: identity.connection_epoch,
        grant: SettingsGrantReference {
            grant_id: identity.grant_id,
            revision: identity.grant_revision,
        },
        parent_id: Some(identity.remote_parent_id),
    })
    .map_err(StorageError::from)?;
    let reservation_id = Uuid::new_v4();
    connection.execute("INSERT INTO settings_transport_reply_reservations(reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at) VALUES(?1,?2,?3,?4,'inspection',90112,?5,?6)",rusqlite::params![reservation_id.to_string(),identity.host_id.to_string(),peer.to_string(),provenance,identity.admitted_at.get() as i64,identity.expires_at.get() as i64]).map_err(StorageError::from)?;
    let actual: (String, String, String, String, String, i64, i64, i64) = connection.query_row("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1",[reservation_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).map_err(StorageError::from)?;
    if actual
        != (
            reservation_id.to_string(),
            identity.host_id.to_string(),
            peer.to_string(),
            provenance.clone(),
            "inspection".into(),
            90_112,
            identity.admitted_at.get() as i64,
            identity.expires_at.get() as i64,
        )
    {
        return Err(SettingsCommandAdmissionError::Bookkeeping);
    }
    Ok(StoredSettingsCommandReply {
        reservation_id,
        local_device_id: identity.host_id,
        peer_device_id: peer,
        provenance,
        created_at: identity.admitted_at,
        expires_at: identity.expires_at,
    })
}

#[cfg(unix)]
pub(crate) struct SettingsBindAdmission<'source> {
    pub(crate) source: &'source AuthenticatedSettingsSource,
    pub(crate) intent: &'source BindingIntent,
}

#[cfg(unix)]
#[derive(Error, Debug)]
pub(crate) enum SettingsBindAdmissionError {
    #[error("Bind was rejected by its current scope")]
    Rejected(BindRejection),
    #[error("Bind history capacity is exhausted")]
    HistoryCapacity,
    #[error("Bind replay receipt capacity is exhausted")]
    ReplayCapacity(#[source] StorageError),
    #[error("Bind reply capacity is exhausted")]
    ReplyCapacity(#[source] StorageError),
    #[error("Bind storage operation failed")]
    Storage(#[from] StorageError),
    #[error("Bind bookkeeping is unavailable")]
    Bookkeeping,
}

#[cfg(unix)]
impl SettingsBindAdmissionError {
    pub(crate) fn is_capacity(&self) -> bool {
        matches!(
            self,
            Self::HistoryCapacity | Self::ReplayCapacity(_) | Self::ReplyCapacity(_)
        )
    }
}

#[cfg(unix)]
pub(crate) enum SettingsBindReceiptCommit {
    Duplicate,
    Fresh {
        reservation: StoredSettingsBindReply,
        grant: Box<SettingsGrantHead>,
        lifetime: BindingLifetime,
        creator: BindingCreator,
    },
}

#[cfg(unix)]
pub(crate) struct StoredSettingsBindReply {
    pub(crate) reservation_id: Uuid,
    pub(crate) local_device_id: DeviceId,
    pub(crate) peer_device_id: DeviceId,
    pub(crate) provenance: String,
    pub(crate) created_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

#[cfg(unix)]
#[derive(Serialize)]
struct HostBindProvenance<'keys> {
    side: &'static str,
    version: u16,
    local_id: DeviceId,
    peer_keys: &'keys crate::crypto::DevicePublicKeys,
    host_epoch: Uuid,
    connection_epoch: Uuid,
    grant: SettingsGrantReference,
    parent_id: Option<Uuid>,
}

#[cfg(unix)]
pub(super) fn validate_bind_on(
    store: &super::Store,
    connection: &Connection,
    admission: &SettingsBindAdmission<'_>,
) -> Result<(SettingsGrantHead, BindingLifetime, UnixMillis), SettingsBindAdmissionError> {
    let (now, _) = admission
        .source
        .check_precommit(store, admission.intent)
        .map_err(SettingsBindAdmissionError::Rejected)?;
    let owner = SettingsGrantOwner {
        host_id: admission.intent.host_id,
        principal_id: *admission.intent.host_id.as_uuid(),
    };
    let head = super::settings_grant_head_on(
        connection,
        owner,
        admission.source.verified_peer_keys().device_id,
    )?
    .ok_or(SettingsBindAdmissionError::Rejected(
        BindRejection::OutsideScope,
    ))?;
    if head.host_revoked
        || head.peer_revoked
        || head.grant.change.peer_keys != *admission.source.verified_peer_keys()
        || head.grant.revision != admission.intent.generation.grant_revision
        || head.grant.change.target.grant_id != admission.intent.generation.grant_id
        || !matches!(
            &head.grant.change.target.state,
            SettingsGrantState::Active { .. }
        )
        || now < head.grant.change.target.issued_at
        || now >= head.grant.change.target.expires_at
    {
        return Err(SettingsBindAdmissionError::Rejected(
            BindRejection::OutsideScope,
        ));
    }
    let lifetime = admission
        .source
        .parent_lifetime(admission.intent, head.grant.change.target.expires_at)
        .map_err(SettingsBindAdmissionError::Rejected)?;
    Ok((head, lifetime, now))
}

#[cfg(unix)]
pub(super) fn reserve_bind_reply_in(
    connection: &Connection,
    admission: &SettingsBindAdmission<'_>,
    lifetime: BindingLifetime,
    now: UnixMillis,
) -> Result<StoredSettingsBindReply, SettingsBindAdmissionError> {
    let peer = admission.source.verified_peer_keys().device_id;
    match check_settings_reply_capacity(connection, &peer.to_string(), 49_152) {
        Err(error @ StorageError::TransportCapacity(_)) => {
            return Err(SettingsBindAdmissionError::ReplyCapacity(error));
        }
        result => result?,
    }
    let expires = now
        .get()
        .checked_add(60_000)
        .ok_or(SettingsBindAdmissionError::Rejected(BindRejection::Expired))?
        .min(lifetime.expires_at.get());
    let expires_at = UnixMillis::try_from(expires)
        .map_err(|_| SettingsBindAdmissionError::Rejected(BindRejection::Expired))?;
    let provenance = serde_json::to_string(&HostBindProvenance {
        side: "host",
        version: 1,
        local_id: admission.intent.host_id,
        peer_keys: admission.source.verified_peer_keys(),
        host_epoch: admission.intent.generation.host_epoch,
        connection_epoch: admission.intent.generation.connection_epoch,
        grant: SettingsGrantReference {
            grant_id: admission.intent.generation.grant_id,
            revision: admission.intent.generation.grant_revision,
        },
        parent_id: None,
    })
    .map_err(StorageError::from)?;
    let reservation_id = Uuid::new_v4();
    connection.execute("INSERT INTO settings_transport_reply_reservations(reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at) VALUES(?1,?2,?3,?4,'default',49152,?5,?6)",rusqlite::params![reservation_id.to_string(),admission.intent.host_id.to_string(),peer.to_string(),provenance,now.get() as i64,expires_at.get() as i64]).map_err(StorageError::from)?;
    let actual:(String,String,String,String,String,i64,i64,i64)=connection.query_row("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1",[reservation_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).map_err(StorageError::from)?;
    if actual
        != (
            reservation_id.to_string(),
            admission.intent.host_id.to_string(),
            peer.to_string(),
            provenance.clone(),
            "default".into(),
            49_152,
            now.get() as i64,
            expires_at.get() as i64,
        )
    {
        return Err(SettingsBindAdmissionError::Bookkeeping);
    }
    Ok(StoredSettingsBindReply {
        reservation_id,
        local_device_id: admission.intent.host_id,
        peer_device_id: peer,
        provenance,
        created_at: now,
        expires_at,
    })
}

const MAX_OUTBOX_MESSAGES: i64 = 1_024;
const MAX_OUTBOX_BYTES: i64 = 128 * 1_048_576;
const MAX_PEER_OUTBOX_MESSAGES: i64 = 64;
const MAX_PEER_OUTBOX_BYTES: i64 = 16 * 1_048_576;
#[cfg(unix)]
const MAX_SETTINGS_REPLIES: i64 = 128;
#[cfg(unix)]
const MAX_SETTINGS_REPLY_BYTES: i64 = 32 * 1_048_576;
#[cfg(unix)]
const MAX_PEER_SETTINGS_REPLIES: i64 = 16;
#[cfg(unix)]
const MAX_PEER_SETTINGS_REPLY_BYTES: i64 = 8 * 1_048_576;
const SETTINGS_ORIGINAL_RETENTION_MS: i64 = 86_400_000;

struct OutboxUsage {
    queued_count: i64,
    queued_bytes: i64,
    reserved_count: i64,
    reserved_bytes: i64,
}

impl OutboxUsage {
    fn permits(self, incoming: i64, count_limit: i64, byte_limit: i64) -> StorageResult<bool> {
        if self.queued_count < 0
            || self.queued_bytes < 0
            || self.reserved_count < 0
            || self.reserved_bytes < 0
        {
            return Err(StorageError::TransportConflict(
                "outbox capacity accounting is invalid".into(),
            ));
        }
        let count = self
            .queued_count
            .checked_add(self.reserved_count)
            .and_then(|value| value.checked_add(1));
        let bytes = self
            .queued_bytes
            .checked_add(self.reserved_bytes)
            .and_then(|value| value.checked_add(incoming));
        Ok(count.is_some_and(|value| value <= count_limit)
            && bytes.is_some_and(|value| value <= byte_limit))
    }
}

pub(super) fn check_outbox_capacity(
    connection: &Connection,
    peer_id: &str,
    incoming_bytes: usize,
) -> StorageResult<()> {
    let incoming = i64::try_from(incoming_bytes)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| StorageError::InvalidInput("invalid outbox capacity size".into()))?;
    let global = connection.query_row(
        "SELECT count(*),coalesce(sum(length(o.envelope)),0),0,0 FROM outbox AS o WHERE o.delivered_at IS NULL AND NOT EXISTS(SELECT 1 FROM settings_transport_private_outbox AS p WHERE p.message_id=o.id)",
        [],
        outbox_usage,
    )?;
    let peer = connection.query_row(
        "SELECT count(*),coalesce(sum(length(o.envelope)),0),0,0 FROM outbox AS o WHERE o.delivered_at IS NULL AND o.device_id=?1 AND NOT EXISTS(SELECT 1 FROM settings_transport_private_outbox AS p WHERE p.message_id=o.id)",
        [peer_id],
        outbox_usage,
    )?;
    let global_permitted = global.permits(incoming, MAX_OUTBOX_MESSAGES, MAX_OUTBOX_BYTES)?;
    let peer_permitted = peer.permits(incoming, MAX_PEER_OUTBOX_MESSAGES, MAX_PEER_OUTBOX_BYTES)?;
    if !global_permitted || !peer_permitted {
        return Err(StorageError::TransportCapacity("encrypted sender outbox"));
    }
    Ok(())
}

#[cfg(unix)]
fn check_settings_reply_capacity(
    connection: &Connection,
    peer_id: &str,
    incoming_bytes: usize,
) -> StorageResult<()> {
    let incoming = i64::try_from(incoming_bytes)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| StorageError::InvalidInput("invalid Settings reply capacity size".into()))?;
    let global = connection.query_row(
        "SELECT q.message_count,q.message_bytes,r.message_count,r.message_bytes FROM (SELECT count(*) AS message_count,coalesce(sum(envelope_bytes),0) AS message_bytes FROM settings_transport_private_outbox) AS q CROSS JOIN (SELECT count(*) AS message_count,coalesce(sum(reserved_bytes),0) AS message_bytes FROM settings_transport_reply_reservations) AS r",
        [],
        outbox_usage,
    )?;
    let peer = connection.query_row(
        "SELECT q.message_count,q.message_bytes,r.message_count,r.message_bytes FROM (SELECT count(*) AS message_count,coalesce(sum(envelope_bytes),0) AS message_bytes FROM settings_transport_private_outbox WHERE peer_device_id=?1) AS q CROSS JOIN (SELECT count(*) AS message_count,coalesce(sum(reserved_bytes),0) AS message_bytes FROM settings_transport_reply_reservations WHERE peer_device_id=?1) AS r",
        [peer_id],
        outbox_usage,
    )?;
    let global_permitted =
        global.permits(incoming, MAX_SETTINGS_REPLIES, MAX_SETTINGS_REPLY_BYTES)?;
    let peer_permitted = peer.permits(
        incoming,
        MAX_PEER_SETTINGS_REPLIES,
        MAX_PEER_SETTINGS_REPLY_BYTES,
    )?;
    if !global_permitted || !peer_permitted {
        return Err(StorageError::TransportCapacity("private Settings replies"));
    }
    Ok(())
}

pub(super) fn sweep_settings_transport(connection: &Connection, now: i64) -> StorageResult<()> {
    connection.execute(
        "DELETE FROM settings_transport_reply_reservations WHERE expires_at<=?1",
        [now],
    )?;
    connection.execute(
        "DELETE FROM outbox WHERE id IN (SELECT message_id FROM settings_transport_private_outbox WHERE expires_at<=?1)",
        [now],
    )?;
    connection.execute(
        "DELETE FROM settings_remote_originals WHERE terminal IS NOT NULL AND coalesce(json_extract(identity,'$.expires_at'),json_extract(identity,'$.admitted_at'))<=?1-?2",
        [now, SETTINGS_ORIGINAL_RETENTION_MS],
    )?;
    Ok(())
}

pub(super) fn is_private_outbox(connection: &Connection, message_id: &str) -> StorageResult<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings_transport_private_outbox WHERE message_id=?1)",
        [message_id],
        |row| row.get(0),
    )?)
}

pub(super) fn require_ordinary_outbox(
    connection: &Connection,
    message_id: &str,
) -> StorageResult<()> {
    if is_private_outbox(connection, message_id)? {
        return Err(StorageError::TransportConflict(
            "private settings outbox requires its owned transport".into(),
        ));
    }
    Ok(())
}

fn outbox_usage(row: &Row<'_>) -> rusqlite::Result<OutboxUsage> {
    Ok(OutboxUsage {
        queued_count: row.get(0)?,
        queued_bytes: row.get(1)?,
        reserved_count: row.get(2)?,
        reserved_bytes: row.get(3)?,
    })
}

#[cfg(unix)]
pub(crate) struct ConsumedSettingsReply<'sealed, 'transaction, 'connection, 'source> {
    sealed: &'sealed crate::devices::SealedSettingsReply<'transaction, 'connection, 'source>,
}

#[cfg(unix)]
pub(crate) fn consume_settings_reply_in<'sealed, 'transaction, 'connection, 'source>(
    transaction: &'transaction rusqlite::Transaction<'connection>,
    sealed: &'sealed crate::devices::SealedSettingsReply<'transaction, 'connection, 'source>,
) -> Result<
    ConsumedSettingsReply<'sealed, 'transaction, 'connection, 'source>,
    crate::devices::SettingsReplyPublicationError,
> {
    sealed.check_before_transfer(transaction)?;
    check_reply_reservation_on(transaction, sealed)?;
    let reply = sealed.reservation();
    let reserved_bytes = i64::try_from(reply.family.reserved_bytes())
        .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?;
    let created_at = i64::try_from(reply.created_at.get())
        .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?;
    let expires_at = i64::try_from(reply.expires_at.get())
        .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?;
    let deleted = transaction.execute(
        "DELETE FROM settings_transport_reply_reservations WHERE reservation_id=?1 AND local_device_id=?2 AND peer_device_id=?3 AND provenance=?4 AND family=?5 AND reserved_bytes=?6 AND created_at=?7 AND expires_at=?8",
        rusqlite::params![reply.reservation_id.to_string(), reply.local_device_id.to_string(), reply.peer_device_id.to_string(), reply.provenance, reply.family.name(), reserved_bytes, created_at, expires_at],
    )?;
    if deleted != 1 {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    }
    let item = sealed.item();
    let envelope_bytes = i64::try_from(item.message.envelope.len())
        .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?;
    transaction.execute(
        "INSERT INTO settings_transport_private_outbox(message_id,reservation_id,local_device_id,peer_device_id,provenance,family,envelope_bytes,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![item.message.id,reply.reservation_id.to_string(),reply.local_device_id.to_string(),reply.peer_device_id.to_string(),reply.provenance,reply.family.name(),envelope_bytes,item.message.created_at,item.expires_at],
    )?;
    super::transport::enqueue_transport(transaction, item)?;
    check_consumed_reply_on(transaction, sealed)?;
    sealed.check_local_on(transaction)?;
    Ok(ConsumedSettingsReply { sealed })
}

#[cfg(unix)]
impl<'sealed, 'transaction, 'connection, 'source>
    ConsumedSettingsReply<'sealed, 'transaction, 'connection, 'source>
{
    pub(crate) fn check_on(
        &self,
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<(), crate::devices::SettingsReplyPublicationError> {
        self.sealed.check_local_on(transaction)?;
        check_consumed_reply_on(transaction, self.sealed)
    }

    pub(crate) fn sealed(
        &self,
    ) -> &'sealed crate::devices::SealedSettingsReply<'transaction, 'connection, 'source> {
        self.sealed
    }
}

#[cfg(unix)]
impl std::fmt::Debug for ConsumedSettingsReply<'_, '_, '_, '_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConsumedSettingsReply([transaction-local transition])")
    }
}

#[cfg(unix)]
fn check_reply_reservation_on(
    connection: &Connection,
    sealed: &crate::devices::SealedSettingsReply<'_, '_, '_>,
) -> Result<(), crate::devices::SettingsReplyPublicationError> {
    let reply = sealed.reservation();
    if reply.provenance.len() > 2048 {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    }
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1")?;
    let mut rows = statement.query([reply.reservation_id.to_string()])?;
    let row = rows
        .next()?
        .ok_or(crate::devices::SettingsReplyPublicationError::Conflict)?;
    if reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || reply_text(row, 3, 2048)? != reply.provenance
        || reply_text(row, 4, 10)? != reply.family.name()
        || row.get::<_, i64>(5)?
            != i64::try_from(reply.family.reserved_bytes())
                .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?
        || row.get::<_, i64>(6)?
            != i64::try_from(reply.created_at.get())
                .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?
        || row.get::<_, i64>(7)?
            != i64::try_from(reply.expires_at.get())
                .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?
    {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    }
    Ok(())
}

#[cfg(unix)]
fn check_consumed_reply_on(
    connection: &Connection,
    sealed: &crate::devices::SealedSettingsReply<'_, '_, '_>,
) -> Result<(), crate::devices::SettingsReplyPublicationError> {
    let reply = sealed.reservation();
    let item = sealed.item();
    let reservation_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings_transport_reply_reservations WHERE reservation_id=?1)",
        [reply.reservation_id.to_string()], |row|row.get(0),
    )?;
    if reservation_exists {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    }
    let mut statement = connection.prepare("SELECT o.id,o.device_id,o.envelope,o.created_at,o.not_before,o.attempts,o.delivered_at,t.message_id,t.expires_at,p.message_id,p.reservation_id,p.local_device_id,p.peer_device_id,p.provenance,p.family,p.envelope_bytes,p.created_at,p.expires_at FROM outbox AS o JOIN transport_outbox AS t ON t.message_id=o.id JOIN settings_transport_private_outbox AS p ON p.message_id=o.id WHERE o.id=?1")?;
    let mut rows = statement.query([&item.message.id])?;
    let row = rows
        .next()?
        .ok_or(crate::devices::SettingsReplyPublicationError::Conflict)?;
    let rusqlite::types::ValueRef::Blob(actual_bytes) = row.get_ref(2)? else {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    };
    if actual_bytes.len() > reply.family.reserved_bytes()
        || actual_bytes != item.message.envelope.as_slice()
        || reply_text(row, 0, 36)? != item.message.id
        || reply_text(row, 1, 36)? != item.message.device_id
        || row.get::<_, i64>(3)? != item.message.created_at
        || row.get::<_, i64>(4)? != item.message.not_before
        || row.get::<_, i64>(5)? != 0
        || row.get::<_, Option<i64>>(6)?.is_some()
        || reply_text(row, 7, 36)? != item.message.id
        || row.get::<_, i64>(8)? != item.expires_at
        || reply_text(row, 9, 36)? != item.message.id
        || reply_text(row, 10, 36)? != reply.reservation_id.to_string()
        || reply_text(row, 11, 36)? != reply.local_device_id.to_string()
        || reply_text(row, 12, 36)? != reply.peer_device_id.to_string()
        || reply_text(row, 13, 2048)? != reply.provenance
        || reply_text(row, 14, 10)? != reply.family.name()
        || row.get::<_, i64>(15)?
            != i64::try_from(item.message.envelope.len())
                .map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)?
        || row.get::<_, i64>(16)? != item.message.created_at
        || row.get::<_, i64>(17)? != item.expires_at
    {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    }
    Ok(())
}

#[cfg(unix)]
fn reply_text<'row>(
    row: &'row Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, crate::devices::SettingsReplyPublicationError> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column)? else {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    };
    if bytes.len() > maximum {
        return Err(crate::devices::SettingsReplyPublicationError::Conflict);
    }
    std::str::from_utf8(bytes).map_err(|_| crate::devices::SettingsReplyPublicationError::Conflict)
}

#[cfg(unix)]
#[derive(Error)]
pub(crate) enum SettingsAccessAdmissionError {
    #[error("Settings access receipt authority check failed")]
    Authority(#[from] crate::host::settings::service::SettingsAccessQualificationError),
    #[error("Settings access replay receipt capacity is exhausted")]
    ReplayCapacity(#[source] StorageError),
    #[error("Settings access reply capacity is exhausted")]
    ReplyCapacity(#[source] StorageError),
    #[error("Settings access receipt storage failed")]
    Storage(#[from] StorageError),
    #[error("Settings access receipt bookkeeping failed")]
    Bookkeeping,
    #[error("Settings access receipt commit is uncertain")]
    Commit(#[source] StorageError),
    #[error("Settings access receipt rollback failed")]
    Rollback {
        #[source]
        cause: Box<SettingsAccessAdmissionError>,
        rollback: StorageError,
    },
}

#[cfg(unix)]
impl std::fmt::Debug for SettingsAccessAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SettingsAccessAdmissionError([retained cause])")
    }
}

#[cfg(unix)]
impl SettingsAccessAdmissionError {
    pub(crate) fn is_capacity(&self) -> bool {
        use crate::host::settings::service::SettingsAccessQualificationError;

        match self {
            Self::ReplayCapacity(_) | Self::ReplyCapacity(_) => true,
            Self::Authority(SettingsAccessQualificationError::Authority(error)) => {
                matches!(error.as_ref(), SettingsServiceError::Capacity)
            }
            _ => false,
        }
    }

    pub(crate) fn is_policy_rejection(&self) -> bool {
        use crate::devices::AccessStatusRejection;
        use crate::host::settings::service::SettingsAccessQualificationError;

        match self {
            Self::Authority(SettingsAccessQualificationError::Authority(error)) => matches!(
                error.as_ref(),
                SettingsServiceError::Expired | SettingsServiceError::OutsideScope
            ),
            Self::Authority(SettingsAccessQualificationError::Source(
                AccessStatusRejection::Expired | AccessStatusRejection::OutsideScope,
            )) => true,
            _ => false,
        }
    }
}

#[cfg(unix)]
pub(crate) enum SettingsAccessReceiptCommit {
    Duplicate,
    Fresh { reply: StoredSettingsAccessReply },
}

#[cfg(unix)]
pub(crate) struct StoredSettingsAccessReply {
    pub(crate) reservation_id: Uuid,
    pub(crate) local_device_id: DeviceId,
    pub(crate) peer_device_id: DeviceId,
    pub(crate) provenance: String,
    pub(crate) family: crate::host::settings::service::SettingsAccessReplyFamily,
    pub(crate) created_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

#[cfg(unix)]
pub(super) fn reserve_access_reply_in(
    connection: &Connection,
    spec: &crate::host::settings::service::SettingsAccessReservationSpec,
) -> Result<StoredSettingsAccessReply, SettingsAccessAdmissionError> {
    let reserved_bytes = i64::try_from(spec.family().reserved_bytes())
        .map_err(|_| SettingsAccessAdmissionError::Bookkeeping)?;
    let created_at = i64::try_from(spec.created_at().get())
        .map_err(|_| SettingsAccessAdmissionError::Bookkeeping)?;
    let expires_at = i64::try_from(spec.expires_at().get())
        .map_err(|_| SettingsAccessAdmissionError::Bookkeeping)?;
    if spec.provenance().len() > 2048 || created_at >= expires_at {
        return Err(SettingsAccessAdmissionError::Bookkeeping);
    }
    match check_settings_reply_capacity(
        connection,
        &spec.peer_device_id().to_string(),
        spec.family().reserved_bytes(),
    ) {
        Err(error @ StorageError::TransportCapacity(_)) => {
            return Err(SettingsAccessAdmissionError::ReplyCapacity(error));
        }
        result => result?,
    }
    let reservation_id = Uuid::new_v4();
    connection.execute(
        "INSERT INTO settings_transport_reply_reservations(reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![reservation_id.to_string(),spec.local_device_id().to_string(),spec.peer_device_id().to_string(),spec.provenance(),spec.family().name(),reserved_bytes,created_at,expires_at],
    ).map_err(StorageError::from)?;
    let reply = StoredSettingsAccessReply {
        reservation_id,
        local_device_id: spec.local_device_id(),
        peer_device_id: spec.peer_device_id(),
        provenance: spec.provenance().to_owned(),
        family: spec.family(),
        created_at: spec.created_at(),
        expires_at: spec.expires_at(),
    };
    check_access_reply_on(connection, &reply)?;
    Ok(reply)
}

#[cfg(unix)]
pub(super) fn check_access_reply_on(
    connection: &Connection,
    reply: &StoredSettingsAccessReply,
) -> Result<(), SettingsAccessAdmissionError> {
    let reserved_bytes = i64::try_from(reply.family.reserved_bytes())
        .map_err(|_| SettingsAccessAdmissionError::Bookkeeping)?;
    let created_at = i64::try_from(reply.created_at.get())
        .map_err(|_| SettingsAccessAdmissionError::Bookkeeping)?;
    let expires_at = i64::try_from(reply.expires_at.get())
        .map_err(|_| SettingsAccessAdmissionError::Bookkeeping)?;
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1").map_err(StorageError::from)?;
    let mut rows = statement
        .query([reply.reservation_id.to_string()])
        .map_err(StorageError::from)?;
    let row = rows
        .next()
        .map_err(StorageError::from)?
        .ok_or(SettingsAccessAdmissionError::Bookkeeping)?;
    if access_reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || access_reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || access_reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || access_reply_text(row, 3, 2048)? != reply.provenance
        || access_reply_text(row, 4, 7)? != reply.family.name()
        || row.get::<_, i64>(5).map_err(StorageError::from)? != reserved_bytes
        || row.get::<_, i64>(6).map_err(StorageError::from)? != created_at
        || row.get::<_, i64>(7).map_err(StorageError::from)? != expires_at
    {
        return Err(SettingsAccessAdmissionError::Bookkeeping);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn access_reply_text<'row>(
    row: &'row Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, SettingsAccessAdmissionError> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column).map_err(StorageError::from)?
    else {
        return Err(SettingsAccessAdmissionError::Bookkeeping);
    };
    if bytes.len() > maximum {
        return Err(SettingsAccessAdmissionError::Bookkeeping);
    }
    std::str::from_utf8(bytes).map_err(|_| SettingsAccessAdmissionError::Bookkeeping)
}

#[cfg(unix)]
#[derive(Error)]
pub(crate) enum SettingsReadAdmissionError {
    #[error("Settings read receipt authority check failed")]
    Authority(#[from] crate::host::settings::service::SettingsReadQualificationError),
    #[error("Settings read replay receipt capacity is exhausted")]
    ReplayCapacity(#[source] StorageError),
    #[error("Settings read reply capacity is exhausted")]
    ReplyCapacity(#[source] StorageError),
    #[error("Settings read receipt storage failed")]
    Storage(#[from] StorageError),
    #[error("Settings read receipt bookkeeping failed")]
    Bookkeeping,
    #[error("Settings read receipt commit is uncertain")]
    Commit(#[source] StorageError),
    #[error("Settings read receipt rollback failed")]
    Rollback {
        #[source]
        cause: Box<SettingsReadAdmissionError>,
        rollback: StorageError,
    },
}

#[cfg(unix)]
impl std::fmt::Debug for SettingsReadAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SettingsReadAdmissionError([retained cause])")
    }
}

#[cfg(unix)]
impl SettingsReadAdmissionError {
    pub(crate) fn is_capacity(&self) -> bool {
        use crate::host::settings::service::SettingsReadQualificationError;

        match self {
            Self::ReplayCapacity(_) | Self::ReplyCapacity(_) => true,
            Self::Authority(SettingsReadQualificationError::Authority(error)) => {
                matches!(error.as_ref(), SettingsServiceError::Capacity)
            }
            _ => false,
        }
    }

    pub(crate) fn is_policy_rejection(&self) -> bool {
        use crate::devices::SettingsReadSourceError;
        use crate::host::settings::service::SettingsReadQualificationError;

        match self {
            Self::Authority(SettingsReadQualificationError::Authority(error)) => matches!(
                error.as_ref(),
                SettingsServiceError::Expired | SettingsServiceError::OutsideScope
            ),
            Self::Authority(SettingsReadQualificationError::Source(
                SettingsReadSourceError::Source(
                    BindRejection::Expired | BindRejection::OutsideScope,
                ),
            )) => true,
            _ => false,
        }
    }
}

#[cfg(unix)]
pub(crate) enum SettingsReadReceiptCommit {
    Duplicate,
    Known,
    Fresh { reply: StoredSettingsReadReply },
}

#[cfg(unix)]
pub(crate) struct StoredSettingsReadReply {
    pub(crate) reservation_id: Uuid,
    pub(crate) local_device_id: DeviceId,
    pub(crate) peer_device_id: DeviceId,
    pub(crate) provenance: String,
    pub(crate) family: crate::host::settings::service::SettingsReadReplyFamily,
    pub(crate) created_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

#[cfg(unix)]
pub(super) fn reserve_read_reply_in(
    connection: &Connection,
    spec: &crate::host::settings::service::SettingsReadReservationSpec<'_>,
) -> Result<StoredSettingsReadReply, SettingsReadAdmissionError> {
    let reserved_bytes = i64::try_from(spec.family().reserved_bytes())
        .map_err(|_| SettingsReadAdmissionError::Bookkeeping)?;
    let created_at = i64::try_from(spec.created_at().get())
        .map_err(|_| SettingsReadAdmissionError::Bookkeeping)?;
    let expires_at = i64::try_from(spec.expires_at().get())
        .map_err(|_| SettingsReadAdmissionError::Bookkeeping)?;
    let parent = spec.parent();
    let provenance = serde_json::to_string(&HostBindProvenance {
        side: "host",
        version: 1,
        local_id: spec.local_device_id(),
        peer_keys: spec.verified_peer_keys(),
        host_epoch: parent.generation.host_epoch,
        connection_epoch: parent.generation.connection_epoch,
        grant: SettingsGrantReference {
            grant_id: parent.generation.grant_id,
            revision: parent.generation.grant_revision,
        },
        parent_id: Some(parent.parent_id),
    })
    .map_err(StorageError::from)?;
    if provenance.len() > 2048 || created_at >= expires_at {
        return Err(SettingsReadAdmissionError::Bookkeeping);
    }
    match check_settings_reply_capacity(
        connection,
        &spec.peer_device_id().to_string(),
        spec.family().reserved_bytes(),
    ) {
        Err(error @ StorageError::TransportCapacity(_)) => {
            return Err(SettingsReadAdmissionError::ReplyCapacity(error));
        }
        result => result?,
    }
    let reservation_id = Uuid::new_v4();
    connection.execute(
        "INSERT INTO settings_transport_reply_reservations(reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![reservation_id.to_string(),spec.local_device_id().to_string(),spec.peer_device_id().to_string(),provenance,spec.family().name(),reserved_bytes,created_at,expires_at],
    ).map_err(StorageError::from)?;
    let reply = StoredSettingsReadReply {
        reservation_id,
        local_device_id: spec.local_device_id(),
        peer_device_id: spec.peer_device_id(),
        provenance,
        family: spec.family(),
        created_at: spec.created_at(),
        expires_at: spec.expires_at(),
    };
    check_read_reply_on(connection, &reply)?;
    Ok(reply)
}

#[cfg(unix)]
pub(super) fn check_read_reply_on(
    connection: &Connection,
    reply: &StoredSettingsReadReply,
) -> Result<(), SettingsReadAdmissionError> {
    let reserved_bytes = i64::try_from(reply.family.reserved_bytes())
        .map_err(|_| SettingsReadAdmissionError::Bookkeeping)?;
    let created_at = i64::try_from(reply.created_at.get())
        .map_err(|_| SettingsReadAdmissionError::Bookkeeping)?;
    let expires_at = i64::try_from(reply.expires_at.get())
        .map_err(|_| SettingsReadAdmissionError::Bookkeeping)?;
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1").map_err(StorageError::from)?;
    let mut rows = statement
        .query([reply.reservation_id.to_string()])
        .map_err(StorageError::from)?;
    let row = rows
        .next()
        .map_err(StorageError::from)?
        .ok_or(SettingsReadAdmissionError::Bookkeeping)?;
    if read_reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || read_reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || read_reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || read_reply_text(row, 3, 2048)? != reply.provenance
        || read_reply_text(row, 4, 10)? != reply.family.name()
        || row.get::<_, i64>(5).map_err(StorageError::from)? != reserved_bytes
        || row.get::<_, i64>(6).map_err(StorageError::from)? != created_at
        || row.get::<_, i64>(7).map_err(StorageError::from)? != expires_at
    {
        return Err(SettingsReadAdmissionError::Bookkeeping);
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn read_reply_text<'row>(
    row: &'row Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, SettingsReadAdmissionError> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column).map_err(StorageError::from)?
    else {
        return Err(SettingsReadAdmissionError::Bookkeeping);
    };
    if bytes.len() > maximum {
        return Err(SettingsReadAdmissionError::Bookkeeping);
    }
    std::str::from_utf8(bytes).map_err(|_| SettingsReadAdmissionError::Bookkeeping)
}
