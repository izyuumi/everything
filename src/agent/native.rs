use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use super::settings::NativeAttemptFloor;
use super::*;
use crate::context::{
    BuiltContext, CompactionRecord, ContextBudget, ContextBuilder, ContextInput,
    ContinuationBinding, HistoryEntry, MemoryExcerpt, TaskContext,
};
use crate::permissions::{ApprovalRequest, ExecutionMode};
use crate::protocol::{OperationId, PermissionInspection, TaskId, ToolCall, ToolResult};
use crate::storage::{TaskState, ToolCallState};
use crate::tools::{
    RetainedToolOwner, ToolRegistry,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_reserved_output_tokens: Option<u64>,
    pub max_duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absolute_deadline: Option<UnixMillis>,
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
            max_reserved_output_tokens: None,
            max_duration_ms: 3_600_000,
            absolute_deadline: None,
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

#[derive(Clone)]
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
    WaitingForEvents { wait_id: CommandId },
    ChildOutcomeUnknown { child_task_id: TaskId },
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
    #[serde(default)]
    last_observed_wall_ms: u64,
    deadline_ms: u64,
    attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reserved_output_tokens: Option<u64>,
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
    pub(super) fn has_turn(&self) -> bool {
        self.turn.is_some()
    }

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
            checkpoint.inputs.validate_task(turn.definition.task_id)?;
            let pending = checkpoint
                .pending
                .as_ref()
                .ok_or(AgentError::InvalidData("native turn has no command"))?;
            turn.definition.limits.validate()?;
            ContextBuilder::new(turn.definition.budget.clone())?;
            let reserved_output_tokens = output_reservation(turn)?;
            if turn
                .definition
                .limits
                .max_reserved_output_tokens
                .is_some_and(|limit| reserved_output_tokens > limit)
            {
                return Err(AgentError::InvalidData(
                    "native output reservation exceeds its limit",
                ));
            }
            if let LoopStatus::Paused {
                reason: PauseReason::WaitingForEvents { wait_id },
            } = &turn.status
                && (checkpoint.inputs.wait_id() != Some(*wait_id) || turn.round.is_some())
            {
                return Err(AgentError::InvalidData(
                    "native child wait does not match its inbox",
                ));
            }
            if turn.command_id != pending.command.command_id
                || definition_digest(&turn.definition)? != turn.definition_digest
                || turn.last_observed_wall_ms > i64::MAX as u64
                || (turn.last_observed_wall_ms != 0
                    && turn.last_observed_wall_ms < turn.started_at_ms)
                || turn.deadline_ms
                    != effective_deadline(turn.started_at_ms, &turn.definition.limits)?
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
    pub async fn capture_tools_setup(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
        setup: ToolRunSetup,
        cancellation: CancellationToken,
    ) -> Result<NativeRunInfo, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        self.check_settings_setup(conversation_id, command_id, Some(&setup), None)?;
        let admission = self.admit(conversation_id)?;
        let conversation = self.conversation(conversation_id).await?;
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if conversation.info.pending_command_id != Some(command_id) {
            return Err(AgentError::CommandConflict(command_id));
        }
        let conversation = self
            .capture_native_setup(conversation, Arc::new(setup), admission)
            .await?;
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        conversation
            .native_run()
            .ok_or(AgentError::NativeSetupRequired)
    }

    pub(crate) async fn guard_unstarted_tools(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
        task_id: TaskId,
    ) -> Result<impl Send + 'static, AgentError> {
        let admission = self.admit(conversation_id)?;
        let write = self.write_gate(conversation_id)?.lock_owned().await;
        let owner = self.clone();
        blocking(move || {
            let conversation = load_conversation(&owner.store, conversation_id)?;
            if conversation.info.pending_command_id != Some(command_id)
                || conversation.checkpoint.continuation.is_some()
                || !conversation.checkpoint.inputs.is_empty()
            {
                return Err(AgentError::InvalidData("unstarted task boundary changed"));
            }
            if let Some(native) = &conversation.checkpoint.native {
                let turn = native
                    .turn
                    .as_ref()
                    .ok_or(AgentError::NativeSetupRequired)?;
                if turn.command_id != command_id
                    || turn.definition.task_id != task_id
                    || turn.status != LoopStatus::Ready
                    || turn.round.is_some()
                    || turn.attempts != 0
                    || turn.tool_calls != 0
                    || turn.reserved_output_tokens != Some(0)
                    || turn.charged_tokens != 0
                    || turn.reported_input_tokens != 0
                    || turn.reported_output_tokens != 0
                {
                    return Err(AgentError::InvalidData("task has entered native execution"));
                }
            }
            Ok((owner, admission, write))
        })
        .await
    }

    async fn capture_native_setup(
        &self,
        mut conversation: Conversation,
        setup: Arc<ToolRunSetup>,
        admission: Admission,
    ) -> Result<Conversation, AgentError> {
        let owner = self.clone();
        let (captured, native) = blocking(move || {
            let retained_agent = owner;
            let definition = prepare_definition(&retained_agent.store, &conversation, &setup)?;
            let native = prepare_native(&conversation, definition)?;
            if native
                .turn
                .as_ref()
                .is_some_and(|turn| turn.round.is_none())
            {
                let mut validated = conversation.checkpoint.clone();
                validated.native = Some(native.clone());
                let checkpoint = std::mem::replace(&mut conversation.checkpoint, validated);
                build_context(&conversation, &setup)?;
                conversation.checkpoint = checkpoint;
            }
            Ok((conversation, native))
        })
        .await?;
        conversation = captured;
        let mut checkpoint = conversation.checkpoint.clone();
        checkpoint.native = Some(native);
        self.save_native(&mut conversation, checkpoint, Vec::new(), admission)
            .await?;
        Ok(conversation)
    }

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
        self.check_settings_setup(conversation_id, command_id, setup.as_ref(), Some(&provider))?;
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
            conversation = self
                .capture_native_setup(conversation, Arc::clone(setup), Arc::clone(&admission))
                .await?;
        } else if conversation.checkpoint.native.is_some()
            || conversation.checkpoint.inputs.has_pending()
        {
            return Err(AgentError::NativeSetupRequired);
        }
        let stream_deadline = run_deadline(&conversation)?;
        let agent = self.clone();
        let child = cancellation.child_token();
        let cancel_on_drop = child.clone().drop_guard();
        let stream = async_stream::try_stream! {
            let _cancel_on_drop = cancel_on_drop;
            let admission = admission;
            loop {
                if child.is_cancelled() { Err(AgentError::Cancelled)?; }
                if let Some(setup) = &setup {
                    if let Some(reason) = agent.consume_native_inputs(&mut conversation, Arc::clone(&admission)).await? {
                        yield TurnEvent::Paused { reason }; break;
                    }
                    let status = native(&conversation)?.turn.as_ref().ok_or(AgentError::NoPendingTurn)?.status.clone();
                    match status {
                        LoopStatus::ProviderInFlight { .. } if setup.provider_resume != ProviderResume::RetryUncertain => {
                            yield TurnEvent::Paused { reason: PauseReason::ProviderOutcomeUncertain }; break;
                        }
                        LoopStatus::ProviderInFlight { .. } => {
                            let mut checkpoint = conversation.checkpoint.clone(); turn_mut(&mut checkpoint)?.status = LoopStatus::Ready;
                            agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                            continue;
                        }
                        LoopStatus::Paused { reason } => {
                            if matches!(reason, PauseReason::OperationInFlight { .. }) || (setup.provider_resume == ProviderResume::RetryUncertain && matches!(reason, PauseReason::Cancellation | PauseReason::InterruptedTool)) {
                                if matches!(reason, PauseReason::OperationInFlight { .. })
                                    && !clock_allows(native(&conversation)?.turn.as_ref().ok_or(AgentError::NoPendingTurn)?, UnixMillis::now()?.get(), stream_deadline)
                                {
                                    let reason = agent.expired_round_reason(&conversation).await?;
                                    let mut checkpoint = conversation.checkpoint.clone();
                                    turn_mut(&mut checkpoint)?.status = LoopStatus::Paused { reason: reason.clone() };
                                    agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                                    yield TurnEvent::Paused { reason }; break;
                                }
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
                            let events = agent.execute_round(&mut conversation, Arc::clone(setup), &child, Arc::clone(&admission), stream_deadline).await?;
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
                        let retained_agent = owner;
                        ensure_task(&retained_agent.store, preparation.broker.authority().task_id(), preparation.broker.authority().conversation_id())?;
                        let built = build_context(&conversation, &preparation)?;
                        let request = built.request.clone();
                        Ok((conversation, built, request))
                    }).await?;
                    conversation = captured;
                    if let Some(reason) = next_attempt_limit(&conversation, &built, stream_deadline)? {
                        let mut checkpoint = conversation.checkpoint.clone(); turn_mut(&mut checkpoint)?.status = LoopStatus::Paused { reason: reason.clone() };
                        agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                        yield TurnEvent::Paused { reason }; break;
                    }
                    #[cfg(unix)]
                    let source_admission = if let Some(profile) = &agent.settings_profile {
                        Some(tokio::select! {
                            biased;
                            _ = child.cancelled() => Err(AgentError::Cancelled),
                            _ = tokio::time::sleep_until(stream_deadline) => Err(AgentError::ResourceLimit("restricted source admission deadline")),
                            admitted = profile.admit_source(child.clone()) => admitted,
                        }?)
                    } else { None };
                    let attempt_id = CommandId::new();
                    let mut checkpoint = conversation.checkpoint.clone();
                    let state = checkpoint.native.as_mut().ok_or(AgentError::NativeSetupRequired)?;
                    state.context.compaction = built.compaction.clone();
                    if built.request.continuation.is_none() { state.context.binding = None; checkpoint.continuation = None; }
                    let turn = state.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
                    turn.last_observed_wall_ms = observed_floor(turn).max(UnixMillis::now()?.get());
                    let reserved_output_tokens = output_reservation(turn)?.checked_add(request_output_reservation(turn, &built)?).ok_or(AgentError::ResourceLimit("output reservation accounting"))?;
                    turn.attempts += 1;
                    turn.reserved_output_tokens = Some(reserved_output_tokens);
                    turn.charged_tokens = turn.charged_tokens.checked_add(reservation(&built)).ok_or(AgentError::ResourceLimit("token accounting"))?;
                    turn.status = LoopStatus::ProviderInFlight { attempt_id };
                    if agent.settings_profile.is_some() {
                        let floor = NativeAttemptFloor { attempt_id, model_attempts: turn.attempts, tool_calls: turn.tool_calls,
                            charged_tokens: turn.charged_tokens, reserved_output_tokens, reported_input_tokens: turn.reported_input_tokens,
                            reported_output_tokens: turn.reported_output_tokens, reservation: reservation(&built), deadline_ms: turn.deadline_ms,
                            observed_wall_ms: turn.last_observed_wall_ms };
                        settings_preflight(&checkpoint)?;
                        let owner = agent.clone();
                        blocking(move || { let owner = owner; owner.settings_profile.as_ref().ok_or(AgentError::ConfigurationMismatch)?.before_attempt(floor) }).await?;
                    }
                    if !agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await? {
                        if agent.settings_profile.is_some() { Err(AgentError::Cancelled)?; }
                        continue;
                    }
                    #[cfg(unix)]
                    drop(source_admission);
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
                let deadline = stream_deadline.min(run_deadline(&conversation)?);
                if let Some(turn) = conversation.checkpoint.native.as_ref().and_then(|native| native.turn.as_ref())
                    && !clock_allows(turn, UnixMillis::now()?.get(), stream_deadline)
                {
                    let mut checkpoint = conversation.checkpoint.clone();
                    turn_mut(&mut checkpoint)?.status = LoopStatus::Paused { reason: PauseReason::DurationLimit };
                    agent.save_native(&mut conversation, checkpoint, Vec::new(), Arc::clone(&admission)).await?;
                    yield TurnEvent::Paused { reason: PauseReason::DurationLimit }; break;
                }
                let (request, prepared) = prepared.into_parts();
                let stream_result = tokio::select! {
                    biased;
                    _ = child.cancelled() => Err(AgentError::Cancelled),
                    _ = tokio::time::sleep_until(deadline) => { child.cancel(); Err(AgentError::ResourceLimit("turn duration")) },
                    result = provider.stream(request, child.clone()) => result.map_err(AgentError::from),
                };
                if stream_result.is_err() { agent.observe_settings_provider().await?; }
                let mut events = stream_result?;
                let mut displayed = 0_usize;
                let response = loop {
                    let event_result = tokio::select! {
                        biased;
                        _ = child.cancelled() => Err(AgentError::Cancelled),
                        _ = tokio::time::sleep_until(deadline) => { child.cancel(); Err(AgentError::ResourceLimit("turn duration")) },
                        event = events.next() => event.ok_or(AgentError::InvalidData("provider stream ended without completed response")).and_then(|event| event.map_err(AgentError::from)),
                    };
                    if event_result.is_err() { agent.observe_settings_provider().await?; }
                    match event_result? {
                        ModelEvent::TextDelta { index, delta } => {
                            let text_limit = if agent.settings_profile.is_some() { crate::agent::settings::MAX_TEXT } else { MAX_CHAT_TEXT_BYTES };
                            let proposed = displayed.checked_add(delta.len()).filter(|size| *size <= text_limit);
                            if proposed.is_none() { agent.observe_settings_provider().await?; }
                            displayed = proposed.ok_or(AgentError::ResourceLimit("assistant text"))?;
                            yield TurnEvent::ProvisionalText { index, delta };
                        }
                        ModelEvent::Completed { response } => {
                            agent.observe_settings_provider().await?;
                            break response;
                        }
                        ModelEvent::Usage { .. } => { agent.observe_settings_provider().await?; }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_output_tokens: Option<u64>,
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
            if !definition_matches(
                &setup.registry,
                saved,
                setup.broker.settings_profile().is_some(),
            )? || !authority.permits_tool(&saved.name)
            {
                return Err(AgentError::ConfigurationMismatch);
            }
        }
    }
    let mut names = BTreeSet::new();
    for tool in &setup.selected_tools {
        tool.validate()?;
        if !names.insert(tool.name.clone())
            || !definition_matches(
                &setup.registry,
                tool,
                setup.broker.settings_profile().is_some(),
            )?
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
        allowed_tools: if setup.broker.settings_profile().is_some() {
            setup
                .selected_tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect()
        } else {
            setup
                .registry
                .discover()
                .into_iter()
                .filter(|descriptor| authority.permits_tool(&descriptor.name))
                .map(|descriptor| descriptor.name)
                .collect()
        },
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
    if let Some(turn) = &mut checkpoint.turn {
        if turn.definition_digest != digest {
            return Err(AgentError::ConfigurationMismatch);
        }
        turn.reserved_output_tokens = Some(output_reservation(turn)?);
    } else {
        let command_id = conversation
            .checkpoint
            .pending
            .as_ref()
            .ok_or(AgentError::NoPendingTurn)?
            .command
            .command_id;
        let now = UnixMillis::now()?.get();
        let deadline_ms = effective_deadline(now, &definition.limits)?;
        checkpoint.context.tools = definition.initial_tools.clone();
        checkpoint.turn = Some(NativeTurn {
            command_id,
            definition,
            definition_digest: digest,
            started_at_ms: now,
            last_observed_wall_ms: now,
            deadline_ms,
            attempts: 0,
            reserved_output_tokens: Some(0),
            tool_calls: 0,
            charged_tokens: 0,
            reported_input_tokens: 0,
            reported_output_tokens: 0,
            status: if deadline_ms <= now {
                LoopStatus::Paused {
                    reason: PauseReason::DurationLimit,
                }
            } else {
                LoopStatus::Ready
            },
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
    restricted: bool,
) -> Result<model::ToolDefinition, AgentError> {
    let descriptor = if restricted {
        registry.settings_descriptor(name)
    } else {
        registry.descriptor(name)
    }
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
        if !definition_matches(
            &setup.registry,
            tool,
            setup.broker.settings_profile().is_some(),
        )? {
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

fn reconstructed_output_reservation(turn: &NativeTurn) -> Option<u64> {
    u64::from(turn.attempts).checked_mul(u64::from(turn.definition.budget.reserved_output_tokens))
}

fn output_reservation(turn: &NativeTurn) -> Result<u64, AgentError> {
    let expected = reconstructed_output_reservation(turn)
        .ok_or(AgentError::ResourceLimit("output reservation accounting"))?;
    if turn
        .reserved_output_tokens
        .is_some_and(|reserved| reserved != expected)
    {
        return Err(AgentError::InvalidData(
            "native output reservation does not match attempts",
        ));
    }
    Ok(expected)
}

fn request_output_reservation(turn: &NativeTurn, built: &BuiltContext) -> Result<u64, AgentError> {
    let captured = turn.definition.budget.reserved_output_tokens;
    if built.request.max_output_tokens != Some(captured)
        || built.usage.reserved_output_tokens != captured
    {
        return Err(AgentError::ConfigurationMismatch);
    }
    Ok(u64::from(captured))
}

fn effective_deadline(started_at_ms: u64, limits: &LoopLimits) -> Result<u64, AgentError> {
    let relative = started_at_ms
        .checked_add(limits.max_duration_ms)
        .filter(|deadline| *deadline <= i64::MAX as u64)
        .ok_or(AgentError::ResourceLimit("turn deadline"))?;
    Ok(limits
        .absolute_deadline
        .map_or(relative, |absolute| relative.min(absolute.get())))
}

fn next_attempt_limit(
    conversation: &Conversation,
    built: &BuiltContext,
    stream_deadline: Instant,
) -> Result<Option<PauseReason>, AgentError> {
    let turn = native(conversation)?
        .turn
        .as_ref()
        .ok_or(AgentError::NoPendingTurn)?;
    let now = UnixMillis::now()?.get();
    let output_total = output_reservation(turn)?
        .checked_add(request_output_reservation(turn, built)?)
        .ok_or(AgentError::ResourceLimit("output reservation accounting"))?;
    Ok(if !clock_allows(turn, now, stream_deadline) {
        Some(PauseReason::DurationLimit)
    } else if turn.attempts >= turn.definition.limits.max_model_attempts {
        Some(PauseReason::ModelAttemptLimit)
    } else if turn
        .definition
        .limits
        .max_reserved_output_tokens
        .is_some_and(|limit| output_total > limit)
        || turn
            .charged_tokens
            .checked_add(reservation(built))
            .is_none_or(|total| total > turn.definition.limits.max_charged_tokens)
    {
        Some(PauseReason::TokenLimit)
    } else {
        None
    })
}

fn observed_floor(turn: &NativeTurn) -> u64 {
    turn.last_observed_wall_ms.max(turn.started_at_ms)
}

fn clock_allows(turn: &NativeTurn, now: u64, stream_deadline: Instant) -> bool {
    now >= observed_floor(turn) && now < turn.deadline_ms && Instant::now() < stream_deadline
}

fn run_deadline(conversation: &Conversation) -> Result<Instant, AgentError> {
    let remaining = match conversation
        .checkpoint
        .native
        .as_ref()
        .and_then(|native| native.turn.as_ref())
    {
        Some(turn) => {
            let now = UnixMillis::now()?.get();
            if now < observed_floor(turn) {
                0
            } else {
                turn.deadline_ms
                    .saturating_sub(now)
                    .min(turn.definition.limits.max_duration_ms)
            }
        }
        None => 3_600_000,
    };
    Ok(Instant::now() + Duration::from_millis(remaining))
}

impl Agent {
    async fn save_native(
        &self,
        conversation: &mut Conversation,
        checkpoint: Checkpoint,
        payloads: Vec<ConversationEvent>,
        admission: Admission,
    ) -> Result<bool, AgentError> {
        self.save_native_events(
            conversation,
            checkpoint,
            payloads
                .into_iter()
                .map(|payload| (None, payload))
                .collect(),
            admission,
        )
        .await
    }

    async fn refresh_native_controls(
        &self,
        conversation: &mut Conversation,
        admission: Admission,
    ) -> Result<(), AgentError> {
        let write = self.write_gate(conversation.info.id)?.lock_owned().await;
        let owner = self.clone();
        let captured = conversation.clone();
        *conversation = blocking(move || {
            let retained_agent = owner;
            let _write = write;
            let _admission = admission;
            let current = load_conversation(&retained_agent.store, captured.info.id)?;
            let mut proposed = captured.checkpoint.clone();
            steering::merge_controls(&retained_agent.store, &captured, &current, &mut proposed)?;
            Ok(current)
        })
        .await?;
        Ok(())
    }

    async fn consume_native_inputs(
        &self,
        conversation: &mut Conversation,
        admission: Admission,
    ) -> Result<Option<PauseReason>, AgentError> {
        self.refresh_native_controls(conversation, Arc::clone(&admission))
            .await?;
        let status = native(conversation)?.status();
        if !matches!(
            status,
            LoopStatus::Ready
                | LoopStatus::Paused {
                    reason: PauseReason::WaitingForEvents { .. }
                }
        ) {
            return Ok(None);
        }
        if native(conversation)?
            .turn
            .as_ref()
            .is_some_and(|turn| turn.round.is_some())
        {
            return Err(AgentError::InvalidData(
                "task inputs cannot split an unresolved tool round",
            ));
        }
        if !conversation.checkpoint.inputs.has_pending() {
            return Ok(match status {
                LoopStatus::Paused {
                    reason: PauseReason::WaitingForEvents { wait_id },
                } => Some(PauseReason::WaitingForEvents { wait_id }),
                _ => None,
            });
        }
        let mut checkpoint = conversation.checkpoint.clone();
        let consumed = checkpoint.inputs.consume()?;
        let reason = consumed
            .unknown_child
            .map(|child_task_id| PauseReason::ChildOutcomeUnknown { child_task_id })
            .or_else(|| {
                consumed
                    .wait_id
                    .map(|wait_id| PauseReason::WaitingForEvents { wait_id })
            });
        turn_mut(&mut checkpoint)?.status = reason
            .clone()
            .map_or(LoopStatus::Ready, |reason| LoopStatus::Paused { reason });
        self.save_native_events(conversation, checkpoint, consumed.events, admission)
            .await?;
        Ok(reason)
    }

    async fn save_native_events(
        &self,
        conversation: &mut Conversation,
        mut checkpoint: Checkpoint,
        payloads: Vec<(Option<CommandId>, ConversationEvent)>,
        admission: Admission,
    ) -> Result<bool, AgentError> {
        let write = self.write_gate(conversation.info.id)?.lock_owned().await;
        let owner = self.clone();
        let captured = conversation.clone();
        let (saved, committed) = blocking(move || {
            let retained_agent = owner;
            let _admission = admission;
            let _write = write;
            let mut current = load_conversation(&retained_agent.store, captured.info.id)?;
            steering::merge_controls(&retained_agent.store, &captured, &current, &mut checkpoint)?;
            let proposed_status = checkpoint.native.as_ref().map(NativeCheckpoint::status);
            if checkpoint.inputs.has_pending()
                && matches!(proposed_status, Some(LoopStatus::ProviderInFlight { .. }))
                && matches!(
                    captured
                        .checkpoint
                        .native
                        .as_ref()
                        .map(NativeCheckpoint::status),
                    Some(LoopStatus::Ready)
                )
            {
                return Ok((current, false));
            }
            if matches!(proposed_status, Some(LoopStatus::Completed))
                && checkpoint.pending.is_some()
            {
                let native = checkpoint
                    .native
                    .as_mut()
                    .ok_or(AgentError::NativeSetupRequired)?;
                let turn = native.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
                if checkpoint.inputs.has_pending() {
                    turn.status = LoopStatus::Ready;
                } else {
                    native.last_run = Some(run_info(turn));
                    native.turn = None;
                    checkpoint.pending = None;
                }
            }
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
            let mut sequence = current.info.last_sequence;
            let expected_revision = current.info.checkpoint_revision;
            let mut rows = Vec::new();
            let now = UnixMillis::now()?;
            for (causation, payload) in payloads {
                sequence = next_sequence(sequence)?;
                if let ConversationEvent::MessageAppended { .. } = &payload {
                    checkpoint.message_head = sequence;
                    checkpoint.message_count = checkpoint
                        .message_count
                        .checked_add(1)
                        .ok_or(AgentError::ResourceLimit("message count"))?;
                }
                let row = event_input(
                    current.info.id,
                    sequence,
                    causation.unwrap_or(command),
                    now,
                    payload,
                )?;
                let event: EventEnvelope = serde_json::from_value(row.payload.clone())
                    .map_err(|_| AgentError::InvalidData("new event encoding changed"))?;
                if let ConversationEvent::MessageAppended { message } = event.payload {
                    current.messages.push(RecordedMessage {
                        event_id: event.event_id,
                        sequence: event.sequence,
                        causation_id: event.causation_id,
                        message,
                    });
                }
                rows.push(row);
            }
            if let (Some(profile), Some(turn)) = (
                &retained_agent.settings_profile,
                checkpoint
                    .native
                    .as_ref()
                    .and_then(|native| native.turn.as_ref()),
            ) {
                profile.native_counters(
                    turn.command_id,
                    turn.tool_calls,
                    turn.charged_tokens,
                    output_reservation(turn)?,
                    turn.reported_input_tokens,
                    turn.reported_output_tokens,
                )?;
            }
            let update = ConversationStateUpdate {
                expected_revision,
                payload: checkpoint_payload(&checkpoint)?,
            };
            let append = AppendRequest {
                conversation_id: current.info.id.to_string(),
                command_id: CommandId::new().to_string(),
                expected_sequence: current.info.last_sequence,
                events: rows,
            };
            if let Some(profile) = &retained_agent.settings_profile {
                let checkpoint_bytes = encoded_len(&checkpoint, settings::MAX_CHECKPOINT)?;
                let future = checkpoint
                    .native
                    .as_ref()
                    .and_then(|native| native.turn.as_ref())
                    .and_then(|turn| turn.round.as_ref())
                    .map_or(0, |round| {
                        round
                            .calls
                            .iter()
                            .filter(|call| {
                                matches!(call.phase, CallPhase::Pending | CallPhase::Dispatched)
                            })
                            .count()
                            * 66
                            * 1024
                    });
                if checkpoint_bytes
                    .checked_add(future)
                    .is_none_or(|bytes| bytes > settings::MAX_CHECKPOINT)
                {
                    return Err(AgentError::ResourceLimit("restricted future checkpoint"));
                }
                for row in &append.events {
                    encoded_len(&row.payload, settings::MAX_EVENT)?;
                }
                let append_bytes = encoded_len(&append, settings::MAX_APPEND)?;
                profile.journal().settings_memory_capacity(
                    checkpoint_bytes + future + 2 * append_bytes + 4 * settings::MAX_BROKER_RECORD,
                )?;
            }
            let journal_command = append.command_id.clone();
            let revision = if append.events.is_empty() {
                retained_agent
                    .store
                    .checkpoint_conversation_state(
                        &current.info.id.to_string(),
                        current.info.last_sequence,
                        &journal_command,
                        &update,
                    )?
                    .revision
            } else {
                retained_agent.store.append_with_state(&append, &update)?;
                expected_revision
                    .checked_add(1)
                    .ok_or(AgentError::ResourceLimit("checkpoint revision"))?
            };
            current.info.last_sequence = sequence;
            current.info.checkpoint_revision = revision;
            current.info.message_count = checkpoint.message_count;
            current.info.pending_command_id = checkpoint
                .pending
                .as_ref()
                .map(|pending| pending.command.command_id);
            current.info.native_status = checkpoint.native.as_ref().map(NativeCheckpoint::status);
            current.checkpoint = checkpoint;
            Ok((current, true))
        })
        .await?;
        *conversation = saved;
        Ok(committed)
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
        let restricted = self.settings_profile.clone();
        let (captured, checkpoint, payloads, events) = blocking(move || {
            let _owner = owner; let _mapping_admission = mapping_admission;
            let captured = conversation;
            response.validate()?;
            if let Some(profile) = &restricted { profile.validate_response(&response)?; }
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
                            if restricted.is_some() { registry.validate_settings(&converted.name, &converted.arguments) } else { registry.validate(&converted.name, &converted.arguments) }.map_err(|_| "invalid_arguments")
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
            } else {
                turn.round = Some(Round { assistant_message_id: message.id, calls });
                turn.status = if turn.charged_tokens > turn.definition.limits.max_charged_tokens {
                    LoopStatus::Paused { reason: PauseReason::TokenLimit }
                } else { LoopStatus::ExecutingTools };
            }
            if restricted.is_some() { encoded_len(&summary, 4096)?; }
            checkpoint.last_turn = Some(summary);
            checkpoint.continuation = response.continuation;
            Ok((captured, checkpoint, payloads, events))
        }).await?;
        conversation = captured;
        self.save_native(&mut conversation, checkpoint, payloads, admission)
            .await?;
        let mut events = events;
        if conversation.checkpoint.pending.is_none() {
            let run = conversation
                .native_run()
                .ok_or(AgentError::NativeSetupRequired)?;
            events.push(TurnEvent::RunCompleted { run });
        }
        Ok((conversation, events))
    }

    async fn expired_round_reason(
        &self,
        conversation: &Conversation,
    ) -> Result<PauseReason, AgentError> {
        let turn = native(conversation)?
            .turn
            .as_ref()
            .ok_or(AgentError::NoPendingTurn)?;
        let calls = turn
            .round
            .as_ref()
            .ok_or(AgentError::InvalidData("expired tool phase lost its round"))?
            .calls
            .clone();
        let id = conversation.info.id.to_string();
        let task_id = turn.definition.task_id.to_string();
        let workspace = turn.definition.workspace.clone();
        let mode = turn.definition.mode;
        let owner = self.clone();
        blocking(move || {
            let retained_agent = owner;
            let mode = serde_json::to_value(mode).map_err(|_| AgentError::Worker)?;
            let mut retained = PauseReason::DurationLimit;
            for call in calls {
                if let CallPhase::Blocked { reason, .. } = &call.phase
                    && stop_priority(reason) > stop_priority(&retained)
                {
                    retained = reason.clone();
                }
                let operation_id = call.call.id.to_string();
                if let Some(row) = retained_agent.store.tool_call(&operation_id)? {
                    if row.id != operation_id
                        || row.conversation_id != id
                        || row.task_id.as_deref() != Some(task_id.as_str())
                        || row.tool_name != call.call.name
                        || row.request.get("arguments") != Some(&call.call.arguments)
                        || row.request.get("workspace_root").and_then(Value::as_str)
                            != Some(workspace.as_str())
                        || row.request.get("mode") != Some(&mode)
                        || row.request.get("version").and_then(Value::as_u64) != Some(1)
                    {
                        return Err(AgentError::InvalidData(
                            "expired operation journal identity differs from its native call",
                        ));
                    }
                    let reason = match row.state {
                        ToolCallState::Unknown => {
                            PauseReason::ReconciliationRequired { operation_id }
                        }
                        ToolCallState::Pending if matches!(call.phase, CallPhase::Dispatched) => {
                            PauseReason::ReconciliationRequired { operation_id }
                        }
                        ToolCallState::Running => PauseReason::OperationInFlight { operation_id },
                        ToolCallState::Succeeded
                        | ToolCallState::Failed
                        | ToolCallState::Cancelled
                        | ToolCallState::Denied
                        | ToolCallState::Pending => PauseReason::DurationLimit,
                    };
                    if stop_priority(&reason) > stop_priority(&retained) {
                        retained = reason;
                    }
                }
            }
            Ok(retained)
        })
        .await
    }

    async fn execute_round(
        &self,
        conversation: &mut Conversation,
        setup: Arc<ToolRunSetup>,
        cancellation: &CancellationToken,
        admission: Admission,
        stream_deadline: Instant,
    ) -> Result<Vec<TurnEvent>, AgentError> {
        let owner = self.clone();
        let scoped = setup.broker.clone();
        blocking(move || {
            let retained_agent = owner;
            ensure_task(
                &retained_agent.store,
                scoped.authority().task_id(),
                scoped.authority().conversation_id(),
            )
        })
        .await?;
        let mut checkpoint = conversation.checkpoint.clone();
        let turn = turn_mut(&mut checkpoint)?;
        let task_id = turn.definition.task_id;
        let deadline = stream_deadline.min(run_deadline(conversation)?);
        let current_time = UnixMillis::now()?.get();
        if !clock_allows(turn, current_time, stream_deadline) {
            let reason = self.expired_round_reason(conversation).await?;
            turn.status = LoopStatus::Paused {
                reason: reason.clone(),
            };
            self.save_native(conversation, checkpoint, Vec::new(), admission)
                .await?;
            return Ok(vec![TurnEvent::Paused { reason }]);
        }
        turn.last_observed_wall_ms = observed_floor(turn).max(current_time);
        let wall_deadline = turn.deadline_ms;
        let observed_wall = Arc::new(AtomicU64::new(turn.last_observed_wall_ms));
        let admission_stopped = Arc::new(AtomicBool::new(false));
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
        let call_observed_wall = Arc::clone(&observed_wall);
        let retained_owner = RetainedToolOwner::new(self.clone());
        let calls = stream::iter(requests)
            .map(move |(index, invocation)| {
                let broker = broker.clone();
                let token = child.clone();
                let observed_wall = Arc::clone(&call_observed_wall);
                let admission_stopped = Arc::clone(&admission_stopped);
                let owner = retained_owner.clone();
                async move {
                    let now = UnixMillis::now()?.get();
                    let floor = observed_wall.fetch_max(now, Ordering::AcqRel);
                    if admission_stopped.load(Ordering::Acquire)
                        || now < floor
                        || now >= wall_deadline
                        || Instant::now() >= deadline
                    {
                        admission_stopped.store(true, Ordering::Release);
                        Ok::<_, AgentError>((index, None))
                    } else {
                        Ok((
                            index,
                            Some(broker.execute_retained(invocation, token, owner).await),
                        ))
                    }
                }
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
            let Some(returned) = next else {
                break;
            };
            let (index, returned) = returned?;
            let mut checkpoint = conversation.checkpoint.clone();
            let turn = turn_mut(&mut checkpoint)?;
            turn.last_observed_wall_ms =
                observed_floor(turn).max(observed_wall.load(Ordering::Acquire));
            let Some(returned) = returned else {
                let call = turn
                    .round
                    .as_mut()
                    .and_then(|round| round.calls.get_mut(index))
                    .ok_or(AgentError::InvalidData(
                        "clock-stopped call lost its identity",
                    ))?;
                call.phase = CallPhase::Blocked { reason: PauseReason::DurationLimit, diagnostic: "Native deadline or observed clock rollback stopped this call before broker admission".into() };
                self.save_native(conversation, checkpoint, Vec::new(), Arc::clone(&admission))
                    .await?;
                continue;
            };
            let registry = Arc::clone(&setup.registry);
            let conversation_id = conversation.info.id;
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
                        if approval.scope.task_id != task_id.to_string()
                            || approval.scope.operation_id != operation_id.to_string()
                            || approval.scope.conversation_id != conversation_id.to_string()
                            || approval.scope.tool_name != call.call.name
                        {
                            return Err(AgentError::InvalidData("broker approval scope changed"));
                        }
                        let inspection = PermissionInspection {
                            approval_id: approval.id.clone(),
                            workspace_root: approval.scope.workspace_root.clone(),
                            mode: approval.scope.mode,
                            capabilities: approval.scope.capabilities.clone(),
                            targets: approval.scope.targets.clone(),
                            arguments_json: serde_json::to_string(&approval.arguments)
                                .map_err(|_| AgentError::InvalidData("approval arguments cannot be encoded"))?,
                            arguments_digest: approval.scope.arguments_digest.clone(),
                            requested_at: UnixMillis::try_from(u64::try_from(approval.requested_at).map_err(|_| AgentError::InvalidData("negative approval time"))?)?,
                            expires_at: UnixMillis::try_from(u64::try_from(approval.expires_at).map_err(|_| AgentError::InvalidData("negative approval expiry"))?)?,
                        };
                        let inspection = match inspection.validate() {
                            Ok(()) => Some(Box::new(inspection)),
                            Err(_) => {
                                tracing::warn!("approval inspection exceeds its public bounds; inspect the exact request on the host");
                                None
                            }
                        };
                        payloads.push(ConversationEvent::PermissionRequested {
                            task_id,
                            operation_id,
                            tool_call_id: call.call.id,
                            tool_name: call.call.name.clone(),
                            inspection,
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
                    if let CallPhase::Blocked { reason, .. } = &call.phase
                        && !matches!(reason, PauseReason::DurationLimit)
                    {
                        Some(reason.clone())
                    } else {
                        None
                    }
                })
            })
            .or_else(|| {
                round.calls.iter().find_map(|call| match &call.phase {
                    CallPhase::Blocked { reason, .. } => Some(reason.clone()),
                    _ => None,
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

fn stop_priority(reason: &PauseReason) -> u8 {
    match reason {
        PauseReason::ReconciliationRequired { .. } | PauseReason::BrokerRejected { .. } => 3,
        PauseReason::OperationInFlight { .. } => 2,
        _ => 0,
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
        reserved_output_tokens: turn
            .reserved_output_tokens
            .or_else(|| reconstructed_output_reservation(turn)),
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
        if !definition_matches(registry, &definition, false)?
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
    restricted: bool,
) -> Result<bool, AgentError> {
    let actual = registered_definition(registry, &definition.name, restricted)?;
    Ok(actual.name == definition.name
        && actual.input_schema == definition.input_schema
        && (actual.description == definition.description
            || definition.description
                == format!(
                    "Execution requires explicit host approval for the exact operation. {}",
                    actual.description
                )))
}

impl Agent {
    fn check_settings_setup(
        &self,
        conversation_id: ConversationId,
        command_id: CommandId,
        setup: Option<&ToolRunSetup>,
        provider: Option<&Arc<dyn ModelProvider>>,
    ) -> Result<(), AgentError> {
        match (
            &self.settings_profile,
            setup.and_then(|setup| setup.broker.settings_profile()),
        ) {
            (None, None) => Ok(()),
            (Some(profile), Some(scoped)) if Arc::ptr_eq(profile, scoped) => {
                profile.check_turn(conversation_id, command_id)?;
                if let Some(provider) = provider {
                    profile.check_provider(provider)?;
                }
                let setup = setup.ok_or(AgentError::NativeSetupRequired)?;
                if !setup.memory.is_empty()
                    || setup.selected_tools != profile.definitions()
                    || setup.provider_resume != ProviderResume::OnlyUnstarted
                    || setup.limits.max_model_attempts > 4
                    || setup.limits.max_tool_calls > 8
                    || setup.limits.max_calls_per_round > 4
                    || setup.limits.max_parallel_calls != 1
                    || setup.limits.max_selected_tools != 7
                    || setup.budget.max_context_tokens > 16 * 1024
                    || setup.budget.max_request_bytes > 1024 * 1024
                    || setup.budget.max_compaction_bytes > 8 * 1024
                    || setup.limits.max_duration_ms > 120_000
                    || !profile.journal().shares_connection(&self.store)
                {
                    return Err(AgentError::ConfigurationMismatch);
                }
                Ok(())
            }
            _ => Err(AgentError::ConfigurationMismatch),
        }
    }

    pub(crate) async fn observe_settings_provider(&self) -> Result<(), AgentError> {
        if self.settings_profile.is_none() {
            return Ok(());
        }
        let owner = self.clone();
        blocking(move || {
            let owner = owner;
            let profile = owner
                .settings_profile
                .as_ref()
                .ok_or(AgentError::ConfigurationMismatch)?;
            let _observation = profile.observe_provider()?;
            Ok(())
        })
        .await
    }
}

fn settings_preflight(checkpoint: &Checkpoint) -> Result<(), AgentError> {
    let mut skeleton = checkpoint.clone();
    skeleton.continuation = None;
    skeleton.last_turn = None;
    let native = skeleton
        .native
        .as_mut()
        .ok_or(AgentError::NativeSetupRequired)?;
    native.context.binding = None;
    let turn = native.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
    turn.round = None;
    let fixed = encoded_len(&skeleton, settings::MAX_CHECKPOINT)?;
    let continuation = settings::MAX_CONTINUATION + 33;
    let binding = continuation + 323;
    let round = 74 + 4 * (18 + 625 + settings::MAX_ARGUMENTS + 124 + 65_538) + 3;
    let complete_future_delta = continuation + binding + round - 12 + 4096 + 4096;
    if fixed
        .checked_add(complete_future_delta)
        .is_none_or(|bytes| bytes > settings::MAX_CHECKPOINT)
    {
        return Err(AgentError::ResourceLimit(
            "restricted complete future checkpoint",
        ));
    }
    Ok(())
}
