use std::fmt;
use std::sync::Arc;

use rusqlite::TransactionBehavior;

use crate::devices::{BindRejection, HostSettingsTransport, OwnedCommittedSettingsBind};
use crate::protocol::UnixMillis;
use crate::storage::settings_grant_head_on;

use super::super::grants::{SettingsGrantOwner, SettingsGrantState};
use super::{ServiceCore, SettingsService, SettingsServiceError};

pub(crate) use super::super::authority::PairedSettingsParent;

pub(crate) struct PairedParentAdmission {
    request: OwnedCommittedSettingsBind,
    parent: Result<PairedSettingsParent, SettingsServiceError>,
}

impl fmt::Debug for PairedParentAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedParentAdmission([owned])")
    }
}

impl PairedParentAdmission {
    pub(crate) fn into_parts(
        self,
    ) -> (
        OwnedCommittedSettingsBind,
        Result<PairedSettingsParent, SettingsServiceError>,
    ) {
        (self.request, self.parent)
    }
}

impl SettingsService {
    pub(crate) fn bind_paired_transport(
        &self,
        transport: &HostSettingsTransport,
    ) -> Result<(), SettingsServiceError> {
        if self.invocation.is_some() {
            return Err(SettingsServiceError::OutsideScope);
        }
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        self.core
            .authority
            .attach_paired_transport(Arc::clone(transport.host_identity()))
    }

    pub(crate) async fn install_paired_parent(
        &self,
        request: OwnedCommittedSettingsBind,
    ) -> Result<PairedParentAdmission, SettingsServiceError> {
        if self.invocation.is_some() {
            return Ok(PairedParentAdmission {
                request,
                parent: Err(SettingsServiceError::OutsideScope),
            });
        }
        if self.core.application_stop.is_cancelled() {
            return Ok(PairedParentAdmission {
                request,
                parent: Err(SettingsServiceError::Stopped),
            });
        }
        let cancellation = request.authority_source().connection().cancellation();
        let deadline = request.committed_source().lifetime().deadline;
        let mutation = tokio::select! {
            biased;
            _ = self.core.application_stop.cancelled() => {
                return Ok(PairedParentAdmission { request, parent: Err(SettingsServiceError::Stopped) });
            }
            _ = cancellation.cancelled() => {
                return Ok(PairedParentAdmission { request, parent: Err(SettingsServiceError::Cancelled) });
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                return Ok(PairedParentAdmission { request, parent: Err(SettingsServiceError::Expired) });
            }
            mutation = Arc::clone(&self.core.mutation).lock_owned() => mutation,
        };
        self.blocking(move |core| {
            let _mutation = mutation;
            let parent = core.install_paired_parent(&request);
            Ok(PairedParentAdmission { request, parent })
        })
        .await
    }

    pub(crate) fn close_paired_parent(
        &self,
        parent: &PairedSettingsParent,
    ) -> Result<(), SettingsServiceError> {
        if self.invocation.is_some() {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.core.authority.close_paired_parent(parent)
    }
}

impl ServiceCore {
    fn install_paired_parent(
        &self,
        request: &OwnedCommittedSettingsBind,
    ) -> Result<PairedSettingsParent, SettingsServiceError> {
        if self.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        let source = request.authority_source();
        let intent = request.binding_intent();
        let committed = request.committed_source();
        let connection = source.connection();
        if !source.matches_context(&self.agent, &self.endpoint, &self.mutation)
            || intent.host_id != self.authority.host_id
            || connection.host_identity().host_id() != self.authority.host_id
            || intent.generation.host_epoch != connection.host_identity().host_epoch()
            || intent.generation.connection_epoch != connection.connection_epoch()
            || !connection.is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let store = self.agent.store();
        let mut main = store.connection()?;
        let transaction = main.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let head = settings_grant_head_on(
            &transaction,
            SettingsGrantOwner {
                host_id: self.authority.host_id,
                principal_id: *self.authority.host_id.as_uuid(),
            },
            source.verified_peer_keys().device_id,
        )?
        .ok_or(SettingsServiceError::OutsideScope)?;
        let now = UnixMillis::now()?;
        if head.host_revoked
            || head.peer_revoked
            || head.grant.change.peer_keys != *source.verified_peer_keys()
            || head.grant.revision != intent.generation.grant_revision
            || head.grant.change.target.grant_id != intent.generation.grant_id
            || !matches!(
                head.grant.change.target.state,
                SettingsGrantState::Active { .. }
            )
            || head != *committed.grant()
            || now < head.grant.change.target.issued_at
            || now >= head.grant.change.target.expires_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let lifetime = source
            .parent_lifetime(intent, head.grant.change.target.expires_at)
            .map_err(|reason| match reason {
                BindRejection::Expired => SettingsServiceError::Expired,
                _ => SettingsServiceError::OutsideScope,
            })?;
        let original = committed.lifetime();
        let reply = committed.reply_reservation();
        if lifetime.expires_at != original.expires_at
            || lifetime.deadline != original.deadline
            || reply.reservation_id.is_nil()
            || reply.local_device_id != intent.host_id
            || reply.peer_device_id != source.verified_peer_keys().device_id
            || reply.created_at.get() == 0
            || now < reply.created_at
            || reply.created_at >= reply.expires_at
            || reply.created_at.get() < intent.issued_at
            || reply.created_at.get() >= intent.expires_at
            || reply.expires_at > original.expires_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        transaction.commit()?;
        if self.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        if !source.matches_context(&self.agent, &self.endpoint, &self.mutation)
            || !connection.is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.authority
            .install_paired_parent(request, &self.application_stop)
    }
}
