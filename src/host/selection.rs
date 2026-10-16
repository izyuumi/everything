use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::agent::Agent;
use crate::credentials::NativeCredentialStore;
use crate::permissions::{ExecutionAuthority, ExecutionMode};
use crate::protocol::{CommandId, ConversationId, TaskId};
use crate::selector::system_one::{
    ConfigurationError, PreparedSelection, SelectionConfig, SystemOneDisclosure, SystemOneError,
    SystemOneSelector,
};
use crate::selector::{SEARCH_TOOLS_NAME, SelectionMethod, ToolSelection, ToolSelector};
use crate::tools::ToolRegistry;

const MAX_TICKETS: usize = 16;
const MAX_HELD_BYTES: usize = 8 * 1_024 * 1_024;
const MAX_PAYLOAD_BYTES: usize = 512 * 1_024;
const MAX_RESULT_BYTES: usize = 512 * 1_024;
const MAX_AUTHORITY_BYTES: usize = 512 * 1_024;
const MAX_LIFETIME: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectionSettings {
    #[default]
    Local,
    SystemOne {
        endpoint: String,
        model: String,
        credential_reference: Option<Uuid>,
    },
}

impl SelectionSettings {
    pub fn configuration(&self) -> Result<SelectionConfig, ConfigurationError> {
        match self {
            Self::Local => Ok(SelectionConfig::Local),
            Self::SystemOne {
                endpoint,
                model,
                credential_reference,
            } => {
                if credential_reference.is_some_and(|reference| reference.is_nil()) {
                    return Err(ConfigurationError::InvalidCredentialReference);
                }
                SelectionConfig::system_one(endpoint, model, *credential_reference)
            }
        }
    }

    pub fn typesafe(credential_reference: Option<Uuid>) -> Result<Self, ConfigurationError> {
        let SelectionConfig::SystemOne(settings) = SelectionConfig::typesafe(credential_reference)?
        else {
            return Err(ConfigurationError::MissingConfiguration);
        };
        let value = Self::SystemOne {
            endpoint: settings.endpoint().into(),
            model: settings.model().into(),
            credential_reference,
        };
        value.configuration()?;
        Ok(value)
    }
}

#[derive(Clone)]
pub struct SelectionCatalog {
    sha256: String,
    bytes: usize,
}

impl SelectionCatalog {
    pub fn from_registry(registry: &ToolRegistry) -> Result<Self, SelectionPreflightError> {
        let mut digest = Sha256::new();
        digest.update(b"[");
        let mut bytes: usize = 2;
        for (count, descriptor) in registry
            .descriptors()
            .filter(|descriptor| descriptor.name != SEARCH_TOOLS_NAME)
            .enumerate()
        {
            let encoded = serde_json::to_vec(descriptor)
                .map_err(|_| SelectionPreflightError::Serialization)?;
            bytes = bytes.saturating_add(encoded.len() + usize::from(count != 0));
            if count == 1_000 || bytes > 16 * 1_024 * 1_024 {
                return Err(SelectionPreflightError::Capacity);
            }
            if count != 0 {
                digest.update(b",");
            }
            digest.update(&encoded);
        }
        digest.update(b"]");
        Ok(Self {
            sha256: format!("{:x}", digest.finalize()),
            bytes,
        })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionBinding {
    pub conversation_id: ConversationId,
    pub task_id: TaskId,
    pub command_id: CommandId,
    pub original_sha256: String,
    pub execution_sha256: String,
    pub catalog_sha256: String,
}

impl SelectionBinding {
    fn validate(
        &self,
        authority: &ExecutionAuthority,
        catalog: &SelectionCatalog,
    ) -> Result<(), SelectionPreflightError> {
        if self.conversation_id.to_string() != authority.conversation_id()
            || self.task_id.to_string() != authority.task_id()
            || self.catalog_sha256 != catalog.sha256
            || [
                &self.original_sha256,
                &self.execution_sha256,
                &self.catalog_sha256,
            ]
            .iter()
            .any(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(SelectionPreflightError::BindingChanged);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionAuthority {
    pub conversation_id: String,
    pub task_id: String,
    pub workspace_root: PathBuf,
    pub mode: ExecutionMode,
    pub allowed_tools: Option<Vec<String>>,
}

impl From<&ExecutionAuthority> for SelectionAuthority {
    fn from(authority: &ExecutionAuthority) -> Self {
        Self {
            conversation_id: authority.conversation_id().into(),
            task_id: authority.task_id().into(),
            workspace_root: authority.workspace_root().into(),
            mode: authority.mode(),
            allowed_tools: authority
                .allowed_tools()
                .map(|tools| tools.iter().cloned().collect()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionDisclosure {
    pub destination: String,
    pub model: String,
    pub payload: Value,
    pub fingerprint: String,
    pub authority: SelectionAuthority,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionSummary {
    pub method: SelectionMethod,
    pub tool_names: Vec<String>,
    pub omitted_for_budget: usize,
    pub diagnostics: Vec<String>,
}

impl From<&ToolSelection> for SelectionSummary {
    fn from(selection: &ToolSelection) -> Self {
        Self {
            method: selection.method,
            tool_names: selection
                .tools
                .iter()
                .map(|tool| tool.definition.name.clone())
                .collect(),
            omitted_for_budget: selection.omitted_for_budget,
            diagnostics: selection.diagnostics.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SelectionPreparation {
    Local { result: SelectionSummary },
    Disclosure { disclosure: SelectionDisclosure },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSelectionTicket {
    pub ticket: Uuid,
    pub binding: SelectionBinding,
    pub expires_in_ms: u64,
    pub preparation: SelectionPreparation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionPreflightStatus {
    pub closed: bool,
    pub tickets: usize,
    pub active_requests: usize,
    pub reserved_bytes: usize,
}

#[derive(Debug, Error)]
pub enum SelectionPreflightError {
    #[error("selection settings are invalid")]
    Configuration(#[source] ConfigurationError),
    #[error("selection preflight lifetime must be positive and at most five minutes")]
    InvalidLifetime,
    #[error("selection preflight binding or current authority changed")]
    BindingChanged,
    #[error("selection preflight is at its bounded capacity")]
    Capacity,
    #[error("selection preflight is unavailable")]
    Unavailable,
    #[error("selection preflight has expired")]
    Expired,
    #[error("selection preflight is not ready")]
    NotReady,
    #[error("selection preflight is draining")]
    Draining,
    #[error("selection preflight was cancelled")]
    Cancelled,
    #[error("selection preflight worker failed")]
    Worker,
    #[error("selection preflight serialization failed")]
    Serialization,
    #[error("tool selection could not be evaluated")]
    Selection(#[source] SystemOneError),
}

impl From<SystemOneError> for SelectionPreflightError {
    fn from(error: SystemOneError) -> Self {
        match error {
            SystemOneError::Cancelled => Self::Cancelled,
            error => Self::Selection(error),
        }
    }
}

#[derive(Clone)]
pub struct SelectionPreflight {
    inner: Arc<PreflightInner>,
}

struct PreflightInner {
    agent: Agent,
    selector: SystemOneSelector,
    catalog: SelectionCatalog,
    lifetime: Duration,
    shutdown: CancellationToken,
    state: Mutex<PreflightState>,
    settled: Notify,
}

struct PreflightState {
    closed: bool,
    tickets: BTreeMap<Uuid, Ticket>,
    active: BTreeMap<Uuid, usize>,
    bytes: usize,
}

struct Ticket {
    binding: SelectionBinding,
    authority: ExecutionAuthority,
    expires_at: Instant,
    charge: usize,
    state: TicketState,
}

enum TicketState {
    Preparing,
    Disclosure(Box<SystemOneDisclosure>),
    Selecting(CancellationToken),
    Ready(ToolSelection),
}

impl SelectionPreflight {
    pub fn new(
        agent: Agent,
        local: ToolSelector,
        settings: &SelectionSettings,
        credentials: NativeCredentialStore,
        catalog: SelectionCatalog,
        lifetime: Duration,
    ) -> Result<Self, SelectionPreflightError> {
        if lifetime.is_zero() || lifetime > MAX_LIFETIME {
            return Err(SelectionPreflightError::InvalidLifetime);
        }
        let configuration = settings
            .configuration()
            .map_err(SelectionPreflightError::Configuration)?;
        Ok(Self {
            inner: Arc::new(PreflightInner {
                agent,
                selector: SystemOneSelector::new(local, configuration)
                    .with_credential_store(credentials),
                catalog,
                lifetime,
                shutdown: CancellationToken::new(),
                state: Mutex::new(PreflightState {
                    closed: false,
                    tickets: BTreeMap::new(),
                    active: BTreeMap::new(),
                    bytes: 0,
                }),
                settled: Notify::new(),
            }),
        })
    }

    pub async fn prepare(
        &self,
        binding: SelectionBinding,
        authority: ExecutionAuthority,
        query: String,
        limit: usize,
        cancellation: CancellationToken,
    ) -> Result<PreparedSelectionTicket, SelectionPreflightError> {
        if cancellation.is_cancelled() {
            return Err(SelectionPreflightError::Cancelled);
        }
        binding.validate(&authority, &self.inner.catalog)?;
        let authority_bytes = serde_json::to_vec(&SelectionAuthority::from(&authority))
            .map_err(|_| SelectionPreflightError::Serialization)?
            .len();
        if authority_bytes > MAX_AUTHORITY_BYTES
            || query.len() > 4_096
            || !(1..=32).contains(&limit)
        {
            return Err(SelectionPreflightError::Capacity);
        }
        let charge = self
            .inner
            .catalog
            .bytes
            .saturating_mul(2)
            .saturating_add(2 * MAX_PAYLOAD_BYTES + MAX_RESULT_BYTES)
            .saturating_add(authority_bytes)
            .saturating_add(query.len() + 16_384);
        let ticket = Uuid::new_v4();
        let expires_at = Instant::now()
            .checked_add(self.inner.lifetime)
            .ok_or(SelectionPreflightError::InvalidLifetime)?;
        {
            let mut state = self.lock()?;
            state.prune();
            if state.closed {
                return Err(SelectionPreflightError::Draining);
            }
            if state.tickets.len() >= MAX_TICKETS
                || state.active.len() >= MAX_TICKETS
                || charge > MAX_HELD_BYTES.saturating_sub(state.bytes)
            {
                return Err(SelectionPreflightError::Capacity);
            }
            state.tickets.insert(
                ticket,
                Ticket {
                    binding: binding.clone(),
                    authority: authority.clone(),
                    expires_at,
                    charge,
                    state: TicketState::Preparing,
                },
            );
            state.active.insert(ticket, charge);
            state.bytes += charge;
        }
        let this = self.clone();
        let request_guard = RequestGuard {
            inner: Arc::clone(&this.inner),
            ticket,
        };
        let worker = tokio::spawn(async move {
            let _request = request_guard;
            let selector = this.inner.selector.clone();
            let agent = this.inner.agent.clone();
            let current = authority.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                let _agent = agent;
                selector.prepare(&current, &query, limit)
            })
            .await
            .map_err(|_| SelectionPreflightError::Worker)?
            .map_err(SelectionPreflightError::from)?;
            if cancellation.is_cancelled() || this.inner.shutdown.is_cancelled() {
                return Err(SelectionPreflightError::Cancelled);
            }
            let (preparation, ready) = match prepared {
                PreparedSelection::Local(selection) => {
                    validate_result(&selection)?;
                    (
                        SelectionPreparation::Local {
                            result: SelectionSummary::from(&selection),
                        },
                        TicketState::Ready(selection),
                    )
                }
                PreparedSelection::Disclosure(disclosure) => {
                    if serde_json::to_vec(disclosure.payload())
                        .map_err(|_| SelectionPreflightError::Serialization)?
                        .len()
                        > MAX_PAYLOAD_BYTES
                    {
                        return Err(SelectionPreflightError::Capacity);
                    }
                    let value = SelectionDisclosure {
                        destination: disclosure.destination().into(),
                        model: disclosure.model().into(),
                        payload: disclosure.payload().clone(),
                        fingerprint: disclosure.fingerprint().into(),
                        authority: SelectionAuthority::from(disclosure.authority()),
                    };
                    (
                        SelectionPreparation::Disclosure { disclosure: value },
                        TicketState::Disclosure(disclosure),
                    )
                }
            };
            let mut state = this.lock()?;
            let stored = state
                .tickets
                .get_mut(&ticket)
                .ok_or(SelectionPreflightError::Unavailable)?;
            if Instant::now() >= stored.expires_at {
                return Err(SelectionPreflightError::Expired);
            }
            stored.state = ready;
            Ok(PreparedSelectionTicket {
                ticket,
                binding,
                expires_in_ms: u64::try_from(
                    expires_at
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .map_err(|_| SelectionPreflightError::InvalidLifetime)?,
                preparation,
            })
        });
        worker.await.map_err(|_| SelectionPreflightError::Worker)?
    }

    pub async fn approve(
        &self,
        ticket: Uuid,
        binding: SelectionBinding,
        authority: ExecutionAuthority,
        cancellation: CancellationToken,
    ) -> Result<SelectionSummary, SelectionPreflightError> {
        if cancellation.is_cancelled() {
            return Err(SelectionPreflightError::Cancelled);
        }
        let request_cancellation = self.inner.shutdown.child_token();
        let (disclosure, expires_at) = {
            let mut state = self.lock()?;
            if state.closed {
                return Err(SelectionPreflightError::Draining);
            }
            state.require(ticket, &binding, &authority, &self.inner.catalog)?;
            if state.active.len() >= MAX_TICKETS {
                return Err(SelectionPreflightError::Capacity);
            }
            let stored = state
                .tickets
                .get_mut(&ticket)
                .ok_or(SelectionPreflightError::Unavailable)?;
            if !matches!(stored.state, TicketState::Disclosure(_)) {
                return Err(SelectionPreflightError::NotReady);
            }
            let TicketState::Disclosure(disclosure) = std::mem::replace(
                &mut stored.state,
                TicketState::Selecting(request_cancellation.clone()),
            ) else {
                return Err(SelectionPreflightError::NotReady);
            };
            let expires_at = stored.expires_at;
            let charge = stored.charge;
            state.active.insert(ticket, charge);
            (disclosure, expires_at)
        };
        let this = self.clone();
        let request_guard = RequestGuard {
            inner: Arc::clone(&this.inner),
            ticket,
        };
        let worker = tokio::spawn(async move {
            let _request = request_guard;
            let remaining = expires_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SelectionPreflightError::Expired);
            }
            let approved = disclosure
                .approve(remaining)
                .map_err(SelectionPreflightError::from)?;
            let request = this.inner.selector.select_approved(
                &authority,
                approved,
                request_cancellation.clone(),
            );
            tokio::pin!(request);
            let result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    request_cancellation.cancel();
                    let _settled = request.await;
                    return Err(SelectionPreflightError::Cancelled);
                }
                _ = tokio::time::sleep(remaining) => {
                    request_cancellation.cancel();
                    let _settled = request.await;
                    return Err(SelectionPreflightError::Expired);
                }
                result = &mut request => result.map_err(SelectionPreflightError::from)?,
            };
            if request_cancellation.is_cancelled() {
                return Err(SelectionPreflightError::Cancelled);
            }
            validate_result(&result)?;
            let summary = SelectionSummary::from(&result);
            let mut state = this.lock()?;
            state.require(ticket, &binding, &authority, &this.inner.catalog)?;
            if state.closed {
                return Err(SelectionPreflightError::Draining);
            }
            let stored = state
                .tickets
                .get_mut(&ticket)
                .ok_or(SelectionPreflightError::Unavailable)?;
            stored.state = TicketState::Ready(result);
            Ok(summary)
        });
        worker.await.map_err(|_| SelectionPreflightError::Worker)?
    }

    pub fn take(
        &self,
        ticket: Uuid,
        binding: &SelectionBinding,
        authority: &ExecutionAuthority,
    ) -> Result<ToolSelection, SelectionPreflightError> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(SelectionPreflightError::Draining);
        }
        state.require(ticket, binding, authority, &self.inner.catalog)?;
        let stored = state
            .remove(ticket)
            .ok_or(SelectionPreflightError::Unavailable)?;
        match stored.state {
            TicketState::Ready(result) => Ok(result),
            _ => Err(SelectionPreflightError::NotReady),
        }
    }

    pub fn discard(&self, ticket: Uuid) -> Result<(), SelectionPreflightError> {
        self.lock()?
            .remove(ticket)
            .ok_or(SelectionPreflightError::Unavailable)?;
        Ok(())
    }

    pub fn status(&self) -> Result<SelectionPreflightStatus, SelectionPreflightError> {
        let mut state = self.lock()?;
        state.prune();
        Ok(SelectionPreflightStatus {
            closed: state.closed,
            tickets: state.tickets.len(),
            active_requests: state.active.len(),
            reserved_bytes: state.bytes,
        })
    }

    pub async fn shutdown(&self) -> Result<(), SelectionPreflightError> {
        {
            let mut state = self.lock()?;
            state.closed = true;
            self.inner.shutdown.cancel();
            let tickets = state.tickets.keys().copied().collect::<Vec<_>>();
            for ticket in tickets {
                state.remove(ticket);
            }
        }
        loop {
            let settled = self.inner.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if self.lock()?.active.is_empty() {
                return Ok(());
            }
            settled.await;
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, PreflightState>, SelectionPreflightError> {
        self.inner
            .state
            .lock()
            .map_err(|_| SelectionPreflightError::Unavailable)
    }
}

impl PreflightState {
    fn remove(&mut self, ticket: Uuid) -> Option<Ticket> {
        let removed = self.tickets.remove(&ticket)?;
        if !self.active.contains_key(&ticket) {
            self.bytes = self.bytes.saturating_sub(removed.charge);
        }
        if let TicketState::Selecting(cancellation) = &removed.state {
            cancellation.cancel();
        }
        Some(removed)
    }

    fn prune(&mut self) {
        let expired = self
            .tickets
            .iter()
            .filter_map(|(id, ticket)| {
                (Instant::now() >= ticket.expires_at
                    && !matches!(
                        ticket.state,
                        TicketState::Preparing | TicketState::Selecting(_)
                    ))
                .then_some(*id)
            })
            .collect::<Vec<_>>();
        for ticket in expired {
            self.remove(ticket);
        }
    }

    fn require(
        &mut self,
        ticket: Uuid,
        binding: &SelectionBinding,
        authority: &ExecutionAuthority,
        catalog: &SelectionCatalog,
    ) -> Result<(), SelectionPreflightError> {
        let stored = self
            .tickets
            .get(&ticket)
            .ok_or(SelectionPreflightError::Unavailable)?;
        let result = if Instant::now() >= stored.expires_at {
            Err(SelectionPreflightError::Expired)
        } else if &stored.binding != binding || &stored.authority != authority {
            Err(SelectionPreflightError::BindingChanged)
        } else {
            binding.validate(authority, catalog)
        };
        if result.is_err() {
            self.remove(ticket);
        }
        result
    }
}

struct RequestGuard {
    inner: Arc<PreflightInner>,
    ticket: Uuid,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        match self.inner.state.lock() {
            Ok(mut state) => {
                if state.tickets.get(&self.ticket).is_some_and(|ticket| {
                    matches!(
                        ticket.state,
                        TicketState::Preparing | TicketState::Selecting(_)
                    )
                }) {
                    state.remove(self.ticket);
                }
                if let Some(charge) = state.active.remove(&self.ticket)
                    && !state.tickets.contains_key(&self.ticket)
                {
                    state.bytes = state.bytes.saturating_sub(charge);
                }
            }
            Err(_) => eprintln!("selection preflight cleanup failed: poisoned lock"),
        }
        self.inner.settled.notify_waiters();
    }
}

fn validate_result(selection: &ToolSelection) -> Result<(), SelectionPreflightError> {
    if serde_json::to_vec(selection)
        .map_err(|_| SelectionPreflightError::Serialization)?
        .len()
        > MAX_RESULT_BYTES
    {
        return Err(SelectionPreflightError::Capacity);
    }
    Ok(())
}
