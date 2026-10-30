use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::credentials::CredentialSecret;
use crate::host::credentials::SecretTransfer;
use crate::host::settings::SettingsRepository;
use crate::protocol::UnixMillis;

use super::external::{
    MAX_CACHED_ITEMS, ProviderCli, cached_summary, forget, remember, valid_binary_path,
};
use super::{
    LocalVaultProvider, ProviderStatus, VaultError, VaultItemKind, VaultItemMetadata,
    VaultItemSummary, VaultProvider, VaultProviderId, VaultPutRequest, VaultReference,
};

const PROVIDER: &str = "protonpass";
const TOKEN_ENVIRONMENT: &str = "PROTON_PASS_PERSONAL_ACCESS_TOKEN";
const TOKEN_LABEL: &str = "Proton Pass personal access token";
const MAX_LIST_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_SMALL_BYTES: usize = 64 * 1_024;
const MAX_VALUE_BYTES: usize = 32_769;
const MAX_VAULTS: usize = 32;
const MAX_TEXT_CHARS: usize = 256;
const MAX_LABEL_CHARS: usize = 128;

const PASS: ProviderCli = ProviderCli {
    provider: PROVIDER,
    program: "pass-cli",
    classify,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtonPassConnection {
    pub binary: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<VaultReference>,
}

impl ProtonPassConnection {
    pub fn validate(&self) -> Result<(), VaultError> {
        if !valid_binary_path(&self.binary) {
            return Err(VaultError::InvalidItem(
                "the pass-cli binary must be an absolute path without relative components",
            ));
        }
        if self
            .token
            .as_ref()
            .is_some_and(|token| token.local_id().is_err() || token.field().is_some())
        {
            return Err(VaultError::InvalidItem(
                "the Proton Pass token must be a local vault item reference",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtonPassProbe {
    pub binary: PathBuf,
    pub version: String,
    pub session_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_in: Option<String>,
}

impl ProtonPassProbe {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_binary_path(&self.binary)
            || !valid_binary_path(&self.session_dir)
            || !valid_version(&self.version)
            || self
                .signed_in
                .as_deref()
                .is_some_and(|account| !valid_text(account))
        {
            return Err("invalid Proton Pass connection probe");
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct RawInfo {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    personal_access_token_name: Option<String>,
}

#[derive(Deserialize)]
struct RawVaults {
    vaults: Vec<RawVault>,
}

#[derive(Deserialize)]
struct RawVault {
    share_id: String,
}

#[derive(Deserialize)]
struct RawItems {
    items: Vec<RawItem>,
}

#[derive(Deserialize)]
struct RawItem {
    id: String,
    share_id: String,
    title: String,
    item_type: String,
}

pub struct ProtonPassProvider {
    settings: SettingsRepository,
    local: Arc<LocalVaultProvider>,
    session_dir: PathBuf,
    login: tokio::sync::Mutex<()>,
}

impl ProtonPassProvider {
    pub(crate) fn new(
        settings: SettingsRepository,
        local: Arc<LocalVaultProvider>,
        session_dir: PathBuf,
    ) -> Self {
        Self {
            settings,
            local,
            session_dir,
            login: tokio::sync::Mutex::new(()),
        }
    }

    fn connection(&self) -> Result<ProtonPassConnection, VaultError> {
        self.settings
            .snapshot()
            .map_err(|_| VaultError::Worker)?
            .vault
            .protonpass
            .ok_or(VaultError::ProviderDisconnected(PROVIDER))
    }

    fn session(&self) -> Result<&Path, VaultError> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let parent = self
            .session_dir
            .parent()
            .ok_or_else(|| PASS.failed("the Proton Pass session directory has no parent"))?;
        for directory in [parent, self.session_dir.as_path()] {
            match std::fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(
                        PASS.failed("the Proton Pass session directory is not a plain directory")
                    );
                }
                Err(_) => std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(directory)
                    .map_err(|_| {
                        PASS.failed("the Proton Pass session directory could not be created")
                    })?,
            }
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| PASS.failed("the Proton Pass session directory is not owner-only"))?;
        }
        Ok(&self.session_dir)
    }

    async fn run(
        &self,
        binary: &Path,
        arguments: &[&str],
        token: Option<&CredentialSecret>,
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        let environment = child_environment(self.session()?);
        let mut pairs: Vec<(&str, &OsStr)> = environment
            .iter()
            .map(|(key, value)| (*key, value.as_os_str()))
            .collect();
        if let Some(token) = token {
            pairs.push((TOKEN_ENVIRONMENT, OsStr::from_bytes(token.expose())));
        }
        PASS.run(binary, arguments, &pairs, limit, cancellation)
            .await
    }

    async fn pass(
        &self,
        connection: &ProtonPassConnection,
        arguments: &[&str],
        limit: usize,
        cancellation: &CancellationToken,
    ) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        match self
            .run(&connection.binary, arguments, None, limit, cancellation)
            .await
        {
            Err(VaultError::ProviderLocked(_)) if connection.token.is_some() => {
                self.login(connection, cancellation).await?;
                self.run(&connection.binary, arguments, None, limit, cancellation)
                    .await
            }
            result => result,
        }
    }

    async fn login(
        &self,
        connection: &ProtonPassConnection,
        cancellation: &CancellationToken,
    ) -> Result<(), VaultError> {
        let token = connection
            .token
            .as_ref()
            .ok_or(VaultError::ProviderLocked(PROVIDER))?;
        let _login = self.login.lock().await;
        let reference = VaultReference::local(token.local_id()?, Some("value"))?;
        let secret = self
            .local
            .resolve(&reference, cancellation.clone())
            .await
            .map_err(|error| match error {
                VaultError::NotFound
                | VaultError::NotReady
                | VaultError::MissingSecret
                | VaultError::MissingField(_) => PASS.failed(
                    "the stored personal access token item is missing; run everything vault connect protonpass",
                ),
                error => error,
            })?;
        match self
            .run(
                &connection.binary,
                &["login"],
                Some(&secret),
                MAX_SMALL_BYTES,
                cancellation,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(VaultError::ProviderLocked(_) | VaultError::NotFound) => {
                Err(PASS.failed("pass-cli login with the stored personal access token failed"))
            }
            Err(error) => Err(error),
        }
    }

    async fn info(
        &self,
        connection: &ProtonPassConnection,
        cancellation: &CancellationToken,
    ) -> Result<String, VaultError> {
        let output = self
            .pass(
                connection,
                &["info", "--output", "json"],
                MAX_SMALL_BYTES,
                cancellation,
            )
            .await?;
        let info: RawInfo = serde_json::from_slice(&output).map_err(|_| PASS.malformed())?;
        let account = info
            .personal_access_token_name
            .map(|name| format!("token {name}"))
            .or(info.email)
            .or(info.username)
            .unwrap_or_else(|| "signed in".to_owned());
        Ok(clean(&account, MAX_TEXT_CHARS))
    }

    pub(crate) async fn connect(
        &self,
        binary: PathBuf,
        token: Option<SecretTransfer>,
        cancellation: CancellationToken,
    ) -> Result<(ProtonPassProbe, Option<VaultReference>), VaultError> {
        PASS.executable(&binary)?;
        let output = self
            .run(
                &binary,
                &["--version"],
                None,
                MAX_SMALL_BYTES,
                &cancellation,
            )
            .await?;
        let version = std::str::from_utf8(&output)
            .ok()
            .and_then(|text| {
                text.split_whitespace()
                    .find(|word| word.starts_with(|character: char| character.is_ascii_digit()))
            })
            .filter(|version| valid_version(version))
            .ok_or_else(|| PASS.failed("pass-cli --version did not report a version"))?
            .to_owned();
        let session_dir = self.session()?.to_path_buf();
        let token = match token {
            None => None,
            Some(token) => Some(
                self.local
                    .put(VaultPutRequest {
                        metadata: VaultItemMetadata::ApiSecret {
                            label: TOKEN_LABEL.to_owned(),
                            env_name: None,
                            service: Some(PROVIDER.to_owned()),
                        },
                        secrets: BTreeMap::from([("value".to_owned(), token)]),
                    })
                    .await?,
            ),
        };
        let connection = ProtonPassConnection {
            binary: binary.clone(),
            token: token.clone(),
        };
        let signed_in = match &token {
            Some(_) => match self.login(&connection, &cancellation).await {
                Ok(()) => self.info(&connection, &cancellation).await.map(Some),
                Err(error) => Err(error),
            },
            None => match self.info(&connection, &cancellation).await {
                Ok(account) => Ok(Some(account)),
                Err(VaultError::ProviderLocked(_)) => Ok(None),
                Err(error) => Err(error),
            },
        };
        let signed_in = match signed_in {
            Ok(signed_in) => signed_in,
            Err(error) => {
                if let Some(token) = &token {
                    self.forget_token(token).await;
                }
                return Err(error);
            }
        };
        Ok((
            ProtonPassProbe {
                binary,
                version,
                session_dir,
                signed_in,
            },
            token,
        ))
    }

    pub(crate) async fn forget_token(&self, token: &VaultReference) {
        let removed = match token.local_id() {
            Ok(id) => self.local.remove(id).await,
            Err(error) => Err(error),
        };
        if let Err(error) = removed {
            tracing::warn!(error = %error, "the Proton Pass token item could not be removed");
        }
    }

    pub(crate) async fn disconnect(
        &self,
        cancellation: CancellationToken,
    ) -> Result<(), VaultError> {
        let connection = self.connection()?;
        forget(VaultProviderId::ProtonPass, "");
        let result = match PASS.executable(&connection.binary) {
            Ok(()) => self
                .run(
                    &connection.binary,
                    &["logout"],
                    None,
                    MAX_SMALL_BYTES,
                    &cancellation,
                )
                .await
                .map(drop),
            Err(error) => Err(error),
        };
        if let Some(token) = &connection.token {
            self.forget_token(token).await;
        }
        match result {
            Ok(()) | Err(VaultError::ProviderLocked(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl VaultProvider for ProtonPassProvider {
    async fn status(&self, cancellation: CancellationToken) -> ProviderStatus {
        let connection = match self.connection() {
            Ok(connection) => connection,
            Err(VaultError::ProviderDisconnected(_)) => return ProviderStatus::Disconnected,
            Err(error) => {
                return ProviderStatus::Unavailable {
                    reason: error.to_string(),
                };
            }
        };
        let account = match PASS.executable(&connection.binary) {
            Ok(()) => self.info(&connection, &cancellation).await,
            Err(error) => Err(error),
        };
        match account {
            Ok(account) => ProviderStatus::Ready {
                account: format!("{account} via {}", connection.binary.display()),
            },
            Err(VaultError::ProviderLocked(_)) => ProviderStatus::Locked,
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
        cancellation: CancellationToken,
    ) -> Result<Vec<VaultItemSummary>, VaultError> {
        let connection = self.connection()?;
        PASS.executable(&connection.binary)?;
        let output = self
            .pass(
                &connection,
                &["vault", "list", "--output", "json"],
                MAX_LIST_BYTES,
                &cancellation,
            )
            .await?;
        let vaults: RawVaults = serde_json::from_slice(&output).map_err(|_| PASS.malformed())?;
        drop(output);
        let updated_at = UnixMillis::now().map_err(|_| VaultError::Worker)?;
        let mut summaries: Vec<VaultItemSummary> = Vec::new();
        for vault in vaults.vaults.into_iter().take(MAX_VAULTS) {
            if summaries.len() >= MAX_CACHED_ITEMS {
                break;
            }
            if VaultReference::from_parts(
                VaultProviderId::ProtonPass,
                &format!("{}/item", vault.share_id),
                None,
            )
            .is_err()
            {
                continue;
            }
            let share = format!("--share-id={}", vault.share_id);
            let output = self
                .pass(
                    &connection,
                    &["item", "list", &share, "--output", "json"],
                    MAX_LIST_BYTES,
                    &cancellation,
                )
                .await?;
            let items: RawItems = serde_json::from_slice(&output).map_err(|_| PASS.malformed())?;
            drop(output);
            summaries.extend(
                items
                    .items
                    .into_iter()
                    .filter_map(|item| summary(item, updated_at)),
            );
        }
        summaries.truncate(MAX_CACHED_ITEMS);
        remember(VaultProviderId::ProtonPass, "", &summaries);
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
        let (share, item) = reference
            .id()
            .split_once('/')
            .ok_or(VaultError::InvalidReference)?;
        if let Ok(summary) = cached_summary(reference)
            && !summary.fields.iter().any(|existing| existing == field)
        {
            return Err(VaultError::MissingField(field.to_owned()));
        }
        PASS.executable(&connection.binary)?;
        let target = format!("pass://{share}/{item}/{field}");
        let mut value = self
            .pass(
                &connection,
                &["item", "view", &target],
                MAX_VALUE_BYTES,
                &cancellation,
            )
            .await
            .map_err(|error| match error {
                VaultError::NotFound => VaultError::MissingField(field.to_owned()),
                error => error,
            })?;
        if value.last() == Some(&b'\n') {
            value.pop();
        }
        if value.is_empty() {
            return Err(VaultError::MissingField(field.to_owned()));
        }
        Ok(CredentialSecret::new(std::mem::take(&mut *value))?)
    }
}

pub fn child_environment(session_dir: &Path) -> Vec<(&'static str, OsString)> {
    let mut environment = vec![
        (
            "PROTON_PASS_SESSION_DIR",
            session_dir.as_os_str().to_owned(),
        ),
        ("PROTON_PASS_DISABLE_TELEMETRY", OsString::from("1")),
        ("PROTON_PASS_NO_UPDATE_CHECK", OsString::from("1")),
    ];
    if cfg!(target_os = "linux") {
        for key in ["PROTON_PASS_LINUX_KEYRING", "DBUS_SESSION_BUS_ADDRESS"] {
            if let Some(value) = std::env::var_os(key) {
                environment.push((key, value));
            }
        }
    }
    environment
}

fn summary(item: RawItem, updated_at: UnixMillis) -> Option<VaultItemSummary> {
    let reference = VaultReference::from_parts(
        VaultProviderId::ProtonPass,
        &format!("{}/{}", item.share_id, item.id),
        None,
    )
    .ok()?;
    let label = clean(&item.title, MAX_LABEL_CHARS);
    if label.trim().is_empty() {
        return None;
    }
    let (metadata, fields): (VaultItemMetadata, &[&str]) = match item.item_type.as_str() {
        "login" => (
            VaultItemMetadata::Login {
                label,
                sites: Vec::new(),
                username: None,
            },
            &["password", "totp", "username", "email"],
        ),
        "credit_card" => (
            VaultItemMetadata::Card {
                label,
                brand: None,
                last4: None,
                expiry_month: None,
                expiry_year: None,
            },
            &["number", "verification_number", "cardholder_name"],
        ),
        "identity" => (
            VaultItemMetadata::Identity { label },
            &["email", "phone_number"],
        ),
        _ => return None,
    };
    metadata.validate().ok()?;
    Some(VaultItemSummary::new(
        reference,
        metadata,
        fields.iter().map(|field| (*field).to_owned()).collect(),
        updated_at,
    ))
}

fn classify(text: &str) -> Option<VaultError> {
    if ["requires an authenticated client", "not logged in"]
        .iter()
        .any(|needle| text.contains(needle))
    {
        return Some(VaultError::ProviderLocked(PROVIDER));
    }
    ["does not exist", "not found"]
        .iter()
        .any(|needle| text.contains(needle))
        .then_some(VaultError::NotFound)
}

fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

fn valid_text(value: &str) -> bool {
    value.chars().count() <= MAX_TEXT_CHARS && !value.chars().any(char::is_control)
}

fn valid_version(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
}
