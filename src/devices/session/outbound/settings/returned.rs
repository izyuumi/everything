use super::socket::{ReturnedSocketEvidence, ReturnedSocketParts};
use super::*;
use crate::devices::settings_ingress::work::SettingsStartedReturnAdoption;

#[derive(Clone, Copy)]
pub(in crate::devices) struct SettingsStartedReturnFailure {
    cause: ReturnFailure,
}

#[derive(Clone, Copy)]
enum ReturnFailure {
    Missing,
    Association,
    SharedInput,
    SharedSource,
}

pub(in crate::devices) struct OwnedStartedSettingsReturn {
    stage: Option<ReturnStage>,
}

enum ReturnStage {
    Phase(Box<SettingsCustodyState>),
    Input {
        evidence: ReturnEvidence,
        input: Arc<SettingsPreparationInput>,
    },
    Source {
        evidence: ReturnEvidence,
        retained: RetainedPreparationInput,
        source: Arc<SettingsSendSource>,
    },
    OwnedNoResend {
        _evidence: ReturnEvidence,
        retained: RetainedPreparationInput,
        source: SettingsSendSource,
    },
}

struct RetainedPreparationInput {
    _deadline: Instant,
    _cancellation: CancellationToken,
    _connection_cancellation: CancellationToken,
    _window: Arc<()>,
    _slot: Arc<()>,
    ticket: PendingSettingsSend,
}

enum ReturnEvidence {
    Stopped {
        _cause: SettingsPreparationStop,
    },
    Preparation {
        _expected: Id,
        _outcome: Result<ReturnedPreparationOutput, JoinError>,
        _stop: Option<SettingsPreparationStop>,
        _issue: Option<SettingsPreparationObserveFailure>,
    },
    Socket {
        _evidence: ReturnedSocketEvidence,
    },
}

enum CollectAction {
    Preparation,
    Socket,
    Ready,
}

struct ReturnedPreparationOutput {
    _outcome: Result<(), SettingsPreparationWorkerFailure>,
    _slot: Arc<()>,
}

struct AbandonedStartedReturn {
    _cause: SettingsStartedReturnFailure,
    stage: ReturnStage,
}

pub(in crate::devices) struct SettingsStartedWindowReturn<'window> {
    entry: Option<std::collections::btree_map::OccupiedEntry<'window, MessageId, RelaySendEntry>>,
    failure: &'window mut Option<OrdinaryFailureBundle>,
    endpoint: &'window DeviceEndpoint,
    extracted: bool,
}

impl SettingsStartedReturnFailure {
    pub(in crate::devices) const MISSING: Self = Self {
        cause: ReturnFailure::Missing,
    };
    pub(in crate::devices) const ASSOCIATION: Self = Self {
        cause: ReturnFailure::Association,
    };
    const SHARED_INPUT: Self = Self {
        cause: ReturnFailure::SharedInput,
    };
    const SHARED_SOURCE: Self = Self {
        cause: ReturnFailure::SharedSource,
    };
}

impl ReturnStage {
    fn source(&self) -> Option<&SettingsSendSource> {
        match self {
            Self::Phase(phase) => phase.source().map(Arc::as_ref),
            Self::Input { input, .. } => Some(&input.source),
            Self::Source { source, .. } => Some(source),
            Self::OwnedNoResend { source, .. } => Some(source),
        }
    }
}

impl OwnedStartedSettingsReturn {
    pub(in crate::devices) fn source(&self) -> Option<&SettingsSendSource> {
        self.stage.as_ref()?.source()
    }

    pub(in crate::devices) fn owned_no_resend(&self) -> bool {
        matches!(self.stage, Some(ReturnStage::OwnedNoResend { .. }))
    }

    pub(in crate::devices) fn finish(&mut self) -> Result<(), SettingsStartedReturnFailure> {
        loop {
            if self.owned_no_resend() {
                return Ok(());
            }
            let Some(stage) = self.stage.take() else {
                return Err(SettingsStartedReturnFailure::ASSOCIATION);
            };
            match stage {
                ReturnStage::Phase(phase) => match phase_parts(*phase) {
                    Ok((input, evidence)) => {
                        self.stage = Some(ReturnStage::Input { evidence, input });
                    }
                    Err(phase) => {
                        self.stage = Some(ReturnStage::Phase(Box::new(phase)));
                        return Err(SettingsStartedReturnFailure::ASSOCIATION);
                    }
                },
                ReturnStage::Input { evidence, input } => match Arc::try_unwrap(input) {
                    Ok(input) => {
                        let SettingsPreparationInput {
                            deadline,
                            cancellation,
                            connection_cancellation,
                            window,
                            slot,
                            ticket,
                            source,
                        } = input;
                        self.stage = Some(ReturnStage::Source {
                            evidence,
                            retained: RetainedPreparationInput {
                                _deadline: deadline,
                                _cancellation: cancellation,
                                _connection_cancellation: connection_cancellation,
                                _window: window,
                                _slot: slot,
                                ticket,
                            },
                            source,
                        });
                    }
                    Err(input) => {
                        self.stage = Some(ReturnStage::Input { evidence, input });
                        return Err(SettingsStartedReturnFailure::SHARED_INPUT);
                    }
                },
                ReturnStage::Source {
                    evidence,
                    retained,
                    source,
                } => match Arc::try_unwrap(source) {
                    Ok(source) => {
                        self.stage = Some(ReturnStage::OwnedNoResend {
                            _evidence: evidence,
                            retained,
                            source,
                        });
                    }
                    Err(source) => {
                        self.stage = Some(ReturnStage::Source {
                            evidence,
                            retained,
                            source,
                        });
                        return Err(SettingsStartedReturnFailure::SHARED_SOURCE);
                    }
                },
                complete @ ReturnStage::OwnedNoResend { .. } => self.stage = Some(complete),
            }
        }
    }

    pub(in crate::devices) fn ticket_matches(&self) -> bool {
        match self.stage.as_ref() {
            Some(ReturnStage::OwnedNoResend {
                retained, source, ..
            }) => retained.ticket.matches_source(source),
            _ => false,
        }
    }
}

impl Drop for OwnedStartedSettingsReturn {
    fn drop(&mut self) {
        if let Some(stage) = self.stage.take() {
            let retained = Box::leak(Box::new(AbandonedStartedReturn {
                _cause: SettingsStartedReturnFailure::ASSOCIATION,
                stage,
            }));
            if let Some(source) = retained.stage.source() {
                source.quarantine_socket_owner();
            }
        }
    }
}

impl RelaySendWindow {
    pub(in crate::devices) async fn collect_started_settings_return(
        &mut self,
        id: MessageId,
    ) -> Result<(), SettingsStartedReturnFailure> {
        loop {
            let action = {
                let Some(RelaySendEntry::Settings(cell)) = self.entries.get_mut(&id) else {
                    return Err(SettingsStartedReturnFailure::MISSING);
                };
                cell.state.stop_preparation(SettingsPreparationStop::Closed);
                if !phase_identity_matches(
                    &cell.state,
                    &self.identity,
                    &cell.slot.identity,
                    &self.socket,
                ) {
                    return Err(SettingsStartedReturnFailure::ASSOCIATION);
                }
                match &cell.state {
                    SettingsCustodyState::Preparing { .. } => CollectAction::Preparation,
                    SettingsCustodyState::Socket(attempt) if attempt.is_running() => {
                        CollectAction::Socket
                    }
                    state if phase_ready(state) && cell.matches_window(&self.identity) => {
                        CollectAction::Ready
                    }
                    _ => return Err(SettingsStartedReturnFailure::ASSOCIATION),
                }
            };
            match action {
                CollectAction::Preparation => {
                    let _ = self.observe_settings_preparation(id).await;
                }
                CollectAction::Socket => {
                    let _ = self.join_settings_socket_on_close(id).await;
                }
                CollectAction::Ready => return Ok(()),
            }
        }
    }

    pub(in crate::devices) fn started_settings_return(
        &mut self,
        id: MessageId,
        association: &Arc<SettingsSendLoanAssociation>,
    ) -> Result<SettingsStartedWindowReturn<'_>, SettingsStartedReturnFailure> {
        let Entry::Occupied(entry) = self.entries.entry(id) else {
            return Err(SettingsStartedReturnFailure::MISSING);
        };
        let RelaySendEntry::Settings(cell) = entry.get() else {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        };
        if !phase_ready(&cell.state)
            || !phase_identity_matches(
                &cell.state,
                &self.identity,
                &cell.slot.identity,
                &self.socket,
            )
            || !cell.matches_window(&self.identity)
            || cell
                .association
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, association))
            || !association.matches_slot(&self.identity, &cell.slot.identity)
        {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        }
        Ok(SettingsStartedWindowReturn {
            entry: Some(entry),
            failure: &mut self.failure,
            endpoint: &self.endpoint,
            extracted: false,
        })
    }
}

impl SettingsStartedWindowReturn<'_> {
    pub(in crate::devices) fn source(&self) -> Option<&SettingsSendSource> {
        match self.entry.as_ref()?.get() {
            RelaySendEntry::Settings(cell) => cell.state.source().map(Arc::as_ref),
            _ => None,
        }
    }

    pub(in crate::devices) fn take_phase(&mut self) -> Option<OwnedStartedSettingsReturn> {
        if self.extracted {
            return None;
        }
        let RelaySendEntry::Settings(cell) = self.entry.as_mut()?.get_mut() else {
            return None;
        };
        self.extracted = true;
        let phase = std::mem::replace(&mut cell.state, SettingsCustodyState::Returning);
        Some(OwnedStartedSettingsReturn {
            stage: Some(ReturnStage::Phase(Box::new(phase))),
        })
    }

    pub(in crate::devices) fn complete(
        mut self,
        adoption: SettingsStartedReturnAdoption<'_>,
    ) -> Result<(), SettingsStartedReturnFailure> {
        let entry = self
            .entry
            .as_ref()
            .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
        let RelaySendEntry::Settings(cell) = entry.get() else {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        };
        let association = cell
            .association
            .as_ref()
            .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
        if !self.extracted
            || !matches!(cell.state, SettingsCustodyState::Returning)
            || !adoption.matches_slot(association, &cell.slot.window, &cell.slot.identity)
        {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        }
        if let Some(entry) = self.entry.take() {
            drop(entry.remove());
        }
        Ok(())
    }
}

impl Drop for SettingsStartedWindowReturn<'_> {
    fn drop(&mut self) {
        if self.extracted && self.entry.is_some() {
            if self.failure.is_none() {
                *self.failure = Some(OrdinaryFailureBundle {
                    _primary: None,
                    _cleanup: OrdinaryIssue::PrivateCustodyOutstanding,
                });
            }
            if let Some(entry) = self.entry.as_ref()
                && let RelaySendEntry::Settings(cell) = entry.get()
            {
                cell.quarantine_work();
            }
            self.endpoint.quarantine_outbound();
        }
    }
}

fn phase_identity_matches(
    state: &SettingsCustodyState,
    window: &Arc<()>,
    slot: &Arc<()>,
    socket: &Arc<RelaySocketIdentity>,
) -> bool {
    let input = match state {
        SettingsCustodyState::Stopped { input, .. }
        | SettingsCustodyState::Observed { input, .. }
        | SettingsCustodyState::Preparing { input, .. } => input,
        SettingsCustodyState::Socket(attempt) => {
            return attempt.return_identity_matches(window, slot, socket);
        }
        _ => return false,
    };
    Arc::ptr_eq(&input.window, window)
        && Arc::ptr_eq(&input.slot, slot)
        && input.ticket.matches_slot(window, slot)
        && input.ticket.shares_socket(socket)
        && input.ticket.matches_source(&input.source)
}

fn phase_ready(state: &SettingsCustodyState) -> bool {
    match state {
        SettingsCustodyState::Stopped { .. } => true,
        SettingsCustodyState::Observed {
            expected,
            joined,
            input,
            ..
        } => match joined {
            Ok(output) => {
                Arc::ptr_eq(&output.slot, &input.slot) && Arc::ptr_eq(&output.source, &input.source)
            }
            Err(error) => error.id() == *expected,
        },
        SettingsCustodyState::Socket(attempt) => attempt.return_ready(),
        _ => false,
    }
}

fn phase_parts(
    phase: SettingsCustodyState,
) -> Result<(Arc<SettingsPreparationInput>, ReturnEvidence), SettingsCustodyState> {
    if !phase_ready(&phase) {
        return Err(phase);
    }
    match phase {
        SettingsCustodyState::Stopped { cause, input } => {
            Ok((input, ReturnEvidence::Stopped { _cause: cause }))
        }
        SettingsCustodyState::Observed {
            expected,
            joined,
            input,
            stop,
            issue,
        } => {
            let outcome = joined.map(|output| {
                let SettingsPreparationOutput {
                    outcome,
                    slot,
                    source,
                } = output;
                let outcome = outcome.map(drop);
                drop(source);
                ReturnedPreparationOutput {
                    _outcome: outcome,
                    _slot: slot,
                }
            });
            Ok((
                input,
                ReturnEvidence::Preparation {
                    _expected: expected,
                    _outcome: outcome,
                    _stop: stop,
                    _issue: issue,
                },
            ))
        }
        SettingsCustodyState::Socket(attempt) => match attempt.return_parts() {
            Ok(ReturnedSocketParts { input, evidence }) => Ok((
                input,
                ReturnEvidence::Socket {
                    _evidence: evidence,
                },
            )),
            Err(attempt) => Err(SettingsCustodyState::Socket(attempt)),
        },
        phase => Err(phase),
    }
}

impl fmt::Debug for SettingsStartedReturnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsStartedReturnFailure([retained custody])")
    }
}

impl fmt::Display for SettingsStartedReturnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.cause {
            ReturnFailure::Missing => "Settings return has no matching retained owner",
            ReturnFailure::Association => "Settings return ownership does not match",
            ReturnFailure::SharedInput => "Settings return input still has an owner",
            ReturnFailure::SharedSource => "Settings return source still has an owner",
        })
    }
}

impl std::error::Error for SettingsStartedReturnFailure {}
