use super::{
    database::Database,
    error::RelayError,
    server::{now_ms, send_frame, send_socket},
};
use crate::{
    crypto::{PairingAcceptance, PairingRequest},
    protocol::{DeviceId, MessageId},
    relay_protocol::{RELAY_SCHEMA_VERSION, RelayErrorCode, RelayServerMessage},
};
use axum::extract::ws::{Message, WebSocket};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{broadcast, oneshot},
    time::{Instant, interval_at, sleep_until},
};
use tokio_util::sync::CancellationToken;

const MAX_WAITERS: usize = 32;
const MAX_WAITERS_PER_HOST: usize = 4;
const HEARTBEAT: Duration = Duration::from_secs(20);

type Decision = Result<PairingAcceptance, RelayErrorCode>;

struct Waiter {
    token: u64,
    host: DeviceId,
    frame: RelayServerMessage,
    request: PairingRequest,
    decision: oneshot::Sender<Decision>,
}

#[derive(Clone, Default)]
pub(super) struct Rendezvous {
    inner: Arc<Mutex<RendezvousInner>>,
}

#[derive(Default)]
struct RendezvousInner {
    next: u64,
    waiters: HashMap<MessageId, Waiter>,
}

struct Registration {
    rendezvous: Rendezvous,
    pairing_id: MessageId,
    token: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.rendezvous.inner.lock()
            && inner
                .waiters
                .get(&self.pairing_id)
                .is_some_and(|waiter| waiter.token == self.token)
        {
            inner.waiters.remove(&self.pairing_id);
        }
    }
}

fn poisoned() -> RelayError {
    RelayError::Configuration("pairing rendezvous lock poisoned".to_owned())
}

impl Rendezvous {
    fn register(
        &self,
        request: PairingRequest,
        device_name: String,
        name_signature: [u8; 64],
    ) -> Result<(Registration, oneshot::Receiver<Decision>), RelayError> {
        let mut inner = self.inner.lock().map_err(|_| poisoned())?;
        let pairing_id = request.pairing_id;
        if inner.waiters.contains_key(&pairing_id) {
            return Err(RelayError::Pairing(RelayErrorCode::PairingInUse));
        }
        let host = request.inviter;
        if inner.waiters.len() >= MAX_WAITERS
            || inner
                .waiters
                .values()
                .filter(|waiter| waiter.host == host)
                .count()
                >= MAX_WAITERS_PER_HOST
        {
            return Err(RelayError::QueueFull);
        }
        inner.next += 1;
        let token = inner.next;
        let (sender, receiver) = oneshot::channel();
        inner.waiters.insert(
            pairing_id,
            Waiter {
                token,
                host,
                frame: RelayServerMessage::PairingRequest {
                    schema_version: RELAY_SCHEMA_VERSION,
                    request: request.clone(),
                    device_name,
                    name_signature,
                },
                request,
                decision: sender,
            },
        );
        Ok((
            Registration {
                rendezvous: self.clone(),
                pairing_id,
                token,
            },
            receiver,
        ))
    }

    pub(super) fn undelivered(
        &self,
        host: DeviceId,
        delivered: &mut HashSet<u64>,
    ) -> Result<Vec<RelayServerMessage>, RelayError> {
        let inner = self.inner.lock().map_err(|_| poisoned())?;
        let live: HashSet<u64> = inner
            .waiters
            .values()
            .filter(|waiter| waiter.host == host)
            .map(|waiter| waiter.token)
            .collect();
        delivered.retain(|token| live.contains(token));
        Ok(inner
            .waiters
            .values()
            .filter(|waiter| waiter.host == host && delivered.insert(waiter.token))
            .map(|waiter| waiter.frame.clone())
            .collect())
    }

    pub(super) async fn decide(
        &self,
        database: &Database,
        host: DeviceId,
        pairing_id: MessageId,
        responder: DeviceId,
        acceptance: Option<PairingAcceptance>,
        reason: Option<RelayErrorCode>,
    ) -> Result<(), RelayError> {
        let waiter = {
            let mut inner = self.inner.lock().map_err(|_| poisoned())?;
            match inner.waiters.get(&pairing_id) {
                Some(waiter)
                    if waiter.host == host && waiter.request.responder.device_id == responder =>
                {
                    inner.waiters.remove(&pairing_id)
                }
                _ => None,
            }
        };
        let Some(waiter) = waiter else {
            return Ok(());
        };
        let decision = match acceptance {
            None => Err(match reason {
                Some(
                    code @ (RelayErrorCode::PairingExpired
                    | RelayErrorCode::PairingInUse
                    | RelayErrorCode::PairingUnavailable),
                ) => code,
                _ => RelayErrorCode::PairingRejected,
            }),
            Some(acceptance) => match sponsor(database, host, &waiter.request, &acceptance).await {
                Ok(()) => Ok(acceptance),
                Err(error) => {
                    eprintln!("relay pairing sponsorship refused: {error}");
                    Err(match error {
                        RelayError::Pairing(code) => code,
                        _ => RelayErrorCode::PairingUnavailable,
                    })
                }
            },
        };
        let _ = waiter.decision.send(decision);
        Ok(())
    }
}

async fn sponsor(
    database: &Database,
    host: DeviceId,
    request: &PairingRequest,
    acceptance: &PairingAcceptance,
) -> Result<(), RelayError> {
    let keys = database.pairing_host_keys(host).await?;
    acceptance.verify_for(&keys, request)?;
    if acceptance.expires_at_ms <= now_ms()? {
        return Err(RelayError::Pairing(RelayErrorCode::PairingExpired));
    }
    database
        .enroll_sponsored(request.responder.clone(), host, now_ms()?)
        .await?;
    Ok(())
}

pub(super) struct PairingContext<'a> {
    pub database: &'a Database,
    pub rendezvous: &'a Rendezvous,
    pub active_sessions: &'a Arc<Mutex<HashSet<DeviceId>>>,
    pub notifications: &'a broadcast::Sender<DeviceId>,
    pub cancellation: &'a CancellationToken,
}

pub(super) async fn run(
    socket: &mut WebSocket,
    context: PairingContext<'_>,
    request: PairingRequest,
    device_name: String,
    name_signature: [u8; 64],
) -> Result<(), RelayError> {
    request.verify_possession()?;
    request.verify_name(&device_name, &name_signature)?;
    let now = now_ms()?;
    if request.expires_at_ms <= now {
        return Err(RelayError::Pairing(RelayErrorCode::PairingExpired));
    }
    let host = request.inviter;
    context.database.pairing_host_keys(host).await?;
    context
        .database
        .enrollable(request.responder.clone())
        .await
        .map_err(|error| match error {
            RelayError::DeviceConflict => RelayError::Pairing(RelayErrorCode::PairingUnavailable),
            error => error,
        })?;
    let online = context
        .active_sessions
        .lock()
        .map_err(|_| poisoned())?
        .contains(&host);
    if !online {
        return Err(RelayError::RecipientUnavailable);
    }
    let expires = Instant::now()
        + Duration::from_millis(
            (request.expires_at_ms - now).min(crate::crypto::MAX_PAIRING_LIFETIME_MS),
        );
    let (_registration, mut decision) =
        context
            .rendezvous
            .register(request, device_name, name_signature)?;
    send_frame(
        socket,
        &RelayServerMessage::PairingPending {
            schema_version: RELAY_SCHEMA_VERSION,
        },
        context.cancellation,
    )
    .await?;
    let _ = context.notifications.send(host);
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    loop {
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => return Err(RelayError::Cancelled),
            result = &mut decision => {
                return match result {
                    Ok(Ok(acceptance)) => send_frame(
                        socket,
                        &RelayServerMessage::PairingAccepted {
                            schema_version: RELAY_SCHEMA_VERSION,
                            acceptance,
                        },
                        context.cancellation,
                    )
                    .await,
                    Ok(Err(code)) => Err(RelayError::Pairing(code)),
                    Err(_) => Err(RelayError::Pairing(RelayErrorCode::PairingUnavailable)),
                };
            }
            _ = sleep_until(expires) => return Err(RelayError::Pairing(RelayErrorCode::PairingExpired)),
            frame = socket.recv() => match frame {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(error)) => return Err(RelayError::WebSocket(error.to_string())),
                Some(Ok(_)) => return Err(RelayError::AuthenticationFailed),
            },
            _ = heartbeat.tick() => {
                send_socket(socket, Message::Ping(Vec::new().into()), context.cancellation).await?;
            }
        }
    }
}
