use std::time::Duration;

use tokio::sync::OwnedMutexGuard;

use crate::devices::{
    AuthenticatedSettingsAccessSource, BindingGeneration, OwnedCommittedSettingsAccess,
    SettingsRemoteDenial, SettingsRemoteOperation, SettingsRemoteReply, SettingsRemoteRequest,
};
use crate::host::settings::contract::{SETTINGS_CONTRACT_VERSION, SettingsPatch, SettingsScope};
use crate::host::settings::grants::SettingsGrantReference;
use crate::host::settings::{SettingsGrantConsent, SettingsGrantHead, SettingsGrantState};
use crate::storage::Store;

use super::access::{AccessDecision, SettingsAccessQualificationError};
use super::session::SettingsRemoteSessionPlan;
use super::{SettingsReceiptAuthority, SettingsServiceError};

const MAX_REMOTE_CHANGE_BYTES: usize = 24 * 1024;
const SESSION_PUBLICATION_MARGIN: Duration = Duration::from_secs(2);

pub(crate) struct SettingsRemotePayload {
    pub(super) generation: BindingGeneration,
    pub(super) reply: SettingsRemoteReply,
}

pub(crate) enum SettingsRemotePreparation {
    Reply(Option<SettingsRemotePayload>),
    Session(Box<SettingsRemoteSessionPlan>),
}

impl SettingsReceiptAuthority {
    pub(crate) fn prepare_remote_reply(
        &self,
        store: &Store,
        mutation: &OwnedMutexGuard<()>,
        request: &OwnedCommittedSettingsAccess,
    ) -> Result<SettingsRemotePreparation, SettingsAccessQualificationError> {
        let source = request.verified_request().authority_source();
        let intent = source.intent();
        let Some(requested) = intent.request.generation() else {
            return Ok(SettingsRemotePreparation::Reply(None));
        };
        let observed = {
            let connection = store.connection().map_err(SettingsServiceError::from)?;
            self.observe_access_reply_on(store, &connection, mutation, request)?
        };
        let AccessDecision::Active(head) = &observed.status.decision else {
            self.core
                .authority
                .retire_paired_sessions(source.verified_peer_keys().device_id)?;
            return Ok(SettingsRemotePreparation::Reply(None));
        };
        let generation = remote_generation(source, head);
        let SettingsGrantState::Active {
            readable_keys,
            writable_keys,
            session_consent,
        } = &head.grant.change.target.state
        else {
            return Err(SettingsServiceError::OutsideScope.into());
        };
        let reply = if *requested != generation {
            SettingsRemoteReply::Denied(SettingsRemoteDenial::GrantChanged)
        } else {
            match &intent.request {
                SettingsRemoteRequest::SessionOpen { .. }
                | SettingsRemoteRequest::SessionSend { .. }
                | SettingsRemoteRequest::SessionPoll { .. }
                | SettingsRemoteRequest::SessionClose { .. } => match session_consent {
                    SettingsGrantConsent::ConfiguredProvider {
                        provider,
                        model,
                        recipient,
                    } => {
                        return Ok(SettingsRemotePreparation::Session(Box::new(
                            SettingsRemoteSessionPlan {
                                generation,
                                peer: source.verified_peer_keys().clone(),
                                grant: SettingsGrantReference {
                                    grant_id: head.grant.change.target.grant_id,
                                    revision: head.grant.revision,
                                },
                                grant_expires_at: head.grant.change.target.expires_at,
                                provider: *provider,
                                model: model.clone(),
                                recipient: recipient.clone(),
                                request: intent.request.clone(),
                                deadline: source
                                    .original_deadline()
                                    .checked_sub(SESSION_PUBLICATION_MARGIN)
                                    .ok_or(SettingsServiceError::Expired)?,
                            },
                        )));
                    }
                    SettingsGrantConsent::None => {
                        SettingsRemoteReply::Denied(SettingsRemoteDenial::OutsideScope)
                    }
                },
                SettingsRemoteRequest::Snapshot { keys, .. } => {
                    if keys.iter().all(|key| readable_keys.contains(key)) {
                        match self.core.host_snapshot(SettingsScope::Host {
                            host_id: self.core.authority.host_id,
                        }) {
                            Ok(mut snapshot) => {
                                snapshot.entries.retain(|entry| keys.contains(&entry.key));
                                SettingsRemoteReply::Snapshot(snapshot)
                            }
                            Err(error) => SettingsRemoteReply::Denied(remote_denial(&error)),
                        }
                    } else {
                        SettingsRemoteReply::Denied(SettingsRemoteDenial::OutsideScope)
                    }
                }
                SettingsRemoteRequest::Changes {
                    after_sequence,
                    limit,
                    ..
                } => {
                    let caller = self.core.authority.paired_caller(
                        &self.host,
                        source,
                        &self.core.application_stop,
                    )?;
                    let page = self.core.change_page(
                        &caller,
                        Some(*after_sequence),
                        *limit,
                        Some(readable_keys.as_slice()),
                        MAX_REMOTE_CHANGE_BYTES,
                    );
                    self.core.authority.retire_paired_caller(&caller);
                    match page {
                        Ok(page) => SettingsRemoteReply::Changes(page),
                        Err(error) => SettingsRemoteReply::Denied(remote_denial(&error)),
                    }
                }
                SettingsRemoteRequest::Update {
                    request_id,
                    expected_revision,
                    changes,
                    ..
                } => {
                    if changes
                        .iter()
                        .all(|change| writable_keys.contains(&change.key))
                    {
                        let caller = self.core.authority.paired_caller(
                            &self.host,
                            source,
                            &self.core.application_stop,
                        )?;
                        let patch = SettingsPatch {
                            contract_version: SETTINGS_CONTRACT_VERSION,
                            request_id: *request_id,
                            scope: SettingsScope::Host {
                                host_id: self.core.authority.host_id,
                            },
                            expected_revision: *expected_revision,
                            changes: changes.clone(),
                            confirmation: None,
                        };
                        let result = self.core.paired_update(
                            &caller,
                            patch,
                            mutation,
                            self.core.application_stop.child_token(),
                        );
                        self.core.authority.retire_paired_caller(&caller);
                        match result {
                            Ok(inspection) => SettingsRemoteReply::Update {
                                request_id: *request_id,
                                operation: SettingsRemoteOperation::from_inspection(&inspection),
                            },
                            Err(error) => SettingsRemoteReply::Denied(remote_denial(&error)),
                        }
                    } else {
                        SettingsRemoteReply::Denied(SettingsRemoteDenial::OutsideScope)
                    }
                }
                SettingsRemoteRequest::AccessStatus | SettingsRemoteRequest::HostAction { .. } => {
                    return Ok(SettingsRemotePreparation::Reply(None));
                }
            }
        };
        Ok(SettingsRemotePreparation::Reply(Some(
            SettingsRemotePayload { generation, reply },
        )))
    }
}

pub(super) fn remote_generation(
    source: &AuthenticatedSettingsAccessSource,
    head: &SettingsGrantHead,
) -> BindingGeneration {
    BindingGeneration {
        grant_id: head.grant.change.target.grant_id,
        grant_revision: head.grant.revision,
        host_epoch: source.connection().host_identity().host_epoch(),
        connection_epoch: source.connection().connection_epoch(),
    }
}

pub(super) fn remote_denial(error: &SettingsServiceError) -> SettingsRemoteDenial {
    match error {
        SettingsServiceError::Conflict => SettingsRemoteDenial::Conflict,
        SettingsServiceError::OutsideScope => SettingsRemoteDenial::OutsideScope,
        SettingsServiceError::WriteBlocked { .. } => SettingsRemoteDenial::WriteBlocked,
        SettingsServiceError::Capacity => SettingsRemoteDenial::Capacity,
        SettingsServiceError::Invalid(_)
        | SettingsServiceError::Contract(_)
        | SettingsServiceError::EmbeddingMetadataUnavailable => SettingsRemoteDenial::Invalid,
        SettingsServiceError::OutcomeUnknown { .. } => SettingsRemoteDenial::OutcomeUnknown,
        _ => SettingsRemoteDenial::Unavailable,
    }
}
