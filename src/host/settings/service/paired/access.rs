use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::crypto::{DevicePublicKeys, EnvelopeHeader};
use crate::devices::{
    AccessStatusRejection, BindingGeneration, OwnedCommittedSettingsAccess,
    OwnedQueuedSettingsReply, OwnedVerifiedSettingsAccess, ReplyBuildPermit,
    SettingsAccessReplyPacket, SettingsReplyBuildRequest, SettingsReplyPublicationError,
    SettingsReplyQualification,
};
use crate::host::settings::{SettingsGrantHead, SettingsGrantReference};
use crate::protocol::{DeviceId, MessageId, UnixMillis};
use crate::storage::{ConsumedSettingsReply, Store, settings_grant_head_on};

use super::pins::ReplyPinError;
use super::replies::SettingsQueuedReplyCheckError;
use super::{
    SettingsGrantOwner, SettingsGrantState, SettingsReceiptAuthority, SettingsReplyProjectionError,
    SettingsServiceError, original_reply_text,
};

#[derive(Error)]
pub(crate) enum SettingsAccessQualificationError {
    #[error("Settings access current authority check failed")]
    Authority(#[source] Box<SettingsServiceError>),
    #[error("Settings access verified source check failed")]
    Source(#[from] AccessStatusRejection),
    #[error("Settings access metadata validation failed")]
    Metadata(#[source] serde_json::Error),
}

impl From<SettingsServiceError> for SettingsAccessQualificationError {
    fn from(error: SettingsServiceError) -> Self {
        Self::Authority(Box::new(error))
    }
}

impl From<ReplyPinError> for SettingsAccessQualificationError {
    fn from(error: ReplyPinError) -> Self {
        match error {
            ReplyPinError::Authority(cause) => Self::Authority(cause),
            ReplyPinError::Metadata(cause) => Self::Metadata(cause),
        }
    }
}

impl fmt::Debug for SettingsAccessQualificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAccessQualificationError([retained cause])")
    }
}

#[derive(PartialEq, Eq)]
enum AccessDecision {
    GrantUnavailable,
    Active(Box<SettingsGrantHead>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsAccessReplyFamily {
    Default,
    Denial,
}

pub(crate) struct SettingsAccessReservationSpec {
    local_device_id: DeviceId,
    peer_device_id: DeviceId,
    provenance: String,
    family: SettingsAccessReplyFamily,
    created_at: UnixMillis,
    expires_at: UnixMillis,
}

pub(crate) struct SettingsAccessReceiptPlan<'transaction, 'connection, 'request> {
    reservation: SettingsAccessReservationSpec,
    decision: AccessDecision,
    query: Arc<()>,
    invocation: Arc<()>,
    query_floor: (UnixMillis, Instant),
    query_deadline: Instant,
    reply_deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
    authority: &'request SettingsReceiptAuthority,
    request: &'request OwnedVerifiedSettingsAccess,
    transaction: &'transaction Transaction<'connection>,
}

struct AccessObservation {
    query: Arc<()>,
    query_floor: (UnixMillis, Instant),
    query_deadline: Instant,
    decision: AccessDecision,
    wall: UnixMillis,
    monotonic: Instant,
}

impl SettingsAccessReplyFamily {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Denial => "denial",
        }
    }

    pub(crate) fn reserved_bytes(self) -> usize {
        match self {
            Self::Default => 49_152,
            Self::Denial => 4_096,
        }
    }
}

impl SettingsAccessReservationSpec {
    pub(crate) fn local_device_id(&self) -> DeviceId {
        self.local_device_id
    }

    pub(crate) fn peer_device_id(&self) -> DeviceId {
        self.peer_device_id
    }

    pub(crate) fn provenance(&self) -> &str {
        &self.provenance
    }

    pub(crate) fn family(&self) -> SettingsAccessReplyFamily {
        self.family
    }

    pub(crate) fn created_at(&self) -> UnixMillis {
        self.created_at
    }

    pub(crate) fn expires_at(&self) -> UnixMillis {
        self.expires_at
    }
}

impl SettingsAccessReceiptPlan<'_, '_, '_> {
    pub(crate) fn reservation_spec(&self) -> &SettingsAccessReservationSpec {
        &self.reservation
    }

    fn check_time(
        &self,
        wall: UnixMillis,
        monotonic: Instant,
    ) -> Result<(), SettingsAccessQualificationError> {
        if wall < self.observed_wall || monotonic < self.observed_monotonic {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if wall >= self.reservation.expires_at || monotonic >= self.reply_deadline {
            return Err(SettingsServiceError::Expired.into());
        }
        Ok(())
    }
}

impl fmt::Debug for SettingsAccessReservationSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAccessReservationSpec([original query reservation])")
    }
}

impl fmt::Debug for SettingsAccessReceiptPlan<'_, '_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAccessReceiptPlan([transaction-local own status])")
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn check_access_receipt_on<'transaction, 'connection, 'request>(
        &'request self,
        store: &Store,
        transaction: &'transaction Transaction<'connection>,
        request: &'request OwnedVerifiedSettingsAccess,
    ) -> Result<
        SettingsAccessReceiptPlan<'transaction, 'connection, 'request>,
        SettingsAccessQualificationError,
    > {
        self.check_access_context(store, request)?;
        let source = request.authority_source();
        source.check_precommit(store)?;
        self.check_access_pins_on(transaction, request)?;
        self.core.authority.register_access_query(
            &self.host,
            source,
            &self.core.application_stop,
        )?;
        let observed = self.observe_access_on(store, transaction, request)?;
        let (reservation, reply_deadline) = access_reservation_spec(request, &observed)?;
        let mut plan = SettingsAccessReceiptPlan {
            reservation,
            decision: observed.decision,
            query: observed.query,
            invocation: Arc::clone(source.invocation_identity()),
            query_floor: observed.query_floor,
            query_deadline: observed.query_deadline,
            reply_deadline,
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
            authority: self,
            request,
            transaction,
        };
        let (wall, monotonic) = source.check_precommit(store)?;
        plan.check_time(wall, monotonic)?;
        plan.observed_wall = wall;
        plan.observed_monotonic = monotonic;
        Ok(plan)
    }

    pub(crate) fn recheck_access_receipt_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        plan: &SettingsAccessReceiptPlan<'_, '_, '_>,
    ) -> Result<(), SettingsAccessQualificationError> {
        if !std::ptr::eq(self, plan.authority)
            || !std::ptr::eq(transaction, plan.transaction)
            || !Arc::ptr_eq(
                &plan.invocation,
                plan.request.authority_source().invocation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let source = plan.request.authority_source();
        source.check_precommit(store)?;
        let observed = self.observe_access_on(store, transaction, plan.request)?;
        if !Arc::ptr_eq(&observed.query, &plan.query)
            || observed.query_floor != plan.query_floor
            || observed.query_deadline != plan.query_deadline
            || observed.decision != plan.decision
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        plan.check_time(observed.wall, observed.monotonic)?;
        let (wall, monotonic) = source.check_precommit(store)?;
        if wall < observed.wall || monotonic < observed.monotonic {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        plan.check_time(wall, monotonic)
    }

    fn check_access_context(
        &self,
        store: &Store,
        request: &OwnedVerifiedSettingsAccess,
    ) -> Result<(), SettingsAccessQualificationError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped.into());
        }
        let source = request.authority_source();
        if !std::ptr::eq(self, request.receipt_authority())
            || !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
            || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
            || !source.connection().is_live()
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        self.core.authority.paired_receipt_owner(&self.host)?;
        let floor = source.receipt_floor();
        let deadline = floor
            .1
            .checked_add(Duration::from_millis(
                source
                    .intent()
                    .expires_at
                    .get()
                    .checked_sub(floor.0.get())
                    .ok_or(SettingsServiceError::Expired)?,
            ))
            .ok_or(SettingsServiceError::Expired)?;
        if deadline != source.original_deadline() {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
    }

    fn observe_access_on(
        &self,
        store: &Store,
        connection: &Connection,
        request: &OwnedVerifiedSettingsAccess,
    ) -> Result<AccessObservation, SettingsAccessQualificationError> {
        self.check_access_context(store, request)?;
        let source = request.authority_source();
        self.check_access_pins_on(connection, request)?;
        let first = self.core.authority.observe_access_query(
            &self.host,
            source,
            &self.core.application_stop,
        )?;
        let head = settings_grant_head_on(
            connection,
            first.owner,
            source.verified_peer_keys().device_id,
        )
        .map_err(SettingsServiceError::from)?;
        let observed = self.core.authority.observe_access_query(
            &self.host,
            source,
            &self.core.application_stop,
        )?;
        if !Arc::ptr_eq(&first.query, &observed.query)
            || first.floor != observed.floor
            || first.deadline != observed.deadline
            || observed.wall < first.wall
            || observed.monotonic < first.monotonic
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let decision = match head {
            None => AccessDecision::GrantUnavailable,
            Some(head) => {
                if head.host_revoked || head.peer_revoked {
                    return Err(SettingsServiceError::OutsideScope.into());
                }
                if head.grant.change.peer_keys != *source.verified_peer_keys() {
                    return Err(SettingsServiceError::OutsideScope.into());
                }
                if observed.wall < head.grant.change.target.issued_at {
                    return Err(SettingsServiceError::OutsideScope.into());
                }
                if observed.wall >= head.grant.change.target.expires_at
                    || matches!(head.grant.change.target.state, SettingsGrantState::Revoked)
                {
                    AccessDecision::GrantUnavailable
                } else {
                    AccessDecision::Active(Box::new(head))
                }
            }
        };
        self.check_access_context(store, request)?;
        Ok(AccessObservation {
            query: observed.query,
            query_floor: observed.floor,
            query_deadline: observed.deadline,
            decision,
            wall: observed.wall,
            monotonic: observed.monotonic,
        })
    }

    fn check_access_pins_on(
        &self,
        connection: &Connection,
        request: &OwnedVerifiedSettingsAccess,
    ) -> Result<(), SettingsAccessQualificationError> {
        let source = request.authority_source();
        self.check_reply_pins_on(
            connection,
            source.verified_peer_keys(),
            source.pinned_identity(),
        )
        .map_err(SettingsAccessQualificationError::from)
    }
}

fn access_reservation_spec(
    request: &OwnedVerifiedSettingsAccess,
    observed: &AccessObservation,
) -> Result<(SettingsAccessReservationSpec, Instant), SettingsAccessQualificationError> {
    let source = request.authority_source();
    let intent = source.intent();
    let query_lifetime = intent
        .expires_at
        .get()
        .checked_sub(observed.query_floor.0.get())
        .ok_or(SettingsServiceError::Expired)?;
    if observed
        .query_floor
        .1
        .checked_add(Duration::from_millis(query_lifetime))
        != Some(observed.query_deadline)
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    let (family, grant, grant_expiry) = match &observed.decision {
        AccessDecision::GrantUnavailable => (SettingsAccessReplyFamily::Denial, None, None),
        AccessDecision::Active(head) => (
            SettingsAccessReplyFamily::Default,
            Some(SettingsGrantReference {
                grant_id: head.grant.change.target.grant_id,
                revision: head.grant.revision,
            }),
            Some(head.grant.change.target.expires_at),
        ),
    };
    let expires = observed
        .wall
        .get()
        .checked_add(60_000)
        .ok_or(SettingsServiceError::Expired)?
        .min(intent.expires_at.get());
    let expires_at = UnixMillis::try_from(
        grant_expiry.map_or(expires, |grant_expiry| expires.min(grant_expiry.get())),
    )
    .map_err(SettingsServiceError::from)?;
    let clip = intent
        .expires_at
        .get()
        .checked_sub(expires_at.get())
        .ok_or(SettingsServiceError::Expired)?;
    let reply_deadline = observed
        .query_deadline
        .min(source.original_deadline())
        .checked_sub(Duration::from_millis(clip))
        .ok_or(SettingsServiceError::Expired)?;
    if observed.wall >= expires_at || observed.monotonic >= reply_deadline {
        return Err(SettingsServiceError::Expired.into());
    }
    let provenance = serde_json::to_string(&AccessReplyProvenance {
        side: "host",
        version: 1,
        local_id: intent.host_id,
        peer_keys: source.verified_peer_keys(),
        host_epoch: source.connection().host_identity().host_epoch(),
        connection_epoch: source.connection().connection_epoch(),
        grant,
        parent_id: None,
    })
    .map_err(SettingsAccessQualificationError::Metadata)?;
    if provenance.len() > 2048 {
        return Err(SettingsServiceError::CorruptJournal.into());
    }
    Ok((
        SettingsAccessReservationSpec {
            local_device_id: intent.host_id,
            peer_device_id: source.verified_peer_keys().device_id,
            provenance,
            family,
            created_at: observed.wall,
            expires_at,
        },
        reply_deadline,
    ))
}

#[derive(Serialize)]
struct AccessReplyProvenance<'keys> {
    side: &'static str,
    version: u16,
    local_id: DeviceId,
    peer_keys: &'keys DevicePublicKeys,
    host_epoch: Uuid,
    connection_epoch: Uuid,
    grant: Option<SettingsGrantReference>,
    parent_id: Option<Uuid>,
}

pub(crate) struct QualifiedSettingsAccessReply<'scope> {
    authority: &'scope SettingsReceiptAuthority,
    request: &'scope OwnedCommittedSettingsAccess,
    mutation: &'scope OwnedMutexGuard<()>,
    query: Arc<()>,
    invocation: Arc<()>,
    decision: AccessDecision,
    query_floor: (UnixMillis, Instant),
    query_deadline: Instant,
    reply_deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

struct AccessReplyObservation {
    status: AccessObservation,
    reply_deadline: Instant,
}

struct AccessReplyRead {
    status: AccessObservation,
    owner: SettingsGrantOwner,
    reply_deadline: Instant,
}

pub(crate) struct SettingsAccessReplyProjection {
    plaintext: Zeroizing<Vec<u8>>,
    floor: (UnixMillis, Instant),
    deadline: Instant,
}

pub(super) struct AccessReplyComparison {
    query: Arc<()>,
    invocation: Arc<()>,
    family: SettingsAccessReplyFamily,
    grant: Option<SettingsGrantReference>,
    query_floor: (UnixMillis, Instant),
    query_deadline: Instant,
    reply_deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

impl SettingsAccessReplyProjection {
    pub(crate) fn into_parts(self) -> (Zeroizing<Vec<u8>>, (UnixMillis, Instant), Instant) {
        (self.plaintext, self.floor, self.deadline)
    }
}

impl fmt::Debug for SettingsAccessReplyProjection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAccessReplyProjection([owned private data])")
    }
}

impl fmt::Debug for AccessReplyComparison {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessReplyComparison([sealed association only])")
    }
}

impl fmt::Debug for QualifiedSettingsAccessReply<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedSettingsAccessReply([joined own-status read])")
    }
}

impl SettingsReceiptAuthority {
    pub(super) fn recheck_queued_access_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
        comparison: &AccessReplyComparison,
    ) -> Result<(), SettingsQueuedReplyCheckError> {
        let SettingsReplyBuildRequest::Access(request) = queued.request()? else {
            return Err(SettingsReplyPublicationError::Conflict.into());
        };
        let header = queued.committed()?.header();
        let read = self
            .read_access_reply_on(store, connection, mutation, request)
            .map_err(SettingsReplyProjectionError::from)?;
        check_queued_access_observation(
            comparison,
            &read.status,
            read.reply_deadline,
            request,
            header,
        )
        .map_err(SettingsReplyProjectionError::from)?;
        queued.request()?;
        queued.committed()?;
        let observed = self
            .finish_access_reply_observation(store, request, read)
            .map_err(SettingsReplyProjectionError::from)?;
        check_queued_access_observation(
            comparison,
            &observed.status,
            observed.reply_deadline,
            request,
            header,
        )
        .map_err(SettingsReplyProjectionError::from)?;
        Ok(())
    }

    pub(crate) fn qualify_access_reply_on<'scope>(
        &'scope self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &'scope OwnedMutexGuard<()>,
        request: &'scope OwnedCommittedSettingsAccess,
    ) -> Result<QualifiedSettingsAccessReply<'scope>, SettingsAccessQualificationError> {
        let observed = self.observe_access_reply_on(store, transaction, mutation, request)?;
        Ok(QualifiedSettingsAccessReply {
            authority: self,
            request,
            mutation,
            query: observed.status.query,
            invocation: Arc::clone(
                request
                    .verified_request()
                    .authority_source()
                    .invocation_identity(),
            ),
            decision: observed.status.decision,
            query_floor: observed.status.query_floor,
            query_deadline: observed.status.query_deadline,
            reply_deadline: observed.reply_deadline,
            observed_wall: observed.status.wall,
            observed_monotonic: observed.status.monotonic,
        })
    }

    pub(crate) fn recheck_access_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsAccessReply<'_>,
    ) -> Result<(), SettingsAccessQualificationError> {
        self.check_access_reply_association(qualified)?;
        let observed = self.observe_access_reply_on(
            store,
            transaction,
            qualified.mutation,
            qualified.request,
        )?;
        check_access_reply_observation(qualified, &observed)
    }

    pub(crate) fn project_access_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsAccessReply<'_>,
        permit: &ReplyBuildPermit,
    ) -> Result<SettingsAccessReplyProjection, SettingsReplyProjectionError> {
        permit.check_access(qualified.request)?;
        self.check_access_reply_association(qualified)?;
        let observed = self.observe_access_reply_on(
            store,
            transaction,
            qualified.mutation,
            qualified.request,
        )?;
        check_access_reply_observation(qualified, &observed)?;
        let source = qualified.request.verified_request().authority_source();
        let in_reply_to: MessageId = source
            .receipt_metadata()
            .map_err(SettingsAccessQualificationError::from)?
            .message_id
            .parse()
            .map_err(SettingsServiceError::from)?;
        let intent = source.intent();
        let packet = match &observed.status.decision {
            AccessDecision::GrantUnavailable => SettingsAccessReplyPacket::grant_unavailable(
                intent.host_id,
                in_reply_to,
                intent.query_id,
            )?,
            AccessDecision::Active(head) => SettingsAccessReplyPacket::from_grant_head(
                intent.host_id,
                in_reply_to,
                intent.query_id,
                BindingGeneration {
                    grant_id: head.grant.change.target.grant_id,
                    grant_revision: head.grant.revision,
                    host_epoch: source.connection().host_identity().host_epoch(),
                    connection_epoch: source.connection().connection_epoch(),
                },
                head,
            )?,
        };
        Ok(SettingsAccessReplyProjection {
            plaintext: packet.encode_checked()?,
            floor: (observed.status.wall, observed.status.monotonic),
            deadline: observed.reply_deadline,
        })
    }

    pub(super) fn recheck_consumed_access_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<(), SettingsReplyPublicationError> {
        consumed.check_on(transaction)?;
        let SettingsReplyQualification::Access(qualified) = consumed.sealed().qualification()
        else {
            return Err(SettingsReplyPublicationError::Conflict);
        };
        self.check_access_reply_association(qualified)
            .map_err(SettingsReplyProjectionError::from)?;
        let read = self
            .read_access_reply_on(store, transaction, qualified.mutation, qualified.request)
            .map_err(SettingsReplyProjectionError::from)?;
        let observed = self
            .finish_access_reply_observation(store, qualified.request, read)
            .map_err(SettingsReplyProjectionError::from)?;
        check_access_reply_observation(qualified, &observed)
            .map_err(SettingsReplyProjectionError::from)?;
        Ok(())
    }

    pub(super) fn capture_access_reply_comparison(
        &self,
        qualified: &QualifiedSettingsAccessReply<'_>,
    ) -> Result<AccessReplyComparison, SettingsAccessQualificationError> {
        self.check_access_reply_association(qualified)?;
        Ok(AccessReplyComparison {
            query: Arc::clone(&qualified.query),
            invocation: Arc::clone(&qualified.invocation),
            family: qualified.request.reply_reservation().family,
            grant: match &qualified.decision {
                AccessDecision::GrantUnavailable => None,
                AccessDecision::Active(head) => Some(SettingsGrantReference {
                    grant_id: head.grant.change.target.grant_id,
                    revision: head.grant.revision,
                }),
            },
            query_floor: qualified.query_floor,
            query_deadline: qualified.query_deadline,
            reply_deadline: qualified.reply_deadline,
            observed_wall: qualified.observed_wall,
            observed_monotonic: qualified.observed_monotonic,
        })
    }

    fn check_access_reply_association(
        &self,
        qualified: &QualifiedSettingsAccessReply<'_>,
    ) -> Result<(), SettingsAccessQualificationError> {
        if !std::ptr::eq(self, qualified.authority)
            || !Arc::ptr_eq(
                &qualified.invocation,
                qualified
                    .request
                    .verified_request()
                    .authority_source()
                    .invocation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
    }

    fn observe_access_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsAccess,
    ) -> Result<AccessReplyObservation, SettingsAccessQualificationError> {
        let read = self.read_access_reply_on(store, connection, mutation, request)?;
        check_access_reply_reservation(connection, request)?;
        self.finish_access_reply_observation(store, request, read)
    }

    fn read_access_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsAccess,
    ) -> Result<AccessReplyRead, SettingsAccessQualificationError> {
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.core.mutation) {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let verified = request.verified_request();
        let status = self.observe_access_on(store, connection, verified)?;
        let owner = self.core.authority.paired_receipt_owner(&self.host)?;
        let reply_deadline = check_access_reply_metadata(request, &status)?;
        Ok(AccessReplyRead {
            status,
            owner,
            reply_deadline,
        })
    }

    fn finish_access_reply_observation(
        &self,
        store: &Store,
        request: &OwnedCommittedSettingsAccess,
        read: AccessReplyRead,
    ) -> Result<AccessReplyObservation, SettingsAccessQualificationError> {
        let AccessReplyRead {
            status,
            owner,
            reply_deadline,
        } = read;
        let verified = request.verified_request();
        let observed = self.core.authority.observe_access_query(
            &self.host,
            verified.authority_source(),
            &self.core.application_stop,
        )?;
        if observed.owner != owner
            || !Arc::ptr_eq(&observed.query, &status.query)
            || observed.floor != status.query_floor
            || observed.deadline != status.query_deadline
            || observed.wall < status.wall
            || observed.monotonic < status.monotonic
            || observed.wall < request.reply_reservation().created_at
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if observed.wall >= request.reply_reservation().expires_at
            || observed.monotonic >= reply_deadline
            || matches!(
                &status.decision,
                AccessDecision::Active(head)
                    if observed.wall >= head.grant.change.target.expires_at
            )
        {
            return Err(SettingsServiceError::Expired.into());
        }
        self.check_access_context(store, verified)?;
        Ok(AccessReplyObservation {
            status: AccessObservation {
                query: observed.query,
                query_floor: observed.floor,
                query_deadline: observed.deadline,
                decision: status.decision,
                wall: observed.wall,
                monotonic: observed.monotonic,
            },
            reply_deadline,
        })
    }
}

fn check_queued_access_observation(
    comparison: &AccessReplyComparison,
    observed: &AccessObservation,
    reply_deadline: Instant,
    request: &OwnedCommittedSettingsAccess,
    header: &EnvelopeHeader,
) -> Result<(), SettingsAccessQualificationError> {
    let source = request.verified_request().authority_source();
    if !Arc::ptr_eq(&comparison.invocation, source.invocation_identity())
        || !Arc::ptr_eq(&comparison.query, &observed.query)
        || comparison.family != request.reply_reservation().family
        || comparison.query_floor != observed.query_floor
        || comparison.query_deadline != observed.query_deadline
        || comparison.reply_deadline != reply_deadline
        || observed.wall < comparison.observed_wall
        || observed.monotonic < comparison.observed_monotonic
        || observed.wall.get() < header.created_at_ms
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    match (
        &observed.decision,
        comparison.family,
        comparison.grant.as_ref(),
    ) {
        (AccessDecision::GrantUnavailable, SettingsAccessReplyFamily::Denial, None) => Ok(()),
        (AccessDecision::Active(head), SettingsAccessReplyFamily::Default, Some(grant))
            if grant.grant_id == head.grant.change.target.grant_id
                && grant.revision == head.grant.revision =>
        {
            Ok(())
        }
        _ => Err(SettingsServiceError::OutsideScope.into()),
    }
}

fn check_access_reply_observation(
    qualified: &QualifiedSettingsAccessReply<'_>,
    observed: &AccessReplyObservation,
) -> Result<(), SettingsAccessQualificationError> {
    if !Arc::ptr_eq(&observed.status.query, &qualified.query)
        || observed.status.query_floor != qualified.query_floor
        || observed.status.query_deadline != qualified.query_deadline
        || observed.reply_deadline != qualified.reply_deadline
        || observed.status.decision != qualified.decision
        || observed.status.wall < qualified.observed_wall
        || observed.status.monotonic < qualified.observed_monotonic
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    Ok(())
}

fn check_access_reply_metadata(
    request: &OwnedCommittedSettingsAccess,
    observed: &AccessObservation,
) -> Result<Instant, SettingsAccessQualificationError> {
    let source = request.verified_request().authority_source();
    let intent = source.intent();
    let reply = request.reply_reservation();
    let floor = source.receipt_floor();
    if reply.reservation_id.is_nil()
        || reply.local_device_id != intent.host_id
        || reply.peer_device_id != source.verified_peer_keys().device_id
        || reply.provenance.len() > 2048
        || reply.created_at < floor.0
        || reply.created_at < observed.query_floor.0
        || reply.created_at < intent.issued_at
        || reply.created_at > observed.wall
        || reply.created_at >= reply.expires_at
        || reply.created_at >= intent.expires_at
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    let query_lifetime = intent
        .expires_at
        .get()
        .checked_sub(observed.query_floor.0.get())
        .ok_or(SettingsServiceError::Expired)?;
    if observed
        .query_floor
        .1
        .checked_add(Duration::from_millis(query_lifetime))
        != Some(observed.query_deadline)
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    let (family, grant, grant_expiry) = match &observed.decision {
        AccessDecision::GrantUnavailable => (SettingsAccessReplyFamily::Denial, None, None),
        AccessDecision::Active(head) => {
            if reply.created_at < head.grant.change.target.issued_at {
                return Err(SettingsServiceError::OutsideScope.into());
            }
            (
                SettingsAccessReplyFamily::Default,
                Some(SettingsGrantReference {
                    grant_id: head.grant.change.target.grant_id,
                    revision: head.grant.revision,
                }),
                Some(head.grant.change.target.expires_at),
            )
        }
    };
    let expected_expiry = reply
        .created_at
        .get()
        .checked_add(60_000)
        .ok_or(SettingsServiceError::Expired)?
        .min(intent.expires_at.get());
    let expected_expiry =
        grant_expiry.map_or(expected_expiry, |expiry| expected_expiry.min(expiry.get()));
    let provenance = serde_json::to_string(&AccessReplyProvenance {
        side: "host",
        version: 1,
        local_id: intent.host_id,
        peer_keys: source.verified_peer_keys(),
        host_epoch: source.connection().host_identity().host_epoch(),
        connection_epoch: source.connection().connection_epoch(),
        grant,
        parent_id: None,
    })
    .map_err(SettingsAccessQualificationError::Metadata)?;
    if reply.family != family
        || reply.expires_at.get() != expected_expiry
        || reply.provenance != provenance
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    let clip = intent
        .expires_at
        .get()
        .checked_sub(reply.expires_at.get())
        .ok_or(SettingsServiceError::Expired)?;
    let deadline = observed
        .query_deadline
        .min(source.original_deadline())
        .checked_sub(Duration::from_millis(clip))
        .ok_or(SettingsServiceError::Expired)?;
    if observed.wall >= reply.expires_at || observed.monotonic >= deadline {
        return Err(SettingsServiceError::Expired.into());
    }
    Ok(deadline)
}

fn check_access_reply_reservation(
    connection: &Connection,
    request: &OwnedCommittedSettingsAccess,
) -> Result<(), SettingsAccessQualificationError> {
    let reply = request.reply_reservation();
    let reserved_bytes = i64::try_from(reply.family.reserved_bytes())
        .map_err(|_| SettingsServiceError::CorruptJournal)?;
    let created_at =
        i64::try_from(reply.created_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    let expires_at =
        i64::try_from(reply.expires_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1")
        .map_err(SettingsServiceError::from)?;
    let mut rows = statement
        .query([reply.reservation_id.to_string()])
        .map_err(SettingsServiceError::from)?;
    let row = rows
        .next()
        .map_err(SettingsServiceError::from)?
        .ok_or(SettingsServiceError::NotFound)?;
    if original_reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || original_reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || original_reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || original_reply_text(row, 3, 2048)? != reply.provenance
        || original_reply_text(row, 4, 7)? != reply.family.name()
        || row.get::<_, i64>(5).map_err(SettingsServiceError::from)? != reserved_bytes
        || row.get::<_, i64>(6).map_err(SettingsServiceError::from)? != created_at
        || row.get::<_, i64>(7).map_err(SettingsServiceError::from)? != expires_at
    {
        return Err(SettingsServiceError::CorruptJournal.into());
    }
    Ok(())
}
