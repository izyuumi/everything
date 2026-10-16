mod control_events;
mod foreground;
mod native;
mod scheduled;
mod steering;

pub use control_events::{PermissionPublicationReceipt, TaskPublicationReceipt};
pub use native::{
    LoopLimits, LoopStatus, NativeRunInfo, PauseReason, ProviderResume, ToolRunSetup,
};
pub use scheduled::{ScheduledResultCommand, ScheduledResultReceipt};
pub use steering::{TaskInput, TaskInputCommand, TaskInputReceipt, TaskInputState};

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex as AdmissionMutex, Weak};

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::model::{
    self, ContentBlock, ModelError, ModelEvent, ModelProvider, ModelRequest, ModelResponse,
    ProviderContinuation, StopReason, TokenUsage, ToolChoice,
};
use crate::protocol::{
    CommandId, ConversationEvent, ConversationId, ConversationMessage, EventEnvelope, EventId,
    EventSequence, MessageContent, MessageId, MessageRole, ProtocolError, ToolCallId, UnixMillis,
};
use crate::storage::{
    AppendRequest, ConversationRow, ConversationStateUpdate, EventInput, EventRow, StorageError,
    Store,
};

pub const MAX_PUBLIC_EVENT_BYTES: usize = 256 * 1_024;
pub const MAX_CHAT_TEXT_BYTES: usize = 128 * 1_024;
const MAX_INSTRUCTIONS_BYTES: usize = 32 * 1_024;
const MAX_CHECKPOINT_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_HISTORY_BYTES: usize = 64 * 1_024 * 1_024;
const MAX_HISTORY_EVENTS: usize = 65_536;
const CHECKPOINT_VERSION: u16 = 2;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Context(#[from] crate::context::ContextError),
    #[error(transparent)]
    Broker(#[from] crate::tools::broker::BrokerError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Service(#[from] crate::service::ServiceError),
    #[error("invalid conversation data: {0}")]
    InvalidData(&'static str),
    #[error("{0} exceeds its size limit")]
    ResourceLimit(&'static str),
    #[error("command {0} was previously used for different contents")]
    CommandConflict(CommandId),
    #[error(
        "conversation has pending command {0}; resume it explicitly before sending another message"
    )]
    PendingTurn(CommandId),
    #[error("conversation has no pending text turn")]
    NoPendingTurn,
    #[error(
        "conversation {0} initialization is incomplete; retry init with its original title and configuration"
    )]
    IncompleteConversation(ConversationId),
    #[error("this conversation already has an active turn")]
    Busy,
    #[error("native tool/context execution requires its matching saved setup")]
    NativeSetupRequired,
    #[error("legacy provider continuation cannot enter tool mode; create a new conversation")]
    LegacyContinuation,
    #[error("provider or conversation configuration does not match")]
    ConfigurationMismatch,
    #[error("text-only conversations cannot execute or resume tool calls")]
    ToolsUnsupported,
    #[error("host storage worker failed")]
    Worker,
    #[error(
        "operation cancelled; inspect the conversation before resuming an uncertain provider request"
    )]
    Cancelled,
    #[error("foreground host storage is supported on Unix platforms")]
    UnsupportedPlatform,
}

impl AgentError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Model(ModelError::Cancelled))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
}

impl ProviderKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "url", rename_all = "snake_case")]
pub enum ProviderEndpoint {
    Official,
    Loopback(String),
}

impl ProviderEndpoint {
    pub fn loopback(value: &str) -> Result<Self, AgentError> {
        let url = reqwest::Url::parse(value)
            .map_err(|_| AgentError::InvalidData("invalid loopback endpoint"))?;
        let loopback = url
            .host_str()
            .and_then(|host| {
                host.trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .ok()
            })
            .is_some_and(|address| address.is_loopback());
        if !loopback
            || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || value.len() > 2_048
        {
            return Err(AgentError::InvalidData(
                "endpoint must use an explicit loopback IP and HTTP(S) without credentials, query, or fragment",
            ));
        }
        Ok(Self::Loopback(url.to_string()))
    }

    fn validate(&self) -> Result<(), AgentError> {
        if let Self::Loopback(url) = self
            && Self::loopback(url)? != *self
        {
            return Err(AgentError::InvalidData("endpoint is not canonical"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationConfig {
    pub provider: ProviderKind,
    pub model: String,
    pub instructions: String,
    pub endpoint: ProviderEndpoint,
    pub max_output_tokens: u32,
    pub temperature: Option<f32>,
}

impl ConversationConfig {
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.instructions.trim().is_empty() || self.instructions.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(AgentError::InvalidData(
                "instructions require 1 to 32768 bytes",
            ));
        }
        self.endpoint.validate()?;
        let mut request = ModelRequest::new(
            self.model.clone(),
            vec![model::Message::text(model::MessageRole::User, "validate")?],
        )?;
        request.instructions = Some(self.instructions.clone());
        request.max_output_tokens = Some(self.max_output_tokens);
        request.temperature = self.temperature;
        request.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextCommand {
    pub conversation_id: ConversationId,
    pub command_id: CommandId,
    pub text: String,
}

impl TextCommand {
    fn validate(&self) -> Result<(), AgentError> {
        validate_text(&self.text)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordedMessage {
    pub event_id: EventId,
    pub sequence: EventSequence,
    pub causation_id: Option<CommandId>,
    pub message: ConversationMessage,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationInfo {
    pub id: ConversationId,
    pub title: String,
    pub provider: ProviderKind,
    pub model: String,
    pub endpoint: ProviderEndpoint,
    pub last_sequence: u64,
    pub checkpoint_revision: u64,
    pub message_count: usize,
    pub pending_command_id: Option<CommandId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_status: Option<LoopStatus>,
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub info: ConversationInfo,
    pub config: ConversationConfig,
    pub messages: Vec<RecordedMessage>,
    checkpoint: Checkpoint,
}

#[derive(Clone, Debug)]
pub enum TextAcceptance {
    Pending {
        command_id: CommandId,
        message_id: MessageId,
        duplicate: bool,
    },
    Completed {
        command_id: CommandId,
        message: RecordedMessage,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnSummary {
    pub command_id: CommandId,
    pub message_id: MessageId,
    pub response_id: String,
    pub model: String,
    pub usage: TokenUsage,
    pub stop_reason: StopReason,
}

#[derive(Clone, Debug)]
pub enum TurnEvent {
    ProvisionalText {
        index: u32,
        delta: String,
    },
    Committed {
        message: ConversationMessage,
        summary: TurnSummary,
    },
    ContextBuilt {
        usage: crate::context::ContextUsage,
        citations: Vec<crate::context::MemoryCitation>,
        omitted_memory: Vec<crate::context::MemoryCitation>,
        continuation: crate::context::ContinuationDisposition,
    },
    ModelAttemptStarted {
        attempt_id: CommandId,
    },
    ToolCallRecorded {
        call: crate::protocol::ToolCall,
    },
    ToolResultRecorded {
        result: crate::protocol::ToolResult,
    },
    AwaitingPermission {
        call_id: ToolCallId,
        approval: Box<crate::permissions::ApprovalRequest>,
    },
    Paused {
        reason: PauseReason,
    },
    RunCompleted {
        run: NativeRunInfo,
    },
    ToolBlocked {
        call_id: ToolCallId,
        reason: PauseReason,
        diagnostic: String,
    },
}

pub type TurnStream = Pin<Box<dyn Stream<Item = Result<TurnEvent, AgentError>> + Send>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingText {
    command: TextCommand,
    message_id: MessageId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    version: u16,
    config: ConversationConfig,
    message_head: u64,
    message_count: usize,
    pending: Option<PendingText>,
    continuation: Option<ProviderContinuation>,
    last_turn: Option<TurnSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native: Option<native::NativeCheckpoint>,
    #[serde(default, skip_serializing_if = "steering::InputInbox::is_empty")]
    inputs: steering::InputInbox,
}

#[derive(Clone)]
pub struct Agent {
    store: Store,
    execution: Arc<AdmissionMutex<HashMap<ConversationId, Weak<Mutex<()>>>>>,
    acceptance: Arc<Mutex<()>>,
    writes: Arc<AdmissionMutex<HashMap<ConversationId, Weak<Mutex<()>>>>>,
    #[cfg(unix)]
    _foreground: Option<Arc<crate::service::StateLease>>,
}

impl Agent {
    pub fn store(&self) -> Store {
        self.store.clone()
    }

    #[cfg(unix)]
    pub fn state_directory(&self) -> Option<&Path> {
        self._foreground.as_ref().map(|lease| lease.path())
    }

    pub fn from_store(store: Store) -> Self {
        Self {
            store,
            execution: Arc::new(AdmissionMutex::new(HashMap::new())),
            acceptance: Arc::new(Mutex::new(())),
            writes: Arc::new(AdmissionMutex::new(HashMap::new())),
            #[cfg(unix)]
            _foreground: None,
        }
    }

    pub async fn open(path: impl AsRef<Path>) -> Result<Self, AgentError> {
        #[cfg(unix)]
        {
            let lease = crate::service::StateLease::acquire(path).await?;
            blocking(move || {
                let database = lease.validate_database()?;
                let store =
                    Store::open_validated(lease.path().join("everything.sqlite3"), database)?;
                Ok(Self::from_leased_store(store, lease))
            })
            .await
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(AgentError::UnsupportedPlatform)
        }
    }

    #[cfg(unix)]
    pub(crate) fn from_leased_store(store: Store, lease: Arc<crate::service::StateLease>) -> Self {
        let mut agent = Self::from_store(store);
        agent._foreground = Some(lease);
        agent
    }

    pub async fn create_conversation(
        &self,
        id: ConversationId,
        command_id: CommandId,
        title: String,
        config: ConversationConfig,
    ) -> Result<Conversation, AgentError> {
        let acceptance = self.acceptance.clone().lock_owned().await;
        let write = self.write_gate(id)?.lock_owned().await;
        let host = self.clone();
        let store = self.store.clone();
        blocking(move || {
            let _host = host;
            let _acceptance = acceptance;
            let _write = write;
            config.validate()?;
            if title.trim().is_empty() || title.len() > 4_096 || title.contains('\0') {
                return Err(AgentError::InvalidData("title requires 1 to 4096 bytes without NUL"));
            }
            if let Some(original) = store.command_request(&command_id.to_string())? {
                if original.conversation_id != id.to_string() || original.events.len() != 1 {
                    return Err(AgentError::CommandConflict(command_id));
                }
                let event: EventEnvelope = serde_json::from_value(original.events[0].payload.clone())
                    .map_err(|_| AgentError::CommandConflict(command_id))?;
                if event.causation_id != Some(command_id)
                    || !matches!(event.payload, ConversationEvent::ConversationCreated { title: Some(ref original) } if *original == title)
                {
                    return Err(AgentError::CommandConflict(command_id));
                }
                let conversation = load_conversation(&store, id)?;
                if conversation.config != config {
                    return Err(AgentError::ConfigurationMismatch);
                }
                return Ok(conversation);
            }
            let now = UnixMillis::now()?;
            match store.conversation(&id.to_string())? {
                Some(existing) if existing.last_sequence == 0 && existing.title == title
                    && store.conversation_state(&id.to_string())?.is_none() => {}
                Some(_) => return Err(AgentError::InvalidData("conversation ID already exists")),
                None => { store.create_conversation(&id.to_string(), &title, timestamp(now)?)?; }
            }
            let checkpoint = Checkpoint {
                version: CHECKPOINT_VERSION, config, message_head: 0, message_count: 0,
                pending: None, continuation: None, last_turn: None, native: None, inputs: Default::default(),
            };
            let event = event_input(id, 1, command_id, now, ConversationEvent::ConversationCreated { title: Some(title) })?;
            store.append_with_state(
                &AppendRequest { conversation_id: id.to_string(), command_id: command_id.to_string(), expected_sequence: 0, events: vec![event] },
                &ConversationStateUpdate { expected_revision: 0, payload: checkpoint_payload(&checkpoint)? },
            )?;
            load_conversation(&store, id)
        }).await
    }

    pub async fn conversation(&self, id: ConversationId) -> Result<Conversation, AgentError> {
        let host = self.clone();
        let store = self.store.clone();
        blocking(move || {
            let _host = host;
            load_conversation(&store, id)
        })
        .await
    }

    pub async fn conversations(&self, limit: usize) -> Result<Vec<ConversationInfo>, AgentError> {
        let host = self.clone();
        let store = self.store.clone();
        blocking(move || {
            let _host = host;
            store
                .conversations(limit)?
                .into_iter()
                .map(|row| {
                    let id: ConversationId = row.id.parse()?;
                    let snapshot = store.conversation_snapshot(&row.id, row.last_sequence, 1)?;
                    let state = snapshot
                        .state
                        .ok_or(AgentError::IncompleteConversation(id))?;
                    let checkpoint = decode_checkpoint(state.payload)?;
                    info(snapshot.conversation, state.revision, &checkpoint, id)
                })
                .collect()
        })
        .await
    }

    pub async fn accept_text(&self, command: TextCommand) -> Result<TextAcceptance, AgentError> {
        let acceptance = self.acceptance.clone().lock_owned().await;
        let write = self.write_gate(command.conversation_id)?.lock_owned().await;
        let host = self.clone();
        let store = self.store.clone();
        blocking(move || {
            let _host = host;
            let _acceptance = acceptance;
            let _write = write;
            command.validate()?;
            let conversation = load_conversation(&store, command.conversation_id)?;
            if let Some(original) = store.command_request(&command.command_id.to_string())? {
                if original.conversation_id != command.conversation_id.to_string()
                    || original.events.len() != 1
                {
                    return Err(AgentError::CommandConflict(command.command_id));
                }
                let row = &original.events[0];
                let event: EventEnvelope = serde_json::from_value(row.payload.clone())
                    .map_err(|_| AgentError::CommandConflict(command.command_id))?;
                let ConversationEvent::MessageAppended { message } = &event.payload else {
                    return Err(AgentError::CommandConflict(command.command_id));
                };
                if event.conversation_id != command.conversation_id
                    || event.causation_id != Some(command.command_id)
                    || message.role != MessageRole::User
                    || message.content
                        != vec![MessageContent::Text {
                            text: command.text.clone(),
                        }]
                {
                    return Err(AgentError::CommandConflict(command.command_id));
                }
                if let Some(pending) = &conversation.checkpoint.pending
                    && pending.command == command
                    && pending.message_id == message.id
                {
                    return Ok(TextAcceptance::Pending {
                        command_id: command.command_id,
                        message_id: message.id,
                        duplicate: true,
                    });
                }
                let response = conversation
                    .messages
                    .iter()
                    .rev()
                    .find(|message| {
                        message.causation_id == Some(command.command_id)
                            && message.message.role == MessageRole::Assistant
                    })
                    .cloned()
                    .ok_or(AgentError::InvalidData(
                        "accepted command has no pending turn or completed response",
                    ))?;
                return Ok(TextAcceptance::Completed {
                    command_id: command.command_id,
                    message: response,
                });
            }
            if let Some(pending) = &conversation.checkpoint.pending {
                return Err(AgentError::PendingTurn(pending.command.command_id));
            }
            let now = UnixMillis::now()?;
            let message = ConversationMessage {
                id: MessageId::new(),
                role: MessageRole::User,
                content: vec![MessageContent::Text {
                    text: command.text.clone(),
                }],
                created_at: now,
            };
            if conversation.checkpoint.native.is_none() {
                build_request(&conversation, Some(&message))?;
            }
            let sequence = next_sequence(conversation.info.last_sequence)?;
            let mut checkpoint = conversation.checkpoint;
            checkpoint.message_head = sequence;
            checkpoint.message_count += 1;
            checkpoint.pending = Some(PendingText {
                command: command.clone(),
                message_id: message.id,
            });
            let event = event_input(
                command.conversation_id,
                sequence,
                command.command_id,
                now,
                ConversationEvent::MessageAppended {
                    message: message.clone(),
                },
            )?;
            store.append_with_state(
                &AppendRequest {
                    conversation_id: command.conversation_id.to_string(),
                    command_id: command.command_id.to_string(),
                    expected_sequence: conversation.info.last_sequence,
                    events: vec![event],
                },
                &ConversationStateUpdate {
                    expected_revision: conversation.info.checkpoint_revision,
                    payload: checkpoint_payload(&checkpoint)?,
                },
            )?;
            Ok(TextAcceptance::Pending {
                command_id: command.command_id,
                message_id: message.id,
                duplicate: false,
            })
        })
        .await
    }

    pub async fn run_pending(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
        provider: Arc<dyn ModelProvider>,
        cancellation: CancellationToken,
    ) -> Result<TurnStream, AgentError> {
        self.run_native(conversation_id, command_id, provider, None, cancellation)
            .await
    }

    fn admit(
        &self,
        id: ConversationId,
    ) -> Result<Arc<tokio::sync::OwnedMutexGuard<()>>, AgentError> {
        let gate = {
            let mut active = self.execution.lock().map_err(|_| AgentError::Worker)?;
            active.retain(|_, gate| gate.strong_count() > 0);
            match active.get(&id).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    if active.len() >= 1_024 {
                        return Err(AgentError::Busy);
                    }
                    let gate = Arc::new(Mutex::new(()));
                    active.insert(id, Arc::downgrade(&gate));
                    gate
                }
            }
        };
        Ok(Arc::new(
            gate.try_lock_owned().map_err(|_| AgentError::Busy)?,
        ))
    }

    async fn commit_response(
        &self,
        conversation: Conversation,
        command_id: CommandId,
        response: ModelResponse,
        cancellation: CancellationToken,
        admission: Arc<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<(ConversationMessage, TurnSummary), AgentError> {
        let write = self.write_gate(conversation.info.id)?.lock_owned().await;
        let host = self.clone();
        let store = self.store.clone();
        blocking(move || {
            let _host = host;
            let _admission = admission;
            let _write = write;
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            response.validate()?;
            if response.stop_reason == StopReason::ToolCall {
                return Err(AgentError::ToolsUnsupported);
            }
            let mut content = Vec::with_capacity(response.message.content.len());
            let mut bytes = 0_usize;
            for block in response.message.content {
                let ContentBlock::Text(text) = block else {
                    return Err(AgentError::ToolsUnsupported);
                };
                bytes = bytes.saturating_add(text.len());
                if bytes > MAX_CHAT_TEXT_BYTES {
                    return Err(AgentError::ResourceLimit("assistant text"));
                }
                content.push(MessageContent::Text { text });
            }
            if let Some(continuation) = &response.continuation
                && continuation.provider() != conversation.config.provider.id()
            {
                return Err(AgentError::ConfigurationMismatch);
            }
            let now = UnixMillis::now()?;
            let message = ConversationMessage {
                id: MessageId::new(),
                role: MessageRole::Assistant,
                content,
                created_at: now,
            };
            let summary = TurnSummary {
                command_id,
                message_id: message.id,
                response_id: response.id,
                model: response.model,
                usage: response.usage,
                stop_reason: response.stop_reason,
            };
            let sequence = next_sequence(conversation.info.last_sequence)?;
            let event = event_input(
                conversation.info.id,
                sequence,
                command_id,
                now,
                ConversationEvent::MessageAppended {
                    message: message.clone(),
                },
            )?;
            let mut checkpoint = conversation.checkpoint;
            checkpoint.message_head = sequence;
            checkpoint.message_count += 1;
            checkpoint.pending = None;
            checkpoint.continuation = response.continuation;
            checkpoint.last_turn = Some(summary.clone());
            store.append_with_state(
                &AppendRequest {
                    conversation_id: conversation.info.id.to_string(),
                    command_id: CommandId::new().to_string(),
                    expected_sequence: conversation.info.last_sequence,
                    events: vec![event],
                },
                &ConversationStateUpdate {
                    expected_revision: conversation.info.checkpoint_revision,
                    payload: checkpoint_payload(&checkpoint)?,
                },
            )?;
            Ok((message, summary))
        })
        .await
    }
}

fn load_conversation(store: &Store, id: ConversationId) -> Result<Conversation, AgentError> {
    let mut page_size = 64;
    let snapshot = loop {
        match store.conversation_snapshot(&id.to_string(), 0, page_size) {
            Err(StorageError::ReadBudgetExceeded) if page_size > 1 => page_size /= 2,
            result => break result?,
        }
    };
    let state = snapshot
        .state
        .ok_or(AgentError::IncompleteConversation(id))?;
    let checkpoint = decode_checkpoint(state.payload)?;
    let head = snapshot.conversation.last_sequence;
    let mut rows = snapshot.events;
    let mut history_bytes = rows.iter().try_fold(0_usize, |bytes, row| {
        let bytes = bytes.saturating_add(encoded_len(row, MAX_HISTORY_BYTES)?);
        if bytes > MAX_HISTORY_BYTES {
            return Err(AgentError::ResourceLimit("history bytes"));
        }
        Ok(bytes)
    })?;
    let mut cursor = rows.last().map_or(0, |row| row.sequence);
    while cursor < head {
        let page = loop {
            match store.events(&id.to_string(), cursor, page_size) {
                Err(StorageError::ReadBudgetExceeded) if page_size > 1 => page_size /= 2,
                result => break result?,
            }
        };
        let page: Vec<_> = page
            .into_iter()
            .take_while(|row| row.sequence <= head)
            .collect();
        let next = page.last().map_or(cursor, |row| row.sequence);
        if next <= cursor {
            return Err(AgentError::InvalidData(
                "conversation journal has a sequence gap",
            ));
        }
        for row in &page {
            history_bytes = history_bytes.saturating_add(encoded_len(row, MAX_HISTORY_BYTES)?);
            if history_bytes > MAX_HISTORY_BYTES {
                return Err(AgentError::ResourceLimit("history bytes"));
            }
        }
        rows.extend(page);
        if rows.len() > MAX_HISTORY_EVENTS {
            return Err(AgentError::ResourceLimit("history event count"));
        }
        cursor = next;
    }
    let mut messages = Vec::new();
    let mut expected = 1;
    let mut total_bytes = 0_usize;
    let mut message_head = 0;
    let mut saw_creation = false;
    let mut mapping = MessageMapping::default();
    for row in rows {
        if row.sequence != expected {
            return Err(AgentError::InvalidData(
                "conversation journal has a sequence gap",
            ));
        }
        expected = next_sequence(expected)?;
        total_bytes = total_bytes.saturating_add(encoded_len(&row, MAX_HISTORY_BYTES)?);
        if total_bytes > MAX_HISTORY_BYTES {
            return Err(AgentError::ResourceLimit("history bytes"));
        }
        let event = decode_event(&row, id)?;
        checkpoint.inputs.validate_event(&event)?;
        match event.payload {
            ConversationEvent::ConversationCreated { .. } => {
                if saw_creation || row.sequence != 1 {
                    return Err(AgentError::InvalidData(
                        "invalid conversation creation event",
                    ));
                }
                saw_creation = true;
            }
            ConversationEvent::MessageAppended { message } => {
                model_message(&message, &mut mapping)?;
                message_head = row.sequence;
                messages.push(RecordedMessage {
                    event_id: event.event_id,
                    sequence: event.sequence,
                    causation_id: event.causation_id,
                    message,
                });
            }
            _ => {}
        }
    }
    if !saw_creation
        || checkpoint.message_head != message_head
        || checkpoint.message_count != messages.len()
    {
        return Err(AgentError::InvalidData(
            "private checkpoint does not match conversation messages",
        ));
    }
    if let Some(pending) = &checkpoint.pending {
        let latest = messages
            .iter()
            .find(|entry| entry.message.id == pending.message_id)
            .ok_or(AgentError::InvalidData("pending turn has no user message"))?;
        if checkpoint.native.is_none()
            && messages.last().map(|entry| entry.message.id) != Some(pending.message_id)
        {
            return Err(AgentError::InvalidData(
                "legacy pending message is not the latest message",
            ));
        }
        if pending.command.conversation_id != id
            || latest.message.id != pending.message_id
            || latest.causation_id != Some(pending.command.command_id)
            || latest.message.role != MessageRole::User
            || latest.message.content
                != vec![MessageContent::Text {
                    text: pending.command.text.clone(),
                }]
        {
            return Err(AgentError::InvalidData(
                "pending command does not match its durable user message",
            ));
        }
    }
    if let Some(native) = &checkpoint.native {
        native.validate(&checkpoint, &messages)?;
    }
    checkpoint.inputs.validate_target(
        id,
        checkpoint
            .pending
            .as_ref()
            .map(|pending| pending.command.command_id),
        head,
    )?;
    let info = info(snapshot.conversation, state.revision, &checkpoint, id)?;
    Ok(Conversation {
        config: checkpoint.config.clone(),
        info,
        messages,
        checkpoint,
    })
}

fn decode_checkpoint(value: serde_json::Value) -> Result<Checkpoint, AgentError> {
    let mut checkpoint: Checkpoint = serde_json::from_value(value)
        .map_err(|_| AgentError::InvalidData("malformed private conversation checkpoint"))?;
    if !matches!(checkpoint.version, 1 | CHECKPOINT_VERSION)
        || checkpoint.message_count > MAX_HISTORY_EVENTS
        || (checkpoint.version == 1 && checkpoint.native.is_some())
    {
        return Err(AgentError::InvalidData(
            "unsupported private conversation checkpoint",
        ));
    }
    checkpoint.version = CHECKPOINT_VERSION;
    checkpoint.config.validate()?;
    checkpoint.inputs.validate()?;
    if let Some(continuation) = &checkpoint.continuation {
        continuation.validate()?;
        if continuation.provider() != checkpoint.config.provider.id() {
            return Err(AgentError::ConfigurationMismatch);
        }
    }
    if let Some(pending) = &checkpoint.pending {
        pending.command.validate()?;
    }
    if let Some(summary) = &checkpoint.last_turn {
        summary.usage.validate()?;
    }
    Ok(checkpoint)
}

fn info(
    row: ConversationRow,
    revision: u64,
    checkpoint: &Checkpoint,
    id: ConversationId,
) -> Result<ConversationInfo, AgentError> {
    if row.id != id.to_string() || checkpoint.message_head > row.last_sequence || revision == 0 {
        return Err(AgentError::InvalidData(
            "checkpoint identity or sequence is invalid",
        ));
    }
    Ok(ConversationInfo {
        id,
        title: row.title,
        provider: checkpoint.config.provider,
        model: checkpoint.config.model.clone(),
        endpoint: checkpoint.config.endpoint.clone(),
        last_sequence: row.last_sequence,
        checkpoint_revision: revision,
        message_count: checkpoint.message_count,
        pending_command_id: checkpoint
            .pending
            .as_ref()
            .map(|pending| pending.command.command_id),
        native_status: checkpoint
            .native
            .as_ref()
            .map(native::NativeCheckpoint::status),
    })
}

fn build_request(
    conversation: &Conversation,
    additional: Option<&ConversationMessage>,
) -> Result<ModelRequest, AgentError> {
    let mut mapping = MessageMapping::default();
    let mut messages = conversation
        .messages
        .iter()
        .map(|message| model_message(&message.message, &mut mapping))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(message) = additional {
        messages.push(model_message(message, &mut mapping)?);
    }
    let mut request = ModelRequest::new(conversation.config.model.clone(), messages)?;
    request.instructions = Some(conversation.config.instructions.clone());
    request.max_output_tokens = Some(conversation.config.max_output_tokens);
    request.temperature = conversation.config.temperature;
    request.tool_choice = ToolChoice::None;
    request.continuation = conversation.checkpoint.continuation.clone();
    request.validate()?;
    Ok(request)
}

#[derive(Default)]
struct MessageMapping {
    calls: BTreeMap<ToolCallId, model::ToolCall>,
    provider_ids: std::collections::HashSet<String>,
    completed: std::collections::HashSet<ToolCallId>,
}

fn model_message(
    message: &ConversationMessage,
    mapping: &mut MessageMapping,
) -> Result<model::Message, AgentError> {
    let role = match message.role {
        MessageRole::System => model::MessageRole::System,
        MessageRole::User => model::MessageRole::User,
        MessageRole::Assistant => model::MessageRole::Assistant,
        MessageRole::Tool => model::MessageRole::Tool,
    };
    let mut content = Vec::with_capacity(message.content.len());
    for block in &message.content {
        match block {
            MessageContent::Text { text } => content.push(ContentBlock::Text(text.clone())),
            MessageContent::ToolCall(call) => {
                let provider_id = call
                    .provider_call_id
                    .clone()
                    .ok_or(AgentError::InvalidData(
                        "host tool call cannot become a native provider call",
                    ))?;
                if mapping.calls.contains_key(&call.id)
                    || !mapping.provider_ids.insert(provider_id.clone())
                {
                    return Err(AgentError::InvalidData(
                        "duplicate durable or provider tool call identifier",
                    ));
                }
                let converted =
                    model::ToolCall::new(provider_id, call.name.clone(), call.arguments.clone())?;
                mapping.calls.insert(call.id, converted.clone());
                content.push(ContentBlock::ToolCall(converted));
            }
            MessageContent::ToolResult(result) => {
                let call =
                    mapping
                        .calls
                        .get(&result.tool_call_id)
                        .ok_or(AgentError::InvalidData(
                            "tool result has no durable native call mapping",
                        ))?;
                if !mapping.completed.insert(result.tool_call_id) {
                    return Err(AgentError::InvalidData("duplicate durable tool result"));
                }
                let output = serde_json::from_str(&result.content)
                    .map_err(|_| AgentError::InvalidData("native tool result is not valid JSON"))?;
                content.push(ContentBlock::ToolResult(model::ToolResult::new(
                    call.id.clone(),
                    output,
                    result.is_error,
                )?));
            }
        }
    }
    let message = model::Message { role, content };
    message.validate()?;
    Ok(message)
}

fn decode_event(
    row: &EventRow,
    conversation_id: ConversationId,
) -> Result<EventEnvelope, AgentError> {
    let event: EventEnvelope = serde_json::from_value(row.payload.clone())
        .map_err(|_| AgentError::InvalidData("malformed public conversation event"))?;
    if event.event_id.to_string() != row.id
        || event.conversation_id != conversation_id
        || row.conversation_id != conversation_id.to_string()
        || event.sequence.get() != row.sequence
        || timestamp(event.occurred_at)? != row.occurred_at
        || event_kind(&event.payload) != row.kind
    {
        return Err(AgentError::InvalidData(
            "conversation event metadata mismatch",
        ));
    }
    Ok(event)
}

fn event_input(
    id: ConversationId,
    sequence: u64,
    command: CommandId,
    now: UnixMillis,
    payload: ConversationEvent,
) -> Result<EventInput, AgentError> {
    let event = EventEnvelope::new(
        id,
        EventSequence::try_from(sequence)?,
        now,
        Some(command),
        payload,
    );
    encoded_len(&event, MAX_PUBLIC_EVENT_BYTES)?;
    Ok(EventInput {
        id: event.event_id.to_string(),
        kind: event_kind(&event.payload).to_owned(),
        payload: serde_json::to_value(event)
            .map_err(|_| AgentError::InvalidData("could not encode public event"))?,
        occurred_at: timestamp(now)?,
    })
}

fn event_kind(event: &ConversationEvent) -> &'static str {
    match event {
        ConversationEvent::ConversationCreated { .. } => "conversation_created",
        ConversationEvent::MessageAppended { .. } => "message_appended",
        ConversationEvent::AssistantTextDelta { .. } => "assistant_text_delta",
        ConversationEvent::TaskCreated { .. } => "task_created",
        ConversationEvent::TaskStatusChanged { .. } => "task_status_changed",
        ConversationEvent::ToolRequested { .. } => "tool_requested",
        ConversationEvent::ToolCompleted { .. } => "tool_completed",
        ConversationEvent::PermissionRequested { .. } => "permission_requested",
        ConversationEvent::PermissionDecided { .. } => "permission_decided",
        ConversationEvent::SteeringReceived { .. } => "steering_received",
        ConversationEvent::TaskWaitRequested { .. } => "task_wait_requested",
        ConversationEvent::ScheduledRunFinished { .. } => "scheduled_run_finished",
        ConversationEvent::SubagentFinished { .. } => "subagent_finished",
    }
}

fn checkpoint_payload(checkpoint: &Checkpoint) -> Result<serde_json::Value, AgentError> {
    encoded_len(checkpoint, MAX_CHECKPOINT_BYTES)?;
    serde_json::to_value(checkpoint)
        .map_err(|_| AgentError::InvalidData("could not encode private checkpoint"))
}

fn validate_text(text: &str) -> Result<(), AgentError> {
    if text.trim().is_empty() || text.contains('\0') || text.len() > MAX_CHAT_TEXT_BYTES {
        return Err(AgentError::InvalidData(
            "text requires 1 to 131072 UTF-8 bytes without NUL",
        ));
    }
    Ok(())
}

fn next_sequence(sequence: u64) -> Result<u64, AgentError> {
    sequence
        .checked_add(1)
        .filter(|next| *next <= i64::MAX as u64)
        .ok_or(AgentError::ResourceLimit("conversation sequence"))
}

fn timestamp(now: UnixMillis) -> Result<i64, AgentError> {
    i64::try_from(now.get()).map_err(|_| AgentError::InvalidData("timestamp exceeds SQLite range"))
}

fn encoded_len(value: &impl Serialize, maximum: usize) -> Result<usize, AgentError> {
    let mut writer = SizeWriter { length: 0, maximum };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| AgentError::ResourceLimit("serialized conversation data"))?;
    Ok(writer.length)
}

struct SizeWriter {
    length: usize,
    maximum: usize,
}

impl Write for SizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.length = self
            .length
            .checked_add(bytes.len())
            .filter(|length| *length <= self.maximum)
            .ok_or_else(|| io::Error::other("serialized size limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn blocking<T, F>(operation: F) -> Result<T, AgentError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AgentError> + Send + 'static,
{
    tokio::runtime::Handle::try_current()
        .map_err(|_| AgentError::Worker)?
        .spawn_blocking(operation)
        .await
        .map_err(|_| AgentError::Worker)?
}
