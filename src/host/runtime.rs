use std::path::Path;
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
    Inbox(#[from] InboxError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
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
    Credential(#[from] crate::credentials::CredentialError),
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
    _scheduler_binding: SchedulerToolBinding,
}

pub(super) struct HostInner {
    pub(super) agent: Agent,
    pub(super) inbox: HostInbox,
    pub(super) settings: SettingsRepository,
    pub(super) credentials: CredentialVault,
    pub(super) devices: DeviceEndpoint,
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) authorizer: Arc<Authorizer>,
    pub(super) scheduler: Scheduler,
    pub(super) broker: ToolBroker,
    pub(super) changed: Notify,
    pub(super) mutation: Mutex<()>,
    pub(super) relay_origin: Option<crate::relay_protocol::RelayOrigin>,
    pub(super) relay_status: watch::Receiver<crate::devices::RelaySessionStatus>,
    pub(super) relay_updates: watch::Sender<crate::devices::RelaySessionStatus>,
    pub(super) receipts: watch::Sender<u64>,
    pub(super) draining: AtomicBool,
    pub(super) workers_drained: AtomicBool,
    pub(super) recovery: Mutex<()>,
}

enum HostWorker {
    Local(Result<(), HostError>),
    Dispatcher(Result<(), HostError>),
    Relay(Result<(), DeviceError>),
}

impl HostService {
    pub async fn start(state_directory: impl AsRef<Path>) -> Result<Self, HostError> {
        LocalServer::validate_state_path(state_directory.as_ref())?;
        let lease = StateLease::acquire(state_directory).await?;
        let prepared_lease = Arc::clone(&lease);
        let (
            agent,
            settings,
            devices,
            registry,
            authorizer,
            discovery,
            task_tools,
            relay_origin,
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
            let relay_origin = settings.snapshot()?.relay_origin;
            let agent = Agent::from_leased_store(store.clone(), prepared_lease);
            let mut registry = ToolRegistry::new();
            let mut scopes =
                BuiltinTools::new(BuiltinConfig::default())?.register_into(&mut registry)?;
            let task_tools = SchedulerTools::new();
            scopes.extend(task_tools.register_into(&mut registry)?);
            let catalog_authorizer = Arc::new(Authorizer::new(
                store.clone(),
                Arc::new(registry.clone()),
                scopes.clone(),
            )?);
            let selector = ToolSelector::new(&registry, catalog_authorizer)?;
            let search = Arc::new(SearchTools::new(selector.clone()));
            registry.register(search.clone())?;
            scopes.insert(SEARCH_TOOLS_NAME.into(), ToolScopeRule::LocalReadOnly);
            let registry = Arc::new(registry);
            let authorizer = Arc::new(Authorizer::new(store, registry.clone(), scopes)?);
            Ok::<_, HostError>((
                agent,
                settings,
                devices,
                registry,
                authorizer,
                DiscoveryServices { selector, search },
                task_tools,
                relay_origin,
                preparation_lease,
            ))
        })
        .await
        .map_err(|_| HostError::Worker)??;
        let credentials = CredentialVault::from_environment();
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
        let (server, calls) = LocalServer::bind_with_lease(lease, LocalOptions::default()).await?;
        let scheduler = Scheduler::start(
            SchedulerServices {
                agent: agent.clone(),
                registry: registry.clone(),
                broker: broker.clone(),
                providers: Arc::new(HostProviderFactory::new(
                    settings.clone(),
                    credentials.clone(),
                )),
                discovery: Some(discovery),
            },
            SchedulerLimits::default(),
        )
        .await?;
        let scheduler_binding = match task_tools.bind(&scheduler) {
            Ok(binding) => binding,
            Err(error) => {
                scheduler.shutdown().await?;
                return Err(error.into());
            }
        };
        let (relay_updates, relay_status) =
            watch::channel(crate::devices::RelaySessionStatus::Stopped);
        let (receipts, _) = watch::channel(0);
        let inner = Arc::new(HostInner {
            inbox: HostInbox::new(agent.store()),
            agent,
            settings,
            credentials,
            devices,
            registry,
            authorizer,
            scheduler,
            broker,
            changed: Notify::new(),
            mutation: Mutex::new(()),
            relay_origin,
            relay_status,
            relay_updates,
            receipts,
            draining: AtomicBool::new(false),
            workers_drained: AtomicBool::new(false),
            recovery: Mutex::new(()),
        });
        Ok(Self {
            inner,
            server,
            calls,
            _scheduler_binding: scheduler_binding,
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
            _scheduler_binding,
        } = self;
        let work_stop = cancellation.child_token();
        let local_stop = CancellationToken::new();
        let mut workers = JoinSet::new();
        let local_token = local_stop.clone();
        workers.spawn(async move {
            HostWorker::Local(server.run(local_token).await.map_err(HostError::from))
        });
        let dispatcher = inner.clone();
        let dispatcher_stop = work_stop.clone();
        workers.spawn(
            async move { HostWorker::Dispatcher(dispatcher.dispatch(dispatcher_stop).await) },
        );
        if let Some(origin) = inner.relay_origin.clone() {
            let host = inner.clone();
            let relay_stop = work_stop.clone();
            workers.spawn(async move {
                HostWorker::Relay(
                    host.devices
                        .run_relay(origin, relay_stop, host.relay_updates.clone())
                        .await,
                )
            });
        }
        let mut handlers = JoinSet::new();
        let activation = if cancellation.is_cancelled() {
            Ok(())
        } else {
            inner.scheduler.activate().await.map_err(HostError::from)
        };
        let mut result = if let Err(error) = activation {
            Err(error)
        } else {
            loop {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break Ok(()),
                    joined = workers.join_next() => match joined {
                        Some(Ok(HostWorker::Relay(outcome))) => {
                            if outcome.is_err() {
                                inner.relay_updates.send_replace(crate::devices::RelaySessionStatus::Failed { reason: "relay operation requires operator action" });
                                tracing::warn!("optional relay stopped; local host remains available");
                            }
                        }
                        Some(Ok(HostWorker::Local(outcome) | HostWorker::Dispatcher(outcome))) => break outcome.and(Err(HostError::Worker)),
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
        }
        inner.receipt_changed();
        work_stop.cancel();
        let scheduler_owner = inner.clone();
        let mut shutdown = Some(tokio::spawn(async move {
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
        loop {
            if inner.workers_drained.load(Ordering::Acquire)
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
                if matches!((&report,&running), (Ok(report),Ok(false)) if report.running_count == 0 && report.unresolved_count == 0 && report.requirements.is_empty())
                {
                    break;
                }
                if report.is_err() || running.is_err() {
                    tracing::error!(
                        "cleanup inspection failed; host retains ownership for explicit recovery"
                    );
                }
            }
            tokio::select! {
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
                        Ok(_) => inner.workers_drained.store(true, Ordering::Release),
                        Err(error) => { if result.is_ok() { result = Err(error); } tracing::error!("scheduler drain failed; state ownership retained"); }
                    }
                }
                joined = workers.join_next(), if !workers.is_empty() => {
                    let failure = match joined {
                        Some(Ok(HostWorker::Local(outcome) | HostWorker::Dispatcher(outcome))) => outcome.err(),
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
                Ok(HostWorker::Local(outcome) | HostWorker::Dispatcher(outcome)) => {
                    if result.is_ok() {
                        result = outcome;
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
            LocalRequestPayload::Query { query } => Ok(LocalResponsePayload::QueryResult {
                result: self.query(query.clone()).await?,
            }),
            LocalRequestPayload::OwnerOperation { operation } => {
                Ok(LocalResponsePayload::OwnerOperationResult {
                    result: self
                        .operate(operation.clone(), call.request.request_id)
                        .await?,
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
                let conversation = self.agent.conversation(request.conversation_id).await?;
                if request.after_sequence > conversation.info.last_sequence {
                    return Err(HostError::Invalid("wait cursor exceeds the host head"));
                }
                let timed_out = if conversation.info.last_sequence > request.after_sequence {
                    false
                } else {
                    tokio::select! {
                        _ = call.cancellation.cancelled() => return Err(LocalError::Cancelled.into()),
                        result = tokio::time::timeout(Duration::from_millis(request.timeout_ms), changed.changed()) => {
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
        HostError::Draining => (
            LocalFailureCode::Unavailable,
            "host is draining; inspect status and complete explicit cleanup before restarting",
        ),
        HostError::LegacyForeground => (
            LocalFailureCode::Unavailable,
            "legacy foreground task requires stopping the host and explicitly resuming or settling it through the foreground CLI",
        ),
        HostError::Inbox(InboxError::OriginDenied)
        | HostError::Device(DeviceError::AccessDenied | DeviceError::EndpointRevoked) => (
            LocalFailureCode::PermissionDenied,
            "command origin is no longer authorized",
        ),
        HostError::Inbox(InboxError::Capacity)
        | HostError::Scheduler(SchedulerError::Capacity(_)) => (
            LocalFailureCode::Busy,
            "host command or task capacity is exhausted",
        ),
        HostError::Invalid(message) | HostError::Inbox(InboxError::Invalid(message)) => {
            (LocalFailureCode::InvalidRequest, *message)
        }
        HostError::Inbox(InboxError::Conflict(_))
        | HostError::Settings(SettingsError::Conflict) => (
            LocalFailureCode::InvalidRequest,
            "identity or revision conflicts with existing durable state",
        ),
        HostError::Credential(_) => (
            LocalFailureCode::Unavailable,
            "credential operation failed; inspect native store availability and exact reference",
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
