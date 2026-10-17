use super::*;
use crate::storage::{TransportOutboundCandidate, TransportOutboundRouting};
use tokio::task::{Id, JoinError, JoinHandle};

const MAX_ORDINARY_INFLIGHT: usize = 16;

pub(super) struct OrdinarySendWindow {
    endpoint: DeviceEndpoint,
    identity: Arc<()>,
    entries: BTreeMap<MessageId, OrdinaryEntry>,
    finished: bool,
    completion: Option<Result<(), DeviceError>>,
    failure: Option<OrdinaryFailureBundle>,
}

struct OrdinarySlot {
    window: Arc<()>,
    identity: Arc<()>,
}

struct OrdinaryInput {
    endpoint: DeviceEndpoint,
    candidate: TransportOutboundCandidate,
    selected_at: UnixMillis,
    cancellation: CancellationToken,
    slot: OrdinarySlot,
}

struct OrdinaryPrepared {
    envelope: EncryptedEnvelope,
}

struct OrdinaryNotSent;

enum OrdinaryPreparation {
    Prepared(Box<OrdinaryPrepared>),
    NotSent(OrdinaryNotSent),
}

struct OrdinaryWorkerOutput {
    result: Result<OrdinaryPreparation, DeviceError>,
    slot_identity: Arc<()>,
}

enum OrdinaryEntry {
    Registered {
        input: Arc<OrdinaryInput>,
    },
    Preparing {
        task: Id,
        handle: JoinHandle<OrdinaryWorkerOutput>,
        input: Arc<OrdinaryInput>,
    },
    Observed {
        task: Id,
        joined: Result<OrdinaryWorkerOutput, JoinError>,
        input: Arc<OrdinaryInput>,
    },
    Prepared {
        prepared: Box<OrdinaryPrepared>,
        input: Arc<OrdinaryInput>,
    },
    SocketOwned {
        message: Box<RelayClientMessage>,
        outcome: Option<Result<(), RemoteError>>,
        input: Arc<OrdinaryInput>,
    },
    AwaitAccepted {
        sent_at: tokio::time::Instant,
        slot: OrdinarySlot,
    },
    Retained {
        input: Arc<OrdinaryInput>,
    },
}

enum OrdinaryIssue {
    Bookkeeping { _reason: &'static str },
    Abandoned,
}

struct OrdinaryFailureBundle {
    _primary: Option<DeviceError>,
    _cleanup: OrdinaryIssue,
}

struct AbandonedOrdinaryWindow {
    _completion: Option<Result<(), DeviceError>>,
    _entries: BTreeMap<MessageId, OrdinaryEntry>,
    _failure: OrdinaryFailureBundle,
    _identity: Arc<()>,
    _endpoint: DeviceEndpoint,
}

pub(super) struct OrdinaryAccepted {
    message_id: MessageId,
}

impl OrdinaryAccepted {
    pub(super) fn into_message_id(self) -> MessageId {
        self.message_id
    }
}

impl OrdinarySendWindow {
    pub(super) fn new(endpoint: DeviceEndpoint) -> Result<Self, DeviceError> {
        endpoint.check_outbound_health()?;
        Ok(Self {
            endpoint,
            identity: Arc::new(()),
            entries: BTreeMap::new(),
            finished: false,
            completion: None,
            failure: None,
        })
    }

    pub(super) fn excluded(&self) -> BTreeSet<MessageId> {
        self.entries.keys().copied().collect()
    }

    pub(super) fn has_capacity(&self) -> bool {
        !self.finished && self.failure.is_none() && self.entries.len() < MAX_ORDINARY_INFLIGHT
    }

    pub(super) async fn prepare_and_send(
        &mut self,
        candidate: TransportOutboundCandidate,
        selected_at: UnixMillis,
        cancellation: CancellationToken,
        connection: &mut RelayConnection,
    ) -> Result<(), DeviceError> {
        self.endpoint.check_outbound_health()?;
        if !self.has_capacity()
            || !matches!(candidate.routing, TransportOutboundRouting::Ordinary)
            || self.entries.contains_key(&candidate.message_id)
        {
            return self.fault("ordinary preparation admission mismatch");
        }
        let id = candidate.message_id;
        let input = Arc::new(OrdinaryInput {
            endpoint: self.endpoint.clone(),
            candidate,
            selected_at,
            cancellation,
            slot: OrdinarySlot {
                window: Arc::clone(&self.identity),
                identity: Arc::new(()),
            },
        });
        self.entries.insert(id, OrdinaryEntry::Registered { input });
        let input = match self.entries.get(&id) {
            Some(OrdinaryEntry::Registered { input }) => Arc::clone(input),
            _ => return self.fault("ordinary registration was not retained"),
        };
        let worker = Arc::clone(&input);
        let handle = tokio::task::spawn_blocking(move || {
            let result = prepare_ordinary(&worker);
            let slot_identity = Arc::clone(&worker.slot.identity);
            drop(worker);
            OrdinaryWorkerOutput {
                result,
                slot_identity,
            }
        });
        let task = handle.id();
        self.entries.insert(
            id,
            OrdinaryEntry::Preparing {
                task,
                handle,
                input,
            },
        );
        self.observe_preparation(id).await?;
        self.check_observed(id)?;
        if matches!(
            self.entries.get(&id),
            Some(OrdinaryEntry::Observed { joined: Err(_), .. })
        ) {
            return Err(DeviceError::Worker);
        }
        let (output, input) = match self.entries.remove(&id) {
            Some(OrdinaryEntry::Observed {
                joined: Ok(output),
                input,
                ..
            }) => (output, input),
            entry => {
                if let Some(entry) = entry {
                    self.entries.insert(id, entry);
                }
                return self.fault("ordinary joined output was not retained");
            }
        };
        match output.result? {
            OrdinaryPreparation::NotSent(_) => return Ok(()),
            OrdinaryPreparation::Prepared(prepared) => {
                self.entries
                    .insert(id, OrdinaryEntry::Prepared { prepared, input });
            }
        }
        if matches!(self.entries.get(&id), Some(OrdinaryEntry::Prepared { input, .. }) if input.cancellation.is_cancelled())
        {
            self.entries.remove(&id);
            return Ok(());
        }
        let (prepared, input) = match self.entries.remove(&id) {
            Some(OrdinaryEntry::Prepared { prepared, input }) => (prepared, input),
            entry => {
                if let Some(entry) = entry {
                    self.entries.insert(id, entry);
                }
                return self.fault("ordinary prepared owner was not retained");
            }
        };
        self.entries.insert(
            id,
            OrdinaryEntry::SocketOwned {
                message: Box::new(RelayClientMessage::Send {
                    schema_version: RELAY_SCHEMA_VERSION,
                    envelope: prepared.envelope,
                }),
                outcome: None,
                input,
            },
        );
        let outcome = match self.entries.get(&id) {
            Some(OrdinaryEntry::SocketOwned { message, .. }) => connection.send(message).await,
            _ => return self.fault("ordinary socket owner was not retained"),
        };
        match self.entries.get_mut(&id) {
            Some(OrdinaryEntry::SocketOwned {
                outcome: observed, ..
            }) => *observed = Some(outcome),
            _ => {
                self.failure = Some(OrdinaryFailureBundle {
                    _primary: outcome.err().map(DeviceError::from),
                    _cleanup: OrdinaryIssue::Bookkeeping {
                        _reason: "ordinary send owner was not retained",
                    },
                });
                return Err(DeviceError::OutboundQuarantined);
            }
        }
        let (message, outcome, input) = match self.entries.remove(&id) {
            Some(OrdinaryEntry::SocketOwned {
                message,
                outcome: Some(outcome),
                input,
            }) => (message, outcome, input),
            entry => {
                if let Some(entry) = entry {
                    self.entries.insert(id, entry);
                }
                return self.fault("ordinary send result was not retained");
            }
        };
        drop(message);
        outcome?;
        let input = match Arc::try_unwrap(input) {
            Ok(input) => input,
            Err(input) => {
                self.entries.insert(id, OrdinaryEntry::Retained { input });
                return self.fault("ordinary input remained shared after join and send");
            }
        };
        let OrdinaryInput {
            endpoint,
            candidate,
            cancellation,
            slot,
            ..
        } = input;
        drop((endpoint, candidate, cancellation));
        self.entries.insert(
            id,
            OrdinaryEntry::AwaitAccepted {
                sent_at: tokio::time::Instant::now(),
                slot,
            },
        );
        Ok(())
    }

    async fn observe_preparation(&mut self, id: MessageId) -> Result<(), DeviceError> {
        let (task, joined, input) = match self.entries.get_mut(&id) {
            Some(OrdinaryEntry::Preparing {
                task,
                handle,
                input,
            }) => (*task, handle.await, Arc::clone(input)),
            _ => return self.fault("ordinary preparation handle was not retained"),
        };
        self.entries.insert(
            id,
            OrdinaryEntry::Observed {
                task,
                joined,
                input,
            },
        );
        Ok(())
    }

    fn check_observed(&mut self, id: MessageId) -> Result<(), DeviceError> {
        let matches = match self.entries.get(&id) {
            Some(OrdinaryEntry::Observed {
                task,
                joined,
                input,
            }) => {
                input.candidate.message_id == id
                    && Arc::ptr_eq(&input.slot.window, &self.identity)
                    && match joined {
                        Ok(output) => Arc::ptr_eq(&output.slot_identity, &input.slot.identity),
                        Err(error) => error.id() == *task,
                    }
            }
            _ => false,
        };
        if !matches {
            return self.fault("ordinary joined task or slot association mismatch");
        }
        Ok(())
    }

    pub(super) fn accepted(
        &mut self,
        message_id: MessageId,
    ) -> Result<OrdinaryAccepted, DeviceError> {
        if !self.awaiting(message_id) {
            return Err(RemoteError::Protocol.into());
        }
        self.entries.remove(&message_id);
        Ok(OrdinaryAccepted { message_id })
    }

    pub(super) fn expired(&mut self, message_id: MessageId) -> Result<(), DeviceError> {
        if !self.entries.contains_key(&message_id) {
            return Ok(());
        }
        if !self.awaiting(message_id) {
            return self.fault("ordinary maintenance encountered an unfinished owner");
        }
        self.entries.remove(&message_id);
        Ok(())
    }

    fn awaiting(&self, id: MessageId) -> bool {
        matches!(self.entries.get(&id), Some(OrdinaryEntry::AwaitAccepted { slot, .. }) if Arc::ptr_eq(&slot.window, &self.identity))
    }

    fn check_associations(&mut self) -> Result<(), DeviceError> {
        let valid = self.entries.iter().all(|(id, entry)| match entry {
            OrdinaryEntry::Registered { input }
            | OrdinaryEntry::Preparing { input, .. }
            | OrdinaryEntry::Observed { input, .. }
            | OrdinaryEntry::Prepared { input, .. }
            | OrdinaryEntry::SocketOwned { input, .. }
            | OrdinaryEntry::Retained { input } => {
                input.candidate.message_id == *id && Arc::ptr_eq(&input.slot.window, &self.identity)
            }
            OrdinaryEntry::AwaitAccepted { slot, .. } => Arc::ptr_eq(&slot.window, &self.identity),
        });
        if !valid {
            return self.fault("ordinary window ownership association mismatch");
        }
        Ok(())
    }

    pub(super) fn check_timeout(&mut self) -> Result<(), DeviceError> {
        self.check_associations()?;
        if self.entries.values().any(|entry| {
            matches!(entry, OrdinaryEntry::AwaitAccepted { sent_at, .. } if sent_at.elapsed() > Duration::from_secs(15))
        }) {
            return Err(RemoteError::Timeout.into());
        }
        Ok(())
    }

    pub(super) async fn finish(
        &mut self,
        outcome: Result<(), DeviceError>,
    ) -> Result<(), DeviceError> {
        self.completion = Some(outcome);
        let associations_ok = self.check_associations().is_ok();
        while let Some(id) = self
            .entries
            .iter()
            .find_map(|(id, entry)| matches!(entry, OrdinaryEntry::Preparing { .. }).then_some(*id))
        {
            if self.observe_preparation(id).await.is_err() || self.check_observed(id).is_err() {
                break;
            }
        }
        if (!associations_ok
            || self.entries.values().any(|entry| {
                matches!(
                    entry,
                    OrdinaryEntry::Registered { .. }
                        | OrdinaryEntry::Preparing { .. }
                        | OrdinaryEntry::SocketOwned { outcome: None, .. }
                        | OrdinaryEntry::Retained { .. }
                )
            }))
            && self.failure.is_none()
        {
            self.failure = Some(OrdinaryFailureBundle {
                _primary: None,
                _cleanup: OrdinaryIssue::Bookkeeping {
                    _reason: "ordinary cleanup retained unfinished ownership",
                },
            });
        }
        if let Some(failure) = self.failure.take() {
            self.retain(failure);
            return Err(DeviceError::OutboundQuarantined);
        }
        match self.completion.take() {
            Some(outcome) => {
                self.entries.clear();
                self.finished = true;
                outcome
            }
            None => {
                self.retain(OrdinaryFailureBundle {
                    _primary: None,
                    _cleanup: OrdinaryIssue::Bookkeeping {
                        _reason: "ordinary cleanup outcome was not retained",
                    },
                });
                Err(DeviceError::OutboundQuarantined)
            }
        }
    }

    fn fault<T>(&mut self, reason: &'static str) -> Result<T, DeviceError> {
        if self.failure.is_none() {
            self.failure = Some(OrdinaryFailureBundle {
                _primary: None,
                _cleanup: OrdinaryIssue::Bookkeeping { _reason: reason },
            });
        }
        Err(DeviceError::OutboundQuarantined)
    }

    fn retain(&mut self, failure: OrdinaryFailureBundle) {
        let retained = AbandonedOrdinaryWindow {
            _completion: self.completion.take(),
            _entries: std::mem::take(&mut self.entries),
            _failure: failure,
            _identity: Arc::clone(&self.identity),
            _endpoint: self.endpoint.clone(),
        };
        let _ = Box::leak(Box::new(retained));
        self.finished = true;
        self.endpoint.quarantine_outbound();
    }
}

impl Drop for OrdinarySendWindow {
    fn drop(&mut self) {
        if !self.finished
            && (!self.entries.is_empty() || self.completion.is_some() || self.failure.is_some())
        {
            let failure = self.failure.take().unwrap_or(OrdinaryFailureBundle {
                _primary: None,
                _cleanup: OrdinaryIssue::Abandoned,
            });
            self.retain(failure);
        }
    }
}

fn prepare_ordinary(input: &OrdinaryInput) -> Result<OrdinaryPreparation, DeviceError> {
    let endpoint = &input.endpoint;
    let now = input.selected_at;
    endpoint.ensure_active()?;
    let Some(loaded) = endpoint
        .inner
        .store
        .load_ordinary_transport_candidate(time(now)?, &input.candidate)?
    else {
        return Ok(OrdinaryPreparation::NotSent(OrdinaryNotSent));
    };
    let row = loaded.into_row();
    let peer = match endpoint.peer(input.candidate.recipient) {
        Err(DeviceError::Storage(StorageError::RevokedDevice(_))) => {
            return Ok(OrdinaryPreparation::NotSent(OrdinaryNotSent));
        }
        result => result?,
    };
    let envelope: EncryptedEnvelope = serde_json::from_slice(&row.envelope)?;
    if envelope.header.message_id != input.candidate.message_id
        || envelope.header.sender != endpoint.public_keys().device_id
        || envelope.header.recipient != input.candidate.recipient
        || envelope.header.expires_at_ms <= now.get()
    {
        return Err(DeviceError::Invalid(
            "queued ciphertext metadata does not match endpoint",
        ));
    }
    let retry_ms = 1_000_i64 << row.attempts.min(8);
    if let Err(error) = endpoint
        .inner
        .store
        .defer_outbox(&row.id, time(now)?.saturating_add(retry_ms))
    {
        if matches!(
            &error,
            StorageError::NotFound { entity: "outbox message", id } if id == &row.id
        ) && !endpoint.inner.store.confirm_transport_outbox(
            &row.id,
            &row.envelope,
            &serde_json::to_value(peer)?,
            time(now)?,
        )? {
            return Ok(OrdinaryPreparation::NotSent(OrdinaryNotSent));
        }
        return Err(error.into());
    }
    if input.cancellation.is_cancelled() {
        return Ok(OrdinaryPreparation::NotSent(OrdinaryNotSent));
    }
    let now = UnixMillis::now()?;
    let peer = match endpoint.peer(envelope.header.recipient) {
        Err(DeviceError::Storage(StorageError::RevokedDevice(_))) => {
            return Ok(OrdinaryPreparation::NotSent(OrdinaryNotSent));
        }
        result => result?,
    };
    if !endpoint.inner.store.confirm_transport_outbox(
        &envelope.header.message_id.to_string(),
        &serde_json::to_vec(&envelope)?,
        &serde_json::to_value(peer)?,
        time(now)?,
    )? {
        return Ok(OrdinaryPreparation::NotSent(OrdinaryNotSent));
    }
    Ok(OrdinaryPreparation::Prepared(Box::new(OrdinaryPrepared {
        envelope,
    })))
}
