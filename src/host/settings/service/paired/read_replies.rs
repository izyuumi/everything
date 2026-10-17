use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    BindingLifetime, EndpointRole, OwnedCommittedSettingsRead, SettingsParentReference,
    SettingsReadRequest,
};
use crate::host::settings::SettingsGrantReference;
use crate::host::settings::contract::SettingKey;
use crate::protocol::{DeviceId, UnixMillis};
use crate::storage::{Store, settings_grant_head_on};

use super::pins::ReplyPinError;
use super::{
    SettingsGrantOwner, SettingsGrantState, SettingsReadReplyFamily, SettingsReceiptAuthority,
    SettingsServiceError, original_reply_text,
};

#[derive(Error)]
pub(crate) enum SettingsReadReplyError {
    #[error("Settings Read reply current authority check failed")]
    Authority(#[source] Box<SettingsServiceError>),
    #[error("Settings Read reply metadata validation failed")]
    Metadata(#[source] serde_json::Error),
}

impl From<SettingsServiceError> for SettingsReadReplyError {
    fn from(error: SettingsServiceError) -> Self {
        Self::Authority(Box::new(error))
    }
}

impl From<ReplyPinError> for SettingsReadReplyError {
    fn from(error: ReplyPinError) -> Self {
        match error {
            ReplyPinError::Authority(cause) => Self::Authority(cause),
            ReplyPinError::Metadata(cause) => Self::Metadata(cause),
        }
    }
}

pub(crate) struct QualifiedSettingsReadReply<'transaction, 'connection, 'owner> {
    authority: &'owner SettingsReceiptAuthority,
    request: &'owner OwnedCommittedSettingsRead,
    transaction: &'transaction Transaction<'connection>,
    mutation: &'owner OwnedMutexGuard<()>,
    invocation: Arc<()>,
    owner: SettingsGrantOwner,
    query: Arc<()>,
    floor: (UnixMillis, Instant),
    query_deadline: Instant,
    parent_lifetime: BindingLifetime,
    grant: ReadReplyGrantFacts,
    reply_deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

#[derive(PartialEq, Eq)]
struct ReadReplyGrantFacts {
    reference: SettingsGrantReference,
    issued_at: UnixMillis,
    expires_at: UnixMillis,
    readable_keys: Vec<SettingKey>,
}

struct ReadReplyObservation {
    owner: SettingsGrantOwner,
    query: Arc<()>,
    floor: (UnixMillis, Instant),
    query_deadline: Instant,
    parent_lifetime: BindingLifetime,
    grant: ReadReplyGrantFacts,
    reply_deadline: Instant,
    wall: UnixMillis,
    monotonic: Instant,
}

impl SettingsReceiptAuthority {
    pub(crate) fn qualify_read_reply_on<'transaction, 'connection, 'owner>(
        &'owner self,
        store: &Store,
        transaction: &'transaction Transaction<'connection>,
        mutation: &'owner OwnedMutexGuard<()>,
        request: &'owner OwnedCommittedSettingsRead,
    ) -> Result<QualifiedSettingsReadReply<'transaction, 'connection, 'owner>, SettingsReadReplyError>
    {
        let observed = self.observe_read_reply_on(store, transaction, mutation, request)?;
        Ok(QualifiedSettingsReadReply {
            authority: self,
            request,
            transaction,
            mutation,
            invocation: Arc::clone(
                request
                    .verified_request()
                    .authority_source()
                    .invocation_identity(),
            ),
            owner: observed.owner,
            query: observed.query,
            floor: observed.floor,
            query_deadline: observed.query_deadline,
            parent_lifetime: observed.parent_lifetime,
            grant: observed.grant,
            reply_deadline: observed.reply_deadline,
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
        })
    }

    pub(crate) fn recheck_read_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsReadReply<'_, '_, '_>,
    ) -> Result<(), SettingsReadReplyError> {
        if !std::ptr::eq(self, qualified.authority)
            || !std::ptr::eq(transaction, qualified.transaction)
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
        let observed =
            self.observe_read_reply_on(store, transaction, qualified.mutation, qualified.request)?;
        if observed.owner != qualified.owner
            || !Arc::ptr_eq(&observed.query, &qualified.query)
            || observed.floor != qualified.floor
            || observed.query_deadline != qualified.query_deadline
            || observed.parent_lifetime.expires_at != qualified.parent_lifetime.expires_at
            || observed.parent_lifetime.deadline != qualified.parent_lifetime.deadline
            || observed.grant != qualified.grant
            || observed.reply_deadline != qualified.reply_deadline
            || observed.wall < qualified.observed_wall
            || observed.monotonic < qualified.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
    }

    fn check_joined_read_context(
        &self,
        store: &Store,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsRead,
    ) -> Result<(), SettingsReadReplyError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped.into());
        }
        store
            .check_connection_health()
            .map_err(SettingsServiceError::from)?;
        let verified = request.verified_request();
        let source = verified.authority_source();
        if !std::ptr::eq(self, verified.receipt_authority())
            || !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.core.mutation)
            || !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
            || self.core.endpoint.role() != &EndpointRole::Host
            || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
            || !source.connection().is_live()
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        self.core.authority.paired_receipt_owner(&self.host)?;
        let (_, _, expires_at) = request_parts(source.request());
        if deadline_from_floor(source.receipt_floor(), expires_at)? != source.original_deadline() {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
    }

    fn observe_read_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsRead,
    ) -> Result<ReadReplyObservation, SettingsReadReplyError> {
        self.check_joined_read_context(store, mutation, request)?;
        let source = request.verified_request().authority_source();
        self.check_reply_pins_on(
            connection,
            source.verified_peer_keys(),
            source.pinned_identity(),
        )?;
        let first = self.core.authority.observe_committed_read_query(
            &self.host,
            request,
            &self.core.application_stop,
        )?;
        let head = settings_grant_head_on(
            connection,
            first.owner,
            source.verified_peer_keys().device_id,
        )
        .map_err(SettingsServiceError::from)?
        .ok_or(SettingsServiceError::OutsideScope)?;
        let (parent, issued_at, expires_at) = request_parts(source.request());
        let SettingsGrantState::Active { readable_keys, .. } = &head.grant.change.target.state
        else {
            return Err(SettingsServiceError::OutsideScope.into());
        };
        if head.host_revoked
            || head.peer_revoked
            || head.grant.change.peer_keys != *source.verified_peer_keys()
            || head.grant.revision != parent.generation.grant_revision
            || head.grant.change.target.grant_id != parent.generation.grant_id
            || first.wall < head.grant.change.target.issued_at
            || first.wall >= head.grant.change.target.expires_at
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if let SettingsReadRequest::Snapshot(intent) = source.request()
            && intent.keys.iter().any(|key| !readable_keys.contains(key))
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let grant = ReadReplyGrantFacts {
            reference: SettingsGrantReference {
                grant_id: head.grant.change.target.grant_id,
                revision: head.grant.revision,
            },
            issued_at: head.grant.change.target.issued_at,
            expires_at: head.grant.change.target.expires_at,
            readable_keys: readable_keys.clone(),
        };
        let reply = request.reply_reservation();
        let family = match source.request() {
            SettingsReadRequest::Snapshot(_) => SettingsReadReplyFamily::Default,
            SettingsReadRequest::InspectOriginal(_) => SettingsReadReplyFamily::Inspection,
        };
        let expected_expiry = expires_at
            .min(first.parent_lifetime.expires_at)
            .min(grant.expires_at);
        if deadline_from_floor(first.floor, expires_at)? != first.deadline {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let clip = Duration::from_millis(
            expires_at
                .get()
                .checked_sub(expected_expiry.get())
                .ok_or(SettingsServiceError::Expired)?,
        );
        let reply_deadline = first
            .deadline
            .checked_sub(clip)
            .ok_or(SettingsServiceError::Expired)?
            .min(
                source
                    .original_deadline()
                    .checked_sub(clip)
                    .ok_or(SettingsServiceError::Expired)?,
            )
            .min(first.parent_lifetime.deadline);
        let expected_provenance = serde_json::to_string(&ReadReplyProvenance {
            side: "host",
            version: 1,
            local_id: source.connection().host_identity().host_id(),
            peer_keys: source.verified_peer_keys(),
            host_epoch: parent.generation.host_epoch,
            connection_epoch: parent.generation.connection_epoch,
            grant: grant.reference,
            parent_id: Some(parent.parent_id),
        })
        .map_err(SettingsReadReplyError::Metadata)?;
        if reply.reservation_id.is_nil()
            || reply.local_device_id != source.connection().host_identity().host_id()
            || reply.peer_device_id != source.verified_peer_keys().device_id
            || reply.family != family
            || reply.provenance.len() > 2048
            || reply.provenance != expected_provenance
            || reply.created_at < source.receipt_floor().0
            || reply.created_at < first.floor.0
            || reply.created_at < issued_at
            || reply.created_at < grant.issued_at
            || reply.created_at > first.wall
            || reply.created_at >= reply.expires_at
            || reply.expires_at != expected_expiry
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        check_read_reply_reservation(connection, request)?;
        self.check_joined_read_context(store, mutation, request)?;
        let observed = self.core.authority.observe_committed_read_query(
            &self.host,
            request,
            &self.core.application_stop,
        )?;
        if observed.owner != first.owner
            || !Arc::ptr_eq(&observed.query, &first.query)
            || observed.floor != first.floor
            || observed.deadline != first.deadline
            || observed.parent_lifetime.expires_at != first.parent_lifetime.expires_at
            || observed.parent_lifetime.deadline != first.parent_lifetime.deadline
            || observed.wall < first.wall
            || observed.monotonic < first.monotonic
            || observed.wall < reply.created_at
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if observed.wall >= reply.expires_at
            || observed.monotonic >= reply_deadline
            || observed.wall >= grant.expires_at
        {
            return Err(SettingsServiceError::Expired.into());
        }
        Ok(ReadReplyObservation {
            owner: observed.owner,
            query: observed.query,
            floor: observed.floor,
            query_deadline: observed.deadline,
            parent_lifetime: observed.parent_lifetime,
            grant,
            reply_deadline,
            wall: observed.wall,
            monotonic: observed.monotonic,
        })
    }
}

fn deadline_from_floor(
    floor: (UnixMillis, Instant),
    expires_at: UnixMillis,
) -> Result<Instant, SettingsServiceError> {
    let remaining = expires_at
        .get()
        .checked_sub(floor.0.get())
        .ok_or(SettingsServiceError::Expired)?;
    floor
        .1
        .checked_add(Duration::from_millis(remaining))
        .ok_or(SettingsServiceError::Expired)
}

fn request_parts(
    request: &SettingsReadRequest,
) -> (&SettingsParentReference, UnixMillis, UnixMillis) {
    match request {
        SettingsReadRequest::Snapshot(intent) => {
            (&intent.parent, intent.issued_at, intent.expires_at)
        }
        SettingsReadRequest::InspectOriginal(intent) => {
            (&intent.parent, intent.issued_at, intent.expires_at)
        }
    }
}

#[derive(Serialize)]
struct ReadReplyProvenance<'keys> {
    side: &'static str,
    version: u16,
    local_id: DeviceId,
    peer_keys: &'keys DevicePublicKeys,
    host_epoch: Uuid,
    connection_epoch: Uuid,
    grant: SettingsGrantReference,
    parent_id: Option<Uuid>,
}

fn check_read_reply_reservation(
    connection: &Connection,
    request: &OwnedCommittedSettingsRead,
) -> Result<(), SettingsServiceError> {
    let reply = request.reply_reservation();
    let created_at =
        i64::try_from(reply.created_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    let expires_at =
        i64::try_from(reply.expires_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    let reserved_bytes = i64::try_from(reply.family.reserved_bytes())
        .map_err(|_| SettingsServiceError::CorruptJournal)?;
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1")?;
    let mut rows = statement.query([reply.reservation_id.to_string()])?;
    let row = rows.next()?.ok_or(SettingsServiceError::NotFound)?;
    if original_reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || original_reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || original_reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || original_reply_text(row, 3, 2048)? != reply.provenance
        || original_reply_text(row, 4, 10)? != reply.family.name()
        || row.get::<_, i64>(5)? != reserved_bytes
        || row.get::<_, i64>(6)? != created_at
        || row.get::<_, i64>(7)? != expires_at
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

impl fmt::Debug for SettingsReadReplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReadReplyError([retained cause])")
    }
}

impl fmt::Debug for QualifiedSettingsReadReply<'_, '_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedSettingsReadReply([joined currentness only])")
    }
}
