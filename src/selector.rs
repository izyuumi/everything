use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

use crate::model::ToolDefinition;
use crate::permissions::{Authorizer, ExecutionAuthority, PermissionError, ToolVisibility};
use crate::tools::{
    Tool, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext, ToolOutput,
    ToolRegistry,
};

pub const SEARCH_TOOLS_NAME: &str = "search_tools";
const MAX_QUERY_BYTES: usize = 4_096;
const MAX_CATALOG_BYTES: usize = 16 * 1_024 * 1_024;
const MAX_RESULT_BYTES: usize = 512 * 1_024;
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
        let candidates = self.candidates(authority, query)?;
        selection(candidates, limit, SelectionMethod::Local, Vec::new())
    }

    fn candidates(
        &self,
        authority: &ExecutionAuthority,
        query: &str,
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
            if score == 0 && !query_terms.is_empty() {
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
