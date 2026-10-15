use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tokio_util::sync::CancellationToken;

use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::credentials::{CredentialError, CredentialSecret, NativeCredentialStore};

use super::mcp::{McpEnvironmentBinding, McpEnvironmentResolver};
use super::settings::{CredentialStorage, HostSettings, ModelCredentialReference};

const MAX_SESSION_CREDENTIALS: usize = 128;
const MAX_MODEL_SESSION_CREDENTIALS: usize = 16;

#[derive(Clone)]
pub struct SecretTransfer(Arc<CredentialSecret>);

impl SecretTransfer {
    pub fn new(value: String) -> Result<Self, CredentialError> {
        let secret = CredentialSecret::new(value.into_bytes())?;
        let text =
            std::str::from_utf8(secret.expose()).map_err(|_| CredentialError::InvalidSecret)?;
        if text.trim().is_empty() || text.chars().any(char::is_control) {
            return Err(CredentialError::InvalidSecret);
        }
        Ok(Self(Arc::new(secret)))
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
