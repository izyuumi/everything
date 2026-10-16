use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::host::runtime::{HostError, HostInner};
use crate::protocol::{CommandId, DeviceId};
use crate::transport::local::LocalCall;

use super::actions::{
    NativeActionAdmissionResult, NativeActionInvocation, NativeActionObservation,
    NativeActionReference, NativePairingCreation, SettingsActionTarget,
};
use super::contract::{
    MAX_SETTINGS_DEVICE_PAGE, NativeSettingsAction, SettingRegistry, SettingsActionRequest,
    SettingsDeviceAction, SettingsDevicePage, SettingsDeviceRequest, SettingsOperationResult,
    SettingsPatch, SettingsScope, SettingsSnapshot,
};
use super::service::NativeDeviceActionResult;
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
    Describe {
        proof: NativeSessionProof,
    },
    Get {
        proof: NativeSessionProof,
        scope: SettingsScope,
    },
    Update {
        proof: NativeSessionProof,
        patch: SettingsPatch,
    },
    ConfirmPatch {
        proof: NativeSessionProof,
        invocation: NativeActionInvocation,
    },
    Changes {
        proof: NativeSessionProof,
        after_sequence: Option<u64>,
        limit: usize,
    },
    Reconcile {
        proof: NativeSessionProof,
        operation_id: CommandId,
    },
    RequestAction {
        proof: NativeSessionProof,
        request: SettingsActionRequest,
    },
    Devices {
        proof: NativeSessionProof,
        scope: SettingsScope,
        after: Option<DeviceId>,
        limit: usize,
    },
    PairDevice {
        proof: NativeSessionProof,
        request: SettingsDeviceRequest,
    },
    RevokeDevice {
        proof: NativeSessionProof,
        request: SettingsDeviceRequest,
    },
    ConfirmDevice {
        proof: NativeSessionProof,
        invocation: NativeActionInvocation,
    },
    Pairing {
        proof: NativeSessionProof,
        operation_id: CommandId,
    },
    CancelPairing {
        proof: NativeSessionProof,
        operation_id: CommandId,
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
    SubmitCredential {
        proof: NativeSessionProof,
        invocation: NativeActionInvocation,
        secret: crate::host::credentials::SecretTransfer,
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
        match self {
            Self::Changes {
                after_sequence,
                limit,
                ..
            } if *limit == 0
                || *limit > 128
                || after_sequence.is_some_and(|value| value > i64::MAX as u64) =>
            {
                return Err("invalid native settings change page");
            }
            Self::Update { patch, .. } => {
                patch
                    .validate()
                    .map_err(|_| "invalid native settings patch")?;
            }
            Self::PairDevice { request, .. }
                if !matches!(&request.action, SettingsDeviceAction::Pair) =>
            {
                return Err("invalid native pairing action");
            }
            Self::RevokeDevice { request, .. }
                if !matches!(&request.action, SettingsDeviceAction::Revoke { .. }) =>
            {
                return Err("invalid native revocation action");
            }
            Self::Devices { limit, .. } if *limit == 0 || *limit > MAX_SETTINGS_DEVICE_PAGE => {
                return Err("invalid native device page limit");
            }
            Self::PairDevice { request, .. } | Self::RevokeDevice { request, .. } => {
                request
                    .validate()
                    .map_err(|_| "invalid native device request")?;
            }
            _ => {}
        }
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
            Self::Inspect { .. }
                | Self::Describe { .. }
                | Self::Get { .. }
                | Self::Changes { .. }
                | Self::Reconcile { .. }
                | Self::Finalize { .. }
                | Self::Cancel { .. }
                | Self::Close { .. }
                | Self::Pairing { .. }
                | Self::CancelPairing { .. }
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeSettingsResult {
    Described {
        registry: SettingRegistry,
    },
    Snapshot {
        snapshot: SettingsSnapshot,
    },
    Changes {
        page: super::service::SettingsChangePage,
    },
    Devices {
        page: SettingsDevicePage,
    },
    DeviceConfirmed {
        result: NativeDeviceActionResult,
    },
    Pairing {
        creation: NativePairingCreation,
    },
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
            (NativeSettingsOperation::Describe { .. }, Self::Described { registry }) => {
                validate_registry(registry)
            }
            (NativeSettingsOperation::Get { scope, .. }, Self::Snapshot { snapshot })
                if &snapshot.scope == scope =>
            {
                snapshot
                    .validate()
                    .map_err(|_| "invalid native settings snapshot")
            }
            (NativeSettingsOperation::Update { patch, .. }, Self::Inspected { operation })
                if operation.binding.request_id == patch.request_id
                    && operation.binding.scope == patch.scope
                    && operation.binding.expected_revision == Some(patch.expected_revision) =>
            {
                validate_inspection(operation)
            }
            (
                NativeSettingsOperation::Changes {
                    after_sequence,
                    limit,
                    ..
                },
                Self::Changes { page },
            ) => validate_changes(page, *after_sequence, *limit),
            (
                NativeSettingsOperation::Devices {
                    scope: SettingsScope::Host { host_id },
                    limit,
                    ..
                },
                Self::Devices { page },
            ) if page.host_id == *host_id && page.devices.len() <= *limit => {
                page.validate().map_err(|_| "invalid native device page")
            }
            (
                NativeSettingsOperation::PairDevice { request, .. }
                | NativeSettingsOperation::RevokeDevice { request, .. },
                Self::Prepared { result },
            ) if result.request_id == request.request_id && result.scope == request.scope => result
                .validate()
                .map_err(|_| "invalid native device result"),
            (
                NativeSettingsOperation::ConfirmDevice { invocation, .. },
                Self::DeviceConfirmed { result },
            ) => match result {
                NativeDeviceActionResult::Pairing { creation } => {
                    validate_pairing(creation, invocation.operation_id)
                }
                NativeDeviceActionResult::Existing { operation }
                | NativeDeviceActionResult::Revoked { operation }
                    if operation.binding.request_id == invocation.operation_id =>
                {
                    validate_inspection(operation)
                }
                _ => Err("native device result does not match its invocation"),
            },
            (
                NativeSettingsOperation::Pairing { operation_id, .. }
                | NativeSettingsOperation::CancelPairing { operation_id, .. },
                Self::Pairing { creation },
            ) => validate_pairing(creation, *operation_id),
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
                | NativeSettingsOperation::ConfirmPatch { invocation, .. }
                | NativeSettingsOperation::SubmitCredential { invocation, .. }
                | NativeSettingsOperation::Finalize { invocation, .. },
                Self::Inspected { operation },
            ) if operation.binding.request_id == invocation.operation_id => {
                validate_inspection(operation)
            }
            (
                NativeSettingsOperation::Inspect { operation_id, .. }
                | NativeSettingsOperation::Reconcile { operation_id, .. }
                | NativeSettingsOperation::Cancel { operation_id, .. },
                Self::Inspected { operation },
            ) if operation.binding.request_id == *operation_id => validate_inspection(operation),
            _ => Err("native settings response does not match its request"),
        }
    }
}

fn validate_inspection(operation: &SettingsOperationInspection) -> Result<(), &'static str> {
    operation
        .validate()
        .map_err(|_| "invalid settings operation inspection")
}

fn validate_registry(registry: &SettingRegistry) -> Result<(), &'static str> {
    let expected = SettingRegistry::from_profile(None);
    if registry.contract_version != expected.contract_version
        || registry.descriptors.len() != expected.descriptors.len()
    {
        return Err("invalid native settings descriptions");
    }
    for (actual, mut expected) in registry.descriptors.iter().zip(expected.descriptors) {
        if let super::contract::SettingConstraints::OptionalDimensions { choices } =
            &actual.constraints
        {
            if !matches!(
                expected.constraints,
                super::contract::SettingConstraints::OptionalDimensions { .. }
            ) {
                return Err("invalid native settings constraint type");
            }
            if choices.as_ref().is_some_and(|choices| {
                choices.is_empty()
                    || choices.len() > 256
                    || choices.iter().any(|value| *value == 0 || *value > 65_536)
                    || !choices.windows(2).all(|pair| pair[0] < pair[1])
            }) {
                return Err("invalid native embedding dimension choices");
            }
            expected.constraints = actual.constraints.clone();
        }
        if actual != &expected {
            return Err("invalid native settings descriptor");
        }
    }
    Ok(())
}

fn validate_changes(
    page: &super::service::SettingsChangePage,
    after_sequence: Option<u64>,
    limit: usize,
) -> Result<(), &'static str> {
    if page.contract_version != super::contract::SETTINGS_CONTRACT_VERSION
        || page.changes.len() > limit
        || limit == 0
        || limit > 128
        || page.changes.iter().any(|change| {
            change.sequence <= after_sequence.unwrap_or(0)
                || change.sequence > i64::MAX as u64
                || change.changed_keys.len() > super::contract::SettingKey::ALL.len()
                || change
                    .changed_keys
                    .iter()
                    .any(|key| key.scope() != change.scope.kind())
                || !change.changed_keys.windows(2).all(|pair| pair[0] < pair[1])
        })
        || !page
            .changes
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
        || page.next_sequence.is_some_and(|sequence| {
            sequence <= after_sequence.unwrap_or(0)
                || sequence > i64::MAX as u64
                || page
                    .changes
                    .last()
                    .is_some_and(|change| change.sequence > sequence)
        })
        || !page.changes.is_empty() && page.next_sequence.is_none()
        || serde_json::to_vec(page)
            .map_err(|_| "invalid native settings change encoding")?
            .len()
            > 32 * 1024
    {
        return Err("invalid native settings change page");
    }
    Ok(())
}

fn validate_pairing(
    creation: &NativePairingCreation,
    operation_id: CommandId,
) -> Result<(), &'static str> {
    validate_inspection(&creation.operation)?;
    if creation.operation.binding.request_id != operation_id {
        return Err("pairing result does not match its operation");
    }
    if let Some(invitation) = &creation.invitation
        && (invitation.operation_id != operation_id
            || invitation.expires_at.get() > creation.operation.binding.expires_at.get()
            || invitation.fingerprint.len() > 128
            || invitation.fingerprint.is_empty())
    {
        return Err("invalid native pairing invitation metadata");
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
            NativeSettingsOperation::Describe { proof } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Described {
                    registry: service.describe(caller).await?,
                })
            }
            NativeSettingsOperation::Get { proof, scope } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Snapshot {
                    snapshot: service.get(caller, scope).await?,
                })
            }
            NativeSettingsOperation::Update { proof, patch } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service
                        .update(caller, patch, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::ConfirmPatch { proof, invocation } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service
                        .confirm_patch(caller, invocation, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::Changes {
                proof,
                after_sequence,
                limit,
            } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Changes {
                    page: service.changes(caller, after_sequence, limit).await?,
                })
            }
            NativeSettingsOperation::Reconcile {
                proof,
                operation_id,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service
                        .reconcile_write(caller, operation_id, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::Devices {
                proof,
                scope,
                after,
                limit,
            } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Devices {
                    page: service.devices(caller, scope, after, limit).await?,
                })
            }
            NativeSettingsOperation::PairDevice { proof, request } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Prepared {
                    result: service
                        .pair_device(caller, request, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::RevokeDevice { proof, request } => {
                let caller = service.native_caller(call, &proof)?;
                Ok(NativeSettingsResult::Prepared {
                    result: service
                        .revoke_device(caller, request, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::ConfirmDevice { proof, invocation } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::DeviceConfirmed {
                    result: service
                        .confirm_device_action(caller, invocation, call.cancellation.clone())
                        .await?,
                })
            }
            NativeSettingsOperation::Pairing {
                proof,
                operation_id,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Pairing {
                    creation: service.native_pairing(caller, operation_id).await?,
                })
            }
            NativeSettingsOperation::CancelPairing {
                proof,
                operation_id,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Pairing {
                    creation: service.cancel_native_pairing(caller, operation_id).await?,
                })
            }
            NativeSettingsOperation::Open => Ok(NativeSettingsResult::Opened {
                session: service.open_native_session(call)?,
            }),
            NativeSettingsOperation::SubmitCredential {
                proof,
                invocation,
                secret,
            } => {
                let caller = service.native_cleanup_caller(call, &proof)?;
                Ok(NativeSettingsResult::Inspected {
                    operation: service
                        .submit_native_credential(
                            caller,
                            invocation,
                            secret,
                            self.credentials.clone(),
                            call.cancellation.clone(),
                        )
                        .await?,
                })
            }
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
