use crate::{
    database::{DELIVERY_WINDOW, Database},
    error::RelayError,
};
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use everything::{
    crypto::DevicePublicKeys,
    protocol::{DeviceId, MessageId, UnixMillis},
    relay_protocol::{
        AUTH_CHALLENGE_LIFETIME_MS, MAX_RELAY_FRAME_BYTES, RELAY_SCHEMA_VERSION, RelayChallenge,
        RelayClientMessage, RelayErrorCode, RelayOrigin, RelayServerMessage,
    },
};
use hyper::server::conn::http1;
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::{
    collections::HashSet,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, broadcast},
    time::{Instant, interval_at, sleep_until, timeout},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_AUTH_FRAME_BYTES: usize = 4_096;

#[derive(Clone)]
struct ServerState {
    database: Database,
    origin: RelayOrigin,
    cancellation: CancellationToken,
    tasks: TaskTracker,
    active_sessions: Arc<Mutex<HashSet<DeviceId>>>,
    notifications: broadcast::Sender<DeviceId>,
    authentication_admission: Arc<Mutex<AuthenticationAdmission>>,
}

pub async fn run(
    database: Database,
    bind: SocketAddr,
    origin: RelayOrigin,
    max_connections: usize,
    cancellation: CancellationToken,
) -> Result<(), RelayError> {
    if max_connections == 0 || max_connections > 256 {
        return Err(RelayError::Configuration(
            "max-connections must be between 1 and 256".to_owned(),
        ));
    }
    let listener = TcpListener::bind(bind).await?;
    let address = listener.local_addr()?;
    let tasks = TaskTracker::new();
    let (notifications, _) = broadcast::channel(256);
    let state = ServerState {
        database: database.clone(),
        origin,
        cancellation: cancellation.clone(),
        tasks: tasks.clone(),
        active_sessions: Arc::new(Mutex::new(HashSet::new())),
        notifications,
        authentication_admission: Arc::new(Mutex::new(AuthenticationAdmission {
            window_started: Instant::now(),
            attempts: 0,
        })),
    };
    let router = Router::new()
        .route("/ws", get(websocket))
        .route("/health", get(|| async { StatusCode::NO_CONTENT }))
        .with_state(state);
    let capacity = Arc::new(Semaphore::new(max_connections));
    let mut maintenance = interval_at(Instant::now(), HEARTBEAT_INTERVAL);
    eprintln!("Everything relay listening on {address}");
    let result = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break Ok(()),
            _ = maintenance.tick() => {
                let result = match now_ms() {
                    Ok(now) => database.prune(now).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    break Err(error);
                }
            }
            accepted = accept_bounded(&listener, capacity.clone()) => {
                let (stream, permit) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                let service = TowerToHyperService::new(router.clone());
                let shutdown = cancellation.clone();
                tasks.spawn(async move {
                    let io = TokioIo::new(LimitedStream { stream, _permit: permit });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10))
                        .max_buf_size(16 * 1_024)
                        .keep_alive(true);
                    let connection = builder.serve_connection(io, service).with_upgrades();
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = shutdown.cancelled() => {}
                        result = &mut connection => {
                            if result.is_err() {
                                eprintln!("relay HTTP connection ended with a protocol or network error");
                            }
                        }
                    }
                });
            }
        }
    };
    cancellation.cancel();
    tasks.close();
    timeout(Duration::from_secs(10), tasks.wait())
        .await
        .map_err(|_| RelayError::Timeout)?;
    result
}

async fn accept_bounded(
    listener: &TcpListener,
    capacity: Arc<Semaphore>,
) -> Result<(TcpStream, OwnedSemaphorePermit), RelayError> {
    let permit = capacity
        .acquire_owned()
        .await
        .map_err(|_| RelayError::Cancelled)?;
    let (stream, _) = listener.accept().await?;
    stream.set_nodelay(true)?;
    Ok((stream, permit))
}

async fn websocket(State(state): State<ServerState>, upgrade: WebSocketUpgrade) -> Response {
    if state.cancellation.is_cancelled() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let token = state.tasks.token();
    upgrade
        .max_message_size(MAX_RELAY_FRAME_BYTES)
        .max_frame_size(MAX_RELAY_FRAME_BYTES)
        .write_buffer_size(16 * 1_024)
        .max_write_buffer_size(MAX_RELAY_FRAME_BYTES + 64 * 1_024)
        .on_upgrade(move |mut socket| async move {
            let _token = token;
            let result = run_session(&mut socket, &state).await;
            if let Err(error) = result {
                if !matches!(error, RelayError::Cancelled) {
                    let code = error_code(&error);
                    eprintln!("relay WebSocket session ended: {code:?}");
                    let frame = RelayServerMessage::Error {
                        schema_version: RELAY_SCHEMA_VERSION,
                        code,
                    };
                    if send_frame(&mut socket, &frame, &state.cancellation)
                        .await
                        .is_err()
                    {
                        eprintln!("relay could not send the session error to a disconnected peer");
                    }
                }
            }
            match timeout(Duration::from_secs(1), socket.send(Message::Close(None))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => eprintln!("relay WebSocket peer disconnected before close"),
                Err(_) => eprintln!("relay WebSocket close timed out"),
            }
        })
        .into_response()
}

async fn run_session(socket: &mut WebSocket, state: &ServerState) -> Result<(), RelayError> {
    let keys = tokio::select! {
        _ = state.cancellation.cancelled() => return Err(RelayError::Cancelled),
        result = timeout(Duration::from_millis(AUTH_CHALLENGE_LIFETIME_MS), authenticate(socket, state)) => {
            result.map_err(|_| RelayError::Timeout)??
        }
    };
    let _lease = SessionLease::acquire(keys.device_id, state.active_sessions.clone())?;
    send_frame(
        socket,
        &RelayServerMessage::Authenticated {
            schema_version: RELAY_SCHEMA_VERSION,
            device_id: keys.device_id,
        },
        &state.cancellation,
    )
    .await?;
    let mut notifications = state.notifications.subscribe();
    let mut delivered = HashSet::with_capacity(DELIVERY_WINDOW);
    deliver_pending(socket, state, keys.device_id, &mut delivered).await?;
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
    let mut last_activity = Instant::now();
    loop {
        let trigger = tokio::select! {
            biased;
            _ = state.cancellation.cancelled() => return Err(RelayError::Cancelled),
            _ = sleep_until(last_activity + IDLE_TIMEOUT) => return Err(RelayError::Timeout),
            frame = socket.recv() => Trigger::Inbound(frame),
            notification = notifications.recv() => Trigger::Notification(notification),
            _ = heartbeat.tick() => Trigger::Heartbeat,
        };
        match trigger {
            Trigger::Inbound(Some(Ok(Message::Close(_)))) | Trigger::Inbound(None) => return Ok(()),
            Trigger::Inbound(Some(Err(error))) => {
                return Err(RelayError::WebSocket(error.to_string()));
            }
            Trigger::Inbound(Some(Ok(message))) => {
                last_activity = Instant::now();
                let size = match &message {
                    Message::Text(bytes) => bytes.len(),
                    Message::Binary(bytes) | Message::Ping(bytes) | Message::Pong(bytes) => {
                        bytes.len()
                    }
                    Message::Close(_) => 0,
                };
                state
                    .database
                    .charge(keys.device_id, size, now_ms()?)
                    .await?;
                match message {
                    Message::Text(bytes) => {
                        process_frame(
                            socket,
                            state,
                            keys.device_id,
                            bytes.as_bytes(),
                            &mut delivered,
                        )
                        .await?;
                    }
                    Message::Binary(bytes) => {
                        process_frame(socket, state, keys.device_id, &bytes, &mut delivered)
                            .await?;
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => return Ok(()),
                }
            }
            Trigger::Notification(Ok(recipient)) if recipient == keys.device_id => {
                deliver_pending(socket, state, keys.device_id, &mut delivered).await?;
            }
            Trigger::Notification(Ok(_)) => {}
            Trigger::Notification(Err(broadcast::error::RecvError::Lagged(_))) => {
                deliver_pending(socket, state, keys.device_id, &mut delivered).await?;
            }
            Trigger::Notification(Err(broadcast::error::RecvError::Closed)) => {
                return Err(RelayError::Cancelled);
            }
            Trigger::Heartbeat => {
                state.database.active_keys(keys.device_id).await?;
                deliver_pending(socket, state, keys.device_id, &mut delivered).await?;
                send_socket(
                    socket,
                    Message::Ping(Vec::new().into()),
                    &state.cancellation,
                )
                .await?;
            }
        }
    }
}

enum Trigger {
    Inbound(Option<Result<Message, axum::Error>>),
    Notification(Result<DeviceId, broadcast::error::RecvError>),
    Heartbeat,
}

async fn authenticate(
    socket: &mut WebSocket,
    state: &ServerState,
) -> Result<DevicePublicKeys, RelayError> {
    {
        let mut admission = state.authentication_admission.lock().map_err(|_| {
            RelayError::Configuration("authentication admission lock poisoned".to_owned())
        })?;
        admission.admit()?;
    }
    let begin_bytes = authentication_frame(socket).await?;
    let RelayClientMessage::BeginAuthentication { device_id, .. } =
        RelayClientMessage::parse(&begin_bytes)?
    else {
        return Err(RelayError::AuthenticationFailed);
    };
    let now = now_ms()?;
    let keys = state.database.active_keys(device_id).await?;
    let challenge = RelayChallenge::new(state.origin.clone(), device_id, now)?;
    send_frame(
        socket,
        &RelayServerMessage::Challenge {
            schema_version: RELAY_SCHEMA_VERSION,
            challenge: challenge.clone(),
        },
        &state.cancellation,
    )
    .await?;
    let bytes = authentication_frame(socket).await?;
    let RelayClientMessage::Authenticate {
        session_id,
        signature,
        ..
    } = RelayClientMessage::parse(&bytes)?
    else {
        return Err(RelayError::AuthenticationFailed);
    };
    if session_id != challenge.session_id {
        return Err(RelayError::AuthenticationFailed);
    }
    let signed = challenge.signing_bytes(&state.origin, device_id, now_ms()?)?;
    keys.verify_challenge(&signed, &signature)
        .map_err(|_| RelayError::AuthenticationFailed)?;
    state.database.active_keys(device_id).await?;
    state
        .database
        .charge(device_id, begin_bytes.len() + bytes.len(), now_ms()?)
        .await?;
    Ok(keys)
}

async fn authentication_frame(socket: &mut WebSocket) -> Result<Vec<u8>, RelayError> {
    let frame = socket
        .recv()
        .await
        .ok_or(RelayError::AuthenticationFailed)?
        .map_err(|error| RelayError::WebSocket(error.to_string()))?;
    let bytes = match frame {
        Message::Text(bytes) => bytes.as_bytes().to_vec(),
        Message::Binary(bytes) => bytes.to_vec(),
        _ => return Err(RelayError::AuthenticationFailed),
    };
    if bytes.len() > MAX_AUTH_FRAME_BYTES {
        return Err(RelayError::AuthenticationFailed);
    }
    Ok(bytes)
}

async fn process_frame(
    socket: &mut WebSocket,
    state: &ServerState,
    device_id: DeviceId,
    bytes: &[u8],
    delivered: &mut HashSet<MessageId>,
) -> Result<(), RelayError> {
    match RelayClientMessage::parse(bytes)? {
        RelayClientMessage::Send { envelope, .. } => {
            let message_id = envelope.header.message_id;
            let recipient = envelope.header.recipient;
            let duplicate = state
                .database
                .enqueue(device_id, envelope, now_ms()?)
                .await?;
            send_frame(
                socket,
                &RelayServerMessage::Accepted {
                    schema_version: RELAY_SCHEMA_VERSION,
                    message_id,
                    duplicate,
                },
                &state.cancellation,
            )
            .await?;
            if !duplicate {
                match state.notifications.send(recipient) {
                    Ok(_) => {}
                    Err(_) => {
                        eprintln!("relay recipient is offline; ciphertext remains durably queued")
                    }
                }
            }
        }
        RelayClientMessage::Acknowledge { message_id, .. } => {
            state.database.acknowledge(device_id, message_id).await?;
            delivered.remove(&message_id);
            send_frame(
                socket,
                &RelayServerMessage::Acknowledged {
                    schema_version: RELAY_SCHEMA_VERSION,
                    message_id,
                },
                &state.cancellation,
            )
            .await?;
            deliver_pending(socket, state, device_id, delivered).await?;
        }
        RelayClientMessage::Ping { .. } => {
            send_frame(
                socket,
                &RelayServerMessage::Pong {
                    schema_version: RELAY_SCHEMA_VERSION,
                },
                &state.cancellation,
            )
            .await?;
        }
        RelayClientMessage::Authenticate { .. }
        | RelayClientMessage::BeginAuthentication { .. } => {
            return Err(RelayError::AuthenticationFailed);
        }
    }
    Ok(())
}

async fn deliver_pending(
    socket: &mut WebSocket,
    state: &ServerState,
    recipient: DeviceId,
    delivered: &mut HashSet<MessageId>,
) -> Result<(), RelayError> {
    let pending = state.database.pending(recipient, now_ms()?).await?;
    let current: HashSet<_> = pending
        .iter()
        .map(|envelope| envelope.header.message_id)
        .collect();
    delivered.retain(|message_id| current.contains(message_id));
    for envelope in pending {
        let message_id = envelope.header.message_id;
        if delivered.contains(&message_id) {
            continue;
        }
        if !state
            .database
            .delivery_available(recipient, message_id, now_ms()?)
            .await?
        {
            continue;
        }
        send_frame(
            socket,
            &RelayServerMessage::Delivery {
                schema_version: RELAY_SCHEMA_VERSION,
                envelope,
            },
            &state.cancellation,
        )
        .await?;
        delivered.insert(message_id);
    }
    Ok(())
}

async fn send_frame(
    socket: &mut WebSocket,
    frame: &RelayServerMessage,
    cancellation: &CancellationToken,
) -> Result<(), RelayError> {
    send_socket(
        socket,
        Message::Binary(frame.to_bytes()?.into()),
        cancellation,
    )
    .await
}

async fn send_socket(
    socket: &mut WebSocket,
    frame: Message,
    cancellation: &CancellationToken,
) -> Result<(), RelayError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(RelayError::Cancelled),
        result = timeout(WRITE_TIMEOUT, socket.send(frame)) => {
            result.map_err(|_| RelayError::Timeout)?
                .map_err(|error| RelayError::WebSocket(error.to_string()))
        }
    }
}

fn error_code(error: &RelayError) -> RelayErrorCode {
    match error {
        RelayError::AuthenticationFailed | RelayError::Timeout => {
            RelayErrorCode::AuthenticationFailed
        }
        RelayError::Protocol(_) => RelayErrorCode::InvalidFrame,
        RelayError::Crypto(_) => RelayErrorCode::InvalidEnvelope,
        RelayError::RecipientUnavailable => RelayErrorCode::RecipientUnavailable,
        RelayError::QueueFull => RelayErrorCode::QueueFull,
        RelayError::MessageConflict => RelayErrorCode::MessageConflict,
        RelayError::RateLimited => RelayErrorCode::RateLimited,
        RelayError::SessionAlreadyActive => RelayErrorCode::SessionAlreadyActive,
        _ => RelayErrorCode::Internal,
    }
}

pub fn now_ms() -> Result<u64, RelayError> {
    UnixMillis::now()
        .map(UnixMillis::get)
        .map_err(|_| RelayError::Clock)
}

struct SessionLease {
    device_id: DeviceId,
    registry: Arc<Mutex<HashSet<DeviceId>>>,
}

impl SessionLease {
    fn acquire(
        device_id: DeviceId,
        registry: Arc<Mutex<HashSet<DeviceId>>>,
    ) -> Result<Self, RelayError> {
        {
            let mut sessions = registry.lock().map_err(|_| {
                RelayError::Configuration("session registry lock poisoned".to_owned())
            })?;
            if !sessions.insert(device_id) {
                return Err(RelayError::SessionAlreadyActive);
            }
        }
        Ok(Self {
            device_id,
            registry,
        })
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        match self.registry.lock() {
            Ok(mut registry) => {
                registry.remove(&self.device_id);
            }
            Err(_) => eprintln!("relay session registry lock poisoned during cleanup"),
        }
    }
}

struct LimitedStream {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

struct AuthenticationAdmission {
    window_started: Instant,
    attempts: usize,
}

impl AuthenticationAdmission {
    fn admit(&mut self) -> Result<(), RelayError> {
        if self.window_started.elapsed() >= Duration::from_secs(60) {
            self.window_started = Instant::now();
            self.attempts = 0;
        }
        if self.attempts >= 1_024 {
            return Err(RelayError::RateLimited);
        }
        self.attempts += 1;
        Ok(())
    }
}
