use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::host::runtime::{HostError, HostInner};
use crate::protocol::CommandId;
use crate::transport::local::LocalCall;

use super::actions::{
    NativeActionAdmissionResult, NativeActionInvocation, NativeActionObservation,
    NativeActionReference, SettingsActionTarget,
};
use super::contract::{NativeSettingsAction, SettingsActionRequest, SettingsOperationResult};
use super::{
    NativeSessionOpened, NativeSessionProof, SettingsOperationInspection, SettingsOperationPhase,
};

pub const MAX_NATIVE_SETTINGS_INPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSettingsInput {
    pub request_id: CommandId,
    pub operation: NativeSettingsOperation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeSettingsOperation {
    Open,
    RequestAction {
        proof: NativeSessionProof,
        request: SettingsActionRequest,
    },
    Details {
        proof: NativeSessionProof,
        operation_id: CommandId,
        reference: NativeActionReference,
    },
    Admit {
        proof: NativeSessionProof,
        invocation: NativeActionInvocation,
    },
    Inspect {
        proof: NativeSessionProof,
        operation_id: CommandId,
    },
    Finalize {
        proof: NativeSessionProof,
        invocation: NativeActionInvocation,
        observation: NativeActionObservation,
    },
    Cancel {
        proof: NativeSessionProof,
        operation_id: CommandId,
    },
    Close {
        proof: NativeSessionProof,
    },
}

impl NativeSettingsOperation {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Self::RequestAction { request, .. } = self {
            request
                .validate()
                .map_err(|_| "invalid native settings action")?;
        }
        let bytes = zeroize::Zeroizing::new(
            serde_json::to_vec(self).map_err(|_| "invalid native settings request")?,
        );
        if bytes.len() > MAX_NATIVE_SETTINGS_INPUT_BYTES {
            return Err("native settings request exceeds its byte limit");
        }
        Ok(())
    }

    fn allows_during_drain(&self) -> bool {
        matches!(
            self,
            Self::Inspect { .. } | Self::Finalize { .. } | Self::Cancel { .. } | Self::Close { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeSettingsResult {
    Opened {
        session: NativeSessionOpened,
    },
    Prepared {
        result: SettingsOperationResult,
    },
    Details {
        operation: SettingsOperationInspection,
        target: SettingsActionTarget,
    },
    Admitted {
        operation: SettingsOperationInspection,
        target: SettingsActionTarget,
    },
    Inspected {
        operation: SettingsOperationInspection,
    },
    Closed {
        session_id: Uuid,
    },
}

impl NativeSettingsResult {
    pub fn validate_for(&self, request: &NativeSettingsOperation) -> Result<(), &'static str> {
        match (request, self) {
            (NativeSettingsOperation::Open, Self::Opened { session })
                if !session.proof.session_id().is_nil() && session.expires_at.get() > 0 =>
            {
                Ok(())
            }
            (NativeSettingsOperation::RequestAction { request, .. }, Self::Prepared { result })
                if result.request_id == request.request_id && result.scope == request.scope =>
            {
                result
                    .validate()
                    .map_err(|_| "invalid native action result")
            }
            (NativeSettingsOperation::Close { proof }, Self::Closed { session_id })
                if proof.session_id() == *session_id =>
            {
                Ok(())
            }
            (
                NativeSettingsOperation::Details { operation_id, .. },
                Self::Details { operation, target },
            ) if operation.binding.request_id == *operation_id
                && target.request_id() == *operation_id
                && target.scope() == &operation.binding.scope =>
            {
                target
                    .validate()
                    .map_err(|_| "invalid native action target")?;
                validate_inspection(operation)
            }
            (
                NativeSettingsOperation::Admit { invocation, .. },
                Self::Admitted { operation, target },
            ) if operation.binding.request_id == invocation.operation_id
                && matches!(operation.phase, SettingsOperationPhase::InFlight { native_request_id } if native_request_id == invocation.native_request_id)
                && target.request_id() == invocation.operation_id
                && target.scope() == &operation.binding.scope
                && presentation_target(target) =>
            {
                target
                    .validate()
                    .map_err(|_| "invalid native action target")?;
                validate_inspection(operation)
            }
            (
                NativeSettingsOperation::Admit { invocation, .. }
                | NativeSettingsOperation::Finalize { invocation, .. },
                Self::Inspected { operation },
            ) if operation.binding.request_id == invocation.operation_id => {
                validate_inspection(operation)
            }
            (
                NativeSettingsOperation::Inspect { operation_id, .. }
                | NativeSettingsOperation::Cancel { operation_id, .. },
                Self::Inspected { operation },
            ) if operation.binding.request_id == *operation_id => validate_inspection(operation),
            _ => Err("native settings response does not match its request"),
        }
    }
}

fn validate_inspection(operation: &SettingsOperationInspection) -> Result<(), &'static str> {
    operation
        .binding
        .validate()
        .map_err(|_| "invalid settings operation binding")?;
    if let SettingsOperationPhase::Finished { result } = &operation.phase {
        result
            .validate()
            .map_err(|_| "invalid settings operation result")?;
        if result.request_id != operation.binding.request_id
            || result.scope != operation.binding.scope
        {
            return Err("settings operation result does not match its binding");
        }
    }
    Ok(())
}

fn presentation_target(target: &SettingsActionTarget) -> bool {
    matches!(
        target,
        SettingsActionTarget::Native(SettingsActionRequest {
            action: NativeSettingsAction::NotificationAuthorization
                | NativeSettingsAction::OpenSection { .. },
            ..
        })
    )
}

impl HostInner {
    pub(crate) async fn native_settings(
        self: &Arc<Self>,
        call: &LocalCall,
        operation: NativeSettingsOperation,
    ) -> Result<NativeSettingsResult, HostError> {
        if !operation.allows_during_drain() {
            self.ensure_accepting()?;
        }
        let service = &self.settings_service;
        match operation {
            NativeSettingsOperation::Open => Ok(NativeSettingsResult::Opened {
                session: service.open_native_session(call)?,
            }),
            NativeSettingsOperation::RequestAction { proof, request } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Prepared {
                    result: service
                        .request_action(caller, request, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::Details {
                proof,
                operation_id,
                reference,
            } => {
                let caller = service.native_caller(call, &proof)?;
                let details = service
                    .native_action_details(caller, operation_id, reference)
                    .await?;
                Ok(NativeSettingsResult::Details {
                    operation: details.operation,
                    target: details.target,
                })
            }
            NativeSettingsOperation::Admit { proof, invocation } => {
                let caller = service.native_caller(call, &proof)?;
                let original = service.operation(caller, invocation.operation_id).await?;
                if matches!(
                    original.phase,
                    SettingsOperationPhase::AwaitingAction { .. }
                        | SettingsOperationPhase::AwaitingConfirmation { .. }
                ) {
                    let caller = service.native_caller(call, &proof)?;
                    let details = service
                        .native_action_details(
                            caller,
                            invocation.operation_id,
                            invocation.reference.clone(),
                        )
                        .await?;
                    if !presentation_target(&details.target) {
                        return Err(HostError::Invalid(
                            "the native adapter for this action is unavailable",
                        ));
                    }
                }
                let caller = service.native_caller(call, &proof)?;
                match service
                    .admit_native_action(caller, invocation.clone(), call.cancellation.clone())
                    .await?
                {
                    NativeActionAdmissionResult::Existing(operation) => {
                        Ok(NativeSettingsResult::Inspected { operation })
                    }
                    NativeActionAdmissionResult::Admitted(admission) => {
                        let target = admission.target().clone();
                        let caller = service.native_caller(call, &proof)?;
                        let operation = service.operation(caller, invocation.operation_id).await?;
                        admission.check_new_effect()?;
                        Ok(NativeSettingsResult::Admitted { operation, target })
                    }
                }
            }
            NativeSettingsOperation::Inspect {
                proof,
                operation_id,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service
                        .inspect_native_operation(caller, operation_id)
                        .await?,
                })
            }
            NativeSettingsOperation::Finalize {
                proof,
                invocation,
                observation,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service
                        .finalize_native_action(caller, invocation, observation)
                        .await?,
                })
            }
            NativeSettingsOperation::Cancel {
                proof,
                operation_id,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service.cancel_native_action(caller, operation_id).await?,
                })
            }
            NativeSettingsOperation::Close { proof } => {
                let session_id = proof.session_id();
                let caller = service.native_cleanup_caller(call, &proof)?;
                service.close_native_session(caller).await?;
                Ok(NativeSettingsResult::Closed { session_id })
            }
        }
    }
}
