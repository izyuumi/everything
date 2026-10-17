use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    time::{Instant, timeout_at},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_with_config, connect_async_with_config,
    tungstenite::{Error as SocketError, Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use crate::{
    crypto::{DeviceIdentity, MAX_CIPHERTEXT_BYTES},
    protocol::{DeviceId, UnixMillis},
    relay_protocol::{
        AUTH_CHALLENGE_LIFETIME_MS, MAX_RELAY_FRAME_BYTES, RELAY_SCHEMA_VERSION,
        RelayClientMessage, RelayErrorCode, RelayOrigin, RelayServerMessage,
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_AUTH_FRAME_BYTES: usize = 4_096;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum RemoteError {
    #[error("relay operation was cancelled")]
    Cancelled,
    #[error("relay connection is closed")]
    Closed,
    #[error("relay operation timed out")]
    Timeout,
    #[error("relay protocol validation failed")]
    Protocol,
    #[error("relay network or TLS connection failed")]
    Socket,
    #[error("relay rejected the operation: {0:?}")]
    Relay(RelayErrorCode),
    #[error("system clock cannot represent the relay authentication timestamp")]
    Clock,
}

pub struct RelayConnection {
    socket: Option<Socket>,
    socket_identity: Arc<RelaySocketIdentity>,
    device_id: DeviceId,
    cancellation: CancellationToken,
    last_received: Instant,
}

pub(crate) struct RelaySocketIdentity {
    closed: AtomicBool,
    cancellation: CancellationToken,
}

impl RelaySocketIdentity {
    pub(crate) fn is_live(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && !self.cancellation.is_cancelled()
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl RelayConnection {
    pub async fn connect(
        origin: RelayOrigin,
        identity: Arc<DeviceIdentity>,
        cancellation: CancellationToken,
    ) -> Result<Self, RemoteError> {
        if cancellation.is_cancelled() {
            return Err(RemoteError::Cancelled);
        }
        let configuration = WebSocketConfig::default()
            .read_buffer_size(16 * 1_024)
            .write_buffer_size(16 * 1_024)
            .max_write_buffer_size(MAX_RELAY_FRAME_BYTES + 64 * 1_024)
            .max_message_size(Some(MAX_RELAY_FRAME_BYTES))
            .max_frame_size(Some(MAX_RELAY_FRAME_BYTES));
        let (mut socket, _) = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RemoteError::Cancelled),
            result = timeout_at(
                Instant::now() + CONNECT_TIMEOUT,
                connect_socket(&origin, configuration),
            ) => result.map_err(|_| RemoteError::Timeout)?.map_err(socket_error)?,
        };
        let device_id = identity.public_keys().device_id;
        let deadline = Instant::now() + Duration::from_millis(AUTH_CHALLENGE_LIFETIME_MS);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RemoteError::Cancelled),
            result = timeout_at(deadline, authenticate(&mut socket, &origin, &identity, &cancellation, deadline)) => {
                result.map_err(|_| RemoteError::Timeout)??;
            }
        }
        Ok(Self {
            socket: Some(socket),
            socket_identity: Arc::new(RelaySocketIdentity {
                closed: AtomicBool::new(false),
                cancellation: cancellation.clone(),
            }),
            device_id,
            cancellation,
            last_received: Instant::now(),
        })
    }

    pub(crate) fn capture_socket_identity(
        &self,
        local_device: DeviceId,
    ) -> Result<Arc<RelaySocketIdentity>, RemoteError> {
        if self.device_id != local_device {
            return Err(RemoteError::Protocol);
        }
        if self.cancellation.is_cancelled() {
            return Err(RemoteError::Cancelled);
        }
        if self.socket.is_none() || !self.socket_identity.is_live() {
            return Err(RemoteError::Closed);
        }
        Ok(Arc::clone(&self.socket_identity))
    }

    fn live_socket(&mut self) -> Result<&mut Socket, RemoteError> {
        if !self.socket_identity.is_live() {
            self.socket = None;
            return Err(if self.cancellation.is_cancelled() {
                RemoteError::Cancelled
            } else {
                RemoteError::Closed
            });
        }
        self.socket.as_mut().ok_or(RemoteError::Closed)
    }

    pub async fn send(&mut self, message: &RelayClientMessage) -> Result<(), RemoteError> {
        match message {
            RelayClientMessage::Send { envelope, .. } => {
                if envelope.header.sender != self.device_id
                    || envelope.ciphertext.len() <= 16
                    || envelope.ciphertext.len() > MAX_CIPHERTEXT_BYTES
                {
                    return Err(RemoteError::Protocol);
                }
            }
            RelayClientMessage::Acknowledge { .. } | RelayClientMessage::Ping { .. } => {}
            RelayClientMessage::BeginAuthentication { .. }
            | RelayClientMessage::Authenticate { .. } => {
                return Err(RemoteError::Protocol);
            }
        }
        let cancellation = self.cancellation.clone();
        let socket = self.live_socket()?;
        let result = send_frame(socket, message, &cancellation).await;
        if result.is_err() {
            self.socket_identity.close();
            self.socket = None;
        }
        result
    }

    pub async fn receive(&mut self) -> Result<RelayServerMessage, RemoteError> {
        let cancellation = self.cancellation.clone();
        let mut last_received = self.last_received;
        let socket = self.live_socket()?;
        let result = receive_frame(
            socket,
            &cancellation,
            &mut last_received,
            MAX_RELAY_FRAME_BYTES,
            None,
        )
        .await;
        self.last_received = last_received;
        let result = result.and_then(|message| match &message {
            RelayServerMessage::Challenge { .. } | RelayServerMessage::Authenticated { .. } => {
                Err(RemoteError::Protocol)
            }
            RelayServerMessage::Delivery { envelope, .. }
                if envelope.header.recipient != self.device_id =>
            {
                Err(RemoteError::Protocol)
            }
            RelayServerMessage::Error { code, .. } => Err(RemoteError::Relay(*code)),
            _ => Ok(message),
        });
        if result.is_err() {
            self.socket_identity.close();
            self.socket = None;
        }
        result
    }

    pub async fn close(&mut self) -> Result<(), RemoteError> {
        self.socket_identity.close();
        let Some(mut socket) = self.socket.take() else {
            return Ok(());
        };
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(RemoteError::Cancelled),
            result = timeout_at(Instant::now() + CLOSE_TIMEOUT, socket.close(None)) => {
                result.map_err(|_| RemoteError::Timeout)?.map_err(socket_error)
            }
        }
    }
}

impl Drop for RelayConnection {
    fn drop(&mut self) {
        self.socket_identity.close();
    }
}

async fn connect_socket(
    origin: &RelayOrigin,
    configuration: WebSocketConfig,
) -> Result<
    (
        Socket,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    SocketError,
> {
    let parsed = Url::parse(origin.as_str())
        .map_err(|_| SocketError::Io(std::io::Error::other("invalid relay origin")))?;
    if parsed.scheme() == "wss" {
        return connect_async_with_config(origin.websocket_url(), Some(configuration), true).await;
    }
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| SocketError::Io(std::io::Error::other("relay origin has no port")))?;
    let addresses = match parsed.host() {
        Some(Host::Domain("localhost")) => vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
        ],
        Some(Host::Ipv4(address)) if address.is_loopback() => {
            vec![SocketAddr::new(IpAddr::V4(address), port)]
        }
        Some(Host::Ipv6(address)) if address.is_loopback() => {
            vec![SocketAddr::new(IpAddr::V6(address), port)]
        }
        _ => {
            return Err(SocketError::Io(std::io::Error::other(
                "plaintext relay requires loopback",
            )));
        }
    };
    let stream = TcpStream::connect(addresses.as_slice()).await?;
    stream.set_nodelay(true)?;
    client_async_with_config(
        origin.websocket_url(),
        MaybeTlsStream::Plain(stream),
        Some(configuration),
    )
    .await
}

async fn authenticate(
    socket: &mut Socket,
    origin: &RelayOrigin,
    identity: &DeviceIdentity,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> Result<(), RemoteError> {
    let device_id = identity.public_keys().device_id;
    send_frame(
        socket,
        &RelayClientMessage::BeginAuthentication {
            schema_version: RELAY_SCHEMA_VERSION,
            device_id,
        },
        cancellation,
    )
    .await?;
    let mut last_received = Instant::now();
    let challenge = match receive_frame(
        socket,
        cancellation,
        &mut last_received,
        MAX_AUTH_FRAME_BYTES,
        Some(deadline),
    )
    .await?
    {
        RelayServerMessage::Challenge { challenge, .. } => challenge,
        RelayServerMessage::Error { code, .. } => return Err(RemoteError::Relay(code)),
        _ => return Err(RemoteError::Protocol),
    };
    let now = UnixMillis::now().map_err(|_| RemoteError::Clock)?.get();
    let signed = challenge
        .signing_bytes(origin, device_id, now)
        .map_err(|_| RemoteError::Protocol)?;
    let signature = identity
        .sign_challenge(&signed)
        .map_err(|_| RemoteError::Protocol)?;
    send_frame(
        socket,
        &RelayClientMessage::Authenticate {
            schema_version: RELAY_SCHEMA_VERSION,
            session_id: challenge.session_id,
            signature,
        },
        cancellation,
    )
    .await?;
    match receive_frame(
        socket,
        cancellation,
        &mut last_received,
        MAX_AUTH_FRAME_BYTES,
        Some(deadline),
    )
    .await?
    {
        RelayServerMessage::Authenticated {
            device_id: authenticated,
            ..
        } if authenticated == device_id => Ok(()),
        RelayServerMessage::Error { code, .. } => Err(RemoteError::Relay(code)),
        _ => Err(RemoteError::Protocol),
    }
}

async fn send_frame(
    socket: &mut Socket,
    frame: &RelayClientMessage,
    cancellation: &CancellationToken,
) -> Result<(), RemoteError> {
    let bytes = frame.to_bytes().map_err(|_| RemoteError::Protocol)?;
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(RemoteError::Cancelled),
        result = timeout_at(Instant::now() + WRITE_TIMEOUT, socket.send(Message::Binary(bytes.into()))) => {
            result.map_err(|_| RemoteError::Timeout)?.map_err(socket_error)
        }
    }
}

async fn receive_frame(
    socket: &mut Socket,
    cancellation: &CancellationToken,
    last_received: &mut Instant,
    maximum: usize,
    authentication_deadline: Option<Instant>,
) -> Result<RelayServerMessage, RemoteError> {
    loop {
        let deadline = authentication_deadline.unwrap_or(*last_received + READ_IDLE_TIMEOUT);
        let message = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RemoteError::Cancelled),
            result = timeout_at(deadline, socket.next()) => {
                result.map_err(|_| RemoteError::Timeout)?.ok_or(RemoteError::Closed)?.map_err(socket_error)?
            }
        };
        *last_received = Instant::now();
        match message {
            Message::Binary(bytes) => return parse_frame(&bytes, maximum),
            Message::Text(bytes) => return parse_frame(bytes.as_bytes(), maximum),
            Message::Ping(_) => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(RemoteError::Cancelled),
                    result = timeout_at(Instant::now() + WRITE_TIMEOUT, socket.flush()) => {
                        result.map_err(|_| RemoteError::Timeout)?.map_err(socket_error)?;
                    }
                }
            }
            Message::Pong(_) => {}
            Message::Close(_) => return Err(RemoteError::Closed),
            Message::Frame(_) => return Err(RemoteError::Protocol),
        }
    }
}

fn parse_frame(bytes: &[u8], maximum: usize) -> Result<RelayServerMessage, RemoteError> {
    if bytes.len() > maximum {
        return Err(RemoteError::Protocol);
    }
    RelayServerMessage::parse(bytes).map_err(|_| RemoteError::Protocol)
}

fn socket_error(error: SocketError) -> RemoteError {
    match error {
        SocketError::ConnectionClosed | SocketError::AlreadyClosed => RemoteError::Closed,
        SocketError::Capacity(_) | SocketError::Protocol(_) | SocketError::Utf8(_) => {
            RemoteError::Protocol
        }
        _ => RemoteError::Socket,
    }
}
