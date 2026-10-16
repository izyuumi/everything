use std::sync::Arc;

use crate::agent::{ProviderEndpoint, ProviderKind};
use crate::ipc::{OwnerOperationResult, OwnerQuery, OwnerQueryResult};
use crate::protocol::UnixMillis;
use crate::transport::local::{LocalCall, LocalError};

use super::super::runtime::{HostError, HostInner};
use super::super::settings::grants::{
    SettingsGrantChange, SettingsGrantConsent, SettingsGrantOwner, SettingsGrantState,
};

impl HostInner {
    pub(in crate::host) async fn settings_grant_query(
        self: &Arc<Self>,
        call: &LocalCall,
        query: OwnerQuery,
    ) -> Result<OwnerQueryResult, HostError> {
        query.validate().map_err(HostError::Invalid)?;
        let caller = self.settings_service.local_owner_cleanup_caller(call)?;
        let cancellation = call.cancellation.clone();
        let mutation = Arc::clone(&self.mutation).lock_owned().await;
        self.blocking(move |host| {
            let _mutation = mutation;
            if cancellation.is_cancelled() {
                return Err(LocalError::Cancelled.into());
            }
            let owner = SettingsGrantOwner {
                host_id: host.devices.public_keys().device_id,
                principal_id: host.settings_service.settings_cleanup_principal(&caller)?,
            };
            let store = host.agent.store();
            let result = match &query {
                OwnerQuery::SettingsGrantCurrent { device_id } => {
                    OwnerQueryResult::SettingsGrantCurrent {
                        head: store.settings_grant_head(owner, *device_id)?.map(Box::new),
                    }
                }
                OwnerQuery::SettingsGrantVersion {
                    device_id,
                    revision,
                } => OwnerQueryResult::SettingsGrantVersion {
                    grant: store
                        .settings_grant_version(owner, *device_id, *revision)?
                        .map(Box::new),
                },
                OwnerQuery::SettingsGrantReceipt { request_id } => {
                    OwnerQueryResult::SettingsGrantReceipt {
                        receipt: store.settings_grant_receipt(owner, *request_id)?,
                    }
                }
                _ => return Err(HostError::Invalid("invalid Settings grant query")),
            };
            result.validate_for(&query).map_err(HostError::Invalid)?;
            Ok(result)
        })
        .await
    }

    pub(in crate::host) async fn settings_grant_change(
        self: &Arc<Self>,
        call: &LocalCall,
        change: SettingsGrantChange,
    ) -> Result<OwnerOperationResult, HostError> {
        if change.request_id != call.request.request_id {
            return Err(HostError::Invalid(
                "Settings grant requires its original owner request ID",
            ));
        }
        change
            .validate()
            .map_err(|_| HostError::Invalid("invalid Settings grant change"))?;
        let caller = self.settings_service.local_owner_cleanup_caller(call)?;
        let cancellation = call.cancellation.clone();
        let mutation = Arc::clone(&self.mutation).lock_owned().await;
        self.blocking(move |host| {
            let _mutation = mutation;
            if cancellation.is_cancelled() {
                return Err(LocalError::Cancelled.into());
            }
            let owner = SettingsGrantOwner {
                host_id: host.devices.public_keys().device_id,
                principal_id: host.settings_service.settings_cleanup_principal(&caller)?,
            };
            let store = host.agent.store();
            let commit =
                if let Some(receipt) = store.settings_grant_receipt(owner, change.request_id)? {
                    store.set_settings_grant(owner, &change, receipt.committed_at)?
                } else {
                    if let SettingsGrantState::Active {
                        session_consent, ..
                    } = &change.target.state
                    {
                        host.ensure_accepting()?;
                        if let SettingsGrantConsent::ConfiguredProvider {
                            provider,
                            model,
                            recipient,
                        } = session_consent
                        {
                            let (revision, selection) =
                                host.settings_service.owner_grant_model(&caller)?;
                            let selection = selection.ok_or(HostError::Invalid(
                                "Settings grant requires an actual configured model",
                            ))?;
                            let current_recipient = match selection.endpoint.as_native() {
                                ProviderEndpoint::Loopback(endpoint) => endpoint,
                                ProviderEndpoint::Official => match selection.provider {
                                    ProviderKind::OpenAi => {
                                        "https://api.openai.com/v1/responses".into()
                                    }
                                    ProviderKind::Anthropic => {
                                        "https://api.anthropic.com/v1/messages".into()
                                    }
                                },
                            };
                            if change.configured_settings_revision != Some(revision)
                                || *provider != selection.provider
                                || *model != selection.model
                                || *recipient != current_recipient
                            {
                                return Err(HostError::Invalid(
                                    "Settings grant configured revision or recipient changed",
                                ));
                            }
                        }
                    }
                    if cancellation.is_cancelled() {
                        return Err(LocalError::Cancelled.into());
                    }
                    store.set_settings_grant(owner, &change, UnixMillis::now()?)?
                };
            if !commit.receipt.matches_change(&change) {
                return Err(HostError::Invalid("Settings grant receipt mismatch"));
            }
            Ok(OwnerOperationResult::SettingsGrantCommitted {
                receipt: commit.receipt,
                replayed: commit.replayed,
            })
        })
        .await
    }
}
