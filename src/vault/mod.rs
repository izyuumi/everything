pub mod item;
pub mod local;
pub mod onepassword;
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
use crate::host::settings::SettingsRepository;
use crate::storage::{StorageError, Store};

pub use item::{VaultItemKind, VaultItemMetadata, VaultItemSummary, VaultPutRequest};
pub use local::{LocalVaultProvider, VaultItemState, VaultPendingItem};
pub use onepassword::{
    OnePasswordAccount, OnePasswordConnection, OnePasswordProbe, OnePasswordProvider,
};
pub use reference::{VaultProviderId, VaultReference};

pub const DEFAULT_VAULT_SERVICE: &str = "ai.everything.vault";
pub const VAULT_SERVICE_ENVIRONMENT: &str = "EVERYTHING_VAULT_SERVICE";
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
    #[error("vault provider {0} is not available yet; see {1}")]
    ProviderNotAvailable(&'static str, &'static str),
    #[error("vault provider {0} is not connected; run everything vault connect")]
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
}

impl VaultSettings {
    pub fn is_empty(&self) -> bool {
        self.onepassword.is_none()
    }

    pub fn validate(&self) -> Result<(), VaultError> {
        self.onepassword
            .as_ref()
            .map_or(Ok(()), OnePasswordConnection::validate)
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
}

impl VaultService {
    pub(crate) fn new(
        store: Store,
        native: NativeCredentialStore,
        settings: SettingsRepository,
    ) -> Self {
        Self {
            local: Arc::new(LocalVaultProvider::new(
                store,
                native,
                service_from_environment(),
            )),
            onepassword: Arc::new(OnePasswordProvider::new(settings)),
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
                not_available(VaultProviderId::ProtonPass, "#221"),
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
        let mut unavailable = vec![not_available(VaultProviderId::ProtonPass, "#221")];
        if items.len() < limit {
            let status = match self
                .onepassword
                .search(query, kinds, limit - items.len(), cancellation)
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
            unavailable.extend(status.map(|status| VaultProviderState {
                provider: VaultProviderId::OnePassword,
                status,
            }));
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
            provider => Err(unavailable_provider(provider)),
        }
    }

    pub(crate) async fn connect(
        &self,
        provider: VaultProviderId,
        binary: PathBuf,
        account: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<OnePasswordProbe, VaultError> {
        match provider {
            VaultProviderId::OnePassword => {
                self.onepassword
                    .connect(binary, account, cancellation)
                    .await
            }
            provider => Err(unavailable_provider(provider)),
        }
    }

    pub(crate) async fn disconnect(
        &self,
        provider: VaultProviderId,
        cancellation: CancellationToken,
    ) -> Result<(), VaultError> {
        match provider {
            VaultProviderId::OnePassword => self.onepassword.disconnect(cancellation).await,
            provider => Err(unavailable_provider(provider)),
        }
    }
}

pub(crate) fn item_summary(
    store: &Store,
    reference: &VaultReference,
) -> Result<VaultItemSummary, VaultError> {
    match reference.provider() {
        VaultProviderId::Local => local::ready_summary(store, reference),
        VaultProviderId::OnePassword => onepassword::cached_summary(reference),
        provider => Err(unavailable_provider(provider)),
    }
}

fn unavailable_provider(provider: VaultProviderId) -> VaultError {
    match provider {
        VaultProviderId::ProtonPass => VaultError::ProviderNotAvailable("protonpass", "#221"),
        VaultProviderId::Local | VaultProviderId::OnePassword => VaultError::InvalidReference,
    }
}

fn not_available(provider: VaultProviderId, issue: &'static str) -> VaultProviderState {
    VaultProviderState {
        provider,
        status: ProviderStatus::Unavailable {
            reason: VaultError::ProviderNotAvailable(provider.as_str(), issue).to_string(),
        },
    }
}

fn service_from_environment() -> String {
    std::env::var(VAULT_SERVICE_ENVIRONMENT)
        .ok()
        .filter(|value| {
            value
                .strip_prefix("ai.everything.vault.")
                .is_some_and(|suffix| {
                    (1..=64).contains(&suffix.len())
                        && suffix.bytes().all(|byte| {
                            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                        })
                })
        })
        .unwrap_or_else(|| DEFAULT_VAULT_SERVICE.to_owned())
}
