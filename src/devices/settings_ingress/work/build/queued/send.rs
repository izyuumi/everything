use super::*;
use crate::devices::session::outbound::{
    OwnedStartedSettingsReturn, RelaySendWindow, ReturnedSettingsSqlInput, SettingsRemovedSendSlot,
    SettingsStartedReturnFailure,
};
use crate::devices::session::outbound::{
    SettingsPreparationStop, SettingsPreparationWorkerFailure,
};
use crate::storage::{TransportOutboundCandidate, TransportOutboundRouting};
use crate::transport::remote::{RelaySocketIdentity, RemoteError};
use thiserror::Error;

pub(in crate::devices) struct PendingSettingsSend {
    association: Arc<SettingsSendLoanAssociation>,
}

pub(in crate::devices) struct SettingsSendSource {
    candidate: TransportOutboundCandidate,
    reply: Option<OwnedQueuedSettingsReply>,
    association: Arc<SettingsSendLoanAssociation>,
}

pub(in crate::devices) struct SettingsSendLoanAssociation {
    message_id: MessageId,
    ledger: Arc<Ledger>,
    connection: Arc<SettingsConnectionIdentity>,
    window: Arc<()>,
    slot: Arc<()>,
    build_slot: Arc<()>,
    build_association: Arc<super::super::super::ReplyBuildAssociation>,
}

pub(super) struct WorkSettingsSendLoan {
    association: Arc<SettingsSendLoanAssociation>,
}

pub(super) struct ReturnedSettingsSend {
    loan: WorkSettingsSendLoan,
}

#[derive(Error)]
pub(in crate::devices) enum SettingsSendAdmissionError {
    #[error("relay send window is full")]
    Full,
    #[error("Settings reply cannot be resent")]
    NoResend,
    #[error("private Settings reply has no matching retained owner")]
    MissingOwner,
    #[error("Settings receive ownership does not admit lending")]
    Receive(#[source] SettingsReceiveError),
    #[error("Settings queued ownership is unavailable")]
    Owner(#[source] PairedHandoffReadError),
    #[error("relay endpoint does not admit lending")]
    Endpoint(#[source] crate::devices::DeviceError),
    #[error("Settings reply socket is unavailable")]
    Socket(#[source] RemoteError),
    #[error("Settings reply custody association does not match")]
    Association,
}

#[derive(Debug, Error)]
pub(in crate::devices) enum SettingsSendReturnReason {
    #[error("Settings reply custody association does not match")]
    Association,
    #[error("Settings reply custody source remains shared")]
    SharedSource,
    #[error("Settings reply custody has no matching loan")]
    MissingLoan,
}

pub(in crate::devices) struct SettingsSendReturnFailure {
    reason: SettingsSendReturnReason,
    _ticket: PendingSettingsSend,
}

impl SettingsSendLoanAssociation {
    pub(in crate::devices) fn matches_slot(&self, window: &Arc<()>, slot: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.window, window) && Arc::ptr_eq(&self.slot, slot)
    }

    pub(in crate::devices) fn quarantine(&self) {
        self.ledger.fail();
        self.ledger.terminal.store(true, Ordering::Release);
        self.ledger.changed.notify_waiters();
    }

    fn matches_payload(
        &self,
        payload: &QueuedReplyPayload,
        ledger: &Arc<Ledger>,
        message_id: MessageId,
    ) -> bool {
        let permit = &payload.input.record.permit;
        self.message_id == message_id
            && Arc::ptr_eq(&self.ledger, ledger)
            && Arc::ptr_eq(&permit.ledger, ledger)
            && Arc::ptr_eq(&permit.slot, &self.build_slot)
            && Arc::ptr_eq(&permit.association, &self.build_association)
            && payload.committed.header().message_id == message_id
            && payload.settled()
    }
}

impl SettingsSendSource {
    pub(in crate::devices) fn matches_association(
        &self,
        association: &Arc<SettingsSendLoanAssociation>,
    ) -> bool {
        Arc::ptr_eq(&self.association, association) && self.reply.is_some()
    }

    pub(in crate::devices) fn preparation_wait_inputs(
        &self,
    ) -> Result<
        (
            Arc<tokio::sync::Mutex<()>>,
            std::time::Instant,
            tokio_util::sync::CancellationToken,
        ),
        SettingsPreparationWorkerFailure,
    > {
        use SettingsPreparationWorkerFailure as Failure;

        let owner = self.reply.as_ref().ok_or(Failure::Association)?;
        let payload = owner.settled_payload().map_err(Failure::Owner)?;
        if !self.association.matches_payload(
            payload,
            &self.association.ledger,
            self.candidate.message_id,
        ) || self.candidate.recipient != payload.committed.header().recipient
        {
            return Err(Failure::Association);
        }
        let (deadline, expiry) = match owner.request().map_err(Failure::Owner)? {
            SettingsReplyBuildRequest::Fresh(request) => {
                let source = request.committed_source();
                (
                    source.original_deadline(),
                    source.authority_source().intent().expires_at.get(),
                )
            }
            SettingsReplyBuildRequest::Historical(request) => (
                request.original_deadline(),
                request.authority_source().intent().expires_at.get(),
            ),
            SettingsReplyBuildRequest::Bind { request, .. } => {
                let lifetime = request.committed_source().lifetime();
                (lifetime.deadline, lifetime.expires_at.get())
            }
            SettingsReplyBuildRequest::Access(request) => {
                let source = request.verified_request().authority_source();
                (source.original_deadline(), source.intent().expires_at.get())
            }
            SettingsReplyBuildRequest::Snapshot(request) => {
                let source = request.read_owner().verified_request().authority_source();
                let super::super::super::super::read::SettingsReadRequest::Snapshot(intent) =
                    source.request()
                else {
                    return Err(Failure::Association);
                };
                (source.original_deadline(), intent.expires_at.get())
            }
            SettingsReplyBuildRequest::Inspection(request) => {
                let source = request.read_owner().verified_request().authority_source();
                let super::super::super::super::read::SettingsReadRequest::InspectOriginal(intent) =
                    source.request()
                else {
                    return Err(Failure::Association);
                };
                (source.original_deadline(), intent.expires_at.get())
            }
        };
        let clipping = expiry
            .checked_sub(payload.committed.header().expires_at_ms)
            .ok_or(Failure::Association)?;
        let deadline = deadline
            .checked_sub(std::time::Duration::from_millis(clipping))
            .ok_or(Failure::Association)?;
        Ok((
            Arc::clone(&payload.input.context.mutation),
            deadline,
            self.association.connection.cancellation(),
        ))
    }

    pub(in crate::devices) fn load_preparation_ciphertext(
        &self,
        mutation: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<crate::relay_protocol::RelayClientMessage, SettingsPreparationWorkerFailure> {
        use SettingsPreparationWorkerFailure as Failure;

        if !self.association.connection.is_live() {
            return Err(Failure::Stopped(SettingsPreparationStop::ConnectionClosed));
        }
        let owner = self.reply.as_ref().ok_or(Failure::Association)?;
        let payload = owner.settled_payload().map_err(Failure::Owner)?;
        let request = owner.request().map_err(Failure::Owner)?;
        let fresh;
        let authority = match request {
            SettingsReplyBuildRequest::Fresh(request) => {
                fresh = request.committed_source();
                fresh.receipt_authority()
            }
            SettingsReplyBuildRequest::Historical(request) => request.receipt_authority(),
            SettingsReplyBuildRequest::Bind { .. } => &payload.input.authority,
            SettingsReplyBuildRequest::Access(request) => {
                request.verified_request().receipt_authority()
            }
            SettingsReplyBuildRequest::Snapshot(request) => {
                request.read_owner().verified_request().receipt_authority()
            }
            SettingsReplyBuildRequest::Inspection(request) => {
                request.read_owner().verified_request().receipt_authority()
            }
        };
        let context = &payload.input.context;
        if context.endpoint.outbound_is_quarantined() {
            return Err(Failure::Stopped(SettingsPreparationStop::Closed));
        }
        context
            .agent
            .store()
            .load_private_settings_ciphertext(
                &context.endpoint,
                authority,
                mutation,
                &self.candidate,
                owner,
            )
            .map_err(Failure::Load)
    }
}

impl SettingsSendSource {
    fn socket_owner(
        &self,
    ) -> Result<
        (&OwnedQueuedSettingsReply, &QueuedReplyPayload),
        crate::host::settings::service::SettingsQueuedReplyCheckError,
    > {
        let owner = self
            .reply
            .as_ref()
            .ok_or(PairedHandoffReadError::Bookkeeping)?;
        let payload = owner.settled_payload()?;
        if !self.association.matches_payload(
            payload,
            &self.association.ledger,
            self.candidate.message_id,
        ) || self.candidate.recipient != payload.committed.header().recipient
            || !matches!(
                self.candidate.routing,
                TransportOutboundRouting::PrivateSettings
            )
        {
            return Err(PairedHandoffReadError::Bookkeeping.into());
        }
        Ok((owner, payload))
    }

    pub(in crate::devices) fn quarantine_socket_owner(&self) {
        self.association.quarantine();
    }

    pub(in crate::devices) fn recheck_socket_sql(
        &self,
        mutation: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), Box<crate::storage::SettingsSocketCheckFailure>> {
        use crate::storage::SettingsSocketCheckFailure;

        let (owner, payload) = self
            .socket_owner()
            .map_err(|cause| Box::new(SettingsSocketCheckFailure::BeforeTransaction { cause }))?;
        let request = owner.request().map_err(|cause| {
            Box::new(SettingsSocketCheckFailure::BeforeTransaction {
                cause: cause.into(),
            })
        })?;
        let fresh;
        let authority = match request {
            SettingsReplyBuildRequest::Fresh(request) => {
                fresh = request.committed_source();
                fresh.receipt_authority()
            }
            SettingsReplyBuildRequest::Historical(request) => request.receipt_authority(),
            SettingsReplyBuildRequest::Bind { .. } => &payload.input.authority,
            SettingsReplyBuildRequest::Access(request) => {
                request.verified_request().receipt_authority()
            }
            SettingsReplyBuildRequest::Snapshot(request) => {
                request.read_owner().verified_request().receipt_authority()
            }
            SettingsReplyBuildRequest::Inspection(request) => {
                request.read_owner().verified_request().receipt_authority()
            }
        };
        payload
            .input
            .context
            .agent
            .store()
            .recheck_private_settings_socket(authority, mutation, owner)
    }

    pub(in crate::devices) fn recheck_socket_lifetime(
        &self,
        mutation: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), crate::host::settings::service::SettingsQueuedReplyCheckError> {
        let (owner, payload) = self.socket_owner()?;
        let request = owner.request()?;
        let fresh;
        let authority = match request {
            SettingsReplyBuildRequest::Fresh(request) => {
                fresh = request.committed_source();
                fresh.receipt_authority()
            }
            SettingsReplyBuildRequest::Historical(request) => request.receipt_authority(),
            SettingsReplyBuildRequest::Bind { .. } => &payload.input.authority,
            SettingsReplyBuildRequest::Access(request) => {
                request.verified_request().receipt_authority()
            }
            SettingsReplyBuildRequest::Snapshot(request) => {
                request.read_owner().verified_request().receipt_authority()
            }
            SettingsReplyBuildRequest::Inspection(request) => {
                request.read_owner().verified_request().receipt_authority()
            }
        };
        authority.recheck_queued_socket_lifetime(
            &payload.input.context.agent.store(),
            mutation,
            owner,
        )
    }
}

impl PendingSettingsSend {
    pub(in crate::devices) fn message_id(&self) -> MessageId {
        self.association.message_id
    }

    pub(in crate::devices) fn matches_slot(&self, window: &Arc<()>, slot: &Arc<()>) -> bool {
        self.association.matches_slot(window, slot)
    }

    pub(in crate::devices) fn shares_socket(&self, socket: &Arc<RelaySocketIdentity>) -> bool {
        self.association.connection.shares_socket(socket)
    }

    pub(in crate::devices) fn matches_source(&self, source: &SettingsSendSource) -> bool {
        source.matches_association(&self.association)
    }
}

impl WorkSettingsSendLoan {
    pub(super) fn matches_registry(&self, ledger: &Arc<Ledger>, message_id: MessageId) -> bool {
        Arc::ptr_eq(&self.association.ledger, ledger) && self.association.message_id == message_id
    }
}

impl Drop for WorkSettingsSendLoan {
    fn drop(&mut self) {
        self.association.quarantine();
    }
}

impl ReturnedSettingsSend {
    pub(super) fn matches_payload(
        &self,
        payload: &QueuedReplyPayload,
        ledger: &Arc<Ledger>,
        message_id: MessageId,
    ) -> bool {
        self.loan
            .association
            .matches_payload(payload, ledger, message_id)
    }
}

impl SettingsReceiveWork {
    pub(in crate::devices) fn try_lend_queued_reply(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        window: &mut RelaySendWindow,
        candidate: TransportOutboundCandidate,
    ) -> Result<PendingSettingsSend, SettingsSendAdmissionError> {
        use SettingsSendAdmissionError as Admission;

        self.ledger.check_admission().map_err(Admission::Receive)?;
        if self.closed || self.failure.is_some() || !connection.is_live() {
            return Err(Admission::Receive(SettingsReceiveError::Closed));
        }
        if !self.report().bookkeeping_ok
            || !matches!(candidate.routing, TransportOutboundRouting::PrivateSettings)
            || self.history.as_ref().is_none_or(|history| {
                !Arc::ptr_eq(&history.connection, connection) || !history.healthy()
            })
        {
            return Err(Admission::Association);
        }
        let id = candidate.message_id;
        if !self.queued.entries.contains_key(&id) {
            self.latch_failure();
            return Err(Admission::MissingOwner);
        }
        let entry = self
            .queued
            .entries
            .get_mut(&id)
            .ok_or(Admission::MissingOwner)?;
        if matches!(entry, QueuedReplyEntry::StartedReturned(_)) {
            return Err(Admission::NoResend);
        }
        let QueuedReplyEntry::Owned { returned, reply } = entry else {
            return Err(Admission::Association);
        };
        if returned.is_some() {
            return Err(Admission::NoResend);
        }
        let owner = reply.as_ref().ok_or(Admission::Association)?;
        let payload = owner.settled_payload().map_err(Admission::Owner)?;
        let context = self.context.as_ref().ok_or(Admission::Association)?;
        let header = payload.committed.header();
        if !Arc::ptr_eq(context, &payload.input.context)
            || !owner
                .request()
                .map_err(Admission::Owner)?
                .matches_context(context, connection)
            || !Arc::ptr_eq(&payload.input.record.permit.ledger, &self.ledger)
            || header.message_id != id
            || header.recipient != candidate.recipient
            || !window.matches_endpoint(&context.endpoint)
        {
            return Err(Admission::Association);
        }
        let mut reserved = window.reserve_settings(owner, connection)?;
        let (window_identity, slot_identity) = reserved.identities();
        let association = Arc::new(SettingsSendLoanAssociation {
            message_id: id,
            ledger: Arc::clone(&self.ledger),
            connection: Arc::clone(connection),
            window: Arc::clone(window_identity),
            slot: Arc::clone(slot_identity),
            build_slot: Arc::clone(&payload.input.record.permit.slot),
            build_association: Arc::clone(&payload.input.record.permit.association),
        });
        reserved.associate(Arc::clone(&association));
        let witness = WorkSettingsSendLoan {
            association: Arc::clone(&association),
        };
        let ticket = PendingSettingsSend {
            association: Arc::clone(&association),
        };
        let owner = reply.take().ok_or(Admission::Association)?;
        reserved.install(Arc::new(SettingsSendSource {
            candidate,
            reply: Some(owner),
            association,
        }));
        *entry = QueuedReplyEntry::Loaned {
            witness: Some(witness),
        };
        Ok(ticket)
    }

    pub(in crate::devices) fn try_return_queued_reply(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        window: &mut RelaySendWindow,
        ticket: PendingSettingsSend,
    ) -> Result<(), SettingsSendReturnFailure> {
        match self.return_unstarted_reply(connection, window, &ticket) {
            Ok(()) => Ok(()),
            Err(reason) => Err(SettingsSendReturnFailure {
                reason,
                _ticket: ticket,
            }),
        }
    }

    fn return_unstarted_reply(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        window: &mut RelaySendWindow,
        ticket: &PendingSettingsSend,
    ) -> Result<(), SettingsSendReturnReason> {
        use SettingsSendReturnReason as Reason;

        let id = ticket.message_id();
        let entry = self
            .queued
            .entries
            .get_mut(&id)
            .ok_or(Reason::MissingLoan)?;
        let QueuedReplyEntry::Loaned { witness } = entry else {
            return Err(Reason::MissingLoan);
        };
        let loan = witness.as_ref().ok_or(Reason::MissingLoan)?;
        if !Arc::ptr_eq(&loan.association, &ticket.association)
            || !Arc::ptr_eq(&loan.association.connection, connection)
            || !Arc::ptr_eq(&loan.association.ledger, &self.ledger)
            || self
                .history
                .as_ref()
                .is_none_or(|history| !Arc::ptr_eq(&history.connection, connection))
        {
            return Err(Reason::Association);
        }
        let mut returning = window.settings_return(ticket)?;
        let source = returning.source().ok_or(Reason::Association)?;
        let owner = source.reply.as_ref().ok_or(Reason::Association)?;
        let payload = owner.payload.as_ref().ok_or(Reason::Association)?;
        if !Arc::ptr_eq(&source.association, &loan.association)
            || !loan.association.matches_payload(payload, &self.ledger, id)
            || self
                .context
                .as_ref()
                .is_none_or(|context| !Arc::ptr_eq(context, &payload.input.context))
        {
            return Err(Reason::Association);
        }
        let source = returning.source_mut().ok_or(Reason::Association)?;
        let source = Arc::get_mut(source).ok_or(Reason::SharedSource)?;
        let loan = witness.take().ok_or(Reason::MissingLoan)?;
        let Some(owner) = source.reply.take() else {
            *witness = Some(loan);
            return Err(Reason::Association);
        };
        *entry = QueuedReplyEntry::Owned {
            returned: Some(ReturnedSettingsSend { loan }),
            reply: Some(owner),
        };
        returning.complete();
        Ok(())
    }
}

impl fmt::Debug for PendingSettingsSend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingSettingsSend([retained custody])")
    }
}

impl fmt::Debug for SettingsSendAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsSendAdmissionError([retained cause])")
    }
}

impl fmt::Debug for SettingsSendReturnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsSendReturnFailure([retained custody])")
    }
}

impl fmt::Display for SettingsSendReturnFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Settings reply custody could not return")
    }
}

impl std::error::Error for SettingsSendReturnFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.reason)
    }
}

pub(in crate::devices) struct SettingsStartedReturnAdoption<'registry> {
    payload: &'registry StartedReturnPayload,
}

impl SettingsStartedReturnAdoption<'_> {
    pub(in crate::devices) fn matches_slot(
        &self,
        association: &Arc<SettingsSendLoanAssociation>,
        window: &Arc<()>,
        slot: &Arc<()>,
    ) -> bool {
        let payload = self.payload;
        payload.owner.owned_no_resend()
            && payload.owner.ticket_matches()
            && Arc::ptr_eq(&payload.loan.association, association)
            && association.matches_slot(window, slot)
            && StartedReturnedSettingsSend::payload_healthy(
                payload,
                &association.ledger,
                association.message_id,
            )
    }
}

pub(super) struct StartedReturnedSettingsSend {
    payload: Option<StartedReturnPayload>,
}

struct StartedReturnPayload {
    owner: OwnedStartedSettingsReturn,
    removed: Option<SettingsRemovedSendSlot>,
    disposition: Option<super::super::super::disposition::RetainedDisposition>,
    loan: WorkSettingsSendLoan,
}

struct AbandonedStartedReturnPayload {
    _cause: SettingsStartedReturnFailure,
    payload: StartedReturnPayload,
}

impl StartedReturnedSettingsSend {
    pub(super) fn healthy(&self, ledger: &Arc<Ledger>, message_id: MessageId) -> bool {
        self.payload
            .as_ref()
            .is_some_and(|payload| Self::payload_healthy(payload, ledger, message_id))
    }

    fn payload_healthy(
        payload: &StartedReturnPayload,
        ledger: &Arc<Ledger>,
        message_id: MessageId,
    ) -> bool {
        let Some(source) = payload.owner.source() else {
            return false;
        };
        let Some(queued) = source
            .reply
            .as_ref()
            .and_then(|owner| owner.payload.as_ref())
        else {
            return false;
        };
        payload
            .removed
            .as_ref()
            .is_none_or(|removed| removed.matches_source(source))
            && payload.owner.sql_input().is_none_or(|input| {
                payload
                    .removed
                    .as_ref()
                    .is_some_and(|removed| removed.matches_input(input))
                    && ReturnedSettingsReplySource::new(input).is_ok()
            })
            && Arc::ptr_eq(&source.association, &payload.loan.association)
            && payload
                .loan
                .association
                .matches_payload(queued, ledger, message_id)
            && source.candidate.message_id == message_id
            && source.candidate.recipient == queued.committed.header().recipient
            && matches!(
                source.candidate.routing,
                TransportOutboundRouting::PrivateSettings
            )
    }

    fn finish(
        &mut self,
        ledger: &Arc<Ledger>,
        message_id: MessageId,
    ) -> Result<(), SettingsStartedReturnFailure> {
        if !self.healthy(ledger, message_id) {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        }
        let payload = self
            .payload
            .as_mut()
            .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
        payload.owner.finish()?;
        if !payload.owner.ticket_matches() {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        }
        if !self.healthy(ledger, message_id) {
            return Err(SettingsStartedReturnFailure::ASSOCIATION);
        }
        Ok(())
    }
}

impl Drop for StartedReturnedSettingsSend {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            let retained = Box::leak(Box::new(AbandonedStartedReturnPayload {
                _cause: SettingsStartedReturnFailure::ASSOCIATION,
                payload,
            }));
            retained.payload.loan.association.quarantine();
        }
    }
}

impl SettingsReceiveWork {
    pub(in crate::devices) async fn return_started_settings_send(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        window: &mut RelaySendWindow,
        message_id: MessageId,
    ) -> Result<(), SettingsStartedReturnFailure> {
        let association = match self.queued.entries.get(&message_id) {
            Some(QueuedReplyEntry::Loaned {
                witness: Some(loan),
            }) if Arc::ptr_eq(&loan.association.connection, connection)
                && Arc::ptr_eq(&loan.association.ledger, &self.ledger)
                && self
                    .history
                    .as_ref()
                    .is_some_and(|history| Arc::ptr_eq(&history.connection, connection)) =>
            {
                Arc::clone(&loan.association)
            }
            _ => return Err(SettingsStartedReturnFailure::ASSOCIATION),
        };
        if let Err(error) = window.collect_started_settings_return(message_id).await {
            self.latch_failure();
            return Err(error);
        }
        let returned = (|| {
            let entry = self
                .queued
                .entries
                .get_mut(&message_id)
                .ok_or(SettingsStartedReturnFailure::MISSING)?;
            let QueuedReplyEntry::Loaned { witness } = entry else {
                return Err(SettingsStartedReturnFailure::ASSOCIATION);
            };
            if witness
                .as_ref()
                .is_none_or(|loan| !Arc::ptr_eq(&loan.association, &association))
            {
                return Err(SettingsStartedReturnFailure::ASSOCIATION);
            }
            let mut returning = window.started_settings_return(message_id, &association)?;
            let source = returning
                .source()
                .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
            let queued = source
                .reply
                .as_ref()
                .and_then(|owner| owner.payload.as_ref())
                .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
            if !Arc::ptr_eq(&source.association, &association)
                || !association.matches_payload(queued, &self.ledger, message_id)
                || self
                    .context
                    .as_ref()
                    .is_none_or(|context| !Arc::ptr_eq(context, &queued.input.context))
                || source.candidate.message_id != message_id
                || source.candidate.recipient != queued.committed.header().recipient
                || !matches!(
                    source.candidate.routing,
                    TransportOutboundRouting::PrivateSettings
                )
            {
                return Err(SettingsStartedReturnFailure::ASSOCIATION);
            }
            let loan = witness
                .take()
                .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
            let Some(owner) = returning.take_phase() else {
                *witness = Some(loan);
                return Err(SettingsStartedReturnFailure::ASSOCIATION);
            };
            *entry = QueuedReplyEntry::StartedReturned(Box::new(StartedReturnedSettingsSend {
                payload: Some(StartedReturnPayload {
                    owner,
                    removed: None,
                    disposition: None,
                    loan,
                }),
            }));
            let QueuedReplyEntry::StartedReturned(returned) = entry else {
                return Err(SettingsStartedReturnFailure::ASSOCIATION);
            };
            returned.finish(&self.ledger, message_id)?;
            let payload = returned
                .payload
                .as_mut()
                .ok_or(SettingsStartedReturnFailure::ASSOCIATION)?;
            let adoption = SettingsStartedReturnAdoption { payload };
            let removed = returning.complete(adoption)?;
            payload.removed = Some(removed);
            Ok(())
        })();
        if returned.is_err() {
            self.latch_failure();
        }
        returned
    }
}

pub(crate) struct ReturnedSettingsReplySource<'source> {
    input: &'source ReturnedSettingsSqlInput,
    request: &'source SettingsReplyBuildRequest,
    committed: &'source crate::storage::CommittedSettingsReply,
}

impl<'source> ReturnedSettingsReplySource<'source> {
    pub(in crate::devices::settings_ingress::work) fn new(
        input: &'source ReturnedSettingsSqlInput,
    ) -> Result<Self, PairedHandoffReadError> {
        let source = input.source();
        let owner = source
            .reply
            .as_ref()
            .ok_or(PairedHandoffReadError::Bookkeeping)?;
        let payload = owner
            .payload
            .as_ref()
            .ok_or(PairedHandoffReadError::Bookkeeping)?;
        if !source.association.matches_payload(
            payload,
            &source.association.ledger,
            source.candidate.message_id,
        ) || !source
            .association
            .matches_slot(input.window_identity(), input.slot_identity())
            || source.candidate.recipient != payload.committed.header().recipient
            || !matches!(
                source.candidate.routing,
                TransportOutboundRouting::PrivateSettings
            )
        {
            return Err(PairedHandoffReadError::Bookkeeping);
        }
        let request = payload.input.record.returned_reply_request()?;
        let permit = &payload.input.record.permit;
        let witness = request.witness();
        let joined = witness
            .build
            .as_ref()
            .ok_or(PairedHandoffReadError::Bookkeeping)?;
        if !request.matches_context(&payload.input.context, &source.association.connection)
            || !request.matches_association(&permit.association)
            || !Arc::ptr_eq(&witness.ledger, &permit.ledger)
            || witness.peer != permit.association.reply.peer_device_id
            || !Arc::ptr_eq(&joined.association, &permit.association)
            || joined.claim != super::super::super::ReplyBuildClaim::Claimed
        {
            return Err(PairedHandoffReadError::Bookkeeping);
        }
        Ok(Self {
            input,
            request,
            committed: &payload.committed,
        })
    }

    pub(crate) fn candidate(&self) -> &TransportOutboundCandidate {
        &self.input.source().candidate
    }

    pub(crate) fn committed(&self) -> &crate::storage::CommittedSettingsReply {
        self.committed
    }

    pub(crate) fn request(&self) -> &SettingsReplyBuildRequest {
        self.request
    }

    pub(crate) fn slot_identity(&self) -> &Arc<()> {
        self.input.slot_identity()
    }

    pub(crate) fn shares_store(&self, store: &crate::storage::Store) -> bool {
        self.input
            .source()
            .reply
            .as_ref()
            .and_then(|owner| owner.payload.as_ref())
            .is_some_and(|payload| {
                payload.input.context.endpoint.shares_store(store)
                    && payload.input.context.agent.store().shares_connection(store)
            })
    }
}

impl super::QueuedReplyRegistry {
    pub(in crate::devices::settings_ingress::work) fn disposition_input(
        &self,
        message_id: MessageId,
    ) -> Option<&Arc<ReturnedSettingsSqlInput>> {
        let QueuedReplyEntry::StartedReturned(record) = self.entries.get(&message_id)? else {
            return None;
        };
        record.payload.as_ref()?.owner.sql_input()
    }

    pub(in crate::devices::settings_ingress::work) fn disposition_association(
        &self,
        message_id: MessageId,
    ) -> Option<&Arc<SettingsSendLoanAssociation>> {
        let QueuedReplyEntry::StartedReturned(record) = self.entries.get(&message_id)? else {
            return None;
        };
        Some(&record.payload.as_ref()?.loan.association)
    }
}

impl SettingsReceiveWork {
    pub(in crate::devices) fn try_start_returned_settings_disposition(
        &mut self,
        connection: &Arc<SettingsConnectionIdentity>,
        message_id: MessageId,
    ) -> Result<(), SettingsReceiveError> {
        use super::super::super::disposition::ReturnedDispositionState;
        if self.disposition.is_some() {
            return Err(SettingsReceiveError::Capacity);
        }
        let counts = self.ledger.counts.lock().map_err(|_| self.ledger.fail())?;
        if !self.queued.healthy(&self.ledger, counts.private)
            || self.unexpected_join.is_some()
            || counts.jobs != self.receipts.len()
            || self
                .receipts
                .len()
                .checked_add(usize::from(self.reply_build_task().is_some()))
                != Some(self.jobs.len())
        {
            return Err(self.ledger.fail());
        }
        let Some(QueuedReplyEntry::StartedReturned(record)) =
            self.queued.entries.get_mut(&message_id)
        else {
            return Err(self.ledger.fail());
        };
        let payload = record.payload.as_mut().ok_or_else(|| self.ledger.fail())?;
        let removed = payload.removed.as_ref().ok_or_else(|| self.ledger.fail())?;
        let source = payload.owner.source().ok_or_else(|| self.ledger.fail())?;
        let queued = source
            .reply
            .as_ref()
            .and_then(|owner| owner.payload.as_ref())
            .ok_or_else(|| self.ledger.fail())?;
        if !payload.owner.owned_no_resend()
            || payload.disposition.is_some()
            || !payload.owner.ticket_matches()
            || !removed.matches_source(source)
            || !Arc::ptr_eq(&payload.loan.association.connection, connection)
            || self
                .history
                .as_ref()
                .is_none_or(|history| !Arc::ptr_eq(&history.connection, connection))
            || self
                .context
                .as_ref()
                .is_none_or(|context| !Arc::ptr_eq(context, &queued.input.context))
        {
            return Err(self.ledger.fail());
        }
        let mutation = Arc::clone(&queued.input.context.mutation);
        let association = Arc::clone(&payload.loan.association);
        payload
            .owner
            .loan_sql(removed)
            .map_err(|_| self.ledger.fail())?;
        let input = payload
            .owner
            .sql_input()
            .ok_or_else(|| self.ledger.fail())?;
        if !removed.matches_input(input) || ReturnedSettingsReplySource::new(input).is_err() {
            return Err(self.ledger.fail());
        }
        self.disposition = Some(ReturnedDispositionState::Waiting {
            message_id,
            association,
            waiting: Box::pin(mutation.lock_owned()),
        });
        drop(counts);
        Ok(())
    }
}

impl ReturnedSettingsReplySource<'_> {
    pub(in crate::devices::settings_ingress::work) fn store(
        &self,
    ) -> Option<crate::storage::Store> {
        self.input
            .source()
            .reply
            .as_ref()
            .and_then(|owner| owner.payload.as_ref())
            .map(|payload| payload.input.context.agent.store())
    }
}

impl super::QueuedReplyRegistry {
    pub(in crate::devices::settings_ingress::work) fn disposition_mutation(
        &self,
        message_id: MessageId,
    ) -> Option<&Arc<tokio::sync::Mutex<()>>> {
        let input = self.disposition_input(message_id)?;
        Some(
            &input
                .source()
                .reply
                .as_ref()?
                .payload
                .as_ref()?
                .input
                .context
                .mutation,
        )
    }

    pub(in crate::devices::settings_ingress::work) fn retain_disposition_join(
        &mut self,
        message_id: MessageId,
        expected: tokio::task::Id,
        joined: super::super::super::disposition::JoinedDispositionJob,
    ) -> Result<(), super::super::super::disposition::JoinedDispositionJob> {
        use super::super::super::disposition::RetainedDisposition;
        if let Some(QueuedReplyEntry::StartedReturned(record)) = self.entries.get_mut(&message_id)
            && let Some(payload) = record.payload.as_mut()
            && payload.disposition.is_none()
        {
            payload.disposition = Some(RetainedDisposition::Observed { expected, joined });
            Ok(())
        } else {
            Err(joined)
        }
    }

    pub(in crate::devices::settings_ingress::work) fn finish_disposition_join(
        &mut self,
        ledger: &Arc<Ledger>,
        message_id: MessageId,
    ) -> Result<(), SettingsReceiveError> {
        use super::super::super::disposition::{RetainedDisposition, RetainedDispositionOutcome};
        let Some(QueuedReplyEntry::StartedReturned(record)) = self.entries.get_mut(&message_id)
        else {
            return Err(ledger.fail());
        };
        let payload = record.payload.as_mut().ok_or_else(|| ledger.fail())?;
        let Some(RetainedDisposition::Observed { expected, joined }) = payload.disposition.as_ref()
        else {
            return Err(ledger.fail());
        };
        let input = payload.owner.sql_input().ok_or_else(|| ledger.fail())?;
        let removed = payload.removed.as_ref().ok_or_else(|| ledger.fail())?;
        if !removed.matches_input(input) {
            return Err(ledger.fail());
        }
        let status = match joined {
            Ok((actual, output)) => {
                if actual != expected || !Arc::ptr_eq(&output.input, input) {
                    return Err(ledger.fail());
                }
                match &output.outcome {
                    Ok(disposition) => {
                        let source =
                            ReturnedSettingsReplySource::new(input).map_err(|_| ledger.fail())?;
                        if !disposition.matches_source(&source) {
                            return Err(ledger.fail());
                        }
                        Ok(())
                    }
                    Err(_) => Err(SettingsReceiveError::Worker),
                }
            }
            Err(error) if error.id() == *expected => Err(SettingsReceiveError::Worker),
            Err(_) => return Err(ledger.fail()),
        };
        {
            let mut counts = ledger.counts.lock().map_err(|_| ledger.fail())?;
            counts.jobs = counts.jobs.checked_sub(1).ok_or_else(|| ledger.fail())?;
        }
        let Some(RetainedDisposition::Observed { joined, .. }) = payload.disposition.take() else {
            return Err(ledger.fail());
        };
        let outcome = match joined {
            Ok((_, output)) => {
                let super::super::super::disposition::ReturnedDispositionOutput { outcome, input } =
                    output;
                drop(input);
                RetainedDispositionOutcome::Worker { _outcome: outcome }
            }
            Err(cause) => RetainedDispositionOutcome::Join { _cause: cause },
        };
        let source_return = payload.owner.finish_sql_loan();
        let returned_ok = source_return.is_ok();
        payload.disposition = Some(RetainedDisposition::Finished {
            _outcome: outcome,
            _source_return: source_return,
        });
        if !returned_ok {
            return Err(ledger.fail());
        }
        status
    }
}
