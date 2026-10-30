use std::collections::BTreeMap;

use async_trait::async_trait;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::credentials::{CredentialError, CredentialKey, CredentialSecret, NativeCredentialStore};
use crate::host::credentials::SecretTransfer;
use crate::protocol::UnixMillis;
use crate::storage::Store;

use super::item::MAX_METADATA_BYTES;
use super::{
    ProviderStatus, VaultError, VaultItemKind, VaultItemMetadata, VaultItemSummary, VaultProvider,
    VaultPutRequest, VaultReference,
};

const MAX_VAULT_ITEMS: i64 = 1_024;
const MAX_PAYLOAD_BUFFER_BYTES: usize = 64 * 1_024;
const STATUS_PROBE_ACCOUNT: &str = "00000000-0000-0000-0000-000000000000";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultItemState {
    Pending,
    Ready,
    Removing,
}

impl VaultItemState {
    fn parse(value: &str) -> Result<Self, VaultError> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "removing" => Ok(Self::Removing),
            _ => Err(VaultError::Malformed),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultPendingItem {
    #[serde(rename = "ref")]
    pub reference: VaultReference,
    pub kind: VaultItemKind,
    pub label: String,
    pub state: VaultItemState,
}

pub struct LocalVaultProvider {
    store: Store,
    native: NativeCredentialStore,
    service: String,
}

impl LocalVaultProvider {
    pub(crate) fn new(store: Store, native: NativeCredentialStore, service: String) -> Self {
        Self {
            store,
            native,
            service,
        }
    }

    fn key(&self, id: uuid::Uuid) -> Result<CredentialKey, CredentialError> {
        CredentialKey::new(&self.service, &id.hyphenated().to_string())
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T, VaultError> + Send + 'static,
    ) -> Result<T, VaultError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || work(&store))
            .await
            .map_err(|_| VaultError::Worker)?
    }

    pub(crate) async fn put(&self, request: VaultPutRequest) -> Result<VaultReference, VaultError> {
        request.validate()?;
        let id = uuid::Uuid::new_v4();
        let reference = VaultReference::local(id, None)?;
        let kind = request.metadata.kind();
        let metadata =
            serde_json::to_string(&request.metadata).map_err(|_| VaultError::Malformed)?;
        if metadata.len() > MAX_METADATA_BYTES {
            return Err(VaultError::InvalidItem("metadata exceeds 4 KiB"));
        }
        let fields = serde_json::to_string(&request.secrets.keys().collect::<Vec<_>>())
            .map_err(|_| VaultError::Malformed)?;
        let mut payload = Zeroizing::new(Vec::with_capacity(MAX_PAYLOAD_BUFFER_BYTES));
        serde_json::to_writer(&mut *payload, &request.secrets)
            .map_err(|_| CredentialError::InvalidSecret)?;
        drop(request);
        let secret = CredentialSecret::new(std::mem::take(&mut *payload))?;
        let key = self.key(id)?;
        let now = millis(UnixMillis::now().map_err(|_| VaultError::Worker)?)?;
        self.blocking(move |store| {
            let mut connection = store.connection()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let count: i64 =
                transaction.query_row("SELECT count(*) FROM vault_items", [], |row| row.get(0))?;
            if count >= MAX_VAULT_ITEMS {
                return Err(VaultError::Capacity);
            }
            transaction.execute(
                "INSERT INTO vault_items (id, kind, metadata, fields, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
                params![id.hyphenated().to_string(), kind.as_str(), metadata, fields, now],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await?;
        if let Err(error) = self.native.store(key.clone(), secret).await {
            if self.native.delete(key).await.is_ok() {
                self.blocking(move |store| {
                    store.connection()?.execute(
                        "DELETE FROM vault_items WHERE id = ?1 AND state = 'pending'",
                        params![id.hyphenated().to_string()],
                    )?;
                    Ok(())
                })
                .await?;
            }
            return Err(error.into());
        }
        self.blocking(move |store| {
            let changed = store.connection()?.execute(
                "UPDATE vault_items SET state = 'ready' WHERE id = ?1 AND state = 'pending'",
                params![id.hyphenated().to_string()],
            )?;
            if changed != 1 {
                return Err(VaultError::NotFound);
            }
            Ok(())
        })
        .await?;
        Ok(reference)
    }

    pub(crate) async fn remove(&self, id: uuid::Uuid) -> Result<(), VaultError> {
        let key = self.key(id)?;
        let row = id.hyphenated().to_string();
        let marked = row.clone();
        self.blocking(move |store| {
            let changed = store.connection()?.execute(
                "UPDATE vault_items SET state = 'removing' WHERE id = ?1",
                params![marked],
            )?;
            if changed != 1 {
                return Err(VaultError::NotFound);
            }
            Ok(())
        })
        .await?;
        self.native.delete(key).await?;
        self.blocking(move |store| {
            store.connection()?.execute(
                "DELETE FROM vault_items WHERE id = ?1 AND state = 'removing'",
                params![row],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn incomplete(
        &self,
        limit: usize,
    ) -> Result<Vec<VaultPendingItem>, VaultError> {
        let limit = i64::try_from(limit).map_err(|_| VaultError::Malformed)?;
        self.blocking(move |store| {
            let connection = store.connection()?;
            let mut statement = connection.prepare(
                "SELECT id, metadata, state FROM vault_items WHERE state <> 'ready' ORDER BY updated_at DESC, id LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut items = Vec::new();
            for row in rows {
                let (id, metadata, state) = row?;
                let metadata: VaultItemMetadata =
                    serde_json::from_str(&metadata).map_err(|_| VaultError::Malformed)?;
                items.push(VaultPendingItem {
                    reference: VaultReference::local(parse_id(&id)?, None)?,
                    kind: metadata.kind(),
                    label: metadata.label().to_owned(),
                    state: VaultItemState::parse(&state)?,
                });
            }
            Ok(items)
        })
        .await
    }
}

#[async_trait]
impl VaultProvider for LocalVaultProvider {
    async fn status(&self, cancellation: CancellationToken) -> ProviderStatus {
        let probe = match CredentialKey::new(&self.service, STATUS_PROBE_ACCOUNT) {
            Ok(key) => self.native.read(key, cancellation).await.map(|_| ()),
            Err(error) => Err(error),
        };
        match probe {
            Ok(()) => ProviderStatus::Ready {
                account: self.service.clone(),
            },
            Err(error) => ProviderStatus::Unavailable {
                reason: error.to_string(),
            },
        }
    }

    async fn search(
        &self,
        query: &str,
        kinds: &[VaultItemKind],
        limit: usize,
    ) -> Result<Vec<VaultItemSummary>, VaultError> {
        let rows = self
            .blocking(move |store| {
                let connection = store.connection()?;
                let mut statement = connection.prepare(
                    "SELECT id, kind, metadata, fields, updated_at FROM vault_items WHERE state = 'ready' ORDER BY updated_at DESC, id LIMIT ?1",
                )?;
                let rows = statement.query_map(params![MAX_VAULT_ITEMS], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })?;
                let mut summaries = Vec::new();
                for row in rows {
                    let (id, kind, metadata, fields, updated_at) = row?;
                    let metadata: VaultItemMetadata =
                        serde_json::from_str(&metadata).map_err(|_| VaultError::Malformed)?;
                    let fields: Vec<String> =
                        serde_json::from_str(&fields).map_err(|_| VaultError::Malformed)?;
                    if VaultItemKind::parse(&kind) != Some(metadata.kind())
                        || !fields
                            .iter()
                            .all(|field| metadata.kind().secret_fields().contains(&field.as_str()))
                    {
                        return Err(VaultError::Malformed);
                    }
                    let updated_at = u64::try_from(updated_at)
                        .ok()
                        .and_then(|value| UnixMillis::try_from(value).ok())
                        .ok_or(VaultError::Malformed)?;
                    summaries.push(VaultItemSummary::new(
                        VaultReference::local(parse_id(&id)?, None)?,
                        metadata,
                        fields,
                        updated_at,
                    ));
                }
                Ok(summaries)
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter(|summary| summary.matches(query, kinds))
            .take(limit)
            .collect())
    }

    async fn resolve(
        &self,
        reference: &VaultReference,
        cancellation: CancellationToken,
    ) -> Result<CredentialSecret, VaultError> {
        let id = reference.local_id()?;
        let field = reference
            .field()
            .ok_or(VaultError::InvalidReference)?
            .to_owned();
        let row = id.hyphenated().to_string();
        let kind = self
            .blocking(move |store| {
                store
                    .connection()?
                    .query_row(
                        "SELECT kind, state FROM vault_items WHERE id = ?1",
                        params![row],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?
                    .ok_or(VaultError::NotFound)
            })
            .await?;
        if VaultItemState::parse(&kind.1)? != VaultItemState::Ready {
            return Err(VaultError::NotReady);
        }
        let kind = VaultItemKind::parse(&kind.0).ok_or(VaultError::Malformed)?;
        if !kind.secret_fields().contains(&field.as_str()) {
            return Err(VaultError::MissingField(field));
        }
        let payload = self
            .native
            .read(self.key(id)?, cancellation)
            .await?
            .ok_or(VaultError::MissingSecret)?;
        let mut fields: BTreeMap<String, SecretTransfer> =
            serde_json::from_slice(payload.expose()).map_err(|_| VaultError::Malformed)?;
        let value = fields
            .remove(&field)
            .ok_or(VaultError::MissingField(field))?;
        Ok(CredentialSecret::new(value.expose().to_vec())?)
    }
}

fn parse_id(value: &str) -> Result<uuid::Uuid, VaultError> {
    uuid::Uuid::parse_str(value).map_err(|_| VaultError::Malformed)
}

fn millis(value: UnixMillis) -> Result<i64, VaultError> {
    i64::try_from(value.get()).map_err(|_| VaultError::Malformed)
}
