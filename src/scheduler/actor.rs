use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc, oneshot, watch};
use tokio::task::{Id, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    Conversation, LoopStatus, PauseReason, RemoteResultReceipt, TaskInput, TaskInputCommand,
    TaskInputReceipt, TextCommand, VerifiedRemoteOutcome,
};
use crate::permissions::ExecutionMode;
use crate::protocol::{
    CommandId, ConversationId, MessageContent, MessageRole, TaskId, TaskRecord, TaskStatus,
    UnixMillis,
};
use crate::storage::{Store, ToolCallState};

use super::remote::{
    self, RemoteDispatchPermit, RemoteRelease, RemoteTaskAdmission, RemoteTaskBinding,
    RemoteTaskSnapshot,
};
use super::repository;
use super::schedule_context::ScheduleCallerContext;
use super::types::*;
use super::worker::{self, ExecutionOutcome, ExecutionStop};
use super::workspace;

const BROKER_QUARANTINED: &str = "Tool broker is quarantined by unresolved tool cleanup; this task stays queued until the owner inspects the tool_recovery query and records acknowledge_cleanup for each requirement";

#[derive(Clone)]
pub struct Scheduler {
    pub(super) inner: Arc<HandleInner>,
}

pub(super) struct HandleInner {
    sender: mpsc::Sender<Control>,
    shutdown_grace: std::time::Duration,
    changed: watch::Sender<u64>,
    live: watch::Sender<HashMap<TaskId, LiveText>>,
    drained: watch::Receiver<Option<ShutdownReport>>,
    remote_admission: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShutdownReport {
    pub active_tasks: usize,
    pub broker_quarantined: bool,
    pub unresolved_remote_tasks: usize,
}

type Reply = oneshot::Sender<Result<TaskSnapshot, SchedulerError>>;
pub(super) type MessageReceipt = (TaskSnapshot, Option<String>);

enum Control {
    Activate(oneshot::Sender<Result<(), SchedulerError>>),
    Enqueue(TaskSubmission, Reply),
    Status(TaskId, Reply),
    ScheduleContext {
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        tool_name: String,
        reply: oneshot::Sender<Result<ScheduleCallerContext, SchedulerError>>,
    },
    Active(
        ConversationId,
        oneshot::Sender<Result<Option<TaskSnapshot>, SchedulerError>>,
    ),
    Wake(TaskId, Reply),
    Steer {
        task: TaskId,
        delivery: CommandId,
        accepted_at: UnixMillis,
        instruction: String,
        reply: oneshot::Sender<Result<TaskInputReceipt, SchedulerError>>,
    },
    Cancel(TaskId, OwnedMutexGuard<()>, Reply),
    AdmitRemote {
        authorization: Box<RemoteTaskAdmission>,
        admission: OwnedMutexGuard<()>,
        reply: Reply,
    },
    RemoteStatus(
        TaskId,
        oneshot::Sender<Result<RemoteTaskSnapshot, SchedulerError>>,
    ),
    RemoteResult {
        task: TaskId,
        outcome: Box<VerifiedRemoteOutcome>,
        admission: OwnedMutexGuard<()>,
        reply: oneshot::Sender<Result<RemoteResultReceipt, SchedulerError>>,
    },
    RemoteReceipt(
        TaskId,
        oneshot::Sender<Result<RemoteResultReceipt, SchedulerError>>,
    ),
    RemoteByBinding(
        RemoteTaskBinding,
        oneshot::Sender<Result<RemoteTaskSnapshot, SchedulerError>>,
    ),
    RemotePermit {
        task: TaskId,
        binding: RemoteTaskBinding,
        admission: OwnedMutexGuard<()>,
        reply: oneshot::Sender<Result<RemoteDispatchPermit, SchedulerError>>,
    },
    ReleaseRemote {
        task: TaskId,
        binding: RemoteTaskBinding,
        release: Box<RemoteRelease>,
        admission: OwnedMutexGuard<()>,
        reply: oneshot::Sender<Result<RemoteTaskSnapshot, SchedulerError>>,
    },
    RetireProviderUnavailable {
        task: TaskId,
        command: CommandId,
        request: CommandId,
        reply: Reply,
    },
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
    Wait {
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        children: Vec<TaskId>,
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
    Message {
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        to: String,
        text: String,
        wait: bool,
        reply: oneshot::Sender<Result<MessageReceipt, SchedulerError>>,
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
        admission: OwnedMutexGuard<()>,
        reply: Reply,
    },
    Shutdown(oneshot::Sender<ShutdownReport>, OwnedMutexGuard<()>),
    Redispatch(oneshot::Sender<Result<(), SchedulerError>>),
}

impl Scheduler {
    pub async fn start(
        services: SchedulerServices,
        limits: SchedulerLimits,
    ) -> Result<Self, SchedulerError> {
        Self::start_inner(services, limits, None).await
    }

    pub async fn start_with_preparation(
        services: SchedulerServices,
        limits: SchedulerLimits,
        preparation: Arc<dyn TaskPreparation>,
    ) -> Result<Self, SchedulerError> {
        Self::start_inner(services, limits, Some(preparation)).await
    }

    async fn start_inner(
        services: SchedulerServices,
        limits: SchedulerLimits,
        preparation: Option<Arc<dyn TaskPreparation>>,
    ) -> Result<Self, SchedulerError> {
        limits.validate()?;
        let (sender, receiver) = mpsc::channel(64);
        let (ready, started) = oneshot::channel();
        let (changed, _) = watch::channel(0);
        let actor_changed = changed.clone();
        let (live, _) = watch::channel(HashMap::new());
        let actor_live = live.clone();
        let (drained_sender, drained) = watch::channel(None);
        let runtime = tokio::runtime::Handle::current();
        let shutdown_grace = limits.shutdown_grace;
        tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                let mut actor = Actor::new(
                    services,
                    limits,
                    receiver,
                    actor_changed,
                    actor_live,
                    drained_sender,
                    preparation,
                );
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
                live,
                drained,
                remote_admission: Arc::new(Mutex::new(())),
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

    pub async fn schedule_context(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        tool_name: String,
    ) -> Result<ScheduleCallerContext, SchedulerError> {
        if tool_name.is_empty()
            || tool_name.len() > 128
            || tool_name.chars().any(char::is_control)
            || !workspace.is_absolute()
            || workspace
                .to_str()
                .is_none_or(|path| path.len() > 4096 || path.chars().any(char::is_control))
        {
            return Err(SchedulerError::Invalid("invalid scheduling tool context"));
        }
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::ScheduleContext {
                caller,
                workspace,
                operation,
                tool_name,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
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

    pub fn live_text(&self, task: TaskId) -> Option<LiveText> {
        self.inner.live.borrow().get(&task).cloned()
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

    pub async fn redispatch(&self) -> Result<(), SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::Redispatch(reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
    }

    pub async fn steer(
        &self,
        task: TaskId,
        delivery_id: CommandId,
        accepted_at: UnixMillis,
        instruction: String,
    ) -> Result<TaskInputReceipt, SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::Steer {
                task,
                delivery: delivery_id,
                accepted_at,
                instruction,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| {
            SchedulerError::MutationUncertain(
                "control actor stopped after accepting steering".into(),
            )
        })?
    }

    pub async fn cancel(&self, task: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
        self.request(|reply| Control::Cancel(task, admission, reply))
            .await
    }

    pub async fn admit_remote(
        &self,
        authorization: RemoteTaskAdmission,
    ) -> Result<TaskSnapshot, SchedulerError> {
        let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
        self.request(|reply| Control::AdmitRemote {
            authorization: Box::new(authorization),
            admission,
            reply,
        })
        .await
    }

    pub async fn remote_status(&self, task: TaskId) -> Result<RemoteTaskSnapshot, SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::RemoteStatus(task, reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
    }

    pub async fn remote_dispatch_permit(
        &self,
        task: TaskId,
        binding: RemoteTaskBinding,
    ) -> Result<RemoteDispatchPermit, SchedulerError> {
        binding.validate()?;
        let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::RemotePermit {
                task,
                binding,
                admission,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| {
            SchedulerError::MutationUncertain(
                "remote dispatch authorization response was lost".into(),
            )
        })?
    }

    pub async fn admit_remote_result(
        &self,
        task: TaskId,
        outcome: VerifiedRemoteOutcome,
    ) -> Result<RemoteResultReceipt, SchedulerError> {
        let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::RemoteResult {
                task,
                outcome: Box::new(outcome),
                admission,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| {
            SchedulerError::MutationUncertain(
                "remote result response was lost; inspect the original receipt".into(),
            )
        })?
    }

    pub async fn release_remote(
        &self,
        task: TaskId,
        binding: RemoteTaskBinding,
        release: RemoteRelease,
    ) -> Result<RemoteTaskSnapshot, SchedulerError> {
        binding.validate()?;
        release.validate()?;
        let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::ReleaseRemote {
                task,
                binding,
                release: Box::new(release),
                admission,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| {
            SchedulerError::MutationUncertain(
                "remote release response was lost; inspect the original task".into(),
            )
        })?
    }

    pub(crate) async fn remote_result_receipt(
        &self,
        task: TaskId,
    ) -> Result<RemoteResultReceipt, SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::RemoteReceipt(task, reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
    }

    pub async fn remote_by_binding(
        &self,
        binding: RemoteTaskBinding,
    ) -> Result<RemoteTaskSnapshot, SchedulerError> {
        binding.validate()?;
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::RemoteByBinding(binding, reply))
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| SchedulerError::Stopped)?
    }

    pub async fn retire_provider_unavailable(
        &self,
        task: TaskId,
        expected_command: CommandId,
        request_id: CommandId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::RetireProviderUnavailable {
            task,
            command: expected_command,
            request: request_id,
            reply,
        })
        .await
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
            let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
            self.inner
                .sender
                .send(Control::Shutdown(reply, admission))
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

    pub(super) async fn tool_wait(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        children: Vec<TaskId>,
    ) -> Result<TaskSnapshot, SchedulerError> {
        self.request(|reply| Control::Wait {
            caller,
            workspace,
            operation,
            children,
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

    pub(super) async fn tool_message(
        &self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        to: String,
        text: String,
        wait: bool,
    ) -> Result<MessageReceipt, SchedulerError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .sender
            .send(Control::Message {
                caller,
                workspace,
                operation,
                to,
                text,
                wait,
                reply,
            })
            .await
            .map_err(|_| SchedulerError::Stopped)?;
        result.await.map_err(|_| {
            SchedulerError::MutationUncertain(
                "control actor stopped after accepting the request".into(),
            )
        })?
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
        let admission = Arc::clone(&self.inner.remote_admission).lock_owned().await;
        self.request(|reply| Control::ScopedCancel {
            caller,
            workspace,
            target,
            admission,
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

impl Drop for Active {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

struct Actor {
    services: Arc<SchedulerServices>,
    preparation: Option<Arc<dyn TaskPreparation>>,
    limits: SchedulerLimits,
    receiver: mpsc::Receiver<Control>,
    queue: VecDeque<TaskId>,
    active: HashMap<TaskId, Active>,
    remote_reserved: HashSet<TaskId>,
    workers: JoinSet<ExecutionOutcome>,
    retry_once: HashMap<TaskId, ResumeTarget>,
    wake_active: HashSet<TaskId>,
    approval_deadlines: HashMap<TaskId, tokio::time::Instant>,
    stopping: bool,
    activated: bool,
    shutdown: Vec<oneshot::Sender<ShutdownReport>>,
    controls_closed: bool,
    changed: watch::Sender<u64>,
    live: watch::Sender<HashMap<TaskId, LiveText>>,
    drained: watch::Sender<Option<ShutdownReport>>,
}

impl Actor {
    fn new(
        services: SchedulerServices,
        limits: SchedulerLimits,
        receiver: mpsc::Receiver<Control>,
        changed: watch::Sender<u64>,
        live: watch::Sender<HashMap<TaskId, LiveText>>,
        drained: watch::Sender<Option<ShutdownReport>>,
        preparation: Option<Arc<dyn TaskPreparation>>,
    ) -> Self {
        Self {
            services: Arc::new(services),
            preparation,
            limits,
            receiver,
            queue: VecDeque::new(),
            active: HashMap::new(),
            remote_reserved: HashSet::new(),
            workers: JoinSet::new(),
            retry_once: HashMap::new(),
            wake_active: HashSet::new(),
            approval_deadlines: HashMap::new(),
            stopping: false,
            activated: false,
            shutdown: Vec::new(),
            controls_closed: false,
            changed,
            live,
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
        for id in repository::remote_ids(&self.store())? {
            let task = repository::load(&self.store(), id)?;
            let saved = remote::snapshot(&task)?;
            if self.services.agent.state_directory().is_none() {
                return Err(SchedulerError::Invalid(
                    "remote recovery requires a leased Agent",
                ));
            }
            if let Some(result) = saved.result {
                self.services
                    .agent
                    .restore_remote_result_receipt(
                        task.definition.initial.conversation_id,
                        saved.binding,
                        result,
                    )
                    .await?;
            }
        }
        let tasks = repository::unfinished(&self.store(), self.limits.max_pending_tasks)?;
        for saved in tasks {
            let id = saved.definition.initial.id;
            let mut task = repository::load(&self.store(), id)?;
            if task.checkpoint.task.status.is_terminal() {
                continue;
            }
            if task.definition.remote.is_some() {
                if self.services.agent.state_directory().is_none() {
                    return Err(SchedulerError::Invalid(
                        "remote recovery requires a leased Agent",
                    ));
                }
                self.remote_reserved.insert(id);
                continue;
            }
            match repository::encode_checkpoint(&task.checkpoint) {
                Err(SchedulerError::Capacity("task checkpoint bytes")) => {
                    tracing::warn!(task_id=%id, "legacy scheduler checkpoint lacks reserved lifecycle and child delivery capacity; owner inspection is required before admission");
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
            match native_waiting(&conversation, &task)
                .filter(|_| !pending_input_can_run(&conversation, &task))
            {
                Some(WaitingReason::Approval | WaitingReason::InFlight) => {
                    task.checkpoint.waiting = None;
                    repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
                    self.queue(id);
                }
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
        self.diagnose_stranded_waits()?;
        self.repair_child_deliveries().await?;
        Ok(())
    }

    fn diagnose_stranded_waits(&self) -> Result<(), SchedulerError> {
        for mut parent in repository::unfinished(&self.store(), self.limits.max_pending_tasks)? {
            let Some(wait) = parent.checkpoint.child_wait.as_ref() else {
                continue;
            };
            if parent.checkpoint.cancel_requested
                || !matches!(
                    parent.checkpoint.waiting,
                    Some(WaitingReason::Children { .. })
                )
            {
                continue;
            }
            let mut stranded = Vec::new();
            for child in wait
                .children
                .iter()
                .filter(|child| child.delivery.is_none())
            {
                let task = repository::load(&self.store(), child.task_id)?;
                if task.checkpoint.task.status.is_terminal() || task.checkpoint.cancel_requested {
                    continue;
                }
                if let Some(reason) = task.checkpoint.waiting.filter(owner_decision) {
                    stranded.push(format!(
                        "child {}, which needs an owner decision ({reason:?})",
                        child.task_id
                    ));
                }
            }
            if stranded.is_empty() {
                continue;
            }
            let message = diagnostic(&format!(
                "At host startup this task was waiting for {}: resume or cancel that child, or cancel this task",
                stranded.join("; ")
            ));
            if parent.checkpoint.diagnostic.as_deref() == Some(message.as_str()) {
                continue;
            }
            tracing::warn!(task_id=%parent.definition.initial.id, diagnostic=%message, "parent waits on children that need an owner decision");
            parent.checkpoint.diagnostic = Some(message);
            let status = parent.checkpoint.task.status;
            repository::save(&self.store(), &mut parent, status)?;
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
                    unresolved_remote_tasks: self.remote_reserved.len(),
                };
                self.drained.send_replace(Some(report.clone()));
                for reply in self.shutdown.drain(..) {
                    let _ = reply.send(report.clone());
                }
                return;
            }
            let approval_deadline = self
                .approval_deadlines
                .values()
                .min()
                .copied()
                .filter(|_| self.activated && !self.stopping);
            tokio::select! {
                _ = tokio::time::sleep_until(approval_deadline.unwrap_or_else(tokio::time::Instant::now)), if approval_deadline.is_some() => {
                    self.expire_approvals().await;
                    self.changed.send_modify(|revision| *revision = revision.wrapping_add(1));
                }
                control = self.receiver.recv(), if !self.controls_closed => {
                    match control {
                        Some(control) => { let notify=!matches!(control,Control::Status(..)|Control::RemoteStatus(..)|Control::RemoteReceipt(..)|Control::RemoteByBinding(..)|Control::ScopedStatus{..}|Control::ScheduleContext{..}|Control::Active(..)); self.control(control).await; if notify{self.changed.send_modify(|revision| *revision = revision.wrapping_add(1));} },
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
                                self.wake_active.remove(&id);
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

    async fn expire_approvals(&mut self) {
        let now = tokio::time::Instant::now();
        let expired = self
            .approval_deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired {
            self.approval_deadlines.remove(&id);
            if let Err(error) = self.wake(id).await {
                tracing::warn!(task_id=%id, error=%error, "approval expiry wake failed; owner wake remains available");
            }
        }
    }

    async fn track_approval(&mut self, task: &StoredTask) -> Result<(), SchedulerError> {
        let id = task.definition.initial.id;
        let conversation = self
            .services
            .agent
            .conversation(task.definition.initial.conversation_id)
            .await?;
        match conversation.approval_expires_at() {
            Some(expires_at) => {
                let remaining = u64::try_from(expires_at)
                    .unwrap_or(0)
                    .saturating_sub(UnixMillis::now()?.get());
                self.approval_deadlines.insert(
                    id,
                    tokio::time::Instant::now() + std::time::Duration::from_millis(remaining),
                );
            }
            None => {
                self.approval_deadlines.remove(&id);
            }
        }
        Ok(())
    }

    async fn redispatch(&mut self) -> Result<(), SchedulerError> {
        for task in repository::unfinished(&self.store(), self.limits.max_pending_tasks)? {
            let id = task.definition.initial.id;
            if task.definition.remote.is_none()
                && !task.checkpoint.task.status.is_terminal()
                && repository::cancelled_ancestor(&self.store(), id)?
            {
                self.queue(id);
            }
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.stopping = true;
        self.wake_active.clear();
        self.approval_deadlines.clear();
        for active in self.active.values() {
            active.cancellation.cancel();
        }
    }

    async fn control(&mut self, control: Control) {
        if let Control::Shutdown(reply, _admission) = control {
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
            Control::RemoteStatus(id, reply) => {
                let _ = reply.send(
                    repository::load(&self.store(), id).and_then(|task| remote::snapshot(&task)),
                );
            }
            Control::RemoteByBinding(binding, reply) => {
                let result = (|| {
                    let id =
                        repository::remote_id_by_delegation(&self.store(), binding.delegation_id)?;
                    let saved = remote::snapshot(&repository::load(&self.store(), id)?)?;
                    if saved.binding != binding {
                        return Err(SchedulerError::Conflict(id));
                    }
                    Ok(saved)
                })();
                let _ = reply.send(result);
            }
            Control::RemoteReceipt(task, reply) => {
                let result = self.remote_result_receipt(task).await;
                let _ = reply.send(result);
            }
            Control::RemoteResult {
                task,
                outcome,
                admission,
                reply,
            } => {
                let result = self.admit_remote_result(task, *outcome, admission).await;
                if matches!(result, Err(SchedulerError::MutationUncertain(_))) {
                    self.stop();
                }
                let _ = reply.send(result);
            }
            Control::AdmitRemote {
                authorization,
                admission,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.admit_remote(*authorization, admission).await
                };
                if matches!(result, Err(SchedulerError::MutationUncertain(_))) {
                    self.stop();
                }
                let _ = reply.send(result);
            }
            Control::RemotePermit {
                task,
                binding,
                admission,
                reply,
            } => {
                let result = self.remote_dispatch_permit(task, binding, admission).await;
                let _ = reply.send(result);
            }
            Control::ReleaseRemote {
                task,
                binding,
                release,
                admission,
                reply,
            } => {
                let result = self.release_remote(task, binding, *release, admission);
                if matches!(result, Err(SchedulerError::MutationUncertain(_))) {
                    self.stop();
                }
                let _ = reply.send(result);
            }
            Control::ScheduleContext {
                caller,
                workspace,
                operation,
                tool_name,
                reply,
            } => {
                let result = (|| {
                    if self.stopping {
                        return Err(SchedulerError::Stopped);
                    }
                    let task = self.validate_caller(caller, &workspace)?;
                    self.operation_time(caller, operation, &tool_name)?;
                    let recorded = self
                        .store()
                        .tool_call(&operation.to_string())?
                        .ok_or(SchedulerError::OutsideScope)?;
                    if recorded.state != ToolCallState::Running
                        || recorded.task_id.as_deref() != Some(caller.to_string().as_str())
                        || recorded.conversation_id
                            != task.definition.initial.conversation_id.to_string()
                        || recorded.tool_name != tool_name
                    {
                        return Err(SchedulerError::OutsideScope);
                    }
                    let active = self
                        .active
                        .get(&caller)
                        .ok_or(SchedulerError::OutsideScope)?;
                    if active.cancellation.is_cancelled() {
                        return Err(SchedulerError::OutsideScope);
                    }
                    Ok(ScheduleCallerContext::new(
                        task.definition.configuration,
                        repository::authority(&self.store(), caller)?,
                        task.checkpoint.active_command,
                        operation,
                        tool_name,
                        active.cancellation.child_token(),
                        task.definition.execution.agent.map(|agent| agent.id),
                    ))
                })();
                let _ = reply.send(result);
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
            Control::Redispatch(reply) => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.redispatch().await
                };
                let _ = reply.send(result);
            }
            Control::Steer {
                task,
                delivery,
                accepted_at,
                instruction,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.steer(task, delivery, accepted_at, instruction).await
                };
                let _ = reply.send(result);
            }
            Control::Cancel(id, _admission, reply) => {
                let result = self.cancel(id).await;
                let _ = reply.send(result);
            }
            Control::RetireProviderUnavailable {
                task,
                command,
                request,
                reply,
            } => {
                let result = self
                    .retire_provider_unavailable(task, command, request)
                    .await;
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
            Control::Wait {
                caller,
                workspace,
                operation,
                children,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.wait(caller, workspace, operation, children, "wait_agents")
                        .await
                };
                let _ = reply.send(result);
            }
            Control::Message {
                caller,
                workspace,
                operation,
                to,
                text,
                wait,
                reply,
            } => {
                let result = if self.stopping {
                    Err(SchedulerError::Stopped)
                } else {
                    self.message(caller, workspace, operation, to, text, wait)
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
                admission: _admission,
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
            Control::Shutdown(..) => {}
        }
    }

    async fn admit_remote(
        &mut self,
        authorization: RemoteTaskAdmission,
        admission: OwnedMutexGuard<()>,
    ) -> Result<TaskSnapshot, SchedulerError> {
        authorization.validate_owner(&self.store())?;
        let submission = authorization.submission().clone();
        if self.services.agent.state_directory().is_none() {
            return Err(SchedulerError::Invalid(
                "remote admission requires a leased Agent",
            ));
        }
        let id = submission.task.task_id;
        match repository::load(&self.store(), id) {
            Ok(existing) => {
                authorization.validate_task(&existing)?;
                let saved = remote::snapshot(&existing)?;
                if existing.definition.initial.conversation_id != submission.task.conversation_id
                    || existing.definition.initial.originating_command_id
                        != submission.task.command_id
                    || existing.definition.initial.created_at != submission.task.accepted_at
                    || repository::encode(&existing.definition.execution)?
                        != repository::encode(&submission.task.execution)?
                    || saved.binding != submission.binding
                    || saved.approval != submission.approval
                {
                    return Err(SchedulerError::Conflict(id));
                }
                if let Some(result) = saved.result {
                    self.services
                        .agent
                        .restore_remote_result_receipt(
                            existing.definition.initial.conversation_id,
                            saved.binding,
                            result,
                        )
                        .await?;
                }
                if !matches!(
                    saved.phase,
                    remote::RemoteTaskPhase::Settled | remote::RemoteTaskPhase::Released
                ) {
                    self.remote_reserved.insert(id);
                }
                return Ok(existing.snapshot());
            }
            Err(SchedulerError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let conversation_id = submission.task.conversation_id;
        let command = submission.task.command_id;
        let conversation = self.services.agent.conversation(conversation_id).await?;
        let task_instruction = instruction(&conversation, command)?;
        if conversation.config != *authorization.configuration()
            || task_instruction != authorization.instruction()
        {
            return Err(SchedulerError::Conflict(id));
        }
        authorization.validate_fresh(&self.store())?;
        let mut initial = TaskRecord::new(
            conversation_id,
            None,
            command,
            task_instruction,
            submission.task.accepted_at,
        );
        initial.id = id;
        let mut task = initial_task(initial, conversation.config, submission.task.execution);
        task.definition.remote = Some(remote::RemoteDefinition {
            version: 1,
            binding: submission.binding,
            approval: submission.approval,
        });
        let definition = task
            .definition
            .remote
            .as_ref()
            .ok_or(SchedulerError::Invalid("remote definition is missing"))?;
        remote::validate_definition(
            definition,
            &task.definition.execution,
            &task.definition.configuration,
        )?;
        repository::check_remote_identity(&self.store(), &task)?;
        if self.active.values().filter(|active| !active.child).count() + self.remote_reserved.len()
            >= self.limits.max_primary_tasks
        {
            return Err(SchedulerError::Capacity("primary execution slots"));
        }
        if self.services.broker.is_quarantined() {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let now = UnixMillis::now()?;
        if now.get() < definition.approval.approved_at.get()
            || now.get() >= definition.approval.expires_at.get()
            || submission.task.accepted_at.get() > definition.approval.approved_at.get()
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        task.definition.execution.authority(conversation_id, id)?;
        for tool in task
            .definition
            .execution
            .allowed_tools
            .as_ref()
            .ok_or(SchedulerError::Invalid("remote policy was not captured"))?
        {
            self.services.registry.descriptor(tool)?;
        }
        let _native = self
            .services
            .agent
            .guard_unstarted_tools(conversation_id, command, id)
            .await?;
        let current = self.services.agent.conversation(conversation_id).await?;
        if current.native_run().is_some()
            || current.config != task.definition.configuration
            || instruction(&current, command)? != task.definition.initial.instruction
        {
            return Err(SchedulerError::Conflict(id));
        }
        let fresh_now = UnixMillis::now()?;
        if fresh_now.get() < now.get() || fresh_now.get() >= definition.approval.expires_at.get() {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let now = fresh_now;
        task.checkpoint
            .task
            .transition(TaskStatus::Running, now, None)?;
        task.checkpoint
            .task
            .transition(TaskStatus::WaitingForChildren, now, None)?;
        task.state = crate::storage::TaskState::Waiting;
        task.checkpoint.waiting = Some(WaitingReason::Remote {
            phase: remote::RemoteTaskPhase::Reserved,
        });
        task.checkpoint.remote = Some(remote::RemoteCheckpoint {
            charge: remote::RemoteBudgetCharge::from_limits(&task.definition.execution.limits)?,
            phase: remote::RemoteTaskPhase::Reserved,
            admitted_at: now,
            last_dispatch_at: None,
            result: None,
            release: None,
        });
        remote::validate_task(&task)?;
        let resources = (
            self.store(),
            task,
            authorization,
            admission,
            _native,
            self.services.agent.clone(),
        );
        let max_pending = self.limits.max_pending_tasks;
        let inserted = tokio::task::spawn_blocking(move || {
            let resources = resources;
            repository::insert_remote(&resources.0, &resources.1, max_pending, &resources.2)?;
            repository::load(&resources.0, id).map(|task| task.snapshot())
        })
        .await
        .map_err(|_| mutation_uncertain(SchedulerError::Worker))?
        .map_err(|error| match error {
            SchedulerError::Capacity(_)
            | SchedulerError::Invalid(_)
            | SchedulerError::Conflict(_)
            | SchedulerError::OwnerDecision(_) => error,
            _ => mutation_uncertain(error),
        })?;
        self.remote_reserved.insert(id);
        Ok(inserted)
    }

    async fn remote_result_receipt(
        &self,
        id: TaskId,
    ) -> Result<RemoteResultReceipt, SchedulerError> {
        let task = repository::load(&self.store(), id)?;
        let saved = remote::snapshot(&task)?;
        let result = saved.result.ok_or(SchedulerError::OwnerDecision(id))?;
        Ok(self
            .services
            .agent
            .restore_remote_result_receipt(
                task.definition.initial.conversation_id,
                saved.binding,
                result,
            )
            .await?)
    }

    async fn admit_remote_result(
        &mut self,
        id: TaskId,
        outcome: VerifiedRemoteOutcome,
        admission: OwnedMutexGuard<()>,
    ) -> Result<RemoteResultReceipt, SchedulerError> {
        outcome.validate_owner(&self.services.agent)?;
        let task = repository::load(&self.store(), id)?;
        let saved = remote::snapshot(&task)?;
        if outcome.binding() != &saved.binding
            || outcome.report().task_id != id
            || outcome.report().command_id != task.checkpoint.active_command
        {
            return Err(SchedulerError::Conflict(id));
        }
        if let Some(result) = saved.result {
            if !outcome.same_source(&result.report) {
                return Err(SchedulerError::Conflict(id));
            }
            let _resources = (outcome, admission);
            return self.remote_result_receipt(id).await;
        }
        if self.stopping {
            return Err(SchedulerError::Stopped);
        }
        if !self.activated
            || !self.remote_reserved.contains(&id)
            || self.services.broker.is_quarantined()
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        outcome.validate_fresh_owner(&self.services.agent)?;
        repository::authority(&self.store(), id)?;
        let completion = remote::RemoteResultCommit::new(task, outcome, admission)?;
        let receipt = self
            .services
            .agent
            .commit_remote_result(completion)
            .await
            .map_err(|error| match error {
                crate::agent::AgentError::Busy
                | crate::agent::AgentError::NoPendingTurn
                | crate::agent::AgentError::InvalidData(_)
                | crate::agent::AgentError::CommandConflict(_)
                | crate::agent::AgentError::ConfigurationMismatch => SchedulerError::Agent(error),
                crate::agent::AgentError::Storage(
                    crate::storage::StorageError::StateConflict { .. }
                    | crate::storage::StorageError::StateRevisionConflict { .. }
                    | crate::storage::StorageError::SequenceConflict { .. }
                    | crate::storage::StorageError::CommandConflict(_),
                ) => SchedulerError::Agent(error),
                _ => mutation_uncertain(SchedulerError::Agent(error)),
            })?;
        if receipt.inspection().task_status.is_terminal() {
            self.remote_reserved.remove(&id);
        }
        Ok(receipt)
    }

    fn release_remote(
        &mut self,
        id: TaskId,
        binding: RemoteTaskBinding,
        release: RemoteRelease,
        _admission: OwnedMutexGuard<()>,
    ) -> Result<RemoteTaskSnapshot, SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        let saved = remote::snapshot(&task)?;
        if saved.binding != binding {
            return Err(SchedulerError::Conflict(id));
        }
        if let Some(existing) = &saved.release {
            let mut replay = release.clone();
            replay.released_at = existing.released_at;
            if *existing != replay {
                return Err(SchedulerError::Conflict(id));
            }
            return Ok(saved);
        }
        if saved.phase != release.expected_phase {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let now = UnixMillis::now()?;
        let checkpoint = task
            .checkpoint
            .remote
            .as_mut()
            .ok_or(SchedulerError::Invalid("remote checkpoint is missing"))?;
        checkpoint.phase = remote::RemoteTaskPhase::Released;
        checkpoint.release = Some(RemoteRelease {
            released_at: now,
            ..release
        });
        task.checkpoint.waiting = None;
        task.checkpoint.diagnostic = Some("Owner released the unresolved remote reservation; the remote outcome remains unknown, the original allowance stays charged and later worker results are refused".into());
        task.checkpoint.task.transition(
            TaskStatus::Failed,
            now,
            Some(crate::protocol::TaskFailure {
                code: remote::RELEASED_FAILURE_CODE.into(),
                message: "Owner released an unresolved remote reservation".into(),
                retryable: false,
            }),
        )?;
        remote::validate_task(&task)?;
        repository::save(&self.store(), &mut task, TaskStatus::Failed)
            .map_err(mutation_uncertain)?;
        self.remote_reserved.remove(&id);
        remote::snapshot(&task)
    }

    async fn remote_dispatch_permit(
        &mut self,
        id: TaskId,
        binding: RemoteTaskBinding,
        admission: OwnedMutexGuard<()>,
    ) -> Result<RemoteDispatchPermit, SchedulerError> {
        if self.stopping {
            return Err(SchedulerError::Stopped);
        }
        if self.services.agent.state_directory().is_none() {
            return Err(SchedulerError::Invalid(
                "remote dispatch requires a leased Agent",
            ));
        }
        if !self.activated
            || !self.remote_reserved.contains(&id)
            || self.services.broker.is_quarantined()
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let mut task = repository::load(&self.store(), id)?;
        let saved = remote::snapshot(&task)?;
        let now = UnixMillis::now()?;
        if saved.binding != binding {
            return Err(SchedulerError::Conflict(id));
        }
        if saved.phase != remote::RemoteTaskPhase::Reserved
            || task.checkpoint.cancel_requested
            || now.get() < saved.last_dispatch_at.unwrap_or(saved.admitted_at).get()
            || now.get() >= saved.binding.deadline.get()
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        repository::authority(&self.store(), id)?;
        let native = self
            .services
            .agent
            .guard_unstarted_tools(
                task.definition.initial.conversation_id,
                task.checkpoint.active_command,
                id,
            )
            .await?;
        let conversation = self
            .services
            .agent
            .conversation(task.definition.initial.conversation_id)
            .await?;
        if conversation.native_run().is_some()
            || conversation.config != task.definition.configuration
            || instruction(&conversation, task.checkpoint.active_command)?
                != task.definition.initial.instruction
        {
            return Err(SchedulerError::Conflict(id));
        }
        let fresh_now = UnixMillis::now()?;
        if fresh_now.get() < now.get() || fresh_now.get() >= saved.binding.deadline.get() {
            return Err(SchedulerError::OwnerDecision(id));
        }
        task.checkpoint
            .remote
            .as_mut()
            .ok_or(SchedulerError::Invalid("remote checkpoint is missing"))?
            .last_dispatch_at = Some(fresh_now);
        repository::save(&self.store(), &mut task, TaskStatus::WaitingForChildren)
            .map_err(mutation_uncertain)?;
        Ok(RemoteDispatchPermit::new(
            remote::snapshot(&task)?,
            admission,
            Box::new(native),
            self.services.agent.clone(),
        ))
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

    async fn retire_provider_unavailable(
        &mut self,
        id: TaskId,
        command: CommandId,
        request: CommandId,
    ) -> Result<TaskSnapshot, SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        if let Some(retired) = &task.checkpoint.provider_retirement {
            return if retired.command_id == command && retired.request_id == request {
                Ok(task.snapshot())
            } else {
                Err(SchedulerError::Conflict(id))
            };
        }
        if self.stopping {
            return Err(SchedulerError::Stopped);
        }
        if self.active.contains_key(&id)
            || self.services.broker.is_quarantined()
            || task.definition.initial.parent_task_id.is_some()
            || task.checkpoint.active_command != command
            || task.checkpoint.task.status != TaskStatus::WaitingForChildren
            || !matches!(
                task.checkpoint.waiting,
                Some(WaitingReason::ProviderUnavailable)
            )
            || task.checkpoint.cancel_requested
            || !task.checkpoint.queued.is_empty()
            || !task.checkpoint.resumes.is_empty()
            || task.checkpoint.child_wait.is_some()
            || repository::cancelled_ancestor(&self.store(), id)?
        {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let native_guard = self
            .services
            .agent
            .guard_unstarted_tools(task.definition.initial.conversation_id, command, id)
            .await?;
        repository::retire_provider_unavailable(&self.store(), &mut task, request).map_err(
            |error| match error {
                SchedulerError::OwnerDecision(_)
                | SchedulerError::Conflict(_)
                | SchedulerError::Capacity(_)
                | SchedulerError::Invalid(_) => error,
                _ => mutation_uncertain(error),
            },
        )?;
        self.queue.retain(|queued| *queued != id);
        self.wake_active.remove(&id);
        self.retry_once.remove(&id);
        drop(native_guard);
        Ok(task.snapshot())
    }

    fn validate_caller(
        &self,
        caller: TaskId,
        workspace: &std::path::Path,
    ) -> Result<StoredTask, SchedulerError> {
        let task = repository::load(&self.store(), caller)?;
        if task.checkpoint.task.status != TaskStatus::Running
            || task.definition.remote.is_some()
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

    async fn message(
        &mut self,
        caller: TaskId,
        workspace_root: PathBuf,
        operation: CommandId,
        to: String,
        text: String,
        wait: bool,
    ) -> Result<MessageReceipt, SchedulerError> {
        validate_instruction(&text)?;
        let parent = self.validate_caller(caller, &workspace_root)?;
        let now = self.operation_time(caller, operation, "message_agent")?;
        let ids = MessageIds {
            task: TaskId::try_from(derived_id(operation, b"task"))?,
            conversation: ConversationId::try_from(derived_id(operation, b"conversation"))?,
            create: CommandId::try_from(derived_id(operation, b"create"))?,
            instruction: CommandId::try_from(derived_id(operation, b"instruction"))?,
        };
        let id = ids.task;
        let snapshot = match repository::load(&self.store(), id) {
            Ok(existing) => {
                if existing.definition.initial.parent_task_id != Some(caller)
                    || existing.definition.initial.instruction != text
                    || existing.definition.initial.conversation_id != ids.conversation
                    || existing
                        .definition
                        .delegation
                        .as_ref()
                        .is_none_or(|delegation| delegation.name.as_str() != to)
                {
                    return Err(SchedulerError::Conflict(id));
                }
                existing.snapshot()
            }
            Err(SchedulerError::NotFound(_)) => {
                self.admit_message(caller, parent, &workspace_root, now, ids, &to, text)
                    .await?
            }
            Err(error) => return Err(error),
        };
        if !wait {
            return Ok((snapshot, None));
        }
        match self
            .wait(caller, workspace_root, operation, vec![id], "message_agent")
            .await
        {
            Ok(_) => Ok((snapshot, None)),
            Err(error @ SchedulerError::MutationUncertain(_)) => Err(error),
            Err(error) => Ok((snapshot, Some(error.to_string()))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn admit_message(
        &mut self,
        caller: TaskId,
        parent: StoredTask,
        workspace_root: &std::path::Path,
        now: UnixMillis,
        ids: MessageIds,
        to: &str,
        text: String,
    ) -> Result<TaskSnapshot, SchedulerError> {
        let refuse = |code, message| SchedulerError::Delegation { code, message };
        let agents = crate::host::agents::AgentRepository::for_agent(&self.services.agent)
            .and_then(|repository| repository.snapshot())
            .map_err(|error| {
                tracing::warn!(%error, "agent messaging could not read the agents file");
                refuse("agents_unavailable", "the agents file cannot be read")
            })?;
        let caller_agent = parent
            .definition
            .execution
            .agent
            .and_then(|agent| agents.get(agent.id))
            .ok_or(refuse(
                "agent_not_found",
                "the calling task's agent no longer exists",
            ))?;
        let target = agents
            .by_name(to)
            .ok_or(refuse("agent_not_found", "no agent has that name"))?;
        if !matches!(&caller_agent.messaging, crate::host::agents::Messaging::May(targets) if targets.contains(&target.id))
        {
            return Err(refuse(
                "agent_not_allowed",
                "the owner has not allowed this agent to message that agent",
            ));
        }
        let lineage = repository::lineage(&self.store(), caller)?;
        let root = lineage.last().ok_or(SchedulerError::NotFound(caller))?;
        let mut path = lineage
            .iter()
            .find_map(|task| task.definition.delegation.as_ref())
            .map_or_else(
                || vec![caller_agent.id],
                |delegation| delegation.path.clone(),
            );
        if path.contains(&target.id) {
            return Err(refuse(
                "agent_loop",
                "that agent is already in this message chain",
            ));
        }
        if path.len() > 3 {
            return Err(refuse(
                "agent_depth",
                "a message chain is limited to three hops below the root turn",
            ));
        }
        if repository::delegated_children(&self.store(), caller)? >= 16 {
            return Err(refuse(
                "agent_message_limit",
                "a task can send at most sixteen agent messages",
            ));
        }
        let ceiling = root
            .definition
            .execution
            .delegation_ceiling
            .clone()
            .ok_or(refuse(
                "agent_messaging_unavailable",
                "the root turn did not capture an agent messaging ceiling",
            ))?;
        let root_authority = repository::delegation_authority(root)?;
        repository::check_capacity(&self.store(), self.limits.max_pending_tasks)?;
        repository::check_child_depth(&self.store(), caller)?;
        if parent.definition.initial.parent_task_id.is_some()
            && self.active.values().filter(|active| active.child).count()
                >= self.limits.max_subagents
        {
            return Err(SchedulerError::Capacity("nested subagent execution slots"));
        }
        let mut policy = parent.definition.execution.clone();
        policy.mode = ceiling.mode;
        policy.allowed_tools = ceiling.allowed_tools;
        policy.tools_enabled = true;
        let mut policy = target.narrow(policy, &self.services.registry);
        policy.delegation_ceiling = None;
        let conversation = self
            .services
            .agent
            .conversation(parent.definition.initial.conversation_id)
            .await?;
        if let Some(run) = conversation
            .native_run()
            .filter(|run| run.task_id == caller)
        {
            let remaining = parent
                .definition
                .execution
                .limits
                .max_charged_tokens
                .saturating_sub(run.charged_tokens)
                .saturating_sub(repository::active_delegated_grants(&self.store(), caller)?);
            if remaining == 0 {
                return Err(SchedulerError::Capacity("caller token budget"));
            }
            policy.limits.max_charged_tokens = remaining;
            let deadline = UnixMillis::try_from(run.deadline_ms)?;
            policy.limits.absolute_deadline = Some(
                policy
                    .limits
                    .absolute_deadline
                    .map_or(deadline, |current| current.min(deadline)),
            );
        }
        let mut base = parent.definition.configuration.clone();
        crate::host::agents::DEFAULT_INSTRUCTIONS.clone_into(&mut base.instructions);
        let configuration = target.configure(base).map_err(|error| {
            tracing::warn!(%error, target = %target.id, "agent message target configuration is invalid");
            refuse(
                "agent_configuration",
                "the target agent produces an invalid conversation configuration",
            )
        })?;
        path.push(target.id);
        let delegation = Delegation {
            from: caller_agent.id,
            to: target.id,
            name: target.name.clone(),
            path,
        };
        let title = format!("Agent {} {}", target.name.as_str(), ids.task);
        let workspace =
            workspace::prepare(workspace_root, caller, ids.task).map_err(|error| match error {
                SchedulerError::Capacity(_) => error,
                _ => mutation_uncertain(error),
            })?;
        let admitted = async {
            policy.workspace_root = workspace;
            let policy = policy.capture(&self.services.registry)?;
            root_authority.delegate(
                ids.conversation.to_string(),
                ids.task.to_string(),
                policy.workspace_root.clone(),
                policy.mode,
                policy
                    .allowed_tools
                    .clone()
                    .ok_or(SchedulerError::Invalid("delegated policy was not captured"))?,
            )?;
            self.services
                .agent
                .create_conversation_as(
                    ids.conversation,
                    ids.create,
                    title,
                    configuration.clone(),
                    Some(delegation.to),
                )
                .await?;
            let mut initial = TaskRecord::new(
                ids.conversation,
                Some(caller),
                ids.instruction,
                text.clone(),
                now,
            );
            initial.id = ids.task;
            let mut task = initial_task(initial, configuration, policy);
            task.definition.delegation = Some(delegation);
            repository::insert(&self.store(), &task, self.limits.max_pending_tasks)?;
            self.services
                .agent
                .accept_text(TextCommand {
                    conversation_id: ids.conversation,
                    command_id: ids.instruction,
                    text,
                })
                .await?;
            self.queue(ids.task);
            Ok(repository::load(&self.store(), ids.task)?.snapshot())
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

    async fn wait(
        &mut self,
        caller: TaskId,
        workspace: PathBuf,
        operation: CommandId,
        children: Vec<TaskId>,
        tool: &str,
    ) -> Result<TaskSnapshot, SchedulerError> {
        if children.is_empty()
            || children.len() > 32
            || children
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != children.len()
        {
            return Err(SchedulerError::Invalid(
                "wait requires 1 to 32 distinct descendants",
            ));
        }
        let mut task = self.validate_caller(caller, &workspace)?;
        let accepted_at = self.operation_time(caller, operation, tool)?;
        let wait_id = CommandId::try_from(derived_id(operation, b"wait"))?;
        for child in &children {
            repository::authorize_target(&self.store(), caller, *child, false)?;
        }
        if let Some(existing) = &task.checkpoint.child_wait {
            if existing.wait_id == wait_id {
                if existing.active_command_id != task.checkpoint.active_command
                    || existing
                        .children
                        .iter()
                        .map(|child| child.task_id)
                        .collect::<Vec<_>>()
                        != children
                    || existing.accepted_at != accepted_at
                {
                    return Err(SchedulerError::Conflict(caller));
                }
                if !super::inputs::repair(&self.services.agent, &self.store(), &mut task)
                    .await
                    .map_err(mutation_uncertain)?
                {
                    task.checkpoint.child_wait = None;
                    repository::save(&self.store(), &mut task, TaskStatus::Running)
                        .map_err(mutation_uncertain)?;
                    return Err(SchedulerError::Capacity("native input inbox is full"));
                }
                return Ok(task.snapshot());
            }
            let conversation = self
                .services
                .agent
                .conversation(task.definition.initial.conversation_id)
                .await?;
            let state = conversation.task_input_state();
            if state.wait_id.is_some()
                || state.pending != 0
                || existing
                    .children
                    .iter()
                    .any(|child| child.delivery.is_none())
            {
                return Err(SchedulerError::Capacity(
                    "task already has an unresolved child wait",
                ));
            }
        }
        let previous_wait = task.checkpoint.child_wait.clone();
        task.checkpoint.child_wait = Some(ChildWaitSet {
            wait_id,
            active_command_id: task.checkpoint.active_command,
            accepted_at,
            children: children
                .into_iter()
                .map(|task_id| WaitChild {
                    task_id,
                    delivery: None,
                })
                .collect(),
        });
        repository::encode_checkpoint(&task.checkpoint)?;
        repository::save(&self.store(), &mut task, TaskStatus::Running)
            .map_err(mutation_uncertain)?;
        if !super::inputs::repair(&self.services.agent, &self.store(), &mut task)
            .await
            .map_err(mutation_uncertain)?
        {
            task.checkpoint.child_wait = previous_wait;
            repository::save(&self.store(), &mut task, TaskStatus::Running)
                .map_err(mutation_uncertain)?;
            return Err(SchedulerError::Capacity("native input inbox is full"));
        }
        Ok(task.snapshot())
    }

    async fn repair_child_deliveries(&mut self) -> Result<(), SchedulerError> {
        let tasks = repository::unfinished(&self.store(), self.limits.max_pending_tasks)?;
        for mut task in tasks {
            if task.checkpoint.child_wait.is_some() {
                let id = task.definition.initial.id;
                super::inputs::repair(&self.services.agent, &self.store(), &mut task).await?;
                self.queue_pending_input(id).await?;
            }
        }
        Ok(())
    }

    async fn steer(
        &mut self,
        id: TaskId,
        delivery: CommandId,
        accepted_at: UnixMillis,
        instruction: String,
    ) -> Result<TaskInputReceipt, SchedulerError> {
        if instruction.trim().is_empty() || instruction.len() > 32_768 || instruction.contains('\0')
        {
            return Err(SchedulerError::Invalid(
                "steering requires 1 to 32768 UTF-8 bytes without NUL",
            ));
        }
        let task = repository::load(&self.store(), id)?;
        if task.definition.remote.is_some() {
            return Err(SchedulerError::OwnerDecision(id));
        }
        let command = if let Some(original) = self.store().command_request(&delivery.to_string())? {
            let event = original
                .events
                .first()
                .filter(|_| original.events.len() == 1)
                .ok_or(crate::agent::AgentError::CommandConflict(delivery))?;
            let event: crate::protocol::EventEnvelope =
                serde_json::from_value(event.payload.clone())
                    .map_err(|_| crate::agent::AgentError::CommandConflict(delivery))?;
            match event.payload {
                crate::protocol::ConversationEvent::SteeringReceived {
                    task_id,
                    command_id,
                    instruction: ref saved,
                } if task_id == id && *saved == instruction => command_id,
                _ => return Err(crate::agent::AgentError::CommandConflict(delivery).into()),
            }
        } else {
            if task.checkpoint.task.status.is_terminal() {
                return Err(SchedulerError::Terminal(id));
            }
            if repository::cancelled_ancestor(&self.store(), id)? {
                return Err(SchedulerError::OutsideScope);
            }
            task.checkpoint.active_command
        };
        let receipt = self
            .services
            .agent
            .accept_task_input(TaskInputCommand {
                conversation_id: task.definition.initial.conversation_id,
                task_id: id,
                active_command_id: command,
                delivery_id: delivery,
                accepted_at,
                input: TaskInput::Steering { instruction },
            })
            .await
            .map_err(|error| match error {
                crate::agent::AgentError::Storage(_) | crate::agent::AgentError::Worker => {
                    mutation_uncertain(error.into())
                }
                _ => error.into(),
            })?;
        self.queue_pending_input(id)
            .await
            .map_err(mutation_uncertain)?;
        Ok(receipt)
    }

    async fn queue_pending_input(&mut self, id: TaskId) -> Result<(), SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        if task.checkpoint.task.status.is_terminal()
            || self.active.contains_key(&id)
            || repository::cancelled_ancestor(&self.store(), id)?
            || matches!(
                task.checkpoint.waiting,
                Some(WaitingReason::ProviderUnavailable | WaitingReason::Stopped)
            )
        {
            return Ok(());
        }
        if matches!(
            repository::encode_checkpoint(&task.checkpoint),
            Err(SchedulerError::Capacity("task checkpoint bytes"))
        ) {
            tracing::warn!(task_id=%id, "native input remains durable but legacy scheduler checkpoint capacity prevents admission; owner inspection is required");
            return Ok(());
        }
        let conversation = self
            .services
            .agent
            .conversation(task.definition.initial.conversation_id)
            .await?;
        if conversation.config != task.definition.configuration {
            return Err(SchedulerError::Conflict(id));
        }
        if pending_input_can_run(&conversation, &task) {
            task.checkpoint.waiting = None;
            repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
            self.queue(id);
        }
        Ok(())
    }

    async fn wake(&mut self, id: TaskId) -> Result<TaskSnapshot, SchedulerError> {
        let mut task = repository::load(&self.store(), id)?;
        if task.definition.remote.is_some() {
            return Err(SchedulerError::OwnerDecision(id));
        }
        if task.checkpoint.task.status.is_terminal() {
            return Ok(task.snapshot());
        }
        if self.active.contains_key(&id) {
            self.wake_active.insert(id);
            return Ok(task.snapshot());
        }
        let conversation = self.repair_initial_acceptance(&task, None).await?;
        if native_waiting(&conversation, &task).is_some_and(|reason| owner_decision(&reason)) {
            return Err(SchedulerError::OwnerDecision(id));
        }
        if matches!(
            native_waiting(&conversation, &task),
            Some(WaitingReason::Children { .. })
        ) && !pending_input_can_run(&conversation, &task)
        {
            return Ok(task.snapshot());
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
        if task.definition.remote.is_some() {
            return Err(SchedulerError::OwnerDecision(id));
        }
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
        if task.definition.remote.is_some() {
            if task.checkpoint.task.status.is_terminal() {
                return Ok(task.snapshot());
            }
            let remote = task
                .checkpoint
                .remote
                .as_mut()
                .ok_or(SchedulerError::Invalid("remote checkpoint is missing"))?;
            if remote.phase != remote::RemoteTaskPhase::OutcomeUnknown {
                remote.phase = remote::RemoteTaskPhase::CancellationRequested;
            }
            task.checkpoint.cancel_requested = true;
            task.checkpoint.waiting = Some(WaitingReason::Remote {
                phase: remote.phase,
            });
            task.checkpoint.diagnostic = Some("Remote cancellation requested; execution outcome remains unconfirmed and the original allowance remains charged".into());
            repository::save(&self.store(), &mut task, TaskStatus::WaitingForChildren)
                .map_err(mutation_uncertain)?;
            return Ok(task.snapshot());
        }
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
                self.wake_active.remove(&target);
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
            if task.definition.remote.is_some() {
                if !matches!(
                    remote::snapshot(&task)?.phase,
                    remote::RemoteTaskPhase::Settled | remote::RemoteTaskPhase::Released
                ) {
                    self.remote_reserved.insert(id);
                }
                continue;
            }
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
                            | WaitingReason::ChildOutcomeUnknown { .. }
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
                    self.save_terminal(&mut task, TaskStatus::Failed).await?;
                } else {
                    task.checkpoint.waiting = None;
                    self.save_terminal(&mut task, TaskStatus::Cancelled).await?;
                }
                self.repair_child_deliveries().await?;
                continue;
            }
            if task.checkpoint.task.status.is_terminal() {
                self.repair_child_deliveries().await?;
                continue;
            }
            if self.services.broker.is_quarantined() {
                if task.checkpoint.diagnostic.is_none() {
                    task.checkpoint.diagnostic = Some(BROKER_QUARANTINED.into());
                    let status = task.checkpoint.task.status;
                    repository::save(&self.store(), &mut task, status)?;
                }
                self.queue.push_back(id);
                continue;
            }
            if task.checkpoint.diagnostic.as_deref() == Some(BROKER_QUARANTINED) {
                task.checkpoint.diagnostic = None;
            }
            let child = task.definition.initial.parent_task_id.is_some();
            let running = self
                .active
                .values()
                .filter(|active| active.child == child)
                .count()
                + if child { 0 } else { self.remote_reserved.len() };
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
            if let Some(reason) = native_waiting(&conversation, &task)
                && !pending_input_can_run(&conversation, &task)
                && resume.is_none()
                && !matches!(reason, WaitingReason::Approval | WaitingReason::InFlight)
            {
                progress = true;
                task.checkpoint.waiting = Some(reason);
                save_waiting(&self.store(), &mut task)?;
                continue;
            }
            progress = true;
            task.checkpoint.waiting = None;
            repository::save(&self.store(), &mut task, TaskStatus::Running)?;
            self.approval_deadlines.remove(&id);
            let cancellation = CancellationToken::new();
            let services = Arc::clone(&self.services);
            let preparation = self.preparation.clone();
            let worker_cancellation = cancellation.clone();
            let changed = self.changed.clone();
            let live = self.live.clone();
            let handle = self.workers.spawn(async move {
                worker::execute(
                    services,
                    task,
                    worker_cancellation,
                    resume,
                    changed,
                    live,
                    preparation,
                )
                .await
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

    async fn save_terminal(
        &self,
        task: &mut StoredTask,
        status: TaskStatus,
    ) -> Result<(), SchedulerError> {
        if !status.is_terminal() || task.checkpoint.task.status.is_terminal() {
            return Err(SchedulerError::Conflict(task.definition.initial.id));
        }
        let conversation = self
            .services
            .agent
            .conversation(task.definition.initial.conversation_id)
            .await?;
        task.checkpoint.terminal_evidence = Some(super::evidence::TerminalTaskEvidence::capture(
            &task.definition,
            task.checkpoint.active_command,
            &conversation,
        )?);
        repository::save(&self.store(), task, status)
    }

    async fn finish(&mut self, outcome: ExecutionOutcome) -> Result<(), SchedulerError> {
        let wake_active = self.wake_active.remove(&outcome.task_id);
        let mut task = repository::load(&self.store(), outcome.task_id)?;
        super::inputs::retire_consumed(&self.services.agent, &self.store(), &mut task).await?;
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
                    task.checkpoint.child_wait = None;
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
                    self.save_terminal(&mut task, TaskStatus::Cancelled).await?;
                } else {
                    self.save_terminal(&mut task, TaskStatus::Completed).await?;
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
                        self.save_terminal(&mut task, TaskStatus::Failed).await?;
                    } else {
                        task.checkpoint.waiting = None;
                        self.save_terminal(&mut task, TaskStatus::Cancelled).await?;
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
                                    | WaitingReason::ChildOutcomeUnknown { .. }
                            );
                        if journal_unknown {
                            task.checkpoint.waiting = Some(WaitingReason::Reconciliation);
                        }
                        task.checkpoint.diagnostic=Some(if unknown{"Task execution cancelled; its previous native outcome remains uncertain and is not retryable"}else{"Task execution and queued follow-up commands cancelled"}.into());
                        self.save_terminal(
                            &mut task,
                            if unknown {
                                TaskStatus::Failed
                            } else {
                                TaskStatus::Cancelled
                            },
                        )
                        .await?;
                        self.repair_child_deliveries().await?;
                        return Ok(());
                    }
                }
                if wake_active
                    && !self.stopping
                    && !task.checkpoint.cancel_requested
                    && matches!(reason, WaitingReason::Approval | WaitingReason::InFlight)
                {
                    let conversation = self
                        .services
                        .agent
                        .conversation(task.definition.initial.conversation_id)
                        .await?;
                    if conversation.config != task.definition.configuration {
                        return Err(SchedulerError::Conflict(outcome.task_id));
                    }
                    if matches!(
                        native_waiting(&conversation, &task),
                        Some(WaitingReason::Approval | WaitingReason::InFlight)
                    ) {
                        task.checkpoint.waiting = None;
                        task.checkpoint.diagnostic = diagnostic;
                        repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
                        self.queue(outcome.task_id);
                        self.repair_child_deliveries().await?;
                        return Ok(());
                    }
                }
                if matches!(reason, WaitingReason::Children { .. })
                    && !self.stopping
                    && !task.checkpoint.cancel_requested
                {
                    let conversation = self
                        .services
                        .agent
                        .conversation(task.definition.initial.conversation_id)
                        .await?;
                    if conversation.config != task.definition.configuration {
                        return Err(SchedulerError::Conflict(outcome.task_id));
                    }
                    if pending_input_can_run(&conversation, &task) {
                        task.checkpoint.waiting = None;
                        task.checkpoint.diagnostic = diagnostic;
                        repository::save(&self.store(), &mut task, TaskStatus::Queued)?;
                        self.queue(outcome.task_id);
                        self.repair_child_deliveries().await?;
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
                if permission
                    && !self.stopping
                    && let Err(error) = self.track_approval(&task).await
                {
                    tracing::warn!(task_id=%outcome.task_id, error=%error, "approval expiry is not tracked; owner wake remains available");
                }
            }
            ExecutionStop::Failed { diagnostic } => {
                task.checkpoint.diagnostic = Some(diagnostic);
                self.save_terminal(&mut task, TaskStatus::Failed).await?;
            }
        }
        self.repair_child_deliveries().await?;
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
        task.checkpoint.child_wait = None;
        task.checkpoint.queued.remove(0);
        task.checkpoint.waiting = None;
        repository::save(store, task, TaskStatus::Queued)?;
    }
    Ok(())
}

struct MessageIds {
    task: TaskId,
    conversation: ConversationId,
    create: CommandId,
    instruction: CommandId,
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
            remote: None,
            delegation: None,
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
            child_wait: None,
            provider_retirement: None,
            terminal_evidence: None,
            remote: None,
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

fn owner_decision(reason: &WaitingReason) -> bool {
    matches!(
        reason,
        WaitingReason::ProviderUncertain { .. }
            | WaitingReason::Limit
            | WaitingReason::Reconciliation
            | WaitingReason::ChildOutcomeUnknown { .. }
            | WaitingReason::Interrupted
            | WaitingReason::NativePaused { .. }
    )
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
            PauseReason::WaitingForEvents { wait_id } => WaitingReason::Children { wait_id },
            PauseReason::ChildOutcomeUnknown { child_task_id } => {
                WaitingReason::ChildOutcomeUnknown { child_task_id }
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

fn pending_input_can_run(conversation: &Conversation, task: &StoredTask) -> bool {
    conversation.task_input_state().pending > 0
        && conversation.info.pending_command_id == Some(task.checkpoint.active_command)
        && conversation.native_run().is_none_or(|run| {
            (run.status == LoopStatus::Completed
                && run.command_id != task.checkpoint.active_command)
                || (run.task_id == task.definition.initial.id
                    && run.command_id == task.checkpoint.active_command
                    && matches!(
                        run.status,
                        LoopStatus::Ready
                            | LoopStatus::Paused {
                                reason: PauseReason::WaitingForEvents { .. }
                            }
                    ))
        })
}
