use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
#[cfg(unix)]
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
#[cfg(unix)]
use zeroize::Zeroizing;

use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::credentials::{CredentialError, CredentialSecret, NativeCredentialStore};

use super::mcp::{
    McpEnvironmentBinding, McpEnvironmentResolver, McpLaunchAuthorization, McpSettings,
};
use super::settings::{CredentialStorage, HostSettings, ModelCredentialReference};

const MAX_SESSION_CREDENTIALS: usize = 128;
const MAX_MODEL_SESSION_CREDENTIALS: usize = 16;
#[cfg(unix)]
const MAX_BOOTSTRAP_FILE_BYTES: usize = 64 * 1_024;
const MAX_BOOTSTRAP_SECRET_BYTES: usize = 256 * 1_024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSessionBootstrap {
    pub bindings: Vec<McpSessionEnvironmentSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSessionEnvironmentSource {
    pub binding: McpEnvironmentBinding,
    pub source_environment_variable: String,
}

#[derive(Debug, Error)]
pub enum McpSessionBootstrapError {
    #[error("invalid MCP session bootstrap: {0}")]
    Invalid(&'static str),
    #[error("MCP session binding file must be an owner-controlled regular single-link file")]
    File,
    #[error("MCP session binding file exceeds 64 KiB")]
    FileTooLarge,
    #[error("MCP session binding file is malformed")]
    Malformed,
    #[error("MCP session binding does not match an authorized server manifest")]
    Unauthorized,
    #[error("an explicitly named MCP source environment variable is missing")]
    MissingEnvironment,
    #[error("MCP session environment values exceed 256 KiB")]
    SecretCapacity,
    #[error("MCP session bootstrap worker failed")]
    Worker,
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

#[derive(Clone)]
pub struct SecretTransfer(Arc<CredentialSecret>);

impl SecretTransfer {
    pub fn new(value: String) -> Result<Self, CredentialError> {
        printable_secret(value.into_bytes()).map(|secret| Self(Arc::new(secret)))
    }
}

impl fmt::Debug for SecretTransfer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretTransfer([redacted])")
    }
}

impl Serialize for SecretTransfer {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = std::str::from_utf8(self.0.expose()).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(text)
    }
}

impl<'de> Deserialize<'de> for SecretTransfer {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialOperation {
    Store {
        binding: ModelCredentialReference,
        secret: SecretTransfer,
    },
    Remove {
        binding: ModelCredentialReference,
    },
    StoreMcp {
        binding: McpEnvironmentBinding,
        secret: SecretTransfer,
    },
    RemoveMcp {
        binding: McpEnvironmentBinding,
    },
}

struct EnvironmentCredential {
    provider: ProviderKind,
    secret: Result<Arc<CredentialSecret>, CredentialError>,
}

#[derive(Clone)]
pub struct CredentialVault {
    native: NativeCredentialStore,
    session: Arc<Mutex<BTreeMap<String, Arc<CredentialSecret>>>>,
    environment: Arc<Vec<EnvironmentCredential>>,
}

impl CredentialVault {
    pub fn from_environment() -> Self {
        let mut environment = Vec::new();
        for (provider, variable) in [
            (ProviderKind::Anthropic, "ANTHROPIC_API_KEY"),
            (ProviderKind::OpenAi, "OPENAI_API_KEY"),
        ] {
            match std::env::var(variable) {
                Ok(value) => environment.push(EnvironmentCredential {
                    provider,
                    secret: SecretTransfer::new(value).map(|secret| secret.0),
                }),
                Err(std::env::VarError::NotPresent) => {}
                Err(std::env::VarError::NotUnicode(_)) => environment.push(EnvironmentCredential {
                    provider,
                    secret: Err(CredentialError::InvalidSecret),
                }),
            }
        }
        Self {
            native: NativeCredentialStore::new(),
            session: Arc::new(Mutex::new(BTreeMap::new())),
            environment: Arc::new(environment),
        }
    }

    pub async fn bootstrap_mcp_session_from_file(
        &self,
        path: PathBuf,
        settings: McpSettings,
        cancellation: CancellationToken,
    ) -> Result<usize, McpSessionBootstrapError> {
        let token = cancellation.child_token();
        let _cancel_on_drop = token.clone().drop_guard();
        if token.is_cancelled() {
            return Err(CredentialError::Cancelled.into());
        }
        let vault = self.clone();
        tokio::task::spawn_blocking(move || {
            if token.is_cancelled() {
                return Err(CredentialError::Cancelled.into());
            }
            let bootstrap = read_mcp_bootstrap(path)?;
            let sources = validate_mcp_bootstrap(bootstrap, &settings)?;
            let mut staged = BTreeMap::new();
            let mut secret_bytes = 0_usize;
            for (key, source) in sources {
                if token.is_cancelled() {
                    return Err(CredentialError::Cancelled.into());
                }
                let value =
                    std::env::var_os(source).ok_or(McpSessionBootstrapError::MissingEnvironment)?;
                let secret = printable_secret(value.into_encoded_bytes())?;
                secret_bytes = secret_bytes
                    .checked_add(secret.expose().len())
                    .filter(|total| *total <= MAX_BOOTSTRAP_SECRET_BYTES)
                    .ok_or(McpSessionBootstrapError::SecretCapacity)?;
                staged.insert(key, Arc::new(secret));
            }
            let mut session = vault
                .session
                .lock()
                .map_err(|_| CredentialError::Unavailable)?;
            if token.is_cancelled() {
                return Err(CredentialError::Cancelled.into());
            }
            let additions = staged
                .keys()
                .filter(|key| !session.contains_key(*key))
                .count();
            if session
                .len()
                .checked_add(additions)
                .is_none_or(|total| total > MAX_SESSION_CREDENTIALS)
            {
                return Err(CredentialError::Busy.into());
            }
            let count = staged.len();
            session.extend(staged);
            Ok(count)
        })
        .await
        .map_err(|_| McpSessionBootstrapError::Worker)?
    }

    pub async fn apply(&self, operation: CredentialOperation) -> Result<(), CredentialError> {
        match &operation {
            CredentialOperation::Store { binding, .. }
            | CredentialOperation::Remove { binding } => {
                binding
                    .validate()
                    .map_err(|_| CredentialError::InvalidKey)?;
            }
            CredentialOperation::StoreMcp { binding, .. }
            | CredentialOperation::RemoveMcp { binding } => {
                binding.scope_key()?;
            }
        }
        match operation {
            CredentialOperation::Store { binding, secret } => match binding.storage {
                CredentialStorage::Session => {
                    self.store_session(format!("model:{}", binding.scope_key()), secret.0, true)
                }
                CredentialStorage::Native => {
                    self.native
                        .store(
                            binding.credential_key()?,
                            CredentialSecret::new(secret.0.expose().to_vec())?,
                        )
                        .await
                }
            },
            CredentialOperation::Remove { binding } => match binding.storage {
                CredentialStorage::Session => {
                    self.session
                        .lock()
                        .map_err(|_| CredentialError::Unavailable)?
                        .remove(&format!("model:{}", binding.scope_key()));
                    Ok(())
                }
                CredentialStorage::Native => self.native.delete(binding.credential_key()?).await,
            },
            CredentialOperation::StoreMcp { binding, secret } => match binding.storage {
                CredentialStorage::Session => {
                    self.store_session(format!("mcp:{}", binding.scope_key()?), secret.0, false)
                }
                CredentialStorage::Native => {
                    self.native
                        .store(
                            binding.credential_key()?,
                            CredentialSecret::new(secret.0.expose().to_vec())?,
                        )
                        .await
                }
            },
            CredentialOperation::RemoveMcp { binding } => match binding.storage {
                CredentialStorage::Session => {
                    self.session
                        .lock()
                        .map_err(|_| CredentialError::Unavailable)?
                        .remove(&format!("mcp:{}", binding.scope_key()?));
                    Ok(())
                }
                CredentialStorage::Native => self.native.delete(binding.credential_key()?).await,
            },
        }
    }

    pub fn native_store(&self) -> NativeCredentialStore {
        self.native.clone()
    }

    pub(in crate::host) fn store_settings_credential(
        &self,
        binding: &super::settings::contract::SettingsCredentialBinding,
        secret: SecretTransfer,
    ) -> Result<(), CredentialError> {
        match binding {
            super::settings::contract::SettingsCredentialBinding::Model { binding } => {
                let binding = binding.as_native();
                binding
                    .validate()
                    .map_err(|_| CredentialError::InvalidKey)?;
                match binding.storage {
                    CredentialStorage::Session => {
                        self.store_session(format!("model:{}", binding.scope_key()), secret.0, true)
                    }
                    CredentialStorage::Native => self.native.store_blocking(
                        binding.credential_key()?,
                        CredentialSecret::new(secret.0.expose().to_vec())?,
                    ),
                }
            }
            super::settings::contract::SettingsCredentialBinding::SystemOne {
                endpoint,
                reference,
            } => {
                if reference.is_nil() {
                    return Err(CredentialError::InvalidKey);
                }
                let configuration = crate::selector::system_one::SelectionConfig::system_one(
                    endpoint,
                    "credential-binding",
                    Some(*reference),
                )
                .map_err(|_| CredentialError::InvalidKey)?;
                let crate::selector::system_one::SelectionConfig::SystemOne(configuration) =
                    configuration
                else {
                    return Err(CredentialError::InvalidKey);
                };
                let key = configuration
                    .credential_key()?
                    .ok_or(CredentialError::InvalidKey)?;
                self.native
                    .store_blocking(key, CredentialSecret::new(secret.0.expose().to_vec())?)
            }
        }
    }

    fn store_session(
        &self,
        key: String,
        secret: Arc<CredentialSecret>,
        model: bool,
    ) -> Result<(), CredentialError> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| CredentialError::Unavailable)?;
        if !session.contains_key(&key)
            && (session.len() >= MAX_SESSION_CREDENTIALS
                || model
                    && session
                        .keys()
                        .filter(|key| key.starts_with("model:"))
                        .count()
                        >= MAX_MODEL_SESSION_CREDENTIALS)
        {
            return Err(CredentialError::Busy);
        }
        session.insert(key, secret);
        Ok(())
    }

    pub async fn resolve(
        &self,
        configuration: &ConversationConfig,
        settings: &HostSettings,
        cancellation: CancellationToken,
    ) -> Result<Option<Arc<CredentialSecret>>, CredentialError> {
        if cancellation.is_cancelled() {
            return Err(CredentialError::Cancelled);
        }
        let Some(binding) = settings.credential_for(configuration) else {
            if configuration.endpoint != ProviderEndpoint::Official {
                return Ok(None);
            }
            return self
                .environment
                .iter()
                .find(|entry| entry.provider == configuration.provider)
                .map(|entry| {
                    entry
                        .secret
                        .as_ref()
                        .map(Arc::clone)
                        .map_err(|error| *error)
                })
                .transpose();
        };
        match binding.storage {
            CredentialStorage::Session => Ok(self
                .session
                .lock()
                .map_err(|_| CredentialError::Unavailable)?
                .get(&format!("model:{}", binding.scope_key()))
                .cloned()),
            CredentialStorage::Native => self
                .native
                .read(binding.credential_key()?, cancellation)
                .await
                .map(|secret| secret.map(Arc::new)),
        }
    }
}

fn printable_secret(bytes: Vec<u8>) -> Result<CredentialSecret, CredentialError> {
    let secret = CredentialSecret::new(bytes)?;
    let text = std::str::from_utf8(secret.expose()).map_err(|_| CredentialError::InvalidSecret)?;
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err(CredentialError::InvalidSecret);
    }
    Ok(secret)
}

fn validate_mcp_bootstrap(
    bootstrap: McpSessionBootstrap,
    settings: &McpSettings,
) -> Result<Vec<(String, String)>, McpSessionBootstrapError> {
    settings
        .validate()
        .map_err(|_| McpSessionBootstrapError::Invalid("invalid MCP settings"))?;
    if bootstrap.bindings.len() > MAX_SESSION_CREDENTIALS {
        return Err(McpSessionBootstrapError::Invalid(
            "at most 128 bindings are supported",
        ));
    }
    let mut destinations = BTreeSet::new();
    let mut source_names = BTreeSet::new();
    let mut sources = Vec::with_capacity(bootstrap.bindings.len());
    for source in bootstrap.bindings {
        if source.binding.storage != CredentialStorage::Session {
            return Err(McpSessionBootstrapError::Invalid(
                "bootstrap requires session storage",
            ));
        }
        if !valid_source_environment_name(&source.source_environment_variable) {
            return Err(McpSessionBootstrapError::Invalid(
                "invalid source environment variable name",
            ));
        }
        let key = source.binding.scope_key()?;
        let server = settings
            .servers
            .iter()
            .find(|server| server.server_id == source.binding.server_id)
            .ok_or(McpSessionBootstrapError::Unauthorized)?;
        let expected = server
            .environment_binding(&source.binding.variable)
            .map_err(|_| McpSessionBootstrapError::Unauthorized)?;
        if source.binding != expected
            || !matches!(
                &server.launch_authorization,
                Some(McpLaunchAuthorization::FullHost { manifest_fingerprint })
                    if manifest_fingerprint == &expected.manifest_fingerprint
            )
        {
            return Err(McpSessionBootstrapError::Unauthorized);
        }
        if !destinations.insert(key.clone()) {
            return Err(McpSessionBootstrapError::Invalid(
                "duplicate destination binding",
            ));
        }
        if !source_names.insert(source.source_environment_variable.clone()) {
            return Err(McpSessionBootstrapError::Invalid(
                "source environment variables must be unique",
            ));
        }
        sources.push((format!("mcp:{key}"), source.source_environment_variable));
    }
    Ok(sources)
}

fn valid_source_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    !value.is_empty()
        && value.len() <= 128
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(unix)]
fn read_mcp_bootstrap(path: PathBuf) -> Result<McpSessionBootstrap, McpSessionBootstrapError> {
    use rustix::fs::{FileType, Mode, OFlags};

    if path.as_os_str().is_empty()
        || path.as_os_str().as_encoded_bytes().len() > 4096
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(McpSessionBootstrapError::Invalid(
            "invalid binding file path",
        ));
    }
    let descriptor = rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| McpSessionBootstrapError::File)?;
    let metadata = rustix::fs::fstat(&descriptor).map_err(|_| McpSessionBootstrapError::File)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_uid != rustix::process::geteuid().as_raw()
        || metadata.st_mode & 0o022 != 0
        || metadata.st_nlink != 1
    {
        return Err(McpSessionBootstrapError::File);
    }
    if metadata.st_size < 0 || metadata.st_size as u64 > MAX_BOOTSTRAP_FILE_BYTES as u64 {
        return Err(McpSessionBootstrapError::FileTooLarge);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    std::fs::File::from(descriptor)
        .take((MAX_BOOTSTRAP_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| McpSessionBootstrapError::File)?;
    if bytes.len() > MAX_BOOTSTRAP_FILE_BYTES {
        return Err(McpSessionBootstrapError::FileTooLarge);
    }
    serde_json::from_slice(&bytes).map_err(|_| McpSessionBootstrapError::Malformed)
}

#[cfg(not(unix))]
fn read_mcp_bootstrap(_path: PathBuf) -> Result<McpSessionBootstrap, McpSessionBootstrapError> {
    Err(CredentialError::Unsupported.into())
}

#[async_trait]
impl McpEnvironmentResolver for CredentialVault {
    async fn resolve(
        &self,
        binding: &McpEnvironmentBinding,
        cancellation: CancellationToken,
    ) -> Result<Option<Arc<CredentialSecret>>, CredentialError> {
        if cancellation.is_cancelled() {
            return Err(CredentialError::Cancelled);
        }
        let key = binding.scope_key()?;
        match binding.storage {
            CredentialStorage::Session => {
                let session = self
                    .session
                    .lock()
                    .map_err(|_| CredentialError::Unavailable)?;
                if cancellation.is_cancelled() {
                    return Err(CredentialError::Cancelled);
                }
                Ok(session.get(&format!("mcp:{key}")).cloned())
            }
            CredentialStorage::Native => self
                .native
                .read(binding.credential_key()?, cancellation)
                .await
                .map(|secret| secret.map(Arc::new)),
        }
    }
}
