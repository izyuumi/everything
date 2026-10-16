use rusqlite::{Connection, Row};
#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use thiserror::Error;
#[cfg(unix)]
use uuid::Uuid;

#[cfg(unix)]
use crate::devices::{
    AuthenticatedSettingsSource, BindRejection, BindingCreator, BindingIntent, BindingLifetime,
};
#[cfg(unix)]
use crate::host::settings::grants::SettingsGrantOwner;
#[cfg(unix)]
use crate::host::settings::{SettingsGrantHead, SettingsGrantReference, SettingsGrantState};
#[cfg(unix)]
use crate::protocol::{DeviceId, UnixMillis};

use super::{StorageError, StorageResult};

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
        grant: SettingsGrantHead,
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
    match check_outbox_capacity(connection, &peer.to_string(), 49_152) {
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
        "SELECT q.message_count,q.message_bytes,r.message_count,r.message_bytes FROM (SELECT count(*) AS message_count,coalesce(sum(length(envelope)),0) AS message_bytes FROM outbox WHERE delivered_at IS NULL) AS q CROSS JOIN (SELECT count(*) AS message_count,coalesce(sum(reserved_bytes),0) AS message_bytes FROM settings_transport_reply_reservations) AS r",
        [],
        outbox_usage,
    )?;
    let peer = connection.query_row(
        "SELECT q.message_count,q.message_bytes,r.message_count,r.message_bytes FROM (SELECT count(*) AS message_count,coalesce(sum(length(envelope)),0) AS message_bytes FROM outbox WHERE delivered_at IS NULL AND device_id=?1) AS q CROSS JOIN (SELECT count(*) AS message_count,coalesce(sum(reserved_bytes),0) AS message_bytes FROM settings_transport_reply_reservations WHERE peer_device_id=?1) AS r",
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

pub(super) fn require_ordinary_outbox(
    connection: &Connection,
    message_id: &str,
) -> StorageResult<()> {
    let private: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings_transport_private_outbox WHERE message_id=?1)",
        [message_id],
        |row| row.get(0),
    )?;
    if private {
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
