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
        let socket = self.socket.as_mut().ok_or(RemoteError::Closed)?;
        let result = send_frame(socket, message, &self.cancellation).await;
        if result.is_err() {
            self.socket_identity.close();
            self.socket = None;
        }
        result
    }

    pub async fn receive(&mut self) -> Result<RelayServerMessage, RemoteError> {
        let socket = self.socket.as_mut().ok_or(RemoteError::Closed)?;
        let result = receive_frame(
            socket,
            &self.cancellation,
            &mut self.last_received,
            MAX_RELAY_FRAME_BYTES,
            None,
        )
        .await;
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

#[cfg(unix)]
mod settings_write {
    use std::any::Any;
    use std::fmt;
    use std::future::Future;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use futures_util::Sink;

    use super::*;
    use crate::devices::{
        SettingsSocketAdmissionFailure, SettingsSocketObserveFailure, SettingsSocketSendLoan,
    };

    type StopWait = Pin<Box<dyn Future<Output = ()> + Send>>;

    enum WriteProgress {
        BeforeFirstPoll,
        ReadinessPolled,
        FrameStarted,
        Flushed,
    }

    enum WriteResult {
        Flushed,
        Admission(SettingsSocketAdmissionFailure),
        Remote(RemoteError),
        Panicked(Box<dyn Any + Send>),
        FutureDropped,
    }

    pub(crate) struct SettingsSocketWriteOutcome {
        result: WriteResult,
        progress: WriteProgress,
        returned_abandoned: bool,
        socket: Arc<RelaySocketIdentity>,
    }

    pub(crate) struct SettingsSocketWriteReturn<'window> {
        payload: Option<WriteReturnPayload<'window>>,
    }

    struct WriteReturnPayload<'window> {
        outcome: SettingsSocketWriteOutcome,
        loan: SettingsSocketSendLoan<'window>,
    }

    struct WritePayload<'window> {
        cancellation: StopWait,
        progress: WriteProgress,
        loan: SettingsSocketSendLoan<'window>,
    }

    struct SettingsWriter<'connection, 'window> {
        connection: &'connection mut RelayConnection,
        payload: Option<WritePayload<'window>>,
    }

    impl RelayConnection {
        pub(crate) async fn send_settings<'window>(
            &mut self,
            loan: SettingsSocketSendLoan<'window>,
        ) -> SettingsSocketWriteReturn<'window> {
            let cancellation = Box::pin(self.cancellation.clone().cancelled_owned());
            SettingsWriter {
                connection: self,
                payload: Some(WritePayload {
                    cancellation,
                    progress: WriteProgress::BeforeFirstPoll,
                    loan,
                }),
            }
            .await
        }
    }

    impl SettingsWriter<'_, '_> {
        fn poll_write(&mut self, cx: &mut Context<'_>) -> Poll<WriteResult> {
            let Some(payload) = self.payload.as_mut() else {
                panic!("completed Settings writer was polled again");
            };
            if payload.cancellation.as_mut().poll(cx).is_ready() {
                return Poll::Ready(WriteResult::Remote(RemoteError::Cancelled));
            }
            if let Poll::Ready(cause) = payload.loan.poll_stop(cx) {
                return Poll::Ready(WriteResult::Admission(cause));
            }
            if !payload
                .loan
                .matches_socket(&self.connection.socket_identity, self.connection.device_id)
            {
                return Poll::Ready(WriteResult::Remote(RemoteError::Protocol));
            }
            let Some(socket) = self.connection.socket.as_mut() else {
                return Poll::Ready(WriteResult::Remote(RemoteError::Closed));
            };
            if matches!(payload.progress, WriteProgress::BeforeFirstPoll) {
                if let Err(cause) = payload.loan.admit_current_then_release_guard(
                    &self.connection.socket_identity,
                    self.connection.device_id,
                ) {
                    return Poll::Ready(WriteResult::Admission(cause));
                }
                payload.progress = WriteProgress::ReadinessPolled;
                match Pin::new(&mut *socket).poll_ready(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(cause)) => {
                        return Poll::Ready(WriteResult::Remote(socket_error(cause)));
                    }
                    Poll::Ready(Ok(())) => {}
                }
            } else if matches!(payload.progress, WriteProgress::ReadinessPolled) {
                match Pin::new(&mut *socket).poll_ready(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(cause)) => {
                        return Poll::Ready(WriteResult::Remote(socket_error(cause)));
                    }
                    Poll::Ready(Ok(())) => {}
                }
            }
            if matches!(payload.progress, WriteProgress::ReadinessPolled) {
                let message = match payload.loan.take_preencoded_for_start_send() {
                    Ok(message) => message,
                    Err(cause) => return Poll::Ready(WriteResult::Admission(cause)),
                };
                payload.progress = WriteProgress::FrameStarted;
                if let Err(cause) = Pin::new(&mut *socket).start_send(message) {
                    return Poll::Ready(WriteResult::Remote(socket_error(cause)));
                }
            }
            match Pin::new(socket).poll_flush(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(cause)) => Poll::Ready(WriteResult::Remote(socket_error(cause))),
                Poll::Ready(Ok(())) => {
                    payload.progress = WriteProgress::Flushed;
                    Poll::Ready(WriteResult::Flushed)
                }
            }
        }
    }

    impl<'window> Future for SettingsWriter<'_, 'window> {
        type Output = SettingsSocketWriteReturn<'window>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let result = catch_unwind(AssertUnwindSafe(|| self.poll_write(cx)));
            let result = match result {
                Ok(Poll::Pending) => return Poll::Pending,
                Ok(Poll::Ready(result)) => result,
                Err(payload) => WriteResult::Panicked(payload),
            };
            let Some(mut payload) = self.payload.take() else {
                panic!("completed Settings writer was polled again");
            };
            payload.loan.release_guard_on_drop();
            if !matches!(&result, WriteResult::Flushed) {
                self.connection.socket_identity.close();
                drop(self.connection.socket.take());
            }
            let outcome = SettingsSocketWriteOutcome {
                result,
                progress: payload.progress,
                returned_abandoned: false,
                socket: Arc::clone(&self.connection.socket_identity),
            };
            Poll::Ready(SettingsSocketWriteReturn {
                payload: Some(WriteReturnPayload {
                    outcome,
                    loan: payload.loan,
                }),
            })
        }
    }

    impl Drop for SettingsWriter<'_, '_> {
        fn drop(&mut self) {
            if let Some(mut payload) = self.payload.take() {
                payload.loan.release_guard_on_drop();
                self.connection.socket_identity.close();
                drop(self.connection.socket.take());
                let outcome = SettingsSocketWriteOutcome {
                    result: WriteResult::FutureDropped,
                    progress: payload.progress,
                    returned_abandoned: false,
                    socket: Arc::clone(&self.connection.socket_identity),
                };
                payload.loan.record_actual_write(outcome);
            }
        }
    }

    impl SettingsSocketWriteReturn<'_> {
        pub(crate) fn register(mut self) {
            if let Some(payload) = self.payload.take() {
                payload.loan.record_actual_write(payload.outcome);
            }
        }
    }

    impl Drop for SettingsSocketWriteReturn<'_> {
        fn drop(&mut self) {
            if let Some(mut payload) = self.payload.take() {
                payload.outcome.returned_abandoned = true;
                payload.loan.record_actual_write(payload.outcome);
            }
        }
    }

    impl SettingsSocketWriteOutcome {
        pub(crate) fn fence_abandoned_return(&self) {
            if self.returned_abandoned {
                self.socket.close();
            }
        }

        pub(crate) fn observation(&self) -> Result<(), SettingsSocketObserveFailure> {
            if matches!(self.result, WriteResult::Flushed)
                && matches!(self.progress, WriteProgress::Flushed)
                && !self.returned_abandoned
            {
                Ok(())
            } else {
                Err(SettingsSocketObserveFailure::WRITE)
            }
        }
    }

    impl fmt::Debug for SettingsSocketWriteOutcome {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("SettingsSocketWriteOutcome([retained result])")
        }
    }
}

#[cfg(unix)]
pub(crate) use settings_write::{SettingsSocketWriteOutcome, SettingsSocketWriteReturn};
