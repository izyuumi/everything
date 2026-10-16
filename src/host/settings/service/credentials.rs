use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::host::credentials::{CredentialVault, SecretTransfer};
use crate::host::selection::SelectionSettings;
use crate::protocol::UnixMillis;
use crate::selector::system_one::SelectionConfig;

use super::{ServiceCore, SettingsService, SettingsServiceError};
use crate::host::settings::actions::{
    ActionClaim, NativeActionAdmissionResult, NativeActionInvocation, NativeHostEffectAdmission,
    SettingsActionTarget, validate_current,
};
use crate::host::settings::authority::{CallerSource, SettingsCaller};
use crate::host::settings::contract::{
    NativeSettingsAction, SettingsActionRequest, SettingsCredentialBinding, SettingsOperationState,
};
use crate::host::settings::operations::{
    NativeRequest, OperationIdentity, SettingsOperationInspection,
};

impl SettingsService {
    pub(crate) async fn submit_native_credential(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
        secret: SecretTransfer,
        vault: CredentialVault,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let service = self.clone();
        tokio::spawn(async move {
            service
                .submit_native_credential_owned(caller, invocation, secret, vault, cancellation)
                .await
        })
        .await
        .map_err(|_| SettingsServiceError::Worker)?
    }

    async fn submit_native_credential_owned(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
        secret: SecretTransfer,
        vault: CredentialVault,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
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
                    if !matches!(
                        record.identity.target,
                        SettingsActionTarget::Native(SettingsActionRequest {
                            action: NativeSettingsAction::CredentialEntry { .. },
                            ..
                        })
                    ) || record.identity.binding.client_session_id != session_id
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
                    if replay.is_none() {
                        core.require_resolved_file()?;
                        core.check_current(&record.identity)?;
                        core.credential_binding(&record.identity)?;
                    }
                    Ok((caller, replay))
                }
            })
            .await?;
        if let Some(operation) = replay {
            return Ok(operation);
        }
        self.core.authority.validate(&caller, false)?;
        let admission = match self
            .admit_native_action(caller, invocation, cancellation.clone())
            .await?
        {
            NativeActionAdmissionResult::Existing(operation) => return Ok(operation),
            NativeActionAdmissionResult::Admitted(admission) => admission,
        };
        let claim = Arc::clone(&admission.claim);
        let effect = match self.start_host_effect(admission, cancellation).await {
            Ok(effect) => effect,
            Err(cause) => {
                return self
                    .blocking(move |core| core.finish_credential_attempt(&claim, Err(cause)))
                    .await;
            }
        };
        self.blocking(move |core| {
            let retained = (vault, secret, effect);
            core.store_native_credential(&retained.2, &retained.0, retained.1)
        })
        .await
    }
}

impl ServiceCore {
    fn credential_binding(
        &self,
        identity: &OperationIdentity,
    ) -> Result<SettingsCredentialBinding, SettingsServiceError> {
        let SettingsActionTarget::Native(SettingsActionRequest {
            action: NativeSettingsAction::CredentialEntry { binding },
            ..
        }) = &identity.target
        else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let settings = self.settings.snapshot()?;
        let matches = match binding {
            SettingsCredentialBinding::Model { binding } => {
                settings.model_credentials.contains(&binding.as_native())
            }
            SettingsCredentialBinding::SystemOne {
                endpoint,
                reference,
            } => {
                let requested =
                    SelectionConfig::system_one(endpoint, "credential-binding", Some(*reference))
                        .map_err(|_| SettingsServiceError::Conflict)?;
                match (
                    &settings.selection,
                    settings.selection.configuration(),
                    requested,
                ) {
                    (
                        SelectionSettings::SystemOne {
                            credential_reference,
                            ..
                        },
                        Ok(SelectionConfig::SystemOne(current)),
                        SelectionConfig::SystemOne(requested),
                    ) => {
                        *credential_reference == Some(*reference)
                            && current.endpoint() == requested.endpoint()
                    }
                    _ => false,
                }
            }
        };
        if !matches {
            return Err(SettingsServiceError::Conflict);
        }
        Ok(binding.clone())
    }

    fn store_native_credential(
        &self,
        effect: &NativeHostEffectAdmission,
        vault: &CredentialVault,
        secret: SecretTransfer,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let outcome = (|| {
            self.require_resolved_file()?;
            let binding = self.credential_binding(&effect.claim.identity)?;
            {
                let state = effect
                    .claim
                    .state
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?;
                validate_current(&self.authority, &self.settings, &effect.claim.identity)?;
                if state.closed
                    || !state.host_effect_started
                    || effect.claim.cancellation.is_cancelled()
                    || Instant::now() >= effect.claim.deadline
                    || UnixMillis::now()?.get() >= effect.claim.identity.binding.expires_at.get()
                {
                    return Err(SettingsServiceError::Expired);
                }
            }
            vault.store_settings_credential(&binding, secret)?;
            self.repository.finish(
                &effect.claim.identity,
                effect.claim.native.native_request_id,
                self.result(
                    &effect.claim.identity,
                    SettingsOperationState::Completed {
                        receipt_id: Uuid::new_v4(),
                    },
                ),
                None,
            )
        })();
        self.finish_credential_attempt(&effect.claim, outcome)
    }

    fn finish_credential_attempt(
        &self,
        claim: &Arc<ActionClaim>,
        outcome: Result<SettingsOperationInspection, SettingsServiceError>,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let outcome = outcome.map_err(|error| self.failed_claim_bookkeeping(claim, error));
        let cleanup = (|| {
            claim.close()?;
            claim
                .session
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .take();
            let mut claims = self
                .claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if claims
                .get(&claim.identity.binding.request_id)
                .is_some_and(|current| Arc::ptr_eq(current, claim))
            {
                claims.remove(&claim.identity.binding.request_id);
            }
            Ok(())
        })();
        match (outcome, cleanup) {
            (Ok(operation), Ok(())) => Ok(operation),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(self.failed_claim_bookkeeping(claim, error)),
            (Err(cause), Err(repair)) => Err(SettingsServiceError::OutcomeUnknown {
                request_id: claim.identity.binding.request_id,
                cause: Box::new(cause),
                repair_error: Some(Box::new(repair)),
            }),
        }
    }
}
