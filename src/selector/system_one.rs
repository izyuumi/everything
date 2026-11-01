use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    SelectedTool, SelectionMethod, SelectorError, ToolSelection, ToolSelector, require_discovery,
    selection, sort_candidates, validate_query,
};
use crate::credentials::{CredentialError, CredentialKey, CredentialSecret, NativeCredentialStore};
use crate::permissions::ExecutionAuthority;
use crate::tools::RetainedToolOwner;

const MAX_CONFIG_BYTES: usize = 16 * 1_024;
const MAX_REQUEST_BYTES: usize = 512 * 1_024;
const MAX_RESPONSE_BYTES: usize = 64 * 1_024;
const MAX_CANDIDATES: usize = 254;
const MAX_APPROVAL: Duration = Duration::from_secs(300);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api/v1";
const CHAT_COMPLETIONS_SUFFIX: &[u8] = b"/chat/completions";
const INSTRUCTIONS: &str = "Which tool is most relevant to the task? Rank only by relevance; execution requires independent permission.";
const CHAT_INSTRUCTIONS: &str = "Which tool is most relevant to the task? Rank only by relevance; execution requires independent permission. The user message is JSON with the task and the options keyed by option ID. Answer with only a JSON object {\"choice\": ID} where ID is exactly one option key, or \"none\" if no option is relevant.";

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationError {
    #[error("selection configuration exceeds 16 KiB")]
    TooLarge,
    #[error("invalid selection TOML or unsupported field; credentials belong in the native store")]
    InvalidDocument,
    #[error(
        "selection provider must be local, system-one, typesafe, openrouter, openai-compatible or openai_compatible, local-model or local_model and fallback must be local"
    )]
    InvalidProvider,
    #[error("system-one requires an endpoint and model")]
    MissingConfiguration,
    #[error("local selection must not contain external classifier settings")]
    UnexpectedExternalSettings,
    #[error(
        "endpoint must be HTTPS or literal-loopback HTTP without userinfo, fragment or credential query fields"
    )]
    InvalidEndpoint,
    #[error("model must contain 1 through 256 bytes without control characters")]
    InvalidModel,
    #[error("credential reference must be a UUID, never a secret")]
    InvalidCredentialReference,
    #[error(
        "chat-completions providers take a base URL, not the completions URL; OpenRouter requires https://openrouter.ai/api/v1 and local-model requires a literal-loopback endpoint"
    )]
    ProviderEndpoint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemOneProvider {
    #[default]
    Typesafe,
    Openrouter,
    OpenaiCompatible,
    LocalModel,
}

impl SystemOneProvider {
    pub fn is_typesafe(&self) -> bool {
        matches!(self, Self::Typesafe)
    }
}

#[derive(Clone, Default)]
pub enum SelectionConfig {
    #[default]
    Local,
    SystemOne(SystemOneSettings),
}

#[derive(Clone)]
pub struct SystemOneSettings {
    provider: SystemOneProvider,
    endpoint: Url,
    model: String,
    credential_reference: Option<Uuid>,
}

impl SelectionConfig {
    pub fn from_toml(document: &str) -> Result<Self, ConfigurationError> {
        if document.len() > MAX_CONFIG_BYTES {
            return Err(ConfigurationError::TooLarge);
        }
        let root: ConfigurationDocument =
            toml::from_str(document).map_err(|_| ConfigurationError::InvalidDocument)?;
        let raw = root.tools.selection;
        if raw.fallback != "local" {
            return Err(ConfigurationError::InvalidProvider);
        }
        match raw.provider.as_str() {
            "local" => {
                if raw.endpoint.is_some()
                    || raw.model.is_some()
                    || raw.credential_reference.is_some()
                {
                    return Err(ConfigurationError::UnexpectedExternalSettings);
                }
                Ok(Self::Local)
            }
            name => {
                let provider = match name {
                    "system-one" | "typesafe" => SystemOneProvider::Typesafe,
                    "openrouter" => SystemOneProvider::Openrouter,
                    "openai-compatible" | "openai_compatible" => {
                        SystemOneProvider::OpenaiCompatible
                    }
                    "local-model" | "local_model" => SystemOneProvider::LocalModel,
                    _ => return Err(ConfigurationError::InvalidProvider),
                };
                let endpoint = raw
                    .endpoint
                    .ok_or(ConfigurationError::MissingConfiguration)?;
                let model = raw.model.ok_or(ConfigurationError::MissingConfiguration)?;
                let reference = raw
                    .credential_reference
                    .map(|value| Uuid::parse_str(&value))
                    .transpose()
                    .map_err(|_| ConfigurationError::InvalidCredentialReference)?;
                Self::system_one(provider, &endpoint, &model, reference)
            }
        }
    }

    pub fn system_one(
        provider: SystemOneProvider,
        endpoint: &str,
        model: &str,
        credential_reference: Option<Uuid>,
    ) -> Result<Self, ConfigurationError> {
        let endpoint = validate_endpoint(endpoint)?;
        if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
            return Err(ConfigurationError::InvalidModel);
        }
        let loopback = match endpoint.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        match provider {
            SystemOneProvider::Typesafe => {}
            _ if {
                let path = endpoint.path().trim_end_matches('/').as_bytes();
                path.len()
                    .checked_sub(CHAT_COMPLETIONS_SUFFIX.len())
                    .is_some_and(|start| {
                        path[start..].eq_ignore_ascii_case(CHAT_COMPLETIONS_SUFFIX)
                    })
            } =>
            {
                return Err(ConfigurationError::ProviderEndpoint);
            }
            SystemOneProvider::Openrouter
                if endpoint.as_str().trim_end_matches('/') != OPENROUTER_ENDPOINT =>
            {
                return Err(ConfigurationError::ProviderEndpoint);
            }
            SystemOneProvider::LocalModel if !loopback => {
                return Err(ConfigurationError::ProviderEndpoint);
            }
            _ => {}
        }
        Ok(Self::SystemOne(SystemOneSettings {
            provider,
            endpoint,
            model: model.into(),
            credential_reference,
        }))
    }
}

impl SystemOneSettings {
    pub fn provider(&self) -> SystemOneProvider {
        self.provider
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn request_url(&self) -> Url {
        if self.provider.is_typesafe() {
            return self.endpoint.clone();
        }
        let mut url = self.endpoint.clone();
        let path = format!("{}/chat/completions", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn credential_key(&self) -> Result<Option<CredentialKey>, CredentialError> {
        self.credential_reference
            .map(|reference| {
                let mut digest = Sha256::new();
                digest.update(b"everything.system-one.credential.v1\0");
                digest.update(self.endpoint.as_str().as_bytes());
                digest.update([0]);
                digest.update(reference.as_bytes());
                CredentialKey::new(
                    "to.yumi.vorra.system-one",
                    &format!("{:x}", digest.finalize()),
                )
            })
            .transpose()
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ConfigurationDocument {
    tools: ToolsConfiguration,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct ToolsConfiguration {
    selection: RawSelection,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawSelection {
    provider: String,
    endpoint: Option<String>,
    model: Option<String>,
    fallback: String,
    credential_reference: Option<String>,
}

impl Default for RawSelection {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            endpoint: None,
            model: None,
            fallback: "local".into(),
            credential_reference: None,
        }
    }
}

fn validate_endpoint(value: &str) -> Result<Url, ConfigurationError> {
    if value.is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
        return Err(ConfigurationError::InvalidEndpoint);
    }
    let endpoint = Url::parse(value).map_err(|_| ConfigurationError::InvalidEndpoint)?;
    let (_, raw_rest) = value
        .split_once("://")
        .ok_or(ConfigurationError::InvalidEndpoint)?;
    let raw_authority = raw_rest.split(['/', '?', '#']).next().unwrap_or_default();
    if raw_authority.is_empty() {
        return Err(ConfigurationError::InvalidEndpoint);
    }
    let raw_host = if let Some(rest) = raw_authority.strip_prefix('[') {
        rest.split_once(']')
            .map(|(host, _)| host)
            .unwrap_or_default()
    } else {
        raw_authority.split(':').next().unwrap_or_default()
    };
    let loopback = raw_host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    if !((endpoint.scheme() == "https" && endpoint.host().is_some())
        || (endpoint.scheme() == "http" && loopback))
        || raw_authority.contains('@')
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
        || endpoint.query().is_some_and(|query| query.len() > 1_024)
    {
        return Err(ConfigurationError::InvalidEndpoint);
    }
    for (index, (key, _)) in endpoint.query_pairs().enumerate() {
        let normalized: String = key
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if index >= 32
            || matches!(
                normalized.as_str(),
                "key"
                    | "apikey"
                    | "token"
                    | "accesstoken"
                    | "auth"
                    | "authorization"
                    | "password"
                    | "secret"
                    | "clientsecret"
                    | "credential"
                    | "bearer"
                    | "apitoken"
                    | "refreshtoken"
                    | "idtoken"
                    | "sessiontoken"
                    | "passwd"
                    | "pwd"
                    | "apisecret"
                    | "credentials"
                    | "accesskey"
                    | "secretaccesskey"
                    | "awsaccesskeyid"
                    | "awssecretaccesskey"
                    | "awssessiontoken"
                    | "xapikey"
                    | "xamzcredential"
                    | "xamzsignature"
                    | "xamzsecuritytoken"
                    | "signature"
                    | "sig"
            )
        {
            return Err(ConfigurationError::InvalidEndpoint);
        }
    }
    Ok(endpoint)
}

#[derive(Debug, Error)]
pub enum SystemOneError {
    #[error("tool selection failed: {0}")]
    Selection(#[from] SelectorError),
    #[error("system-one selection was cancelled")]
    Cancelled,
    #[error("external disclosure approval must last between one nanosecond and five minutes")]
    InvalidApprovalLifetime,
    #[error("tool selection worker failed")]
    Worker,
}

#[derive(Clone)]
pub struct SystemOneSelector {
    identity: Uuid,
    local: ToolSelector,
    configuration: SelectionConfig,
    credentials: Option<NativeCredentialStore>,
    configuration_failure: Option<ConfigurationError>,
    admission: Arc<Semaphore>,
    _retained_owner: Option<RetainedToolOwner>,
}

impl SystemOneSelector {
    pub fn new(local: ToolSelector, configuration: SelectionConfig) -> Self {
        let credentials = matches!(&configuration, SelectionConfig::SystemOne(_))
            .then(NativeCredentialStore::new);
        Self {
            identity: Uuid::new_v4(),
            local,
            configuration,
            credentials,
            configuration_failure: None,
            admission: Arc::new(Semaphore::new(4)),
            _retained_owner: None,
        }
    }

    pub fn from_toml(local: ToolSelector, document: &str) -> Self {
        match SelectionConfig::from_toml(document) {
            Ok(configuration) => Self::new(local, configuration),
            Err(error) => {
                let mut selector = Self::new(local, SelectionConfig::Local);
                selector.configuration_failure = Some(error);
                selector
            }
        }
    }

    pub fn with_credential_store(mut self, store: NativeCredentialStore) -> Self {
        if matches!(self.configuration, SelectionConfig::SystemOne(_)) {
            self.credentials = Some(store);
        }
        self
    }

    pub(crate) fn with_retained_owner(mut self, owner: RetainedToolOwner) -> Self {
        self._retained_owner = Some(owner);
        self
    }

    pub(crate) fn reconfigured(
        &self,
        configuration: SelectionConfig,
        credentials: NativeCredentialStore,
    ) -> Self {
        let credentials =
            matches!(&configuration, SelectionConfig::SystemOne(_)).then_some(credentials);
        Self {
            identity: Uuid::new_v4(),
            local: self.local.clone(),
            configuration,
            credentials,
            configuration_failure: None,
            admission: Arc::clone(&self.admission),
            _retained_owner: self._retained_owner.clone(),
        }
    }

    pub fn prepare(
        &self,
        authority: &ExecutionAuthority,
        query: &str,
        limit: usize,
    ) -> Result<PreparedSelection, SystemOneError> {
        validate_query(query, limit)?;
        require_discovery(authority)?;
        if let Some(error) = self.configuration_failure {
            return self
                .fallback(authority, query, limit, &error.to_string())
                .map(PreparedSelection::Local);
        }
        let SelectionConfig::SystemOne(settings) = &self.configuration else {
            return Ok(PreparedSelection::Local(
                self.local.select_local(authority, query, limit)?,
            ));
        };
        let mut candidates = self.local.candidates(authority, query, true)?;
        let truncated = candidates.len().saturating_sub(MAX_CANDIDATES);
        candidates.truncate(MAX_CANDIDATES);
        if candidates.is_empty() {
            return Ok(PreparedSelection::Local(
                self.local.select_local(authority, query, limit)?,
            ));
        }
        let mut criteria = BTreeMap::new();
        criteria.insert("none".to_owned(), "No listed tool is relevant".to_owned());
        for (index, candidate) in candidates.iter().enumerate() {
            criteria.insert(
                format!("tool_{index:03}"),
                format!(
                    "{}: {}",
                    candidate.definition.name, candidate.definition.description
                ),
            );
        }
        let body = if settings.provider.is_typesafe() {
            json!({
                "model": settings.model,
                "state": query,
                "questions": {
                    "tool": {
                        "type": "choice",
                        "instructions": INSTRUCTIONS,
                        "criteria": criteria
                    }
                }
            })
        } else {
            let options: Vec<&String> = criteria.keys().collect();
            let task = serde_json::to_string(&json!({"task": query, "options": criteria}))
                .map_err(SelectorError::from)?;
            json!({
                "model": settings.model,
                "messages": [
                    {"role": "system", "content": CHAT_INSTRUCTIONS},
                    {"role": "user", "content": task}
                ],
                "temperature": 0,
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {
                        "name": "tool_choice",
                        "strict": true,
                        "schema": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["choice"],
                            "properties": {"choice": {"type": "string", "enum": options}}
                        }
                    }
                }
            })
        };
        let encoded = serde_json::to_vec(&body).map_err(SelectorError::from)?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return self
                .fallback(
                    authority,
                    query,
                    limit,
                    "System One request exceeds 512 KiB; used local selection",
                )
                .map(PreparedSelection::Local);
        }
        let Some(workspace) = authority.workspace_root().to_str() else {
            return self
                .fallback(
                    authority,
                    query,
                    limit,
                    "System One authority cannot be represented; used local selection",
                )
                .map(PreparedSelection::Local);
        };
        let catalog =
            serde_json::to_vec(self.local.catalog.as_ref()).map_err(SelectorError::from)?;
        let identity = json!({
            "version": 1,
            "selector": self.identity,
            "catalog_sha256": format!("{:x}", Sha256::digest(catalog)),
            "provider": settings.provider,
            "endpoint": settings.endpoint.as_str(),
            "request_url": settings.request_url().as_str(),
            "model": settings.model,
            "body": body,
            "authority": {
                "conversation_id": authority.conversation_id(),
                "task_id": authority.task_id(),
                "workspace": workspace,
                "mode": authority.mode(),
                "allowed_tools": authority.allowed_tools(),
            },
            "limit": limit,
            "credential_reference": settings.credential_reference,
        });
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&identity).map_err(SelectorError::from)?)
        );
        Ok(PreparedSelection::Disclosure(Box::new(
            SystemOneDisclosure {
                selector_identity: self.identity,
                settings: settings.clone(),
                body,
                encoded,
                fingerprint,
                authority: authority.clone(),
                query: query.into(),
                candidates,
                limit,
                truncated,
            },
        )))
    }

    pub async fn select_approved(
        &self,
        current_authority: &ExecutionAuthority,
        approved: ApprovedSelection,
        cancellation: CancellationToken,
    ) -> Result<ToolSelection, SystemOneError> {
        if cancellation.is_cancelled() {
            return Err(SystemOneError::Cancelled);
        }
        let Ok(permit) = Arc::clone(&self.admission).try_acquire_owned() else {
            require_discovery(current_authority)?;
            return Ok(selection(
                Vec::new(),
                approved.disclosure.limit,
                SelectionMethod::LocalFallback,
                vec!["System One classification is busy; local search_tools discovery remains available".into()],
            )?);
        };
        let admission = Arc::new(permit);
        let current = current_authority.clone();
        let this = self.clone();
        let worker_admission = Arc::clone(&admission);
        let (approved, valid) = tokio::task::spawn_blocking(move || {
            let _admission = worker_admission;
            let valid = this.approval_valid(&current, &approved)?;
            Ok::<_, SystemOneError>((approved, valid))
        })
        .await
        .map_err(|_| SystemOneError::Worker)??;
        if !valid {
            return self.fallback_async(Arc::clone(&admission), current_authority, &approved.disclosure, "System One disclosure approval or current authority changed; used local selection").await;
        }
        let credential_key = approved.disclosure.settings.credential_key();
        let secret = match credential_key {
            Ok(None) => None,
            Ok(Some(key)) => {
                let Some(store) = &self.credentials else {
                    return self.fallback_async(Arc::clone(&admission), current_authority, &approved.disclosure, "System One native credential store is unavailable; used local selection").await;
                };
                match store.read(key, cancellation.clone()).await {
                    Ok(Some(secret)) => Some(secret),
                    Err(CredentialError::Cancelled) => return Err(SystemOneError::Cancelled),
                    Ok(None) => return self.fallback_async(Arc::clone(&admission), current_authority, &approved.disclosure, "System One credential is missing; used local selection").await,
                    Err(_) => return self.fallback_async(Arc::clone(&admission), current_authority, &approved.disclosure, "System One native credential lookup failed or timed out; used local selection").await,
                }
            }
            Err(_) => {
                return self
                    .fallback_async(
                        Arc::clone(&admission),
                        current_authority,
                        &approved.disclosure,
                        "System One credential scope is invalid; used local selection",
                    )
                    .await;
            }
        };
        let client = match ClassificationClient::new(secret.as_ref()) {
            Ok(client) => client,
            Err(failure) => {
                return self
                    .fallback_async(
                        Arc::clone(&admission),
                        current_authority,
                        &approved.disclosure,
                        failure.message(),
                    )
                    .await;
            }
        };
        drop(secret);
        let current = current_authority.clone();
        let this = self.clone();
        let worker_admission = Arc::clone(&admission);
        let (approved, valid) = tokio::task::spawn_blocking(move || {
            let _admission = worker_admission;
            let valid = this.approval_valid(&current, &approved)?;
            Ok::<_, SystemOneError>((approved, valid))
        })
        .await
        .map_err(|_| SystemOneError::Worker)??;
        if !valid {
            return self.fallback_async(Arc::clone(&admission), current_authority, &approved.disclosure, "System One disclosure approval or visibility expired during credential lookup; used local selection").await;
        }
        if cancellation.is_cancelled() {
            return Err(SystemOneError::Cancelled);
        }
        if Instant::now() >= approved.expires_at
            || &approved.disclosure.authority != current_authority
        {
            return self
                .fallback_async(
                    Arc::clone(&admission),
                    current_authority,
                    &approved.disclosure,
                    "System One disclosure approval expired; used local selection",
                )
                .await;
        }
        let response = match client.rank(&approved.disclosure, &cancellation).await {
            Ok(response) => response,
            Err(ClassificationFailure::Cancelled) => return Err(SystemOneError::Cancelled),
            Err(failure) => {
                return self
                    .fallback_async(
                        Arc::clone(&admission),
                        current_authority,
                        &approved.disclosure,
                        failure.message(),
                    )
                    .await;
            }
        };
        let current = current_authority.clone();
        let this = self.clone();
        let worker_admission = Arc::clone(&admission);
        tokio::task::spawn_blocking(move || {
            let _admission = worker_admission;
            let disclosure = approved.disclosure;
            if current != disclosure.authority || !this.visibility_unchanged(&current, &disclosure.candidates)? {
                return this.fallback(&current, &disclosure.query, disclosure.limit, "Tool visibility changed during System One selection; used local selection");
            }
            let mut ranked = match normalize_response(response, &disclosure.candidates) {
                Ok(ranked) => ranked,
                Err(failure) => return this.fallback(&current, &disclosure.query, disclosure.limit, failure.message()),
            };
            sort_candidates(&mut ranked);
            let diagnostics = if disclosure.truncated == 0 { Vec::new() } else {
                vec![format!("System One considered 254 local candidates; {} additional tools remain discoverable with search_tools", disclosure.truncated)]
            };
            Ok(selection(ranked, disclosure.limit, SelectionMethod::SystemOne, diagnostics)?)
        }).await.map_err(|_| SystemOneError::Worker)?
    }

    fn approval_valid(
        &self,
        current: &ExecutionAuthority,
        approved: &ApprovedSelection,
    ) -> Result<bool, SystemOneError> {
        if self.identity != approved.disclosure.selector_identity
            || current != &approved.disclosure.authority
            || Instant::now() >= approved.expires_at
        {
            return Ok(false);
        }
        self.visibility_unchanged(current, &approved.disclosure.candidates)
    }

    fn visibility_unchanged(
        &self,
        current: &ExecutionAuthority,
        original: &[SelectedTool],
    ) -> Result<bool, SystemOneError> {
        require_discovery(current)?;
        let candidates = self.local.candidates(current, "", true)?;
        Ok(original.iter().all(|before| {
            candidates.iter().any(|after| {
                before.definition.name == after.definition.name
                    && before.permission == after.permission
            })
        }))
    }

    fn fallback(
        &self,
        authority: &ExecutionAuthority,
        query: &str,
        limit: usize,
        reason: &str,
    ) -> Result<ToolSelection, SystemOneError> {
        let mut result = self.local.select_local(authority, query, limit)?;
        result.method = SelectionMethod::LocalFallback;
        result.diagnostics.push(reason.into());
        Ok(result)
    }

    async fn fallback_async(
        &self,
        admission: Arc<OwnedSemaphorePermit>,
        authority: &ExecutionAuthority,
        disclosure: &SystemOneDisclosure,
        reason: &'static str,
    ) -> Result<ToolSelection, SystemOneError> {
        let this = self.clone();
        let authority = authority.clone();
        let query = disclosure.query.clone();
        let limit = disclosure.limit;
        tokio::task::spawn_blocking(move || {
            let _admission = admission;
            this.fallback(&authority, &query, limit, reason)
        })
        .await
        .map_err(|_| SystemOneError::Worker)?
    }
}

pub enum PreparedSelection {
    Local(ToolSelection),
    Disclosure(Box<SystemOneDisclosure>),
}

pub struct SystemOneDisclosure {
    selector_identity: Uuid,
    settings: SystemOneSettings,
    body: Value,
    encoded: Vec<u8>,
    fingerprint: String,
    authority: ExecutionAuthority,
    query: String,
    candidates: Vec<SelectedTool>,
    limit: usize,
    truncated: usize,
}

impl SystemOneDisclosure {
    pub fn destination(&self) -> String {
        self.settings.request_url().into()
    }
    pub fn provider(&self) -> SystemOneProvider {
        self.settings.provider
    }
    pub fn model(&self) -> &str {
        self.settings.model()
    }
    pub fn payload(&self) -> &Value {
        &self.body
    }
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    pub fn authority(&self) -> &ExecutionAuthority {
        &self.authority
    }

    pub fn approve(self, valid_for: Duration) -> Result<ApprovedSelection, SystemOneError> {
        if valid_for.is_zero() || valid_for > MAX_APPROVAL {
            return Err(SystemOneError::InvalidApprovalLifetime);
        }
        let expires_at = Instant::now()
            .checked_add(valid_for)
            .ok_or(SystemOneError::InvalidApprovalLifetime)?;
        Ok(ApprovedSelection {
            disclosure: self,
            expires_at,
        })
    }
}

pub struct ApprovedSelection {
    disclosure: SystemOneDisclosure,
    expires_at: Instant,
}

struct ClassificationClient {
    client: reqwest::Client,
    authorization: Option<HeaderValue>,
}

impl ClassificationClient {
    fn new(secret: Option<&CredentialSecret>) -> Result<Self, ClassificationFailure> {
        let authorization = secret
            .map(|secret| {
                if secret.expose().iter().any(|byte| !byte.is_ascii_graphic()) {
                    return Err(ClassificationFailure::Credential);
                }
                let mut bytes = Zeroizing::new(b"Bearer ".to_vec());
                bytes.extend_from_slice(secret.expose());
                let mut value = HeaderValue::from_bytes(&bytes)
                    .map_err(|_| ClassificationFailure::Credential)?;
                value.set_sensitive(true);
                Ok(value)
            })
            .transpose()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(3))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| ClassificationFailure::Transport)?;
        Ok(Self {
            client,
            authorization,
        })
    }

    async fn rank(
        &self,
        disclosure: &SystemOneDisclosure,
        cancellation: &CancellationToken,
    ) -> Result<ClassificationResponse, ClassificationFailure> {
        let operation = async {
            let mut request = self
                .client
                .post(disclosure.settings.request_url())
                .header(CONTENT_TYPE, "application/json")
                .body(disclosure.encoded.clone());
            if let Some(authorization) = &self.authorization {
                request = request.header(AUTHORIZATION, authorization.clone());
            }
            let response = request
                .send()
                .await
                .map_err(|_| ClassificationFailure::Transport)?;
            if !response.status().is_success() {
                return Err(ClassificationFailure::Http);
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(ClassificationFailure::ResponseLimit);
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| ClassificationFailure::Transport)?;
                if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                    return Err(ClassificationFailure::ResponseLimit);
                }
                bytes.extend_from_slice(&chunk);
            }
            if disclosure.settings.provider.is_typesafe() {
                return serde_json::from_slice(&bytes)
                    .map_err(|_| ClassificationFailure::InvalidResponse);
            }
            let response: ChatResponse = serde_json::from_slice(&bytes)
                .map_err(|_| ClassificationFailure::InvalidResponse)?;
            let [choice] = <[ChatChoice; 1]>::try_from(response.choices)
                .map_err(|_| ClassificationFailure::InvalidResponse)?;
            let answer: ChatAnswer = serde_json::from_str(
                choice
                    .message
                    .content
                    .as_deref()
                    .ok_or(ClassificationFailure::InvalidResponse)?
                    .trim(),
            )
            .map_err(|_| ClassificationFailure::InvalidResponse)?;
            Ok(ClassificationResponse {
                answers: BTreeMap::from([(
                    "tool".to_owned(),
                    ChoiceAnswer {
                        kind: "choice".into(),
                        choice: answer.choice,
                        confidence: None,
                        probabilities: None,
                    },
                )]),
            })
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ClassificationFailure::Cancelled),
            result = tokio::time::timeout(REQUEST_TIMEOUT, operation) => result.map_err(|_| ClassificationFailure::Transport)?,
        }
    }
}

#[derive(Deserialize)]
struct ClassificationResponse {
    answers: BTreeMap<String, ChoiceAnswer>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatAnswer {
    choice: String,
}

#[derive(Deserialize)]
struct ChoiceAnswer {
    #[serde(rename = "type")]
    kind: String,
    choice: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: Option<BTreeMap<String, f64>>,
}

fn normalize_response(
    response: ClassificationResponse,
    candidates: &[SelectedTool],
) -> Result<Vec<SelectedTool>, ClassificationFailure> {
    if response.answers.len() != 1 {
        return Err(ClassificationFailure::InvalidResponse);
    }
    let answer = response
        .answers
        .get("tool")
        .ok_or(ClassificationFailure::InvalidResponse)?;
    let allowed: BTreeSet<String> = (0..candidates.len())
        .map(|index| format!("tool_{index:03}"))
        .chain(std::iter::once("none".into()))
        .collect();
    if answer.kind != "choice"
        || !allowed.contains(&answer.choice)
        || answer
            .confidence
            .is_some_and(|value| !unit_probability(value))
    {
        return Err(ClassificationFailure::InvalidResponse);
    }
    if let Some(probabilities) = &answer.probabilities {
        if probabilities.keys().cloned().collect::<BTreeSet<_>>() != allowed
            || probabilities
                .values()
                .any(|value| !unit_probability(*value))
            || (probabilities.values().sum::<f64>() - 1.0).abs() > 0.02
        {
            return Err(ClassificationFailure::InvalidResponse);
        }
        let chosen = probabilities
            .get(&answer.choice)
            .ok_or(ClassificationFailure::InvalidResponse)?;
        if probabilities
            .values()
            .any(|probability| probability > &(chosen + 1e-9))
            || answer
                .confidence
                .is_some_and(|value| (value - chosen).abs() > 0.02)
        {
            return Err(ClassificationFailure::InvalidResponse);
        }
        let none = probabilities
            .get("none")
            .ok_or(ClassificationFailure::InvalidResponse)?;
        Ok(candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                let probability = *probabilities.get(&format!("tool_{index:03}"))?;
                if probability <= *none || probability == 0.0 {
                    return None;
                }
                let mut candidate = candidate.clone();
                candidate.score = probability;
                Some(candidate)
            })
            .collect())
    } else if answer.choice == "none" {
        Ok(Vec::new())
    } else {
        let index = answer
            .choice
            .strip_prefix("tool_")
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or(ClassificationFailure::InvalidResponse)?;
        let mut candidate = candidates
            .get(index)
            .cloned()
            .ok_or(ClassificationFailure::InvalidResponse)?;
        candidate.score = answer.confidence.unwrap_or(1.0);
        Ok(vec![candidate])
    }
}

fn unit_probability(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

enum ClassificationFailure {
    Cancelled,
    Credential,
    Transport,
    Http,
    ResponseLimit,
    InvalidResponse,
}

impl ClassificationFailure {
    fn message(&self) -> &'static str {
        match self {
            Self::Cancelled => "System One selection was cancelled",
            Self::Credential => "System One credential is invalid; used local selection",
            Self::Transport => "System One transport failed or timed out; used local selection",
            Self::Http => "System One rejected the request; used local selection",
            Self::ResponseLimit => "System One response exceeds 64 KiB; used local selection",
            Self::InvalidResponse => {
                "System One response is malformed or chose an unknown option; used local selection"
            }
        }
    }
}
