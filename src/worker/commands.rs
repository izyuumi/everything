use std::{
    collections::BTreeSet,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{ArgGroup, Args, Subcommand};
use rustix::fs::{FileType, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::agent::Agent;
use crate::credentials::{CredentialError, CredentialSecret, NativeCredentialStore};
use crate::crypto::DevicePublicKeys;
use crate::host::client::{HostClient, HostClientError};
use crate::ipc::{
    HostRuntimeState, OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult,
    RelayConnectionState, RelayStatus, TaskExecutionMetadata,
};
use crate::protocol::{CommandId, DeviceId, TaskId, UnixMillis};
use crate::relay_protocol::RelayOrigin;
use crate::scheduler::TaskSnapshot;
use crate::service::ServiceError;
use crate::transport::local::{
    LocalCall, LocalError, LocalFailure, LocalFailureCode, LocalOptions, LocalRequestPayload,
    LocalResponsePayload, LocalServer,
};

use super::repository::WorkerPeerState;
use super::{
    DelegationSnapshot, DelegationState, WorkerEndpoint, WorkerError, WorkerJobBinding,
    WorkerModelBinding, WorkerRejection, WorkerRelayStatus, WorkerResultStatus, WorkerRole,
    WorkerRuntime, WorkerRuntimePhase, load_or_create_worker_identity,
};

const MAX_WORKER_OWNER_REPLY_BYTES: usize = 256 * 1024;
const MAX_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCoordinatorPin {
    pub keys: DevicePublicKeys,
    pub expected_fingerprint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerExecutorConfig {
    pub schema_version: u16,
    pub relay: RelayOrigin,
    pub model: WorkerModelBinding,
    pub coordinators: Vec<WorkerCoordinatorPin>,
}

impl WorkerExecutorConfig {
    pub fn validate(&self) -> Result<(), WorkerError> {
        self.model.validate()?;
        if self.schema_version != 1 || self.coordinators.len() > 16 {
            return Err(WorkerError::Invalid(
                "executor configuration version or peer count",
            ));
        }
        let mut ids = BTreeSet::new();
        let mut signing = BTreeSet::new();
        let mut encryption = BTreeSet::new();
        for pin in &self.coordinators {
            pin.keys.validate()?;
            if pin.expected_fingerprint != pin.keys.fingerprint()
                || !ids.insert(pin.keys.device_id)
                || !signing.insert(pin.keys.signing_key)
                || !encryption.insert(pin.keys.encryption_key)
            {
                return Err(WorkerError::Invalid("executor coordinator confirmation"));
            }
        }
        Ok(())
    }
}

#[derive(Args)]
pub struct WorkerArguments {
    #[command(subcommand)]
    command: WorkerCommand,
}

#[derive(Subcommand)]
enum WorkerCommand {
    Coordinator(crate::host::worker_commands::WorkerCoordinatorArguments),
    Serve {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    Identity,
    Status,
    Inspect(InspectArguments),
    Revoke {
        #[arg(long)]
        peer: DeviceId,
        #[arg(long)]
        request_id: CommandId,
    },
    AcknowledgeCleanup {
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        original_result_digest: String,
        #[arg(long)]
        operator: String,
        #[arg(long)]
        note: String,
        #[arg(long)]
        request_id: CommandId,
    },
    Credential {
        #[command(subcommand)]
        operation: CredentialCommand,
    },
}

#[derive(Args)]
#[command(group(ArgGroup::new("inspection").required(true).args(["task", "delegation", "cleanup"])))]
struct InspectArguments {
    #[arg(long)]
    task: Option<TaskId>,
    #[arg(long)]
    delegation: Option<Uuid>,
    #[arg(long)]
    cleanup: bool,
}

#[derive(Subcommand)]
enum CredentialCommand {
    Store {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        #[arg(long)]
        source_environment_variable: String,
    },
    Delete {
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
}

#[derive(Debug, Error)]
pub enum WorkerCommandError {
    #[error("invalid worker command: {0}")]
    Invalid(&'static str),
    #[error("worker command cancelled before dispatch")]
    Cancelled,
    #[error("worker executor service failed after native cleanup and owned joins")]
    ServiceFailed,
    #[error("worker command operation failed")]
    Worker,
    #[error(transparent)]
    WorkerOperation(#[from] WorkerError),
    #[error(transparent)]
    Agent(#[from] crate::agent::AgentError),
    #[error(transparent)]
    Local(#[from] LocalError),
    #[error(transparent)]
    Client(#[from] HostClientError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Coordinator(#[from] crate::host::worker_commands::WorkerCoordinatorCommandError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerQuery {
    Status,
    Delegation { delegation_id: Uuid },
}

impl WorkerQuery {
    pub fn validate(&self) -> Result<(), &'static str> {
        if matches!(self, Self::Delegation { delegation_id } if delegation_id.is_nil()) {
            return Err("worker delegation identity must not be nil");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerExecutorPhase {
    Starting,
    Running,
    Draining,
    NeedsOwner,
    Stopped,
}

impl From<WorkerRuntimePhase> for WorkerExecutorPhase {
    fn from(phase: WorkerRuntimePhase) -> Self {
        match phase {
            WorkerRuntimePhase::Starting => Self::Starting,
            WorkerRuntimePhase::Running => Self::Running,
            WorkerRuntimePhase::Draining => Self::Draining,
            WorkerRuntimePhase::NeedsOwner => Self::NeedsOwner,
            WorkerRuntimePhase::Stopped => Self::Stopped,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerExecutorStatus {
    pub phase: WorkerExecutorPhase,
    pub relay: RelayStatus,
    pub profile: WorkerModelBinding,
    pub peers: Vec<WorkerPeerState>,
    pub job_count: usize,
    pub needs_owner: bool,
    pub supervisor_join_failed: bool,
    pub failure_code: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDelegationSummary {
    pub binding: WorkerJobBinding,
    pub peer: DeviceId,
    pub state: DelegationState,
    pub cancellation_requested: bool,
    pub cancellation_acknowledged: bool,
    pub rejection: Option<WorkerRejection>,
    pub result_status: Option<WorkerResultStatus>,
    pub result_digest: Option<String>,
    pub native_task_id: Option<TaskId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerQueryResult {
    Status {
        status: Box<WorkerExecutorStatus>,
    },
    Delegation {
        delegation: Option<WorkerDelegationSummary>,
    },
}

impl WorkerQueryResult {
    pub fn validate_for(&self, query: &WorkerQuery) -> Result<(), &'static str> {
        query.validate()?;
        match (query, self) {
            (WorkerQuery::Status, Self::Status { status }) => {
                status
                    .profile
                    .validate()
                    .map_err(|_| "invalid worker model profile")?;
                if status.peers.len() > 16
                    || status.job_count > 256
                    || status.failure_code.as_ref().is_some_and(|code| {
                        code.is_empty()
                            || code.len() > 128
                            || !code
                                .bytes()
                                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
                    })
                {
                    return Err("invalid worker status bounds");
                }
                let mut ids = std::collections::BTreeSet::new();
                for peer in &status.peers {
                    peer.keys
                        .validate()
                        .map_err(|_| "invalid worker peer keys")?;
                    if !ids.insert(peer.keys.device_id)
                        || peer.revoked_at.is_some_and(|time| time < peer.pinned_at)
                    {
                        return Err("invalid worker peer identity or time");
                    }
                }
            }
            (WorkerQuery::Delegation { delegation_id }, Self::Delegation { delegation }) => {
                if let Some(delegation) = delegation {
                    delegation
                        .binding
                        .validate()
                        .map_err(|_| "invalid worker delegation binding")?;
                    if delegation.binding.delegation_id != *delegation_id
                        || delegation.cancellation_acknowledged
                            && !delegation.cancellation_requested
                        || delegation.result_status.is_some() != delegation.result_digest.is_some()
                        || delegation.result_digest.as_ref().is_some_and(|digest| {
                            digest.len() != 64
                                || !digest.bytes().all(|byte| {
                                    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
                                })
                        })
                    {
                        return Err("worker delegation response does not match request");
                    }
                    let has_result = delegation.result_status.is_some();
                    let has_rejection = delegation.rejection.is_some();
                    let valid_terminal = match delegation.state {
                        DelegationState::Offered
                        | DelegationState::Accepted
                        | DelegationState::Running
                        | DelegationState::CancelRequested => !has_result && !has_rejection,
                        DelegationState::ResultReady | DelegationState::Acknowledged => {
                            has_result && !has_rejection
                        }
                        DelegationState::Rejected => has_rejection && !has_result,
                        DelegationState::Revoked => !(has_result && has_rejection),
                    };
                    if !valid_terminal {
                        return Err("worker terminal metadata is inconsistent");
                    }
                }
            }
            _ => return Err("worker response does not match request"),
        }
        if serde_json::to_vec(self)
            .map_err(|_| "invalid worker owner response")?
            .len()
            > MAX_WORKER_OWNER_REPLY_BYTES
        {
            return Err("worker owner response exceeds byte limit");
        }
        Ok(())
    }
}

pub async fn run(
    arguments: WorkerArguments,
    state: PathBuf,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    ensure_not_cancelled(&cancellation)?;
    match arguments.command {
        WorkerCommand::Coordinator(arguments) => {
            Ok(crate::host::worker_commands::run(arguments, state, cancellation).await?)
        }
        WorkerCommand::Serve { config } => {
            let config = read_config(config, cancellation.clone()).await?;
            serve(state, config, cancellation).await
        }
        WorkerCommand::Identity => identity(state, cancellation).await,
        WorkerCommand::Status => {
            query(
                &state,
                OwnerQuery::Worker {
                    query: WorkerQuery::Status,
                },
                cancellation,
            )
            .await
        }
        WorkerCommand::Inspect(arguments) => {
            let request = if let Some(task_id) = arguments.task {
                OwnerQuery::Task { task_id }
            } else if let Some(delegation_id) = arguments.delegation {
                OwnerQuery::Worker {
                    query: WorkerQuery::Delegation { delegation_id },
                }
            } else if arguments.cleanup {
                OwnerQuery::ToolRecovery
            } else {
                return Err(WorkerCommandError::Invalid("select a worker inspection"));
            };
            query(&state, request, cancellation).await
        }
        WorkerCommand::Revoke { peer, request_id } => {
            operate(
                &state,
                OwnerOperation::RevokeWorkerPeer { device_id: peer },
                request_id,
                cancellation,
            )
            .await
        }
        WorkerCommand::AcknowledgeCleanup {
            operation_id,
            original_result_digest,
            operator,
            note,
            request_id,
        } => {
            operate(
                &state,
                OwnerOperation::AcknowledgeCleanup {
                    operation_id,
                    original_result_digest,
                    operator,
                    note,
                },
                request_id,
                cancellation,
            )
            .await
        }
        WorkerCommand::Credential { operation } => credential(state, operation, cancellation).await,
    }
}

fn ensure_not_cancelled(token: &CancellationToken) -> Result<(), WorkerCommandError> {
    if token.is_cancelled() {
        Err(WorkerCommandError::Cancelled)
    } else {
        Ok(())
    }
}

async fn query(
    state: &Path,
    request: OwnerQuery,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let client = if matches!(request, OwnerQuery::Worker { .. }) {
        HostClient::new(state)?
    } else {
        executor_client(state, cancellation.clone()).await?
    };
    serde_json::to_value(client.query(request, cancellation).await?)
        .map_err(|_| WorkerCommandError::Worker)
}

async fn operate(
    state: &Path,
    operation: OwnerOperation,
    request_id: CommandId,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let client = executor_client(state, cancellation.clone()).await?;
    serde_json::to_value(client.operate(operation, request_id, cancellation).await?)
        .map_err(|_| WorkerCommandError::Worker)
}

async fn executor_client(
    state: &Path,
    cancellation: CancellationToken,
) -> Result<HostClient, WorkerCommandError> {
    let client = HostClient::new(state)?;
    client
        .query(
            OwnerQuery::Worker {
                query: WorkerQuery::Status,
            },
            cancellation,
        )
        .await?;
    Ok(client)
}

async fn identity(
    state: PathBuf,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let client = HostClient::new(&state)?;
    match client
        .query(
            OwnerQuery::Worker {
                query: WorkerQuery::Status,
            },
            cancellation.clone(),
        )
        .await
    {
        Ok(_) => serde_json::to_value(client.query(OwnerQuery::Identity, cancellation).await?)
            .map_err(|_| WorkerCommandError::Worker),
        Err(error) if socket_absent(&error) => {
            let agent = Agent::open(state).await?;
            ensure_not_cancelled(&cancellation)?;
            let keys = tokio::task::spawn_blocking(move || {
                WorkerEndpoint::validate_executor_agent(&agent)?;
                Ok::<_, WorkerError>(load_or_create_worker_identity(&agent)?.public_keys())
            })
            .await
            .map_err(|_| WorkerCommandError::Worker)??;
            Ok(json!({"keys":keys,"fingerprint":keys.fingerprint()}))
        }
        Err(error) => Err(error.into()),
    }
}

fn socket_absent(error: &HostClientError) -> bool {
    let error = match error {
        HostClientError::Transport(LocalError::Io(error))
        | HostClientError::Transport(LocalError::Service(ServiceError::Io(error))) => error,
        _ => return false,
    };
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

pub async fn read_config(
    path: PathBuf,
    cancellation: CancellationToken,
) -> Result<WorkerExecutorConfig, WorkerCommandError> {
    ensure_not_cancelled(&cancellation)?;
    tokio::task::spawn_blocking(move || {
        ensure_not_cancelled(&cancellation)?;
        if path.to_str().is_none_or(|value|value.is_empty() || value.len()>4096 || value.chars().any(char::is_control)) {
            return Err(WorkerCommandError::Invalid("invalid executor configuration path"));
        }
        let descriptor = rustix::fs::open(&path,OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,rustix::fs::Mode::empty())
            .map_err(|_|WorkerCommandError::Invalid("executor configuration cannot be opened safely"))?;
        let metadata = rustix::fs::fstat(&descriptor).map_err(|_|WorkerCommandError::Invalid("executor configuration metadata is unavailable"))?;
        if FileType::from_raw_mode(metadata.st_mode)!=FileType::RegularFile
            || metadata.st_uid!=rustix::process::geteuid().as_raw()
            || metadata.st_nlink!=1
            || metadata.st_mode & 0o022 !=0
            || metadata.st_size<0
            || metadata.st_size>MAX_CONFIG_BYTES as i64
        {
            return Err(WorkerCommandError::Invalid("executor configuration requires an owner-controlled single-link regular file within 64 KiB"));
        }
        let mut bytes=Vec::new();
        File::from(descriptor).take(MAX_CONFIG_BYTES as u64+1).read_to_end(&mut bytes)
            .map_err(|_|WorkerCommandError::Invalid("executor configuration cannot be read"))?;
        if bytes.len()>MAX_CONFIG_BYTES {
            return Err(WorkerCommandError::Invalid("executor configuration exceeds 64 KiB"));
        }
        let config:WorkerExecutorConfig=serde_json::from_slice(&bytes)
            .map_err(|_|WorkerCommandError::Invalid("invalid executor configuration encoding"))?;
        config.validate()?;
        ensure_not_cancelled(&cancellation)?;
        Ok(config)
    }).await.map_err(|_|WorkerCommandError::Worker)?
}

async fn open_endpoint(
    state: PathBuf,
    config: &WorkerExecutorConfig,
    cancellation: &CancellationToken,
) -> Result<WorkerEndpoint, WorkerCommandError> {
    ensure_not_cancelled(cancellation)?;
    let agent = Agent::open(state).await?;
    ensure_not_cancelled(cancellation)?;
    let origin = config.relay.clone();
    let endpoint = tokio::task::spawn_blocking(move || {
        WorkerEndpoint::validate_executor_agent(&agent)?;
        let identity = load_or_create_worker_identity(&agent)?;
        WorkerEndpoint::open(agent, identity, WorkerRole::Executor, origin)
    })
    .await
    .map_err(|_| WorkerCommandError::Worker)??;
    let profile = config.model.clone();
    endpoint
        .blocking(move |repository| {
            repository.validate_executor_state()?;
            repository.bind_profile(&profile)
        })
        .await?;
    ensure_not_cancelled(cancellation)?;
    Ok(endpoint)
}

async fn apply_pins(
    endpoint: &WorkerEndpoint,
    config: &WorkerExecutorConfig,
    cancellation: &CancellationToken,
) -> Result<(), WorkerCommandError> {
    let retained = endpoint.peers().await?;
    let own = endpoint.public_keys();
    for existing in &retained {
        let declared = config
            .coordinators
            .iter()
            .find(|pin| pin.keys.device_id == existing.keys.device_id);
        if existing.revoked_at.is_none() && declared.is_none()
            || declared.is_some_and(|pin| pin.keys != existing.keys)
        {
            return Err(WorkerCommandError::Invalid(
                "configuration differs from retained coordinator pins; explicitly revoke omitted active peers",
            ));
        }
    }
    let fresh = config
        .coordinators
        .iter()
        .filter(|pin| {
            !retained
                .iter()
                .any(|peer| peer.keys.device_id == pin.keys.device_id)
        })
        .count();
    if retained.len() + fresh > 16 {
        return Err(WorkerCommandError::Invalid(
            "executor lifetime peer capacity exceeded",
        ));
    }
    for pin in &config.coordinators {
        if pin.keys.device_id == own.device_id
            || pin.keys.signing_key == own.signing_key
            || pin.keys.encryption_key == own.encryption_key
        {
            return Err(WorkerCommandError::Invalid(
                "executor and coordinator purpose identities must differ",
            ));
        }
    }
    for pin in &config.coordinators {
        ensure_not_cancelled(cancellation)?;
        if retained
            .iter()
            .any(|peer| peer.keys.device_id == pin.keys.device_id && peer.revoked_at.is_some())
        {
            continue;
        }
        endpoint
            .pin_peer(
                pin.keys.clone(),
                &pin.expected_fingerprint,
                UnixMillis::now().map_err(WorkerError::from)?,
            )
            .await?;
    }
    Ok(())
}

async fn credential(
    state: PathBuf,
    operation: CredentialCommand,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let owned_cancellation = cancellation.child_token();
    let _cancel_on_drop = owned_cancellation.clone().drop_guard();
    tokio::spawn(credential_owned(state, operation, owned_cancellation))
        .await
        .map_err(|_| WorkerCommandError::Worker)?
}

async fn credential_owned(
    state: PathBuf,
    operation: CredentialCommand,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let (path, source) = match operation {
        CredentialCommand::Store {
            config,
            source_environment_variable,
        } => (config, Some(source_environment_variable)),
        CredentialCommand::Delete { config } => (config, None),
    };
    if source.as_ref().is_some_and(|name| !environment_name(name)) {
        return Err(WorkerCommandError::Invalid(
            "invalid explicit source environment variable",
        ));
    }
    let config = read_config(path, cancellation.clone()).await?;
    let endpoint = open_endpoint(state, &config, &cancellation).await?;
    let key = config
        .model
        .credential_key(endpoint.public_keys().device_id)?;
    ensure_not_cancelled(&cancellation)?;
    let native = NativeCredentialStore::new();
    let action = if let Some(source) = source {
        let value = std::env::var_os(source).ok_or(WorkerCommandError::Invalid(
            "explicit worker credential environment value is absent",
        ))?;
        let mut value = Zeroizing::new(value.into_encoded_bytes());
        let text = std::str::from_utf8(&value).map_err(|_| CredentialError::InvalidSecret)?;
        if text.is_empty() || text.len() > 8192 || text.chars().any(char::is_control) {
            return Err(CredentialError::InvalidSecret.into());
        }
        let secret = CredentialSecret::new(std::mem::take(&mut *value))?;
        ensure_not_cancelled(&cancellation)?;
        native.store(key, secret).await?;
        "stored"
    } else {
        native.delete(key).await?;
        "deleted"
    };
    Ok(
        json!({"credential":action,"purpose":"ai.everything.worker.models","executor":endpoint.public_keys().device_id,"credential_reference":config.model.credential_reference}),
    )
}

fn environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || index > 0 && byte.is_ascii_digit()
        })
}

enum ServiceWorker {
    Local(Result<(), LocalError>),
    Relay(Result<(), WorkerError>),
}

async fn serve(
    state: PathBuf,
    config: WorkerExecutorConfig,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let owned_cancellation = cancellation.child_token();
    let _cancel_on_drop = owned_cancellation.clone().drop_guard();
    tokio::spawn(serve_owned(state, config, owned_cancellation))
        .await
        .map_err(|_| WorkerCommandError::Worker)?
}

async fn serve_owned(
    state: PathBuf,
    config: WorkerExecutorConfig,
    cancellation: CancellationToken,
) -> Result<Value, WorkerCommandError> {
    let endpoint = open_endpoint(state, &config, &cancellation).await?;
    apply_pins(&endpoint, &config, &cancellation).await?;
    let lease = endpoint
        .agent
        .state_lease()
        .ok_or(WorkerError::LeaseRequired)?;
    let (server, mut calls) =
        LocalServer::bind_with_lease(lease.clone(), LocalOptions::default()).await?;
    ensure_not_cancelled(&cancellation)?;
    let mut runtime = WorkerRuntime::start(endpoint.clone(), config.model.clone()).await?;
    let mut phase = runtime.subscribe();
    let local_stop = CancellationToken::new();
    let relay_stop = CancellationToken::new();
    let mut draining = cancellation.is_cancelled();
    if draining {
        runtime.request_shutdown();
        relay_stop.cancel();
    }
    let (relay_updates, relay_status) = watch::channel(WorkerRelayStatus::default());
    let mut workers = JoinSet::new();
    let server_stop = local_stop.clone();
    workers.spawn(async move { ServiceWorker::Local(server.run(server_stop).await) });
    let relay_endpoint = endpoint.clone();
    let relay_token = relay_stop.clone();
    workers.spawn(async move {
        ServiceWorker::Relay(
            relay_endpoint
                .run_relay(None, relay_token, relay_updates)
                .await,
        )
    });
    let mut failed = false;
    let mut join_failed = false;
    let mut phase_open = true;
    let mut local_live = true;
    let mut rebind_at = tokio::time::Instant::now();
    loop {
        if runtime.failure().is_some() {
            failed = true;
        }
        if runtime.phase() == WorkerRuntimePhase::Stopped && !join_failed {
            match runtime.join().await {
                Ok(()) => break,
                Err(_) => {
                    failed = true;
                    join_failed = true;
                    draining = true;
                    relay_stop.cancel();
                    tracing::error!(
                        "worker supervisor join failed; owner endpoint and state ownership retained"
                    );
                }
            }
        }
        tokio::select! {
            biased;
            _=cancellation.cancelled(),if !draining => {
                draining=true;
                runtime.request_shutdown();
                relay_stop.cancel();
            }
            changed=phase.changed(),if phase_open => {
                if changed.is_err() {
                    phase_open=false;
                    if runtime.phase()!=WorkerRuntimePhase::Stopped {
                        draining=true;
                        failed=true;
                        relay_stop.cancel();
                        if runtime.join().await.is_err() {
                            join_failed=true;
                            tracing::error!("worker supervisor stopped unexpectedly; owner endpoint and state ownership retained");
                        }
                    }
                } else if runtime.phase()==WorkerRuntimePhase::NeedsOwner {
                    draining=true;
                    runtime.request_shutdown();
                    relay_stop.cancel();
                    tracing::warn!("worker native cleanup needs explicit owner inspection; owner endpoint remains available");
                }
            }
            joined=workers.join_next(),if !workers.is_empty() => {
                match joined {
                    Some(Ok(ServiceWorker::Relay(result))) => {
                        if !relay_stop.is_cancelled() || result.is_err() {
                            failed=true;
                            draining=true;
                            runtime.request_shutdown();
                            relay_stop.cancel();
                        }
                    }
                    Some(Ok(ServiceWorker::Local(result))) => {
                        if result.is_err() || !local_stop.is_cancelled() {
                            failed=true;
                        }
                        local_live=false;
                        draining=true;
                        runtime.request_shutdown();
                        relay_stop.cancel();
                        rebind_at=tokio::time::Instant::now()+Duration::from_secs(1);
                    }
                    _ => {
                        failed=true;
                        draining=true;
                        runtime.request_shutdown();
                        relay_stop.cancel();
                    }
                }
            }
            call=calls.recv(),if local_live => {
                if let Some(call)=call {
                    handle_call(&endpoint,&runtime,&config,&relay_status,draining,join_failed,call).await;
                } else {
                    local_live=false;
                    rebind_at=tokio::time::Instant::now()+Duration::from_secs(1);
                }
            }
            _=tokio::time::sleep_until(rebind_at),if !local_live => {
                match LocalServer::bind_with_lease(lease.clone(),LocalOptions::default()).await {
                    Ok((server,receiver)) => {
                        calls=receiver;
                        local_live=true;
                        let stop=local_stop.clone();
                        workers.spawn(async move { ServiceWorker::Local(server.run(stop).await) });
                    }
                    Err(_) => {
                        rebind_at=tokio::time::Instant::now()+Duration::from_secs(5);
                        tracing::error!("worker recovery endpoint cannot bind; native state ownership retained");
                    }
                }
            }
        }
    }
    local_stop.cancel();
    relay_stop.cancel();
    while let Some(joined) = workers.join_next().await {
        match joined {
            Ok(ServiceWorker::Local(Ok(())) | ServiceWorker::Relay(Ok(()))) => {}
            _ => failed = true,
        }
    }
    if failed {
        Err(WorkerCommandError::ServiceFailed)
    } else {
        Ok(
            json!({"worker":"stopped","supervisor_joined":true,"local_endpoint_joined":true,"relay_joined":true}),
        )
    }
}

async fn handle_call(
    endpoint: &WorkerEndpoint,
    runtime: &WorkerRuntime,
    config: &WorkerExecutorConfig,
    relay: &watch::Receiver<WorkerRelayStatus>,
    draining: bool,
    join_failed: bool,
    call: LocalCall,
) {
    let result = if call.cancellation.is_cancelled() {
        Err(WorkerCommandError::Cancelled)
    } else {
        handle_payload(
            endpoint,
            runtime,
            config,
            relay,
            draining,
            join_failed,
            &call,
        )
        .await
    };
    let response = result.unwrap_or_else(|error| {
        let (code, message) = match error {
            WorkerCommandError::Cancelled => (
                LocalFailureCode::Cancelled,
                "worker owner request was cancelled",
            ),
            WorkerCommandError::Invalid(message) => (LocalFailureCode::InvalidRequest, message),
            _ => (
                LocalFailureCode::Unavailable,
                "worker owner request failed; inspect runtime and cleanup state",
            ),
        };
        LocalResponsePayload::Rejected {
            error: LocalFailure {
                code,
                message: message.into(),
            },
        }
    });
    if let Err(error) = call.respond(response)
        && !matches!(error, LocalError::Disconnected)
    {
        tracing::warn!("worker owner reply could not be delivered");
    }
}

async fn handle_payload(
    endpoint: &WorkerEndpoint,
    runtime: &WorkerRuntime,
    config: &WorkerExecutorConfig,
    relay: &watch::Receiver<WorkerRelayStatus>,
    draining: bool,
    join_failed: bool,
    call: &LocalCall,
) -> Result<LocalResponsePayload, WorkerCommandError> {
    let result = match &call.request.payload {
        LocalRequestPayload::Query { query } => {
            let result = match query {
                OwnerQuery::Worker { query } => OwnerQueryResult::Worker {
                    result: worker_query(endpoint, runtime, config, relay, join_failed, query)
                        .await?,
                },
                OwnerQuery::Identity => {
                    let keys = endpoint.public_keys();
                    OwnerQueryResult::Identity {
                        fingerprint: keys.fingerprint(),
                        keys,
                    }
                }
                OwnerQuery::Runtime => {
                    let report = runtime.cleanup_report().await?;
                    OwnerQueryResult::Runtime {
                        state: HostRuntimeState {
                            draining,
                            workers_drained: runtime.phase() == WorkerRuntimePhase::Stopped
                                && !join_failed,
                            broker_quarantined: report.quarantined,
                        },
                    }
                }
                OwnerQuery::Transport => OwnerQueryResult::Transport {
                    relay: relay_view(endpoint, relay, draining),
                },
                OwnerQuery::ToolRecovery => {
                    let report = runtime.cleanup_report().await?;
                    let agent = endpoint.agent.clone();
                    let (running_operations,more_running)=tokio::task::spawn_blocking(move || {
                        let store=agent.store();
                        let connection=store.connection()?;
                        let mut statement=connection.prepare("SELECT id FROM tool_calls WHERE state='running' ORDER BY id LIMIT 129").map_err(crate::storage::StorageError::from)?;
                        let mut ids=statement.query_map([],|row|row.get::<_,String>(0)).map_err(crate::storage::StorageError::from)?
                            .collect::<Result<Vec<_>,_>>().map_err(crate::storage::StorageError::from)?;
                        let more=ids.len()>128;
                        ids.truncate(128);
                        Ok::<_,WorkerError>((ids,more))
                    }).await.map_err(|_|WorkerCommandError::Worker)??;
                    OwnerQueryResult::ToolRecovery {
                        report,
                        running_operations,
                        more_running,
                    }
                }
                OwnerQuery::Task { task_id } => {
                    let job = endpoint
                        .jobs()
                        .await?
                        .into_iter()
                        .find(|job| {
                            job.manifest
                                .as_ref()
                                .is_some_and(|manifest| manifest.ids.task_id == *task_id)
                        })
                        .ok_or(WorkerCommandError::Invalid(
                            "task is not owned by this worker",
                        ))?;
                    let snapshot = owned_native_task(endpoint, &job).await?.ok_or(
                        WorkerCommandError::Invalid("worker native task is not admitted"),
                    )?;
                    OwnerQueryResult::Task {
                        task: snapshot.task,
                        execution: TaskExecutionMetadata {
                            active_command_id: snapshot.active_command_id,
                            queued_messages: snapshot.queued_messages,
                            cancel_requested: snapshot.cancel_requested,
                            waiting: snapshot.waiting,
                            diagnostic: snapshot.diagnostic,
                            live_text: None,
                        },
                    }
                }
                _ => {
                    return Err(WorkerCommandError::Invalid(
                        "this executor serves only worker identity, runtime, transport, task and cleanup inspection",
                    ));
                }
            };
            LocalResponsePayload::QueryResult { result }
        }
        LocalRequestPayload::OwnerOperation { operation } => {
            let result = match operation {
                OwnerOperation::AcknowledgeCleanup {
                    operation_id,
                    original_result_digest,
                    operator,
                    note,
                } => OwnerOperationResult::ToolRecovery {
                    report: runtime
                        .acknowledge_cleanup(operation_id, original_result_digest, operator, note)
                        .await?,
                },
                OwnerOperation::RevokeWorkerPeer { device_id } => {
                    endpoint
                        .revoke_peer(
                            *device_id,
                            call.request.request_id,
                            UnixMillis::now().map_err(WorkerError::from)?,
                        )
                        .await?;
                    OwnerOperationResult::Applied
                }
                _ => {
                    return Err(WorkerCommandError::Invalid(
                        "this executor permits only explicit worker revocation and exact native cleanup acknowledgment",
                    ));
                }
            };
            LocalResponsePayload::OwnerOperationResult { result }
        }
        _ => {
            return Err(WorkerCommandError::Invalid(
                "ordinary host commands and synchronization are unavailable on an executor",
            ));
        }
    };
    Ok(result)
}

fn relay_view(
    endpoint: &WorkerEndpoint,
    relay: &watch::Receiver<WorkerRelayStatus>,
    draining: bool,
) -> RelayStatus {
    let state = *relay.borrow();
    let connection = if state.connected {
        RelayConnectionState::Connected
    } else if draining {
        RelayConnectionState::Stopped
    } else if state.retrying {
        RelayConnectionState::Retrying
    } else {
        RelayConnectionState::Connecting
    };
    RelayStatus {
        origin: Some(endpoint.origin().clone()),
        connection,
    }
}

async fn worker_query(
    endpoint: &WorkerEndpoint,
    runtime: &WorkerRuntime,
    config: &WorkerExecutorConfig,
    relay: &watch::Receiver<WorkerRelayStatus>,
    join_failed: bool,
    query: &WorkerQuery,
) -> Result<WorkerQueryResult, WorkerCommandError> {
    query.validate().map_err(WorkerCommandError::Invalid)?;
    let result = match query {
        WorkerQuery::Status => {
            let report = runtime.cleanup_report().await?;
            WorkerQueryResult::Status {
                status: Box::new(WorkerExecutorStatus {
                    phase: if join_failed {
                        WorkerExecutorPhase::NeedsOwner
                    } else {
                        runtime.phase().into()
                    },
                    relay: relay_view(
                        endpoint,
                        relay,
                        matches!(
                            runtime.phase(),
                            WorkerRuntimePhase::Draining
                                | WorkerRuntimePhase::NeedsOwner
                                | WorkerRuntimePhase::Stopped
                        ),
                    ),
                    profile: config.model.clone(),
                    peers: endpoint.peers().await?,
                    job_count: endpoint.jobs().await?.len(),
                    needs_owner: join_failed
                        || report.quarantined
                        || runtime.phase() == WorkerRuntimePhase::NeedsOwner,
                    supervisor_join_failed: join_failed,
                    failure_code: runtime.failure().map(|_| "native_runtime_failed".into()),
                }),
            }
        }
        WorkerQuery::Delegation { delegation_id } => {
            let delegation = if let Some(job) = endpoint.snapshot(*delegation_id).await? {
                let native_task_id = owned_native_task(endpoint, &job)
                    .await?
                    .map(|snapshot| snapshot.task.id);
                Some(WorkerDelegationSummary {
                    binding: job.binding,
                    peer: job.peer,
                    state: job.state,
                    cancellation_requested: job.cancellation_requested,
                    cancellation_acknowledged: job.cancellation_acknowledged,
                    rejection: job.rejection,
                    result_status: job.result.as_ref().map(|result| result.status),
                    result_digest: job.result_digest,
                    native_task_id,
                })
            } else {
                None
            };
            WorkerQueryResult::Delegation { delegation }
        }
    };
    result
        .validate_for(query)
        .map_err(WorkerCommandError::Invalid)?;
    Ok(result)
}

async fn owned_native_task(
    endpoint: &WorkerEndpoint,
    job: &DelegationSnapshot,
) -> Result<Option<TaskSnapshot>, WorkerCommandError> {
    let Some(manifest) = job.manifest.clone() else {
        return Ok(None);
    };
    let agent = endpoint.agent.clone();
    endpoint
        .blocking(move |repository| {
            let Some(submission) = repository.native_submission(&manifest)? else {
                return Ok(None);
            };
            let store = agent.store();
            if store.task(&submission.task_id.to_string())?.is_none() {
                return Ok(None);
            }
            let snapshot = crate::scheduler::task_snapshot(&store, submission.task_id)?;
            if snapshot.task.id != submission.task_id
                || snapshot.task.conversation_id != submission.conversation_id
                || snapshot.task.parent_task_id.is_some()
                || snapshot.active_command_id != submission.command_id
                || snapshot.task.created_at != submission.accepted_at
            {
                return Err(WorkerError::Conflict(
                    "worker native admission differs from task",
                ));
            }
            Ok(Some(snapshot))
        })
        .await
        .map_err(WorkerCommandError::from)
}
