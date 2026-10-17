#[cfg(unix)]
mod settings;

use super::*;
#[cfg(unix)]
use crate::devices::settings_ingress::work::{
    OwnedQueuedSettingsReply, PendingSettingsSend, SettingsSendAdmissionError,
    SettingsSendLoanAssociation, SettingsSendReturnReason, SettingsSendSource,
};
use crate::storage::{TransportOutboundCandidate, TransportOutboundRouting};
#[cfg(unix)]
use crate::transport::remote::RelaySocketIdentity;
#[cfg(unix)]
use settings::{GuardWait, SettingsPreparationInput, SettingsPreparationOutput};
#[cfg(unix)]
pub(in crate::devices) use settings::{
    OwnedStartedSettingsReturn, ReturnedSettingsSqlInput, SettingsRemovedSendSlot,
    SettingsStartedReturnFailure,
};
#[cfg(unix)]
pub(in crate::devices) use settings::{SettingsPreparationStop, SettingsPreparationWorkerFailure};
#[cfg(unix)]
pub(crate) use settings::{
    SettingsSocketAdmissionFailure, SettingsSocketObserveFailure, SettingsSocketSendLoan,
};
#[cfg(unix)]
use std::collections::btree_map::Entry;
use tokio::task::{Id, JoinError, JoinHandle};

const MAX_ORDINARY_INFLIGHT: usize = 16;

pub(in crate::devices) struct RelaySendWindow {
    endpoint: DeviceEndpoint,
    identity: Arc<()>,
    #[cfg(unix)]
    socket: Arc<RelaySocketIdentity>,
    entries: BTreeMap<MessageId, RelaySendEntry>,
    finished: bool,
    completion: Option<Result<(), DeviceError>>,
    failure: Option<OrdinaryFailureBundle>,
}

enum RelaySendEntry {
    Ordinary(OrdinaryEntry),
    #[cfg(unix)]
    Settings(SettingsSendCell),
}

impl RelaySendEntry {
    fn ordinary(&self) -> Option<&OrdinaryEntry> {
        match self {
            Self::Ordinary(entry) => Some(entry),
            #[cfg(unix)]
            Self::Settings(_) => None,
        }
    }

    fn ordinary_mut(&mut self) -> Option<&mut OrdinaryEntry> {
        match self {
            Self::Ordinary(entry) => Some(entry),
            #[cfg(unix)]
            Self::Settings(_) => None,
        }
    }
}

#[cfg(unix)]
struct SettingsSendCell {
    association: Option<Arc<SettingsSendLoanAssociation>>,
    state: SettingsCustodyState,
    slot: OrdinarySlot,
}

#[cfg(unix)]
enum SettingsCustodyState {
    Provisional,
    Returning,
    LoanedUnstarted(Arc<SettingsSendSource>),
    Waiting {
        waiting: GuardWait,
        input: Arc<SettingsPreparationInput>,
    },
    Preparing {
        task: Id,
        handle: JoinHandle<SettingsPreparationOutput>,
        input: Arc<SettingsPreparationInput>,
        stop: Option<SettingsPreparationStop>,
    },
    Observed {
        expected: Id,
        joined: Result<SettingsPreparationOutput, JoinError>,
        input: Arc<SettingsPreparationInput>,
        stop: Option<SettingsPreparationStop>,
        issue: Option<settings::SettingsPreparationObserveFailure>,
    },
    Prepared {
        frame: Box<RelayClientMessage>,
        input: Arc<SettingsPreparationInput>,
    },
    Socket(Box<settings::SettingsSocketAttempt>),
    Stopped {
        cause: SettingsPreparationStop,
        input: Arc<SettingsPreparationInput>,
    },
}

#[cfg(unix)]
pub(in crate::devices) struct SettingsWindowReservation<'window> {
    cell: &'window mut SettingsSendCell,
    failure: &'window mut Option<OrdinaryFailureBundle>,
}

#[cfg(unix)]
pub(in crate::devices) struct SettingsWindowReturn<'window> {
    entry: Option<std::collections::btree_map::OccupiedEntry<'window, MessageId, RelaySendEntry>>,
    failure: &'window mut Option<OrdinaryFailureBundle>,
}

#[cfg(unix)]
impl SettingsSendCell {
    fn matches_window(&self, window: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.slot.window, window)
            && match &self.state {
                SettingsCustodyState::Provisional | SettingsCustodyState::Returning => {
                    self.association.as_ref().is_none_or(|association| {
                        association.matches_slot(window, &self.slot.identity)
                    })
                }
                state => self.association.as_ref().is_some_and(|association| {
                    association.matches_slot(window, &self.slot.identity)
                        && state
                            .source()
                            .is_some_and(|source| source.matches_association(association))
                }),
            }
    }

    fn quarantine_work(&self) {
        if let Some(association) = &self.association {
            association.quarantine();
        }
    }
}

#[cfg(unix)]
impl SettingsWindowReservation<'_> {
    pub(in crate::devices) fn identities(&self) -> (&Arc<()>, &Arc<()>) {
        (&self.cell.slot.window, &self.cell.slot.identity)
    }

    pub(in crate::devices) fn associate(&mut self, association: Arc<SettingsSendLoanAssociation>) {
        self.cell.association = Some(association);
    }

    pub(in crate::devices) fn install(self, source: Arc<SettingsSendSource>) {
        self.cell.state = SettingsCustodyState::LoanedUnstarted(source);
    }
}

#[cfg(unix)]
impl Drop for SettingsWindowReservation<'_> {
    fn drop(&mut self) {
        if matches!(self.cell.state, SettingsCustodyState::Provisional) && self.failure.is_none() {
            *self.failure = Some(OrdinaryFailureBundle {
                _primary: None,
                _cleanup: OrdinaryIssue::Bookkeeping {
                    _reason: "Settings custody reservation was abandoned",
                },
            });
        }
    }
}

#[cfg(unix)]
impl SettingsWindowReturn<'_> {
    pub(in crate::devices) fn source(&self) -> Option<&Arc<SettingsSendSource>> {
        match self.entry.as_ref()?.get() {
            RelaySendEntry::Settings(SettingsSendCell {
                state: SettingsCustodyState::LoanedUnstarted(source),
                ..
            }) => Some(source),
            _ => None,
        }
    }

    pub(in crate::devices) fn source_mut(&mut self) -> Option<&mut Arc<SettingsSendSource>> {
        match self.entry.as_mut()?.get_mut() {
            RelaySendEntry::Settings(SettingsSendCell {
                state: SettingsCustodyState::LoanedUnstarted(source),
                ..
            }) => Some(source),
            _ => None,
        }
    }

    pub(in crate::devices) fn complete(mut self) {
        if let Some(entry) = self.entry.take() {
            drop(entry.remove());
        }
    }
}

#[cfg(unix)]
impl Drop for SettingsWindowReturn<'_> {
    fn drop(&mut self) {
        if self.entry.is_some() && self.failure.is_none() {
            *self.failure = Some(OrdinaryFailureBundle {
                _primary: None,
                _cleanup: OrdinaryIssue::Bookkeeping {
                    _reason: "Settings custody return did not complete",
                },
            });
        }
    }
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

enum ActivePreparation {
    Ordinary(MessageId),
    #[cfg(unix)]
    Settings(MessageId),
    #[cfg(unix)]
    SettingsSocket(MessageId),
}

enum OrdinaryIssue {
    Bookkeeping {
        _reason: &'static str,
    },
    Abandoned,
    #[cfg(unix)]
    PrivateCustodyOutstanding,
}

struct OrdinaryFailureBundle {
    _primary: Option<DeviceError>,
    _cleanup: OrdinaryIssue,
}

struct AbandonedOrdinaryWindow {
    _completion: Option<Result<(), DeviceError>>,
    _entries: BTreeMap<MessageId, RelaySendEntry>,
    _failure: OrdinaryFailureBundle,
    _identity: Arc<()>,
    #[cfg(unix)]
    _socket: Arc<RelaySocketIdentity>,
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

impl RelaySendWindow {
    pub(super) fn new(
        endpoint: DeviceEndpoint,
        _connection: &RelayConnection,
    ) -> Result<Self, DeviceError> {
        endpoint.check_outbound_health()?;
        #[cfg(unix)]
        let socket = _connection.capture_socket_identity(endpoint.public_keys().device_id)?;
        Ok(Self {
            endpoint,
            identity: Arc::new(()),
            #[cfg(unix)]
            socket,
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

    #[cfg(unix)]
    pub(in crate::devices) fn matches_endpoint(&self, endpoint: &DeviceEndpoint) -> bool {
        Arc::ptr_eq(&self.endpoint.inner, &endpoint.inner)
    }

    #[cfg(unix)]
    pub(in crate::devices) fn reserve_settings(
        &mut self,
        queued: &OwnedQueuedSettingsReply,
        connection: &Arc<SettingsConnectionIdentity>,
    ) -> Result<SettingsWindowReservation<'_>, SettingsSendAdmissionError> {
        use SettingsSendAdmissionError as Admission;

        self.endpoint
            .check_outbound_health()
            .map_err(Admission::Endpoint)?;
        if self.finished || self.failure.is_some() {
            return Err(Admission::Association);
        }
        if !self.has_capacity() {
            return Err(Admission::Full);
        }
        if !connection.is_live() || !self.socket.is_live() {
            return Err(Admission::Socket(RemoteError::Closed));
        }
        if !connection.shares_socket(&self.socket) {
            return Err(Admission::Socket(RemoteError::Protocol));
        }
        let id = queued
            .committed()
            .map_err(Admission::Owner)?
            .header()
            .message_id;
        let Entry::Vacant(entry) = self.entries.entry(id) else {
            return Err(Admission::Association);
        };
        let entry = entry.insert(RelaySendEntry::Settings(SettingsSendCell {
            association: None,
            state: SettingsCustodyState::Provisional,
            slot: OrdinarySlot {
                window: Arc::clone(&self.identity),
                identity: Arc::new(()),
            },
        }));
        let RelaySendEntry::Settings(cell) = entry else {
            return Err(Admission::Association);
        };
        Ok(SettingsWindowReservation {
            cell,
            failure: &mut self.failure,
        })
    }

    #[cfg(unix)]
    pub(in crate::devices) fn settings_return(
        &mut self,
        ticket: &PendingSettingsSend,
    ) -> Result<SettingsWindowReturn<'_>, SettingsSendReturnReason> {
        use SettingsSendReturnReason as Reason;

        if !ticket.shares_socket(&self.socket) {
            return Err(Reason::Association);
        }
        let Entry::Occupied(entry) = self.entries.entry(ticket.message_id()) else {
            return Err(Reason::Association);
        };
        let RelaySendEntry::Settings(cell) = entry.get() else {
            return Err(Reason::Association);
        };
        let SettingsCustodyState::LoanedUnstarted(source) = &cell.state else {
            return Err(Reason::Association);
        };
        if !ticket.matches_slot(&self.identity, &cell.slot.identity)
            || !Arc::ptr_eq(&cell.slot.window, &self.identity)
            || !ticket.matches_source(source)
        {
            return Err(Reason::Association);
        }
        Ok(SettingsWindowReturn {
            entry: Some(entry),
            failure: &mut self.failure,
        })
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
        self.insert_ordinary(id, OrdinaryEntry::Registered { input });
        let input = match self.entries.get(&id).and_then(RelaySendEntry::ordinary) {
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
        self.insert_ordinary(
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
            self.entries.get(&id).and_then(RelaySendEntry::ordinary),
            Some(OrdinaryEntry::Observed { joined: Err(_), .. })
        ) {
            return Err(DeviceError::Worker);
        }
        let (output, input) = match self.take_ordinary(id) {
            Some(OrdinaryEntry::Observed {
                joined: Ok(output),
                input,
                ..
            }) => (output, input),
            entry => {
                if let Some(entry) = entry {
                    self.insert_ordinary(id, entry);
                }
                return self.fault("ordinary joined output was not retained");
            }
        };
        match output.result? {
            OrdinaryPreparation::NotSent(_) => return Ok(()),
            OrdinaryPreparation::Prepared(prepared) => {
                self.insert_ordinary(id, OrdinaryEntry::Prepared { prepared, input });
            }
        }
        if matches!(self.entries.get(&id).and_then(RelaySendEntry::ordinary), Some(OrdinaryEntry::Prepared { input, .. }) if input.cancellation.is_cancelled())
        {
            self.take_ordinary(id);
            return Ok(());
        }
        let (prepared, input) = match self.take_ordinary(id) {
            Some(OrdinaryEntry::Prepared { prepared, input }) => (prepared, input),
            entry => {
                if let Some(entry) = entry {
                    self.insert_ordinary(id, entry);
                }
                return self.fault("ordinary prepared owner was not retained");
            }
        };
        self.insert_ordinary(
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
        let outcome = match self.entries.get(&id).and_then(RelaySendEntry::ordinary) {
            Some(OrdinaryEntry::SocketOwned { message, .. }) => connection.send(message).await,
            _ => return self.fault("ordinary socket owner was not retained"),
        };
        match self
            .entries
            .get_mut(&id)
            .and_then(RelaySendEntry::ordinary_mut)
        {
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
        let (message, outcome, input) = match self.take_ordinary(id) {
            Some(OrdinaryEntry::SocketOwned {
                message,
                outcome: Some(outcome),
                input,
            }) => (message, outcome, input),
            entry => {
                if let Some(entry) = entry {
                    self.insert_ordinary(id, entry);
                }
                return self.fault("ordinary send result was not retained");
            }
        };
        drop(message);
        outcome?;
        let input = match Arc::try_unwrap(input) {
            Ok(input) => input,
            Err(input) => {
                self.insert_ordinary(id, OrdinaryEntry::Retained { input });
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
        self.insert_ordinary(
            id,
            OrdinaryEntry::AwaitAccepted {
                sent_at: tokio::time::Instant::now(),
                slot,
            },
        );
        Ok(())
    }

    fn insert_ordinary(&mut self, id: MessageId, entry: OrdinaryEntry) {
        self.entries.insert(id, RelaySendEntry::Ordinary(entry));
    }

    fn take_ordinary(&mut self, id: MessageId) -> Option<OrdinaryEntry> {
        match self.entries.remove(&id) {
            Some(RelaySendEntry::Ordinary(entry)) => Some(entry),
            #[cfg(unix)]
            Some(entry) => {
                self.entries.insert(id, entry);
                None
            }
            None => None,
        }
    }

    async fn observe_preparation(&mut self, id: MessageId) -> Result<(), DeviceError> {
        let (task, joined, input) = match self
            .entries
            .get_mut(&id)
            .and_then(RelaySendEntry::ordinary_mut)
        {
            Some(OrdinaryEntry::Preparing {
                task,
                handle,
                input,
            }) => (*task, handle.await, Arc::clone(input)),
            _ => return self.fault("ordinary preparation handle was not retained"),
        };
        self.insert_ordinary(
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
        let matches = match self.entries.get(&id).and_then(RelaySendEntry::ordinary) {
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
            return self.fault("relay maintenance encountered unfinished custody");
        }
        self.entries.remove(&message_id);
        Ok(())
    }

    fn awaiting(&self, id: MessageId) -> bool {
        matches!(self.entries.get(&id).and_then(RelaySendEntry::ordinary), Some(OrdinaryEntry::AwaitAccepted { slot, .. }) if Arc::ptr_eq(&slot.window, &self.identity))
    }

    fn check_associations(&mut self) -> Result<(), DeviceError> {
        let valid = self.entries.iter().all(|(id, entry)| match entry {
            RelaySendEntry::Ordinary(entry) => match entry {
                OrdinaryEntry::Registered { input }
                | OrdinaryEntry::Preparing { input, .. }
                | OrdinaryEntry::Observed { input, .. }
                | OrdinaryEntry::Prepared { input, .. }
                | OrdinaryEntry::SocketOwned { input, .. }
                | OrdinaryEntry::Retained { input } => {
                    input.candidate.message_id == *id
                        && Arc::ptr_eq(&input.slot.window, &self.identity)
                }
                OrdinaryEntry::AwaitAccepted { slot, .. } => {
                    Arc::ptr_eq(&slot.window, &self.identity)
                }
            },
            #[cfg(unix)]
            RelaySendEntry::Settings(cell) => cell.matches_window(&self.identity),
        });
        if !valid {
            return self.fault("ordinary window ownership association mismatch");
        }
        Ok(())
    }

    pub(super) fn check_timeout(&mut self) -> Result<(), DeviceError> {
        self.check_associations()?;
        if self.entries.values().filter_map(RelaySendEntry::ordinary).any(|entry| {
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
        #[cfg(unix)]
        self.stop_settings_waiters();
        let associations_ok = self.check_associations().is_ok();
        loop {
            let preparing = {
                self.entries.iter().find_map(|(id, entry)| match entry {
                    RelaySendEntry::Ordinary(OrdinaryEntry::Preparing { .. }) => {
                        Some(ActivePreparation::Ordinary(*id))
                    }
                    #[cfg(unix)]
                    RelaySendEntry::Settings(SettingsSendCell {
                        state: SettingsCustodyState::Preparing { .. },
                        ..
                    }) => Some(ActivePreparation::Settings(*id)),
                    #[cfg(unix)]
                    RelaySendEntry::Settings(SettingsSendCell {
                        state: SettingsCustodyState::Socket(attempt),
                        ..
                    }) if attempt.is_running() => Some(ActivePreparation::SettingsSocket(*id)),
                    _ => None,
                })
            };
            match preparing {
                Some(ActivePreparation::Ordinary(id)) => {
                    let complete = self.observe_preparation(id).await.is_ok()
                        && self.check_observed(id).is_ok();
                    if !complete && self.failure.is_none() {
                        self.failure = Some(OrdinaryFailureBundle {
                            _primary: None,
                            _cleanup: OrdinaryIssue::Bookkeeping {
                                _reason: "ordinary cleanup could not observe preparation",
                            },
                        });
                    }
                }
                #[cfg(unix)]
                Some(ActivePreparation::Settings(id)) => {
                    if self.observe_settings_preparation(id).await.is_err()
                        && self.failure.is_none()
                    {
                        self.failure = Some(OrdinaryFailureBundle {
                            _primary: None,
                            _cleanup: OrdinaryIssue::PrivateCustodyOutstanding,
                        });
                    }
                }
                #[cfg(unix)]
                Some(ActivePreparation::SettingsSocket(id)) => {
                    if self.join_settings_socket_on_close(id).await.is_err()
                        && self.failure.is_none()
                    {
                        self.failure = Some(OrdinaryFailureBundle {
                            _primary: None,
                            _cleanup: OrdinaryIssue::PrivateCustodyOutstanding,
                        });
                    }
                }
                None => break,
            }
        }
        if (!associations_ok
            || self
                .entries
                .values()
                .filter_map(RelaySendEntry::ordinary)
                .any(|entry| {
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
        #[cfg(unix)]
        if self
            .entries
            .values()
            .any(|entry| matches!(entry, RelaySendEntry::Settings(_)))
            && self.failure.is_none()
        {
            self.failure = Some(OrdinaryFailureBundle {
                _primary: None,
                _cleanup: OrdinaryIssue::PrivateCustodyOutstanding,
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
        #[cfg(unix)]
        self.stop_settings_waiters();
        let retained = AbandonedOrdinaryWindow {
            _completion: self.completion.take(),
            _entries: std::mem::take(&mut self.entries),
            _failure: failure,
            _identity: Arc::clone(&self.identity),
            #[cfg(unix)]
            _socket: Arc::clone(&self.socket),
            _endpoint: self.endpoint.clone(),
        };
        let _retained = Box::leak(Box::new(retained));
        #[cfg(unix)]
        for entry in _retained._entries.values() {
            if let RelaySendEntry::Settings(cell) = entry {
                cell.quarantine_work();
            }
        }
        self.finished = true;
        self.endpoint.quarantine_outbound();
    }
}

impl Drop for RelaySendWindow {
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
