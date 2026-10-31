pub(crate) mod external;
pub mod item;
pub mod local;
pub mod onepassword;
pub mod protonpass;
pub mod reference;
pub(crate) mod task;
pub(crate) mod tools;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::credentials::{CredentialError, CredentialSecret, NativeCredentialStore};
use crate::host::credentials::SecretTransfer;
use crate::host::settings::SettingsRepository;
use crate::storage::{StorageError, Store};

pub use item::{VaultItemKind, VaultItemMetadata, VaultItemSummary, VaultPutRequest};
pub use local::{LocalVaultProvider, VaultItemState, VaultPendingItem};
pub use onepassword::{
    OnePasswordAccount, OnePasswordConnection, OnePasswordProbe, OnePasswordProvider,
};
pub use protonpass::{ProtonPassConnection, ProtonPassProbe, ProtonPassProvider};
pub use reference::{VaultProviderId, VaultReference};

pub const DEFAULT_VAULT_SERVICE: &str = "to.yumi.vorra.vault";
pub const VAULT_SERVICE_ENVIRONMENT: &str = "VORRA_VAULT_SERVICE";
pub const MAX_VAULT_LIST: usize = 100;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("invalid vault reference")]
    InvalidReference,
    #[error("invalid vault item: {0}")]
    InvalidItem(&'static str),
    #[error("invalid vault item metadata field {0}")]
    InvalidMetadata(&'static str),
    #[error("vault item was not found")]
    NotFound,
    #[error("vault item is not ready")]
    NotReady,
    #[error("vault item secret is missing from the native credential store")]
    MissingSecret,
    #[error("vault item has no {0} field")]
    MissingField(String),
    #[error("vault provider {0} is not connected; run vorra vault connect")]
    ProviderDisconnected(&'static str),
    #[error("vault provider {0} is locked or not signed in; approve the sign-in and retry")]
    ProviderLocked(&'static str),
    #[error("vault provider {0} failed: {1}")]
    ProviderFailed(&'static str, String),
    #[error("vault index reached its item limit")]
    Capacity,
    #[error("vault index record is malformed")]
    Malformed,
    #[error("vault operation was cancelled")]
    Cancelled,
    #[error("vault worker failed")]
    Worker,
    #[error(
        "vault values for this task were discarded after an internal failure; call vault_use again in a new task"
    )]
    TaskDiscarded,
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<rusqlite::Error> for VaultError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderStatus {
    Disconnected,
    Locked,
    Ready { account: String },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultProviderState {
    pub provider: VaultProviderId,
    pub status: ProviderStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultListing {
    pub providers: Vec<VaultProviderState>,
    pub items: Vec<VaultItemSummary>,
    pub incomplete: Vec<VaultPendingItem>,
}

impl VaultListing {
    pub fn validate_for(&self, limit: usize) -> Result<(), &'static str> {
        if self.items.len() > limit || self.incomplete.len() > limit {
            return Err("vault listing exceeds its requested limit");
        }
        if self
            .items
            .iter()
            .any(|item| item.reference.field().is_some() || item.metadata.validate().is_err())
        {
            return Err("invalid vault item summary");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VaultSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onepassword: Option<OnePasswordConnection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protonpass: Option<ProtonPassConnection>,
}

impl VaultSettings {
    pub fn is_empty(&self) -> bool {
        self.onepassword.is_none() && self.protonpass.is_none()
    }

    pub fn validate(&self) -> Result<(), VaultError> {
        self.onepassword
            .as_ref()
            .map_or(Ok(()), OnePasswordConnection::validate)?;
        self.protonpass
            .as_ref()
            .map_or(Ok(()), ProtonPassConnection::validate)
    }
}

#[async_trait]
pub trait VaultProvider: Send + Sync {
    async fn status(&self, cancellation: CancellationToken) -> ProviderStatus;

    async fn search(
        &self,
        query: &str,
        kinds: &[VaultItemKind],
        limit: usize,
        cancellation: CancellationToken,
    ) -> Result<Vec<VaultItemSummary>, VaultError>;

    async fn resolve(
        &self,
        reference: &VaultReference,
        cancellation: CancellationToken,
    ) -> Result<CredentialSecret, VaultError>;
}

pub(crate) struct VaultSearch {
    pub(crate) items: Vec<VaultItemSummary>,
    pub(crate) unavailable: Vec<VaultProviderState>,
}

#[derive(Clone)]
pub struct VaultService {
    local: Arc<LocalVaultProvider>,
    onepassword: Arc<OnePasswordProvider>,
    protonpass: Arc<ProtonPassProvider>,
}

impl VaultService {
    pub(crate) fn new(
        store: Store,
        native: NativeCredentialStore,
        settings: SettingsRepository,
        protonpass_session: PathBuf,
    ) -> Self {
        let local = Arc::new(LocalVaultProvider::new(
            store,
            native,
            service_from_environment(),
        ));
        Self {
            onepassword: Arc::new(OnePasswordProvider::new(settings.clone())),
            protonpass: Arc::new(ProtonPassProvider::new(
                settings,
                Arc::clone(&local),
                protonpass_session,
            )),
            local,
        }
    }

    pub(crate) async fn list(
        &self,
        limit: usize,
        cancellation: CancellationToken,
    ) -> Result<VaultListing, VaultError> {
        if !(1..=MAX_VAULT_LIST).contains(&limit) {
            return Err(VaultError::InvalidItem(
                "vault list limit must be between one and one hundred",
            ));
        }
        let local = self.local.status(cancellation.clone()).await;
        let onepassword = self.onepassword.status(cancellation.clone()).await;
        let protonpass = self.protonpass.status(cancellation.clone()).await;
        if cancellation.is_cancelled() {
            return Err(VaultError::Cancelled);
        }
        Ok(VaultListing {
            providers: vec![
                VaultProviderState {
                    provider: VaultProviderId::Local,
                    status: local,
                },
                VaultProviderState {
                    provider: VaultProviderId::OnePassword,
                    status: onepassword,
                },
                VaultProviderState {
                    provider: VaultProviderId::ProtonPass,
                    status: protonpass,
                },
            ],
            items: self.local.search("", &[], limit, cancellation).await?,
            incomplete: self.local.incomplete(limit).await?,
        })
    }

    pub(crate) async fn put(&self, request: VaultPutRequest) -> Result<VaultReference, VaultError> {
        self.local.put(request).await
    }

    pub(crate) async fn remove(&self, id: uuid::Uuid) -> Result<(), VaultError> {
        self.local.remove(id).await
    }

    pub(crate) async fn search_local(
        &self,
        query: &str,
        kinds: &[VaultItemKind],
        limit: usize,
    ) -> Result<Vec<VaultItemSummary>, VaultError> {
        self.local
            .search(query, kinds, limit, CancellationToken::new())
            .await
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        kinds: &[VaultItemKind],
        limit: usize,
        cancellation: CancellationToken,
    ) -> Result<VaultSearch, VaultError> {
        let mut items = self.search_local(query, kinds, limit).await?;
        let mut unavailable = Vec::new();
        let providers: [(VaultProviderId, &dyn VaultProvider); 2] = [
            (VaultProviderId::OnePassword, self.onepassword.as_ref()),
            (VaultProviderId::ProtonPass, self.protonpass.as_ref()),
        ];
        for (provider, external) in providers {
            if items.len() >= limit {
                break;
            }
            let status = match external
                .search(query, kinds, limit - items.len(), cancellation.clone())
                .await
            {
                Ok(found) => {
                    items.extend(found);
                    None
                }
                Err(VaultError::ProviderDisconnected(_)) => Some(ProviderStatus::Disconnected),
                Err(VaultError::ProviderLocked(_)) => Some(ProviderStatus::Locked),
                Err(VaultError::Cancelled) => return Err(VaultError::Cancelled),
                Err(error) => Some(ProviderStatus::Unavailable {
                    reason: error.to_string(),
                }),
            };
            unavailable.extend(status.map(|status| VaultProviderState { provider, status }));
        }
        Ok(VaultSearch { items, unavailable })
    }

    pub(crate) async fn resolve(
        &self,
        reference: &VaultReference,
        cancellation: CancellationToken,
    ) -> Result<CredentialSecret, VaultError> {
        match reference.provider() {
            VaultProviderId::Local => self.local.resolve(reference, cancellation).await,
            VaultProviderId::OnePassword => self.onepassword.resolve(reference, cancellation).await,
            VaultProviderId::ProtonPass => self.protonpass.resolve(reference, cancellation).await,
        }
    }

    pub(crate) async fn connect_onepassword(
        &self,
        binary: PathBuf,
        account: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<OnePasswordProbe, VaultError> {
        self.onepassword
            .connect(binary, account, cancellation)
            .await
    }

    pub(crate) async fn connect_protonpass(
        &self,
        binary: PathBuf,
        token: Option<SecretTransfer>,
        cancellation: CancellationToken,
    ) -> Result<(ProtonPassProbe, Option<VaultReference>), VaultError> {
        self.protonpass.connect(binary, token, cancellation).await
    }

    pub(crate) async fn forget_protonpass_token(&self, token: &VaultReference) {
        self.protonpass.forget_token(token).await;
    }

    pub(crate) async fn disconnect(
        &self,
        provider: VaultProviderId,
        cancellation: CancellationToken,
    ) -> Result<(), VaultError> {
        match provider {
            VaultProviderId::OnePassword => self.onepassword.disconnect(cancellation).await,
            VaultProviderId::ProtonPass => self.protonpass.disconnect(cancellation).await,
            VaultProviderId::Local => Err(VaultError::InvalidReference),
        }
    }
}

pub(crate) fn item_summary(
    store: &Store,
    reference: &VaultReference,
) -> Result<VaultItemSummary, VaultError> {
    match reference.provider() {
        VaultProviderId::Local => local::ready_summary(store, reference),
        VaultProviderId::OnePassword | VaultProviderId::ProtonPass => {
            external::cached_summary(reference)
        }
    }
}

fn service_from_environment() -> String {
    std::env::var(VAULT_SERVICE_ENVIRONMENT)
        .ok()
        .filter(|value| {
            value
                .strip_prefix("to.yumi.vorra.vault.")
                .is_some_and(|suffix| {
                    (1..=64).contains(&suffix.len())
                        && suffix.bytes().all(|byte| {
                            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                        })
                })
        })
        .unwrap_or_else(|| DEFAULT_VAULT_SERVICE.to_owned())
}
