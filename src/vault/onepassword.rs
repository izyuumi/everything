use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::credentials::CredentialSecret;
use crate::host::settings::SettingsRepository;
use crate::protocol::UnixMillis;

use super::external::{
    MAX_CACHED_ITEMS, ProviderCli, cached_summary, forget, remember, valid_binary_path,
};
use super::item::valid_host;
use super::{
    ProviderStatus, VaultError, VaultItemKind, VaultItemMetadata, VaultItemSummary, VaultProvider,
    VaultReference,
};

const PROVIDER: &str = "1password";
const MAX_LIST_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_SMALL_BYTES: usize = 64 * 1_024;
const MAX_VALUE_BYTES: usize = 32_768;
const MAX_ACCOUNTS: usize = 32;
const MAX_TEXT_CHARS: usize = 256;
const MAX_LABEL_CHARS: usize = 128;
const MAX_SITES: usize = 16;

const OP: ProviderCli = ProviderCli {
    provider: PROVIDER,
    program: "op",
    classify,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnePasswordConnection {
    pub binary: PathBuf,
    pub account: String,
}

impl OnePasswordConnection {
    pub fn validate(&self) -> Result<(), VaultError> {
        if !valid_binary_path(&self.binary) {
            return Err(VaultError::InvalidItem(
                "the op binary must be an absolute path without relative components",
            ));
        }
        if !valid_account(&self.account) {
            return Err(VaultError::InvalidItem(
                "the 1Password account must be an account UUID",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnePasswordAccount {
    pub account_uuid: String,
    pub url: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shorthand: Option<String>,
}

impl OnePasswordAccount {
    pub fn validate(&self) -> Result<(), VaultError> {
        if !valid_account(&self.account_uuid)
            || !valid_text(&self.url)
            || !valid_text(&self.email)
            || self
                .shorthand
                .as_deref()
                .is_some_and(|value| !valid_text(value))
        {
            return Err(VaultError::InvalidItem("invalid 1Password account"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnePasswordProbe {
    pub binary: PathBuf,
    pub version: String,
    pub accounts: Vec<OnePasswordAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<OnePasswordAccount>,
}

impl OnePasswordProbe {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_binary_path(&self.binary)
            || !valid_version(&self.version)
            || self.accounts.len() > MAX_ACCOUNTS
            || self
                .accounts
                .iter()
                .chain(&self.signed_in)
                .any(|account| account.validate().is_err())
        {
            return Err("invalid 1Password connection probe");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct RawAccount {
    url: String,
    email: String,
    account_uuid: String,
    #[serde(default)]
    shorthand: Option<String>,
}

impl RawAccount {
    fn into_account(self) -> Result<OnePasswordAccount, VaultError> {
        let account = OnePasswordAccount {
            account_uuid: self.account_uuid,
            url: self.url,
            email: self.email,
            shorthand: self.shorthand.filter(|value| !value.is_empty()),
        };
        account.validate().map_err(|_| malformed())?;
        Ok(account)
    }
}

#[derive(Deserialize)]
struct RawItem {
    id: String,
    title: String,
    vault: RawVault,
    category: String,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    urls: Vec<RawUrl>,
}

#[derive(Deserialize)]
struct RawVault {
    id: String,
}

#[derive(Deserialize)]
struct RawUrl {
    href: String,
}

pub struct OnePasswordProvider {
    settings: SettingsRepository,
    locked: AtomicBool,
}

impl OnePasswordProvider {
    pub(crate) fn new(settings: SettingsRepository) -> Self {
        Self {
            settings,
            locked: AtomicBool::new(false),
        }
    }

    fn connection(&self) -> Result<OnePasswordConnection, VaultError> {
        self.settings
            .snapshot()
            .map_err(|_| VaultError::Worker)?
            .vault
            .onepassword
            .ok_or(VaultError::ProviderDisconnected(PROVIDER))
    }

    async fn op(
        &self,
        binary: &Path,
        arguments: &[&str],
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let result = OP.run(binary, arguments, &[], limit, cancellation).await;
        match &result {
            Ok(_) => self.locked.store(false, Ordering::Relaxed),
            Err(VaultError::ProviderLocked(_)) => self.locked.store(true, Ordering::Relaxed),
            Err(_) => {}
        }
        result
    }

    pub(crate) async fn connect(
        &self,
        binary: PathBuf,
        account: Option<String>,
        cancellation: CancellationToken,
    ) -> Result<OnePasswordProbe, VaultError> {
        OP.executable(&binary)?;
        let version = self
            .op(&binary, &["--version"], MAX_SMALL_BYTES, &cancellation)
            .await?;
        let version = std::str::from_utf8(&version)
            .map(str::trim)
            .ok()
            .filter(|version| valid_version(version))
            .ok_or_else(|| failed("op --version did not report a 2.x version"))?
            .to_owned();
        let output = self
            .op(
                &binary,
                &["account", "list", "--format", "json"],
                MAX_SMALL_BYTES,
                &cancellation,
            )
            .await?;
        let accounts = serde_json::from_slice::<Vec<RawAccount>>(&output)
            .map_err(|_| malformed())?
            .into_iter()
            .take(MAX_ACCOUNTS)
            .map(RawAccount::into_account)
            .collect::<Result<Vec<_>, _>>()?;
        let signed_in = match account {
            None => None,
            Some(account) => {
                if !accounts.iter().any(|listed| listed.account_uuid == account) {
                    return Err(VaultError::InvalidItem(
                        "the 1Password account is not listed by op account list",
                    ));
                }
                let output = self
                    .op(
                        &binary,
                        &["whoami", "--account", &account, "--format", "json"],
                        MAX_SMALL_BYTES,
                        &cancellation,
                    )
                    .await?;
                let signed_in = serde_json::from_slice::<RawAccount>(&output)
                    .map_err(|_| malformed())?
                    .into_account()?;
                if signed_in.account_uuid != account {
                    return Err(failed("op whoami reported a different account"));
                }
                Some(signed_in)
            }
        };
        Ok(OnePasswordProbe {
            binary,
            version,
            accounts,
            signed_in,
        })
    }

    pub(crate) async fn disconnect(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), VaultError> {
        let connection = self.connection()?;
        forget(
            super::VaultProviderId::OnePassword,
            &format!("{}/", connection.account),
        );
        self.locked.store(false, Ordering::Relaxed);
        OP.executable(&connection.binary)?;
        match self
            .op(
                &connection.binary,
                &["signout", "--account", &connection.account],
                MAX_SMALL_BYTES,
                &cancellation,
            )
            .await
        {
            Ok(_) | Err(VaultError::ProviderLocked(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl VaultProvider for OnePasswordProvider {
    async fn status(&self, _cancellation: CancellationToken) -> ProviderStatus {
        match self.connection() {
            Err(VaultError::ProviderDisconnected(_)) => ProviderStatus::Disconnected,
            Err(error) => ProviderStatus::Unavailable {
                reason: error.to_string(),
            },
            Ok(connection) => match OP.executable(&connection.binary) {
                Err(error) => ProviderStatus::Unavailable {
                    reason: error.to_string(),
                },
                Ok(()) if self.locked.load(Ordering::Relaxed) => ProviderStatus::Locked,
                Ok(()) => ProviderStatus::Ready {
                    account: format!("{} via {}", connection.account, connection.binary.display()),
                },
            },
        }
    }

    async fn search(
        &self,
        query: &str,
        kinds: &[VaultItemKind],
        limit: usize,
        cancellation: CancellationToken,
    ) -> Result<Vec<VaultItemSummary>, VaultError> {
        let connection = self.connection()?;
        OP.executable(&connection.binary)?;
        let output = self
            .op(
                &connection.binary,
                &[
                    "item",
                    "list",
                    "--account",
                    &connection.account,
                    "--format",
                    "json",
                ],
                MAX_LIST_BYTES,
                &cancellation,
            )
            .await?;
        let items: Vec<RawItem> = serde_json::from_slice(&output).map_err(|_| malformed())?;
        drop(output);
        let summaries: Vec<VaultItemSummary> = items
            .into_iter()
            .filter_map(|item| summary(&connection.account, item))
            .take(MAX_CACHED_ITEMS)
            .collect();
        remember(
            super::VaultProviderId::OnePassword,
            &format!("{}/", connection.account),
            &summaries,
        );
        Ok(summaries
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
        let connection = self.connection()?;
        let field = reference.field().ok_or(VaultError::InvalidReference)?;
        let mut segments = reference.id().split('/');
        let (Some(account), Some(vault), Some(item), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return Err(VaultError::InvalidReference);
        };
        if account != connection.account {
            return Err(VaultError::InvalidReference);
        }
        if !valid_id(vault) || !valid_id(item) {
            return Err(VaultError::InvalidReference);
        }
        if let Ok(summary) = cached_summary(reference)
            && !summary.fields.iter().any(|existing| existing == field)
        {
            return Err(VaultError::MissingField(field.to_owned()));
        }
        OP.executable(&connection.binary)?;
        let target = format!("op://{vault}/{item}/{field}");
        let value = self
            .op(
                &connection.binary,
                &[
                    "read",
                    "--no-newline",
                    "--account",
                    &connection.account,
                    &target,
                ],
                MAX_VALUE_BYTES,
                &cancellation,
            )
            .await
            .map_err(|error| match error {
                VaultError::NotFound => VaultError::MissingField(field.to_owned()),
                error => error,
            })?;
        if value.is_empty() {
            return Err(VaultError::MissingField(field.to_owned()));
        }
        Ok(CredentialSecret::new(value.to_vec())?)
    }
}

fn summary(account: &str, item: RawItem) -> Option<VaultItemSummary> {
    let (kind, fields) = category(&item.category)?;
    if !valid_id(&item.id) || !valid_id(&item.vault.id) {
        return None;
    }
    let label: String = item
        .title
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    if label.trim().is_empty() {
        return None;
    }
    let metadata = match kind {
        VaultItemKind::Login => VaultItemMetadata::Login {
            label,
            sites: sites(&item.urls),
            username: None,
        },
        VaultItemKind::ApiSecret => VaultItemMetadata::ApiSecret {
            label,
            env_name: None,
            service: None,
        },
        VaultItemKind::Card => VaultItemMetadata::Card {
            label,
            brand: None,
            last4: None,
            expiry_month: None,
            expiry_year: None,
        },
        VaultItemKind::Identity => VaultItemMetadata::Identity { label },
    };
    metadata.validate().ok()?;
    let updated_at = item
        .updated_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
        .and_then(|value| UnixMillis::try_from(value).ok())?;
    let reference = VaultReference::from_parts(
        super::VaultProviderId::OnePassword,
        &format!("{account}/{}/{}", item.vault.id, item.id),
        None,
    )
    .ok()?;
    Some(VaultItemSummary::new(
        reference,
        metadata,
        fields.iter().map(|field| (*field).to_owned()).collect(),
        updated_at,
    ))
}

fn category(value: &str) -> Option<(VaultItemKind, &'static [&'static str])> {
    Some(match value {
        "LOGIN" => (VaultItemKind::Login, &["username", "password"]),
        "PASSWORD" => (VaultItemKind::ApiSecret, &["password"]),
        "API_CREDENTIAL" => (VaultItemKind::ApiSecret, &["credential"]),
        "DATABASE" => (VaultItemKind::ApiSecret, &["username", "password"]),
        "CREDIT_CARD" => (VaultItemKind::Card, &[]),
        "IDENTITY" => (VaultItemKind::Identity, &[]),
        _ => return None,
    })
}

fn sites(urls: &[RawUrl]) -> Vec<String> {
    let mut sites = Vec::new();
    for url in urls {
        let rest = url
            .href
            .split_once("://")
            .map_or(url.href.as_str(), |(_, rest)| rest);
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host)
            .split(':')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if valid_host(&host) && !sites.contains(&host) && sites.len() < MAX_SITES {
            sites.push(host);
        }
    }
    sites
}

fn classify(text: &str) -> Option<VaultError> {
    if text.contains("signed in") || text.contains("sign in") {
        return Some(VaultError::ProviderLocked(PROVIDER));
    }
    [
        "not found",
        "cannot be found",
        "isn't an item",
        "isn't a field",
        "no item",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    .then_some(VaultError::NotFound)
}

fn failed(reason: &str) -> VaultError {
    OP.failed(reason)
}

fn malformed() -> VaultError {
    OP.malformed()
}

pub(crate) fn valid_account(value: &str) -> bool {
    (1..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn valid_id(value: &str) -> bool {
    value.len() == 26
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn valid_text(value: &str) -> bool {
    value.chars().count() <= MAX_TEXT_CHARS && !value.chars().any(char::is_control)
}

fn valid_version(value: &str) -> bool {
    value.len() <= 64
        && value.strip_prefix("2.").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
        })
}
