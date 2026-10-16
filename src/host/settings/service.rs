mod agent_reads;
mod application;
mod changes;
mod credentials;
mod devices;
mod grant_owner;
#[cfg(unix)]
mod paired;
mod pairing;
mod persistence;
mod projection;
mod updates;
mod work;
mod writes;

pub use changes::{SettingsChangeHint, SettingsChangePage, SettingsChangeState};
pub use devices::NativeDeviceActionResult;
pub(super) use paired::PairedWriteAdmission;
#[cfg(unix)]
pub(crate) use paired::{
    OriginalReplySource, PairedParentAdmission, PairedSettingsParent, PairedUpdateOutcome,
    QualifiedOriginalReply, QualifiedSettingsCommand, SettingsReceiptAuthority,
};
pub use pairing::{
    NativePairingAcceptance, NativePairingAccess, NativePairingApprovalInput,
    NativePairingApprovalResult, NativePairingConversationAccess, NativePairingPinState,
    NativePairingRequest,
};
pub use persistence::OwnerSettingsWriteResult;
pub(crate) use work::{SettingsInvocationCompletion, SettingsInvocationDisposition};

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::Agent;
use crate::devices::{DeviceEndpoint, DeviceError, EndpointRole};
use crate::host::credentials::SecretTransfer;
use crate::protocol::{CommandId, ProtocolError, UnixMillis};
use crate::storage::StorageError;
use crate::transport::local::LocalCall;

use super::SettingsRepository;
use super::actions::{
    ActionClaim, ClaimState, NativeActionAdmission, NativeActionAdmissionResult,
    NativeActionDetails, NativeActionInvocation, NativeActionObservation, NativeActionReference,
    NativeHostEffectAdmission, NativePairingCreation, NativePairingLifecycle, PendingPairingLease,
    PendingPairingState, SettingsActionTarget,
};
use super::authority::{
    Authority, CallerSource, NativeSessionOpened, NativeSessionProof, SettingsAgentBinding,
    SettingsAgentSession, SettingsCaller, SettingsOperationOrigin,
};
use super::contract::{
    SETTINGS_CONTRACT_VERSION, SettingsActionReference, SettingsActionRequest,
    SettingsConfirmationReference, SettingsContractError, SettingsDeviceAction,
    SettingsDeviceRequest, SettingsOperationResult, SettingsOperationState, SettingsRequestBinding,
    SettingsScope, SettingsToolName, SettingsUnavailableReason,
};
use super::operations::{
    NativeRequest, OperationIdentity, OperationRecord, OperationRepository,
    PairingCreationMetadata, SettingsOperationInspection, SettingsOperationPhase, request_digest,
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
    #[error("settings mutation may be admitted; inspect its original operation {request_id}")]
    OutcomeUnknown {
        request_id: CommandId,
        cause: Box<SettingsServiceError>,
        repair_error: Option<Box<SettingsServiceError>>,
    },
    #[error("settings worker failed; inspect its original operation")]
    Worker,
    #[error("settings file reconciliation is required for original operation {request_id}")]
    WriteBlocked { request_id: CommandId },
    #[error("embedding profile metadata is unavailable")]
    EmbeddingMetadataUnavailable,
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
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error("optional settings application failed: {0}")]
    Application(#[source] Box<crate::host::runtime::HostError>),
    #[error(transparent)]
    Memory(#[from] crate::host::memory::MemoryError),
    #[error(transparent)]
    Selection(#[from] crate::host::selection::SelectionPreflightError),
    #[error(transparent)]
    Credential(#[from] crate::credentials::CredentialError),
}

#[derive(Clone)]
pub struct SettingsService {
    core: Arc<ServiceCore>,
    invocation: Option<Arc<work::InvocationWork>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingsStartupPolicy {
    Ready,
    OptionalFeaturesSuspended { operation_id: CommandId },
}

#[derive(Clone, Debug)]
pub(crate) struct SettingsStartupContext {
    pub settings: super::HostSettings,
    pub policy: SettingsStartupPolicy,
    pub pending_application: Option<SettingsOperationInspection>,
}

impl SettingsStartupContext {
    pub(crate) fn embedding_preferences(&self) -> crate::host::memory::EmbeddingPreferences {
        let mut preferences = updates::embedding_preferences(&self.settings);
        if !matches!(self.policy, SettingsStartupPolicy::Ready) {
            preferences.enabled = false;
        }
        preferences
    }

    pub(crate) fn selection_configuration(
        &self,
    ) -> (crate::host::selection::SelectionSettings, bool) {
        (
            self.settings.selection.clone(),
            matches!(self.policy, SettingsStartupPolicy::Ready)
                && updates::selection_enabled(&self.settings),
        )
    }
}

struct ActiveSettingsWrite {
    request_id: CommandId,
    client_session_id: Uuid,
    origin: SettingsOperationOrigin,
    cancellation: CancellationToken,
}

struct ServiceCore {
    repository: OperationRepository,
    settings: SettingsRepository,
    authority: Arc<Authority>,
    mutation: Arc<AsyncMutex<()>>,
    claims: Mutex<BTreeMap<CommandId, Arc<ActionClaim>>>,
    pending: Mutex<BTreeMap<CommandId, Instant>>,
    pairings: Mutex<BTreeMap<CommandId, Arc<PendingPairingLease>>>,
    endpoint: DeviceEndpoint,
    active_write: Mutex<Option<Arc<ActiveSettingsWrite>>>,
    composition: Mutex<Option<Arc<crate::host::composition::Composition>>>,
    applications: Mutex<BTreeMap<CommandId, writes::ApplicationCapture>>,
    application_stop: CancellationToken,
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
                pairings: Mutex::new(BTreeMap::new()),
                endpoint: endpoint.clone(),
                active_write: Mutex::new(None),
                composition: Mutex::new(None),
                applications: Mutex::new(BTreeMap::new()),
                application_stop: CancellationToken::new(),
                agent,
            }),
            invocation: None,
        };
        service
            .blocking(move |core| {
                core.check_endpoint(public_keys)?;
                for operation in core.repository.pending()? {
                    if operation.write().is_some() {
                        continue;
                    }
                    if core.recover_pairing_approval(&operation)? {
                        continue;
                    }
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

    pub(in crate::host) fn bind_composition(
        &self,
        composition: Arc<crate::host::composition::Composition>,
    ) -> Result<(), SettingsServiceError> {
        if !composition.shares_agent(&self.core.agent) {
            return Err(SettingsServiceError::LeaseRequired);
        }
        let mut current = self
            .core
            .composition
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if let Some(existing) = &*current {
            if !Arc::ptr_eq(existing, &composition) {
                return Err(SettingsServiceError::Conflict);
            }
            return Ok(());
        }
        *current = Some(composition);
        Ok(())
    }

    pub(crate) fn local_owner_caller(
        &self,
        call: &LocalCall,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.owner(call)
    }

    pub(crate) fn local_owner_cleanup_caller(
        &self,
        call: &LocalCall,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.cleanup_owner(call)
    }

    pub(crate) async fn startup_context(
        &self,
    ) -> Result<SettingsStartupContext, SettingsServiceError> {
        self.blocking(|core| {
            let settings = core.settings.observed_snapshot()?;
            let fingerprint = persistence::file_fingerprint(&settings)?;
            let unresolved = core.repository.unresolved_write()?;
            let mut pending_application = None;
            for operation in core.repository.pending_applications()? {
                if operation
                    .write()
                    .is_some_and(|write| write.intent.after == fingerprint)
                {
                    if pending_application.is_some() {
                        return Err(SettingsServiceError::CorruptJournal);
                    }
                    pending_application = Some(operation.inspection());
                }
            }
            let operation_id = unresolved
                .map(|record| record.identity.binding.request_id)
                .or_else(|| {
                    pending_application
                        .as_ref()
                        .map(|record| record.binding.request_id)
                });
            Ok(SettingsStartupContext {
                settings,
                policy: match operation_id {
                    Some(operation_id) => {
                        SettingsStartupPolicy::OptionalFeaturesSuspended { operation_id }
                    }
                    None => SettingsStartupPolicy::Ready,
                },
                pending_application,
            })
        })
        .await
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

    pub(crate) fn settings_cleanup_principal(
        &self,
        caller: &SettingsCaller,
    ) -> Result<Uuid, SettingsServiceError> {
        self.core.authority.validate_owner_cleanup(caller)?;
        Ok(self.core.authority.principal())
    }

    pub(crate) async fn inspect_settings_cleanup(
        &self,
        caller: SettingsCaller,
        command_id: CommandId,
        expected_fence_sha256: String,
    ) -> Result<(), SettingsServiceError> {
        self.core.authority.validate_owner_cleanup(&caller)?;
        self.blocking(move |core| {
            core.authority.validate_owner_cleanup(&caller)?;
            let store = core.agent.store();
            let fence = store
                .settings_turn(command_id)?
                .ok_or(SettingsServiceError::NotFound)?;
            let session = store
                .settings_session(fence.session_id)?
                .ok_or(SettingsServiceError::NotFound)?;
            if fence.sha256()? != expected_fence_sha256
                || session.principal_id != core.authority.principal()
            {
                return Err(SettingsServiceError::OutsideScope);
            }
            for id in fence.mutation_ids {
                match core.repository.get(id) {
                    Ok(record)
                        if record.identity.origin
                            == (SettingsOperationOrigin::SettingsAgent {
                                session_id: session.session_id,
                                native_session_id: session.native_session_id,
                            })
                            && record.identity.binding.principal_id == session.principal_id =>
                    {
                        record.inspection().validate()?;
                    }
                    Err(SettingsServiceError::NotFound) => {}
                    Ok(_) => return Err(SettingsServiceError::OutsideScope),
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })
        .await
    }

    pub fn begin_settings_session(
        &self,
        caller: SettingsCaller,
    ) -> Result<SettingsAgentSession, SettingsServiceError> {
        self.core.authority.begin_agent(&caller)
    }

    pub fn settings_session_caller(
        &self,
        session: &SettingsAgentSession,
        tool: SettingsToolName,
    ) -> Result<SettingsCaller, SettingsServiceError> {
        self.core.authority.agent(session, tool)
    }

    pub(crate) fn settings_agent_binding(
        &self,
        session: &SettingsAgentSession,
    ) -> Result<SettingsAgentBinding, SettingsServiceError> {
        self.core.authority.agent_binding(session)
    }

    pub(crate) fn settings_invocation(
        &self,
        session: &SettingsAgentSession,
        tool: SettingsToolName,
        operation_id: CommandId,
    ) -> Result<
        (
            SettingsService,
            SettingsCaller,
            SettingsInvocationCompletion,
        ),
        SettingsServiceError,
    > {
        if self.invocation.is_some() || operation_id.as_uuid().is_nil() {
            return Err(SettingsServiceError::OutsideScope);
        }
        let binding = self.core.authority.agent_binding(session)?;
        let caller = self.core.authority.agent(session, tool)?;
        let (work, completion) = work::InvocationWork::new(binding, tool, operation_id);
        Ok((
            Self {
                core: Arc::clone(&self.core),
                invocation: Some(work),
            },
            caller,
            completion,
        ))
    }

    pub(crate) async fn close_settings_agent(
        &self,
        session: &SettingsAgentSession,
    ) -> Result<(), SettingsServiceError> {
        let (session_id, native_session_id) = self.core.authority.close_agent(session)?;
        let origin = SettingsOperationOrigin::SettingsAgent {
            session_id,
            native_session_id,
        };
        let active = self
            .core
            .active_write
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .clone();
        if let Some(active) = active
            && active.origin == origin
        {
            active.cancellation.cancel();
        }
        let mutation = Arc::clone(&self.core.mutation).lock_owned().await;
        let service = Self {
            core: Arc::clone(&self.core),
            invocation: None,
        };
        service
            .blocking(move |core| {
                let _mutation = mutation;
                for record in core.repository.pending()? {
                    if record.identity.origin != origin {
                        continue;
                    }
                    if record.write().is_some() {
                        core.cancel_write(record.identity.binding.request_id)?;
                        continue;
                    }
                    if matches!(
                        record.phase(),
                        SettingsOperationPhase::AwaitingAction { .. }
                            | SettingsOperationPhase::AwaitingConfirmation { .. }
                    ) {
                        core.repository.finish_unadmitted(
                            &record.identity,
                            core.result(&record.identity, SettingsOperationState::Cancelled),
                        )?;
                        core.pending
                            .lock()
                            .map_err(|_| SettingsServiceError::Poisoned)?
                            .remove(&record.identity.binding.request_id);
                    }
                }
                Ok(())
            })
            .await
    }

    pub async fn operation(
        &self,
        caller: SettingsCaller,
        request_id: CommandId,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.core.check_operation_reader(&caller)?;
        self.blocking(move |core| {
            core.check_operation_reader(&caller)?;
            if matches!(caller.source, CallerSource::SettingsAgent { .. }) {
                core.expire_unadmitted()?;
            }
            let record = core.repository.get(request_id)?;
            core.check_principal(&caller, &record.identity)?;
            core.check_operation_reader(&caller)?;
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
        let _work = self.invocation_guard(
            &caller,
            SettingsToolName::RequestAction,
            Some(request.request_id),
        )?;
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::RequestAction)?;
        self.prepare(caller, SettingsActionTarget::Native(request), cancellation)
            .await
    }

    pub(crate) async fn inspect_native_operation(
        &self,
        caller: SettingsCaller,
        request_id: CommandId,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        let CallerSource::NativeUi(session_id) = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        self.blocking(move |core| {
            core.authority.validate(&caller, true)?;
            let record = core.repository.get(request_id)?;
            core.check_principal(&caller, &record.identity)?;
            if record.identity.binding.client_session_id != session_id {
                return Err(SettingsServiceError::OutsideScope);
            }
            Ok(record.inspection())
        })
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

    async fn prepare(
        &self,
        caller: SettingsCaller,
        target: SettingsActionTarget,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationResult, SettingsServiceError> {
        if let Err(error) = target.validate() {
            self.reject_invocation()?;
            return Err(error);
        }
        if let Err(error) = self.core.check_scope(target.scope()) {
            self.reject_invocation()?;
            return Err(error);
        }
        let session_id = self.core.operation_session(&caller, &target)?;
        let mutation = self.enter(&caller, &cancellation).await?;
        let work = self.invocation.clone();
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
                    if let Some(work) = &work {
                        work.recorded(record.identity.binding.request_id)?;
                    }
                    return core.project(&record.inspection());
                }
                Err(SettingsServiceError::NotFound) => {}
                Err(error) => return Err(error),
            }
            let (identity, deadline) = core.operation_identity(&caller, target)?;
            let phase = if matches!(caller.source, CallerSource::LocalOwner { .. }) {
                SettingsOperationPhase::Finished {
                    result: core.result(&identity, SettingsOperationState::HostActionRequired),
                }
            } else if let Some(expected) = identity.binding.expected_revision {
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
            if let Some(work) = &work {
                work.attempting()?;
            }
            let record = core
                .repository
                .prepare(identity, phase, || core.authority.validate(&caller, false))?;
            if let Some(work) = &work {
                work.recorded(record.identity.binding.request_id)?;
            }
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
        let mutation = self.enter(&caller, &cancellation).await?;
        self.blocking(move |core| {
            let _mutation = mutation;
            core.admit_native_action(&caller, invocation, &cancellation)
        })
        .await
    }

    pub(crate) async fn start_host_effect(
        &self,
        admission: NativeActionAdmission,
        cancellation: CancellationToken,
    ) -> Result<NativeHostEffectAdmission, SettingsServiceError> {
        if !Arc::ptr_eq(&self.core.authority, &admission.claim.authority) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mutation = tokio::select! {
            biased;
            _=cancellation.cancelled()=>return Err(SettingsServiceError::Cancelled),
            mutation=Arc::clone(&self.core.mutation).lock_owned()=>mutation,
        };
        self.blocking(move |core| {
            if cancellation.is_cancelled() {
                return Err(SettingsServiceError::Cancelled);
            }
            core.start_host_effect(&admission)?;
            Ok(NativeHostEffectAdmission {
                _mutation: mutation,
                claim: admission.claim,
            })
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

    pub(crate) async fn create_pairing(
        &self,
        effect: NativeHostEffectAdmission,
    ) -> Result<NativePairingCreation, SettingsServiceError> {
        if !Arc::ptr_eq(&self.core.authority, &effect.claim.authority) {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.blocking(move |core| {
            let retained_effect = effect;
            core.create_pairing(&retained_effect)
        })
        .await
    }

    pub(crate) async fn native_pairing(
        &self,
        caller: SettingsCaller,
        request_id: CommandId,
    ) -> Result<NativePairingCreation, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        self.blocking(move |core| {
            core.authority.validate(&caller, true)?;
            let record = core.repository.get(request_id)?;
            core.check_pairing_caller(&caller, &record.identity)?;
            core.pairing_result(&caller, &record)
        })
        .await
    }

    pub(crate) async fn cancel_native_pairing(
        &self,
        caller: SettingsCaller,
        request_id: CommandId,
    ) -> Result<NativePairingCreation, SettingsServiceError> {
        self.core.authority.validate(&caller, true)?;
        self.blocking(move |core| {
            core.authority.validate(&caller, true)?;
            let record = core.repository.get(request_id)?;
            core.check_pairing_caller(&caller, &record.identity)?;
            let pairing = core.pairing(request_id)?;
            let lifecycle = match &pairing {
                Some(pairing) => pairing.cancel()?,
                None => NativePairingLifecycle::Unavailable,
            };
            if let Some(pairing) = &pairing {
                core.release_pairing(pairing)?;
            }
            Ok(NativePairingCreation {
                operation: record.inspection(),
                lifecycle,
                invitation: None,
            })
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
            if record.write().is_some() {
                core.cancel_write(request_id)?;
                return Ok(record.inspection());
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
                    let pairing = claim
                        .state
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?
                        .pairing
                        .clone();
                    if let Some(pairing) = pairing {
                        pairing.cancel()?;
                        core.release_pairing(&pairing)?;
                    }
                    Ok(record.inspection())
                }
                SettingsOperationPhase::Finished { .. } => Ok(record.inspection()),
                SettingsOperationPhase::Applying { .. } => {
                    Err(SettingsServiceError::CorruptJournal)
                }
                _ => {
                    let inspection = core.repository.finish_unadmitted(
                        &record.identity,
                        core.result(&record.identity, SettingsOperationState::Cancelled),
                    )?;
                    core.pending
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?
                        .remove(&request_id);
                    Ok(inspection)
                }
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
                if record.write().is_some() {
                    core.cancel_write(record.identity.binding.request_id)?;
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
                    core.pending
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?
                        .remove(&record.identity.binding.request_id);
                }
            }
            let active = core
                .active_write
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .clone();
            if let Some(active) = active
                && active.client_session_id == session_id
            {
                active.cancellation.cancel();
            }
            for pairing in core.retained_pairings()? {
                if pairing.session_id == session_id {
                    pairing.cancel()?;
                    core.release_pairing(&pairing)?;
                }
            }
            Ok(())
        })
        .await
    }

    pub(crate) fn stop_intake(&self) -> Result<(), SettingsServiceError> {
        self.core.application_stop.cancel();
        self.core.authority.close()
    }

    pub(crate) async fn close(&self) -> Result<(), SettingsServiceError> {
        self.stop_intake()?;
        let active = self
            .core
            .active_write
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .clone();
        if let Some(active) = active {
            active.cancellation.cancel();
        }
        let mutation = Arc::clone(&self.core.mutation).lock_owned().await;
        drop(mutation);
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
            for pairing in core.retained_pairings()? {
                pairing.cancel()?;
                core.release_pairing(&pairing)?;
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
        let child = self.core.authority.agent_cancellation(caller)?;
        let child_cancelled = async move {
            match child {
                Some(child) => child.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        let mutation = tokio::select! {
            biased;
            _=cancellation.cancelled()=>return Err(SettingsServiceError::Cancelled),
            _=child_cancelled=>return Err(SettingsServiceError::Cancelled),
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
        let work = self
            .invocation
            .as_ref()
            .map(work::InvocationWork::fork)
            .transpose()?;
        tokio::task::spawn_blocking(move || {
            let _work = work;
            let result = operation(&core);
            drop(core);
            result
        })
        .await
        .map_err(|_| SettingsServiceError::Worker)?
    }
}

impl ServiceCore {
    fn admit_native_action(
        &self,
        caller: &SettingsCaller,
        invocation: NativeActionInvocation,
        cancellation: &CancellationToken,
    ) -> Result<NativeActionAdmissionResult, SettingsServiceError> {
        let session_id = self.authority.session(caller)?;
        if cancellation.is_cancelled() {
            return Err(SettingsServiceError::Cancelled);
        }
        let record = self.repository.get(invocation.operation_id)?;
        self.check_principal(caller, &record.identity)?;
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
        let original_deadline = self
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
        if let Some(inspection) = self.repository.replay_claim(&record.identity, &native)? {
            return Ok(NativeActionAdmissionResult::Existing(Box::new(inspection)));
        }
        {
            let claims = self
                .claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if claims.len() >= 64 {
                return Err(SettingsServiceError::Capacity);
            }
            if claims.contains_key(&invocation.operation_id) {
                return Err(SettingsServiceError::Conflict);
            }
        }
        let retained_session = self.authority.admit(caller)?;
        let (record, fresh) = self.repository.claim(&record.identity, &native, || {
            self.authority.validate(caller, false)?;
            if original_deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                return Err(SettingsServiceError::Expired);
            }
            if cancellation.is_cancelled() {
                return Err(SettingsServiceError::Cancelled);
            }
            self.check_current(&record.identity)
        })?;
        if !fresh {
            return Ok(NativeActionAdmissionResult::Existing(Box::new(
                record.inspection(),
            )));
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
                pairing: None,
            }),
            repository: self.repository.clone(),
            authority: Arc::clone(&self.authority),
            settings: self.settings.clone(),
            session: Mutex::new(Some(retained_session)),
            _agent: self.agent.clone(),
        });
        let insertion = match self.claims.lock() {
            Ok(mut claims) => match claims.entry(invocation.operation_id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(Arc::clone(&claim));
                    Ok(())
                }
                std::collections::btree_map::Entry::Occupied(_) => {
                    Err(SettingsServiceError::Conflict)
                }
            },
            Err(_) => Err(SettingsServiceError::Poisoned),
        };
        if let Err(error) = insertion {
            return Err(self.failed_claim_bookkeeping(&claim, error));
        }
        match self.pending.lock() {
            Ok(mut pending) => {
                pending.remove(&invocation.operation_id);
            }
            Err(_) => {
                return Err(self.failed_claim_bookkeeping(&claim, SettingsServiceError::Poisoned));
            }
        }
        Ok(NativeActionAdmissionResult::Admitted(
            NativeActionAdmission { claim },
        ))
    }

    fn start_host_effect(
        &self,
        admission: &NativeActionAdmission,
    ) -> Result<(), SettingsServiceError> {
        if !Arc::ptr_eq(&self.authority, &admission.claim.authority) {
            return Err(SettingsServiceError::OutsideScope);
        }
        let mut state = admission
            .claim
            .state
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        admission.claim.check_effect(&state)?;
        match &admission.claim.identity.target {
            SettingsActionTarget::Device(_)
            | SettingsActionTarget::Patch(_)
            | SettingsActionTarget::PairingApproval(_) => {}
            SettingsActionTarget::Native(super::contract::SettingsActionRequest {
                action: super::contract::NativeSettingsAction::CredentialEntry { .. },
                ..
            }) => {}
            _ => return Err(SettingsServiceError::OutsideScope),
        }
        state.host_effect_started = true;
        drop(state);
        Ok(())
    }

    fn check_operation_reader(&self, caller: &SettingsCaller) -> Result<(), SettingsServiceError> {
        match caller.source {
            CallerSource::LocalOwner { .. } => self.authority.validate_owner_cleanup(caller),
            CallerSource::NativeUi(_) => self.authority.validate(caller, true),
            CallerSource::SettingsAgent { .. } => self.authority.validate(caller, false),
            CallerSource::PairedSettings { .. } => Err(SettingsServiceError::OutsideScope),
        }
    }

    fn operation_session(
        &self,
        caller: &SettingsCaller,
        target: &SettingsActionTarget,
    ) -> Result<Uuid, SettingsServiceError> {
        self.authority.validate(caller, false)?;
        match caller.source {
            CallerSource::LocalOwner { request_id }
                if request_id == target.request_id()
                    && matches!(
                        target,
                        SettingsActionTarget::Patch(_) | SettingsActionTarget::OwnerReplacement(_)
                    ) =>
            {
                Ok(*request_id.as_uuid())
            }
            CallerSource::LocalOwner { .. } => Err(SettingsServiceError::OutsideScope),
            _ => self.authority.session(caller),
        }
    }

    fn operation_identity(
        &self,
        caller: &SettingsCaller,
        target: SettingsActionTarget,
    ) -> Result<(OperationIdentity, Instant), SettingsServiceError> {
        target.validate()?;
        self.check_scope(target.scope())?;
        let client_session_id = self.operation_session(caller, &target)?;
        let issued_at = UnixMillis::now()?;
        let (expires_at, deadline) = match caller.source {
            CallerSource::LocalOwner { .. } => {
                let lifetime = super::contract::MAX_SETTINGS_ACTION_LIFETIME_MS;
                let expiry = issued_at
                    .get()
                    .checked_add(lifetime)
                    .ok_or(SettingsServiceError::Expired)?;
                let deadline = Instant::now()
                    .checked_add(std::time::Duration::from_millis(lifetime))
                    .ok_or(SettingsServiceError::Expired)?;
                (UnixMillis::try_from(expiry)?, deadline)
            }
            _ => self.authority.lifetime(caller)?,
        };
        let identity = OperationIdentity {
            version: 1,
            binding: SettingsRequestBinding {
                request_id: target.request_id(),
                principal_id: self.authority.principal(),
                client_session_id,
                scope: target.scope().clone(),
                expected_revision: target.expected_revision(),
                exact_request_sha256: request_digest(&target)?,
                issued_at,
                expires_at,
            },
            target,
            origin: self.authority.origin(caller),
        };
        identity.validate()?;
        Ok((identity, deadline))
    }

    fn check_write_origin(
        &self,
        identity: &OperationIdentity,
        deadline: Instant,
        cancellation: &CancellationToken,
        request_cancellation: &CancellationToken,
    ) -> Result<(), SettingsServiceError> {
        self.authority.check_origin(&identity.origin)?;
        self.check_scope(&identity.binding.scope)?;
        if cancellation.is_cancelled() || request_cancellation.is_cancelled() {
            return Err(SettingsServiceError::Cancelled);
        }
        if UnixMillis::now()?.get() >= identity.binding.expires_at.get()
            || Instant::now() >= deadline
        {
            return Err(SettingsServiceError::Expired);
        }
        Ok(())
    }

    fn require_resolved_file(&self) -> Result<(), SettingsServiceError> {
        if let Some(operation) = self.repository.unresolved_write()? {
            return Err(SettingsServiceError::WriteBlocked {
                request_id: operation.identity.binding.request_id,
            });
        }
        Ok(())
    }

    fn cancel_write(&self, request_id: CommandId) -> Result<(), SettingsServiceError> {
        let active = self
            .active_write
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .clone();
        if let Some(active) = active
            && active.request_id == request_id
        {
            active.cancellation.cancel();
        }
        Ok(())
    }
    fn create_pairing(
        &self,
        effect: &NativeHostEffectAdmission,
    ) -> Result<NativePairingCreation, SettingsServiceError> {
        let endpoint = self.endpoint.clone();
        if !matches!(
            effect.target(),
            SettingsActionTarget::Device(SettingsDeviceRequest {
                action: SettingsDeviceAction::Pair,
                ..
            })
        ) || endpoint.role() != &EndpointRole::Host
            || endpoint.public_keys().device_id != self.authority.host_id
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.check_endpoint(endpoint.public_keys())?;
        for pairing in self.retained_pairings()? {
            if Instant::now() >= pairing.deadline
                || UnixMillis::now()?.get() >= pairing.expires_at.get()
            {
                pairing.cancel()?;
            }
            self.release_pairing(&pairing)?;
        }
        {
            let pairings = self
                .pairings
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if pairings.contains_key(&effect.operation_id()) {
                return Err(SettingsServiceError::Conflict);
            }
            if pairings.len() >= 8 {
                return Err(SettingsServiceError::Capacity);
            }
        }
        let session = effect
            .claim
            .session
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .as_ref()
            .ok_or(SettingsServiceError::OutsideScope)?
            .retain();
        let (now, lifetime) = {
            let state = effect
                .claim
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            super::actions::validate_current(
                &self.authority,
                &self.settings,
                &effect.claim.identity,
            )?;
            let current = UnixMillis::now()?;
            if state.closed
                || !state.host_effect_started
                || effect.claim.cancellation.is_cancelled()
                || current.get() >= effect.claim.identity.binding.expires_at.get()
            {
                return Err(SettingsServiceError::Expired);
            }
            let monotonic = effect
                .claim
                .deadline
                .checked_duration_since(Instant::now())
                .ok_or(SettingsServiceError::Expired)?;
            let monotonic =
                u64::try_from(monotonic.as_millis()).map_err(|_| SettingsServiceError::Expired)?;
            let remaining = effect.claim.identity.binding.expires_at.get() - current.get();
            let lifetime = monotonic.min(remaining);
            if lifetime == 0 {
                return Err(SettingsServiceError::Expired);
            }
            (current, lifetime)
        };
        let mut display = endpoint.begin_pairing(now, lifetime)?;
        let pairing_id = display.pairing_id;
        let created = (|| {
            let pairing = Arc::new(PendingPairingLease {
                operation_id: effect.operation_id(),
                session_id: effect.claim.native.session_id,
                pairing_id,
                expires_at: UnixMillis::try_from(display.expires_at_ms)?,
                deadline: effect.claim.deadline,
                fingerprint: display.fingerprint,
                endpoint: endpoint.clone(),
                state: Mutex::new(PendingPairingState {
                    lifecycle: NativePairingLifecycle::Retained,
                    invitation: Some(SecretTransfer::new(std::mem::take(
                        &mut *display.invitation_json,
                    ))?),
                    approval: None,
                }),
                _session: session,
                _agent: self.agent.clone(),
            });
            let closed = {
                let mut state = effect
                    .claim
                    .state
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?;
                if state.pairing.is_some() {
                    return Err(SettingsServiceError::Conflict);
                }
                state.pairing = Some(Arc::clone(&pairing));
                state.closed || effect.claim.cancellation.is_cancelled()
            };
            {
                let mut pairings = self
                    .pairings
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?;
                match pairings.entry(effect.operation_id()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(Arc::clone(&pairing));
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {
                        return Err(SettingsServiceError::Conflict);
                    }
                }
            }
            let inspection = self.repository.finish_pairing(
                &effect.claim.identity,
                effect.claim.native.native_request_id,
                PairingCreationMetadata {
                    pairing_id,
                    expires_at: pairing.expires_at,
                },
            )?;
            effect.claim.close()?;
            effect
                .claim
                .session
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .take();
            self.claims
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .remove(&effect.operation_id());
            let source_live = match self.authority.check_origin(&effect.claim.identity.origin) {
                Ok(()) => true,
                Err(
                    SettingsServiceError::Expired
                    | SettingsServiceError::OutsideScope
                    | SettingsServiceError::Stopped,
                ) => false,
                Err(error) => return Err(error),
            };
            if closed
                || !source_live
                || Instant::now() >= pairing.deadline
                || UnixMillis::now()?.get() >= pairing.expires_at.get()
            {
                let lifecycle = pairing.cancel()?;
                self.release_pairing(&pairing)?;
                return Ok(NativePairingCreation {
                    operation: inspection,
                    lifecycle,
                    invitation: None,
                });
            }
            Ok(NativePairingCreation {
                operation: inspection,
                lifecycle: pairing.lifecycle()?,
                invitation: Some(pairing.invitation()?),
            })
        })();
        match created {
            Ok(result) => Ok(result),
            Err(cause) => {
                let repair = match self.pairing(effect.operation_id()) {
                    Ok(Some(pairing)) => pairing.cancel().map(|_| ()),
                    Ok(None) => UnixMillis::now()
                        .map_err(SettingsServiceError::from)
                        .and_then(|now| {
                            endpoint
                                .cancel_pairing(pairing_id, now)
                                .map(|_| ())
                                .map_err(SettingsServiceError::from)
                        }),
                    Err(error) => Err(error),
                };
                Err(SettingsServiceError::OutcomeUnknown {
                    request_id: effect.operation_id(),
                    cause: Box::new(cause),
                    repair_error: repair.err().map(Box::new),
                })
            }
        }
    }

    fn retained_pairings(&self) -> Result<Vec<Arc<PendingPairingLease>>, SettingsServiceError> {
        Ok(self
            .pairings
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .values()
            .cloned()
            .collect())
    }

    fn pairing(
        &self,
        request_id: CommandId,
    ) -> Result<Option<Arc<PendingPairingLease>>, SettingsServiceError> {
        Ok(self
            .pairings
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .get(&request_id)
            .cloned())
    }

    fn release_pairing(
        &self,
        pairing: &Arc<PendingPairingLease>,
    ) -> Result<(), SettingsServiceError> {
        {
            let state = pairing
                .state
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if matches!(
                state.lifecycle,
                NativePairingLifecycle::Retained | NativePairingLifecycle::CancellationPending
            ) || state
                .approval
                .as_ref()
                .is_some_and(|approval| !approval.delivery_closed && approval.acceptance.is_some())
            {
                return Ok(());
            }
        }
        let mut pairings = self
            .pairings
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?;
        if pairings
            .get(&pairing.operation_id)
            .is_some_and(|current| Arc::ptr_eq(current, pairing))
        {
            pairings.remove(&pairing.operation_id);
        }
        Ok(())
    }

    fn check_pairing_caller(
        &self,
        caller: &SettingsCaller,
        identity: &OperationIdentity,
    ) -> Result<(), SettingsServiceError> {
        self.check_principal(caller, identity)?;
        if !matches!(caller.source,CallerSource::NativeUi(id) if id==identity.binding.client_session_id)
            || !matches!(
                identity.target,
                SettingsActionTarget::Device(SettingsDeviceRequest {
                    action: SettingsDeviceAction::Pair,
                    ..
                })
            )
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        Ok(())
    }

    fn pairing_result(
        &self,
        caller: &SettingsCaller,
        record: &OperationRecord,
    ) -> Result<NativePairingCreation, SettingsServiceError> {
        let metadata = record.pairing().ok_or(SettingsServiceError::NotFound)?;
        let Some(pairing) = self.pairing(record.identity.binding.request_id)? else {
            return Ok(NativePairingCreation {
                operation: record.inspection(),
                lifecycle: NativePairingLifecycle::Unavailable,
                invitation: None,
            });
        };
        if metadata.pairing_id != pairing.pairing_id
            || metadata.expires_at != pairing.expires_at
            || pairing.session_id != record.identity.binding.client_session_id
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let fresh = match self.authority.validate(caller, false) {
            Ok(()) => true,
            Err(
                SettingsServiceError::Expired
                | SettingsServiceError::OutsideScope
                | SettingsServiceError::Stopped,
            ) => false,
            Err(error) => return Err(error),
        };
        if !fresh
            || Instant::now() >= pairing.deadline
            || UnixMillis::now()?.get() >= pairing.expires_at.get()
        {
            let lifecycle = pairing.cancel()?;
            self.release_pairing(&pairing)?;
            return Ok(NativePairingCreation {
                operation: record.inspection(),
                lifecycle,
                invitation: None,
            });
        }
        let lifecycle = pairing.lifecycle()?;
        Ok(NativePairingCreation {
            operation: record.inspection(),
            lifecycle,
            invitation: if lifecycle == NativePairingLifecycle::Retained {
                Some(pairing.invitation()?)
            } else {
                None
            },
        })
    }

    fn failed_claim_bookkeeping(
        &self,
        claim: &ActionClaim,
        cause: SettingsServiceError,
    ) -> SettingsServiceError {
        let repair = self.repository.finish(
            &claim.identity,
            claim.native.native_request_id,
            self.result(&claim.identity, SettingsOperationState::Unknown),
            None,
        );
        SettingsServiceError::OutcomeUnknown {
            request_id: claim.identity.binding.request_id,
            cause: Box::new(cause),
            repair_error: repair.err().map(Box::new),
        }
    }

    fn expire_unadmitted(&self) -> Result<(), SettingsServiceError> {
        let now = UnixMillis::now()?;
        for record in self.repository.pending()? {
            if record.admitted() {
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
        if matches!(caller.source, CallerSource::PairedSettings { .. }) {
            return Err(SettingsServiceError::OutsideScope);
        }
        if matches!(
            identity.origin,
            SettingsOperationOrigin::PairedSettings { .. }
                | SettingsOperationOrigin::PairedSettingsAgent { .. }
        ) {
            return Err(SettingsServiceError::OutsideScope);
        }
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
        super::actions::validate_current(&self.authority, &self.settings, identity)?;
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
            SettingsOperationPhase::InFlight { .. } | SettingsOperationPhase::Applying { .. } => {
                SettingsOperationState::Unknown
            }
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
