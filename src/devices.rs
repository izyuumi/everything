mod routing;
mod session;
mod settings;
#[cfg(unix)]
mod settings_access_reply;
mod settings_connection;
#[cfg(unix)]
mod settings_ingress;
#[cfg(unix)]
mod settings_publication;

pub use session::RelaySessionStatus;
pub use settings::{
    DeviceSettingsPage, DeviceSettingsSummary, MAX_DEVICE_SETTINGS_PAGE,
    MAX_DEVICE_SETTINGS_PAGE_BYTES, PairingCancellation,
};
pub(crate) use settings_connection::{
    HostSettingsTransport, HostSettingsTransportOwner, SettingsConnectionIdentity,
    SettingsHostIdentity,
};

#[cfg(unix)]
pub(crate) use settings_access_reply::{
    SettingsAccessReplyContractError, SettingsAccessReplyPacket, SettingsRemoteDenial,
    SettingsRemoteOperation, SettingsRemoteReply,
};
#[cfg(unix)]
pub(crate) use settings_ingress::work::{
    OwnedQueuedSettingsReply, ReplyBuildPermit, SettingsOwnerError, SettingsReplyBuildRequest,
};
#[cfg(unix)]
pub(crate) use settings_ingress::{
    AccessStatusRejection, AuthenticatedSettingsAccessSource, BindingGeneration,
    OwnedCommittedSettingsAccess, OwnedVerifiedSettingsAccess, SETTINGS_CLOCK_SKEW_MS,
    SettingsAccessStatusIntent, SettingsHostAction, SettingsReceiveDrain, SettingsReceiveError,
    SettingsReceiveWitness, SettingsRemoteRequest,
};

#[cfg(unix)]
pub(crate) use settings_publication::{
    SealedSettingsReply, SettingsReplyPublicationError, SettingsReplyQualification,
    SettingsReplySealContext,
};

use crate::crypto::{
    CryptoError, DeviceIdentity, DeviceIdentitySecrets, DevicePublicKeys, EncryptedEnvelope,
    MAX_PAIRING_LIFETIME_MS, PairingAcceptance, PairingInvitation, PairingRequest, PendingPairing,
    PendingPairingCompletion, ReplayGuard, ReplayRecord,
};
use crate::protocol::{
    Command, CommandEnvelope, ConversationId, EventEnvelope, EventId, MAX_SYNC_BATCH_BYTES,
    MessageId, MessageRole, ProtocolError, SchemaVersion, SyncBatch, SyncCursor, SyncRequest,
    UnixMillis,
};
use crate::storage::{
    DeviceRow, OutboxRow, RemoteCommandCompletion, RemoteCommandCursor, RemoteCommandRow,
    RemoteCommandState, StorageError, Store, TransportInboxCursor, TransportInboxMetadata,
    TransportInboxRejection, TransportInboxRow, TransportOutboxInsert, TransportPruneReport,
};
use qrcode::{QrCode, render::svg};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::Notify;
use zeroize::Zeroizing;

const REGISTRATION_VERSION: u16 = 1;
const MAX_PAIRINGS: usize = 8;
const MAX_PACKET_BYTES: usize = crate::crypto::MAX_PLAINTEXT_BYTES;
const ENVELOPE_LIFETIME_MS: u64 = crate::crypto::MAX_ENVELOPE_LIFETIME_MS;
const IDENTITY_MAGIC: &[u8] = b"EVERYTHING-IDENTITY\0\x01";

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("device serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("identity storage failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid device or synchronization input: {0}")]
    Invalid(&'static str),
    #[error("paired device is outside its authorized scope")]
    AccessDenied,
    #[error("paired device identity metadata is malformed")]
    MalformedRegistration,
    #[error("pairing invitation or completion was not found")]
    PairingNotFound,
    #[error("pairing fingerprint was not explicitly confirmed")]
    FingerprintMismatch,
    #[error("pending pairing limit reached")]
    PairingCapacity,
    #[error("pairing state lock is poisoned")]
    Poisoned,
    #[error("local identity requires an owner-only Unix state directory")]
    PrivateStorageRequired,
    #[error("local endpoint identity has been revoked")]
    EndpointRevoked,
    #[error("sync event exceeds the bounded batch size")]
    SyncEventTooLarge,
    #[error("native event metadata does not match its stored envelope")]
    InvalidEvent,
    #[error("device worker failed")]
    Worker,
    #[error("a relay session is already running for this endpoint")]
    SessionAlreadyRunning,
    #[error("relay outbound ownership is quarantined; process restart required")]
    OutboundQuarantined,
    #[error(transparent)]
    Remote(#[from] crate::transport::remote::RemoteError),
    #[error("device operation was cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum EndpointRole {
    Host,
    Replica {
        authoritative_host: crate::protocol::DeviceId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "ids", rename_all = "snake_case")]
pub enum ConversationAccess {
    All,
    Selected(BTreeSet<ConversationId>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceAccess {
    pub conversations: ConversationAccess,
    pub submit_commands: bool,
    pub resolve_permissions: bool,
}

impl DeviceAccess {
    pub fn validate(&self) -> Result<(), DeviceError> {
        if matches!(&self.conversations,ConversationAccess::Selected(ids) if ids.len()>64)
            || (self.resolve_permissions && !self.submit_commands)
        {
            return Err(DeviceError::Invalid("invalid paired-device access"));
        }
        Ok(())
    }
    pub fn permits(&self, id: ConversationId) -> bool {
        match &self.conversations {
            ConversationAccess::All => true,
            ConversationAccess::Selected(ids) => ids.contains(&id),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerRegistration {
    version: u16,
    keys: DevicePublicKeys,
    access: DeviceAccess,
}

pub struct PairingApproval {
    label: String,
    expected_fingerprint: String,
    access: DeviceAccess,
}

impl PairingApproval {
    pub fn new(
        label: String,
        expected_fingerprint: String,
        access: DeviceAccess,
    ) -> Result<Self, DeviceError> {
        validate_label(&label)?;
        access.validate()?;
        if expected_fingerprint.len() != 71 {
            return Err(DeviceError::FingerprintMismatch);
        }
        Ok(Self {
            label,
            expected_fingerprint,
            access,
        })
    }
}

pub struct PairingDisplay {
    pub pairing_id: MessageId,
    pub expires_at_ms: u64,
    pub fingerprint: String,
    pub invitation_json: Zeroizing<String>,
    pub qr_svg: Zeroizing<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePacket {
    pub schema_version: SchemaVersion,
    pub payload: DevicePayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DevicePayload {
    Command {
        command: CommandEnvelope,
    },
    CommandResult {
        command_id: crate::protocol::CommandId,
        state: RemoteCommandState,
        result: Option<Value>,
    },
    SyncRequest {
        request: SyncRequest,
    },
    SyncBatch {
        batch: SyncBatch,
    },
    SyncAcknowledgement {
        batch_message_id: MessageId,
        cursor: SyncCursor,
        last_event_id: Option<EventId>,
    },
    Revoked {
        device_id: crate::protocol::DeviceId,
        revoked_at: UnixMillis,
    },
}

#[derive(Debug, Clone)]
pub struct ReceivedCommand {
    pub sender: crate::protocol::DeviceId,
    pub command: CommandEnvelope,
    pub original: Value,
    pub state: RemoteCommandState,
    pub newly_reserved: bool,
    pub previous_result: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum ReceivedOperation {
    Command(Box<ReceivedCommand>),
    CommandResult {
        message_id: MessageId,
        sender: crate::protocol::DeviceId,
        command_id: crate::protocol::CommandId,
        state: RemoteCommandState,
        result: Option<Value>,
    },
    Rejected {
        message_id: MessageId,
        sender: crate::protocol::DeviceId,
        reason: &'static str,
    },
}

pub struct DeviceInboxPage {
    pub scanned: usize,
    pub operations: Vec<ReceivedOperation>,
    pub next_cursor: Option<TransportInboxCursor>,
}

pub struct DeviceCommandsPage {
    pub scanned: usize,
    pub commands: Vec<ReceivedCommand>,
    pub next_cursor: Option<RemoteCommandCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeAcceptance {
    New,
    Duplicate,
}

#[derive(Clone)]
pub struct DeviceEndpoint {
    inner: Arc<EndpointInner>,
}

struct EndpointInner {
    store: Store,
    identity: Arc<DeviceIdentity>,
    role: EndpointRole,
    pairings: Mutex<BTreeMap<MessageId, PendingPairing>>,
    completions: Mutex<BTreeMap<MessageId, (u64, PendingPairingCompletion)>>,
    outbox_ready: Notify,
    inbox_ready: Notify,
    session: tokio::sync::Mutex<()>,
    outbound_failed: AtomicBool,
}

impl DeviceEndpoint {
    pub fn new(
        store: Store,
        identity: Arc<DeviceIdentity>,
        role: EndpointRole,
    ) -> Result<Self, DeviceError> {
        let keys = identity.public_keys();
        keys.validate()?;
        if matches!(role,EndpointRole::Replica{authoritative_host} if authoritative_host==keys.device_id)
        {
            return Err(DeviceError::Invalid(
                "replica host must be another identity",
            ));
        }
        let configuration = serde_json::json!({"version":1,"keys":keys,"role":role});
        store.configure_transport_endpoint(&keys.device_id.to_string(), &configuration)?;
        Ok(Self {
            inner: Arc::new(EndpointInner {
                store,
                identity,
                role,
                pairings: Mutex::new(BTreeMap::new()),
                completions: Mutex::new(BTreeMap::new()),
                outbox_ready: Notify::new(),
                inbox_ready: Notify::new(),
                session: tokio::sync::Mutex::new(()),
                outbound_failed: AtomicBool::new(false),
            }),
        })
    }

    pub fn public_keys(&self) -> DevicePublicKeys {
        self.inner.identity.public_keys()
    }
    pub fn role(&self) -> &EndpointRole {
        &self.inner.role
    }
    pub(crate) fn shares_store(&self, store: &Store) -> bool {
        self.inner.store.shares_connection(store)
    }
    pub(crate) fn outbound_is_quarantined(&self) -> bool {
        self.inner.outbound_failed.load(Ordering::Acquire)
    }
    fn check_outbound_health(&self) -> Result<(), DeviceError> {
        if self.outbound_is_quarantined() {
            return Err(DeviceError::OutboundQuarantined);
        }
        Ok(())
    }
    fn quarantine_outbound(&self) {
        self.inner.outbound_failed.store(true, Ordering::Release);
    }
    pub fn identity(&self) -> Arc<DeviceIdentity> {
        Arc::clone(&self.inner.identity)
    }
    pub async fn inbox_changed(&self) {
        self.inner.inbox_ready.notified().await;
    }
    pub async fn outbox_changed(&self) {
        self.inner.outbox_ready.notified().await;
    }

    pub fn begin_pairing(
        &self,
        now: UnixMillis,
        lifetime_ms: u64,
    ) -> Result<PairingDisplay, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        if lifetime_ms == 0 || lifetime_ms > MAX_PAIRING_LIFETIME_MS {
            return Err(DeviceError::Invalid("invalid pairing lifetime"));
        }
        let pending = self.inner.identity.begin_pairing(now.get(), lifetime_ms)?;
        let invitation = pending.invitation();
        let id = invitation.pairing_id;
        let expires = invitation.expires_at_ms;
        let fingerprint = invitation.inviter.fingerprint();
        let json = Zeroizing::new(serde_json::to_string(invitation)?);
        if json.len() > 4096 {
            return Err(DeviceError::Invalid("pairing invitation exceeds QR bound"));
        }
        let code = QrCode::new(json.as_bytes())
            .map_err(|_| DeviceError::Invalid("pairing QR encoding failed"))?;
        let qr = Zeroizing::new(code.render::<svg::Color>().min_dimensions(256, 256).build());
        let mut pairings = self
            .inner
            .pairings
            .lock()
            .map_err(|_| DeviceError::Poisoned)?;
        pairings.retain(|_, pending| pending.invitation().expires_at_ms > now.get());
        if pairings.len() >= MAX_PAIRINGS {
            return Err(DeviceError::PairingCapacity);
        }
        pairings.insert(id, pending);
        Ok(PairingDisplay {
            pairing_id: id,
            expires_at_ms: expires,
            fingerprint,
            invitation_json: json,
            qr_svg: qr,
        })
    }

    pub fn create_pairing_request(
        &self,
        invitation_json: &str,
        expected_fingerprint: &str,
        now: UnixMillis,
    ) -> Result<PairingRequest, DeviceError> {
        self.ensure_active()?;
        if !matches!(self.inner.role, EndpointRole::Replica { .. }) {
            return Err(DeviceError::AccessDenied);
        }
        if invitation_json.len() > 4096 {
            return Err(DeviceError::Invalid("pairing invitation exceeds bound"));
        }
        let invitation: PairingInvitation = serde_json::from_str(invitation_json)?;
        invitation.verify(now.get())?;
        if invitation.inviter.fingerprint() != expected_fingerprint {
            return Err(DeviceError::FingerprintMismatch);
        }
        if matches!(self.inner.role,EndpointRole::Replica{authoritative_host} if authoritative_host!=invitation.inviter.device_id)
        {
            return Err(DeviceError::AccessDenied);
        }
        let id = invitation.pairing_id;
        let expires = invitation.expires_at_ms;
        let (request, completion) = invitation.create_request(&self.inner.identity, now.get())?;
        let mut completions = self
            .inner
            .completions
            .lock()
            .map_err(|_| DeviceError::Poisoned)?;
        completions.retain(|_, (expiry, _)| *expiry > now.get());
        if completions.len() >= MAX_PAIRINGS || completions.contains_key(&id) {
            return Err(DeviceError::PairingCapacity);
        }
        completions.insert(id, (expires, completion));
        Ok(request)
    }

    pub fn approve_pairing(
        &self,
        request: &PairingRequest,
        approval: PairingApproval,
        now: UnixMillis,
    ) -> Result<PairingAcceptance, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        if request.responder.fingerprint() != approval.expected_fingerprint {
            return Err(DeviceError::FingerprintMismatch);
        }
        let pending = self
            .inner
            .pairings
            .lock()
            .map_err(|_| DeviceError::Poisoned)?
            .remove(&request.pairing_id)
            .ok_or(DeviceError::PairingNotFound)?;
        let (keys, acceptance) = pending.accept(&self.inner.identity, request, now.get())?;
        self.pin_peer(keys, request.pairing_id, approval, now)?;
        Ok(acceptance)
    }

    pub fn finish_pairing(
        &self,
        acceptance: &PairingAcceptance,
        approval: PairingApproval,
        now: UnixMillis,
    ) -> Result<DevicePublicKeys, DeviceError> {
        self.ensure_active()?;
        if !matches!(self.inner.role, EndpointRole::Replica { .. }) {
            return Err(DeviceError::AccessDenied);
        }
        if acceptance.inviter.fingerprint() != approval.expected_fingerprint {
            return Err(DeviceError::FingerprintMismatch);
        }
        let (_, completion) = self
            .inner
            .completions
            .lock()
            .map_err(|_| DeviceError::Poisoned)?
            .remove(&acceptance.pairing_id)
            .ok_or(DeviceError::PairingNotFound)?;
        let keys = completion.finish(acceptance, now.get())?;
        self.pin_peer(keys.clone(), acceptance.pairing_id, approval, now)?;
        Ok(keys)
    }

    pub fn revoke_device(
        &self,
        device_id: crate::protocol::DeviceId,
        now: UnixMillis,
    ) -> Result<(), DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        let existing = self
            .inner
            .store
            .device(&device_id.to_string())?
            .ok_or(DeviceError::AccessDenied)?;
        if existing.revoked_at.is_none() {
            self.inner
                .store
                .revoke_device(&device_id.to_string(), time(now)?)?;
        }
        self.inner.outbox_ready.notify_one();
        Ok(())
    }

    pub fn receive_envelope(
        &self,
        envelope: &EncryptedEnvelope,
        now: UnixMillis,
    ) -> Result<EnvelopeAcceptance, DeviceError> {
        let PreparedEnvelope {
            peer,
            pinned_identity,
            digest,
        } = self.prepare_envelope(envelope)?;
        let mut guard = DurableInbox {
            store: self.inner.store.clone(),
            pinned_identity,
            envelope_digest: digest,
            outcome: DurableInboxOutcome::Unobserved,
        };
        let opened = self
            .inner
            .identity
            .open(&peer.keys, envelope, now.get(), &mut guard);
        self.finish_envelope(opened, guard.outcome)
    }

    fn prepare_envelope(
        &self,
        envelope: &EncryptedEnvelope,
    ) -> Result<PreparedEnvelope, DeviceError> {
        self.ensure_active()?;
        let peer = self.peer(envelope.header.sender)?;
        if matches!(self.inner.role,EndpointRole::Replica{authoritative_host} if authoritative_host!=peer.keys.device_id)
        {
            return Err(DeviceError::AccessDenied);
        }
        let encoded = serde_json::to_vec(envelope)?;
        if encoded.len() > crate::relay_protocol::MAX_RELAY_FRAME_BYTES {
            return Err(DeviceError::Invalid(
                "encrypted envelope exceeds frame bound",
            ));
        }
        let digest = format!("{:x}", Sha256::digest(&encoded));
        Ok(PreparedEnvelope {
            pinned_identity: serde_json::to_value(&peer)?,
            peer,
            digest,
        })
    }

    fn finish_envelope(
        &self,
        opened: Result<Zeroizing<Vec<u8>>, CryptoError>,
        outcome: DurableInboxOutcome,
    ) -> Result<EnvelopeAcceptance, DeviceError> {
        match (opened, outcome) {
            (Ok(_plaintext), DurableInboxOutcome::OrdinaryNew) => {
                self.inner.inbox_ready.notify_one();
                Ok(EnvelopeAcceptance::New)
            }
            (Ok(plaintext), DurableInboxOutcome::RejectedNew) => {
                drop(plaintext);
                Ok(EnvelopeAcceptance::New)
            }
            (
                Err(CryptoError::Replay),
                DurableInboxOutcome::OrdinaryDuplicate | DurableInboxOutcome::RejectedDuplicate,
            ) => Ok(EnvelopeAcceptance::Duplicate),
            (
                Err(CryptoError::ReplayStorage(message)),
                DurableInboxOutcome::RejectedFailure(failure),
            ) if message == PRIVATE_INBOX_STORAGE_FAILURE => Err(failure.into_device_error()),
            (Err(error @ CryptoError::ReplayStorage(_)), DurableInboxOutcome::OrdinaryFailure) => {
                Err(error.into())
            }
            (Err(error), DurableInboxOutcome::Unobserved)
                if !matches!(&error, CryptoError::Replay | CryptoError::ReplayStorage(_)) =>
            {
                Err(error.into())
            }
            _ => Err(DeviceError::Invalid("durable inbox result is inconsistent")),
        }
    }

    pub fn submit_command(
        &self,
        host: crate::protocol::DeviceId,
        command: &CommandEnvelope,
        now: UnixMillis,
    ) -> Result<MessageId, DeviceError> {
        self.ensure_replica_host(host)?;
        if command.issuing_device_id != Some(self.public_keys().device_id) {
            return Err(DeviceError::AccessDenied);
        }
        let peer = self.peer(host)?;
        self.check_command(&peer.access, command)?;
        self.queue_payload(
            host,
            DevicePayload::Command {
                command: command.clone(),
            },
            now,
            None,
            None,
        )
    }

    pub fn request_sync(
        &self,
        host: crate::protocol::DeviceId,
        cursor: SyncCursor,
        max_events: u16,
        now: UnixMillis,
    ) -> Result<MessageId, DeviceError> {
        self.ensure_replica_host(host)?;
        let peer = self.peer(host)?;
        if !peer.access.permits(cursor.conversation_id) {
            return Err(DeviceError::AccessDenied);
        }
        if self
            .inner
            .store
            .replica_cursor(&host.to_string(), &cursor.conversation_id.to_string())?
            != cursor.after_sequence
        {
            return Err(DeviceError::Invalid(
                "sync request must use the durable replica cursor",
            ));
        }
        let request = SyncRequest::new(vec![cursor], max_events)?;
        self.queue_payload(
            host,
            DevicePayload::SyncRequest { request },
            now,
            None,
            None,
        )
    }

    pub fn complete_command(
        &self,
        command: &ReceivedCommand,
        result: Value,
        rejected: bool,
        now: UnixMillis,
    ) -> Result<(), DeviceError> {
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        self.ensure_active()?;
        let peer = self.peer(command.sender)?;
        let reply = self.seal_payload(
            &peer.keys,
            DevicePayload::CommandResult {
                command_id: command.command.command_id,
                state: if rejected {
                    RemoteCommandState::Rejected
                } else {
                    RemoteCommandState::Completed
                },
                result: Some(result.clone()),
            },
            now,
        )?;
        self.inner.store.complete_remote_command(
            &RemoteCommandCompletion {
                command_id: &command.command.command_id.to_string(),
                original: &command.original,
                result: &result,
                rejected,
                now: time(now)?,
            },
            &reply,
            &serde_json::to_value(peer)?,
        )?;
        self.inner.outbox_ready.notify_one();
        Ok(())
    }

    pub fn pending_commands(&self, limit: usize) -> Result<Vec<ReceivedCommand>, DeviceError> {
        Ok(self.pending_commands_after(None, limit)?.commands)
    }

    pub fn pending_commands_after(
        &self,
        cursor: Option<&RemoteCommandCursor>,
        limit: usize,
    ) -> Result<DeviceCommandsPage, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        let mut page_size = limit;
        let rows = loop {
            match self
                .inner
                .store
                .pending_remote_commands_after(cursor, page_size)
            {
                Err(StorageError::ReadBudgetExceeded) if page_size > 1 => page_size /= 2,
                result => break result?,
            }
        };
        let scanned = rows.len();
        let next_cursor = rows.last().map(|row| RemoteCommandCursor {
            received_at: row.received_at,
            command_id: row.command_id.clone(),
        });
        let commands = rows
            .into_iter()
            .map(|row| {
                let original = row.request.ok_or(DeviceError::Invalid(
                    "pending remote command lost its original request",
                ))?;
                let command: CommandEnvelope = serde_json::from_value(original.clone())?;
                let sender = row.sender.parse()?;
                let peer = self.peer(sender)?;
                self.check_command(&peer.access, &command)?;
                Ok(ReceivedCommand {
                    sender,
                    command,
                    original,
                    state: row.state,
                    newly_reserved: false,
                    previous_result: row.result,
                })
            })
            .collect::<Result<Vec<_>, DeviceError>>()?;
        Ok(DeviceCommandsPage {
            scanned,
            commands,
            next_cursor,
        })
    }

    pub fn process_inbox(
        &self,
        now: UnixMillis,
        limit: usize,
    ) -> Result<Vec<ReceivedOperation>, DeviceError> {
        Ok(self.process_inbox_after(now, None, limit)?.operations)
    }

    pub fn process_inbox_after(
        &self,
        now: UnixMillis,
        cursor: Option<&TransportInboxCursor>,
        limit: usize,
    ) -> Result<DeviceInboxPage, DeviceError> {
        self.ensure_active()?;
        let mut page_size = limit;
        let rows = loop {
            match self
                .inner
                .store
                .pending_transport_inbox_after(cursor, page_size)
            {
                Err(StorageError::ReadBudgetExceeded) if page_size > 1 => page_size /= 2,
                result => break result?,
            }
        };
        let scanned = rows.len();
        let next_cursor = rows.last().map(|row| TransportInboxCursor {
            received_at: row.received_at,
            message_id: row.message_id.clone(),
        });
        let mut operations = Vec::new();
        for row in rows {
            let sender = row.sender.parse()?;
            let id = row.message_id.parse()?;
            let peer = self.peer(sender)?;
            let raw: Value = match serde_json::from_slice(&row.payload) {
                Ok(value) => value,
                Err(_) => {
                    self.reject(&row, "invalid_json")?;
                    operations.push(ReceivedOperation::Rejected {
                        message_id: id,
                        sender,
                        reason: "invalid_json",
                    });
                    continue;
                }
            };
            let packet: DevicePacket = match serde_json::from_value(raw.clone()) {
                Ok(value) => value,
                Err(_) => {
                    self.reject(&row, "invalid_packet")?;
                    operations.push(ReceivedOperation::Rejected {
                        message_id: id,
                        sender,
                        reason: "invalid_packet",
                    });
                    continue;
                }
            };
            let result = self.process_packet(&row, &peer, packet, raw, now);
            match result {
                Ok(Some(operation)) => operations.push(operation),
                Ok(None) => {}
                Err(DeviceError::AccessDenied) => {
                    self.reject(&row, "access_denied")?;
                    operations.push(ReceivedOperation::Rejected {
                        message_id: id,
                        sender,
                        reason: "access_denied",
                    });
                }
                Err(DeviceError::Storage(StorageError::SyncGap { .. })) => {
                    if let EndpointRole::Replica { authoritative_host } = self.inner.role
                        && let Ok(packet) = serde_json::from_slice::<DevicePacket>(&row.payload)
                        && let DevicePayload::SyncBatch { batch } = packet.payload
                    {
                        let cursor = SyncCursor {
                            conversation_id: batch.cursor.conversation_id,
                            after_sequence: self.inner.store.replica_cursor(
                                &authoritative_host.to_string(),
                                &batch.cursor.conversation_id.to_string(),
                            )?,
                        };
                        self.request_sync(authoritative_host, cursor, 256, now)?;
                    }
                    self.reject(&row, "sync_gap_requested")?;
                    operations.push(ReceivedOperation::Rejected {
                        message_id: id,
                        sender,
                        reason: "sync_gap_requested",
                    });
                }
                Err(
                    DeviceError::Invalid(_)
                    | DeviceError::Protocol(_)
                    | DeviceError::Json(_)
                    | DeviceError::Storage(StorageError::TransportConflict(_))
                    | DeviceError::Storage(StorageError::CommandConflict(_))
                    | DeviceError::Storage(StorageError::InvalidInput(_))
                    | DeviceError::Storage(StorageError::NotFound { .. }),
                ) => {
                    self.reject(&row, "invalid_operation")?;
                    operations.push(ReceivedOperation::Rejected {
                        message_id: id,
                        sender,
                        reason: "invalid_operation",
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(DeviceInboxPage {
            scanned,
            operations,
            next_cursor,
        })
    }

    pub fn acknowledge_command_result(&self, message_id: MessageId) -> Result<(), DeviceError> {
        self.ensure_active()?;
        if !matches!(self.inner.role, EndpointRole::Replica { .. }) {
            return Err(DeviceError::AccessDenied);
        }
        self.inner
            .store
            .acknowledge_command_result(&message_id.to_string())?;
        Ok(())
    }

    pub fn maintain(&self, now: UnixMillis) -> Result<TransportPruneReport, DeviceError> {
        Ok(self.inner.store.maintain_transport(time(now)?)?)
    }

    fn process_packet(
        &self,
        row: &TransportInboxRow,
        peer: &PeerRegistration,
        packet: DevicePacket,
        raw: Value,
        now: UnixMillis,
    ) -> Result<Option<ReceivedOperation>, DeviceError> {
        let pinned = serde_json::to_value(peer)?;
        match packet.payload {
            DevicePayload::Command { command } => {
                if self.inner.role != EndpointRole::Host
                    || command.issuing_device_id != Some(peer.keys.device_id)
                {
                    return Err(DeviceError::AccessDenied);
                }
                self.check_command(&peer.access, &command)?;
                let original = raw
                    .pointer("/payload/command")
                    .cloned()
                    .ok_or(DeviceError::Invalid("original command missing"))?;
                let reservation = self.inner.store.reserve_remote_command(
                    &row.message_id,
                    &command,
                    &original,
                    &pinned,
                    time(now)?,
                )?;
                if reservation.command.state != RemoteCommandState::Pending {
                    self.queue_command_result(
                        peer.keys.device_id,
                        &reservation.command,
                        now,
                        Some(&row.message_id),
                    )?;
                }
                Ok(Some(ReceivedOperation::Command(Box::new(
                    ReceivedCommand {
                        sender: peer.keys.device_id,
                        command,
                        original,
                        state: reservation.command.state,
                        newly_reserved: reservation.created,
                        previous_result: reservation.command.result,
                    },
                ))))
            }
            DevicePayload::CommandResult {
                command_id,
                state,
                result,
            } => {
                self.ensure_replica_host(peer.keys.device_id)?;
                if state == RemoteCommandState::Pending {
                    return Err(DeviceError::Invalid("command result cannot remain pending"));
                }
                Ok(Some(ReceivedOperation::CommandResult {
                    message_id: row.message_id.parse()?,
                    sender: peer.keys.device_id,
                    command_id,
                    state,
                    result,
                }))
            }
            DevicePayload::SyncRequest { request } => {
                if self.inner.role != EndpointRole::Host || request.cursors.len() != 1 {
                    return Err(DeviceError::AccessDenied);
                }
                request.validate()?;
                let cursor = request.cursors[0];
                if !peer.access.permits(cursor.conversation_id) {
                    return Err(DeviceError::AccessDenied);
                }
                let batch = self.build_sync_batch(cursor, request.max_events)?;
                self.queue_payload(
                    peer.keys.device_id,
                    DevicePayload::SyncBatch {
                        batch: batch.clone(),
                    },
                    now,
                    Some(&batch),
                    Some(&row.message_id),
                )?;
                Ok(None)
            }
            DevicePayload::SyncBatch { batch } => {
                self.ensure_replica_host(peer.keys.device_id)?;
                if !peer.access.permits(batch.cursor.conversation_id) {
                    return Err(DeviceError::AccessDenied);
                }
                batch.validate()?;
                let acknowledgement = DevicePayload::SyncAcknowledgement {
                    batch_message_id: row.message_id.parse()?,
                    cursor: batch.next_cursor(),
                    last_event_id: batch.events.last().map(|event| event.event_id),
                };
                let mut replies = vec![self.seal_payload(&peer.keys, acknowledgement, now)?];
                if batch.has_more {
                    let durable = self.inner.store.replica_cursor(
                        &peer.keys.device_id.to_string(),
                        &batch.cursor.conversation_id.to_string(),
                    )?;
                    let cursor = SyncCursor {
                        conversation_id: batch.cursor.conversation_id,
                        after_sequence: durable.max(batch.next_cursor().after_sequence),
                    };
                    let request = SyncRequest::new(vec![cursor], 256)?;
                    replies.push(self.seal_payload(
                        &peer.keys,
                        DevicePayload::SyncRequest { request },
                        now,
                    )?);
                }
                self.inner.store.apply_replica_sync(
                    &row.message_id,
                    &peer.keys.device_id.to_string(),
                    &pinned,
                    &batch,
                    &replies,
                )?;
                self.inner.outbox_ready.notify_one();
                Ok(None)
            }
            DevicePayload::SyncAcknowledgement {
                batch_message_id,
                cursor,
                last_event_id,
            } => {
                if self.inner.role != EndpointRole::Host
                    || !peer.access.permits(cursor.conversation_id)
                {
                    return Err(DeviceError::AccessDenied);
                }
                self.inner.store.acknowledge_sync_offer(
                    &row.message_id,
                    &peer.keys.device_id.to_string(),
                    &pinned,
                    &batch_message_id.to_string(),
                    cursor,
                    last_event_id.as_ref().map(|id| id.to_string()).as_deref(),
                )?;
                Ok(None)
            }
            DevicePayload::Revoked {
                device_id,
                revoked_at,
            } => {
                self.ensure_replica_host(peer.keys.device_id)?;
                if device_id != self.public_keys().device_id {
                    return Err(DeviceError::AccessDenied);
                }
                self.inner
                    .store
                    .revoke_transport_endpoint(time(revoked_at)?)?;
                Ok(None)
            }
        }
    }

    pub fn build_sync_batch(
        &self,
        cursor: SyncCursor,
        max_events: u16,
    ) -> Result<SyncBatch, DeviceError> {
        self.ensure_active()?;
        if self.inner.role != EndpointRole::Host {
            return Err(DeviceError::AccessDenied);
        }
        SyncRequest::new(vec![cursor], max_events)?;
        let conversation = self
            .inner
            .store
            .conversation(&cursor.conversation_id.to_string())?
            .ok_or(DeviceError::Invalid("unknown sync conversation"))?;
        if cursor.after_sequence > conversation.last_sequence {
            return Err(DeviceError::Invalid("sync cursor exceeds the host head"));
        }
        let mut limit = usize::from(max_events);
        let rows = loop {
            match self.inner.store.events(
                &cursor.conversation_id.to_string(),
                cursor.after_sequence,
                limit,
            ) {
                Err(StorageError::ReadBudgetExceeded) if limit > 1 => limit /= 2,
                result => break result?,
            }
        };
        let mut events = Vec::new();
        let mut encoded_bytes =
            serde_json::to_vec(&SyncBatch::new(cursor, Vec::new(), false)?)?.len();
        for row in rows
            .into_iter()
            .take_while(|row| row.sequence <= conversation.last_sequence)
        {
            let event: EventEnvelope = serde_json::from_value(row.payload)?;
            let kind = serde_json::to_value(&event.payload)?;
            if event.event_id.to_string() != row.id
                || event.conversation_id != cursor.conversation_id
                || event.sequence.get() != row.sequence
                || event.occurred_at.get() != row.occurred_at as u64
                || kind.get("kind").and_then(Value::as_str) != Some(row.kind.as_str())
            {
                return Err(DeviceError::InvalidEvent);
            }
            let candidate =
                encoded_bytes + serde_json::to_vec(&event)?.len() + usize::from(!events.is_empty());
            if candidate > MAX_SYNC_BATCH_BYTES {
                if events.is_empty() {
                    return Err(DeviceError::SyncEventTooLarge);
                }
                break;
            }
            encoded_bytes = candidate;
            events.push(event);
        }
        let through = events
            .last()
            .map_or(cursor.after_sequence, |event| event.sequence.get());
        if through < conversation.last_sequence && events.is_empty() {
            return Err(DeviceError::Invalid(
                "host journal has a synchronization gap",
            ));
        }
        Ok(SyncBatch::new(
            cursor,
            events,
            through < conversation.last_sequence,
        )?)
    }

    fn queue_command_result(
        &self,
        recipient: crate::protocol::DeviceId,
        row: &RemoteCommandRow,
        now: UnixMillis,
        processed_inbox: Option<&str>,
    ) -> Result<MessageId, DeviceError> {
        self.queue_payload(
            recipient,
            DevicePayload::CommandResult {
                command_id: row.command_id.parse()?,
                state: row.state,
                result: row.result.clone(),
            },
            now,
            None,
            processed_inbox,
        )
    }

    fn queue_payload(
        &self,
        recipient: crate::protocol::DeviceId,
        payload: DevicePayload,
        now: UnixMillis,
        offer: Option<&SyncBatch>,
        processed_inbox: Option<&str>,
    ) -> Result<MessageId, DeviceError> {
        self.ensure_active()?;
        let peer = self.peer(recipient)?;
        let item = self.seal_payload(&peer.keys, payload, now)?;
        let id = item.message.id.parse()?;
        self.inner.store.queue_transport_message(
            &item,
            &serde_json::to_value(&peer)?,
            offer,
            processed_inbox,
        )?;
        self.inner.outbox_ready.notify_one();
        Ok(id)
    }

    fn seal_payload(
        &self,
        recipient: &DevicePublicKeys,
        payload: DevicePayload,
        now: UnixMillis,
    ) -> Result<TransportOutboxInsert, DeviceError> {
        let packet = DevicePacket {
            schema_version: SchemaVersion::CURRENT,
            payload,
        };
        let plaintext = Zeroizing::new(serde_json::to_vec(&packet)?);
        if plaintext.len() > MAX_PACKET_BYTES {
            return Err(DeviceError::Invalid(
                "application packet exceeds plaintext bound",
            ));
        }
        let envelope =
            self.inner
                .identity
                .seal(recipient, &plaintext, now.get(), ENVELOPE_LIFETIME_MS)?;
        Ok(TransportOutboxInsert {
            message: OutboxRow {
                id: envelope.header.message_id.to_string(),
                device_id: recipient.device_id.to_string(),
                envelope: serde_json::to_vec(&envelope)?,
                created_at: time(now)?,
                not_before: time(now)?,
                attempts: 0,
                delivered_at: None,
            },
            expires_at: i64::try_from(envelope.header.expires_at_ms)
                .map_err(|_| DeviceError::Invalid("envelope expiry exceeds storage range"))?,
        })
    }

    fn pin_peer(
        &self,
        keys: DevicePublicKeys,
        pairing_id: MessageId,
        approval: PairingApproval,
        now: UnixMillis,
    ) -> Result<(), DeviceError> {
        keys.validate()?;
        if matches!(self.inner.role, EndpointRole::Replica { authoritative_host } if authoritative_host != keys.device_id)
        {
            return Err(DeviceError::AccessDenied);
        }
        approval.access.validate()?;
        if keys.fingerprint() != approval.expected_fingerprint {
            return Err(DeviceError::FingerprintMismatch);
        }
        if keys.device_id == self.public_keys().device_id {
            return Err(DeviceError::AccessDenied);
        }
        let registration = PeerRegistration {
            version: REGISTRATION_VERSION,
            keys: keys.clone(),
            access: approval.access,
        };
        self.inner.store.pin_transport_device(
            &DeviceRow {
                id: keys.device_id.to_string(),
                label: approval.label,
                public_identity: serde_json::to_value(registration)?,
                created_at: time(now)?,
                revoked_at: None,
            },
            &pairing_id.to_string(),
        )?;
        Ok(())
    }

    fn peer(&self, id: crate::protocol::DeviceId) -> Result<PeerRegistration, DeviceError> {
        let row = self
            .inner
            .store
            .device(&id.to_string())?
            .ok_or(DeviceError::AccessDenied)?;
        if row.revoked_at.is_some() {
            return Err(StorageError::RevokedDevice(id.to_string()).into());
        }
        let registration: PeerRegistration = serde_json::from_value(row.public_identity)
            .map_err(|_| DeviceError::MalformedRegistration)?;
        if registration.version != REGISTRATION_VERSION || registration.keys.device_id != id {
            return Err(DeviceError::MalformedRegistration);
        }
        registration.keys.validate()?;
        registration.access.validate()?;
        Ok(registration)
    }

    fn check_command(
        &self,
        access: &DeviceAccess,
        command: &CommandEnvelope,
    ) -> Result<(), DeviceError> {
        if !access.submit_commands || !access.permits(command.conversation_id) {
            return Err(DeviceError::AccessDenied);
        }
        match &command.payload {
            Command::ResolvePermission { .. } if !access.resolve_permissions => {
                return Err(DeviceError::AccessDenied);
            }
            Command::SendMessage { message } if message.role != MessageRole::User => {
                return Err(DeviceError::AccessDenied);
            }
            _ => {}
        }
        Ok(())
    }
    fn ensure_active(&self) -> Result<(), DeviceError> {
        if self.inner.store.transport_endpoint_revoked()? {
            return Err(DeviceError::EndpointRevoked);
        }
        Ok(())
    }
    fn ensure_replica_host(&self, host: crate::protocol::DeviceId) -> Result<(), DeviceError> {
        self.ensure_active()?;
        if !matches!(self.inner.role,EndpointRole::Replica{authoritative_host}if authoritative_host==host)
        {
            return Err(DeviceError::AccessDenied);
        }
        Ok(())
    }
    fn reject(&self, row: &TransportInboxRow, reason: &str) -> Result<(), DeviceError> {
        Ok(self
            .inner
            .store
            .finish_transport_inbox(&row.message_id, Some(reason))?)
    }
}

const PRIVATE_INBOX_STORAGE_FAILURE: &str = "private_ingress_storage_failure";

struct PreparedEnvelope {
    peer: PeerRegistration,
    pinned_identity: Value,
    digest: String,
}

struct DurableInbox {
    store: Store,
    pinned_identity: Value,
    envelope_digest: String,
    outcome: DurableInboxOutcome,
}

enum DurableInboxOutcome {
    Unobserved,
    OrdinaryNew,
    OrdinaryDuplicate,
    OrdinaryFailure,
    RejectedNew,
    RejectedDuplicate,
    RejectedFailure(PrivateInboxFailure),
}

enum PrivateInboxFailure {
    Storage(StorageError),
    ReceiptTimeOutOfRange,
    ExpiryOutOfRange,
}

impl PrivateInboxFailure {
    fn into_device_error(self) -> DeviceError {
        match self {
            Self::Storage(error) => DeviceError::Storage(error),
            Self::ReceiptTimeOutOfRange => {
                DeviceError::Invalid("private receipt time exceeds storage range")
            }
            Self::ExpiryOutOfRange => DeviceError::Invalid("private expiry exceeds storage range"),
        }
    }
}

impl ReplayGuard for DurableInbox {
    fn record_if_new(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError> {
        if !matches!(&self.outcome, DurableInboxOutcome::Unobserved) {
            return Err(CryptoError::ReplayStorage(
                PRIVATE_INBOX_STORAGE_FAILURE.into(),
            ));
        }
        match routing::classify(plaintext) {
            routing::RoutingDecision::Ordinary => self.record_ordinary(record, plaintext, now_ms),
            routing::RoutingDecision::Rejected(reason) => {
                self.record_rejected(record, now_ms, reason)
            }
        }
    }
}

impl DurableInbox {
    fn record_ordinary(
        &mut self,
        record: ReplayRecord,
        plaintext: &[u8],
        now_ms: u64,
    ) -> Result<bool, CryptoError> {
        self.outcome = DurableInboxOutcome::OrdinaryFailure;
        let receipt = TransportInboxRow {
            message_id: record.message_id.to_string(),
            sender: record.sender.to_string(),
            recipient: record.recipient.to_string(),
            envelope_digest: self.envelope_digest.clone(),
            payload: plaintext.to_vec(),
            received_at: i64::try_from(now_ms).map_err(|_| {
                CryptoError::ReplayStorage("receipt time exceeds storage range".into())
            })?,
            expires_at: i64::try_from(record.expires_at_ms)
                .map_err(|_| CryptoError::ReplayStorage("expiry exceeds storage range".into()))?,
        };
        match self
            .store
            .accept_transport_inbox(&receipt, &self.pinned_identity)
        {
            Ok(fresh) => {
                self.outcome = if fresh {
                    DurableInboxOutcome::OrdinaryNew
                } else {
                    DurableInboxOutcome::OrdinaryDuplicate
                };
                Ok(fresh)
            }
            Err(error) => Err(CryptoError::ReplayStorage(error.to_string())),
        }
    }

    fn record_rejected(
        &mut self,
        record: ReplayRecord,
        now_ms: u64,
        reason: TransportInboxRejection,
    ) -> Result<bool, CryptoError> {
        let received_at = i64::try_from(now_ms)
            .map_err(|_| self.record_private_failure(PrivateInboxFailure::ReceiptTimeOutOfRange))?;
        let expires_at = i64::try_from(record.expires_at_ms)
            .map_err(|_| self.record_private_failure(PrivateInboxFailure::ExpiryOutOfRange))?;
        let message_id = record.message_id.to_string();
        let sender = record.sender.to_string();
        let recipient = record.recipient.to_string();
        let receipt = TransportInboxMetadata {
            message_id: &message_id,
            sender: &sender,
            recipient: &recipient,
            envelope_digest: &self.envelope_digest,
            received_at,
            expires_at,
        };
        match self
            .store
            .reject_transport_inbox(&receipt, &self.pinned_identity, reason)
        {
            Ok(fresh) => {
                self.outcome = if fresh {
                    DurableInboxOutcome::RejectedNew
                } else {
                    DurableInboxOutcome::RejectedDuplicate
                };
                Ok(fresh)
            }
            Err(error) => Err(self.record_private_failure(PrivateInboxFailure::Storage(error))),
        }
    }

    fn record_private_failure(&mut self, failure: PrivateInboxFailure) -> CryptoError {
        self.outcome = DurableInboxOutcome::RejectedFailure(failure);
        CryptoError::ReplayStorage(PRIVATE_INBOX_STORAGE_FAILURE.into())
    }
}

pub fn load_or_create_identity(state_directory: &Path) -> Result<Arc<DeviceIdentity>, DeviceError> {
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    {
        use cap_std::fs::Dir;
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(state_directory)?;
        let metadata = std::fs::symlink_metadata(state_directory)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(DeviceError::PrivateStorageRequired);
        }
        let directory = Dir::open_ambient_dir(state_directory, cap_std::ambient_authority())?;
        load_or_create_identity_in(&directory)
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    {
        let _ = state_directory;
        Err(DeviceError::PrivateStorageRequired)
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub fn load_or_create_identity_in(
    directory: &cap_std::fs::Dir,
) -> Result<Arc<DeviceIdentity>, DeviceError> {
    use cap_std::fs::{MetadataExt, OpenOptions, OpenOptionsExt};
    let metadata = directory.dir_metadata()?;
    let uid = rustix::process::geteuid().as_raw();
    if uid != rustix::process::getuid().as_raw() {
        return Err(DeviceError::PrivateStorageRequired);
    }
    if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o700 {
        return Err(DeviceError::PrivateStorageRequired);
    }
    let name = "device-identity-v1.bin";
    let mut read_options = OpenOptions::new();
    read_options
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    let existing = match directory.open_with(name, &read_options) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(file) = existing {
        return read_identity(file, uid);
    }
    let identity = Arc::new(DeviceIdentity::generate()?);
    let secrets = identity.export_secrets();
    let mut encoded = Zeroizing::new(Vec::with_capacity(IDENTITY_MAGIC.len() + 80));
    encoded.extend_from_slice(IDENTITY_MAGIC);
    encoded.extend_from_slice(secrets.device_id.as_uuid().as_bytes());
    encoded.extend_from_slice(secrets.signing_seed.as_ref());
    encoded.extend_from_slice(secrets.encryption_secret.as_ref());
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let temporary = format!(".device-identity-{}.tmp", uuid::Uuid::new_v4());
    let mut file = directory.open_with(&temporary, &options)?;
    let metadata = file.metadata()?;
    if metadata.uid() != uid || metadata.mode() & 0o7777 != 0o600 || metadata.nlink() != 1 {
        directory.remove_file(&temporary)?;
        return Err(DeviceError::PrivateStorageRequired);
    }
    let written = file.write_all(&encoded).and_then(|()| file.sync_all());
    if let Err(error) = written {
        directory.remove_file(&temporary)?;
        return Err(error.into());
    }
    match rustix::fs::renameat_with(
        directory,
        &temporary,
        directory,
        name,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {
            directory
                .open_with(".", OpenOptions::new().read(true))?
                .sync_all()?;
            Ok(identity)
        }
        Err(rustix::io::Errno::EXIST) => {
            directory.remove_file(&temporary)?;
            directory
                .open_with(".", OpenOptions::new().read(true))?
                .sync_all()?;
            read_identity(directory.open_with(name, &read_options)?, uid)
        }
        Err(error) => {
            directory.remove_file(&temporary)?;
            Err(std::io::Error::from(error).into())
        }
    }
}

#[cfg(unix)]
fn read_identity(file: cap_std::fs::File, uid: u32) -> Result<Arc<DeviceIdentity>, DeviceError> {
    use cap_std::fs::MetadataExt;
    let metadata = file.metadata()?;
    let length = IDENTITY_MAGIC.len() + 80;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() != length as u64
    {
        return Err(DeviceError::PrivateStorageRequired);
    }
    let mut encoded = Zeroizing::new(Vec::new());
    file.take(length as u64 + 1).read_to_end(&mut encoded)?;
    if encoded.len() != length || !encoded.starts_with(IDENTITY_MAGIC) {
        return Err(DeviceError::Invalid(
            "identity file is incomplete or unsupported",
        ));
    }
    let offset = IDENTITY_MAGIC.len();
    let uuid = uuid::Uuid::from_slice(&encoded[offset..offset + 16])
        .map_err(|_| DeviceError::Invalid("invalid identity UUID"))?;
    let signing_seed = Zeroizing::new(
        encoded[offset + 16..offset + 48]
            .try_into()
            .map_err(|_| DeviceError::Invalid("invalid signing seed"))?,
    );
    let encryption_secret = Zeroizing::new(
        encoded[offset + 48..offset + 80]
            .try_into()
            .map_err(|_| DeviceError::Invalid("invalid encryption secret"))?,
    );
    Ok(Arc::new(DeviceIdentity::from_secrets(
        DeviceIdentitySecrets {
            device_id: crate::protocol::DeviceId::try_from(uuid)?,
            signing_seed,
            encryption_secret,
        },
    )))
}

fn validate_label(value: &str) -> Result<(), DeviceError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(DeviceError::Invalid(
            "label requires 1–256 bytes without controls",
        ));
    }
    Ok(())
}
fn time(now: UnixMillis) -> Result<i64, DeviceError> {
    i64::try_from(now.get()).map_err(|_| DeviceError::Invalid("timestamp exceeds storage range"))
}
