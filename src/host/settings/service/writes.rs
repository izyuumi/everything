use std::sync::Arc;
use std::time::Instant;

use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use crate::host::memory::SemanticGeneration;
use crate::host::selection::SelectionGeneration;
use crate::protocol::CommandId;

use super::super::HostSettings;
use super::super::actions::{
    NativeActionAdmissionResult, NativeActionInvocation, NativeHostEffectAdmission,
    OwnerReplacementIdentity, SettingsActionTarget,
};
use super::super::authority::{CallerSource, SettingsCaller};
use super::super::contract::{
    AgentMutability, SettingsOperationState, SettingsPatch, SettingsScope, SettingsToolName,
};
use super::super::operations::{
    NativeRequest, OperationIdentity, SettingsOperationInspection, SettingsOperationPhase,
    SettingsWriteIntent,
};
use super::persistence::{OwnerSettingsWriteResult, PreparedSettingsFile, file_fingerprint};
use super::{ActiveSettingsWrite, ServiceCore, SettingsService, SettingsServiceError, updates};

#[derive(Clone)]
pub(super) struct ApplicationCapture {
    pub semantic: Option<SemanticGeneration>,
    pub selection: Option<SelectionGeneration>,
    pub previous_selection: Option<(crate::host::selection::SelectionSettings, bool)>,
}

struct FileWrite {
    identity: OperationIdentity,
    prepared: PreparedSettingsFile,
    intent: SettingsWriteIntent,
    deadline: Instant,
    cancellation: CancellationToken,
    request_cancellation: CancellationToken,
    invocation: Option<Arc<super::work::InvocationWork>>,
}

enum WriteGuard<'held> {
    Ordinary(OwnedMutexGuard<()>),
    Native(NativeHostEffectAdmission),
    #[cfg(unix)]
    Held(&'held OwnedMutexGuard<()>),
}

impl SettingsService {
    pub async fn update(
        &self,
        caller: SettingsCaller,
        patch: SettingsPatch,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let work =
            self.invocation_guard(&caller, SettingsToolName::Update, Some(patch.request_id))?;
        let service = self.clone();
        tokio::spawn(async move {
            let _work = work;
            let result = service.update_owned(caller, patch, cancellation).await;
            drop(service);
            result
        })
        .await
        .map_err(|_| SettingsServiceError::Worker)?
    }

    async fn update_owned(
        &self,
        caller: SettingsCaller,
        patch: SettingsPatch,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.core
            .authority
            .require_tool(&caller, SettingsToolName::Update)?;
        let target = SettingsActionTarget::Patch(patch.clone());
        if let Err(error) = target.validate() {
            self.reject_invocation()?;
            return Err(error);
        }
        if let Err(error) = self.core.check_scope(&patch.scope) {
            self.reject_invocation()?;
            return Err(error);
        }
        if !matches!(patch.scope, SettingsScope::Host { .. }) {
            self.reject_invocation()?;
            return Err(SettingsServiceError::OutsideScope);
        }
        let mutation = self.enter(&caller, &cancellation).await?;
        let work = self.invocation.clone();
        self.blocking(move |core| {
            core.update_locked(
                &caller,
                patch,
                target,
                WriteGuard::Ordinary(mutation),
                cancellation,
                work,
            )
        })
        .await
    }

    pub async fn replace_as_owner(
        &self,
        caller: SettingsCaller,
        expected_revision: u64,
        settings: HostSettings,
        cancellation: CancellationToken,
    ) -> Result<OwnerSettingsWriteResult, SettingsServiceError> {
        let service = self.clone();
        tokio::spawn(async move {
            service
                .replace_as_owner_owned(caller, expected_revision, settings, cancellation)
                .await
        })
        .await
        .map_err(|_| SettingsServiceError::Worker)?
    }

    async fn replace_as_owner_owned(
        &self,
        caller: SettingsCaller,
        expected_revision: u64,
        settings: HostSettings,
        cancellation: CancellationToken,
    ) -> Result<OwnerSettingsWriteResult, SettingsServiceError> {
        let CallerSource::LocalOwner { request_id } = caller.source else {
            return Err(SettingsServiceError::OutsideScope);
        };
        if settings.revision != expected_revision {
            return Err(SettingsServiceError::Conflict);
        }
        let mutation = self.enter(&caller, &cancellation).await?;
        self.blocking(move |core| {
            let retained_guard = WriteGuard::Ordinary(mutation);
            core.authority.validate(&caller, false)?;
            let submitted = file_fingerprint(&settings)?;
            match core.repository.get(request_id) {
                Ok(record) => {
                    core.check_principal(&caller, &record.identity)?;
                    let SettingsActionTarget::OwnerReplacement(original) = &record.identity.target
                    else {
                        return Err(SettingsServiceError::Conflict);
                    };
                    if original.request_id != request_id
                        || original.expected_revision != expected_revision
                        || original.exact_settings_sha256 != submitted.sha256
                        || record.identity.origin != core.authority.origin(&caller)
                    {
                        return Err(SettingsServiceError::Conflict);
                    }
                    return core.owner_receipt(&record);
                }
                Err(SettingsServiceError::NotFound) => {}
                Err(error) => return Err(error),
            }
            core.require_resolved_file()?;
            settings.validate()?;
            let current = core.settings.observed_snapshot()?;
            let prepared = core.settings.prepare_file(settings, expected_revision)?;
            let difference = updates::owner_difference(&current, &prepared.candidate)?;
            let target = SettingsActionTarget::OwnerReplacement(OwnerReplacementIdentity {
                request_id,
                scope: SettingsScope::Host {
                    host_id: core.authority.host_id,
                },
                expected_revision,
                exact_settings_sha256: submitted.sha256,
                changed_keys: difference.changed_keys.clone(),
                restart_required: difference.restart_required,
            });
            let (identity, deadline) = core.operation_identity(&caller, target)?;
            if difference.protected {
                let result = core.result(&identity, SettingsOperationState::HostActionRequired);
                let record = core.repository.prepare(
                    identity,
                    SettingsOperationPhase::Finished { result },
                    || core.authority.validate(&caller, false),
                )?;
                return core.owner_receipt(&record);
            }
            let intent = SettingsWriteIntent {
                version: 1,
                before: prepared.before.clone(),
                after: prepared.after.clone(),
                native_request_id: None,
                changed_keys: difference.changed_keys,
                memory_changed: difference.memory_changed,
                selection_changed: difference.selection_changed,
                restart_required: difference.restart_required,
            };
            core.write_file(
                FileWrite {
                    identity,
                    prepared,
                    intent,
                    deadline,
                    cancellation: cancellation.child_token(),
                    request_cancellation: cancellation,
                    invocation: None,
                },
                retained_guard,
            )?;
            core.owner_receipt(&core.repository.get(request_id)?)
        })
        .await
    }

    pub(crate) async fn confirm_patch(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let service = self.clone();
        tokio::spawn(async move {
            service
                .confirm_patch_owned(caller, invocation, cancellation)
                .await
        })
        .await
        .map_err(|_| SettingsServiceError::Worker)?
    }

    async fn confirm_patch_owned(
        &self,
        caller: SettingsCaller,
        invocation: NativeActionInvocation,
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
                    if !matches!(record.identity.target, SettingsActionTarget::Patch(_))
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
        if let Some(replay) = replay {
            return Ok(replay);
        }
        let admission = match self
            .admit_native_action(caller, invocation, cancellation.clone())
            .await?
        {
            NativeActionAdmissionResult::Existing(record) => return Ok(*record),
            NativeActionAdmissionResult::Admitted(admission) => admission,
        };
        let claim = Arc::clone(&admission.claim);
        let effect = match self
            .start_host_effect(admission, cancellation.clone())
            .await
        {
            Ok(effect) => effect,
            Err(cause) => {
                return self
                    .blocking(move |core| Err(core.failed_claim_bookkeeping(&claim, cause)))
                    .await;
            }
        };
        self.blocking(move |core| {
            let outcome = (|| {
                core.require_resolved_file()?;
                let SettingsActionTarget::Patch(patch) = &effect.claim.identity.target else {
                    return Err(SettingsServiceError::OutsideScope);
                };
                let current = core.settings.observed_snapshot()?;
                let metadata = core.composition()?.memory.embedding_metadata()?;
                let candidate =
                    updates::patch_candidate(&current, patch, metadata.profile.as_ref())?;
                let prepared = core
                    .settings
                    .prepare_file(candidate, patch.expected_revision)?;
                let difference = updates::owner_difference(&current, &prepared.candidate)?;
                let mut changed_keys = patch
                    .changes
                    .iter()
                    .map(|change| change.key)
                    .collect::<Vec<_>>();
                changed_keys.sort_unstable();
                let intent = SettingsWriteIntent {
                    version: 1,
                    before: prepared.before.clone(),
                    after: prepared.after.clone(),
                    native_request_id: Some(effect.claim.native.native_request_id),
                    changed_keys,
                    memory_changed: difference.memory_changed,
                    selection_changed: difference.selection_changed,
                    restart_required: difference.restart_required,
                };
                let write = FileWrite {
                    identity: effect.claim.identity.clone(),
                    prepared,
                    intent,
                    deadline: effect.claim.deadline,
                    cancellation: effect.cancellation(),
                    request_cancellation: cancellation,
                    invocation: None,
                };
                Ok((write, effect))
            })();
            match outcome {
                Ok((write, effect)) => match core.write_file(write, WriteGuard::Native(effect)) {
                    Ok(operation) => Ok(operation),
                    Err(cause) => match core.repository.get(claim.identity.binding.request_id) {
                        Ok(record) if record.write().is_none() => {
                            Err(core.failed_claim_bookkeeping(&claim, cause))
                        }
                        Ok(_) => Err(cause),
                        Err(repair) => Err(SettingsServiceError::OutcomeUnknown {
                            request_id: claim.identity.binding.request_id,
                            cause: Box::new(cause),
                            repair_error: Some(Box::new(repair)),
                        }),
                    },
                },
                Err(cause) => Err(core.failed_claim_bookkeeping(&claim, cause)),
            }
        })
        .await
    }
}

impl ServiceCore {
    #[cfg(unix)]
    pub(super) fn paired_update(
        &self,
        caller: &SettingsCaller,
        patch: SettingsPatch,
        mutation: &OwnedMutexGuard<()>,
        cancellation: CancellationToken,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(mutation), &self.mutation) {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.authority.validate_paired_update(caller)?;
        let target = SettingsActionTarget::Patch(patch.clone());
        target.validate()?;
        self.check_scope(&patch.scope)?;
        if !matches!(patch.scope, SettingsScope::Host { .. })
            || patch.confirmation.is_some()
            || patch.changes.iter().any(|change| {
                change.key.agent_mutability() != AgentMutability::AllowlistedPreference
            })
        {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.update_locked(
            caller,
            patch,
            target,
            WriteGuard::Held(mutation),
            cancellation,
            None,
        )
    }

    fn update_locked(
        &self,
        caller: &SettingsCaller,
        patch: SettingsPatch,
        target: SettingsActionTarget,
        retained_guard: WriteGuard<'_>,
        cancellation: CancellationToken,
        work: Option<Arc<super::work::InvocationWork>>,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        self.authority.require_update(caller)?;
        if let Some(record) = self.replay_target(caller, &target)? {
            if let Some(work) = &work {
                work.recorded(record.identity.binding.request_id)?;
            }
            return Ok(record.inspection());
        }
        #[cfg(unix)]
        if let CallerSource::PairedSettings { device_id } = caller.source {
            self.repository.check_paired_capacity(device_id)?;
        }
        self.require_resolved_file()?;
        self.expire_unadmitted()?;
        let composition = self.composition()?;
        let current = self.settings.observed_snapshot()?;
        let metadata = composition.memory.embedding_metadata()?;
        if let Some(work) = &work
            && current.revision != patch.expected_revision
        {
            work.reject()?;
            return Err(SettingsServiceError::Conflict);
        }
        let candidate = updates::patch_candidate(&current, &patch, metadata.profile.as_ref())?;
        let protected = patch.changes.iter().any(|change| {
            change.key.agent_mutability() == AgentMutability::RequiresNativeConfirmation
        });
        let (identity, deadline) = self.operation_identity(caller, target)?;
        if protected {
            let phase = if matches!(caller.source, CallerSource::LocalOwner { .. }) {
                SettingsOperationPhase::Finished {
                    result: self.result(&identity, SettingsOperationState::HostActionRequired),
                }
            } else {
                self.pending_phase(&identity)
            };
            if let Some(work) = &work {
                work.attempting()?;
            }
            let record = self
                .repository
                .prepare(identity, phase, || self.authority.validate(caller, false))?;
            if let Some(work) = &work {
                work.recorded(record.identity.binding.request_id)?;
            }
            if matches!(
                record.phase(),
                SettingsOperationPhase::AwaitingConfirmation { .. }
            ) {
                self.pending
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?
                    .insert(record.identity.binding.request_id, deadline);
            }
            return Ok(record.inspection());
        }
        let prepared = self
            .settings
            .prepare_file(candidate, patch.expected_revision)?;
        let difference = updates::owner_difference(&current, &prepared.candidate)?;
        let mut changed_keys = patch
            .changes
            .iter()
            .map(|change| change.key)
            .collect::<Vec<_>>();
        changed_keys.sort_unstable();
        let intent = SettingsWriteIntent {
            version: 1,
            before: prepared.before.clone(),
            after: prepared.after.clone(),
            native_request_id: None,
            changed_keys,
            memory_changed: difference.memory_changed,
            selection_changed: difference.selection_changed,
            restart_required: difference.restart_required,
        };
        let write = FileWrite {
            identity,
            prepared,
            intent,
            deadline,
            cancellation: cancellation.child_token(),
            request_cancellation: cancellation,
            invocation: work,
        };
        self.write_file(write, retained_guard)
    }

    fn replay_target(
        &self,
        caller: &SettingsCaller,
        target: &SettingsActionTarget,
    ) -> Result<Option<super::super::operations::OperationRecord>, SettingsServiceError> {
        match self.repository.get(target.request_id()) {
            Ok(record) => {
                self.check_principal(caller, &record.identity)?;
                if record.identity.target != *target
                    || record.identity.origin != self.authority.origin(caller)
                    || record.identity.binding.client_session_id
                        != self.operation_session(caller, target)?
                {
                    return Err(SettingsServiceError::Conflict);
                }
                Ok(Some(record))
            }
            Err(SettingsServiceError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write_file(
        &self,
        write: FileWrite,
        guard: WriteGuard<'_>,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let retained = (write, guard);
        let write = &retained.0;
        match &retained.1 {
            WriteGuard::Ordinary(guard) => {
                let _ = guard;
            }
            #[cfg(unix)]
            WriteGuard::Held(guard)
                if !Arc::ptr_eq(OwnedMutexGuard::mutex(guard), &self.mutation) =>
            {
                return Err(SettingsServiceError::OutsideScope);
            }
            #[cfg(unix)]
            WriteGuard::Held(_) => {}
            WriteGuard::Native(effect) if effect.claim.identity != write.identity => {
                return Err(SettingsServiceError::Conflict);
            }
            WriteGuard::Native(_) => {}
        }
        let composition = self.composition()?;
        {
            let applications = self
                .applications
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if (write.intent.memory_changed || write.intent.selection_changed)
                && (applications.len() >= 64
                    || applications.contains_key(&write.identity.binding.request_id))
            {
                return Err(SettingsServiceError::Capacity);
            }
        }
        let active = Arc::new(ActiveSettingsWrite {
            request_id: write.identity.binding.request_id,
            client_session_id: write.identity.binding.client_session_id,
            origin: write.identity.origin,
            cancellation: write.cancellation.clone(),
        });
        {
            let mut current = self
                .active_write
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if current.is_some() {
                return Err(SettingsServiceError::Capacity);
            }
            *current = Some(Arc::clone(&active));
        }
        let check = || {
            self.check_write_origin(
                &write.identity,
                write.deadline,
                &write.cancellation,
                &write.request_cancellation,
            )
        };
        let admitted = (|| {
            if let Some(work) = &write.invocation {
                work.attempting()?;
            }
            self.repository
                .begin_write(write.identity.clone(), write.intent.clone(), check)
        })();
        if let Err(cause) = admitted {
            let mut current = self
                .active_write
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?;
            if current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &active))
            {
                current.take();
            }
            return Err(cause);
        }
        if let Some(work) = &write.invocation {
            work.recorded(write.identity.binding.request_id)?;
        }
        let mut capture = ApplicationCapture {
            semantic: None,
            selection: None,
            previous_selection: None,
        };
        let attempted = (|| {
            check()?;
            if write.intent.memory_changed {
                capture.semantic = Some(
                    composition
                        .memory
                        .suspend_semantics(write.identity.binding.request_id)?,
                );
            }
            if write.intent.selection_changed {
                let previous = composition.selection.configuration_status()?;
                capture.previous_selection = Some((
                    previous.settings,
                    previous.remote_enabled && !previous.suspended,
                ));
                capture.selection = Some(
                    composition
                        .suspend_selection()
                        .map_err(|cause| SettingsServiceError::Application(Box::new(cause)))?,
                );
            }
            self.settings.replace_checked(
                write.prepared.candidate.clone(),
                write.intent.before.revision,
                check,
            )?;
            Ok::<(), SettingsServiceError>(())
        })();
        if capture.semantic.is_some() || capture.selection.is_some() {
            self.applications
                .lock()
                .map_err(|_| SettingsServiceError::OutcomeUnknown {
                    request_id: write.identity.binding.request_id,
                    cause: Box::new(SettingsServiceError::Poisoned),
                    repair_error: None,
                })?
                .insert(write.identity.binding.request_id, capture);
        }
        let inspection = self.reconcile_file(&write.identity, &write.intent)?;
        if let Err(cause) = attempted {
            tracing::warn!(operation_id=%write.identity.binding.request_id,"settings file attempt required authoritative original-file observation");
            if matches!(
                inspection.phase,
                SettingsOperationPhase::Finished {
                    result: super::super::contract::SettingsOperationResult {
                        state: SettingsOperationState::Unknown,
                        ..
                    }
                }
            ) {
                return Err(SettingsServiceError::OutcomeUnknown {
                    request_id: write.identity.binding.request_id,
                    cause: Box::new(cause),
                    repair_error: None,
                });
            }
        }
        Ok(inspection)
    }
}

impl ServiceCore {
    pub(super) fn restore_file_admission(
        &self,
        operation_id: CommandId,
    ) -> Result<(), SettingsServiceError> {
        let capture = self
            .applications
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .get(&operation_id)
            .cloned();
        let Some(capture) = capture else {
            return Ok(());
        };
        if self.application_stop.is_cancelled() {
            self.applications
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .remove(&operation_id);
            return Ok(());
        }
        let composition = self.composition()?;
        if let Some(generation) = &capture.semantic {
            composition.memory.restore_semantics(generation)?;
            if let Some(current) = self
                .applications
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .get_mut(&operation_id)
            {
                current.semantic = None;
            }
        }
        if let Some(generation) = capture.selection {
            let (settings, enabled) = capture
                .previous_selection
                .ok_or(SettingsServiceError::CorruptJournal)?;
            composition
                .configure_selection(generation, settings, enabled)
                .map_err(|cause| SettingsServiceError::Application(Box::new(cause)))?;
            if let Some(current) = self
                .applications
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .get_mut(&operation_id)
            {
                current.selection = None;
            }
        }
        self.applications
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .remove(&operation_id);
        Ok(())
    }
}
