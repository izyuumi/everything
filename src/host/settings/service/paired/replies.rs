mod comparison;

pub(crate) use comparison::SettingsReplyComparison;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::crypto::DevicePublicKeys;
use crate::devices::{
    BindRejection, BindingHistoryState, CommandRejection, OriginalKind, OriginalLink, OriginalView,
    OwnedCommittedSettingsBind, RemoteOperationPhase, RemoteOperationState, RemoteServiceReceipt,
    ReplyBuildPermit, ServiceLookup, SettingsAccessReplyContractError,
    SettingsBindReplyContractError, SettingsBindReplyPacket, SettingsOriginalReplyPacket,
    SettingsParentReference, SettingsReceiveError, SettingsReplyContractError,
    SettingsReplyPublicationError, SettingsReplyQualification,
};
use crate::host::settings::SettingsGrantReference;
use crate::host::settings::contract::SettingsOperationState;
use crate::protocol::{DeviceId, MessageId, UnixMillis};
use crate::storage::settings_originals::{
    RemoteOriginalIdentity, RemoteOriginalLink, RemotePublicCommand,
};
use crate::storage::{ConsumedSettingsReply, Store, settings_grant_head_on};

use super::access::SettingsAccessQualificationError;
use super::{
    OriginalReplySource, PairedSettingsParent, QualifiedOriginalReply, SettingsGrantOwner,
    SettingsGrantState, SettingsOperationPhase, SettingsReceiptAuthority, SettingsServiceError,
    original_reply_text,
};

#[derive(Error)]
pub(crate) enum SettingsReplyProjectionError {
    #[error("Settings reply current authority check failed")]
    Authority(#[source] Box<SettingsServiceError>),
    #[error("Settings reply build ownership check failed")]
    Build(#[from] SettingsReceiveError),
    #[error("Settings Bind reply source check failed")]
    BindSource(#[from] BindRejection),
    #[error("Settings command reply source check failed")]
    CommandSource(#[from] CommandRejection),
    #[error("Settings Original reply data is invalid")]
    OriginalData(#[from] SettingsReplyContractError),
    #[error("Settings Bind reply data is invalid")]
    BindData(#[from] SettingsBindReplyContractError),
    #[error("Settings Access reply current authority check failed")]
    AccessQualification(#[from] SettingsAccessQualificationError),
    #[error("Settings Access reply data is invalid")]
    AccessData(#[from] SettingsAccessReplyContractError),
    #[error("Settings reply provenance encoding failed")]
    Provenance(#[source] serde_json::Error),
}

impl From<SettingsServiceError> for SettingsReplyProjectionError {
    fn from(error: SettingsServiceError) -> Self {
        Self::Authority(Box::new(error))
    }
}

impl fmt::Debug for SettingsReplyProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyProjectionError([retained cause])")
    }
}

pub(crate) struct QualifiedBindReply<'scope> {
    authority: &'scope SettingsReceiptAuthority,
    request: &'scope OwnedCommittedSettingsBind,
    parent: &'scope PairedSettingsParent,
    mutation: &'scope OwnedMutexGuard<()>,
    creation: Arc<()>,
    reply_deadline: Instant,
    observed_wall: UnixMillis,
    observed_monotonic: Instant,
}

struct BindReplyRead {
    owner: SettingsGrantOwner,
    reply_deadline: Instant,
    grant_expires_at: UnixMillis,
    wall: UnixMillis,
    monotonic: Instant,
}

struct BindReplyObservation {
    reply_deadline: Instant,
    wall: UnixMillis,
    monotonic: Instant,
}

impl fmt::Debug for QualifiedBindReply<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QualifiedBindReply([current read only])")
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn recheck_consumed_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<(), SettingsReplyPublicationError> {
        consumed.check_on(transaction)?;
        match consumed.sealed().qualification() {
            SettingsReplyQualification::Original(qualified) => {
                self.recheck_consumed_original_reply_on(store, transaction, qualified)
                    .map_err(SettingsReplyProjectionError::from)?;
            }
            SettingsReplyQualification::Bind(qualified) => {
                self.recheck_consumed_bind_reply_on(store, transaction, qualified)?;
            }
            SettingsReplyQualification::Access(_) => {
                self.recheck_consumed_access_reply_on(store, transaction, consumed)?;
            }
        }
        Ok(())
    }

    pub(crate) fn project_original_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedOriginalReply<'_>,
        permit: &ReplyBuildPermit,
    ) -> Result<Zeroizing<Vec<u8>>, SettingsReplyProjectionError> {
        match &qualified.source {
            OriginalReplySource::Fresh(source) => permit.check_fresh(source)?,
            OriginalReplySource::Historical(request) => permit.check_historical(request)?,
        }
        self.recheck_original_reply_on(store, transaction, qualified)?;
        if !matches!(
            &qualified.original.identity,
            RemoteOriginalIdentity::Accepted(identity)
                if matches!(identity.command, RemotePublicCommand::Update { .. })
        ) {
            return Err(SettingsServiceError::CorruptJournal.into());
        }
        let (link, service) = match (&qualified.original.link, &qualified.operation) {
            (None, None) => (OriginalLink::Unlinked, ServiceLookup::NotRequested),
            (Some(RemoteOriginalLink::Operation { operation_id }), Some(operation)) => {
                let inspection = operation.inspection();
                let phase = remote_phase(&inspection.phase)?;
                (
                    OriginalLink::Operation {
                        operation_id: *operation_id,
                    },
                    ServiceLookup::Recorded {
                        receipt: RemoteServiceReceipt {
                            operation_id: inspection.binding.request_id,
                            expected_revision: inspection.binding.expected_revision,
                            updated_at: inspection.updated_at,
                            phase,
                            write: inspection.write,
                            application: inspection.application,
                        },
                    },
                )
            }
            _ => return Err(SettingsServiceError::CorruptJournal.into()),
        };
        let source = qualified.source.authenticated();
        let in_reply_to: MessageId = source
            .receipt_metadata()?
            .message_id
            .parse()
            .map_err(SettingsServiceError::from)?;
        let intent = source.intent();
        let packet = SettingsOriginalReplyPacket::new(
            intent.host_id,
            in_reply_to,
            intent.request_id,
            intent.parent.generation.clone(),
            OriginalView {
                original_kind: OriginalKind::Update,
                terminal: qualified.original.terminal,
                link,
                mutation_operation_ids: Vec::new(),
                service,
            },
        )?;
        packet
            .encode_checked()
            .map_err(SettingsReplyProjectionError::from)
    }

    pub(crate) fn qualify_bind_reply_on<'scope>(
        &'scope self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &'scope OwnedMutexGuard<()>,
        request: &'scope OwnedCommittedSettingsBind,
        parent: &'scope PairedSettingsParent,
    ) -> Result<QualifiedBindReply<'scope>, SettingsReplyProjectionError> {
        let observed = self.observe_bind_reply_on(store, transaction, mutation, request, parent)?;
        Ok(QualifiedBindReply {
            authority: self,
            request,
            parent,
            mutation,
            creation: Arc::clone(request.committed_source().creation_identity()),
            reply_deadline: observed.reply_deadline,
            observed_wall: observed.wall,
            observed_monotonic: observed.monotonic,
        })
    }

    pub(crate) fn recheck_bind_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedBindReply<'_>,
    ) -> Result<(), SettingsReplyProjectionError> {
        self.check_bind_reply_association(qualified)?;
        let observed = self.observe_bind_reply_on(
            store,
            transaction,
            qualified.mutation,
            qualified.request,
            qualified.parent,
        )?;
        check_bind_reply_observation(qualified, observed)
    }

    fn recheck_consumed_bind_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedBindReply<'_>,
    ) -> Result<(), SettingsReplyProjectionError> {
        self.check_bind_reply_association(qualified)?;
        let read = self.read_bind_reply_on(
            store,
            transaction,
            qualified.mutation,
            qualified.request,
            qualified.parent,
        )?;
        check_bind_reservation_identity(qualified.request, read.wall)?;
        let observed =
            self.finish_bind_reply_observation(qualified.request, qualified.parent, read)?;
        check_bind_reply_observation(qualified, observed)
    }

    fn check_bind_reply_association(
        &self,
        qualified: &QualifiedBindReply<'_>,
    ) -> Result<(), SettingsReplyProjectionError> {
        if !std::ptr::eq(self, qualified.authority)
            || !Arc::ptr_eq(
                &qualified.creation,
                qualified.request.committed_source().creation_identity(),
            )
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(())
    }

    pub(crate) fn project_bind_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        qualified: &QualifiedBindReply<'_>,
        permit: &ReplyBuildPermit,
    ) -> Result<Zeroizing<Vec<u8>>, SettingsReplyProjectionError> {
        let committed = qualified.request.committed_source();
        permit.check_bind(&committed)?;
        self.recheck_bind_reply_on(store, transaction, qualified)?;
        let source = committed.authority_source();
        let in_reply_to: MessageId = source
            .receipt_metadata()?
            .message_id
            .parse()
            .map_err(SettingsServiceError::from)?;
        let intent = qualified.request.binding_intent();
        let packet = SettingsBindReplyPacket::new(
            intent.host_id,
            in_reply_to,
            intent.binding_request_id,
            SettingsParentReference {
                parent_id: qualified.parent.id(),
                generation: intent.generation.clone(),
            },
            qualified.parent.expires_at(),
        )?;
        packet
            .encode_checked()
            .map_err(SettingsReplyProjectionError::from)
    }

    fn observe_bind_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsBind,
        parent: &PairedSettingsParent,
    ) -> Result<BindReplyObservation, SettingsReplyProjectionError> {
        let read = self.read_bind_reply_on(store, connection, mutation, request, parent)?;
        check_bind_reservation(connection, request, read.wall)?;
        self.finish_bind_reply_observation(request, parent, read)
    }

    fn read_bind_reply_on(
        &self,
        store: &Store,
        connection: &Connection,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsBind,
        parent: &PairedSettingsParent,
    ) -> Result<BindReplyRead, SettingsReplyProjectionError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped.into());
        }
        let source = request.authority_source();
        let committed = request.committed_source();
        let lifetime = committed.lifetime();
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.core.mutation)
            || !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
            || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let snapshot = request.binding_snapshot()?;
        let intent = request.binding_intent();
        if snapshot.state != BindingHistoryState::Committed
            || snapshot.intent != *intent
            || snapshot.lifetime.is_none_or(|actual| {
                actual.expires_at != lifetime.expires_at || actual.deadline != lifetime.deadline
            })
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let (owner, first_wall, first_monotonic) = self.core.authority.observe_paired_bind_reply(
            &self.host,
            request,
            parent,
            &self.core.application_stop,
        )?;
        let head = settings_grant_head_on(connection, owner, source.verified_peer_keys().device_id)
            .map_err(SettingsServiceError::from)?
            .ok_or(SettingsServiceError::OutsideScope)?;
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
            || first_wall < head.grant.change.target.issued_at
            || first_wall >= head.grant.change.target.expires_at
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let expected_lifetime =
            source.parent_lifetime(intent, head.grant.change.target.expires_at)?;
        if expected_lifetime.expires_at != lifetime.expires_at
            || expected_lifetime.deadline != lifetime.deadline
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        let reply = committed.reply_reservation();
        let reply_deadline = lifetime
            .deadline
            .checked_sub(Duration::from_millis(
                lifetime
                    .expires_at
                    .get()
                    .checked_sub(reply.expires_at.get())
                    .ok_or(SettingsServiceError::Expired)?,
            ))
            .ok_or(SettingsServiceError::Expired)?;
        Ok(BindReplyRead {
            owner,
            reply_deadline,
            grant_expires_at: head.grant.change.target.expires_at,
            wall: first_wall,
            monotonic: first_monotonic,
        })
    }

    fn finish_bind_reply_observation(
        &self,
        request: &OwnedCommittedSettingsBind,
        parent: &PairedSettingsParent,
        read: BindReplyRead,
    ) -> Result<BindReplyObservation, SettingsReplyProjectionError> {
        let source = request.authority_source();
        let committed = request.committed_source();
        let reply = committed.reply_reservation();
        let (checked_owner, wall, monotonic) = self.core.authority.observe_paired_bind_reply(
            &self.host,
            request,
            parent,
            &self.core.application_stop,
        )?;
        if checked_owner != read.owner
            || wall < read.wall
            || monotonic < read.monotonic
            || wall >= read.grant_expires_at
            || wall >= reply.expires_at
            || monotonic >= read.reply_deadline
            || self.core.application_stop.is_cancelled()
            || !source.connection().is_live()
            || !source.matches_context(&self.core.agent, &self.core.endpoint, &self.core.mutation)
        {
            return Err(SettingsServiceError::OutsideScope.into());
        }
        Ok(BindReplyObservation {
            reply_deadline: read.reply_deadline,
            wall,
            monotonic,
        })
    }
}

fn check_bind_reply_observation(
    qualified: &QualifiedBindReply<'_>,
    observed: BindReplyObservation,
) -> Result<(), SettingsReplyProjectionError> {
    if observed.reply_deadline != qualified.reply_deadline
        || observed.wall < qualified.observed_wall
        || observed.monotonic < qualified.observed_monotonic
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    Ok(())
}

fn remote_phase(
    phase: &SettingsOperationPhase,
) -> Result<RemoteOperationPhase, SettingsServiceError> {
    Ok(match phase {
        SettingsOperationPhase::Applying {
            expected_revision,
            revision,
        } => RemoteOperationPhase::Applying {
            expected_revision: *expected_revision,
            revision: *revision,
        },
        SettingsOperationPhase::Finished { result } => RemoteOperationPhase::Finished {
            state: match result.state {
                SettingsOperationState::Applied { revision } => {
                    RemoteOperationState::Applied { revision }
                }
                SettingsOperationState::PendingApplication { revision } => {
                    RemoteOperationState::PendingApplication { revision }
                }
                SettingsOperationState::RestartRequired { revision } => {
                    RemoteOperationState::RestartRequired { revision }
                }
                SettingsOperationState::Conflict { observed_revision } => {
                    RemoteOperationState::Conflict { observed_revision }
                }
                SettingsOperationState::HostActionRequired => {
                    RemoteOperationState::HostActionRequired
                }
                SettingsOperationState::Unavailable { reason } => {
                    RemoteOperationState::Unavailable { reason }
                }
                SettingsOperationState::Failed { reason } => {
                    RemoteOperationState::Failed { reason }
                }
                SettingsOperationState::Unknown => RemoteOperationState::Unknown,
                SettingsOperationState::Cancelled => RemoteOperationState::Cancelled,
                SettingsOperationState::Completed { .. }
                | SettingsOperationState::RequiresConfirmation { .. }
                | SettingsOperationState::RequiresUserAction { .. } => {
                    return Err(SettingsServiceError::CorruptJournal);
                }
            },
        },
        _ => return Err(SettingsServiceError::CorruptJournal),
    })
}

#[derive(Serialize)]
struct BindReplyProvenance<'keys> {
    side: &'static str,
    version: u16,
    local_id: DeviceId,
    peer_keys: &'keys DevicePublicKeys,
    host_epoch: Uuid,
    connection_epoch: Uuid,
    grant: SettingsGrantReference,
    parent_id: Option<Uuid>,
}

fn check_bind_reservation_identity(
    request: &OwnedCommittedSettingsBind,
    now: UnixMillis,
) -> Result<(), SettingsReplyProjectionError> {
    let source = request.authority_source();
    let intent = request.binding_intent();
    let committed = request.committed_source();
    let reply = committed.reply_reservation();
    let expected_expiry = reply
        .created_at
        .get()
        .checked_add(60_000)
        .ok_or(SettingsServiceError::Expired)?
        .min(committed.lifetime().expires_at.get());
    let receipt = source.receipt_metadata()?;
    let receipt_floor =
        u64::try_from(receipt.received_at).map_err(|_| SettingsServiceError::CorruptJournal)?;
    if reply.reservation_id.is_nil()
        || reply.local_device_id != intent.host_id
        || reply.peer_device_id != source.verified_peer_keys().device_id
        || reply.provenance.len() > 2048
        || reply.created_at.get() < receipt_floor
        || reply.created_at.get() < intent.issued_at
        || reply.created_at.get() >= intent.expires_at
        || reply.created_at > now
        || reply.created_at >= reply.expires_at
        || reply.expires_at.get() != expected_expiry
        || now >= reply.expires_at
    {
        return Err(SettingsServiceError::OutsideScope.into());
    }
    let expected_provenance = serde_json::to_string(&BindReplyProvenance {
        side: "host",
        version: 1,
        local_id: intent.host_id,
        peer_keys: source.verified_peer_keys(),
        host_epoch: intent.generation.host_epoch,
        connection_epoch: intent.generation.connection_epoch,
        grant: SettingsGrantReference {
            grant_id: intent.generation.grant_id,
            revision: intent.generation.grant_revision,
        },
        parent_id: None,
    })
    .map_err(SettingsReplyProjectionError::Provenance)?;
    if reply.provenance != expected_provenance {
        return Err(SettingsServiceError::CorruptJournal.into());
    }
    Ok(())
}

fn check_bind_reservation(
    connection: &Connection,
    request: &OwnedCommittedSettingsBind,
    now: UnixMillis,
) -> Result<(), SettingsReplyProjectionError> {
    check_bind_reservation_identity(request, now)?;
    let committed = request.committed_source();
    let reply = committed.reply_reservation();
    let mut statement = connection.prepare("SELECT reservation_id,local_device_id,peer_device_id,provenance,family,reserved_bytes,created_at,expires_at FROM settings_transport_reply_reservations WHERE reservation_id=?1")
        .map_err(SettingsServiceError::from)?;
    let mut rows = statement
        .query([reply.reservation_id.to_string()])
        .map_err(SettingsServiceError::from)?;
    let row = rows
        .next()
        .map_err(SettingsServiceError::from)?
        .ok_or(SettingsServiceError::NotFound)?;
    let created_at =
        i64::try_from(reply.created_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    let expires_at =
        i64::try_from(reply.expires_at.get()).map_err(|_| SettingsServiceError::CorruptJournal)?;
    if original_reply_text(row, 0, 36)? != reply.reservation_id.to_string()
        || original_reply_text(row, 1, 36)? != reply.local_device_id.to_string()
        || original_reply_text(row, 2, 36)? != reply.peer_device_id.to_string()
        || original_reply_text(row, 3, 2048)? != reply.provenance
        || original_reply_text(row, 4, 7)? != "default"
        || row.get::<_, i64>(5).map_err(SettingsServiceError::from)? != 49_152
        || row.get::<_, i64>(6).map_err(SettingsServiceError::from)? != created_at
        || row.get::<_, i64>(7).map_err(SettingsServiceError::from)? != expires_at
    {
        return Err(SettingsServiceError::CorruptJournal.into());
    }
    Ok(())
}
