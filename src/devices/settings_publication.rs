use std::fmt;
use std::time::{Duration, Instant};

use rusqlite::Transaction;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

use crate::agent::Agent;
use crate::crypto::{CryptoError, EnvelopeHeader};
use crate::host::settings::service::{
    OriginalReplySource, QualifiedBindReply, QualifiedOriginalReply, SettingsReceiptAuthority,
    SettingsReplyProjectionError,
};
use crate::protocol::{DeviceId, ProtocolError, UnixMillis};
use crate::relay_protocol::{MAX_RELAY_FRAME_BYTES, RELAY_SCHEMA_VERSION, RelayClientMessage};
use crate::storage::{OutboxRow, StorageError, Store, TransportOutboxInsert};

use super::{DeviceEndpoint, EndpointRole, OwnedCommittedSettingsBind, ReplyBuildPermit};

pub(crate) struct SettingsReplySealContext<'source> {
    pub(crate) agent: &'source Agent,
    pub(crate) endpoint: &'source DeviceEndpoint,
    pub(crate) store: &'source Store,
    pub(crate) mutation: &'source OwnedMutexGuard<()>,
    pub(crate) authority: &'source SettingsReceiptAuthority,
    pub(crate) permit: &'source ReplyBuildPermit,
}

pub(crate) enum SettingsReplyQualification<'qualified, 'source> {
    Original(&'qualified QualifiedOriginalReply<'source>),
    Bind(&'qualified QualifiedBindReply<'source>),
}

pub(crate) enum SettingsReplyFamily {
    OriginalInspection,
    BindDefault,
}

pub(crate) struct SettingsReplyReservation {
    pub(crate) reservation_id: Uuid,
    pub(crate) local_device_id: DeviceId,
    pub(crate) peer_device_id: DeviceId,
    pub(crate) provenance: String,
    pub(crate) family: SettingsReplyFamily,
    pub(crate) created_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

enum PublicationSource<'source> {
    Original(OriginalReplySource<'source>),
    Bind(&'source OwnedCommittedSettingsBind),
}

struct SealPlan {
    reservation: SettingsReplyReservation,
    floor: (UnixMillis, Instant),
    deadline: Instant,
}

pub(crate) struct SealedSettingsReply<'transaction, 'connection, 'source> {
    item: TransportOutboxInsert,
    header: EnvelopeHeader,
    signature: [u8; 64],
    encrypted_sha256: [u8; 32],
    reservation: SettingsReplyReservation,
    seal_wall: UnixMillis,
    seal_monotonic: Instant,
    floor_wall: UnixMillis,
    floor_monotonic: Instant,
    deadline: Instant,
    qualification: SettingsReplyQualification<'transaction, 'source>,
    source: PublicationSource<'source>,
    transaction: &'transaction Transaction<'connection>,
    context: SettingsReplySealContext<'source>,
}

#[derive(Error)]
pub(crate) enum SettingsReplyPublicationError {
    #[error("Settings reply projection failed")]
    Projection(#[from] SettingsReplyProjectionError),
    #[error("Settings reply clock observation failed")]
    Clock(#[from] ProtocolError),
    #[error("Settings reply encryption failed")]
    Crypto(#[from] CryptoError),
    #[error("Settings reply encrypted encoding failed")]
    Encoding(#[source] serde_json::Error),
    #[error("Settings reply storage failed")]
    Storage(#[from] StorageError),
    #[error("Settings reply association conflicts with its original")]
    Conflict,
    #[error("Settings reply original deadline has expired")]
    Expired,
    #[error("Settings reply exceeds its original encoded limit")]
    TooLarge,
}

impl SettingsReplyFamily {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::OriginalInspection => "inspection",
            Self::BindDefault => "default",
        }
    }

    pub(crate) fn reserved_bytes(&self) -> usize {
        match self {
            Self::OriginalInspection => 90_112,
            Self::BindDefault => 49_152,
        }
    }

    fn plaintext_bytes(&self) -> usize {
        match self {
            Self::OriginalInspection => 65_536,
            Self::BindDefault => 32_768,
        }
    }
}

impl<'transaction, 'connection, 'source> SealedSettingsReply<'transaction, 'connection, 'source> {
    pub(crate) fn original(
        context: SettingsReplySealContext<'source>,
        transaction: &'transaction Transaction<'connection>,
        qualified: &'transaction QualifiedOriginalReply<'source>,
        source: OriginalReplySource<'source>,
    ) -> Result<Self, SettingsReplyPublicationError> {
        let (reply, floor, deadline, matches) = match &source {
            OriginalReplySource::Fresh(request) => {
                context
                    .permit
                    .check_fresh(request)
                    .map_err(SettingsReplyProjectionError::from)?;
                (
                    request.reply_reservation(),
                    request.receipt_floor(),
                    request.original_deadline(),
                    request.authority_source().matches_context(
                        context.agent,
                        context.endpoint,
                        OwnedMutexGuard::mutex(context.mutation),
                    ),
                )
            }
            OriginalReplySource::Historical(request) => {
                context
                    .permit
                    .check_historical(request)
                    .map_err(SettingsReplyProjectionError::from)?;
                (
                    request.reply_reservation(),
                    request.receipt_floor(),
                    request.original_deadline(),
                    request.authority_source().matches_context(
                        context.agent,
                        context.endpoint,
                        OwnedMutexGuard::mutex(context.mutation),
                    ),
                )
            }
        };
        context.check(matches)?;
        let reservation = SettingsReplyReservation {
            reservation_id: reply.reservation_id,
            local_device_id: reply.local_device_id,
            peer_device_id: reply.peer_device_id,
            provenance: reply.provenance.clone(),
            family: SettingsReplyFamily::OriginalInspection,
            created_at: reply.created_at,
            expires_at: reply.expires_at,
        };
        let plaintext = context.authority.project_original_reply_on(
            context.store,
            transaction,
            qualified,
            context.permit,
        )?;
        Self::seal(
            context,
            transaction,
            SettingsReplyQualification::Original(qualified),
            PublicationSource::Original(source),
            SealPlan {
                reservation,
                floor,
                deadline,
            },
            plaintext,
        )
    }

    pub(crate) fn bind(
        context: SettingsReplySealContext<'source>,
        transaction: &'transaction Transaction<'connection>,
        qualified: &'transaction QualifiedBindReply<'source>,
        request: &'source OwnedCommittedSettingsBind,
    ) -> Result<Self, SettingsReplyPublicationError> {
        let committed = request.committed_source();
        context
            .permit
            .check_bind(&committed)
            .map_err(SettingsReplyProjectionError::from)?;
        context.check(request.authority_source().matches_context(
            context.agent,
            context.endpoint,
            OwnedMutexGuard::mutex(context.mutation),
        ))?;
        let reply = committed.reply_reservation();
        let lifetime = committed.lifetime();
        let receipt = request
            .authority_source()
            .receipt_metadata()
            .map_err(SettingsReplyProjectionError::from)?;
        let floor_wall = UnixMillis::try_from(
            u64::try_from(receipt.received_at)
                .map_err(|_| SettingsReplyPublicationError::Conflict)?,
        )?;
        let floor_monotonic = lifetime
            .deadline
            .checked_sub(Duration::from_millis(
                lifetime
                    .expires_at
                    .get()
                    .checked_sub(floor_wall.get())
                    .ok_or(SettingsReplyPublicationError::Conflict)?,
            ))
            .ok_or(SettingsReplyPublicationError::Conflict)?;
        let deadline = lifetime
            .deadline
            .checked_sub(Duration::from_millis(
                lifetime
                    .expires_at
                    .get()
                    .checked_sub(reply.expires_at.get())
                    .ok_or(SettingsReplyPublicationError::Conflict)?,
            ))
            .ok_or(SettingsReplyPublicationError::Conflict)?;
        let reservation = SettingsReplyReservation {
            reservation_id: reply.reservation_id,
            local_device_id: reply.local_device_id,
            peer_device_id: reply.peer_device_id,
            provenance: reply.provenance.clone(),
            family: SettingsReplyFamily::BindDefault,
            created_at: reply.created_at,
            expires_at: reply.expires_at,
        };
        let plaintext = context.authority.project_bind_reply_on(
            context.store,
            transaction,
            qualified,
            context.permit,
        )?;
        Self::seal(
            context,
            transaction,
            SettingsReplyQualification::Bind(qualified),
            PublicationSource::Bind(request),
            SealPlan {
                reservation,
                floor: (floor_wall, floor_monotonic),
                deadline,
            },
            plaintext,
        )
    }

    fn seal(
        context: SettingsReplySealContext<'source>,
        transaction: &'transaction Transaction<'connection>,
        qualification: SettingsReplyQualification<'transaction, 'source>,
        source: PublicationSource<'source>,
        plan: SealPlan,
        plaintext: zeroize::Zeroizing<Vec<u8>>,
    ) -> Result<Self, SettingsReplyPublicationError> {
        let SealPlan {
            reservation,
            floor,
            deadline,
        } = plan;
        if plaintext.is_empty()
            || plaintext.len() > reservation.family.plaintext_bytes()
            || reservation.local_device_id != context.endpoint.public_keys().device_id
            || reservation.reservation_id.is_nil()
            || reservation.peer_device_id == reservation.local_device_id
            || reservation.created_at < floor.0
            || reservation.created_at >= reservation.expires_at
        {
            return Err(SettingsReplyPublicationError::Conflict);
        }
        let (seal_wall, seal_monotonic) = observe(floor, deadline, reservation.expires_at)?;
        if seal_wall < reservation.created_at {
            return Err(SettingsReplyPublicationError::Expired);
        }
        let lifetime = reservation
            .expires_at
            .get()
            .checked_sub(seal_wall.get())
            .filter(|value| *value > 0 && *value <= 60_000)
            .ok_or(SettingsReplyPublicationError::Expired)?;
        let peer = match &source {
            PublicationSource::Original(OriginalReplySource::Fresh(request)) => {
                request.authority_source().verified_peer_keys()
            }
            PublicationSource::Original(OriginalReplySource::Historical(request)) => {
                request.authority_source().verified_peer_keys()
            }
            PublicationSource::Bind(request) => request.authority_source().verified_peer_keys(),
        };
        if peer.device_id != reservation.peer_device_id {
            return Err(SettingsReplyPublicationError::Conflict);
        }
        let envelope =
            context
                .endpoint
                .inner
                .identity
                .seal(peer, &plaintext, seal_wall.get(), lifetime)?;
        drop(plaintext);
        envelope.verify_authenticated_sender(&context.endpoint.public_keys(), seal_wall.get())?;
        if envelope.header.sender != reservation.local_device_id
            || envelope.header.recipient != reservation.peer_device_id
            || envelope.header.created_at_ms != seal_wall.get()
            || envelope.header.expires_at_ms != reservation.expires_at.get()
        {
            return Err(SettingsReplyPublicationError::Conflict);
        }
        let header = envelope.header.clone();
        let signature = envelope.signature;
        let bytes =
            serde_json::to_vec(&envelope).map_err(SettingsReplyPublicationError::Encoding)?;
        if bytes.len() > reservation.family.reserved_bytes() {
            return Err(SettingsReplyPublicationError::TooLarge);
        }
        let encrypted_sha256: [u8; 32] = Sha256::digest(&bytes).into();
        let frame = RelayClientMessage::Send {
            schema_version: RELAY_SCHEMA_VERSION,
            envelope,
        };
        let mut encoded_frame = zeroize::Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *encoded_frame, &frame)
            .map_err(SettingsReplyPublicationError::Encoding)?;
        if encoded_frame.len() > MAX_RELAY_FRAME_BYTES {
            return Err(SettingsReplyPublicationError::TooLarge);
        }
        drop(encoded_frame);
        drop(frame);
        let created_at =
            i64::try_from(seal_wall.get()).map_err(|_| SettingsReplyPublicationError::Conflict)?;
        let expires_at = i64::try_from(reservation.expires_at.get())
            .map_err(|_| SettingsReplyPublicationError::Conflict)?;
        let sealed = Self {
            item: TransportOutboxInsert {
                message: OutboxRow {
                    id: header.message_id.to_string(),
                    device_id: reservation.peer_device_id.to_string(),
                    envelope: bytes,
                    created_at,
                    not_before: created_at,
                    attempts: 0,
                    delivered_at: None,
                },
                expires_at,
            },
            header,
            signature,
            encrypted_sha256,
            reservation,
            seal_wall,
            seal_monotonic,
            floor_wall: floor.0,
            floor_monotonic: floor.1,
            deadline,
            qualification,
            source,
            transaction,
            context,
        };
        sealed.check_before_transfer(transaction)?;
        Ok(sealed)
    }

    pub(crate) fn check_before_transfer(
        &self,
        transaction: &Transaction<'_>,
    ) -> Result<(), SettingsReplyPublicationError> {
        self.check_local_on(transaction)?;
        match &self.qualification {
            SettingsReplyQualification::Original(qualified) => self
                .context
                .authority
                .recheck_original_reply_on(self.context.store, transaction, qualified)
                .map_err(SettingsReplyProjectionError::from)?,
            SettingsReplyQualification::Bind(qualified) => self
                .context
                .authority
                .recheck_bind_reply_on(self.context.store, transaction, qualified)?,
        }
        Ok(())
    }

    pub(crate) fn check_local_on(
        &self,
        transaction: &Transaction<'_>,
    ) -> Result<(), SettingsReplyPublicationError> {
        if !std::ptr::eq(self.transaction, transaction) {
            return Err(SettingsReplyPublicationError::Conflict);
        }
        let matches = match &self.source {
            PublicationSource::Original(OriginalReplySource::Fresh(request)) => {
                self.context
                    .permit
                    .check_fresh(request)
                    .map_err(SettingsReplyProjectionError::from)?;
                request.authority_source().connection().is_live()
                    && request.authority_source().matches_context(
                        self.context.agent,
                        self.context.endpoint,
                        OwnedMutexGuard::mutex(self.context.mutation),
                    )
            }
            PublicationSource::Original(OriginalReplySource::Historical(request)) => {
                self.context
                    .permit
                    .check_historical(request)
                    .map_err(SettingsReplyProjectionError::from)?;
                request.authority_source().connection().is_live()
                    && request.authority_source().matches_context(
                        self.context.agent,
                        self.context.endpoint,
                        OwnedMutexGuard::mutex(self.context.mutation),
                    )
            }
            PublicationSource::Bind(request) => {
                self.context
                    .permit
                    .check_bind(&request.committed_source())
                    .map_err(SettingsReplyProjectionError::from)?;
                request.authority_source().connection().is_live()
                    && request.authority_source().matches_context(
                        self.context.agent,
                        self.context.endpoint,
                        OwnedMutexGuard::mutex(self.context.mutation),
                    )
            }
        };
        self.context.check(matches)?;
        let observed = observe(
            (self.floor_wall, self.floor_monotonic),
            self.deadline,
            self.reservation.expires_at,
        )?;
        if observed.0 < self.seal_wall || observed.1 < self.seal_monotonic {
            return Err(SettingsReplyPublicationError::Expired);
        }
        Ok(())
    }

    pub(crate) fn item(&self) -> &TransportOutboxInsert {
        &self.item
    }
    pub(crate) fn reservation(&self) -> &SettingsReplyReservation {
        &self.reservation
    }
    pub(crate) fn qualification(&self) -> &SettingsReplyQualification<'transaction, 'source> {
        &self.qualification
    }
    pub(crate) fn header(&self) -> &EnvelopeHeader {
        &self.header
    }
    pub(crate) fn signature(&self) -> &[u8; 64] {
        &self.signature
    }
    pub(crate) fn encrypted_sha256(&self) -> &[u8; 32] {
        &self.encrypted_sha256
    }
}

impl SettingsReplySealContext<'_> {
    fn check(&self, source_matches: bool) -> Result<(), SettingsReplyPublicationError> {
        if !source_matches
            || self.endpoint.role() != &EndpointRole::Host
            || !self.endpoint.shares_store(self.store)
            || !self.agent.store().shares_connection(self.store)
            || self.agent.state_lease().is_none()
        {
            return Err(SettingsReplyPublicationError::Conflict);
        }
        Ok(())
    }
}

fn observe(
    floor: (UnixMillis, Instant),
    deadline: Instant,
    expires_at: UnixMillis,
) -> Result<(UnixMillis, Instant), SettingsReplyPublicationError> {
    let observed = (UnixMillis::now()?, Instant::now());
    if observed.0 < floor.0
        || observed.1 < floor.1
        || observed.0 >= expires_at
        || observed.1 >= deadline
    {
        return Err(SettingsReplyPublicationError::Expired);
    }
    Ok(observed)
}

impl fmt::Debug for SealedSettingsReply<'_, '_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SealedSettingsReply([owned encrypted build output])")
    }
}

impl fmt::Debug for SettingsReplyPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReplyPublicationError([retained cause])")
    }
}

impl From<rusqlite::Error> for SettingsReplyPublicationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(StorageError::from(error))
    }
}
