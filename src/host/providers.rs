use std::sync::Arc;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::credentials::CredentialError;
use crate::model::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::model::openai::{OpenAiConfig, OpenAiProvider};
use crate::model::{ModelError, ModelProvider};

use super::credentials::CredentialVault;
use super::settings::{SettingsError, SettingsRepository};

#[derive(Debug, Error)]
pub enum HostProviderError {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("the selected provider endpoint has no configured credential")]
    MissingCredential,
    #[error("the saved conversation configuration is invalid")]
    InvalidConfiguration,
    #[error("host provider settings worker failed")]
    Worker,
}

#[derive(Clone)]
pub struct HostProviderFactory {
    settings: SettingsRepository,
    credentials: CredentialVault,
}

impl HostProviderFactory {
    pub fn new(settings: SettingsRepository, credentials: CredentialVault) -> Self {
        Self {
            settings,
            credentials,
        }
    }

    pub async fn create(
        &self,
        configuration: &ConversationConfig,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn ModelProvider>, HostProviderError> {
        configuration
            .validate()
            .map_err(|_| HostProviderError::InvalidConfiguration)?;
        let repository = self.settings.clone();
        let settings = tokio::task::spawn_blocking(move || repository.snapshot())
            .await
            .map_err(|_| HostProviderError::Worker)??;
        let credential = self
            .credentials
            .resolve(configuration, &settings, cancellation.clone())
            .await?
            .ok_or(HostProviderError::MissingCredential)?;
        if cancellation.is_cancelled() {
            return Err(CredentialError::Cancelled.into());
        }
        let secret =
            std::str::from_utf8(credential.expose()).map_err(|_| CredentialError::InvalidSecret)?;
        let provider: Arc<dyn ModelProvider> = match configuration.provider {
            ProviderKind::Anthropic => {
                let mut provider = AnthropicConfig::new(secret.to_owned())?
                    .with_default_max_output_tokens(configuration.max_output_tokens)?;
                if let ProviderEndpoint::Loopback(endpoint) = &configuration.endpoint {
                    provider = provider.with_loopback_endpoint(endpoint)?;
                }
                Arc::new(AnthropicProvider::with_config(provider)?)
            }
            ProviderKind::OpenAi => {
                let mut provider = OpenAiConfig::new(secret)?;
                if let ProviderEndpoint::Loopback(endpoint) = &configuration.endpoint {
                    provider = provider.with_loopback_endpoint(endpoint)?;
                }
                Arc::new(OpenAiProvider::with_config(provider)?)
            }
        };
        Ok(provider)
    }
}
