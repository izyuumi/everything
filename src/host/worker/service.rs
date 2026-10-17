use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::Agent;
use crate::crypto::DevicePublicKeys;
use crate::host::runtime::HostInner;
use crate::ipc::{RelayConnectionState, RelayStatus};
use crate::protocol::{CommandId, ConversationId, TaskId, UnixMillis};
use crate::scheduler::{
    ExecutionPolicy, RemoteDispatchPermit, RemoteTaskBinding, RemoteTaskPhase, RemoteTaskSnapshot,
    SchedulerError,
};
use crate::worker::{
    DelegationSnapshot, DelegationState, PreparedDelegation, WorkerEndpoint, WorkerError,
    WorkerJobBinding, WorkerJobIds, WorkerManifest, WorkerOutboundGate, WorkerOutboundPermit,
    WorkerRelayStatus, capture_inputs,
};

use super::{
    WorkerCoordinatorQuery, WorkerCoordinatorQueryResult, WorkerCoordinatorSettings,
    WorkerCoordinatorStatus, WorkerJobInspection, WorkerJobSummary, WorkerLocalPlan,
    WorkerNativeInspection, WorkerPreparation, WorkerPreparationRequest,
};

const MAX_PREPARATIONS: usize = 4;
const MAX_OPERATIONS: u32 = 4;
const MAX_HELD_BYTES: usize = 2 * 1024 * 1024;
const CAPTURE_RESERVATION: usize = 1536 * 1024;
const TICKET_LIFETIME: Duration = Duration::from_secs(60);

pub(in crate::host) struct WorkerCoordinator {
    host: Weak<HostInner>,
    endpoint: WorkerEndpoint,
    effective: WorkerCoordinatorSettings,
    state: Mutex<State>,
    preparations: Arc<Semaphore>,
    operations: Arc<Semaphore>,
    queries: Arc<Semaphore>,
    scan: Arc<AsyncMutex<()>>,
    settings_match: AtomicBool,
    closed: AtomicBool,
    cleanup_closed: AtomicBool,
    queries_closed: AtomicBool,
    running: AtomicBool,
    stop: CancellationToken,
    stopped: tokio::sync::Notify,
    relay_sender: watch::Sender<WorkerRelayStatus>,
    relay: watch::Receiver<WorkerRelayStatus>,
}

struct State {
    tickets: BTreeMap<Uuid, PreparedTicket>,
    held_bytes: usize,
    failure_code: Option<&'static str>,
}

struct PreparedTicket {
    disclosure: WorkerPreparation,
    prepared: PreparedDelegation,
    deadline: Instant,
    lease: PreparedBudgetLease,
}

pub(in crate::host) struct OwnedPreparedTicket {
    ticket: PreparedTicket,
}

impl OwnedPreparedTicket {
    pub(in crate::host) fn into_parts(
        self,
    ) -> (WorkerPreparation, PreparedDelegation, PreparedBudgetLease) {
        let PreparedTicket {
            disclosure,
            prepared,
            lease,
            ..
        } = self.ticket;
        (disclosure, prepared, lease)
    }
}

pub(in crate::host) struct PreparedBudgetLease {
    owner: Weak<WorkerCoordinator>,
    agent: Option<Agent>,
    permit: Option<OwnedSemaphorePermit>,
    bytes: usize,
}

impl PreparedBudgetLease {
    fn shrink(&mut self, bytes: usize) -> Result<(), WorkerError> {
        if bytes > self.bytes {
            return Err(WorkerError::Capacity("worker preparation payload"));
        }
        let owner = self.owner.upgrade().ok_or(WorkerError::Worker)?;
        let mut state = owner.state()?;
        state.held_bytes -= self.bytes - bytes;
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for PreparedBudgetLease {
    fn drop(&mut self) {
        self.agent.take();
        self.permit.take();
        if let Some(owner) = self.owner.upgrade() {
            let mut state = match owner.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    tracing::error!("worker preparation accounting requires cleanup");
                    poisoned.into_inner()
                }
            };
            state.held_bytes = state.held_bytes.saturating_sub(self.bytes);
        }
    }
}

impl WorkerCoordinator {
    pub(in crate::host) fn new(
        host: Weak<HostInner>,
        endpoint: WorkerEndpoint,
        effective: WorkerCoordinatorSettings,
    ) -> Arc<Self> {
        let (relay_sender, relay) = watch::channel(WorkerRelayStatus::default());
        Arc::new(Self {
            host,
            endpoint,
            effective,
            state: Mutex::new(State {
                tickets: BTreeMap::new(),
                held_bytes: 0,
                failure_code: None,
            }),
            preparations: Arc::new(Semaphore::new(MAX_PREPARATIONS)),
            operations: Arc::new(Semaphore::new(MAX_OPERATIONS as usize)),
            queries: Arc::new(Semaphore::new(MAX_OPERATIONS as usize)),
            scan: Arc::new(AsyncMutex::new(())),
            settings_match: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            cleanup_closed: AtomicBool::new(false),
            queries_closed: AtomicBool::new(false),
            running: AtomicBool::new(false),
            stop: CancellationToken::new(),
            stopped: tokio::sync::Notify::new(),
            relay_sender,
            relay,
        })
    }

    pub(in crate::host) fn endpoint(&self) -> &WorkerEndpoint {
        &self.endpoint
    }

    pub(in crate::host) fn gate(self: &Arc<Self>) -> Arc<dyn WorkerOutboundGate> {
        Arc::new(HostGate {
            coordinator: Arc::downgrade(self),
        })
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, WorkerError> {
        self.state.lock().map_err(|_| WorkerError::Worker)
    }

    fn host(&self) -> Result<Arc<HostInner>, WorkerError> {
        self.host.upgrade().ok_or(WorkerError::Worker)
    }

    pub(in crate::host) fn ensure_enabled(&self) -> Result<(), WorkerError> {
        let host = self.host()?;
        if self.closed.load(Ordering::Acquire) || host.draining.load(Ordering::Acquire) {
            return Err(WorkerError::Cancelled);
        }
        if !self.settings_match.load(Ordering::Acquire)
            || self.endpoint.origin() != &self.effective.relay_origin
        {
            return Err(WorkerError::Conflict("worker coordinator settings changed"));
        }
        Ok(())
    }

    pub(in crate::host) async fn begin_operation(
        &self,
    ) -> Result<OwnedSemaphorePermit, WorkerError> {
        self.ensure_enabled()?;
        let permit = self
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkerError::Capacity("worker coordinator operations"))?;
        self.ensure_enabled()?;
        Ok(permit)
    }

    pub(in crate::host) async fn begin_cleanup_operation(
        &self,
    ) -> Result<OwnedSemaphorePermit, WorkerError> {
        if self.cleanup_closed.load(Ordering::Acquire) {
            return Err(WorkerError::Cancelled);
        }
        let permit = self
            .operations
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkerError::Capacity("worker coordinator operations"))?;
        if self.cleanup_closed.load(Ordering::Acquire) {
            return Err(WorkerError::Cancelled);
        }
        Ok(permit)
    }

    pub(in crate::host) fn settings_changed(&self, settings: Option<&WorkerCoordinatorSettings>) {
        let matches = settings == Some(&self.effective);
        self.settings_match.store(matches, Ordering::Release);
        if !matches {
            self.close();
        }
    }

    fn prune(&self) -> Result<(), WorkerError> {
        let now = UnixMillis::now()?;
        let expired = {
            let mut state = self.state()?;
            let ids: Vec<_> = state
                .tickets
                .iter()
                .filter(|(_, ticket)| {
                    Instant::now() >= ticket.deadline
                        || now < ticket.disclosure.prepared_at
                        || now >= ticket.disclosure.expires_at
                })
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| state.tickets.remove(&id))
                .collect::<Vec<_>>()
        };
        drop(expired);
        Ok(())
    }

    fn preparation_lease(self: &Arc<Self>) -> Result<PreparedBudgetLease, WorkerError> {
        self.ensure_enabled()?;
        self.prune()?;
        let host = self.host()?;
        let permit = self
            .preparations
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkerError::Capacity("worker preparations"))?;
        {
            let mut state = self.state()?;
            if state.held_bytes.saturating_add(CAPTURE_RESERVATION) > MAX_HELD_BYTES {
                return Err(WorkerError::Capacity("worker preparation bytes"));
            }
            state.held_bytes += CAPTURE_RESERVATION;
        }
        Ok(PreparedBudgetLease {
            owner: Arc::downgrade(self),
            agent: Some(host.agent.clone()),
            permit: Some(permit),
            bytes: CAPTURE_RESERVATION,
        })
    }

    pub(in crate::host) async fn prepare(
        self: &Arc<Self>,
        request: WorkerPreparationRequest,
        cancellation: CancellationToken,
    ) -> Result<WorkerPreparation, WorkerError> {
        request.validate()?;
        let lease = self.preparation_lease()?;
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator
                .prepare_owned(request, cancellation, lease)
                .await
        })
        .await
        .map_err(|_| WorkerError::Worker)?
    }

    async fn prepare_owned(
        self: Arc<Self>,
        request: WorkerPreparationRequest,
        cancellation: CancellationToken,
        mut lease: PreparedBudgetLease,
    ) -> Result<WorkerPreparation, WorkerError> {
        self.ensure_enabled()?;
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let peer = self
            .endpoint
            .peers()
            .await?
            .into_iter()
            .find(|peer| peer.keys.device_id == request.executor && peer.revoked_at.is_none())
            .ok_or(WorkerError::UntrustedPeer)?;
        let host = self.host()?;
        let coordinator_id = self.endpoint.public_keys().device_id;
        let origin = self.effective.relay_origin.clone();
        let (manifest, local) = tokio::task::spawn_blocking(move || {
            let _agent = host.agent.clone();
            let execution = ExecutionPolicy {
                workspace_root: request.workspace_root.clone(),
                mode: request.mode,
                tools_enabled: !request.allowed_tools.is_empty(),
                allowed_tools: Some(request.allowed_tools.clone()),
                budget: request.budget.clone(),
                limits: request.limits.clone(),
            }
            .capture(&host.registry)?;
            if execution.workspace_root != request.workspace_root
                || execution.limits != request.limits
                || execution.allowed_tools.as_ref() != Some(&request.allowed_tools)
            {
                return Err(WorkerError::Conflict(
                    "worker policy capture changed request",
                ));
            }
            let inputs =
                capture_inputs(&host.agent, &request.workspace_root, &request.input_paths)?;
            let created_at = UnixMillis::now()?;
            let manifest = WorkerManifest {
                version: 1,
                delegation_id: Uuid::new_v4(),
                attempt_id: Uuid::new_v4(),
                coordinator: coordinator_id,
                executor: request.executor,
                relay_origin: origin,
                created_at,
                deadline: request.deadline,
                ids: WorkerJobIds {
                    task_id: TaskId::new(),
                    conversation_id: ConversationId::new(),
                    create_command_id: CommandId::new(),
                    input_command_id: CommandId::new(),
                },
                parent: None,
                objective: request.objective,
                constraints: request.constraints,
                context: request.context,
                inputs,
                outputs: request.outputs,
                allowed_tools: request.allowed_tools,
                mode: request.mode,
                model: request.model,
                budget: request.budget,
                limits: request.limits,
                hard_limits: None,
            };
            manifest.validate()?;
            let local = WorkerLocalPlan {
                conversation_id: ConversationId::new(),
                create_command_id: CommandId::new(),
                input_command_id: CommandId::new(),
                task_id: TaskId::new(),
                title: format!("Delegated {}", manifest.delegation_id),
                execution,
            };
            local.validate()?;
            let store = host.agent.store();
            if store
                .conversation(&local.conversation_id.to_string())?
                .is_some()
                || store.task(&local.task_id.to_string())?.is_some()
                || store
                    .command_request(&local.create_command_id.to_string())?
                    .is_some()
                || store
                    .command_request(&local.input_command_id.to_string())?
                    .is_some()
            {
                return Err(WorkerError::Conflict(
                    "fresh worker local identity already exists",
                ));
            }
            Ok::<_, WorkerError>((manifest, local))
        })
        .await
        .map_err(|_| WorkerError::Worker)??;
        self.ensure_enabled()?;
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let prepared = self.endpoint.prepare(manifest).await?;
        let prepared_at = UnixMillis::now()?;
        let remaining = prepared
            .manifest()
            .deadline
            .get()
            .checked_sub(prepared_at.get())
            .ok_or(WorkerError::Expired)?
            .min(TICKET_LIFETIME.as_millis() as u64);
        if remaining == 0 {
            return Err(WorkerError::Expired);
        }
        let deadline = Instant::now() + Duration::from_millis(remaining);
        let disclosure = WorkerPreparation {
            ticket: Uuid::new_v4(),
            prepared_at,
            expires_at: UnixMillis::try_from(prepared_at.get() + remaining)?,
            manifest_digest: prepared.fingerprint().to_owned(),
            executor_keys: peer.keys.clone(),
            executor_fingerprint: peer.keys.fingerprint(),
            manifest: prepared.manifest().clone(),
            local,
        };
        disclosure.validate()?;
        let bytes = encoded_size(&disclosure)? + encoded_size(prepared.manifest())?;
        lease.shrink(bytes)?;
        self.ensure_enabled()?;
        if cancellation.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let response = disclosure.clone();
        let mut state = self.state()?;
        if self.closed.load(Ordering::Acquire) {
            return Err(WorkerError::Cancelled);
        }
        if state.tickets.contains_key(&disclosure.ticket) {
            return Err(WorkerError::Conflict("worker preparation identity reused"));
        }
        state.tickets.insert(
            disclosure.ticket,
            PreparedTicket {
                disclosure,
                prepared,
                deadline,
                lease,
            },
        );
        Ok(response)
    }

    pub(in crate::host) async fn take_prepared(
        &self,
        ticket: Uuid,
        digest: &str,
        fingerprint: &str,
        request_id: CommandId,
    ) -> Result<OwnedPreparedTicket, WorkerError> {
        self.ensure_enabled()?;
        self.prune()?;
        let prepared = self
            .state()?
            .tickets
            .remove(&ticket)
            .ok_or(WorkerError::ApprovalRequired)?;
        let now = UnixMillis::now()?;
        if Instant::now() >= prepared.deadline
            || now < prepared.disclosure.prepared_at
            || now >= prepared.disclosure.expires_at
        {
            return Err(WorkerError::Expired);
        }
        if prepared.disclosure.manifest_digest != digest
            || prepared.disclosure.executor_fingerprint != fingerprint
            || prepared.disclosure.local.create_command_id != request_id
        {
            return Err(WorkerError::Conflict("worker preparation binding changed"));
        }
        Ok(OwnedPreparedTicket { ticket: prepared })
    }

    pub(in crate::host) async fn discard(&self, ticket: Uuid) -> Result<bool, WorkerError> {
        let removed = self.state()?.tickets.remove(&ticket);
        let found = removed.is_some();
        drop(removed);
        Ok(found)
    }

    pub(in crate::host) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.stop.cancel();
        let tickets = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    tracing::error!("worker preparations closed after accounting failure");
                    poisoned.into_inner()
                }
            };
            std::mem::take(&mut state.tickets)
        };
        drop(tickets);
    }

    pub(in crate::host) async fn wait_operations(&self) -> Result<(), WorkerError> {
        if !self.closed.load(Ordering::Acquire) {
            return Err(WorkerError::Invalid("close worker intake before waiting"));
        }
        self.cleanup_closed.store(true, Ordering::Release);
        self.queries_closed.store(true, Ordering::Release);
        let queries = self
            .queries
            .clone()
            .acquire_many_owned(MAX_OPERATIONS)
            .await
            .map_err(|_| WorkerError::Worker)?;
        let operations = self
            .operations
            .clone()
            .acquire_many_owned(MAX_OPERATIONS)
            .await
            .map_err(|_| WorkerError::Worker)?;
        let preparations = self
            .preparations
            .clone()
            .acquire_many_owned(MAX_PREPARATIONS as u32)
            .await
            .map_err(|_| WorkerError::Worker)?;
        drop((operations, preparations, queries));
        Ok(())
    }

    pub(in crate::host) async fn shutdown(&self) -> Result<(), WorkerError> {
        self.close();
        loop {
            let stopped = self.stopped.notified();
            tokio::pin!(stopped);
            stopped.as_mut().enable();
            if !self.running.load(Ordering::Acquire) {
                break;
            }
            stopped.await;
        }
        self.wait_operations().await
    }

    pub(in crate::host) async fn query(
        self: &Arc<Self>,
        query: WorkerCoordinatorQuery,
    ) -> Result<WorkerCoordinatorQueryResult, WorkerError> {
        query.validate()?;
        self.prune()?;
        if self.queries_closed.load(Ordering::Acquire) {
            return Err(WorkerError::Cancelled);
        }
        let permit = self
            .queries
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkerError::Capacity("worker coordinator queries"))?;
        if self.queries_closed.load(Ordering::Acquire) {
            return Err(WorkerError::Cancelled);
        }
        let coordinator = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            coordinator.query_owned(query).await
        })
        .await
        .map_err(|_| WorkerError::Worker)?
    }

    async fn query_owned(
        &self,
        query: WorkerCoordinatorQuery,
    ) -> Result<WorkerCoordinatorQueryResult, WorkerError> {
        let _scan = self.scan.lock().await;
        let host = self.host()?;
        let result = match &query {
            WorkerCoordinatorQuery::Identity => {
                let keys = self.endpoint.public_keys();
                WorkerCoordinatorQueryResult::Identity {
                    fingerprint: keys.fingerprint(),
                    keys,
                }
            }
            WorkerCoordinatorQuery::Peers => WorkerCoordinatorQueryResult::Peers {
                peers: self.endpoint.peers().await?,
            },
            WorkerCoordinatorQuery::Task { task_id } => WorkerCoordinatorQueryResult::Task {
                task: native_inspection(host.scheduler.remote_status(*task_id).await?)?,
            },
            WorkerCoordinatorQuery::Delegation { delegation_id } => {
                let job = match self.endpoint.snapshot(*delegation_id).await? {
                    Some(job) => Some(self.inspect_job(job).await?),
                    None => None,
                };
                WorkerCoordinatorQueryResult::Delegation { job }
            }
            WorkerCoordinatorQuery::Jobs {
                after_delegation_id,
                limit,
            } => {
                let mut jobs = self.endpoint.jobs().await?;
                jobs.sort_by_key(|job| job.binding.delegation_id);
                let mut candidates = jobs.into_iter().filter(|job| {
                    after_delegation_id.is_none_or(|after| job.binding.delegation_id > after)
                });
                let mut page = Vec::new();
                for job in candidates.by_ref().take(*limit) {
                    let native = self.native_job(&job).await?;
                    page.push(WorkerJobSummary {
                        binding: job.binding,
                        peer: job.peer,
                        state: job.state,
                        cancellation_requested: job.cancellation_requested,
                        cancellation_acknowledged: job.cancellation_acknowledged,
                        local_task_id: native.as_ref().map(|native| native.task.task.id),
                        local_phase: native.as_ref().map(|native| native.phase),
                    });
                }
                let next_after = if candidates.next().is_some() {
                    page.last().map(|job| job.binding.delegation_id)
                } else {
                    None
                };
                WorkerCoordinatorQueryResult::Jobs {
                    jobs: page,
                    next_after,
                }
            }
            WorkerCoordinatorQuery::Status => {
                let jobs = self.endpoint.jobs().await?;
                let mut unresolved = 0;
                for job in &jobs {
                    if self.native_job(job).await?.is_some_and(|native| {
                        !matches!(
                            native.phase,
                            RemoteTaskPhase::Settled | RemoteTaskPhase::Released
                        )
                    }) {
                        unresolved += 1;
                    }
                }
                let relay = *self.relay.borrow();
                let draining =
                    host.draining.load(Ordering::Acquire) || self.closed.load(Ordering::Acquire);
                let enabled = self.settings_match.load(Ordering::Acquire);
                let connection = if relay.connected {
                    RelayConnectionState::Connected
                } else if relay.retrying {
                    RelayConnectionState::Retrying
                } else if self.running.load(Ordering::Acquire) {
                    RelayConnectionState::Connecting
                } else {
                    RelayConnectionState::Stopped
                };
                WorkerCoordinatorQueryResult::Status {
                    status: WorkerCoordinatorStatus {
                        enabled,
                        accepting: enabled && !draining,
                        draining,
                        relay: RelayStatus {
                            origin: Some(self.effective.relay_origin.clone()),
                            connection,
                        },
                        admission_blocked: relay.admission_blocked,
                        preparations: MAX_PREPARATIONS - self.preparations.available_permits(),
                        in_flight: MAX_OPERATIONS as usize - self.operations.available_permits(),
                        jobs: Some(jobs.len()),
                        unresolved_remote_tasks: Some(unresolved),
                        failure_code: self.state()?.failure_code.map(str::to_owned),
                    },
                }
            }
        };
        result.validate_for(&query)?;
        Ok(result)
    }

    async fn native_job(
        &self,
        job: &DelegationSnapshot,
    ) -> Result<Option<RemoteTaskSnapshot>, WorkerError> {
        let manifest = job
            .manifest
            .as_ref()
            .ok_or(WorkerError::Conflict("worker job manifest unavailable"))?;
        if job.binding != WorkerJobBinding::from_manifest(manifest)?
            || job.peer != manifest.executor
            || manifest.coordinator != self.endpoint.public_keys().device_id
            || manifest.relay_origin != self.effective.relay_origin
            || &manifest.relay_origin != self.endpoint.origin()
        {
            return Err(WorkerError::Conflict("worker job manifest mapping changed"));
        }
        let peer = self
            .endpoint
            .peers()
            .await?
            .into_iter()
            .find(|peer| peer.keys.device_id == job.peer)
            .ok_or(WorkerError::UntrustedPeer)?;
        let binding = manifest_binding(manifest, &peer.keys)?;
        match self.host()?.scheduler.remote_by_binding(binding).await {
            Ok(native) => Ok(Some(native)),
            Err(SchedulerError::RemoteBindingNotFound(id)) if id == job.binding.delegation_id => {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn inspect_job(
        &self,
        job: DelegationSnapshot,
    ) -> Result<WorkerJobInspection, WorkerError> {
        let native = self
            .native_job(&job)
            .await?
            .map(native_inspection)
            .transpose()?;
        let inspection = WorkerJobInspection {
            binding: job.binding,
            peer: job.peer,
            state: job.state,
            cancellation_requested: job.cancellation_requested,
            cancellation_acknowledged: job.cancellation_acknowledged,
            rejection: job.rejection,
            result_digest: job.result_digest,
            quarantined: job.quarantined,
            receipt_id: job.receipt_id.map(|id| id.parse()).transpose()?,
            native,
        };
        inspection.validate()?;
        Ok(inspection)
    }

    fn failure(&self, code: &'static str) {
        if let Ok(mut state) = self.state() {
            state.failure_code = Some(code);
        } else {
            tracing::error!("worker coordinator diagnostic state unavailable");
        }
        tracing::warn!(code, "worker coordinator operation remains unavailable");
    }

    pub(in crate::host) async fn run(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(), WorkerError> {
        self.ensure_enabled()?;
        if self.running.swap(true, Ordering::AcqRel) {
            return Err(WorkerError::Conflict("worker coordinator already running"));
        }
        let coordinator = self.clone();
        tokio::spawn(async move {
            let _running = RunningGuard(coordinator.clone());
            coordinator.run_owned(cancellation).await
        })
        .await
        .map_err(|_| WorkerError::Worker)?
    }

    async fn run_owned(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(), WorkerError> {
        let stop = self.stop.child_token();
        let mut tasks = JoinSet::new();
        let relay = self.endpoint.clone();
        let gate = self.gate();
        let relay_stop = stop.clone();
        let updates = self.relay_sender.clone();
        tasks.spawn(async move { relay.run_relay(Some(gate), relay_stop, updates).await });
        let reconciler = self.clone();
        let reconcile_stop = stop.clone();
        tasks.spawn(async move { reconciler.reconcile(reconcile_stop).await });
        let first = tokio::select! { _ = cancellation.cancelled() => None, _ = stop.cancelled() => None, outcome = tasks.join_next() => outcome };
        let requested_stop = cancellation.is_cancelled() || stop.is_cancelled();
        stop.cancel();
        let mut failure = match first {
            Some(Ok(Err(error))) => Some(error),
            Some(Err(_)) => Some(WorkerError::Worker),
            Some(Ok(Ok(()))) if !requested_stop => Some(WorkerError::Worker),
            _ => None,
        };
        while let Some(outcome) = tasks.join_next().await {
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) if failure.is_none() => failure = Some(error),
                Err(_) if failure.is_none() => failure = Some(WorkerError::Worker),
                _ => {}
            }
        }
        if let Some(error) = failure {
            self.failure("worker_coordinator_failed");
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn reconcile(
        self: Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<(), WorkerError> {
        let mut worker = self.endpoint.subscribe();
        let mut scheduler = self.host()?.scheduler.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut offered = BTreeSet::new();
        let mut ready_at = tokio::time::Instant::now();
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = cancellation.cancelled() => return Ok(()), _ = tokio::time::sleep_until(ready_at) => {} }
            ready_at = tokio::time::Instant::now() + Duration::from_secs(1);
            let jobs = {
                let _scan = self.scan.lock().await;
                self.endpoint.jobs().await?
            };
            for job in jobs {
                if cancellation.is_cancelled() {
                    return Ok(());
                }
                if let Err(error) = self.reconcile_job(job, &cancellation, &mut offered).await {
                    self.failure(match error {
                        WorkerError::UntrustedPeer => "worker_peer_untrusted",
                        WorkerError::Expired => "worker_deadline_expired",
                        WorkerError::Cancelled => "worker_coordinator_draining",
                        WorkerError::Capacity(_) => "worker_admission_busy",
                        _ => "worker_reconciliation_failed",
                    });
                }
            }
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                _ = tick.tick() => {},
                result = worker.changed() => result.map_err(|_| WorkerError::Worker)?,
                result = scheduler.changed() => result.map_err(|_| WorkerError::Worker)?,
            }
            worker.borrow_and_update();
            scheduler.borrow_and_update();
        }
    }

    async fn reconcile_job(
        self: &Arc<Self>,
        job: DelegationSnapshot,
        cancellation: &CancellationToken,
        offered: &mut BTreeSet<Uuid>,
    ) -> Result<(), WorkerError> {
        let Some(mut native) = self.native_job(&job).await? else {
            return Ok(());
        };
        if native.phase == RemoteTaskPhase::Released {
            return Ok(());
        }
        let revoked = self
            .endpoint
            .peers()
            .await?
            .into_iter()
            .find(|peer| peer.keys.device_id == job.peer)
            .ok_or(WorkerError::UntrustedPeer)?
            .revoked_at
            .is_some();
        if native.task.cancel_requested
            && !job.cancellation_requested
            && !revoked
            && !job.state.is_terminal()
            && job.result.is_none()
            && job.rejection.is_none()
            && native.result.is_none()
        {
            let _operation = self.begin_cleanup_operation().await?;
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            self.endpoint
                .cancel(job.binding.clone(), CommandId::new(), UnixMillis::now()?)
                .await?;
            return Ok(());
        }
        if (job.cancellation_requested || revoked)
            && !native.task.cancel_requested
            && !native.task.task.status.is_terminal()
        {
            let _operation = self.begin_cleanup_operation().await?;
            let host = self.host()?;
            let _mutation = host.mutation.clone().lock_owned().await;
            let current = host
                .scheduler
                .remote_by_binding(native.binding.clone())
                .await?;
            if !current.task.cancel_requested && !current.task.task.status.is_terminal() {
                host.scheduler.cancel(current.task.task.id).await?;
                host.receipt_changed();
            }
            native = host.scheduler.remote_by_binding(native.binding).await?;
        }
        if native.result.is_some() {
            if job.state == DelegationState::Acknowledged || job.rejection.is_some() {
                return Ok(());
            }
            let _operation = self.begin_operation().await?;
            let receipt = self
                .host()?
                .scheduler
                .remote_result_receipt(native.task.task.id)
                .await?;
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            self.ensure_enabled()?;
            self.endpoint
                .acknowledge_native_result(receipt, UnixMillis::now()?)
                .await?;
            return Ok(());
        }
        if job.result.is_some() || job.rejection.is_some() {
            let _operation = self.begin_operation().await?;
            let host = self.host()?;
            let task = native.task.task.id;
            let mut outcome = self.endpoint.verify_native_outcome(native).await?;
            let mutation = host.mutation.clone().lock_owned().await;
            self.ensure_enabled()?;
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            outcome.retain_host_authority(Box::new(mutation))?;
            let receipt = host.scheduler.admit_remote_result(task, outcome).await?;
            host.receipt_changed();
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            self.ensure_enabled()?;
            self.endpoint
                .acknowledge_native_result(receipt, UnixMillis::now()?)
                .await?;
        } else if job.state == DelegationState::Offered
            && !job.cancellation_requested
            && native.phase == RemoteTaskPhase::Reserved
            && !offered.contains(&job.binding.delegation_id)
        {
            let _operation = self.begin_operation().await?;
            if cancellation.is_cancelled() {
                return Err(WorkerError::Cancelled);
            }
            self.endpoint
                .offer_reserved(native.binding, self.gate().as_ref(), UnixMillis::now()?)
                .await?;
            offered.insert(job.binding.delegation_id);
        }
        Ok(())
    }
}

pub(in crate::host) fn native_inspection(
    native: RemoteTaskSnapshot,
) -> Result<WorkerNativeInspection, WorkerError> {
    let inspection = WorkerNativeInspection {
        task_id: native.task.task.id,
        conversation_id: native.task.task.conversation_id,
        command_id: native.task.active_command_id,
        status: native.task.task.status,
        phase: native.phase,
        charge: native.charge,
        admitted_at: native.admitted_at,
        last_dispatch_at: native.last_dispatch_at,
        result: native.result,
        release: native.release,
    };
    inspection.validate()?;
    Ok(inspection)
}

struct RunningGuard(Arc<WorkerCoordinator>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.close();
        self.0.running.store(false, Ordering::Release);
        self.0.stopped.notify_waiters();
    }
}

fn manifest_binding(
    manifest: &WorkerManifest,
    peer: &DevicePublicKeys,
) -> Result<RemoteTaskBinding, WorkerError> {
    if peer.device_id != manifest.executor {
        return Err(WorkerError::UntrustedPeer);
    }
    let binding = RemoteTaskBinding {
        delegation_id: manifest.delegation_id,
        attempt_id: manifest.attempt_id,
        manifest_digest: manifest.fingerprint()?,
        coordinator: manifest.coordinator,
        executor: peer.clone(),
        relay_origin: manifest.relay_origin.clone(),
        worker_task_id: manifest.ids.task_id,
        worker_conversation_id: manifest.ids.conversation_id,
        worker_create_command_id: manifest.ids.create_command_id,
        worker_input_command_id: manifest.ids.input_command_id,
        created_at: manifest.created_at,
        deadline: manifest.deadline,
    };
    binding.validate()?;
    Ok(binding)
}

struct HostGate {
    coordinator: Weak<WorkerCoordinator>,
}

struct HostPermit {
    _native: RemoteDispatchPermit,
    _mutation: tokio::sync::OwnedMutexGuard<()>,
    _host: Arc<HostInner>,
    _operation: OwnedSemaphorePermit,
}

impl WorkerOutboundPermit for HostPermit {}

#[async_trait]
impl WorkerOutboundGate for HostGate {
    async fn acquire(
        &self,
        manifest: &WorkerManifest,
        peer: &DevicePublicKeys,
        _now: UnixMillis,
    ) -> Result<Box<dyn WorkerOutboundPermit>, WorkerError> {
        let coordinator = self.coordinator.upgrade().ok_or(WorkerError::Worker)?;
        let manifest = manifest.clone();
        let peer = peer.clone();
        tokio::spawn(async move { acquire_gate(coordinator, manifest, peer).await })
            .await
            .map_err(|_| WorkerError::Worker)?
    }
}

async fn acquire_gate(
    coordinator: Arc<WorkerCoordinator>,
    manifest: WorkerManifest,
    peer: DevicePublicKeys,
) -> Result<Box<dyn WorkerOutboundPermit>, WorkerError> {
    let operation = coordinator.begin_operation().await?;
    let host = coordinator.host()?;
    let mutation = host.mutation.clone().lock_owned().await;
    coordinator.ensure_enabled()?;
    if manifest.coordinator != coordinator.endpoint.public_keys().device_id
        || manifest.relay_origin != coordinator.effective.relay_origin
    {
        return Err(WorkerError::Conflict("worker gate endpoint"));
    }
    let binding = manifest_binding(&manifest, &peer)?;
    let native = host.scheduler.remote_by_binding(binding.clone()).await?;
    if native.configuration != manifest.model.configuration
        || native.execution.mode != manifest.mode
        || native.execution.tools_enabled == manifest.allowed_tools.is_empty()
        || native.execution.allowed_tools.as_ref() != Some(&manifest.allowed_tools)
        || native.execution.limits != manifest.limits
        || serde_json::to_vec(&native.execution.budget)? != serde_json::to_vec(&manifest.budget)?
    {
        return Err(WorkerError::Conflict("worker gate native policy"));
    }
    let execution = native.execution.clone();
    let registry = host.registry.clone();
    let (host, mutation) = tokio::task::spawn_blocking(move || {
        if serde_json::to_vec(&execution.capture(&registry)?)? != serde_json::to_vec(&execution)? {
            return Err(WorkerError::Conflict("worker gate current authority"));
        }
        Ok::<_, WorkerError>((host, mutation))
    })
    .await
    .map_err(|_| WorkerError::Worker)??;
    coordinator.ensure_enabled()?;
    let permit = host
        .scheduler
        .remote_dispatch_permit(native.task.task.id, binding)
        .await?;
    Ok(Box::new(HostPermit {
        _native: permit,
        _mutation: mutation,
        _host: host,
        _operation: operation,
    }))
}

fn encoded_size(value: &impl Serialize) -> Result<usize, WorkerError> {
    struct Counter {
        bytes: usize,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("worker payload size overflow"))?;
            if self.bytes > MAX_HELD_BYTES {
                return Err(io::Error::other("worker payload bound"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0 };
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.bytes)
}
