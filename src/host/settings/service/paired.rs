mod access;

pub(crate) use access::{QualifiedSettingsAccess, SettingsAccessQualificationError};

mod replies;

pub(crate) use replies::{QualifiedBindReply, SettingsReplyProjectionError};

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::{Connection, Row, Transaction, TransactionBehavior, types::ValueRef};
use tokio::sync::OwnedMutexGuard;

use crate::devices::{
    AuthenticatedSettingsCommandSource, BindRejection, CommandRejection,
    CommittedSettingsCommandSource, HostSettingsTransport, OwnedCommittedSettingsBind,
    OwnedHistoricalSettingsCommand, SettingsHostIdentity,
};
use crate::protocol::UnixMillis;
use crate::storage::settings_originals::{
    RemoteAcceptedIdentity, RemoteDimensionSelection, RemoteOriginalAdmission,
    RemoteOriginalIdentity, RemoteOriginalLink, RemoteOriginalRecord, RemotePublicCommand,
    read_original_on,
};
use crate::storage::{Store, StoredSettingsCommandReply, settings_grant_head_on};

use super::super::actions::SettingsActionTarget;
use super::super::authority::SettingsOperationOrigin;
use super::super::contract::{SettingKey, SettingValue, SettingsScope};
use super::super::grants::{SettingsGrantOwner, SettingsGrantState};
use super::super::operations::{OperationRecord, SettingsOperationPhase};
use super::{ServiceCore, SettingsService, SettingsServiceError};

pub(crate) use super::super::authority::PairedSettingsParent;

pub(crate) struct PairedParentAdmission {
    parent: Result<PairedSettingsParent, SettingsServiceError>,
    request: OwnedCommittedSettingsBind,
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
        Result<PairedSettingsParent, SettingsServiceError>,
        OwnedCommittedSettingsBind,
    ) {
        (self.parent, self.request)
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

pub(crate) struct PairedUpdateOutcome {
    result: Result<super::super::operations::SettingsOperationInspection, SettingsServiceError>,
    request: Arc<crate::devices::OwnedCommittedSettingsCommand>,
}

impl fmt::Debug for PairedUpdateOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PairedUpdateOutcome([owned])")
    }
}

impl PairedUpdateOutcome {
    pub(crate) fn into_parts(
        self,
    ) -> Result<
        (
            Result<super::super::operations::SettingsOperationInspection, SettingsServiceError>,
            crate::devices::OwnedCommittedSettingsCommand,
        ),
        Box<Self>,
    > {
        let Self { result, request } = self;
        match Arc::try_unwrap(request) {
            Ok(request) => Ok((result, request)),
            Err(request) => Err(Box::new(Self { result, request })),
        }
    }
}

pub(in crate::host::settings) struct PairedWriteAdmission<'request> {
    core: &'request ServiceCore,
    request: &'request crate::devices::OwnedCommittedSettingsCommand,
    caller: &'request super::super::authority::SettingsCaller,
    cancellation: tokio_util::sync::CancellationToken,
}

struct PairedUpdatePreflight {
    cancellation: tokio_util::sync::CancellationToken,
    core: Arc<ServiceCore>,
    request: Arc<crate::devices::OwnedCommittedSettingsCommand>,
}

struct PairedUpdatePreflightOutcome {
    result: Result<super::super::authority::SettingsCaller, SettingsServiceError>,
    request: Arc<crate::devices::OwnedCommittedSettingsCommand>,
}

impl PairedUpdatePreflight {
    fn run(self) -> PairedUpdatePreflightOutcome {
        let result = self
            .core
            .paired_update_caller(&self.request, self.cancellation.clone());
        PairedUpdatePreflightOutcome {
            result,
            request: self.request,
        }
    }
}

struct PairedUpdateWork {
    caller: super::super::authority::SettingsCaller,
    mutation: Option<tokio::sync::OwnedMutexGuard<()>>,
    cancellation: tokio_util::sync::CancellationToken,
    core: Arc<ServiceCore>,
    request: Arc<crate::devices::OwnedCommittedSettingsCommand>,
}

impl PairedUpdateWork {
    fn run(mut self) -> PairedUpdateOutcome {
        let result = match self.mutation.take() {
            Some(mutation) => self.core.update_paired_guarded(
                &self.request,
                &self.caller,
                mutation,
                self.cancellation.clone(),
            ),
            None => Err(SettingsServiceError::Poisoned),
        };
        PairedUpdateOutcome {
            result,
            request: self.request,
        }
    }
}

impl SettingsService {
    pub(in crate::host) async fn update_paired(
        &self,
        request: crate::devices::OwnedCommittedSettingsCommand,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> PairedUpdateOutcome {
        let request = Arc::new(request);
        if self.invocation.is_some() {
            return PairedUpdateOutcome {
                result: Err(SettingsServiceError::OutsideScope),
                request,
            };
        }
        let work = PairedUpdatePreflight {
            cancellation: cancellation.clone(),
            core: Arc::clone(&self.core),
            request: Arc::clone(&request),
        };
        let checked = tokio::task::spawn_blocking(move || work.run()).await;
        let caller = match checked {
            Ok(PairedUpdatePreflightOutcome {
                result: Ok(caller),
                request: retained,
            }) => {
                drop(retained);
                caller
            }
            Ok(PairedUpdatePreflightOutcome {
                result: Err(error),
                request: retained,
            }) => {
                drop(retained);
                return PairedUpdateOutcome {
                    result: Err(error),
                    request,
                };
            }
            Err(_) => {
                return PairedUpdateOutcome {
                    result: Err(SettingsServiceError::Worker),
                    request,
                };
            }
        };
        let parent_cancelled = match self.core.authority.paired_update_cancellation(&caller) {
            Ok(token) => token,
            Err(error) => {
                return PairedUpdateOutcome {
                    result: Err(error),
                    request,
                };
            }
        };
        let connection_cancelled = request
            .committed_source()
            .authority_source()
            .connection()
            .cancellation();
        let deadline = request.committed_source().original_deadline();
        let mutation = tokio::select! {
            biased;
            _ = self.core.application_stop.cancelled() => {
                return PairedUpdateOutcome { result: Err(SettingsServiceError::Stopped), request };
            }
            _ = cancellation.cancelled() => {
                return PairedUpdateOutcome { result: Err(SettingsServiceError::Cancelled), request };
            }
            _ = parent_cancelled.cancelled() => {
                return PairedUpdateOutcome { result: Err(SettingsServiceError::Cancelled), request };
            }
            _ = connection_cancelled.cancelled() => {
                return PairedUpdateOutcome { result: Err(SettingsServiceError::Cancelled), request };
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                return PairedUpdateOutcome { result: Err(SettingsServiceError::Expired), request };
            }
            mutation = Arc::clone(&self.core.mutation).lock_owned() => mutation,
        };
        let work = PairedUpdateWork {
            caller,
            mutation: Some(mutation),
            cancellation,
            core: Arc::clone(&self.core),
            request: Arc::clone(&request),
        };
        match tokio::task::spawn_blocking(move || work.run()).await {
            Ok(outcome) => {
                drop(request);
                outcome
            }
            Err(_) => PairedUpdateOutcome {
                result: Err(SettingsServiceError::Worker),
                request,
            },
        }
    }
}

impl ServiceCore {
    fn paired_update_caller(
        &self,
        request: &crate::devices::OwnedCommittedSettingsCommand,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<super::super::authority::SettingsCaller, SettingsServiceError> {
        self.check_committed_update(request)?;
        let committed = request.committed_source();
        let caller = self.authority.paired_update_caller(&committed)?;
        let admission = PairedWriteAdmission::new(self, request, &caller, cancellation);
        let identity = admission.identity()?;
        let store = self.agent.store();
        let mut main = store.connection()?;
        let transaction = main.transaction_with_behavior(TransactionBehavior::Deferred)?;
        admission.check_unlinked_on(&store, &transaction, &identity)?;
        transaction.commit()?;
        self.authority.check_paired_update(&caller)?;
        Ok(caller)
    }

    fn check_committed_update(
        &self,
        request: &crate::devices::OwnedCommittedSettingsCommand,
    ) -> Result<(), SettingsServiceError> {
        let committed = request.committed_source();
        let authority = committed.receipt_authority();
        let source = committed.authority_source();
        if self.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        if !std::ptr::eq(self, Arc::as_ptr(&authority.core))
            || !source.matches_context(&self.agent, &self.endpoint, &self.mutation)
            || !Arc::ptr_eq(&authority.host, source.connection().host_identity())
            || !source.connection().is_live()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let owner = self.authority.paired_receipt_owner(&authority.host)?;
        let original = committed.original();
        let crate::storage::settings_originals::RemoteOriginalIdentity::Accepted(identity) =
            &original.identity
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let intent = source.intent();
        let expected = RemoteAcceptedIdentity {
            version: 1,
            host_id: owner.host_id,
            owner_principal_id: owner.principal_id,
            peer_keys: source.verified_peer_keys().clone(),
            request_id: intent.request_id,
            remote_parent_id: intent.parent.parent_id,
            grant_id: intent.parent.generation.grant_id,
            grant_revision: intent.parent.generation.grant_revision,
            host_epoch: intent.parent.generation.host_epoch,
            connection_epoch: intent.parent.generation.connection_epoch,
            admitted_at: identity.admitted_at,
            issued_at: intent.issued_at,
            expires_at: intent.expires_at,
            command: RemotePublicCommand::Update {
                expected_settings_revision: intent.update.expected_settings_revision,
                enabled: intent.update.enabled,
                dimensions: intent.update.dimensions.clone(),
            },
        };
        let reply = committed.reply_reservation();
        let floor = committed.receipt_floor();
        let deadline = floor
            .1
            .checked_add(std::time::Duration::from_millis(
                identity
                    .expires_at
                    .get()
                    .checked_sub(floor.0.get())
                    .ok_or(SettingsServiceError::Expired)?,
            ))
            .ok_or(SettingsServiceError::Expired)?;
        if *identity != expected
            || original.key != original.identity.key()
            || original.link.is_some()
            || original.close.is_some()
            || original.terminal.is_some()
            || reply.reservation_id.is_nil()
            || reply.local_device_id != identity.host_id
            || reply.peer_device_id != identity.peer_keys.device_id
            || reply.created_at != identity.admitted_at
            || reply.expires_at != identity.expires_at
            || identity.admitted_at < floor.0
            || committed.original_deadline() != deadline
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }
}

impl<'request> PairedWriteAdmission<'request> {
    pub(super) fn new(
        core: &'request ServiceCore,
        request: &'request crate::devices::OwnedCommittedSettingsCommand,
        caller: &'request super::super::authority::SettingsCaller,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            core,
            request,
            caller,
            cancellation,
        }
    }

    pub(super) fn deadline(&self) -> Instant {
        self.request.committed_source().original_deadline()
    }

    pub(super) fn check_unlinked(
        &self,
        identity: &super::super::operations::OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        let store = self.core.agent.store();
        let mut main = store.connection()?;
        let transaction = main.transaction_with_behavior(TransactionBehavior::Deferred)?;
        self.check_unlinked_on(&store, &transaction, identity)?;
        transaction.commit()?;
        self.core.authority.check_paired_update(self.caller)?;
        Ok(())
    }

    pub(in crate::host::settings) fn identity(
        &self,
    ) -> Result<super::super::operations::OperationIdentity, SettingsServiceError> {
        use super::super::actions::SettingsActionTarget;
        use super::super::contract::{
            SETTINGS_CONTRACT_VERSION, SettingChange, SettingValue, SettingsPatch,
            SettingsRequestBinding, SettingsScope,
        };
        use crate::storage::settings_originals::RemoteDimensionSelection;
        let original = self.core.authority.paired_update_identity(self.caller)?;
        let RemotePublicCommand::Update {
            expected_settings_revision,
            enabled,
            dimensions,
        } = &original.command
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let mut changes = Vec::with_capacity(2);
        if let Some(value) = enabled {
            changes.push(SettingChange {
                key: SettingKey::EmbeddingsEnabled,
                value: SettingValue::Boolean { value: *value },
            });
        }
        if let Some(value) = dimensions {
            changes.push(SettingChange {
                key: SettingKey::EmbeddingsDimensions,
                value: SettingValue::Dimensions {
                    value: match value {
                        RemoteDimensionSelection::BackendDefault => None,
                        RemoteDimensionSelection::Selected { dimensions } => Some(*dimensions),
                    },
                },
            });
        }
        changes.sort_unstable_by_key(|change| change.key);
        let scope = SettingsScope::Host {
            host_id: original.host_id,
        };
        let target = SettingsActionTarget::Patch(SettingsPatch {
            contract_version: SETTINGS_CONTRACT_VERSION,
            request_id: original.request_id,
            scope: scope.clone(),
            expected_revision: *expected_settings_revision,
            changes,
            confirmation: None,
        });
        let identity = super::super::operations::OperationIdentity {
            version: 2,
            binding: SettingsRequestBinding {
                request_id: original.request_id,
                principal_id: *original.peer_keys.device_id.as_uuid(),
                client_session_id: original.remote_parent_id,
                scope,
                expected_revision: Some(*expected_settings_revision),
                exact_request_sha256: super::super::operations::request_digest(&target)?,
                issued_at: original.issued_at,
                expires_at: original.expires_at,
            },
            target,
            origin: self.core.authority.origin(self.caller),
        };
        identity.validate()?;
        Ok(identity)
    }

    pub(in crate::host::settings) fn check_unlinked_on(
        &self,
        store: &Store,
        connection: &rusqlite::Connection,
        identity: &super::super::operations::OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        let original = self.observe_on(store, connection, identity)?;
        if original.link.is_some() {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(())
    }

    pub(in crate::host::settings) fn check_linked_on(
        &self,
        store: &Store,
        connection: &rusqlite::Connection,
        identity: &super::super::operations::OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        let original = self.observe_on(store, connection, identity)?;
        if original.link
            != Some(
                crate::storage::settings_originals::RemoteOriginalLink::Operation {
                    operation_id: identity.binding.request_id,
                },
            )
        {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(())
    }

    pub(super) fn check_linked(
        &self,
        identity: &super::super::operations::OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        let store = self.core.agent.store();
        let mut main = store.connection()?;
        let transaction = main.transaction_with_behavior(TransactionBehavior::Deferred)?;
        self.check_linked_on(&store, &transaction, identity)?;
        transaction.commit()?;
        self.core.authority.check_paired_update(self.caller)?;
        Ok(())
    }

    fn observe_on(
        &self,
        store: &Store,
        connection: &rusqlite::Connection,
        identity: &super::super::operations::OperationIdentity,
    ) -> Result<crate::storage::settings_originals::RemoteOriginalRecord, SettingsServiceError>
    {
        use crate::storage::settings_originals::read_original_on;
        if self.cancellation.is_cancelled() {
            return Err(SettingsServiceError::Cancelled);
        }
        self.core.check_committed_update(self.request)?;
        if !self.core.agent.store().shares_connection(store) || *identity != self.identity()? {
            return Err(SettingsServiceError::OutsideScope);
        }
        let committed = self.request.committed_source();
        self.core
            .authority
            .paired_update_matches(self.caller, &committed)?;
        let source = committed.authority_source();
        let owner = self
            .core
            .authority
            .paired_receipt_owner(&committed.receipt_authority().host)?;
        let head =
            settings_grant_head_on(connection, owner, source.verified_peer_keys().device_id)?
                .ok_or(SettingsServiceError::OutsideScope)?;
        let original = read_original_on(connection, committed.original().key)?
            .ok_or(SettingsServiceError::NotFound)?;
        let (wall, _) = self.core.authority.check_paired_update(self.caller)?;
        if original.identity != committed.original().identity
            || original.close.is_some()
            || original.terminal.is_some()
            || head.host_revoked
            || head.peer_revoked
            || head.grant.change.peer_keys != *source.verified_peer_keys()
            || head.grant.revision != source.intent().parent.generation.grant_revision
            || head.grant.change.target.grant_id != source.intent().parent.generation.grant_id
            || wall < head.grant.change.target.issued_at
            || wall >= head.grant.change.target.expires_at
            || source.intent().expires_at > head.grant.change.target.expires_at
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
        let super::super::actions::SettingsActionTarget::Patch(patch) = &identity.target else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if patch.changes.iter().any(|change| {
            !readable_keys.contains(&change.key) || !writable_keys.contains(&change.key)
        }) {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.core.check_committed_update(self.request)?;
        if self.cancellation.is_cancelled() {
            return Err(SettingsServiceError::Cancelled);
        }
        self.core.authority.check_paired_update(self.caller)?;
        Ok(original)
    }
}

pub(crate) enum OriginalReplySource<'request> {
    Fresh(CommittedSettingsCommandSource<'request>),
    Historical(&'request OwnedHistoricalSettingsCommand),
}

pub(crate) struct QualifiedOriginalReply<'scope> {
    authority: &'scope SettingsReceiptAuthority,
    source: OriginalReplySource<'scope>,
    mutation: &'scope OwnedMutexGuard<()>,
    invocation: Arc<()>,
    original: RemoteOriginalRecord,
    operation: Option<OperationRecord>,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

struct OriginalReplyObservation {
    original: RemoteOriginalRecord,
    operation: Option<OperationRecord>,
    wall: UnixMillis,
    monotonic: Instant,
}

impl fmt::Debug for OriginalReplySource<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OriginalReplySource([owned association])")
    }
}

impl fmt::Debug for QualifiedOriginalReply<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedOriginalReply([current read only])")
    }
}

impl OriginalReplySource<'_> {
    fn authenticated(&self) -> &AuthenticatedSettingsCommandSource {
        match self {
            Self::Fresh(source) => source.authority_source(),
            Self::Historical(request) => request.authority_source(),
        }
    }

    fn receipt_authority(&self) -> &SettingsReceiptAuthority {
        match self {
            Self::Fresh(source) => source.receipt_authority(),
            Self::Historical(request) => request.receipt_authority(),
        }
    }

    fn original(&self) -> &RemoteOriginalRecord {
        match self {
            Self::Fresh(source) => source.original(),
            Self::Historical(request) => request.original(),
        }
    }

    fn reply_reservation(&self) -> &StoredSettingsCommandReply {
        match self {
            Self::Fresh(source) => source.reply_reservation(),
            Self::Historical(request) => request.reply_reservation(),
        }
    }

    fn original_deadline(&self) -> Instant {
        match self {
            Self::Fresh(source) => source.original_deadline(),
            Self::Historical(request) => request.original_deadline(),
        }
    }

    fn receipt_floor(&self) -> (UnixMillis, Instant) {
        match self {
            Self::Fresh(source) => source.receipt_floor(),
            Self::Historical(request) => request.receipt_floor(),
        }
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn qualify_original_reply_on<'scope>(
        &'scope self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &'scope OwnedMutexGuard<()>,
        source: OriginalReplySource<'scope>,
    ) -> Result<QualifiedOriginalReply<'scope>, SettingsServiceError> {
        let observed = self.observe_original_reply_on(store, transaction, mutation, &source)?;
        let invocation = Arc::clone(source.authenticated().invocation_identity());
        Ok(QualifiedOriginalReply {
            authority: self,
            source,
            mutation,
            invocation,
            original: observed.original,
            operation: observed.operation,
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
        })
    }

    pub(crate) fn recheck_original_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedOriginalReply<'_>,
    ) -> Result<(), SettingsServiceError> {
        if !std::ptr::eq(self, qualified.authority)
            || !Arc::ptr_eq(
                &qualified.invocation,
                qualified.source.authenticated().invocation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let observed = self.observe_original_reply_on(
            store,
            transaction,
            qualified.mutation,
            &qualified.source,
        )?;
        let same_operation = match (&qualified.operation, &observed.operation) {
            (None, None) => true,
            (Some(previous), Some(current)) => {
                previous.identity == current.identity
                    && previous.inspection() == current.inspection()
            }
            _ => false,
        };
        if observed.original != qualified.original
            || !same_operation
            || observed.wall < qualified.observed_wall
            || observed.monotonic < qualified.observed_monotonic
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    fn observe_original_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        subject: &OriginalReplySource<'_>,
    ) -> Result<OriginalReplyObservation, SettingsServiceError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        let source = subject.authenticated();
        if !std::ptr::eq(self, subject.receipt_authority())
            || !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.core.mutation)
            || !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
            || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let snapshot = subject.original();
        let RemoteOriginalIdentity::Accepted(identity) = &snapshot.identity else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let floor = subject.receipt_floor();
        let deadline = floor
            .1
            .checked_add(std::time::Duration::from_millis(
                identity
                    .expires_at
                    .get()
                    .checked_sub(floor.0.get())
                    .ok_or(SettingsServiceError::Expired)?,
            ))
            .ok_or(SettingsServiceError::Expired)?;
        if deadline != subject.original_deadline() || snapshot.key != snapshot.identity.key() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let (owner, first_wall, first_monotonic) = self.core.authority.observe_paired_reply(
            &self.host,
            source,
            floor,
            deadline,
            &self.core.application_stop,
        )?;
        let intent = source.intent();
        let expected = RemoteAcceptedIdentity {
            version: 1,
            host_id: owner.host_id,
            owner_principal_id: owner.principal_id,
            peer_keys: source.verified_peer_keys().clone(),
            request_id: intent.request_id,
            remote_parent_id: intent.parent.parent_id,
            grant_id: intent.parent.generation.grant_id,
            grant_revision: intent.parent.generation.grant_revision,
            host_epoch: intent.parent.generation.host_epoch,
            connection_epoch: intent.parent.generation.connection_epoch,
            admitted_at: identity.admitted_at,
            issued_at: intent.issued_at,
            expires_at: intent.expires_at,
            command: RemotePublicCommand::Update {
                expected_settings_revision: intent.update.expected_settings_revision,
                enabled: intent.update.enabled,
                dimensions: intent.update.dimensions.clone(),
            },
        };
        if *identity != expected || first_wall < identity.admitted_at {
            return Err(SettingsServiceError::OutsideScope);
        }
        let head =
            settings_grant_head_on(connection, owner, source.verified_peer_keys().device_id)?
                .ok_or(SettingsServiceError::OutsideScope)?;
        let SettingsGrantState::Active { readable_keys, .. } = &head.grant.change.target.state
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if head.host_revoked
            || head.peer_revoked
            || head.grant.change.peer_keys != *source.verified_peer_keys()
            || head.grant.revision != identity.grant_revision
            || head.grant.change.target.grant_id != identity.grant_id
            || first_wall < head.grant.change.target.issued_at
            || identity.expires_at > head.grant.change.target.expires_at
            || (intent.update.enabled.is_some()
                && !readable_keys.contains(&SettingKey::EmbeddingsEnabled))
            || (intent.update.dimensions.is_some()
                && !readable_keys.contains(&SettingKey::EmbeddingsDimensions))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let original =
            read_original_on(connection, snapshot.key)?.ok_or(SettingsServiceError::NotFound)?;
        if original.identity != snapshot.identity
            || snapshot
                .link
                .as_ref()
                .is_some_and(|link| original.link.as_ref() != Some(link))
            || snapshot
                .close
                .as_ref()
                .is_some_and(|close| original.close.as_ref() != Some(close))
            || snapshot
                .terminal
                .is_some_and(|terminal| original.terminal != Some(terminal))
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let operation = match &original.link {
            None => None,
            Some(RemoteOriginalLink::Operation { operation_id }) => {
                if *operation_id != identity.request_id {
                    return Err(SettingsServiceError::CorruptJournal);
                }
                let operation = super::super::operations::read(connection, *operation_id)?
                    .ok_or(SettingsServiceError::CorruptJournal)?;
                check_original_reply_operation(identity, &operation)?;
                if operation.write().is_some_and(|write| {
                    write
                        .intent
                        .changed_keys
                        .iter()
                        .any(|key| !readable_keys.contains(key))
                }) {
                    return Err(SettingsServiceError::OutsideScope);
                }
                Some(operation)
            }
            Some(_) => return Err(SettingsServiceError::CorruptJournal),
        };
        check_original_reply_reservation(connection, subject, first_wall)?;
        let (checked_owner, wall, monotonic) = self.core.authority.observe_paired_reply(
            &self.host,
            source,
            floor,
            deadline,
            &self.core.application_stop,
        )?;
        if checked_owner != owner
            || wall < first_wall
            || monotonic < first_monotonic
            || wall >= head.grant.change.target.expires_at
            || wall >= subject.reply_reservation().expires_at
            || self.core.application_stop.is_cancelled()
            || !source.connection().is_live()
            || !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(OriginalReplyObservation {
            original,
            operation,
            wall,
            monotonic,
        })
    }
}

fn check_original_reply_operation(
    original: &RemoteAcceptedIdentity,
    operation: &OperationRecord,
) -> Result<(), SettingsServiceError> {
    let identity = &operation.identity;
    let RemotePublicCommand::Update {
        expected_settings_revision,
        enabled,
        dimensions,
    } = &original.command
    else {
        return Err(SettingsServiceError::OutsideScope);
    };
    let SettingsActionTarget::Patch(patch) = &identity.target else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    let expected_origin = SettingsOperationOrigin::PairedSettings {
        actor_device_id: original.peer_keys.device_id,
        remote_parent_id: original.remote_parent_id,
        grant_id: original.grant_id,
        grant_revision: original.grant_revision,
        host_epoch: original.host_epoch,
        connection_epoch: original.connection_epoch,
    };
    if identity.version != 2
        || identity.origin != expected_origin
        || identity.binding.request_id != original.request_id
        || identity.binding.principal_id != *original.peer_keys.device_id.as_uuid()
        || identity.binding.client_session_id != original.remote_parent_id
        || identity.binding.scope
            != (SettingsScope::Host {
                host_id: original.host_id,
            })
        || identity.binding.expected_revision != Some(*expected_settings_revision)
        || identity.binding.issued_at != original.issued_at
        || identity.binding.expires_at != original.expires_at
        || patch.expected_revision != *expected_settings_revision
        || patch.confirmation.is_some()
        || patch.changes.len() != usize::from(enabled.is_some()) + usize::from(dimensions.is_some())
        || !patch
            .changes
            .iter()
            .all(|change| match (&change.key, &change.value) {
                (SettingKey::EmbeddingsEnabled, SettingValue::Boolean { value }) => {
                    enabled == &Some(*value)
                }
                (SettingKey::EmbeddingsDimensions, SettingValue::Dimensions { value }) => {
                    match dimensions {
                        Some(RemoteDimensionSelection::BackendDefault) => value.is_none(),
                        Some(RemoteDimensionSelection::Selected { dimensions }) => {
                            *value == Some(*dimensions)
                        }
                        None => false,
                    }
                }
                _ => false,
            })
        || !matches!(
            operation.phase(),
            SettingsOperationPhase::Applying { .. } | SettingsOperationPhase::Finished { .. }
        )
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

fn check_original_reply_reservation(
    connection: &Connection,
    subject: &OriginalReplySource<'_>,
    now: UnixMillis,
) -> Result<(), SettingsServiceError> {
    let reply = subject.reply_reservation();
    let source = subject.authenticated();
    let floor = subject.receipt_floor();
    if reply.reservation_id.is_nil()
        || reply.local_device_id != source.intent().host_id
        || reply.peer_device_id != source.verified_peer_keys().device_id
        || reply.provenance.len() > 2048
        || reply.created_at < floor.0
        || reply.created_at > now
        || reply.created_at >= reply.expires_at
        || reply.expires_at != source.intent().expires_at
        || now >= reply.expires_at
        || matches!(subject, OriginalReplySource::Fresh(_))
            && !matches!(&subject.original().identity, RemoteOriginalIdentity::Accepted(identity) if reply.created_at == identity.admitted_at)
    {
        return Err(SettingsServiceError::OutsideScope);
    }
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1")?;
    let mut rows = statement.query([reply.reservation_id.to_string()])?;
    let row = rows.next()?.ok_or(SettingsServiceError::NotFound)?;
    let created_at =
        i64::try_from(reply.created_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    let expires_at =
        i64::try_from(reply.expires_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    if original_reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || original_reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || original_reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || original_reply_text(row, 3, 2048)? != reply.provenance
        || original_reply_text(row, 4, 10)? != "inspection"
        || row.get::<_, i64>(5)? != 90_112
        || row.get::<_, i64>(6)? != created_at
        || row.get::<_, i64>(7)? != expires_at
    {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(())
}

fn original_reply_text<'row>(
    row: &'row Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, SettingsServiceError> {
    let ValueRef::Text(bytes) = row.get_ref(column)? else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    if bytes.len() > maximum {
        return Err(SettingsServiceError::CorruptJournal);
    }
    std::str::from_utf8(bytes).map_err(|_| SettingsServiceError::CorruptJournal)
}
