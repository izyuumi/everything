use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::host::settings::actions::{
    NativeActionAdmissionResult, NativeActionInvocation, NativeHostEffectAdmission,
    NativePairingCreation, SettingsActionTarget, validate_current,
};
use crate::host::settings::authority::{CallerSource, SettingsCaller};
use crate::host::settings::contract::{
    MAX_SETTINGS_DEVICE_PAGE, SETTINGS_CONTRACT_VERSION, SettingsDeviceAction, SettingsDevicePage,
    SettingsDeviceRequest, SettingsDeviceSummary, SettingsOperationResult, SettingsOperationState,
    SettingsScope, SettingsToolName,
};
use crate::host::settings::operations::{NativeRequest, SettingsOperationInspection};
use crate::protocol::{DeviceId, UnixMillis};

use super::{ServiceCore, SettingsService, SettingsServiceError};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeDeviceActionResult {
    Existing {
        operation: SettingsOperationInspection,
    },
    Pairing {
        creation: NativePairingCreation,
    },
    Revoked {
        operation: SettingsOperationInspection,
    },
}

impl SettingsService {
    pub async fn devices(
        &self,
        caller: SettingsCaller,
        scope: SettingsScope,
        after: Option<DeviceId>,
        limit: usize,
    ) -> Result<SettingsDevicePage, SettingsServiceError> {
        let _work = self.invocation_guard(&caller, SettingsToolName::DevicesList, None)?;
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::DevicesList)?;
        if !matches!(scope, SettingsScope::Host { .. }) {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.core.check_scope(&scope)?;
        if limit == 0 || limit > MAX_SETTINGS_DEVICE_PAGE {
            return Err(SettingsServiceError::Invalid("device page limit"));
        }
        self.blocking(move |core| {
            core.authority
                .require_tool(&caller, SettingsToolName::DevicesList)?;
            core.check_scope(&scope)?;
            let page = core
                .endpoint
                .settings_devices(after, limit, UnixMillis::now()?)?;
            core.authority
                .require_tool(&caller, SettingsToolName::DevicesList)?;
            let page = SettingsDevicePage {
                contract_version: SETTINGS_CONTRACT_VERSION,
                host_id: page.host_id,
                observed_at: page.observed_at,
                devices: page
                    .devices
                    .into_iter()
                    .map(|device| SettingsDeviceSummary {
                        device_id: device.device_id,
                        label: device.label,
                        public_keys: device.public_keys,
                        revoked: device.revoked,
                        this_device: device.this_device,
                    })
                    .collect(),
                next_after: page.next_after,
            };
            page.validate()?;
            Ok(page)
        })
        .await
    }

    pub async fn pair_device(
        &self,
        caller: SettingsCaller,
        request: SettingsDeviceRequest,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        let _work = self.invocation_guard(
            &caller,
            SettingsToolName::DevicesPair,
            Some(request.request_id),
        )?;
        if !matches!(request.action, SettingsDeviceAction::Pair) {
            self.reject_invocation()?;
            return Err(SettingsServiceError::Invalid("pairing action"));
        }
        self.prepare_device_action(caller, request, cancellation)
            .await
    }

    pub async fn revoke_device(
        &self,
        caller: SettingsCaller,
        request: SettingsDeviceRequest,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        let _work = self.invocation_guard(
            &caller,
            SettingsToolName::DevicesRevoke,
            Some(request.request_id),
        )?;
        if !matches!(request.action, SettingsDeviceAction::Revoke { .. }) {
            self.reject_invocation()?;
            return Err(SettingsServiceError::Invalid("revocation action"));
        }
        self.prepare_device_action(caller, request, cancellation)
            .await
    }

    pub(crate) async fn confirm_device_action(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
        cancellation: CancellationToken,
    ) -> Result<NativeDeviceActionResult, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        let CallerSource::NativeUi(session_id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let (caller, replay) = self
            .blocking({
                let invocation = invocation.clone();
                move |core| {
                    core.authority.validate(&caller, true)?;
                    let record = core.repository.get(invocation.operation_id)?;
                    core.check_principal(&caller, &record.identity)?;
                    if !matches!(record.identity.target, SettingsActionTarget::Device(_))
                        || record.identity.binding.client_session_id != session_id
                    {
                        return Err(SettingsServiceError::OutsideScope);
                    }
                    let replay = core.repository.replay_claim(
                        &record.identity,
                        &NativeRequest {
                            operation_id: invocation.operation_id,
                            native_request_id: invocation.native_request_id,
                            session_id,
                            reference: invocation.reference,
                        },
                    )?;
                    Ok((caller, replay))
                }
            })
            .await?;
        if let Some(operation) = replay {
            return Ok(NativeDeviceActionResult::Existing { operation });
        }
        self.core.authority.validate(&caller, false)?;
        let admission = match self
            .admit_native_action(caller, invocation, cancellation.clone())
            .await?
        {
            NativeActionAdmissionResult::Existing(operation) => {
                return Ok(NativeDeviceActionResult::Existing {
                    operation: *operation,
                });
            }
            NativeActionAdmissionResult::Admitted(admission) => admission,
        };
        let effect = self.start_host_effect(admission, cancellation).await?;
        match effect.target() {
            SettingsActionTarget::Device(SettingsDeviceRequest {
                action: SettingsDeviceAction::Pair,
                ..
            }) => Ok(NativeDeviceActionResult::Pairing {
                creation: self.create_pairing(effect).await?,
            }),
            SettingsActionTarget::Device(SettingsDeviceRequest {
                action: SettingsDeviceAction::Revoke { .. },
                ..
            }) => {
                self.blocking(move |core| {
                    let retained_effect = effect;
                    core.revoke_device(&retained_effect)
                        .map(|operation| NativeDeviceActionResult::Revoked { operation })
                })
                .await
            }
            _ => Err(SettingsServiceError::OutsideScope),
        }
    }
}

impl ServiceCore {
    fn revoke_device(
        &self,
        effect: &NativeHostEffectAdmission,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let outcome = (|| {
            let SettingsActionTarget::Device(SettingsDeviceRequest {
                action:
                    SettingsDeviceAction::Revoke {
                        device_id,
                        public_keys,
                    },
                ..
            }) = effect.target()
            else {
                return Err(SettingsServiceError::OutsideScope);
            };
            self.check_endpoint(self.endpoint.public_keys())?;
            let now = {
                let state = effect
                    .claim
                    .state
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?;
                validate_current(&self.authority, &self.settings, &effect.claim.identity)?;
                let now = UnixMillis::now()?;
                if state.closed
                    || !state.host_effect_started
                    || effect.claim.cancellation.is_cancelled()
                    || Instant::now() >= effect.claim.deadline
                    || now.get() >= effect.claim.identity.binding.expires_at.get()
                {
                    return Err(SettingsServiceError::Expired);
                }
                if *device_id != public_keys.device_id {
                    return Err(SettingsServiceError::Conflict);
                }
                now
            };
            self.endpoint.revoke_device_exact(public_keys, now)?;
            let inspection = self.repository.finish(
                &effect.claim.identity,
                effect.claim.native.native_request_id,
                self.result(
                    &effect.claim.identity,
                    SettingsOperationState::Completed {
                        receipt_id: Uuid::new_v4(),
                    },
                ),
                None,
            )?;
            effect.claim.close()?;
            effect
                .claim
                .session
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .take();
            let mut claims = self
                .claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if claims
                .get(&effect.operation_id())
                .is_some_and(|claim| Arc::ptr_eq(claim, &effect.claim))
            {
                claims.remove(&effect.operation_id());
            }
            Ok(inspection)
        })();
        outcome.map_err(|error| self.failed_claim_bookkeeping(&effect.claim, error))
    }
}
