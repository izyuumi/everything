pub(in crate::devices) mod outbound;

#[cfg(unix)]
use super::settings_connection::SettingsConnectionOwner;
#[cfg(unix)]
use super::settings_ingress::work::SettingsReceiveWorkEvent;
use super::*;
use crate::relay_protocol::{
    RELAY_SCHEMA_VERSION, RelayClientMessage, RelayOrigin, RelayServerMessage,
};
use crate::transport::remote::{RelayConnection, RemoteError};
#[cfg(unix)]
use futures_util::FutureExt;
use outbound::RelaySendWindow;
#[cfg(unix)]
use std::panic::AssertUnwindSafe;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaySessionStatus {
    Stopped,
    Connecting { attempt: u32 },
    Connected,
    Retrying { attempt: u32, reason: RemoteError },
    Maintenance { report: TransportPruneReport },
    Failed { reason: &'static str },
}

#[cfg(unix)]
type SettingsSession<'session, 'owner> = Option<&'session mut SettingsConnectionOwner<'owner>>;
#[cfg(not(unix))]
type SettingsSession<'session, 'owner> = Option<&'session mut (&'owner (), ())>;

#[cfg(unix)]
struct PrivateSends {
    accepted: BTreeMap<MessageId, tokio::time::Instant>,
    delivery: Option<MessageId>,
}

impl DeviceEndpoint {
    pub async fn run_relay(
        &self,
        origin: RelayOrigin,
        cancellation: CancellationToken,
        status: watch::Sender<RelaySessionStatus>,
    ) -> Result<(), DeviceError> {
        let session = self
            .inner
            .session
            .try_lock()
            .map_err(|_| DeviceError::SessionAlreadyRunning)?;
        self.check_outbound_health()?;
        self.run_relay_owned(origin, cancellation, status, None, &session)
            .await
    }

    #[cfg(unix)]
    pub(crate) async fn run_relay_with_settings(
        &self,
        origin: RelayOrigin,
        cancellation: CancellationToken,
        status: watch::Sender<RelaySessionStatus>,
        mut owner: HostSettingsTransportOwner,
    ) -> Result<(), DeviceError> {
        let session = self.inner.session.try_lock();
        let result = match &session {
            Ok(session) => {
                let result = AssertUnwindSafe(self.run_relay_owned(
                    origin,
                    cancellation,
                    status.clone(),
                    Some(&mut owner),
                    session,
                ))
                .catch_unwind()
                .await;
                match result {
                    Ok(result) => result,
                    Err(_) => {
                        owner.latch_failure();
                        Err(DeviceError::Worker)
                    }
                }
            }
            Err(_) => Err(DeviceError::SessionAlreadyRunning),
        };
        if owner.close_and_wait().await.is_err() {
            status.send_replace(RelaySessionStatus::Failed {
                reason: "Settings transport ownership is quarantined; process restart required",
            });
            return Err(match result {
                Err(error) => error,
                Ok(()) => DeviceError::Worker,
            });
        }
        if let Ok(_session) = &session {
            self.check_outbound_health()?;
            if result.is_ok() {
                status.send_replace(RelaySessionStatus::Stopped);
            }
        }
        result
    }

    async fn run_relay_owned(
        &self,
        origin: RelayOrigin,
        cancellation: CancellationToken,
        status: watch::Sender<RelaySessionStatus>,
        mut settings_owner: Option<&mut HostSettingsTransportOwner>,
        _session: &tokio::sync::MutexGuard<'_, ()>,
    ) -> Result<(), DeviceError> {
        self.check_outbound_health()?;
        let mut attempt = 0_u32;
        loop {
            self.check_outbound_health()?;
            if cancellation.is_cancelled() {
                if settings_owner.is_none() {
                    status.send_replace(RelaySessionStatus::Stopped);
                }
                return Ok(());
            }
            self.blocking(|endpoint| endpoint.ensure_active()).await?;
            self.check_outbound_health()?;
            status.send_replace(RelaySessionStatus::Connecting { attempt });
            let connection =
                RelayConnection::connect(origin.clone(), self.identity(), cancellation.clone())
                    .await;
            let result = match connection {
                Ok(mut connection) => {
                    self.check_outbound_health()?;
                    #[cfg(unix)]
                    let mut incarnation = settings_owner
                        .as_mut()
                        .map(|owner| owner.connected_relay(self, &connection))
                        .transpose()?;
                    #[cfg(not(unix))]
                    let mut incarnation: Option<(&(), ())> = None;
                    status.send_replace(RelaySessionStatus::Connected);
                    let started = tokio::time::Instant::now();
                    let result = self
                        .relay_session(
                            &mut connection,
                            &cancellation,
                            &status,
                            incarnation.as_mut(),
                        )
                        .await;
                    drop(incarnation);
                    #[cfg(unix)]
                    if let Some(owner) = settings_owner.as_mut()
                        && owner.drain_connection().await.is_err()
                    {
                        return Err(DeviceError::Worker);
                    }
                    let close = connection.close().await;
                    if started.elapsed() >= Duration::from_secs(30) {
                        attempt = 0;
                    }
                    match (result, close) {
                        (Ok(()), _) if cancellation.is_cancelled() => {
                            self.check_outbound_health()?;
                            if settings_owner.is_none() {
                                status.send_replace(RelaySessionStatus::Stopped);
                            }
                            return Ok(());
                        }
                        (Err(error), _) => Err(error),
                        (Ok(()), Err(error)) => Err(error.into()),
                        (Ok(()), Ok(())) => Err(RemoteError::Closed.into()),
                    }
                }
                Err(RemoteError::Cancelled) if cancellation.is_cancelled() => {
                    self.check_outbound_health()?;
                    if settings_owner.is_none() {
                        status.send_replace(RelaySessionStatus::Stopped);
                    }
                    return Ok(());
                }
                Err(error) => Err(error.into()),
            };
            let reason = match result {
                Err(DeviceError::Remote(
                    reason @ (RemoteError::Socket | RemoteError::Closed | RemoteError::Timeout),
                )) => reason,
                Err(DeviceError::Remote(
                    reason @ RemoteError::Relay(
                        crate::relay_protocol::RelayErrorCode::RateLimited
                        | crate::relay_protocol::RelayErrorCode::QueueFull
                        | crate::relay_protocol::RelayErrorCode::SessionAlreadyActive
                        | crate::relay_protocol::RelayErrorCode::Internal
                        | crate::relay_protocol::RelayErrorCode::RecipientUnavailable,
                    ),
                )) => reason,
                Err(DeviceError::Remote(RemoteError::Cancelled)) if cancellation.is_cancelled() => {
                    self.check_outbound_health()?;
                    if settings_owner.is_none() {
                        status.send_replace(RelaySessionStatus::Stopped);
                    }
                    return Ok(());
                }
                Err(error) => {
                    status.send_replace(RelaySessionStatus::Failed {
                        reason: "endpoint operation failed; operator action required",
                    });
                    return Err(error);
                }
                Ok(()) => RemoteError::Closed,
            };
            self.check_outbound_health()?;
            attempt = attempt.saturating_add(1);
            status.send_replace(RelaySessionStatus::Retrying { attempt, reason });
            let delay = Duration::from_secs(1_u64 << attempt.min(5));
            tokio::select! {
                _ = cancellation.cancelled() => {
                    self.check_outbound_health()?;
                    if settings_owner.is_none() {
                        status.send_replace(RelaySessionStatus::Stopped);
                    }
                    return Ok(());
                }
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    async fn relay_session(
        &self,
        connection: &mut RelayConnection,
        cancellation: &CancellationToken,
        status: &watch::Sender<RelaySessionStatus>,
        settings: SettingsSession<'_, '_>,
    ) -> Result<(), DeviceError> {
        let mut window = RelaySendWindow::new(self.clone())?;
        let outcome = self
            .relay_session_loop(connection, cancellation, status, &mut window, settings)
            .await;
        window.finish(outcome).await
    }

    async fn relay_session_loop(
        &self,
        connection: &mut RelayConnection,
        cancellation: &CancellationToken,
        status: &watch::Sender<RelaySessionStatus>,
        window: &mut RelaySendWindow,
        mut settings: SettingsSession<'_, '_>,
    ) -> Result<(), DeviceError> {
        let mut acknowledgements = BTreeSet::new();
        #[cfg(unix)]
        let mut private = PrivateSends {
            accepted: BTreeMap::new(),
            delivery: None,
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut heartbeat = tokio::time::Instant::now();
        loop {
            #[cfg(unix)]
            let receiving = private.delivery.is_none();
            #[cfg(not(unix))]
            let receiving = true;
            #[cfg(unix)]
            let settings_busy = settings.as_ref().is_some_and(|owner| owner.busy());
            #[cfg(unix)]
            let settings_event = async {
                match settings.as_deref_mut() {
                    Some(owner) if settings_busy => owner.next_event().await,
                    _ => std::future::pending().await,
                }
            };
            #[cfg(not(unix))]
            let settings_event = std::future::pending::<()>();
            let event = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                frame = connection.receive(), if receiving => SessionEvent::Frame(Box::new(frame?)),
                event = settings_event => SessionEvent::Settings(event),
                _ = tick.tick() => SessionEvent::Idle,
                _ = self.inner.outbox_ready.notified() => SessionEvent::Idle,
            };
            match event {
                SessionEvent::Frame(frame) => match *frame {
                    RelayServerMessage::Accepted { message_id, .. } => {
                        #[cfg(unix)]
                        if private.accepted.remove(&message_id).is_some() {
                            continue;
                        }
                        let accepted = window.accepted(message_id)?;
                        self.blocking(move |endpoint| {
                            endpoint
                                .inner
                                .store
                                .accept_transport_outbox(&accepted.into_message_id().to_string())?;
                            Ok(())
                        })
                        .await?;
                    }
                    RelayServerMessage::Delivery { envelope, .. } => {
                        let id = envelope.header.message_id;
                        if acknowledgements.len() >= 128 {
                            return Err(RemoteError::Protocol.into());
                        }
                        #[cfg(unix)]
                        if let Some(owner) = settings.as_deref_mut() {
                            let authority = owner.receipt_authority()?;
                            match owner.try_receive(envelope, authority) {
                                Ok(()) => private.delivery = Some(id),
                                Err(SettingsReceiveError::Capacity) => {}
                                Err(_) => return Err(DeviceError::Worker),
                            }
                            continue;
                        }
                        let now = UnixMillis::now()?;
                        self.blocking(move |endpoint| endpoint.receive_envelope(&envelope, now))
                            .await?;
                        acknowledge(connection, &mut acknowledgements, id).await?;
                    }
                    RelayServerMessage::Acknowledged { message_id, .. } => {
                        if !acknowledgements.remove(&message_id) {
                            return Err(RemoteError::Protocol.into());
                        }
                    }
                    RelayServerMessage::Pong { .. } => {}
                    RelayServerMessage::PairingRequest {
                        request,
                        device_name,
                        name_signature,
                        ..
                    } => {
                        let now = UnixMillis::now()?;
                        self.blocking(move |endpoint| {
                            endpoint.receive_pairing_request(
                                request,
                                device_name,
                                name_signature,
                                now,
                            )
                        })
                        .await?;
                    }
                    _ => return Err(RemoteError::Protocol.into()),
                },
                #[cfg(unix)]
                SessionEvent::Settings(event) => {
                    let Some(owner) = settings.as_deref_mut() else {
                        return Err(DeviceError::Worker);
                    };
                    match event.map_err(|_| DeviceError::Worker)? {
                        Some(SettingsReceiveWorkEvent::Received(receipt)) => {
                            let Some(id) = private.delivery.take() else {
                                return Err(DeviceError::Worker);
                            };
                            if let Ok(receipt) = receipt {
                                if receipt.message_id() != id {
                                    return Err(DeviceError::Worker);
                                }
                                acknowledge(connection, &mut acknowledgements, id).await?;
                            }
                        }
                        Some(SettingsReceiveWorkEvent::ReplyQueued(message_id)) => {
                            self.send_settings_reply(owner, connection, message_id, &mut private)
                                .await?;
                        }
                        None => return Err(DeviceError::Worker),
                    }
                }
                #[cfg(not(unix))]
                SessionEvent::Settings(()) => {}
                SessionEvent::Idle => {}
            }
            if cancellation.is_cancelled() {
                return Ok(());
            }
            if heartbeat.elapsed() >= Duration::from_secs(30) {
                connection
                    .send(&RelayClientMessage::Ping {
                        schema_version: RELAY_SCHEMA_VERSION,
                    })
                    .await?;
                heartbeat = tokio::time::Instant::now();
            }
            for reply in self.take_pairing_replies()? {
                connection.send(&reply).await?;
            }
            #[cfg(unix)]
            private
                .accepted
                .retain(|_, sent_at| sent_at.elapsed() <= Duration::from_secs(15));
            let now = UnixMillis::now()?;
            let report = self
                .blocking(move |endpoint| endpoint.maintain(now))
                .await?;
            for id in &report.expired_outbox_ids {
                if let Ok(id) = id.parse::<MessageId>() {
                    window.expired(id)?;
                }
            }
            if !report.expired_outbox_ids.is_empty() || report.archived_command_payloads > 0 {
                status.send_replace(RelaySessionStatus::Maintenance { report });
            }
            window.check_timeout()?;
            if window.has_capacity() {
                self.check_outbound_health()?;
                let excluded = window.excluded();
                let candidate = self
                    .blocking(move |endpoint| {
                        Ok(endpoint
                            .inner
                            .store
                            .next_transport_outbound_candidate(time(now)?, &excluded)?)
                    })
                    .await?;
                if let Some(candidate) = candidate {
                    if cancellation.is_cancelled() {
                        return Ok(());
                    }
                    window
                        .prepare_and_send(candidate, now, cancellation.clone(), connection)
                        .await?;
                }
            }
        }
    }

    #[cfg(unix)]
    async fn send_settings_reply(
        &self,
        owner: &mut SettingsConnectionOwner<'_>,
        connection: &mut RelayConnection,
        message_id: MessageId,
        private: &mut PrivateSends,
    ) -> Result<(), DeviceError> {
        let Some(reply) = owner.take_queued_reply(message_id) else {
            owner.latch_failure();
            return Err(DeviceError::Worker);
        };
        let guard = Arc::clone(&owner.mutation()).lock_owned().await;
        let loaded = tokio::task::spawn_blocking({
            let endpoint = self.clone();
            move || {
                let loaded = match reply.request() {
                    Ok(SettingsReplyBuildRequest::Access(request)) => {
                        endpoint.inner.store.load_private_settings_ciphertext(
                            &endpoint,
                            request.verified_request().receipt_authority(),
                            &guard,
                            &reply,
                        )
                    }
                    Err(_) => Err(Box::new(
                        crate::storage::SettingsCiphertextLoadFailure::BeforeTransaction {
                            cause: SettingsReplyPublicationError::Conflict.into(),
                        },
                    )),
                };
                drop(guard);
                (reply, loaded)
            }
        })
        .await;
        let Ok((reply, loaded)) = loaded else {
            owner.latch_failure();
            return Err(DeviceError::Worker);
        };
        let sent = match loaded {
            Ok(frame) => Some(connection.send(&frame).await),
            Err(failure) if failure.rolled_back() => None,
            Err(_) => {
                owner.latch_failure();
                drop(reply);
                return Err(DeviceError::Worker);
            }
        };
        let retired = self
            .blocking(move |endpoint| {
                let retired = endpoint.inner.store.retire_private_settings_reply(&reply);
                match retired {
                    Ok(()) => {
                        reply.retire();
                        Ok(true)
                    }
                    Err(_) => Ok(false),
                }
            })
            .await?;
        if !retired {
            owner.latch_failure();
            return Err(DeviceError::Worker);
        }
        match sent {
            Some(Ok(())) => {
                private
                    .accepted
                    .insert(message_id, tokio::time::Instant::now());
                Ok(())
            }
            Some(Err(error)) => Err(error.into()),
            None => Ok(()),
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(DeviceEndpoint) -> Result<T, DeviceError> + Send + 'static,
    ) -> Result<T, DeviceError> {
        let endpoint = self.clone();
        tokio::task::spawn_blocking(move || work(endpoint))
            .await
            .map_err(|_| DeviceError::Worker)?
    }
}

enum SessionEvent<T> {
    Frame(Box<RelayServerMessage>),
    Settings(T),
    Idle,
}

async fn acknowledge(
    connection: &mut RelayConnection,
    acknowledgements: &mut BTreeSet<MessageId>,
    id: MessageId,
) -> Result<(), DeviceError> {
    acknowledgements.insert(id);
    connection
        .send(&RelayClientMessage::Acknowledge {
            schema_version: RELAY_SCHEMA_VERSION,
            message_id: id,
        })
        .await?;
    Ok(())
}
