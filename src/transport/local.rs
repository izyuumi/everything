use std::fs::File;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::protocol::{
    CommandEnvelope, CommandId, SchemaVersion, SyncBatch, SyncRequest, UnixMillis,
};
use crate::service::ServiceError;
use crate::service::state::StateDirectory;

pub const MAX_LOCAL_FRAME_BYTES: usize = 1_024 * 1_024;
pub const MAX_LOCAL_COMMAND_BYTES: usize = 256 * 1_024;
pub const MAX_LOCAL_CONNECTIONS: usize = 32;
pub const LOCAL_COMMAND_CAPACITY: usize = 32;
const MAX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Debug, Error)]
pub enum LocalError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("local IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("local IPC peer is not owned by this user")]
    Authentication,
    #[error("invalid local IPC protocol: {0}")]
    Protocol(&'static str),
    #[error("local IPC operation timed out")]
    Timeout,
    #[error("local IPC operation was cancelled")]
    Cancelled,
    #[error("local IPC worker failed")]
    Worker,
    #[error("local IPC reply receiver disconnected")]
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct LocalOptions {
    pub frame_timeout: Duration,
    pub command_timeout: Duration,
}

impl Default for LocalOptions {
    fn default() -> Self {
        Self {
            frame_timeout: Duration::from_secs(10),
            command_timeout: Duration::from_secs(120),
        }
    }
}

impl LocalOptions {
    fn validate(&self) -> Result<(), LocalError> {
        if self.frame_timeout.is_zero()
            || self.frame_timeout > Duration::from_secs(120)
            || self.command_timeout.is_zero()
            || self.command_timeout > Duration::from_secs(3_600)
        {
            return Err(LocalError::Protocol(
                "timeouts must be positive and bounded",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalRequest {
    pub schema_version: SchemaVersion,
    pub request_id: CommandId,
    pub payload: LocalRequestPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocalRequestPayload {
    Status,
    SubmitCommand { command: CommandEnvelope },
    Synchronize { request: SyncRequest },
}

impl LocalRequest {
    pub fn new(payload: LocalRequestPayload) -> Result<Self, LocalError> {
        let request_id = match &payload {
            LocalRequestPayload::SubmitCommand { command } => command.command_id,
            _ => CommandId::new(),
        };
        let request = Self {
            schema_version: SchemaVersion::CURRENT,
            request_id,
            payload,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), LocalError> {
        match &self.payload {
            LocalRequestPayload::Status => {}
            LocalRequestPayload::SubmitCommand { command } => {
                if command.command_id != self.request_id || command.issuing_device_id.is_some() {
                    return Err(LocalError::Protocol(
                        "local command requires its own request ID and no remote device attribution",
                    ));
                }
                encode(command, MAX_LOCAL_COMMAND_BYTES)?;
            }
            LocalRequestPayload::Synchronize { request } => {
                request
                    .validate()
                    .map_err(|_| LocalError::Protocol("invalid synchronization request"))?;
                if request.cursors.len() != 1 {
                    return Err(LocalError::Protocol(
                        "local synchronization requests exactly one conversation",
                    ));
                }
            }
        }
        encode(self, MAX_LOCAL_FRAME_BYTES)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStatus {
    pub process_id: u32,
    pub started_at: UnixMillis,
    pub version: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalFailureCode {
    InvalidRequest,
    PermissionDenied,
    Unavailable,
    Busy,
    Timeout,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalFailure {
    pub code: LocalFailureCode,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LocalResponsePayload {
    Status {
        status: HostStatus,
    },
    CommandAccepted {
        command_id: CommandId,
        accepted_at: UnixMillis,
        duplicate: bool,
    },
    Synchronized {
        batch: SyncBatch,
    },
    Rejected {
        error: LocalFailure,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalResponse {
    pub schema_version: SchemaVersion,
    pub request_id: CommandId,
    pub payload: LocalResponsePayload,
}

impl LocalResponse {
    pub fn validate_for(&self, request: &LocalRequest) -> Result<(), LocalError> {
        if self.schema_version != request.schema_version || self.request_id != request.request_id {
            return Err(LocalError::Protocol(
                "response identity or schema does not match the request",
            ));
        }
        match (&request.payload, &self.payload) {
            (_, LocalResponsePayload::Rejected { error }) => {
                if error.message.len() > 1_024 || error.message.contains('\0') {
                    return Err(LocalError::Protocol("invalid rejection message"));
                }
            }
            (LocalRequestPayload::Status, LocalResponsePayload::Status { status }) => {
                if status.process_id == 0 || status.version.is_empty() || status.version.len() > 128
                {
                    return Err(LocalError::Protocol("invalid host status"));
                }
            }
            (
                LocalRequestPayload::SubmitCommand { command },
                LocalResponsePayload::CommandAccepted { command_id, .. },
            ) if *command_id == command.command_id => {}
            (
                LocalRequestPayload::Synchronize { request },
                LocalResponsePayload::Synchronized { batch },
            ) => {
                batch.validate_for(request).map_err(|_| {
                    LocalError::Protocol("synchronization response does not match its request")
                })?;
            }
            _ => {
                return Err(LocalError::Protocol(
                    "response payload does not match the request",
                ));
            }
        }
        encode(self, MAX_LOCAL_FRAME_BYTES)?;
        Ok(())
    }
}

pub struct LocalCall {
    pub peer_uid: u32,
    pub request: LocalRequest,
    pub cancellation: CancellationToken,
    reply: oneshot::Sender<LocalResponsePayload>,
}

impl LocalCall {
    pub fn respond(self, response: LocalResponsePayload) -> Result<(), LocalError> {
        self.reply
            .send(response)
            .map_err(|_| LocalError::Disconnected)
    }
}

pub struct LocalServer {
    listener: UnixListener,
    directory: StateDirectory,
    _lock: File,
    socket_metadata: rustix::fs::Stat,
    sender: mpsc::Sender<LocalCall>,
    options: LocalOptions,
    status: HostStatus,
}

impl LocalServer {
    pub async fn bind(
        state_directory: impl AsRef<Path>,
        options: LocalOptions,
    ) -> Result<(Self, mpsc::Receiver<LocalCall>), LocalError> {
        options.validate()?;
        let directory = StateDirectory::open(state_directory.as_ref(), true)?;
        let lock = directory.lock()?;
        let socket_path = socket_path(&directory.path)?;
        if directory.socket_metadata()?.is_some() {
            match tokio::time::timeout(options.frame_timeout, UnixStream::connect(&socket_path))
                .await
            {
                Ok(Ok(_)) => return Err(ServiceError::AlreadyRunning.into()),
                Ok(Err(error))
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    directory.unlink_socket()?
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => return Err(LocalError::Timeout),
            }
        }
        let listener = UnixListener::bind(&socket_path)?;
        if let Err(error) =
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        {
            if let Err(cleanup) = directory.unlink_socket() {
                tracing::warn!(error = %cleanup, "could not remove socket after permission setup failed");
            }
            return Err(error.into());
        }
        let metadata = directory
            .socket_metadata()?
            .ok_or(LocalError::Protocol("bound socket disappeared"))?;
        let (sender, receiver) = mpsc::channel(LOCAL_COMMAND_CAPACITY);
        let status = HostStatus {
            process_id: std::process::id(),
            started_at: UnixMillis::now()
                .map_err(|_| LocalError::Protocol("host clock cannot encode a timestamp"))?,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        Ok((
            Self {
                listener,
                directory,
                _lock: lock,
                socket_metadata: metadata,
                sender,
                options,
                status,
            },
            receiver,
        ))
    }

    pub async fn run(self, cancellation: CancellationToken) -> Result<(), LocalError> {
        let semaphore = Arc::new(Semaphore::new(MAX_LOCAL_CONNECTIONS));
        let mut workers = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                result = workers.join_next(), if !workers.is_empty() => {
                    report_worker(result.ok_or(LocalError::Worker)?)?;
                }
                connection = self.listener.accept() => {
                    let (stream, _) = connection?;
                    let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                        tracing::warn!("local IPC connection limit reached");
                        drop(stream);
                        continue;
                    };
                    let sender = self.sender.clone();
                    let options = self.options.clone();
                    let status = self.status.clone();
                    let uid = self.directory.uid;
                    let token = cancellation.child_token();
                    workers.spawn(async move {
                        let _permit = permit;
                        handle_connection(stream, uid, sender, options, status, token).await
                    });
                }
            }
        }
        workers.abort_all();
        while let Some(result) = workers.join_next().await {
            match result {
                Err(error) if error.is_cancelled() => {}
                result => report_worker(result)?,
            }
        }
        Ok(())
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        match self.directory.socket_metadata() {
            Ok(Some(metadata))
                if metadata.st_dev == self.socket_metadata.st_dev
                    && metadata.st_ino == self.socket_metadata.st_ino =>
            {
                if let Err(error) = self.directory.unlink_socket() {
                    tracing::warn!(error = %error, "could not remove stopped local IPC socket");
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(error = %error, "could not inspect stopped local IPC socket")
            }
        }
    }
}

pub struct LocalClient {
    options: LocalOptions,
}

impl LocalClient {
    pub fn new(options: LocalOptions) -> Result<Self, LocalError> {
        options.validate()?;
        Ok(Self { options })
    }

    pub async fn request(
        &self,
        state_directory: impl AsRef<Path>,
        request: LocalRequest,
        cancellation: CancellationToken,
    ) -> Result<LocalResponse, LocalError> {
        if cancellation.is_cancelled() {
            return Err(LocalError::Cancelled);
        }
        request.validate()?;
        let directory = StateDirectory::open(state_directory.as_ref(), false)?;
        if directory.socket_metadata()?.is_none() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "host socket is absent").into());
        }
        let operation = async {
            let mut stream = tokio::time::timeout(
                self.options.frame_timeout,
                UnixStream::connect(socket_path(&directory.path)?),
            )
            .await
            .map_err(|_| LocalError::Timeout)??;
            authenticate(&stream, directory.uid)?;
            tokio::time::timeout(
                self.options.frame_timeout,
                write_frame(&mut stream, &request),
            )
            .await
            .map_err(|_| LocalError::Timeout)??;
            let response: LocalResponse = read_frame(&mut stream).await?;
            response.validate_for(&request)?;
            Ok(response)
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(LocalError::Cancelled),
            result = tokio::time::timeout(self.options.command_timeout, operation) => result.map_err(|_| LocalError::Timeout)?,
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    uid: u32,
    sender: mpsc::Sender<LocalCall>,
    options: LocalOptions,
    status: HostStatus,
    cancellation: CancellationToken,
) -> Result<(), LocalError> {
    authenticate(&stream, uid)?;
    let request: LocalRequest = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(LocalError::Cancelled),
        request = tokio::time::timeout(options.frame_timeout, read_frame(&mut stream)) => request.map_err(|_| LocalError::Timeout)??,
    };
    request.validate()?;
    let payload = if matches!(request.payload, LocalRequestPayload::Status) {
        LocalResponsePayload::Status { status }
    } else {
        let call_cancellation = cancellation.child_token();
        let _guard = call_cancellation.clone().drop_guard();
        let (reply, received) = oneshot::channel();
        let call = LocalCall {
            peer_uid: uid,
            request: request.clone(),
            cancellation: call_cancellation,
            reply,
        };
        let operation = async {
            sender
                .send(call)
                .await
                .map_err(|_| LocalError::Disconnected)?;
            let mut extra = [0u8; 1];
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(LocalError::Cancelled),
                reply = received => reply.map_err(|_| LocalError::Disconnected),
                extra = stream.read(&mut extra) => match extra? {
                    0 => Err(LocalError::Disconnected),
                    _ => Err(LocalError::Protocol("only one request is allowed per connection")),
                },
            }
        };
        match tokio::time::timeout(options.command_timeout, operation).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(LocalError::Disconnected)) => rejection(
                LocalFailureCode::Unavailable,
                "host request processor is unavailable",
            ),
            Ok(Err(error)) => return Err(error),
            Err(_) => rejection(
                LocalFailureCode::Timeout,
                "host request processing timed out; command outcome may be durable",
            ),
        }
    };
    let response = LocalResponse {
        schema_version: SchemaVersion::CURRENT,
        request_id: request.request_id,
        payload,
    };
    response.validate_for(&request)?;
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(LocalError::Cancelled),
        written = tokio::time::timeout(options.frame_timeout, write_frame(&mut stream, &response)) => written.map_err(|_| LocalError::Timeout)?,
    }
}

fn rejection(code: LocalFailureCode, message: &str) -> LocalResponsePayload {
    LocalResponsePayload::Rejected {
        error: LocalFailure {
            code,
            message: message.to_owned(),
        },
    }
}

fn authenticate(stream: &UnixStream, uid: u32) -> Result<(), LocalError> {
    if stream.peer_cred()?.uid() != uid {
        return Err(LocalError::Authentication);
    }
    Ok(())
}

fn socket_path(directory: &Path) -> Result<PathBuf, LocalError> {
    use std::os::unix::ffi::OsStrExt;
    let path = directory.join("host.sock");
    if path.as_os_str().as_bytes().len() > MAX_SOCKET_PATH_BYTES {
        return Err(LocalError::Protocol(
            "socket path exceeds the portable 100-byte limit",
        ));
    }
    Ok(path)
}

fn encode(value: &impl Serialize, maximum: usize) -> Result<Vec<u8>, LocalError> {
    let mut writer = FrameWriter {
        bytes: Vec::new(),
        maximum,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| LocalError::Protocol("JSON frame exceeds its byte limit"))?;
    Ok(writer.bytes)
}

struct FrameWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl io::Write for FrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("frame byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> Result<T, LocalError> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_LOCAL_FRAME_BYTES {
        return Err(LocalError::Protocol("frame length is out of bounds"));
    }
    let mut bytes = vec![0u8; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(|_| LocalError::Protocol("invalid JSON frame or schema"))
}

async fn write_frame(stream: &mut UnixStream, value: &impl Serialize) -> Result<(), LocalError> {
    let bytes = encode(value, MAX_LOCAL_FRAME_BYTES)?;
    let length =
        u32::try_from(bytes.len()).map_err(|_| LocalError::Protocol("frame length overflow"))?;
    stream.write_u32(length).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

fn report_worker(
    result: Result<Result<(), LocalError>, tokio::task::JoinError>,
) -> Result<(), LocalError> {
    match result {
        Ok(Ok(())) | Ok(Err(LocalError::Cancelled | LocalError::Disconnected)) => Ok(()),
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "local IPC request failed");
            Ok(())
        }
        Err(_) => Err(LocalError::Worker),
    }
}
