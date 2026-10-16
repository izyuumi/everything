use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{Agent, AgentError, ConversationConfig, ModelProvider, encoded_len};
use crate::model::accounting::{ProviderAccounting, ProviderObservation};
use crate::model::{ContentBlock, ModelResponse, ToolDefinition};
use crate::protocol::{CommandId, ConversationId, TaskId, UnixMillis};
use crate::storage::Store;
use crate::storage::settings_agent::{
    MAX_SETTINGS_MUTATIONS, MAX_SETTINGS_TURNS, SettingsFencePhase, SettingsSessionReservation,
    SettingsTurnFence,
};

pub(crate) const MAX_INPUT: usize = 8 * 1024;
pub(crate) const MAX_TEXT: usize = 16 * 1024;
pub(crate) const MAX_RESPONSE: usize = 512 * 1024;
pub(crate) const MAX_CONTINUATION: usize = 256 * 1024;
pub(crate) const MAX_ARGUMENTS: usize = 16 * 1024;
pub(crate) const MAX_EVENT: usize = 192 * 1024;
pub(crate) const MAX_APPEND: usize = 512 * 1024;
pub(crate) const MAX_CHECKPOINT: usize = 2 * 1024 * 1024;
pub(crate) const MAX_BROKER_RECORD: usize = 64 * 1024;
const TRANSITION_HEADROOM: usize = MAX_CHECKPOINT + 4 * MAX_APPEND + 8 * MAX_BROKER_RECORD;
const OUTCOME_SLOTS: usize = 32;

#[derive(Clone, Copy)]
pub(crate) enum SettingsProviderStop {
    StreamBoundary,
    ProvisionalTextLimit,
    ResponseShape,
    InvalidAccounting,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsOutcome {
    Reserved,
    Completed,
    Unknown,
}

#[derive(Clone, Copy, Serialize)]
struct OutcomeSlot {
    operation_id: CommandId,
    outcome: SettingsOutcome,
}

struct AttemptFloor {
    attempt_id: CommandId,
    reservation: u64,
    charged: u64,
    reported_input: u64,
    reported_output: u64,
}

struct ProfileState {
    turn: Option<SettingsTurnFence>,
    turns: usize,
    attempt: Option<AttemptFloor>,
    turn_deadline: Option<Instant>,
    observed_wall: u64,
    provider_stop: Option<(CommandId, SettingsProviderStop)>,
    outcomes: [Option<OutcomeSlot>; OUTCOME_SLOTS],
}

pub(crate) struct SettingsProfile {
    #[cfg(unix)]
    source_admission: SettingsProviderAdmission,
    main: Store,
    journal: Store,
    reservation: SettingsSessionReservation,
    configuration: ConversationConfig,
    definitions: Vec<ToolDefinition>,
    names: BTreeSet<String>,
    provider: Arc<dyn ModelProvider>,
    pub(crate) accounting: Arc<ProviderAccounting>,
    cancellation: CancellationToken,
    deadline: Instant,
    retired: AtomicBool,
    broker_intake_closed: AtomicBool,
    broker_workers: AtomicUsize,
    broker_changed: tokio::sync::Notify,
    state: Mutex<ProfileState>,
}

#[derive(Clone)]
pub(crate) struct SettingsToolCapability {
    profile: Arc<SettingsProfile>,
    operation_id: String,
    task_id: String,
    tool: String,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeAttemptFloor {
    pub attempt_id: CommandId,
    pub model_attempts: u32,
    pub tool_calls: u32,
    pub charged_tokens: u64,
    pub reserved_output_tokens: u64,
    pub reported_input_tokens: u64,
    pub reported_output_tokens: u64,
    pub reservation: u64,
    pub deadline_ms: u64,
    pub observed_wall_ms: u64,
}

impl SettingsProfile {
    pub(crate) fn check_live(&self) -> Result<(), AgentError> {
        if self.retired.load(Ordering::Acquire)
            || self.cancellation.is_cancelled()
            || Instant::now() >= self.deadline
            || UnixMillis::now()? >= self.reservation.expires_at
        {
            return Err(AgentError::Cancelled);
        }
        Ok(())
    }

    pub(crate) fn session_id(&self) -> Uuid {
        self.reservation.session_id
    }

    pub(crate) fn native_session_id(&self) -> Uuid {
        self.reservation.native_session_id
    }

    pub(crate) fn create_command_id(&self) -> CommandId {
        self.reservation.create_command_id
    }

    pub(crate) fn conversation_id(&self) -> ConversationId {
        self.reservation.conversation_id
    }

    pub(crate) fn shares_main(&self, main: &Store) -> bool {
        self.main.shares_connection(main)
    }

    pub(crate) fn journal(&self) -> Store {
        self.journal.clone()
    }

    pub(crate) fn provider_handle(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    pub(crate) fn broker_is_closed_and_idle(&self) -> bool {
        self.broker_intake_closed.load(Ordering::Acquire)
            && self.broker_workers.load(Ordering::Acquire) == 0
    }

    pub(crate) fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub(crate) fn configuration(&self) -> &ConversationConfig {
        &self.configuration
    }

    pub(crate) fn check_conversation(&self, id: ConversationId) -> Result<(), AgentError> {
        self.check_live()?;
        if id != self.reservation.conversation_id {
            return Err(AgentError::ConfigurationMismatch);
        }
        Ok(())
    }

    pub(crate) fn check_turn(
        &self,
        id: ConversationId,
        command: CommandId,
    ) -> Result<(), AgentError> {
        self.check_conversation(id)?;
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        check_turn_deadline(&mut state)?;
        if state.turn.as_ref().is_none_or(|turn| {
            turn.command_id != command || turn.phase != SettingsFencePhase::Armed
        }) {
            return Err(AgentError::ConfigurationMismatch);
        }
        Ok(())
    }

    pub(crate) fn check_provider(
        &self,
        provider: &Arc<dyn ModelProvider>,
    ) -> Result<(), AgentError> {
        self.check_live()?;
        if !Arc::ptr_eq(provider, &self.provider) {
            return Err(AgentError::ConfigurationMismatch);
        }
        Ok(())
    }

    pub(crate) fn begin_turn(&self, command: CommandId) -> Result<SettingsTurnFence, AgentError> {
        self.check_live()?;
        if self.broker_workers.load(Ordering::Acquire) != 0 {
            return Err(AgentError::Busy);
        }
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        if state.turns >= MAX_SETTINGS_TURNS
            || state
                .turn
                .as_ref()
                .is_some_and(|turn| turn.phase != SettingsFencePhase::Settled)
        {
            return Err(AgentError::Busy);
        }
        self.journal.settings_memory_capacity(TRANSITION_HEADROOM)?;
        let started_at = UnixMillis::now()?;
        let monotonic_start = Instant::now();
        let deadline = UnixMillis::try_from(
            started_at
                .get()
                .saturating_add(120_000)
                .min(self.reservation.expires_at.get()),
        )?;
        let fence = SettingsTurnFence {
            version: 1,
            session_id: self.reservation.session_id,
            command_id: command,
            task_id: TaskId::try_from(*command.as_uuid())?,
            phase: SettingsFencePhase::Armed,
            attempt_id: None,
            model_attempts: 0,
            tool_calls: 0,
            charged_tokens: 0,
            reserved_output_tokens: 0,
            reported_input_tokens: 0,
            reported_output_tokens: 0,
            started_at,
            deadline,
            last_observed_wall: started_at,
            mutation_ids: Vec::with_capacity(MAX_SETTINGS_MUTATIONS),
            provider_complete: false,
            usage_complete: false,
        };
        self.main.arm_settings_turn(&fence)?;
        self.broker_intake_closed.store(false, Ordering::Release);
        state.turn_deadline = Some(
            monotonic_start
                .checked_add(std::time::Duration::from_millis(
                    deadline.get().saturating_sub(started_at.get()),
                ))
                .ok_or(AgentError::ResourceLimit("restricted monotonic deadline"))?
                .min(self.deadline),
        );
        state.observed_wall = started_at.get();
        state.turn = Some(fence.clone());
        state.turns += 1;
        state.attempt = None;
        state.provider_stop = None;
        state.outcomes.fill(None);
        state.outcomes[0] = Some(OutcomeSlot {
            operation_id: command,
            outcome: SettingsOutcome::Reserved,
        });
        Ok(fence)
    }

    pub(crate) fn before_attempt(&self, floor: NativeAttemptFloor) -> Result<(), AgentError> {
        self.check_live()?;
        self.journal.settings_memory_capacity(TRANSITION_HEADROOM)?;
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        check_turn_deadline(&mut state)?;
        let turn = state.turn.as_mut().ok_or(AgentError::NoPendingTurn)?;
        if turn.phase != SettingsFencePhase::Armed
            || floor.model_attempts <= turn.model_attempts
            || floor.model_attempts > 4
            || floor.tool_calls > 8
            || floor.deadline_ms > turn.deadline.get()
            || floor.charged_tokens < turn.charged_tokens
            || floor.reserved_output_tokens < turn.reserved_output_tokens
            || floor.reported_input_tokens < turn.reported_input_tokens
            || floor.reported_output_tokens < turn.reported_output_tokens
        {
            return Err(AgentError::ConfigurationMismatch);
        }
        let mut next = turn.clone();
        next.attempt_id = Some(floor.attempt_id);
        next.model_attempts = floor.model_attempts;
        next.tool_calls = floor.tool_calls;
        next.charged_tokens = floor.charged_tokens;
        next.reserved_output_tokens = floor.reserved_output_tokens;
        next.reported_input_tokens = floor.reported_input_tokens;
        next.reported_output_tokens = floor.reported_output_tokens;
        next.last_observed_wall = UnixMillis::try_from(floor.observed_wall_ms)?;
        next.provider_complete = false;
        next.usage_complete = false;
        reserve_outcome(&mut state, floor.attempt_id)?;
        self.main.update_settings_turn(&next)?;
        state.turn = Some(next);
        state.attempt = Some(AttemptFloor {
            attempt_id: floor.attempt_id,
            reservation: floor.reservation,
            charged: floor.charged_tokens,
            reported_input: floor.reported_input_tokens,
            reported_output: floor.reported_output_tokens,
        });
        self.accounting.arm(floor.attempt_id)?;
        Ok(())
    }

    pub(crate) fn native_counters(
        &self,
        command: CommandId,
        tool_calls: u32,
        charged: u64,
        reserved: u64,
        reported_input: u64,
        reported_output: u64,
    ) -> Result<(), AgentError> {
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        let mut next = state.turn.clone().ok_or(AgentError::NoPendingTurn)?;
        if next.command_id != command || next.phase != SettingsFencePhase::Armed || tool_calls > 8 {
            return Err(AgentError::ConfigurationMismatch);
        }
        next.tool_calls = next.tool_calls.max(tool_calls);
        next.charged_tokens = next.charged_tokens.max(charged);
        next.reserved_output_tokens = next.reserved_output_tokens.max(reserved);
        next.reported_input_tokens = next.reported_input_tokens.max(reported_input);
        next.reported_output_tokens = next.reported_output_tokens.max(reported_output);
        if state.turn.as_ref() != Some(&next) {
            self.main.update_settings_turn(&next)?;
            state.turn = Some(next);
        }
        Ok(())
    }

    pub(crate) fn note_provider_stop(
        &self,
        boundary: SettingsProviderStop,
    ) -> Result<(), AgentError> {
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        let turn = state.turn.as_ref().ok_or(AgentError::NoPendingTurn)?;
        if turn.phase != SettingsFencePhase::Armed {
            return Err(AgentError::ConfigurationMismatch);
        }
        let attempt_id = turn.attempt_id.ok_or(AgentError::ConfigurationMismatch)?;
        state.provider_stop = Some((attempt_id, boundary));
        Ok(())
    }

    pub(crate) fn provider_stop_observed(&self) -> Result<bool, AgentError> {
        let state = self.state.lock().map_err(|_| AgentError::Worker)?;
        Ok(state.provider_stop.is_some_and(|(id, _)| {
            state
                .turn
                .as_ref()
                .is_some_and(|turn| turn.attempt_id == Some(id))
        }))
    }

    pub(crate) fn provider_observation(&self) -> Result<Option<ProviderObservation>, AgentError> {
        let state = self.state.lock().map_err(|_| AgentError::Worker)?;
        state
            .attempt
            .as_ref()
            .map(|attempt| {
                self.accounting
                    .snapshot(attempt.attempt_id)
                    .map_err(AgentError::from)
            })
            .transpose()
    }

    pub(crate) fn observe_provider(&self) -> Result<Option<ProviderObservation>, AgentError> {
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        let Some(attempt) = &state.attempt else {
            return Ok(None);
        };
        let observation = self.accounting.snapshot(attempt.attempt_id)?;
        let mut next = state.turn.clone().ok_or(AgentError::NoPendingTurn)?;
        next.charged_tokens = attempt
            .charged
            .checked_add(
                observation
                    .usage
                    .total_tokens()
                    .saturating_sub(attempt.reservation),
            )
            .ok_or(AgentError::ResourceLimit("restricted token accounting"))?
            .max(next.charged_tokens);
        next.reported_input_tokens = attempt
            .reported_input
            .checked_add(observation.usage.input_tokens)
            .ok_or(AgentError::ResourceLimit("restricted reported input"))?
            .max(next.reported_input_tokens);
        next.reported_output_tokens = attempt
            .reported_output
            .checked_add(observation.usage.output_tokens)
            .ok_or(AgentError::ResourceLimit("restricted reported output"))?
            .max(next.reported_output_tokens);
        next.provider_complete |= observation.terminal_observed;
        next.usage_complete |= observation.terminal_observed && !observation.invalid;
        if state.turn.as_ref() != Some(&next) {
            self.main.update_settings_turn(&next)?;
            state.turn = Some(next);
        }
        if observation.invalid {
            return Err(AgentError::RestrictedProviderAccounting);
        }
        Ok(Some(observation))
    }

    pub(crate) fn validate_response(&self, response: &ModelResponse) -> Result<(), AgentError> {
        self.check_live()?;
        encoded_len(response, MAX_RESPONSE)?;
        encoded_len(&response.stop_reason, 1024)?;
        if response.message.content.len() > 32 {
            return Err(AgentError::ResourceLimit("restricted content blocks"));
        }
        if let Some(continuation) = &response.continuation {
            encoded_len(continuation.state(), MAX_CONTINUATION)?;
        }
        let mut text_bytes = 0usize;
        let mut calls = 0usize;
        for block in &response.message.content {
            match block {
                ContentBlock::Text(text) => {
                    text_bytes = text_bytes
                        .checked_add(text.len())
                        .ok_or(AgentError::ResourceLimit("restricted assistant text"))?;
                    if text_bytes > MAX_TEXT {
                        return Err(AgentError::ResourceLimit("restricted assistant text"));
                    }
                }
                ContentBlock::ToolCall(call) => {
                    calls += 1;
                    if !self.names.contains(&call.name) {
                        return Err(AgentError::ToolsUnsupported);
                    }
                    encoded_len(&call.arguments, MAX_ARGUMENTS)?;
                }
                ContentBlock::ToolResult(_) => {
                    return Err(AgentError::InvalidData("restricted assistant tool result"));
                }
            }
        }
        if calls > 4 {
            return Err(AgentError::ResourceLimit("restricted round calls"));
        }
        Ok(())
    }

    pub(crate) fn check_tool(
        &self,
        conversation: &str,
        task: &str,
        tool: &str,
    ) -> Result<(), AgentError> {
        self.check_live()?;
        if conversation != self.conversation_id().to_string() || !self.names.contains(tool) {
            return Err(AgentError::ToolsUnsupported);
        }
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        check_turn_deadline(&mut state)?;
        if state.turn.as_ref().is_none_or(|turn| {
            turn.task_id.to_string() != task || turn.phase != SettingsFencePhase::Armed
        }) {
            return Err(AgentError::ConfigurationMismatch);
        }
        Ok(())
    }

    pub(crate) fn tool_capability(
        self: &Arc<Self>,
        operation: &str,
        task: &str,
        tool: &str,
    ) -> Result<SettingsToolCapability, AgentError> {
        self.check_tool(&self.conversation_id().to_string(), task, tool)?;
        let id: CommandId = operation.parse()?;
        {
            let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
            reserve_outcome(&mut state, id)?;
        }
        self.journal.settings_memory_capacity(TRANSITION_HEADROOM)?;
        Ok(SettingsToolCapability {
            profile: Arc::clone(self),
            operation_id: operation.into(),
            task_id: task.into(),
            tool: tool.into(),
        })
    }

    pub(crate) fn link_mutation(&self, operation: CommandId) -> Result<(), AgentError> {
        self.check_live()?;
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        let mut next = state.turn.clone().ok_or(AgentError::NoPendingTurn)?;
        if !next.mutation_ids.contains(&operation) {
            if next.mutation_ids.len() >= MAX_SETTINGS_MUTATIONS {
                return Err(AgentError::ResourceLimit("restricted mutation links"));
            }
            next.mutation_ids.push(operation);
            next.mutation_ids.sort_unstable();
            self.main.update_settings_turn(&next)?;
            state.turn = Some(next);
        }
        Ok(())
    }

    pub(crate) fn record_outcome(
        &self,
        operation: CommandId,
        outcome: SettingsOutcome,
    ) -> Result<(), AgentError> {
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        let slot = state
            .outcomes
            .iter_mut()
            .flatten()
            .find(|slot| slot.operation_id == operation)
            .ok_or(AgentError::InvalidData("restricted outcome reservation"))?;
        slot.outcome = outcome;
        Ok(())
    }

    pub(crate) fn fence(&self) -> Result<Option<SettingsTurnFence>, AgentError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| AgentError::Worker)?
            .turn
            .clone())
    }

    pub(crate) fn finish_turn(&self, phase: SettingsFencePhase) -> Result<(), AgentError> {
        let observation = match self.observe_provider() {
            Ok(observation) => observation,
            Err(AgentError::RestrictedProviderAccounting)
                if matches!(
                    phase,
                    SettingsFencePhase::StoppedUnusable
                        | SettingsFencePhase::StoppedUncertain
                        | SettingsFencePhase::Unknown
                ) =>
            {
                self.provider_observation()?
            }
            Err(error) => return Err(error),
        };
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        let mut next = state.turn.clone().ok_or(AgentError::NoPendingTurn)?;
        if phase == SettingsFencePhase::Armed {
            return Err(AgentError::InvalidData("restricted terminal phase"));
        }
        if matches!(
            phase,
            SettingsFencePhase::StoppedUncertain | SettingsFencePhase::StoppedUnusable
        ) {
            if next.tool_calls != 0 || !next.mutation_ids.is_empty() {
                return Err(AgentError::ConfigurationMismatch);
            }
            let terminal = observation
                .as_ref()
                .is_some_and(|value| value.terminal_observed);
            if (phase == SettingsFencePhase::StoppedUnusable) != terminal {
                return Err(AgentError::ConfigurationMismatch);
            }
            next.provider_complete = terminal;
            next.usage_complete =
                terminal && observation.as_ref().is_some_and(|value| !value.invalid);
        }
        next.phase = phase;
        self.main.update_settings_turn(&next)?;
        state.turn = Some(next);
        if phase != SettingsFencePhase::Settled {
            self.retire();
        }
        Ok(())
    }

    pub(crate) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.cancellation.cancel();
    }
}

impl SettingsToolCapability {
    pub(crate) fn check(&self, operation: &str, task: &str, tool: &str) -> Result<(), AgentError> {
        if operation != self.operation_id || task != self.task_id || tool != self.tool {
            return Err(AgentError::ConfigurationMismatch);
        }
        self.profile
            .check_tool(&self.profile.conversation_id().to_string(), task, tool)
    }

    pub(crate) fn profile(&self) -> &Arc<SettingsProfile> {
        &self.profile
    }
}

fn reserve_outcome(state: &mut ProfileState, operation: CommandId) -> Result<(), AgentError> {
    if state
        .outcomes
        .iter()
        .flatten()
        .any(|slot| slot.operation_id == operation)
    {
        return Ok(());
    }
    let slot = state
        .outcomes
        .iter_mut()
        .find(|slot| slot.is_none())
        .ok_or(AgentError::ResourceLimit("restricted outcomes"))?;
    let value = OutcomeSlot {
        operation_id: operation,
        outcome: SettingsOutcome::Reserved,
    };
    encoded_len(&value, 1024)?;
    *slot = Some(value);
    Ok(())
}

#[cfg(unix)]
impl Agent {
    pub(crate) fn settings_view(&self, capture: SettingsViewCapture) -> Result<Self, AgentError> {
        let SettingsViewCapture {
            journal,
            reservation,
            binding,
            configuration,
            definitions,
            provider,
            accounting,
            source_admission,
        } = capture;
        use crate::host::settings::contract::SettingsToolName;
        if self.settings_profile.is_some()
            || self._foreground.is_none()
            || self.store.shares_connection(&journal)
        {
            return Err(AgentError::ConfigurationMismatch);
        }
        let names: BTreeSet<_> = SettingsToolName::ALL
            .iter()
            .map(|name| name.provider_name().to_owned())
            .collect();
        let defined: BTreeSet<_> = definitions
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&definitions)
                    .map_err(|_| AgentError::InvalidData("restricted definitions"))?
            )
        );
        if definitions.len() != 7
            || names != defined
            || digest != reservation.tools_sha256
            || binding.session_id != reservation.session_id
            || binding.native_session_id != reservation.native_session_id
            || binding.host_id != reservation.host_id
            || binding.principal_id != reservation.principal_id
            || binding.expires_at != reservation.expires_at
            || self
                .store
                .settings_session(reservation.session_id)?
                .as_ref()
                != Some(&reservation)
        {
            return Err(AgentError::ConfigurationMismatch);
        }
        encoded_len(&definitions, 64 * 1024)?;
        journal.settings_memory_capacity(TRANSITION_HEADROOM)?;
        let profile = Arc::new(SettingsProfile {
            source_admission,
            main: self.store.clone(),
            journal: journal.clone(),
            reservation,
            configuration,
            definitions,
            names,
            provider,
            accounting,
            cancellation: binding.cancellation,
            deadline: binding.deadline,
            retired: AtomicBool::new(false),
            broker_intake_closed: AtomicBool::new(true),
            broker_workers: AtomicUsize::new(0),
            broker_changed: tokio::sync::Notify::new(),
            state: Mutex::new(ProfileState {
                turn: None,
                turns: 0,
                attempt: None,
                turn_deadline: None,
                observed_wall: 0,
                provider_stop: None,
                outcomes: [None; OUTCOME_SLOTS],
            }),
        });
        profile.check_live()?;
        let mut selected = self.clone();
        selected.store = journal;
        selected.settings_profile = Some(profile);
        Ok(selected)
    }

    pub(crate) fn settings_profile(&self) -> Result<Arc<SettingsProfile>, AgentError> {
        self.settings_profile
            .as_ref()
            .map(Arc::clone)
            .ok_or(AgentError::ConfigurationMismatch)
    }
}

impl std::fmt::Debug for SettingsToolCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SettingsToolCapability([sealed])")
    }
}

pub(crate) struct SettingsBrokerWork {
    profile: Arc<SettingsProfile>,
}

impl SettingsProfile {
    pub(crate) fn broker_work(self: &Arc<Self>) -> Result<SettingsBrokerWork, AgentError> {
        self.check_live()?;
        let mut state = self.state.lock().map_err(|_| AgentError::Worker)?;
        check_turn_deadline(&mut state)?;
        if self.broker_intake_closed.load(Ordering::Acquire) {
            return Err(AgentError::Cancelled);
        }
        let mut count = self.broker_workers.load(Ordering::Acquire);
        loop {
            let next = count
                .checked_add(1)
                .filter(|next| *next <= 8)
                .ok_or(AgentError::Busy)?;
            match self.broker_workers.compare_exchange_weak(
                count,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => count = actual,
            }
        }
        Ok(SettingsBrokerWork {
            profile: Arc::clone(self),
        })
    }

    pub(crate) async fn close_broker_and_wait(&self) -> Result<(), AgentError> {
        {
            let _state = self.state.lock().map_err(|_| AgentError::Worker)?;
            self.broker_intake_closed.store(true, Ordering::Release);
        }
        loop {
            let changed = self.broker_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.broker_workers.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            changed.await;
        }
    }
}

impl Drop for SettingsBrokerWork {
    fn drop(&mut self) {
        let mut count = self.profile.broker_workers.load(Ordering::Acquire);
        loop {
            let Some(next) = count.checked_sub(1) else {
                self.profile.retire();
                break;
            };
            match self.profile.broker_workers.compare_exchange_weak(
                count,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => count = actual,
            }
        }
        self.profile.broker_changed.notify_waiters();
    }
}

#[cfg(unix)]
pub(crate) struct SettingsProviderAdmission {
    pub factory: crate::host::providers::HostProviderFactory,
    pub binding: crate::host::providers::SettingsProviderBinding,
    pub mutation: Arc<tokio::sync::Mutex<()>>,
}

#[cfg(unix)]
pub(crate) struct SettingsViewCapture {
    pub journal: Store,
    pub reservation: SettingsSessionReservation,
    pub binding: crate::host::settings::SettingsAgentBinding,
    pub configuration: ConversationConfig,
    pub definitions: Vec<ToolDefinition>,
    pub provider: Arc<dyn ModelProvider>,
    pub accounting: Arc<ProviderAccounting>,
    pub source_admission: SettingsProviderAdmission,
}

#[cfg(unix)]
impl SettingsProfile {
    pub(crate) async fn admit_source(
        &self,
        cancellation: CancellationToken,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, AgentError> {
        self.check_live()?;
        let admission = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
            guard = Arc::clone(&self.source_admission.mutation).lock_owned() => guard,
        };
        self.check_live()?;
        if !self
            .source_admission
            .factory
            .settings_binding_matches(&self.source_admission.binding, cancellation)
            .await
            .map_err(|_| AgentError::ConfigurationMismatch)?
        {
            self.retire();
            return Err(AgentError::ConfigurationMismatch);
        }
        Ok(admission)
    }
}

fn check_turn_deadline(state: &mut ProfileState) -> Result<(), AgentError> {
    let turn = state.turn.as_ref().ok_or(AgentError::NoPendingTurn)?;
    let now = UnixMillis::now()?.get();
    if now < state.observed_wall.max(turn.last_observed_wall.get())
        || now >= turn.deadline.get()
        || state
            .turn_deadline
            .is_none_or(|deadline| Instant::now() >= deadline)
    {
        return Err(AgentError::Cancelled);
    }
    state.observed_wall = state.observed_wall.max(now);
    Ok(())
}
