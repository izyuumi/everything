use crate::error::RelayError;
use everything::{
    crypto::{DevicePublicKeys, EncryptedEnvelope},
    protocol::{DeviceId, MessageId},
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, path::PathBuf, time::Duration};
use tokio_rusqlite::{
    Connection,
    rusqlite::{self, OptionalExtension, TransactionBehavior, params},
};

const APPLICATION_ID: i64 = 0x4556_524c;
const SCHEMA_VERSION: i64 = 1;
const MAX_DEVICES: i64 = 4_096;
const MAX_DEVICE_QUEUE_MESSAGES: i64 = 64;
const MAX_DEVICE_QUEUE_BYTES: i64 = 16 * 1_024 * 1_024;
const MAX_TOTAL_QUEUE_MESSAGES: i64 = 1_024;
const MAX_TOTAL_QUEUE_BYTES: i64 = 128 * 1_024 * 1_024;
const MAX_SEEN_MESSAGES: i64 = 100_000;
const MAX_FRAMES_PER_MINUTE: i64 = 240;
const MAX_BYTES_PER_MINUTE: i64 = 32 * 1_024 * 1_024;
pub const DELIVERY_WINDOW: usize = 16;

#[derive(Clone)]
pub struct Database {
    connection: Connection,
}

#[derive(Debug, Serialize)]
pub struct EnrolledDevice {
    pub keys: DevicePublicKeys,
    pub enrolled_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
    pub queued_messages: i64,
    pub queued_bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct QueueStatus {
    pub queued_messages: i64,
    pub queued_bytes: i64,
    pub retained_message_ids: i64,
    pub devices: i64,
}

impl Database {
    pub async fn open(path: PathBuf) -> Result<Self, RelayError> {
        let path = tokio::task::spawn_blocking(move || prepare_database_file(path))
            .await
            .map_err(|error| RelayError::DatabaseWorker(error.to_string()))??;
        let database = Self {
            connection: Connection::open(path).await?,
        };
        database.call(initialize).await?;
        Ok(database)
    }

    pub async fn close(self) -> Result<(), RelayError> {
        self.connection
            .close()
            .await
            .map_err(|error| RelayError::DatabaseWorker(error.to_string()))
    }

    pub async fn enroll(&self, keys: DevicePublicKeys, now_ms: u64) -> Result<bool, RelayError> {
        keys.validate()?;
        let now = timestamp(now_ms)?;
        self.call(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id = keys.device_id.to_string();
            let existing: Option<(String, Option<i64>)> = transaction
                .query_row(
                    "SELECT public_keys, revoked_at_ms FROM relay_devices WHERE device_id = ?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((existing, revoked)) = existing {
                let pinned: DevicePublicKeys = serde_json::from_str(&existing)?;
                if pinned != keys || revoked.is_some() {
                    return Err(RelayError::DeviceConflict);
                }
                return Ok(false);
            }
            let count: i64 = transaction.query_row("SELECT COUNT(*) FROM relay_devices", [], |row| row.get(0))?;
            if count >= MAX_DEVICES {
                return Err(RelayError::QueueFull);
            }
            transaction.execute(
                "INSERT INTO relay_devices (device_id, public_keys, enrolled_at_ms) VALUES (?1, ?2, ?3)",
                params![id, serde_json::to_string(&keys)?, now],
            )?;
            transaction.commit()?;
            Ok(true)
        }).await
    }

    pub async fn revoke(&self, device_id: DeviceId, now_ms: u64) -> Result<bool, RelayError> {
        let now = timestamp(now_ms)?;
        self.call(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id = device_id.to_string();
            let changed = transaction.execute(
                "UPDATE relay_devices SET revoked_at_ms = ?2 WHERE device_id = ?1 AND revoked_at_ms IS NULL",
                params![id, now],
            )?;
            transaction.execute(
                "DELETE FROM relay_messages WHERE sender = ?1 OR recipient = ?1",
                [&id],
            )?;
            transaction.commit()?;
            Ok(changed != 0)
        }).await
    }

    pub async fn devices(&self) -> Result<Vec<EnrolledDevice>, RelayError> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT public_keys, enrolled_at_ms, revoked_at_ms, queued_messages, queued_bytes
                 FROM relay_devices ORDER BY enrolled_at_ms, device_id LIMIT 4096",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?;
            rows.map(|row| {
                let (keys, enrolled_at_ms, revoked_at_ms, queued_messages, queued_bytes) = row?;
                Ok(EnrolledDevice {
                    keys: serde_json::from_str(&keys)?,
                    enrolled_at_ms,
                    revoked_at_ms,
                    queued_messages,
                    queued_bytes,
                })
            })
            .collect()
        })
        .await
    }

    pub async fn active_keys(&self, device_id: DeviceId) -> Result<DevicePublicKeys, RelayError> {
        self.call(move |connection| active_keys(connection, device_id))
            .await
    }

    pub async fn charge(
        &self,
        device_id: DeviceId,
        bytes: usize,
        now_ms: u64,
    ) -> Result<(), RelayError> {
        let window = timestamp(now_ms / 60_000)?;
        let byte_count = i64::try_from(bytes).map_err(|_| RelayError::RateLimited)?;
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id = device_id.to_string();
            let existing: Option<(i64, i64, i64)> = transaction
                .query_row(
                    "SELECT rate_window, rate_frames, rate_bytes FROM relay_devices
                 WHERE device_id = ?1 AND revoked_at_ms IS NULL",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((previous_window, mut frames, mut bytes)) = existing else {
                return Err(RelayError::AuthenticationFailed);
            };
            if previous_window != window {
                frames = 0;
                bytes = 0;
            }
            if frames >= MAX_FRAMES_PER_MINUTE
                || bytes.saturating_add(byte_count) > MAX_BYTES_PER_MINUTE
            {
                return Err(RelayError::RateLimited);
            }
            transaction.execute(
                "UPDATE relay_devices SET rate_window = ?2, rate_frames = ?3, rate_bytes = ?4
                 WHERE device_id = ?1",
                params![id, window, frames + 1, bytes + byte_count],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn enqueue(
        &self,
        authenticated_sender: DeviceId,
        envelope: EncryptedEnvelope,
        now_ms: u64,
    ) -> Result<bool, RelayError> {
        if authenticated_sender != envelope.header.sender {
            return Err(RelayError::AuthenticationFailed);
        }
        let sender_keys = self.active_keys(authenticated_sender).await?;
        envelope.verify_authenticated_sender(&sender_keys, now_ms)?;
        let encoded = serde_json::to_vec(&envelope)?;
        let digest = Sha256::digest(&encoded).to_vec();
        let size = i64::try_from(encoded.len()).map_err(|_| RelayError::QueueFull)?;
        let now = timestamp(now_ms)?;
        let expiry = timestamp(envelope.header.expires_at_ms)?;
        self.call(move |connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            prune(&transaction, now)?;
            active_keys(&transaction, authenticated_sender)?;
            let recipient = envelope.header.recipient;
            active_keys(&transaction, recipient).map_err(|error| match error {
                RelayError::AuthenticationFailed => RelayError::RecipientUnavailable,
                other => other,
            })?;
            let message_id = envelope.header.message_id.to_string();
            let previous: Option<Vec<u8>> = transaction.query_row(
                "SELECT digest FROM relay_seen WHERE message_id = ?1",
                [&message_id],
                |row| row.get(0),
            ).optional()?;
            if let Some(previous) = previous {
                if previous != digest {
                    return Err(RelayError::MessageConflict);
                }
                transaction.commit()?;
                return Ok(true);
            }
            let (total_count, total_bytes, seen_count): (i64, i64, i64) = transaction.query_row(
                "SELECT queued_messages, queued_bytes, retained_message_ids FROM relay_stats WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let (device_count, device_bytes): (i64, i64) = transaction.query_row(
                "SELECT queued_messages, queued_bytes FROM relay_devices WHERE device_id = ?1",
                [recipient.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if total_count >= MAX_TOTAL_QUEUE_MESSAGES
                || total_bytes.saturating_add(size) > MAX_TOTAL_QUEUE_BYTES
                || device_count >= MAX_DEVICE_QUEUE_MESSAGES
                || device_bytes.saturating_add(size) > MAX_DEVICE_QUEUE_BYTES
                || seen_count >= MAX_SEEN_MESSAGES
            {
                return Err(RelayError::QueueFull);
            }
            transaction.execute(
                "INSERT INTO relay_seen (message_id, digest, expires_at_ms) VALUES (?1, ?2, ?3)",
                params![message_id, digest, expiry],
            )?;
            transaction.execute(
                "INSERT INTO relay_messages (message_id, sender, recipient, envelope, byte_count, accepted_at_ms, expires_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![message_id, authenticated_sender.to_string(), recipient.to_string(), encoded, size, now, expiry],
            )?;
            transaction.commit()?;
            Ok(false)
        }).await
    }

    pub async fn pending(
        &self,
        recipient: DeviceId,
        now_ms: u64,
    ) -> Result<Vec<EncryptedEnvelope>, RelayError> {
        let now = timestamp(now_ms)?;
        self.call(move |connection| {
            active_keys(connection, recipient)?;
            let mut statement = connection.prepare(
                "SELECT message.envelope FROM relay_messages AS message
                 JOIN relay_devices AS sender ON sender.device_id = message.sender
                 WHERE message.recipient = ?1 AND message.expires_at_ms > ?2 AND sender.revoked_at_ms IS NULL
                 ORDER BY message.accepted_at_ms, message.message_id LIMIT ?3",
            )?;
            let rows = statement.query_map(params![recipient.to_string(), now, DELIVERY_WINDOW as i64], |row| row.get::<_, Vec<u8>>(0))?;
            rows.map(|row| Ok(serde_json::from_slice(&row?)?)).collect()
        }).await
    }

    pub async fn delivery_available(
        &self,
        recipient: DeviceId,
        message_id: MessageId,
        now_ms: u64,
    ) -> Result<bool, RelayError> {
        let now = timestamp(now_ms)?;
        self.call(move |connection| {
            active_keys(connection, recipient)?;
            let available = connection.query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM relay_messages AS message
                     JOIN relay_devices AS sender ON sender.device_id = message.sender
                     WHERE message.message_id = ?1 AND message.recipient = ?2
                       AND message.expires_at_ms > ?3 AND sender.revoked_at_ms IS NULL
                 )",
                params![message_id.to_string(), recipient.to_string(), now],
                |row| row.get(0),
            )?;
            Ok(available)
        })
        .await
    }

    pub async fn acknowledge(
        &self,
        recipient: DeviceId,
        message_id: MessageId,
    ) -> Result<(), RelayError> {
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            active_keys(&transaction, recipient)?;
            transaction.execute(
                "DELETE FROM relay_messages WHERE recipient = ?1 AND message_id = ?2",
                params![recipient.to_string(), message_id.to_string()],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn prune(&self, now_ms: u64) -> Result<(), RelayError> {
        let now = timestamp(now_ms)?;
        self.call(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            prune(&transaction, now)?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn status(&self) -> Result<QueueStatus, RelayError> {
        self.call(|connection| {
            let (queued_messages, queued_bytes, retained_message_ids) = connection.query_row(
                "SELECT queued_messages, queued_bytes, retained_message_ids FROM relay_stats WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let devices = connection.query_row("SELECT COUNT(*) FROM relay_devices", [], |row| row.get(0))?;
            Ok(QueueStatus { queued_messages, queued_bytes, retained_message_ids, devices })
        }).await
    }

    async fn call<T, F>(&self, operation: F) -> Result<T, RelayError>
    where
        T: Send + 'static,
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RelayError> + Send + 'static,
    {
        match self.connection.call(operation).await {
            Ok(result) => Ok(result),
            Err(tokio_rusqlite::Error::Error(error)) => Err(error),
            Err(error) => Err(RelayError::DatabaseWorker(error.to_string())),
        }
    }
}

fn initialize(connection: &mut rusqlite::Connection) -> Result<(), RelayError> {
    connection.busy_timeout(Duration::from_secs(5))?;
    let application_id: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let schema_version: i64 =
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if (application_id != APPLICATION_ID && (application_id != 0 || schema_version != 0))
        || schema_version > SCHEMA_VERSION
    {
        return Err(RelayError::Configuration(
            "not a supported dedicated relay database".to_owned(),
        ));
    }
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = 16777216;
         PRAGMA max_page_count = 131072;",
    )?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS relay_devices (
             device_id TEXT PRIMARY KEY,
             public_keys TEXT NOT NULL,
             enrolled_at_ms INTEGER NOT NULL,
             revoked_at_ms INTEGER,
             queued_messages INTEGER NOT NULL DEFAULT 0 CHECK (queued_messages >= 0),
             queued_bytes INTEGER NOT NULL DEFAULT 0 CHECK (queued_bytes >= 0),
             rate_window INTEGER NOT NULL DEFAULT -1,
             rate_frames INTEGER NOT NULL DEFAULT 0,
             rate_bytes INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS relay_stats (
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
             queued_messages INTEGER NOT NULL DEFAULT 0 CHECK (queued_messages >= 0),
             queued_bytes INTEGER NOT NULL DEFAULT 0 CHECK (queued_bytes >= 0),
             retained_message_ids INTEGER NOT NULL DEFAULT 0 CHECK (retained_message_ids >= 0)
         );
         INSERT OR IGNORE INTO relay_stats (singleton) VALUES (1);
         CREATE TABLE IF NOT EXISTS relay_seen (
             message_id TEXT PRIMARY KEY,
             digest BLOB NOT NULL CHECK (length(digest) = 32),
             expires_at_ms INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS relay_seen_expiry ON relay_seen (expires_at_ms);
         CREATE TABLE IF NOT EXISTS relay_messages (
             message_id TEXT PRIMARY KEY REFERENCES relay_seen(message_id),
             sender TEXT NOT NULL REFERENCES relay_devices(device_id),
             recipient TEXT NOT NULL REFERENCES relay_devices(device_id),
             envelope BLOB NOT NULL,
             byte_count INTEGER NOT NULL CHECK (byte_count > 0),
             accepted_at_ms INTEGER NOT NULL,
             expires_at_ms INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS relay_message_delivery ON relay_messages (recipient, accepted_at_ms, message_id);
         CREATE INDEX IF NOT EXISTS relay_message_expiry ON relay_messages (expires_at_ms);
         CREATE INDEX IF NOT EXISTS relay_message_sender ON relay_messages (sender);
         CREATE TRIGGER IF NOT EXISTS relay_message_insert AFTER INSERT ON relay_messages BEGIN
             UPDATE relay_stats SET queued_messages = queued_messages + 1, queued_bytes = queued_bytes + NEW.byte_count WHERE singleton = 1;
             UPDATE relay_devices SET queued_messages = queued_messages + 1, queued_bytes = queued_bytes + NEW.byte_count WHERE device_id = NEW.recipient;
         END;
         CREATE TRIGGER IF NOT EXISTS relay_message_delete AFTER DELETE ON relay_messages BEGIN
             UPDATE relay_stats SET queued_messages = queued_messages - 1, queued_bytes = queued_bytes - OLD.byte_count WHERE singleton = 1;
             UPDATE relay_devices SET queued_messages = queued_messages - 1, queued_bytes = queued_bytes - OLD.byte_count WHERE device_id = OLD.recipient;
         END;
         CREATE TRIGGER IF NOT EXISTS relay_seen_insert AFTER INSERT ON relay_seen BEGIN
             UPDATE relay_stats SET retained_message_ids = retained_message_ids + 1 WHERE singleton = 1;
         END;
         CREATE TRIGGER IF NOT EXISTS relay_seen_delete AFTER DELETE ON relay_seen BEGIN
             UPDATE relay_stats SET retained_message_ids = retained_message_ids - 1 WHERE singleton = 1;
         END;",
    )?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn active_keys(
    connection: &rusqlite::Connection,
    device_id: DeviceId,
) -> Result<DevicePublicKeys, RelayError> {
    let keys: Option<String> = connection
        .query_row(
            "SELECT public_keys FROM relay_devices WHERE device_id = ?1 AND revoked_at_ms IS NULL",
            [device_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let keys = keys.ok_or(RelayError::AuthenticationFailed)?;
    let keys: DevicePublicKeys = serde_json::from_str(&keys)?;
    keys.validate()?;
    if keys.device_id != device_id {
        return Err(RelayError::AuthenticationFailed);
    }
    Ok(keys)
}

fn prune(connection: &rusqlite::Connection, now_ms: i64) -> Result<(), RelayError> {
    connection.execute(
        "DELETE FROM relay_messages WHERE expires_at_ms <= ?1",
        [now_ms],
    )?;
    connection.execute("DELETE FROM relay_seen WHERE expires_at_ms <= ?1", [now_ms])?;
    Ok(())
}

fn timestamp(now_ms: u64) -> Result<i64, RelayError> {
    i64::try_from(now_ms).map_err(|_| RelayError::Clock)
}

fn prepare_database_file(path: PathBuf) -> Result<PathBuf, RelayError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(RelayError::Configuration(
                "relay database must be a regular file".to_owned(),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}
