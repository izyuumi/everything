use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentError, ConversationConfig};
use crate::devices::{DeviceEndpoint, DeviceError, EndpointRole};
use crate::ipc::CommandReceipt;
use crate::permissions::{Authorizer, ToolScopeRule};
use crate::protocol::{ProtocolError, UnixMillis};
use crate::scheduler::{
    DiscoveryServices, Scheduler, SchedulerError, SchedulerLimits, SchedulerServices,
    SchedulerToolBinding, SchedulerTools, TaskProviderFactory,
};
use crate::scheduling::controls::{ControlError, ScheduleControls};
use crate::scheduling::tools::{ScheduleToolBinding, ScheduleTools};
use crate::selector::{SEARCH_TOOLS_NAME, SearchTools, SelectorError, ToolSelector};
use crate::service::{ServiceError, StateLease};
use crate::storage::{StorageError, Store};
use crate::tools::broker::{BrokerError, BrokerLimits, ToolBroker};
use crate::tools::builtin::{BuiltinConfig, BuiltinTools};
use crate::tools::{ToolError, ToolRegistry};
use crate::transport::local::{
    LocalCall, LocalError, LocalFailure, LocalFailureCode, LocalOptions, LocalRequestPayload,
    LocalResponsePayload, LocalServer,
};

use super::credentials::CredentialVault;
use super::inbox::{
    CommandOrigin, CommandPhase, HostCommandRecord, HostInbox, InboxError, InboxInput,
};
use super::providers::HostProviderFactory;
use super::settings::service::{SettingsService, SettingsServiceError};
use super::settings::{SettingsError, SettingsRepository};

#[derive(Debug, Error)]
pub enum HostError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Local(#[from] LocalError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    SettingsService(#[from] SettingsServiceError),
    #[error(transparent)]
    Inbox(#[from] InboxError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    ScheduleEngine(#[from] crate::scheduling::engine::EngineError),
    #[error(transparent)]
    ScheduleControl(#[from] ControlError),
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    Selector(#[from] SelectorError),
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Permission(#[from] crate::permissions::PermissionError),
    #[error(transparent)]
    Memory(#[from] super::memory::MemoryError),
    #[error(transparent)]
    Consolidation(#[from] crate::memory::consolidate::ConsolidationError),
    #[error(transparent)]
    Mcp(#[from] super::mcp::McpServiceError),
    #[error(transparent)]
    Selection(#[from] super::selection::SelectionPreflightError),
    #[error(transparent)]
    Credential(#[from] crate::credentials::CredentialError),
    #[error(transparent)]
    McpSessionBootstrap(#[from] super::credentials::McpSessionBootstrapError),
    #[error(transparent)]
    WorkerProtocol(#[from] crate::worker::WorkerError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid host operation: {0}")]
    Invalid(&'static str),
    #[error(
        "legacy foreground task requires stopping the host and explicitly resuming or settling it through the foreground CLI"
    )]
    LegacyForeground,
    #[error(
        "host is draining; only inspection and explicit tool cleanup controls remain available"
    )]
    Draining,
    #[error(
        "worker initialization has no native reservation; expired approval cannot be reconstructed"
    )]
    WorkerUnadmitted,
    #[error("worker initialization recovery awaits the configured coordinator")]
    WorkerRecoveryUnavailable,
    #[error("host worker failed")]
    Worker,
}

#[async_trait::async_trait]
impl TaskProviderFactory for HostProviderFactory {
    async fn create(
        &self,
        configuration: &ConversationConfig,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn crate::model::ModelProvider>, SchedulerError> {
        HostProviderFactory::create(self, configuration, cancellation)
            .await
            .map_err(|error| SchedulerError::Provider(error.to_string()))
    }
}

pub struct HostService {
    inner: Arc<HostInner>,
    server: LocalServer,
    calls: mpsc::Receiver<LocalCall>,
    settings_supervisor: super::settings::agent::SettingsAgentSupervisor,
    _scheduler_binding: SchedulerToolBinding,
    _schedule_binding: ScheduleToolBinding,
}

pub(super) struct HostInner {
    pub(super) agent: Agent,
    pub(super) inbox: HostInbox,
    pub(super) settings: SettingsRepository,
    pub(super) settings_service: SettingsService,
    pub(super) settings_agents: Arc<super::settings::agent::SettingsAgentRuntime>,
    pub(super) credentials: CredentialVault,
    pub(super) devices: DeviceEndpoint,
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) authorizer: Arc<Authorizer>,
    pub(super) scheduler: Scheduler,
    pub(super) schedule_engine: crate::scheduling::engine::ScheduleEngine,
    pub(super) schedule_controls: Arc<ScheduleControls>,
    pub(super) broker: ToolBroker,
    pub(super) composition: Arc<super::composition::Composition>,
    pub(super) changed: Notify,
    pub(super) mutation: Arc<Mutex<()>>,
    pub(super) relay_origin: Option<crate::relay_protocol::RelayOrigin>,
    pub(super) relay_status: watch::Receiver<crate::devices::RelaySessionStatus>,
    pub(super) relay_updates: watch::Sender<crate::devices::RelaySessionStatus>,
    pub(super) receipts: watch::Sender<u64>,
    pub(super) metadata_changed: watch::Sender<u64>,
    pub(super) publication: watch::Receiver<super::publication::PublicationStatus>,
    pub(super) publication_updates: watch::Sender<super::publication::PublicationStatus>,
    pub(super) draining: AtomicBool,
    pub(super) workers_drained: AtomicBool,
    pub(super) settings_work_drained: AtomicBool,
    pub(super) recovery: Mutex<()>,
    pub(super) worker: std::sync::OnceLock<Arc<super::worker::service::WorkerCoordinator>>,
}

enum HostWorker {
    Local(Result<(), HostError>),
    Dispatcher(Result<(), HostError>),
    Publication(Result<(), HostError>),
    Settings(Result<(), HostError>),
    SettingsAgent(Result<(), HostError>),
    Relay(Result<(), DeviceError>),
    Coordinator(Result<(), crate::worker::WorkerError>),
}

impl HostService {
    pub async fn start(state_directory: impl AsRef<Path>) -> Result<Self, HostError> {
        Self::start_with_mcp_session_bootstrap(state_directory, None, CancellationToken::new())
            .await
    }

    pub async fn start_with_mcp_session_bootstrap(
        state_directory: impl AsRef<Path>,
        binding_file: Option<PathBuf>,
        cancellation: CancellationToken,
    ) -> Result<Self, HostError> {
        LocalServer::validate_state_path(state_directory.as_ref())?;
        let lease = StateLease::acquire(state_directory).await?;
        let prepared_lease = Arc::clone(&lease);
        let (
            agent,
            settings,
            devices,
            registry,
            mut scopes,
            task_tools,
            schedule_tools,
            _preparation_lease,
        ) = tokio::task::spawn_blocking(move || {
            let preparation_lease = Arc::clone(&prepared_lease);
            let database = prepared_lease.validate_database()?;
            let store =
                Store::open_validated(prepared_lease.path().join("everything.sqlite3"), database)?;
            let directory =
                cap_std::fs::Dir::from_std_file(prepared_lease.directory.file.try_clone()?);
            let identity = crate::devices::load_or_create_identity_in(&directory)?;
            let devices = DeviceEndpoint::new(store.clone(), identity, EndpointRole::Host)?;
            let settings = SettingsRepository::open(Arc::clone(&prepared_lease))?;
            let agent = Agent::from_leased_store(store, prepared_lease);
            let mut registry = ToolRegistry::new();
            let mut scopes =
                BuiltinTools::new(BuiltinConfig::default())?.register_into(&mut registry)?;
            let task_tools = SchedulerTools::new();
            scopes.extend(task_tools.register_into(&mut registry)?);
            let schedule_tools = ScheduleTools::new();
            scopes.extend(schedule_tools.register_into(&mut registry)?);
            Ok::<_, HostError>((
                agent,
                settings,
                devices,
                registry,
                scopes,
                task_tools,
                schedule_tools,
                preparation_lease,
            ))
        })
        .await
        .map_err(|_| HostError::Worker)??;
        let mutation = Arc::new(Mutex::new(()));
        let settings_service =
            SettingsService::open(agent.clone(), settings.clone(), &devices, mutation.clone())
                .await?;
        let startup = settings_service.startup_context().await?;
        let effective = startup.settings.clone();
        if let super::settings::service::SettingsStartupPolicy::OptionalFeaturesSuspended {
            operation_id,
        } = &startup.policy
        {
            tracing::warn!(
                %operation_id,
                pending_application = startup.pending_application.is_some(),
                "optional settings execution remains suspended for original-operation recovery"
            );
        }
        let worker_endpoint = if let Some(configuration) = effective.worker.clone() {
            let owned = (configuration, agent.clone());
            let (endpoint, _worker_owner) = tokio::task::spawn_blocking(move || {
                let owned = owned;
                let identity = crate::worker::load_or_create_worker_identity(&owned.1)?;
                let endpoint = crate::worker::WorkerEndpoint::open(
                    owned.1.clone(),
                    identity,
                    crate::worker::WorkerRole::Coordinator,
                    owned.0.relay_origin,
                )?;
                Ok::<_, HostError>((endpoint, owned.1))
            })
            .await
            .map_err(|_| HostError::Worker)??;
            Some(endpoint)
        } else {
            None
        };
        let credentials = CredentialVault::from_environment();
        if let Some(path) = binding_file {
            credentials
                .bootstrap_mcp_session_from_file(path, effective.mcp.clone(), cancellation.clone())
                .await?;
        }
        if cancellation.is_cancelled() {
            return Err(crate::credentials::CredentialError::Cancelled.into());
        }
        let mcp = super::mcp::McpService::start(
            effective.mcp.clone(),
            Arc::new(credentials.clone()),
            agent.clone(),
            CancellationToken::new(),
        )?;
        let (registry, mcp_scopes) = mcp.register(registry).await?;
        let settings_tools =
            super::settings::agent_tools::SettingsTools::new(settings_service.clone());
        scopes.extend(mcp_scopes);
        let settings_registration = Arc::clone(&settings_tools);
        let registry_resources = (agent.store(), agent.clone());
        let (registry, authorizer, discovery, _registry_owner) =
            tokio::task::spawn_blocking(move || {
                let registry_resources = registry_resources;
                let mut registry = registry;
                let catalog_authorizer = Arc::new(Authorizer::new(
                    registry_resources.0.clone(),
                    Arc::new(registry.clone()),
                    scopes.clone(),
                )?);
                let selector = ToolSelector::new(&registry, catalog_authorizer)?;
                let search = Arc::new(SearchTools::new(selector.clone()));
                registry.register(search.clone())?;
                scopes.insert(SEARCH_TOOLS_NAME.into(), ToolScopeRule::LocalReadOnly);
                settings_registration.register(&mut registry)?;
                let registry = Arc::new(registry);
                let authorizer = Arc::new(Authorizer::new(
                    registry_resources.0,
                    registry.clone(),
                    scopes,
                )?);
                Ok::<_, HostError>((
                    registry,
                    authorizer,
                    DiscoveryServices { selector, search },
                    registry_resources.1,
                ))
            })
            .await
            .map_err(|_| HostError::Worker)??;
        let providers = Arc::new(HostProviderFactory::new(
            settings.clone(),
            credentials.clone(),
        ));
        let memory = super::memory::MemoryService::open_with_embedding_preferences(
            agent.clone(),
            effective.memory.clone(),
            Some(providers.clone()),
            CancellationToken::new(),
            startup.embedding_preferences(),
        )
        .await?;
        let catalog = super::selection::SelectionCatalog::from_registry(&registry)?;
        let (selection_settings, remote_enabled) = startup.selection_configuration();
        let selection = super::selection::SelectionPreflight::new(
            agent.clone(),
            discovery.selector.clone(),
            &selection_settings,
            credentials.native_store(),
            catalog.clone(),
            Duration::from_secs(300),
        )?;
        if !remote_enabled {
            let generation = selection.suspend_remote()?;
            selection.configure_remote(generation, selection_settings, false)?;
        }
        let relay_origin = effective.relay_origin.clone();
        let worker_settings = effective.worker.clone();
        let composition = Arc::new(super::composition::Composition::new(
            memory,
            mcp,
            selection,
            catalog,
            effective,
            HostInbox::new(agent.store()),
            agent.clone(),
        ));
        settings_service.bind_composition(composition.clone())?;
        let broker = ToolBroker::start_with_owner(
            agent.store(),
            registry.clone(),
            authorizer.clone(),
            BrokerLimits {
                max_concurrent: 8,
                max_in_flight: 64,
                queue_timeout: Duration::from_secs(30),
                execution_timeout: Duration::from_secs(300),
                cancellation_grace: Duration::from_secs(5),
            },
            agent.clone(),
        )
        .await?;
        let (settings_agents, settings_supervisor) =
            super::settings::agent::SettingsAgentRuntime::new(
                super::settings::agent::SettingsAgentComponents {
                    service: settings_service.clone(),
                    tools: settings_tools,
                    registry: Arc::clone(&registry),
                    broker: broker.clone(),
                    providers: providers.as_ref().clone(),
                    mutation: Arc::clone(&mutation),
                    agent: agent.clone(),
                },
            );
        let (server, calls) = LocalServer::bind_with_lease(lease, LocalOptions::default()).await?;
        let scheduler = Scheduler::start_with_preparation(
            SchedulerServices {
                agent: agent.clone(),
                registry: registry.clone(),
                broker: broker.clone(),
                providers,
                discovery: Some(discovery),
            },
            SchedulerLimits::default(),
            composition.clone(),
        )
        .await?;
        let scheduler_binding = match task_tools.bind(&scheduler) {
            Ok(binding) => binding,
            Err(error) => {
                scheduler.shutdown().await?;
                return Err(error.into());
            }
        };
        let schedule_engine = match crate::scheduling::engine::ScheduleEngine::start(
            agent.clone(),
            scheduler.clone(),
            registry.clone(),
        )
        .await
        {
            Ok(engine) => engine,
            Err(error) => {
                scheduler.shutdown().await?;
                return Err(error.into());
            }
        };
        let schedule_controls =
            match ScheduleControls::new(agent.clone(), schedule_engine.clone(), authorizer.clone())
            {
                Ok(controls) => controls,
                Err(error) => {
                    schedule_engine.shutdown().await?;
                    scheduler.shutdown().await?;
                    return Err(error.into());
                }
            };
        let schedule_binding = match schedule_tools.bind(&schedule_controls, &scheduler) {
            Ok(binding) => binding,
            Err(error) => {
                schedule_controls.shutdown().await?;
                schedule_engine.shutdown().await?;
                scheduler.shutdown().await?;
                return Err(error.into());
            }
        };
        let (relay_updates, relay_status) =
            watch::channel(crate::devices::RelaySessionStatus::Stopped);
        let (receipts, _) = watch::channel(0);
        let (metadata_changed, _) = watch::channel(0);
        let (publication_updates, publication) = watch::channel(Default::default());
        let inner = Arc::new(HostInner {
            inbox: HostInbox::new(agent.store()),
            agent,
            settings,
            settings_service,
            settings_agents,
            credentials,
            devices,
            registry,
            authorizer,
            scheduler,
            schedule_engine,
            schedule_controls,
            broker,
            composition,
            changed: Notify::new(),
            mutation,
            relay_origin,
            relay_status,
            relay_updates,
            receipts,
            metadata_changed,
            publication,
            publication_updates,
            draining: AtomicBool::new(false),
            workers_drained: AtomicBool::new(false),
            settings_work_drained: AtomicBool::new(false),
            recovery: Mutex::new(()),
            worker: std::sync::OnceLock::new(),
        });
        if let (Some(endpoint), Some(effective)) = (worker_endpoint, worker_settings) {
            let coordinator = super::worker::service::WorkerCoordinator::new(
                Arc::downgrade(&inner),
                endpoint,
                effective,
            );
            inner
                .worker
                .set(coordinator)
                .map_err(|_| HostError::Worker)?;
        }
        Ok(Self {
            inner,
            server,
            calls,
            settings_supervisor,
            _scheduler_binding: scheduler_binding,
            _schedule_binding: schedule_binding,
        })
    }

    pub async fn run(self, cancellation: CancellationToken) -> Result<(), HostError> {
        tokio::spawn(self.run_owned(cancellation))
            .await
            .map_err(|_| HostError::Worker)?
    }

    async fn run_owned(self, cancellation: CancellationToken) -> Result<(), HostError> {
        let Self {
            inner,
            server,
            mut calls,
            settings_supervisor,
            _scheduler_binding,
            _schedule_binding,
        } = self;
        let work_stop = cancellation.child_token();
        let local_stop = CancellationToken::new();
        let mut workers = JoinSet::new();
        let settings_agent_owner = Arc::clone(&inner);
        let settings_agent_work_stop = work_stop.clone();
        let settings_agent_owner_stop = local_stop.clone();
        workers.spawn(async move {
            let owner = settings_agent_owner;
            let outcome = settings_supervisor
                .run(settings_agent_work_stop, settings_agent_owner_stop)
                .await;
            drop(owner);
            HostWorker::SettingsAgent(outcome)
        });
        let local_token = local_stop.clone();
        workers.spawn(async move {
            HostWorker::Local(server.run(local_token).await.map_err(HostError::from))
        });
        let dispatcher = inner.clone();
        let dispatcher_stop = work_stop.clone();
        workers.spawn(
            async move { HostWorker::Dispatcher(dispatcher.dispatch(dispatcher_stop).await) },
        );
        let settings_owner = inner.clone();
        let settings_stop = work_stop.clone();
        workers.spawn(async move {
            HostWorker::Settings(settings_owner.apply_settings(settings_stop).await)
        });
        if let Some(origin) = inner.relay_origin.clone() {
            let host = inner.clone();
            let relay_stop = work_stop.clone();
            workers.spawn(async move {
                let outcome = match super::settings::transport::host_settings_transport(
                    host.agent.clone(),
                    host.devices.clone(),
                    Arc::clone(&host.mutation),
                ) {
                    Ok((_, owner)) => {
                        host.devices
                            .run_relay_with_settings(
                                origin,
                                relay_stop,
                                host.relay_updates.clone(),
                                owner,
                            )
                            .await
                    }
                    Err(error) => Err(error),
                };
                HostWorker::Relay(outcome)
            });
        }
        if let Some(coordinator) = inner.worker.get().cloned() {
            let coordinator_stop = work_stop.clone();
            workers.spawn(async move {
                HostWorker::Coordinator(coordinator.run(coordinator_stop).await)
            });
        }
        let publisher = inner.clone();
        let publisher_stop = work_stop.clone();
        let (initialized, initial_pass) = tokio::sync::oneshot::channel();
        workers.spawn(async move {
            let outcome = publisher
                .clone()
                .publish_metadata(
                    publisher_stop,
                    publisher.publication_updates.clone(),
                    initialized,
                )
                .await;
            if outcome.is_err() {
                publisher.publication_updates.send_modify(|status| {
                    status.stopped = true;
                    status.last_error = Some("metadata publication worker failed".into());
                });
            }
            HostWorker::Publication(outcome)
        });
        let mut handlers = JoinSet::new();
        tokio::pin!(initial_pass);
        let activation = loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break Ok(()),
                ready = &mut initial_pass => break match ready {
                    Ok(()) => async {
                        inner.scheduler.activate().await?;
                        inner.schedule_engine.activate()?;
                        Ok::<_, HostError>(())
                    }.await,
                    Err(_) => Err(HostError::Worker),
                },
                joined = workers.join_next() => match joined {
                    Some(Ok(HostWorker::Coordinator(outcome))) => {
                        if outcome.is_err() { tracing::warn!("optional worker coordinator stopped; inspect its actual status"); }
                    }
                    Some(Ok(HostWorker::Relay(outcome))) => {
                        if outcome.is_err() {
                            inner.relay_updates.send_replace(crate::devices::RelaySessionStatus::Failed { reason: "relay operation requires operator action" });
                            tracing::warn!("optional relay stopped during initial metadata reconciliation");
                        }
                    }
                    Some(Ok(HostWorker::Local(outcome) | HostWorker::Dispatcher(outcome) | HostWorker::Publication(outcome) | HostWorker::Settings(outcome) | HostWorker::SettingsAgent(outcome))) => break outcome.and(Err(HostError::Worker)),
                    _ => break Err(HostError::Worker),
                },
                joined = handlers.join_next(), if !handlers.is_empty() => {
                    if !matches!(joined, Some(Ok(()))) { break Err(HostError::Worker); }
                },
                call = calls.recv(), if handlers.len() < 32 => {
                    let Some(call) = call else { break Err(HostError::Worker); };
                    let host = inner.clone();
                    handlers.spawn(async move { host.handle(call).await; });
                }
            }
        };
        let mut result = if let Err(error) = activation {
            Err(error)
        } else {
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break Ok(()),
                    joined = workers.join_next() => match joined {
                        Some(Ok(HostWorker::Coordinator(outcome))) => {
                        if outcome.is_err() { tracing::warn!("optional worker coordinator stopped; inspect its actual status"); }
                    }
                    Some(Ok(HostWorker::Relay(outcome))) => {
                            if outcome.is_err() {
                                inner.relay_updates.send_replace(crate::devices::RelaySessionStatus::Failed { reason: "relay operation requires operator action" });
                                tracing::warn!("optional relay stopped; local host remains available");
                            }
                        }
                        Some(Ok(HostWorker::Local(outcome) | HostWorker::Dispatcher(outcome) | HostWorker::Publication(outcome) | HostWorker::Settings(outcome) | HostWorker::SettingsAgent(outcome))) => break outcome.and(Err(HostError::Worker)),
                        _ => break Err(HostError::Worker),
                    },
                    joined = handlers.join_next(), if !handlers.is_empty() => {
                        if !matches!(joined, Some(Ok(()))) { break Err(HostError::Worker); }
                    }
                    call = calls.recv(), if handlers.len() < 32 => {
                        let Some(call) = call else { break Err(HostError::Worker); };
                        let host = inner.clone();
                        handlers.spawn(async move { host.handle(call).await; });
                    }
                }
            }
        };
        {
            let _mutation = inner.mutation.lock().await;
            inner.draining.store(true, Ordering::Release);
            if let Err(error) = inner.settings_service.stop_intake()
                && result.is_ok()
            {
                result = Err(error.into());
            }
            inner.composition.close();
            inner.schedule_controls.close();
            if let Some(coordinator) = inner.worker.get() {
                coordinator.close();
            }
        }
        inner.receipt_changed();
        work_stop.cancel();
        let service_owner = inner.clone();
        let mut service_work = Some(tokio::spawn(async move {
            loop {
                match service_owner.settings_service.close().await {
                    Ok(()) => break,
                    Err(_) => tracing::error!(
                        "settings cleanup remains unconfirmed; state ownership retained"
                    ),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            loop {
                match service_owner.composition.stop_work().await {
                    Ok(()) => return,
                    Err(_) => tracing::error!(
                        "memory or selection cleanup remains unconfirmed; state ownership retained"
                    ),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }));
        let mut service_work_stopped = false;
        let scheduler_owner = inner.clone();
        let mut shutdown = Some(tokio::spawn(async move {
            if let Some(coordinator) = scheduler_owner.worker.get() {
                loop {
                    match coordinator.shutdown().await {
                        Ok(()) => break,
                        Err(_) => tracing::error!(
                            "worker coordinator retains admitted work; scheduler and state ownership retained"
                        ),
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            loop {
                match scheduler_owner.schedule_controls.shutdown().await {
                    Ok(()) => break,
                    Err(_) => tracing::error!(
                        "schedule controls retain admitted work; engine and state ownership retained"
                    ),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            loop {
                match scheduler_owner.schedule_engine.shutdown().await {
                    Ok(()) => break,
                    Err(_) => tracing::error!(
                        "schedule admission cleanup remains unconfirmed; native scheduler and state ownership retained"
                    ),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            loop {
                match scheduler_owner.scheduler.shutdown().await {
                    Ok(report) => return Ok::<_, HostError>(report),
                    Err(SchedulerError::ShutdownPending) => tracing::warn!(
                        "host retains state ownership while scheduler cleanup drains"
                    ),
                    Err(error) => return Err(error.into()),
                }
            }
        }));
        let mut cleanup_changed = inner.receipts.subscribe();
        let mut calls_open = true;
        let mut quiescence = None;
        let mut quiescence_proved = false;
        let mut composition_shutdown = None;
        let mut composition_stopped = false;
        loop {
            if inner.workers_drained.load(Ordering::Acquire)
                && inner.settings_agents.is_quiescent()
                && !quiescence_proved
                && quiescence.is_none()
            {
                let host = inner.clone();
                quiescence = Some(tokio::spawn(async move {
                    loop {
                        let outcome = {
                            let _recovery = host.recovery.lock().await;
                            host.broker.recover_after_drain().await
                        };
                        match outcome {
                            Ok(report) => return report,
                            Err(BrokerError::RecoveryBusy) => tracing::warn!(
                                "broker supervisors still own admissions; host retains state ownership"
                            ),
                            Err(_) => tracing::error!(
                                "broker quiescence inspection failed; host retains state ownership"
                            ),
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }));
            }
            if quiescence_proved {
                let _recovery = inner.recovery.lock().await;
                let report = inner.broker.recovery_report().await;
                let running = inner
                    .blocking(|host| global_running(&host.agent.store()))
                    .await;
                if matches!((&report,&running), (Ok(report),Ok(false)) if report.running_count == 0 && report.unresolved_count == 0 && report.settings_unresolved_count == 0 && report.requirements.is_empty() && inner.settings_agents.is_quiescent())
                {
                    if composition_stopped
                        && service_work_stopped
                        && inner.publication.borrow().stopped
                    {
                        break;
                    }
                    if service_work_stopped && composition_shutdown.is_none() {
                        let host = inner.clone();
                        composition_shutdown = Some(tokio::spawn(async move {
                            loop {
                                match host.composition.finish_mcp().await {
                                    Ok(()) => return,
                                    Err(_) => tracing::error!(
                                        "host service cleanup remains unconfirmed; owner inspection stays available"
                                    ),
                                }
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }));
                    }
                }
                if report.is_err() || running.is_err() {
                    tracing::error!(
                        "cleanup inspection failed; host retains ownership for explicit recovery"
                    );
                }
            }
            tokio::select! {
                joined = async { service_work.as_mut().ok_or(HostError::Worker)?.await.map_err(|_| HostError::Worker) }, if service_work.is_some() => {
                    service_work.take();
                    match joined {
                        Ok(()) => { service_work_stopped = true; inner.settings_work_drained.store(true, Ordering::Release); inner.receipt_changed(); },
                        Err(error) => { if result.is_ok() { result = Err(error); } }
                    }
                }
                joined = async { composition_shutdown.as_mut().ok_or(HostError::Worker)?.await.map_err(|_| HostError::Worker) }, if composition_shutdown.is_some() => {
                    composition_shutdown.take();
                    match joined {
                        Ok(()) => composition_stopped = true,
                        Err(error) => { if result.is_ok() { result = Err(error); } }
                    }
                }
                joined = async { quiescence.as_mut().ok_or(HostError::Worker)?.await.map_err(|_| HostError::Worker) }, if quiescence.is_some() => {
                    quiescence.take();
                    match joined {
                        Ok(_) => quiescence_proved = true,
                        Err(error) => {
                            if result.is_ok() { result = Err(error); }
                            tracing::error!("broker quiescence worker failed; state ownership retained");
                        }
                    }
                }
                joined = async { shutdown.as_mut().ok_or(HostError::Worker)?.await.map_err(|_| HostError::Worker)? }, if shutdown.is_some() => {
                    shutdown.take();
                    match joined {
                        Ok(_) => { inner.workers_drained.store(true, Ordering::Release); inner.receipt_changed(); },
                        Err(error) => { if result.is_ok() { result = Err(error); } tracing::error!("scheduler drain failed; state ownership retained"); }
                    }
                }
                joined = workers.join_next(), if !workers.is_empty() => {
                    let failure = match joined {
                        Some(Ok(HostWorker::Local(outcome) | HostWorker::Dispatcher(outcome) | HostWorker::Publication(outcome) | HostWorker::Settings(outcome) | HostWorker::SettingsAgent(outcome))) => outcome.err(),
                        Some(Ok(HostWorker::Coordinator(outcome))) => { if outcome.is_err() { tracing::warn!("optional worker coordinator ended during host drain"); } None },
                        Some(Ok(HostWorker::Relay(outcome))) => { if outcome.is_err() { tracing::warn!("optional relay stopped during host drain"); } None },
                        _ => Some(HostError::Worker),
                    };
                    if result.is_ok() && let Some(error) = failure { result = Err(error); }
                }
                joined = handlers.join_next(), if !handlers.is_empty() => {
                    if !matches!(joined, Some(Ok(()))) && result.is_ok() { result = Err(HostError::Worker); }
                }
                call = calls.recv(), if calls_open && handlers.len() < 32 => {
                    if let Some(call) = call {
                        let host = inner.clone();
                        handlers.spawn(async move { host.handle(call).await; });
                    } else { calls_open = false; }
                }
                _ = cleanup_changed.changed() => {},
                _ = inner.settings_agents.changed() => {},
            }
        }
        local_stop.cancel();
        calls.close();
        while let Ok(call) = calls.try_recv() {
            if call
                .respond(LocalResponsePayload::Rejected {
                    error: LocalFailure {
                        code: LocalFailureCode::Unavailable,
                        message: "host stopped before command admission".into(),
                    },
                })
                .is_err()
            {
                tracing::debug!("queued owner request disconnected during shutdown");
            }
        }
        while let Some(joined) = handlers.join_next().await {
            if joined.is_err() && result.is_ok() {
                result = Err(HostError::Worker);
            }
        }
        while let Some(joined) = workers.join_next().await {
            match joined {
                Ok(
                    HostWorker::Local(outcome)
                    | HostWorker::Dispatcher(outcome)
                    | HostWorker::Publication(outcome)
                    | HostWorker::Settings(outcome)
                    | HostWorker::SettingsAgent(outcome),
                ) => {
                    if result.is_ok() {
                        result = outcome;
                    }
                }
                Ok(HostWorker::Coordinator(outcome)) => {
                    if outcome.is_err() {
                        tracing::warn!("optional worker coordinator ended during host shutdown");
                    }
                }
                Ok(HostWorker::Relay(outcome)) => {
                    if outcome.is_err() {
                        tracing::warn!("optional relay ended with an error during shutdown");
                    }
                }
                Err(_) => {
                    if result.is_ok() {
                        result = Err(HostError::Worker);
                    }
                }
            }
        }
        drop(_scheduler_binding);
        drop(_schedule_binding);
        result
    }
}

impl HostInner {
    pub(super) fn ensure_accepting(&self) -> Result<(), HostError> {
        if self.draining.load(Ordering::Acquire) {
            return Err(HostError::Draining);
        }
        Ok(())
    }
    pub(super) fn receipt_changed(&self) {
        self.receipts
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    pub(super) async fn blocking<T: Send + 'static>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Self) -> Result<T, HostError> + Send + 'static,
    ) -> Result<T, HostError> {
        let host = self.clone();
        tokio::task::spawn_blocking(move || operation(&host))
            .await
            .map_err(|_| HostError::Worker)?
    }

    async fn handle(self: Arc<Self>, call: LocalCall) {
        let result = self.handle_payload(&call).await;
        let response = result.unwrap_or_else(|error| LocalResponsePayload::Rejected {
            error: public_failure(&error),
        });
        if let Err(error) = call.respond(response)
            && !matches!(error, LocalError::Disconnected)
        {
            tracing::warn!("owner reply could not be delivered");
        }
    }

    async fn handle_payload(
        self: &Arc<Self>,
        call: &LocalCall,
    ) -> Result<LocalResponsePayload, HostError> {
        if call.cancellation.is_cancelled() {
            return Err(LocalError::Cancelled.into());
        }
        match &call.request.payload {
            LocalRequestPayload::Status => {
                Err(HostError::Invalid("status is handled by the endpoint"))
            }
            LocalRequestPayload::SubmitCommand { command } => {
                self.admit(
                    InboxInput::Domain(command.clone()),
                    call.original_command.clone(),
                    CommandOrigin::LocalOwner,
                )
                .await
            }
            LocalRequestPayload::OwnerCommand { command } => {
                self.admit(
                    InboxInput::Owner(command.clone()),
                    call.original_command.clone(),
                    CommandOrigin::LocalOwner,
                )
                .await
            }
            LocalRequestPayload::Query {
                query:
                    query @ (crate::ipc::OwnerQuery::SettingsGrantCurrent { .. }
                    | crate::ipc::OwnerQuery::SettingsGrantVersion { .. }
                    | crate::ipc::OwnerQuery::SettingsGrantReceipt { .. }),
            } => Ok(LocalResponsePayload::QueryResult {
                result: self.settings_grant_query(call, query.clone()).await?,
            }),
            LocalRequestPayload::Query {
                query: crate::ipc::OwnerQuery::SettingsOperation { request_id },
            } => {
                let caller = self.settings_service.local_owner_cleanup_caller(call)?;
                Ok(LocalResponsePayload::QueryResult {
                    result: crate::ipc::OwnerQueryResult::SettingsOperation {
                        operation: self.settings_service.operation(caller, *request_id).await?,
                    },
                })
            }
            LocalRequestPayload::Query { query } => Ok(LocalResponsePayload::QueryResult {
                result: self.query(query.clone(), call.cancellation.clone()).await?,
            }),
            LocalRequestPayload::OwnerOperation {
                operation: crate::ipc::OwnerOperation::SetSettingsGrant { change },
            } => Ok(LocalResponsePayload::OwnerOperationResult {
                result: self
                    .settings_grant_change(call, change.as_ref().clone())
                    .await?,
            }),
            LocalRequestPayload::OwnerOperation {
                operation:
                    crate::ipc::OwnerOperation::AcknowledgeSettingsUncertainty {
                        command_id,
                        exact_fence_sha256,
                        lost_ram_uncertainty_accepted,
                    },
            } => {
                if !*lost_ram_uncertainty_accepted {
                    return Err(HostError::Invalid(
                        "lost-RAM uncertainty must be explicitly accepted",
                    ));
                }
                let caller = self.settings_service.local_owner_cleanup_caller(call)?;
                let principal = self.settings_service.settings_cleanup_principal(&caller)?;
                let _recovery = self.recovery.lock().await;
                let _mutation = self.mutation.lock().await;
                if !self.draining.load(Ordering::Acquire)
                    || !self.workers_drained.load(Ordering::Acquire)
                    || !self.settings_work_drained.load(Ordering::Acquire)
                    || !self.settings_agents.is_quiescent()
                    || self.agent.state_directory().is_none()
                {
                    return Err(HostError::Invalid(
                        "Settings uncertainty cleanup requires the leased Host and actual native, Broker and service drains",
                    ));
                }
                let command_id = *command_id;
                self.settings_service
                    .inspect_settings_cleanup(caller, command_id, exact_fence_sha256.clone())
                    .await?;
                let acknowledgement = self
                    .broker
                    .acknowledge_settings_cleanup(command_id, exact_fence_sha256.clone(), principal)
                    .await?;
                self.receipt_changed();
                Ok(LocalResponsePayload::OwnerOperationResult {
                    result: crate::ipc::OwnerOperationResult::SettingsUncertaintyAcknowledged {
                        acknowledgement:
                            super::settings::agent::SettingsUncertaintyAcknowledgement {
                                command_id: acknowledgement.command_id,
                                exact_fence_sha256: acknowledgement.exact_fence_sha256,
                                owner_principal_id: acknowledgement.owner_principal_id,
                                attested_at: acknowledgement.attested_at,
                                lost_ram_uncertainty_accepted: acknowledgement
                                    .lost_ram_uncertainty_accepted,
                            },
                    },
                })
            }
            LocalRequestPayload::OwnerOperation {
                operation: crate::ipc::OwnerOperation::ReconcileSettings { operation_id },
            } => {
                let caller = self.settings_service.local_owner_cleanup_caller(call)?;
                let operation = self
                    .settings_service
                    .reconcile_write(caller, *operation_id, call.cancellation.clone())
                    .await?;
                Ok(LocalResponsePayload::OwnerOperationResult {
                    result: crate::ipc::OwnerOperationResult::SettingsReconciled {
                        operation: Box::new(operation),
                    },
                })
            }
            LocalRequestPayload::OwnerOperation {
                operation:
                    crate::ipc::OwnerOperation::ReplaceSettings {
                        expected_revision,
                        settings,
                    },
            } => {
                self.ensure_accepting()?;
                let caller = self.settings_service.local_owner_caller(call)?;
                let receipt = self
                    .settings_service
                    .replace_as_owner(
                        caller,
                        *expected_revision,
                        settings.as_ref().clone(),
                        call.cancellation.clone(),
                    )
                    .await?;
                if let (Some(coordinator), Some(settings)) =
                    (self.worker.get(), receipt.settings.as_ref())
                {
                    coordinator.settings_changed(settings.worker.as_ref());
                }
                Ok(LocalResponsePayload::OwnerOperationResult {
                    result: crate::ipc::OwnerOperationResult::SettingsReceipt {
                        operation: Box::new(receipt.operation),
                        settings: receipt.settings,
                        restart_required: receipt.restart_required,
                    },
                })
            }
            LocalRequestPayload::OwnerOperation { operation } => {
                Ok(LocalResponsePayload::OwnerOperationResult {
                    result: self
                        .operate(
                            operation.clone(),
                            call.request.request_id,
                            call.cancellation.clone(),
                        )
                        .await?,
                })
            }
            LocalRequestPayload::NativeSettings { operation } => {
                Ok(LocalResponsePayload::NativeSettingsResult {
                    result: self.native_settings(call, operation.clone()).await?,
                })
            }
            LocalRequestPayload::Synchronize { request } => {
                let cursor = request.cursors[0];
                let max_events = request.max_events;
                let batch = self
                    .blocking(move |host| Ok(host.devices.build_sync_batch(cursor, max_events)?))
                    .await?;
                Ok(LocalResponsePayload::Synchronized { batch })
            }
            LocalRequestPayload::WaitCommand {
                command_id,
                timeout_ms,
            } => {
                let command_id = *command_id;
                let mut changed = self.receipts.subscribe();
                let deadline = tokio::time::Instant::now() + Duration::from_millis(*timeout_ms);
                loop {
                    let receipt = self
                        .blocking(move |host| {
                            Ok(host.inbox.record(command_id)?.map(command_receipt))
                        })
                        .await?;
                    if receipt.as_ref().is_some_and(|receipt| {
                        matches!(
                            receipt.phase,
                            CommandPhase::Completed | CommandPhase::Rejected
                        )
                    }) {
                        return Ok(LocalResponsePayload::CommandWaited {
                            receipt,
                            timed_out: false,
                        });
                    }
                    tokio::select! {
                        _ = call.cancellation.cancelled() => return Err(LocalError::Cancelled.into()),
                        _ = tokio::time::sleep_until(deadline) => return Ok(LocalResponsePayload::CommandWaited { receipt, timed_out: true }),
                        result = changed.changed() => result.map_err(|_| HostError::Worker)?,
                    }
                }
            }
            LocalRequestPayload::Wait { request } => {
                let mut changed = self.scheduler.subscribe();
                let mut scheduled = self.schedule_engine.subscribe();
                let mut metadata = self.metadata_changed.subscribe();
                let conversation = self.agent.conversation(request.conversation_id).await?;
                if request.after_sequence > conversation.info.last_sequence {
                    return Err(HostError::Invalid("wait cursor exceeds the host head"));
                }
                let timed_out = if conversation.info.last_sequence > request.after_sequence {
                    false
                } else {
                    tokio::select! {
                        _ = call.cancellation.cancelled() => return Err(LocalError::Cancelled.into()),
                        result = tokio::time::timeout(Duration::from_millis(request.timeout_ms), async {
                            tokio::select! {
                                result = changed.changed() => result,
                                result = scheduled.changed() => result,
                                result = metadata.changed() => result,
                            }
                        }) => {
                            match result {
                                Ok(Ok(())) => false,
                                Ok(Err(_)) => return Err(HostError::Worker),
                                Err(_) => true,
                            }
                        }
                    }
                };
                let head = self
                    .agent
                    .conversation(request.conversation_id)
                    .await?
                    .info
                    .last_sequence;
                Ok(LocalResponsePayload::ConversationChanged {
                    conversation_id: request.conversation_id,
                    through_sequence: head,
                    timed_out,
                })
            }
        }
    }
}

pub(super) fn public_failure(error: &HostError) -> LocalFailure {
    let (code, message) = match error {
        HostError::Local(LocalError::Cancelled)
        | HostError::Memory(super::memory::MemoryError::Cancelled)
        | HostError::Consolidation(crate::memory::consolidate::ConsolidationError::Cancelled)
        | HostError::Selection(super::selection::SelectionPreflightError::Cancelled) => (
            LocalFailureCode::Cancelled,
            "operation was cancelled; inspect durable state before retrying a mutation",
        ),
        HostError::ScheduleControl(ControlError::Cancelled)
        | HostError::ScheduleEngine(crate::scheduling::engine::EngineError::Cancelled) => (
            LocalFailureCode::Cancelled,
            "schedule operation was cancelled; inspect durable state before retrying a mutation",
        ),
        HostError::ScheduleControl(ControlError::OutsideScope) => (
            LocalFailureCode::PermissionDenied,
            "schedule operation exceeds its captured authority",
        ),
        HostError::ScheduleControl(ControlError::Busy) => {
            (LocalFailureCode::Busy, "schedule control admission is busy")
        }
        HostError::ScheduleControl(ControlError::Stopped | ControlError::ShutdownPending)
        | HostError::ScheduleEngine(_) => (
            LocalFailureCode::Unavailable,
            "schedule controls or engine are unavailable; inspect actual status",
        ),
        HostError::ScheduleControl(
            ControlError::Schedule(_) | ControlError::Invalid(_) | ControlError::ReplyTooLarge,
        ) => (
            LocalFailureCode::InvalidRequest,
            "schedule request is invalid or conflicts with durable state; inspect its identity and revision",
        ),
        HostError::ScheduleControl(ControlError::OutcomeUnknown(_)) => (
            LocalFailureCode::Internal,
            "schedule mutation outcome is unknown; inspect the original job or occurrence before an explicit retry",
        ),
        HostError::WorkerUnadmitted => (
            LocalFailureCode::InvalidRequest,
            "worker initialization was retained without native admission; expired approval cannot be reconstructed",
        ),
        HostError::WorkerRecoveryUnavailable => (
            LocalFailureCode::Unavailable,
            "worker initialization recovery awaits its configured coordinator",
        ),
        HostError::WorkerProtocol(
            crate::worker::WorkerError::UntrustedPeer | crate::worker::WorkerError::WrongRole,
        ) => (
            LocalFailureCode::PermissionDenied,
            "worker purpose identity is not authorized",
        ),
        HostError::WorkerProtocol(crate::worker::WorkerError::Capacity(_)) => (
            LocalFailureCode::Busy,
            "worker coordinator capacity is exhausted",
        ),
        HostError::WorkerProtocol(
            crate::worker::WorkerError::Expired
            | crate::worker::WorkerError::ApprovalRequired
            | crate::worker::WorkerError::Invalid(_)
            | crate::worker::WorkerError::Conflict(_),
        ) => (
            LocalFailureCode::InvalidRequest,
            "worker request expired or conflicts with its exact retained scope; inspect the original disclosure and identifiers",
        ),
        HostError::Draining => (
            LocalFailureCode::Unavailable,
            "host is draining; inspect status and complete explicit cleanup before restarting",
        ),
        HostError::LegacyForeground => (
            LocalFailureCode::Unavailable,
            "legacy foreground task requires stopping the host and explicitly resuming or settling it through the foreground CLI",
        ),
        HostError::Inbox(InboxError::OriginDenied)
        | HostError::SettingsService(SettingsServiceError::OutsideScope)
        | HostError::Device(DeviceError::AccessDenied | DeviceError::EndpointRevoked) => (
            LocalFailureCode::PermissionDenied,
            "command origin is no longer authorized",
        ),
        HostError::Inbox(InboxError::Capacity)
        | HostError::SettingsService(SettingsServiceError::Capacity)
        | HostError::Scheduler(SchedulerError::Capacity(_)) => (
            LocalFailureCode::Busy,
            "host command or task capacity is exhausted",
        ),
        HostError::Invalid(message) | HostError::Inbox(InboxError::Invalid(message)) => {
            (LocalFailureCode::InvalidRequest, *message)
        }
        HostError::Inbox(InboxError::Conflict(_))
        | HostError::SettingsService(SettingsServiceError::Conflict)
        | HostError::Settings(SettingsError::Conflict) => (
            LocalFailureCode::InvalidRequest,
            "identity or revision conflicts with existing durable state",
        ),
        HostError::Credential(_) => (
            LocalFailureCode::Unavailable,
            "credential operation failed; inspect native store availability and exact reference",
        ),
        HostError::SettingsService(
            SettingsServiceError::Invalid(_) | SettingsServiceError::Contract(_),
        ) => (LocalFailureCode::InvalidRequest, "invalid settings request"),
        HostError::SettingsService(SettingsServiceError::NotFound) => (
            LocalFailureCode::InvalidRequest,
            "settings operation was not found in this scope",
        ),
        HostError::SettingsService(
            SettingsServiceError::Expired
            | SettingsServiceError::Cancelled
            | SettingsServiceError::Stopped,
        ) => (
            LocalFailureCode::Unavailable,
            "settings admission is closed, expired or cancelled",
        ),
        HostError::SettingsService(
            SettingsServiceError::OutcomeUnknown { .. }
            | SettingsServiceError::Worker
            | SettingsServiceError::WriteBlocked { .. },
        ) => (
            LocalFailureCode::Unavailable,
            "settings outcome is uncertain; inspect the original operation",
        ),
        HostError::Scheduler(_) => (
            LocalFailureCode::Unavailable,
            "scheduler operation requires an owner decision or is unavailable",
        ),
        _ => (
            LocalFailureCode::Internal,
            "host operation failed; durable acceptance may already exist",
        ),
    };
    LocalFailure {
        code,
        message: message.into(),
    }
}

pub(super) fn timestamp(time: UnixMillis) -> Result<i64, HostError> {
    i64::try_from(time.get())
        .map_err(|_| HostError::Invalid("timestamp exceeds the supported range"))
}

pub(super) fn command_receipt(record: HostCommandRecord) -> CommandReceipt {
    CommandReceipt {
        command_id: record.command_id,
        conversation_id: record.conversation_id,
        phase: record.phase,
        accepted_at: record.accepted_at,
        applied: record.applied_receipt,
        result: record.terminal_result,
        archived: record.original.is_none(),
    }
}

fn global_running(store: &Store) -> Result<bool, HostError> {
    store
        .connection()?
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tool_calls WHERE state='running')",
            [],
            |row| row.get(0),
        )
        .map_err(StorageError::from)
        .map_err(HostError::from)
}
