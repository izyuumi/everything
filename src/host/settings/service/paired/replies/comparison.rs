use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use rusqlite::Transaction;
use tokio::sync::OwnedMutexGuard;

use crate::devices::{
    OwnedQueuedSettingsReply, SettingsReplyBuildRequest, SettingsReplyPublicationError,
    SettingsReplyQualification,
};
use crate::host::settings::contract::SettingKey;
use crate::host::settings::operations::PairedReplyHistoryAnchor;
use crate::protocol::{MessageId, UnixMillis};
use crate::storage::settings_originals::{
    RemoteCloseReason, RemoteOriginalIdentity, RemoteOriginalLink, RemoteOriginalTerminal,
    RemotePublicCommand,
};
use crate::storage::{ConsumedSettingsReply, Store};

use super::super::access::AccessReplyComparison;
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
    pub(crate) fn recheck_queued_reply_on(
        &self,
        store: &Store,
        transaction: &Transaction<'_>,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
    ) -> Result<(), SettingsQueuedReplyCheckError> {
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
        };
        Ok(SettingsReplyComparison {
            message_id: sealed.header().message_id,
            encrypted_sha256: *sealed.encrypted_sha256(),
            observation,
        })
    }
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
