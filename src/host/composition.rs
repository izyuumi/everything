use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::commands::EffectiveCommand;
use super::inbox::{CommandOrigin, HostInbox, InboxInput};
use super::mcp::{McpService, McpServiceStatus};
use super::memory::{MemoryService, MemoryStatus};
use super::runtime::{HostError, HostInner};
use super::selection::{
    PreparedSelectionTicket, SelectionBinding, SelectionCatalog, SelectionConfigurationStatus,
    SelectionGeneration, SelectionPreflight, SelectionPreflightStatus, SelectionPreparation,
    SelectionSettings, SelectionSummary,
};
use super::settings::HostSettings;
use crate::permissions::ExecutionAuthority;
use crate::protocol::{Command, CommandEnvelope, CommandId, MessageContent, TaskId};
use crate::scheduler::{
    ExecutionPolicy, PreparedTaskInputs, SchedulerError, TaskPreparation, TaskPreparationRequest,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompositionStatus {
    pub memory: MemoryStatus,
    pub mcp: McpServiceStatus,
    pub selection: SelectionPreflightStatus,
    pub selection_configuration: SelectionConfigurationStatus,
    pub restart_required: RestartRequired,
    pub active_memory: super::memory::MemorySettings,
    pub active_mcp: super::mcp::McpSettings,
    pub active_selection: super::selection::SelectionSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestartRequired {
    pub memory: bool,
    pub mcp: bool,
    pub selection: bool,
}

pub(super) struct Composition {
    pub(super) memory: MemoryService,
    pub(super) mcp: McpService,
    pub(super) selection: SelectionPreflight,
    pub(super) catalog: SelectionCatalog,
    pub(super) effective: HostSettings,
    closed: AtomicBool,
    admission: Arc<RwLock<()>>,
    inbox: HostInbox,
    handoffs: Mutex<BTreeMap<CommandId, SelectionHandoff>>,
    agent: crate::agent::Agent,
}

#[derive(Clone)]
struct SelectionHandoff {
    ticket: Uuid,
    binding: SelectionBinding,
    ready: bool,
    expires_at: Instant,
}

impl Composition {
    pub(super) fn new(
        memory: MemoryService,
        mcp: McpService,
        selection: SelectionPreflight,
        catalog: SelectionCatalog,
        effective: HostSettings,
        inbox: HostInbox,
        agent: crate::agent::Agent,
    ) -> Self {
        Self {
            memory,
            mcp,
            selection,
            catalog,
            effective,
            closed: AtomicBool::new(false),
            admission: Arc::new(RwLock::new(())),
            inbox,
            handoffs: Mutex::new(BTreeMap::new()),
            agent,
        }
    }

    pub(super) async fn enter(&self) -> Result<OwnedRwLockReadGuard<()>, HostError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostError::Draining);
        }
        let admission = Arc::clone(&self.admission).read_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(HostError::Draining);
        }
        Ok(admission)
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(super) fn suspend_selection(&self) -> Result<SelectionGeneration, HostError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostError::Draining);
        }
        let generation = self.selection.suspend_remote()?;
        self.handoffs.lock().map_err(|_| HostError::Worker)?.clear();
        Ok(generation)
    }

    pub(super) fn configure_selection(
        &self,
        generation: SelectionGeneration,
        settings: SelectionSettings,
        enabled: bool,
    ) -> Result<SelectionConfigurationStatus, HostError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(HostError::Draining);
        }
        Ok(self
            .selection
            .configure_remote(generation, settings, enabled)?)
    }

    fn selection_binding(
        &self,
        original: &Value,
        execution: &ExecutionPolicy,
        conversation_id: crate::protocol::ConversationId,
        task_id: TaskId,
        command_id: CommandId,
    ) -> Result<SelectionBinding, HostError> {
        Ok(SelectionBinding {
            conversation_id,
            task_id,
            command_id,
            original_sha256: digest(original)?,
            execution_sha256: digest(execution)?,
            catalog_sha256: self.catalog.sha256().into(),
        })
    }

    async fn take_selection(
        &self,
        request: &TaskPreparationRequest,
    ) -> Result<Option<Vec<crate::model::ToolDefinition>>, HostError> {
        let saved = self
            .handoffs
            .lock()
            .map_err(|_| HostError::Worker)?
            .get(&request.command_id)
            .cloned();
        let Some(saved) = saved else {
            return Ok(None);
        };
        if Instant::now() >= saved.expires_at {
            self.handoffs
                .lock()
                .map_err(|_| HostError::Worker)?
                .remove(&request.command_id);
            match self.selection.discard(saved.ticket) {
                Ok(()) | Err(super::selection::SelectionPreflightError::Unavailable) => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(None);
        }
        if !saved.ready {
            self.handoffs
                .lock()
                .map_err(|_| HostError::Worker)?
                .remove(&request.command_id);
            match self.selection.discard(saved.ticket) {
                Ok(()) | Err(super::selection::SelectionPreflightError::Unavailable) => {}
                Err(error) => return Err(error.into()),
            }
            return Ok(None);
        }
        let inbox = self.inbox.clone();
        let agent = self.agent.clone();
        let command_id = request.command_id;
        let record = tokio::task::spawn_blocking(move || {
            let _agent = agent;
            inbox.record(command_id)
        })
        .await
        .map_err(|_| HostError::Worker)??
        .ok_or(HostError::Invalid("selection command is missing"))?;
        if record.origin != CommandOrigin::LocalOwner {
            return Err(HostError::Invalid(
                "selection handoff requires its local owner command",
            ));
        }
        let original = record
            .original
            .as_ref()
            .ok_or(HostError::Invalid("selection original was archived"))?;
        let binding = self.selection_binding(
            original,
            &request.execution,
            request.conversation_id,
            request.task_id,
            request.command_id,
        )?;
        if binding != saved.binding {
            self.handoffs
                .lock()
                .map_err(|_| HostError::Worker)?
                .remove(&request.command_id);
            match self.selection.discard(saved.ticket) {
                Ok(()) | Err(super::selection::SelectionPreflightError::Unavailable) => {}
                Err(error) => return Err(error.into()),
            }
            tracing::warn!("optional selection inputs changed; local selection retained");
            return Ok(None);
        }
        let mut handoffs = self.handoffs.lock().map_err(|_| HostError::Worker)?;
        if !handoffs
            .get(&request.command_id)
            .is_some_and(|current| current.ticket == saved.ticket && current.binding == binding)
        {
            return Err(HostError::Invalid("selection ticket was discarded"));
        }
        let selected = self
            .selection
            .take(saved.ticket, &binding, &request.authority);
        handoffs.remove(&request.command_id);
        match selected {
            Ok(selection) => Ok(Some(selection.definitions())),
            Err(
                super::selection::SelectionPreflightError::Unavailable
                | super::selection::SelectionPreflightError::Expired
                | super::selection::SelectionPreflightError::BindingChanged
                | super::selection::SelectionPreflightError::NotReady,
            ) => {
                tracing::warn!("optional selection ticket unavailable; local selection retained");
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn status(
        &self,
        desired: &HostSettings,
    ) -> Result<CompositionStatus, HostError> {
        let selection_configuration = self.selection.configuration_status()?;
        Ok(CompositionStatus {
            memory: self.memory.status(),
            mcp: self.mcp.status().await,
            selection: self.selection.status()?,
            active_selection: selection_configuration.settings.clone(),
            selection_configuration: selection_configuration.clone(),
            active_memory: self.effective.memory.clone(),
            active_mcp: self.effective.mcp.clone(),
            restart_required: RestartRequired {
                memory: serde_json::to_value(&desired.memory)?
                    != serde_json::to_value(&self.effective.memory)?,
                mcp: serde_json::to_value(&desired.mcp)?
                    != serde_json::to_value(&self.effective.mcp)?,
                selection: desired.selection != selection_configuration.settings,
            },
        })
    }

    pub(super) async fn stop_work(&self) -> Result<(), HostError> {
        self.close();
        let (selection, memory) = tokio::join!(self.selection.shutdown(), self.memory.shutdown());
        selection?;
        memory?;
        let _admission = Arc::clone(&self.admission).write_owned().await;
        self.memory.shutdown().await?;
        Ok(())
    }

    pub(super) async fn finish_mcp(&self) -> Result<(), HostError> {
        self.mcp.shutdown().await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TaskPreparation for Composition {
    async fn prepare(
        &self,
        request: TaskPreparationRequest,
        cancellation: CancellationToken,
    ) -> Result<PreparedTaskInputs, SchedulerError> {
        let _admission = self.enter().await.map_err(|_| SchedulerError::Stopped)?;
        let query = memory_query(&request.instruction);
        let memory = if query.is_empty() {
            Vec::new()
        } else {
            self.memory
                .retrieve(query, cancellation)
                .await
                .map_err(|error| SchedulerError::Provider(error.to_string()))?
                .excerpts
        };
        let selected_tools = self
            .take_selection(&request)
            .await
            .map_err(|error| SchedulerError::Provider(error.to_string()))?
            .unwrap_or(request.selected_tools);
        Ok(PreparedTaskInputs {
            memory,
            selected_tools,
        })
    }
}

impl HostInner {
    async fn selection_request(
        self: &Arc<Self>,
        original: Value,
    ) -> Result<(SelectionBinding, ExecutionAuthority, String, usize), HostError> {
        let command: CommandEnvelope = serde_json::from_value(original.clone())?;
        if command.issuing_device_id.is_some() {
            return Err(HostError::Invalid(
                "selection preparation requires a local owner command",
            ));
        }
        let instruction = match &command.payload {
            Command::SendMessage { message } => match message.content.as_slice() {
                [MessageContent::Text { text }] => text.clone(),
                _ => return Err(HostError::Invalid("selection requires one user text block")),
            },
            Command::CreateTask {
                instruction,
                parent_task_id: None,
            } => instruction.clone(),
            _ => {
                return Err(HostError::Invalid(
                    "selection requires a fresh root task or message",
                ));
            }
        };
        let id = command.command_id;
        if self
            .blocking(move |host| Ok(host.inbox.record(id)?.is_some()))
            .await?
        {
            return Err(HostError::Invalid(
                "selection must be prepared before command admission",
            ));
        }
        let conversation = self.agent.conversation(command.conversation_id).await?;
        if conversation.info.pending_command_id.is_some() {
            return Err(HostError::Invalid(
                "selection requires an idle conversation",
            ));
        }
        let input = InboxInput::Domain(command.clone());
        input.validate_original(&original)?;
        let EffectiveCommand::Turn { execution } = self.capture_command(&input).await? else {
            return Err(HostError::Invalid(
                "selection command has no execution policy",
            ));
        };
        if !execution.tools_enabled || execution.limits.max_selected_tools < 2 {
            return Err(HostError::Invalid(
                "optional selection requires tools and capacity for a candidate plus local discovery",
            ));
        }
        let task = TaskId::try_from(*id.as_uuid())?;
        let authority = execution.authority(command.conversation_id, task)?;
        let binding = self.composition.selection_binding(
            &original,
            &execution,
            command.conversation_id,
            task,
            id,
        )?;
        Ok((
            binding,
            authority,
            memory_query(&instruction),
            (execution.limits.max_selected_tools - 1).min(16),
        ))
    }

    pub(super) async fn prepare_selection(
        self: &Arc<Self>,
        command: Value,
        cancellation: CancellationToken,
    ) -> Result<PreparedSelectionTicket, HostError> {
        let _admission = self.composition.enter().await?;
        let (binding, authority, query, limit) = {
            let _mutation = self.mutation.lock().await;
            self.ensure_accepting()?;
            self.selection_request(command.clone()).await?
        };
        {
            let mut handoffs = self
                .composition
                .handoffs
                .lock()
                .map_err(|_| HostError::Worker)?;
            handoffs.retain(|_, value| value.expires_at > Instant::now());
            if handoffs.len() >= 16 || handoffs.contains_key(&binding.command_id) {
                return Err(HostError::Invalid(
                    "discard the previous selection ticket before preparing another",
                ));
            }
        }
        let prepared = self
            .composition
            .selection
            .prepare(binding.clone(), authority, query, limit, cancellation)
            .await?;
        let _mutation = self.mutation.lock().await;
        let current = match self.selection_request(command).await {
            Ok((current, _, _, _)) if current == binding => current,
            _ => {
                self.composition.selection.discard(prepared.ticket)?;
                return Err(HostError::Invalid(
                    "selection inputs changed during preparation",
                ));
            }
        };
        self.ensure_accepting()?;
        let mut handoffs = self
            .composition
            .handoffs
            .lock()
            .map_err(|_| HostError::Worker)?;
        if handoffs.len() >= 16 || handoffs.contains_key(&binding.command_id) {
            self.composition.selection.discard(prepared.ticket)?;
            return Err(HostError::Invalid(
                "another selection was prepared concurrently",
            ));
        }
        let expires_at = Instant::now()
            .checked_add(Duration::from_millis(prepared.expires_in_ms))
            .ok_or(HostError::Invalid("selection lifetime overflow"))?;
        handoffs.insert(
            binding.command_id,
            SelectionHandoff {
                ticket: prepared.ticket,
                binding: current,
                ready: matches!(&prepared.preparation, SelectionPreparation::Local { .. }),
                expires_at,
            },
        );
        Ok(prepared)
    }

    pub(super) async fn approve_selection(
        self: &Arc<Self>,
        ticket: Uuid,
        command: Value,
        cancellation: CancellationToken,
    ) -> Result<SelectionSummary, HostError> {
        let _admission = self.composition.enter().await?;
        let (binding, authority, _, _) = {
            let _mutation = self.mutation.lock().await;
            self.ensure_accepting()?;
            self.selection_request(command).await?
        };
        {
            let handoffs = self
                .composition
                .handoffs
                .lock()
                .map_err(|_| HostError::Worker)?;
            if !handoffs.get(&binding.command_id).is_some_and(|saved| {
                saved.ticket == ticket
                    && saved.binding == binding
                    && saved.expires_at > Instant::now()
            }) {
                return Err(HostError::Invalid("selection binding changed or expired"));
            }
        }
        let summary = self
            .composition
            .selection
            .approve(ticket, binding.clone(), authority, cancellation)
            .await?;
        let mut handoffs = self
            .composition
            .handoffs
            .lock()
            .map_err(|_| HostError::Worker)?;
        let saved = handoffs
            .get_mut(&binding.command_id)
            .filter(|saved| saved.ticket == ticket && saved.binding == binding)
            .ok_or(HostError::Invalid("selection ticket was discarded"))?;
        saved.ready = true;
        Ok(summary)
    }

    pub(super) fn discard_selection(&self, ticket: Uuid) -> Result<(), HostError> {
        let mut handoffs = self
            .composition
            .handoffs
            .lock()
            .map_err(|_| HostError::Worker)?;
        let command = handoffs
            .iter()
            .find_map(|(command, saved)| (saved.ticket == ticket).then_some(*command))
            .ok_or(HostError::Invalid("unknown selection ticket"))?;
        self.composition.selection.discard(ticket)?;
        handoffs.remove(&command);
        Ok(())
    }
}

fn digest(value: &impl Serialize) -> Result<String, HostError> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn memory_query(instruction: &str) -> String {
    let mut query = String::new();
    for word in instruction.split_whitespace().take(64) {
        if !query.is_empty() {
            if query.len() == 4096 {
                break;
            }
            query.push(' ');
        }
        for character in word.chars().filter(|value| !value.is_control()) {
            if query.len() + character.len_utf8() > 4096 {
                return query;
            }
            query.push(character);
        }
    }
    query
}
