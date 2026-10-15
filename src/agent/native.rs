use std::collections::BTreeSet;
use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::*;
use crate::context::{
    BuiltContext, CompactionRecord, ContextBudget, ContextBuilder, ContextInput,
    ContinuationBinding, HistoryEntry, MemoryExcerpt, TaskContext,
};
use crate::permissions::{ApprovalRequest, ExecutionMode};
use crate::protocol::{OperationId, TaskId, ToolCall, ToolResult};
use crate::storage::TaskState;
use crate::tools::{
    ToolRegistry,
    broker::{BrokerOutcome, OperationResult, TaskToolBroker, ToolInvocation},
};

const MAX_RESULT_BYTES: usize = 32 * 1_024;
const MAX_PREVIEW_BYTES: usize = 4 * 1_024;

type Admission = Arc<tokio::sync::OwnedMutexGuard<()>>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopLimits {
    pub max_model_attempts: u32,
    pub max_tool_calls: u32,
    pub max_calls_per_round: usize,
    pub max_parallel_calls: usize,
    pub max_selected_tools: usize,
    pub max_charged_tokens: u64,
    pub max_duration_ms: u64,
}

impl Default for LoopLimits {
    fn default() -> Self {
        Self {
            max_model_attempts: 16,
            max_tool_calls: 64,
            max_calls_per_round: 16,
            max_parallel_calls: 8,
            max_selected_tools: 64,
            max_charged_tokens: 1_000_000,
            max_duration_ms: 3_600_000,
        }
    }
}

impl LoopLimits {
    pub fn validate(&self) -> Result<(), AgentError> {
        if !(1..=128).contains(&self.max_model_attempts)
            || self.max_tool_calls > 1_024
            || !(1..=32).contains(&self.max_calls_per_round)
            || !(1..=self.max_calls_per_round).contains(&self.max_parallel_calls)
            || !(1..=128).contains(&self.max_selected_tools)
            || self.max_charged_tokens == 0
            || !(1_000..=86_400_000).contains(&self.max_duration_ms)
        {
            return Err(AgentError::InvalidData("invalid native loop limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProviderResume {
    #[default]
    OnlyUnstarted,
    RetryUncertain,
}

pub struct ToolRunSetup {
    pub broker: TaskToolBroker,
    pub registry: Arc<ToolRegistry>,
    pub task: TaskContext,
    pub budget: ContextBudget,
    pub memory: Vec<MemoryExcerpt>,
    pub selected_tools: Vec<model::ToolDefinition>,
    pub limits: LoopLimits,
    pub provider_resume: ProviderResume,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum PauseReason {
    ProviderOutcomeUncertain,
    Cancellation,
    InterruptedTool,
    ModelAttemptLimit,
    ToolCallLimit,
    TokenLimit,
    DurationLimit,
    OperationInFlight { operation_id: String },
    ReconciliationRequired { operation_id: String },
    BrokerRejected { operation_id: String },
    InvalidModelResponse,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum LoopStatus {
    Ready,
    ProviderInFlight { attempt_id: CommandId },
    ExecutingTools,
    AwaitingPermission,
    Paused { reason: PauseReason },
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedMemory {
    chunk_id: String,
    title: String,
    heading: String,
    text: String,
    page_path: String,
    start_line: usize,
    end_line: usize,
}

impl From<&MemoryExcerpt> for SavedMemory {
    fn from(memory: &MemoryExcerpt) -> Self {
        Self {
            chunk_id: memory.chunk_id.clone(),
            title: memory.title.clone(),
            heading: memory.heading.clone(),
            text: memory.text.clone(),
            page_path: memory.provenance.page_path.clone(),
            start_line: memory.provenance.start_line,
            end_line: memory.provenance.end_line,
        }
    }
}

impl SavedMemory {
    fn excerpt(&self) -> MemoryExcerpt {
        MemoryExcerpt {
            chunk_id: self.chunk_id.clone(),
            title: self.title.clone(),
            heading: self.heading.clone(),
            text: self.text.clone(),
            provenance: crate::memory::Provenance {
                page_path: self.page_path.clone(),
                start_line: self.start_line,
                end_line: self.end_line,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    task_id: TaskId,
    workspace: String,
    mode: ExecutionMode,
    allowed_tools: Vec<String>,
    task: TaskContext,
    budget: ContextBudget,
    memory: Vec<SavedMemory>,
    initial_tools: Vec<model::ToolDefinition>,
    limits: LoopLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedContext {
    compaction: Option<CompactionRecord>,
    binding: Option<ContinuationBinding>,
    tools: Vec<model::ToolDefinition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NativeCheckpoint {
    context: SavedContext,
    turn: Option<NativeTurn>,
    #[serde(default)]
    last_run: Option<NativeRunInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeTurn {
    command_id: CommandId,
    definition: Definition,
    definition_digest: String,
    started_at_ms: u64,
    deadline_ms: u64,
    attempts: u32,
    tool_calls: u32,
    charged_tokens: u64,
    reported_input_tokens: u64,
    reported_output_tokens: u64,
    status: LoopStatus,
    round: Option<Round>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Round {
    assistant_message_id: MessageId,
    calls: Vec<CallCheckpoint>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallCheckpoint {
    call: ToolCall,
    phase: CallPhase,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum CallPhase {
    Pending,
    Dispatched,
    AwaitingPermission {
        approval: Box<ApprovalRequest>,
    },
    Result {
        result: ToolResult,
        stop: bool,
    },
    Blocked {
        reason: PauseReason,
        diagnostic: String,
    },
}

impl NativeCheckpoint {
    pub(super) fn status(&self) -> LoopStatus {
        self.turn
            .as_ref()
            .map_or(LoopStatus::Completed, |turn| turn.status.clone())
    }

    pub(super) fn validate(
        &self,
        checkpoint: &Checkpoint,
        messages: &[RecordedMessage],
    ) -> Result<(), AgentError> {
        if let Some(binding) = &self.context.binding {
            binding.continuation.validate()?;
            if checkpoint.continuation.as_ref() != Some(&binding.continuation) {
                return Err(AgentError::InvalidData(
                    "native context continuation does not match checkpoint",
                ));
            }
        } else if checkpoint.continuation.is_some() {
            return Err(AgentError::InvalidData(
                "native continuation lost its context binding",
            ));
        }
        if self.context.tools.len() > 128 {
            return Err(AgentError::ResourceLimit("selected tools"));
        }
        for definition in &self.context.tools {
            definition.validate()?;
        }
        if let Some(turn) = &self.turn {
            let pending = checkpoint
                .pending
                .as_ref()
                .ok_or(AgentError::InvalidData("native turn has no command"))?;
            turn.definition.limits.validate()?;
            ContextBuilder::new(turn.definition.budget.clone())?;
            if turn.command_id != pending.command.command_id
                || definition_digest(&turn.definition)? != turn.definition_digest
                || turn.deadline_ms < turn.started_at_ms
                || turn.deadline_ms - turn.started_at_ms != turn.definition.limits.max_duration_ms
                || turn.attempts > turn.definition.limits.max_model_attempts
                || turn.tool_calls > turn.definition.limits.max_tool_calls
            {
                return Err(AgentError::InvalidData(
                    "native turn identity or limits are invalid",
                ));
            }
            if let Some(round) = &turn.round {
                let assistant = messages
                    .iter()
                    .find(|message| message.message.id == round.assistant_message_id)
                    .ok_or(AgentError::InvalidData(
                        "native round lost its assistant message",
                    ))?;
                let calls: Vec<_> = assistant
                    .message
                    .content
                    .iter()
                    .filter_map(|block| {
                        if let MessageContent::ToolCall(call) = block {
                            Some(call)
                        } else {
                            None
                        }
                    })
                    .collect();
                if calls.len() != round.calls.len()
                    || calls.is_empty()
                    || calls.len() > turn.definition.limits.max_calls_per_round
                {
                    return Err(AgentError::InvalidData("native round call count mismatch"));
                }
                for (expected, call) in calls.iter().zip(&round.calls) {
                    if **expected != call.call {
                        return Err(AgentError::InvalidData("native round call mapping changed"));
                    }
                    if let CallPhase::Result { result, .. } = &call.phase {
                        if result.tool_call_id != call.call.id
                            || result.content.len() > MAX_RESULT_BYTES
                        {
                            return Err(AgentError::InvalidData(
                                "native tool result checkpoint is invalid",
                            ));
                        }
                        serde_json::from_str::<Value>(&result.content).map_err(|_| {
                            AgentError::InvalidData("native tool output checkpoint is invalid JSON")
                        })?;
                    }
                }
            } else if matches!(
                turn.status,
                LoopStatus::ExecutingTools | LoopStatus::AwaitingPermission
            ) {
                return Err(AgentError::InvalidData("native tool phase has no round"));
            }
        } else if checkpoint.pending.is_some() {
            return Ok(());
        }
        Ok(())
    }
}

impl Agent {
    pub async fn run_tools(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
        provider: Arc<dyn ModelProvider>,
        setup: ToolRunSetup,
        cancellation: CancellationToken,
    ) -> Result<TurnStream, AgentError> {
        self.run_native(
            conversation_id,
            command_id,
            provider,
            Some(setup),
            cancellation,
        )
        .await
    }

    pub(super) async fn run_native(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
        provider: Arc<dyn ModelProvider>,
        setup: Option<ToolRunSetup>,
        cancellation: CancellationToken,
    ) -> Result<TurnStream, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let admission = self.admit(conversation_id)?;
        let mut conversation = self.conversation(conversation_id).await?;
        let pending = conversation
            .checkpoint
            .pending
            .as_ref()
            .ok_or(AgentError::NoPendingTurn)?;
        if pending.command.command_id != command_id {
            return Err(AgentError::CommandConflict(command_id));
        }
        if provider.id() != conversation.config.provider.id() {
            return Err(AgentError::ConfigurationMismatch);
        }
        let setup = setup.map(Arc::new);
        if let Some(setup) = &setup {
            if !provider.capabilities().tool_calling
                && setup.limits.max_tool_calls > 0
                && !setup.selected_tools.is_empty()
            {
                return Err(AgentError::ToolsUnsupported);
            }
            let owner = self.clone();
            let preparation = Arc::clone(setup);
            let (captured, native) = blocking(move || {
                let definition = prepare_definition(&owner.store, &conversation, &preparation)?;
                let native = prepare_native(&conversation, definition)?;
                if native
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.round.is_none())
                {
                    let mut validated = conversation.checkpoint.clone();
                    validated.native = Some(native.clone());
                    let checkpoint = std::mem::replace(&mut conversation.checkpoint, validated);
                    build_context(&conversation, &preparation)?;
                    conversation.checkpoint = checkpoint;
                }
                Ok((conversation, native))
            })
            .await?;
            conversation = captured;
            let mut checkpoint = conversation.checkpoint.clone();
            checkpoint.native = Some(native);
            self.save_native(
                &mut conversation,
                checkpoint,
                Vec::new(),
                Arc::clone(&admission),
            )
            .await?;
        } else if conversation.checkpoint.native.is_some() {
            return Err(AgentError::NativeSetupRequired);
        }
        let agent = self.clone();
        let child = cancellation.child_token();
        let cancel_on_drop = child.clone().drop_guard();
        let stream = async_stream::try_stream! {
            let _cancel_on_drop = cancel_on_drop;
            let admission = admission;
            loop {
                if child.is_cancelled() { Err(AgentError::Cancelled)?; }
                if let Some(setup) = &setup {
                    let status = native(&conversation)?.turn.as_ref().ok_or(AgentError::NoPendingTurn)?.status.clone();
                    match status {
                        LoopStatus::ProviderInFlight { .. } if setup.provider_resume != ProviderResume::RetryUncertain => {
                            yield TurnEvent::Paused { reason: PauseReason::ProviderOutcomeUncertain }; break;
                        }
                        LoopStatus::ProviderInFlight { .. } => {
                            let mut checkpoint = conversation.checkpoint.clone(); turn_mut(&mut checkpoint)?.status = LoopStatus::Ready;
                            agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                        }
                        LoopStatus::Paused { reason } => {
                            if matches!(reason, PauseReason::OperationInFlight { .. }) || (setup.provider_resume == ProviderResume::RetryUncertain && matches!(reason, PauseReason::Cancellation | PauseReason::InterruptedTool)) {
                                let mut checkpoint = conversation.checkpoint.clone();
                                let turn = turn_mut(&mut checkpoint)?;
                                if let Some(round) = &mut turn.round {
                                    for call in &mut round.calls {
                                        if matches!(call.phase, CallPhase::Blocked { reason: PauseReason::OperationInFlight { .. }, .. }) { call.phase = CallPhase::Dispatched; }
                                    }
                                }
                                turn.status = if turn.round.is_some() { LoopStatus::ExecutingTools } else { LoopStatus::Ready };
                                agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                                continue;
                            } else { yield TurnEvent::Paused { reason }; break; }
                        }
                        LoopStatus::ExecutingTools | LoopStatus::AwaitingPermission => {
                            let events = agent.execute_round(&mut conversation, Arc::clone(setup), &child, Arc::clone(&admission)).await?;
                            let mut stop = false;
                            for event in events { stop |= matches!(event, TurnEvent::Paused { .. } | TurnEvent::AwaitingPermission { .. }); yield event; }
                            if stop { break; }
                            continue;
                        }
                        LoopStatus::Ready => {}
                        LoopStatus::Completed => break,
                    }
                }
                let prepared = if let Some(setup) = &setup {
                    let preparation = Arc::clone(setup); let owner = agent.clone();
                    let (captured, built, request) = blocking(move || {
                        ensure_task(&owner.store, preparation.broker.authority().task_id(), preparation.broker.authority().conversation_id())?;
                        let built = build_context(&conversation, &preparation)?;
                        let request = built.request.clone();
                        Ok((conversation, built, request))
                    }).await?;
                    conversation = captured;
                    if let Some(reason) = next_attempt_limit(&conversation, &built)? {
                        let mut checkpoint = conversation.checkpoint.clone(); turn_mut(&mut checkpoint)?.status = LoopStatus::Paused { reason: reason.clone() };
                        agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                        yield TurnEvent::Paused { reason }; break;
                    }
                    let attempt_id = CommandId::new();
                    let mut checkpoint = conversation.checkpoint.clone();
                    let state = checkpoint.native.as_mut().ok_or(AgentError::NativeSetupRequired)?;
                    state.context.compaction = built.compaction.clone();
                    if built.request.continuation.is_none() { state.context.binding = None; checkpoint.continuation = None; }
                    let turn = state.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
                    turn.attempts += 1;
                    turn.charged_tokens = turn.charged_tokens.checked_add(reservation(&built)).ok_or(AgentError::ResourceLimit("token accounting"))?;
                    turn.status = LoopStatus::ProviderInFlight { attempt_id };
                    agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                    yield TurnEvent::ContextBuilt { usage: built.usage.clone(), citations: built.citations.clone(), omitted_memory: built.omitted_memory.clone(), continuation: built.continuation.clone() };
                    yield TurnEvent::ModelAttemptStarted { attempt_id };
                    Prepared::Context { built: Box::new(built), request }
                } else {
                    let (captured, request) = blocking(move || {
                        let request = build_request(&conversation, None)?;
                        Ok((conversation, request))
                    }).await?;
                    conversation = captured;
                    Prepared::Legacy(request)
                };
                let deadline = run_deadline(&conversation)?;
                let (request, prepared) = prepared.into_parts();
                let mut events = tokio::select! {
                    biased;
                    _ = child.cancelled() => Err(AgentError::Cancelled),
                    _ = tokio::time::sleep_until(deadline) => { child.cancel(); Err(AgentError::ResourceLimit("turn duration")) },
                    result = provider.stream(request, child.clone()) => result.map_err(AgentError::from),
                }?;
                let mut displayed = 0_usize;
                let response = loop {
                    let event = tokio::select! {
                        biased;
                        _ = child.cancelled() => Err(AgentError::Cancelled),
                        _ = tokio::time::sleep_until(deadline) => { child.cancel(); Err(AgentError::ResourceLimit("turn duration")) },
                        event = events.next() => event.ok_or(AgentError::InvalidData("provider stream ended without completed response")).and_then(|event| event.map_err(AgentError::from)),
                    }?;
                    match event {
                        ModelEvent::TextDelta { index, delta } => {
                            displayed = displayed.checked_add(delta.len()).filter(|size| *size <= MAX_CHAT_TEXT_BYTES).ok_or(AgentError::ResourceLimit("assistant text"))?;
                            yield TurnEvent::ProvisionalText { index, delta };
                        }
                        ModelEvent::Completed { response } => break response,
                        ModelEvent::ToolCallStarted { .. } | ModelEvent::ToolCallArgumentsDelta { .. } if setup.is_none() => Err(AgentError::ToolsUnsupported)?,
                        ModelEvent::ContentBlockCompleted { block, .. } if setup.is_none() && !matches!(block, ContentBlock::Text(_)) => Err(AgentError::ToolsUnsupported)?,
                        _ => {}
                    }
                };
                drop(events);
                if child.is_cancelled() { Err(AgentError::Cancelled)?; }
                match prepared {
                    None => {
                        let (message, summary) = agent.commit_response(conversation, command_id, response, child.clone(), Arc::clone(&admission)).await?;
                        yield TurnEvent::Committed { message, summary }; break;
                    }
                    Some(built) => {
                        let setup = setup.as_ref().ok_or(AgentError::NativeSetupRequired)?;
                        let (saved, output) = agent.commit_native_response(conversation, response, *built, setup, Arc::clone(&admission)).await?;
                        conversation = saved;
                        for event in output { yield event; }
                        if conversation.checkpoint.pending.is_none() { break; }
                    }
                }
            }
        };
        Ok(Box::pin(stream))
    }
}

enum Prepared {
    Legacy(ModelRequest),
    Context {
        built: Box<BuiltContext>,
        request: ModelRequest,
    },
}
impl Prepared {
    fn into_parts(self) -> (ModelRequest, Option<Box<BuiltContext>>) {
        match self {
            Self::Legacy(request) => (request, None),
            Self::Context { built, request } => (request, Some(built)),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeRunInfo {
    pub command_id: CommandId,
    pub task_id: TaskId,
    pub workspace_root: std::path::PathBuf,
    pub mode: ExecutionMode,
    pub allowed_tools: Vec<String>,
    pub status: LoopStatus,
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
    pub deadline_ms: u64,
}

impl Conversation {
    pub fn native_run(&self) -> Option<NativeRunInfo> {
        let state = self.checkpoint.native.as_ref()?;
        state
            .turn
            .as_ref()
            .map(run_info)
            .or_else(|| state.last_run.clone())
    }
}

impl ToolRunSetup {
    pub fn resume(
        broker: TaskToolBroker,
        registry: Arc<ToolRegistry>,
        conversation: &Conversation,
        provider_resume: ProviderResume,
    ) -> Result<Self, AgentError> {
        let definition = &native(conversation)?
            .turn
            .as_ref()
            .ok_or(AgentError::NoPendingTurn)?
            .definition;
        Ok(Self {
            broker,
            registry,
            task: definition.task.clone(),
            budget: definition.budget.clone(),
            memory: definition.memory.iter().map(SavedMemory::excerpt).collect(),
            selected_tools: definition.initial_tools.clone(),
            limits: definition.limits.clone(),
            provider_resume,
        })
    }
}

fn prepare_definition(
    store: &Store,
    conversation: &Conversation,
    setup: &ToolRunSetup,
) -> Result<Definition, AgentError> {
    setup.limits.validate()?;
    ContextBuilder::new(setup.budget.clone())?;
    let authority = setup.broker.authority();
    let task_id: TaskId = authority.task_id().parse()?;
    if authority.conversation_id() != conversation.info.id.to_string()
        || setup.budget.reserved_output_tokens != conversation.config.max_output_tokens
        || setup.selected_tools.len() > setup.limits.max_selected_tools
    {
        return Err(AgentError::ConfigurationMismatch);
    }
    ensure_task(store, authority.task_id(), authority.conversation_id())?;
    if let Some(native) = conversation
        .checkpoint
        .native
        .as_ref()
        .filter(|native| native.turn.is_some())
    {
        for saved in &native.context.tools {
            if !definition_matches(&setup.registry, saved)? || !authority.permits_tool(&saved.name)
            {
                return Err(AgentError::ConfigurationMismatch);
            }
        }
    }
    let mut names = BTreeSet::new();
    for tool in &setup.selected_tools {
        tool.validate()?;
        if !names.insert(tool.name.clone())
            || !definition_matches(&setup.registry, tool)?
            || !authority.permits_tool(&tool.name)
        {
            return Err(AgentError::ConfigurationMismatch);
        }
    }
    encoded_len(&setup.selected_tools, 512 * 1_024)?;
    let memory: Vec<SavedMemory> = setup.memory.iter().map(SavedMemory::from).collect();
    encoded_len(&memory, 1_048_576)?;
    let definition = Definition {
        task_id,
        workspace: authority
            .workspace_root()
            .to_str()
            .ok_or(AgentError::InvalidData("workspace is not UTF-8"))?
            .to_owned(),
        mode: authority.mode(),
        allowed_tools: setup
            .registry
            .discover()
            .into_iter()
            .filter(|descriptor| authority.permits_tool(&descriptor.name))
            .map(|descriptor| descriptor.name)
            .collect(),
        task: setup.task.clone(),
        budget: setup.budget.clone(),
        memory,
        initial_tools: setup.selected_tools.clone(),
        limits: setup.limits.clone(),
    };
    encoded_len(&definition, 2 * 1_048_576)?;
    Ok(definition)
}

fn ensure_task(store: &Store, task_id: &str, conversation_id: &str) -> Result<(), AgentError> {
    let task = store
        .task(task_id)?
        .ok_or(AgentError::InvalidData("native host task is missing"))?;
    if task.conversation_id != conversation_id || task.state != TaskState::Running {
        return Err(AgentError::InvalidData(
            "native host task is not running in this conversation",
        ));
    }
    Ok(())
}

fn prepare_native(
    conversation: &Conversation,
    definition: Definition,
) -> Result<NativeCheckpoint, AgentError> {
    let digest = definition_digest(&definition)?;
    let mut checkpoint = match &conversation.checkpoint.native {
        Some(existing) => existing.clone(),
        None => {
            if conversation.checkpoint.continuation.is_some() {
                return Err(AgentError::LegacyContinuation);
            }
            NativeCheckpoint {
                context: SavedContext {
                    compaction: None,
                    binding: None,
                    tools: definition.initial_tools.clone(),
                },
                turn: None,
                last_run: None,
            }
        }
    };
    if let Some(turn) = &checkpoint.turn {
        if turn.definition_digest != digest {
            return Err(AgentError::ConfigurationMismatch);
        }
    } else {
        let command_id = conversation
            .checkpoint
            .pending
            .as_ref()
            .ok_or(AgentError::NoPendingTurn)?
            .command
            .command_id;
        let now = UnixMillis::now()?.get();
        let deadline_ms = now
            .checked_add(definition.limits.max_duration_ms)
            .filter(|deadline| *deadline <= i64::MAX as u64)
            .ok_or(AgentError::ResourceLimit("turn deadline"))?;
        checkpoint.context.tools = definition.initial_tools.clone();
        checkpoint.turn = Some(NativeTurn {
            command_id,
            definition,
            definition_digest: digest,
            started_at_ms: now,
            deadline_ms,
            attempts: 0,
            tool_calls: 0,
            charged_tokens: 0,
            reported_input_tokens: 0,
            reported_output_tokens: 0,
            status: LoopStatus::Ready,
            round: None,
        });
    }
    Ok(checkpoint)
}

fn definition_digest(definition: &Definition) -> Result<String, AgentError> {
    let encoded = serde_json::to_vec(definition)
        .map_err(|_| AgentError::InvalidData("native setup could not be encoded"))?;
    let mut hash = Sha256::new();
    hash.update(b"everything.native.agent.definition.v1\0");
    hash.update(encoded);
    Ok(format!("{:x}", hash.finalize()))
}

fn native(conversation: &Conversation) -> Result<&NativeCheckpoint, AgentError> {
    conversation
        .checkpoint
        .native
        .as_ref()
        .ok_or(AgentError::NativeSetupRequired)
}

fn turn_mut(checkpoint: &mut Checkpoint) -> Result<&mut NativeTurn, AgentError> {
    checkpoint
        .native
        .as_mut()
        .and_then(|native| native.turn.as_mut())
        .ok_or(AgentError::NativeSetupRequired)
}

fn registered_definition(
    registry: &ToolRegistry,
    name: &str,
) -> Result<model::ToolDefinition, AgentError> {
    let descriptor = registry
        .descriptor(name)
        .map_err(|_| AgentError::InvalidData("selected tool is not registered"))?;
    let definition = model::ToolDefinition {
        name: descriptor.name.clone(),
        description: descriptor.description.clone(),
        input_schema: descriptor.input_schema.clone(),
    };
    definition.validate()?;
    Ok(definition)
}

fn build_context(
    conversation: &Conversation,
    setup: &ToolRunSetup,
) -> Result<BuiltContext, AgentError> {
    let state = native(conversation)?;
    let turn = state.turn.as_ref().ok_or(AgentError::NoPendingTurn)?;
    let mut mapping = MessageMapping::default();
    let history = conversation
        .messages
        .iter()
        .map(|entry| {
            Ok(HistoryEntry {
                event_id: entry.event_id,
                sequence: entry.sequence,
                message: model_message(&entry.message, &mut mapping)?,
            })
        })
        .collect::<Result<Vec<_>, AgentError>>()?;
    for tool in &state.context.tools {
        if !definition_matches(&setup.registry, tool)? {
            return Err(AgentError::ConfigurationMismatch);
        }
    }
    ContextBuilder::new(turn.definition.budget.clone())?
        .build(ContextInput {
            model: conversation.config.model.clone(),
            instructions: conversation.config.instructions.clone(),
            task: turn.definition.task.clone(),
            history,
            memory: turn
                .definition
                .memory
                .iter()
                .map(SavedMemory::excerpt)
                .collect(),
            selected_tools: if turn.definition.limits.max_tool_calls == 0 {
                Vec::new()
            } else {
                state.context.tools.clone()
            },
            tool_choice: if state.context.tools.is_empty()
                || turn.definition.limits.max_tool_calls == 0
            {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            },
            temperature: conversation.config.temperature,
            summary: state.context.compaction.clone(),
            continuation: state.context.binding.clone(),
        })
        .map_err(AgentError::from)
}

fn reservation(built: &BuiltContext) -> u64 {
    built
        .usage
        .estimated_input_tokens
        .saturating_add(u64::from(built.usage.reserved_output_tokens))
}

fn next_attempt_limit(
    conversation: &Conversation,
    built: &BuiltContext,
) -> Result<Option<PauseReason>, AgentError> {
    let turn = native(conversation)?
        .turn
        .as_ref()
        .ok_or(AgentError::NoPendingTurn)?;
    let now = UnixMillis::now()?.get();
    Ok(if now < turn.started_at_ms || now >= turn.deadline_ms {
        Some(PauseReason::DurationLimit)
    } else if turn.attempts >= turn.definition.limits.max_model_attempts {
        Some(PauseReason::ModelAttemptLimit)
    } else if turn
        .charged_tokens
        .checked_add(reservation(built))
        .is_none_or(|total| total > turn.definition.limits.max_charged_tokens)
    {
        Some(PauseReason::TokenLimit)
    } else {
        None
    })
}

fn run_deadline(conversation: &Conversation) -> Result<Instant, AgentError> {
    let remaining = match conversation
        .checkpoint
        .native
        .as_ref()
        .and_then(|native| native.turn.as_ref())
    {
        Some(turn) => turn.deadline_ms.saturating_sub(UnixMillis::now()?.get()),
        None => 3_600_000,
    };
    Ok(Instant::now() + Duration::from_millis(remaining))
}

impl Agent {
    async fn save_native(
        &self,
        conversation: &mut Conversation,
        mut checkpoint: Checkpoint,
        payloads: Vec<ConversationEvent>,
        admission: Admission,
    ) -> Result<(), AgentError> {
        let owner = self.clone();
        let id = conversation.info.id;
        let command = checkpoint
            .pending
            .as_ref()
            .map(|pending| pending.command.command_id)
            .or_else(|| {
                checkpoint
                    .last_turn
                    .as_ref()
                    .map(|summary| summary.command_id)
            })
            .ok_or(AgentError::NoPendingTurn)?;
        let expected_sequence = conversation.info.last_sequence;
        let expected_revision = conversation.info.checkpoint_revision;
        let (checkpoint, head, revision, messages) = blocking(move || {
            let _admission = admission;
            let mut sequence = expected_sequence;
            let mut rows = Vec::new();
            let mut messages = Vec::new();
            let now = UnixMillis::now()?;
            for payload in payloads {
                sequence = next_sequence(sequence)?;
                if let ConversationEvent::MessageAppended { .. } = &payload {
                    checkpoint.message_head = sequence;
                    checkpoint.message_count = checkpoint
                        .message_count
                        .checked_add(1)
                        .ok_or(AgentError::ResourceLimit("message count"))?;
                }
                let row = event_input(id, sequence, command, now, payload)?;
                let event: EventEnvelope = serde_json::from_value(row.payload.clone())
                    .map_err(|_| AgentError::InvalidData("new event encoding changed"))?;
                if let ConversationEvent::MessageAppended { message } = event.payload {
                    messages.push(RecordedMessage {
                        event_id: event.event_id,
                        sequence: event.sequence,
                        causation_id: event.causation_id,
                        message,
                    });
                }
                rows.push(row);
            }
            let update = ConversationStateUpdate {
                expected_revision,
                payload: checkpoint_payload(&checkpoint)?,
            };
            let journal_command = CommandId::new().to_string();
            let revision = if rows.is_empty() {
                owner
                    .store
                    .checkpoint_conversation_state(
                        &id.to_string(),
                        expected_sequence,
                        &journal_command,
                        &update,
                    )?
                    .revision
            } else {
                owner.store.append_with_state(
                    &AppendRequest {
                        conversation_id: id.to_string(),
                        command_id: journal_command,
                        expected_sequence,
                        events: rows,
                    },
                    &update,
                )?;
                expected_revision
                    .checked_add(1)
                    .ok_or(AgentError::ResourceLimit("checkpoint revision"))?
            };
            Ok((checkpoint, sequence, revision, messages))
        })
        .await?;
        conversation.info.last_sequence = head;
        conversation.info.checkpoint_revision = revision;
        conversation.info.message_count = checkpoint.message_count;
        conversation.info.pending_command_id = checkpoint
            .pending
            .as_ref()
            .map(|pending| pending.command.command_id);
        conversation.info.native_status = checkpoint.native.as_ref().map(NativeCheckpoint::status);
        conversation.messages.extend(messages);
        conversation.checkpoint = checkpoint;
        Ok(())
    }
}

impl Agent {
    async fn commit_native_response(
        &self,
        mut conversation: Conversation,
        response: ModelResponse,
        built: BuiltContext,
        setup: &ToolRunSetup,
        admission: Admission,
    ) -> Result<(Conversation, Vec<TurnEvent>), AgentError> {
        let registry = Arc::clone(&setup.registry);
        let owner = self.clone();
        let mapping_admission = Arc::clone(&admission);
        let (captured, checkpoint, payloads, events) = blocking(move || {
            let _owner = owner; let _mapping_admission = mapping_admission;
            let captured = conversation;
            response.validate()?;
            if response.continuation.as_ref().is_some_and(|value| value.provider() != captured.config.provider.id()) {
                return Err(AgentError::ConfigurationMismatch);
            }
            let mut checkpoint = captured.checkpoint.clone();
            let turn = turn_mut(&mut checkpoint)?;
            let command_id = turn.command_id;
            let task_id = turn.definition.task_id;
            turn.reported_input_tokens = turn.reported_input_tokens.checked_add(response.usage.input_tokens).ok_or(AgentError::ResourceLimit("reported token accounting"))?;
            turn.reported_output_tokens = turn.reported_output_tokens.checked_add(response.usage.output_tokens).ok_or(AgentError::ResourceLimit("reported token accounting"))?;
            turn.charged_tokens = turn.charged_tokens.checked_add(response.usage.total_tokens().saturating_sub(reservation(&built))).ok_or(AgentError::ResourceLimit("token accounting"))?;
            let count = response.message.content.iter().filter(|block| matches!(block, ContentBlock::ToolCall(_))).count();
            let count_u32 = u32::try_from(count).map_err(|_| AgentError::ResourceLimit("tool call count"))?;
            if count > turn.definition.limits.max_calls_per_round || turn.tool_calls.checked_add(count_u32).is_none_or(|total| total > turn.definition.limits.max_tool_calls) {
                turn.status = LoopStatus::Paused { reason: PauseReason::ToolCallLimit };
                return Ok((captured, checkpoint, Vec::new(), vec![TurnEvent::Paused { reason: PauseReason::ToolCallLimit }]));
            }
            if (count == 0) == (response.stop_reason == StopReason::ToolCall) {
                turn.status = LoopStatus::Paused { reason: PauseReason::InvalidModelResponse };
                return Ok((captured, checkpoint, Vec::new(), vec![TurnEvent::Paused { reason: PauseReason::InvalidModelResponse }]));
            }
            let state = captured.checkpoint.native.as_ref().ok_or(AgentError::NativeSetupRequired)?;
            let selected: BTreeSet<_> = state.context.tools.iter().map(|tool| tool.name.as_str()).collect();
            let mut provider_ids: BTreeSet<String> = captured.messages.iter().flat_map(|entry| &entry.message.content).filter_map(|block| {
                if let MessageContent::ToolCall(call) = block { call.provider_call_id.clone() } else { None }
            }).collect();
            let now = UnixMillis::now()?;
            let mut text_bytes = 0_usize; let mut content = Vec::new(); let mut calls = Vec::new();
            for block in response.message.content {
                match block {
                    ContentBlock::Text(text) => {
                        text_bytes = text_bytes.checked_add(text.len()).filter(|size| *size <= MAX_CHAT_TEXT_BYTES).ok_or(AgentError::ResourceLimit("assistant text"))?;
                        content.push(MessageContent::Text { text });
                    }
                    ContentBlock::ToolCall(call) => {
                        if !provider_ids.insert(call.id.clone()) { return Err(AgentError::InvalidData("provider reused a tool call identifier")); }
                        let converted = ToolCall { id: ToolCallId::new(), provider_call_id: Some(call.id), name: call.name, arguments: call.arguments };
                        let validation = if selected.contains(converted.name.as_str()) {
                            registry.validate(&converted.name, &converted.arguments).map_err(|_| "invalid_arguments")
                        } else { Err("tool_not_advertised") };
                        let phase = match validation {
                            Ok(()) => CallPhase::Pending,
                            Err(code) => CallPhase::Result { result: bounded_result(converted.id, json!({"error":{"code":code,"message":"The completed call did not match the advertised native tool contract."}}), true)?, stop: false },
                        };
                        content.push(MessageContent::ToolCall(converted.clone())); calls.push(CallCheckpoint { call: converted, phase });
                    }
                    ContentBlock::ToolResult(_) => return Err(AgentError::InvalidData("provider returned a tool result as assistant output")),
                }
            }
            let message = ConversationMessage { id: MessageId::new(), role: MessageRole::Assistant, content, created_at: now };
            let summary = TurnSummary { command_id, message_id: message.id, response_id: response.id, model: response.model, usage: response.usage, stop_reason: response.stop_reason };
            let mut payloads = vec![ConversationEvent::MessageAppended { message: message.clone() }];
            let mut events = vec![TurnEvent::Committed { message: message.clone(), summary: summary.clone() }];
            for call in &calls {
                payloads.push(ConversationEvent::ToolRequested { task_id, call: call.call.clone() });
                events.push(TurnEvent::ToolCallRecorded { call: call.call.clone() });
                if let CallPhase::Result { result, .. } = &call.phase {
                    payloads.push(ConversationEvent::ToolCompleted { task_id, result: result.clone() });
                    events.push(TurnEvent::ToolResultRecorded { result: result.clone() });
                }
            }
            let native = checkpoint.native.as_mut().ok_or(AgentError::NativeSetupRequired)?;
            native.context.binding = response.continuation.clone().map(|continuation| built.bind_continuation(continuation));
            let turn = native.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
            turn.tool_calls += count_u32;
            if calls.is_empty() {
                turn.status = LoopStatus::Completed;
                native.last_run = Some(run_info(turn));
                native.turn = None;
                checkpoint.pending = None;
            } else {
                turn.round = Some(Round { assistant_message_id: message.id, calls });
                turn.status = if turn.charged_tokens > turn.definition.limits.max_charged_tokens {
                    LoopStatus::Paused { reason: PauseReason::TokenLimit }
                } else { LoopStatus::ExecutingTools };
            }
            checkpoint.last_turn = Some(summary);
            checkpoint.continuation = response.continuation;
            if let Some(run) = &native.last_run
                && checkpoint.pending.is_none() { events.push(TurnEvent::RunCompleted { run: run.clone() }); }
            Ok((captured, checkpoint, payloads, events))
        }).await?;
        conversation = captured;
        self.save_native(&mut conversation, checkpoint, payloads, admission)
            .await?;
        Ok((conversation, events))
    }

    async fn execute_round(
        &self,
        conversation: &mut Conversation,
        setup: Arc<ToolRunSetup>,
        cancellation: &CancellationToken,
        admission: Admission,
    ) -> Result<Vec<TurnEvent>, AgentError> {
        let owner = self.clone();
        let scoped = setup.broker.clone();
        blocking(move || {
            ensure_task(
                &owner.store,
                scoped.authority().task_id(),
                scoped.authority().conversation_id(),
            )
        })
        .await?;
        let mut checkpoint = conversation.checkpoint.clone();
        let turn = turn_mut(&mut checkpoint)?;
        let task_id = turn.definition.task_id;
        let deadline = run_deadline(conversation)?;
        let current_time = UnixMillis::now()?.get();
        if current_time < turn.started_at_ms || current_time >= turn.deadline_ms {
            turn.status = LoopStatus::Paused {
                reason: PauseReason::DurationLimit,
            };
            self.save_native(conversation, checkpoint, Vec::new(), admission)
                .await?;
            return Ok(vec![TurnEvent::Paused {
                reason: PauseReason::DurationLimit,
            }]);
        }
        let round = turn
            .round
            .as_mut()
            .ok_or(AgentError::InvalidData("tool phase has no persisted round"))?;
        let mut requests = Vec::new();
        for (index, call) in round.calls.iter_mut().enumerate() {
            if matches!(
                call.phase,
                CallPhase::Pending | CallPhase::Dispatched | CallPhase::AwaitingPermission { .. }
            ) {
                call.phase = CallPhase::Dispatched;
                requests.push((
                    index,
                    ToolInvocation {
                        operation_id: call.call.id.to_string(),
                        tool_name: call.call.name.clone(),
                        arguments: call.call.arguments.clone(),
                    },
                ));
            }
        }
        turn.status = LoopStatus::ExecutingTools;
        self.save_native(conversation, checkpoint, Vec::new(), Arc::clone(&admission))
            .await?;
        let token = cancellation.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        let broker = setup.broker.clone();
        let child = token.clone();
        let calls = stream::iter(requests)
            .map(move |(index, invocation)| {
                let broker = broker.clone();
                let token = child.clone();
                async move { (index, broker.execute(invocation, token).await) }
            })
            .buffer_unordered(setup.limits.max_parallel_calls);
        tokio::pin!(calls);
        let mut events = Vec::new();
        let mut deadline_reached = false;
        loop {
            let next = if deadline_reached || token.is_cancelled() {
                calls.next().await
            } else {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => { token.cancel(); calls.next().await },
                    _ = tokio::time::sleep_until(deadline) => { deadline_reached = true; token.cancel(); calls.next().await },
                    result = calls.next() => result,
                }
            };
            let Some((index, returned)) = next else {
                break;
            };
            let mut checkpoint = conversation.checkpoint.clone();
            let registry = Arc::clone(&setup.registry);
            let (checkpoint, payloads, emitted, stop) = blocking(move || {
                let mut emitted = Vec::new();
                let mut stop_request = false;
                let native = checkpoint
                    .native
                    .as_mut()
                    .ok_or(AgentError::NativeSetupRequired)?;
                let turn = native.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
                let call = turn
                    .round
                    .as_mut()
                    .and_then(|round| round.calls.get_mut(index))
                    .ok_or(AgentError::InvalidData(
                        "tool completion lost its persisted index",
                    ))?;
                let operation_id = call.call.id.to_string();
                let mut payloads = Vec::new();
                match returned {
                    Ok(BrokerOutcome::Completed {
                        operation_id: returned,
                        record,
                        ..
                    }) => {
                        if returned != operation_id {
                            return Err(AgentError::InvalidData(
                                "broker operation identity changed",
                            ));
                        }
                        let stop = record.requires_stop();
                        match output_from_record(call.call.id, &record.result)? {
                            Some(result) => {
                                if call.call.name == "search_tools" && !result.is_error {
                                    expand_discovery(native, &registry, &record.result)?;
                                }
                                let call = turn_mut(&mut checkpoint)?
                                    .round
                                    .as_mut()
                                    .and_then(|round| round.calls.get_mut(index))
                                    .ok_or(AgentError::InvalidData(
                                        "tool call checkpoint changed",
                                    ))?;
                                call.phase = CallPhase::Result {
                                    result: result.clone(),
                                    stop,
                                };
                                payloads.push(ConversationEvent::ToolCompleted {
                                    task_id,
                                    result: result.clone(),
                                });
                                emitted.push(TurnEvent::ToolResultRecorded { result });
                                if stop {
                                    stop_request = true;
                                }
                            }
                            None => {
                                call.phase = CallPhase::Blocked {
                                    reason: PauseReason::ReconciliationRequired {
                                        operation_id: operation_id.clone(),
                                    },
                                    diagnostic: "broker returned an unknown outcome".to_owned(),
                                };
                                stop_request = true;
                            }
                        }
                    }
                    Ok(BrokerOutcome::ApprovalRequired {
                        operation_id: returned,
                        approval,
                    }) => {
                        if returned != operation_id {
                            return Err(AgentError::InvalidData(
                                "broker approval identity changed",
                            ));
                        }
                        let operation_id = OperationId::try_from(*call.call.id.as_uuid())?;
                        payloads.push(ConversationEvent::PermissionRequested {
                            task_id,
                            operation_id,
                            tool_call_id: call.call.id,
                            tool_name: call.call.name.clone(),
                        });
                        emitted.push(TurnEvent::AwaitingPermission {
                            call_id: call.call.id,
                            approval: approval.clone(),
                        });
                        call.phase = CallPhase::AwaitingPermission { approval };
                    }
                    Ok(BrokerOutcome::InFlight {
                        operation_id: returned,
                    }) => {
                        if returned != operation_id {
                            return Err(AgentError::InvalidData("broker running identity changed"));
                        }
                        call.phase = CallPhase::Blocked {
                            reason: PauseReason::OperationInFlight { operation_id },
                            diagnostic: "original broker operation is still running".to_owned(),
                        };
                        stop_request = true;
                    }
                    Err(error) => {
                        let reason = if error.requires_reconciliation() {
                            PauseReason::ReconciliationRequired { operation_id }
                        } else {
                            PauseReason::BrokerRejected { operation_id }
                        };
                        let diagnostic: String = error.to_string().chars().take(2048).collect();
                        emitted.push(TurnEvent::ToolBlocked {
                            call_id: call.call.id,
                            reason: reason.clone(),
                            diagnostic: diagnostic.clone(),
                        });
                        call.phase = CallPhase::Blocked { reason, diagnostic };
                        stop_request = true;
                    }
                }
                Ok((checkpoint, payloads, emitted, stop_request))
            })
            .await?;
            if stop {
                token.cancel();
            }
            events.extend(emitted);
            self.save_native(conversation, checkpoint, payloads, Arc::clone(&admission))
                .await?;
        }
        let mut checkpoint = conversation.checkpoint.clone();
        let turn = turn_mut(&mut checkpoint)?;
        let round = turn
            .round
            .as_ref()
            .ok_or(AgentError::InvalidData("finished tools lost their round"))?;
        let blocked = round
            .calls
            .iter()
            .find_map(|call| {
                if let CallPhase::Blocked {
                    reason: reason @ PauseReason::ReconciliationRequired { .. },
                    ..
                } = &call.phase
                {
                    Some(reason.clone())
                } else {
                    None
                }
            })
            .or_else(|| {
                round.calls.iter().find_map(|call| {
                    if let CallPhase::Blocked { reason, .. } = &call.phase {
                        Some(reason.clone())
                    } else {
                        None
                    }
                })
            });
        let waiting = round
            .calls
            .iter()
            .any(|call| matches!(call.phase, CallPhase::AwaitingPermission { .. }));
        let mut payloads = Vec::new();
        if let Some(reason) = blocked {
            turn.status = LoopStatus::Paused {
                reason: reason.clone(),
            };
            events.push(TurnEvent::Paused { reason });
        } else if waiting {
            turn.status = LoopStatus::AwaitingPermission;
        } else {
            let mut stop = false;
            for call in &round.calls {
                let CallPhase::Result {
                    result,
                    stop: interrupted,
                } = &call.phase
                else {
                    return Err(AgentError::InvalidData(
                        "tool round finished without every result",
                    ));
                };
                stop |= *interrupted;
                payloads.push(ConversationEvent::MessageAppended {
                    message: ConversationMessage {
                        id: MessageId::new(),
                        role: MessageRole::Tool,
                        content: vec![MessageContent::ToolResult(result.clone())],
                        created_at: UnixMillis::now()?,
                    },
                });
            }
            turn.round = None;
            let reason = if deadline_reached {
                Some(PauseReason::DurationLimit)
            } else if cancellation.is_cancelled() {
                Some(PauseReason::Cancellation)
            } else if stop {
                Some(PauseReason::InterruptedTool)
            } else {
                None
            };
            turn.status = match reason {
                Some(reason) => {
                    events.push(TurnEvent::Paused {
                        reason: reason.clone(),
                    });
                    LoopStatus::Paused { reason }
                }
                None => LoopStatus::Ready,
            };
        }
        self.save_native(conversation, checkpoint, payloads, admission)
            .await?;
        Ok(events)
    }
}

fn run_info(turn: &NativeTurn) -> NativeRunInfo {
    NativeRunInfo {
        command_id: turn.command_id,
        task_id: turn.definition.task_id,
        workspace_root: turn.definition.workspace.clone().into(),
        mode: turn.definition.mode,
        allowed_tools: turn.definition.allowed_tools.clone(),
        status: turn.status.clone(),
        model_attempts: turn.attempts,
        tool_calls: turn.tool_calls,
        charged_tokens: turn.charged_tokens,
        reported_input_tokens: turn.reported_input_tokens,
        reported_output_tokens: turn.reported_output_tokens,
        deadline_ms: turn.deadline_ms,
    }
}

fn output_from_record(
    call_id: ToolCallId,
    result: &OperationResult,
) -> Result<Option<ToolResult>, AgentError> {
    let (output, is_error) = match result {
        OperationResult::Succeeded { output } => (output.content.clone(), output.is_error),
        OperationResult::Failed {
            code,
            message,
            output,
        } => (
            output.as_ref().map_or_else(
                || json!({"error":{"code":code,"message":message}}),
                |output| output.content.clone(),
            ),
            true,
        ),
        OperationResult::Denied { reason } => (
            json!({"error":{"code":"permission_denied","message":reason}}),
            true,
        ),
        OperationResult::Cancelled { reason } => {
            (json!({"error":{"code":"cancelled","reason":reason}}), true)
        }
        OperationResult::Unknown { .. } => return Ok(None),
    };
    Ok(Some(bounded_result(call_id, output, is_error)?))
}

fn bounded_result(
    call_id: ToolCallId,
    output: Value,
    is_error: bool,
) -> Result<ToolResult, AgentError> {
    let encoded = serde_json::to_string(&output)
        .map_err(|_| AgentError::InvalidData("tool output could not be encoded"))?;
    let content = if encoded.len() <= MAX_RESULT_BYTES {
        encoded
    } else {
        let mut end = encoded.len().min(MAX_PREVIEW_BYTES);
        while !encoded.is_char_boundary(end) {
            end -= 1;
        }
        serde_json::to_string(&json!({
            "truncated":true, "original_json_bytes":encoded.len(),
            "sha256":format!("{:x}",Sha256::digest(encoded.as_bytes())),
            "preview_json_text":&encoded[..end], "notice":"Full output remains in the host operation journal. This preview may end inside a JSON value."
        })).map_err(|_| AgentError::InvalidData("truncated tool output could not be encoded"))?
    };
    if content.len() > MAX_RESULT_BYTES {
        return Err(AgentError::ResourceLimit("canonical tool output"));
    }
    Ok(ToolResult {
        tool_call_id: call_id,
        content,
        is_error,
    })
}

fn expand_discovery(
    native: &mut NativeCheckpoint,
    registry: &ToolRegistry,
    result: &OperationResult,
) -> Result<(), AgentError> {
    let OperationResult::Succeeded { output } = result else {
        return Ok(());
    };
    let Some(candidates) = output.content.get("tools").and_then(Value::as_array) else {
        return Err(AgentError::InvalidData(
            "local discovery returned no native definitions",
        ));
    };
    if candidates.len() > 32 {
        return Err(AgentError::ResourceLimit("discovered tool count"));
    }
    let turn = native.turn.as_ref().ok_or(AgentError::NoPendingTurn)?;
    for candidate in candidates {
        let definition: model::ToolDefinition = serde_json::from_value(
            candidate
                .get("definition")
                .cloned()
                .ok_or(AgentError::InvalidData("discovered tool has no definition"))?,
        )
        .map_err(|_| AgentError::InvalidData("discovered tool definition is invalid"))?;
        if !definition_matches(registry, &definition)?
            || !turn.definition.allowed_tools.contains(&definition.name)
        {
            return Err(AgentError::ConfigurationMismatch);
        }
        if native
            .context
            .tools
            .iter()
            .any(|tool| tool.name == definition.name)
        {
            continue;
        }
        if native.context.tools.len() >= turn.definition.limits.max_selected_tools {
            break;
        }
        let mut expanded = native.context.tools.clone();
        expanded.push(definition);
        if encoded_len(&expanded, 512 * 1_024).is_ok() {
            native.context.tools = expanded;
        }
    }
    Ok(())
}

fn definition_matches(
    registry: &ToolRegistry,
    definition: &model::ToolDefinition,
) -> Result<bool, AgentError> {
    let actual = registered_definition(registry, &definition.name)?;
    Ok(actual.name == definition.name
        && actual.input_schema == definition.input_schema
        && (actual.description == definition.description
            || definition.description
                == format!(
                    "Execution requires explicit host approval for the exact operation. {}",
                    actual.description
                )))
}
