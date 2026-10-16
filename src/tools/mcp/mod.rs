mod config;
mod process;
mod transport;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{
    ClientCapabilities, ClientConfig, ClientRequest, Implementation, ListToolsRequest,
    PaginatedRequestParams, ServerResult,
};
use rmcp::service::{NotificationContext, PeerRequestOptions};
use rmcp::{ClientHandler, Peer, RoleClient, ServiceExt};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use super::{
    MAX_TOOL_SCHEMA_BYTES, Tool, ToolCapability, ToolConcurrency, ToolDescriptor, ToolError,
    ToolExecutionContext, ToolOutput, ToolRegistry,
};
use crate::permissions::ToolScopeRule;

pub use config::{McpLimits, McpStdioConfig};
use process::{ProcessOwner, ServiceOwner};
use transport::StdioTransport;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOOLS: usize = 128;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("invalid trusted MCP configuration: {0}")]
    Configuration(&'static str),
    #[error("MCP process admission limit reached")]
    Capacity,
    #[error("MCP server has an in-flight request")]
    Busy,
    #[error("MCP server process could not start or be supervised")]
    Process,
    #[error("MCP server is disconnected")]
    Disconnected,
    #[error("MCP tool catalog changed; explicitly replace this session and registry")]
    StaleCatalog,
    #[error("MCP operation cancelled; remote effects may be unknown")]
    Cancelled,
    #[error("MCP operation deadline elapsed; remote effects may be unknown")]
    Deadline,
    #[error("MCP protocol failed: {0}")]
    Protocol(&'static str),
    #[error("MCP {0} exceeds its resource limit")]
    ResourceLimit(&'static str),
    #[error("MCP service or process cleanup could not be confirmed")]
    Cleanup,
    #[error("MCP host worker failed")]
    Worker,
    #[error("MCP stdio process ownership requires Unix")]
    UnsupportedPlatform,
    #[error("MCP registry preparation failed: {0}")]
    Registry(#[from] ToolError),
}

#[derive(Clone)]
pub struct McpHost {
    admission: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct McpSession {
    inner: Arc<SessionInner>,
}

struct SessionInner {
    catalog: McpCatalog,
    peer: Peer<RoleClient>,
    service: ServiceOwner,
    process: Arc<ProcessOwner>,
    generation: Arc<AtomicU64>,
    catalog_generation: u64,
    request_gate: Mutex<()>,
    limits: McpLimits,
    scope: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct McpToolInfo {
    pub native_name: String,
    pub original_name: String,
    pub descriptor: ToolDescriptor,
}

#[derive(Clone, Debug, Serialize)]
pub struct McpCatalog {
    pub server_id: String,
    pub launch_fingerprint: String,
    pub tools: Vec<McpToolInfo>,
}

struct McpTool {
    descriptor: ToolDescriptor,
    original_name: String,
    session: McpSession,
}

#[derive(Clone)]
struct Handler {
    generation: Arc<AtomicU64>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("everything", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

impl McpHost {
    pub fn new(max_servers: usize) -> Result<Self, McpError> {
        if !(1..=16).contains(&max_servers) {
            return Err(McpError::Configuration(
                "server admission requires 1 through 16 processes",
            ));
        }
        Ok(Self {
            admission: Arc::new(Semaphore::new(max_servers)),
        })
    }

    pub async fn connect(
        &self,
        config: McpStdioConfig,
        cancellation: CancellationToken,
    ) -> Result<McpSession, McpError> {
        if !cfg!(unix) {
            return Err(McpError::UnsupportedPlatform);
        }
        if cancellation.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        let startup_deadline = tokio::time::Instant::now() + config.limits.startup_timeout;
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| McpError::Capacity)?;
        let preparation =
            tokio::task::spawn_blocking(move || config.prepare().map(|config| (config, permit)));
        let (config, permit) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(McpError::Cancelled),
            result = tokio::time::timeout_at(startup_deadline, preparation) => result.map_err(|_| McpError::Deadline)?.map_err(|_| McpError::Worker)??,
        };
        if cancellation.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        let fingerprint = config.fingerprint()?;
        let mut command = tokio::process::Command::new(&config.executable);
        command
            .current_dir(&config.working_directory)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(false);
        command.args(config.arguments.iter().map(|value| value.as_str()));
        for (key, value) in &config.environment {
            command.env(key, value.as_str());
        }
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|_| McpError::Process)?;
        let (process, stdout, stdin) =
            ProcessOwner::supervise(child, permit, config.limits.cleanup_timeout)?;
        let mut guard = StopOnDrop::new(process.stop.clone());
        let generation = Arc::new(AtomicU64::new(0));
        let handler = Handler {
            generation: Arc::clone(&generation),
        };
        let transport = StdioTransport::new(stdout, stdin, process.stop.clone());
        let initialization = handler.serve_with_ct(transport, process.stop.child_token());
        let service = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(McpError::Cancelled),
            result = tokio::time::timeout_at(startup_deadline, initialization) => match result {
                Ok(Ok(service)) => Ok(service),
                Ok(Err(_)) => Err(McpError::Protocol("initialization failed")),
                Err(_) => Err(McpError::Deadline),
            }
        };
        let service = match service {
            Ok(service) => service,
            Err(error) => {
                process.shutdown().await?;
                return Err(error);
            }
        };
        let peer = service.peer().clone();
        let service =
            ServiceOwner::supervise(service, process.stop.clone(), config.limits.cleanup_timeout);
        let snapshot = generation.load(Ordering::Acquire);
        let discovery = discover(
            &peer,
            &config.server_id,
            &process.stop,
            &cancellation,
            config.limits.request_timeout,
        );
        let discovered = match tokio::time::timeout_at(startup_deadline, discovery).await {
            Ok(result) => result,
            Err(_) => Err(McpError::Deadline),
        };
        let tools = match discovered {
            Ok(tools) if snapshot == generation.load(Ordering::Acquire) => tools,
            result => {
                process.stop.cancel();
                let (service_clean, process_clean) =
                    tokio::join!(service.shutdown(), process.shutdown());
                if service_clean.is_err() || process_clean.is_err() {
                    return Err(McpError::Cleanup);
                }
                return Err(result.err().unwrap_or(McpError::StaleCatalog));
            }
        };
        let catalog = McpCatalog {
            server_id: config.server_id.clone(),
            launch_fingerprint: fingerprint.clone(),
            tools,
        };
        let session = McpSession {
            inner: Arc::new(SessionInner {
                catalog,
                peer,
                service,
                process,
                generation,
                catalog_generation: snapshot,
                request_gate: Mutex::new(()),
                limits: config.limits,
                scope: format!("mcp:{}:{fingerprint}", config.server_id),
            }),
        };
        if let Err(error) = session.check_connected() {
            session.shutdown().await?;
            return Err(error);
        }
        guard.disarm();
        Ok(session)
    }
}

impl McpSession {
    pub fn catalog(&self) -> &McpCatalog {
        &self.inner.catalog
    }

    pub fn process_id(&self) -> u32 {
        self.inner.process.pid
    }

    pub fn check_connected(&self) -> Result<(), McpError> {
        if self.inner.process.stop.is_cancelled() || self.inner.peer.is_transport_closed() {
            return Err(McpError::Disconnected);
        }
        if self.inner.generation.load(Ordering::Acquire) != self.inner.catalog_generation {
            return Err(McpError::StaleCatalog);
        }
        Ok(())
    }

    pub fn register(
        &self,
        registry: &mut ToolRegistry,
    ) -> Result<BTreeMap<String, ToolScopeRule>, McpError> {
        self.check_connected()?;
        let mut prepared = registry.clone();
        let mut scopes = BTreeMap::new();
        for info in &self.inner.catalog.tools {
            prepared.register(Arc::new(McpTool {
                descriptor: info.descriptor.clone(),
                original_name: info.original_name.clone(),
                session: self.clone(),
            }))?;
            scopes.insert(
                info.native_name.clone(),
                ToolScopeRule::Host {
                    service: self.inner.scope.clone(),
                },
            );
        }
        self.check_connected()?;
        *registry = prepared;
        Ok(scopes)
    }

    pub async fn shutdown(&self) -> Result<(), McpError> {
        let (service, process) =
            tokio::join!(self.inner.service.shutdown(), self.inner.process.shutdown());
        service?;
        process
    }
}

#[async_trait]
impl Tool for McpTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let before = |error: McpError| ToolError::FailedWithoutSideEffects {
            name: self.descriptor.name.clone(),
            message: error.to_string(),
        };
        context.check_cancelled()?;
        let _gate = self
            .session
            .inner
            .request_gate
            .try_lock()
            .map_err(|_| before(McpError::Busy))?;
        self.session.check_connected().map_err(before)?;
        encoded_size(&arguments, MAX_ARGUMENT_BYTES).map_err(before)?;
        let arguments = arguments
            .as_object()
            .ok_or_else(|| before(McpError::Protocol("tool arguments must be an object")))?
            .clone();
        let params = rmcp::model::CallToolRequestParams::new(self.original_name.clone())
            .with_arguments(arguments);
        let request = ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(params));
        let mut guard = StopOnDrop::new(self.session.inner.process.stop.clone());
        let response = request_once(
            &self.session.inner.peer,
            request,
            &self.session.inner.process.stop,
            context.cancellation(),
            self.session.inner.limits.request_timeout,
        )
        .await;
        let output = match response {
            Ok(ServerResult::CallToolResult(result)) => {
                let output = ToolOutput {
                    is_error: result.is_error.unwrap_or(false),
                    content: serde_json::json!({
                        "server_id":self.session.inner.catalog.server_id,
                        "original_tool_name":self.original_name,
                        "content":result.content,
                        "structured_content":result.structured_content
                    }),
                };
                encoded_size(&output, MAX_OUTPUT_BYTES).map(|_| output)
            }
            Ok(_) => Err(McpError::Protocol(
                "server did not return a completed tool result",
            )),
            Err(error) => Err(error),
        };
        match output {
            Ok(output) => {
                guard.disarm();
                Ok(output)
            }
            Err(error) => {
                let cleanup = self.session.shutdown().await;
                Err(ToolError::EffectsUnknown {
                    name: self.descriptor.name.clone(),
                    message: if cleanup.is_ok() {
                        error.to_string()
                    } else {
                        McpError::Cleanup.to_string()
                    },
                    cleanup_completed: false,
                })
            }
        }
    }
}

async fn discover(
    peer: &Peer<RoleClient>,
    server_id: &str,
    stop: &CancellationToken,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<Vec<McpToolInfo>, McpError> {
    if peer
        .peer_info()
        .is_none_or(|info| info.capabilities.tools.is_none())
    {
        return Err(McpError::Protocol("server does not advertise tools"));
    }
    let mut cursor = None;
    let mut cursors = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    let mut tools = Vec::new();
    let mut bytes = 0_usize;
    for _ in 0..16 {
        let params = PaginatedRequestParams::default().with_cursor(cursor);
        let request = ClientRequest::ListToolsRequest(ListToolsRequest::with_param(params));
        let ServerResult::ListToolsResult(result) =
            request_once(peer, request, stop, cancellation, timeout).await?
        else {
            return Err(McpError::Protocol("invalid tool listing response"));
        };
        bytes = bytes.saturating_add(encoded_size(&result, MAX_FRAME_BYTES)?);
        if bytes > MAX_CATALOG_BYTES {
            return Err(McpError::ResourceLimit("catalog bytes"));
        }
        for tool in result.tools {
            if tools.len() >= MAX_TOOLS {
                return Err(McpError::ResourceLimit("catalog tool count"));
            }
            let original = tool.name.into_owned();
            if original.is_empty()
                || original.len() > 128
                || original.chars().any(char::is_control)
                || original.trim().is_empty()
            {
                return Err(McpError::Protocol("invalid original tool name"));
            }
            let native_name = native_alias(server_id, &original);
            if !names.insert(original.clone()) || !aliases.insert(native_name.clone()) {
                return Err(McpError::Protocol("duplicate tool name or native alias"));
            }
            let description = tool.description.as_deref().unwrap_or("");
            if description.len() > 7000 || description.contains('\0') {
                return Err(McpError::ResourceLimit("tool description"));
            }
            let input_schema = Value::Object((*tool.input_schema).clone());
            encoded_size(&input_schema, MAX_TOOL_SCHEMA_BYTES)?;
            if input_schema.get("type").and_then(Value::as_str) != Some("object") {
                return Err(McpError::Protocol(
                    "tool input schema must declare an object",
                ));
            }
            let descriptor = ToolDescriptor {
                name: native_name.clone(),
                description: format!(
                    "Call MCP server {server_id} tool {original}. Exact host approval is required. {description}"
                ),
                input_schema,
                capabilities: vec![ToolCapability::Mcp],
                read_only: false,
                concurrency: ToolConcurrency::Exclusive,
            };
            tools.push(McpToolInfo {
                native_name,
                original_name: original,
                descriptor,
            });
        }
        match result.next_cursor {
            None => {
                tools.sort_by(|left, right| left.native_name.cmp(&right.native_name));
                return Ok(tools);
            }
            Some(next)
                if !next.is_empty()
                    && next.len() <= 256
                    && !next.chars().any(char::is_control)
                    && cursors.insert(next.clone()) =>
            {
                cursor = Some(next)
            }
            Some(_) => return Err(McpError::Protocol("invalid or repeated discovery cursor")),
        }
    }
    Err(McpError::ResourceLimit("discovery page count"))
}

async fn request_once(
    peer: &Peer<RoleClient>,
    request: ClientRequest,
    stop: &CancellationToken,
    cancellation: &CancellationToken,
    timeout: Duration,
) -> Result<ServerResult, McpError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut handle = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(McpError::Cancelled),
        _ = stop.cancelled() => return Err(McpError::Disconnected),
        handle = tokio::time::timeout_at(deadline, peer.send_cancellable_request(request, PeerRequestOptions::no_options())) => handle.map_err(|_| McpError::Deadline)?.map_err(|_| McpError::Disconnected)?,
    };
    let response = tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(McpError::Cancelled),
        _ = stop.cancelled() => Err(McpError::Disconnected),
        result = tokio::time::timeout_at(deadline, &mut handle.rx) => match result {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(_))) => Err(McpError::Protocol("request failed")),
            Ok(Err(_)) => Err(McpError::Disconnected),
            Err(_) => Err(McpError::Deadline),
        }
    };
    if response.is_err() {
        let cancelled = tokio::time::timeout(
            Duration::from_millis(250),
            handle.cancel(Some("Everything stopped the request".to_owned())),
        )
        .await;
        if !matches!(cancelled, Ok(Ok(()))) {
            stop.cancel();
        }
    }
    response
}

fn native_alias(server: &str, original: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"everything.mcp.tool.v1\0");
    hash.update((server.len() as u64).to_le_bytes());
    hash.update(server.as_bytes());
    hash.update((original.len() as u64).to_le_bytes());
    hash.update(original.as_bytes());
    let digest = format!("{:x}", hash.finalize());
    let readable: String = original
        .chars()
        .take(20)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!("mcp_{server}_{readable}_{}", &digest[..16])
}

struct StopOnDrop {
    stop: CancellationToken,
    armed: bool,
}
impl StopOnDrop {
    fn new(stop: CancellationToken) -> Self {
        Self { stop, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.stop.cancel();
        }
    }
}

fn encoded_size(value: &impl Serialize, maximum: usize) -> Result<usize, McpError> {
    let mut writer = SizeWriter { length: 0, maximum };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| McpError::ResourceLimit("serialized JSON"))?;
    Ok(writer.length)
}

struct SizeWriter {
    length: usize,
    maximum: usize,
}
impl Write for SizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.length = self
            .length
            .checked_add(bytes.len())
            .filter(|length| *length <= self.maximum)
            .ok_or_else(|| io::Error::other("MCP JSON limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
