use std::sync::{Arc, Weak};
use std::time::Instant;

use uuid::Uuid;

use crate::agent::ProviderKind;
use crate::crypto::DevicePublicKeys;
use crate::devices::{
    BindingGeneration, SettingsRemoteChatStatus, SettingsRemoteDenial, SettingsRemoteReply,
    SettingsRemoteRequest,
};
use crate::host::runtime::HostError;
use crate::host::settings::SettingsGrantState;
use crate::host::settings::agent::{
    SETTINGS_TURN_CONFLICT, SettingsAgentCommand, SettingsAgentRuntime, SettingsAssistantReply,
    SettingsChatOpened, SettingsOpenState,
};
use crate::host::settings::contract::SettingKey;
use crate::host::settings::grants::{SettingsGrantOwner, SettingsGrantReference};
use crate::protocol::{DeviceId, UnixMillis};

use super::super::super::authority::{SettingsAgentSession, SettingsCaller};
use super::remote::remote_denial;
use super::{
    ServiceCore, SettingsReceiptAuthority, SettingsRemotePayload, SettingsService,
    SettingsServiceError,
};

pub(crate) struct SettingsRemoteSessionPlan {
    pub(super) generation: BindingGeneration,
    pub(super) peer: DevicePublicKeys,
    pub(super) grant: SettingsGrantReference,
    pub(super) grant_expires_at: UnixMillis,
    pub(super) provider: ProviderKind,
    pub(super) model: String,
    pub(super) recipient: String,
    pub(super) request: SettingsRemoteRequest,
    pub(super) deadline: Instant,
}

pub(in crate::host::settings::service) struct PairedAgentScope {
    pub(in crate::host::settings::service) readable: Vec<SettingKey>,
    pub(in crate::host::settings::service) writable: Vec<SettingKey>,
}

impl SettingsService {
    pub(crate) fn bind_settings_agents(
        &self,
        runtime: &Arc<SettingsAgentRuntime>,
    ) -> Result<(), SettingsServiceError> {
        if self.invocation.is_some() {
            return Err(SettingsServiceError::OutsideScope);
        }
        self.core
            .agents
            .set(Arc::downgrade(runtime))
            .map_err(|_| SettingsServiceError::Conflict)
    }

    pub(crate) fn retire_paired_sessions(
        &self,
        device_id: DeviceId,
    ) -> Result<(), SettingsServiceError> {
        self.core.authority.retire_paired_sessions(device_id)
    }

    pub(crate) fn begin_paired_settings_session(
        &self,
        parent: Uuid,
    ) -> Result<SettingsAgentSession, SettingsServiceError> {
        self.core.authority.begin_paired_agent(parent)
    }

    pub(crate) fn paired_settings_session(
        &self,
        session: &SettingsAgentSession,
    ) -> Result<bool, SettingsServiceError> {
        self.core.authority.paired_agent_session(session)
    }
}

impl ServiceCore {
    pub(in crate::host::settings::service) fn paired_agent_scope(
        &self,
        caller: &SettingsCaller,
    ) -> Result<Option<PairedAgentScope>, SettingsServiceError> {
        let Some((peer, grant)) = self.authority.paired_agent_grant(caller)? else {
            return Ok(None);
        };
        let head = self.agent.store().settings_grant_head(
            SettingsGrantOwner {
                host_id: self.authority.host_id,
                principal_id: self.authority.principal(),
            },
            peer.device_id,
        )?;
        let now = UnixMillis::now()?;
        let scope = head.and_then(|head| match &head.grant.change.target.state {
            SettingsGrantState::Active {
                readable_keys,
                writable_keys,
                ..
            } if !head.peer_revoked
                && !head.host_revoked
                && head.grant.change.peer_keys == peer
                && head.grant.change.target.grant_id == grant.grant_id
                && head.grant.revision == grant.revision
                && now >= head.grant.change.target.issued_at
                && now < head.grant.change.target.expires_at =>
            {
                Some(PairedAgentScope {
                    readable: readable_keys.clone(),
                    writable: writable_keys.clone(),
                })
            }
            _ => None,
        });
        match scope {
            Some(scope) => Ok(Some(scope)),
            None => {
                self.authority.retire_paired_sessions(peer.device_id)?;
                Err(SettingsServiceError::OutsideScope)
            }
        }
    }
}

impl SettingsReceiptAuthority {
    pub(crate) fn run_remote_session(
        &self,
        plan: SettingsRemoteSessionPlan,
    ) -> SettingsRemotePayload {
        let reply = match (
            self.core.agents.get().and_then(Weak::upgrade),
            tokio::runtime::Handle::try_current(),
        ) {
            (Some(runtime), Ok(handle)) => handle.block_on(async {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(plan.deadline),
                    self.session_reply(&runtime, &plan),
                )
                .await
                .unwrap_or(SettingsRemoteReply::Denied(
                    SettingsRemoteDenial::Unavailable,
                ))
            }),
            _ => SettingsRemoteReply::Denied(SettingsRemoteDenial::Unavailable),
        };
        SettingsRemotePayload {
            generation: plan.generation,
            reply,
        }
    }

    async fn session_reply(
        &self,
        runtime: &SettingsAgentRuntime,
        plan: &SettingsRemoteSessionPlan,
    ) -> SettingsRemoteReply {
        let authority = &self.core.authority;
        if let SettingsRemoteRequest::SessionOpen {
            open_request_id, ..
        } = &plan.request
        {
            let parent = match authority.paired_session_parent(
                &plan.peer,
                plan.grant,
                plan.grant_expires_at,
            ) {
                Ok(parent) => parent,
                Err(error) => return SettingsRemoteReply::Denied(remote_denial(&error)),
            };
            let chat = match runtime
                .request(SettingsAgentCommand::OpenPaired {
                    native_session_id: parent,
                    open_request_id: *open_request_id,
                })
                .await
            {
                Ok(SettingsAssistantReply::Opened { chat })
                | Ok(SettingsAssistantReply::OpenState {
                    state: SettingsOpenState::Ready { chat },
                    ..
                }) => chat,
                Ok(SettingsAssistantReply::OpenState {
                    state: SettingsOpenState::Unknown { .. },
                    ..
                }) => {
                    return SettingsRemoteReply::Denied(SettingsRemoteDenial::OutcomeUnknown);
                }
                Ok(_) => return SettingsRemoteReply::Denied(SettingsRemoteDenial::Unavailable),
                Err(error) => return SettingsRemoteReply::Denied(session_denial(&error)),
            };
            if consented(plan, &chat) {
                return SettingsRemoteReply::SessionOpened(Box::new(chat));
            }
            if runtime
                .request(SettingsAgentCommand::CloseOpen {
                    native_session_id: parent,
                    open_request_id: *open_request_id,
                })
                .await
                .is_err()
            {
                tracing::warn!("paired Settings session closure after consent mismatch failed");
            }
            return SettingsRemoteReply::Denied(SettingsRemoteDenial::OutsideScope);
        }
        let parent = match authority.current_paired_session_parent(plan.peer.device_id, plan.grant)
        {
            Ok(parent) => parent,
            Err(SettingsServiceError::NotFound) => {
                return SettingsRemoteReply::Denied(SettingsRemoteDenial::OutsideScope);
            }
            Err(error) => return SettingsRemoteReply::Denied(remote_denial(&error)),
        };
        let (command, text_offset) = match &plan.request {
            SettingsRemoteRequest::SessionSend {
                chat_id,
                turn_id,
                provider_binding_sha256,
                text,
                ..
            } => (
                SettingsAgentCommand::Send {
                    native_session_id: parent,
                    chat_id: *chat_id,
                    turn_id: *turn_id,
                    provider_binding_sha256: provider_binding_sha256.clone(),
                    text: text.clone(),
                },
                0,
            ),
            SettingsRemoteRequest::SessionPoll {
                chat_id,
                text_offset,
                ..
            } => (
                SettingsAgentCommand::Poll {
                    native_session_id: parent,
                    chat_id: *chat_id,
                },
                *text_offset,
            ),
            SettingsRemoteRequest::SessionClose { chat_id, .. } => (
                SettingsAgentCommand::Close {
                    native_session_id: parent,
                    chat_id: *chat_id,
                },
                0,
            ),
            _ => return SettingsRemoteReply::Denied(SettingsRemoteDenial::Invalid),
        };
        match runtime.request(command).await {
            Ok(SettingsAssistantReply::Accepted { chat_id, turn_id }) => {
                SettingsRemoteReply::SessionAccepted { chat_id, turn_id }
            }
            Ok(SettingsAssistantReply::Status { status }) => {
                match SettingsRemoteChatStatus::project(&status, text_offset) {
                    Some(status) => SettingsRemoteReply::SessionStatus(Box::new(status)),
                    None => SettingsRemoteReply::Denied(SettingsRemoteDenial::Invalid),
                }
            }
            Ok(_) => SettingsRemoteReply::Denied(SettingsRemoteDenial::Unavailable),
            Err(error) => SettingsRemoteReply::Denied(session_denial(&error)),
        }
    }
}

fn consented(plan: &SettingsRemoteSessionPlan, chat: &SettingsChatOpened) -> bool {
    chat.provider == plan.provider.id()
        && chat.model == plan.model
        && chat.recipient == plan.recipient
}

fn session_denial(error: &HostError) -> SettingsRemoteDenial {
    match error {
        HostError::Invalid(message) if *message == SETTINGS_TURN_CONFLICT => {
            SettingsRemoteDenial::Conflict
        }
        HostError::Invalid(_) => SettingsRemoteDenial::OutsideScope,
        _ => SettingsRemoteDenial::Unavailable,
    }
}
