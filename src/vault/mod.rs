pub mod item;
pub mod local;
pub mod reference;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::credentials::{CredentialError, CredentialSecret, NativeCredentialStore};
use crate::storage::{StorageError, Store};

pub use item::{VaultItemKind, VaultItemMetadata, VaultItemSummary, VaultPutRequest};
pub use local::{LocalVaultProvider, VaultItemState, VaultPendingItem};
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

#[async_trait]
pub trait VaultProvider: Send + Sync {
    async fn status(&self, cancellation: CancellationToken) -> ProviderStatus;

    async fn search(
        &self,
        query: &str,
        kinds: &[VaultItemKind],
        limit: usize,
    ) -> Result<Vec<VaultItemSummary>, VaultError>;

    async fn resolve(
        &self,
        reference: &VaultReference,
        cancellation: CancellationToken,
    ) -> Result<CredentialSecret, VaultError>;
}

#[derive(Clone)]
pub struct VaultService {
    local: Arc<LocalVaultProvider>,
}

impl VaultService {
    pub(crate) fn new(store: Store, native: NativeCredentialStore) -> Self {
        Self {
            local: Arc::new(LocalVaultProvider::new(
                store,
                native,
                service_from_environment(),
            )),
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
        if cancellation.is_cancelled() {
            return Err(VaultError::Cancelled);
        }
        Ok(VaultListing {
            providers: vec![
                VaultProviderState {
                    provider: VaultProviderId::Local,
                    status: local,
                },
                not_available(VaultProviderId::OnePassword, "#220"),
                not_available(VaultProviderId::ProtonPass, "#221"),
            ],
            items: self.local.search("", &[], limit).await?,
            incomplete: self.local.incomplete(limit).await?,
        })
    }

    pub(crate) async fn put(&self, request: VaultPutRequest) -> Result<VaultReference, VaultError> {
        self.local.put(request).await
    }

    pub(crate) async fn remove(&self, id: uuid::Uuid) -> Result<(), VaultError> {
        self.local.remove(id).await
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
