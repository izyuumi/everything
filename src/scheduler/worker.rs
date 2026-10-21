use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    Conversation, LoopStatus, MAX_CHAT_TEXT_BYTES, PauseReason, ProviderResume, ToolRunSetup,
    TurnEvent,
};
use crate::context::TaskContext;
use crate::model::ToolDefinition;
use crate::protocol::{CommandId, MessageContent, MessageRole, TaskId};
use crate::selector::{DiscoveryBinding, SEARCH_TOOLS_NAME};
use crate::tools::ToolDescriptor;

use super::repository;
use super::types::{
    LiveText, ResumeTarget, SchedulerError, SchedulerServices, StoredTask, TaskPreparation,
    TaskPreparationRequest, WaitingReason, store,
};

const MAX_OUTPUT_BYTES: usize = 8_192;
const MAX_DIAGNOSTIC_BYTES: usize = 2_048;
const MAX_QUERY_BYTES: usize = 512;
const PROVIDER_CONSTRUCTION_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) struct ExecutionOutcome {
    pub task_id: TaskId,
    pub stop: ExecutionStop,
}

pub(super) enum ExecutionStop {
    Completed {
        output: String,
    },
    Waiting {
        reason: WaitingReason,
        diagnostic: Option<String>,
    },
    Cancelled,
    Failed {
        diagnostic: String,
    },
}

pub(super) async fn execute(
    services: Arc<SchedulerServices>,
    task: StoredTask,
    cancellation: CancellationToken,
    resume: Option<ResumeTarget>,
    changed: tokio::sync::watch::Sender<u64>,
    live: tokio::sync::watch::Sender<HashMap<TaskId, LiveText>>,
    preparation: Option<Arc<dyn TaskPreparation>>,
) -> ExecutionOutcome {
    let task_id = task.definition.initial.id;
    let stop = run(
        &services,
        &task,
        &cancellation,
        resume,
        &changed,
        &live,
        preparation,
    )
    .await;
    live.send_modify(|texts| {
        texts.remove(&task_id);
    });
    changed.send_modify(|revision| *revision = revision.wrapping_add(1));
    ExecutionOutcome { task_id, stop }
}

async fn run(
    services: &Arc<SchedulerServices>,
    task: &StoredTask,
    cancellation: &CancellationToken,
    resume: Option<ResumeTarget>,
    changed: &tokio::sync::watch::Sender<u64>,
    live: &tokio::sync::watch::Sender<HashMap<TaskId, LiveText>>,
    preparation_service: Option<Arc<dyn TaskPreparation>>,
) -> ExecutionStop {
    let conversation_id = task.definition.initial.conversation_id;
    let command_id = task.checkpoint.active_command;
    let conversation = match services.agent.conversation(conversation_id).await {
        Ok(conversation) => Arc::new(conversation),
        Err(error) => return waiting(WaitingReason::Interrupted, error.to_string()),
    };
    if conversation.config != task.definition.configuration {
        return failed("conversation configuration differs from the saved task configuration");
    }
    if is_completed(&conversation, task) {
        return completed(&conversation, command_id);
    }
    if conversation.info.pending_command_id != Some(command_id) {
        return failed("conversation does not contain the task's pending command");
    }
    if cancellation.is_cancelled() {
        return saved_stop(&conversation, task, true, None);
    }
    if resume
        .as_ref()
        .is_some_and(|target| !target.matches(&conversation, task))
    {
        return waiting(
            WaitingReason::Interrupted,
            "owner resume no longer matches the exact native boundary",
        );
    }
    let retry_uncertain = resume.is_some();

    let preparation = {
        let services = Arc::clone(services);
        let task = task.clone();
        let conversation = Arc::clone(&conversation);
        tokio::task::spawn_blocking(move || {
            prepare(&services, &task, &conversation, retry_uncertain)
        })
        .await
    };
    let (mut setup, _discovery) = match preparation {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            return saved_stop(&conversation, task, false, Some(error.to_string()));
        }
        Err(error) => {
            return waiting(WaitingReason::Interrupted, error.to_string());
        }
    };

    let fresh = conversation
        .native_run()
        .is_none_or(|run| run.command_id != command_id);
    if let Some(preparation) = preparation_service.filter(|_| fresh) {
        let request = TaskPreparationRequest {
            conversation_id,
            task_id: task.definition.initial.id,
            command_id,
            instruction: setup.task.objective.clone(),
            execution: task.definition.execution.clone(),
            authority: setup.broker.authority().clone(),
            selected_tools: setup.selected_tools.clone(),
        };
        let prepared = match preparation
            .prepare(request, cancellation.child_token())
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => return waiting(WaitingReason::Interrupted, error.to_string()),
        };
        setup.memory = prepared.memory;
        setup.selected_tools = prepared.selected_tools;
    }
    let captured = services
        .agent
        .capture_tools_setup(
            conversation_id,
            command_id,
            setup.clone(),
            cancellation.child_token(),
        )
        .await;
    match captured {
        Ok(run)
            if matches!(
                run.status,
                LoopStatus::Paused {
                    reason: PauseReason::DurationLimit
                }
            ) =>
        {
            return waiting(
                WaitingReason::Limit,
                "native execution deadline elapsed before provider construction",
            );
        }
        Ok(_) => {}
        Err(error) => return waiting(WaitingReason::Interrupted, error.to_string()),
    }

    let factory_cancellation = cancellation.child_token();
    let _cancel_factory_on_drop = factory_cancellation.clone().drop_guard();
    let provider = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            return saved_stop(&conversation, task, true, None);
        }
        result = tokio::time::timeout(
            PROVIDER_CONSTRUCTION_TIMEOUT,
            services.providers.create(&task.definition.configuration, factory_cancellation.clone()),
        ) => match result {
            Ok(Ok(provider)) => provider,
            Ok(Err(error)) => return waiting(WaitingReason::ProviderUnavailable, error.to_string()),
            Err(_) => return waiting(WaitingReason::ProviderUnavailable, "provider construction exceeded 30 seconds"),
        }
    };

    drop(conversation);
    if let Some(target) = &resume {
        match services.agent.conversation(conversation_id).await {
            Ok(current) if target.matches(&current, task) => {}
            Ok(_) => {
                return waiting(
                    WaitingReason::Interrupted,
                    "native boundary changed during provider construction; owner review required",
                );
            }
            Err(error) => return waiting(WaitingReason::Interrupted, error.to_string()),
        }
    }
    let execution_cancellation = cancellation.child_token();
    let stream = services
        .agent
        .run_tools(
            conversation_id,
            command_id,
            provider,
            setup,
            execution_cancellation.clone(),
        )
        .await;
    let mut diagnostic = None;
    let mut final_signal = false;
    match stream {
        Ok(mut stream) => {
            while let Some(event) = stream.next().await {
                match &event {
                    Ok(TurnEvent::ModelAttemptStarted { attempt_id }) => {
                        live.send_modify(|texts| {
                            texts.insert(
                                task.definition.initial.id,
                                LiveText {
                                    attempt_id: *attempt_id,
                                    text: String::new(),
                                },
                            );
                        });
                    }
                    Ok(TurnEvent::ProvisionalText { delta, .. }) => {
                        live.send_modify(|texts| {
                            if let Some(live) = texts.get_mut(&task.definition.initial.id)
                                && live.text.len() + delta.len() <= MAX_CHAT_TEXT_BYTES
                            {
                                live.text.push_str(delta);
                            }
                        });
                    }
                    _ => {}
                }
                changed.send_modify(|revision| *revision = revision.wrapping_add(1));
                match event {
                    Ok(TurnEvent::RunCompleted { run }) => {
                        if run.command_id == command_id
                            && run.task_id == task.definition.initial.id
                            && run.status == LoopStatus::Completed
                        {
                            final_signal = true;
                        } else if diagnostic.is_none() {
                            diagnostic = Some(
                                "native completion identity did not match the task".to_owned(),
                            );
                        }
                    }
                    Ok(TurnEvent::ToolBlocked {
                        diagnostic: message,
                        ..
                    }) => {
                        if diagnostic.is_none() {
                            diagnostic = Some(bounded(&message, MAX_DIAGNOSTIC_BYTES));
                        }
                    }
                    Err(error) => {
                        if diagnostic.is_none() {
                            diagnostic = Some(bounded(&error.to_string(), MAX_DIAGNOSTIC_BYTES));
                        }
                        execution_cancellation.cancel();
                    }
                    _ => {}
                }
            }
        }
        Err(error) => diagnostic = Some(bounded(&error.to_string(), MAX_DIAGNOSTIC_BYTES)),
    }
    match services.agent.conversation(conversation_id).await {
        Ok(saved) => {
            if saved.config != task.definition.configuration {
                return waiting(
                    WaitingReason::Reconciliation,
                    "conversation configuration changed during task execution",
                );
            }
            if final_signal && !is_completed(&saved, task) {
                return waiting(
                    WaitingReason::Reconciliation,
                    "native completion signal does not match the durable conversation",
                );
            }
            saved_stop(&saved, task, cancellation.is_cancelled(), diagnostic)
        }
        Err(error) => waiting(WaitingReason::Reconciliation, error.to_string()),
    }
}

fn prepare(
    services: &SchedulerServices,
    task: &StoredTask,
    conversation: &Conversation,
    retry_uncertain: bool,
) -> Result<(ToolRunSetup, Option<DiscoveryBinding>), SchedulerError> {
    if repository::cancelled_ancestor(&store(services), task.definition.initial.id)? {
        return Err(SchedulerError::Invalid("task or an ancestor was cancelled"));
    }
    let authority = repository::authority(&store(services), task.definition.initial.id)?;
    let policy = &task.definition.execution;
    let discovery = if policy.tools_enabled && authority.permits_tool(SEARCH_TOOLS_NAME) {
        services
            .discovery
            .as_ref()
            .map(|discovery| discovery.search.bind_task(authority.clone()))
            .transpose()?
    } else {
        None
    };
    let resume = if retry_uncertain {
        ProviderResume::RetryUncertain
    } else {
        ProviderResume::OnlyUnstarted
    };
    let scoped = services.broker.scoped(authority.clone());
    let setup = if let Some(run) = conversation
        .native_run()
        .filter(|run| run.command_id == task.checkpoint.active_command)
    {
        if run.task_id != task.definition.initial.id {
            return Err(SchedulerError::Conflict(task.definition.initial.id));
        }
        let setup =
            ToolRunSetup::resume(scoped, Arc::clone(&services.registry), conversation, resume)?;
        if discovery.is_none()
            && setup
                .selected_tools
                .iter()
                .any(|tool| tool.name == SEARCH_TOOLS_NAME)
        {
            return Err(SchedulerError::Invalid(
                "saved task discovery service is unavailable",
            ));
        }
        setup
    } else {
        let instruction = accepted_instruction(conversation, task.checkpoint.active_command)?;
        let selected_tools = if !policy.tools_enabled {
            Vec::new()
        } else if let Some(discovery) = services.discovery.as_ref().filter(|_| discovery.is_some())
        {
            if policy.limits.max_selected_tools == 1 {
                vec![definition(services.registry.descriptor(SEARCH_TOOLS_NAME)?)]
            } else {
                discovery
                    .selector
                    .select_local(
                        &authority,
                        &bounded(&instruction, MAX_QUERY_BYTES),
                        31.min(policy.limits.max_selected_tools - 1),
                    )?
                    .definitions()
            }
        } else {
            services
                .registry
                .descriptors()
                .filter(|tool| authority.permits_tool(&tool.name) && tool.name != SEARCH_TOOLS_NAME)
                .take(policy.limits.max_selected_tools)
                .map(definition)
                .collect()
        };
        let mut limits = policy.limits.clone();
        if !policy.tools_enabled {
            limits.max_tool_calls = 0;
        }
        ToolRunSetup {
            broker: scoped,
            registry: Arc::clone(&services.registry),
            task: TaskContext {
                objective: instruction,
                constraints: Vec::new(),
                state: String::new(),
            },
            budget: policy.budget.clone(),
            memory: Vec::new(),
            selected_tools,
            limits,
            provider_resume: resume,
        }
    };
    Ok((setup, discovery))
}

fn accepted_instruction(
    conversation: &Conversation,
    command_id: CommandId,
) -> Result<String, SchedulerError> {
    let message = conversation
        .messages
        .iter()
        .find(|entry| {
            entry.causation_id == Some(command_id) && entry.message.role == MessageRole::User
        })
        .ok_or(SchedulerError::Invalid(
            "accepted task instruction is missing",
        ))?;
    let mut instruction = String::new();
    for content in &message.message.content {
        if let MessageContent::Text { text } = content {
            instruction.push_str(text);
        }
    }
    if instruction.is_empty() {
        return Err(SchedulerError::Invalid(
            "accepted task instruction is empty",
        ));
    }
    Ok(instruction)
}

fn definition(descriptor: &ToolDescriptor) -> ToolDefinition {
    ToolDefinition {
        name: descriptor.name.clone(),
        description: descriptor.description.clone(),
        input_schema: descriptor.input_schema.clone(),
    }
}

pub(super) fn is_completed(conversation: &Conversation, task: &StoredTask) -> bool {
    conversation.info.pending_command_id.is_none()
        && conversation.native_run().is_some_and(|run| {
            run.command_id == task.checkpoint.active_command
                && run.task_id == task.definition.initial.id
                && run.status == LoopStatus::Completed
        })
        && conversation
            .messages
            .iter()
            .rev()
            .find(|entry| {
                entry.causation_id == Some(task.checkpoint.active_command)
                    && entry.message.role == MessageRole::Assistant
            })
            .is_some_and(|entry| {
                !entry
                    .message
                    .content
                    .iter()
                    .any(|content| matches!(content, MessageContent::ToolCall(_)))
            })
}

pub(super) fn completed(conversation: &Conversation, command_id: CommandId) -> ExecutionStop {
    let mut output = String::new();
    if let Some(message) = conversation.messages.iter().rfind(|entry| {
        entry.causation_id == Some(command_id) && entry.message.role == MessageRole::Assistant
    }) {
        for content in &message.message.content {
            if let MessageContent::Text { text } = content {
                output.push_str(&bounded(
                    text,
                    MAX_OUTPUT_BYTES.saturating_sub(output.len()),
                ));
            }
        }
    }
    ExecutionStop::Completed { output }
}

fn saved_stop(
    conversation: &Conversation,
    task: &StoredTask,
    cancelled: bool,
    diagnostic: Option<String>,
) -> ExecutionStop {
    if is_completed(conversation, task) {
        return completed(conversation, task.checkpoint.active_command);
    }
    let matching = conversation.native_run().filter(|run| {
        run.command_id == task.checkpoint.active_command
            && run.task_id == task.definition.initial.id
    });
    let reason = match matching.map(|run| run.status) {
        Some(LoopStatus::ProviderInFlight { attempt_id }) => {
            WaitingReason::ProviderUncertain { attempt_id }
        }
        Some(LoopStatus::AwaitingPermission) => WaitingReason::Approval,
        Some(LoopStatus::Paused { reason }) => match reason {
            PauseReason::OperationInFlight { .. } => WaitingReason::InFlight,
            PauseReason::ReconciliationRequired { .. } | PauseReason::BrokerRejected { .. } => {
                WaitingReason::Reconciliation
            }
            PauseReason::WaitingForEvents { wait_id } => WaitingReason::Children { wait_id },
            PauseReason::ChildOutcomeUnknown { child_task_id } => {
                WaitingReason::ChildOutcomeUnknown { child_task_id }
            }
            PauseReason::InterruptedTool => WaitingReason::Interrupted,
            PauseReason::Cancellation if cancelled => return ExecutionStop::Cancelled,
            PauseReason::Cancellation => WaitingReason::Interrupted,
            PauseReason::ModelAttemptLimit
            | PauseReason::ToolCallLimit
            | PauseReason::TokenLimit
            | PauseReason::DurationLimit => WaitingReason::Limit,
            reason => WaitingReason::NativePaused {
                status: LoopStatus::Paused { reason },
            },
        },
        Some(LoopStatus::ExecutingTools) => WaitingReason::Reconciliation,
        Some(LoopStatus::Ready) | None if cancelled => return ExecutionStop::Cancelled,
        Some(status) => WaitingReason::NativePaused { status },
        None => WaitingReason::Interrupted,
    };
    ExecutionStop::Waiting {
        reason,
        diagnostic: diagnostic.map(|message| bounded(&message, MAX_DIAGNOSTIC_BYTES)),
    }
}

fn waiting(reason: WaitingReason, diagnostic: impl AsRef<str>) -> ExecutionStop {
    ExecutionStop::Waiting {
        reason,
        diagnostic: Some(bounded(diagnostic.as_ref(), MAX_DIAGNOSTIC_BYTES)),
    }
}

fn failed(diagnostic: &str) -> ExecutionStop {
    ExecutionStop::Failed {
        diagnostic: bounded(diagnostic, MAX_DIAGNOSTIC_BYTES),
    }
}

fn bounded(value: &str, maximum: usize) -> String {
    let mut end = value.len().min(maximum);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}
