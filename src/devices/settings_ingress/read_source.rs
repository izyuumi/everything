use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::command::SettingsParentReference;
use super::read::{SettingsReadContractError, SettingsReadRequest};
use super::{BindRejection, VerifiedSettingsEnvelope, work::PrivateWitness};
use crate::agent::Agent;
use crate::crypto::DevicePublicKeys;
use crate::devices::{DeviceEndpoint, SettingsConnectionIdentity};
use crate::host::settings::service::SettingsReceiptAuthority;
use crate::protocol::{DeviceId, UnixMillis};
use crate::storage::{Store, TransportInboxMetadata};

pub(crate) struct AuthenticatedSettingsReadSource {
    request: SettingsReadRequest,
    envelope: VerifiedSettingsEnvelope,
    invocation: Arc<()>,
    deadline: Instant,
}

pub(crate) struct OwnedVerifiedSettingsRead {
    authority: SettingsReceiptAuthority,
    source: AuthenticatedSettingsReadSource,
    _witness: PrivateWitness,
}

struct ReadVerificationInput {
    authority: SettingsReceiptAuthority,
    envelope: VerifiedSettingsEnvelope,
    request: SettingsReadRequest,
    witness: PrivateWitness,
}

#[derive(Error)]
pub(crate) enum SettingsReadSourceError {
    #[error("Settings read data validation failed")]
    Data(#[from] SettingsReadContractError),
    #[error("Settings read verified source check failed")]
    Source(#[from] BindRejection),
}

impl OwnedVerifiedSettingsRead {
    pub(super) fn from_verified(
        envelope: VerifiedSettingsEnvelope,
        request: SettingsReadRequest,
        authority: SettingsReceiptAuthority,
        witness: PrivateWitness,
    ) -> Result<Self, SettingsReadSourceError> {
        let input = ReadVerificationInput {
            authority,
            envelope,
            request,
            witness,
        };
        input.request.validate()?;
        let (_, _, _, _, expires_at) = request_parts(&input.request);
        let deadline = input.envelope.clock.deadline(expires_at.get())?;
        let invocation = Arc::new(());
        let owner = Self {
            authority: input.authority,
            source: AuthenticatedSettingsReadSource {
                request: input.request,
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

    pub(crate) fn authority_source(&self) -> &AuthenticatedSettingsReadSource {
        &self.source
    }

    pub(crate) fn receipt_authority(&self) -> &SettingsReceiptAuthority {
        &self.authority
    }
}

impl AuthenticatedSettingsReadSource {
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

    pub(crate) fn pinned_identity(&self) -> &Value {
        self.envelope.pinned_identity()
    }

    pub(crate) fn request(&self) -> &SettingsReadRequest {
        &self.request
    }

    pub(crate) fn invocation_identity(&self) -> &Arc<()> {
        &self.invocation
    }

    pub(crate) fn receipt_metadata(
        &self,
    ) -> Result<TransportInboxMetadata<'_>, SettingsReadSourceError> {
        self.envelope
            .receipt_metadata()
            .map_err(SettingsReadSourceError::Source)
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
    ) -> Result<(UnixMillis, Instant), SettingsReadSourceError> {
        self.request.validate()?;
        let (host_id, query_id, parent, issued_at, expires_at) = request_parts(&self.request);
        if self.envelope.message_id == query_id.to_string() {
            return Err(BindRejection::OutsideScope.into());
        }
        self.envelope
            .check_precommit(
                store,
                host_id,
                &parent.generation,
                issued_at.get(),
                expires_at.get(),
            )
            .map_err(SettingsReadSourceError::Source)
    }
}

fn request_parts(
    request: &SettingsReadRequest,
) -> (
    DeviceId,
    Uuid,
    &SettingsParentReference,
    UnixMillis,
    UnixMillis,
) {
    match request {
        SettingsReadRequest::Snapshot(intent) => (
            intent.host_id,
            intent.query_id,
            &intent.parent,
            intent.issued_at,
            intent.expires_at,
        ),
        SettingsReadRequest::InspectOriginal(intent) => (
            intent.host_id,
            intent.query_id,
            &intent.parent,
            intent.issued_at,
            intent.expires_at,
        ),
    }
}

impl fmt::Debug for SettingsReadSourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SettingsReadSourceError([retained cause])")
    }
}

impl fmt::Debug for AuthenticatedSettingsReadSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatedSettingsReadSource([owned verification])")
    }
}

impl fmt::Debug for OwnedVerifiedSettingsRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OwnedVerifiedSettingsRead([private])")
    }
}
