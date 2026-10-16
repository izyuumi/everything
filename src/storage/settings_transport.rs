use rusqlite::{Connection, Row};

use super::{StorageError, StorageResult};

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
