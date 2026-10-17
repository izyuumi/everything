use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Transaction;
use tokio::sync::OwnedMutexGuard;

use crate::devices::{
    BindingHistoryState, OwnedQueuedSettingsReply, SettingsReplyBuildRequest,
    SettingsReplyPublicationError, SettingsReplyQualification,
};
use crate::host::settings::contract::SettingKey;
use crate::host::settings::operations::PairedReplyHistoryAnchor;
use crate::protocol::{MessageId, UnixMillis};
use crate::storage::settings_originals::{
    RemoteCloseReason, RemoteOriginalIdentity, RemoteOriginalLink, RemoteOriginalTerminal,
    RemotePublicCommand,
};
use crate::storage::{CommittedSettingsReply, ConsumedSettingsReply, Store};

use super::super::access::AccessReplyComparison;
use super::super::read_replies::{InspectionReplyComparison, SnapshotReplyComparison};
use super::super::{
    OriginalReplySource, QualifiedOriginalReply, check_original_reply_reservation_identity,
};
use super::{
    SettingsQueuedReplyCheckError, SettingsReceiptAuthority, SettingsReplyProjectionError,
    SettingsServiceError, check_bind_reservation_identity,
};

pub(crate) struct SettingsReplyComparison {
    message_id: MessageId,
    encrypted_sha256: [u8; 32],
    observation: ReplyComparisonObservation,
}

enum ReplyComparisonObservation {
    Access(AccessReplyComparison),
    Snapshot(SnapshotReplyComparison),
    Inspection(Box<InspectionReplyComparison>),
    Original {
        invocation: Arc<()>,
        linked: bool,
        close: Option<OriginalCloseFloor>,
        terminal: Option<RemoteOriginalTerminal>,
        disclosed: DisclosedSettingsKeys,
        operation: Option<PairedReplyHistoryAnchor>,
        wall: UnixMillis,
        monotonic: Instant,
    },
    Bind {
        creation: Arc<()>,
        wall: UnixMillis,
        monotonic: Instant,
    },
}

struct OriginalCloseFloor {
    requested_at: UnixMillis,
    reason: RemoteCloseReason,
}

struct DisclosedSettingsKeys {
    embeddings_enabled: bool,
    embeddings_dimensions: bool,
}

impl SettingsReceiptAuthority {
    pub(crate) fn recheck_queued_socket_lifetime(
        &self,
        store: &Store,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
    ) -> Result<(), SettingsQueuedReplyCheckError> {
        let (request, committed) = checked_queued_reply(queued)?;
        let comparison = committed.comparison();
        let header = committed.header();
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsReplyProjectionError::from(SettingsServiceError::Stopped).into());
        }
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.core.mutation)
            || !self.core.agent.store().shares_connection(store)
        {
            return Err(
                SettingsReplyProjectionError::from(SettingsServiceError::OutsideScope).into(),
            );
        }
        match (request, &comparison.observation) {
            (
                SettingsReplyBuildRequest::Fresh(_) | SettingsReplyBuildRequest::Historical(_),
                ReplyComparisonObservation::Original {
                    invocation,
                    wall,
                    monotonic,
                    ..
                },
            ) => {
                let subject = match request {
                    SettingsReplyBuildRequest::Fresh(request) => {
                        OriginalReplySource::Fresh(request.committed_source())
                    }
                    SettingsReplyBuildRequest::Historical(request) => {
                        OriginalReplySource::Historical(request)
                    }
                    _ => return Err(SettingsReplyPublicationError::Conflict.into()),
                };
                let source = subject.authenticated();
                if !Arc::ptr_eq(invocation, source.invocation_identity()) {
                    return Err(SettingsReplyPublicationError::Conflict.into());
                }
                if !std::ptr::eq(self, subject.receipt_authority())
                    || !source.matches_context(
                        &self.core.agent,
                        &self.core.endpoint,
                        &self.core.mutation,
                    )
                    || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                store
                    .check_connection_health()
                    .map_err(SettingsReplyPublicationError::from)?;
                let original = subject.original();
                let RemoteOriginalIdentity::Accepted(identity) = &original.identity else {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                };
                let floor = subject.receipt_floor();
                let deadline = floor
                    .1
                    .checked_add(Duration::from_millis(
                        identity
                            .expires_at
                            .get()
                            .checked_sub(floor.0.get())
                            .ok_or(SettingsServiceError::Expired)
                            .map_err(SettingsReplyProjectionError::from)?,
                    ))
                    .ok_or(SettingsServiceError::Expired)
                    .map_err(SettingsReplyProjectionError::from)?;
                if deadline != subject.original_deadline()
                    || original.key != original.identity.key()
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                check_original_reply_reservation_identity(&subject, *wall)
                    .map_err(SettingsReplyProjectionError::from)?;
                queued.request()?;
                queued.committed()?;
                let (_, observed_wall, observed_monotonic) = self
                    .core
                    .authority
                    .observe_paired_reply(
                        &self.host,
                        source,
                        floor,
                        deadline,
                        &self.core.application_stop,
                    )
                    .map_err(SettingsReplyProjectionError::from)?;
                if observed_wall < *wall
                    || observed_monotonic < *monotonic
                    || observed_wall.get() < header.created_at_ms
                    || observed_wall.get() >= header.expires_at_ms
                    || self.core.application_stop.is_cancelled()
                    || !source.connection().is_live()
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                Ok(())
            }
            (
                SettingsReplyBuildRequest::Bind { parent, request },
                ReplyComparisonObservation::Bind {
                    creation,
                    wall,
                    monotonic,
                },
            ) => {
                let source = request.authority_source();
                let committed = request.committed_source();
                let lifetime = committed.lifetime();
                if !Arc::ptr_eq(creation, committed.creation_identity()) {
                    return Err(SettingsReplyPublicationError::Conflict.into());
                }
                if !source.matches_context(
                    &self.core.agent,
                    &self.core.endpoint,
                    &self.core.mutation,
                ) || !Arc::ptr_eq(source.connection().host_identity(), &self.host)
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                store
                    .check_connection_health()
                    .map_err(SettingsReplyPublicationError::from)?;
                let snapshot = request
                    .binding_snapshot()
                    .map_err(SettingsReplyProjectionError::from)?;
                if snapshot.state != BindingHistoryState::Committed
                    || snapshot.intent != *request.binding_intent()
                    || snapshot.lifetime.is_none_or(|actual| {
                        actual.expires_at != lifetime.expires_at
                            || actual.deadline != lifetime.deadline
                    })
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                check_bind_reservation_identity(request, *wall)?;
                let reply = committed.reply_reservation();
                let deadline = lifetime
                    .deadline
                    .checked_sub(Duration::from_millis(
                        lifetime
                            .expires_at
                            .get()
                            .checked_sub(reply.expires_at.get())
                            .ok_or(SettingsServiceError::Expired)
                            .map_err(SettingsReplyProjectionError::from)?,
                    ))
                    .ok_or(SettingsServiceError::Expired)
                    .map_err(SettingsReplyProjectionError::from)?;
                queued.request()?;
                queued.committed()?;
                let (_, observed_wall, observed_monotonic) = self
                    .core
                    .authority
                    .observe_paired_bind_reply(
                        &self.host,
                        request,
                        parent,
                        &self.core.application_stop,
                    )
                    .map_err(SettingsReplyProjectionError::from)?;
                if observed_wall < *wall
                    || observed_monotonic < *monotonic
                    || observed_wall.get() < header.created_at_ms
                    || observed_wall >= reply.expires_at
                    || observed_monotonic >= deadline
                    || self.core.application_stop.is_cancelled()
                    || !source.connection().is_live()
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                Ok(())
            }
            (SettingsReplyBuildRequest::Access(_), ReplyComparisonObservation::Access(access)) => {
                self.recheck_queued_access_socket_lifetime(store, mutation, queued, access)
            }
            (
                SettingsReplyBuildRequest::Snapshot(_),
                ReplyComparisonObservation::Snapshot(snapshot),
            ) => self.recheck_queued_snapshot_socket_lifetime(store, mutation, queued, snapshot),
            (
                SettingsReplyBuildRequest::Inspection(_),
                ReplyComparisonObservation::Inspection(inspection),
            ) => {
                self.recheck_queued_inspection_socket_lifetime(store, mutation, queued, inspection)
            }
            _ => Err(SettingsReplyPublicationError::Conflict.into()),
        }
    }

    pub(crate) fn recheck_queued_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
    ) -> Result<(), SettingsQueuedReplyCheckError> {
        let (request, committed) = checked_queued_reply(queued)?;
        let comparison = committed.comparison();
        let header = committed.header();
        match (request, &comparison.observation) {
            (
                SettingsReplyBuildRequest::Fresh(_) | SettingsReplyBuildRequest::Historical(_),
                ReplyComparisonObservation::Original { .. },
            ) => self.recheck_queued_original_reply_on(
                store,
                transaction,
                mutation,
                queued,
                &comparison.observation,
            ),
            (
                SettingsReplyBuildRequest::Bind { parent, request },
                ReplyComparisonObservation::Bind {
                    creation,
                    wall,
                    monotonic,
                },
            ) => {
                if !Arc::ptr_eq(creation, request.committed_source().creation_identity()) {
                    return Err(SettingsReplyPublicationError::Conflict.into());
                }
                let read =
                    self.read_bind_reply_on(store, transaction, mutation, request, parent)?;
                check_bind_reservation_identity(request, read.wall)?;
                if read.wall < *wall
                    || read.monotonic < *monotonic
                    || read.wall.get() < header.created_at_ms
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                queued.request()?;
                queued.committed()?;
                let observed = self.finish_bind_reply_observation(request, parent, read)?;
                if observed.wall < *wall
                    || observed.monotonic < *monotonic
                    || observed.wall.get() < header.created_at_ms
                {
                    return Err(SettingsReplyProjectionError::from(
                        SettingsServiceError::OutsideScope,
                    )
                    .into());
                }
                Ok(())
            }
            (SettingsReplyBuildRequest::Access(_), ReplyComparisonObservation::Access(access)) => {
                self.recheck_queued_access_reply_on(store, transaction, mutation, queued, access)
            }
            (
                SettingsReplyBuildRequest::Snapshot(_),
                ReplyComparisonObservation::Snapshot(snapshot),
            ) => self.recheck_queued_snapshot_reply_on(
                store,
                transaction,
                mutation,
                queued,
                snapshot,
            ),
            (
                SettingsReplyBuildRequest::Inspection(_),
                ReplyComparisonObservation::Inspection(inspection),
            ) => self.recheck_queued_inspection_reply_on(
                store,
                transaction,
                mutation,
                queued,
                inspection,
            ),
            _ => Err(SettingsReplyPublicationError::Conflict.into()),
        }
    }

    fn recheck_queued_original_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
        comparison: &ReplyComparisonObservation,
    ) -> Result<(), SettingsQueuedReplyCheckError> {
        let subject = match queued.request()? {
            SettingsReplyBuildRequest::Fresh(request) => {
                OriginalReplySource::Fresh(request.committed_source())
            }
            SettingsReplyBuildRequest::Historical(request) => {
                OriginalReplySource::Historical(request)
            }
            _ => return Err(SettingsReplyPublicationError::Conflict.into()),
        };
        let ReplyComparisonObservation::Original {
            invocation,
            linked,
            close,
            terminal,
            disclosed,
            operation,
            wall,
            monotonic,
        } = comparison
        else {
            return Err(SettingsReplyPublicationError::Conflict.into());
        };
        if !Arc::ptr_eq(invocation, subject.authenticated().invocation_identity()) {
            return Err(SettingsReplyPublicationError::Conflict.into());
        }
        let header = queued.committed()?.header();
        let read = self
            .read_original_reply_on(store, transaction, mutation, &subject)
            .map_err(SettingsReplyProjectionError::from)?;
        check_original_reply_reservation_identity(&subject, read.wall)
            .map_err(SettingsReplyProjectionError::from)?;
        if *linked != operation.is_some()
            || close.as_ref().is_some_and(|floor| {
                read.original.close.as_ref().is_none_or(|current| {
                    current.requested_at != floor.requested_at || current.reason != floor.reason
                })
            })
            || terminal.is_some_and(|floor| read.original.terminal != Some(floor))
            || !disclosed.embeddings_enabled && !disclosed.embeddings_dimensions
        {
            return Err(
                SettingsReplyProjectionError::from(SettingsServiceError::CorruptJournal).into(),
            );
        }
        if disclosed.embeddings_enabled && !read.readable_enabled
            || disclosed.embeddings_dimensions && !read.readable_dimensions
            || read.wall < *wall
            || read.monotonic < *monotonic
            || read.wall.get() < header.created_at_ms
        {
            return Err(
                SettingsReplyProjectionError::from(SettingsServiceError::OutsideScope).into(),
            );
        }
        match (&read.original.link, &read.operation, operation) {
            (None, None, None) => {}
            (Some(RemoteOriginalLink::Operation { .. }), Some(current), Some(anchor)) => {
                anchor
                    .check_current_on(transaction, current)
                    .map_err(SettingsReplyProjectionError::from)?;
            }
            (Some(RemoteOriginalLink::Operation { .. }), Some(current), None) => {
                current
                    .check_new_paired_reply_history_on(transaction)
                    .map_err(SettingsReplyProjectionError::from)?;
            }
            _ => {
                return Err(SettingsReplyProjectionError::from(
                    SettingsServiceError::CorruptJournal,
                )
                .into());
            }
        }
        queued.request()?;
        queued.committed()?;
        let observed = self
            .finish_original_reply_observation(&subject, read)
            .map_err(SettingsReplyProjectionError::from)?;
        if observed.wall < *wall
            || observed.monotonic < *monotonic
            || observed.wall.get() < header.created_at_ms
        {
            return Err(
                SettingsReplyProjectionError::from(SettingsServiceError::OutsideScope).into(),
            );
        }
        Ok(())
    }

    pub(crate) fn capture_reply_comparison_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        consumed: &ConsumedSettingsReply<'_, '_, '_, '_>,
    ) -> Result<SettingsReplyComparison, SettingsReplyPublicationError> {
        self.recheck_consumed_reply_on(store, transaction, consumed)?;
        let sealed = consumed.sealed();
        let observation = match sealed.qualification() {
            SettingsReplyQualification::Original(qualified) => {
                capture_original_on(transaction, qualified)
                    .map_err(SettingsReplyProjectionError::from)?
            }
            SettingsReplyQualification::Bind(qualified) => ReplyComparisonObservation::Bind {
                creation: Arc::clone(&qualified.creation),
                wall: qualified.observed_wall,
                monotonic: qualified.observed_monotonic,
            },
            SettingsReplyQualification::Access(qualified) => ReplyComparisonObservation::Access(
                self.capture_access_reply_comparison(qualified)
                    .map_err(SettingsReplyProjectionError::from)?,
            ),
            SettingsReplyQualification::Snapshot(qualified) => {
                ReplyComparisonObservation::Snapshot(
                    self.capture_snapshot_reply_comparison(transaction, qualified)
                        .map_err(SettingsReplyProjectionError::from)?,
                )
            }
            SettingsReplyQualification::Inspection(qualified) => {
                ReplyComparisonObservation::Inspection(Box::new(
                    self.capture_inspection_reply_comparison(transaction, qualified)
                        .map_err(SettingsReplyProjectionError::from)?,
                ))
            }
        };
        Ok(SettingsReplyComparison {
            message_id: sealed.header().message_id,
            encrypted_sha256: *sealed.encrypted_sha256(),
            observation,
        })
    }
}

fn checked_queued_reply(
    queued: &OwnedQueuedSettingsReply,
) -> Result<(&SettingsReplyBuildRequest, &CommittedSettingsReply), SettingsQueuedReplyCheckError> {
    let request = queued.request()?;
    let committed = queued.committed()?;
    let comparison = committed.comparison();
    let header = committed.header();
    let (local, peer, created_at, expires_at) = match request {
        SettingsReplyBuildRequest::Fresh(request) => {
            let source = request.committed_source();
            let reply = source.reply_reservation();
            (
                reply.local_device_id,
                reply.peer_device_id,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Historical(request) => {
            let reply = request.reply_reservation();
            (
                reply.local_device_id,
                reply.peer_device_id,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Bind { request, .. } => {
            let source = request.committed_source();
            let reply = source.reply_reservation();
            (
                reply.local_device_id,
                reply.peer_device_id,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Access(request) => {
            let reply = request.reply_reservation();
            (
                reply.local_device_id,
                reply.peer_device_id,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Snapshot(request) => {
            let reply = request.read_owner().reply_reservation();
            (
                reply.local_device_id,
                reply.peer_device_id,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Inspection(request) => {
            let reply = request.read_owner().reply_reservation();
            (
                reply.local_device_id,
                reply.peer_device_id,
                reply.created_at,
                reply.expires_at,
            )
        }
    };
    if comparison.message_id != header.message_id
        || comparison.encrypted_sha256 != *committed.encrypted_sha256()
        || header.sender != local
        || header.recipient != peer
        || header.created_at_ms < created_at.get()
        || header.created_at_ms >= header.expires_at_ms
        || header.expires_at_ms != expires_at.get()
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    Ok((request, committed))
}

fn capture_original_on(
    transaction: &Transaction<'_>,
    qualified: &QualifiedOriginalReply<'_>,
) -> Result<ReplyComparisonObservation, SettingsServiceError> {
    let RemoteOriginalIdentity::Accepted(original) = &qualified.original.identity else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    let RemotePublicCommand::Update {
        enabled,
        dimensions,
        ..
    } = &original.command
    else {
        return Err(SettingsServiceError::CorruptJournal);
    };
    let mut disclosed = DisclosedSettingsKeys {
        embeddings_enabled: enabled.is_some(),
        embeddings_dimensions: dimensions.is_some(),
    };
    let operation = match (&qualified.original.link, &qualified.operation) {
        (None, None) => None,
        (Some(RemoteOriginalLink::Operation { operation_id }), Some(operation))
            if *operation_id == original.request_id
                && operation.identity.binding.request_id == original.request_id =>
        {
            if let Some(write) = operation.write() {
                for key in &write.intent.changed_keys {
                    match key {
                        SettingKey::EmbeddingsEnabled => disclosed.embeddings_enabled = true,
                        SettingKey::EmbeddingsDimensions => disclosed.embeddings_dimensions = true,
                        _ => return Err(SettingsServiceError::CorruptJournal),
                    }
                }
            }
            Some(PairedReplyHistoryAnchor::capture_on(
                transaction,
                operation,
            )?)
        }
        _ => return Err(SettingsServiceError::CorruptJournal),
    };
    if !disclosed.embeddings_enabled && !disclosed.embeddings_dimensions {
        return Err(SettingsServiceError::CorruptJournal);
    }
    Ok(ReplyComparisonObservation::Original {
        invocation: Arc::clone(&qualified.invocation),
        linked: qualified.original.link.is_some(),
        close: qualified
            .original
            .close
            .as_ref()
            .map(|close| OriginalCloseFloor {
                requested_at: close.requested_at,
                reason: close.reason,
            }),
        terminal: qualified.original.terminal,
        disclosed,
        operation,
        wall: qualified.observed_wall,
        monotonic: qualified.observed_monotonic,
    })
}

impl fmt::Debug for SettingsReplyComparison {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyComparison([sealed comparison only])")
    }
}
