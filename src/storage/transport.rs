use super::*;
use crate::protocol::{CommandEnvelope, DeviceId, MessageId, SyncBatch, SyncCursor};

const MAX_TRANSPORT_ENVELOPE: usize = 2 * MAX_JSON_BYTES;
const MAX_PENDING_INBOX: i64 = 1_024;
const MAX_PENDING_INBOX_BYTES: i64 = 64 * 1_048_576;
const MAX_PEER_INBOX: i64 = 128;
const MAX_PEER_INBOX_BYTES: i64 = 8 * 1_048_576;
const COMMAND_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_OUTBOUND_EXCLUSIONS: usize = 16;
const MAX_OUTBOUND_CANDIDATES: i64 = 17;

pub(crate) enum TransportOutboundRouting {
    Ordinary,
    PrivateSettings,
}

pub(crate) enum TransportOutboundSelection {
    OrdinaryOnly,
    SettingsEnabled,
}

pub(crate) struct TransportOutboundCandidate {
    pub(crate) message_id: MessageId,
    pub(crate) recipient: DeviceId,
    pub(crate) not_before: i64,
    pub(crate) created_at: i64,
    pub(crate) routing: TransportOutboundRouting,
}

pub(crate) struct LoadedOrdinaryOutbound {
    row: OutboxRow,
}

impl LoadedOrdinaryOutbound {
    pub(crate) fn into_row(self) -> OutboxRow {
        self.row
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransportInboxRow {
    pub message_id: String,
    pub sender: String,
    pub recipient: String,
    pub envelope_digest: String,
    pub payload: Vec<u8>,
    pub received_at: i64,
    pub expires_at: i64,
}

pub(crate) struct TransportInboxMetadata<'a> {
    pub message_id: &'a str,
    pub sender: &'a str,
    pub recipient: &'a str,
    pub envelope_digest: &'a str,
    pub received_at: i64,
    pub expires_at: i64,
}

#[derive(Clone, Copy)]
pub(crate) enum TransportInboxRejection {
    SettingsUnavailable,
    InvalidPacket,
    UnsupportedVersion,
}

impl TransportInboxRejection {
    fn detail(self) -> &'static str {
        match self {
            Self::SettingsUnavailable => "settings_unavailable",
            Self::InvalidPacket => "invalid_packet",
            Self::UnsupportedVersion => "unsupported_version",
        }
    }
}

enum TransportInboxDisposition<'a> {
    Pending(&'a [u8]),
    Rejected(TransportInboxRejection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportInboxCursor {
    pub received_at: i64,
    pub message_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransportOutboxInsert {
    pub message: OutboxRow,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCommandState {
    Pending,
    Completed,
    Rejected,
}

impl RemoteCommandState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteCommandRow {
    pub command_id: String,
    pub sender: String,
    pub conversation_id: String,
    pub request: Option<Value>,
    pub request_digest: String,
    pub state: RemoteCommandState,
    pub result: Option<Value>,
    pub result_digest: Option<String>,
    pub received_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommandCursor {
    pub received_at: i64,
    pub command_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteCommandReservation {
    pub created: bool,
    pub command: RemoteCommandRow,
}

pub struct RemoteCommandCompletion<'a> {
    pub command_id: &'a str,
    pub original: &'a Value,
    pub result: &'a Value,
    pub rejected: bool,
    pub now: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransportPruneReport {
    pub expired_outbox_ids: Vec<String>,
    pub expired_replay_markers: usize,
    pub expired_sync_offers: usize,
    pub archived_command_payloads: usize,
}

impl Store {
    pub fn configure_transport_endpoint(
        &self,
        device_id: &str,
        configuration: &Value,
    ) -> StorageResult<bool> {
        validate_id(device_id)?;
        let configuration = encode_json_with_limit(configuration, 16_384)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, Option<i64>)> = transaction.query_row(
            "SELECT device_id, configuration, revoked_at FROM transport_endpoint WHERE singleton = 1", [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if let Some((id, original, revoked_at)) = existing {
            if id != device_id || original != configuration {
                return Err(StorageError::TransportConflict(
                    "local endpoint identity or role".into(),
                ));
            }
            return Ok(revoked_at.is_some());
        }
        transaction.execute("INSERT INTO transport_endpoint(singleton, device_id, configuration) VALUES (1, ?1, ?2)", params![device_id, configuration])?;
        transaction.commit()?;
        Ok(false)
    }

    pub fn transport_endpoint_revoked(&self) -> StorageResult<bool> {
        Ok(self.connection()?.query_row(
            "SELECT revoked_at IS NOT NULL FROM transport_endpoint WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn revoke_transport_endpoint(&self, now: i64) -> StorageResult<()> {
        validate_time(now)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("UPDATE transport_endpoint SET revoked_at = coalesce(revoked_at, ?1) WHERE singleton = 1", [now])?;
        transaction.execute(
            "DELETE FROM outbox WHERE id IN (SELECT message_id FROM transport_outbox)",
            [],
        )?;
        transaction.execute("UPDATE transport_inbox SET state='rejected', detail='endpoint_revoked', payload=NULL WHERE state='pending'",[])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pin_transport_device(
        &self,
        device: &DeviceRow,
        pairing_id: &str,
    ) -> StorageResult<DeviceRow> {
        validate_id(&device.id)?;
        validate_id(pairing_id)?;
        validate_text(&device.label, "device label", 256)?;
        validate_time(device.created_at)?;
        if device.revoked_at.is_some() {
            return invalid("new paired devices cannot be revoked");
        }
        let payload = encode_json_with_limit(&device.public_identity, 16_384)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let used: Option<String> = transaction
            .query_row(
                "SELECT device_id FROM pairing_uses WHERE pairing_id = ?1",
                [pairing_id],
                |row| row.get(0),
            )
            .optional()?;
        if used.as_ref().is_some_and(|id| id != &device.id) {
            return Err(StorageError::TransportConflict(pairing_id.into()));
        }
        let existing = transaction.query_row("SELECT id, label, public_identity, created_at, revoked_at FROM devices WHERE id = ?1", [&device.id], device_row).optional()?;
        let pinned = if let Some(existing) = existing {
            if existing.revoked_at.is_some() {
                return Err(StorageError::RevokedDevice(device.id.clone()));
            }
            if existing.public_identity != device.public_identity {
                return Err(StorageError::TransportConflict(device.id.clone()));
            }
            existing
        } else {
            let count: i64 =
                transaction.query_row("SELECT count(*) FROM devices", [], |row| row.get(0))?;
            if count >= 4_096 {
                return Err(StorageError::TransportCapacity(
                    "paired device registrations",
                ));
            }
            transaction.execute("INSERT INTO devices(id, label, public_identity, created_at) VALUES (?1, ?2, ?3, ?4)", params![device.id, device.label, payload, device.created_at])?;
            device.clone()
        };
        if used.is_none() {
            let uses: i64 =
                transaction.query_row("SELECT count(*) FROM pairing_uses", [], |row| row.get(0))?;
            if uses >= 100_000 {
                return Err(StorageError::TransportCapacity(
                    "completed pairing identifiers",
                ));
            }
        }
        transaction.execute("INSERT OR IGNORE INTO pairing_uses(pairing_id, device_id, completed_at) VALUES (?1, ?2, ?3)", params![pairing_id, device.id, device.created_at])?;
        transaction.commit()?;
        Ok(pinned)
    }

    pub fn accept_transport_inbox(
        &self,
        receipt: &TransportInboxRow,
        pinned_identity: &Value,
    ) -> StorageResult<bool> {
        self.record_transport_inbox(
            &TransportInboxMetadata {
                message_id: &receipt.message_id,
                sender: &receipt.sender,
                recipient: &receipt.recipient,
                envelope_digest: &receipt.envelope_digest,
                received_at: receipt.received_at,
                expires_at: receipt.expires_at,
            },
            pinned_identity,
            TransportInboxDisposition::Pending(&receipt.payload),
        )
    }

    pub(crate) fn reject_transport_inbox(
        &self,
        receipt: &TransportInboxMetadata<'_>,
        pinned_identity: &Value,
        rejection: TransportInboxRejection,
    ) -> StorageResult<bool> {
        self.record_transport_inbox(
            receipt,
            pinned_identity,
            TransportInboxDisposition::Rejected(rejection),
        )
    }

    fn record_transport_inbox(
        &self,
        receipt: &TransportInboxMetadata<'_>,
        pinned_identity: &Value,
        disposition: TransportInboxDisposition<'_>,
    ) -> StorageResult<bool> {
        validate_inbox(receipt, &disposition)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !prepare_transport_receipt_in(&transaction, receipt, pinned_identity)? {
            return Ok(false);
        }
        let (markers, pending, pending_bytes): (i64, i64, i64) = transaction.query_row("SELECT count(*), coalesce(sum(state = 'pending'), 0), coalesce(sum(CASE WHEN state = 'pending' THEN length(payload) ELSE 0 END), 0) FROM transport_inbox", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let marker_full = markers >= 100_000;
        let pending_full = match &disposition {
            TransportInboxDisposition::Pending(payload) => {
                let (peer_count, peer_bytes): (i64, i64) = transaction.query_row("SELECT count(*), coalesce(sum(length(payload)), 0) FROM transport_inbox WHERE sender = ?1 AND state = 'pending'", [receipt.sender], |row| Ok((row.get(0)?, row.get(1)?)))?;
                !marker_full
                    && (pending >= MAX_PENDING_INBOX
                        || pending_bytes + payload.len() as i64 > MAX_PENDING_INBOX_BYTES
                        || peer_count >= MAX_PEER_INBOX
                        || peer_bytes + payload.len() as i64 > MAX_PEER_INBOX_BYTES)
            }
            TransportInboxDisposition::Rejected(_) => false,
        };
        if marker_full || pending_full {
            return Err(StorageError::TransportCapacity("durable receiver inbox"));
        }
        match disposition {
            TransportInboxDisposition::Pending(payload) => {
                transaction.execute("INSERT INTO transport_inbox(message_id, sender, recipient, envelope_digest, payload, received_at, expires_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')", params![receipt.message_id, receipt.sender, receipt.recipient, receipt.envelope_digest, payload, receipt.received_at, receipt.expires_at])?;
            }
            TransportInboxDisposition::Rejected(rejection) => {
                transaction.execute("INSERT INTO transport_inbox(message_id, sender, recipient, envelope_digest, payload, received_at, expires_at, state, command_id, detail) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, 'rejected', NULL, ?7)", params![receipt.message_id, receipt.sender, receipt.recipient, receipt.envelope_digest, receipt.received_at, receipt.expires_at, rejection.detail()])?;
            }
        }
        transaction.commit()?;
        Ok(true)
    }

    #[cfg(unix)]
    pub(crate) fn admit_settings_bind(
        &self,
        admission: &super::SettingsBindAdmission<'_>,
    ) -> Result<super::SettingsBindReceiptCommit, super::SettingsBindAdmissionError> {
        use super::{SettingsBindAdmissionError, SettingsBindReceiptCommit};
        admission
            .source
            .check_precommit(self, admission.intent)
            .map_err(SettingsBindAdmissionError::Rejected)?;
        let receipt = admission
            .source
            .receipt_metadata()
            .map_err(SettingsBindAdmissionError::Rejected)?;
        validate_inbox_metadata(&receipt)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::from)?;
        if !prepare_transport_receipt_in(
            &transaction,
            &receipt,
            admission.source.pinned_identity(),
        )? {
            return Ok(SettingsBindReceiptCommit::Duplicate);
        }
        let markers: i64 = transaction
            .query_row("SELECT count(*) FROM transport_inbox", [], |row| row.get(0))
            .map_err(StorageError::from)?;
        if markers >= 100_000 {
            return Err(SettingsBindAdmissionError::ReplayCapacity(
                StorageError::TransportCapacity("durable receiver inbox"),
            ));
        }
        let (grant, lifetime, now) =
            super::settings_transport::validate_bind_on(self, &transaction, admission)?;
        let creator = admission.source.reserve_binding(admission.intent)?;
        let reservation = super::settings_transport::reserve_bind_reply_in(
            &transaction,
            admission,
            lifetime,
            now,
        )?;
        transaction.execute("INSERT INTO transport_inbox(message_id,sender,recipient,envelope_digest,payload,received_at,expires_at,state,command_id,detail) VALUES(?1,?2,?3,?4,NULL,?5,?6,'processed',NULL,NULL)",params![receipt.message_id,receipt.sender,receipt.recipient,receipt.envelope_digest,receipt.received_at,receipt.expires_at]).map_err(StorageError::from)?;
        check_receipt_receiver(&transaction, &receipt, admission.source.pinned_identity())?;
        let (current, final_lifetime, _) =
            super::settings_transport::validate_bind_on(self, &transaction, admission)?;
        if current != grant
            || final_lifetime.expires_at != lifetime.expires_at
            || final_lifetime.deadline != lifetime.deadline
        {
            return Err(SettingsBindAdmissionError::Bookkeeping);
        }
        transaction.commit().map_err(StorageError::from)?;
        creator.committed(lifetime)?;
        Ok(SettingsBindReceiptCommit::Fresh {
            reservation,
            grant: Box::new(grant),
            lifetime,
            creator,
        })
    }

    #[cfg(unix)]
    pub(crate) fn admit_settings_command(
        &self,
        admission: &super::SettingsCommandAdmission<'_>,
    ) -> Result<super::SettingsCommandReceiptCommit, super::SettingsCommandAdmissionError> {
        use super::settings_originals::{
            RemoteOriginalAdmission, RemoteOriginalKey, insert_original_in, read_original_on,
        };
        use super::{SettingsCommandAdmissionError, SettingsCommandReceiptCommit};
        use crate::host::settings::service::SettingsServiceError;

        admission.source.check_precommit(self).map_err(|error| {
            SettingsCommandAdmissionError::Authority(SettingsServiceError::from(error))
        })?;
        let receipt = admission.source.receipt_metadata().map_err(|error| {
            SettingsCommandAdmissionError::Authority(SettingsServiceError::from(error))
        })?;
        validate_inbox_metadata(&receipt)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::from)?;
        check_receipt_receiver(&transaction, &receipt, admission.source.pinned_identity())?;
        let existing: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT sender, recipient, envelope_digest FROM transport_inbox WHERE message_id = ?1",
                [receipt.message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(StorageError::from)?;
        if let Some((sender, recipient, digest)) = existing {
            if sender != receipt.sender
                || recipient != receipt.recipient
                || digest != receipt.envelope_digest
            {
                return Err(StorageError::TransportConflict(receipt.message_id.into()).into());
            }
            return Ok(SettingsCommandReceiptCommit::Duplicate);
        }
        transaction
            .execute(
                "DELETE FROM transport_inbox WHERE expires_at <= ?1 AND state != 'pending'",
                [receipt.received_at],
            )
            .map_err(StorageError::from)?;
        let markers: i64 = transaction
            .query_row("SELECT count(*) FROM transport_inbox", [], |row| row.get(0))
            .map_err(StorageError::from)?;
        if markers >= 100_000 {
            return Err(rollback_command_receipt(
                transaction,
                SettingsCommandAdmissionError::ReplayCapacity(StorageError::TransportCapacity(
                    "durable receiver inbox",
                )),
            ));
        }
        let qualified =
            match admission
                .authority
                .precommit_command_on(self, &transaction, admission.source)
            {
                Ok(qualified) => qualified,
                Err(error) => {
                    return Err(rollback_command_receipt(
                        transaction,
                        SettingsCommandAdmissionError::Authority(error),
                    ));
                }
            };
        let RemoteOriginalAdmission::Accepted(identity) = qualified.original_admission() else {
            return Err(SettingsCommandAdmissionError::Bookkeeping);
        };
        let key = RemoteOriginalKey {
            host_id: identity.host_id,
            actor_device_id: identity.peer_keys.device_id,
            request_id: identity.request_id,
        };
        let (original, known) = match read_original_on(&transaction, key)? {
            Some(original) => {
                if !super::settings_transport::matches_command_original(&original, identity) {
                    return Err(StorageError::StateConflict {
                        entity: "remote Settings original",
                        id: identity.request_id.to_string(),
                    }
                    .into());
                }
                (original, true)
            }
            None => {
                let original =
                    match insert_original_in(&transaction, qualified.original_admission()) {
                        Err(error @ StorageError::TransportCapacity(_)) => {
                            return Err(rollback_command_receipt(
                                transaction,
                                SettingsCommandAdmissionError::OriginalCapacity(error),
                            ));
                        }
                        result => result?,
                    };
                (original, false)
            }
        };
        let reply =
            match super::settings_transport::reserve_command_reply_in(&transaction, identity) {
                Err(error @ SettingsCommandAdmissionError::ReplyCapacity(_)) => {
                    return Err(rollback_command_receipt(transaction, error));
                }
                result => result?,
            };
        transaction.execute("INSERT INTO transport_inbox(message_id,sender,recipient,envelope_digest,payload,received_at,expires_at,state,command_id,detail) VALUES(?1,?2,?3,?4,NULL,?5,?6,'processed',NULL,NULL)",params![receipt.message_id,receipt.sender,receipt.recipient,receipt.envelope_digest,receipt.received_at,receipt.expires_at]).map_err(StorageError::from)?;
        check_receipt_receiver(&transaction, &receipt, admission.source.pinned_identity())?;
        if let Err(error) = admission
            .authority
            .recheck_command_on(self, &transaction, &qualified)
        {
            return Err(rollback_command_receipt(
                transaction,
                SettingsCommandAdmissionError::Authority(error),
            ));
        }
        transaction.commit().map_err(StorageError::from)?;
        if known {
            Ok(SettingsCommandReceiptCommit::Known { original, reply })
        } else {
            Ok(SettingsCommandReceiptCommit::Fresh { original, reply })
        }
    }

    pub fn pending_transport_inbox(&self, limit: usize) -> StorageResult<Vec<TransportInboxRow>> {
        self.pending_transport_inbox_after(None, limit)
    }

    pub fn pending_transport_inbox_after(
        &self,
        cursor: Option<&TransportInboxCursor>,
        limit: usize,
    ) -> StorageResult<Vec<TransportInboxRow>> {
        validate_limit(limit)?;
        if let Some(cursor) = cursor {
            validate_time(cursor.received_at)?;
            validate_id(&cursor.message_id)?;
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT i.message_id, i.sender, i.recipient, i.envelope_digest, i.payload, i.received_at, i.expires_at FROM transport_inbox i JOIN devices d ON d.id = i.sender WHERE i.state = 'pending' AND d.revoked_at IS NULL AND (?1 IS NULL OR (i.received_at, i.message_id) > (?1, ?2)) ORDER BY i.received_at, i.message_id LIMIT ?3")?;
        collect_rows(statement.query_map(
            params![
                cursor.map(|cursor| cursor.received_at),
                cursor.map(|cursor| cursor.message_id.as_str()),
                limit as i64
            ],
            inbox_row,
        )?)
    }

    pub fn finish_transport_inbox(
        &self,
        message_id: &str,
        rejection: Option<&str>,
    ) -> StorageResult<()> {
        validate_id(message_id)?;
        if let Some(reason) = rejection {
            validate_text(reason, "inbox rejection", 256)?;
        }
        let connection = self.connection()?;
        finish_inbox(&connection, message_id, rejection)
    }

    pub fn queue_transport_message(
        &self,
        item: &TransportOutboxInsert,
        pinned_identity: &Value,
        offer: Option<&SyncBatch>,
        processed_inbox: Option<&str>,
    ) -> StorageResult<()> {
        validate_outbound(item)?;
        if let Some(batch) = offer {
            batch
                .validate()
                .map_err(|_| StorageError::InvalidInput("invalid sync offer".into()))?;
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_peer(&transaction, &item.message.device_id, Some(pinned_identity))?;
        enqueue_transport(&transaction, item)?;
        if let Some(batch) = offer {
            let conversation_id = batch.cursor.conversation_id.to_string();
            let head: u64 = transaction
                .query_row(
                    "SELECT last_sequence FROM conversations WHERE id = ?1",
                    [&conversation_id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| missing("conversation", &conversation_id))?;
            if batch.next_cursor().after_sequence > head {
                return invalid("sync offer cursor exceeds the host head");
            }
            let offers: i64 =
                transaction.query_row("SELECT count(*) FROM sync_offers", [], |row| row.get(0))?;
            if offers >= 4_096 {
                return Err(StorageError::TransportCapacity(
                    "unacknowledged synchronization offers",
                ));
            }
            transaction.execute("INSERT INTO sync_offers(message_id, device_id, conversation_id, after_sequence, through_sequence, last_event_id, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![item.message.id, item.message.device_id, conversation_id, batch.cursor.after_sequence, batch.next_cursor().after_sequence, batch.events.last().map(|event|event.event_id.to_string()), item.expires_at])?;
        }
        if let Some(id) = processed_inbox {
            finish_inbox(&transaction, id, None)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn next_transport_outbound_candidate(
        &self,
        now: i64,
        excluded: &std::collections::BTreeSet<MessageId>,
        selection: TransportOutboundSelection,
    ) -> StorageResult<Option<TransportOutboundCandidate>> {
        validate_time(now)?;
        if excluded.len() > MAX_OUTBOUND_EXCLUSIONS {
            return invalid("outbound candidate exclusion limit exceeded");
        }
        let include_private = matches!(selection, TransportOutboundSelection::SettingsEnabled);
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT o.id,o.device_id,o.not_before,o.created_at,EXISTS(SELECT 1 FROM settings_transport_private_outbox AS p WHERE p.message_id=o.id) FROM outbox AS o JOIN transport_outbox AS t ON t.message_id=o.id JOIN devices AS d ON d.id=o.device_id WHERE o.delivered_at IS NULL AND o.not_before<=?1 AND t.expires_at>?1 AND d.revoked_at IS NULL AND (?2 OR NOT EXISTS(SELECT 1 FROM settings_transport_private_outbox AS p WHERE p.message_id=o.id)) ORDER BY o.not_before,o.created_at,o.id LIMIT ?3")?;
        let mut rows = statement.query(params![now, include_private, MAX_OUTBOUND_CANDIDATES])?;
        while let Some(row) = rows.next()? {
            let candidate = outbound_candidate_row(row)?;
            if !excluded.contains(&candidate.message_id) {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    pub(crate) fn load_ordinary_transport_candidate(
        &self,
        now: i64,
        candidate: &TransportOutboundCandidate,
    ) -> StorageResult<Option<LoadedOrdinaryOutbound>> {
        validate_time(now)?;
        validate_interval(candidate.created_at, candidate.not_before)?;
        if !matches!(candidate.routing, TransportOutboundRouting::Ordinary) {
            return invalid("ordinary outbound loading requires ordinary routing");
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT o.id,o.device_id,o.envelope,o.created_at,o.not_before,o.attempts,o.delivered_at FROM outbox AS o JOIN transport_outbox AS t ON t.message_id=o.id JOIN devices AS d ON d.id=o.device_id WHERE o.id=?1 AND o.device_id=?2 AND o.created_at=?3 AND o.not_before=?4 AND o.delivered_at IS NULL AND o.not_before<=?5 AND t.expires_at>?5 AND d.revoked_at IS NULL AND NOT EXISTS(SELECT 1 FROM settings_transport_private_outbox AS p WHERE p.message_id=o.id)")?;
        let mut rows = statement.query(params![
            candidate.message_id.to_string(),
            candidate.recipient.to_string(),
            candidate.created_at,
            candidate.not_before,
            now,
        ])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(2)? else {
            return invalid("ordinary outbound envelope is not a blob");
        };
        if bytes.is_empty() || bytes.len() > MAX_TRANSPORT_ENVELOPE {
            return invalid("ordinary outbound envelope exceeds bound");
        }
        Ok(Some(LoadedOrdinaryOutbound {
            row: outbox_row(row)?,
        }))
    }

    pub fn pending_transport_outbox(
        &self,
        now: i64,
        limit: usize,
    ) -> StorageResult<Vec<OutboxRow>> {
        validate_time(now)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT o.id, o.device_id, o.envelope, o.created_at, o.not_before, o.attempts, o.delivered_at FROM outbox o JOIN transport_outbox t ON t.message_id = o.id JOIN devices d ON d.id = o.device_id WHERE o.delivered_at IS NULL AND o.not_before <= ?1 AND t.expires_at > ?1 AND d.revoked_at IS NULL AND NOT EXISTS (SELECT 1 FROM settings_transport_private_outbox AS p WHERE p.message_id=o.id) ORDER BY o.not_before, o.created_at, o.id LIMIT ?2")?;
        collect_rows(statement.query_map(params![now, limit as i64], outbox_row)?)
    }

    pub fn confirm_transport_outbox(
        &self,
        message_id: &str,
        expected_envelope: &[u8],
        pinned_identity: &Value,
        now: i64,
    ) -> StorageResult<bool> {
        validate_id(message_id)?;
        validate_time(now)?;
        if expected_envelope.len() > MAX_TRANSPORT_ENVELOPE {
            return invalid("encrypted envelope exceeds bound");
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        super::settings_transport::require_ordinary_outbox(&transaction, message_id)?;
        let row: Option<(String,Vec<u8>)> = transaction.query_row("SELECT o.device_id,o.envelope FROM outbox o JOIN transport_outbox t ON t.message_id=o.id JOIN transport_endpoint e ON e.singleton=1 WHERE o.id=?1 AND o.delivered_at IS NULL AND t.expires_at>?2 AND e.revoked_at IS NULL",params![message_id,now],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let Some((recipient, bytes)) = row else {
            return Ok(false);
        };
        if bytes != expected_envelope {
            return Err(StorageError::TransportConflict(message_id.into()));
        }
        match active_peer(&transaction, &recipient, Some(pinned_identity)) {
            Err(StorageError::RevokedDevice(_)) => return Ok(false),
            result => {
                result?;
            }
        }
        transaction.commit()?;
        Ok(true)
    }

    pub fn accept_transport_outbox(&self, message_id: &str) -> StorageResult<()> {
        validate_id(message_id)?;
        self.connection()?.execute("DELETE FROM outbox WHERE id = ?1 AND EXISTS (SELECT 1 FROM transport_outbox t WHERE t.message_id = outbox.id)", [message_id])?;
        Ok(())
    }

    pub fn maintain_transport(&self, now: i64) -> StorageResult<TransportPruneReport> {
        validate_time(now)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expired_outbox_ids = {
            let mut statement = transaction.prepare("SELECT message_id FROM transport_outbox WHERE expires_at <= ?1 ORDER BY message_id LIMIT 1024")?;
            collect_rows(statement.query_map([now], |row| row.get::<_, String>(0))?)?
        };
        transaction.execute("DELETE FROM outbox WHERE id IN (SELECT message_id FROM transport_outbox WHERE expires_at <= ?1)", [now])?;
        let expired_replay_markers = transaction.execute(
            "DELETE FROM transport_inbox WHERE expires_at <= ?1 AND state != 'pending'",
            [now],
        )?;
        let expired_sync_offers =
            transaction.execute("DELETE FROM sync_offers WHERE expires_at <= ?1", [now])?;
        let archived_command_payloads = transaction.execute("UPDATE remote_commands SET request = NULL, result = NULL WHERE state != 'pending' AND updated_at < ?1 AND (request IS NOT NULL OR result IS NOT NULL)", [now.saturating_sub(COMMAND_RETENTION_MS)])?;
        transaction.commit()?;
        Ok(TransportPruneReport {
            expired_outbox_ids,
            expired_replay_markers,
            expired_sync_offers,
            archived_command_payloads,
        })
    }

    pub fn reserve_remote_command(
        &self,
        message_id: &str,
        command: &CommandEnvelope,
        original: &Value,
        pinned_identity: &Value,
        now: i64,
    ) -> StorageResult<RemoteCommandReservation> {
        validate_id(message_id)?;
        validate_time(now)?;
        let decoded: CommandEnvelope = serde_json::from_value(original.clone())?;
        if decoded != *command {
            return invalid("original remote command does not match typed command");
        }
        let encoded = encode_json(original)?;
        let request_digest =
            transport_digest(b"everything.remote-command.v1\0", encoded.as_bytes());
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sender: String = transaction
            .query_row(
                "SELECT sender FROM transport_inbox WHERE message_id = ?1 AND state = 'pending'",
                [message_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| missing("pending inbox message", message_id))?;
        active_peer(&transaction, &sender, Some(pinned_identity))?;
        if command
            .issuing_device_id
            .map(|id| id.to_string())
            .as_deref()
            != Some(sender.as_str())
        {
            return Err(StorageError::TransportConflict(
                "command issuing device".into(),
            ));
        }
        let id = command.command_id.to_string();
        let existing = read_remote_command(&transaction, &id)?;
        let (created, stored) = if let Some(existing) = existing {
            if existing.sender != sender
                || existing.conversation_id != command.conversation_id.to_string()
                || existing.request_digest != request_digest
                || existing
                    .request
                    .as_ref()
                    .is_some_and(|request| request != original)
            {
                return Err(StorageError::TransportConflict(id));
            }
            (false, existing)
        } else {
            let (count, pending, bytes): (i64, i64, i64) = transaction.query_row("SELECT count(*), coalesce(sum(state = 'pending'), 0), coalesce(sum(coalesce(length(CAST(request AS BLOB)), 0) + coalesce(length(CAST(result AS BLOB)), 0)), 0) FROM remote_commands", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            if count >= 100_000
                || pending >= 1024
                || bytes + encoded.len() as i64 + (pending + 1) * 65_536 > 128 * 1_048_576
            {
                return Err(StorageError::TransportCapacity("remote command journal"));
            }
            transaction.execute("INSERT INTO remote_commands(command_id, sender, conversation_id, request, request_digest, state, received_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)", params![id, sender, command.conversation_id.to_string(), encoded, request_digest, now])?;
            let row = RemoteCommandRow {
                command_id: id.clone(),
                sender,
                conversation_id: command.conversation_id.to_string(),
                request: Some(original.clone()),
                request_digest,
                state: RemoteCommandState::Pending,
                result: None,
                result_digest: None,
                received_at: now,
                updated_at: now,
            };
            (true, row)
        };
        transaction.execute(
            "UPDATE transport_inbox SET command_id = ?2 WHERE message_id = ?1",
            params![message_id, id],
        )?;
        transaction.commit()?;
        Ok(RemoteCommandReservation {
            created,
            command: stored,
        })
    }

    pub fn pending_remote_commands(&self, limit: usize) -> StorageResult<Vec<RemoteCommandRow>> {
        self.pending_remote_commands_after(None, limit)
    }

    pub fn pending_remote_commands_after(
        &self,
        cursor: Option<&RemoteCommandCursor>,
        limit: usize,
    ) -> StorageResult<Vec<RemoteCommandRow>> {
        validate_limit(limit)?;
        if let Some(cursor) = cursor {
            validate_time(cursor.received_at)?;
            validate_id(&cursor.command_id)?;
        }
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT command_id, sender, conversation_id, request, request_digest, state, result, result_digest, received_at, updated_at FROM remote_commands WHERE state = 'pending' AND (?1 IS NULL OR (received_at, command_id) > (?1, ?2)) ORDER BY received_at, command_id LIMIT ?3")?;
        collect_rows(statement.query_map(
            params![
                cursor.map(|cursor| cursor.received_at),
                cursor.map(|cursor| cursor.command_id.as_str()),
                limit as i64
            ],
            remote_command_row,
        )?)
    }

    pub fn complete_remote_command(
        &self,
        completion: &RemoteCommandCompletion<'_>,
        reply: &TransportOutboxInsert,
        pinned_identity: &Value,
    ) -> StorageResult<RemoteCommandRow> {
        let RemoteCommandCompletion {
            command_id,
            original,
            result,
            rejected,
            now,
        } = *completion;
        validate_id(command_id)?;
        validate_time(now)?;
        validate_outbound(reply)?;
        let request_digest = transport_digest(
            b"everything.remote-command.v1\0",
            encode_json(original)?.as_bytes(),
        );
        let result_json = encode_json_with_limit(result, 65_536)?;
        let result_digest = transport_digest(
            b"everything.remote-command.result.v1\0",
            result_json.as_bytes(),
        );
        let next = if rejected {
            RemoteCommandState::Rejected
        } else {
            RemoteCommandState::Completed
        };
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut row = read_remote_command(&transaction, command_id)?
            .ok_or_else(|| missing("remote command", command_id))?;
        active_peer(&transaction, &row.sender, Some(pinned_identity))?;
        if reply.message.device_id != row.sender {
            return invalid("command completion reply has wrong recipient");
        }
        if row.request_digest != request_digest || now < row.updated_at {
            return Err(StorageError::TransportConflict(command_id.into()));
        }
        if row.state != RemoteCommandState::Pending {
            if row.state != next || row.result_digest.as_deref() != Some(result_digest.as_str()) {
                return Err(StorageError::TransportConflict(command_id.into()));
            }
            enqueue_transport(&transaction, reply)?;
            transaction.commit()?;
            return Ok(row);
        }
        transaction.execute("UPDATE remote_commands SET state = ?2, result = ?3, result_digest = ?4, updated_at = ?5 WHERE command_id = ?1", params![command_id,next.as_str(),result_json,result_digest,now])?;
        transaction.execute("UPDATE transport_inbox SET state = 'processed', payload = NULL WHERE command_id = ?1 AND state = 'pending'", [command_id])?;
        enqueue_transport(&transaction, reply)?;
        transaction.commit()?;
        row.state = next;
        row.result = Some(result.clone());
        row.result_digest = Some(result_digest);
        row.updated_at = now;
        Ok(row)
    }

    pub fn acknowledge_command_result(&self, message_id: &str) -> StorageResult<()> {
        validate_id(message_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, payload): (String, Option<Vec<u8>>) = transaction
            .query_row(
                "SELECT state, payload FROM transport_inbox WHERE message_id = ?1",
                [message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| missing("transport inbox", message_id))?;
        if state == "processed" {
            return Ok(());
        }
        if state != "pending" {
            return Err(StorageError::TransportConflict(message_id.into()));
        }
        let value: Value = serde_json::from_slice(
            payload
                .as_deref()
                .ok_or_else(|| missing("inbox payload", message_id))?,
        )?;
        if value.pointer("/payload/kind").and_then(Value::as_str) != Some("command_result") {
            return invalid("application acknowledgement requires a command result");
        }
        finish_inbox(&transaction, message_id, None)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn replica_cursor(&self, host_id: &str, conversation_id: &str) -> StorageResult<u64> {
        validate_id(host_id)?;
        validate_id(conversation_id)?;
        Ok(self.connection()?.query_row("SELECT after_sequence FROM replica_cursors WHERE host_id = ?1 AND conversation_id = ?2", params![host_id,conversation_id], |row|row.get(0)).optional()?.unwrap_or(0))
    }

    pub fn replica_events(
        &self,
        host_id: &str,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> StorageResult<Vec<crate::protocol::EventEnvelope>> {
        validate_id(host_id)?;
        validate_id(conversation_id)?;
        validate_sequence(after_sequence)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT envelope FROM replica_events WHERE host_id = ?1 AND conversation_id = ?2 AND sequence > ?3 ORDER BY sequence LIMIT ?4")?;
        let rows = collect_rows(statement.query_map(
            params![host_id, conversation_id, after_sequence, limit as i64],
            |row| row.get::<_, String>(0),
        )?)?;
        rows.into_iter()
            .map(|raw| serde_json::from_str(&raw).map_err(StorageError::from))
            .collect()
    }

    pub fn apply_replica_sync(
        &self,
        message_id: &str,
        host_id: &str,
        pinned_identity: &Value,
        batch: &SyncBatch,
        replies: &[TransportOutboxInsert],
    ) -> StorageResult<SyncCursor> {
        validate_id(message_id)?;
        validate_id(host_id)?;
        batch.validate().map_err(|_| {
            StorageError::InvalidInput("invalid replica synchronization batch".into())
        })?;
        if replies.len() > 2 {
            return invalid("replica application permits at most two response envelopes");
        }
        for reply in replies {
            validate_outbound(reply)?;
            if reply.message.device_id != host_id {
                return invalid("replica replies must target the authoritative host");
            }
        }
        let conversation_id = batch.cursor.conversation_id.to_string();
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_peer(&transaction, host_id, Some(pinned_identity))?;
        require_pending_sender(&transaction, message_id, host_id)?;
        let current: u64 = transaction.query_row("SELECT after_sequence FROM replica_cursors WHERE host_id = ?1 AND conversation_id = ?2",params![host_id,conversation_id],|row|row.get(0)).optional()?.unwrap_or(0);
        if batch.cursor.after_sequence > current {
            return Err(StorageError::SyncGap {
                expected: current,
                actual: batch.cursor.after_sequence,
            });
        }
        let mut through = current;
        for event in &batch.events {
            validate_sequence(event.sequence.get())?;
            let encoded = serde_json::to_string(event)?;
            let existing: Option<(String,String)> = transaction.query_row("SELECT event_id, envelope FROM replica_events WHERE host_id = ?1 AND conversation_id = ?2 AND sequence = ?3",params![host_id,conversation_id,event.sequence.get()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((id, original)) = existing {
                if id != event.event_id.to_string() || original != encoded {
                    return Err(StorageError::TransportConflict(format!(
                        "replica event {}",
                        event.event_id
                    )));
                }
            } else {
                if event.sequence.get() != through.saturating_add(1) {
                    return Err(StorageError::SyncGap {
                        expected: through,
                        actual: event.sequence.get(),
                    });
                }
                transaction.execute("INSERT INTO replica_events(host_id,conversation_id,sequence,event_id,envelope) VALUES (?1,?2,?3,?4,?5)",params![host_id,conversation_id,event.sequence.get(),event.event_id.to_string(),encoded])?;
            }
            through = through.max(event.sequence.get());
        }
        transaction.execute("INSERT INTO replica_cursors(host_id,conversation_id,after_sequence) VALUES (?1,?2,?3) ON CONFLICT(host_id,conversation_id) DO UPDATE SET after_sequence=max(replica_cursors.after_sequence,excluded.after_sequence)",params![host_id,conversation_id,through])?;
        for reply in replies {
            enqueue_transport(&transaction, reply)?;
        }
        finish_inbox(&transaction, message_id, None)?;
        transaction.commit()?;
        Ok(SyncCursor {
            conversation_id: batch.cursor.conversation_id,
            after_sequence: through,
        })
    }

    pub fn acknowledge_sync_offer(
        &self,
        message_id: &str,
        sender: &str,
        pinned_identity: &Value,
        offer_message_id: &str,
        cursor: SyncCursor,
        last_event_id: Option<&str>,
    ) -> StorageResult<()> {
        validate_id(message_id)?;
        validate_id(sender)?;
        validate_id(offer_message_id)?;
        validate_sequence(cursor.after_sequence)?;
        validate_optional_id(last_event_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        active_peer(&transaction, sender, Some(pinned_identity))?;
        require_pending_sender(&transaction, message_id, sender)?;
        let offer:Option<(String,String,u64,Option<String>)>=transaction.query_row("SELECT device_id,conversation_id,through_sequence,last_event_id FROM sync_offers WHERE message_id=?1",[offer_message_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        let Some((device, conversation, through, last)) = offer else {
            let acknowledged: u64 = transaction.query_row(
                "SELECT after_sequence FROM device_sync_cursors WHERE device_id=?1 AND conversation_id=?2",
                params![sender,cursor.conversation_id.to_string()], |row|row.get(0),
            ).optional()?.unwrap_or(0);
            if cursor.after_sequence <= acknowledged {
                finish_inbox(&transaction, message_id, None)?;
                transaction.commit()?;
                return Ok(());
            }
            return Err(missing("sync offer", offer_message_id));
        };
        if device != sender
            || conversation != cursor.conversation_id.to_string()
            || through != cursor.after_sequence
            || last.as_deref() != last_event_id
        {
            return Err(StorageError::TransportConflict(offer_message_id.into()));
        }
        transaction.execute("INSERT INTO device_sync_cursors(device_id,conversation_id,after_sequence) VALUES (?1,?2,?3) ON CONFLICT(device_id,conversation_id) DO UPDATE SET after_sequence=max(device_sync_cursors.after_sequence,excluded.after_sequence)",params![sender,conversation,through])?;
        transaction.execute(
            "DELETE FROM sync_offers WHERE message_id=?1",
            [offer_message_id],
        )?;
        finish_inbox(&transaction, message_id, None)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn acknowledged_sync_cursor(
        &self,
        device_id: &str,
        conversation_id: &str,
    ) -> StorageResult<u64> {
        validate_id(device_id)?;
        validate_id(conversation_id)?;
        Ok(self.connection()?.query_row("SELECT after_sequence FROM device_sync_cursors WHERE device_id=?1 AND conversation_id=?2",params![device_id,conversation_id],|row|row.get(0)).optional()?.unwrap_or(0))
    }
}

#[cfg(unix)]
fn rollback_command_receipt(
    transaction: rusqlite::Transaction<'_>,
    cause: super::SettingsCommandAdmissionError,
) -> super::SettingsCommandAdmissionError {
    match transaction.rollback() {
        Ok(()) => cause,
        Err(error) => super::SettingsCommandAdmissionError::Rollback {
            cause: Box::new(cause),
            rollback: StorageError::from(error),
        },
    }
}

fn check_receipt_receiver(
    connection: &Connection,
    receipt: &TransportInboxMetadata<'_>,
    pinned_identity: &Value,
) -> StorageResult<()> {
    active_peer(connection, receipt.sender, Some(pinned_identity))?;
    let local: Option<String> = connection
        .query_row(
            "SELECT device_id FROM transport_endpoint WHERE singleton = 1 AND revoked_at IS NULL",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if local.as_deref() != Some(receipt.recipient) {
        return Err(StorageError::TransportConflict(
            "local recipient identity".into(),
        ));
    }
    Ok(())
}

fn prepare_transport_receipt_in(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &TransportInboxMetadata<'_>,
    pinned_identity: &Value,
) -> StorageResult<bool> {
    check_receipt_receiver(transaction, receipt, pinned_identity)?;
    transaction.execute(
        "DELETE FROM transport_inbox WHERE expires_at <= ?1 AND state != 'pending'",
        [receipt.received_at],
    )?;
    let existing: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT sender, recipient, envelope_digest FROM transport_inbox WHERE message_id = ?1",
            [receipt.message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((sender, recipient, digest)) = existing {
        if sender != receipt.sender
            || recipient != receipt.recipient
            || digest != receipt.envelope_digest
        {
            return Err(StorageError::TransportConflict(receipt.message_id.into()));
        }
        return Ok(false);
    }
    Ok(true)
}

fn outbound_candidate_row(row: &Row<'_>) -> StorageResult<TransportOutboundCandidate> {
    let message_id = MessageId::try_from(outbound_candidate_uuid(row, 0)?)
        .map_err(|_| StorageError::InvalidInput("invalid outbound candidate message ID".into()))?;
    let recipient = DeviceId::try_from(outbound_candidate_uuid(row, 1)?).map_err(|_| {
        StorageError::InvalidInput("invalid outbound candidate recipient ID".into())
    })?;
    let not_before = outbound_candidate_integer(row, 2)?;
    let created_at = outbound_candidate_integer(row, 3)?;
    validate_interval(created_at, not_before)?;
    let routing = match outbound_candidate_integer(row, 4)? {
        0 => TransportOutboundRouting::Ordinary,
        1 => TransportOutboundRouting::PrivateSettings,
        _ => return invalid("invalid outbound candidate routing marker"),
    };
    Ok(TransportOutboundCandidate {
        message_id,
        recipient,
        not_before,
        created_at,
        routing,
    })
}

fn outbound_candidate_uuid(row: &Row<'_>, column: usize) -> StorageResult<uuid::Uuid> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column)? else {
        return invalid("outbound candidate identifier is not text");
    };
    if bytes.len() != 36 {
        return invalid("outbound candidate identifier is not a canonical UUID");
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        StorageError::InvalidInput("invalid outbound candidate identifier text".into())
    })?;
    let id = uuid::Uuid::parse_str(text)
        .map_err(|_| StorageError::InvalidInput("invalid outbound candidate UUID".into()))?;
    let mut encoded = [0_u8; 36];
    let canonical = id.hyphenated().encode_lower(&mut encoded);
    if id.is_nil() || canonical.as_bytes() != bytes {
        return invalid("outbound candidate identifier is not a canonical nonnil UUID");
    }
    Ok(id)
}

fn outbound_candidate_integer(row: &Row<'_>, column: usize) -> StorageResult<i64> {
    match row.get_ref(column)? {
        rusqlite::types::ValueRef::Integer(value) => Ok(value),
        _ => invalid("outbound candidate scalar is not an integer"),
    }
}

fn active_peer(
    connection: &Connection,
    id: &str,
    expected: Option<&Value>,
) -> StorageResult<DeviceRow> {
    let row = connection
        .query_row(
            "SELECT id,label,public_identity,created_at,revoked_at FROM devices WHERE id=?1",
            [id],
            device_row,
        )
        .optional()?
        .ok_or_else(|| missing("device", id))?;
    if row.revoked_at.is_some() {
        return Err(StorageError::RevokedDevice(id.into()));
    }
    if expected.is_some_and(|expected| expected != &row.public_identity) {
        return Err(StorageError::TransportConflict(id.into()));
    }
    Ok(row)
}

fn validate_inbox_metadata(row: &TransportInboxMetadata<'_>) -> StorageResult<()> {
    validate_id(row.message_id)?;
    validate_id(row.sender)?;
    validate_id(row.recipient)?;
    validate_time(row.received_at)?;
    validate_time(row.expires_at)?;
    if row.expires_at <= row.received_at
        || row.envelope_digest.len() != 64
        || !row
            .envelope_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return invalid("invalid durable inbox receipt");
    }
    Ok(())
}

fn validate_inbox(
    row: &TransportInboxMetadata<'_>,
    disposition: &TransportInboxDisposition<'_>,
) -> StorageResult<()> {
    validate_inbox_metadata(row)?;
    if matches!(disposition, TransportInboxDisposition::Pending(payload) if payload.is_empty() || payload.len() > MAX_JSON_BYTES)
    {
        return invalid("invalid durable inbox receipt");
    }
    Ok(())
}

fn validate_outbound(item: &TransportOutboxInsert) -> StorageResult<()> {
    let message = &item.message;
    validate_id(&message.id)?;
    validate_id(&message.device_id)?;
    validate_interval(message.created_at, message.not_before)?;
    if item.expires_at <= message.created_at
        || message.envelope.is_empty()
        || message.envelope.len() > MAX_TRANSPORT_ENVELOPE
        || message.attempts != 0
        || message.delivered_at.is_some()
    {
        return invalid("invalid encrypted outbox envelope");
    }
    Ok(())
}

pub(super) fn enqueue_transport(
    connection: &Connection,
    item: &TransportOutboxInsert,
) -> StorageResult<()> {
    active_peer(connection, &item.message.device_id, None)?;
    let local_active: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM transport_endpoint WHERE singleton=1 AND revoked_at IS NULL)",
        [],
        |row| row.get(0),
    )?;
    if !local_active {
        return Err(StorageError::TransportConflict(
            "local endpoint is revoked or unconfigured".into(),
        ));
    }
    super::settings_transport::check_outbox_capacity(
        connection,
        &item.message.device_id,
        item.message.envelope.len(),
    )?;
    connection.execute(
        "INSERT INTO outbox(id,device_id,envelope,created_at,not_before) VALUES (?1,?2,?3,?4,?5)",
        params![
            item.message.id,
            item.message.device_id,
            item.message.envelope,
            item.message.created_at,
            item.message.not_before
        ],
    )?;
    connection.execute(
        "INSERT INTO transport_outbox(message_id,expires_at) VALUES (?1,?2)",
        params![item.message.id, item.expires_at],
    )?;
    Ok(())
}

fn finish_inbox(connection: &Connection, id: &str, rejection: Option<&str>) -> StorageResult<()> {
    let next = if rejection.is_some() {
        "rejected"
    } else {
        "processed"
    };
    let changed=connection.execute("UPDATE transport_inbox SET state=?2,detail=?3,payload=NULL WHERE message_id=?1 AND state='pending'",params![id,next,rejection])?;
    if changed == 0 {
        let state: Option<String> = connection
            .query_row(
                "SELECT state FROM transport_inbox WHERE message_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if state.as_deref() != Some(next) {
            return Err(state_conflict("inbox message", id));
        }
    }
    Ok(())
}

fn require_pending_sender(connection: &Connection, id: &str, sender: &str) -> StorageResult<()> {
    let matches:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM transport_inbox WHERE message_id=?1 AND sender=?2 AND state='pending')",params![id,sender],|row|row.get(0))?;
    if !matches {
        return Err(state_conflict("inbox message", id));
    }
    Ok(())
}

fn inbox_row(row: &Row<'_>) -> rusqlite::Result<TransportInboxRow> {
    Ok(TransportInboxRow {
        message_id: row.get(0)?,
        sender: row.get(1)?,
        recipient: row.get(2)?,
        envelope_digest: row.get(3)?,
        payload: row.get(4)?,
        received_at: row.get(5)?,
        expires_at: row.get(6)?,
    })
}

fn read_remote_command(
    connection: &Connection,
    id: &str,
) -> StorageResult<Option<RemoteCommandRow>> {
    Ok(connection.query_row("SELECT command_id,sender,conversation_id,request,request_digest,state,result,result_digest,received_at,updated_at FROM remote_commands WHERE command_id=?1",[id],remote_command_row).optional()?)
}

fn remote_command_row(row: &Row<'_>) -> rusqlite::Result<RemoteCommandRow> {
    let request: Option<String> = row.get(3)?;
    let result: Option<String> = row.get(6)?;
    let state = match row.get::<_, String>(5)?.as_str() {
        "pending" => RemoteCommandState::Pending,
        "completed" => RemoteCommandState::Completed,
        "rejected" => RemoteCommandState::Rejected,
        state => {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid remote command state {state}"
            )));
        }
    };
    Ok(RemoteCommandRow {
        command_id: row.get(0)?,
        sender: row.get(1)?,
        conversation_id: row.get(2)?,
        request: request.map(|raw| parse_json(&raw, 3)).transpose()?,
        request_digest: row.get(4)?,
        state,
        result: result.map(|raw| parse_json(&raw, 6)).transpose()?,
        result_digest: row.get(7)?,
        received_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn transport_digest(domain: &[u8], bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}

#[cfg(unix)]
impl Store {
    pub(crate) fn admit_settings_access(
        &self,
        request: &crate::devices::OwnedVerifiedSettingsAccess,
    ) -> Result<
        super::settings_transport::SettingsAccessReceiptCommit,
        super::settings_transport::SettingsAccessAdmissionError,
    > {
        use super::settings_transport::{
            SettingsAccessAdmissionError, SettingsAccessReceiptCommit, check_access_reply_on,
            reserve_access_reply_in,
        };
        use crate::host::settings::service::SettingsAccessQualificationError;

        let source = request.authority_source();
        source
            .check_precommit(self)
            .map_err(SettingsAccessQualificationError::from)?;
        let receipt = source
            .receipt_metadata()
            .map_err(SettingsAccessQualificationError::from)?;
        validate_inbox_metadata(&receipt)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StorageError::from)?;
        let result = (|| {
            check_receipt_receiver(&transaction, &receipt, source.pinned_identity())?;
            if access_receipt_duplicate_on(&transaction, &receipt)? {
                source
                    .check_precommit(self)
                    .map_err(SettingsAccessQualificationError::from)?;
                return Ok(SettingsAccessReceiptCommit::Duplicate);
            }
            transaction
                .execute(
                    "DELETE FROM transport_inbox WHERE expires_at <= ?1 AND state != 'pending'",
                    [receipt.received_at],
                )
                .map_err(StorageError::from)?;
            let markers: i64 = transaction
                .query_row("SELECT count(*) FROM transport_inbox", [], |row| row.get(0))
                .map_err(StorageError::from)?;
            if markers >= 100_000 {
                return Err(SettingsAccessAdmissionError::ReplayCapacity(
                    StorageError::TransportCapacity("durable receiver inbox"),
                ));
            }
            let authority = request.receipt_authority();
            let plan = authority.check_access_receipt_on(self, &transaction, request)?;
            let reply = reserve_access_reply_in(&transaction, plan.reservation_spec())?;
            transaction.execute(
                "INSERT INTO transport_inbox(message_id,sender,recipient,envelope_digest,payload,received_at,expires_at,state,command_id,detail) VALUES(?1,?2,?3,?4,NULL,?5,?6,'processed',NULL,NULL)",
                params![receipt.message_id,receipt.sender,receipt.recipient,receipt.envelope_digest,receipt.received_at,receipt.expires_at],
            ).map_err(StorageError::from)?;
            check_access_marker_on(&transaction, &receipt)?;
            check_access_reply_on(&transaction, &reply)?;
            check_receipt_receiver(&transaction, &receipt, source.pinned_identity())?;
            authority.recheck_access_receipt_on(self, &transaction, &plan)?;
            Ok(SettingsAccessReceiptCommit::Fresh { reply })
        })();
        let committed = match result {
            Ok(committed) => committed,
            Err(cause) => return Err(rollback_access_receipt(self, transaction, cause)),
        };
        if let Err(error) = transaction.execute_batch("COMMIT") {
            return Err(rollback_access_receipt(
                self,
                transaction,
                SettingsAccessAdmissionError::Commit(StorageError::from(error)),
            ));
        }
        drop(transaction);
        Ok(committed)
    }
}

#[cfg(unix)]
fn rollback_access_receipt(
    store: &Store,
    mut transaction: rusqlite::Transaction<'_>,
    cause: super::settings_transport::SettingsAccessAdmissionError,
) -> super::settings_transport::SettingsAccessAdmissionError {
    transaction.set_drop_behavior(rusqlite::DropBehavior::Ignore);
    match transaction.rollback() {
        Ok(()) => cause,
        Err(error) => {
            store.quarantine_connection();
            super::settings_transport::SettingsAccessAdmissionError::Rollback {
                cause: Box::new(cause),
                rollback: StorageError::from(error),
            }
        }
    }
}

#[cfg(unix)]
fn access_receipt_duplicate_on(
    connection: &Connection,
    receipt: &TransportInboxMetadata<'_>,
) -> Result<bool, super::settings_transport::SettingsAccessAdmissionError> {
    use super::settings_transport::access_reply_text;

    let mut statement = connection
        .prepare("SELECT sender,recipient,envelope_digest FROM transport_inbox WHERE message_id=?1")
        .map_err(StorageError::from)?;
    let mut rows = statement
        .query([receipt.message_id])
        .map_err(StorageError::from)?;
    let Some(row) = rows.next().map_err(StorageError::from)? else {
        return Ok(false);
    };
    if access_reply_text(row, 0, 36)? != receipt.sender
        || access_reply_text(row, 1, 36)? != receipt.recipient
        || access_reply_text(row, 2, 64)? != receipt.envelope_digest
    {
        return Err(StorageError::TransportConflict(receipt.message_id.into()).into());
    }
    Ok(true)
}

#[cfg(unix)]
fn check_access_marker_on(
    connection: &Connection,
    receipt: &TransportInboxMetadata<'_>,
) -> Result<(), super::settings_transport::SettingsAccessAdmissionError> {
    use super::settings_transport::{SettingsAccessAdmissionError, access_reply_text};

    let mut statement = connection.prepare("SELECT message_id,sender,recipient,envelope_digest,payload,received_at,expires_at,state,command_id,detail FROM transport_inbox WHERE message_id=?1").map_err(StorageError::from)?;
    let mut rows = statement
        .query([receipt.message_id])
        .map_err(StorageError::from)?;
    let row = rows
        .next()
        .map_err(StorageError::from)?
        .ok_or(SettingsAccessAdmissionError::Bookkeeping)?;
    if access_reply_text(row, 0, 36)? != receipt.message_id
        || access_reply_text(row, 1, 36)? != receipt.sender
        || access_reply_text(row, 2, 36)? != receipt.recipient
        || access_reply_text(row, 3, 64)? != receipt.envelope_digest
        || !matches!(
            row.get_ref(4).map_err(StorageError::from)?,
            rusqlite::types::ValueRef::Null
        )
        || row.get::<_, i64>(5).map_err(StorageError::from)? != receipt.received_at
        || row.get::<_, i64>(6).map_err(StorageError::from)? != receipt.expires_at
        || access_reply_text(row, 7, 9)? != "processed"
        || !matches!(
            row.get_ref(8).map_err(StorageError::from)?,
            rusqlite::types::ValueRef::Null
        )
        || !matches!(
            row.get_ref(9).map_err(StorageError::from)?,
            rusqlite::types::ValueRef::Null
        )
    {
        return Err(SettingsAccessAdmissionError::Bookkeeping);
    }
    Ok(())
}
