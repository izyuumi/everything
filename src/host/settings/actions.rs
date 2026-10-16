use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::protocol::{CommandId, UnixMillis};

use super::authority::SessionAdmission;
use super::contract::{
    NativeSettingsAction, NotificationAuthorization, SETTINGS_CONTRACT_VERSION,
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
        }
        Ok(())
    }

    pub fn request_id(&self) -> CommandId {
        match self {
            Self::Patch(request) => request.request_id,
            Self::Native(request) => request.request_id,
            Self::Device(request) => request.request_id,
        }
    }

    pub fn scope(&self) -> &SettingsScope {
        match self {
            Self::Patch(request) => &request.scope,
            Self::Native(request) => &request.scope,
            Self::Device(request) => &request.scope,
        }
    }

    pub fn expected_revision(&self) -> Option<u64> {
        match self {
            Self::Patch(request) => Some(request.expected_revision),
            Self::Native(request) => Some(request.expected_revision),
            Self::Device(_) => None,
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
    Existing(SettingsOperationInspection),
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
        if state.closed
            || self.claim.cancellation.is_cancelled()
            || Instant::now() >= self.claim.deadline
            || UnixMillis::now()?.get() >= self.claim.identity.binding.expires_at.get()
        {
            return Err(SettingsServiceError::Expired);
        }
        Ok(())
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
    pub session: Mutex<Option<SessionAdmission>>,
    pub _agent: Agent,
}

pub(super) struct ClaimState {
    pub closed: bool,
    pub host_effect_started: bool,
}

impl ActionClaim {
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
