use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::agent::{Conversation, LoopStatus, PauseReason, TextCommand};
use crate::permissions::ExecutionMode;
use crate::protocol::{
    CommandId, ConversationId, MessageContent, MessageRole, TaskId, TaskRecord, TaskStatus,
    UnixMillis,
};
use crate::storage::{Store, ToolCallState};

use super::repository;
use super::types::*;
use super::worker::{self, ExecutionOutcome, ExecutionStop};
use super::workspace;

#[derive(Clone)]
pub struct Scheduler {
    pub(super) inner: Arc<HandleInner>,
}

pub(super) struct HandleInner {
    sender: mpsc::Sender<Control>,
    shutdown_grace: std::time::Duration,
    changed: watch::Sender<u64>,
    drained: watch::Receiver<Option<ShutdownReport>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShutdownReport {
    pub active_tasks: usize,
    pub broker_quarantined: bool,
}

type Reply = oneshot::Sender<Result<TaskSnapshot, SchedulerError>>;

enum Control {
    Activate(oneshot::Sender<Result<(), SchedulerError>>),
    Enqueue(TaskSubmission, Reply),
    Status(TaskId, Reply),
    Active(
        ConversationId,
        oneshot::Sender<Result<Option<TaskSnapshot>, SchedulerError>>,
    ),
    Wake(TaskId, Reply),
    Cancel(TaskId, Reply),
    Resume {
        task: TaskId,
        target: ResumeTarget,
        request: CommandId,
        reply: Reply,
    },
    Spawn {
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        instruction: String,
        mode: Option<ExecutionMode>,
        reply: Reply,
    },
    Send {
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        target: TaskId,
        instruction: String,
        reply: Reply,
    },
    ScopedStatus {
        caller: TaskId,
        workspace: PathBuf,
        target: TaskId,
        reply: Reply,
    },
    ScopedCancel {
        caller: TaskId,
        workspace: PathBuf,
        target: TaskId,
        reply: Reply,
    },
    Shutdown(oneshot::Sender<ShutdownReport>),
}

impl Scheduler {
    pub async fn start(
        services: SchedulerServices,
        limits: SchedulerLimits,
    ) -> Result<Self, SchedulerError> {
        limits.validate()?;
        let (sender, receiver) = mpsc::channel(64);
        let (ready, started) = oneshot::channel();
        let (changed, _) = watch::channel(0);
        let actor_changed = changed.clone();
        let (drained_sender, drained) = watch::channel(None);
        let runtime = tokio::runtime::Handle::current();
        let shutdown_grace = limits.shutdown_grace;
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let mut actor =
                    Actor::new(services, limits, receiver, actor_changed, drained_sender);
                match actor.recover().await {
                    Ok(()) => {
                        if ready.send(Ok(())).is_ok() {
                            actor.run().await;
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            });
        });
        started.await.map_err(|_| SchedulerError::Worker)??;
        Ok(Self {
            inner: Arc::new(HandleInner {
                sender,
                shutdown_grace,
                changed,
                drained,
            }),
        })
    }

    async fn request(
        &self,
        control: impl FnOnce(Reply) -> Control,
    ) -> Result<TaskSnapshot, SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(control(reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| {
            SchedulerError::MutationUncertain(
                "control actor stopped after accepting the request".into(),
            )
        })?
    }

    pub async fn enqueue(
        &self,
        submission: TaskSubmission,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Enqueue(submission, reply))
            .await
    }

    pub async fn status(&self, task: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Status(task, reply)).await
    }

    pub async fn active_in_conversation(
        &self,
        conversation: ConversationId,
    ) -> Result<Option<TaskSnapshot>, SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::Active(conversation, reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changed.subscribe()
    }

    pub async fn activate(&self) -> Result<(), SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::Activate(reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
    }

    pub async fn wake(&self, task: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Wake(task, reply)).await
    }

    pub async fn cancel(&self, task: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Cancel(task, reply)).await
    }

    pub async fn resume_provider_once(
        &self,
        task: TaskId,
        expected_attempt: CommandId,
        request_id: CommandId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Resume {
            task,
            target: ResumeTarget::ProviderAttempt {
                attempt_id: expected_attempt,
            },
            request: request_id,
            reply,
        })
        .await
    }

    pub async fn resume_interrupted_once(
        &self,
        task: TaskId,
        expected_command: CommandId,
        expected_revision: u64,
        expected_status: LoopStatus,
        request_id: CommandId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        if !matches!(
            expected_status,
            LoopStatus::Paused {
                reason: PauseReason::Cancellation | PauseReason::InterruptedTool
            }
        ) {
            return Err(SchedulerError::OwnerDecision(task));
        }
        self.request(|reply| Control::Resume {
            task,
            target: ResumeTarget::KnownInterruption {
                command_id: expected_command,
                checkpoint_revision: expected_revision,
                status: expected_status,
            },
            request: request_id,
            reply,
        })
        .await
    }

    pub async fn shutdown(&self) -> Result<ShutdownReport, SchedulerError> {
        if let Some(report) = self.inner.drained.borrow().clone() {
            return Ok(report);
        }
        let (reply, result) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + self.inner.shutdown_grace;
        let shutdown = async {
            self.inner
                .sender
                .send(Control::Shutdown(reply))
                .await
                .map_err(|_| SchedulerError::Stopped)?;
            result.await.map_err(|_| SchedulerError::Stopped)
        };
        match tokio::time::timeout_at(deadline, shutdown).await {
            Ok(Ok(report)) => Ok(report),
            Ok(Err(_)) => self
                .inner
                .drained
                .borrow()
                .clone()
                .ok_or(SchedulerError::Stopped),
            Err(_) => self
                .inner
                .drained
                .borrow()
                .clone()
                .ok_or(SchedulerError::ShutdownPending),
        }
    }

    pub(super) async fn tool_spawn(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        instruction: String,
        mode: Option<ExecutionMode>,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Spawn {
            caller,
            workspace,
            operation,
            instruction,
            mode,
            reply,
        })
        .await
    }

    pub(super) async fn tool_send(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        target: TaskId,
        instruction: String,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Send {
            caller,
            workspace,
            operation,
            target,
            instruction,
            reply,
        })
        .await
    }

    pub(super) async fn tool_status(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        target: TaskId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::ScopedStatus {
            caller,
            workspace,
            target,
            reply,
        })
        .await
    }

    pub(super) async fn tool_cancel(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        target: TaskId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::ScopedCancel {
            caller,
            workspace,
            target,
            reply,
        })
        .await
    }
}

struct Active {
    cancellation: CancellationToken,
    child: bool,
    join: Id,
}

struct Actor {
    services: Arc<SchedulerServices>,
    limits: SchedulerLimits,
    receiver: mpsc::Receiver<Control>,
    queue: VecDeque<TaskId>,
    active: HashMap<TaskId, Active>,
    workers: JoinSet<ExecutionOutcome>,
    retry_once: HashMap<TaskId, ResumeTarget>,
    stopping: bool,
    activated: bool,
    shutdown: Vec<oneshot::Sender<ShutdownReport>>,
    controls_closed: bool,
    changed: watch::Sender<u64>,
    drained: watch::Sender<Option<ShutdownReport>>,
}

impl Actor {
    fn new(
        services: SchedulerServices,
        limits: SchedulerLimits,
        receiver: mpsc::Receiver<Control>,
        changed: watch::Sender<u64>,
        drained: watch::Sender<Option<ShutdownReport>>,
    ) -> Self {
        Self {
            services: Arc::new(services),
            limits,
            receiver,
            queue: VecDeque::new(),
            active: HashMap::new(),
            workers: JoinSet::new(),
            retry_once: HashMap::new(),
            stopping: false,
            activated: false,
            shutdown: Vec::new(),
            controls_closed: false,
            changed,
            drained,
        }
    }

    fn store(&self) -> Store {
        store(&self.services)
    }

    fn queue(&mut self, id: TaskId) {
        if !self.queue.contains(&id) && !self.active.contains_key(&id) {
            self.queue.push_back(id);
        }
    }

    async fn recover(&mut self) -> Result<(), SchedulerError> {
        let tasks = repository::unfinished(&self.store(), self.limits.max_pending_tasks)?;
        for mut task in tasks {
            let id = task.definition.initial.id;
            match repository::encode_checkpoint(&task.checkpoint) {
                Err(SchedulerError::Capacity("task checkpoint bytes")) => {
                    tracing::warn!(task_id=%id, "legacy scheduler checkpoint lacks reserved lifecycle capacity; owner inspection is required before admission");
                    continue;
                }
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            let captured = self
                .services
                .agent
                .conversation(task.definition.initial.conversation_id)
                .await?;
            if captured.config != task.definition.configuration {
                return Err(SchedulerError::Conflict(id));
            }
            repair_handoff(&self.store(), &mut task, &captured)?;
            if worker::is_completed(&captured, &task) {
                self.finish(ExecutionOutcome {
                    task_id: id,
                    stop: worker::completed(&captured, task.checkpoint.active_command),
                })
                .await?;
                continue;
            }
            if let Err(error) = repository::authority(&self.store(), id) {
                task.checkpoint.waiting = Some(WaitingReason::Interrupted);
                task.checkpoint.diagnostic = Some(diagnostic(&error.to_string()));
                save_waiting(&self.store(), &mut task)?;
                continue;
            }
            if repository::cancelled_ancestor(&self.store(), id)? {
                task.checkpoint.cancel_requested = true;
                repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
                self.queue(id);
                continue;
            }
            let conversation = self
                .repair_initial_acceptance(&task, Some(captured))
                .await?;
            if conversation.config != task.definition.configuration {
                return Err(SchedulerError::Conflict(id));
            }
            repair_handoff(&self.store(), &mut task, &conversation)?;
            match native_waiting(&conversation, &task) {
                Some(reason) => {
                    task.checkpoint.waiting = Some(reason);
                    save_waiting(&self.store(), &mut task)?;
                }
                None => {
                    task.checkpoint.waiting = None;
                    repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
                    self.queue(id);
                }
            }
        }
        Ok(())
    }

    async fn run(&mut self) {
        loop {
            if self.activated && !self.stopping {
                match self.dispatch().await {
                    Ok(true) => {
                        self.changed
                            .send_modify(|revision| *revision = revision.wrapping_add(1));
                        if !self.queue.is_empty() {
                            continue;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(error = %error, "scheduler dispatch stopped");
                        self.stop();
                    }
                }
            }
            if self.stopping && self.workers.is_empty() {
                let report = ShutdownReport {
                    active_tasks: 0,
                    broker_quarantined: self.services.broker.is_quarantined(),
                };
                self.drained.send_replace(Some(report.clone()));
                for reply in self.shutdown.drain(..) {
                    let _ = reply.send(report.clone());
                }
                return;
            }
            tokio::select! {
                control = self.receiver.recv(), if !self.controls_closed => {
                    match control {
                        Some(control) => { let notify=!matches!(control,Control::Status(..)|Control::ScopedStatus{..}|Control::Active(..)); self.control(control).await; if notify{self.changed.send_modify(|revision| *revision = revision.wrapping_add(1));} },
                        None => {self.controls_closed=true; self.stop();},
                    }
                }
                result = self.workers.join_next(), if !self.workers.is_empty() => {
                    match result {
                        Some(Ok(outcome)) => {
                            self.active.remove(&outcome.task_id);
                            if let Err(error) = self.finish(outcome).await {
                                tracing::error!(error = %error, "scheduler result persistence stopped");
                                self.stop();
                            }
                            self.changed.send_modify(|revision| *revision=revision.wrapping_add(1));
                        }
                        Some(Err(error)) => {
                            if let Some(id) = self.active.iter().find_map(|(id, active)| (active.join == error.id()).then_some(*id)) {
                                self.active.remove(&id);
                                if let Ok(mut task) = repository::load(&self.store(), id) {
                                    task.checkpoint.waiting = Some(WaitingReason::Reconciliation);
                                    task.checkpoint.diagnostic = Some("scheduler worker stopped unexpectedly; inspect native and broker journals".into());
                                    if let Err(save_error) = repository::save(&self.store(), &mut task, TaskStatus::WaitingForChildren) { tracing::error!(error = %save_error, "scheduler worker failure could not be saved"); }
                                }
                            }
                            self.stop();
                        }
                        None => {}
                    }
                }
            }
        }
    }

    fn stop(&mut self) {
        self.stopping = true;
        for active in self.active.values() {
            active.cancellation.cancel();
        }
    }

    async fn control(&mut self, control: Control) {
        if let Control::Shutdown(reply) = control {
            self.shutdown.push(reply);
            self.stop();
            return;
        }
        match control {
            Control::Active(conversation, reply) => {
                let _ = reply.send(repository::active_in_conversation(
                    &self.store(),
                    conversation,
                ));
            }
            Control::Activate(reply) => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.activated = true;
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Control::Status(id, reply) => {
                let _ = reply.send(repository::load(&self.store(), id).map(|task| task.snapshot()));
            }
            Control::ScopedStatus {
                caller,
                workspace,
                target,
                reply,
            } => {
                let result = self
                    .validate_caller(caller, &workspace)
                    .and_then(|_| repository::authorize_target(&self.store(), caller, target, true))
                    .and_then(|_| repository::load(&self.store(), target))
                    .map(|task| task.snapshot());
                let _ = reply.send(result);
            }
            Control::Enqueue(submission, reply) => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.enqueue(submission).await
                };
                let _ = reply.send(result);
            }
            Control::Wake(id, reply) => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.wake(id).await
                };
                let _ = reply.send(result);
            }
            Control::Cancel(id, reply) => {
                let result = self.cancel(id).await;
                let _ = reply.send(result);
            }
            Control::Resume {
                task,
                target,
                request,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.resume(task, target, request).await
                };
                let _ = reply.send(result);
            }
            Control::Spawn {
                caller,
                workspace,
                operation,
                instruction,
                mode,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.spawn(caller, workspace, operation, instruction, mode)
                        .await
                };
                let _ = reply.send(result);
            }
            Control::Send {
                caller,
                workspace,
                operation,
                target,
                instruction,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.send(caller, workspace, operation, target, instruction)
                        .await
                };
                let _ = reply.send(result);
            }
            Control::ScopedCancel {
                caller,
                workspace,
                target,
                reply,
            } => {
                let authorized = self.validate_caller(caller, &workspace).and_then(|_| {
                    repository::authorize_target(&self.store(), caller, target, false)
                });
                let result = match authorized {
                    Ok(_) => self.cancel(target).await,
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            }
            Control::Shutdown(_) => {}
        }
    }

    async fn enqueue(
        &mut self,
        submission: TaskSubmission,
    ) -> Result<TaskSnapshot, SchedulerError> {
        submission.execution.validate()?;
        let conversation = self
            .services
            .agent
            .conversation(submission.conversation_id)
            .await?;
        let instruction = instruction(&conversation, submission.command_id)?;
        let mut initial = TaskRecord::new(
            submission.conversation_id,
            None,
            submission.command_id,
            instruction,
            submission.accepted_at,
        );
        initial.id = submission.task_id;
        let task = initial_task(initial, conversation.config, submission.execution);
        match repository::load(&self.store(), submission.task_id) {
            Ok(existing) => {
                if repository::encode(&existing.definition)?
                    != repository::encode(&task.definition)?
                {
                    return Err(SchedulerError::Conflict(submission.task_id));
                }
                if existing.checkpoint.task.status == TaskStatus::Queued {
                    self.queue(submission.task_id);
                }
                return Ok(existing.snapshot());
            }
            Err(SchedulerError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        task.definition
            .execution
            .authority(submission.conversation_id, submission.task_id)?;
        let inserted = repository::insert(&self.store(), &task, self.limits.max_pending_tasks)?;
        let existing = repository::load(&self.store(), submission.task_id)?;
        if inserted || existing.checkpoint.task.status == TaskStatus::Queued {
            self.queue(submission.task_id);
        }
        Ok(existing.snapshot())
    }

    fn validate_caller(
        &self,
        caller: TaskId,
        workspace: &std::path::Path,
    ) -> Result<StoredTask, SchedulerError> {
        let task = repository::load(&self.store(), caller)?;
        if task.checkpoint.task.status != TaskStatus::Running
            || task.definition.execution.workspace_root != workspace
            || repository::cancelled_ancestor(&self.store(), caller)?
        {
            return Err(SchedulerError::OutsideScope);
        }
        repository::authority(&self.store(), caller)?;
        Ok(task)
    }

    fn operation_time(
        &self,
        caller: TaskId,
        operation: CommandId,
        tool: &str,
    ) -> Result<UnixMillis, SchedulerError> {
        let row =
            self.store()
                .tool_call(&operation.to_string())?
                .ok_or(SchedulerError::Invalid(
                    "task control must execute through the broker",
                ))?;
        if row.state != ToolCallState::Running
            || row.task_id.as_deref() != Some(caller.to_string().as_str())
            || row.tool_name != tool
        {
            return Err(SchedulerError::OutsideScope);
        }
        UnixMillis::try_from(
            u64::try_from(row.created_at)
                .map_err(|_| SchedulerError::Invalid("negative operation timestamp"))?,
        )
        .map_err(SchedulerError::from)
    }

    async fn spawn(
        &mut self,
        caller: TaskId,
        workspace_root: PathBuf,
        operation: CommandId,
        instruction: String,
        mode: Option<ExecutionMode>,
    ) -> Result<TaskSnapshot, SchedulerError> {
        validate_instruction(&instruction)?;
        let parent = self.validate_caller(caller, &workspace_root)?;
        let now = self.operation_time(caller, operation, "spawn_agent")?;
        let id = TaskId::try_from(derived_id(operation, b"task"))?;
        let conversation_id = ConversationId::try_from(derived_id(operation, b"conversation"))?;
        let create_id = CommandId::try_from(derived_id(operation, b"create"))?;
        let command_id = CommandId::try_from(derived_id(operation, b"instruction"))?;
        match repository::load(&self.store(), id) {
            Ok(existing) => {
                if existing.definition.initial.parent_task_id != Some(caller)
                    || existing.definition.initial.instruction != instruction
                    || existing.definition.initial.conversation_id != conversation_id
                    || existing.definition.execution.mode
                        != mode.unwrap_or(parent.definition.execution.mode)
                {
                    return Err(SchedulerError::Conflict(id));
                }
                return Ok(existing.snapshot());
            }
            Err(SchedulerError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        repository::check_capacity(&self.store(), self.limits.max_pending_tasks)?;
        repository::check_child_depth(&self.store(), caller)?;
        if parent.definition.initial.parent_task_id.is_some()
            && self.active.values().filter(|active| active.child).count()
                >= self.limits.max_subagents
        {
            return Err(SchedulerError::Capacity("nested subagent execution slots"));
        }
        let parent_authority = repository::authority(&self.store(), caller)?;
        parent_authority.child_in_conversation(
            conversation_id.to_string(),
            id.to_string(),
            mode.unwrap_or(parent.definition.execution.mode),
        )?;
        let root =
            workspace::prepare(&workspace_root, caller, id).map_err(|error| match error {
                SchedulerError::Capacity(_) => error,
                _ => mutation_uncertain(error),
            })?;
        let admitted = async {
            let mut policy = parent.definition.execution.clone();
            policy.workspace_root = root;
            policy.mode = mode.unwrap_or(policy.mode);
            let authority = parent_authority.child_in_workspace(
                conversation_id.to_string(),
                id.to_string(),
                policy.workspace_root.clone(),
                policy.mode,
            )?;
            if policy
                .allowed_tools
                .as_ref()
                .is_some_and(|names| names.iter().any(|name| !authority.permits_tool(name)))
            {
                return Err(SchedulerError::OutsideScope);
            }
            let title = format!("Subagent {id}");
            self.services
                .agent
                .create_conversation(
                    conversation_id,
                    create_id,
                    title,
                    parent.definition.configuration.clone(),
                )
                .await?;
            let mut initial = TaskRecord::new(
                conversation_id,
                Some(caller),
                command_id,
                instruction.clone(),
                now,
            );
            initial.id = id;
            let task = initial_task(initial, parent.definition.configuration, policy);
            repository::insert(&self.store(), &task, self.limits.max_pending_tasks)?;
            self.services
                .agent
                .accept_text(TextCommand {
                    conversation_id,
                    command_id,
                    text: instruction.clone(),
                })
                .await?;
            self.queue(id);
            Ok(repository::load(&self.store(), id)?.snapshot())
        }
        .await;
        admitted.map_err(mutation_uncertain)
    }

    async fn send(
        &mut self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        target: TaskId,
        instruction: String,
    ) -> Result<TaskSnapshot, SchedulerError> {
        validate_instruction(&instruction)?;
        self.validate_caller(caller, &workspace)?;
        repository::authorize_target(&self.store(), caller, target, false)?;
        let now = self.operation_time(caller, operation, "send_agent")?;
        let command_id = CommandId::try_from(derived_id(operation, b"send"))?;
        let mut task = repository::load(&self.store(), target)?;
        if let Some(message) = task
            .checkpoint
            .queued
            .iter()
            .find(|message| message.command_id == command_id)
        {
            if message.instruction != instruction {
                return Err(SchedulerError::Conflict(target));
            }
            return Ok(task.snapshot());
        }
        if repository::command_exists(&self.store(), command_id)? {
            self.services
                .agent
                .accept_text(TextCommand {
                    conversation_id: task.definition.initial.conversation_id,
                    command_id,
                    text: instruction,
                })
                .await?;
            return Ok(task.snapshot());
        }
        if task.checkpoint.task.status.is_terminal() {
            return Err(SchedulerError::Terminal(target));
        }
        if task.checkpoint.queued.len() >= 32 {
            return Err(SchedulerError::Capacity("queued task messages"));
        }
        task.checkpoint.queued.push(QueuedMessage {
            command_id,
            instruction,
            accepted_at: now,
        });
        repository::encode_checkpoint(&task.checkpoint)?;
        let status = task.checkpoint.task.status;
        repository::save(&self.store(), &mut task, status).map_err(mutation_uncertain)?;
        Ok(task.snapshot())
    }

    async fn wake(&mut self, id: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        if task.checkpoint.task.status.is_terminal() {
            return Ok(task.snapshot());
        }
        if self.active.contains_key(&id) {
            return Ok(task.snapshot());
        }
        let conversation = self.repair_initial_acceptance(&task, None).await?;
        if native_waiting(&conversation, &task).is_some_and(|reason| {
            matches!(
                reason,
                WaitingReason::ProviderUncertain { .. }
                    | WaitingReason::Limit
                    | WaitingReason::Reconciliation
                    | WaitingReason::Interrupted
                    | WaitingReason::NativePaused { .. }
            )
        }) {
            return Err(SchedulerError::OwnerDecision(id));
        }
        task.checkpoint.waiting = None;
        repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
        self.queue(id);
        Ok(task.snapshot())
    }

    async fn repair_initial_acceptance(
        &self,
        task: &StoredTask,
        captured: Option<Conversation>,
    ) -> Result<Conversation, SchedulerError> {
        let conversation = match captured {
            Some(conversation) => conversation,
            None => {
                self.services
                    .agent
                    .conversation(task.definition.initial.conversation_id)
                    .await?
            }
        };
        if conversation.config != task.definition.configuration {
            return Err(SchedulerError::Conflict(task.definition.initial.id));
        }
        if task.definition.initial.parent_task_id.is_some()
            && task.checkpoint.active_command == task.definition.initial.originating_command_id
            && conversation.info.pending_command_id.is_none()
            && conversation.native_run().is_none()
            && !conversation
                .messages
                .iter()
                .any(|message| message.causation_id == Some(task.checkpoint.active_command))
        {
            self.services
                .agent
                .accept_text(TextCommand {
                    conversation_id: task.definition.initial.conversation_id,
                    command_id: task.checkpoint.active_command,
                    text: task.definition.initial.instruction.clone(),
                })
                .await?;
            return Ok(self
                .services
                .agent
                .conversation(task.definition.initial.conversation_id)
                .await?);
        }
        Ok(conversation)
    }

    async fn resume(
        &mut self,
        id: TaskId,
        target: ResumeTarget,
        request: CommandId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        if let Some(audit) = task
            .checkpoint
            .resumes
            .iter()
            .find(|audit| audit.request_id == request)
        {
            if audit.target != target {
                return Err(SchedulerError::Conflict(id));
            }
            return Ok(task.snapshot());
        }
        if self.active.contains_key(&id)
            || task.checkpoint.task.status.is_terminal()
            || repository::cancelled_ancestor(&self.store(), id)?
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let conversation = self
            .services
            .agent
            .conversation(task.definition.initial.conversation_id)
            .await?;
        if !target.matches(&conversation, &task)
            || repository::has_running_operations(&self.store(), id)?
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        if task.checkpoint.resumes.len() >= 32 {
            return Err(SchedulerError::Capacity("provider retry audit records"));
        }
        task.checkpoint.resumes.push(ResumeAudit {
            request_id: request,
            target: target.clone(),
        });
        task.checkpoint.waiting = None;
        repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
        self.retry_once.insert(id, target);
        self.queue(id);
        Ok(task.snapshot())
    }

    async fn cancel(&mut self, id: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        if task.checkpoint.task.status.is_terminal() {
            return Ok(task.snapshot());
        }
        let admitted = async {
            if !task.checkpoint.task.status.is_terminal() {
                task.checkpoint.cancel_requested = true;
                let status = task.checkpoint.task.status;
                repository::save(&self.store(), &mut task, status)?;
            }
            let mut ids = vec![id];
            ids.extend(repository::descendants(&self.store(), id, 4096)?);
            for target in ids {
                let mut child = repository::load(&self.store(), target)?;
                if child.checkpoint.task.status.is_terminal() {
                    continue;
                }
                child.checkpoint.cancel_requested = true;
                let status = child.checkpoint.task.status;
                repository::save(&self.store(), &mut child, status)?;
                self.retry_once.remove(&target);
                if let Some(active) = self.active.get(&target) {
                    active.cancellation.cancel();
                } else {
                    self.queue(target);
                }
            }
            Ok(repository::load(&self.store(), id)?.snapshot())
        }
        .await;
        admitted.map_err(mutation_uncertain)
    }

    async fn dispatch(&mut self) -> Result<bool, SchedulerError> {
        let mut progress = false;
        let rounds = self.queue.len();
        for _ in 0..rounds {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            if self.active.contains_key(&id) {
                continue;
            }
            let mut task = repository::load(&self.store(), id)?;
            if task.checkpoint.task.status.is_terminal() {
                continue;
            }
            let conversation = self
                .services
                .agent
                .conversation(task.definition.initial.conversation_id)
                .await?;
            if conversation.config != task.definition.configuration {
                return Err(SchedulerError::Conflict(id));
            }
            repair_handoff(&self.store(), &mut task, &conversation)?;
            if worker::is_completed(&conversation, &task) {
                progress = true;
                self.finish(ExecutionOutcome {
                    task_id: id,
                    stop: worker::completed(&conversation, task.checkpoint.active_command),
                })
                .await?;
                continue;
            }
            if repository::cancelled_ancestor(&self.store(), id)? {
                progress = true;
                task.checkpoint.cancel_requested = true;
                let mut uncertainty = native_waiting(&conversation, &task).filter(|reason| {
                    matches!(
                        reason,
                        WaitingReason::ProviderUncertain { .. }
                            | WaitingReason::Reconciliation
                            | WaitingReason::InFlight
                    )
                });
                if repository::has_unknown_operations(&self.store(), id)? && uncertainty.is_none() {
                    uncertainty = Some(WaitingReason::Reconciliation);
                }
                if self.services.broker.is_quarantined()
                    || repository::has_running_operations(&self.store(), id)?
                {
                    task.checkpoint.waiting = Some(uncertainty.unwrap_or(WaitingReason::InFlight));
                    save_waiting(&self.store(), &mut task)?;
                } else if let Some(reason) = uncertainty {
                    task.checkpoint.waiting = Some(reason);
                    task.checkpoint.diagnostic=Some("Task execution cancelled; its previous native outcome remains uncertain and is not retryable".into());
                    repository::save(&self.store(), &mut task, TaskStatus::Failed)?;
                } else {
                    task.checkpoint.waiting = None;
                    repository::save(&self.store(), &mut task, TaskStatus::Cancelled)?;
                }
                continue;
            }
            if self.services.broker.is_quarantined() {
                self.queue.push_back(id);
                continue;
            }
            let child = task.definition.initial.parent_task_id.is_some();
            let running = self
                .active
                .values()
                .filter(|active| active.child == child)
                .count();
            let maximum = if child {
                self.limits.max_subagents
            } else {
                self.limits.max_primary_tasks
            };
            if running >= maximum {
                self.queue.push_back(id);
                continue;
            }
            if let Err(error) = repository::authority(&self.store(), id) {
                progress = true;
                task.checkpoint.waiting = Some(WaitingReason::Interrupted);
                task.checkpoint.diagnostic = Some(diagnostic(&error.to_string()));
                save_waiting(&self.store(), &mut task)?;
                continue;
            }
            let resume = self
                .retry_once
                .remove(&id)
                .filter(|target| target.matches(&conversation, &task));
            if let Some(reason) = native_waiting(&conversation, &task) {
                if resume.is_none()
                    && !matches!(reason, WaitingReason::Approval | WaitingReason::InFlight)
                {
                    progress = true;
                    task.checkpoint.waiting = Some(reason);
                    save_waiting(&self.store(), &mut task)?;
                    continue;
                }
            }
            progress = true;
            task.checkpoint.waiting = None;
            repository::save(&self.store(), &mut task, TaskStatus::Running)?;
            let cancellation = CancellationToken::new();
            let services = Arc::clone(&self.services);
            let worker_cancellation = cancellation.clone();
            let changed = self.changed.clone();
            let handle = self.workers.spawn(async move {
                worker::execute(services, task, worker_cancellation, resume, changed).await
            });
            self.active.insert(
                id,
                Active {
                    cancellation,
                    child,
                    join: handle.id(),
                },
            );
        }
        Ok(progress)
    }

    async fn finish(&mut self, outcome: ExecutionOutcome) -> Result<(), SchedulerError> {
        let mut task = repository::load(&self.store(), outcome.task_id)?;
        if repository::cancelled_ancestor(&self.store(), outcome.task_id)? {
            task.checkpoint.cancel_requested = true;
        }
        if task.checkpoint.task.status != TaskStatus::Running
            && !task.checkpoint.task.status.is_terminal()
        {
            repository::save(&self.store(), &mut task, TaskStatus::Running)?;
        }
        match outcome.stop {
            ExecutionStop::Completed { output } => {
                task.checkpoint.last_output = Some(output);
                task.checkpoint.diagnostic = None;
                task.checkpoint.waiting = None;
                if !task.checkpoint.queued.is_empty()
                    && !task.checkpoint.cancel_requested
                    && !self.stopping
                {
                    let next = task.checkpoint.queued[0].clone();
                    self.services
                        .agent
                        .accept_text(TextCommand {
                            conversation_id: task.definition.initial.conversation_id,
                            command_id: next.command_id,
                            text: next.instruction,
                        })
                        .await?;
                    task.checkpoint.queued.remove(0);
                    task.checkpoint.active_command = next.command_id;
                    repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
                    self.queue(outcome.task_id);
                } else if !task.checkpoint.queued.is_empty()
                    && self.stopping
                    && !task.checkpoint.cancel_requested
                {
                    task.checkpoint.waiting = Some(WaitingReason::Stopped);
                    repository::save(&self.store(), &mut task, TaskStatus::WaitingForChildren)?;
                } else if task.checkpoint.cancel_requested && !task.checkpoint.queued.is_empty() {
                    task.checkpoint.diagnostic=Some("Committed final output retained; queued follow-up commands were cancelled without execution".into());
                    repository::save(&self.store(), &mut task, TaskStatus::Cancelled)?;
                } else {
                    repository::save(&self.store(), &mut task, TaskStatus::Completed)?;
                }
            }
            ExecutionStop::Cancelled => {
                let unknown = repository::has_unknown_operations(&self.store(), outcome.task_id)?;
                if task.checkpoint.cancel_requested
                    || repository::cancelled_ancestor(&self.store(), outcome.task_id)?
                {
                    if self.services.broker.is_quarantined()
                        || repository::has_running_operations(&self.store(), outcome.task_id)?
                    {
                        task.checkpoint.waiting = Some(WaitingReason::Reconciliation);
                        save_waiting(&self.store(), &mut task)?;
                    } else if unknown {
                        task.checkpoint.waiting = Some(WaitingReason::Reconciliation);
                        task.checkpoint.diagnostic = Some("Task cancelled with an immutable unknown tool outcome; owner reconciliation remains required".into());
                        repository::save(&self.store(), &mut task, TaskStatus::Failed)?;
                    } else {
                        task.checkpoint.waiting = None;
                        repository::save(&self.store(), &mut task, TaskStatus::Cancelled)?;
                    }
                } else {
                    task.checkpoint.waiting = Some(if unknown {
                        WaitingReason::Reconciliation
                    } else {
                        WaitingReason::Stopped
                    });
                    repository::save(&self.store(), &mut task, TaskStatus::WaitingForChildren)?;
                }
            }
            ExecutionStop::Waiting { reason, diagnostic } => {
                if task.checkpoint.cancel_requested
                    || repository::cancelled_ancestor(&self.store(), outcome.task_id)?
                {
                    task.checkpoint.cancel_requested = true;
                    task.checkpoint.waiting = Some(reason.clone());
                    if !self.services.broker.is_quarantined()
                        && !repository::has_running_operations(&self.store(), outcome.task_id)?
                    {
                        let journal_unknown =
                            repository::has_unknown_operations(&self.store(), outcome.task_id)?;
                        let unknown = journal_unknown
                            || matches!(
                                reason,
                                WaitingReason::ProviderUncertain { .. }
                                    | WaitingReason::Reconciliation
                                    | WaitingReason::InFlight
                            );
                        if journal_unknown {
                            task.checkpoint.waiting = Some(WaitingReason::Reconciliation);
                        }
                        task.checkpoint.diagnostic=Some(if unknown{"Task execution cancelled; its previous native outcome remains uncertain and is not retryable"}else{"Task execution and queued follow-up commands cancelled"}.into());
                        repository::save(
                            &self.store(),
                            &mut task,
                            if unknown {
                                TaskStatus::Failed
                            } else {
                                TaskStatus::Cancelled
                            },
                        )?;
                        return Ok(());
                    }
                }
                let permission = matches!(reason, WaitingReason::Approval);
                task.checkpoint.waiting = Some(reason);
                task.checkpoint.diagnostic = diagnostic;
                repository::save(
                    &self.store(),
                    &mut task,
                    if permission {
                        TaskStatus::AwaitingPermission
                    } else {
                        TaskStatus::WaitingForChildren
                    },
                )?;
            }
            ExecutionStop::Failed { diagnostic } => {
                task.checkpoint.diagnostic = Some(diagnostic);
                repository::save(&self.store(), &mut task, TaskStatus::Failed)?;
            }
        }
        Ok(())
    }
}

fn save_waiting(store: &Store, task: &mut StoredTask) -> Result<(), SchedulerError> {
    let status = if matches!(task.checkpoint.waiting, Some(WaitingReason::Approval)) {
        TaskStatus::AwaitingPermission
    } else {
        TaskStatus::WaitingForChildren
    };
    if task.checkpoint.task.status != status && task.checkpoint.task.status != TaskStatus::Running {
        repository::save(store, task, TaskStatus::Running)?;
    }
    repository::save(store, task, status)
}

fn repair_handoff(
    store: &Store,
    task: &mut StoredTask,
    conversation: &Conversation,
) -> Result<(), SchedulerError> {
    let Some(next) = task.checkpoint.queued.first() else {
        return Ok(());
    };
    if conversation.info.pending_command_id == Some(next.command_id)
        && conversation.native_run().is_some_and(|run| {
            run.task_id == task.definition.initial.id
                && run.command_id == task.checkpoint.active_command
                && run.status == LoopStatus::Completed
        })
    {
        if instruction(conversation, next.command_id)? != next.instruction {
            return Err(SchedulerError::Conflict(task.definition.initial.id));
        }
        task.checkpoint.active_command = next.command_id;
        task.checkpoint.queued.remove(0);
        task.checkpoint.waiting = None;
        repository::save(store, task, TaskStatus::Queued)?;
    }
    Ok(())
}

fn initial_task(
    initial: TaskRecord,
    configuration: crate::agent::ConversationConfig,
    execution: ExecutionPolicy,
) -> StoredTask {
    StoredTask {
        definition: Definition {
            scheduler_version: 1,
            initial: initial.clone(),
            configuration,
            execution,
        },
        checkpoint: TaskCheckpoint {
            scheduler_version: 1,
            active_command: initial.originating_command_id,
            task: initial,
            queued: Vec::new(),
            waiting: None,
            cancel_requested: false,
            last_output: None,
            diagnostic: None,
            resumes: Vec::new(),
        },
        state: crate::storage::TaskState::Queued,
    }
}

fn instruction(conversation: &Conversation, command: CommandId) -> Result<String, SchedulerError> {
    let message = conversation
        .messages
        .iter()
        .find(|message| {
            message.causation_id == Some(command) && message.message.role == MessageRole::User
        })
        .ok_or(SchedulerError::Invalid(
            "task user command has not been durably accepted",
        ))?;
    let mut instruction = String::new();
    for content in &message.message.content {
        if let MessageContent::Text { text } = content {
            instruction.push_str(text);
        }
    }
    validate_instruction(&instruction)?;
    Ok(instruction)
}

fn validate_instruction(instruction: &str) -> Result<(), SchedulerError> {
    if instruction.trim().is_empty() || instruction.len() > 131_072 || instruction.contains('\0') {
        return Err(SchedulerError::Invalid(
            "instruction requires 1 to 131072 UTF-8 bytes without NUL",
        ));
    }
    Ok(())
}

fn mutation_uncertain(error: SchedulerError) -> SchedulerError {
    let message = error.to_string();
    let mut end = message.len().min(2048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    SchedulerError::MutationUncertain(message[..end].to_owned())
}

fn diagnostic(message: &str) -> String {
    let mut end = message.len().min(2048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

fn native_waiting(conversation: &Conversation, task: &StoredTask) -> Option<WaitingReason> {
    let run = conversation.native_run().filter(|run| {
        run.command_id == task.checkpoint.active_command
            && run.task_id == task.definition.initial.id
    })?;
    match run.status {
        LoopStatus::ProviderInFlight { attempt_id } => {
            Some(WaitingReason::ProviderUncertain { attempt_id })
        }
        LoopStatus::AwaitingPermission => Some(WaitingReason::Approval),
        LoopStatus::Paused { reason } => Some(match reason {
            PauseReason::OperationInFlight { .. } => WaitingReason::InFlight,
            PauseReason::ReconciliationRequired { .. } | PauseReason::BrokerRejected { .. } => {
                WaitingReason::Reconciliation
            }
            PauseReason::InterruptedTool => WaitingReason::Interrupted,
            PauseReason::ModelAttemptLimit
            | PauseReason::ToolCallLimit
            | PauseReason::TokenLimit
            | PauseReason::DurationLimit => WaitingReason::Limit,
            PauseReason::Cancellation => WaitingReason::Interrupted,
            reason => WaitingReason::NativePaused {
                status: LoopStatus::Paused { reason },
            },
        }),
        LoopStatus::Ready | LoopStatus::ExecutingTools | LoopStatus::Completed => None,
    }
}

fn derived_id(operation: CommandId, purpose: &[u8]) -> uuid::Uuid {
    let mut digest = Sha256::new();
    digest.update(b"everything.scheduler.task-control.v1\0");
    digest.update(operation.as_uuid().as_bytes());
    digest.update(purpose);
    let hash = digest.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}
