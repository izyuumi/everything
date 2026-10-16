use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::crypto::{PairingAcceptance, PairingRequest};
use crate::devices::PairingApproval;
use crate::protocol::{CommandId, ConversationId, DeviceId, UnixMillis};

use super::super::actions::{
    ActionClaim, NativeActionAdmissionResult, NativeActionInvocation, NativeActionReference,
    NativeHostEffectAdmission, NativePairingLifecycle, PairingApprovalIdentity,
    PendingPairingApproval, PendingPairingLease, SettingsActionTarget,
};
use super::super::authority::{CallerSource, SettingsCaller, SettingsOperationOrigin};
use super::super::contract::{
    SettingsFailureReason, SettingsOperationState, SettingsScope, SettingsUnavailableReason,
};
use super::super::operations::{
    OperationRecord, SettingsOperationInspection, SettingsOperationPhase, request_digest,
};
use super::{ServiceCore, SettingsService, SettingsServiceError};

#[derive(Clone)]
pub struct NativePairingRequest(Arc<PrivatePairingRequest>);

struct PrivatePairingRequest(PairingRequest);

impl Drop for PrivatePairingRequest {
    fn drop(&mut self) {
        self.0.proof.zeroize();
        self.0.signature.zeroize();
    }
}

impl fmt::Debug for NativePairingRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativePairingRequest([redacted])")
    }
}

impl Serialize for NativePairingRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NativePairingRequest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let request = PairingRequest::deserialize(deserializer)?;
        Ok(Self(Arc::new(PrivatePairingRequest(request))))
    }
}

#[derive(Clone)]
pub struct NativePairingAcceptance(Arc<PrivatePairingAcceptance>);

struct PrivatePairingAcceptance(PairingAcceptance);

impl Drop for PrivatePairingAcceptance {
    fn drop(&mut self) {
        self.0.signature.zeroize();
    }
}

impl fmt::Debug for NativePairingAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativePairingAcceptance([redacted])")
    }
}

impl Serialize for NativePairingAcceptance {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for NativePairingAcceptance {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let acceptance = PairingAcceptance::deserialize(deserializer)?;
        Ok(Self(Arc::new(PrivatePairingAcceptance(acceptance))))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePairingAccess {
    pub conversations: NativePairingConversationAccess,
    pub submit_commands: bool,
    pub resolve_permissions: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "scope",
    content = "ids",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum NativePairingConversationAccess {
    All,
    Selected(BTreeSet<ConversationId>),
}

impl NativePairingAccess {
    fn as_device_access(&self) -> crate::devices::DeviceAccess {
        crate::devices::DeviceAccess {
            conversations: match &self.conversations {
                NativePairingConversationAccess::All => crate::devices::ConversationAccess::All,
                NativePairingConversationAccess::Selected(ids) => {
                    crate::devices::ConversationAccess::Selected(ids.clone())
                }
            },
            submit_commands: self.submit_commands,
            resolve_permissions: self.resolve_permissions,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePairingApprovalInput {
    pub operation_id: CommandId,
    pub native_request_id: CommandId,
    pub creation_operation_id: CommandId,
    pub request: NativePairingRequest,
    pub label: String,
    pub expected_fingerprint: String,
    pub access: NativePairingAccess,
}

impl NativePairingApprovalInput {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.native_request_id.as_uuid().is_nil()
            || self.native_request_id == self.operation_id
            || self.native_request_id == self.creation_operation_id
        {
            return Err("invalid native approval identity");
        }
        let request = &self.request.0.0;
        if request.version != crate::crypto::ENVELOPE_VERSION
            || Zeroizing::new(serde_json::to_vec(request).map_err(|_| "invalid pairing request")?)
                .len()
                > 4096
        {
            return Err("invalid native pairing request");
        }
        self.identity(request.inviter)
            .map_err(|_| "invalid native pairing approval")?;
        Ok(())
    }

    fn identity(&self, host_id: DeviceId) -> Result<PairingApprovalIdentity, SettingsServiceError> {
        let request = &self.request.0.0;
        if request.inviter != host_id {
            return Err(SettingsServiceError::OutsideScope);
        }
        let identity = PairingApprovalIdentity {
            request_id: self.operation_id,
            native_request_id: self.native_request_id,
            scope: SettingsScope::Host { host_id },
            creation_operation_id: self.creation_operation_id,
            pairing_id: request.pairing_id,
            responder: request.responder.clone(),
            request_sha256: request_digest(request)?,
            label: self.label.clone(),
            expected_fingerprint: self.expected_fingerprint.clone(),
            access: self.access.as_device_access(),
            invitation_expires_at: UnixMillis::try_from(request.expires_at_ms)?,
        };
        identity.validate()?;
        Ok(identity)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativePairingPinState {
    NotEstablished,
    Committed {
        label: String,
        pinned_at: UnixMillis,
        paired_at: UnixMillis,
        revoked_at: Option<UnixMillis>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePairingApprovalResult {
    pub operation: SettingsOperationInspection,
    pub approval: PairingApprovalIdentity,
    pub host_pin: NativePairingPinState,
    pub acceptance: Option<NativePairingAcceptance>,
}

impl NativePairingApprovalResult {
    pub(crate) fn validate_for_input(
        &self,
        input: &NativePairingApprovalInput,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.approval
            != input
                .identity(input.request.0.0.inviter)
                .map_err(|_| "invalid approval input")?
        {
            return Err("native approval does not match its request");
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.operation
            .validate()
            .map_err(|_| "invalid pairing approval operation")?;
        self.approval
            .validate()
            .map_err(|_| "invalid pairing approval metadata")?;
        if self.operation.binding.request_id != self.approval.request_id
            || self.operation.binding.scope != self.approval.scope
            || self.operation.binding.expected_revision.is_some()
            || !matches!(
                self.operation.origin,
                SettingsOperationOrigin::NativeUi { .. }
            )
            || self.operation.binding.exact_request_sha256
                != request_digest(&SettingsActionTarget::PairingApproval(Box::new(
                    self.approval.clone(),
                )))
                .map_err(|_| "invalid pairing approval digest")?
        {
            return Err("pairing approval result binding mismatch");
        }
        if let NativePairingPinState::Committed {
            label,
            pinned_at,
            revoked_at,
            ..
        } = &self.host_pin
        {
            if label.is_empty()
                || label.len() > 256
                || label.chars().any(char::is_control)
                || revoked_at.is_some_and(|time| time.get() < pinned_at.get())
            {
                return Err("invalid pairing pin metadata");
            }
        }
        if let Some(acceptance) = &self.acceptance {
            let acceptance = &acceptance.0.0;
            if !matches!(
                self.operation.phase,
                SettingsOperationPhase::Finished {
                    result: super::super::contract::SettingsOperationResult {
                        state: SettingsOperationState::Completed { .. },
                        ..
                    }
                }
            ) || !matches!(
                self.host_pin,
                NativePairingPinState::Committed {
                    revoked_at: None,
                    ..
                }
            ) || acceptance.version != crate::crypto::ENVELOPE_VERSION
                || acceptance.pairing_id != self.approval.pairing_id
                || acceptance.responder != self.approval.responder
                || !matches!(self.approval.scope, SettingsScope::Host { host_id } if host_id == acceptance.inviter.device_id)
                || acceptance.expires_at_ms != self.approval.invitation_expires_at.get()
                || Zeroizing::new(
                    serde_json::to_vec(acceptance).map_err(|_| "invalid native acceptance")?,
                )
                .len()
                    > 4096
            {
                return Err("native acceptance does not match approval");
            }
            acceptance
                .inviter
                .validate()
                .map_err(|_| "invalid acceptance host")?;
        }
        Ok(())
    }
}

impl SettingsService {
    pub(crate) async fn approve_native_pairing(
        &self,
        caller: SettingsCaller,
        input: NativePairingApprovalInput,
        cancellation: CancellationToken,
    ) -> Result<NativePairingApprovalResult, SettingsServiceError> {
        input.validate().map_err(SettingsServiceError::Invalid)?;
        self.core.authority.validate(&caller, true)?;
        if !matches!(caller.source, CallerSource::NativeUi(_)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mutation = tokio::select! {
            biased;
            _=cancellation.cancelled()=>return Err(SettingsServiceError::Cancelled),
            mutation=Arc::clone(&self.core.mutation).lock_owned()=>mutation,
        };
        self.blocking(move |core| core.approve_pairing_owned(caller, input, cancellation, mutation))
            .await
    }

    pub(crate) async fn native_pairing_approval(
        &self,
        caller: SettingsCaller,
        operation_id: CommandId,
    ) -> Result<NativePairingApprovalResult, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        if !matches!(caller.source, CallerSource::NativeUi(_)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mutation = Arc::clone(&self.core.mutation).lock_owned().await;
        self.blocking(move |core| {
            let _mutation = mutation;
            core.authority.validate(&caller, true)?;
            let record = core.repository.get(operation_id)?;
            core.check_native_approval_reader(&caller, &record)?;
            core.pairing_approval_result(&caller, &record)
        })
        .await
    }
}

impl ServiceCore {
    fn check_native_approval_reader(
        &self,
        caller: &SettingsCaller,
        record: &OperationRecord,
    ) -> Result<(), SettingsServiceError> {
        self.authority.validate(caller, true)?;
        self.check_principal(caller, &record.identity)?;
        if !matches!(caller.source, CallerSource::NativeUi(id) if record.identity.origin == (SettingsOperationOrigin::NativeUi { session_id: id }))
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let SettingsActionTarget::PairingApproval(approval) = &record.identity.target else {
            return Err(SettingsServiceError::OutsideScope);
        };
        self.check_approval_creation(record, approval)
    }

    fn check_approval_creation(
        &self,
        record: &OperationRecord,
        approval: &PairingApprovalIdentity,
    ) -> Result<(), SettingsServiceError> {
        let creation = self.repository.get(approval.creation_operation_id)?;
        let metadata = creation
            .pairing()
            .ok_or(SettingsServiceError::CorruptJournal)?;
        if !matches!(
            creation.identity.target,
            SettingsActionTarget::Device(super::super::contract::SettingsDeviceRequest {
                action: super::super::contract::SettingsDeviceAction::Pair,
                ..
            })
        ) || creation.identity.binding.client_session_id
            != record.identity.binding.client_session_id
            || creation.identity.binding.principal_id != record.identity.binding.principal_id
            || creation.identity.binding.scope != record.identity.binding.scope
            || metadata.pairing_id != approval.pairing_id
            || metadata.expires_at != approval.invitation_expires_at
            || record.identity.binding.expires_at.get() > metadata.expires_at.get()
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        Ok(())
    }

    fn check_live_pairing(
        &self,
        caller: &SettingsCaller,
        pairing: &PendingPairingLease,
        approval: &PairingApprovalIdentity,
    ) -> Result<(), SettingsServiceError> {
        self.authority.validate(caller, false)?;
        let state = pairing
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if pairing.operation_id != approval.creation_operation_id
            || pairing.pairing_id != approval.pairing_id
            || pairing.expires_at != approval.invitation_expires_at
            || !matches!(caller.source, CallerSource::NativeUi(id) if id == pairing.session_id)
            || pairing._session.cancellation.is_cancelled()
            || Instant::now() >= pairing.deadline
            || UnixMillis::now()?.get() >= pairing.expires_at.get()
            || state.lifecycle != NativePairingLifecycle::Retained
            || state
                .approval
                .as_ref()
                .is_some_and(|retained| retained.identity != *approval || retained.delivery_closed)
        {
            return Err(SettingsServiceError::Expired);
        }
        Ok(())
    }

    fn approve_pairing_owned(
        &self,
        caller: SettingsCaller,
        input: NativePairingApprovalInput,
        cancellation: CancellationToken,
        mutation: OwnedMutexGuard<()>,
    ) -> Result<NativePairingApprovalResult, SettingsServiceError> {
        let retained_mutation = mutation;
        self.authority.validate(&caller, true)?;
        let approval = input.identity(self.authority.host_id)?;
        let target = SettingsActionTarget::PairingApproval(Box::new(approval.clone()));
        let existing = match self.repository.get(input.operation_id) {
            Ok(record) => {
                self.check_native_approval_reader(&caller, &record)?;
                if record.identity.target != target {
                    return Err(SettingsServiceError::Conflict);
                }
                if let Some(native) = self
                    .repository
                    .original_native_request(input.operation_id)?
                {
                    if native.native_request_id != input.native_request_id {
                        return Err(SettingsServiceError::Conflict);
                    }
                    if self.recover_pairing_approval(&record)? {
                        let record = self.repository.get(input.operation_id)?;
                        return self.pairing_approval_result(&caller, &record);
                    }
                    return self.pairing_approval_result(&caller, &record);
                }
                if !matches!(
                    record.phase(),
                    SettingsOperationPhase::AwaitingAction { .. }
                ) {
                    return self.pairing_approval_result(&caller, &record);
                }
                Some(record)
            }
            Err(SettingsServiceError::NotFound) => None,
            Err(error) => return Err(error),
        };
        self.authority.validate(&caller, false)?;
        if cancellation.is_cancelled() {
            return Err(SettingsServiceError::Cancelled);
        }
        self.check_endpoint(self.endpoint.public_keys())?;
        let creation = self.repository.get(input.creation_operation_id)?;
        self.check_pairing_caller(&caller, &creation.identity)?;
        let metadata = creation.pairing().ok_or(SettingsServiceError::NotFound)?;
        if metadata.pairing_id != approval.pairing_id
            || metadata.expires_at != approval.invitation_expires_at
        {
            return Err(SettingsServiceError::Conflict);
        }
        let pairing = self
            .pairing(input.creation_operation_id)?
            .ok_or(SettingsServiceError::NotFound)?;
        self.check_live_pairing(&caller, &pairing, &approval)?;
        if self
            .endpoint
            .pairing_approval_state(approval.pairing_id, &approval.responder, &approval.access)?
            .is_some()
        {
            return Err(SettingsServiceError::Conflict);
        }
        {
            let mut state = pairing
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if pairing._session.cancellation.is_cancelled()
                || Instant::now() >= pairing.deadline
                || UnixMillis::now()?.get() >= pairing.expires_at.get()
                || state.lifecycle != NativePairingLifecycle::Retained
            {
                return Err(SettingsServiceError::Expired);
            }
            if let Some(retained) = &state.approval {
                if retained.identity != approval || retained.delivery_closed {
                    return Err(SettingsServiceError::Conflict);
                }
            } else {
                state.approval = Some(PendingPairingApproval {
                    operation_id: input.operation_id,
                    native_request_id: input.native_request_id,
                    identity: approval.clone(),
                    delivery_closed: false,
                    acceptance: None,
                });
            }
        }
        let record = if let Some(record) = existing {
            record
        } else {
            let (mut identity, deadline) = self.operation_identity(&caller, target)?;
            identity.binding.expires_at = UnixMillis::try_from(
                identity
                    .binding
                    .expires_at
                    .get()
                    .min(pairing.expires_at.get()),
            )?;
            let deadline = deadline.min(pairing.deadline);
            identity.validate()?;
            let phase = self.pending_phase(&identity);
            let record = self.repository.prepare(identity.clone(), phase, || {
                self.authority.validate(&caller, false)?;
                if cancellation.is_cancelled()
                    || pairing._session.cancellation.is_cancelled()
                    || Instant::now() >= deadline
                {
                    return Err(SettingsServiceError::Expired);
                }
                Ok(())
            })?;
            self.pending
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .insert(input.operation_id, deadline);
            record
        };
        self.check_approval_creation(&record, &approval)?;
        let reference = match record.phase() {
            SettingsOperationPhase::AwaitingAction { reference } => {
                NativeActionReference::Action(reference.clone())
            }
            _ => return Err(SettingsServiceError::Conflict),
        };
        let invocation = NativeActionInvocation {
            operation_id: input.operation_id,
            native_request_id: input.native_request_id,
            reference,
        };
        let admission = match self.admit_native_action(&caller, invocation, &cancellation)? {
            NativeActionAdmissionResult::Existing(_) => {
                let record = self.repository.get(input.operation_id)?;
                return self.pairing_approval_result(&caller, &record);
            }
            NativeActionAdmissionResult::Admitted(admission) => admission,
        };
        let claim = Arc::clone(&admission.claim);
        {
            let mut state = claim
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if state.pairing.is_some() {
                drop(state);
                return Err(self.failed_claim_bookkeeping(&claim, SettingsServiceError::Conflict));
            }
            state.pairing = Some(Arc::clone(&pairing));
        }
        if let Err(error) = self.start_host_effect(&admission) {
            return Err(self.failed_claim_bookkeeping(&claim, error));
        }
        let retained_effect = NativeHostEffectAdmission {
            _mutation: retained_mutation,
            claim: admission.claim,
        };
        let outcome = (|| {
            self.check_live_pairing(&caller, &pairing, &approval)?;
            {
                let state = retained_effect
                    .claim
                    .state
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?;
                if state.closed
                    || !state.host_effect_started
                    || retained_effect.claim.cancellation.is_cancelled()
                    || cancellation.is_cancelled()
                    || Instant::now() >= retained_effect.claim.deadline
                    || UnixMillis::now()?.get()
                        >= retained_effect.claim.identity.binding.expires_at.get()
                {
                    return Err(SettingsServiceError::Expired);
                }
                self.authority
                    .check_origin(&retained_effect.claim.identity.origin)?;
            }
            let acceptance = self.endpoint.approve_pairing(
                &input.request.0.0,
                PairingApproval::new(
                    approval.label.clone(),
                    approval.expected_fingerprint.clone(),
                    approval.access.clone(),
                )?,
                UnixMillis::now()?,
            )?;
            self.cache_pairing_acceptance(&retained_effect, &pairing, &approval, acceptance)?;
            Ok(())
        })();
        self.settle_pairing_approval(&retained_effect, &pairing, &approval, outcome)?;
        let record = self.repository.get(input.operation_id)?;
        self.pairing_approval_result(&caller, &record)
    }

    fn cache_pairing_acceptance(
        &self,
        effect: &NativeHostEffectAdmission,
        pairing: &PendingPairingLease,
        approval: &PairingApprovalIdentity,
        acceptance: PairingAcceptance,
    ) -> Result<(), SettingsServiceError> {
        let acceptance = NativePairingAcceptance(Arc::new(PrivatePairingAcceptance(acceptance)));
        let mut state = pairing
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        let source_live = match self
            .authority
            .check_origin(&SettingsOperationOrigin::NativeUi {
                session_id: pairing.session_id,
            }) {
            Ok(()) => true,
            Err(
                SettingsServiceError::OutsideScope
                | SettingsServiceError::Stopped
                | SettingsServiceError::Expired,
            ) => false,
            Err(error) => return Err(error),
        };
        let live = source_live
            && !effect.claim.cancellation.is_cancelled()
            && Instant::now() < effect.claim.deadline
            && UnixMillis::now()?.get() < effect.claim.identity.binding.expires_at.get()
            && !pairing._session.cancellation.is_cancelled()
            && Instant::now() < pairing.deadline
            && UnixMillis::now()?.get() < pairing.expires_at.get()
            && state.lifecycle == NativePairingLifecycle::Retained;
        let retained = state
            .approval
            .as_mut()
            .ok_or(SettingsServiceError::Conflict)?;
        if retained.identity != *approval
            || retained.operation_id != approval.request_id
            || retained.native_request_id != approval.native_request_id
        {
            return Err(SettingsServiceError::Conflict);
        }
        if live && !retained.delivery_closed {
            retained.acceptance = Some(acceptance);
        } else {
            retained.delivery_closed = true;
            retained.acceptance = None;
        }
        state.invitation = None;
        if state.lifecycle == NativePairingLifecycle::Retained {
            state.lifecycle = NativePairingLifecycle::NoLongerPending;
        }
        Ok(())
    }

    fn settle_pairing_approval(
        &self,
        effect: &NativeHostEffectAdmission,
        pairing: &Arc<PendingPairingLease>,
        approval: &PairingApprovalIdentity,
        outcome: Result<(), SettingsServiceError>,
    ) -> Result<(), SettingsServiceError> {
        let observed = self.endpoint.pairing_approval_state(
            approval.pairing_id,
            &approval.responder,
            &approval.access,
        );
        let (state, failure) = match (outcome, observed) {
            (Ok(()), Ok(Some(_))) | (Err(_), Ok(Some(_))) => (
                SettingsOperationState::Completed {
                    receipt_id: Uuid::new_v4(),
                },
                None,
            ),
            (Ok(()), Ok(None)) => {
                return Err(self.failed_claim_bookkeeping(
                    &effect.claim,
                    SettingsServiceError::CorruptJournal,
                ));
            }
            (Err(cause), Ok(None)) => {
                let repair = pairing.cancel();
                let cause = match repair {
                    Ok(_) => cause,
                    Err(error) => SettingsServiceError::OutcomeUnknown {
                        request_id: approval.request_id,
                        cause: Box::new(cause),
                        repair_error: Some(Box::new(error)),
                    },
                };
                (
                    SettingsOperationState::Failed {
                        reason: SettingsFailureReason::NativeActionFailed,
                    },
                    Some(cause),
                )
            }
            (outcome, Err(error)) => {
                let error = SettingsServiceError::from(error);
                let cause = match outcome {
                    Ok(()) => error,
                    Err(cause) => SettingsServiceError::OutcomeUnknown {
                        request_id: approval.request_id,
                        cause: Box::new(cause),
                        repair_error: Some(Box::new(error)),
                    },
                };
                let repair = self.repository.finish(
                    &effect.claim.identity,
                    effect.claim.native.native_request_id,
                    self.result(&effect.claim.identity, SettingsOperationState::Unknown),
                    None,
                );
                let cleanup = self.release_approval_claim(&effect.claim);
                let repair_error = match (repair, cleanup) {
                    (Err(error), Err(cleanup)) => {
                        Some(Box::new(SettingsServiceError::OutcomeUnknown {
                            request_id: approval.request_id,
                            cause: Box::new(error),
                            repair_error: Some(Box::new(cleanup)),
                        }))
                    }
                    (Err(error), _) | (_, Err(error)) => Some(Box::new(error)),
                    _ => None,
                };
                return Err(SettingsServiceError::OutcomeUnknown {
                    request_id: approval.request_id,
                    cause: Box::new(cause),
                    repair_error,
                });
            }
        };
        let record = self.repository.get(approval.request_id)?;
        if !matches!(record.phase(), SettingsOperationPhase::Finished { .. }) {
            self.repository
                .finish(
                    &effect.claim.identity,
                    effect.claim.native.native_request_id,
                    self.result(&effect.claim.identity, state),
                    None,
                )
                .map_err(|cause| SettingsServiceError::OutcomeUnknown {
                    request_id: approval.request_id,
                    cause: Box::new(cause),
                    repair_error: None,
                })?;
        }
        {
            let mut cell = pairing
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if cell.lifecycle == NativePairingLifecycle::Retained {
                cell.lifecycle = NativePairingLifecycle::NoLongerPending;
            }
            cell.invitation = None;
        }
        self.release_approval_claim(&effect.claim)
            .map_err(|cause| SettingsServiceError::OutcomeUnknown {
                request_id: approval.request_id,
                cause: Box::new(cause),
                repair_error: None,
            })?;
        self.release_pairing(pairing)?;
        if let Some(cause) = failure {
            return Err(cause);
        }
        Ok(())
    }

    fn release_approval_claim(&self, claim: &Arc<ActionClaim>) -> Result<(), SettingsServiceError> {
        claim.close()?;
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
    }

    fn pairing_approval_result(
        &self,
        caller: &SettingsCaller,
        record: &OperationRecord,
    ) -> Result<NativePairingApprovalResult, SettingsServiceError> {
        self.check_native_approval_reader(caller, record)?;
        let SettingsActionTarget::PairingApproval(approval) = &record.identity.target else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let observed = self.endpoint.pairing_approval_state(
            approval.pairing_id,
            &approval.responder,
            &approval.access,
        )?;
        let active = !self.agent.store().transport_endpoint_revoked()?
            && observed
                .as_ref()
                .is_some_and(|state| state.revoked_at.is_none());
        let host_pin = match observed {
            Some(state) => {
                if state.device_id != approval.responder.device_id
                    || state.public_keys != approval.responder
                    || state.access != approval.access
                {
                    return Err(SettingsServiceError::CorruptJournal);
                }
                NativePairingPinState::Committed {
                    label: state.label,
                    pinned_at: state.pinned_at,
                    paired_at: state.paired_at,
                    revoked_at: state.revoked_at,
                }
            }
            None => NativePairingPinState::NotEstablished,
        };
        let mut acceptance = None;
        if let Some(pairing) = self.pairing(approval.creation_operation_id)? {
            let mut state = pairing
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            let live = match self.authority.validate(caller, false) {
                Ok(()) => true,
                Err(
                    SettingsServiceError::OutsideScope
                    | SettingsServiceError::Stopped
                    | SettingsServiceError::Expired,
                ) => false,
                Err(error) => return Err(error),
            };
            let deliver = active
                && live
                && !pairing._session.cancellation.is_cancelled()
                && pairing.session_id == record.identity.binding.client_session_id
                && pairing.pairing_id == approval.pairing_id
                && pairing.expires_at == approval.invitation_expires_at
                && Instant::now() < pairing.deadline
                && UnixMillis::now()?.get() < pairing.expires_at.get();
            let completed = matches!(record.phase(), SettingsOperationPhase::Finished { result } if matches!(result.state, SettingsOperationState::Completed { .. }));
            let terminal_without_acceptance =
                matches!(record.phase(), SettingsOperationPhase::Finished { .. }) && !completed;
            if let Some(retained) = &mut state.approval {
                if retained.identity != **approval {
                    return Err(SettingsServiceError::Conflict);
                }
                if deliver && completed && !retained.delivery_closed {
                    acceptance = retained.acceptance.clone();
                } else if !deliver || terminal_without_acceptance {
                    retained.delivery_closed = true;
                    retained.acceptance = None;
                }
            }
            drop(state);
            self.release_pairing(&pairing)?;
        }
        self.authority.validate(caller, true)?;
        let result = NativePairingApprovalResult {
            operation: record.inspection(),
            approval: approval.as_ref().clone(),
            host_pin,
            acceptance,
        };
        result.validate().map_err(SettingsServiceError::Invalid)?;
        Ok(result)
    }

    pub(super) fn recover_pairing_approval(
        &self,
        record: &OperationRecord,
    ) -> Result<bool, SettingsServiceError> {
        let SettingsActionTarget::PairingApproval(approval) = &record.identity.target else {
            return Ok(false);
        };
        let SettingsOperationPhase::InFlight { native_request_id } = record.phase() else {
            return Ok(false);
        };
        self.check_approval_creation(record, approval)?;
        if *native_request_id != approval.native_request_id {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let state = match self.endpoint.pairing_approval_state(
            approval.pairing_id,
            &approval.responder,
            &approval.access,
        ) {
            Ok(Some(_)) => SettingsOperationState::Completed {
                receipt_id: Uuid::new_v4(),
            },
            Ok(None) => SettingsOperationState::Unavailable {
                reason: SettingsUnavailableReason::NativeActionUnavailable,
            },
            Err(cause) => {
                let repair = self.repository.finish(
                    &record.identity,
                    *native_request_id,
                    self.result(&record.identity, SettingsOperationState::Unknown),
                    None,
                );
                return Err(SettingsServiceError::OutcomeUnknown {
                    request_id: approval.request_id,
                    cause: Box::new(cause.into()),
                    repair_error: repair.err().map(Box::new),
                });
            }
        };
        self.repository.finish(
            &record.identity,
            *native_request_id,
            self.result(&record.identity, state),
            None,
        )?;
        Ok(true)
    }
}
