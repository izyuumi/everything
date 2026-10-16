use rusqlite::{Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::protocol::{CommandId, MessageId, UnixMillis};
use crate::storage::Store;

use super::actions::{NativeActionObservation, NativeActionReference, SettingsActionTarget};
use super::authority::SettingsOperationOrigin;
use super::contract::{
    SettingsActionReference, SettingsConfirmationReference, SettingsOperationResult,
    SettingsOperationState, SettingsRequestBinding,
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
        }
    }

    pub fn phase(&self) -> &SettingsOperationPhase {
        &self.update.phase
    }

    pub fn pairing(&self) -> Option<&PairingCreationMetadata> {
        self.update.pairing.as_ref()
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
                    && matches!(self.target, SettingsActionTarget::Patch(_)) => {}
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
}

impl OperationRepository {
    pub fn new(store: Store) -> Self {
        Self { store }
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
        let pending: i64 = transaction.query_row(
            "SELECT count(*) FROM settings_operations o WHERE json_extract((SELECT record FROM settings_operation_updates u WHERE u.request_id=o.request_id ORDER BY sequence DESC LIMIT 1),'$.phase.phase') != 'finished'",
            [], |row| row.get(0),
        )?;
        let awaiting = !matches!(phase, SettingsOperationPhase::Finished { .. });
        if matches!(phase, SettingsOperationPhase::InFlight { .. }) {
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
            pairing: None,
            phase,
            observation: None,
            updated_at: now,
        };
        validate_update(&identity, &update)?;
        append(&transaction, identity.binding.request_id, &update)?;
        check_current()?;
        transaction.commit()?;
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
            pairing: None,
            phase: SettingsOperationPhase::InFlight {
                native_request_id: native.native_request_id,
            },
            observation: None,
            updated_at: now,
        };
        validate_update(expected, &update)?;
        append(&transaction, native.operation_id, &update)?;
        check_current()?;
        if UnixMillis::now()?.get() >= expected.binding.expires_at.get() {
            return Err(SettingsServiceError::Expired);
        }
        transaction.commit()?;
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
            pairing: None,
            phase: SettingsOperationPhase::Finished { result },
            observation,
            updated_at: UnixMillis::now()?,
        };
        validate_update(expected, &update)?;
        append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
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
        append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
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
            SettingsOperationPhase::InFlight { .. } | SettingsOperationPhase::Finished { .. }
        ) {
            return Ok(record.inspection());
        }
        let update = UpdateRecord {
            pairing: None,
            phase: SettingsOperationPhase::Finished { result },
            observation: None,
            updated_at: UnixMillis::now()?,
        };
        validate_update(expected, &update)?;
        append(&transaction, expected.binding.request_id, &update)?;
        transaction.commit()?;
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
    if let SettingsOperationPhase::InFlight { native_request_id } = record.phase {
        let native = native_request(connection, native_request_id, request_id)?
            .ok_or(SettingsServiceError::CorruptJournal)?;
        if native.operation_id != request_id
            || native.native_request_id != native_request_id
            || native.session_id != identity.binding.client_session_id
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
    }
    Ok(Some(OperationRecord {
        identity,
        update: record,
    }))
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
    let (identities,updates):(i64,i64)=transaction.query_row("SELECT (SELECT count(*) FROM settings_operations)+(SELECT count(*) FROM settings_native_requests),(SELECT count(*) FROM settings_operation_updates)",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    let (native_reserve,update_reserve):(i64,i64)=transaction.query_row("WITH phases AS (SELECT json_extract((SELECT record FROM settings_operation_updates u WHERE u.request_id=o.request_id ORDER BY sequence DESC LIMIT 1),'$.phase.phase') AS phase FROM settings_operations o) SELECT coalesce(sum(CASE WHEN phase IN ('awaiting_action','awaiting_confirmation') THEN 1 ELSE 0 END),0), coalesce(sum(CASE WHEN phase IN ('awaiting_action','awaiting_confirmation') THEN 2 WHEN phase='in_flight' THEN 1 ELSE 0 END),0) FROM phases",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
    let extra_id = if awaiting { 2 } else { 1 };
    let extra_updates = if awaiting { 3 } else { 1 };
    if identities + native_reserve + extra_id > MAX_IDENTITIES
        || updates + update_reserve + extra_updates > MAX_UPDATES
    {
        return Err(SettingsServiceError::Capacity);
    }
    Ok(())
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
) -> Result<(), SettingsServiceError> {
    let count: i64 = transaction.query_row(
        "SELECT count(*) FROM settings_operation_updates",
        [],
        |row| row.get(0),
    )?;
    if count >= MAX_UPDATES {
        return Err(SettingsServiceError::Capacity);
    }
    transaction.execute(
        "INSERT INTO settings_operation_updates(request_id,record,occurred_at) VALUES(?1,?2,?3)",
        params![
            request_id.to_string(),
            encode(record, MAX_UPDATE_BYTES)?,
            record.updated_at.get() as i64
        ],
    )?;
    Ok(())
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
