use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::Agent;
use crate::devices::{DeviceEndpoint, EndpointRole};
use crate::protocol::{CommandId, DeviceId, ProtocolError, UnixMillis};
use crate::storage::StorageError;
use crate::transport::local::LocalCall;

use super::SettingsRepository;
use super::actions::{
    ActionClaim, ClaimState, NativeActionAdmission, NativeActionAdmissionResult,
    NativeActionDetails, NativeActionInvocation, NativeActionObservation, NativeActionReference,
    SettingsActionTarget,
};
use super::authority::{
    Authority, CallerSource, NativeSessionOpened, NativeSessionProof, SettingsAgentSession,
    SettingsCaller, SettingsOperationOrigin,
};
use super::contract::{
    AgentMutability, SETTINGS_CONTRACT_VERSION, SettingsActionReference, SettingsActionRequest,
    SettingsConfirmationReference, SettingsContractError, SettingsDeviceRequest,
    SettingsOperationResult, SettingsOperationState, SettingsPatch, SettingsRequestBinding,
    SettingsScope, SettingsToolName, SettingsUnavailableReason,
};
use super::operations::{
    NativeRequest, OperationIdentity, OperationRepository, SettingsOperationInspection,
    SettingsOperationPhase, request_digest,
};

#[derive(Debug, Error)]
pub enum SettingsServiceError {
    #[error("settings service requires the same leased Host Agent and repository")]
    LeaseRequired,
    #[error("settings request is outside its current authenticated scope")]
    OutsideScope,
    #[error("settings request conflicts with its original identity or revision")]
    Conflict,
    #[error("invalid settings service input: {0}")]
    Invalid(&'static str),
    #[error("settings service is at its retained capacity")]
    Capacity,
    #[error("settings action has expired")]
    Expired,
    #[error("settings operation was not found")]
    NotFound,
    #[error("settings operation was cancelled before admission")]
    Cancelled,
    #[error("settings intake is stopped")]
    Stopped,
    #[error("settings state lock is poisoned")]
    Poisoned,
    #[error("settings journal identity or history is invalid")]
    CorruptJournal,
    #[error("settings input could not be encoded")]
    Encoding,
    #[error("native session randomness is unavailable")]
    Entropy,
    #[error("settings worker failed; inspect its original operation")]
    Worker,
    #[error(transparent)]
    Contract(#[from] SettingsContractError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Settings(#[from] super::SettingsError),
}

#[derive(Clone)]
pub struct SettingsService {
    core: Arc<ServiceCore>,
}

struct ServiceCore {
    repository: OperationRepository,
    settings: SettingsRepository,
    authority: Arc<Authority>,
    mutation: Arc<AsyncMutex<()>>,
    claims: Mutex<BTreeMap<CommandId, Arc<ActionClaim>>>,
    pending: Mutex<BTreeMap<CommandId, Instant>>,
    agent: Agent,
}

impl SettingsService {
    pub(crate) async fn open(
        agent: Agent,
        settings: SettingsRepository,
        endpoint: &DeviceEndpoint,
        mutation: Arc<AsyncMutex<()>>,
    ) -> Result<Self, SettingsServiceError> {
        let lease = agent
            .state_lease()
            .ok_or(SettingsServiceError::LeaseRequired)?;
        if !Arc::ptr_eq(&lease, &settings.lease) || endpoint.role() != &EndpointRole::Host {
            return Err(SettingsServiceError::LeaseRequired);
        }
        let public_keys = endpoint.public_keys();
        let host_id = public_keys.device_id;
        let service = Self {
            core: Arc::new(ServiceCore {
                repository: OperationRepository::new(agent.store()),
                settings,
                authority: Authority::new(host_id),
                mutation,
                claims: Mutex::new(BTreeMap::new()),
                pending: Mutex::new(BTreeMap::new()),
                agent,
            }),
        };
        service
            .blocking(move |core| {
                core.check_endpoint(public_keys)?;
                for operation in core.repository.pending()? {
                    let state = match operation.phase() {
                        SettingsOperationPhase::InFlight { .. } => SettingsOperationState::Unknown,
                        _ => SettingsOperationState::Unavailable {
                            reason: SettingsUnavailableReason::Expired,
                        },
                    };
                    let result = core.result(&operation.identity, state);
                    match operation.phase() {
                        SettingsOperationPhase::InFlight { native_request_id } => {
                            core.repository.finish(
                                &operation.identity,
                                *native_request_id,
                                result,
                                None,
                            )?;
                        }
                        _ => {
                            core.repository
                                .finish_unadmitted(&operation.identity, result)?;
                        }
                    }
                }
                Ok(())
            })
            .await?;
        Ok(service)
    }

    pub(crate) fn local_owner_caller(
        &self,
        call: &LocalCall,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.owner(call)
    }

    pub(crate) fn open_native_session(
        &self,
        call: &LocalCall,
    ) -> Result<NativeSessionOpened, SettingsServiceError> {
        self.core.authority.open(call)
    }

    pub(crate) fn native_caller(
        &self,
        call: &LocalCall,
        proof: &NativeSessionProof,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.native(call, proof, false)
    }

    pub(crate) fn native_cleanup_caller(
        &self,
        call: &LocalCall,
        proof: &NativeSessionProof,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.native(call, proof, true)
    }

    pub(crate) fn begin_settings_session(
        &self,
        caller: SettingsCaller,
    ) -> Result<SettingsAgentSession, SettingsServiceError> {
        self.core.authority.begin_agent(&caller)
    }

    pub(crate) fn settings_session_caller(
        &self,
        session: &SettingsAgentSession,
        tool: SettingsToolName,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.agent(session, tool)
    }

    pub async fn operation(
        &self,
        caller: SettingsCaller,
        request_id: CommandId,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.core.authority.validate(&caller, false)?;
        self.blocking(move |core| {
            core.authority.validate(&caller, false)?;
            core.expire_unadmitted()?;
            let record = core.repository.get(request_id)?;
            core.check_principal(&caller, &record.identity)?;
            Ok(record.inspection())
        })
        .await
    }

    pub async fn request_action(
        &self,
        caller: SettingsCaller,
        request: SettingsActionRequest,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::RequestAction)?;
        self.prepare(caller, SettingsActionTarget::Native(request), cancellation)
            .await
    }

    pub(crate) async fn prepare_device_action(
        &self,
        caller: SettingsCaller,
        request: SettingsDeviceRequest,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        let tool = match &request.action {
            super::contract::SettingsDeviceAction::Pair => SettingsToolName::DevicesPair,
            super::contract::SettingsDeviceAction::Revoke { .. } => SettingsToolName::DevicesRevoke,
        };
        self.core.authority.require_tool(&caller, tool)?;
        self.prepare(caller, SettingsActionTarget::Device(request), cancellation)
            .await
    }

    pub(crate) async fn prepare_protected_patch(
        &self,
        caller: SettingsCaller,
        request: SettingsPatch,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::Update)?;
        if !request.changes.iter().any(|change| {
            change.key.agent_mutability() == AgentMutability::RequiresNativeConfirmation
        }) {
            return Err(SettingsServiceError::Invalid(
                "patch does not require native confirmation",
            ));
        }
        self.prepare(caller, SettingsActionTarget::Patch(request), cancellation)
            .await
    }

    async fn prepare(
        &self,
        caller: SettingsCaller,
        target: SettingsActionTarget,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        target.validate()?;
        self.core.check_scope(target.scope())?;
        let session_id = self.core.authority.session(&caller)?;
        let mutation = self.enter(&caller, &cancellation).await?;
        self.blocking(move |core| {
            let _mutation = mutation;
            core.authority.validate(&caller, false)?;
            core.expire_unadmitted()?;
            if cancellation.is_cancelled() {
                return Err(SettingsServiceError::Cancelled);
            }
            match core.repository.get(target.request_id()) {
                Ok(record) => {
                    core.check_principal(&caller, &record.identity)?;
                    if record.identity.target != target
                        || record.identity.origin != core.authority.origin(&caller)
                        || record.identity.binding.client_session_id != session_id
                    {
                        return Err(SettingsServiceError::Conflict);
                    }
                    return core.project(&record.inspection());
                }
                Err(SettingsServiceError::NotFound) => {}
                Err(error) => return Err(error),
            }
            let issued_at = UnixMillis::now()?;
            let (expires_at, deadline) = core.authority.lifetime(&caller)?;
            let identity = OperationIdentity {
                version: 1,
                binding: SettingsRequestBinding {
                    request_id: target.request_id(),
                    principal_id: core.authority.principal(),
                    client_session_id: session_id,
                    scope: target.scope().clone(),
                    expected_revision: target.expected_revision(),
                    exact_request_sha256: request_digest(&target)?,
                    issued_at,
                    expires_at,
                },
                target,
                origin: core.authority.origin(&caller),
            };
            let phase = if let Some(expected) = identity.binding.expected_revision {
                let actual = core.settings.snapshot()?.revision;
                if !matches!(identity.binding.scope, SettingsScope::Host { .. }) {
                    SettingsOperationPhase::Finished {
                        result: core.result(
                            &identity,
                            SettingsOperationState::Unavailable {
                                reason: SettingsUnavailableReason::NativeActionUnavailable,
                            },
                        ),
                    }
                } else if expected != actual {
                    SettingsOperationPhase::Finished {
                        result: core.result(
                            &identity,
                            SettingsOperationState::Conflict {
                                observed_revision: actual,
                            },
                        ),
                    }
                } else {
                    core.pending_phase(&identity)
                }
            } else {
                core.pending_phase(&identity)
            };
            let record = core
                .repository
                .prepare(identity, phase, || core.authority.validate(&caller, false))?;
            if matches!(
                record.phase(),
                SettingsOperationPhase::AwaitingAction { .. }
                    | SettingsOperationPhase::AwaitingConfirmation { .. }
            ) {
                core.pending
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?
                    .insert(record.identity.binding.request_id, deadline);
            }
            core.project(&record.inspection())
        })
        .await
    }

    pub(crate) async fn native_action_details(
        &self,
        caller: SettingsCaller,
        operation_id: CommandId,
        reference: NativeActionReference,
    ) -> Result<NativeActionDetails, SettingsServiceError> {
        self.core.authority.validate(&caller, false)?;
        if !matches!(caller.source, CallerSource::NativeUi(_)) {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.blocking(move |core| {
            core.authority.validate(&caller, false)?;
            let record = core.repository.get(operation_id)?;
            core.check_principal(&caller, &record.identity)?;
            let expected = match record.phase() {
                SettingsOperationPhase::AwaitingAction { reference } => {
                    NativeActionReference::Action(reference.clone())
                }
                SettingsOperationPhase::AwaitingConfirmation { reference } => {
                    NativeActionReference::Confirmation(reference.clone())
                }
                _ => return Err(SettingsServiceError::Conflict),
            };
            if expected != reference {
                return Err(SettingsServiceError::Conflict);
            }
            core.check_current(&record.identity)?;
            Ok(NativeActionDetails {
                operation: record.inspection(),
                target: record.identity.target,
            })
        })
        .await
    }

    pub(crate) async fn admit_native_action(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
        cancellation: CancellationToken,
    ) -> Result<NativeActionAdmissionResult, SettingsServiceError> {
        let session_id = self.core.authority.session(&caller)?;
        let mutation = self.enter(&caller, &cancellation).await?;
        self.blocking(move |core| {
            let _mutation = mutation;
            if cancellation.is_cancelled() {
                return Err(SettingsServiceError::Cancelled);
            }
            let record = core.repository.get(invocation.operation_id)?;
            core.check_principal(&caller, &record.identity)?;
            if record.identity.binding.client_session_id != session_id {
                return Err(SettingsServiceError::OutsideScope);
            }
            let expected_reference = match record.phase() {
                SettingsOperationPhase::AwaitingConfirmation { reference } => {
                    Some(NativeActionReference::Confirmation(reference.clone()))
                }
                SettingsOperationPhase::AwaitingAction { reference } => {
                    Some(NativeActionReference::Action(reference.clone()))
                }
                _ => None,
            };
            if expected_reference
                .as_ref()
                .is_some_and(|reference| reference != &invocation.reference)
            {
                return Err(SettingsServiceError::Conflict);
            }
            let original_deadline = core
                .pending
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .get(&invocation.operation_id)
                .copied();
            let native = NativeRequest {
                operation_id: invocation.operation_id,
                native_request_id: invocation.native_request_id,
                session_id,
                reference: invocation.reference,
            };
            let retained_session = core.authority.admit(&caller)?;
            let (record, fresh) = core.repository.claim(&record.identity, &native, || {
                core.authority.validate(&caller, false)?;
                if original_deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                    return Err(SettingsServiceError::Expired);
                }
                if cancellation.is_cancelled() {
                    return Err(SettingsServiceError::Cancelled);
                }
                core.check_current(&record.identity)
            })?;
            if !fresh {
                return Ok(NativeActionAdmissionResult::Existing(record.inspection()));
            }
            let deadline = original_deadline.ok_or(SettingsServiceError::Expired)?;
            let claim = Arc::new(ActionClaim {
                identity: record.identity,
                native: native.clone(),
                deadline,
                cancellation: retained_session.cancellation.child_token(),
                state: Mutex::new(ClaimState {
                    closed: false,
                    host_effect_started: false,
                }),
                repository: core.repository.clone(),
                session: Mutex::new(Some(retained_session)),
                _agent: core.agent.clone(),
            });
            let mut claims = core
                .claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if claims.len() >= 64 || claims.contains_key(&invocation.operation_id) {
                return Err(SettingsServiceError::Capacity);
            }
            claims.insert(invocation.operation_id, Arc::clone(&claim));
            core.pending
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .remove(&invocation.operation_id);
            Ok(NativeActionAdmissionResult::Admitted(
                NativeActionAdmission { claim },
            ))
        })
        .await
    }

    pub(crate) async fn finalize_native_action(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
        observation: NativeActionObservation,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        self.blocking(move |core| {
            core.authority.validate(&caller, true)?;
            let record = core.repository.get(invocation.operation_id)?;
            core.check_principal(&caller, &record.identity)?;
            let super::authority::CallerSource::NativeUi(session_id) = caller.source else {
                return Err(SettingsServiceError::OutsideScope);
            };
            if record.identity.binding.client_session_id != session_id {
                return Err(SettingsServiceError::OutsideScope);
            }
            if matches!(record.phase(), SettingsOperationPhase::Finished { .. }) {
                return core.repository.replay_observation(
                    &NativeRequest {
                        operation_id: invocation.operation_id,
                        native_request_id: invocation.native_request_id,
                        session_id,
                        reference: invocation.reference,
                    },
                    &observation,
                );
            }
            let claim = core.claim(&caller, &invocation)?;
            let inspection = claim.observe(observation)?;
            core.claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .remove(&invocation.operation_id);
            Ok(inspection)
        })
        .await
    }

    pub(crate) async fn cancel_native_action(
        &self,
        caller: SettingsCaller,
        request_id: CommandId,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        self.blocking(move |core| {
            core.authority.validate(&caller, true)?;
            let record = core.repository.get(request_id)?;
            core.check_principal(&caller, &record.identity)?;
            let super::authority::CallerSource::NativeUi(session_id) = caller.source else {
                return Err(SettingsServiceError::OutsideScope);
            };
            if record.identity.binding.client_session_id != session_id {
                return Err(SettingsServiceError::OutsideScope);
            }
            match record.phase() {
                SettingsOperationPhase::InFlight { .. } => {
                    let claim = core
                        .claims
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?
                        .get(&request_id)
                        .cloned()
                        .ok_or(SettingsServiceError::NotFound)?;
                    claim.close()?;
                    Ok(record.inspection())
                }
                SettingsOperationPhase::Finished { .. } => Ok(record.inspection()),
                _ => core.repository.finish_unadmitted(
                    &record.identity,
                    core.result(&record.identity, SettingsOperationState::Cancelled),
                ),
            }
        })
        .await
    }

    pub(crate) async fn close_native_session(
        &self,
        caller: SettingsCaller,
    ) -> Result<(), SettingsServiceError> {
        self.core.authority.close_session(&caller)?;
        let super::authority::CallerSource::NativeUi(session_id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        self.blocking(move |core| {
            for record in core.repository.pending()? {
                if record.identity.binding.client_session_id != session_id {
                    continue;
                }
                if matches!(record.phase(), SettingsOperationPhase::InFlight { .. }) {
                    let claim = core
                        .claims
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?
                        .get(&record.identity.binding.request_id)
                        .cloned();
                    if let Some(claim) = claim {
                        claim.close()?;
                    }
                } else {
                    core.repository.finish_unadmitted(
                        &record.identity,
                        core.result(&record.identity, SettingsOperationState::Cancelled),
                    )?;
                }
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn close(&self) -> Result<(), SettingsServiceError> {
        self.core.authority.close()?;
        self.blocking(|core| {
            let claims = core
                .claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for claim in claims {
                claim.close()?;
            }
            Ok(())
        })
        .await
    }

    async fn enter(
        &self,
        caller: &SettingsCaller,
        cancellation: &CancellationToken,
    ) -> Result<OwnedMutexGuard<()>, SettingsServiceError> {
        self.core.authority.validate(caller, false)?;
        let mutation = tokio::select! {
            biased;
            _=cancellation.cancelled()=>return Err(SettingsServiceError::Cancelled),
            mutation=Arc::clone(&self.core.mutation).lock_owned()=>mutation,
        };
        self.core.authority.validate(caller, false)?;
        Ok(mutation)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&ServiceCore) -> Result<T, SettingsServiceError> + Send + 'static,
    ) -> Result<T, SettingsServiceError> {
        let core = Arc::clone(&self.core);
        tokio::task::spawn_blocking(move || operation(&core))
            .await
            .map_err(|_| SettingsServiceError::Worker)?
    }
}

impl ServiceCore {
    fn expire_unadmitted(&self) -> Result<(), SettingsServiceError> {
        let now = UnixMillis::now()?;
        for record in self.repository.pending()? {
            if matches!(record.phase(), SettingsOperationPhase::InFlight { .. }) {
                continue;
            }
            let deadline = self
                .pending
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .get(&record.identity.binding.request_id)
                .copied();
            let origin_live = match self.authority.check_origin(&record.identity.origin) {
                Ok(()) => true,
                Err(
                    SettingsServiceError::OutsideScope
                    | SettingsServiceError::Stopped
                    | SettingsServiceError::Expired,
                ) => false,
                Err(error) => return Err(error),
            };
            if !origin_live
                || now.get() >= record.identity.binding.expires_at.get()
                || deadline.is_none_or(|deadline| Instant::now() >= deadline)
            {
                self.repository.finish_unadmitted(
                    &record.identity,
                    self.result(
                        &record.identity,
                        SettingsOperationState::Unavailable {
                            reason: SettingsUnavailableReason::Expired,
                        },
                    ),
                )?;
                self.pending
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?
                    .remove(&record.identity.binding.request_id);
            }
        }
        Ok(())
    }

    fn check_endpoint(
        &self,
        keys: crate::crypto::DevicePublicKeys,
    ) -> Result<(), SettingsServiceError> {
        let store = self.agent.store();
        let connection = store.connection()?;
        let mut statement = connection
            .prepare("SELECT device_id,configuration FROM transport_endpoint WHERE singleton=1")?;
        let mut rows = statement.query([])?;
        let row = rows.next()?.ok_or(SettingsServiceError::OutsideScope)?;
        let id = row
            .get_ref(0)?
            .as_str()
            .map_err(|_| SettingsServiceError::OutsideScope)?;
        let configuration = row
            .get_ref(1)?
            .as_str()
            .map_err(|_| SettingsServiceError::OutsideScope)?;
        if id.len() != 36 || configuration.len() > 16 * 1024 || id != keys.device_id.to_string() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let actual: serde_json::Value =
            serde_json::from_str(configuration).map_err(|_| SettingsServiceError::OutsideScope)?;
        if actual != serde_json::json!({"version":1,"keys":keys,"role":EndpointRole::Host}) {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    fn check_scope(&self, scope: &SettingsScope) -> Result<(), SettingsServiceError> {
        let id = match scope {
            SettingsScope::Host { host_id } => *host_id,
            SettingsScope::DevicePreferences { device_id }
            | SettingsScope::ObservedOsState { device_id } => *device_id,
        };
        if id != self.authority.host_id {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    fn check_principal(
        &self,
        caller: &SettingsCaller,
        identity: &OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        if !Arc::ptr_eq(&self.authority, &caller.authority)
            || identity.binding.principal_id != self.authority.principal()
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        match caller.source {
            CallerSource::SettingsAgent { session_id, .. } if !matches!(identity.origin,SettingsOperationOrigin::SettingsAgent{session_id:original,..} if original==session_id) =>
            {
                return Err(SettingsServiceError::OutsideScope);
            }
            CallerSource::NativeUi(session_id)
                if identity.binding.client_session_id != session_id =>
            {
                return Err(SettingsServiceError::OutsideScope);
            }
            _ => {}
        }
        self.check_scope(&identity.binding.scope)
    }

    fn check_current(&self, identity: &OperationIdentity) -> Result<(), SettingsServiceError> {
        self.authority.check_origin(&identity.origin)?;
        if let Some(expected) = identity.binding.expected_revision
            && self.settings.snapshot()?.revision != expected
        {
            return Err(SettingsServiceError::Conflict);
        }
        self.check_scope(&identity.binding.scope)
    }

    fn pending_phase(&self, identity: &OperationIdentity) -> SettingsOperationPhase {
        if matches!(identity.target, SettingsActionTarget::Patch(_)) {
            SettingsOperationPhase::AwaitingConfirmation {
                reference: SettingsConfirmationReference {
                    id: Uuid::new_v4(),
                    expires_at: identity.binding.expires_at,
                },
            }
        } else {
            SettingsOperationPhase::AwaitingAction {
                reference: SettingsActionReference {
                    id: Uuid::new_v4(),
                    expires_at: identity.binding.expires_at,
                },
            }
        }
    }

    fn result(
        &self,
        identity: &OperationIdentity,
        state: SettingsOperationState,
    ) -> SettingsOperationResult {
        SettingsOperationResult {
            contract_version: SETTINGS_CONTRACT_VERSION,
            request_id: identity.binding.request_id,
            scope: identity.binding.scope.clone(),
            state,
        }
    }

    fn project(
        &self,
        inspection: &SettingsOperationInspection,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        let state = match &inspection.phase {
            SettingsOperationPhase::AwaitingConfirmation { reference } => {
                SettingsOperationState::RequiresConfirmation {
                    confirmation: reference.clone(),
                }
            }
            SettingsOperationPhase::AwaitingAction { reference } => {
                SettingsOperationState::RequiresUserAction {
                    action: reference.clone(),
                }
            }
            SettingsOperationPhase::Finished { result } => return Ok(result.clone()),
            SettingsOperationPhase::InFlight { .. } => SettingsOperationState::Unknown,
        };
        Ok(SettingsOperationResult {
            contract_version: SETTINGS_CONTRACT_VERSION,
            request_id: inspection.binding.request_id,
            scope: inspection.binding.scope.clone(),
            state,
        })
    }

    fn claim(
        &self,
        caller: &SettingsCaller,
        invocation: &NativeActionInvocation,
    ) -> Result<Arc<ActionClaim>, SettingsServiceError> {
        let super::authority::CallerSource::NativeUi(session_id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        let claim = self
            .claims
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .get(&invocation.operation_id)
            .cloned()
            .ok_or(SettingsServiceError::NotFound)?;
        if claim.native.native_request_id != invocation.native_request_id
            || claim.native.reference != invocation.reference
            || claim.identity.binding.client_session_id != session_id
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        let record = self.repository.get(invocation.operation_id)?;
        if claim.identity != record.identity {
            return Err(SettingsServiceError::CorruptJournal);
        }
        Ok(claim)
    }
}
