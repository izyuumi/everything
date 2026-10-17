use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};
use thiserror::Error;

use crate::devices::{
    AuthenticatedSettingsReadSource, BindingLifetime, SettingsParentReference, SettingsReadRequest,
    SettingsReadSourceError,
};
use crate::host::settings::SettingsGrantReference;
use crate::host::settings::contract::SettingKey;
use crate::protocol::UnixMillis;
use crate::storage::{Store, settings_grant_head_on};

use super::pins::ReplyPinError;
use super::{
    SettingsGrantOwner, SettingsGrantState, SettingsReceiptAuthority, SettingsServiceError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsReadQueryDisposition {
    First,
    Known,
}

#[derive(Error)]
pub(crate) enum SettingsReadQualificationError {
    #[error("Settings read current authority check failed")]
    Authority(#[source] Box<SettingsServiceError>),
    #[error("Settings read verified source check failed")]
    Source(#[from] SettingsReadSourceError),
    #[error("Settings read pin metadata validation failed")]
    Metadata(#[source] serde_json::Error),
}

impl From<SettingsServiceError> for SettingsReadQualificationError {
    fn from(error: SettingsServiceError) -> Self {
        Self::Authority(Box::new(error))
    }
}

impl From<ReplyPinError> for SettingsReadQualificationError {
    fn from(error: ReplyPinError) -> Self {
        match error {
            ReplyPinError::Authority(cause) => Self::Authority(cause),
            ReplyPinError::Metadata(cause) => Self::Metadata(cause),
        }
    }
}

pub(crate) struct QualifiedSettingsReadQuery<'transaction, 'connection, 'source> {
    authority: &'source SettingsReceiptAuthority,
    source: &'source AuthenticatedSettingsReadSource,
    transaction: &'transaction Transaction<'connection>,
    invocation: Arc<()>,
    query: Arc<()>,
    disposition: SettingsReadQueryDisposition,
    owner: SettingsGrantOwner,
    floor: (UnixMillis, Instant),
    query_deadline: Instant,
    parent_lifetime: BindingLifetime,
    grant: ReadGrantFacts,
    expires_at: UnixMillis,
    deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

#[derive(PartialEq, Eq)]
struct ReadGrantFacts {
    reference: SettingsGrantReference,
    issued_at: UnixMillis,
    expires_at: UnixMillis,
    readable_keys: Vec<SettingKey>,
}

struct ReadObservation {
    owner: SettingsGrantOwner,
    query: Arc<()>,
    floor: (UnixMillis, Instant),
    query_deadline: Instant,
    parent_lifetime: BindingLifetime,
    grant: ReadGrantFacts,
    wall: UnixMillis,
    monotonic: Instant,
}

impl QualifiedSettingsReadQuery<'_, '_, '_> {
    pub(crate) fn disposition(&self) -> SettingsReadQueryDisposition {
        self.disposition
    }

    fn check_time(
        &self,
        wall: UnixMillis,
        monotonic: Instant,
    ) -> Result<(), SettingsReadQualificationError> {
        if wall < self.observed_wall || monotonic < self.observed_monotonic {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if wall >= self.expires_at || monotonic >= self.deadline {
            return Err(SettingsServiceError::Expired.into());
        }
        Ok(())
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn qualify_read_query_on<'transaction, 'connection, 'source>(
        &'source self,
        store: &Store,
        transaction: &'transaction Transaction<'connection>,
        source: &'source AuthenticatedSettingsReadSource,
    ) -> Result<
        QualifiedSettingsReadQuery<'transaction, 'connection, 'source>,
        SettingsReadQualificationError,
    > {
        self.check_read_context(store, source)?;
        source.check_precommit(store)?;
        self.check_reply_pins_on(
            transaction,
            source.verified_peer_keys(),
            source.pinned_identity(),
        )?;
        let registration = self.core.authority.register_read_query(
            &self.host,
            store,
            source,
            &self.core.application_stop,
        )?;
        let observed = self.observe_read_on(store, transaction, source)?;
        if !Arc::ptr_eq(&registration.query, &observed.query) {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let (expires_at, deadline) = read_window(source, &observed)?;
        let qualified = QualifiedSettingsReadQuery {
            authority: self,
            source,
            transaction,
            invocation: Arc::clone(source.invocation_identity()),
            query: observed.query,
            disposition: registration.disposition,
            owner: observed.owner,
            floor: observed.floor,
            query_deadline: observed.query_deadline,
            parent_lifetime: observed.parent_lifetime,
            grant: observed.grant,
            expires_at,
            deadline,
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
        };
        qualified.check_time(observed.wall, observed.monotonic)?;
        Ok(qualified)
    }

    pub(crate) fn recheck_read_query_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsReadQuery<'_, '_, '_>,
    ) -> Result<(), SettingsReadQualificationError> {
        if !std::ptr::eq(self, qualified.authority)
            || !std::ptr::eq(transaction, qualified.transaction)
            || !Arc::ptr_eq(
                &qualified.invocation,
                qualified.source.invocation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let observed = self.observe_read_on(store, transaction, qualified.source)?;
        if observed.owner != qualified.owner
            || !Arc::ptr_eq(&observed.query, &qualified.query)
            || observed.floor != qualified.floor
            || observed.query_deadline != qualified.query_deadline
            || observed.parent_lifetime.expires_at != qualified.parent_lifetime.expires_at
            || observed.parent_lifetime.deadline != qualified.parent_lifetime.deadline
            || observed.grant != qualified.grant
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let (expires_at, deadline) = read_window(qualified.source, &observed)?;
        if expires_at != qualified.expires_at || deadline != qualified.deadline {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        qualified.check_time(observed.wall, observed.monotonic)
    }

    fn check_read_context(
        &self,
        store: &Store,
        source: &AuthenticatedSettingsReadSource,
    ) -> Result<(), SettingsReadQualificationError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped.into());
        }
        if !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
            || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
            || !source.connection().is_live()
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        self.core.authority.paired_receipt_owner(&self.host)?;
        let (_, _, expires_at) = read_request_parts(source.request());
        if deadline_from_floor(source.receipt_floor(), expires_at)? != source.original_deadline() {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
    }

    fn observe_read_on(
        &self,
        store: &Store,
        connection: &Connection,
        source: &AuthenticatedSettingsReadSource,
    ) -> Result<ReadObservation, SettingsReadQualificationError> {
        self.check_read_context(store, source)?;
        self.check_reply_pins_on(
            connection,
            source.verified_peer_keys(),
            source.pinned_identity(),
        )?;
        let first = self.core.authority.observe_read_query(
            &self.host,
            store,
            source,
            &self.core.application_stop,
        )?;
        let head = settings_grant_head_on(
            connection,
            first.owner,
            source.verified_peer_keys().device_id,
        )
        .map_err(SettingsServiceError::from)?
        .ok_or(SettingsServiceError::OutsideScope)?;
        let observed = self.core.authority.observe_read_query(
            &self.host,
            store,
            source,
            &self.core.application_stop,
        )?;
        if first.owner != observed.owner
            || !Arc::ptr_eq(&first.query, &observed.query)
            || first.floor != observed.floor
            || first.deadline != observed.deadline
            || first.parent_lifetime.expires_at != observed.parent_lifetime.expires_at
            || first.parent_lifetime.deadline != observed.parent_lifetime.deadline
            || observed.wall < first.wall
            || observed.monotonic < first.monotonic
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let (parent, _, _) = read_request_parts(source.request());
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
            || observed.wall >= head.grant.change.target.expires_at
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if let SettingsReadRequest::Snapshot(intent) = source.request()
            && intent.keys.iter().any(|key| !readable_keys.contains(key))
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(ReadObservation {
            owner: observed.owner,
            query: observed.query,
            floor: observed.floor,
            query_deadline: observed.deadline,
            parent_lifetime: observed.parent_lifetime,
            grant: ReadGrantFacts {
                reference: SettingsGrantReference {
                    grant_id: head.grant.change.target.grant_id,
                    revision: head.grant.revision,
                },
                issued_at: head.grant.change.target.issued_at,
                expires_at: head.grant.change.target.expires_at,
                readable_keys: readable_keys.clone(),
            },
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

fn read_window(
    source: &AuthenticatedSettingsReadSource,
    observed: &ReadObservation,
) -> Result<(UnixMillis, Instant), SettingsReadQualificationError> {
    let (_, _, expires_at) = read_request_parts(source.request());
    if deadline_from_floor(observed.floor, expires_at)? != observed.query_deadline {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    let clipped = expires_at
        .min(observed.parent_lifetime.expires_at)
        .min(observed.grant.expires_at);
    let clip = Duration::from_millis(
        expires_at
            .get()
            .checked_sub(clipped.get())
            .ok_or(SettingsServiceError::Expired)?,
    );
    let deadline = observed
        .query_deadline
        .checked_sub(clip)
        .ok_or(SettingsServiceError::Expired)?
        .min(
            source
                .original_deadline()
                .checked_sub(clip)
                .ok_or(SettingsServiceError::Expired)?,
        )
        .min(observed.parent_lifetime.deadline);
    if observed.wall >= clipped || observed.monotonic >= deadline {
        return Err(SettingsServiceError::Expired.into());
    }
    Ok((clipped, deadline))
}

fn read_request_parts(
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

impl fmt::Debug for SettingsReadQualificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReadQualificationError([retained cause])")
    }
}

impl fmt::Debug for QualifiedSettingsReadQuery<'_, '_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedSettingsReadQuery([current query only])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsReadReplyFamily {
    Default,
    Inspection,
}

impl SettingsReadReplyFamily {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Inspection => "inspection",
        }
    }

    pub(crate) fn reserved_bytes(self) -> usize {
        match self {
            Self::Default => 49_152,
            Self::Inspection => 90_112,
        }
    }
}

pub(crate) struct SettingsReadReservationSpec<'query> {
    source: &'query AuthenticatedSettingsReadSource,
    family: SettingsReadReplyFamily,
    created_at: UnixMillis,
    expires_at: UnixMillis,
}

impl QualifiedSettingsReadQuery<'_, '_, '_> {
    pub(crate) fn reply_reservation_spec(&self) -> Option<SettingsReadReservationSpec<'_>> {
        if self.disposition != SettingsReadQueryDisposition::First {
            return None;
        }
        Some(SettingsReadReservationSpec {
            source: self.source,
            family: match self.source.request() {
                SettingsReadRequest::Snapshot(_) => SettingsReadReplyFamily::Default,
                SettingsReadRequest::InspectOriginal(_) => SettingsReadReplyFamily::Inspection,
            },
            created_at: self.observed_wall,
            expires_at: self.expires_at,
        })
    }
}

impl SettingsReadReservationSpec<'_> {
    pub(crate) fn local_device_id(&self) -> crate::protocol::DeviceId {
        match self.source.request() {
            SettingsReadRequest::Snapshot(intent) => intent.host_id,
            SettingsReadRequest::InspectOriginal(intent) => intent.host_id,
        }
    }

    pub(crate) fn peer_device_id(&self) -> crate::protocol::DeviceId {
        self.source.verified_peer_keys().device_id
    }

    pub(crate) fn verified_peer_keys(&self) -> &crate::crypto::DevicePublicKeys {
        self.source.verified_peer_keys()
    }

    pub(crate) fn parent(&self) -> &SettingsParentReference {
        read_request_parts(self.source.request()).0
    }

    pub(crate) fn family(&self) -> SettingsReadReplyFamily {
        self.family
    }

    pub(crate) fn created_at(&self) -> UnixMillis {
        self.created_at
    }

    pub(crate) fn expires_at(&self) -> UnixMillis {
        self.expires_at
    }
}
