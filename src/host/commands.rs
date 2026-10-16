use super::inbox::{
    AppliedReceipt, CommandOrigin, CommandPhase, HostCommandRecord, InboxError, InboxInput,
    PendingCursor,
};
use super::runtime::{HostError, HostInner, public_failure, timestamp};
use crate::agent::{AgentError, ConversationConfig, TextAcceptance, TextCommand};
use crate::context::ContextBudget;
use crate::ipc::{OwnerCommand, RequestedRunOptions};
use crate::permissions::{ApprovalChoice, ApprovalResolution};
use crate::protocol::{
    Command, CommandId, ConversationId, MessageContent, PermissionDecision, TaskId, UnixMillis,
};
use crate::scheduler::{ExecutionPolicy, SchedulerError, TaskSubmission};
use crate::storage::{StorageError, Store};
use crate::transport::local::LocalResponsePayload;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EffectiveCommand {
    Initialize {
        configuration: ConversationConfig,
        title: String,
    },
    Turn {
        execution: ExecutionPolicy,
    },
    Control,
}

impl HostInner {
    pub(super) async fn dispatch(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(), HostError> {
        let mut scheduler_changed = self.scheduler.subscribe();
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if cancellation.is_cancelled() {
                return Ok(());
            }
            self.recover_remote(&cancellation).await?;
            let mut cursor = None;
            let mut progress = false;
            loop {
                let page = self
                    .blocking(move |host| {
                        let mut limit = 64;
                        loop {
                            match host.inbox.pending_after(cursor, limit) {
                                Err(InboxError::Capacity) if limit > 1 => limit /= 2,
                                result => break Ok(result?),
                            }
                        }
                    })
                    .await?;
                if page.is_empty() {
                    break;
                }
                for record in &page {
                    if cancellation.is_cancelled() {
                        return Ok(());
                    }
                    progress |= self.apply(record.command_id).await?;
                }
                cursor = page.last().map(|record| PendingCursor {
                    accepted_at: record.accepted_at,
                    command_id: record.command_id,
                });
            }
            if progress {
                continue;
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = &mut notified => {},
                _ = self.devices.inbox_changed() => {},
                result = scheduler_changed.changed() => result.map_err(|_| HostError::Worker)?,
            }
        }
    }

    async fn recover_remote(
        self: &Arc<Self>,
        cancellation: &CancellationToken,
    ) -> Result<(), HostError> {
        let mut inbox_cursor = None;
        let mut scanned = 0;
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let page = {
                let _mutation = self.mutation.lock().await;
                self.blocking(move |host| {
                    Ok(host.devices.process_inbox_after(
                        UnixMillis::now()?,
                        inbox_cursor.as_ref(),
                        64,
                    )?)
                })
                .await?
            };
            scanned += page.scanned;
            inbox_cursor = page.next_cursor;
            if inbox_cursor.is_none() || scanned >= 1024 {
                break;
            }
        }
        let mut command_cursor = None;
        let mut scanned = 0;
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            let page = {
                let _mutation = self.mutation.lock().await;
                self.blocking(move |host| {
                    Ok(host
                        .devices
                        .pending_commands_after(command_cursor.as_ref(), 64)?)
                })
                .await?
            };
            scanned += page.scanned;
            command_cursor = page.next_cursor;
            for command in page.commands {
                if cancellation.is_cancelled() {
                    return Ok(());
                }

                let sender = command.sender;
                let pinned = self
                    .blocking(move |host| {
                        Ok(host
                            .agent
                            .store()
                            .device(&sender.to_string())?
                            .filter(|row| row.revoked_at.is_none())
                            .map(|row| row.public_identity))
                    })
                    .await?;
                let Some(pinned) = pinned else {
                    continue;
                };
                match self
                    .admit(
                        InboxInput::Domain(command.command.clone()),
                        Some(command.original.clone()),
                        CommandOrigin::PairedDevice {
                            device_id: sender,
                            pinned_identity: pinned,
                        },
                    )
                    .await
                {
                    Ok(_) => {
                        let id = command.command.command_id;
                        let record = self
                            .blocking(move |host| Ok(host.inbox.record(id)?))
                            .await?;
                        if let Some(record) = record.filter(|record| {
                            matches!(
                                record.phase,
                                CommandPhase::Completed | CommandPhase::Rejected
                            )
                        }) {
                            let result = record
                                .terminal_result
                                .ok_or(HostError::Invalid("pending remote result was archived"))?;
                            let rejected = record.phase == CommandPhase::Rejected;
                            let _mutation = self.mutation.lock().await;
                            self.blocking(move |host| {
                                if remote_pending(&host.agent.store(), command.command.command_id)?
                                {
                                    host.devices.complete_command(
                                        &command,
                                        result,
                                        rejected,
                                        UnixMillis::now()?,
                                    )?;
                                }
                                Ok(())
                            })
                            .await?;
                        }
                    }
                    Err(error) if retryable_application(&error) => {}
                    Err(HostError::Inbox(InboxError::OriginDenied)) => {}
                    Err(error) => {
                        let failure = public_failure(&error);
                        let _mutation = self.mutation.lock().await;
                        self.blocking(move |host| {
                            if remote_pending(&host.agent.store(), command.command.command_id)? {
                                host.devices.complete_command(
                                    &command,
                                    serde_json::to_value(failure)?,
                                    true,
                                    UnixMillis::now()?,
                                )?;
                            }
                            Ok(())
                        })
                        .await?;
                    }
                }
            }
            if command_cursor.is_none() || scanned >= 1024 {
                break;
            }
        }
        Ok(())
    }

    async fn apply(self: &Arc<Self>, command_id: CommandId) -> Result<bool, HostError> {
        let _mutation = self.mutation.lock().await;
        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(false);
        }
        let record = match self
            .blocking(move |host| Ok(host.inbox.validate_for_apply(command_id)?))
            .await
        {
            Ok(record) => record,
            Err(HostError::Inbox(InboxError::OriginDenied)) => {
                self.blocking(move |host| Ok(host.inbox.finish(command_id, true,
                    &json!({"code":"origin_revoked","message":"command origin is no longer authorized"}), UnixMillis::now()?)?)).await?;
                self.receipt_changed();
                return Ok(true);
            }
            Err(error) => return Err(error),
        };
        let result = if record.phase == CommandPhase::Applied {
            record
                .applied_receipt
                .clone()
                .ok_or(HostError::Invalid("applied command has no receipt"))
        } else {
            self.apply_domain(&record).await
        };
        match result {
            Ok(receipt) => {
                let result = json!({"applied":receipt});
                self.blocking(move |host| {
                    host.inbox
                        .mark_applied(command_id, &receipt, UnixMillis::now()?)?;
                    host.inbox
                        .finish(command_id, false, &result, UnixMillis::now()?)?;
                    Ok(())
                })
                .await?;
                self.receipt_changed();
                Ok(true)
            }
            Err(error) if retryable_application(&error) => Ok(false),
            Err(error) => {
                let failure = public_failure(&error);
                self.blocking(move |host| {
                    Ok(host.inbox.finish(
                        command_id,
                        true,
                        &serde_json::to_value(failure)?,
                        UnixMillis::now()?,
                    )?)
                })
                .await?;
                self.receipt_changed();
                Ok(true)
            }
        }
    }

    async fn apply_domain(
        self: &Arc<Self>,
        record: &HostCommandRecord,
    ) -> Result<AppliedReceipt, HostError> {
        let input = record.input()?.ok_or(HostError::Invalid(
            "pending command lost its original input",
        ))?;
        let effective: EffectiveCommand =
            serde_json::from_value(record.effective_options.clone().ok_or(HostError::Invalid(
                "pending command lost its execution snapshot",
            ))?)?;
        let mut receipt = AppliedReceipt {
            conversation_id: record.conversation_id,
            task_id: None,
            message_id: None,
            operation_id: None,
        };
        match (&input, effective) {
            (
                InboxInput::Owner(OwnerCommand::Initialize { .. })
                | InboxInput::Domain(crate::protocol::CommandEnvelope {
                    payload: Command::CreateConversation { .. },
                    ..
                }),
                EffectiveCommand::Initialize {
                    configuration,
                    title,
                },
            ) => {
                self.agent
                    .create_conversation(
                        record.conversation_id,
                        record.command_id,
                        title,
                        configuration,
                    )
                    .await?;
            }
            (_, EffectiveCommand::Turn { execution }) => {
                if self
                    .scheduler
                    .active_in_conversation(record.conversation_id)
                    .await?
                    .is_some_and(|task| task.active_command_id != record.command_id)
                {
                    return Err(AgentError::Busy.into());
                }
                let text = match &input {
                    InboxInput::Owner(OwnerCommand::Text { command, .. }) => command.text.clone(),
                    InboxInput::Domain(command) => match &command.payload {
                        Command::SendMessage { message } => match message.content.as_slice() {
                            [MessageContent::Text { text }] => text.clone(),
                            _ => {
                                return Err(HostError::Invalid(
                                    "only a single user text block is accepted",
                                ));
                            }
                        },
                        Command::CreateTask {
                            instruction,
                            parent_task_id: None,
                        } => instruction.clone(),
                        _ => {
                            return Err(HostError::Invalid(
                                "turn snapshot does not match its command",
                            ));
                        }
                    },
                    _ => {
                        return Err(HostError::Invalid(
                            "turn snapshot does not match its owner command",
                        ));
                    }
                };
                let accepted = self
                    .agent
                    .accept_text(TextCommand {
                        conversation_id: record.conversation_id,
                        command_id: record.command_id,
                        text,
                    })
                    .await?;
                match accepted {
                    TextAcceptance::Pending { message_id, .. } => {
                        let task_id = TaskId::try_from(*record.command_id.as_uuid())?;
                        self.scheduler
                            .enqueue(TaskSubmission {
                                task_id,
                                conversation_id: record.conversation_id,
                                command_id: record.command_id,
                                accepted_at: record.accepted_at,
                                execution,
                            })
                            .await?;
                        receipt.message_id = Some(message_id);
                        receipt.task_id = Some(task_id);
                    }
                    TextAcceptance::Completed { .. } => {
                        receipt.message_id = self
                            .agent
                            .conversation(record.conversation_id)
                            .await?
                            .messages
                            .into_iter()
                            .find(|message| {
                                message.causation_id == Some(record.command_id)
                                    && message.message.role == crate::protocol::MessageRole::User
                            })
                            .map(|message| message.message.id);
                    }
                }
            }
            (InboxInput::Domain(command), EffectiveCommand::Control) => match &command.payload {
                Command::SteerTask {
                    task_id,
                    instruction,
                } => {
                    let task = self.scheduler.status(*task_id).await?;
                    if task.task.conversation_id != record.conversation_id {
                        return Err(HostError::Invalid("task belongs to another conversation"));
                    }
                    self.scheduler
                        .steer(
                            *task_id,
                            record.command_id,
                            record.accepted_at,
                            instruction.clone(),
                        )
                        .await?;
                    receipt.task_id = Some(*task_id);
                }
                Command::CancelTask { task_id } => {
                    let task = self.scheduler.status(*task_id).await?;
                    if task.task.conversation_id != record.conversation_id {
                        return Err(HostError::Invalid("task belongs to another conversation"));
                    }
                    self.scheduler.cancel(*task_id).await?;
                    receipt.task_id = Some(*task_id);
                }
                Command::ResolvePermission {
                    operation_id,
                    decision,
                } => {
                    let conversation_id = record.conversation_id;
                    let operation_id = *operation_id;
                    let decision = *decision;
                    let task_id = self
                        .blocking(move |host| {
                            let now = timestamp(UnixMillis::now()?)?;
                            let approval = host
                                .authorizer
                                .approval_for_operation(
                                    &conversation_id.to_string(),
                                    &operation_id.to_string(),
                                    now,
                                )?
                                .map(|view| view.request)
                                .ok_or(HostError::Invalid("operation has no permission request"))?;
                            host.authorizer.resolve(
                                &approval.id,
                                &ApprovalResolution {
                                    choice: match decision {
                                        PermissionDecision::Allow => ApprovalChoice::Allow,
                                        PermissionDecision::Deny => ApprovalChoice::Deny,
                                    },
                                    expires_at: None,
                                },
                                now,
                            )?;
                            Ok(approval.scope.task_id.parse::<TaskId>()?)
                        })
                        .await?;
                    self.scheduler.wake(task_id).await?;
                    receipt.operation_id = Some(operation_id);
                    receipt.task_id = Some(task_id);
                }
                _ => {
                    return Err(HostError::Invalid(
                        "control snapshot does not match its command",
                    ));
                }
            },
            _ => {
                return Err(HostError::Invalid(
                    "effective snapshot does not match its owner command",
                ));
            }
        }
        Ok(receipt)
    }

    pub(super) async fn admit(
        self: &Arc<Self>,
        input: InboxInput,
        original: Option<Value>,
        origin: CommandOrigin,
    ) -> Result<LocalResponsePayload, HostError> {
        let _mutation = self.mutation.lock().await;
        self.ensure_accepting()?;
        let original = original.ok_or(HostError::Invalid("original command JSON is required"))?;
        let lookup_input = input.clone();
        let lookup_original = original.clone();
        let lookup_origin = origin.clone();
        if let Some(record) = self
            .blocking(move |host| {
                Ok(host
                    .inbox
                    .lookup(&lookup_input, &lookup_original, &lookup_origin)?)
            })
            .await?
        {
            return Ok(LocalResponsePayload::CommandAccepted {
                command_id: record.command_id,
                accepted_at: record.accepted_at,
                duplicate: true,
            });
        }
        let effective = self.capture_command(&input).await?;
        let saved = self
            .blocking(move |host| {
                let options = serde_json::to_value(effective)?;
                let now = UnixMillis::now()?;
                let reservation = host
                    .inbox
                    .reserve(&input, &original, &origin, &options, now);
                match reservation {
                    Err(InboxError::Capacity) => {
                        host.inbox.archive(now)?;
                        Ok(host
                            .inbox
                            .reserve(&input, &original, &origin, &options, now)?)
                    }
                    result => Ok(result?),
                }
            })
            .await?;
        self.changed.notify_one();
        self.receipt_changed();
        Ok(LocalResponsePayload::CommandAccepted {
            command_id: saved.record.command_id,
            accepted_at: saved.record.accepted_at,
            duplicate: !saved.created,
        })
    }

    async fn capture_command(
        self: &Arc<Self>,
        input: &InboxInput,
    ) -> Result<EffectiveCommand, HostError> {
        match input {
            InboxInput::Owner(OwnerCommand::Initialize {
                configuration,
                title,
                ..
            }) => Ok(EffectiveCommand::Initialize {
                configuration: configuration.clone(),
                title: title.clone(),
            }),
            InboxInput::Domain(command)
                if matches!(command.payload, Command::CreateConversation { .. }) =>
            {
                let settings = self.blocking(|host| Ok(host.settings.snapshot()?)).await?;
                let configuration = settings.default_conversation.ok_or(HostError::Invalid(
                    "configure an owner default model before remote conversation creation",
                ))?;
                let Command::CreateConversation { title } = &command.payload else {
                    return Err(HostError::Invalid("invalid conversation creation"));
                };
                Ok(EffectiveCommand::Initialize {
                    configuration,
                    title: title.clone().unwrap_or_else(|| "New conversation".into()),
                })
            }
            InboxInput::Owner(OwnerCommand::Text { command, options }) => {
                let id = TaskId::try_from(*command.command_id.as_uuid())?;
                match self.scheduler.status(id).await {
                    Err(SchedulerError::NotOwned(_)) => return Err(HostError::LegacyForeground),
                    Ok(_) | Err(SchedulerError::NotFound(_)) => {}
                    Err(error) => return Err(error.into()),
                }
                self.capture_turn(command.conversation_id, command.command_id, options)
                    .await
            }
            InboxInput::Domain(command) => match &command.payload {
                Command::SendMessage { .. }
                | Command::CreateTask {
                    parent_task_id: None,
                    ..
                } => {
                    self.capture_turn(
                        command.conversation_id,
                        command.command_id,
                        &RequestedRunOptions::default(),
                    )
                    .await
                }
                Command::CreateTask {
                    parent_task_id: Some(_),
                    ..
                } => Err(HostError::Invalid(
                    "child task creation uses the native scoped scheduler tools",
                )),
                Command::SteerTask { instruction, .. } => {
                    if instruction.trim().is_empty()
                        || instruction.len() > 32_768
                        || instruction.contains('\0')
                    {
                        return Err(HostError::Invalid(
                            "steering requires 1 to 32768 UTF-8 bytes without NUL",
                        ));
                    }
                    Ok(EffectiveCommand::Control)
                }
                Command::CancelTask { .. } | Command::ResolvePermission { .. } => {
                    Ok(EffectiveCommand::Control)
                }
                Command::CreateConversation { .. } => {
                    Err(HostError::Invalid("invalid conversation creation"))
                }
            },
        }
    }

    async fn capture_turn(
        self: &Arc<Self>,
        conversation_id: ConversationId,
        command_id: CommandId,
        options: &RequestedRunOptions,
    ) -> Result<EffectiveCommand, HostError> {
        let legacy_active = self.blocking(move |host| {
            host.agent.store().connection()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM tasks WHERE conversation_id=?1 AND state IN ('queued','running','waiting') AND coalesce(json_extract(input,'$.scheduler_version'),0)!=1)",
                [conversation_id.to_string()], |row| row.get::<_,bool>(0),
            ).map_err(StorageError::from).map_err(HostError::from)
        }).await?;
        if legacy_active {
            return Err(HostError::LegacyForeground);
        }
        let conversation = self.agent.conversation(conversation_id).await?;
        if let Some(run) = conversation.native_run().filter(|run| {
            run.command_id == command_id && conversation.info.pending_command_id == Some(command_id)
        }) {
            if !options.is_empty() || run.task_id != TaskId::try_from(*command_id.as_uuid())? {
                return Err(HostError::Invalid(
                    "an existing native turn requires its original setup and no option overrides",
                ));
            }
            return self
                .blocking(move |host| {
                    let authority = crate::permissions::ExecutionAuthority::new(
                        conversation_id.to_string(),
                        run.task_id.to_string(),
                        run.workspace_root,
                        run.mode,
                    )?
                    .restrict_tools(run.allowed_tools.into_iter().collect())?;
                    let setup = crate::agent::ToolRunSetup::resume(
                        host.broker.scoped(authority.clone()),
                        host.registry.clone(),
                        &conversation,
                        crate::agent::ProviderResume::OnlyUnstarted,
                    )?;
                    Ok(EffectiveCommand::Turn {
                        execution: ExecutionPolicy {
                            workspace_root: authority.workspace_root().into(),
                            mode: authority.mode(),
                            tools_enabled: setup.limits.max_tool_calls > 0,
                            allowed_tools: Some(
                                host.registry
                                    .descriptors()
                                    .filter(|tool| authority.permits_tool(&tool.name))
                                    .map(|tool| tool.name.clone())
                                    .collect(),
                            ),
                            budget: setup.budget,
                            limits: setup.limits,
                        },
                    })
                })
                .await;
        }
        let options = options.clone();
        let output = conversation.config.max_output_tokens;
        self.blocking(move |host| {
            let settings = host.settings.snapshot()?;
            let mut limits = settings.loop_limits;
            if let Some(value) = options.max_model_attempts {
                limits.max_model_attempts = value;
            }
            if let Some(value) = options.max_tool_calls {
                limits.max_tool_calls = value;
            }
            if let Some(value) = options.max_charged_tokens {
                limits.max_charged_tokens = value;
            }
            if let Some(value) = options.max_duration_seconds {
                limits.max_duration_ms = value
                    .checked_mul(1000)
                    .ok_or(HostError::Invalid("duration exceeds the supported range"))?;
            }
            let policy = ExecutionPolicy {
                workspace_root: options.workspace.unwrap_or(settings.workspace_root),
                mode: options.execution_mode.unwrap_or(settings.execution_mode),
                tools_enabled: options.tools.unwrap_or(settings.tools_enabled),
                allowed_tools: None,
                budget: ContextBudget {
                    max_context_tokens: options.context_tokens.unwrap_or(settings.context_tokens),
                    reserved_output_tokens: output,
                    safety_margin_tokens: 1024,
                    max_request_bytes: 32 * 1024 * 1024,
                    max_compaction_bytes: 16 * 1024,
                },
                limits,
            }
            .capture(&host.registry)?;
            Ok(EffectiveCommand::Turn { execution: policy })
        })
        .await
    }
}

fn remote_pending(store: &Store, command_id: CommandId) -> Result<bool, HostError> {
    store
        .connection()?
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_commands WHERE command_id=?1 AND state='pending')",
            [command_id.to_string()],
            |row| row.get(0),
        )
        .map_err(StorageError::from)
        .map_err(HostError::from)
}

fn retryable_application(error: &HostError) -> bool {
    matches!(
        error,
        HostError::Agent(AgentError::Busy | AgentError::PendingTurn(_))
            | HostError::Draining
            | HostError::Inbox(InboxError::Capacity)
            | HostError::Scheduler(
                SchedulerError::Capacity(_)
                    | SchedulerError::OwnerDecision(_)
                    | SchedulerError::Stopped
                    | SchedulerError::MutationUncertain(_)
            )
    )
}
