use rusqlite::{Connection, Row, TransactionBehavior, params, types::ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{StorageError, StorageResult, Store, invalid, missing, validate_limit};
use uuid::Uuid;

use crate::protocol::{CommandId, ConversationId, DeviceId, TaskId, UnixMillis};

pub(crate) const MAX_SETTINGS_SESSIONS: usize = 2;
pub(crate) const MAX_SETTINGS_TURNS: usize = 8;
pub(crate) const MAX_SETTINGS_MUTATIONS: usize = 8;
pub(crate) const MAX_SETTINGS_PAGES: u64 = 8192;
pub(crate) const SETTINGS_PAGE_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsSessionReservation {
    pub version: u16,
    pub session_id: Uuid,
    pub host_id: DeviceId,
    pub principal_id: Uuid,
    pub native_session_id: Uuid,
    pub conversation_id: ConversationId,
    pub create_command_id: CommandId,
    pub provider_binding_sha256: String,
    pub tools_sha256: String,
    pub created_at: UnixMillis,
    pub expires_at: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsFencePhase {
    Armed,
    Unknown,
    Settled,
    StoppedUncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsTurnFence {
    pub version: u16,
    pub session_id: Uuid,
    pub command_id: CommandId,
    pub task_id: TaskId,
    pub phase: SettingsFencePhase,
    pub attempt_id: Option<CommandId>,
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reserved_output_tokens: u64,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
    pub started_at: UnixMillis,
    pub deadline: UnixMillis,
    pub last_observed_wall: UnixMillis,
    pub mutation_ids: Vec<CommandId>,
    pub provider_complete: bool,
    pub usage_complete: bool,
}

const MAX_RETAINED_SETTINGS_SESSIONS: usize = 1024;
const MAX_SETTINGS_RECORD_BYTES: usize = 4096;
const MAX_SETTINGS_CLEANUP_BYTES: usize = 512;
const SETTINGS_MEMORY_MARGIN_PAGES: u64 = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsCleanupAttestation {
    pub version: u16,
    pub command_id: CommandId,
    pub exact_fence_sha256: String,
    pub owner_principal_id: Uuid,
    pub attested_at: UnixMillis,
    pub lost_ram_uncertainty_accepted: bool,
}

impl SettingsTurnFence {
    pub(crate) fn sha256(&self) -> StorageResult<String> {
        validate_fence(self)?;
        let mut hash = Sha256::new();
        hash.update(b"everything.settings-agent.turn-fence.v1\0");
        hash.update(encode(self, MAX_SETTINGS_RECORD_BYTES)?.as_bytes());
        Ok(format!("{:x}", hash.finalize()))
    }
}

impl Store {
    pub(crate) fn open_settings_memory() -> StorageResult<Self> {
        let connection = Connection::open_in_memory()?;
        connection.pragma_update(None, "page_size", SETTINGS_PAGE_BYTES)?;
        connection.pragma_update(None, "max_page_count", MAX_SETTINGS_PAGES)?;
        connection.pragma_update(None, "journal_mode", "MEMORY")?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        connection.pragma_update(None, "cache_spill", "OFF")?;
        let store = Self::from_connection(connection)?;
        store.settings_memory_capacity(0)?;
        Ok(store)
    }

    pub(crate) fn settings_memory_capacity(&self, additional_bytes: usize) -> StorageResult<()> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("PRAGMA database_list")?;
        let mut rows = statement.query([])?;
        let mut main = false;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let filename: String = row.get(2)?;
            if !filename.is_empty() || !matches!(name.as_str(), "main" | "temp") {
                return invalid("Settings storage must remain fileless and unattached");
            }
            main |= name == "main";
        }
        drop(rows);
        drop(statement);
        let page_size: u64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
        let page_cap: u64 =
            connection.pragma_query_value(None, "max_page_count", |row| row.get(0))?;
        let pages: u64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
        let journal: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let temp: i64 = connection.pragma_query_value(None, "temp_store", |row| row.get(0))?;
        let spill: i64 = connection.pragma_query_value(None, "cache_spill", |row| row.get(0))?;
        let foreign_keys: i64 =
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        let busy: i64 = connection.pragma_query_value(None, "busy_timeout", |row| row.get(0))?;
        if !main
            || page_size != SETTINGS_PAGE_BYTES
            || page_cap != MAX_SETTINGS_PAGES
            || journal != "memory"
            || temp != 2
            || spill != 0
            || foreign_keys != 1
            || busy != 5000
        {
            return invalid("Settings memory storage configuration changed");
        }
        let additional = u64::try_from(additional_bytes)
            .ok()
            .and_then(|bytes| bytes.checked_add(SETTINGS_PAGE_BYTES - 1))
            .map(|bytes| bytes / SETTINGS_PAGE_BYTES)
            .and_then(|pages| pages.checked_add(SETTINGS_MEMORY_MARGIN_PAGES))
            .and_then(|additional| pages.checked_add(additional));
        if additional.is_none_or(|total| total > MAX_SETTINGS_PAGES) {
            return Err(StorageError::TransportCapacity("Settings memory pages"));
        }
        Ok(())
    }

    pub(crate) fn reserve_settings_session(
        &self,
        reservation: &SettingsSessionReservation,
    ) -> StorageResult<()> {
        validate_session(reservation)?;
        let encoded = encode(reservation, MAX_SETTINGS_RECORD_BYTES)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(original) = read_session(&transaction, reservation.session_id)? {
            return if original == *reservation {
                Ok(())
            } else {
                conflict("Settings session", reservation.session_id.to_string())
            };
        }
        let count: i64 =
            transaction.query_row("SELECT count(*) FROM settings_agent_sessions", [], |row| {
                row.get(0)
            })?;
        if count >= MAX_RETAINED_SETTINGS_SESSIONS as i64 {
            return Err(StorageError::TransportCapacity(
                "Settings session tombstones",
            ));
        }
        if wall_now()? >= reservation.expires_at {
            return invalid("Settings session expired before reservation");
        }
        transaction.execute(
            "INSERT INTO settings_agent_sessions(session_id,conversation_id,create_command_id,reservation) VALUES(?1,?2,?3,?4)",
            params![reservation.session_id.to_string(),reservation.conversation_id.to_string(),reservation.create_command_id.to_string(),encoded],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn settings_session(
        &self,
        session_id: Uuid,
    ) -> StorageResult<Option<SettingsSessionReservation>> {
        if session_id.is_nil() {
            return invalid("nil Settings session identity");
        }
        read_session(&self.connection()?, session_id)
    }

    pub(crate) fn arm_settings_turn(&self, fence: &SettingsTurnFence) -> StorageResult<()> {
        validate_fence(fence)?;
        if fence.phase != SettingsFencePhase::Armed
            || fence.attempt_id.is_some()
            || fence.model_attempts != 0
            || fence.tool_calls != 0
            || fence.charged_tokens != 0
            || fence.reserved_output_tokens != 0
            || fence.reported_input_tokens != 0
            || fence.reported_output_tokens != 0
            || !fence.mutation_ids.is_empty()
            || fence.provider_complete
            || fence.usage_complete
            || fence.last_observed_wall != fence.started_at
        {
            return invalid("new Settings turn requires an unspent Armed fence");
        }
        let encoded = encode(fence, MAX_SETTINGS_RECORD_BYTES)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(original) = read_fence(&transaction, fence.command_id)? {
            return if original == *fence {
                Ok(())
            } else {
                conflict("Settings turn", fence.command_id.to_string())
            };
        }
        if !unsettled(&transaction, None, 1)?.is_empty() {
            return Err(StorageError::TransportCapacity("active Settings turn"));
        }
        let session = read_session(&transaction, fence.session_id)?
            .ok_or_else(|| missing("Settings session", &fence.session_id.to_string()))?;
        validate_session_fence(&session, fence)?;
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM settings_agent_turns WHERE session_id=?1",
            [fence.session_id.to_string()],
            |row| row.get(0),
        )?;
        if count >= MAX_SETTINGS_TURNS as i64 {
            return Err(StorageError::TransportCapacity("Settings session turns"));
        }
        if wall_now()? >= fence.deadline {
            return invalid("Settings turn expired before reservation");
        }
        transaction.execute(
            "INSERT INTO settings_agent_turns(command_id,session_id,task_id,phase,record) VALUES(?1,?2,?3,'armed',?4)",
            params![fence.command_id.to_string(),fence.session_id.to_string(),fence.task_id.to_string(),encoded],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn update_settings_turn(&self, fence: &SettingsTurnFence) -> StorageResult<()> {
        validate_fence(fence)?;
        let encoded = encode(fence, MAX_SETTINGS_RECORD_BYTES)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let original = read_fence(&transaction, fence.command_id)?
            .ok_or_else(|| missing("Settings turn", &fence.command_id.to_string()))?;
        if original == *fence {
            return Ok(());
        }
        if original.phase != SettingsFencePhase::Armed
            || read_cleanup(&transaction, &original)?.is_some()
        {
            return conflict("Settings turn", fence.command_id.to_string());
        }
        validate_transition(&original, fence)?;
        let session = read_session(&transaction, fence.session_id)?
            .ok_or_else(|| missing("Settings session", &fence.session_id.to_string()))?;
        validate_session_fence(&session, fence)?;
        validate_mutations(&transaction, &session, fence)?;
        transaction.execute(
            "UPDATE settings_agent_turns SET phase=?2,record=?3 WHERE command_id=?1",
            params![fence.command_id.to_string(), phase(fence.phase), encoded],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn settings_turn(
        &self,
        command_id: CommandId,
    ) -> StorageResult<Option<SettingsTurnFence>> {
        read_fence(&self.connection()?, command_id)
    }

    pub(crate) fn unsettled_settings_turns(
        &self,
        after: Option<CommandId>,
        limit: usize,
    ) -> StorageResult<Vec<SettingsTurnFence>> {
        unsettled(&self.connection()?, after, limit)
    }

    pub(crate) fn has_unsettled_settings_turns(&self) -> StorageResult<bool> {
        Ok(!self.unsettled_settings_turns(None, 1)?.is_empty())
    }

    pub(crate) fn attest_settings_cleanup(
        &self,
        attestation: &SettingsCleanupAttestation,
    ) -> StorageResult<()> {
        validate_attestation(attestation)?;
        let encoded = encode(attestation, MAX_SETTINGS_CLEANUP_BYTES)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fence = read_fence(&transaction, attestation.command_id)?
            .ok_or_else(|| missing("Settings turn", &attestation.command_id.to_string()))?;
        if let Some(original) = read_cleanup(&transaction, &fence)? {
            return if original == *attestation {
                Ok(())
            } else {
                conflict("Settings cleanup", attestation.command_id.to_string())
            };
        }
        validate_cleanup(&transaction, &fence, attestation)?;
        transaction.execute(
            "INSERT INTO settings_agent_cleanup(command_id,attestation) VALUES(?1,?2)",
            params![attestation.command_id.to_string(), encoded],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn settings_cleanup(
        &self,
        command_id: CommandId,
    ) -> StorageResult<Option<SettingsCleanupAttestation>> {
        let connection = self.connection()?;
        let Some(fence) = read_fence(&connection, command_id)? else {
            return Ok(None);
        };
        read_cleanup(&connection, &fence)
    }
}

fn unsettled(
    connection: &Connection,
    after: Option<CommandId>,
    limit: usize,
) -> StorageResult<Vec<SettingsTurnFence>> {
    validate_limit(limit)?;
    let count: i64 =
        connection.query_row("SELECT count(*) FROM settings_agent_turns", [], |row| {
            row.get(0)
        })?;
    if count > (MAX_RETAINED_SETTINGS_SESSIONS * MAX_SETTINGS_TURNS) as i64 {
        return invalid("Settings fence retention bound exceeded");
    }
    let mut statement =
        connection.prepare("SELECT command_id FROM settings_agent_turns ORDER BY command_id")?;
    let mut rows = statement.query([])?;
    let mut result = Vec::with_capacity(limit.min(count as usize));
    while let Some(row) = rows.next()? {
        let command_id: CommandId = text(row, 0, 36)?
            .parse()
            .map_err(|_| malformed("Settings fence command"))?;
        let fence = read_fence(connection, command_id)?
            .ok_or_else(|| missing("Settings turn", &command_id.to_string()))?;
        let acknowledged = read_cleanup(connection, &fence)?.is_some();
        if matches!(
            fence.phase,
            SettingsFencePhase::Armed | SettingsFencePhase::Unknown
        ) && !acknowledged
            && after.is_none_or(|cursor| command_id > cursor)
            && result.len() < limit
        {
            result.push(fence);
        }
    }
    Ok(result)
}

fn validate_session(value: &SettingsSessionReservation) -> StorageResult<()> {
    if value.version != 1
        || value.session_id.is_nil()
        || value.principal_id.is_nil()
        || value.native_session_id.is_nil()
        || value.expires_at <= value.created_at
        || !digest(&value.provider_binding_sha256)
        || !digest(&value.tools_sha256)
    {
        return invalid("invalid Settings session reservation");
    }
    Ok(())
}

fn validate_fence(value: &SettingsTurnFence) -> StorageResult<()> {
    if value.version != 1
        || value.session_id.is_nil()
        || value.model_attempts > 4
        || value.tool_calls > 8
        || value.deadline <= value.started_at
        || value.deadline.get() - value.started_at.get() > 120_000
        || value.last_observed_wall < value.started_at
        || (value.model_attempts == 0) != value.attempt_id.is_none()
        || value.mutation_ids.len() > MAX_SETTINGS_MUTATIONS
        || !value.mutation_ids.windows(2).all(|ids| ids[0] < ids[1])
        || value.tool_calls == 0 && !value.mutation_ids.is_empty()
        || value.model_attempts == 0 && (value.provider_complete || value.usage_complete)
        || value.phase == SettingsFencePhase::StoppedUncertain
            && (value.tool_calls != 0
                || !value.mutation_ids.is_empty()
                || value.provider_complete
                || value.usage_complete)
    {
        return invalid("invalid Settings turn fence");
    }
    Ok(())
}

fn validate_session_fence(
    session: &SettingsSessionReservation,
    fence: &SettingsTurnFence,
) -> StorageResult<()> {
    if session.session_id != fence.session_id
        || fence.started_at < session.created_at
        || fence.deadline > session.expires_at
        || fence.command_id == session.create_command_id
    {
        return invalid("Settings turn does not match its original session");
    }
    Ok(())
}

fn validate_transition(old: &SettingsTurnFence, new: &SettingsTurnFence) -> StorageResult<()> {
    let new_attempt = old.attempt_id != new.attempt_id;
    if old.version != new.version
        || old.session_id != new.session_id
        || old.command_id != new.command_id
        || old.task_id != new.task_id
        || old.started_at != new.started_at
        || old.deadline != new.deadline
        || old.model_attempts > new.model_attempts
        || old.tool_calls > new.tool_calls
        || old.charged_tokens > new.charged_tokens
        || old.reserved_output_tokens > new.reserved_output_tokens
        || old.reported_input_tokens > new.reported_input_tokens
        || old.reported_output_tokens > new.reported_output_tokens
        || old.last_observed_wall > new.last_observed_wall
        || old
            .mutation_ids
            .iter()
            .any(|id| !new.mutation_ids.contains(id))
        || new_attempt != (new.model_attempts > old.model_attempts)
        || !new_attempt
            && new.phase != SettingsFencePhase::StoppedUncertain
            && (old.provider_complete && !new.provider_complete
                || old.usage_complete && !new.usage_complete)
        || new_attempt && (new.provider_complete || new.usage_complete)
    {
        return invalid("Settings fence identity or conservative floors changed");
    }
    Ok(())
}

fn validate_attestation(value: &SettingsCleanupAttestation) -> StorageResult<()> {
    if value.version != 1
        || value.owner_principal_id.is_nil()
        || !value.lost_ram_uncertainty_accepted
        || !digest(&value.exact_fence_sha256)
    {
        return invalid("invalid Settings cleanup attestation");
    }
    Ok(())
}

fn validate_cleanup(
    connection: &Connection,
    fence: &SettingsTurnFence,
    attestation: &SettingsCleanupAttestation,
) -> StorageResult<()> {
    validate_attestation(attestation)?;
    let session = read_session(connection, fence.session_id)?
        .ok_or_else(|| missing("Settings session", &fence.session_id.to_string()))?;
    if !matches!(
        fence.phase,
        SettingsFencePhase::Armed | SettingsFencePhase::Unknown
    ) || attestation.command_id != fence.command_id
        || attestation.exact_fence_sha256 != fence.sha256()?
        || attestation.owner_principal_id != session.principal_id
        || attestation.attested_at < fence.last_observed_wall
    {
        return invalid("Settings cleanup does not match the original fence");
    }
    Ok(())
}

fn validate_mutations(
    connection: &Connection,
    session: &SettingsSessionReservation,
    fence: &SettingsTurnFence,
) -> StorageResult<()> {
    for id in &fence.mutation_ids {
        let mut statement =
            connection.prepare("SELECT identity FROM settings_operations WHERE request_id=?1")?;
        let mut rows = statement.query([id.to_string()])?;
        let Some(row) = rows.next()? else {
            continue;
        };
        let identity: serde_json::Value = serde_json::from_str(text(row, 0, 18 * 1024)?)?;
        if identity
            .pointer("/version")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
            || identity
                .pointer("/binding/request_id")
                .and_then(serde_json::Value::as_str)
                != Some(id.to_string().as_str())
            || identity
                .pointer("/binding/principal_id")
                .and_then(serde_json::Value::as_str)
                != Some(session.principal_id.to_string().as_str())
            || identity
                .pointer("/binding/client_session_id")
                .and_then(serde_json::Value::as_str)
                != Some(session.native_session_id.to_string().as_str())
            || identity
                .pointer("/origin/kind")
                .and_then(serde_json::Value::as_str)
                != Some("settings_agent")
            || identity
                .pointer("/origin/session_id")
                .and_then(serde_json::Value::as_str)
                != Some(session.session_id.to_string().as_str())
            || identity
                .pointer("/origin/native_session_id")
                .and_then(serde_json::Value::as_str)
                != Some(session.native_session_id.to_string().as_str())
        {
            return invalid("Settings mutation belongs to another operation origin");
        }
    }
    Ok(())
}

fn read_session(
    connection: &Connection,
    session_id: Uuid,
) -> StorageResult<Option<SettingsSessionReservation>> {
    let mut statement = connection.prepare("SELECT session_id,conversation_id,create_command_id,reservation FROM settings_agent_sessions WHERE session_id=?1")?;
    let mut rows = statement.query([session_id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let value: SettingsSessionReservation =
        serde_json::from_str(text(row, 3, MAX_SETTINGS_RECORD_BYTES)?)?;
    validate_session(&value)?;
    if value.session_id != session_id
        || text(row, 0, 36)? != value.session_id.to_string()
        || text(row, 1, 36)? != value.conversation_id.to_string()
        || text(row, 2, 36)? != value.create_command_id.to_string()
        || text(row, 3, MAX_SETTINGS_RECORD_BYTES)? != encode(&value, MAX_SETTINGS_RECORD_BYTES)?
    {
        return invalid("stored Settings session identity is inconsistent");
    }
    Ok(Some(value))
}

fn read_fence(
    connection: &Connection,
    command_id: CommandId,
) -> StorageResult<Option<SettingsTurnFence>> {
    let mut statement = connection.prepare("SELECT command_id,session_id,task_id,phase,record FROM settings_agent_turns WHERE command_id=?1")?;
    let mut rows = statement.query([command_id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let fence: SettingsTurnFence = serde_json::from_str(text(row, 4, MAX_SETTINGS_RECORD_BYTES)?)?;
    validate_fence(&fence)?;
    if fence.command_id != command_id
        || text(row, 0, 36)? != fence.command_id.to_string()
        || text(row, 1, 36)? != fence.session_id.to_string()
        || text(row, 2, 36)? != fence.task_id.to_string()
        || text(row, 3, 32)? != phase(fence.phase)
        || text(row, 4, MAX_SETTINGS_RECORD_BYTES)? != encode(&fence, MAX_SETTINGS_RECORD_BYTES)?
    {
        return invalid("stored Settings turn identity is inconsistent");
    }
    let session = read_session(connection, fence.session_id)?
        .ok_or_else(|| missing("Settings session", &fence.session_id.to_string()))?;
    validate_session_fence(&session, &fence)?;
    validate_mutations(connection, &session, &fence)?;
    Ok(Some(fence))
}

fn read_cleanup(
    connection: &Connection,
    fence: &SettingsTurnFence,
) -> StorageResult<Option<SettingsCleanupAttestation>> {
    let mut statement = connection
        .prepare("SELECT command_id,attestation FROM settings_agent_cleanup WHERE command_id=?1")?;
    let mut rows = statement.query([fence.command_id.to_string()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let value: SettingsCleanupAttestation =
        serde_json::from_str(text(row, 1, MAX_SETTINGS_CLEANUP_BYTES)?)?;
    if text(row, 0, 36)? != value.command_id.to_string()
        || text(row, 1, MAX_SETTINGS_CLEANUP_BYTES)? != encode(&value, MAX_SETTINGS_CLEANUP_BYTES)?
    {
        return invalid("stored Settings cleanup identity is inconsistent");
    }
    validate_cleanup(connection, fence, &value)?;
    Ok(Some(value))
}

fn phase(value: SettingsFencePhase) -> &'static str {
    match value {
        SettingsFencePhase::Armed => "armed",
        SettingsFencePhase::Unknown => "unknown",
        SettingsFencePhase::Settled => "settled",
        SettingsFencePhase::StoppedUncertain => "stopped_uncertain",
    }
}

fn encode(value: &impl Serialize, maximum: usize) -> StorageResult<String> {
    let result = serde_json::to_string(value)?;
    if result.len() > maximum {
        return invalid("Settings marker exceeds its encoded byte bound");
    }
    Ok(result)
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn text<'a>(row: &'a Row<'_>, index: usize, maximum: usize) -> StorageResult<&'a str> {
    let ValueRef::Text(bytes) = row.get_ref(index)? else {
        return invalid("Settings marker text field has another SQL type");
    };
    if bytes.len() > maximum {
        return invalid("stored Settings marker exceeds its byte bound");
    }
    std::str::from_utf8(bytes).map_err(|_| malformed("Settings marker text"))
}

fn wall_now() -> StorageResult<UnixMillis> {
    UnixMillis::now().map_err(|_| malformed("Settings wall clock"))
}

fn malformed(value: &'static str) -> StorageError {
    StorageError::InvalidInput(value.into())
}

fn conflict<T>(entity: &'static str, id: String) -> StorageResult<T> {
    Err(StorageError::StateConflict { entity, id })
}
