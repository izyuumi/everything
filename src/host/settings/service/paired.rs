use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::{Transaction, TransactionBehavior};

use crate::devices::{
    AuthenticatedSettingsCommandSource, BindRejection, CommandRejection, HostSettingsTransport,
    OwnedCommittedSettingsBind, SettingsHostIdentity,
};
use crate::protocol::UnixMillis;
use crate::storage::settings_originals::{
    RemoteAcceptedIdentity, RemoteOriginalAdmission, RemotePublicCommand,
};
use crate::storage::{Store, settings_grant_head_on};

use super::super::contract::SettingKey;
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

#[derive(Clone)]
pub(crate) struct SettingsReceiptAuthority {
    core: Arc<ServiceCore>,
    host: Arc<SettingsHostIdentity>,
}

pub(crate) struct QualifiedSettingsCommand<'scope> {
    authority: &'scope SettingsReceiptAuthority,
    source: &'scope AuthenticatedSettingsCommandSource,
    invocation: Arc<()>,
    admission: RemoteOriginalAdmission,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

struct CommandObservation {
    owner: SettingsGrantOwner,
    first_wall: UnixMillis,
    wall: UnixMillis,
    monotonic: Instant,
}

impl fmt::Debug for SettingsReceiptAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReceiptAuthority([restricted])")
    }
}

impl fmt::Debug for QualifiedSettingsCommand<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedSettingsCommand([receipt-only])")
    }
}

impl From<CommandRejection> for SettingsServiceError {
    fn from(reason: CommandRejection) -> Self {
        match reason {
            CommandRejection::Expired => Self::Expired,
            CommandRejection::OutsideScope => Self::OutsideScope,
            CommandRejection::Unavailable => Self::Worker,
            CommandRejection::Bookkeeping => Self::Poisoned,
        }
    }
}

impl SettingsService {
    pub(in crate::host) fn paired_receipt_authority(
        &self,
        transport: &HostSettingsTransport,
    ) -> Result<SettingsReceiptAuthority, SettingsServiceError> {
        if self.invocation.is_some() {
            return Err(SettingsServiceError::OutsideScope);
        }
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        self.core
            .authority
            .paired_receipt_owner(transport.host_identity())?;
        Ok(SettingsReceiptAuthority {
            core: Arc::clone(&self.core),
            host: Arc::clone(transport.host_identity()),
        })
    }
}

impl QualifiedSettingsCommand<'_> {
    pub(crate) fn original_admission(&self) -> &RemoteOriginalAdmission {
        &self.admission
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn precommit_command_on<'scope>(
        &'scope self,
        store: &Store,
        transaction: &Transaction<'_>,
        source: &'scope AuthenticatedSettingsCommandSource,
    ) -> Result<QualifiedSettingsCommand<'scope>, SettingsServiceError> {
        let observed = self.observe_command(store, transaction, source)?;
        let intent = source.intent();
        Ok(QualifiedSettingsCommand {
            authority: self,
            source,
            invocation: Arc::clone(source.invocation_identity()),
            admission: RemoteOriginalAdmission::Accepted(RemoteAcceptedIdentity {
                version: 1,
                host_id: observed.owner.host_id,
                owner_principal_id: observed.owner.principal_id,
                peer_keys: source.verified_peer_keys().clone(),
                request_id: intent.request_id,
                remote_parent_id: intent.parent.parent_id,
                grant_id: intent.parent.generation.grant_id,
                grant_revision: intent.parent.generation.grant_revision,
                host_epoch: intent.parent.generation.host_epoch,
                connection_epoch: intent.parent.generation.connection_epoch,
                admitted_at: observed.wall,
                issued_at: intent.issued_at,
                expires_at: intent.expires_at,
                command: RemotePublicCommand::Update {
                    expected_settings_revision: intent.update.expected_settings_revision,
                    enabled: intent.update.enabled,
                    dimensions: intent.update.dimensions.clone(),
                },
            }),
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
        })
    }

    pub(crate) fn recheck_command_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedSettingsCommand<'_>,
    ) -> Result<(), SettingsServiceError> {
        if !std::ptr::eq(self, qualified.authority)
            || !Arc::ptr_eq(
                &qualified.invocation,
                qualified.source.invocation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let observed = self.observe_command(store, transaction, qualified.source)?;
        let RemoteOriginalAdmission::Accepted(original) = &qualified.admission else {
            return Err(SettingsServiceError::CorruptJournal);
        };
        if observed.owner.host_id != original.host_id
            || observed.owner.principal_id != original.owner_principal_id
            || observed.first_wall < qualified.observed_wall
            || observed.wall < qualified.observed_wall
            || observed.monotonic < qualified.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    fn observe_command(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        source: &AuthenticatedSettingsCommandSource,
    ) -> Result<CommandObservation, SettingsServiceError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        if !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
            || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let (first_wall, first_monotonic) = source.check_precommit(store)?;
        let owner = self.core.authority.paired_receipt_owner(&self.host)?;
        let head =
            settings_grant_head_on(transaction, owner, source.verified_peer_keys().device_id)?
                .ok_or(SettingsServiceError::OutsideScope)?;
        let (checked_owner, wall, monotonic) = self.core.authority.check_paired_command(
            &self.host,
            store,
            source,
            &self.core.application_stop,
        )?;
        let intent = source.intent();
        if checked_owner != owner
            || wall < first_wall
            || monotonic < first_monotonic
            || head.host_revoked
            || head.peer_revoked
            || head.grant.change.peer_keys != *source.verified_peer_keys()
            || head.grant.revision != intent.parent.generation.grant_revision
            || head.grant.change.target.grant_id != intent.parent.generation.grant_id
            || first_wall < head.grant.change.target.issued_at
            || wall >= head.grant.change.target.expires_at
            || intent.expires_at > head.grant.change.target.expires_at
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let SettingsGrantState::Active {
            readable_keys,
            writable_keys,
            ..
        } = &head.grant.change.target.state
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let allowed = |key| readable_keys.contains(&key) && writable_keys.contains(&key);
        if (intent.update.enabled.is_some() && !allowed(SettingKey::EmbeddingsEnabled))
            || (intent.update.dimensions.is_some() && !allowed(SettingKey::EmbeddingsDimensions))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        if self.core.application_stop.is_cancelled() || !source.connection().is_live() {
            return Err(SettingsServiceError::Stopped);
        }
        Ok(CommandObservation {
            owner,
            first_wall,
            wall,
            monotonic,
        })
    }
}
