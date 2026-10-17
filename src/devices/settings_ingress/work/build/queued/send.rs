use super::*;
use crate::devices::session::outbound::RelaySendWindow;
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
