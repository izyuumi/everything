use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{
        Agent, ConversationConfig, LoopStatus, PauseReason, ProviderEndpoint, ProviderKind,
        TextCommand,
    },
    credentials::NativeCredentialStore,
    model::{
        ModelProvider,
        anthropic::{AnthropicConfig, AnthropicProvider},
        openai::{OpenAiConfig, OpenAiProvider},
    },
    permissions::{Authorizer, ToolScopeRule},
    protocol::{TaskId, TaskStatus, UnixMillis},
    scheduler::{
        DiscoveryServices, ExecutionPolicy, Scheduler, SchedulerError, SchedulerLimits,
        SchedulerServices, TaskProviderFactory, TaskSnapshot, TaskSubmission, WaitingReason,
    },
    selector::{SEARCH_TOOLS_NAME, SearchTools, ToolSelector},
    tools::{
        ToolRegistry,
        broker::{BrokerLimits, RecoveryReport, ToolBroker},
        builtin::{BuiltinConfig, BuiltinTools},
    },
};

use super::{
    DelegationSnapshot, DelegationState, WorkerEndpoint, WorkerError, WorkerManifest,
    WorkerMessage, WorkerModelBinding, WorkerRejection, WorkerResult, WorkerResultStatus,
    WorkerRole, WorkerUsage, files,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerRuntimePhase {
    Starting,
    Running,
    Draining,
    NeedsOwner,
    Stopped,
}

pub struct WorkerRuntime {
    core: Arc<RuntimeCore>,
    cancellation: CancellationToken,
    phase: watch::Receiver<WorkerRuntimePhase>,
    failure: watch::Receiver<Option<Arc<WorkerError>>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    join_failed: bool,
}

struct RuntimeCore {
    endpoint: WorkerEndpoint,
    scheduler: Scheduler,
    broker: ToolBroker,
    providers: WorkerProviderFactory,
    agent: Agent,
}

#[derive(Clone)]
struct WorkerProviderFactory {
    profile: WorkerModelBinding,
    executor: crate::protocol::DeviceId,
    credentials: NativeCredentialStore,
}

impl WorkerRuntime {
    pub async fn start(
        endpoint: WorkerEndpoint,
        profile: WorkerModelBinding,
    ) -> Result<Self, WorkerError> {
        if endpoint.role() != WorkerRole::Executor {
            return Err(WorkerError::WrongRole);
        }
        profile.validate()?;
        let bound_profile = profile.clone();
        endpoint
            .blocking(move |repository| {
                repository.validate_executor_state()?;
                repository.bind_profile(&bound_profile)?;
                repository.verify_owned_native_tasks()?;
                Ok(())
            })
            .await?;
        let tools_agent = endpoint.agent.clone();
        let (registry, authorizer, discovery, _tools_owner) =
            tokio::task::spawn_blocking(move || {
                let retained_tools_agent = tools_agent;
                let mut registry = ToolRegistry::new();
                let mut scopes = BuiltinTools::new(BuiltinConfig::default())?
                    .register_workspace_into(&mut registry)?;
                let catalog_authorizer = Arc::new(Authorizer::new(
                    retained_tools_agent.store(),
                    Arc::new(registry.clone()),
                    scopes.clone(),
                )?);
                let selector = ToolSelector::new(&registry, catalog_authorizer)?;
                let search = Arc::new(SearchTools::new(selector.clone()));
                registry.register(search.clone())?;
                scopes.insert(SEARCH_TOOLS_NAME.into(), ToolScopeRule::LocalReadOnly);
                let registry = Arc::new(registry);
                let authorizer = Arc::new(Authorizer::new(
                    retained_tools_agent.store(),
                    registry.clone(),
                    scopes,
                )?);
                Ok::<_, WorkerError>((
                    registry,
                    authorizer,
                    DiscoveryServices { selector, search },
                    retained_tools_agent,
                ))
            })
            .await
            .map_err(|_| WorkerError::Worker)??;
        let broker = ToolBroker::start_with_owner(
            endpoint.agent.store(),
            registry.clone(),
            authorizer,
            BrokerLimits {
                max_concurrent: 4,
                max_in_flight: 8,
                queue_timeout: Duration::from_secs(10),
                execution_timeout: Duration::from_secs(60),
                cancellation_grace: Duration::from_secs(5),
            },
            endpoint.agent.clone(),
        )
        .await?;
        let providers = WorkerProviderFactory {
            profile,
            executor: endpoint.public_keys().device_id,
            credentials: NativeCredentialStore::new(),
        };
        let scheduler = Scheduler::start(
            SchedulerServices {
                agent: endpoint.agent.clone(),
                registry,
                broker: broker.clone(),
                providers: Arc::new(providers.clone()),
                discovery: Some(discovery),
            },
            SchedulerLimits {
                max_primary_tasks: 1,
                max_pending_tasks: 1,
                ..SchedulerLimits::default()
            },
        )
        .await?;
        let core = Arc::new(RuntimeCore {
            agent: endpoint.agent.clone(),
            endpoint,
            scheduler,
            broker,
            providers,
        });
        let cancellation = CancellationToken::new();
        let (updates, phase) = watch::channel(WorkerRuntimePhase::Starting);
        let (failures, failure) = watch::channel(None);
        let owned_core = Arc::clone(&core);
        let owned_cancellation = cancellation.clone();
        let supervisor = tokio::spawn(async move {
            supervise(owned_core, owned_cancellation, updates, failures).await
        });
        Ok(Self {
            core,
            cancellation,
            phase,
            failure,
            supervisor: Some(supervisor),
            join_failed: false,
        })
    }

    pub fn phase(&self) -> WorkerRuntimePhase {
        *self.phase.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<WorkerRuntimePhase> {
        self.phase.clone()
    }

    pub fn failure(&self) -> Option<Arc<WorkerError>> {
        self.failure.borrow().clone()
    }

    pub async fn native_status(&self, delegation: uuid::Uuid) -> Result<TaskSnapshot, WorkerError> {
        let job = self
            .core
            .endpoint
            .snapshot(delegation)
            .await?
            .ok_or(WorkerError::Conflict("unknown worker job"))?;
        let task = job
            .manifest
            .ok_or(WorkerError::Conflict("job has no manifest"))?
            .ids
            .task_id;
        let agent = self.core.agent.clone();
        tokio::task::spawn_blocking(move || {
            let _lease = agent.clone();
            crate::scheduler::task_snapshot(&agent.store(), task)
        })
        .await
        .map_err(|_| WorkerError::Worker)?
        .map_err(WorkerError::from)
    }

    pub async fn cleanup_report(&self) -> Result<RecoveryReport, WorkerError> {
        Ok(self.core.broker.recovery_report().await?)
    }

    pub async fn acknowledge_cleanup(
        &self,
        operation_id: &str,
        result_digest: &str,
        operator: &str,
        note: &str,
    ) -> Result<RecoveryReport, WorkerError> {
        Ok(self
            .core
            .broker
            .acknowledge_cleanup(operation_id, result_digest, operator, note)
            .await?)
    }

    pub async fn shutdown(&mut self) -> Result<(), WorkerError> {
        self.request_shutdown();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        loop {
            if self.phase() == WorkerRuntimePhase::Stopped {
                return tokio::time::timeout_at(deadline, self.join())
                    .await
                    .map_err(|_| WorkerError::ShutdownPending)?;
            }
            match tokio::time::timeout_at(deadline, self.phase.changed()).await {
                Ok(Ok(())) => {}
                _ => return Err(WorkerError::ShutdownPending),
            }
        }
    }

    pub fn request_shutdown(&self) {
        self.cancellation.cancel();
    }

    pub async fn join(&mut self) -> Result<(), WorkerError> {
        self.request_shutdown();
        if let Some(supervisor) = self.supervisor.as_mut() {
            let result = supervisor.await;
            self.supervisor = None;
            self.join_failed = result.is_err() || self.phase() != WorkerRuntimePhase::Stopped;
        }
        if self.join_failed {
            Err(WorkerError::Worker)
        } else if self.phase() == WorkerRuntimePhase::Stopped {
            Ok(())
        } else {
            Err(WorkerError::ShutdownPending)
        }
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

async fn supervise(
    core: Arc<RuntimeCore>,
    cancellation: CancellationToken,
    phase: watch::Sender<WorkerRuntimePhase>,
    failures: watch::Sender<Option<Arc<WorkerError>>>,
) {
    let startup = async {
        reconcile_startup(&core).await?;
        core.scheduler.activate().await?;
        Ok::<_, WorkerError>(())
    }
    .await;
    if startup.is_ok() {
        phase.send_replace(WorkerRuntimePhase::Running);
        let mut changes = core.scheduler.subscribe();
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !cancellation.is_cancelled() {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                changed = changes.changed() => if changed.is_err() {
                    failures.send_replace(Some(Arc::new(WorkerError::Scheduler(SchedulerError::Stopped))));
                    break;
                },
                _ = core.endpoint.changed.notified() => {}
                _ = interval.tick() => {}
            }
            if cancellation.is_cancelled() {
                break;
            }
            match reconcile(&core).await {
                Ok(()) => {
                    phase.send_replace(WorkerRuntimePhase::Running);
                }
                Err(error) => {
                    failures.send_replace(Some(Arc::new(error)));
                    phase.send_replace(WorkerRuntimePhase::NeedsOwner);
                    break;
                }
            }
        }
    } else {
        if let Err(error) = startup {
            failures.send_replace(Some(Arc::new(error)));
        }
        phase.send_replace(WorkerRuntimePhase::NeedsOwner);
        cancellation.cancelled().await;
    }
    phase.send_replace(WorkerRuntimePhase::Draining);
    loop {
        match core.scheduler.shutdown().await {
            Ok(report) if report.active_tasks == 0 => {
                match core.broker.recover_after_drain().await {
                    Ok(cleanup) if cleanup.running_count == 0 && cleanup.unresolved_count == 0 => {
                        phase.send_replace(WorkerRuntimePhase::Stopped);
                        return;
                    }
                    Ok(_) => {
                        phase.send_replace(WorkerRuntimePhase::NeedsOwner);
                    }
                    Err(error) => {
                        failures.send_if_modified(|saved| {
                            if saved.is_none() {
                                *saved = Some(Arc::new(error.into()));
                                true
                            } else {
                                false
                            }
                        });
                        phase.send_replace(WorkerRuntimePhase::NeedsOwner);
                    }
                }
            }
            Ok(_) => {
                phase.send_replace(WorkerRuntimePhase::Draining);
            }
            Err(error) => {
                failures.send_if_modified(|saved| {
                    if saved.is_none() {
                        *saved = Some(Arc::new(error.into()));
                        true
                    } else {
                        false
                    }
                });
                phase.send_replace(WorkerRuntimePhase::Draining);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn reconcile_startup(core: &RuntimeCore) -> Result<(), WorkerError> {
    for job in core.endpoint.jobs().await? {
        let Some(manifest) = job.manifest.as_ref() else {
            continue;
        };
        if native_exists(core, manifest.ids.task_id).await? {
            let owned_manifest = manifest.clone();
            let original = core
                .endpoint
                .blocking(move |repository| repository.native_submission(&owned_manifest))
                .await?
                .ok_or(WorkerError::Conflict(
                    "native task lacks its original worker admission",
                ))?;
            let agent = core.agent.clone();
            let files_manifest = manifest.clone();
            let workspace = tokio::task::spawn_blocking(move || {
                files::prepare_workspace(&agent, &files_manifest)
            })
            .await
            .map_err(|_| WorkerError::Worker)??;
            if serde_json::to_vec(&original)?
                != serde_json::to_vec(&submission(manifest, workspace))?
            {
                return Err(WorkerError::Conflict(
                    "native recovery workspace or execution policy",
                ));
            }
            core.scheduler.enqueue(original).await?;
            let unsafe_dispatch = job.state.is_terminal()
                || job.result.is_some()
                || job.cancellation_requested
                || manifest.model != core.providers.profile
                || manifest.deadline.get() <= UnixMillis::now()?.get()
                || match core
                    .endpoint
                    .blocking({
                        let peer = job.peer;
                        move |repository| repository.peer(peer)
                    })
                    .await
                {
                    Ok(_) => false,
                    Err(WorkerError::UntrustedPeer) => true,
                    Err(error) => return Err(error),
                };
            if unsafe_dispatch {
                core.scheduler.cancel(manifest.ids.task_id).await?;
            }
        }
    }
    Ok(())
}

async fn reconcile(core: &RuntimeCore) -> Result<(), WorkerError> {
    let jobs = core.endpoint.jobs().await?;
    let mut busy = false;
    for job in &jobs {
        if !job.cancellation_acknowledged
            && !job.state.is_terminal()
            && let Some(request_id) = job.cancellation_command_id
        {
            core.endpoint
                .send_executor(
                    WorkerMessage::CancellationAccepted {
                        binding: job.binding.clone(),
                        request_id,
                    },
                    UnixMillis::now()?,
                )
                .await?;
        }
        let Some(manifest) = job.manifest.as_ref() else {
            continue;
        };
        if !native_exists(core, manifest.ids.task_id).await? {
            continue;
        }
        let mut native = core.scheduler.status(manifest.ids.task_id).await?;
        let revoked = match core
            .endpoint
            .blocking({
                let peer = job.peer;
                move |repository| repository.peer(peer)
            })
            .await
        {
            Ok(_) => false,
            Err(WorkerError::UntrustedPeer) => true,
            Err(error) => return Err(error),
        };
        if !native.task.status.is_terminal()
            && (job.cancellation_requested
                || revoked
                || manifest.deadline.get() <= UnixMillis::now()?.get())
        {
            native = core.scheduler.cancel(manifest.ids.task_id).await?;
        }
        if !native.task.status.is_terminal() {
            busy = true;
        }
        if job.result.is_none()
            && job.rejection.is_none()
            && !revoked
            && let Some(status) = result_status(&native)
        {
            publish_result(core, job, &native, status).await?;
        }
    }
    if busy || core.broker.is_quarantined() {
        return Ok(());
    }
    for job in jobs {
        let Some(manifest) = job.manifest.as_ref() else {
            continue;
        };
        if job.state.is_terminal()
            || job.result.is_some()
            || native_exists(core, manifest.ids.task_id).await?
        {
            continue;
        }
        if job.cancellation_requested {
            core.endpoint
                .send_executor(
                    WorkerMessage::Rejected {
                        binding: job.binding,
                        reason: WorkerRejection::CancelledBeforeAcceptance,
                    },
                    UnixMillis::now()?,
                )
                .await?;
            continue;
        }
        if manifest.deadline.get() <= UnixMillis::now()?.get() {
            reject(core, &job, WorkerRejection::Expired).await?;
            continue;
        }
        if manifest.model != core.providers.profile {
            reject(core, &job, WorkerRejection::Configuration).await?;
            continue;
        }
        if !core.providers.credential_available().await? {
            reject(core, &job, WorkerRejection::CredentialUnavailable).await?;
            continue;
        }
        let staged_agent = core.agent.clone();
        let staged_manifest = manifest.clone();
        let workspace = tokio::task::spawn_blocking(move || {
            files::prepare_workspace(&staged_agent, &staged_manifest)
        })
        .await
        .map_err(|_| WorkerError::Worker)??;
        let _authority = Arc::clone(&core.endpoint.authority).lock_owned().await;
        let current = core.endpoint.require_job(&job.binding).await?;
        if current.cancellation_requested || current.state.is_terminal() {
            continue;
        }
        let peer = current.peer;
        core.endpoint
            .blocking(move |repository| repository.peer(peer))
            .await?;
        if manifest.deadline.get() <= UnixMillis::now()?.get() {
            return Err(WorkerError::Expired);
        }
        if current.state == DelegationState::Offered {
            core.endpoint
                .queue_locked(
                    current.peer,
                    WorkerMessage::Accepted {
                        binding: job.binding.clone(),
                    },
                    UnixMillis::now()?,
                )
                .await?;
        }
        let binding = job.binding.clone();
        core.endpoint
            .mutate(move |repository| repository.mark_running(&binding, UnixMillis::now()?))
            .await?;
        let submission = submission(manifest, workspace);
        let admitted_manifest = manifest.clone();
        let admitted_submission = submission.clone();
        core.endpoint
            .mutate(move |repository| {
                repository.reserve_native(&admitted_manifest, &admitted_submission)
            })
            .await?;
        core.agent
            .create_conversation(
                manifest.ids.conversation_id,
                manifest.ids.create_command_id,
                format!("Worker {}", manifest.delegation_id),
                manifest.model.configuration.clone(),
            )
            .await?;
        core.agent
            .accept_text(TextCommand {
                conversation_id: manifest.ids.conversation_id,
                command_id: manifest.ids.input_command_id,
                text: manifest.instruction()?,
            })
            .await?;
        core.scheduler.enqueue(submission).await?;
        break;
    }
    Ok(())
}

async fn reject(
    core: &RuntimeCore,
    job: &DelegationSnapshot,
    reason: WorkerRejection,
) -> Result<(), WorkerError> {
    core.endpoint
        .send_executor(
            WorkerMessage::Rejected {
                binding: job.binding.clone(),
                reason,
            },
            UnixMillis::now()?,
        )
        .await
}

async fn native_exists(core: &RuntimeCore, task: TaskId) -> Result<bool, WorkerError> {
    let agent = core.agent.clone();
    tokio::task::spawn_blocking(move || {
        let _lease = agent.clone();
        Ok(agent.store().task(&task.to_string())?.is_some())
    })
    .await
    .map_err(|_| WorkerError::Worker)?
}

fn submission(manifest: &WorkerManifest, workspace: PathBuf) -> TaskSubmission {
    let mut limits = manifest.limits.clone();
    if manifest.allowed_tools.is_empty() {
        limits.max_tool_calls = 0;
    }
    TaskSubmission {
        task_id: manifest.ids.task_id,
        conversation_id: manifest.ids.conversation_id,
        command_id: manifest.ids.input_command_id,
        accepted_at: manifest.created_at,
        execution: ExecutionPolicy {
            workspace_root: workspace,
            mode: manifest.mode,
            tools_enabled: !manifest.allowed_tools.is_empty(),
            allowed_tools: Some(manifest.allowed_tools.clone()),
            budget: manifest.budget.clone(),
            limits,
        },
    }
}

fn result_status(native: &TaskSnapshot) -> Option<WorkerResultStatus> {
    if matches!(
        native.waiting,
        Some(
            WaitingReason::ProviderUncertain { .. }
                | WaitingReason::Reconciliation
                | WaitingReason::ChildOutcomeUnknown { .. }
                | WaitingReason::InFlight
        )
    ) {
        return Some(WorkerResultStatus::Unknown);
    }
    match native.task.status {
        TaskStatus::Completed => Some(WorkerResultStatus::Succeeded),
        TaskStatus::Failed => Some(WorkerResultStatus::Failed),
        TaskStatus::Cancelled => Some(WorkerResultStatus::Cancelled),
        _ if matches!(
            native.waiting,
            Some(
                WaitingReason::ProviderUncertain { .. }
                    | WaitingReason::Reconciliation
                    | WaitingReason::ChildOutcomeUnknown { .. }
                    | WaitingReason::Interrupted
                    | WaitingReason::InFlight
                    | WaitingReason::ProviderUnavailable
                    | WaitingReason::Limit
                    | WaitingReason::NativePaused { .. }
                    | WaitingReason::Approval
            )
        ) =>
        {
            Some(WorkerResultStatus::Unknown)
        }
        _ => None,
    }
}

async fn publish_result(
    core: &RuntimeCore,
    job: &DelegationSnapshot,
    native: &TaskSnapshot,
    mut status: WorkerResultStatus,
) -> Result<(), WorkerError> {
    let manifest = job
        .manifest
        .as_ref()
        .ok_or(WorkerError::Conflict("native job has no manifest"))?;
    let cleanup = core.broker.recovery_report().await?;
    if cleanup.running_count != 0 {
        return Ok(());
    }
    if cleanup.unresolved_count != 0 {
        status = WorkerResultStatus::Unknown;
    }
    let conversation = core
        .agent
        .conversation(manifest.ids.conversation_id)
        .await?;
    if let Some(run) = conversation.native_run() {
        if run.task_id != manifest.ids.task_id || run.command_id != manifest.ids.input_command_id {
            return Err(WorkerError::Conflict("worker native result identity"));
        }
        if matches!(
            run.status,
            LoopStatus::ProviderInFlight { .. }
                | LoopStatus::ExecutingTools
                | LoopStatus::Paused {
                    reason: PauseReason::ProviderOutcomeUncertain
                        | PauseReason::InterruptedTool
                        | PauseReason::OperationInFlight { .. }
                        | PauseReason::ReconciliationRequired { .. }
                        | PauseReason::ChildOutcomeUnknown { .. }
                }
        ) {
            status = WorkerResultStatus::Unknown;
        }
    }
    let task = manifest.ids.task_id;
    if core
        .endpoint
        .blocking(move |repository| repository.native_uncertain(task))
        .await?
    {
        status = WorkerResultStatus::Unknown;
    }
    let usage = conversation
        .native_run()
        .as_ref()
        .map(WorkerUsage::from)
        .unwrap_or_default();
    let mut summary = native.last_output.as_deref().unwrap_or(match status {
        WorkerResultStatus::Succeeded => "Native worker completed.", WorkerResultStatus::Failed => "Native worker failed.",
        WorkerResultStatus::Cancelled => "Native worker cancellation completed.", WorkerResultStatus::Unknown => "Native worker requires an explicit owner decision; no automatic retry or budget refund.",
    }).replace('\0', "\u{fffd}");
    let artifacts = if status == WorkerResultStatus::Unknown {
        Vec::new()
    } else {
        let agent = core.agent.clone();
        let manifest = manifest.clone();
        match tokio::task::spawn_blocking(move || files::collect_artifacts(&agent, &manifest))
            .await
            .map_err(|_| WorkerError::Worker)?
        {
            Ok(artifacts) => artifacts,
            Err(_) => {
                status = WorkerResultStatus::Failed;
                summary = "Native execution settled, but output artifacts failed strict export validation.".into();
                Vec::new()
            }
        }
    };
    while summary.len() > 8192 {
        summary.pop();
    }
    let result = WorkerResult {
        delegation_id: manifest.delegation_id,
        attempt_id: manifest.attempt_id,
        manifest_digest: manifest.fingerprint()?,
        status,
        summary,
        artifacts,
        usage,
        finished_at: UnixMillis::now()?,
    };
    result.validate(manifest)?;
    core.endpoint
        .send_executor(
            WorkerMessage::Result {
                result: Box::new(result),
            },
            UnixMillis::now()?,
        )
        .await
}

impl WorkerProviderFactory {
    async fn credential_available(&self) -> Result<bool, WorkerError> {
        match self
            .credentials
            .read(
                self.profile.credential_key(self.executor)?,
                CancellationToken::new(),
            )
            .await
        {
            Ok(secret) => Ok(secret.is_some()),
            Err(
                crate::credentials::CredentialError::Unsupported
                | crate::credentials::CredentialError::Unavailable,
            ) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn provider(
        &self,
        configuration: &ConversationConfig,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn ModelProvider>, WorkerError> {
        if configuration != &self.profile.configuration {
            return Err(WorkerError::Conflict("native worker provider profile"));
        }
        let secret = self
            .credentials
            .read(
                self.profile.credential_key(self.executor)?,
                cancellation.clone(),
            )
            .await?
            .ok_or(WorkerError::Invalid("worker native credential is absent"))?;
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let secret = std::str::from_utf8(secret.expose())
            .map_err(|_| crate::credentials::CredentialError::InvalidSecret)?;
        match configuration.provider {
            ProviderKind::Anthropic => {
                let mut configuration_provider = AnthropicConfig::new(secret.to_owned())?
                    .with_default_max_output_tokens(configuration.max_output_tokens)?;
                if let ProviderEndpoint::Loopback(endpoint) = &configuration.endpoint {
                    configuration_provider =
                        configuration_provider.with_loopback_endpoint(endpoint)?;
                }
                Ok(Arc::new(AnthropicProvider::with_config(
                    configuration_provider,
                )?))
            }
            ProviderKind::OpenAi => {
                let mut configuration_provider = OpenAiConfig::new(secret)?;
                if let ProviderEndpoint::Loopback(endpoint) = &configuration.endpoint {
                    configuration_provider =
                        configuration_provider.with_loopback_endpoint(endpoint)?;
                }
                Ok(Arc::new(OpenAiProvider::with_config(
                    configuration_provider,
                )?))
            }
        }
    }
}

#[async_trait]
impl TaskProviderFactory for WorkerProviderFactory {
    async fn create(
        &self,
        configuration: &ConversationConfig,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn ModelProvider>, SchedulerError> {
        self.provider(configuration, cancellation)
            .await
            .map_err(|_| {
                SchedulerError::Provider("worker-owned native provider is unavailable".into())
            })
    }
}
