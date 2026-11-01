use std::io::{self, Write};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::agent::MAX_CHAT_TEXT_BYTES;
use crate::ipc::OwnerCommand;
use crate::protocol::{
    Command, CommandEnvelope, CommandId, ConversationEvent, ConversationId, DeviceId,
    EventEnvelope, MessageContent, MessageId, MessageRole, OperationId, TaskId, UnixMillis,
};
use crate::storage::{AppendRequest, StorageError, Store};

const MAX_ORIGINAL_BYTES: usize = 256 * 1024;
const MAX_OPTIONS_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_RETAINED_BYTES: i64 = 128 * 1024 * 1024;
const MAX_PENDING: i64 = 1024;
const MAX_IDENTITIES: i64 = 100_000;
const RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Error)]
pub enum InboxError {
    #[error("host command storage failed: {0}")]
    Storage(#[from] StorageError),
    #[error("invalid host command: {0}")]
    Invalid(&'static str),
    #[error("host command {0} conflicts with an existing identity or result")]
    Conflict(CommandId),
    #[error("host command journal reached its capacity")]
    Capacity,
    #[error("host command origin is no longer authorized")]
    OriginDenied,
    #[error("host command journal record is malformed")]
    Corrupt,
}

impl From<rusqlite::Error> for InboxError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(StorageError::Sqlite(error))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandOrigin {
    LocalOwner,
    PairedDevice {
        device_id: DeviceId,
        pinned_identity: Value,
    },
}

#[derive(Clone, Debug)]
pub enum InboxInput {
    Domain(CommandEnvelope),
    Owner(OwnerCommand),
}

impl InboxInput {
    pub fn command_id(&self) -> CommandId {
        match self {
            Self::Domain(command) => command.command_id,
            Self::Owner(command) => command.command_id(),
        }
    }

    pub fn conversation_id(&self) -> ConversationId {
        match self {
            Self::Domain(command) => command.conversation_id,
            Self::Owner(command) => command.conversation_id(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Domain(_) => "domain",
            Self::Owner(OwnerCommand::Text { .. }) => "local_text",
            Self::Owner(OwnerCommand::Initialize { .. }) => "local_initialize",
        }
    }

    pub(super) fn validate_original(&self, original: &Value) -> Result<(), InboxError> {
        let matches = match self {
            Self::Domain(command) => serde_json::from_value::<CommandEnvelope>(original.clone())
                .is_ok_and(|decoded| decoded == *command),
            Self::Owner(command) => {
                serde_json::from_value::<OwnerCommand>(original.clone())
                    .ok()
                    .and_then(|decoded| serde_json::to_value(decoded).ok())
                    == serde_json::to_value(command).ok()
            }
        };
        if !matches {
            return Err(InboxError::Invalid(
                "original input does not match its typed command",
            ));
        }
        match self {
            Self::Owner(OwnerCommand::Text { command, options }) => {
                options.validate().map_err(InboxError::Invalid)?;
                validate_text(&command.text)?;
            }
            Self::Owner(OwnerCommand::Initialize {
                title,
                configuration,
                ..
            }) => {
                validate_title(title)?;
                configuration
                    .validate()
                    .map_err(|_| InboxError::Invalid("invalid conversation configuration"))?;
            }
            Self::Domain(command) => match &command.payload {
                Command::SendMessage { message } => match message.content.as_slice() {
                    [MessageContent::Text { text }] if message.role == MessageRole::User => {
                        validate_text(text)?
                    }
                    _ => {
                        return Err(InboxError::Invalid(
                            "submitted messages must contain one user text block",
                        ));
                    }
                },
                Command::CreateTask { instruction, .. }
                | Command::SteerTask { instruction, .. } => validate_text(instruction)?,
                Command::CreateConversation { title: Some(title) } => validate_title(title)?,
                _ => {}
            },
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandPhase {
    Accepted,
    Applied,
    Completed,
    Rejected,
}

impl CommandPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Applied => "applied",
            Self::Completed => "completed",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedReceipt {
    pub conversation_id: ConversationId,
    pub task_id: Option<TaskId>,
    pub message_id: Option<MessageId>,
    pub operation_id: Option<OperationId>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HostCommandRecord {
    pub command_id: CommandId,
    pub conversation_id: ConversationId,
    pub input_kind: String,
    pub origin: CommandOrigin,
    pub request_digest: String,
    pub original: Option<Value>,
    pub effective_options: Option<Value>,
    pub phase: CommandPhase,
    pub applied_receipt: Option<AppliedReceipt>,
    pub terminal_result: Option<Value>,
    pub result_digest: Option<String>,
    pub accepted_at: UnixMillis,
    pub updated_at: UnixMillis,
}

impl HostCommandRecord {
    pub fn input(&self) -> Result<Option<InboxInput>, InboxError> {
        let Some(original) = self.original.clone() else {
            return Ok(None);
        };
        let input = match self.input_kind.as_str() {
            "domain" => serde_json::from_value(original).map(InboxInput::Domain),
            "local_text" | "local_initialize" => {
                serde_json::from_value(original).map(InboxInput::Owner)
            }
            _ => return Err(InboxError::Corrupt),
        }
        .map_err(|_| InboxError::Corrupt)?;
        if input.command_id() != self.command_id
            || input.conversation_id() != self.conversation_id
            || input.kind() != self.input_kind
        {
            return Err(InboxError::Corrupt);
        }
        Ok(Some(input))
    }
}

pub struct InboxReservation {
    pub created: bool,
    pub record: HostCommandRecord,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingCursor {
    pub accepted_at: UnixMillis,
    pub command_id: CommandId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnappliedDiscard {
    pub command_id: CommandId,
    pub expected_accepted_at: UnixMillis,
    pub request_id: CommandId,
    pub operator: String,
    pub reason: String,
}

impl UnappliedDiscard {
    pub(crate) fn validate_metadata(
        expected_accepted_at: UnixMillis,
        operator: &str,
        reason: &str,
    ) -> Result<(), InboxError> {
        if expected_accepted_at.get() > i64::MAX as u64
            || operator.trim().is_empty()
            || operator.len() > 128
            || operator.chars().any(char::is_control)
            || reason.trim().is_empty()
            || reason.len() > 1_024
            || reason.contains('\0')
        {
            return Err(InboxError::Invalid(
                "discard requires the original acceptance time, a bounded operator and a nonempty bounded reason",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct HostInbox {
    store: Store,
}

impl HostInbox {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn lookup(
        &self,
        input: &InboxInput,
        original: &Value,
        origin: &CommandOrigin,
    ) -> Result<Option<HostCommandRecord>, InboxError> {
        input.validate_original(original)?;
        let expected = request_digest(input, original, origin)?;
        let connection = self.store.connection()?;
        let Some(record) = read_record(&connection, input.command_id())? else {
            return Ok(None);
        };
        compare_identity(&record, input, origin, &expected)?;
        validate_origin(&connection, input, original, origin)?;
        Ok(Some(record))
    }

    pub fn reserve(
        &self,
        input: &InboxInput,
        original: &Value,
        origin: &CommandOrigin,
        effective_options: &Value,
        now: UnixMillis,
    ) -> Result<InboxReservation, InboxError> {
        input.validate_original(original)?;
        let original_json = bounded_json(original, MAX_ORIGINAL_BYTES)?;
        let origin_json = bounded_json(origin, 16 * 1024)?;
        let digest = request_digest(input, original, origin)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(record) = read_record(&transaction, input.command_id())? {
            compare_identity(&record, input, origin, &digest)?;
            validate_origin(&transaction, input, original, origin)?;
            return Ok(InboxReservation {
                created: false,
                record,
            });
        }
        validate_origin(&transaction, input, original, origin)?;
        validate_namespace(&transaction, input)?;
        if !effective_options.is_object() {
            return Err(InboxError::Invalid(
                "effective options must be a trusted object",
            ));
        }
        let options_json = bounded_json(effective_options, MAX_OPTIONS_BYTES)?;
        let (identities, pending, bytes): (i64, i64, i64) = transaction.query_row(
            "SELECT count(*), coalesce(sum(phase IN ('accepted','applied')),0), coalesce(sum(length(CAST(origin AS BLOB)) + coalesce(length(CAST(original AS BLOB)),0) + coalesce(length(CAST(effective_options AS BLOB)),0) + coalesce(length(CAST(applied_receipt AS BLOB)),0) + coalesce(length(CAST(terminal_result AS BLOB)),0)),0) FROM host_commands",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let incoming = original_json.len() + origin_json.len() + options_json.len();
        if identities >= MAX_IDENTITIES
            || pending >= MAX_PENDING
            || bytes + incoming as i64 + (pending + 1) * (MAX_RESULT_BYTES + 4096) as i64
                > MAX_RETAINED_BYTES
        {
            return Err(InboxError::Capacity);
        }
        transaction.execute(
            "INSERT INTO host_commands(command_id,conversation_id,input_kind,origin,request_digest,original,effective_options,phase,accepted_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,'accepted',?8,?8)",
            params![input.command_id().to_string(),input.conversation_id().to_string(),input.kind(),origin_json,digest,original_json,options_json,now.get() as i64],
        )?;
        let record = read_record(&transaction, input.command_id())?.ok_or(InboxError::Corrupt)?;
        transaction.commit()?;
        Ok(InboxReservation {
            created: true,
            record,
        })
    }

    pub fn record(&self, id: CommandId) -> Result<Option<HostCommandRecord>, InboxError> {
        let connection = self.store.connection()?;
        read_record(&connection, id)
    }

    pub fn validate_for_apply(&self, id: CommandId) -> Result<HostCommandRecord, InboxError> {
        let connection = self.store.connection()?;
        let record =
            read_record(&connection, id)?.ok_or(InboxError::Invalid("command was not reserved"))?;
        if !matches!(record.phase, CommandPhase::Accepted | CommandPhase::Applied) {
            return Err(InboxError::Conflict(id));
        }
        let input = record.input()?.ok_or(InboxError::Corrupt)?;
        let original = record.original.as_ref().ok_or(InboxError::Corrupt)?;
        input.validate_original(original)?;
        compare_identity(
            &record,
            &input,
            &record.origin,
            &request_digest(&input, original, &record.origin)?,
        )?;
        validate_origin(&connection, &input, original, &record.origin)?;
        Ok(record)
    }

    pub fn pending(&self, limit: usize) -> Result<Vec<HostCommandRecord>, InboxError> {
        self.pending_after(None, limit)
    }

    pub fn pending_after(
        &self,
        after: Option<PendingCursor>,
        limit: usize,
    ) -> Result<Vec<HostCommandRecord>, InboxError> {
        if !(1..=256).contains(&limit) {
            return Err(InboxError::Invalid(
                "pending page requires 1 through 256 rows",
            ));
        }
        let connection = self.store.connection()?;
        let mut statement = connection.prepare("SELECT * FROM host_commands WHERE phase IN ('accepted','applied') AND (?1 IS NULL OR accepted_at > ?1 OR (accepted_at = ?1 AND command_id > ?2)) ORDER BY accepted_at,command_id LIMIT ?3")?;
        let mut result = Vec::new();
        let mut bytes = 0;
        for row in statement.query_map(
            params![
                after.map(|cursor| cursor.accepted_at.get() as i64),
                after.map(|cursor| cursor.command_id.to_string()),
                limit as i64
            ],
            RawRecord::read,
        )? {
            let row = row?;
            bytes += row.retained_bytes();
            if bytes > 16 * 1024 * 1024 {
                return Err(InboxError::Capacity);
            }
            result.push(row.decode()?);
        }
        Ok(result)
    }

    pub fn mark_applied(
        &self,
        id: CommandId,
        receipt: &AppliedReceipt,
        now: UnixMillis,
    ) -> Result<HostCommandRecord, InboxError> {
        let encoded = bounded_json(receipt, 4096)?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read_record(&transaction, id)?
            .ok_or(InboxError::Invalid("command was not reserved"))?;
        if receipt.conversation_id != record.conversation_id {
            return Err(InboxError::Conflict(id));
        }
        if let Some(existing) = &record.applied_receipt {
            return if existing == receipt {
                Ok(record)
            } else {
                Err(InboxError::Conflict(id))
            };
        }
        if record.phase != CommandPhase::Accepted || now < record.updated_at {
            return Err(InboxError::Conflict(id));
        }
        transaction.execute("UPDATE host_commands SET phase='applied',applied_receipt=?2,updated_at=?3 WHERE command_id=?1 AND phase='accepted'",params![id.to_string(),encoded,now.get() as i64])?;
        let record = read_record(&transaction, id)?.ok_or(InboxError::Corrupt)?;
        transaction.commit()?;
        Ok(record)
    }

    pub fn finish(
        &self,
        id: CommandId,
        rejected: bool,
        result: &Value,
        now: UnixMillis,
    ) -> Result<HostCommandRecord, InboxError> {
        let encoded = bounded_json(result, MAX_RESULT_BYTES)?;
        let result_digest =
            digest_bytes(b"everything.host-command.result.v1\0", encoded.as_bytes());
        let phase = if rejected {
            CommandPhase::Rejected
        } else {
            CommandPhase::Completed
        };
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let record = read_record(&transaction, id)?
            .ok_or(InboxError::Invalid("command was not reserved"))?;
        if let Some(existing) = &record.result_digest {
            return if record.phase == phase && *existing == result_digest {
                Ok(record)
            } else {
                Err(InboxError::Conflict(id))
            };
        }
        if now < record.updated_at || (!rejected && record.phase != CommandPhase::Applied) {
            return Err(InboxError::Conflict(id));
        }
        transaction.execute("UPDATE host_commands SET phase=?2,terminal_result=?3,result_digest=?4,updated_at=?5 WHERE command_id=?1 AND phase IN ('accepted','applied')",params![id.to_string(),phase.as_str(),encoded,result_digest,now.get() as i64])?;
        let record = read_record(&transaction, id)?.ok_or(InboxError::Corrupt)?;
        transaction.commit()?;
        Ok(record)
    }

    pub fn discard_unapplied(
        &self,
        discard: &UnappliedDiscard,
        now: UnixMillis,
    ) -> Result<HostCommandRecord, InboxError> {
        UnappliedDiscard::validate_metadata(
            discard.expected_accepted_at,
            &discard.operator,
            &discard.reason,
        )?;
        let mut connection = self.store.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = discard.command_id;
        let record = read_record(&transaction, id)?
            .ok_or(InboxError::Invalid("unknown command cannot be discarded"))?;
        if record.accepted_at != discard.expected_accepted_at {
            return Err(InboxError::Conflict(id));
        }
        let result = serde_json::json!({
            "code":"discarded_by_owner",
            "message":"unapplied queued command was discarded by its owner",
            "discard":discard,
            "original_request_digest":record.request_digest,
            "accepted_at":record.accepted_at,
        });
        let encoded = bounded_json(&result, MAX_RESULT_BYTES)?;
        let digest = digest_bytes(b"everything.host-command.result.v1\0", encoded.as_bytes());
        if let Some(existing) = &record.result_digest {
            return if record.phase == CommandPhase::Rejected && *existing == digest {
                Ok(record)
            } else {
                Err(InboxError::Conflict(id))
            };
        }
        if record.phase != CommandPhase::Accepted
            || now < record.updated_at
            || now.get() > i64::MAX as u64
        {
            return Err(InboxError::Conflict(id));
        }
        let input = record.input()?.ok_or(InboxError::Corrupt)?;
        let original = record.original.as_ref().ok_or(InboxError::Corrupt)?;
        input.validate_original(original)?;
        compare_identity(
            &record,
            &input,
            &record.origin,
            &request_digest(&input, original, &record.origin)?,
        )?;
        let initialization = match &input {
            InboxInput::Owner(OwnerCommand::Text { .. }) => false,
            InboxInput::Owner(OwnerCommand::Initialize { .. }) => true,
            InboxInput::Domain(command) => match command.payload {
                Command::SendMessage { .. }
                | Command::CreateTask {
                    parent_task_id: None,
                    ..
                } => false,
                Command::CreateConversation { .. } => true,
                _ => return Err(InboxError::Invalid("control commands cannot be discarded")),
            },
        };
        let possible_effect: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM commands WHERE id=?1)
                OR EXISTS(SELECT 1 FROM conversation_checkpoint_commands WHERE command_id=?1)
                OR EXISTS(SELECT 1 FROM tasks WHERE id=?1)
                OR (?2 AND EXISTS(SELECT 1 FROM conversations WHERE id=?3))",
            params![
                id.to_string(),
                initialization,
                record.conversation_id.to_string()
            ],
            |row| row.get(0),
        )?;
        if possible_effect {
            return Err(InboxError::Invalid(
                "command may already have applied effects and cannot be discarded",
            ));
        }
        transaction.execute("UPDATE host_commands SET phase='rejected',terminal_result=?2,result_digest=?3,updated_at=?4 WHERE command_id=?1 AND phase='accepted'",params![id.to_string(),encoded,digest,now.get() as i64])?;
        let record = read_record(&transaction, id)?.ok_or(InboxError::Corrupt)?;
        transaction.commit()?;
        Ok(record)
    }

    pub fn archive(&self, now: UnixMillis) -> Result<usize, InboxError> {
        Ok(self.store.connection()?.execute("UPDATE host_commands SET original=NULL,effective_options=NULL,applied_receipt=NULL,terminal_result=NULL WHERE phase IN ('completed','rejected') AND updated_at < ?1 AND original IS NOT NULL AND NOT EXISTS (SELECT 1 FROM remote_commands WHERE remote_commands.command_id=host_commands.command_id AND remote_commands.state='pending')",[now.get().saturating_sub(RETENTION_MS as u64) as i64])?)
    }
}

fn compare_identity(
    record: &HostCommandRecord,
    input: &InboxInput,
    origin: &CommandOrigin,
    digest: &str,
) -> Result<(), InboxError> {
    if record.command_id != input.command_id()
        || record.conversation_id != input.conversation_id()
        || record.input_kind != input.kind()
        || record.origin != *origin
        || record.request_digest != digest
    {
        return Err(InboxError::Conflict(input.command_id()));
    }
    Ok(())
}

fn validate_origin(
    connection: &Connection,
    input: &InboxInput,
    original: &Value,
    origin: &CommandOrigin,
) -> Result<(), InboxError> {
    match origin {
        CommandOrigin::LocalOwner => {
            if matches!(input,InboxInput::Domain(command) if command.issuing_device_id.is_some()) {
                return Err(InboxError::OriginDenied);
            }
            let remote: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM remote_commands WHERE command_id=?1)",
                [input.command_id().to_string()],
                |row| row.get(0),
            )?;
            if remote {
                return Err(InboxError::OriginDenied);
            }
        }
        CommandOrigin::PairedDevice {
            device_id,
            pinned_identity,
        } => {
            let InboxInput::Domain(command) = input else {
                return Err(InboxError::OriginDenied);
            };
            if command.issuing_device_id != Some(*device_id) {
                return Err(InboxError::OriginDenied);
            }
            let pin: Option<String> = connection
                .query_row(
                    "SELECT public_identity FROM devices WHERE id=?1 AND revoked_at IS NULL",
                    [device_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if pin
                .and_then(|value| serde_json::from_str::<Value>(&value).ok())
                .as_ref()
                != Some(pinned_identity)
            {
                return Err(InboxError::OriginDenied);
            }
            let active: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM transport_endpoint WHERE revoked_at IS NULL)",
                [],
                |row| row.get(0),
            )?;
            let reservation: Option<(String, String, String)> = connection
                .query_row(
                    "SELECT sender,request_digest,state FROM remote_commands WHERE command_id=?1",
                    [command.command_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let expected = digest_bytes(
                b"everything.remote-command.v1\0",
                bounded_json(original, MAX_ORIGINAL_BYTES)?.as_bytes(),
            );
            if !active
                || !reservation.is_some_and(|(sender, digest, state)| {
                    sender == device_id.to_string() && digest == expected && state == "pending"
                })
            {
                return Err(InboxError::OriginDenied);
            }
        }
    }
    Ok(())
}

fn validate_namespace(connection: &Connection, input: &InboxInput) -> Result<(), InboxError> {
    let id = input.command_id();
    let private: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_checkpoint_commands WHERE command_id=?1)",
        [id.to_string()],
        |row| row.get(0),
    )?;
    if private {
        return Err(InboxError::Conflict(id));
    }
    let append: Option<String> = connection
        .query_row(
            "SELECT request FROM commands WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(append) = append else { return Ok(()) };
    let InboxInput::Owner(owner) = input else {
        return Err(InboxError::Conflict(id));
    };
    let append: AppendRequest = serde_json::from_str(&append).map_err(|_| InboxError::Corrupt)?;
    if append.conversation_id != input.conversation_id().to_string() || append.events.len() != 1 {
        return Err(InboxError::Conflict(id));
    }
    let event: EventEnvelope = serde_json::from_value(append.events[0].payload.clone())
        .map_err(|_| InboxError::Corrupt)?;
    if event.causation_id != Some(id) || event.conversation_id != input.conversation_id() {
        return Err(InboxError::Conflict(id));
    }
    let matches = match (owner, event.payload) {
        (OwnerCommand::Text { command, .. }, ConversationEvent::MessageAppended { message }) => {
            message.role == MessageRole::User
                && message.content
                    == vec![MessageContent::Text {
                        text: command.text.clone(),
                    }]
        }
        (
            OwnerCommand::Initialize { title, agent, .. },
            ConversationEvent::ConversationCreated {
                title: Some(original),
                agent_id,
            },
        ) => {
            let recorded = match agent_id {
                Some(recorded) => Some(recorded.to_string()),
                None => connection
                    .query_row(
                        "SELECT agent_id FROM conversations WHERE id=?1",
                        [append.conversation_id.as_str()],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten(),
            };
            *title == original
                && recorded
                    .is_none_or(|recorded| agent.is_none_or(|agent| agent.to_string() == recorded))
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(InboxError::Conflict(id))
    }
}

fn request_digest(
    input: &InboxInput,
    original: &Value,
    origin: &CommandOrigin,
) -> Result<String, InboxError> {
    let encoded = bounded_json(
        &(input.kind(), origin, original),
        MAX_ORIGINAL_BYTES + 16 * 1024 + 1024,
    )?;
    Ok(digest_bytes(
        b"everything.host-command.request.v1\0",
        encoded.as_bytes(),
    ))
}

fn digest_bytes(domain: &[u8], bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn validate_text(text: &str) -> Result<(), InboxError> {
    if text.trim().is_empty() || text.contains('\0') || text.len() > MAX_CHAT_TEXT_BYTES {
        Err(InboxError::Invalid(
            "text requires 1 through 131072 bytes without NUL",
        ))
    } else {
        Ok(())
    }
}

fn validate_title(title: &str) -> Result<(), InboxError> {
    if title.trim().is_empty() || title.contains('\0') || title.len() > 4096 {
        Err(InboxError::Invalid(
            "title requires 1 through 4096 bytes without NUL",
        ))
    } else {
        Ok(())
    }
}

fn bounded_json(value: &impl Serialize, maximum: usize) -> Result<String, InboxError> {
    let mut writer = LimitedWriter {
        bytes: Vec::new(),
        maximum,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| InboxError::Invalid("JSON exceeds its byte limit"))?;
    String::from_utf8(writer.bytes).map_err(|_| InboxError::Corrupt)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    maximum: usize,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("JSON limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RawRecord {
    command_id: String,
    conversation_id: String,
    input_kind: String,
    origin: String,
    request_digest: String,
    original: Option<String>,
    effective_options: Option<String>,
    phase: String,
    applied_receipt: Option<String>,
    terminal_result: Option<String>,
    result_digest: Option<String>,
    accepted_at: u64,
    updated_at: u64,
}

impl RawRecord {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            command_id: row.get("command_id")?,
            conversation_id: row.get("conversation_id")?,
            input_kind: row.get("input_kind")?,
            origin: row.get("origin")?,
            request_digest: row.get("request_digest")?,
            original: row.get("original")?,
            effective_options: row.get("effective_options")?,
            phase: row.get("phase")?,
            applied_receipt: row.get("applied_receipt")?,
            terminal_result: row.get("terminal_result")?,
            result_digest: row.get("result_digest")?,
            accepted_at: row.get("accepted_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
    fn retained_bytes(&self) -> usize {
        self.origin.len()
            + self.original.as_ref().map_or(0, String::len)
            + self.effective_options.as_ref().map_or(0, String::len)
            + self.applied_receipt.as_ref().map_or(0, String::len)
            + self.terminal_result.as_ref().map_or(0, String::len)
    }
    fn decode(self) -> Result<HostCommandRecord, InboxError> {
        fn parse<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, InboxError> {
            serde_json::from_str(value).map_err(|_| InboxError::Corrupt)
        }
        Ok(HostCommandRecord {
            command_id: self.command_id.parse().map_err(|_| InboxError::Corrupt)?,
            conversation_id: self
                .conversation_id
                .parse()
                .map_err(|_| InboxError::Corrupt)?,
            input_kind: self.input_kind,
            origin: parse(&self.origin)?,
            request_digest: self.request_digest,
            original: self.original.as_deref().map(parse).transpose()?,
            effective_options: self.effective_options.as_deref().map(parse).transpose()?,
            phase: parse(&format!("\"{}\"", self.phase))?,
            applied_receipt: self.applied_receipt.as_deref().map(parse).transpose()?,
            terminal_result: self.terminal_result.as_deref().map(parse).transpose()?,
            result_digest: self.result_digest,
            accepted_at: UnixMillis::try_from(self.accepted_at).map_err(|_| InboxError::Corrupt)?,
            updated_at: UnixMillis::try_from(self.updated_at).map_err(|_| InboxError::Corrupt)?,
        })
    }
}

fn read_record(
    connection: &Connection,
    id: CommandId,
) -> Result<Option<HostCommandRecord>, InboxError> {
    connection
        .query_row(
            "SELECT * FROM host_commands WHERE command_id=?1",
            [id.to_string()],
            RawRecord::read,
        )
        .optional()?
        .map(RawRecord::decode)
        .transpose()
}
