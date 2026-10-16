use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde::{Deserialize, Deserializer};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::wire::{MAX_PRIVATE_REQUEST_BYTES, canonical_uuid, object};
use super::{SettingsReceiveError, VerifiedSettingsEnvelope, work::PrivateWitness};
use crate::agent::Agent;
use crate::crypto::DevicePublicKeys;
use crate::devices::{DeviceEndpoint, EndpointRole, SettingsConnectionIdentity};
use crate::host::settings::service::SettingsReceiptAuthority;
use crate::protocol::{DeviceId, UnixMillis};
use crate::storage::{Store, StoredSettingsAccessReply, TransportInboxMetadata};

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsAccessStatusIntent {
    pub(crate) host_id: DeviceId,
    pub(crate) query_id: Uuid,
    pub(crate) issued_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
}

pub(crate) struct AuthenticatedSettingsAccessSource {
    intent: SettingsAccessStatusIntent,
    envelope: VerifiedSettingsEnvelope,
    invocation: Arc<()>,
    deadline: Instant,
}

pub(crate) struct OwnedVerifiedSettingsAccess {
    authority: SettingsReceiptAuthority,
    source: AuthenticatedSettingsAccessSource,
    _witness: PrivateWitness,
}

pub(super) struct AccessReceiptOutput {
    pub(super) reply: StoredSettingsAccessReply,
    pub(super) verified: Box<OwnedVerifiedSettingsAccess>,
}

pub(super) enum AccessWorkState {
    Rejected(SettingsReceiveError),
    Verified {
        issue: Option<SettingsReceiveError>,
        reply: Option<StoredSettingsAccessReply>,
        verified: Box<OwnedVerifiedSettingsAccess>,
    },
}

pub(crate) struct OwnedCommittedSettingsAccess {
    reply: StoredSettingsAccessReply,
    verified: Box<OwnedVerifiedSettingsAccess>,
}

impl AccessWorkState {
    pub(super) fn verified(&self) -> Option<&OwnedVerifiedSettingsAccess> {
        match self {
            Self::Rejected(_) => None,
            Self::Verified { verified, .. } => Some(verified),
        }
    }

    pub(super) fn witness(&self) -> Option<&PrivateWitness> {
        self.verified().map(|verified| &verified._witness)
    }

    pub(super) fn witness_mut(&mut self) -> Option<&mut PrivateWitness> {
        match self {
            Self::Rejected(_) => None,
            Self::Verified { verified, .. } => Some(&mut verified._witness),
        }
    }
}

impl OwnedCommittedSettingsAccess {
    pub(super) fn from_joined(output: AccessReceiptOutput) -> Self {
        Self {
            reply: output.reply,
            verified: output.verified,
        }
    }

    pub(crate) fn verified_request(&self) -> &OwnedVerifiedSettingsAccess {
        &self.verified
    }

    pub(crate) fn reply_reservation(&self) -> &StoredSettingsAccessReply {
        &self.reply
    }

    pub(super) fn reply_build_witness(&self) -> &PrivateWitness {
        &self.verified._witness
    }
}

impl fmt::Debug for OwnedCommittedSettingsAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedCommittedSettingsAccess([joined receipt])")
    }
}

struct AccessVerificationInput {
    authority: SettingsReceiptAuthority,
    envelope: VerifiedSettingsEnvelope,
    intent: SettingsAccessStatusIntent,
    witness: PrivateWitness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum AccessStatusRejection {
    #[error("original AccessStatus request is expired")]
    Expired,
    #[error("AccessStatus source is outside its current scope")]
    OutsideScope,
    #[error("AccessStatus source observation is unavailable")]
    Unavailable,
}

pub(super) enum DecodedAccessStatus {
    AccessStatus(SettingsAccessStatusIntent),
    Invalid,
}

impl SettingsAccessStatusIntent {
    fn valid(&self) -> bool {
        !self.host_id.as_uuid().is_nil()
            && !self.query_id.is_nil()
            && self.issued_at.get() > 0
            && self.expires_at > self.issued_at
            && self.expires_at.get() - self.issued_at.get() <= 30_000
    }
}

impl AuthenticatedSettingsAccessSource {
    pub(crate) fn matches_context(
        &self,
        agent: &Agent,
        endpoint: &DeviceEndpoint,
        mutation: &Arc<Mutex<()>>,
    ) -> bool {
        self.envelope.matches_context(agent, endpoint, mutation)
    }

    pub(crate) fn connection(&self) -> &Arc<SettingsConnectionIdentity> {
        self.envelope.connection()
    }

    pub(crate) fn verified_peer_keys(&self) -> &DevicePublicKeys {
        self.envelope.verified_peer_keys()
    }

    pub(crate) fn intent(&self) -> &SettingsAccessStatusIntent {
        &self.intent
    }

    pub(crate) fn invocation_identity(&self) -> &Arc<()> {
        &self.invocation
    }

    pub(crate) fn receipt_metadata(
        &self,
    ) -> Result<TransportInboxMetadata<'_>, AccessStatusRejection> {
        self.envelope
            .receipt_metadata()
            .map_err(|_| AccessStatusRejection::Expired)
    }

    pub(crate) fn pinned_identity(&self) -> &Value {
        self.envelope.pinned_identity()
    }

    pub(crate) fn receipt_floor(&self) -> (UnixMillis, Instant) {
        (self.envelope.clock.wall, self.envelope.clock.monotonic)
    }

    pub(crate) fn original_deadline(&self) -> Instant {
        self.deadline
    }

    pub(crate) fn check_precommit(
        &self,
        store: &Store,
    ) -> Result<(UnixMillis, Instant), AccessStatusRejection> {
        if !self.intent.valid()
            || !self.envelope.guard_active.load(Ordering::Acquire)
            || !self.envelope.connection.is_live()
            || !self.envelope.context.endpoint.shares_store(store)
            || !self.envelope.context.agent.store().shares_connection(store)
            || self.envelope.context.endpoint.role() != &EndpointRole::Host
            || self.intent.host_id != self.envelope.connection.host_identity().host_id()
            || self.envelope.outer_expires_at <= self.envelope.outer_created_at
            || self.envelope.outer_expires_at - self.envelope.outer_created_at > 30_000
            || self.envelope.outer_expires_at > self.intent.expires_at.get()
        {
            return Err(AccessStatusRejection::OutsideScope);
        }
        let observed = self
            .envelope
            .clock
            .observe()
            .map_err(|_| AccessStatusRejection::Unavailable)?;
        let outer_deadline = self
            .envelope
            .clock
            .deadline(self.envelope.outer_expires_at)
            .map_err(|_| AccessStatusRejection::Expired)?;
        if observed.0 < self.intent.issued_at
            || observed.0 >= self.intent.expires_at
            || observed.0.get() >= self.envelope.outer_expires_at
            || observed.1 >= self.deadline
            || observed.1 >= outer_deadline
        {
            return Err(AccessStatusRejection::Expired);
        }
        Ok(observed)
    }
}

impl OwnedVerifiedSettingsAccess {
    pub(super) fn from_verified(
        envelope: VerifiedSettingsEnvelope,
        intent: SettingsAccessStatusIntent,
        authority: SettingsReceiptAuthority,
        witness: PrivateWitness,
    ) -> Result<Self, AccessStatusRejection> {
        let input = AccessVerificationInput {
            authority,
            envelope,
            intent,
            witness,
        };
        if !input.intent.valid() {
            return Err(AccessStatusRejection::OutsideScope);
        }
        let deadline = input
            .envelope
            .clock
            .deadline(input.intent.expires_at.get())
            .map_err(|_| AccessStatusRejection::Expired)?;
        let invocation = Arc::new(());
        let owner = Self {
            authority: input.authority,
            source: AuthenticatedSettingsAccessSource {
                intent: input.intent,
                envelope: input.envelope,
                invocation,
                deadline,
            },
            _witness: input.witness,
        };
        owner
            .source
            .check_precommit(&owner.source.envelope.context.agent.store())?;
        Ok(owner)
    }

    pub(crate) fn authority_source(&self) -> &AuthenticatedSettingsAccessSource {
        &self.source
    }

    pub(crate) fn receipt_authority(&self) -> &SettingsReceiptAuthority {
        &self.authority
    }
}

impl fmt::Debug for SettingsAccessStatusIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsAccessStatusIntent([metadata])")
    }
}

impl fmt::Debug for AuthenticatedSettingsAccessSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatedSettingsAccessSource([owned verification])")
    }
}

impl fmt::Debug for OwnedVerifiedSettingsAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedVerifiedSettingsAccess([private])")
    }
}

pub(super) fn decode(bytes: &[u8]) -> DecodedAccessStatus {
    if bytes.len() > MAX_PRIVATE_REQUEST_BYTES {
        return DecodedAccessStatus::Invalid;
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let Ok(packet) = object::<Packet, _>(&mut deserializer) else {
        return DecodedAccessStatus::Invalid;
    };
    if deserializer.end().is_err() || packet.schema_version != 1 || packet.payload.version != 1 {
        return DecodedAccessStatus::Invalid;
    }
    let (Ok(host_id), Ok(issued_at), Ok(expires_at)) = (
        DeviceId::try_from(packet.payload.host_id),
        UnixMillis::try_from(packet.payload.message.body.time.issued_at),
        UnixMillis::try_from(packet.payload.message.body.time.expires_at),
    ) else {
        return DecodedAccessStatus::Invalid;
    };
    let intent = SettingsAccessStatusIntent {
        host_id,
        query_id: packet.payload.message.query_id,
        issued_at,
        expires_at,
    };
    if !intent.valid() {
        return DecodedAccessStatus::Invalid;
    }
    DecodedAccessStatus::AccessStatus(intent)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet {
    schema_version: u16,
    #[serde(deserialize_with = "object")]
    payload: Payload,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    #[serde(rename = "kind", deserialize_with = "settings_kind")]
    _kind: (),
    version: u16,
    #[serde(deserialize_with = "canonical_uuid")]
    host_id: Uuid,
    #[serde(deserialize_with = "object")]
    message: Query,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    #[serde(rename = "kind", deserialize_with = "query_kind")]
    _kind: (),
    #[serde(deserialize_with = "canonical_uuid")]
    query_id: Uuid,
    #[serde(deserialize_with = "object")]
    body: AccessStatus,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessStatus {
    #[serde(rename = "kind", deserialize_with = "access_status_kind")]
    _kind: (),
    #[serde(deserialize_with = "object")]
    time: RequestTime,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestTime {
    issued_at: u64,
    expires_at: u64,
}

fn settings_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "settings")
}

fn query_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "query")
}

fn access_status_kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    require_kind(deserializer, "access_status")
}

fn require_kind<'de, D: Deserializer<'de>>(
    deserializer: D,
    expected: &'static str,
) -> Result<(), D::Error> {
    let value = String::deserialize(deserializer)?;
    if value != expected {
        return Err(serde::de::Error::custom("unsupported AccessStatus kind"));
    }
    Ok(())
}
