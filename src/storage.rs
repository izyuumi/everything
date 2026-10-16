mod transport;
pub use transport::*;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use thiserror::Error;

const SCHEMA_VERSION: i64 = 9;
const MAX_JSON_BYTES: usize = 1_048_576;
const MAX_APPEND_EVENTS: usize = 256;
const MAX_STATE_BYTES: usize = 8 * MAX_JSON_BYTES;
const MAX_PAGE_SIZE: usize = 1_000;
const MAX_PAGE_BYTES: usize = 16 * MAX_JSON_BYTES;
const MAX_CHECKPOINT_COMMANDS: i64 = 100_000;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("storage connection lock was poisoned")]
    Poisoned,
    #[error("invalid storage input: {0}")]
    InvalidInput(String),
    #[error("read page exceeds the {MAX_PAGE_BYTES}-byte budget; request fewer rows")]
    ReadBudgetExceeded,
    #[error(
        "active permission set exceeds the requested limit; policy evaluation must fail closed"
    )]
    PermissionLimitExceeded,
    #[error("permission history exceeds the requested limit; complete inspection is unavailable")]
    PermissionHistoryLimitExceeded,
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("{entity} {id} was not found")]
    NotFound { entity: &'static str, id: String },
    #[error("conversation sequence conflict: expected {expected}, found {actual}")]
    SequenceConflict { expected: u64, actual: u64 },
    #[error("conversation state revision conflict: expected {expected}, found {actual}")]
    StateRevisionConflict { expected: u64, actual: u64 },
    #[error("private checkpoint journal reached its {MAX_CHECKPOINT_COMMANDS}-entry limit")]
    CheckpointJournalFull,
    #[error("command {0} was previously used for a different request")]
    CommandConflict(String),
    #[error("{entity} {id} changed state or cannot make the requested transition")]
    StateConflict { entity: &'static str, id: String },
    #[error("tool call {0} was previously reserved for different execution inputs")]
    ToolCallConflict(String),
    #[error(
        "cleanup attestation for tool call {0} conflicts with its immutable journal or an existing attestation"
    )]
    ToolCleanupConflict(String),
    #[error("transport capacity exhausted: {0}")]
    TransportCapacity(&'static str),
    #[error("transport identity or durable message conflict: {0}")]
    TransportConflict(String),
    #[error("replica synchronization gap: expected at most {expected}, received {actual}")]
    SyncGap { expected: u64, actual: u64 },
    #[error("device {0} is revoked")]
    RevokedDevice(String),
}

pub type StorageResult<T> = Result<T, StorageError>;

#[derive(Clone)]
pub struct Store {
    connection: Arc<Mutex<Connection>>,
    _database_file: Option<Arc<std::fs::File>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationRow {
    pub id: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_sequence: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationStateRow {
    pub conversation_id: String,
    pub revision: u64,
    pub payload: Value,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationStateUpdate {
    pub expected_revision: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventInput {
    pub id: String,
    pub kind: String,
    pub payload: Value,
    pub occurred_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EventRow {
    pub id: String,
    pub conversation_id: String,
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
    pub occurred_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationSnapshot {
    pub conversation: ConversationRow,
    pub state: Option<ConversationStateRow>,
    pub events: Vec<EventRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppendRequest {
    pub conversation_id: String,
    pub command_id: String,
    pub expected_sequence: u64,
    pub events: Vec<EventInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppendOutcome {
    pub replayed: bool,
    pub events: Vec<EventRow>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Queued => matches!(next, Self::Running | Self::Failed | Self::Cancelled),
            Self::Running => matches!(
                next,
                Self::Queued | Self::Waiting | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::Waiting => matches!(
                next,
                Self::Queued | Self::Running | Self::Failed | Self::Cancelled
            ),
            Self::Completed | Self::Failed | Self::Cancelled => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskRow {
    pub id: String,
    pub conversation_id: String,
    pub parent_task_id: Option<String>,
    pub state: TaskState,
    pub input: Value,
    pub checkpoint: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Denied,
    Unknown,
}

impl ToolCallState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Denied => "denied",
            Self::Unknown => "unknown",
        }
    }

    fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Pending => matches!(
                next,
                Self::Running | Self::Failed | Self::Cancelled | Self::Denied
            ),
            Self::Running => matches!(
                next,
                Self::Succeeded | Self::Failed | Self::Cancelled | Self::Unknown
            ),
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Denied | Self::Unknown => {
                false
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallRow {
    pub id: String,
    pub conversation_id: String,
    pub task_id: Option<String>,
    pub tool_name: String,
    pub state: ToolCallState,
    pub request: Value,
    pub result: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallReservation {
    pub created: bool,
    pub call: ToolCallRow,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCleanupAttestation {
    pub operation_id: String,
    pub original_result_digest: String,
    pub operator: String,
    pub note: String,
    pub attested_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCleanupStatus {
    pub operation_id: String,
    pub original_result_digest: String,
    pub attestation: Option<ToolCleanupAttestation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionRow {
    pub id: String,
    pub conversation_id: Option<String>,
    pub payload: Value,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceRow {
    pub id: String,
    pub label: String,
    pub public_identity: Value,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutboxRow {
    pub id: String,
    pub device_id: String,
    pub envelope: Vec<u8>,
    pub created_at: i64,
    pub not_before: i64,
    pub attempts: u32,
    pub delivered_at: Option<i64>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    #[cfg(unix)]
    pub(crate) fn open_validated(
        path: impl AsRef<Path>,
        database_file: std::fs::File,
    ) -> StorageResult<Self> {
        let mut store = Self::open(path)?;
        store._database_file = Some(Arc::new(database_file));
        Ok(store)
    }

    pub fn open_in_memory() -> StorageResult<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> StorageResult<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 0 {
            return invalid("database schema version must be nonnegative");
        }
        if version > SCHEMA_VERSION {
            return Err(StorageError::UnsupportedSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version == 0 {
            transaction.execute_batch(include_str!("../migrations/0001_initial.sql"))?;
        }
        if version < 2 {
            transaction.execute_batch(include_str!("../migrations/0002_conversation_state.sql"))?;
        }
        if version < 3 {
            transaction.execute_batch(include_str!("../migrations/0003_tool_cleanup.sql"))?;
        }
        if version < 4 {
            transaction.execute_batch(include_str!("../migrations/0004_device_transport.sql"))?;
        }
        if version < 5 {
            transaction
                .execute_batch(include_str!("../migrations/0005_private_checkpoints.sql"))?;
        }
        if version < 6 {
            transaction.execute_batch(include_str!("../migrations/0006_scheduled_jobs.sql"))?;
        }
        if version < 7 {
            transaction.execute_batch(include_str!("../migrations/0007_host_commands.sql"))?;
        }
        if version < 8 {
            transaction.execute_batch(include_str!("../migrations/0008_scheduled_attempts.sql"))?;
        }
        if version < 9 {
            transaction
                .execute_batch(include_str!("../migrations/0009_scheduled_execution.sql"))?;
        }
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            _database_file: None,
        })
    }

    pub(crate) fn connection(&self) -> StorageResult<MutexGuard<'_, Connection>> {
        self.connection.lock().map_err(|_| StorageError::Poisoned)
    }

    pub fn create_conversation(
        &self,
        id: &str,
        title: &str,
        created_at: i64,
    ) -> StorageResult<ConversationRow> {
        validate_id(id)?;
        validate_text(title, "title", 4_096)?;
        validate_time(created_at)?;
        let row = ConversationRow {
            id: id.to_owned(),
            title: title.to_owned(),
            created_at,
            updated_at: created_at,
            last_sequence: 0,
        };
        self.connection()?.execute(
            "INSERT INTO conversations(id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![id, title, created_at],
        )?;
        Ok(row)
    }

    pub fn conversation(&self, id: &str) -> StorageResult<Option<ConversationRow>> {
        validate_id(id)?;
        Ok(self
            .connection()?
            .query_row(
                "SELECT id, title, created_at, updated_at, last_sequence FROM conversations WHERE id = ?1",
                [id],
                conversation_row,
            )
            .optional()?)
    }

    pub fn conversation_state(
        &self,
        conversation_id: &str,
    ) -> StorageResult<Option<ConversationStateRow>> {
        validate_id(conversation_id)?;
        Ok(self.connection()?.query_row(
            "SELECT conversation_id, revision, payload, updated_at FROM conversation_states WHERE conversation_id = ?1",
            [conversation_id],
            conversation_state_row,
        ).optional()?)
    }

    pub fn conversation_snapshot(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> StorageResult<ConversationSnapshot> {
        validate_id(conversation_id)?;
        validate_sequence(after_sequence)?;
        validate_limit(limit)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let conversation = transaction.query_row(
            "SELECT id, title, created_at, updated_at, last_sequence FROM conversations WHERE id = ?1",
            [conversation_id], conversation_row,
        ).optional()?.ok_or_else(|| missing("conversation", conversation_id))?;
        let state = transaction.query_row(
            "SELECT conversation_id, revision, payload, updated_at FROM conversation_states WHERE conversation_id = ?1",
            [conversation_id], conversation_state_row,
        ).optional()?;
        let mut snapshot = ConversationSnapshot {
            conversation,
            state,
            events: Vec::new(),
        };
        let fixed_bytes = serde_json::to_vec(&snapshot)?.len();
        if fixed_bytes > MAX_PAGE_BYTES {
            return Err(StorageError::ReadBudgetExceeded);
        }
        snapshot.events = {
            let mut statement = transaction.prepare(
                "SELECT id, conversation_id, sequence, kind, payload, occurred_at FROM events WHERE conversation_id = ?1 AND sequence > ?2 AND sequence <= ?3 ORDER BY sequence LIMIT ?4",
            )?;
            collect_rows_with_budget(
                statement.query_map(
                    params![
                        conversation_id,
                        after_sequence,
                        snapshot.conversation.last_sequence,
                        limit as i64
                    ],
                    event_row,
                )?,
                fixed_bytes,
            )?
        };
        transaction.commit()?;
        Ok(snapshot)
    }

    pub fn conversations(&self, limit: usize) -> StorageResult<Vec<ConversationRow>> {
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, title, created_at, updated_at, last_sequence FROM conversations ORDER BY updated_at DESC, id LIMIT ?1",
        )?;
        collect_rows(statement.query_map([limit as i64], conversation_row)?)
    }

    pub fn append(&self, request: &AppendRequest) -> StorageResult<AppendOutcome> {
        self.append_internal(request, None)
    }

    pub fn append_with_state(
        &self,
        request: &AppendRequest,
        state: &ConversationStateUpdate,
    ) -> StorageResult<AppendOutcome> {
        self.append_internal(request, Some(state))
    }

    pub fn checkpoint_conversation_state(
        &self,
        conversation_id: &str,
        expected_sequence: u64,
        command_id: &str,
        update: &ConversationStateUpdate,
    ) -> StorageResult<ConversationStateRow> {
        validate_id(conversation_id)?;
        validate_id(command_id)?;
        validate_sequence(expected_sequence)?;
        validate_sequence(update.expected_revision)?;
        let payload = encode_json_with_limit(&update.payload, MAX_STATE_BYTES)?;
        let revision = update
            .expected_revision
            .checked_add(1)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| {
                StorageError::InvalidInput("conversation state revision exhausted".into())
            })?;
        let mut hash = Sha256::new();
        hash.update(b"everything.conversation.private-checkpoint.v1\0");
        hash.update(serde_json::to_vec(&(
            conversation_id,
            expected_sequence,
            update,
        ))?);
        let digest = format!("{:x}", hash.finalize());
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let append_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM commands WHERE id=?1)",
            [command_id],
            |row| row.get(0),
        )?;
        if append_exists {
            return Err(StorageError::CommandConflict(command_id.into()));
        }
        let original: Option<(String,u64,u64,String,u64,i64)> = transaction.query_row(
            "SELECT conversation_id,expected_sequence,expected_revision,request_digest,revision,updated_at FROM conversation_checkpoint_commands WHERE command_id=?1", [command_id],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
        ).optional()?;
        if let Some((
            original_conversation,
            original_sequence,
            original_revision,
            original_digest,
            committed_revision,
            updated_at,
        )) = original
        {
            if original_conversation != conversation_id
                || original_sequence != expected_sequence
                || original_revision != update.expected_revision
                || original_digest != digest
            {
                return Err(StorageError::CommandConflict(command_id.into()));
            }
            return Ok(ConversationStateRow {
                conversation_id: conversation_id.into(),
                revision: committed_revision,
                payload: update.payload.clone(),
                updated_at,
            });
        }
        let (actual_sequence, conversation_updated_at): (u64, i64) = transaction
            .query_row(
                "SELECT last_sequence,updated_at FROM conversations WHERE id=?1",
                [conversation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| missing("conversation", conversation_id))?;
        if actual_sequence != expected_sequence {
            return Err(StorageError::SequenceConflict {
                expected: expected_sequence,
                actual: actual_sequence,
            });
        }
        let current: Option<(u64, i64)> = transaction
            .query_row(
                "SELECT revision,updated_at FROM conversation_states WHERE conversation_id=?1",
                [conversation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let actual_revision = current.map_or(0, |(revision, _)| revision);
        if actual_revision != update.expected_revision {
            return Err(StorageError::StateRevisionConflict {
                expected: update.expected_revision,
                actual: actual_revision,
            });
        }
        let count: i64 = transaction.query_row(
            "SELECT count(*) FROM conversation_checkpoint_commands",
            [],
            |row| row.get(0),
        )?;
        if count >= MAX_CHECKPOINT_COMMANDS {
            return Err(StorageError::CheckpointJournalFull);
        }
        let updated_at = conversation_updated_at.max(current.map_or(0, |(_, timestamp)| timestamp));
        transaction.execute(
            "INSERT INTO conversation_states(conversation_id,revision,payload,updated_at) VALUES (?1,?2,?3,?4) ON CONFLICT(conversation_id) DO UPDATE SET revision=excluded.revision,payload=excluded.payload,updated_at=excluded.updated_at",
            params![conversation_id,revision,payload,updated_at],
        )?;
        transaction.execute(
            "INSERT INTO conversation_checkpoint_commands(command_id,conversation_id,expected_sequence,expected_revision,request_digest,revision,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![command_id,conversation_id,expected_sequence,update.expected_revision,digest,revision,updated_at],
        )?;
        transaction.commit()?;
        Ok(ConversationStateRow {
            conversation_id: conversation_id.into(),
            revision,
            payload: update.payload.clone(),
            updated_at,
        })
    }

    fn append_internal(
        &self,
        request: &AppendRequest,
        state: Option<&ConversationStateUpdate>,
    ) -> StorageResult<AppendOutcome> {
        validate_id(&request.conversation_id)?;
        validate_id(&request.command_id)?;
        validate_sequence(request.expected_sequence)?;
        if request.events.is_empty() || request.events.len() > MAX_APPEND_EVENTS {
            return invalid("an append requires between 1 and 256 events");
        }
        let mut request_bytes = 0usize;
        for event in &request.events {
            validate_id(&event.id)?;
            validate_text(&event.kind, "event kind", 256)?;
            validate_time(event.occurred_at)?;
            request_bytes +=
                encode_json(&event.payload)?.len() + event.id.len() + event.kind.len() + 128;
            if request_bytes > MAX_JSON_BYTES * 4 {
                return invalid("append request exceeds 4 MiB");
            }
        }
        let encoded_request = serde_json::to_string(request)?;
        if encoded_request.len() > MAX_JSON_BYTES * 4 {
            return invalid("append request exceeds 4 MiB");
        }
        let encoded_state = state
            .map(|state| {
                validate_sequence(state.expected_revision)?;
                encode_json_with_limit(&state.payload, MAX_STATE_BYTES)
            })
            .transpose()?;
        let state_digest = state
            .map(|state| {
                let mut hash = Sha256::new();
                hash.update(b"everything.conversation.state-update.v1\0");
                hash.update(serde_json::to_vec(state)?);
                Ok::<_, StorageError>(format!("{:x}", hash.finalize()))
            })
            .transpose()?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let checkpoint_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM conversation_checkpoint_commands WHERE command_id=?1)",
            [&request.command_id],
            |row| row.get(0),
        )?;
        if checkpoint_exists {
            return Err(StorageError::CommandConflict(request.command_id.clone()));
        }
        let previous: Option<(String, String, u64, u64, Option<String>)> = transaction
            .query_row(
                "SELECT conversation_id, request, first_sequence, last_sequence, state_digest FROM commands WHERE id = ?1",
                [&request.command_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        if let Some((conversation_id, original, first, last, original_state_digest)) = previous {
            if original != encoded_request
                || conversation_id != request.conversation_id
                || original_state_digest != state_digest
            {
                return Err(StorageError::CommandConflict(request.command_id.clone()));
            }
            let mut statement = transaction.prepare(
                "SELECT id, conversation_id, sequence, kind, payload, occurred_at FROM events WHERE conversation_id = ?1 AND sequence BETWEEN ?2 AND ?3 ORDER BY sequence",
            )?;
            let events = collect_rows(
                statement.query_map(params![conversation_id, first, last], event_row)?,
            )?;
            return Ok(AppendOutcome {
                replayed: true,
                events,
            });
        }
        let actual: u64 = transaction
            .query_row(
                "SELECT last_sequence FROM conversations WHERE id = ?1",
                [&request.conversation_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| missing("conversation", &request.conversation_id))?;
        if actual != request.expected_sequence {
            return Err(StorageError::SequenceConflict {
                expected: request.expected_sequence,
                actual,
            });
        }
        let next_revision = if let Some(state) = state {
            let actual_revision: u64 = transaction
                .query_row(
                    "SELECT revision FROM conversation_states WHERE conversation_id = ?1",
                    [&request.conversation_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            if actual_revision != state.expected_revision {
                return Err(StorageError::StateRevisionConflict {
                    expected: state.expected_revision,
                    actual: actual_revision,
                });
            }
            Some(
                actual_revision
                    .checked_add(1)
                    .filter(|revision| *revision <= i64::MAX as u64)
                    .ok_or_else(|| {
                        StorageError::InvalidInput("conversation state revision exhausted".into())
                    })?,
            )
        } else {
            None
        };
        let last = actual
            .checked_add(request.events.len() as u64)
            .filter(|value| *value <= i64::MAX as u64)
            .ok_or_else(|| StorageError::InvalidInput("event sequence exhausted".into()))?;
        let mut events = Vec::with_capacity(request.events.len());
        let mut updated_at = 0;
        for (index, event) in request.events.iter().enumerate() {
            let sequence = actual + index as u64 + 1;
            transaction.execute(
                "INSERT INTO events(id, conversation_id, sequence, kind, payload, occurred_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![event.id, request.conversation_id, sequence, event.kind, encode_json(&event.payload)?, event.occurred_at],
            )?;
            updated_at = updated_at.max(event.occurred_at);
            events.push(EventRow {
                id: event.id.clone(),
                conversation_id: request.conversation_id.clone(),
                sequence,
                kind: event.kind.clone(),
                payload: event.payload.clone(),
                occurred_at: event.occurred_at,
            });
        }
        transaction.execute(
            "UPDATE conversations SET last_sequence = ?2, updated_at = max(updated_at, ?3) WHERE id = ?1",
            params![request.conversation_id, last, updated_at],
        )?;
        if let (Some(revision), Some(payload)) = (next_revision, encoded_state) {
            transaction.execute(
                "INSERT INTO conversation_states(conversation_id, revision, payload, updated_at) VALUES (?1, ?2, ?3, (SELECT updated_at FROM conversations WHERE id = ?1)) ON CONFLICT(conversation_id) DO UPDATE SET revision = excluded.revision, payload = excluded.payload, updated_at = max(conversation_states.updated_at, excluded.updated_at)",
                params![request.conversation_id, revision, payload],
            )?;
        }
        transaction.execute(
            "INSERT INTO commands(id, conversation_id, request, first_sequence, last_sequence, state_digest) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![request.command_id, request.conversation_id, encoded_request, actual + 1, last, state_digest],
        )?;
        transaction.commit()?;
        Ok(AppendOutcome {
            replayed: false,
            events,
        })
    }

    pub fn command_request(&self, command_id: &str) -> StorageResult<Option<AppendRequest>> {
        validate_id(command_id)?;
        let stored: Option<String> = self
            .connection()?
            .query_row(
                "SELECT request FROM commands WHERE id = ?1",
                [command_id],
                |row| row.get(0),
            )
            .optional()?;
        stored
            .map(|request| serde_json::from_str(&request).map_err(StorageError::from))
            .transpose()
    }

    pub fn command_outcome(&self, command_id: &str) -> StorageResult<Option<AppendOutcome>> {
        validate_id(command_id)?;
        let connection = self.connection()?;
        let stored: Option<(String, u64, u64)> = connection
            .query_row(
                "SELECT conversation_id, first_sequence, last_sequence FROM commands WHERE id = ?1",
                [command_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((conversation_id, first, last)) = stored else {
            return Ok(None);
        };
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, sequence, kind, payload, occurred_at FROM events WHERE conversation_id = ?1 AND sequence BETWEEN ?2 AND ?3 ORDER BY sequence",
        )?;
        let events =
            collect_rows(statement.query_map(params![conversation_id, first, last], event_row)?)?;
        Ok(Some(AppendOutcome {
            replayed: true,
            events,
        }))
    }

    pub fn events(
        &self,
        conversation_id: &str,
        after_sequence: u64,
        limit: usize,
    ) -> StorageResult<Vec<EventRow>> {
        validate_id(conversation_id)?;
        validate_sequence(after_sequence)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, sequence, kind, payload, occurred_at FROM events WHERE conversation_id = ?1 AND sequence > ?2 ORDER BY sequence LIMIT ?3",
        )?;
        collect_rows(statement.query_map(
            params![conversation_id, after_sequence, limit as i64],
            event_row,
        )?)
    }

    pub fn create_task(&self, task: &TaskRow) -> StorageResult<()> {
        validate_id(&task.id)?;
        validate_id(&task.conversation_id)?;
        validate_optional_id(task.parent_task_id.as_deref())?;
        validate_interval(task.created_at, task.updated_at)?;
        if task.state != TaskState::Queued || task.created_at != task.updated_at {
            return invalid("new tasks must be queued with matching creation and update times");
        }
        let input = encode_json(&task.input)?;
        let checkpoint = encode_json(&task.checkpoint)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(parent) = &task.parent_task_id {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = ?1)",
                [parent],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(missing("task", parent));
            }
        }
        transaction.execute(
            "INSERT INTO tasks(id, conversation_id, parent_task_id, state, input, checkpoint, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![task.id, task.conversation_id, task.parent_task_id, task.state.as_str(), input, checkpoint, task.created_at, task.updated_at],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn task(&self, id: &str) -> StorageResult<Option<TaskRow>> {
        validate_id(id)?;
        Ok(self.connection()?.query_row(
            "SELECT id, conversation_id, parent_task_id, state, input, checkpoint, created_at, updated_at FROM tasks WHERE id = ?1",
            [id], task_row,
        ).optional()?)
    }

    pub fn tasks(
        &self,
        conversation_id: Option<&str>,
        state: Option<TaskState>,
        limit: usize,
    ) -> StorageResult<Vec<TaskRow>> {
        validate_optional_id(conversation_id)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, parent_task_id, state, input, checkpoint, created_at, updated_at FROM tasks WHERE (?1 IS NULL OR conversation_id = ?1) AND (?2 IS NULL OR state = ?2) ORDER BY created_at, id LIMIT ?3",
        )?;
        collect_rows(statement.query_map(
            params![conversation_id, state.map(TaskState::as_str), limit as i64],
            task_row,
        )?)
    }

    pub fn transition_task(
        &self,
        id: &str,
        expected: TaskState,
        next: TaskState,
        checkpoint: &Value,
        updated_at: i64,
    ) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(updated_at)?;
        let checkpoint = encode_json(checkpoint)?;
        if !expected.can_transition_to(next) {
            return Err(state_conflict("task", id));
        }
        let changed = self.connection()?.execute(
            "UPDATE tasks SET state = ?3, checkpoint = ?4, updated_at = ?5 WHERE id = ?1 AND state = ?2 AND updated_at <= ?5",
            params![id, expected.as_str(), next.as_str(), checkpoint, updated_at],
        )?;
        require_change(changed, "task", id)
    }

    pub fn checkpoint_task(
        &self,
        id: &str,
        expected: TaskState,
        checkpoint: &Value,
        updated_at: i64,
    ) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(updated_at)?;
        if !matches!(
            expected,
            TaskState::Queued | TaskState::Running | TaskState::Waiting
        ) {
            return Err(state_conflict("task", id));
        }
        let changed = self.connection()?.execute(
            "UPDATE tasks SET checkpoint = ?3, updated_at = ?4 WHERE id = ?1 AND state = ?2 AND updated_at <= ?4",
            params![id, expected.as_str(), encode_json(checkpoint)?, updated_at],
        )?;
        require_change(changed, "task", id)
    }

    pub fn reserve_tool_call(&self, call: &ToolCallRow) -> StorageResult<ToolCallReservation> {
        validate_id(&call.id)?;
        validate_id(&call.conversation_id)?;
        validate_optional_id(call.task_id.as_deref())?;
        validate_text(&call.tool_name, "tool name", 256)?;
        validate_interval(call.created_at, call.updated_at)?;
        if call.state != ToolCallState::Pending
            || call.result.is_some()
            || call.created_at != call.updated_at
        {
            return invalid(
                "new tool calls must be pending without a result and with matching timestamps",
            );
        }
        let request = encode_json(&call.request)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction.query_row(
            "SELECT id, conversation_id, task_id, tool_name, state, request, result, created_at, updated_at FROM tool_calls WHERE id = ?1",
            [&call.id], tool_call_row,
        ).optional()?;
        if let Some(existing) = existing {
            if existing.conversation_id != call.conversation_id
                || existing.task_id != call.task_id
                || existing.tool_name != call.tool_name
                || existing.request != call.request
            {
                return Err(StorageError::ToolCallConflict(call.id.clone()));
            }
            return Ok(ToolCallReservation {
                created: false,
                call: existing,
            });
        }
        if let Some(task_id) = &call.task_id {
            require_related_task(&transaction, task_id, &call.conversation_id)?;
        }
        transaction.execute(
            "INSERT INTO tool_calls(id, conversation_id, task_id, tool_name, state, request, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![call.id, call.conversation_id, call.task_id, call.tool_name, call.state.as_str(), request, call.created_at, call.updated_at],
        )?;
        transaction.commit()?;
        Ok(ToolCallReservation {
            created: true,
            call: call.clone(),
        })
    }

    pub fn tool_call(&self, id: &str) -> StorageResult<Option<ToolCallRow>> {
        validate_id(id)?;
        Ok(self.connection()?.query_row(
            "SELECT id, conversation_id, task_id, tool_name, state, request, result, created_at, updated_at FROM tool_calls WHERE id = ?1",
            [id], tool_call_row,
        ).optional()?)
    }

    pub fn tool_calls(
        &self,
        conversation_id: Option<&str>,
        state: Option<ToolCallState>,
        limit: usize,
    ) -> StorageResult<Vec<ToolCallRow>> {
        validate_optional_id(conversation_id)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, task_id, tool_name, state, request, result, created_at, updated_at FROM tool_calls WHERE (?1 IS NULL OR conversation_id = ?1) AND (?2 IS NULL OR state = ?2) ORDER BY created_at, id LIMIT ?3",
        )?;
        collect_rows(statement.query_map(
            params![
                conversation_id,
                state.map(ToolCallState::as_str),
                limit as i64
            ],
            tool_call_row,
        )?)
    }

    pub fn tool_calls_after_id(
        &self,
        state: ToolCallState,
        after_id: Option<&str>,
        limit: usize,
    ) -> StorageResult<Vec<ToolCallRow>> {
        validate_optional_id(after_id)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, task_id, tool_name, state, request, result, created_at, updated_at FROM tool_calls WHERE state = ?1 AND (?2 IS NULL OR id > ?2) ORDER BY id LIMIT ?3",
        )?;
        collect_rows(statement.query_map(
            params![state.as_str(), after_id, limit as i64],
            tool_call_row,
        )?)
    }

    pub fn tool_cleanup_status(&self, operation_id: &str) -> StorageResult<ToolCleanupStatus> {
        validate_id(operation_id)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (original_result_digest, _) = unknown_result_digest(&transaction, operation_id)?;
        let attestation = read_cleanup_attestation(&transaction, operation_id)?;
        transaction.commit()?;
        Ok(ToolCleanupStatus {
            operation_id: operation_id.into(),
            original_result_digest,
            attestation,
        })
    }

    pub fn attest_tool_cleanup(
        &self,
        attestation: &ToolCleanupAttestation,
    ) -> StorageResult<ToolCleanupAttestation> {
        validate_id(&attestation.operation_id)?;
        validate_text(&attestation.operator, "operator", 256)?;
        validate_text(&attestation.note, "cleanup note", 8_192)?;
        validate_time(attestation.attested_at)?;
        if attestation.original_result_digest.len() != 64
            || !attestation
                .original_result_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return invalid("cleanup result digest must be lowercase SHA-256 hexadecimal");
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (actual_digest, updated_at) =
            unknown_result_digest(&transaction, &attestation.operation_id)?;
        if actual_digest != attestation.original_result_digest
            || attestation.attested_at < updated_at
        {
            return Err(StorageError::ToolCleanupConflict(
                attestation.operation_id.clone(),
            ));
        }
        if let Some(existing) = read_cleanup_attestation(&transaction, &attestation.operation_id)? {
            if existing != *attestation {
                return Err(StorageError::ToolCleanupConflict(
                    attestation.operation_id.clone(),
                ));
            }
            return Ok(existing);
        }
        transaction.execute(
            "INSERT INTO tool_cleanup_attestations(operation_id, original_result_digest, operator, note, attested_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![attestation.operation_id, attestation.original_result_digest, attestation.operator, attestation.note, attestation.attested_at],
        )?;
        transaction.commit()?;
        Ok(attestation.clone())
    }

    pub fn transition_tool_call(
        &self,
        id: &str,
        expected: ToolCallState,
        next: ToolCallState,
        result: Option<&Value>,
        updated_at: i64,
    ) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(updated_at)?;
        if !expected.can_transition_to(next)
            || (next == ToolCallState::Running && result.is_some())
            || (next != ToolCallState::Running && result.is_none())
        {
            return Err(state_conflict("tool call", id));
        }
        let result = result.map(encode_json).transpose()?;
        let changed = self.connection()?.execute(
            "UPDATE tool_calls SET state = ?3, result = ?4, updated_at = ?5 WHERE id = ?1 AND state = ?2 AND updated_at <= ?5",
            params![id, expected.as_str(), next.as_str(), result, updated_at],
        )?;
        require_change(changed, "tool call", id)
    }

    pub fn mark_running_tool_calls_unknown(
        &self,
        result: &Value,
        updated_at: i64,
    ) -> StorageResult<usize> {
        validate_time(updated_at)?;
        Ok(self.connection()?.execute(
            "UPDATE tool_calls SET state = 'unknown', result = ?1, updated_at = max(updated_at, ?2) WHERE state = 'running'",
            params![encode_json(result)?, updated_at],
        )?)
    }

    pub fn create_permission(&self, permission: &PermissionRow) -> StorageResult<()> {
        validate_id(&permission.id)?;
        validate_optional_id(permission.conversation_id.as_deref())?;
        validate_optional_interval(permission.created_at, permission.expires_at)?;
        if permission.revoked_at.is_some() {
            return invalid("new permissions cannot already be revoked");
        }
        self.connection()?.execute(
            "INSERT INTO permissions(id, conversation_id, payload, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![permission.id, permission.conversation_id, encode_json(&permission.payload)?, permission.created_at, permission.expires_at],
        )?;
        Ok(())
    }

    pub fn permission(&self, id: &str) -> StorageResult<Option<PermissionRow>> {
        validate_id(id)?;
        Ok(self.connection()?.query_row(
            "SELECT id, conversation_id, payload, created_at, expires_at, revoked_at FROM permissions WHERE id = ?1",
            [id], permission_row,
        ).optional()?)
    }

    pub fn permission_history(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> StorageResult<Vec<PermissionRow>> {
        validate_id(conversation_id)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, payload, created_at, expires_at, revoked_at FROM permissions WHERE conversation_id = ?1 ORDER BY created_at, id LIMIT ?2",
        )?;
        let permissions = collect_rows(
            statement.query_map(params![conversation_id, (limit + 1) as i64], permission_row)?,
        )?;
        if permissions.len() > limit {
            return Err(StorageError::PermissionHistoryLimitExceeded);
        }
        Ok(permissions)
    }

    pub fn active_permissions(
        &self,
        conversation_id: Option<&str>,
        now: i64,
        limit: usize,
    ) -> StorageResult<Vec<PermissionRow>> {
        validate_optional_id(conversation_id)?;
        validate_time(now)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, conversation_id, payload, created_at, expires_at, revoked_at FROM permissions WHERE (conversation_id IS NULL OR conversation_id = ?1) AND revoked_at IS NULL AND created_at <= ?2 AND (expires_at IS NULL OR expires_at > ?2) ORDER BY created_at, id LIMIT ?3",
        )?;
        let permissions = collect_rows(statement.query_map(
            params![conversation_id, now, (limit + 1) as i64],
            permission_row,
        )?)?;
        if permissions.len() > limit {
            return Err(StorageError::PermissionLimitExceeded);
        }
        Ok(permissions)
    }

    pub fn revoke_permission(&self, id: &str, revoked_at: i64) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(revoked_at)?;
        let changed = self.connection()?.execute(
            "UPDATE permissions SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL AND created_at <= ?2",
            params![id, revoked_at],
        )?;
        require_change(changed, "permission", id)
    }

    pub fn create_device(&self, device: &DeviceRow) -> StorageResult<()> {
        validate_id(&device.id)?;
        validate_text(&device.label, "device label", 256)?;
        validate_time(device.created_at)?;
        if device.revoked_at.is_some() {
            return invalid("new devices cannot already be revoked");
        }
        self.connection()?.execute(
            "INSERT INTO devices(id, label, public_identity, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                device.id,
                device.label,
                encode_json(&device.public_identity)?,
                device.created_at
            ],
        )?;
        Ok(())
    }

    pub fn device(&self, id: &str) -> StorageResult<Option<DeviceRow>> {
        validate_id(id)?;
        Ok(self.connection()?.query_row(
            "SELECT id, label, public_identity, created_at, revoked_at FROM devices WHERE id = ?1",
            [id], device_row,
        ).optional()?)
    }

    pub fn devices(&self, limit: usize) -> StorageResult<Vec<DeviceRow>> {
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id, label, public_identity, created_at, revoked_at FROM devices ORDER BY created_at, id LIMIT ?1",
        )?;
        collect_rows(statement.query_map([limit as i64], device_row)?)
    }

    pub fn revoke_device(&self, id: &str, revoked_at: i64) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(revoked_at)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE devices SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL AND created_at <= ?2",
            params![id, revoked_at],
        )?;
        require_change(changed, "device", id)?;
        transaction.execute(
            "DELETE FROM outbox WHERE device_id = ?1 AND delivered_at IS NULL",
            [id],
        )?;
        transaction.execute("UPDATE transport_inbox SET state = 'rejected', detail = 'sender_revoked', payload = NULL WHERE sender = ?1 AND state = 'pending'", [id])?;
        transaction.execute("UPDATE remote_commands SET state = 'rejected', result = '{\"reason\":\"sender_revoked\"}', updated_at = max(updated_at, ?2) WHERE sender = ?1 AND state = 'pending'", params![id, revoked_at])?;
        transaction.execute("DELETE FROM sync_offers WHERE device_id = ?1", [id])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn enqueue(&self, message: &OutboxRow) -> StorageResult<()> {
        validate_id(&message.id)?;
        validate_id(&message.device_id)?;
        validate_interval(message.created_at, message.not_before)?;
        if message.envelope.is_empty() || message.envelope.len() > MAX_JSON_BYTES {
            return invalid("outbox envelopes require between 1 byte and 1 MiB");
        }
        if message.attempts != 0 || message.delivered_at.is_some() {
            return invalid("new outbox messages require zero attempts and no delivery time");
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let revoked_at: Option<Option<i64>> = transaction
            .query_row(
                "SELECT revoked_at FROM devices WHERE id = ?1",
                [&message.device_id],
                |row| row.get(0),
            )
            .optional()?;
        match revoked_at {
            None => return Err(missing("device", &message.device_id)),
            Some(Some(_)) => return Err(StorageError::RevokedDevice(message.device_id.clone())),
            Some(None) => {}
        }
        transaction.execute(
            "INSERT INTO outbox(id, device_id, envelope, created_at, not_before) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![message.id, message.device_id, message.envelope, message.created_at, message.not_before],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn pending_outbox(&self, now: i64, limit: usize) -> StorageResult<Vec<OutboxRow>> {
        validate_time(now)?;
        validate_limit(limit)?;
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT o.id, o.device_id, o.envelope, o.created_at, o.not_before, o.attempts, o.delivered_at FROM outbox o JOIN devices d ON d.id = o.device_id WHERE o.delivered_at IS NULL AND o.not_before <= ?1 AND d.revoked_at IS NULL ORDER BY o.not_before, o.created_at, o.id LIMIT ?2",
        )?;
        collect_rows(statement.query_map(params![now, limit as i64], outbox_row)?)
    }

    pub fn defer_outbox(&self, id: &str, not_before: i64) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(not_before)?;
        let changed = self.connection()?.execute(
            "UPDATE outbox SET attempts = attempts + 1, not_before = ?2 WHERE id = ?1 AND delivered_at IS NULL AND not_before <= ?2 AND attempts < 4294967295 AND EXISTS (SELECT 1 FROM devices d WHERE d.id = outbox.device_id AND d.revoked_at IS NULL)",
            params![id, not_before],
        )?;
        require_change(changed, "outbox message", id)
    }

    pub fn acknowledge_outbox(&self, id: &str, delivered_at: i64) -> StorageResult<()> {
        validate_id(id)?;
        validate_time(delivered_at)?;
        let changed = self.connection()?.execute(
            "UPDATE outbox SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL AND created_at <= ?2 AND EXISTS (SELECT 1 FROM devices d WHERE d.id = outbox.device_id AND d.revoked_at IS NULL)",
            params![id, delivered_at],
        )?;
        require_change(changed, "outbox message", id)
    }
}

fn conversation_row(row: &Row<'_>) -> rusqlite::Result<ConversationRow> {
    Ok(ConversationRow {
        id: row.get(0)?,
        title: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        last_sequence: row.get(4)?,
    })
}

fn conversation_state_row(row: &Row<'_>) -> rusqlite::Result<ConversationStateRow> {
    Ok(ConversationStateRow {
        conversation_id: row.get(0)?,
        revision: row.get(1)?,
        payload: decode_json(row, 2)?,
        updated_at: row.get(3)?,
    })
}

fn event_row(row: &Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        sequence: row.get(2)?,
        kind: row.get(3)?,
        payload: decode_json(row, 4)?,
        occurred_at: row.get(5)?,
    })
}

fn task_row(row: &Row<'_>) -> rusqlite::Result<TaskRow> {
    let state = match row.get::<_, String>(3)?.as_str() {
        "queued" => TaskState::Queued,
        "running" => TaskState::Running,
        "waiting" => TaskState::Waiting,
        "completed" => TaskState::Completed,
        "failed" => TaskState::Failed,
        "cancelled" => TaskState::Cancelled,
        state => {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid task state {state}"
            )));
        }
    };
    Ok(TaskRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        parent_task_id: row.get(2)?,
        state,
        input: decode_json(row, 4)?,
        checkpoint: decode_json(row, 5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn tool_call_row(row: &Row<'_>) -> rusqlite::Result<ToolCallRow> {
    let state = match row.get::<_, String>(4)?.as_str() {
        "pending" => ToolCallState::Pending,
        "running" => ToolCallState::Running,
        "succeeded" => ToolCallState::Succeeded,
        "failed" => ToolCallState::Failed,
        "cancelled" => ToolCallState::Cancelled,
        "denied" => ToolCallState::Denied,
        "unknown" => ToolCallState::Unknown,
        state => {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid tool call state {state}"
            )));
        }
    };
    let result: Option<String> = row.get(6)?;
    Ok(ToolCallRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        task_id: row.get(2)?,
        tool_name: row.get(3)?,
        state,
        request: decode_json(row, 5)?,
        result: result.map(|value| parse_json(&value, 6)).transpose()?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn permission_row(row: &Row<'_>) -> rusqlite::Result<PermissionRow> {
    Ok(PermissionRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        payload: decode_json(row, 2)?,
        created_at: row.get(3)?,
        expires_at: row.get(4)?,
        revoked_at: row.get(5)?,
    })
}

fn device_row(row: &Row<'_>) -> rusqlite::Result<DeviceRow> {
    Ok(DeviceRow {
        id: row.get(0)?,
        label: row.get(1)?,
        public_identity: decode_json(row, 2)?,
        created_at: row.get(3)?,
        revoked_at: row.get(4)?,
    })
}

fn outbox_row(row: &Row<'_>) -> rusqlite::Result<OutboxRow> {
    Ok(OutboxRow {
        id: row.get(0)?,
        device_id: row.get(1)?,
        envelope: row.get(2)?,
        created_at: row.get(3)?,
        not_before: row.get(4)?,
        attempts: row.get(5)?,
        delivered_at: row.get(6)?,
    })
}

fn unknown_result_digest(
    connection: &Connection,
    operation_id: &str,
) -> StorageResult<(String, i64)> {
    let row: Option<(String, Option<String>, i64)> = connection
        .query_row(
            "SELECT state, result, updated_at FROM tool_calls WHERE id = ?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (state, result, updated_at) = row.ok_or_else(|| missing("tool call", operation_id))?;
    if state != "unknown" {
        return Err(state_conflict("tool call", operation_id));
    }
    let mut hash = Sha256::new();
    hash.update(b"everything.tool-call.unknown-result.v1\0");
    match result {
        Some(result) => {
            hash.update([1]);
            hash.update(result.as_bytes());
        }
        None => hash.update([0]),
    }
    Ok((format!("{:x}", hash.finalize()), updated_at))
}

fn read_cleanup_attestation(
    connection: &Connection,
    operation_id: &str,
) -> StorageResult<Option<ToolCleanupAttestation>> {
    Ok(connection.query_row(
        "SELECT operation_id, original_result_digest, operator, note, attested_at FROM tool_cleanup_attestations WHERE operation_id = ?1",
        [operation_id],
        |row| Ok(ToolCleanupAttestation { operation_id: row.get(0)?, original_result_digest: row.get(1)?, operator: row.get(2)?, note: row.get(3)?, attested_at: row.get(4)? }),
    ).optional()?)
}

fn require_related_task(
    connection: &Connection,
    task_id: &str,
    conversation_id: &str,
) -> StorageResult<()> {
    let found: Option<String> = connection
        .query_row(
            "SELECT conversation_id FROM tasks WHERE id = ?1",
            [task_id],
            |row| row.get(0),
        )
        .optional()?;
    match found {
        Some(found) if found == conversation_id => Ok(()),
        Some(_) => invalid("tool calls must belong to their task's conversation"),
        None => Err(missing("task", task_id)),
    }
}

fn collect_rows<T: Serialize>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
) -> StorageResult<Vec<T>> {
    collect_rows_with_budget(rows, 2)
}

fn collect_rows_with_budget<T: Serialize>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
    mut bytes: usize,
) -> StorageResult<Vec<T>> {
    let mut result = Vec::new();
    for row in rows {
        let row = row?;
        bytes = bytes.saturating_add(usize::from(!result.is_empty()));
        bytes = bytes.saturating_add(serde_json::to_vec(&row)?.len());
        if bytes > MAX_PAGE_BYTES {
            return Err(StorageError::ReadBudgetExceeded);
        }
        result.push(row);
    }
    Ok(result)
}

fn decode_json(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    parse_json(&row.get::<_, String>(index)?, index)
}

fn parse_json(value: &str, index: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn encode_json(value: &Value) -> StorageResult<String> {
    encode_json_with_limit(value, MAX_JSON_BYTES)
}

fn encode_json_with_limit(value: &Value, max_bytes: usize) -> StorageResult<String> {
    let encoded = serde_json::to_string(value)?;
    if encoded.len() > max_bytes {
        return invalid(&format!("JSON payload exceeds {max_bytes} bytes"));
    }
    Ok(encoded)
}

fn validate_id(value: &str) -> StorageResult<()> {
    validate_text(value, "identifier", 256)
}

fn validate_optional_id(value: Option<&str>) -> StorageResult<()> {
    value.map(validate_id).transpose().map(|_| ())
}

fn validate_text(value: &str, name: &str, max_bytes: usize) -> StorageResult<()> {
    if value.trim().is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return invalid(&format!(
            "{name} must be nonempty, free of controls, and at most {max_bytes} bytes"
        ));
    }
    Ok(())
}

fn validate_time(value: i64) -> StorageResult<()> {
    if value < 0 {
        return invalid("timestamps must be nonnegative Unix milliseconds");
    }
    Ok(())
}

fn validate_interval(start: i64, end: i64) -> StorageResult<()> {
    validate_time(start)?;
    if end < start {
        return invalid("ending timestamp precedes starting timestamp");
    }
    Ok(())
}

fn validate_optional_interval(start: i64, end: Option<i64>) -> StorageResult<()> {
    validate_time(start)?;
    end.map(|end| validate_interval(start, end))
        .transpose()
        .map(|_| ())
}

fn validate_sequence(value: u64) -> StorageResult<()> {
    if value > i64::MAX as u64 {
        return invalid("sequence exceeds SQLite integer range");
    }
    Ok(())
}

fn validate_limit(limit: usize) -> StorageResult<()> {
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return invalid("read limit must be between 1 and 1000");
    }
    Ok(())
}

fn invalid<T>(message: &str) -> StorageResult<T> {
    Err(StorageError::InvalidInput(message.to_owned()))
}

fn missing(entity: &'static str, id: &str) -> StorageError {
    StorageError::NotFound {
        entity,
        id: id.to_owned(),
    }
}

fn state_conflict(entity: &'static str, id: &str) -> StorageError {
    StorageError::StateConflict {
        entity,
        id: id.to_owned(),
    }
}

fn require_change(changed: usize, entity: &'static str, id: &str) -> StorageResult<()> {
    if changed == 0 {
        return Err(state_conflict(entity, id));
    }
    Ok(())
}
