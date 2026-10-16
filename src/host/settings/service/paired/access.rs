use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    AccessStatusRejection, DeviceAccess, DeviceError, OwnedVerifiedSettingsAccess,
};
use crate::host::settings::SettingsGrantHead;
use crate::protocol::UnixMillis;
use crate::storage::{StorageError, Store, settings_grant_head_on};

use super::{
    SettingsGrantState, SettingsReceiptAuthority, SettingsServiceError, original_reply_text,
};

const MAX_ACCESS_IDENTITY_BYTES: usize = 16 * 1024;

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

pub(crate) struct QualifiedSettingsAccess<'scope> {
    authority: &'scope SettingsReceiptAuthority,
    request: &'scope OwnedVerifiedSettingsAccess,
    mutation: &'scope OwnedMutexGuard<()>,
    query: Arc<()>,
    invocation: Arc<()>,
    decision: AccessDecision,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

struct AccessObservation {
    query: Arc<()>,
    decision: AccessDecision,
    wall: UnixMillis,
    monotonic: Instant,
}

impl fmt::Debug for QualifiedSettingsAccess<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedSettingsAccess([own current status])")
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn check_access_receipt_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        request: &OwnedVerifiedSettingsAccess,
    ) -> Result<(), SettingsAccessQualificationError> {
        self.check_access_context(store, request)?;
        let source = request.authority_source();
        source.check_precommit(store)?;
        self.check_access_pins_on(transaction, request)?;
        self.core.authority.register_access_query(
            &self.host,
            source,
            &self.core.application_stop,
        )?;
        self.observe_access_on(store, transaction, request)?;
        source.check_precommit(store)?;
        Ok(())
    }

    pub(crate) fn qualify_access_status_on<'scope>(
        &'scope self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &'scope OwnedMutexGuard<()>,
        request: &'scope OwnedVerifiedSettingsAccess,
    ) -> Result<QualifiedSettingsAccess<'scope>, SettingsAccessQualificationError> {
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.core.mutation) {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let observed = self.observe_access_on(store, transaction, request)?;
        Ok(QualifiedSettingsAccess {
            authority: self,
            request,
            mutation,
            query: observed.query,
            invocation: Arc::clone(request.authority_source().invocation_identity()),
            decision: observed.decision,
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
        })
    }

    pub(crate) fn recheck_access_status_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsAccess<'_>,
    ) -> Result<(), SettingsAccessQualificationError> {
        if !std::ptr::eq(self, qualified.authority)
            || !Arc::ptr_eq(
                OwnedMutexGuard::mutex(qualified.mutation),
                &self.core.mutation,
            )
            || !Arc::ptr_eq(
                &qualified.invocation,
                qualified.request.authority_source().invocation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let observed = self.observe_access_on(store, transaction, qualified.request)?;
        if !Arc::ptr_eq(&observed.query, &qualified.query)
            || observed.decision != qualified.decision
            || observed.wall < qualified.observed_wall
            || observed.monotonic < qualified.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
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
        let (owner, query, first_wall, first_monotonic) = self
            .core
            .authority
            .observe_access_query(&self.host, source, &self.core.application_stop)?;
        let head = settings_grant_head_on(connection, owner, source.verified_peer_keys().device_id)
            .map_err(SettingsServiceError::from)?;
        let (_, checked_query, wall, monotonic) = self.core.authority.observe_access_query(
            &self.host,
            source,
            &self.core.application_stop,
        )?;
        if !Arc::ptr_eq(&query, &checked_query) || wall < first_wall || monotonic < first_monotonic
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
                if wall < head.grant.change.target.issued_at {
                    return Err(SettingsServiceError::OutsideScope.into());
                }
                if wall >= head.grant.change.target.expires_at
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
            query,
            decision,
            wall,
            monotonic,
        })
    }

    fn check_access_pins_on(
        &self,
        connection: &Connection,
        request: &OwnedVerifiedSettingsAccess,
    ) -> Result<(), SettingsAccessQualificationError> {
        let mut statement = connection.prepare("SELECT device_id,configuration,revoked_at FROM transport_endpoint WHERE singleton=1")
            .map_err(SettingsServiceError::from)?;
        let mut rows = statement.query([]).map_err(SettingsServiceError::from)?;
        let row = rows
            .next()
            .map_err(SettingsServiceError::from)?
            .ok_or(SettingsServiceError::NotFound)?;
        let configuration: AccessEndpointConfiguration =
            serde_json::from_str(original_reply_text(row, 1, MAX_ACCESS_IDENTITY_BYTES)?)
                .map_err(SettingsAccessQualificationError::Metadata)?;
        if original_reply_text(row, 0, 36)? != self.core.authority.host_id.to_string()
            || configuration.version != 1
            || configuration.keys != *self.core.endpoint.public_keys()
            || !matches!(configuration.role, AccessEndpointRole::Host)
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if row
            .get::<_, Option<i64>>(2)
            .map_err(SettingsServiceError::from)?
            .is_some()
        {
            return Err(SettingsServiceError::Device(DeviceError::EndpointRevoked).into());
        }
        drop(rows);
        drop(statement);
        let source = request.authority_source();
        let mut statement = connection
            .prepare("SELECT public_identity,revoked_at FROM devices WHERE id=?1")
            .map_err(SettingsServiceError::from)?;
        let mut rows = statement
            .query([source.verified_peer_keys().device_id.to_string()])
            .map_err(SettingsServiceError::from)?;
        let row = rows
            .next()
            .map_err(SettingsServiceError::from)?
            .ok_or(SettingsServiceError::NotFound)?;
        let peer: AccessPinnedPeer =
            serde_json::from_str(original_reply_text(row, 0, MAX_ACCESS_IDENTITY_BYTES)?)
                .map_err(SettingsAccessQualificationError::Metadata)?;
        peer.keys
            .validate()
            .map_err(DeviceError::from)
            .map_err(SettingsServiceError::from)?;
        peer.access.validate().map_err(SettingsServiceError::from)?;
        if peer.version != 1
            || peer.keys != *source.verified_peer_keys()
            || serde_json::to_value(&peer).map_err(SettingsAccessQualificationError::Metadata)?
                != *source.pinned_identity()
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        if row
            .get::<_, Option<i64>>(1)
            .map_err(SettingsServiceError::from)?
            .is_some()
        {
            return Err(SettingsServiceError::Storage(StorageError::RevokedDevice(
                peer.keys.device_id.to_string(),
            ))
            .into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessEndpointConfiguration {
    version: u16,
    keys: DevicePublicKeys,
    role: AccessEndpointRole,
}

#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "snake_case", deny_unknown_fields)]
enum AccessEndpointRole {
    Host,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessPinnedPeer {
    version: u16,
    keys: DevicePublicKeys,
    access: DeviceAccess,
}
