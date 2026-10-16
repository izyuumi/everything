use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::devices::{DeviceEndpoint, PairingCancellation};
use crate::host::credentials::SecretTransfer;
use crate::protocol::{CommandId, MessageId, UnixMillis};

use super::SettingsRepository;
use super::authority::{Authority, SessionAdmission};
use super::contract::{
    NativeSettingsAction, NotificationAuthorization, SETTINGS_CONTRACT_VERSION, SettingKey,
    SettingsActionReference, SettingsActionRequest, SettingsConfirmationReference,
    SettingsDeviceRequest, SettingsOperationResult, SettingsOperationState, SettingsPatch,
    SettingsScope, SettingsSection,
};
use super::operations::{
    NativeRequest, OperationIdentity, OperationRepository, SettingsOperationInspection,
};
use super::service::SettingsServiceError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SettingsActionTarget {
    Patch(SettingsPatch),
    Native(SettingsActionRequest),
    Device(SettingsDeviceRequest),
    OwnerReplacement(OwnerReplacementIdentity),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerReplacementIdentity {
    pub request_id: CommandId,
    pub scope: SettingsScope,
    pub expected_revision: u64,
    pub exact_settings_sha256: String,
    pub changed_keys: Vec<SettingKey>,
    pub restart_required: bool,
}

impl SettingsActionTarget {
    pub(super) fn validate(&self) -> Result<(), SettingsServiceError> {
        match self {
            Self::Patch(patch) => {
                patch.validate()?;
                if patch.confirmation.is_some() {
                    return Err(SettingsServiceError::Invalid("fresh confirmation metadata"));
                }
            }
            Self::Native(request) => request.validate()?,
            Self::Device(request) => request.validate()?,
            Self::OwnerReplacement(request) => {
                super::operations::validate_digest(&request.exact_settings_sha256)?;
                if !matches!(request.scope, SettingsScope::Host { .. })
                    || request.expected_revision == 0
                    || request.expected_revision >= i64::MAX as u64
                    || request.changed_keys.len() > SettingKey::ALL.len()
                    || !request
                        .changed_keys
                        .windows(2)
                        .all(|keys| keys[0] < keys[1])
                    || request
                        .changed_keys
                        .iter()
                        .any(|key| key.scope() != super::contract::SettingScopeKind::Host)
                {
                    return Err(SettingsServiceError::Invalid("owner replacement identity"));
                }
            }
        }
        Ok(())
    }

    pub fn request_id(&self) -> CommandId {
        match self {
            Self::Patch(request) => request.request_id,
            Self::Native(request) => request.request_id,
            Self::Device(request) => request.request_id,
            Self::OwnerReplacement(request) => request.request_id,
        }
    }

    pub fn scope(&self) -> &SettingsScope {
        match self {
            Self::Patch(request) => &request.scope,
            Self::Native(request) => &request.scope,
            Self::Device(request) => &request.scope,
            Self::OwnerReplacement(request) => &request.scope,
        }
    }

    pub fn expected_revision(&self) -> Option<u64> {
        match self {
            Self::Patch(request) => Some(request.expected_revision),
            Self::Native(request) => Some(request.expected_revision),
            Self::Device(_) => None,
            Self::OwnerReplacement(request) => Some(request.expected_revision),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "reference",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum NativeActionReference {
    Confirmation(SettingsConfirmationReference),
    Action(SettingsActionReference),
}

impl NativeActionReference {
    pub fn id(&self) -> uuid::Uuid {
        match self {
            Self::Confirmation(reference) => reference.id,
            Self::Action(reference) => reference.id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeActionInvocation {
    pub operation_id: CommandId,
    pub native_request_id: CommandId,
    pub reference: NativeActionReference,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeActionObservation {
    Dismissed,
    NotificationAuthorization { state: NotificationAuthorization },
    SectionOpened { section: SettingsSection },
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeActionDetails {
    pub operation: SettingsOperationInspection,
    pub target: SettingsActionTarget,
}

pub enum NativeActionAdmissionResult {
    Admitted(NativeActionAdmission),
    Existing(Box<SettingsOperationInspection>),
}

pub struct NativeActionAdmission {
    pub(super) claim: Arc<ActionClaim>,
}

impl fmt::Debug for NativeActionAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeActionAdmission([retained])")
    }
}

impl NativeActionAdmission {
    pub fn target(&self) -> &SettingsActionTarget {
        &self.claim.identity.target
    }

    pub fn operation_id(&self) -> CommandId {
        self.claim.identity.binding.request_id
    }

    pub fn native_request_id(&self) -> CommandId {
        self.claim.native.native_request_id
    }

    pub fn check_new_effect(&self) -> Result<(), SettingsServiceError> {
        let state = self
            .claim
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        self.claim.check_effect(&state)
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.claim.cancellation.child_token()
    }
}

pub struct NativeHostEffectAdmission {
    pub(super) _mutation: OwnedMutexGuard<()>,
    pub(super) claim: Arc<ActionClaim>,
}

impl fmt::Debug for NativeHostEffectAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeHostEffectAdmission([retained])")
    }
}

impl NativeHostEffectAdmission {
    pub fn target(&self) -> &SettingsActionTarget {
        &self.claim.identity.target
    }
    pub fn operation_id(&self) -> CommandId {
        self.claim.identity.binding.request_id
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.claim.cancellation.child_token()
    }
}

pub(super) struct ActionClaim {
    pub identity: OperationIdentity,
    pub native: NativeRequest,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub state: Mutex<ClaimState>,
    pub repository: OperationRepository,
    pub authority: Arc<Authority>,
    pub settings: SettingsRepository,
    pub session: Mutex<Option<SessionAdmission>>,
    pub _agent: Agent,
}

pub(super) struct ClaimState {
    pub closed: bool,
    pub host_effect_started: bool,
    pub pairing: Option<Arc<PendingPairingLease>>,
}

impl ActionClaim {
    pub fn check_effect(&self, state: &ClaimState) -> Result<(), SettingsServiceError> {
        validate_current(&self.authority, &self.settings, &self.identity)?;
        if state.closed
            || self.cancellation.is_cancelled()
            || Instant::now() >= self.deadline
            || UnixMillis::now()?.get() >= self.identity.binding.expires_at.get()
        {
            return Err(SettingsServiceError::Expired);
        }
        if state.host_effect_started {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(())
    }

    pub fn close(&self) -> Result<(), SettingsServiceError> {
        self.state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .closed = true;
        self.cancellation.cancel();
        Ok(())
    }

    pub fn observe(
        &self,
        observation: NativeActionObservation,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if state.host_effect_started {
            return Err(SettingsServiceError::OutsideScope);
        }
        match (&self.identity.target, &observation) {
            (SettingsActionTarget::Native(_), NativeActionObservation::Dismissed) => {}
            (
                SettingsActionTarget::Native(SettingsActionRequest {
                    action: NativeSettingsAction::NotificationAuthorization,
                    ..
                }),
                NativeActionObservation::NotificationAuthorization { .. },
            ) => {}
            (
                SettingsActionTarget::Native(SettingsActionRequest {
                    action: NativeSettingsAction::OpenSection { section: expected },
                    ..
                }),
                NativeActionObservation::SectionOpened { section },
            ) if expected == section => {}
            _ => return Err(SettingsServiceError::OutsideScope),
        }
        let record = self.repository.get(self.identity.binding.request_id)?;
        if let super::operations::SettingsOperationPhase::Finished { result } = record.phase() {
            return self.repository.finish(
                &self.identity,
                self.native.native_request_id,
                result.clone(),
                Some(observation),
            );
        }
        let result = SettingsOperationResult {
            contract_version: SETTINGS_CONTRACT_VERSION,
            request_id: self.identity.binding.request_id,
            scope: self.identity.binding.scope.clone(),
            state: if observation == NativeActionObservation::Dismissed {
                SettingsOperationState::Cancelled
            } else {
                SettingsOperationState::Completed {
                    receipt_id: uuid::Uuid::new_v4(),
                }
            },
        };
        let inspection = self.repository.finish(
            &self.identity,
            self.native.native_request_id,
            result,
            Some(observation),
        )?;
        state.closed = true;
        self.cancellation.cancel();
        drop(state);
        let retained_session = self
            .session
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .take();
        drop(retained_session);
        Ok(inspection)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePairingLifecycle {
    Retained,
    CancellationPending,
    Cancelled,
    Expired,
    NoLongerPending,
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePairingCreation {
    pub operation: SettingsOperationInspection,
    pub lifecycle: NativePairingLifecycle,
    pub invitation: Option<NativePairingInvitation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePairingInvitation {
    pub operation_id: CommandId,
    pub pairing_id: MessageId,
    pub expires_at: UnixMillis,
    pub fingerprint: String,
    pub invitation: SecretTransfer,
}

impl fmt::Debug for NativePairingInvitation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativePairingInvitation([redacted])")
    }
}

pub(super) struct PendingPairingLease {
    pub operation_id: CommandId,
    pub session_id: uuid::Uuid,
    pub pairing_id: MessageId,
    pub expires_at: UnixMillis,
    pub deadline: Instant,
    pub fingerprint: String,
    pub endpoint: DeviceEndpoint,
    pub state: Mutex<PendingPairingState>,
    pub _session: SessionAdmission,
    pub _agent: Agent,
}

pub(super) struct PendingPairingState {
    pub lifecycle: NativePairingLifecycle,
    pub invitation: Option<SecretTransfer>,
}

impl PendingPairingLease {
    pub fn cancel(&self) -> Result<NativePairingLifecycle, SettingsServiceError> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            match state.lifecycle {
                NativePairingLifecycle::Retained | NativePairingLifecycle::CancellationPending => {
                    state.lifecycle = NativePairingLifecycle::CancellationPending
                }
                other => return Ok(other),
            }
        }
        let outcome = self
            .endpoint
            .cancel_pairing(self.pairing_id, UnixMillis::now()?)?;
        let lifecycle = match outcome {
            PairingCancellation::Cancelled => NativePairingLifecycle::Cancelled,
            PairingCancellation::Expired => NativePairingLifecycle::Expired,
            PairingCancellation::NotPending => NativePairingLifecycle::NoLongerPending,
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if state.lifecycle == NativePairingLifecycle::CancellationPending {
            state.lifecycle = lifecycle;
        }
        state.invitation = None;
        Ok(state.lifecycle)
    }

    pub fn lifecycle(&self) -> Result<NativePairingLifecycle, SettingsServiceError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .lifecycle)
    }

    pub fn invitation(&self) -> Result<NativePairingInvitation, SettingsServiceError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if self._session.cancellation.is_cancelled()
            || Instant::now() >= self.deadline
            || UnixMillis::now()?.get() >= self.expires_at.get()
        {
            return Err(SettingsServiceError::Expired);
        }
        if state.lifecycle != NativePairingLifecycle::Retained {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(NativePairingInvitation {
            operation_id: self.operation_id,
            pairing_id: self.pairing_id,
            expires_at: self.expires_at,
            fingerprint: self.fingerprint.clone(),
            invitation: state
                .invitation
                .clone()
                .ok_or(SettingsServiceError::NotFound)?,
        })
    }
}

pub(super) fn validate_current(
    authority: &Authority,
    settings: &SettingsRepository,
    identity: &OperationIdentity,
) -> Result<(), SettingsServiceError> {
    if let Some(expected) = identity.binding.expected_revision
        && settings.snapshot()?.revision != expected
    {
        return Err(SettingsServiceError::Conflict);
    }
    authority.check_origin(&identity.origin)
}
