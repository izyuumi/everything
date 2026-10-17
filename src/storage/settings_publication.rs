use std::any::Any;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use rusqlite::{DropBehavior, Row, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::crypto::{EncryptedEnvelope, EnvelopeHeader};
use crate::devices::{
    DeviceEndpoint, EndpointRole, OwnedQueuedSettingsReply, ReturnedSettingsReplySource,
    SealedSettingsReply, SettingsReplyBuildRequest, SettingsReplyPublicationError,
    SettingsReplySealContext,
};
use crate::host::settings::service::{
    OriginalReplySource, SettingsAccessReplyFamily, SettingsQueuedReplyCheckError,
    SettingsReceiptAuthority, SettingsReplyComparison, SettingsReplyProjectionError,
};
use crate::protocol::{DeviceId, UnixMillis};
use crate::relay_protocol::{RELAY_SCHEMA_VERSION, RelayClientMessage, RelayProtocolError};

use super::{
    Store, TransportOutboundCandidate, TransportOutboundRouting, consume_settings_reply_in,
};

pub(crate) struct CommittedSettingsReply {
    header: EnvelopeHeader,
    signature: [u8; 64],
    encrypted_sha256: [u8; 32],
    envelope_bytes: usize,
    comparison: SettingsReplyComparison,
}

#[derive(Error)]
pub(crate) enum SettingsReplyPublishFailure {
    #[error("Settings reply failed before opening its publication transaction")]
    BeforeTransaction {
        #[source]
        cause: SettingsReplyPublicationError,
    },
    #[error("Settings reply publication failed and was rolled back")]
    RolledBack {
        #[source]
        cause: SettingsReplyPublicationError,
    },
    #[error("Settings reply publication failed and rollback failed")]
    RollbackFailed {
        #[source]
        cause: SettingsReplyPublicationError,
        rollback: rusqlite::Error,
    },
    #[error("Settings reply publication commit outcome is uncertain")]
    CommitUncertain {
        #[source]
        commit: rusqlite::Error,
        rollback: Result<(), rusqlite::Error>,
    },
    #[error("Settings reply publication panicked and its outcome is uncertain")]
    Panicked {
        payload: Box<dyn Any + Send>,
        rollback: Result<(), rusqlite::Error>,
    },
}

enum PublicationAttemptFailure {
    Publication(Box<SettingsReplyPublicationError>),
    Commit(rusqlite::Error),
}

impl CommittedSettingsReply {
    pub(crate) fn header(&self) -> &EnvelopeHeader {
        &self.header
    }

    pub(crate) fn comparison(&self) -> &SettingsReplyComparison {
        &self.comparison
    }

    pub(crate) fn encrypted_sha256(&self) -> &[u8; 32] {
        &self.encrypted_sha256
    }
}

impl Store {
    pub(crate) fn publish_settings_reply(
        &self,
        context: SettingsReplySealContext<'_>,
        request: &SettingsReplyBuildRequest,
    ) -> Result<CommittedSettingsReply, Box<SettingsReplyPublishFailure>> {
        if !self.shares_connection(context.store) {
            return Err(Box::new(SettingsReplyPublishFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::Conflict,
            }));
        }
        let mut main = self.connection().map_err(|cause| {
            Box::new(SettingsReplyPublishFailure::BeforeTransaction {
                cause: cause.into(),
            })
        })?;
        let transaction = main
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|cause| {
                Box::new(SettingsReplyPublishFailure::BeforeTransaction {
                    cause: cause.into(),
                })
            })?;
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            publish_on(self, &transaction, context, request)
        }));
        match attempt {
            Ok(Ok(committed)) => {
                drop(transaction);
                Ok(committed)
            }
            Ok(Err(PublicationAttemptFailure::Publication(cause))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(match rollback {
                    Ok(()) => SettingsReplyPublishFailure::RolledBack { cause: *cause },
                    Err(rollback) => SettingsReplyPublishFailure::RollbackFailed {
                        cause: *cause,
                        rollback,
                    },
                }))
            }
            Ok(Err(PublicationAttemptFailure::Commit(commit))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsReplyPublishFailure::CommitUncertain {
                    commit,
                    rollback,
                }))
            }
            Err(payload) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsReplyPublishFailure::Panicked {
                    payload,
                    rollback,
                }))
            }
        }
    }
}

fn publish_on(
    store: &Store,
    transaction: &Transaction<'_>,
    context: SettingsReplySealContext<'_>,
    request: &SettingsReplyBuildRequest,
) -> Result<CommittedSettingsReply, PublicationAttemptFailure> {
    match request {
        SettingsReplyBuildRequest::Fresh(request) => {
            let source = request.committed_source();
            let authority = source.receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_original_reply_on(
                    store,
                    transaction,
                    context.mutation,
                    OriginalReplySource::Fresh(request.committed_source()),
                )
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed = SealedSettingsReply::original(
                context,
                transaction,
                &qualified,
                OriginalReplySource::Fresh(request.committed_source()),
            )?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Historical(request) => {
            let authority = request.receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_original_reply_on(
                    store,
                    transaction,
                    context.mutation,
                    OriginalReplySource::Historical(request),
                )
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed = SealedSettingsReply::original(
                context,
                transaction,
                &qualified,
                OriginalReplySource::Historical(request),
            )?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Bind { parent, request } => {
            let authority = context.authority;
            let qualified = authority.qualify_bind_reply_on(
                store,
                transaction,
                context.mutation,
                request,
                parent,
            )?;
            let sealed = SealedSettingsReply::bind(context, transaction, &qualified, request)?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Access(request) => {
            let authority = request.verified_request().receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_access_reply_on(store, transaction, context.mutation, request)
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed = SealedSettingsReply::access(context, transaction, &qualified, request)?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Snapshot(request) => {
            let authority = request.read_owner().verified_request().receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_read_reply_on(store, transaction, context.mutation, request.read_owner())
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed = SealedSettingsReply::snapshot(context, transaction, &qualified, request)?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
        SettingsReplyBuildRequest::Inspection(request) => {
            let authority = request.read_owner().verified_request().receipt_authority();
            if !std::ptr::eq(context.authority, authority) {
                return Err(SettingsReplyPublicationError::Conflict.into());
            }
            let qualified = authority
                .qualify_inspection_reply_on(store, transaction, context.mutation, request)
                .map_err(SettingsReplyProjectionError::from)?;
            let sealed =
                SealedSettingsReply::inspection(context, transaction, &qualified, request)?;
            commit_sealed_on(store, transaction, authority, &sealed)
        }
    }
}

fn commit_sealed_on(
    store: &Store,
    transaction: &Transaction<'_>,
    authority: &SettingsReceiptAuthority,
    sealed: &SealedSettingsReply<'_, '_, '_>,
) -> Result<CommittedSettingsReply, PublicationAttemptFailure> {
    let consumed = consume_settings_reply_in(transaction, sealed)?;
    let comparison = authority.capture_reply_comparison_on(store, transaction, &consumed)?;
    let header = sealed.header().clone();
    let signature = *sealed.signature();
    let encrypted_sha256 = *sealed.encrypted_sha256();
    let envelope_bytes = sealed.item().message.envelope.len();
    authority.recheck_consumed_reply_on(store, transaction, &consumed)?;
    transaction
        .execute_batch("COMMIT")
        .map_err(PublicationAttemptFailure::Commit)?;
    Ok(CommittedSettingsReply {
        header,
        signature,
        encrypted_sha256,
        envelope_bytes,
        comparison,
    })
}

fn rollback_publication(
    store: &Store,
    mut transaction: Transaction<'_>,
) -> Result<(), rusqlite::Error> {
    transaction.set_drop_behavior(DropBehavior::Ignore);
    let rollback = transaction.rollback();
    if rollback.is_err() {
        store.quarantine_connection();
    }
    rollback
}

impl From<SettingsReplyPublicationError> for PublicationAttemptFailure {
    fn from(cause: SettingsReplyPublicationError) -> Self {
        Self::Publication(Box::new(cause))
    }
}

impl From<SettingsReplyProjectionError> for PublicationAttemptFailure {
    fn from(cause: SettingsReplyProjectionError) -> Self {
        SettingsReplyPublicationError::from(cause).into()
    }
}

impl fmt::Debug for CommittedSettingsReply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CommittedSettingsReply([observed Main commit only])")
    }
}

impl fmt::Debug for SettingsReplyPublishFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyPublishFailure([retained cause])")
    }
}

#[derive(Error)]
pub(crate) enum SettingsCiphertextLoadCause {
    #[error("Settings ciphertext ownership or current authority check failed")]
    Checked(#[source] SettingsQueuedReplyCheckError),
    #[error("Settings ciphertext relay frame validation failed")]
    Frame(#[source] RelayProtocolError),
}

#[derive(Error)]
pub(crate) enum SettingsCiphertextLoadFailure {
    #[error("Settings ciphertext loading failed before opening its transaction")]
    BeforeTransaction {
        #[source]
        cause: SettingsCiphertextLoadCause,
    },
    #[error("Settings ciphertext loading failed and was rolled back")]
    RolledBack {
        #[source]
        cause: SettingsCiphertextLoadCause,
    },
    #[error("Settings ciphertext loading failed and rollback failed")]
    RollbackFailed {
        #[source]
        cause: SettingsCiphertextLoadCause,
        rollback: rusqlite::Error,
    },
    #[error("Settings ciphertext loading commit outcome is uncertain")]
    CommitUncertain {
        #[source]
        commit: rusqlite::Error,
        rollback: Result<(), rusqlite::Error>,
    },
    #[error("Settings ciphertext loading panicked and its outcome is uncertain")]
    Panicked {
        payload: Box<dyn Any + Send>,
        rollback: Result<(), rusqlite::Error>,
    },
}

enum CiphertextLoadAttemptFailure {
    Load(Box<SettingsCiphertextLoadCause>),
    Commit(rusqlite::Error),
}

const PRIVATE_CIPHERTEXT_QUERY: &str = "SELECT o.id,o.device_id,o.envelope,o.created_at,o.not_before,o.attempts,o.delivered_at,t.message_id,t.expires_at,p.message_id,p.reservation_id,p.local_device_id,p.peer_device_id,p.provenance,p.family,p.envelope_bytes,p.created_at,p.expires_at FROM outbox AS o JOIN transport_outbox AS t ON t.message_id=o.id JOIN settings_transport_private_outbox AS p ON p.message_id=o.id WHERE o.id=?1";

struct QueuedReplyStamp {
    reservation_id: Uuid,
    local_device_id: DeviceId,
    peer_device_id: DeviceId,
    provenance: String,
    family: &'static str,
    maximum_bytes: usize,
    created_at: UnixMillis,
    expires_at: UnixMillis,
}

impl Store {
    pub(crate) fn load_private_settings_ciphertext(
        &self,
        endpoint: &DeviceEndpoint,
        authority: &SettingsReceiptAuthority,
        mutation: &OwnedMutexGuard<()>,
        candidate: &TransportOutboundCandidate,
        queued: &OwnedQueuedSettingsReply,
    ) -> Result<RelayClientMessage, Box<SettingsCiphertextLoadFailure>> {
        if endpoint.role() != &EndpointRole::Host || !endpoint.shares_store(self) {
            return Err(Box::new(SettingsCiphertextLoadFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::Conflict.into(),
            }));
        }
        let mut main = self.connection().map_err(|cause| {
            Box::new(SettingsCiphertextLoadFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::from(cause).into(),
            })
        })?;
        let transaction = main
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|cause| {
                Box::new(SettingsCiphertextLoadFailure::BeforeTransaction {
                    cause: SettingsReplyPublicationError::from(cause).into(),
                })
            })?;
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            load_private_ciphertext_on(
                self,
                &transaction,
                endpoint,
                authority,
                mutation,
                candidate,
                queued,
            )
        }));
        match attempt {
            Ok(Ok(frame)) => {
                drop(transaction);
                Ok(frame)
            }
            Ok(Err(CiphertextLoadAttemptFailure::Load(cause))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(match rollback {
                    Ok(()) => SettingsCiphertextLoadFailure::RolledBack { cause: *cause },
                    Err(rollback) => SettingsCiphertextLoadFailure::RollbackFailed {
                        cause: *cause,
                        rollback,
                    },
                }))
            }
            Ok(Err(CiphertextLoadAttemptFailure::Commit(commit))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsCiphertextLoadFailure::CommitUncertain {
                    commit,
                    rollback,
                }))
            }
            Err(payload) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsCiphertextLoadFailure::Panicked {
                    payload,
                    rollback,
                }))
            }
        }
    }
}

fn load_private_ciphertext_on(
    store: &Store,
    transaction: &Transaction<'_>,
    endpoint: &DeviceEndpoint,
    authority: &SettingsReceiptAuthority,
    mutation: &OwnedMutexGuard<()>,
    candidate: &TransportOutboundCandidate,
    queued: &OwnedQueuedSettingsReply,
) -> Result<RelayClientMessage, CiphertextLoadAttemptFailure> {
    authority.recheck_queued_reply_on(store, transaction, mutation, queued)?;
    let committed = queued
        .committed()
        .map_err(SettingsQueuedReplyCheckError::from)?;
    let stamp = queued_reply_stamp(queued)?;
    let header = committed.header();
    let created_at =
        i64::try_from(header.created_at_ms).map_err(|_| SettingsReplyPublicationError::Conflict)?;
    if !matches!(candidate.routing, TransportOutboundRouting::PrivateSettings)
        || candidate.message_id != header.message_id
        || candidate.recipient != header.recipient
        || candidate.created_at != created_at
        || candidate.not_before < created_at
        || stamp.reservation_id.is_nil()
        || stamp.provenance.len() > 2048
        || stamp.local_device_id != endpoint.public_keys().device_id
        || stamp.local_device_id != header.sender
        || stamp.peer_device_id != header.recipient
        || stamp.created_at.get() > header.created_at_ms
        || stamp.expires_at.get() != header.expires_at_ms
        || header
            .expires_at_ms
            .checked_sub(header.created_at_ms)
            .is_none_or(|lifetime| lifetime == 0 || lifetime > 60_000)
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let now = UnixMillis::now().map_err(SettingsReplyPublicationError::from)?;
    let frame = read_private_ciphertext_on(transaction, candidate, committed, &stamp, now)?;
    if let RelayClientMessage::Send { envelope, .. } = &frame {
        envelope
            .verify_authenticated_sender(&endpoint.public_keys(), header.created_at_ms)
            .map_err(SettingsReplyPublicationError::from)?;
    } else {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let encoded = frame.to_bytes().map_err(|cause| {
        CiphertextLoadAttemptFailure::Load(Box::new(SettingsCiphertextLoadCause::Frame(cause)))
    })?;
    drop(encoded);
    authority.recheck_queued_reply_on(store, transaction, mutation, queued)?;
    transaction
        .execute_batch("COMMIT")
        .map_err(CiphertextLoadAttemptFailure::Commit)?;
    Ok(frame)
}

fn queued_reply_stamp(
    queued: &OwnedQueuedSettingsReply,
) -> Result<QueuedReplyStamp, SettingsQueuedReplyCheckError> {
    reply_request_stamp(queued.request()?).map_err(Into::into)
}

fn reply_request_stamp(
    request: &SettingsReplyBuildRequest,
) -> Result<QueuedReplyStamp, SettingsReplyPublicationError> {
    let (
        reservation_id,
        local_device_id,
        peer_device_id,
        provenance,
        family,
        maximum_bytes,
        created_at,
        expires_at,
    ) = match request {
        SettingsReplyBuildRequest::Fresh(request) => {
            let source = request.committed_source();
            let reply = source.reply_reservation();
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                bounded_reply_provenance(&reply.provenance)?,
                "inspection",
                90_112,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Historical(request) => {
            let reply = request.reply_reservation();
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                bounded_reply_provenance(&reply.provenance)?,
                "inspection",
                90_112,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Bind { request, .. } => {
            let source = request.committed_source();
            let reply = source.reply_reservation();
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                bounded_reply_provenance(&reply.provenance)?,
                "default",
                49_152,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Access(request) => {
            let reply = request.reply_reservation();
            let (family, maximum_bytes) = match reply.family {
                SettingsAccessReplyFamily::Default => ("default", 49_152),
                SettingsAccessReplyFamily::Denial => ("denial", 4_096),
            };
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                bounded_reply_provenance(&reply.provenance)?,
                family,
                maximum_bytes,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Snapshot(request) => {
            let reply = request.read_owner().reply_reservation();
            if reply.family != crate::host::settings::service::SettingsReadReplyFamily::Default {
                return Err(SettingsReplyPublicationError::Conflict);
            }
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                bounded_reply_provenance(&reply.provenance)?,
                "default",
                49_152,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Inspection(request) => {
            let reply = request.read_owner().reply_reservation();
            if reply.family != crate::host::settings::service::SettingsReadReplyFamily::Inspection {
                return Err(SettingsReplyPublicationError::Conflict);
            }
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                bounded_reply_provenance(&reply.provenance)?,
                "inspection",
                90_112,
                reply.created_at,
                reply.expires_at,
            )
        }
    };
    Ok(QueuedReplyStamp {
        reservation_id,
        local_device_id,
        peer_device_id,
        provenance,
        family,
        maximum_bytes,
        created_at,
        expires_at,
    })
}

fn bounded_reply_provenance(provenance: &str) -> Result<String, SettingsReplyPublicationError> {
    if provenance.len() > 2048 {
        return Err(SettingsReplyPublicationError::Conflict);
    }
    Ok(provenance.to_owned())
}

fn read_private_ciphertext_on(
    transaction: &Transaction<'_>,
    candidate: &TransportOutboundCandidate,
    committed: &CommittedSettingsReply,
    stamp: &QueuedReplyStamp,
    now: UnixMillis,
) -> Result<RelayClientMessage, CiphertextLoadAttemptFailure> {
    let reservation_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings_transport_reply_reservations WHERE reservation_id=?1)",
        [stamp.reservation_id.to_string()], |row| row.get(0),
    )?;
    if reservation_exists {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let mut statement = transaction.prepare(PRIVATE_CIPHERTEXT_QUERY)?;
    let message_id = committed.header.message_id.to_string();
    let mut rows = statement.query([&message_id])?;
    let row = rows
        .next()?
        .ok_or(SettingsReplyPublicationError::Conflict)?;
    let bytes = checked_private_ciphertext_row(row, candidate, committed, stamp)?;
    if candidate.not_before
        > i64::try_from(now.get()).map_err(|_| SettingsReplyPublicationError::Conflict)?
        || now.get() < committed.header.created_at_ms
        || now.get() >= committed.header.expires_at_ms
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let envelope = checked_private_envelope(bytes, committed)?;
    Ok(RelayClientMessage::Send {
        schema_version: RELAY_SCHEMA_VERSION,
        envelope,
    })
}

fn ciphertext_text<'row>(
    row: &'row Row<'_>,
    column: usize,
    maximum: usize,
) -> Result<&'row str, SettingsReplyPublicationError> {
    let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(column)? else {
        return Err(SettingsReplyPublicationError::Conflict);
    };
    if bytes.len() > maximum {
        return Err(SettingsReplyPublicationError::Conflict);
    }
    std::str::from_utf8(bytes).map_err(|_| SettingsReplyPublicationError::Conflict)
}

impl From<SettingsReplyPublicationError> for SettingsCiphertextLoadCause {
    fn from(cause: SettingsReplyPublicationError) -> Self {
        Self::Checked(cause.into())
    }
}

impl From<SettingsReplyPublicationError> for CiphertextLoadAttemptFailure {
    fn from(cause: SettingsReplyPublicationError) -> Self {
        Self::Load(Box::new(cause.into()))
    }
}

impl From<SettingsQueuedReplyCheckError> for CiphertextLoadAttemptFailure {
    fn from(cause: SettingsQueuedReplyCheckError) -> Self {
        Self::Load(Box::new(SettingsCiphertextLoadCause::Checked(cause)))
    }
}

impl From<rusqlite::Error> for CiphertextLoadAttemptFailure {
    fn from(cause: rusqlite::Error) -> Self {
        SettingsReplyPublicationError::from(cause).into()
    }
}

impl fmt::Debug for SettingsCiphertextLoadCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsCiphertextLoadCause([retained cause])")
    }
}

impl fmt::Debug for SettingsCiphertextLoadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsCiphertextLoadFailure([retained cause])")
    }
}

#[derive(Error)]
pub(crate) enum SettingsSocketCheckFailure {
    #[error("Settings socket check failed before opening its transaction")]
    BeforeTransaction {
        #[source]
        cause: SettingsQueuedReplyCheckError,
    },
    #[error("Settings socket check failed and was rolled back")]
    RolledBack {
        #[source]
        cause: SettingsQueuedReplyCheckError,
    },
    #[error("Settings socket check failed and rollback failed")]
    RollbackFailed {
        #[source]
        cause: SettingsQueuedReplyCheckError,
        rollback: rusqlite::Error,
    },
    #[error("Settings socket check commit outcome is uncertain")]
    CommitUncertain {
        #[source]
        commit: rusqlite::Error,
        rollback: Result<(), rusqlite::Error>,
    },
    #[error("Settings socket check panicked and its outcome is uncertain")]
    Panicked {
        payload: Box<dyn Any + Send>,
        rollback: Result<(), rusqlite::Error>,
    },
}

enum SocketCheckAttemptFailure {
    Check(SettingsQueuedReplyCheckError),
    Commit(rusqlite::Error),
}

impl Store {
    pub(crate) fn recheck_private_settings_socket(
        &self,
        authority: &SettingsReceiptAuthority,
        mutation: &OwnedMutexGuard<()>,
        queued: &OwnedQueuedSettingsReply,
    ) -> Result<(), Box<SettingsSocketCheckFailure>> {
        let before = |cause: rusqlite::Error| {
            Box::new(SettingsSocketCheckFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::from(cause).into(),
            })
        };
        let mut main = self.connection().map_err(|cause| {
            Box::new(SettingsSocketCheckFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::from(cause).into(),
            })
        })?;
        let transaction = main
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(before)?;
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            authority
                .recheck_queued_reply_on(self, &transaction, mutation, queued)
                .map_err(SocketCheckAttemptFailure::Check)?;
            transaction
                .execute_batch("COMMIT")
                .map_err(SocketCheckAttemptFailure::Commit)
        }));
        match attempt {
            Ok(Ok(())) => {
                drop(transaction);
                Ok(())
            }
            Ok(Err(SocketCheckAttemptFailure::Check(cause))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(match rollback {
                    Ok(()) => SettingsSocketCheckFailure::RolledBack { cause },
                    Err(rollback) => SettingsSocketCheckFailure::RollbackFailed { cause, rollback },
                }))
            }
            Ok(Err(SocketCheckAttemptFailure::Commit(commit))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsSocketCheckFailure::CommitUncertain {
                    commit,
                    rollback,
                }))
            }
            Err(payload) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsSocketCheckFailure::Panicked {
                    payload,
                    rollback,
                }))
            }
        }
    }
}

impl fmt::Debug for SettingsSocketCheckFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsSocketCheckFailure([retained cause])")
    }
}

pub(crate) struct SettingsPrivateOutboxDisposition {
    _kind: PrivateOutboxDispositionKind,
    slot: std::sync::Arc<()>,
    message_id: crate::protocol::MessageId,
    reservation_id: Uuid,
}

enum PrivateOutboxDispositionKind {
    Removed,
    AlreadyAbsent,
}

#[derive(Error)]
pub(crate) enum SettingsPrivateOutboxFailure {
    #[error("Settings disposition failed before its transaction")]
    BeforeTransaction {
        #[source]
        cause: SettingsReplyPublicationError,
    },
    #[error("Settings disposition failed and was rolled back")]
    RolledBack {
        #[source]
        cause: SettingsReplyPublicationError,
    },
    #[error("Settings disposition failed and rollback failed")]
    RollbackFailed {
        #[source]
        cause: SettingsReplyPublicationError,
        rollback: rusqlite::Error,
    },
    #[error("Settings disposition commit outcome is uncertain")]
    CommitUncertain {
        #[source]
        commit: rusqlite::Error,
        rollback: Result<(), rusqlite::Error>,
    },
    #[error("Settings disposition panicked and its outcome is uncertain")]
    Panicked {
        payload: Box<dyn Any + Send>,
        rollback: Result<(), rusqlite::Error>,
    },
}

enum PrivateDispositionAttemptFailure {
    Checked(Box<SettingsReplyPublicationError>),
    Commit(rusqlite::Error),
}

struct PrivateOutboxRoots {
    outbox: bool,
    transport: bool,
    private_message: bool,
    private_reservation: bool,
    reservation: bool,
}

impl PrivateOutboxRoots {
    fn absent(&self) -> bool {
        !self.outbox
            && !self.transport
            && !self.private_message
            && !self.private_reservation
            && !self.reservation
    }

    fn present(&self) -> bool {
        self.outbox
            && self.transport
            && self.private_message
            && self.private_reservation
            && !self.reservation
    }
}

impl Store {
    pub(crate) fn dispose_returned_settings_reply(
        &self,
        source: &ReturnedSettingsReplySource<'_>,
    ) -> Result<SettingsPrivateOutboxDisposition, Box<SettingsPrivateOutboxFailure>> {
        if !source.shares_store(self) {
            return Err(Box::new(SettingsPrivateOutboxFailure::BeforeTransaction {
                cause: SettingsReplyPublicationError::Conflict,
            }));
        }
        let mut main = self.connection().map_err(|cause| {
            Box::new(SettingsPrivateOutboxFailure::BeforeTransaction {
                cause: cause.into(),
            })
        })?;
        let transaction = main
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|cause| {
                Box::new(SettingsPrivateOutboxFailure::BeforeTransaction {
                    cause: cause.into(),
                })
            })?;
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            dispose_private_outbox_on(&transaction, source)
        }));
        match attempt {
            Ok(Ok(disposition)) => {
                drop(transaction);
                Ok(disposition)
            }
            Ok(Err(PrivateDispositionAttemptFailure::Checked(cause))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(match rollback {
                    Ok(()) => SettingsPrivateOutboxFailure::RolledBack { cause: *cause },
                    Err(rollback) => SettingsPrivateOutboxFailure::RollbackFailed {
                        cause: *cause,
                        rollback,
                    },
                }))
            }
            Ok(Err(PrivateDispositionAttemptFailure::Commit(commit))) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsPrivateOutboxFailure::CommitUncertain {
                    commit,
                    rollback,
                }))
            }
            Err(payload) => {
                let rollback = rollback_publication(self, transaction);
                Err(Box::new(SettingsPrivateOutboxFailure::Panicked {
                    payload,
                    rollback,
                }))
            }
        }
    }
}

fn dispose_private_outbox_on(
    transaction: &Transaction<'_>,
    source: &ReturnedSettingsReplySource<'_>,
) -> Result<SettingsPrivateOutboxDisposition, PrivateDispositionAttemptFailure> {
    let stamp = reply_request_stamp(source.request())?;
    let committed = source.committed();
    let header = committed.header();
    let candidate = source.candidate();
    let created_at =
        i64::try_from(header.created_at_ms).map_err(|_| SettingsReplyPublicationError::Conflict)?;
    if !matches!(candidate.routing, TransportOutboundRouting::PrivateSettings)
        || candidate.message_id != header.message_id
        || candidate.recipient != header.recipient
        || candidate.created_at != created_at
        || candidate.not_before < created_at
        || stamp.reservation_id.is_nil()
        || stamp.provenance.len() > 2048
        || stamp.local_device_id != header.sender
        || stamp.peer_device_id != header.recipient
        || stamp.created_at.get() > header.created_at_ms
        || stamp.expires_at.get() != header.expires_at_ms
        || header
            .expires_at_ms
            .checked_sub(header.created_at_ms)
            .is_none_or(|lifetime| lifetime == 0 || lifetime > 60_000)
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let roots = private_outbox_roots(transaction, header.message_id, stamp.reservation_id)?;
    let kind = if roots.absent() {
        PrivateOutboxDispositionKind::AlreadyAbsent
    } else if roots.present() {
        check_disposed_ciphertext_on(transaction, candidate, committed, &stamp)?;
        let removed = transaction.execute(
            "DELETE FROM outbox WHERE id=?1",
            [header.message_id.to_string()],
        )?;
        if removed != 1
            || !private_outbox_roots(transaction, header.message_id, stamp.reservation_id)?.absent()
        {
            return Err(SettingsReplyPublicationError::Conflict.into());
        }
        PrivateOutboxDispositionKind::Removed
    } else {
        return Err(SettingsReplyPublicationError::Conflict.into());
    };
    transaction
        .execute_batch("COMMIT")
        .map_err(PrivateDispositionAttemptFailure::Commit)?;
    Ok(SettingsPrivateOutboxDisposition {
        _kind: kind,
        slot: std::sync::Arc::clone(source.slot_identity()),
        message_id: header.message_id,
        reservation_id: stamp.reservation_id,
    })
}

fn private_outbox_roots(
    transaction: &Transaction<'_>,
    message_id: crate::protocol::MessageId,
    reservation_id: Uuid,
) -> Result<PrivateOutboxRoots, rusqlite::Error> {
    transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM outbox WHERE id=?1),EXISTS(SELECT 1 FROM transport_outbox WHERE message_id=?1),EXISTS(SELECT 1 FROM settings_transport_private_outbox WHERE message_id=?1),EXISTS(SELECT 1 FROM settings_transport_private_outbox WHERE reservation_id=?2),EXISTS(SELECT 1 FROM settings_transport_reply_reservations WHERE reservation_id=?2)",
        rusqlite::params![message_id.to_string(), reservation_id.to_string()],
        |row| Ok(PrivateOutboxRoots { outbox: row.get(0)?, transport: row.get(1)?, private_message: row.get(2)?, private_reservation: row.get(3)?, reservation: row.get(4)? }),
    )
}

fn check_disposed_ciphertext_on(
    transaction: &Transaction<'_>,
    candidate: &TransportOutboundCandidate,
    committed: &CommittedSettingsReply,
    stamp: &QueuedReplyStamp,
) -> Result<(), SettingsReplyPublicationError> {
    let mut statement = transaction.prepare(PRIVATE_CIPHERTEXT_QUERY)?;
    let mut rows = statement.query([committed.header.message_id.to_string()])?;
    let row = rows
        .next()?
        .ok_or(SettingsReplyPublicationError::Conflict)?;
    let bytes = checked_private_ciphertext_row(row, candidate, committed, stamp)?;
    drop(checked_private_envelope(bytes, committed)?);
    Ok(())
}

fn checked_private_ciphertext_row<'row>(
    row: &'row Row<'_>,
    candidate: &TransportOutboundCandidate,
    committed: &CommittedSettingsReply,
    stamp: &QueuedReplyStamp,
) -> Result<&'row [u8], SettingsReplyPublicationError> {
    let message = committed.header.message_id.to_string();
    let peer = stamp.peer_device_id.to_string();
    let expiry = i64::try_from(stamp.expires_at.get())
        .map_err(|_| SettingsReplyPublicationError::Conflict)?;
    let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(2)? else {
        return Err(SettingsReplyPublicationError::Conflict);
    };
    if bytes.is_empty()
        || bytes.len() != committed.envelope_bytes
        || bytes.len() > stamp.maximum_bytes
        || bytes.len() > 2 * super::MAX_JSON_BYTES
    {
        return Err(SettingsReplyPublicationError::TooLarge);
    }
    if ciphertext_text(row, 0, 36)? != message
        || ciphertext_text(row, 1, 36)? != peer
        || row.get::<_, i64>(3)? != candidate.created_at
        || row.get::<_, i64>(4)? != candidate.not_before
        || row.get::<_, i64>(5)? < 0
        || row.get::<_, i64>(5)? > i64::from(u32::MAX)
        || !matches!(row.get_ref(6)?, rusqlite::types::ValueRef::Null)
        || ciphertext_text(row, 7, 36)? != message
        || row.get::<_, i64>(8)? != expiry
        || ciphertext_text(row, 9, 36)? != message
        || ciphertext_text(row, 10, 36)? != stamp.reservation_id.to_string()
        || ciphertext_text(row, 11, 36)? != stamp.local_device_id.to_string()
        || ciphertext_text(row, 12, 36)? != peer
        || ciphertext_text(row, 13, 2048)? != stamp.provenance.as_str()
        || ciphertext_text(row, 14, 10)? != stamp.family
        || row.get::<_, i64>(15)?
            != i64::try_from(bytes.len()).map_err(|_| SettingsReplyPublicationError::Conflict)?
        || row.get::<_, i64>(16)? != candidate.created_at
        || row.get::<_, i64>(17)? != expiry
        || <[u8; 32]>::from(Sha256::digest(bytes)) != committed.encrypted_sha256
    {
        return Err(SettingsReplyPublicationError::Conflict);
    }
    Ok(bytes)
}

fn checked_private_envelope(
    bytes: &[u8],
    committed: &CommittedSettingsReply,
) -> Result<EncryptedEnvelope, SettingsReplyPublicationError> {
    let envelope: EncryptedEnvelope =
        serde_json::from_slice(bytes).map_err(SettingsReplyPublicationError::Encoding)?;
    if envelope.header != committed.header || envelope.signature != committed.signature {
        return Err(SettingsReplyPublicationError::Conflict);
    }
    Ok(envelope)
}

impl SettingsPrivateOutboxDisposition {
    pub(crate) fn matches_source(&self, source: &ReturnedSettingsReplySource<'_>) -> bool {
        std::sync::Arc::ptr_eq(&self.slot, source.slot_identity())
            && self.message_id == source.committed().header().message_id
            && reply_request_stamp(source.request())
                .is_ok_and(|stamp| stamp.reservation_id == self.reservation_id)
    }
}

impl From<SettingsReplyPublicationError> for PrivateDispositionAttemptFailure {
    fn from(cause: SettingsReplyPublicationError) -> Self {
        Self::Checked(Box::new(cause))
    }
}

impl From<rusqlite::Error> for PrivateDispositionAttemptFailure {
    fn from(cause: rusqlite::Error) -> Self {
        SettingsReplyPublicationError::from(cause).into()
    }
}

impl fmt::Debug for SettingsPrivateOutboxFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsPrivateOutboxFailure([retained cause])")
    }
}

impl fmt::Debug for SettingsPrivateOutboxDisposition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsPrivateOutboxDisposition([observed Main commit only])")
    }
}
