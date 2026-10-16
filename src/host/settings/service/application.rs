use std::sync::Arc;

use crate::host::composition::Composition;
use crate::host::memory::{EmbeddingApplyPhase, EmbeddingApplyRequest, SemanticGeneration};
use crate::protocol::CommandId;

use super::super::HostSettings;
use super::super::contract::{SettingsFailureReason, SettingsUnavailableReason};
use super::super::operations::{
    OperationIdentity, SettingsApplicationState, SettingsOperationInspection, SettingsWriteIntent,
};
use super::persistence::file_fingerprint;
use super::writes::ApplicationCapture;
use super::{ServiceCore, SettingsService, SettingsServiceError, updates};

struct ApplicationWork {
    identity: OperationIdentity,
    intent: SettingsWriteIntent,
    settings: HostSettings,
    composition: Arc<Composition>,
    semantic: Option<SemanticGeneration>,
    selection_generation: Option<u64>,
}

enum PreparedApplication {
    Finished(Box<SettingsOperationInspection>),
    Work(Box<ApplicationWork>),
}

impl SettingsService {
    pub(in crate::host) async fn pending_application_work(
        &self,
    ) -> Result<Vec<CommandId>, SettingsServiceError> {
        self.blocking(|core| {
            Ok(core
                .repository
                .pending_applications()?
                .into_iter()
                .map(|record| record.identity.binding.request_id)
                .collect())
        })
        .await
    }

    pub(in crate::host) async fn advance_application(
        &self,
        operation_id: CommandId,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        if self.core.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        let mutation = Arc::clone(&self.core.mutation).lock_owned().await;
        let prepared = self
            .blocking(move |core| {
                let _retained_mutation = mutation;
                core.prepare_application(operation_id)
            })
            .await?;
        let work = match prepared {
            PreparedApplication::Finished(result) => return Ok(*result),
            PreparedApplication::Work(work) => *work,
        };
        let Some(generation) = work.semantic.clone() else {
            return self
                .finish_application(work, SettingsApplicationState::Applied)
                .await;
        };
        let changes = work.composition.memory.embedding_changes();
        let request = EmbeddingApplyRequest {
            operation_id,
            settings_revision: work.intent.after.revision,
            request_sha256: work.identity.binding.exact_request_sha256.clone(),
            preferences: updates::embedding_preferences(&work.settings),
        };
        loop {
            let changed = changes.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let metadata = work.composition.memory.embedding_metadata()?;
            if metadata.generation != generation.generation() {
                return self.finish_application(work, failed()).await;
            }
            if let Some(application) = &metadata.application
                && application.generation == generation.generation()
            {
                if application.operation_id != operation_id
                    || application.settings_revision != work.intent.after.revision
                    || application.generation != generation.generation()
                {
                    return self.finish_application(work, failed()).await;
                }
                match application.phase {
                    EmbeddingApplyPhase::Disabled | EmbeddingApplyPhase::Ready { .. } => {
                        return self
                            .finish_application(work, SettingsApplicationState::Applied)
                            .await;
                    }
                    EmbeddingApplyPhase::Unavailable { .. } => {
                        return self
                            .finish_application(
                                work,
                                SettingsApplicationState::Unavailable {
                                    reason: SettingsUnavailableReason::EmbeddingBoundaryUnavailable,
                                },
                            )
                            .await;
                    }
                    EmbeddingApplyPhase::Failed { .. }
                    | EmbeddingApplyPhase::Superseded
                    | EmbeddingApplyPhase::Stopped => {
                        return self.finish_application(work, failed()).await;
                    }
                    EmbeddingApplyPhase::Pending { .. } => {}
                }
            }
            if metadata.application_active {
                changed.await;
                continue;
            }
            if self.core.application_stop.is_cancelled() {
                return self
                    .finish_application(
                        work,
                        SettingsApplicationState::Unavailable {
                            reason: SettingsUnavailableReason::HostOffline,
                        },
                    )
                    .await;
            }
            let mutation = Arc::clone(&self.core.mutation).lock_owned().await;
            let current = self
                .blocking({
                    let intent = work.intent.clone();
                    move |core| {
                        let retained_mutation = mutation;
                        core.require_resolved_file()?;
                        let current = core.settings.observed_snapshot()?;
                        Ok((
                            retained_mutation,
                            file_fingerprint(&current)? == intent.after,
                        ))
                    }
                })
                .await?;
            let (retained_mutation, still_current) = current;
            if !still_current || self.core.application_stop.is_cancelled() {
                drop(retained_mutation);
                return self.finish_application(work, failed()).await;
            }
            let admitted = work
                .composition
                .memory
                .apply_embeddings(generation.clone(), request.clone())
                .await;
            drop(retained_mutation);
            if let Err(cause) = admitted {
                let repair = self.finish_application(work, failed()).await;
                return match repair {
                    Ok(operation) => {
                        tracing::warn!(operation_id=%operation_id,"original settings application stopped after a typed backend admission failure");
                        Ok(operation)
                    }
                    Err(repair) => Err(SettingsServiceError::OutcomeUnknown {
                        request_id: operation_id,
                        cause: Box::new(cause.into()),
                        repair_error: Some(Box::new(repair)),
                    }),
                };
            }
        }
    }

    async fn finish_application(
        &self,
        work: ApplicationWork,
        application: SettingsApplicationState,
    ) -> Result<SettingsOperationInspection, SettingsServiceError> {
        let mutation = Arc::clone(&self.core.mutation).lock_owned().await;
        self.blocking(move |core| {
            let retained = (work, mutation);
            let work = &retained.0;
            let actual = core.settings.observed_snapshot()?;
            let application = if file_fingerprint(&actual)? != work.intent.after {
                failed()
            } else if core.application_stop.is_cancelled() {
                SettingsApplicationState::Unavailable {
                    reason: SettingsUnavailableReason::HostOffline,
                }
            } else if application == SettingsApplicationState::Applied
                && !work.application_is_current()?
            {
                failed()
            } else if application == SettingsApplicationState::Applied
                && work.intent.restart_required
            {
                SettingsApplicationState::RestartRequired
            } else {
                application
            };
            let result =
                core.repository
                    .record_application(&work.identity, &work.intent, application)?;
            core.applications
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .remove(&work.identity.binding.request_id);
            Ok(result)
        })
        .await
    }
}

impl ServiceCore {
    fn prepare_application(
        &self,
        operation_id: CommandId,
    ) -> Result<PreparedApplication, SettingsServiceError> {
        if self.application_stop.is_cancelled() {
            return Err(SettingsServiceError::Stopped);
        }
        self.require_resolved_file()?;
        let record = self.repository.get(operation_id)?;
        if record.application().is_some() {
            return Ok(PreparedApplication::Finished(Box::new(record.inspection())));
        }
        let write = record.write().ok_or(SettingsServiceError::Conflict)?;
        if write.resolution != Some(super::super::operations::SettingsWriteResolution::Applied)
            || !matches!(
                record.phase(),
                super::super::operations::SettingsOperationPhase::Finished {
                    result: super::super::contract::SettingsOperationResult {
                        state: super::super::contract::SettingsOperationState::PendingApplication { .. },
                        ..
                    }
                }
            )
        {
            return Err(SettingsServiceError::Conflict);
        }
        let current = self.settings.observed_snapshot()?;
        if file_fingerprint(&current)? != write.intent.after {
            let result =
                self.repository
                    .record_application(&record.identity, &write.intent, failed())?;
            self.applications
                .lock()
                .map_err(|_| SettingsServiceError::Poisoned)?
                .remove(&operation_id);
            return Ok(PreparedApplication::Finished(Box::new(result)));
        }
        let composition = self.composition()?;
        let existing = self
            .applications
            .lock()
            .map_err(|_| SettingsServiceError::Poisoned)?
            .get(&operation_id)
            .cloned();
        let capture = match existing {
            Some(capture) => capture,
            None => {
                let capture =
                    ApplicationCapture {
                        semantic: if write.intent.memory_changed {
                            Some(composition.memory.suspend_semantics(operation_id)?)
                        } else {
                            None
                        },
                        selection: if write.intent.selection_changed {
                            Some(composition.suspend_selection().map_err(|cause| {
                                SettingsServiceError::Application(Box::new(cause))
                            })?)
                        } else {
                            None
                        },
                        previous_selection: None,
                    };
                self.applications
                    .lock()
                    .map_err(|_| SettingsServiceError::Poisoned)?
                    .insert(operation_id, capture.clone());
                capture
            }
        };
        if write.intent.memory_changed && capture.semantic.is_none()
            || write.intent.selection_changed && capture.selection.is_none()
        {
            return Err(SettingsServiceError::CorruptJournal);
        }
        let mut selection_generation = None;
        if let Some(generation) = capture.selection {
            match composition.configure_selection(
                generation,
                current.selection.clone(),
                updates::selection_enabled(&current),
            ) {
                Ok(configuration) => selection_generation = Some(configuration.generation),
                Err(cause) => {
                    let result = self.repository.record_application(
                        &record.identity,
                        &write.intent,
                        failed(),
                    )?;
                    self.applications
                        .lock()
                        .map_err(|_| SettingsServiceError::Poisoned)?
                        .remove(&operation_id);
                    tracing::warn!(operation_id=%operation_id,"original selector application stopped after generation rejection");
                    drop(cause);
                    return Ok(PreparedApplication::Finished(Box::new(result)));
                }
            }
        }
        Ok(PreparedApplication::Work(Box::new(ApplicationWork {
            identity: record.identity.clone(),
            intent: write.intent.clone(),
            settings: current,
            composition,
            semantic: capture.semantic,
            selection_generation,
        })))
    }
}

fn failed() -> SettingsApplicationState {
    SettingsApplicationState::Failed {
        reason: SettingsFailureReason::ApplyFailed,
    }
}

impl ApplicationWork {
    fn application_is_current(&self) -> Result<bool, SettingsServiceError> {
        if let Some(generation) = &self.semantic {
            let metadata = self.composition.memory.embedding_metadata()?;
            if metadata.generation != generation.generation() {
                return Ok(false);
            }
            let Some(application) = metadata.application.as_ref().filter(|application| {
                application.operation_id == self.identity.binding.request_id
                    && application.settings_revision == self.intent.after.revision
                    && application.generation == generation.generation()
            }) else {
                return Ok(false);
            };
            let matches = match application.phase {
                EmbeddingApplyPhase::Ready { .. } => {
                    updates::embeddings_enabled(&self.settings)
                        && metadata.accepting_semantics
                        && matches!(
                            metadata.status,
                            crate::host::memory::EmbeddingStatus::Enabled { .. }
                        )
                }
                EmbeddingApplyPhase::Disabled => {
                    !updates::embeddings_enabled(&self.settings)
                        && !metadata.accepting_semantics
                        && matches!(
                            metadata.status,
                            crate::host::memory::EmbeddingStatus::Disabled
                        )
                }
                _ => false,
            };
            if !matches {
                return Ok(false);
            }
        }
        if let Some(generation) = self.selection_generation {
            let configuration = self.composition.selection.configuration_status()?;
            let expected_remote = updates::selection_enabled(&self.settings)
                && matches!(
                    self.settings.selection,
                    crate::host::selection::SelectionSettings::SystemOne { .. }
                );
            if configuration.generation != generation
                || configuration.suspended
                || configuration.settings != self.settings.selection
                || configuration.remote_enabled != expected_remote
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
