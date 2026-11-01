use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::credentials::CredentialError;
use crate::host::credentials::{CredentialVault, SecretTransfer};
use crate::host::selection::SelectionSettings;
use crate::protocol::UnixMillis;
use crate::selector::system_one::{SelectionConfig, SystemOneProvider};

use super::{ServiceCore, SettingsService, SettingsServiceError};
use crate::host::settings::actions::{
    ActionClaim, NativeActionAdmissionResult, NativeActionInvocation, NativeHostEffectAdmission,
    SettingsActionTarget, validate_current,
};
use crate::host::settings::authority::{CallerSource, SettingsCaller, SettingsOperationOrigin};
use crate::host::settings::contract::{
    NativeSettingsAction, SettingsActionRequest, SettingsCredentialBinding, SettingsFailureReason,
    SettingsOperationState, SettingsUnavailableReason,
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
            NativeActionAdmissionResult::Existing(operation) => return Ok(*operation),
            NativeActionAdmissionResult::Admitted(admission) => admission,
        };
        let claim = Arc::clone(&admission.claim);
        let effect = match self.start_host_effect(admission, cancellation).await {
            Ok(effect) => effect,
            Err(cause) => {
                return self
                    .blocking(move |core| {
                        let outcome = core.settle_unwritten_credential(&claim, cause);
                        core.finish_credential_attempt(&claim, outcome)
                    })
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
        let settings = self.settings.observed_snapshot()?;
        let matches = match binding {
            SettingsCredentialBinding::Model { binding } => {
                settings.model_credentials.contains(&binding.as_native())
            }
            SettingsCredentialBinding::SystemOne {
                endpoint,
                reference,
            } => {
                let requested = SelectionConfig::system_one(
                    SystemOneProvider::Typesafe,
                    endpoint,
                    "credential-binding",
                    Some(*reference),
                )
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
        if matches {
            return Ok(binding.clone());
        }
        if !matches!(
            identity.origin,
            SettingsOperationOrigin::NativeUi { session_id }
                if !session_id.is_nil() && session_id == identity.binding.client_session_id
        ) {
            return Err(SettingsServiceError::Conflict);
        }
        let reference = match binding {
            SettingsCredentialBinding::Model { binding } => binding.reference,
            SettingsCredentialBinding::SystemOne { reference, .. } => *reference,
        };
        if reference.is_nil()
            || settings
                .model_credentials
                .iter()
                .any(|configured| configured.reference == reference)
            || settings
                .mcp
                .servers
                .iter()
                .flat_map(|server| server.environment.values())
                .any(|configured| configured.reference == reference)
            || matches!(
                &settings.selection,
                SelectionSettings::SystemOne {
                    credential_reference: Some(configured),
                    ..
                } if *configured == reference
            )
        {
            return Err(SettingsServiceError::Conflict);
        }
        let matches_initial = match binding {
            SettingsCredentialBinding::Model { binding } => settings
                .default_conversation
                .as_ref()
                .is_some_and(|selected| {
                    selected.provider == binding.provider
                        && selected.endpoint == binding.endpoint.as_native()
                        && settings.credential_for(selected).is_none()
                }),
            SettingsCredentialBinding::SystemOne { endpoint, .. } => {
                match (&settings.selection, settings.selection.configuration()) {
                    (
                        SelectionSettings::SystemOne {
                            credential_reference: None,
                            ..
                        },
                        Ok(SelectionConfig::SystemOne(current)),
                    ) => current.endpoint() == endpoint,
                    _ => false,
                }
            }
        };
        if !matches_initial {
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
        let admitted = (|| {
            self.require_resolved_file()?;
            let binding = self.credential_binding(&effect.claim.identity)?;
            let state = effect
                .claim
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            validate_current(&self.authority, &self.settings, &effect.claim.identity)?;
            if !state.host_effect_started {
                return Err(SettingsServiceError::OutsideScope);
            }
            if effect.claim.cancellation.is_cancelled() {
                return Err(SettingsServiceError::Cancelled);
            }
            if state.closed
                || Instant::now() >= effect.claim.deadline
                || UnixMillis::now()?.get() >= effect.claim.identity.binding.expires_at.get()
            {
                return Err(SettingsServiceError::Expired);
            }
            Ok(binding)
        })();
        let outcome = match admitted {
            Err(cause) => self.settle_unwritten_credential(&effect.claim, cause),
            Ok(binding) => match vault.store_settings_credential(&binding, secret) {
                Err(
                    cause @ (CredentialError::Busy
                    | CredentialError::InvalidKey
                    | CredentialError::InvalidSecret
                    | CredentialError::Unsupported),
                ) => self.settle_unwritten_credential(&effect.claim, cause.into()),
                Err(cause) => Err(cause.into()),
                Ok(()) => self.repository.finish(
                    &effect.claim.identity,
                    effect.claim.native.native_request_id,
                    self.result(
                        &effect.claim.identity,
                        SettingsOperationState::Completed {
                            receipt_id: Uuid::new_v4(),
                        },
                    ),
                    None,
                ),
            },
        };
        self.finish_credential_attempt(&effect.claim, outcome)
    }

    fn settle_unwritten_credential(
        &self,
        claim: &ActionClaim,
        cause: SettingsServiceError,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let state = match &cause {
            SettingsServiceError::Credential(CredentialError::Unsupported) => {
                SettingsOperationState::Unavailable {
                    reason: SettingsUnavailableReason::NativeActionUnavailable,
                }
            }
            SettingsServiceError::Credential(
                CredentialError::Busy
                | CredentialError::InvalidKey
                | CredentialError::InvalidSecret,
            ) => SettingsOperationState::Failed {
                reason: SettingsFailureReason::Storage,
            },
            SettingsServiceError::Credential(_) => return Err(cause),
            SettingsServiceError::Cancelled => SettingsOperationState::Cancelled,
            _ if claim.cancellation.is_cancelled() => SettingsOperationState::Cancelled,
            SettingsServiceError::Expired => SettingsOperationState::Unavailable {
                reason: SettingsUnavailableReason::Expired,
            },
            SettingsServiceError::Conflict => match (
                claim.identity.binding.expected_revision,
                self.settings.snapshot(),
            ) {
                (Some(expected), Ok(snapshot)) if snapshot.revision != expected => {
                    SettingsOperationState::Conflict {
                        observed_revision: snapshot.revision,
                    }
                }
                _ => return Err(cause),
            },
            _ => return Err(cause),
        };
        self.repository.finish(
            &claim.identity,
            claim.native.native_request_id,
            self.result(&claim.identity, state),
            None,
        )
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
