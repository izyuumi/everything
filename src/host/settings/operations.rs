use rusqlite::{Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::protocol::{CommandId, MessageId, UnixMillis};
use crate::storage::Store;

use super::actions::{NativeActionObservation, NativeActionReference, SettingsActionTarget};
use super::authority::SettingsOperationOrigin;
use super::contract::{
    AgentMutability, SettingKey, SettingsActionReference, SettingsConfirmationReference,
    SettingsFailureReason, SettingsOperationResult, SettingsOperationState, SettingsRequestBinding,
    SettingsUnavailableReason,
};
use super::service::SettingsServiceError;

const MAX_IDENTITIES: i64 = 1024;
const MAX_PENDING: i64 = 64;
const MAX_UPDATES: i64 = 8192;
const MAX_IDENTITY_BYTES: usize = 18 * 1024;
const MAX_UPDATE_BYTES: usize = 2048;
const MAX_NATIVE_REQUEST_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OperationIdentity {
    pub version: u16,
    pub binding: SettingsRequestBinding,
    pub target: SettingsActionTarget,
    pub origin: SettingsOperationOrigin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsOperationPhase {
    AwaitingConfirmation {
        reference: SettingsConfirmationReference,
    },
    AwaitingAction {
        reference: SettingsActionReference,
    },
    InFlight {
        native_request_id: CommandId,
    },
    Applying {
        expected_revision: u64,
        revision: u64,
    },
    Finished {
        result: SettingsOperationResult,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsOperationInspection {
    pub binding: SettingsRequestBinding,
    pub origin: SettingsOperationOrigin,
    pub phase: SettingsOperationPhase,
    pub updated_at: UnixMillis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<SettingsWriteInspection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application: Option<SettingsApplicationState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsApplicationState {
    Applied,
    RestartRequired,
    Unavailable { reason: SettingsUnavailableReason },
    Failed { reason: SettingsFailureReason },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsWriteInspection {
    pub expected_revision: u64,
    pub revision: u64,
    pub changed_keys: Vec<SettingKey>,
    pub restart_required: bool,
    pub resolution: Option<SettingsWriteResolution>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsWriteResolution {
    Applied,
    NotApplied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsFileFingerprint {
    pub revision: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsWriteIntent {
    pub version: u16,
    pub before: SettingsFileFingerprint,
    pub after: SettingsFileFingerprint,
    pub native_request_id: Option<CommandId>,
    pub changed_keys: Vec<SettingKey>,
    pub memory_changed: bool,
    pub selection_changed: bool,
    pub restart_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsWriteRecord {
    pub intent: SettingsWriteIntent,
    pub resolution: Option<SettingsWriteResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PairingCreationMetadata {
    pub pairing_id: MessageId,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    application: Option<SettingsApplicationState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    write: Option<SettingsWriteRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pairing: Option<PairingCreationMetadata>,
    phase: SettingsOperationPhase,
    observation: Option<NativeActionObservation>,
    updated_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeRequest {
    pub operation_id: CommandId,
    pub native_request_id: CommandId,
    pub session_id: Uuid,
    pub reference: NativeActionReference,
}

pub(super) struct OperationRecord {
    pub identity: OperationIdentity,
    update: UpdateRecord,
}

impl OperationRecord {
    pub fn inspection(&self) -> SettingsOperationInspection {
        SettingsOperationInspection {
            binding: self.identity.binding.clone(),
            origin: self.identity.origin,
            phase: self.update.phase.clone(),
            updated_at: self.update.updated_at,
            application: self.update.application.clone(),
            write: self
                .update
                .write
                .as_ref()
                .map(|write| SettingsWriteInspection {
                    expected_revision: write.intent.before.revision,
                    revision: write.intent.after.revision,
                    changed_keys: write.intent.changed_keys.clone(),
                    restart_required: write.intent.restart_required,
                    resolution: write.resolution,
                }),
        }
    }

    pub fn phase(&self) -> &SettingsOperationPhase {
        &self.update.phase
    }

    pub fn pairing(&self) -> Option<&PairingCreationMetadata> {
        self.update.pairing.as_ref()
    }

    pub fn write(&self) -> Option<&SettingsWriteRecord> {
        self.update.write.as_ref()
    }

    pub fn application(&self) -> Option<&SettingsApplicationState> {
        self.update.application.as_ref()
    }

    pub fn admitted(&self) -> bool {
        matches!(
            self.phase(),
            SettingsOperationPhase::InFlight { .. } | SettingsOperationPhase::Applying { .. }
        ) || self.write().is_some_and(|write| write.resolution.is_none())
    }
}

impl OperationIdentity {
    pub fn validate(&self) -> Result<(), SettingsServiceError> {
        if self.version != 1 {
            return Err(SettingsServiceError::CorruptJournal);
        }
        self.binding.validate()?;
        match &self.origin {
            SettingsOperationOrigin::LocalOwner { request_id }
                if *request_id == self.binding.request_id
                    && *request_id.as_uuid() == self.binding.client_session_id
                    && matches!(
                        self.target,
                        SettingsActionTarget::Patch(_) | SettingsActionTarget::OwnerReplacement(_)
                    ) => {}
            SettingsOperationOrigin::NativeUi { session_id }
                if *session_id == self.binding.client_session_id && !session_id.is_nil() => {}
            SettingsOperationOrigin::SettingsAgent {
                session_id,
                native_session_id,
            } if !session_id.is_nil() && *native_session_id == self.binding.client_session_id => {}
            _ => return Err(SettingsServiceError::CorruptJournal),
        }
        self.target.validate()?;
        if self.binding.request_id != self.target.request_id()
            || self.binding.scope != *self.target.scope()
            || self.binding.expected_revision != self.target.expected_revision()
            || self.binding.exact_request_sha256 != request_digest(&self.target)?
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        encode(self, MAX_IDENTITY_BYTES)?;
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct OperationRepository {
    store: Store,
    changes: tokio::sync::watch::Sender<u64>,
}

impl OperationRepository {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            changes: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn notify_sequence(&self, sequence: u64) {
        self.changes.send_if_modified(|last| {
            if sequence > *last {
                *last = sequence;
                true
            } else {
                false
            }
        });
    }

    pub fn get(&self, request_id: CommandId) -> Result<OperationRecord, SettingsServiceError> {
        let connection = self.store.connection()?;
        read(&connection, request_id)?.ok_or(SettingsServiceError::NotFound)
    }

    pub fn prepare(
        &self,
        identity: OperationIdentity,
        phase: SettingsOperationPhase,
        check_current: impl Fn() -> Result<(), SettingsServiceError>,
    ) -> Result<OperationRecord, SettingsServiceError> {
        identity.validate()?;
        let encoded = encode(&identity, MAX_IDENTITY_BYTES)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = read(&transaction, identity.binding.request_id)? {
            if existing.identity != identity {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok(existing);
        }
        let collision: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings_native_requests WHERE request_id=?1)",
            [identity.binding.request_id.to_string()],
            |row| row.get(0),
        )?;
        if collision {
            return Err(SettingsServiceError::Conflict);
        }
        let pending = pending_count(&transaction)?;
        let awaiting = !matches!(phase, SettingsOperationPhase::Finished { .. });
        if matches!(
            phase,
            SettingsOperationPhase::InFlight { .. } | SettingsOperationPhase::Applying { .. }
        ) {
            return Err(SettingsServiceError::Invalid("initial operation phase"));
        }
        prepare_capacity(&transaction, awaiting)?;
        if awaiting && pending >= MAX_PENDING {
            return Err(SettingsServiceError::Capacity);
        }
        check_current()?;
        let now = UnixMillis::now()?;
        if now.get() >= identity.binding.expires_at.get() {
            return Err(SettingsServiceError::Expired);
        }
        transaction.execute(
            "INSERT INTO settings_operations(request_id,identity,created_at,expires_at) VALUES (?1,?2,?3,?4)",
            params![identity.binding.request_id.to_string(), encoded, identity.binding.issued_at.get() as i64, identity.binding.expires_at.get() as i64],
        )?;
        let update = UpdateRecord {
            application: None,
            write: None,
            pairing: None,
            phase,
            observation: None,
            updated_at: now,
        };
        validate_update(&identity, &update)?;
        let sequence = append(&transaction, identity.binding.request_id, &update)?;
        check_current()?;
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok(OperationRecord { identity, update })
    }

    pub fn replay_claim(
        &self,
        expected: &OperationIdentity,
        native: &NativeRequest,
    ) -> Result<Option<SettingsOperationInspection>, SettingsServiceError> {
        let connection = self.store.connection()?;
        let record =
            read(&connection, native.operation_id)?.ok_or(SettingsServiceError::NotFound)?;
        if &record.identity != expected || expected.binding.client_session_id != native.session_id {
            return Err(SettingsServiceError::Conflict);
        }
        match native_request(&connection, native.native_request_id, native.operation_id)? {
            Some(original) if original == *native => Ok(Some(record.inspection())),
            Some(_) => Err(SettingsServiceError::Conflict),
            None => Ok(None),
        }
    }

    pub fn claim(
        &self,
        expected: &OperationIdentity,
        native: &NativeRequest,
        check_current: impl Fn() -> Result<(), SettingsServiceError>,
    ) -> Result<(OperationRecord, bool), SettingsServiceError> {
        let native_json = encode(native, MAX_NATIVE_REQUEST_BYTES)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record =
            read(&transaction, native.operation_id)?.ok_or(SettingsServiceError::NotFound)?;
        if &record.identity != expected || expected.binding.client_session_id != native.session_id {
            return Err(SettingsServiceError::Conflict);
        }
        let existing = native_request(&transaction, native.native_request_id, native.operation_id)?;
        if let Some(existing) = existing {
            if existing != *native {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok((record, false));
        }
        let reference = match &record.update.phase {
            SettingsOperationPhase::AwaitingAction { reference } => reference.id,
            SettingsOperationPhase::AwaitingConfirmation { reference } => reference.id,
            _ => return Err(SettingsServiceError::Conflict),
        };
        if reference != native.reference.id() {
            return Err(SettingsServiceError::Conflict);
        }
        let collision: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM settings_operations WHERE request_id=?1)",
            [native.native_request_id.to_string()],
            |row| row.get(0),
        )?;
        if collision {
            return Err(SettingsServiceError::Conflict);
        }
        identity_capacity(&transaction)?;
        check_current()?;
        let now = UnixMillis::now()?;
        if now.get() >= expected.binding.expires_at.get() {
            return Err(SettingsServiceError::Expired);
        }
        transaction.execute("INSERT INTO settings_native_requests(request_id,operation_id,exact_request) VALUES(?1,?2,?3)", params![native.native_request_id.to_string(),native.operation_id.to_string(),native_json])?;
        let update = UpdateRecord {
            application: None,
            write: None,
            pairing: None,
            phase: SettingsOperationPhase::InFlight {
                native_request_id: native.native_request_id,
            },
            observation: None,
            updated_at: now,
        };
        validate_update(expected, &update)?;
        let sequence = append(&transaction, native.operation_id, &update)?;
        check_current()?;
        if UnixMillis::now()?.get() >= expected.binding.expires_at.get() {
            return Err(SettingsServiceError::Expired);
        }
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok((
            OperationRecord {
                identity: record.identity,
                update,
            },
            true,
        ))
    }

    pub fn finish(
        &self,
        expected: &OperationIdentity,
        native_request_id: CommandId,
        result: SettingsOperationResult,
        observation: Option<NativeActionObservation>,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read(&transaction, expected.binding.request_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if &record.identity != expected {
            return Err(SettingsServiceError::Conflict);
        }
        if let SettingsOperationPhase::Finished { result: previous } = &record.update.phase {
            if previous != &result || record.update.observation != observation {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok(record.inspection());
        }
        if record.update.phase != (SettingsOperationPhase::InFlight { native_request_id }) {
            return Err(SettingsServiceError::Conflict);
        }
        let update = UpdateRecord {
            application: None,
            write: None,
            pairing: None,
            phase: SettingsOperationPhase::Finished { result },
            observation,
            updated_at: UnixMillis::now()?,
        };
        validate_update(expected, &update)?;
        let sequence = append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok(OperationRecord {
            identity: record.identity,
            update,
        }
        .inspection())
    }

    pub fn finish_pairing(
        &self,
        expected: &OperationIdentity,
        native_request_id: CommandId,
        metadata: PairingCreationMetadata,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read(&transaction, expected.binding.request_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if record.identity != *expected {
            return Err(SettingsServiceError::Conflict);
        }
        let native = native_request(&transaction, native_request_id, expected.binding.request_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if native.operation_id != expected.binding.request_id
            || native.native_request_id != native_request_id
            || native.session_id != expected.binding.client_session_id
        {
            return Err(SettingsServiceError::Conflict);
        }
        if matches!(record.phase(), SettingsOperationPhase::Finished { .. }) {
            if record.update.pairing.as_ref() != Some(&metadata) {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok(record.inspection());
        }
        if *record.phase() != (SettingsOperationPhase::InFlight { native_request_id }) {
            return Err(SettingsServiceError::Conflict);
        }
        let update = UpdateRecord {
            application: None,
            write: None,
            pairing: Some(metadata),
            observation: None,
            updated_at: UnixMillis::now()?,
            phase: SettingsOperationPhase::Finished {
                result: SettingsOperationResult {
                    contract_version: super::contract::SETTINGS_CONTRACT_VERSION,
                    request_id: expected.binding.request_id,
                    scope: expected.binding.scope.clone(),
                    state: SettingsOperationState::Completed {
                        receipt_id: Uuid::new_v4(),
                    },
                },
            },
        };
        validate_update(expected, &update)?;
        let sequence = append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok(OperationRecord {
            identity: record.identity,
            update,
        }
        .inspection())
    }

    pub fn finish_unadmitted(
        &self,
        expected: &OperationIdentity,
        result: SettingsOperationResult,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read(&transaction, expected.binding.request_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if &record.identity != expected {
            return Err(SettingsServiceError::Conflict);
        }
        if matches!(
            record.update.phase,
            SettingsOperationPhase::InFlight { .. }
                | SettingsOperationPhase::Applying { .. }
                | SettingsOperationPhase::Finished { .. }
        ) {
            return Ok(record.inspection());
        }
        let update = UpdateRecord {
            application: None,
            write: None,
            pairing: None,
            phase: SettingsOperationPhase::Finished { result },
            observation: None,
            updated_at: UnixMillis::now()?,
        };
        validate_update(expected, &update)?;
        let sequence = append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok(OperationRecord {
            identity: record.identity,
            update,
        }
        .inspection())
    }

    pub fn replay_observation(
        &self,
        native: &NativeRequest,
        observation: &NativeActionObservation,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let connection = self.store.connection()?;
        let original = native_request(&connection, native.native_request_id, native.operation_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if &original != native {
            return Err(SettingsServiceError::Conflict);
        }
        let record =
            read(&connection, native.operation_id)?.ok_or(SettingsServiceError::NotFound)?;
        if record.identity.binding.client_session_id != native.session_id
            || !matches!(record.phase(), SettingsOperationPhase::Finished { .. })
            || record.update.observation.as_ref() != Some(observation)
        {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(record.inspection())
    }

    pub fn pending(&self) -> Result<Vec<OperationRecord>, SettingsServiceError> {
        let connection = self.store.connection()?;
        let mut statement = connection.prepare("SELECT request_id FROM settings_operations o WHERE json_extract((SELECT record FROM settings_operation_updates u WHERE u.request_id=o.request_id ORDER BY sequence DESC LIMIT 1),'$.phase.phase') != 'finished' ORDER BY request_id LIMIT 65")?;
        let mut rows = statement.query([])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            if records.len() == MAX_PENDING as usize {
                return Err(SettingsServiceError::CorruptJournal);
            }
            let id = bounded_text(row, 0, 36)?
                .parse()
                .map_err(|_| SettingsServiceError::CorruptJournal)?;
            records.push(read(&connection, id)?.ok_or(SettingsServiceError::CorruptJournal)?);
        }
        Ok(records)
    }

    pub fn begin_write(
        &self,
        identity: OperationIdentity,
        intent: SettingsWriteIntent,
        check_current: impl Fn() -> Result<(), SettingsServiceError>,
    ) -> Result<(OperationRecord, bool), SettingsServiceError> {
        identity.validate()?;
        intent.validate(&identity)?;
        let encoded = encode(&identity, MAX_IDENTITY_BYTES)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = read(&transaction, identity.binding.request_id)?;
        let had_existing = existing.is_some();
        if let Some(existing) = existing {
            if existing.identity != identity {
                return Err(SettingsServiceError::Conflict);
            }
            if let Some(write) = existing.write() {
                if write.intent != intent {
                    return Err(SettingsServiceError::Conflict);
                }
                return Ok((existing, false));
            }
            if !matches!(existing.phase(), SettingsOperationPhase::InFlight { native_request_id } if Some(*native_request_id)==intent.native_request_id)
            {
                return Err(SettingsServiceError::Conflict);
            }
        } else if intent.native_request_id.is_some() {
            return Err(SettingsServiceError::NotFound);
        }
        if let Some(unresolved) = unresolved_write(&transaction)? {
            return Err(SettingsServiceError::WriteBlocked {
                request_id: unresolved.identity.binding.request_id,
            });
        }
        if !had_existing {
            let collision: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM settings_native_requests WHERE request_id=?1)",
                [identity.binding.request_id.to_string()],
                |row| row.get(0),
            )?;
            if collision {
                return Err(SettingsServiceError::Conflict);
            }
            if pending_count(&transaction)? >= MAX_PENDING {
                return Err(SettingsServiceError::Capacity);
            }
        }
        reserve_capacity(
            &transaction,
            !had_existing,
            false,
            3,
            Some(identity.binding.request_id),
        )?;
        check_current()?;
        let now = UnixMillis::now()?;
        if now.get() >= identity.binding.expires_at.get() {
            return Err(SettingsServiceError::Expired);
        }
        if !had_existing {
            transaction.execute(
                "INSERT INTO settings_operations(request_id,identity,created_at,expires_at) VALUES (?1,?2,?3,?4)",
                params![identity.binding.request_id.to_string(),encoded,identity.binding.issued_at.get() as i64,identity.binding.expires_at.get() as i64],
            )?;
        }
        let update = UpdateRecord {
            application: None,
            write: Some(SettingsWriteRecord {
                intent,
                resolution: None,
            }),
            pairing: None,
            phase: SettingsOperationPhase::Applying {
                expected_revision: identity
                    .binding
                    .expected_revision
                    .ok_or(SettingsServiceError::CorruptJournal)?,
                revision: identity
                    .binding
                    .expected_revision
                    .ok_or(SettingsServiceError::CorruptJournal)?
                    + 1,
            },
            observation: None,
            updated_at: now,
        };
        validate_update(&identity, &update)?;
        let sequence = append(&transaction, identity.binding.request_id, &update)?;
        check_current()?;
        if UnixMillis::now()?.get() >= identity.binding.expires_at.get() {
            return Err(SettingsServiceError::Expired);
        }
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok((OperationRecord { identity, update }, true))
    }

    pub fn finish_write(
        &self,
        expected: &OperationIdentity,
        intent: &SettingsWriteIntent,
        resolution: Option<SettingsWriteResolution>,
        result: SettingsOperationResult,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read(&transaction, expected.binding.request_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if record.identity != *expected {
            return Err(SettingsServiceError::Conflict);
        }
        let write = record.write().ok_or(SettingsServiceError::Conflict)?;
        if write.intent != *intent {
            return Err(SettingsServiceError::Conflict);
        }
        if let SettingsOperationPhase::Finished { result: previous } = record.phase() {
            if write.resolution == resolution && previous == &result {
                return Ok(record.inspection());
            }
            if write.resolution.is_some() {
                return Err(SettingsServiceError::Conflict);
            }
        }
        let update = UpdateRecord {
            application: None,
            write: Some(SettingsWriteRecord {
                intent: intent.clone(),
                resolution,
            }),
            pairing: None,
            phase: SettingsOperationPhase::Finished { result },
            observation: None,
            updated_at: UnixMillis::now()?,
        };
        validate_update(expected, &update)?;
        let sequence = append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok(OperationRecord {
            identity: record.identity,
            update,
        }
        .inspection())
    }

    pub fn record_application(
        &self,
        expected: &OperationIdentity,
        intent: &SettingsWriteIntent,
        application: SettingsApplicationState,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read(&transaction, expected.binding.request_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        if record.identity != *expected
            || !matches!(
                record.phase(),
                SettingsOperationPhase::Finished {
                    result: SettingsOperationResult {
                        state: SettingsOperationState::PendingApplication { .. },
                        ..
                    }
                }
            )
            || record.write().is_none_or(|write| {
                write.intent != *intent
                    || write.resolution != Some(SettingsWriteResolution::Applied)
            })
        {
            return Err(SettingsServiceError::Conflict);
        }
        if let Some(previous) = &record.update.application {
            if previous != &application {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok(record.inspection());
        }
        let update = UpdateRecord {
            application: Some(application),
            write: record.update.write,
            pairing: None,
            phase: record.update.phase,
            observation: None,
            updated_at: UnixMillis::now()?,
        };
        validate_update(expected, &update)?;
        let sequence = append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
        drop(connection);
        self.notify_sequence(sequence);
        Ok(OperationRecord {
            identity: record.identity,
            update,
        }
        .inspection())
    }

    pub fn change_records(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, OperationRecord)>, SettingsServiceError> {
        if after > i64::MAX as u64 || !(1..=128).contains(&limit) {
            return Err(SettingsServiceError::Invalid("change cursor or count"));
        }
        let connection = self.store.connection()?;
        let mut statement = connection.prepare("SELECT sequence,request_id,record,occurred_at FROM settings_operation_updates WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
        let mut rows = statement.query(params![after as i64, limit as i64])?;
        let mut changes = Vec::with_capacity(limit);
        while let Some(row) = rows.next()? {
            let sequence = u64::try_from(row.get::<_, i64>(0)?)
                .map_err(|_| SettingsServiceError::CorruptJournal)?;
            if sequence <= after {
                return Err(SettingsServiceError::CorruptJournal);
            }
            let id = bounded_text(row, 1, 36)?.parse()?;
            let current = read(&connection, id)?.ok_or(SettingsServiceError::CorruptJournal)?;
            let update: UpdateRecord =
                decode(bounded_text(row, 2, MAX_UPDATE_BYTES)?, MAX_UPDATE_BYTES)?;
            validate_update(&current.identity, &update)?;
            if update.updated_at.get() as i64 != row.get::<_, i64>(3)?
                || update.write.as_ref().is_some_and(|write| {
                    current
                        .write()
                        .is_none_or(|latest| latest.intent != write.intent)
                })
            {
                return Err(SettingsServiceError::CorruptJournal);
            }
            validate_native_link(&connection, &current.identity, &update)?;
            changes.push((
                sequence,
                OperationRecord {
                    identity: current.identity,
                    update,
                },
            ));
        }
        Ok(changes)
    }

    pub fn pending_applications(&self) -> Result<Vec<OperationRecord>, SettingsServiceError> {
        let connection = self.store.connection()?;
        Ok(write_history(&connection)?
            .into_iter()
            .filter(|record| {
                record.application().is_none()
                    && matches!(
                        record.phase(),
                        SettingsOperationPhase::Finished {
                            result: SettingsOperationResult {
                                state: SettingsOperationState::PendingApplication { .. },
                                ..
                            }
                        }
                    )
            })
            .collect())
    }

    pub fn unresolved_write(&self) -> Result<Option<OperationRecord>, SettingsServiceError> {
        let connection = self.store.connection()?;
        unresolved_write(&connection)
    }
}

fn read(
    connection: &rusqlite::Connection,
    request_id: CommandId,
) -> Result<Option<OperationRecord>, SettingsServiceError> {
    let mut statement = connection.prepare(
        "SELECT identity,created_at,expires_at FROM settings_operations WHERE request_id=?1",
    )?;
    let mut rows = statement.query([request_id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let identity: OperationIdentity = decode(
        bounded_text(row, 0, MAX_IDENTITY_BYTES)?,
        MAX_IDENTITY_BYTES,
    )?;
    identity.validate()?;
    if identity.binding.request_id != request_id
        || identity.binding.issued_at.get() as i64 != row.get::<_, i64>(1)?
        || identity.binding.expires_at.get() as i64 != row.get::<_, i64>(2)?
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    drop(rows);
    drop(statement);
    let mut statement=connection.prepare("SELECT record,occurred_at FROM settings_operation_updates WHERE request_id=?1 ORDER BY sequence DESC LIMIT 1")?;
    let mut rows = statement.query([request_id.to_string()])?;
    let row = rows.next()?.ok_or(SettingsServiceError::CorruptJournal)?;
    let record: UpdateRecord = decode(bounded_text(row, 0, MAX_UPDATE_BYTES)?, MAX_UPDATE_BYTES)?;
    validate_update(&identity, &record)?;
    if record.updated_at.get() as i64 != row.get::<_, i64>(1)? {
        return Err(SettingsServiceError::CorruptJournal);
    }
    validate_native_link(connection, &identity, &record)?;
    let original: Option<String> = {
        let mut statement = connection.prepare("SELECT record FROM settings_operation_updates WHERE request_id=?1 AND json_type(record,'$.write')='object' ORDER BY sequence LIMIT 1")?;
        let mut rows = statement.query([request_id.to_string()])?;
        rows.next()?
            .map(|row| bounded_text(row, 0, MAX_UPDATE_BYTES).map(str::to_owned))
            .transpose()?
    };
    if let Some(original) = original {
        let original: UpdateRecord = decode(&original, MAX_UPDATE_BYTES)?;
        validate_update(&identity, &original)?;
        if original.write.as_ref().map(|write| &write.intent)
            != record.write.as_ref().map(|write| &write.intent)
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
    }
    for (predicate, application) in [
        ("json_type(record,'$.write.resolution')='text'", false),
        ("json_type(record,'$.application')='object'", true),
    ] {
        let original: Option<String> = {
            let query = format!(
                "SELECT record FROM settings_operation_updates WHERE request_id=?1 AND {predicate} ORDER BY sequence LIMIT 1"
            );
            let mut statement = connection.prepare(&query)?;
            let mut rows = statement.query([request_id.to_string()])?;
            rows.next()?
                .map(|row| bounded_text(row, 0, MAX_UPDATE_BYTES).map(str::to_owned))
                .transpose()?
        };
        if let Some(original) = original {
            let original: UpdateRecord = decode(&original, MAX_UPDATE_BYTES)?;
            validate_update(&identity, &original)?;
            if if application {
                original.application != record.application
            } else {
                original.phase != record.phase
                    || original.write.as_ref().map(|write| write.resolution)
                        != record.write.as_ref().map(|write| write.resolution)
            } {
                return Err(SettingsServiceError::CorruptJournal);
            }
        }
    }
    Ok(Some(OperationRecord {
        identity,
        update: record,
    }))
}

fn validate_native_link(
    connection: &rusqlite::Connection,
    identity: &OperationIdentity,
    record: &UpdateRecord,
) -> Result<(), SettingsServiceError> {
    let native_request_id = match record.phase {
        SettingsOperationPhase::InFlight { native_request_id } => Some(native_request_id),
        _ => record
            .write
            .as_ref()
            .and_then(|write| write.intent.native_request_id),
    };
    if let Some(native_request_id) = native_request_id {
        let native = native_request(connection, native_request_id, identity.binding.request_id)?
            .ok_or(SettingsServiceError::CorruptJournal)?;
        if native.operation_id != identity.binding.request_id
            || native.native_request_id != native_request_id
            || native.session_id != identity.binding.client_session_id
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
    }
    Ok(())
}

fn native_request(
    connection: &rusqlite::Connection,
    id: CommandId,
    operation: CommandId,
) -> Result<Option<NativeRequest>, SettingsServiceError> {
    let mut statement = connection.prepare("SELECT request_id,operation_id,exact_request FROM settings_native_requests WHERE request_id=?1 OR operation_id=?2 LIMIT 2")?;
    let mut rows = statement.query(params![id.to_string(), operation.to_string()])?;
    let mut found = None;
    while let Some(row) = rows.next()? {
        if found.is_some() {
            return Err(SettingsServiceError::Conflict);
        }
        let request: NativeRequest = decode(
            bounded_text(row, 2, MAX_NATIVE_REQUEST_BYTES)?,
            MAX_NATIVE_REQUEST_BYTES,
        )?;
        if request.session_id.is_nil()
            || request.reference.id().is_nil()
            || bounded_text(row, 0, 36)? != request.native_request_id.to_string()
            || bounded_text(row, 1, 36)? != request.operation_id.to_string()
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        found = Some(request);
    }
    Ok(found)
}

fn prepare_capacity(
    transaction: &Transaction<'_>,
    awaiting: bool,
) -> Result<(), SettingsServiceError> {
    reserve_capacity(
        transaction,
        true,
        awaiting,
        if awaiting { 5 } else { 0 },
        None,
    )
}

fn reserve_capacity(
    transaction: &Transaction<'_>,
    new_identity: bool,
    future_native: bool,
    future_updates: i64,
    replacing: Option<CommandId>,
) -> Result<(), SettingsServiceError> {
    let (identities, updates): (i64, i64) = transaction.query_row(
        "SELECT (SELECT count(*) FROM settings_operations)+(SELECT count(*) FROM settings_native_requests),(SELECT count(*) FROM settings_operation_updates)",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (native_reserve, update_reserve): (i64, i64) = transaction.query_row(
        "WITH current AS (SELECT (SELECT record FROM settings_operation_updates u WHERE u.request_id=o.request_id ORDER BY sequence DESC LIMIT 1) AS record FROM settings_operations o WHERE ?1 IS NULL OR o.request_id != ?1) SELECT coalesce(sum(CASE WHEN json_extract(record,'$.phase.phase') IN ('awaiting_action','awaiting_confirmation') THEN 1 ELSE 0 END),0),coalesce(sum(CASE WHEN json_extract(record,'$.phase.phase') IN ('awaiting_action','awaiting_confirmation') THEN 5 WHEN json_extract(record,'$.phase.phase')='in_flight' THEN 4 WHEN json_extract(record,'$.phase.phase')='applying' THEN 3 WHEN json_type(record,'$.write')='object' AND json_type(record,'$.write.resolution')='null' THEN 2 WHEN json_extract(record,'$.phase.result.state.kind')='pending_application' AND json_type(record,'$.application') IS NULL THEN 1 ELSE 0 END),0) FROM current",
        [replacing.map(|id| id.to_string())], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if identities + native_reserve + i64::from(new_identity) + i64::from(future_native)
        > MAX_IDENTITIES
        || updates + update_reserve + 1 + future_updates > MAX_UPDATES
    {
        return Err(SettingsServiceError::Capacity);
    }
    Ok(())
}

fn pending_count(connection: &rusqlite::Connection) -> Result<i64, SettingsServiceError> {
    Ok(connection.query_row(
        "WITH current AS (SELECT (SELECT record FROM settings_operation_updates u WHERE u.request_id=o.request_id ORDER BY sequence DESC LIMIT 1) AS record FROM settings_operations o) SELECT count(*) FROM current WHERE json_extract(record,'$.phase.phase') != 'finished' OR (json_type(record,'$.write')='object' AND json_type(record,'$.write.resolution')='null')",
        [], |row| row.get(0),
    )?)
}

fn write_history(
    connection: &rusqlite::Connection,
) -> Result<Vec<OperationRecord>, SettingsServiceError> {
    let mut statement = connection.prepare("SELECT o.request_id FROM settings_operations o WHERE EXISTS(SELECT 1 FROM settings_operation_updates h WHERE h.request_id=o.request_id AND json_type(h.record,'$.write')='object') ORDER BY o.request_id LIMIT 1025")?;
    let mut rows = statement.query([])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        if records.len() == MAX_IDENTITIES as usize {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let request_id = bounded_text(row, 0, 36)?.parse()?;
        let record = read(connection, request_id)?.ok_or(SettingsServiceError::CorruptJournal)?;
        record.write().ok_or(SettingsServiceError::CorruptJournal)?;
        records.push(record);
    }
    Ok(records)
}

fn unresolved_write(
    connection: &rusqlite::Connection,
) -> Result<Option<OperationRecord>, SettingsServiceError> {
    let mut found = None;
    for record in write_history(connection)? {
        if record
            .write()
            .is_some_and(|write| write.resolution.is_none())
        {
            if found.is_some() {
                return Err(SettingsServiceError::CorruptJournal);
            }
            found = Some(record);
        }
    }
    Ok(found)
}

fn identity_capacity(transaction: &Transaction<'_>) -> Result<(), SettingsServiceError> {
    let count: i64 = transaction.query_row("SELECT (SELECT count(*) FROM settings_operations)+(SELECT count(*) FROM settings_native_requests)",[],|row|row.get(0))?;
    if count >= MAX_IDENTITIES {
        return Err(SettingsServiceError::Capacity);
    }
    Ok(())
}

fn append(
    transaction: &Transaction<'_>,
    request_id: CommandId,
    record: &UpdateRecord,
) -> Result<u64, SettingsServiceError> {
    let count: i64 = transaction.query_row(
        "SELECT count(*) FROM settings_operation_updates",
        [],
        |row| row.get(0),
    )?;
    if count >= MAX_UPDATES {
        return Err(SettingsServiceError::Capacity);
    }
    let sequence: i64 = transaction.query_row(
        "INSERT INTO settings_operation_updates(request_id,record,occurred_at) VALUES(?1,?2,?3) RETURNING sequence",
        params![
            request_id.to_string(),
            encode(record, MAX_UPDATE_BYTES)?,
            record.updated_at.get() as i64
        ],
        |row| row.get(0),
    )?;
    u64::try_from(sequence).map_err(|_| SettingsServiceError::CorruptJournal)
}

fn validate_update(
    identity: &OperationIdentity,
    update: &UpdateRecord,
) -> Result<(), SettingsServiceError> {
    if update.updated_at.get() < identity.binding.issued_at.get() {
        return Err(SettingsServiceError::CorruptJournal);
    }
    match &update.phase {
        SettingsOperationPhase::AwaitingAction { reference } => {
            if reference.id.is_nil()
                || reference.expires_at != identity.binding.expires_at
                || matches!(identity.target, SettingsActionTarget::Patch(_))
            {
                return Err(SettingsServiceError::CorruptJournal);
            }
        }
        SettingsOperationPhase::AwaitingConfirmation { reference } => {
            if reference.id.is_nil()
                || reference.expires_at != identity.binding.expires_at
                || !matches!(identity.target, SettingsActionTarget::Patch(_))
            {
                return Err(SettingsServiceError::CorruptJournal);
            }
        }
        SettingsOperationPhase::InFlight { .. } => {}
        SettingsOperationPhase::Applying {
            expected_revision,
            revision,
        } => {
            let write = update
                .write
                .as_ref()
                .ok_or(SettingsServiceError::CorruptJournal)?;
            if write.resolution.is_some()
                || *expected_revision != write.intent.before.revision
                || *revision != write.intent.after.revision
            {
                return Err(SettingsServiceError::CorruptJournal);
            }
        }
        SettingsOperationPhase::Finished { result } => {
            result.validate()?;
            if result.request_id != identity.binding.request_id
                || result.scope != identity.binding.scope
                || matches!(
                    result.state,
                    SettingsOperationState::RequiresConfirmation { .. }
                        | SettingsOperationState::RequiresUserAction { .. }
                )
            {
                return Err(SettingsServiceError::CorruptJournal);
            }
        }
    }
    if let Some(write) = &update.write {
        write.intent.validate(identity)?;
        if update.observation.is_some() || update.pairing.is_some() {
            return Err(SettingsServiceError::CorruptJournal);
        }
        match (&update.phase, write.resolution) {
            (SettingsOperationPhase::Applying { .. }, None) => {}
            (SettingsOperationPhase::Finished { result }, None)
                if result.state == SettingsOperationState::Unknown => {}
            (
                SettingsOperationPhase::Finished { result },
                Some(SettingsWriteResolution::Applied),
            ) if matches!(result.state, SettingsOperationState::Applied { revision } | SettingsOperationState::PendingApplication { revision } | SettingsOperationState::RestartRequired { revision } if revision == write.intent.after.revision) =>
                {}
            (
                SettingsOperationPhase::Finished { result },
                Some(SettingsWriteResolution::NotApplied),
            ) if matches!(
                result.state,
                SettingsOperationState::Failed { .. }
                    | SettingsOperationState::Unavailable { .. }
                    | SettingsOperationState::Cancelled
                    | SettingsOperationState::Conflict { .. }
            ) => {}
            _ => return Err(SettingsServiceError::CorruptJournal),
        }
    }
    if update.application.is_some()
        && (!matches!(
            update.phase,
            SettingsOperationPhase::Finished {
                result: SettingsOperationResult {
                    state: SettingsOperationState::PendingApplication { .. },
                    ..
                }
            }
        ) || update
            .write
            .as_ref()
            .is_none_or(|write| write.resolution != Some(SettingsWriteResolution::Applied)))
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    if update.observation.is_some()
        && !matches!(update.phase, SettingsOperationPhase::Finished { .. })
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    if let Some(pairing) = &update.pairing {
        if !matches!(
            identity.target,
            SettingsActionTarget::Device(super::contract::SettingsDeviceRequest {
                action: super::contract::SettingsDeviceAction::Pair,
                ..
            })
        ) || !matches!(
            update.phase,
            SettingsOperationPhase::Finished {
                result: SettingsOperationResult {
                    state: SettingsOperationState::Completed { .. },
                    ..
                }
            }
        ) || update.observation.is_some()
            || pairing.expires_at.get() <= identity.binding.issued_at.get()
            || pairing.expires_at.get() > identity.binding.expires_at.get()
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
    }
    encode(update, MAX_UPDATE_BYTES)?;
    Ok(())
}

fn bounded_text<'row>(
    row: &'row rusqlite::Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, SettingsServiceError> {
    let value = row
        .get_ref(column)?
        .as_str()
        .map_err(|_| SettingsServiceError::CorruptJournal)?;
    if value.len() > maximum {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(value)
}

pub(super) fn request_digest(value: &impl Serialize) -> Result<String, SettingsServiceError> {
    let bytes = encode(value, MAX_IDENTITY_BYTES)?;
    let mut digest = Sha256::new();
    digest.update(b"everything.settings.request.v1\0");
    digest.update(bytes.as_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

fn encode(value: &impl Serialize, maximum: usize) -> Result<String, SettingsServiceError> {
    let value = serde_json::to_string(value).map_err(|_| SettingsServiceError::Encoding)?;
    if value.len() > maximum {
        return Err(SettingsServiceError::Capacity);
    }
    Ok(value)
}

fn decode<T: serde::de::DeserializeOwned + Serialize>(
    value: &str,
    maximum: usize,
) -> Result<T, SettingsServiceError> {
    if value.len() > maximum {
        return Err(SettingsServiceError::CorruptJournal);
    }
    let decoded: T =
        serde_json::from_str(value).map_err(|_| SettingsServiceError::CorruptJournal)?;
    if encode(&decoded, maximum)? != value {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(decoded)
}

impl SettingsWriteIntent {
    pub fn validate(&self, identity: &OperationIdentity) -> Result<(), SettingsServiceError> {
        validate_digest(&self.before.sha256)?;
        validate_digest(&self.after.sha256)?;
        if self.version != 1
            || self.before.revision == 0
            || self.before.revision >= i64::MAX as u64
            || self.after.revision != self.before.revision + 1
            || identity.binding.expected_revision != Some(self.before.revision)
            || !matches!(
                identity.binding.scope,
                super::contract::SettingsScope::Host { .. }
            )
            || self.changed_keys.len() > SettingKey::ALL.len()
            || !self.changed_keys.windows(2).all(|keys| keys[0] < keys[1])
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        match &identity.target {
            SettingsActionTarget::Patch(patch) => {
                let mut keys = patch
                    .changes
                    .iter()
                    .map(|change| change.key)
                    .collect::<Vec<_>>();
                keys.sort_unstable();
                if keys != self.changed_keys
                    || (keys.iter().any(|key| {
                        key.agent_mutability() == AgentMutability::RequiresNativeConfirmation
                    }) && self.native_request_id.is_none())
                {
                    return Err(SettingsServiceError::CorruptJournal);
                }
            }
            SettingsActionTarget::OwnerReplacement(request)
                if matches!(
                    identity.origin,
                    super::authority::SettingsOperationOrigin::LocalOwner { .. }
                ) && self.native_request_id.is_none()
                    && request.changed_keys == self.changed_keys
                    && !self.changed_keys.iter().any(|key| {
                        key.agent_mutability() == AgentMutability::RequiresNativeConfirmation
                    })
                    && request.restart_required == self.restart_required => {}
            _ => return Err(SettingsServiceError::CorruptJournal),
        }
        Ok(())
    }
}

pub(super) fn validate_digest(value: &str) -> Result<(), SettingsServiceError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}
