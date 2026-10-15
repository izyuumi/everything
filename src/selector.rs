use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::model::ToolDefinition;
use crate::permissions::{Authorizer, ExecutionAuthority, PermissionError, ToolVisibility};
use crate::protocol::UnixMillis;
use crate::tools::{
    Tool, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext, ToolOutput,
    ToolRegistry,
};

pub const SEARCH_TOOLS_NAME: &str = "search_tools";
const JEV_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const MAX_QUERY_BYTES: usize = 4_096;
const MAX_CATALOG_BYTES: usize = 16 * 1_024 * 1_024;
const MAX_RESULT_BYTES: usize = 512 * 1_024;
const MAX_EXTERNAL_BYTES: usize = 512 * 1_024;
const MAX_RESPONSE_BYTES: usize = 256 * 1_024;
const MAX_BINDINGS: usize = 256;

#[derive(Debug, Error)]
pub enum SelectorError {
    #[error("invalid tool selection input: {0}")]
    InvalidInput(&'static str),
    #[error("tool visibility failed: {0}")]
    Permission(#[from] PermissionError),
    #[error("tool catalog failed: {0}")]
    Tool(#[from] ToolError),
    #[error("selection serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tool discovery bindings are unavailable")]
    BindingsUnavailable,
    #[error("tool discovery already has a binding for this task")]
    DuplicateBinding,
    #[error("tool discovery has reached its task binding limit")]
    BindingLimit,
    #[error("tool selection was cancelled")]
    Cancelled,
    #[error("system clock cannot represent tool selection time")]
    Clock,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidatePermission {
    Available,
    RequiresApproval,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedTool {
    pub definition: ToolDefinition,
    pub permission: CandidatePermission,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMethod {
    Local,
    Jev,
    LocalFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSelection {
    pub tools: Vec<SelectedTool>,
    pub fallback: ToolDefinition,
    pub method: SelectionMethod,
    pub omitted_for_budget: usize,
    pub diagnostics: Vec<String>,
}

impl ToolSelection {
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| tool.definition.clone())
            .chain(std::iter::once(self.fallback.clone()))
            .collect()
    }
}

#[derive(Clone)]
pub struct ToolSelector {
    identity: Uuid,
    catalog: Arc<Vec<ToolDescriptor>>,
    authorizer: Arc<Authorizer>,
}

impl ToolSelector {
    pub fn new(
        registry: &ToolRegistry,
        authorizer: Arc<Authorizer>,
    ) -> Result<Self, SelectorError> {
        let mut catalog = Vec::new();
        let mut bytes = 0_usize;
        for descriptor in registry
            .descriptors()
            .filter(|tool| tool.name != SEARCH_TOOLS_NAME)
        {
            bytes = bytes.saturating_add(serde_json::to_vec(descriptor)?.len());
            if catalog.len() == 1_000 || bytes > MAX_CATALOG_BYTES {
                return Err(SelectorError::InvalidInput(
                    "tool catalog exceeds its limit",
                ));
            }
            catalog.push(descriptor.clone());
        }
        Ok(Self {
            identity: Uuid::new_v4(),
            catalog: Arc::new(catalog),
            authorizer,
        })
    }

    pub fn select_local(
        &self,
        authority: &ExecutionAuthority,
        query: &str,
        limit: usize,
    ) -> Result<ToolSelection, SelectorError> {
        validate_query(query, limit)?;
        require_discovery(authority)?;
        let candidates = self.candidates(authority, query, false)?;
        selection(candidates, limit, SelectionMethod::Local, Vec::new())
    }

    pub fn prepare_jev_disclosure(
        &self,
        authority: &ExecutionAuthority,
        query: &str,
        limit: usize,
    ) -> Result<JevDisclosure, SelectorError> {
        validate_query(query, limit)?;
        let local = self.select_local(authority, query, limit)?;
        let mut candidates = self.candidates(authority, query, true)?;
        let truncated = candidates.len().saturating_sub(254);
        candidates.truncate(254);
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
        let body = json!({
            "state": query,
            "model": "jev-latest",
            "questions": {
                "tool": {
                    "type": "choice",
                    "instructions": "Which tool is most relevant to the task? Rank only by relevance; a tool may still require separate execution approval.",
                    "criteria": criteria
                }
            }
        });
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_EXTERNAL_BYTES {
            return Err(SelectorError::InvalidInput(
                "external selection disclosure exceeds 512 KiB",
            ));
        }
        let fingerprint = disclosure_fingerprint(authority.task_id(), &encoded);
        Ok(JevDisclosure {
            selector_identity: self.identity,
            body,
            fingerprint,
            authority: authority.clone(),
            candidates,
            local,
            limit,
            truncated,
        })
    }

    pub async fn select_with_jev(
        &self,
        client: &JevClient,
        approved: ApprovedJevDisclosure,
        cancellation: CancellationToken,
    ) -> Result<ToolSelection, SelectorError> {
        if cancellation.is_cancelled() {
            return Err(SelectorError::Cancelled);
        }
        let mut disclosure = approved.disclosure;
        if disclosure.selector_identity != self.identity {
            return Err(SelectorError::InvalidInput(
                "approved disclosure belongs to another selector catalog",
            ));
        }
        if disclosure.candidates.is_empty() {
            return Ok(disclosure.local);
        }
        let current_time = now()?;
        let mut fallback = |reason: &str| {
            disclosure.local.method = SelectionMethod::LocalFallback;
            disclosure.local.diagnostics.push(reason.to_owned());
            disclosure.local.clone()
        };
        if current_time < approved.approved_at || current_time >= approved.expires_at {
            return Ok(fallback("Jev approval is outside its validity interval"));
        }
        for candidate in &disclosure.candidates {
            if matches!(
                self.authorizer
                    .tool_visibility(&disclosure.authority, &candidate.definition.name)?,
                ToolVisibility::Denied { .. }
            ) {
                return self.select_local(
                    &disclosure.authority,
                    disclosure.body["state"]
                        .as_str()
                        .ok_or(SelectorError::InvalidInput("invalid prepared disclosure"))?,
                    disclosure.limit,
                );
            }
        }
        let response = match client.rank(&disclosure.body, cancellation).await {
            Ok(response) => response,
            Err(JevFailure::Cancelled) => return Err(SelectorError::Cancelled),
            Err(error) => return Ok(fallback(error.message())),
        };
        let probabilities = match validate_jev_answer(&response, disclosure.candidates.len()) {
            Ok(probabilities) => probabilities,
            Err(error) => return Ok(fallback(error.message())),
        };
        let none = probabilities["none"];
        let mut ranked: Vec<_> = disclosure
            .candidates
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut candidate)| {
                let probability = probabilities[&format!("tool_{index:03}")];
                if probability <= none || probability == 0.0 {
                    return None;
                }
                candidate.score = probability;
                Some(candidate)
            })
            .collect();
        sort_candidates(&mut ranked);
        let diagnostics = if disclosure.truncated == 0 {
            Vec::new()
        } else {
            vec![format!(
                "Jev considered 254 locally ranked candidates; {} additional tools remain discoverable locally",
                disclosure.truncated
            )]
        };
        selection(ranked, disclosure.limit, SelectionMethod::Jev, diagnostics)
    }

    fn candidates(
        &self,
        authority: &ExecutionAuthority,
        query: &str,
        include_unmatched: bool,
    ) -> Result<Vec<SelectedTool>, SelectorError> {
        let query_terms = terms(query);
        let mut candidates = Vec::new();
        for descriptor in self.catalog.iter() {
            let permission = match self
                .authorizer
                .tool_visibility(authority, &descriptor.name)?
            {
                ToolVisibility::Available => CandidatePermission::Available,
                ToolVisibility::RequiresApproval => CandidatePermission::RequiresApproval,
                ToolVisibility::Denied { .. } => continue,
            };
            let names = terms(&descriptor.name);
            let description = terms(&descriptor.description);
            let score = query_terms.intersection(&names).count() * 4
                + query_terms.intersection(&description).count();
            if score == 0 && !query_terms.is_empty() && !include_unmatched {
                continue;
            }
            let description = if permission == CandidatePermission::RequiresApproval {
                format!(
                    "Execution requires explicit host approval for the exact operation. {}",
                    descriptor.description
                )
            } else {
                descriptor.description.clone()
            };
            candidates.push(SelectedTool {
                definition: ToolDefinition {
                    name: descriptor.name.clone(),
                    description,
                    input_schema: descriptor.input_schema.clone(),
                },
                permission,
                score: score as f64,
            });
        }
        sort_candidates(&mut candidates);
        Ok(candidates)
    }
}

pub struct JevDisclosure {
    selector_identity: Uuid,
    body: Value,
    fingerprint: String,
    authority: ExecutionAuthority,
    candidates: Vec<SelectedTool>,
    local: ToolSelection,
    limit: usize,
    truncated: usize,
}

impl JevDisclosure {
    pub fn destination(&self) -> &'static str {
        JEV_ENDPOINT
    }

    pub fn payload(&self) -> &Value {
        &self.body
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn task_id(&self) -> &str {
        self.authority.task_id()
    }

    pub fn approve_external_disclosure(
        self,
        valid_for: Duration,
    ) -> Result<ApprovedJevDisclosure, SelectorError> {
        if valid_for.as_millis() == 0 || valid_for > Duration::from_secs(300) {
            return Err(SelectorError::InvalidInput(
                "external disclosure approval must last at most five minutes",
            ));
        }
        let approved_at = now()?;
        let expires_at = approved_at
            .checked_add(valid_for.as_millis() as u64)
            .ok_or(SelectorError::Clock)?;
        Ok(ApprovedJevDisclosure {
            disclosure: self,
            approved_at,
            expires_at,
        })
    }
}

pub struct ApprovedJevDisclosure {
    disclosure: JevDisclosure,
    approved_at: u64,
    expires_at: u64,
}

pub struct JevClient {
    client: reqwest::Client,
}

impl JevClient {
    pub fn new(api_key: &str) -> Result<Self, SelectorError> {
        if api_key.trim().is_empty() || api_key.len() > 8_192 {
            return Err(SelectorError::InvalidInput("invalid Jev API credential"));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| SelectorError::InvalidInput("invalid Jev API credential"))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| SelectorError::InvalidInput("could not initialize Jev HTTP client"))?;
        Ok(Self { client })
    }

    async fn rank(
        &self,
        body: &Value,
        cancellation: CancellationToken,
    ) -> Result<JevResponse, JevFailure> {
        let operation = async {
            let response = self
                .client
                .post(JEV_ENDPOINT)
                .json(body)
                .send()
                .await
                .map_err(|_| JevFailure::Transport)?;
            if !response.status().is_success() {
                return Err(JevFailure::Http);
            }
            if response
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
            {
                return Err(JevFailure::ResponseLimit);
            }
            let mut bytes = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| JevFailure::Transport)?;
                if chunk.len() > MAX_RESPONSE_BYTES.saturating_sub(bytes.len()) {
                    return Err(JevFailure::ResponseLimit);
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| JevFailure::InvalidResponse)
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(JevFailure::Cancelled),
            result = operation => result,
        }
    }
}

#[derive(Deserialize)]
struct JevResponse {
    answers: BTreeMap<String, JevAnswer>,
}

#[derive(Deserialize)]
struct JevAnswer {
    #[serde(rename = "type")]
    kind: String,
    choice: String,
    confidence: f64,
    probabilities: BTreeMap<String, f64>,
}

enum JevFailure {
    Cancelled,
    Transport,
    Http,
    ResponseLimit,
    InvalidResponse,
}

impl JevFailure {
    fn message(&self) -> &'static str {
        match self {
            Self::Cancelled => "Jev selection was cancelled",
            Self::Transport => "Jev transport failed or timed out; used local selection",
            Self::Http => "Jev rejected the request; used local selection",
            Self::ResponseLimit => "Jev response exceeded its limit; used local selection",
            Self::InvalidResponse => "Jev response was malformed; used local selection",
        }
    }
}

struct BoundAuthority {
    generation: Uuid,
    authority: ExecutionAuthority,
}

type Bindings = Arc<Mutex<BTreeMap<String, BoundAuthority>>>;

pub struct SearchTools {
    descriptor: ToolDescriptor,
    selector: ToolSelector,
    bindings: Bindings,
}

impl SearchTools {
    pub fn new(selector: ToolSelector) -> Self {
        Self {
            descriptor: search_descriptor(),
            selector,
            bindings: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn bind_task(
        &self,
        authority: ExecutionAuthority,
    ) -> Result<DiscoveryBinding, SelectorError> {
        require_discovery(&authority)?;
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| SelectorError::BindingsUnavailable)?;
        if bindings.contains_key(authority.task_id()) {
            return Err(SelectorError::DuplicateBinding);
        }
        if bindings.len() >= MAX_BINDINGS {
            return Err(SelectorError::BindingLimit);
        }
        let task_id = authority.task_id().to_owned();
        let generation = Uuid::new_v4();
        bindings.insert(
            task_id.clone(),
            BoundAuthority {
                generation,
                authority,
            },
        );
        Ok(DiscoveryBinding {
            task_id,
            generation,
            bindings: Arc::clone(&self.bindings),
        })
    }
}

pub struct DiscoveryBinding {
    task_id: String,
    generation: Uuid,
    bindings: Bindings,
}

impl Drop for DiscoveryBinding {
    fn drop(&mut self) {
        match self.bindings.lock() {
            Ok(mut bindings) => {
                if bindings
                    .get(&self.task_id)
                    .is_some_and(|binding| binding.generation == self.generation)
                {
                    bindings.remove(&self.task_id);
                }
            }
            Err(_) => eprintln!("tool discovery binding cleanup failed: poisoned lock"),
        }
    }
}

#[async_trait]
impl Tool for SearchTools {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        context.check_cancelled()?;
        let authority = self
            .bindings
            .lock()
            .map_err(|_| discovery_error("task bindings are unavailable"))?
            .get(context.task_id())
            .filter(|binding| binding.authority.workspace_root() == context.workspace_root())
            .map(|binding| binding.authority.clone())
            .ok_or_else(|| discovery_error("task has no matching host discovery binding"))?;
        let arguments: SearchArguments = serde_json::from_value(arguments)
            .map_err(|_| discovery_error("invalid local discovery arguments"))?;
        let selector = self.selector.clone();
        let selection = tokio::task::spawn_blocking(move || {
            selector.select_local(&authority, &arguments.query, arguments.limit)
        })
        .await
        .map_err(|_| discovery_error("local discovery worker failed"))?
        .map_err(|_| discovery_error("local discovery could not evaluate tool visibility"))?;
        context.check_cancelled()?;
        Ok(ToolOutput {
            content: serde_json::to_value(selection)
                .map_err(|_| discovery_error("local discovery serialization failed"))?,
            is_error: false,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    #[serde(default)]
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    8
}

fn search_descriptor() -> ToolDescriptor {
    ToolDescriptor {
        name: SEARCH_TOOLS_NAME.to_owned(),
        description: "Discover permitted tools by local name and description search. Use an empty query to browse. Approval-required tools remain subject to exact-operation host approval. This tool does not execute discovered tools or send data externally.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "maxLength": 4096},
                "limit": {"type": "integer", "minimum": 1, "maximum": 32}
            },
            "additionalProperties": false
        }),
        capabilities: Vec::new(),
        read_only: true,
        concurrency: ToolConcurrency::Concurrent,
    }
}

fn discovery_error(message: &str) -> ToolError {
    ToolError::InvalidContext {
        message: message.to_owned(),
    }
}

fn selection(
    candidates: Vec<SelectedTool>,
    limit: usize,
    method: SelectionMethod,
    diagnostics: Vec<String>,
) -> Result<ToolSelection, SelectorError> {
    let descriptor = search_descriptor();
    let mut selection = ToolSelection {
        tools: Vec::new(),
        fallback: ToolDefinition {
            name: descriptor.name,
            description: descriptor.description,
            input_schema: descriptor.input_schema,
        },
        method,
        omitted_for_budget: 0,
        diagnostics,
    };
    let mut remaining = MAX_RESULT_BYTES.saturating_sub(serde_json::to_vec(&selection)?.len());
    for candidate in candidates {
        if selection.tools.len() >= limit {
            break;
        }
        let bytes = serde_json::to_vec(&candidate)?.len().saturating_add(32);
        if bytes > remaining {
            selection.omitted_for_budget += 1;
            continue;
        }
        remaining -= bytes;
        selection.tools.push(candidate);
    }
    Ok(selection)
}

fn validate_query(query: &str, limit: usize) -> Result<(), SelectorError> {
    if query.len() > MAX_QUERY_BYTES || query.contains('\0') || !(1..=32).contains(&limit) {
        return Err(SelectorError::InvalidInput(
            "query must be at most 4096 bytes without NUL; limit must be 1 through 32",
        ));
    }
    Ok(())
}

fn require_discovery(authority: &ExecutionAuthority) -> Result<(), SelectorError> {
    if !authority.permits_tool(SEARCH_TOOLS_NAME) {
        return Err(SelectorError::InvalidInput(
            "task allowlist must explicitly permit search_tools for discovery",
        ));
    }
    Ok(())
}

fn terms(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn sort_candidates(candidates: &mut [SelectedTool]) {
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.definition.name.cmp(&b.definition.name))
    });
}

fn disclosure_fingerprint(task_id: &str, body: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"everything.jev.disclosure.v1\0");
    hash.update((JEV_ENDPOINT.len() as u64).to_le_bytes());
    hash.update(JEV_ENDPOINT.as_bytes());
    hash.update((task_id.len() as u64).to_le_bytes());
    hash.update(task_id.as_bytes());
    hash.update((body.len() as u64).to_le_bytes());
    hash.update(body);
    format!("{:x}", hash.finalize())
}

fn now() -> Result<u64, SelectorError> {
    UnixMillis::now()
        .map(|time| time.get())
        .map_err(|_| SelectorError::Clock)
}

fn validate_jev_answer(
    response: &JevResponse,
    candidate_count: usize,
) -> Result<&BTreeMap<String, f64>, JevFailure> {
    let answer = response
        .answers
        .get("tool")
        .ok_or(JevFailure::InvalidResponse)?;
    if answer.kind != "choice"
        || !answer.confidence.is_finite()
        || !(0.0..=1.0).contains(&answer.confidence)
        || answer.probabilities.len() != candidate_count + 1
        || !answer.probabilities.contains_key("none")
        || !answer.probabilities.contains_key(&answer.choice)
        || (0..candidate_count).any(|index| {
            !answer
                .probabilities
                .contains_key(&format!("tool_{index:03}"))
        })
        || answer
            .probabilities
            .values()
            .any(|probability| !probability.is_finite() || !(0.0..=1.0).contains(probability))
        || (answer.probabilities.values().sum::<f64>() - 1.0).abs() > 0.001
    {
        return Err(JevFailure::InvalidResponse);
    }
    Ok(&answer.probabilities)
}
