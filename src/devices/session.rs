use super::*;
use crate::relay_protocol::{
    RELAY_SCHEMA_VERSION, RelayClientMessage, RelayOrigin, RelayServerMessage,
};
use crate::transport::remote::{RelayConnection, RemoteError};
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

impl DeviceEndpoint {
    pub async fn run_relay(
        &self,
        origin: RelayOrigin,
        cancellation: CancellationToken,
        status: watch::Sender<RelaySessionStatus>,
    ) -> Result<(), DeviceError> {
        let _session = self
            .inner
            .session
            .try_lock()
            .map_err(|_| DeviceError::SessionAlreadyRunning)?;
        let mut attempt = 0_u32;
        loop {
            if cancellation.is_cancelled() {
                status.send_replace(RelaySessionStatus::Stopped);
                return Ok(());
            }
            self.blocking(|endpoint| endpoint.ensure_active()).await?;
            status.send_replace(RelaySessionStatus::Connecting { attempt });
            let connection =
                RelayConnection::connect(origin.clone(), self.identity(), cancellation.clone())
                    .await;
            let result = match connection {
                Ok(mut connection) => {
                    status.send_replace(RelaySessionStatus::Connected);
                    let started = tokio::time::Instant::now();
                    let result = self
                        .relay_session(&mut connection, &cancellation, &status)
                        .await;
                    let close = connection.close().await;
                    if started.elapsed() >= Duration::from_secs(30) {
                        attempt = 0;
                    }
                    match (result, close) {
                        (Ok(()), _) if cancellation.is_cancelled() => {
                            status.send_replace(RelaySessionStatus::Stopped);
                            return Ok(());
                        }
                        (Err(error), _) => Err(error),
                        (Ok(()), Err(error)) => Err(error.into()),
                        (Ok(()), Ok(())) => Err(RemoteError::Closed.into()),
                    }
                }
                Err(RemoteError::Cancelled) if cancellation.is_cancelled() => {
                    status.send_replace(RelaySessionStatus::Stopped);
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
                    status.send_replace(RelaySessionStatus::Stopped);
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
            attempt = attempt.saturating_add(1);
            status.send_replace(RelaySessionStatus::Retrying { attempt, reason });
            let delay = Duration::from_secs(1_u64 << attempt.min(5));
            tokio::select! {
                _ = cancellation.cancelled() => {
                    status.send_replace(RelaySessionStatus::Stopped);
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
    ) -> Result<(), DeviceError> {
        let mut inflight = BTreeMap::new();
        let mut acknowledgements = BTreeSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut heartbeat = tokio::time::Instant::now();
        loop {
            let frame = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                frame = connection.receive() => Some(frame?),
                _ = tick.tick() => None,
                _ = self.inner.outbox_ready.notified() => None,
            };
            if let Some(frame) = frame {
                match frame {
                    RelayServerMessage::Accepted { message_id, .. } => {
                        if inflight.remove(&message_id).is_none() {
                            return Err(RemoteError::Protocol.into());
                        }
                        self.blocking(move |endpoint| {
                            endpoint
                                .inner
                                .store
                                .accept_transport_outbox(&message_id.to_string())?;
                            Ok(())
                        })
                        .await?;
                    }
                    RelayServerMessage::Delivery { envelope, .. } => {
                        let id = envelope.header.message_id;
                        let now = UnixMillis::now()?;
                        self.blocking(move |endpoint| endpoint.receive_envelope(&envelope, now))
                            .await?;
                        if acknowledgements.len() >= 128 {
                            return Err(RemoteError::Protocol.into());
                        }
                        acknowledgements.insert(id);
                        connection
                            .send(&RelayClientMessage::Acknowledge {
                                schema_version: RELAY_SCHEMA_VERSION,
                                message_id: id,
                            })
                            .await?;
                    }
                    RelayServerMessage::Acknowledged { message_id, .. } => {
                        if !acknowledgements.remove(&message_id) {
                            return Err(RemoteError::Protocol.into());
                        }
                    }
                    RelayServerMessage::Pong { .. } => {}
                    _ => return Err(RemoteError::Protocol.into()),
                }
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
            let now = UnixMillis::now()?;
            let report = self
                .blocking(move |endpoint| endpoint.maintain(now))
                .await?;
            for id in &report.expired_outbox_ids {
                if let Ok(id) = id.parse::<MessageId>() {
                    inflight.remove(&id);
                }
            }
            if !report.expired_outbox_ids.is_empty() || report.archived_command_payloads > 0 {
                status.send_replace(RelaySessionStatus::Maintenance { report });
            }
            let expired_inflight = inflight
                .values()
                .any(|sent: &tokio::time::Instant| sent.elapsed() > Duration::from_secs(15));
            if expired_inflight {
                return Err(RemoteError::Timeout.into());
            }
            if inflight.len() < 16 {
                let excluded: BTreeSet<_> = inflight.keys().copied().collect();
                let count = 1;
                let rows = self
                    .blocking(move |endpoint| endpoint.prepare_outbound(now, count, &excluded))
                    .await?;
                for envelope in rows {
                    if cancellation.is_cancelled() {
                        return Ok(());
                    }
                    let id = envelope.header.message_id;
                    let copy = envelope.clone();
                    let permitted = self
                        .blocking(move |endpoint| {
                            let now = UnixMillis::now()?;
                            let peer = match endpoint.peer(copy.header.recipient) {
                                Err(DeviceError::Storage(StorageError::RevokedDevice(_))) => {
                                    return Ok(false);
                                }
                                result => result?,
                            };
                            Ok(endpoint.inner.store.confirm_transport_outbox(
                                &copy.header.message_id.to_string(),
                                &serde_json::to_vec(&copy)?,
                                &serde_json::to_value(peer)?,
                                time(now)?,
                            )?)
                        })
                        .await?;
                    if !permitted {
                        continue;
                    }
                    connection
                        .send(&RelayClientMessage::Send {
                            schema_version: RELAY_SCHEMA_VERSION,
                            envelope,
                        })
                        .await?;
                    inflight.insert(id, tokio::time::Instant::now());
                }
            }
        }
    }

    fn prepare_outbound(
        &self,
        now: UnixMillis,
        count: usize,
        excluded: &BTreeSet<MessageId>,
    ) -> Result<Vec<EncryptedEnvelope>, DeviceError> {
        self.ensure_active()?;
        let mut page = count + excluded.len();
        let rows = loop {
            match self.inner.store.pending_transport_outbox(time(now)?, page) {
                Err(StorageError::ReadBudgetExceeded) if page > 1 => page /= 2,
                result => break result?,
            }
        };
        let mut envelopes = Vec::new();
        for row in rows {
            let id: MessageId = row.id.parse()?;
            if excluded.contains(&id) {
                continue;
            }
            let recipient = row.device_id.parse()?;
            let peer = match self.peer(recipient) {
                Err(DeviceError::Storage(StorageError::RevokedDevice(_))) => continue,
                result => result?,
            };
            let envelope: EncryptedEnvelope = serde_json::from_slice(&row.envelope)?;
            if envelope.header.message_id != id
                || envelope.header.sender != self.public_keys().device_id
                || envelope.header.recipient != recipient
                || envelope.header.expires_at_ms <= now.get()
            {
                return Err(DeviceError::Invalid(
                    "queued ciphertext metadata does not match endpoint",
                ));
            }
            let retry_ms = 1_000_i64 << row.attempts.min(8);
            if let Err(error) = self
                .inner
                .store
                .defer_outbox(&row.id, time(now)?.saturating_add(retry_ms))
            {
                if matches!(
                    &error,
                    StorageError::NotFound { entity: "outbox message", id } if id == &row.id
                ) && !self.inner.store.confirm_transport_outbox(
                    &row.id,
                    &row.envelope,
                    &serde_json::to_value(peer)?,
                    time(now)?,
                )? {
                    continue;
                }
                return Err(error.into());
            }
            envelopes.push(envelope);
            if envelopes.len() == count {
                break;
            }
        }
        Ok(envelopes)
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
