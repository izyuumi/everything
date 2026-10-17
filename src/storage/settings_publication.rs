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
    DeviceEndpoint, EndpointRole, OwnedQueuedSettingsReply, SealedSettingsReply,
    SettingsReplyBuildRequest, SettingsReplyPublicationError, SettingsReplySealContext,
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

struct QueuedReplyStamp<'reply> {
    reservation_id: Uuid,
    local_device_id: DeviceId,
    peer_device_id: DeviceId,
    provenance: &'reply str,
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
) -> Result<QueuedReplyStamp<'_>, SettingsQueuedReplyCheckError> {
    let (
        reservation_id,
        local_device_id,
        peer_device_id,
        provenance,
        family,
        maximum_bytes,
        created_at,
        expires_at,
    ) = match queued.request()? {
        SettingsReplyBuildRequest::Fresh(request) => {
            let reply = request.committed_source().reply_reservation();
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                reply.provenance.as_str(),
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
                reply.provenance.as_str(),
                "inspection",
                90_112,
                reply.created_at,
                reply.expires_at,
            )
        }
        SettingsReplyBuildRequest::Bind { request, .. } => {
            let reply = request.committed_source().reply_reservation();
            (
                reply.reservation_id,
                reply.local_device_id,
                reply.peer_device_id,
                reply.provenance.as_str(),
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
                reply.provenance.as_str(),
                family,
                maximum_bytes,
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

fn read_private_ciphertext_on(
    transaction: &Transaction<'_>,
    candidate: &TransportOutboundCandidate,
    committed: &CommittedSettingsReply,
    stamp: &QueuedReplyStamp<'_>,
    now: UnixMillis,
) -> Result<RelayClientMessage, CiphertextLoadAttemptFailure> {
    let reservation_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings_transport_reply_reservations WHERE reservation_id=?1)",
        [stamp.reservation_id.to_string()], |row| row.get(0),
    )?;
    if reservation_exists {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let mut statement = transaction.prepare("SELECT o.id,o.device_id,o.envelope,o.created_at,o.not_before,o.attempts,o.delivered_at,t.message_id,t.expires_at,p.message_id,p.reservation_id,p.local_device_id,p.peer_device_id,p.provenance,p.family,p.envelope_bytes,p.created_at,p.expires_at FROM outbox AS o JOIN transport_outbox AS t ON t.message_id=o.id JOIN settings_transport_private_outbox AS p ON p.message_id=o.id WHERE o.id=?1")?;
    let message_id = committed.header.message_id.to_string();
    let peer = stamp.peer_device_id.to_string();
    let created_at = candidate.created_at;
    let expires_at = i64::try_from(stamp.expires_at.get())
        .map_err(|_| SettingsReplyPublicationError::Conflict)?;
    let mut rows = statement.query([&message_id])?;
    let row = rows
        .next()?
        .ok_or(SettingsReplyPublicationError::Conflict)?;
    let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(2)? else {
        return Err(SettingsReplyPublicationError::Conflict.into());
    };
    if bytes.is_empty()
        || bytes.len() != committed.envelope_bytes
        || bytes.len() > stamp.maximum_bytes
        || bytes.len() > 2 * super::MAX_JSON_BYTES
    {
        return Err(SettingsReplyPublicationError::TooLarge.into());
    }
    if ciphertext_text(row, 0, 36)? != message_id
        || ciphertext_text(row, 1, 36)? != peer
        || row.get::<_, i64>(3)? != created_at
        || row.get::<_, i64>(4)? != candidate.not_before
        || row.get::<_, i64>(5)? < 0
        || row.get::<_, i64>(5)? > i64::from(u32::MAX)
        || !matches!(row.get_ref(6)?, rusqlite::types::ValueRef::Null)
        || ciphertext_text(row, 7, 36)? != message_id
        || row.get::<_, i64>(8)? != expires_at
        || ciphertext_text(row, 9, 36)? != message_id
        || ciphertext_text(row, 10, 36)? != stamp.reservation_id.to_string()
        || ciphertext_text(row, 11, 36)? != stamp.local_device_id.to_string()
        || ciphertext_text(row, 12, 36)? != peer
        || ciphertext_text(row, 13, 2048)? != stamp.provenance
        || ciphertext_text(row, 14, 10)? != stamp.family
        || row.get::<_, i64>(15)?
            != i64::try_from(bytes.len()).map_err(|_| SettingsReplyPublicationError::Conflict)?
        || row.get::<_, i64>(16)? != created_at
        || row.get::<_, i64>(17)? != expires_at
        || candidate.not_before
            > i64::try_from(now.get()).map_err(|_| SettingsReplyPublicationError::Conflict)?
        || now.get() < committed.header.created_at_ms
        || now.get() >= committed.header.expires_at_ms
        || <[u8; 32]>::from(Sha256::digest(bytes)) != committed.encrypted_sha256
    {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
    let envelope: EncryptedEnvelope =
        serde_json::from_slice(bytes).map_err(SettingsReplyPublicationError::Encoding)?;
    if envelope.header != committed.header || envelope.signature != committed.signature {
        return Err(SettingsReplyPublicationError::Conflict.into());
    }
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
