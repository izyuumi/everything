use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand_core::RngCore;
use rustix::fs::{FileType, Mode, OFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::agent::{ProviderEndpoint, ProviderKind};
use crate::credentials::{CredentialError, CredentialSecret};
use crate::model::ModelError;
use crate::model::openai::OpenAiBearer;
use crate::service::StateLease;

use super::credentials::{ChatGptRecords, CredentialVault};
use super::settings::{ModelCredentialReference, ModelCredentialSource};

const ISSUER: &str = "https://auth.openai.com";
const DISCOVERY: &str = "https://auth.openai.com/.well-known/openid-configuration";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const REGISTRATION_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "Everything";
const PREFERRED_PORT: u16 = 1455;
const HOST_ID_FILE: &str = "chatgpt-host-id";
const REFRESH_MARGIN_SECONDS: i64 = 300;
const CLOCK_SKEW_SECONDS: i64 = 60;
const MAX_CALLBACK_BYTES: usize = 8 * 1_024;
const MAX_RESPONSE_BYTES: usize = 64 * 1_024;
const MODELS: &str = "https://api.openai.com/v1/models";
const MAX_MODELS_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_MODELS: usize = 1_024;
const UNUSABLE_REFRESH: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];
pub const MANAGE_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

#[derive(Debug, Error)]
pub enum ChatGptError {
    #[error("not signed in with ChatGPT; sign in with ChatGPT or use an API key")]
    SignedOut,
    #[error(
        "ChatGPT plan usage is not enabled for this sign-in; sign in with ChatGPT again and allow plan usage, or use an API key"
    )]
    PlanUsageDisabled,
    #[error(
        "the ChatGPT session expired or was revoked; sign in with ChatGPT again or use an API key"
    )]
    SessionEnded,
    #[error("OpenAI rejected the registered ChatGPT client (invalid_client)")]
    InvalidClient,
    #[error("ChatGPT sign-in failed: {0}")]
    SignIn(String),
    #[error("OpenAI authorization server request failed: {0}")]
    Transport(&'static str),
    #[error("invalid ChatGPT request: {0}")]
    Invalid(&'static str),
    #[error("no matching ChatGPT sign-in attempt is pending")]
    NoAttempt,
    #[error("the stored ChatGPT credential record is unreadable")]
    Record,
    #[error("the ChatGPT host identifier file is unsafe or malformed")]
    HostId,
    #[error("ChatGPT sign-in was cancelled")]
    Cancelled,
    #[error("ChatGPT model list failed: {0}")]
    Models(&'static str),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptModel {
    pub slug: String,
    pub display_name: Option<String>,
    pub visibility: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatGptOperation {
    Status {
        binding: ModelCredentialReference,
    },
    SignIn {
        binding: ModelCredentialReference,
        timeout_seconds: u64,
    },
    Wait {
        attempt_id: Uuid,
        timeout_ms: u64,
    },
    Cancel {
        attempt_id: Uuid,
    },
    SignOut {
        binding: ModelCredentialReference,
    },
}

impl ChatGptOperation {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Status { binding } | Self::SignOut { binding } => validate_binding(binding),
            Self::SignIn {
                binding,
                timeout_seconds,
            } => {
                validate_binding(binding)?;
                if !(30..=1_800).contains(timeout_seconds) {
                    return Err("ChatGPT sign-in timeout must be between 30 and 1800 seconds");
                }
                Ok(())
            }
            Self::Wait { timeout_ms, .. } if !(1..=30_000).contains(timeout_ms) => {
                Err("ChatGPT sign-in wait must be between one and thirty thousand milliseconds")
            }
            Self::Wait { .. } | Self::Cancel { .. } => Ok(()),
        }
    }
}

pub fn validate_binding(binding: &ModelCredentialReference) -> Result<(), &'static str> {
    if binding.source != ModelCredentialSource::ChatGpt
        || binding.provider != ProviderKind::OpenAi
        || binding.endpoint != ProviderEndpoint::Official
        || binding.reference.is_nil()
    {
        return Err(
            "a ChatGPT binding requires source chatgpt, OpenAI, the official endpoint and a non-nil reference",
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptStatus {
    pub binding: ModelCredentialReference,
    pub signed_in: bool,
    pub plan_usage_enabled: bool,
    pub email: Option<String>,
    pub access_expires_at_ms: Option<u64>,
    pub client_registered: bool,
    pub manage_usage_url: String,
    pub attempt: Option<ChatGptAttempt>,
    pub revocation: Option<ChatGptRevocation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptAttempt {
    pub attempt_id: Uuid,
    pub authorize_url: String,
    pub redirect_uri: String,
    pub expires_at_ms: u64,
    pub phase: ChatGptAttemptPhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatGptAttemptPhase {
    Pending,
    Completed,
    Failed { reason: String },
    Cancelled,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatGptRevocation {
    Confirmed,
    Unconfirmed,
    NotNeeded,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    issuer: String,
    client_id: String,
    subject: Option<String>,
    email: Option<String>,
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_type: Option<String>,
    scopes: Vec<String>,
    expires_at: Option<i64>,
    saved_at: i64,
}

impl Record {
    fn signed_in(&self) -> bool {
        self.access_token.is_some() || self.refresh_token.is_some()
    }

    fn plan_usage(&self) -> bool {
        self.signed_in() && self.scopes.iter().any(|scope| scope == PLAN_SCOPE)
    }

    fn clear_tokens(&mut self) {
        for token in [
            &mut self.id_token,
            &mut self.access_token,
            &mut self.refresh_token,
        ] {
            if let Some(value) = token.as_mut() {
                value.zeroize();
            }
            *token = None;
        }
        self.expires_at = None;
    }
}

impl Drop for Record {
    fn drop(&mut self) {
        self.clear_tokens();
    }
}

struct Pending {
    attempt_id: Uuid,
    binding: ModelCredentialReference,
    authorize_url: String,
    redirect_uri: String,
    expires_at_ms: u64,
    cancel: CancellationToken,
    phase: watch::Receiver<ChatGptAttemptPhase>,
}

impl Pending {
    fn view(&self) -> ChatGptAttempt {
        ChatGptAttempt {
            attempt_id: self.attempt_id,
            authorize_url: self.authorize_url.clone(),
            redirect_uri: self.redirect_uri.clone(),
            expires_at_ms: self.expires_at_ms,
            phase: self.phase.borrow().clone(),
        }
    }
}

struct Callback {
    binding: ModelCredentialReference,
    state: String,
    nonce: String,
    verifier: String,
    redirect_uri: String,
    client_id: Option<String>,
    jwks_uri: String,
    subject: Option<String>,
}

impl Drop for Callback {
    fn drop(&mut self) {
        self.verifier.zeroize();
    }
}

#[derive(Clone)]
pub struct ChatGptAccounts {
    vault: CredentialVault,
    lease: Arc<StateLease>,
    http: reqwest::Client,
    pending: Arc<Mutex<Option<Arc<Pending>>>>,
}

impl ChatGptAccounts {
    pub fn new(vault: CredentialVault, lease: Arc<StateLease>) -> Result<Self, ChatGptError> {
        Ok(Self {
            vault,
            lease,
            http: http_client()?,
            pending: Arc::new(Mutex::new(None)),
        })
    }

    pub async fn operate(
        &self,
        operation: ChatGptOperation,
        cancellation: CancellationToken,
    ) -> Result<ChatGptStatus, ChatGptError> {
        operation.validate().map_err(ChatGptError::Invalid)?;
        match operation {
            ChatGptOperation::Status { binding } => self.status(binding, None, cancellation).await,
            ChatGptOperation::SignIn {
                binding,
                timeout_seconds,
            } => self.sign_in(binding, timeout_seconds, cancellation).await,
            ChatGptOperation::Wait {
                attempt_id,
                timeout_ms,
            } => {
                let pending = self.pending(attempt_id)?;
                let mut phase = pending.phase.clone();
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return Err(ChatGptError::Cancelled),
                    _ = tokio::time::timeout(
                        Duration::from_millis(timeout_ms),
                        phase.wait_for(|phase| *phase != ChatGptAttemptPhase::Pending),
                    ) => {}
                }
                self.status(pending.binding.clone(), None, cancellation)
                    .await
            }
            ChatGptOperation::Cancel { attempt_id } => {
                let pending = self.pending(attempt_id)?;
                pending.cancel.cancel();
                let mut phase = pending.phase.clone();
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    phase.wait_for(|phase| *phase != ChatGptAttemptPhase::Pending),
                )
                .await;
                self.status(pending.binding.clone(), None, cancellation)
                    .await
            }
            ChatGptOperation::SignOut { binding } => self.sign_out(binding).await,
        }
    }

    pub async fn models(
        &self,
        binding: ModelCredentialReference,
        cancellation: CancellationToken,
    ) -> Result<Vec<ChatGptModel>, ChatGptError> {
        use futures_util::StreamExt;
        validate_binding(&binding).map_err(ChatGptError::Invalid)?;
        let bearer = ChatGptBearer {
            vault: self.vault.clone(),
            http: self.http.clone(),
            binding,
        };
        let mut token = bearer.access_token(cancellation.clone()).await?;
        let request = self
            .http
            .get(MODELS)
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(&token);
        token.zeroize();
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ChatGptError::Cancelled),
            response = request.send() => response.map_err(|_| ChatGptError::Models("the request failed"))?,
        };
        let status = response.status();
        let retry_after_ms = crate::model::openai::retry_after(response.headers());
        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ChatGptError::Cancelled),
                chunk = chunks.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk =
                chunk.map_err(|_| ChatGptError::Models("the response could not be read"))?;
            if body.len() + chunk.len() > MAX_MODELS_BYTES {
                return Err(ChatGptError::Models("the response exceeds its byte limit"));
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(
                crate::model::openai::chatgpt_body_error(status, retry_after_ms, &body).into(),
            );
        }
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| ChatGptError::Models("the response is not JSON"))?;
        let models = value["models"]
            .as_array()
            .ok_or(ChatGptError::Models("the response has no models array"))?;
        if models.len() > MAX_MODELS {
            return Err(ChatGptError::Models("the response lists too many models"));
        }
        models
            .iter()
            .map(|model| {
                let text = |field: &str, maximum: usize| {
                    model[field]
                        .as_str()
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= maximum
                                && !value.chars().any(char::is_control)
                        })
                        .map(str::to_owned)
                };
                Ok(ChatGptModel {
                    slug: text("slug", 256)
                        .filter(|slug| !slug.chars().any(char::is_whitespace))
                        .ok_or(ChatGptError::Models("a model has an invalid slug"))?,
                    display_name: text("display_name", 256),
                    visibility: text("visibility", 64),
                })
            })
            .collect()
    }

    fn pending(&self, attempt_id: Uuid) -> Result<Arc<Pending>, ChatGptError> {
        self.pending
            .lock()
            .map_err(|_| ChatGptError::NoAttempt)?
            .as_ref()
            .filter(|pending| pending.attempt_id == attempt_id)
            .cloned()
            .ok_or(ChatGptError::NoAttempt)
    }

    async fn status(
        &self,
        binding: ModelCredentialReference,
        revocation: Option<ChatGptRevocation>,
        cancellation: CancellationToken,
    ) -> Result<ChatGptStatus, ChatGptError> {
        let mut records = self.vault.chatgpt_records().await;
        let record = load(&self.vault, &mut records, &binding, cancellation).await?;
        drop(records);
        let attempt = self
            .pending
            .lock()
            .map_err(|_| ChatGptError::NoAttempt)?
            .as_ref()
            .filter(|pending| pending.binding == binding)
            .map(|pending| pending.view());
        Ok(ChatGptStatus {
            signed_in: record.as_ref().is_some_and(Record::signed_in),
            plan_usage_enabled: record.as_ref().is_some_and(Record::plan_usage),
            email: record
                .as_ref()
                .filter(|record| record.signed_in())
                .and_then(|record| record.email.clone()),
            access_expires_at_ms: record
                .as_ref()
                .filter(|record| record.access_token.is_some())
                .and_then(|record| record.expires_at)
                .and_then(|seconds| u64::try_from(seconds).ok())
                .map(|seconds| seconds.saturating_mul(1_000)),
            client_registered: record.is_some(),
            manage_usage_url: MANAGE_USAGE_URL.to_owned(),
            binding,
            attempt,
            revocation,
        })
    }

    async fn sign_in(
        &self,
        binding: ModelCredentialReference,
        timeout_seconds: u64,
        cancellation: CancellationToken,
    ) -> Result<ChatGptStatus, ChatGptError> {
        let lease = Arc::clone(&self.lease);
        let host_id = tokio::task::spawn_blocking(move || host_id(&lease))
            .await
            .map_err(|_| ChatGptError::HostId)??;
        let (client_id, email, subject) = {
            let mut records = self.vault.chatgpt_records().await;
            let record = load(&self.vault, &mut records, &binding, cancellation.clone()).await?;
            match record {
                Some(record) => (
                    Some(record.client_id.clone()),
                    record.email.clone(),
                    record.subject.clone(),
                ),
                None => (None, None, None),
            }
        };
        let discovery = get_json(&self.http, DISCOVERY).await?;
        if discovery["issuer"] != ISSUER
            || discovery["authorization_endpoint"] != AUTHORIZE
            || discovery["token_endpoint"] != TOKEN
        {
            return Err(ChatGptError::Transport(
                "OpenAI discovery endpoints differ from the documented flow",
            ));
        }
        let jwks_uri = issuer_url(&discovery["jwks_uri"])?;
        let listener = match TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|_| ChatGptError::Transport("could not bind a loopback callback"))?,
        };
        let port = listener
            .local_addr()
            .map_err(|_| ChatGptError::Transport("could not bind a loopback callback"))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
        let state = random(32);
        let nonce = random(32);
        let verifier = random(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut parameters = vec![(
            "client_id",
            client_id.as_deref().unwrap_or(REGISTRATION_CLIENT),
        )];
        if client_id.is_none() {
            parameters.push(("agent_name_hint", AGENT_NAME));
        }
        parameters.push(("ext_agent_host_id", &host_id));
        if client_id.is_some()
            && let Some(email) = &email
        {
            parameters.push(("login_hint", email));
        }
        parameters.extend([
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPE),
            ("resource", RESOURCE),
            ("state", &state),
            ("nonce", &nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ]);
        let authorize_url = reqwest::Url::parse_with_params(AUTHORIZE, &parameters)
            .map_err(|_| ChatGptError::Invalid("authorization URL"))?
            .to_string();
        let attempt_id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        let (phase_sender, phase) = watch::channel(ChatGptAttemptPhase::Pending);
        let expires_at_ms = now()
            .saturating_add(timeout_seconds as i64)
            .saturating_mul(1_000) as u64;
        let pending = Arc::new(Pending {
            attempt_id,
            binding: binding.clone(),
            authorize_url,
            redirect_uri: redirect_uri.clone(),
            expires_at_ms,
            cancel: cancel.clone(),
            phase,
        });
        let callback = Callback {
            binding: binding.clone(),
            state,
            nonce,
            verifier,
            redirect_uri,
            client_id,
            jwks_uri,
            subject,
        };
        if let Some(previous) = self
            .pending
            .lock()
            .map_err(|_| ChatGptError::NoAttempt)?
            .replace(Arc::clone(&pending))
        {
            previous.cancel.cancel();
        }
        let accounts = self.clone();
        tokio::spawn(async move {
            let phase = accounts
                .listen(
                    listener,
                    callback,
                    cancel,
                    Duration::from_secs(timeout_seconds),
                )
                .await;
            let _ = phase_sender.send(phase);
        });
        self.status(binding, None, cancellation).await
    }

    async fn listen(
        &self,
        listener: TcpListener,
        callback: Callback,
        cancel: CancellationToken,
        timeout: Duration,
    ) -> ChatGptAttemptPhase {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let accepted = tokio::select! {
                biased;
                _ = cancel.cancelled() => return ChatGptAttemptPhase::Cancelled,
                _ = tokio::time::sleep_until(deadline) => return ChatGptAttemptPhase::Expired,
                accepted = listener.accept() => accepted,
            };
            let Ok((mut stream, _)) = accepted else {
                continue;
            };
            let Some(parameters) = read_callback(&mut stream).await else {
                respond(&mut stream, "404 Not Found", "Not found.").await;
                continue;
            };
            if parameters.get("state") != Some(&callback.state) {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "This response does not match the pending Everything sign-in.",
                )
                .await;
                continue;
            }
            if let Some(error) = parameters.get("error") {
                respond(
                    &mut stream,
                    "200 OK",
                    "ChatGPT sign-in was not completed. You can close this window.",
                )
                .await;
                return ChatGptAttemptPhase::Failed {
                    reason: oauth_code(error),
                };
            }
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(ChatGptError::Cancelled),
                result = tokio::time::timeout_at(deadline, self.complete(&callback, &parameters)) => {
                    result.unwrap_or(Err(ChatGptError::Transport("sign-in deadline elapsed")))
                }
            };
            return match result {
                Ok(()) => {
                    respond(
                        &mut stream,
                        "200 OK",
                        "Signed in with ChatGPT. You can close this window and return to Everything.",
                    )
                    .await;
                    ChatGptAttemptPhase::Completed
                }
                Err(ChatGptError::Cancelled) => ChatGptAttemptPhase::Cancelled,
                Err(error) => {
                    respond(
                        &mut stream,
                        "200 OK",
                        "ChatGPT sign-in could not be completed. Return to Everything for details.",
                    )
                    .await;
                    ChatGptAttemptPhase::Failed {
                        reason: error.to_string(),
                    }
                }
            };
        }
    }

    async fn complete(
        &self,
        callback: &Callback,
        parameters: &BTreeMap<String, String>,
    ) -> Result<(), ChatGptError> {
        let code = parameters
            .get("code")
            .filter(|code| !code.is_empty() && code.len() <= 4_096)
            .ok_or_else(|| ChatGptError::SignIn("the callback has no authorization code".into()))?;
        let returned = parameters.get("client_id");
        let client_id = match (&callback.client_id, returned) {
            (Some(saved), Some(returned)) if saved != returned => {
                return Err(ChatGptError::SignIn(
                    "the callback client ID differs from the saved registration".into(),
                ));
            }
            (Some(saved), _) => saved.clone(),
            (None, Some(issued)) if valid_client_id(issued) => issued.clone(),
            (None, _) => {
                return Err(ChatGptError::SignIn(
                    "registration is incomplete: the callback has no issued client ID".into(),
                ));
            }
        };
        let response = self
            .http
            .post(TOKEN)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("code", code.as_str()),
                ("code_verifier", callback.verifier.as_str()),
                ("redirect_uri", callback.redirect_uri.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(|_| ChatGptError::Transport("token exchange failed"))?;
        let tokens = token_response(response)
            .await
            .map_err(|error| match error {
                TokenFailure::Rejected(code) => {
                    ChatGptError::SignIn(format!("the token endpoint rejected the code ({code})"))
                }
                TokenFailure::Transport(message) => ChatGptError::Transport(message),
            })?;
        let id_token = tokens["id_token"]
            .as_str()
            .ok_or_else(|| ChatGptError::SignIn("the token response has no ID token".into()))?;
        let claims = self
            .verify_id_token(id_token, &client_id, &callback.nonce, &callback.jwks_uri)
            .await?;
        let subject = claims["sub"]
            .as_str()
            .filter(|subject| !subject.is_empty())
            .ok_or_else(|| ChatGptError::SignIn("the ID token has no subject".into()))?;
        if callback
            .subject
            .as_deref()
            .is_some_and(|saved| saved != subject)
        {
            return Err(ChatGptError::SignIn(
                "a different ChatGPT account signed in than the saved registration".into(),
            ));
        }
        let scope = tokens["scope"]
            .as_str()
            .or_else(|| parameters.get("scope").map(String::as_str))
            .unwrap_or_default();
        let record = Record {
            issuer: ISSUER.to_owned(),
            client_id,
            subject: Some(subject.to_owned()),
            email: claims["email"].as_str().map(str::to_owned),
            id_token: Some(id_token.to_owned()),
            access_token: Some(bounded_token(&tokens["access_token"])?),
            refresh_token: tokens["refresh_token"].as_str().map(str::to_owned),
            token_type: tokens["token_type"].as_str().map(str::to_owned),
            scopes: scope.split_whitespace().map(str::to_owned).collect(),
            expires_at: Some(now().saturating_add(expires_in(&tokens))),
            saved_at: now(),
        };
        let mut records = self.vault.chatgpt_records().await;
        save(&self.vault, &mut records, &callback.binding, &record, true).await
    }

    async fn verify_id_token(
        &self,
        token: &str,
        client_id: &str,
        nonce: &str,
        jwks_uri: &str,
    ) -> Result<Value, ChatGptError> {
        let invalid = |reason: &str| ChatGptError::SignIn(format!("ID token rejected: {reason}"));
        let mut parts = token.split('.');
        let (Some(header), Some(payload), Some(signature), None, Some((signed, _))) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            token.rsplit_once('.'),
        ) else {
            return Err(invalid("malformed"));
        };
        let decode = |part: &str| {
            URL_SAFE_NO_PAD
                .decode(part)
                .map_err(|_| invalid("malformed"))
        };
        let header: Value =
            serde_json::from_slice(&decode(header)?).map_err(|_| invalid("malformed header"))?;
        if header["alg"] != "RS256" {
            return Err(invalid("unsupported signature algorithm"));
        }
        let kid = header["kid"].as_str().ok_or_else(|| invalid("no key ID"))?;
        let jwks = get_json(&self.http, jwks_uri).await?;
        let key = jwks["keys"]
            .as_array()
            .and_then(|keys| {
                keys.iter()
                    .find(|key| key["kid"] == kid && key["kty"] == "RSA")
            })
            .ok_or_else(|| invalid("unknown signing key"))?;
        let modulus = decode(key["n"].as_str().unwrap_or_default())?;
        let exponent = decode(key["e"].as_str().unwrap_or_default())?;
        ring::signature::RsaPublicKeyComponents {
            n: modulus,
            e: exponent,
        }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &decode(signature)?,
        )
        .map_err(|_| invalid("bad signature"))?;
        let claims: Value =
            serde_json::from_slice(&decode(payload)?).map_err(|_| invalid("malformed claims"))?;
        let audience = match &claims["aud"] {
            Value::String(value) => value == client_id,
            Value::Array(values) => values.iter().any(|value| value == client_id),
            _ => false,
        };
        let current = now();
        if claims["iss"] != ISSUER {
            return Err(invalid("wrong issuer"));
        }
        if !audience {
            return Err(invalid("wrong audience"));
        }
        if claims["exp"]
            .as_i64()
            .is_none_or(|expiry| expiry + CLOCK_SKEW_SECONDS < current)
        {
            return Err(invalid("expired"));
        }
        if claims["iat"].as_i64().is_none() {
            return Err(invalid("no issued-at time"));
        }
        if claims["nonce"] != nonce {
            return Err(invalid("nonce mismatch"));
        }
        Ok(claims)
    }

    async fn sign_out(
        &self,
        binding: ModelCredentialReference,
    ) -> Result<ChatGptStatus, ChatGptError> {
        let cancellation = CancellationToken::new();
        let mut records = self.vault.chatgpt_records().await;
        let Some(mut record) =
            load(&self.vault, &mut records, &binding, cancellation.clone()).await?
        else {
            drop(records);
            return self
                .status(binding, Some(ChatGptRevocation::NotNeeded), cancellation)
                .await;
        };
        let revocation = match record.refresh_token.clone() {
            Some(mut refresh) => {
                let confirmed = self.revoke(&refresh, &record.client_id).await;
                refresh.zeroize();
                if confirmed {
                    ChatGptRevocation::Confirmed
                } else {
                    ChatGptRevocation::Unconfirmed
                }
            }
            None => ChatGptRevocation::NotNeeded,
        };
        if record.signed_in() || record.id_token.is_some() {
            record.clear_tokens();
            save(&self.vault, &mut records, &binding, &record, true).await?;
        }
        drop(records);
        self.status(binding, Some(revocation), cancellation).await
    }

    async fn revoke(&self, refresh: &str, client_id: &str) -> bool {
        let Ok(discovery) = get_json(&self.http, DISCOVERY).await else {
            return false;
        };
        let Ok(endpoint) = issuer_url(&discovery["revocation_endpoint"]) else {
            return false;
        };
        for attempt in 0..3_u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
            }
            match self
                .http
                .post(&endpoint)
                .form(&[
                    ("token", refresh),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", client_id),
                ])
                .send()
                .await
            {
                Ok(response) if response.status() == reqwest::StatusCode::OK => return true,
                Ok(response) if !response.status().is_server_error() => return false,
                _ => {}
            }
        }
        false
    }
}

pub struct ChatGptBearer {
    vault: CredentialVault,
    http: reqwest::Client,
    binding: ModelCredentialReference,
}

impl ChatGptBearer {
    pub(super) async fn new(
        vault: CredentialVault,
        binding: ModelCredentialReference,
        cancellation: CancellationToken,
    ) -> Result<Self, ChatGptError> {
        let bearer = Self {
            vault,
            http: http_client()?,
            binding,
        };
        let mut token = bearer.access_token(cancellation).await?;
        token.zeroize();
        Ok(bearer)
    }

    async fn access_token(&self, cancellation: CancellationToken) -> Result<String, ChatGptError> {
        let mut records = self.vault.chatgpt_records().await;
        let mut record = load(
            &self.vault,
            &mut records,
            &self.binding,
            cancellation.clone(),
        )
        .await?
        .filter(Record::signed_in)
        .ok_or(ChatGptError::SignedOut)?;
        if !record.plan_usage() {
            return Err(ChatGptError::PlanUsageDisabled);
        }
        if let (Some(access), Some(expiry)) = (&record.access_token, record.expires_at)
            && expiry - REFRESH_MARGIN_SECONDS > now()
        {
            return Ok(access.clone());
        }
        let refresh = record
            .refresh_token
            .clone()
            .ok_or(ChatGptError::SessionEnded)?;
        if cancellation.is_cancelled() {
            return Err(ChatGptError::Cancelled);
        }
        let response = self
            .http
            .post(TOKEN)
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", record.client_id.as_str()),
                ("refresh_token", refresh.as_str()),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(|_| ChatGptError::Transport("token refresh failed"));
        let mut refresh = refresh;
        refresh.zeroize();
        match token_response(response?).await {
            Ok(tokens) => {
                let access = bounded_token(&tokens["access_token"])?;
                record.access_token = Some(access.clone());
                if let Some(rotated) = tokens["refresh_token"].as_str() {
                    record.refresh_token = Some(rotated.to_owned());
                }
                if let Some(scope) = tokens["scope"].as_str() {
                    record.scopes = scope.split_whitespace().map(str::to_owned).collect();
                }
                record.expires_at = Some(now().saturating_add(expires_in(&tokens)));
                record.saved_at = now();
                save(&self.vault, &mut records, &self.binding, &record, false).await?;
                if !record.plan_usage() {
                    return Err(ChatGptError::PlanUsageDisabled);
                }
                Ok(access)
            }
            Err(TokenFailure::Rejected(code)) if UNUSABLE_REFRESH.contains(&code.as_str()) => {
                record.clear_tokens();
                save(&self.vault, &mut records, &self.binding, &record, true).await?;
                Err(ChatGptError::SessionEnded)
            }
            Err(TokenFailure::Rejected(code)) if code == "invalid_client" => {
                Err(ChatGptError::InvalidClient)
            }
            Err(TokenFailure::Rejected(_)) => {
                Err(ChatGptError::Transport("token refresh was rejected"))
            }
            Err(TokenFailure::Transport(message)) => Err(ChatGptError::Transport(message)),
        }
    }
}

#[async_trait]
impl OpenAiBearer for ChatGptBearer {
    async fn bearer(&self, cancellation: CancellationToken) -> Result<String, ModelError> {
        self.access_token(cancellation)
            .await
            .map_err(|error| match error {
                ChatGptError::Cancelled | ChatGptError::Credential(CredentialError::Cancelled) => {
                    ModelError::Cancelled
                }
                ChatGptError::Transport(message) => ModelError::Transport {
                    message: message.to_owned(),
                },
                error => ModelError::Provider {
                    code: None,
                    message: error.to_string(),
                    retryable: false,
                },
            })
    }
}

async fn load(
    vault: &CredentialVault,
    records: &mut ChatGptRecords,
    binding: &ModelCredentialReference,
    cancellation: CancellationToken,
) -> Result<Option<Record>, ChatGptError> {
    let key = binding.scope_key();
    let secret = match records.get(&key) {
        Some(secret) => Arc::clone(secret),
        None => match vault.read_binding(binding, cancellation).await? {
            Some(secret) => {
                records.insert(key, Arc::clone(&secret));
                secret
            }
            None => return Ok(None),
        },
    };
    let record: Record =
        serde_json::from_slice(secret.expose()).map_err(|_| ChatGptError::Record)?;
    if record.issuer != ISSUER || !valid_client_id(&record.client_id) {
        return Err(ChatGptError::Record);
    }
    Ok(Some(record))
}

async fn save(
    vault: &CredentialVault,
    records: &mut ChatGptRecords,
    binding: &ModelCredentialReference,
    record: &Record,
    tracked: bool,
) -> Result<(), ChatGptError> {
    let bytes =
        zeroize::Zeroizing::new(serde_json::to_vec(record).map_err(|_| ChatGptError::Record)?);
    let secret = Arc::new(CredentialSecret::new(bytes.to_vec())?);
    records.remove(&binding.scope_key());
    vault
        .write_binding(binding, CredentialSecret::new(bytes.to_vec())?, tracked)
        .await?;
    records.insert(binding.scope_key(), secret);
    Ok(())
}

enum TokenFailure {
    Rejected(String),
    Transport(&'static str),
}

async fn token_response(response: reqwest::Response) -> Result<Value, TokenFailure> {
    let status = response.status();
    let body = bounded_body(response)
        .await
        .map_err(|_| TokenFailure::Transport("token response could not be read"))?;
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if status.is_success() {
        if value["access_token"].is_string() {
            return Ok(value);
        }
        return Err(TokenFailure::Transport("token response is malformed"));
    }
    if status.is_server_error() {
        return Err(TokenFailure::Transport("token endpoint is unavailable"));
    }
    let code = value["error"]
        .as_str()
        .or_else(|| value["error"]["code"].as_str())
        .or_else(|| value["code"].as_str())
        .unwrap_or("unknown_error");
    Err(TokenFailure::Rejected(oauth_code(code)))
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, ChatGptError> {
    let response = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| ChatGptError::Transport("OpenAI discovery or key request failed"))?;
    if !response.status().is_success() {
        return Err(ChatGptError::Transport(
            "OpenAI discovery or key request was refused",
        ));
    }
    let body = bounded_body(response)
        .await
        .map_err(|_| ChatGptError::Transport("OpenAI discovery or key response failed"))?;
    serde_json::from_slice(&body)
        .map_err(|_| ChatGptError::Transport("OpenAI discovery or key response is malformed"))
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, ()> {
    use futures_util::StreamExt;
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| ())?;
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn issuer_url(value: &Value) -> Result<String, ChatGptError> {
    let url = value
        .as_str()
        .and_then(|value| reqwest::Url::parse(value).ok())
        .filter(|url| {
            url.scheme() == "https"
                && url.host_str() == Some("auth.openai.com")
                && url.port().is_none()
        })
        .ok_or(ChatGptError::Transport(
            "OpenAI discovery returned an unexpected endpoint",
        ))?;
    Ok(url.to_string())
}

fn http_client() -> Result<reqwest::Client, ChatGptError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ChatGptError::Transport("could not initialize the HTTP client"))
}

async fn read_callback(stream: &mut TcpStream) -> Option<BTreeMap<String, String>> {
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1_024];
    let read = async {
        while !head.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).await.ok()?;
            if count == 0 || head.len() + count > MAX_CALLBACK_BYTES {
                return None;
            }
            head.extend_from_slice(&buffer[..count]);
        }
        Some(())
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()??;
    let line = std::str::from_utf8(&head).ok()?.lines().next()?;
    let mut fields = line.split(' ');
    let (Some("GET"), Some(target), Some(version)) = (fields.next(), fields.next(), fields.next())
    else {
        return None;
    };
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/auth/callback" {
        return None;
    }
    let mut parameters = BTreeMap::new();
    for (name, value) in url.query_pairs() {
        if parameters
            .insert(name.into_owned(), value.into_owned())
            .is_some()
        {
            return None;
        }
    }
    Some(parameters)
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Everything</title></head><body><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    })
    .await;
}

fn host_id(lease: &StateLease) -> Result<String, ChatGptError> {
    let directory = &lease.directory.file;
    let descriptor = match rustix::fs::openat(
        directory,
        HOST_ID_FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => {
            let value = format!("urn:uuid:{}", Uuid::new_v4());
            let temporary = format!(".{HOST_ID_FILE}-{}.tmp", Uuid::new_v4());
            let file = rustix::fs::openat(
                directory,
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|_| ChatGptError::HostId)?;
            let mut file = std::fs::File::from(file);
            let written = file
                .write_all(value.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| ChatGptError::HostId)
                .and_then(|()| {
                    rustix::fs::renameat(directory, &temporary, directory, HOST_ID_FILE)
                        .map_err(|_| ChatGptError::HostId)
                });
            if written.is_err() {
                let _ = rustix::fs::unlinkat(directory, &temporary, rustix::fs::AtFlags::empty());
            }
            written?;
            let _ = directory.sync_all();
            return Ok(value);
        }
        Err(_) => return Err(ChatGptError::HostId),
    };
    let metadata = rustix::fs::fstat(&descriptor).map_err(|_| ChatGptError::HostId)?;
    if FileType::from_raw_mode(metadata.st_mode) != FileType::RegularFile
        || metadata.st_uid != lease.directory.uid
        || metadata.st_mode & 0o7777 != 0o600
        || metadata.st_nlink != 1
    {
        return Err(ChatGptError::HostId);
    }
    let mut value = String::new();
    std::fs::File::from(descriptor)
        .take(128)
        .read_to_string(&mut value)
        .map_err(|_| ChatGptError::HostId)?;
    let value = value.trim();
    value
        .strip_prefix("urn:uuid:")
        .and_then(|uuid| Uuid::parse_str(uuid).ok())
        .filter(|uuid| !uuid.is_nil())
        .ok_or(ChatGptError::HostId)?;
    Ok(value.to_owned())
}

fn bounded_token(value: &Value) -> Result<String, ChatGptError> {
    value
        .as_str()
        .filter(|token| {
            !token.is_empty()
                && token.len() <= 12 * 1_024
                && token.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            ChatGptError::SignIn("the token response has an invalid access token".into())
        })
}

fn expires_in(tokens: &Value) -> i64 {
    tokens["expires_in"]
        .as_i64()
        .filter(|seconds| (1..=86_400).contains(seconds))
        .unwrap_or(3_600)
}

fn valid_client_id(value: &str) -> bool {
    value != REGISTRATION_CLIENT
        && !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte))
}

fn oauth_code(value: &str) -> String {
    let code: String = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(64)
        .collect();
    if code.is_empty() {
        "unknown_error".to_owned()
    } else {
        code
    }
}

fn random(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    rand_core::OsRng.fill_bytes(&mut buffer);
    let value = URL_SAFE_NO_PAD.encode(&buffer);
    buffer.zeroize();
    value
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}
