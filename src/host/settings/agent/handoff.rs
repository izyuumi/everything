use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::sync::oneshot;

use crate::devices::{PairedWorkAdoption, SettingsReplyBuildRequest};

use super::{
    OwnedHistoricalSettingsCommand, PairedIntake, RetainedBind, RetainedUpdate,
    SettingsOperationInspection, SettingsServiceError,
};

pub(crate) struct PendingPairedResult {
    receiver: oneshot::Receiver<PairedResultHandoff>,
}

pub(crate) struct PairedWaitError {
    source: oneshot::error::RecvError,
}

pub(crate) enum PairedControlSendError {
    Full(PairedWorkAdoption),
    Closed(PairedWorkAdoption),
}

pub(crate) struct PairedResultCorrelation {
    record: u64,
    identity: Arc<()>,
    handoff: Arc<()>,
    intake: Arc<PairedIntake>,
}

pub(crate) struct PairedResultHandoff {
    payload: Option<PairedHandoffPayload>,
}

pub(crate) enum PairedHandoffReadError {
    Closed,
    RejectedBind,
    Bookkeeping,
}

enum PairedHandoffPayload {
    Ready(PairedReadyReply),
    RejectedBind {
        correlation: PairedResultCorrelation,
        _retained: RetainedBind,
    },
}

struct PairedReadyReply {
    _result: PairedServiceResult,
    correlation: PairedResultCorrelation,
    request: SettingsReplyBuildRequest,
}

enum PairedServiceResult {
    Bind {
        _close_error: Option<SettingsServiceError>,
        _close_attempted: bool,
    },
    DirectUpdate {
        _result: Result<Box<SettingsOperationInspection>, SettingsServiceError>,
    },
    Historical,
}

pub(super) enum PairedJoinedOwner {
    Bind(RetainedBind),
    DirectUpdate(RetainedUpdate),
    Historical(Box<OwnedHistoricalSettingsCommand>),
}

struct AbandonedPairedHandoff {
    _cause: PairedHandoffAbandoned,
    _payload: PairedHandoffPayload,
}

struct PairedHandoffAbandoned;

impl PendingPairedResult {
    pub(super) fn new(receiver: oneshot::Receiver<PairedResultHandoff>) -> Self {
        Self { receiver }
    }

    pub(crate) async fn wait(self) -> Result<PairedResultHandoff, PairedWaitError> {
        self.receiver
            .await
            .map_err(|source| PairedWaitError { source })
    }
}

impl PairedResultCorrelation {
    pub(super) fn new(
        record: u64,
        identity: Arc<()>,
        handoff: Arc<()>,
        intake: Arc<PairedIntake>,
    ) -> Self {
        Self {
            record,
            identity,
            handoff,
            intake,
        }
    }

    pub(super) fn record(&self) -> u64 {
        self.record
    }

    pub(super) fn belongs_to(&self, intake: &Arc<PairedIntake>) -> bool {
        Arc::ptr_eq(&self.intake, intake)
    }

    pub(super) fn matches(
        &self,
        record: u64,
        identity: &Arc<()>,
        handoff: &Arc<()>,
        intake: &Arc<PairedIntake>,
    ) -> bool {
        self.record == record
            && Arc::ptr_eq(&self.identity, identity)
            && Arc::ptr_eq(&self.handoff, handoff)
            && Arc::ptr_eq(&self.intake, intake)
    }

    fn for_adoption(&self) -> Self {
        Self {
            record: self.record,
            identity: Arc::clone(&self.identity),
            handoff: Arc::clone(&self.handoff),
            intake: Arc::clone(&self.intake),
        }
    }

    fn check_open(&self) -> Result<(), PairedHandoffReadError> {
        if self.intake.terminal_fault.load(Ordering::Acquire) {
            return Err(PairedHandoffReadError::Bookkeeping);
        }
        let state = self.intake.state.lock().map_err(|_| {
            self.intake.fail();
            PairedHandoffReadError::Bookkeeping
        })?;
        if self.intake.terminal_fault.load(Ordering::Acquire) {
            return Err(PairedHandoffReadError::Bookkeeping);
        }
        if state.closed {
            return Err(PairedHandoffReadError::Closed);
        }
        Ok(())
    }
}

impl PairedReadyReply {
    fn matches_identity(&self) -> bool {
        match &self.request {
            SettingsReplyBuildRequest::Bind { request, .. } => Arc::ptr_eq(
                request.committed_source().creation_identity(),
                &self.correlation.identity,
            ),
            SettingsReplyBuildRequest::Fresh(request) => Arc::ptr_eq(
                request
                    .committed_source()
                    .authority_source()
                    .invocation_identity(),
                &self.correlation.identity,
            ),
            SettingsReplyBuildRequest::Historical(request) => Arc::ptr_eq(
                request.authority_source().invocation_identity(),
                &self.correlation.identity,
            ),
            SettingsReplyBuildRequest::Access(_) => false,
        }
    }
}

impl PairedResultHandoff {
    pub(super) fn new(correlation: PairedResultCorrelation, owner: PairedJoinedOwner) -> Self {
        let payload = match owner {
            PairedJoinedOwner::Bind(retained) => match retained {
                RetainedBind {
                    parent: Ok(parent),
                    close_error,
                    close_attempted,
                    request,
                } => PairedHandoffPayload::Ready(PairedReadyReply {
                    _result: PairedServiceResult::Bind {
                        _close_error: close_error,
                        _close_attempted: close_attempted,
                    },
                    correlation,
                    request: SettingsReplyBuildRequest::Bind { parent, request },
                }),
                retained => PairedHandoffPayload::RejectedBind {
                    correlation,
                    _retained: retained,
                },
            },
            PairedJoinedOwner::DirectUpdate(RetainedUpdate { result, request }) => {
                PairedHandoffPayload::Ready(PairedReadyReply {
                    _result: PairedServiceResult::DirectUpdate { _result: result },
                    correlation,
                    request: SettingsReplyBuildRequest::Fresh(request),
                })
            }
            PairedJoinedOwner::Historical(request) => {
                PairedHandoffPayload::Ready(PairedReadyReply {
                    _result: PairedServiceResult::Historical,
                    correlation,
                    request: SettingsReplyBuildRequest::Historical(request),
                })
            }
        };
        Self {
            payload: Some(payload),
        }
    }

    pub(crate) fn reply_request(
        &self,
    ) -> Result<&SettingsReplyBuildRequest, PairedHandoffReadError> {
        match self
            .payload
            .as_ref()
            .ok_or(PairedHandoffReadError::Bookkeeping)?
        {
            PairedHandoffPayload::Ready(reply) => {
                reply.correlation.check_open()?;
                if !reply.matches_identity() {
                    return Err(PairedHandoffReadError::Bookkeeping);
                }
                Ok(&reply.request)
            }
            PairedHandoffPayload::RejectedBind { .. } => Err(PairedHandoffReadError::RejectedBind),
        }
    }

    pub(crate) fn reply_request_mut(
        &mut self,
    ) -> Result<&mut SettingsReplyBuildRequest, PairedHandoffReadError> {
        match self
            .payload
            .as_mut()
            .ok_or(PairedHandoffReadError::Bookkeeping)?
        {
            PairedHandoffPayload::Ready(reply) => {
                reply.correlation.check_open()?;
                if !reply.matches_identity() {
                    return Err(PairedHandoffReadError::Bookkeeping);
                }
                Ok(&mut reply.request)
            }
            PairedHandoffPayload::RejectedBind { .. } => Err(PairedHandoffReadError::RejectedBind),
        }
    }

    pub(crate) fn adoption_correlation(&self) -> Option<PairedResultCorrelation> {
        match self.payload.as_ref()? {
            PairedHandoffPayload::Ready(reply) if reply.matches_identity() => {
                Some(reply.correlation.for_adoption())
            }
            PairedHandoffPayload::Ready(_) => None,
            PairedHandoffPayload::RejectedBind { .. } => None,
        }
    }
}

impl Drop for PairedResultHandoff {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let correlation = match &payload {
                PairedHandoffPayload::Ready(reply) => &reply.correlation,
                PairedHandoffPayload::RejectedBind { correlation, .. } => correlation,
            };
            correlation.intake.fail();
            let _ = Box::leak(Box::new(AbandonedPairedHandoff {
                _cause: PairedHandoffAbandoned,
                _payload: payload,
            }));
        }
    }
}

impl fmt::Debug for PendingPairedResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingPairedResult([owned custody waiter])")
    }
}

impl fmt::Debug for PairedResultHandoff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedResultHandoff([owned])")
    }
}

impl fmt::Debug for PairedControlSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedControlSendError([retained adoption])")
    }
}

impl fmt::Debug for PairedWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedWaitError([retained cause])")
    }
}

impl fmt::Display for PairedWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("paired Settings result waiter closed")
    }
}

impl Error for PairedWaitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}
