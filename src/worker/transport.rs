use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify, OwnedMutexGuard, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::Agent,
    crypto::{
        CryptoError, DeviceIdentity, DevicePublicKeys, EncryptedEnvelope, ReplayGuard, ReplayRecord,
    },
    protocol::{CommandId, DeviceId, MessageId, UnixMillis},
    relay_protocol::{RELAY_SCHEMA_VERSION, RelayClientMessage, RelayOrigin, RelayServerMessage},
    transport::remote::RelayConnection,
};

use super::{
    ApprovedDelegation, DelegationSnapshot, DelegationState, PreparedDelegation, StagedDelegation,
    WORKER_PURPOSE, WorkerError, WorkerJobBinding, WorkerManifest, WorkerMessage, WorkerPacket,
    WorkerRole, repository::Repository,
};

#[async_trait]
pub trait WorkerOutboundGate: Send + Sync {
    async fn acquire(
        &self,
        manifest: &WorkerManifest,
        peer: &DevicePublicKeys,
        now: UnixMillis,
    ) -> Result<Box<dyn WorkerOutboundPermit>, WorkerError>;
}

pub trait WorkerOutboundPermit: Send + Sync {}

pub fn load_or_create_worker_identity(agent: &Agent) -> Result<Arc<DeviceIdentity>, WorkerError> {
    let state = agent.state_directory().ok_or(WorkerError::LeaseRequired)?;
    Ok(crate::devices::load_or_create_identity(
        &state.join("worker-identity"),
    )?)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerRelayStatus {
    pub connected: bool,
    pub admission_blocked: bool,
    pub retrying: bool,
    pub last_error: Option<crate::transport::remote::RemoteError>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerInboxReceipt {
    pub message_id: MessageId,
    pub duplicate: bool,
}

#[derive(Clone)]
pub struct WorkerEndpoint {
    pub(super) repository: Repository,
    pub(super) identity: Arc<DeviceIdentity>,
    role: WorkerRole,
    origin: RelayOrigin,
    pub(super) authority: Arc<Mutex<()>>,
    pub(super) changed: Arc<Notify>,
    updates: watch::Sender<()>,
    relay_running: Arc<AtomicBool>,
    pub(super) agent: Agent,
}

impl WorkerEndpoint {
    pub fn validate_executor_agent(agent: &Agent) -> Result<(), WorkerError> {
        if agent.state_directory().is_none() {
            return Err(WorkerError::LeaseRequired);
        }
        Repository::validate_executor_store(&agent.store())
    }

    pub fn open(
        agent: Agent,
        identity: Arc<DeviceIdentity>,
        role: WorkerRole,
        origin: RelayOrigin,
    ) -> Result<Self, WorkerError> {
        if agent.state_directory().is_none() {
            return Err(WorkerError::LeaseRequired);
        }
        if role == WorkerRole::Executor {
            Self::validate_executor_agent(&agent)?;
        }
        let repository = Repository::open(agent.store(), &identity.public_keys(), role, &origin)?;
        let (updates, _) = watch::channel(());
        Ok(Self {
            repository,
            identity,
            role,
            origin,
            authority: Arc::new(Mutex::new(())),
            changed: Arc::new(Notify::new()),
            updates,
            relay_running: Arc::new(AtomicBool::new(false)),
            agent,
        })
    }

    pub fn public_keys(&self) -> DevicePublicKeys {
        self.identity.public_keys()
    }

    pub fn role(&self) -> WorkerRole {
        self.role
    }

    pub fn origin(&self) -> &RelayOrigin {
        &self.origin
    }

    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.updates.subscribe()
    }

    pub async fn peers(&self) -> Result<Vec<super::repository::WorkerPeerState>, WorkerError> {
        self.blocking(|repository| repository.peers()).await
    }

    pub async fn pin_peer(
        &self,
        keys: DevicePublicKeys,
        expected_fingerprint: &str,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        keys.validate()?;
        if keys.device_id == self.public_keys().device_id
            || keys.fingerprint() != expected_fingerprint
        {
            return Err(WorkerError::UntrustedPeer);
        }
        let authority = Arc::clone(&self.authority).lock_owned().await;
        self.mutate(move |repository| {
            let _authority = authority;
            repository.pin(&keys, now)
        })
        .await
    }

    pub async fn prepare(
        &self,
        manifest: WorkerManifest,
    ) -> Result<PreparedDelegation, WorkerError> {
        self.validate_manifest_endpoints(&manifest)?;
        if self.role != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let peer = manifest.executor;
        self.blocking(move |repository| repository.peer(peer))
            .await?;
        manifest.prepare()
    }

    pub async fn offer(
        &self,
        approved: ApprovedDelegation,
        gate: &dyn WorkerOutboundGate,
        now: UnixMillis,
    ) -> Result<DelegationSnapshot, WorkerError> {
        approved.validate_for_submission(now)?;
        self.validate_manifest_endpoints(&approved.manifest)?;
        let peer_id = approved.manifest.executor;
        let peer = self
            .blocking(move |repository| repository.peer(peer_id))
            .await?;
        let binding = super::bridge::manifest_binding(&approved.manifest, peer)?;
        self.offer_reserved(binding, gate, now).await
    }

    pub async fn stage_approved(
        &self,
        approved: ApprovedDelegation,
        now: UnixMillis,
    ) -> Result<StagedDelegation, WorkerError> {
        approved.validate_for_submission(now)?;
        self.validate_manifest_endpoints(&approved.manifest)?;
        if self.role != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let expires_at = UnixMillis::try_from(approved.expires_at)?;
        let authority = Arc::clone(&self.authority).lock_owned().await;
        self.mutate(move |repository| {
            let _authority = authority;
            let fresh_now = UnixMillis::now()?;
            if fresh_now < now {
                return Err(WorkerError::Expired);
            }
            approved.validate_for_submission(fresh_now)?;
            repository.stage_manifest(&approved, fresh_now)?;
            Ok(StagedDelegation {
                approved,
                expires_at,
            })
        })
        .await
    }

    pub async fn snapshot(
        &self,
        delegation: uuid::Uuid,
    ) -> Result<Option<DelegationSnapshot>, WorkerError> {
        self.blocking(move |repository| repository.snapshot(delegation))
            .await
    }

    pub async fn jobs(&self) -> Result<Vec<DelegationSnapshot>, WorkerError> {
        self.blocking(|repository| repository.jobs()).await
    }

    pub async fn cancel(
        &self,
        binding: WorkerJobBinding,
        request_id: CommandId,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        if self.role != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let job = self.require_job(&binding).await?;
        self.queue_owned(
            job.peer,
            WorkerMessage::Cancel {
                binding,
                request_id,
            },
            now,
            authority,
            None,
        )
        .await
    }

    pub async fn revoke_peer(
        &self,
        peer: DeviceId,
        request_id: CommandId,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let endpoint = self.clone();
        let (retained, authority) = self
            .blocking(move |_| {
                let retained = super::revocation::retained_revocation(&endpoint, peer, request_id)?;
                Ok((retained, authority))
            })
            .await?;
        if retained {
            return Ok(());
        }
        self.queue_owned(
            peer,
            WorkerMessage::Revoke {
                request_id,
                revoked_at: now,
            },
            now,
            authority,
            None,
        )
        .await
    }

    pub async fn receive_envelope(
        &self,
        envelope: EncryptedEnvelope,
        now: UnixMillis,
    ) -> Result<WorkerInboxReceipt, WorkerError> {
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let endpoint = self.clone();
        self.mutate(move |repository| {
            let _authority = authority;
            let sender = repository.delivery_peer(envelope.header.sender, true)?;
            if envelope.header.recipient != endpoint.public_keys().device_id {
                return Err(WorkerError::UntrustedPeer);
            }
            let mut guard = InboxGuard {
                endpoint: &endpoint,
                repository,
                envelope: &envelope,
                error: None,
            };
            let result = endpoint
                .identity
                .open(&sender, &envelope, now.get(), &mut guard);
            if let Some(error) = guard.error {
                return Err(error);
            }
            match result {
                Ok(_) => Ok(WorkerInboxReceipt {
                    message_id: envelope.header.message_id,
                    duplicate: false,
                }),
                Err(CryptoError::Replay) => Ok(WorkerInboxReceipt {
                    message_id: envelope.header.message_id,
                    duplicate: true,
                }),
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    pub async fn quarantine_result(
        &self,
        binding: WorkerJobBinding,
        now: UnixMillis,
    ) -> Result<super::files::QuarantinedArtifacts, WorkerError> {
        if self.role != WorkerRole::Coordinator {
            return Err(WorkerError::WrongRole);
        }
        let job = self.require_job(&binding).await?;
        let manifest = job
            .manifest
            .ok_or(WorkerError::Conflict("result has no manifest"))?;
        let result = job
            .result
            .ok_or(WorkerError::Conflict("result has not been received"))?;
        let agent = self.agent.clone();
        let quarantine = tokio::task::spawn_blocking(move || {
            super::files::quarantine_result(&agent, &manifest, &result)
        })
        .await
        .map_err(|_| WorkerError::Worker)??;
        let result_digest = quarantine.result_digest.clone();
        self.mutate(move |repository| repository.mark_quarantined(&binding, &result_digest, now))
            .await?;
        Ok(quarantine)
    }

    pub async fn run_relay(
        &self,
        gate: Option<Arc<dyn WorkerOutboundGate>>,
        cancellation: CancellationToken,
        status: watch::Sender<WorkerRelayStatus>,
    ) -> Result<(), WorkerError> {
        if self
            .relay_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(WorkerError::Conflict(
                "worker relay session already running",
            ));
        }
        let _ownership = RelayOwnership(Arc::clone(&self.relay_running));
        let mut backoff = 1;
        let mut last_error = None;
        while !cancellation.is_cancelled() {
            match RelayConnection::connect(
                self.origin.clone(),
                Arc::clone(&self.identity),
                cancellation.clone(),
            )
            .await
            {
                Ok(mut connection) => {
                    backoff = 1;
                    status.send_replace(WorkerRelayStatus {
                        connected: true,
                        admission_blocked: false,
                        retrying: false,
                        last_error: None,
                    });
                    let result = self
                        .connected(&mut connection, gate.as_deref(), &cancellation, &status)
                        .await;
                    let close = connection.close().await;
                    if cancellation.is_cancelled() {
                        status.send_replace(WorkerRelayStatus::default());
                        return Ok(());
                    }
                    if let Err(error) = result {
                        match error {
                            WorkerError::Remote(error) => last_error = Some(error),
                            error => {
                                status.send_replace(WorkerRelayStatus::default());
                                return Err(error);
                            }
                        }
                    } else if let Err(error) = close {
                        last_error = Some(error);
                    }
                }
                Err(_) if cancellation.is_cancelled() => break,
                Err(error) => {
                    last_error = Some(error);
                }
            }
            status.send_replace(WorkerRelayStatus {
                retrying: true,
                last_error,
                ..WorkerRelayStatus::default()
            });
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            }
            backoff = (backoff * 2).min(30);
        }
        status.send_replace(WorkerRelayStatus::default());
        Ok(())
    }

    async fn connected(
        &self,
        connection: &mut RelayConnection,
        gate: Option<&dyn WorkerOutboundGate>,
        cancellation: &CancellationToken,
        status: &watch::Sender<WorkerRelayStatus>,
    ) -> Result<(), WorkerError> {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(()),
                message = connection.receive() => match message? {
                    RelayServerMessage::Delivery { envelope, .. } => {
                        let receipt = self.receive_envelope(envelope, UnixMillis::now()?).await?;
                        connection.send(&RelayClientMessage::Acknowledge { schema_version: RELAY_SCHEMA_VERSION, message_id: receipt.message_id }).await?;
                    }
                    RelayServerMessage::Accepted { message_id, .. } => self.blocking(move |repository| repository.relay_accepted(message_id)).await?,
                    RelayServerMessage::Acknowledged { .. } | RelayServerMessage::Pong { .. } => {}
                    _ => return Err(WorkerError::Conflict("unexpected authenticated relay message")),
                },
                _ = self.changed.notified() => self.flush(connection, gate, status).await?,
                _ = interval.tick() => self.flush(connection, gate, status).await?,
            }
        }
    }

    async fn flush(
        &self,
        connection: &mut RelayConnection,
        gate: Option<&dyn WorkerOutboundGate>,
        status: &watch::Sender<WorkerRelayStatus>,
    ) -> Result<(), WorkerError> {
        let now = UnixMillis::now()?;
        let messages = self
            .blocking(move |repository| repository.outbox(now))
            .await?;
        let mut blocked = false;
        for message in messages {
            let _authority = self.authority.lock().await;
            let revocation = matches!(message.packet.body, WorkerMessage::Revoke { .. });
            let peer_id = message.packet.recipient;
            let peer = match self
                .blocking(move |repository| repository.delivery_peer(peer_id, revocation))
                .await
            {
                Ok(peer) => peer,
                Err(WorkerError::UntrustedPeer) => {
                    let id = message.envelope.header.message_id;
                    self.blocking(move |repository| repository.suppress(id))
                        .await?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let _permit = if let WorkerMessage::Offer { manifest } = &message.packet.body {
                let job = self
                    .require_job(&WorkerJobBinding::from_manifest(manifest)?)
                    .await?;
                if job.cancellation_requested
                    || job.state != DelegationState::Offered
                    || manifest.deadline.get() <= UnixMillis::now()?.get()
                {
                    let id = message.envelope.header.message_id;
                    self.blocking(move |repository| repository.suppress(id))
                        .await?;
                    continue;
                }
                let Some(gate) = gate else {
                    blocked = true;
                    continue;
                };
                match gate.acquire(manifest, &peer, UnixMillis::now()?).await {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        blocked = true;
                        continue;
                    }
                }
            } else {
                None
            };
            if message.envelope.header.expires_at_ms <= UnixMillis::now()?.get() {
                continue;
            }
            connection
                .send(&RelayClientMessage::Send {
                    schema_version: RELAY_SCHEMA_VERSION,
                    envelope: message.envelope,
                })
                .await?;
        }
        status.send_replace(WorkerRelayStatus {
            connected: true,
            admission_blocked: blocked,
            retrying: false,
            last_error: None,
        });
        Ok(())
    }

    pub(super) async fn send_executor(
        &self,
        body: WorkerMessage,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        if self.role != WorkerRole::Executor || body.sender_role() != Some(WorkerRole::Executor) {
            return Err(WorkerError::WrongRole);
        }
        let authority = Arc::clone(&self.authority).lock_owned().await;
        let binding = body
            .binding()?
            .ok_or(WorkerError::Invalid("executor response binding"))?;
        let job = self.require_job(&binding).await?;
        self.queue_owned(job.peer, body, now, authority, None).await
    }

    pub(super) async fn require_job(
        &self,
        binding: &WorkerJobBinding,
    ) -> Result<DelegationSnapshot, WorkerError> {
        let job = self
            .snapshot(binding.delegation_id)
            .await?
            .ok_or(WorkerError::Conflict("unknown delegation"))?;
        if job.binding != *binding {
            return Err(WorkerError::Conflict("delegation binding"));
        }
        Ok(job)
    }

    pub(super) async fn queue_locked(
        &self,
        peer_id: DeviceId,
        body: WorkerMessage,
        now: UnixMillis,
    ) -> Result<(), WorkerError> {
        self.queue_message(peer_id, body, now, None, None).await
    }

    pub(super) async fn queue_owned(
        &self,
        peer_id: DeviceId,
        body: WorkerMessage,
        now: UnixMillis,
        authority: OwnedMutexGuard<()>,
        permit: Option<Box<dyn WorkerOutboundPermit>>,
    ) -> Result<(), WorkerError> {
        self.queue_message(peer_id, body, now, Some(authority), permit)
            .await
    }

    async fn queue_message(
        &self,
        peer_id: DeviceId,
        body: WorkerMessage,
        now: UnixMillis,
        authority: Option<OwnedMutexGuard<()>>,
        permit: Option<Box<dyn WorkerOutboundPermit>>,
    ) -> Result<(), WorkerError> {
        let peer = self
            .blocking(move |repository| repository.peer(peer_id))
            .await?;
        let packet = WorkerPacket {
            version: 1,
            purpose: WORKER_PURPOSE.into(),
            sender_role: self.role,
            sender: self.public_keys().device_id,
            recipient: peer_id,
            body,
        };
        let bytes = packet.to_bytes()?;
        let lifetime = if let WorkerMessage::Offer { manifest } = &packet.body {
            manifest
                .deadline
                .get()
                .checked_sub(now.get())
                .ok_or(WorkerError::Expired)?
                .min(crate::crypto::MAX_ENVELOPE_LIFETIME_MS)
        } else {
            crate::crypto::MAX_ENVELOPE_LIFETIME_MS
        };
        let envelope = self.identity.seal(&peer, &bytes, now.get(), lifetime)?;
        self.mutate(move |repository| {
            let _authority = authority;
            let _permit = permit;
            repository.record_message(&packet, &envelope, false, now)
        })
        .await?;
        Ok(())
    }

    pub(super) fn validate_manifest_endpoints(
        &self,
        manifest: &WorkerManifest,
    ) -> Result<(), WorkerError> {
        manifest.validate()?;
        let local = match self.role {
            WorkerRole::Coordinator => manifest.coordinator,
            WorkerRole::Executor => manifest.executor,
        };
        if local != self.public_keys().device_id || manifest.relay_origin != self.origin {
            return Err(WorkerError::Conflict(
                "worker manifest endpoint or relay origin",
            ));
        }
        Ok(())
    }

    pub(super) async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Repository) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        let resources = (self.repository.clone(), self.agent.clone());
        tokio::task::spawn_blocking(move || {
            let resources = resources;
            operation(&resources.0)
        })
        .await
        .map_err(|_| WorkerError::Worker)?
    }

    pub(super) async fn mutate<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Repository) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        let changed = Arc::clone(&self.changed);
        let updates = self.updates.clone();
        self.blocking(move |repository| {
            let result = operation(repository)?;
            changed.notify_waiters();
            updates.send_replace(());
            Ok(result)
        })
        .await
    }
}

struct RelayOwnership(Arc<AtomicBool>);

impl Drop for RelayOwnership {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct InboxGuard<'a> {
    endpoint: &'a WorkerEndpoint,
    repository: &'a Repository,
    envelope: &'a EncryptedEnvelope,
    error: Option<WorkerError>,
}

impl ReplayGuard for InboxGuard<'_> {
    fn record_if_new(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError> {
        let result = (|| {
            let packet = WorkerPacket::parse(plaintext)?;
            if packet.sender != record.sender
                || packet.recipient != record.recipient
                || packet.sender_role != self.endpoint.role.counterpart()
            {
                return Err(WorkerError::UntrustedPeer);
            }
            if let WorkerMessage::Offer { manifest } = &packet.body {
                self.endpoint.validate_manifest_endpoints(manifest)?;
            }
            self.repository.record_message(
                &packet,
                self.envelope,
                true,
                UnixMillis::try_from(now_ms)?,
            )
        })();
        result.map_err(|error| {
            self.error = Some(error);
            CryptoError::ReplayStorage("worker durable receipt rejected".into())
        })
    }
}
