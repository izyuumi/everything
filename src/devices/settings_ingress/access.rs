use std::fmt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::wire::{BindingGeneration, MAX_PRIVATE_REQUEST_BYTES, canonical_uuid, object};
use super::{SettingsReceiveError, VerifiedSettingsEnvelope, work::PrivateWitness};
use crate::agent::Agent;
use crate::crypto::DevicePublicKeys;
use crate::devices::{DeviceEndpoint, EndpointRole, SettingsConnectionIdentity};
use crate::host::settings::contract::{
    SETTINGS_CONTRACT_VERSION, SettingChange, SettingKey, SettingScopeKind, SettingsPatch,
    SettingsScope,
};
use crate::host::settings::service::SettingsReceiptAuthority;
use crate::protocol::{CommandId, DeviceId, UnixMillis};
use crate::storage::{Store, StoredSettingsAccessReply, TransportInboxMetadata};

const MAX_REMOTE_CHANGES: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsAccessStatusIntent {
    pub(crate) host_id: DeviceId,
    pub(crate) query_id: Uuid,
    pub(crate) issued_at: UnixMillis,
    pub(crate) expires_at: UnixMillis,
    pub(crate) request: SettingsRemoteRequest,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum SettingsRemoteRequest {
    AccessStatus,
    Snapshot {
        generation: BindingGeneration,
        keys: Vec<SettingKey>,
    },
    Changes {
        generation: BindingGeneration,
        after_sequence: u64,
        limit: usize,
    },
    Update {
        generation: BindingGeneration,
        request_id: CommandId,
        expected_revision: u64,
        changes: Vec<SettingChange>,
    },
    HostAction {
        action: SettingsHostAction,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingsHostAction {
    CredentialEntry,
    PairingInvitation,
    PairingApproval,
    DeviceRevocation,
    GrantChange,
    ExecutionPolicy,
    RecipientConsent,
    Lifecycle,
}

impl SettingsRemoteRequest {
    pub(crate) fn generation(&self) -> Option<&BindingGeneration> {
        match self {
            Self::Snapshot { generation, .. }
            | Self::Changes { generation, .. }
            | Self::Update { generation, .. } => Some(generation),
            Self::AccessStatus | Self::HostAction { .. } => None,
        }
    }

    fn valid(&self, host_id: DeviceId) -> bool {
        if self.generation().is_some_and(|generation| {
            generation.grant_revision == 0 || generation.grant_revision > i64::MAX as u64
        }) {
            return false;
        }
        match self {
            Self::AccessStatus | Self::HostAction { .. } => true,
            Self::Snapshot { keys, .. } => {
                let mut sorted = keys.clone();
                sorted.sort_unstable();
                sorted.dedup();
                !keys.is_empty()
                    && sorted.len() == keys.len()
                    && keys.iter().all(|key| key.scope() == SettingScopeKind::Host)
            }
            Self::Changes {
                after_sequence,
                limit,
                ..
            } => (1..=MAX_REMOTE_CHANGES).contains(limit) && *after_sequence <= i64::MAX as u64,
            Self::Update {
                request_id,
                expected_revision,
                changes,
                ..
            } => SettingsPatch {
                contract_version: SETTINGS_CONTRACT_VERSION,
                request_id: *request_id,
                scope: SettingsScope::Host { host_id },
                expected_revision: *expected_revision,
                changes: changes.clone(),
                confirmation: None,
            }
            .validate()
            .is_ok(),
        }
    }
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

    pub(super) fn reply_build_witness_mut(&mut self) -> &mut PrivateWitness {
        &mut self.verified._witness
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
            && self.request.valid(self.host_id)
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
        self.envelope.receipt_metadata()
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
        let observed = self.envelope.clock.observe()?;
        let outer_deadline = self
            .envelope
            .clock
            .deadline(self.envelope.outer_expires_at)?;
        if observed
            .0
            .get()
            .saturating_add(super::SETTINGS_CLOCK_SKEW_MS)
            < self.intent.issued_at.get()
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
            .deadline(input.intent.expires_at.get())?;
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
    let (time, request) = match packet.payload.message.body {
        Body::AccessStatus { time } => (time, SettingsRemoteRequest::AccessStatus),
        Body::Snapshot {
            generation,
            keys,
            time,
        } => (time, SettingsRemoteRequest::Snapshot { generation, keys }),
        Body::Changes {
            generation,
            after_sequence,
            limit,
            time,
        } => (
            time,
            SettingsRemoteRequest::Changes {
                generation,
                after_sequence,
                limit,
            },
        ),
        Body::Update {
            generation,
            request_id,
            expected_revision,
            changes,
            time,
        } => {
            let Ok(request_id) = CommandId::try_from(request_id) else {
                return DecodedAccessStatus::Invalid;
            };
            (
                time,
                SettingsRemoteRequest::Update {
                    generation,
                    request_id,
                    expected_revision,
                    changes,
                },
            )
        }
        Body::HostAction { action, time } => (time, SettingsRemoteRequest::HostAction { action }),
    };
    let (Ok(host_id), Ok(issued_at), Ok(expires_at)) = (
        DeviceId::try_from(packet.payload.host_id),
        UnixMillis::try_from(time.issued_at),
        UnixMillis::try_from(time.expires_at),
    ) else {
        return DecodedAccessStatus::Invalid;
    };
    let intent = SettingsAccessStatusIntent {
        host_id,
        query_id: packet.payload.message.query_id,
        issued_at,
        expires_at,
        request,
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
    body: Body,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Body {
    AccessStatus {
        time: RequestTime,
    },
    Snapshot {
        generation: BindingGeneration,
        keys: Vec<SettingKey>,
        time: RequestTime,
    },
    Changes {
        generation: BindingGeneration,
        after_sequence: u64,
        limit: usize,
        time: RequestTime,
    },
    Update {
        generation: BindingGeneration,
        #[serde(deserialize_with = "canonical_uuid")]
        request_id: Uuid,
        expected_revision: u64,
        changes: Vec<SettingChange>,
        time: RequestTime,
    },
    HostAction {
        action: SettingsHostAction,
        time: RequestTime,
    },
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
