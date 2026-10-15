use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::agent::Agent;
use crate::credentials::{CredentialError, CredentialKey, CredentialSecret};
use crate::permissions::ToolScopeRule;
use crate::tools::ToolRegistry;
use crate::tools::mcp::{McpError, McpHost, McpLimits, McpSession, McpStdioConfig};

use super::settings::CredentialStorage;

const MAX_SETTINGS_BYTES: usize = 48 * 1024;
const MAX_SERVERS: usize = 16;
const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(25);

#[derive(Debug, Error)]
pub enum McpServiceError {
    #[error("invalid MCP host settings: {0}")]
    Configuration(&'static str),
    #[error("MCP host startup was cancelled")]
    Cancelled,
    #[error("MCP host is shutting down")]
    Stopping,
    #[error("MCP process cleanup remains unconfirmed; host ownership is retained")]
    CleanupUnconfirmed,
    #[error("MCP host cleanup is still in progress")]
    ShutdownPending,
    #[error("MCP host worker failed")]
    Worker,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    pub servers: Vec<McpServerSettings>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerSettings {
    pub server_id: String,
    pub executable: PathBuf,
    pub working_directory: PathBuf,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, McpEnvironmentReference>,
    #[serde(default)]
    pub limits: McpTimeoutSettings,
    #[serde(default)]
    pub launch_authorization: Option<McpLaunchAuthorization>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpLaunchAuthorization {
    FullHost { manifest_fingerprint: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEnvironmentReference {
    pub reference: Uuid,
    pub storage: CredentialStorage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpEnvironmentBinding {
    pub server_id: String,
    pub manifest_fingerprint: String,
    pub variable: String,
    pub reference: Uuid,
    pub storage: CredentialStorage,
}

impl McpEnvironmentBinding {
    pub fn scope_key(&self) -> Result<String, CredentialError> {
        if !valid_server_id(&self.server_id)
            || !valid_environment_name(&self.variable)
            || self.reference.is_nil()
            || self.manifest_fingerprint.len() != 64
            || !self
                .manifest_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CredentialError::InvalidKey);
        }
        let encoded = serde_json::to_vec(self).map_err(|_| CredentialError::InvalidKey)?;
        let mut digest = Sha256::new();
        digest.update(b"everything.mcp.environment.v1\0");
        digest.update(encoded);
        Ok(format!("{:x}", digest.finalize()))
    }

    pub fn credential_key(&self) -> Result<CredentialKey, CredentialError> {
        CredentialKey::new("ai.everything.mcp.environment", &self.scope_key()?)
    }
}

#[async_trait]
pub trait McpEnvironmentResolver: Send + Sync {
    async fn resolve(
        &self,
        binding: &McpEnvironmentBinding,
        cancellation: CancellationToken,
    ) -> Result<Option<Arc<CredentialSecret>>, CredentialError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpTimeoutSettings {
    pub startup_ms: u64,
    pub request_ms: u64,
    pub cleanup_ms: u64,
}

impl Default for McpTimeoutSettings {
    fn default() -> Self {
        Self {
            startup_ms: 30_000,
            request_ms: 30_000,
            cleanup_ms: 5_000,
        }
    }
}

impl McpTimeoutSettings {
    fn native(&self) -> Result<McpLimits, McpServiceError> {
        if !(1..=300_000).contains(&self.startup_ms)
            || !(1..=300_000).contains(&self.request_ms)
            || !(100..=10_000).contains(&self.cleanup_ms)
        {
            return Err(McpServiceError::Configuration("deadlines are out of range"));
        }
        Ok(McpLimits {
            startup_timeout: Duration::from_millis(self.startup_ms),
            request_timeout: Duration::from_millis(self.request_ms),
            cleanup_timeout: Duration::from_millis(self.cleanup_ms),
        })
    }
}

impl McpSettings {
    pub fn validate(&self) -> Result<(), McpServiceError> {
        if self.servers.len() > MAX_SERVERS {
            return Err(McpServiceError::Configuration(
                "at most 16 servers are supported",
            ));
        }
        let mut ids = BTreeSet::new();
        for server in &self.servers {
            server.validate()?;
            if !ids.insert(&server.server_id) {
                return Err(McpServiceError::Configuration("server IDs must be unique"));
            }
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|_| McpServiceError::Configuration("settings cannot be encoded"))?;
        if encoded.len() > MAX_SETTINGS_BYTES {
            return Err(McpServiceError::Configuration("settings exceed 48 KiB"));
        }
        Ok(())
    }
}

impl McpServerSettings {
    pub fn manifest_fingerprint(&self) -> Result<String, McpServiceError> {
        self.validate_manifest()?;
        #[derive(Serialize)]
        struct Manifest<'a> {
            server_id: &'a str,
            executable: &'a PathBuf,
            working_directory: &'a PathBuf,
            arguments: &'a [String],
            environment: &'a BTreeMap<String, McpEnvironmentReference>,
            limits: &'a McpTimeoutSettings,
        }
        let encoded = serde_json::to_vec(&Manifest {
            server_id: &self.server_id,
            executable: &self.executable,
            working_directory: &self.working_directory,
            arguments: &self.arguments,
            environment: &self.environment,
            limits: &self.limits,
        })
        .map_err(|_| McpServiceError::Configuration("manifest cannot be encoded"))?;
        let mut digest = Sha256::new();
        digest.update(b"everything.host.mcp.manifest.v1\0");
        digest.update(encoded);
        Ok(format!("{:x}", digest.finalize()))
    }

    pub fn environment_binding(
        &self,
        variable: &str,
    ) -> Result<McpEnvironmentBinding, McpServiceError> {
        let reference = self
            .environment
            .get(variable)
            .ok_or(McpServiceError::Configuration(
                "environment reference is not configured",
            ))?;
        Ok(McpEnvironmentBinding {
            server_id: self.server_id.clone(),
            manifest_fingerprint: self.manifest_fingerprint()?,
            variable: variable.to_owned(),
            reference: reference.reference,
            storage: reference.storage,
        })
    }

    fn validate(&self) -> Result<(), McpServiceError> {
        let fingerprint = self.manifest_fingerprint()?;
        if let Some(McpLaunchAuthorization::FullHost {
            manifest_fingerprint,
        }) = &self.launch_authorization
            && manifest_fingerprint != &fingerprint
        {
            return Err(McpServiceError::Configuration(
                "launch approval does not match the manifest",
            ));
        }
        Ok(())
    }

    fn validate_manifest(&self) -> Result<(), McpServiceError> {
        if !valid_server_id(&self.server_id) {
            return Err(McpServiceError::Configuration("invalid server ID"));
        }
        if self.arguments.len() > 64
            || self.arguments.iter().map(String::len).sum::<usize>() > 32 * 1024
            || self.arguments.iter().any(|value| value.contains('\0'))
        {
            return Err(McpServiceError::Configuration(
                "invalid non-secret arguments",
            ));
        }
        for path in [&self.executable, &self.working_directory] {
            if !path.is_absolute()
                || path
                    .to_str()
                    .is_none_or(|value| value.len() > 4096 || value.contains('\0'))
            {
                return Err(McpServiceError::Configuration(
                    "paths require absolute UTF-8 names of at most 4096 bytes",
                ));
            }
        }
        if self.environment.len() > 64
            || self.environment.iter().any(|(name, reference)| {
                !valid_environment_name(name) || reference.reference.is_nil()
            })
        {
            return Err(McpServiceError::Configuration(
                "invalid environment references",
            ));
        }
        self.limits.native()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServicePhase {
    Starting,
    Ready,
    Stopping,
    CleanupUnconfirmed,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerPhase {
    AwaitingAuthorization,
    Starting,
    Connected,
    Unavailable,
    Stopping,
    CleanupUnconfirmed,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpFailure {
    CredentialUnavailable,
    InvalidEnvironment,
    Process,
    Disconnected,
    StaleCatalog,
    Cancelled,
    Deadline,
    Protocol,
    Capacity,
    Cleanup,
    Registry,
    Worker,
    UnsupportedPlatform,
    Configuration,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerStatus {
    pub server_id: String,
    pub manifest_fingerprint: String,
    pub phase: McpServerPhase,
    pub process_id: Option<u32>,
    pub tool_names: Vec<String>,
    pub failure: Option<McpFailure>,
    pub restart_required: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServiceStatus {
    pub phase: McpServicePhase,
    pub servers: Vec<McpServerStatus>,
}

struct Entry {
    status: McpServerStatus,
    session: Option<McpSession>,
}

struct State {
    phase: McpServicePhase,
    entries: Vec<Entry>,
}

struct ServiceOwner {
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
    changed: watch::Receiver<u64>,
}

impl Drop for ServiceOwner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Clone)]
pub struct McpService {
    owner: Arc<ServiceOwner>,
}

impl McpService {
    pub fn start(
        settings: McpSettings,
        resolver: Arc<dyn McpEnvironmentResolver>,
        agent: Agent,
        cancellation: CancellationToken,
    ) -> Result<Self, McpServiceError> {
        if agent.state_directory().is_none() {
            return Err(McpServiceError::Configuration(
                "a leased Agent is required for MCP host ownership",
            ));
        }
        settings.validate()?;
        let host = McpHost::new(settings.servers.len().max(1))
            .map_err(|_| McpServiceError::Configuration("invalid process admission limit"))?;
        let entries = settings
            .servers
            .iter()
            .map(|server| {
                Ok(Entry {
                    status: McpServerStatus {
                        server_id: server.server_id.clone(),
                        manifest_fingerprint: server.manifest_fingerprint()?,
                        phase: if server.launch_authorization.is_some() {
                            McpServerPhase::Starting
                        } else {
                            McpServerPhase::AwaitingAuthorization
                        },
                        process_id: None,
                        tool_names: Vec::new(),
                        failure: None,
                        restart_required: false,
                    },
                    session: None,
                })
            })
            .collect::<Result<Vec<_>, McpServiceError>>()?;
        let state = Arc::new(Mutex::new(State {
            phase: McpServicePhase::Starting,
            entries,
        }));
        let stop = cancellation.child_token();
        let (sender, changed) = watch::channel(0_u64);
        let worker_state = Arc::clone(&state);
        let worker_stop = stop.clone();
        tokio::runtime::Handle::try_current()
            .map_err(|_| McpServiceError::Worker)?
            .spawn(async move {
                run_service(
                    host,
                    settings,
                    resolver,
                    agent,
                    worker_state,
                    worker_stop,
                    sender,
                )
                .await;
            });
        Ok(Self {
            owner: Arc::new(ServiceOwner {
                state,
                stop,
                changed,
            }),
        })
    }

    pub async fn ready(&self, cancellation: CancellationToken) -> Result<(), McpServiceError> {
        let mut changed = self.owner.changed.clone();
        loop {
            changed.borrow_and_update();
            match self.owner.state.lock().await.phase {
                McpServicePhase::Ready => return Ok(()),
                McpServicePhase::Starting => {}
                _ => return Err(McpServiceError::Stopping),
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(McpServiceError::Cancelled),
                result = changed.changed() => result.map_err(|_| McpServiceError::Worker)?,
            }
        }
    }

    pub async fn status(&self) -> McpServiceStatus {
        let mut state = self.owner.state.lock().await;
        for entry in &mut state.entries {
            if entry.status.phase == McpServerPhase::Connected
                && let Some(session) = &entry.session
                && let Err(error) = session.check_connected()
            {
                entry.status.phase = McpServerPhase::Unavailable;
                entry.status.failure = Some(classify(&error));
                entry.status.restart_required = true;
            }
        }
        McpServiceStatus {
            phase: state.phase,
            servers: state
                .entries
                .iter()
                .map(|entry| entry.status.clone())
                .collect(),
        }
    }

    pub async fn register(
        &self,
        registry: ToolRegistry,
    ) -> Result<(ToolRegistry, BTreeMap<String, ToolScopeRule>), McpServiceError> {
        self.ready(CancellationToken::new()).await?;
        let sessions = {
            let state = self.owner.state.lock().await;
            if state.phase != McpServicePhase::Ready {
                return Err(McpServiceError::Stopping);
            }
            state
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    entry
                        .session
                        .as_ref()
                        .map(|session| (index, session.clone()))
                })
                .collect::<Vec<_>>()
        };
        let (registry, scopes, failures) = tokio::task::spawn_blocking(move || {
            let mut registry = registry;
            let mut scopes = BTreeMap::new();
            let mut failures = Vec::new();
            for (index, session) in sessions {
                match session.register(&mut registry) {
                    Ok(registered) => scopes.extend(registered),
                    Err(error) => failures.push((index, classify(&error))),
                }
            }
            (registry, scopes, failures)
        })
        .await
        .map_err(|_| McpServiceError::Worker)?;
        let mut state = self.owner.state.lock().await;
        if self.owner.stop.is_cancelled() || state.phase != McpServicePhase::Ready {
            return Err(McpServiceError::Stopping);
        }
        for (index, failure) in failures {
            let entry = &mut state.entries[index];
            entry.status.phase = McpServerPhase::Unavailable;
            entry.status.failure = Some(failure);
            entry.status.restart_required = true;
        }
        Ok((registry, scopes))
    }

    pub async fn shutdown(&self) -> Result<(), McpServiceError> {
        self.owner.stop.cancel();
        let wait = async {
            let mut changed = self.owner.changed.clone();
            loop {
                changed.borrow_and_update();
                match self.owner.state.lock().await.phase {
                    McpServicePhase::Stopped => return Ok(()),
                    McpServicePhase::CleanupUnconfirmed => {
                        return Err(McpServiceError::CleanupUnconfirmed);
                    }
                    _ => {}
                }
                changed
                    .changed()
                    .await
                    .map_err(|_| McpServiceError::Worker)?;
            }
        };
        tokio::time::timeout(SHUTDOWN_WAIT, wait)
            .await
            .map_err(|_| McpServiceError::ShutdownPending)?
    }
}

async fn run_service(
    host: McpHost,
    settings: McpSettings,
    resolver: Arc<dyn McpEnvironmentResolver>,
    agent: Agent,
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
    changed: watch::Sender<u64>,
) {
    let _agent = agent;
    let mut unidentified_cleanup = false;
    let connections = stream::iter(
        settings
            .servers
            .into_iter()
            .enumerate()
            .filter(|(_, server)| server.launch_authorization.is_some()),
    )
    .map(|(index, server)| {
        let host = &host;
        let resolver = resolver.as_ref();
        let cancellation = stop.child_token();
        async move {
            (
                index,
                connect_server(host, server, resolver, cancellation).await,
            )
        }
    })
    .buffer_unordered(4);
    tokio::pin!(connections);
    while let Some((index, result)) = connections.next().await {
        let mut state = state.lock().await;
        let entry = &mut state.entries[index];
        match result {
            Ok(session) => {
                entry.status.phase = McpServerPhase::Connected;
                entry.status.process_id = Some(session.process_id());
                entry.status.tool_names = session
                    .catalog()
                    .tools
                    .iter()
                    .map(|tool| tool.native_name.clone())
                    .collect();
                entry.session = Some(session);
            }
            Err(failure) => {
                unidentified_cleanup |= failure == McpFailure::Cleanup;
                entry.status.phase = if failure == McpFailure::Cleanup {
                    McpServerPhase::CleanupUnconfirmed
                } else {
                    McpServerPhase::Unavailable
                };
                entry.status.failure = Some(failure);
                entry.status.restart_required = true;
            }
        }
        changed.send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    {
        let mut state = state.lock().await;
        state.phase = McpServicePhase::Ready;
    }
    changed.send_modify(|revision| *revision = revision.wrapping_add(1));
    stop.cancelled().await;
    loop {
        let sessions = {
            let mut state = state.lock().await;
            state.phase = McpServicePhase::Stopping;
            state
                .entries
                .iter_mut()
                .enumerate()
                .filter_map(|(index, entry)| {
                    entry.session.as_ref().map(|session| {
                        entry.status.phase = McpServerPhase::Stopping;
                        (index, session.clone())
                    })
                })
                .collect::<Vec<_>>()
        };
        changed.send_modify(|revision| *revision = revision.wrapping_add(1));
        let cleanups = stream::iter(sessions)
            .map(|(index, session)| async move { (index, session.shutdown().await) })
            .buffer_unordered(MAX_SERVERS);
        tokio::pin!(cleanups);
        while let Some((index, result)) = cleanups.next().await {
            let mut state = state.lock().await;
            let entry = &mut state.entries[index];
            match result {
                Ok(()) => {
                    entry.session = None;
                    entry.status.phase = McpServerPhase::Stopped;
                }
                Err(error) => {
                    entry.status.phase = McpServerPhase::CleanupUnconfirmed;
                    entry.status.failure = Some(classify(&error));
                }
            }
        }
        let clean = {
            let mut state = state.lock().await;
            let clean =
                !unidentified_cleanup && state.entries.iter().all(|entry| entry.session.is_none());
            state.phase = if clean {
                McpServicePhase::Stopped
            } else {
                McpServicePhase::CleanupUnconfirmed
            };
            if clean {
                for entry in &mut state.entries {
                    entry.status.phase = McpServerPhase::Stopped;
                }
            }
            clean
        };
        changed.send_modify(|revision| *revision = revision.wrapping_add(1));
        if clean {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connect_server(
    host: &McpHost,
    server: McpServerSettings,
    resolver: &dyn McpEnvironmentResolver,
    cancellation: CancellationToken,
) -> Result<McpSession, McpFailure> {
    let mut limits = server
        .limits
        .native()
        .map_err(|_| McpFailure::Configuration)?;
    let deadline = tokio::time::Instant::now() + limits.startup_timeout;
    let mut environment = BTreeMap::new();
    let mut bytes = 0_usize;
    for variable in server.environment.keys() {
        let binding = server
            .environment_binding(variable)
            .map_err(|_| McpFailure::Configuration)?;
        let resolve = resolver.resolve(&binding, cancellation.child_token());
        let secret = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(McpFailure::Cancelled),
            result = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + CREDENTIAL_TIMEOUT), resolve) => {
                result.map_err(|_| McpFailure::CredentialUnavailable)?
                    .map_err(|_| McpFailure::CredentialUnavailable)?
                    .ok_or(McpFailure::CredentialUnavailable)?
            }
        };
        let value =
            std::str::from_utf8(secret.expose()).map_err(|_| McpFailure::InvalidEnvironment)?;
        bytes = bytes
            .saturating_add(variable.len())
            .saturating_add(value.len());
        if value.contains('\0') || bytes > 32 * 1024 {
            return Err(McpFailure::InvalidEnvironment);
        }
        environment.insert(variable.clone(), Zeroizing::new(value.to_owned()));
    }
    let environment = environment
        .into_iter()
        .map(|(name, mut value)| (name, std::mem::take(&mut *value)))
        .collect();
    limits.startup_timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
    if limits.startup_timeout < Duration::from_millis(1) {
        return Err(McpFailure::Deadline);
    }
    let config = McpStdioConfig::new(
        server.server_id,
        server.executable,
        server.working_directory,
    )
    .and_then(|config| config.with_arguments(server.arguments))
    .and_then(|config| config.with_environment(environment))
    .and_then(|config| config.with_limits(limits))
    .map_err(|error| classify(&error))?;
    host.connect(config, cancellation)
        .await
        .map_err(|error| classify(&error))
}

fn valid_server_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn classify(error: &McpError) -> McpFailure {
    match error {
        McpError::Configuration(_) => McpFailure::Configuration,
        McpError::Capacity | McpError::Busy => McpFailure::Capacity,
        McpError::Process => McpFailure::Process,
        McpError::Disconnected => McpFailure::Disconnected,
        McpError::StaleCatalog => McpFailure::StaleCatalog,
        McpError::Cancelled => McpFailure::Cancelled,
        McpError::Deadline => McpFailure::Deadline,
        McpError::Protocol(_) | McpError::ResourceLimit(_) => McpFailure::Protocol,
        McpError::Cleanup => McpFailure::Cleanup,
        McpError::Worker => McpFailure::Worker,
        McpError::UnsupportedPlatform => McpFailure::UnsupportedPlatform,
        McpError::Registry(_) => McpFailure::Registry,
    }
}
