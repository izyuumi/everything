use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::credentials::{CredentialOperation, CredentialVault, SecretTransfer};
use crate::host::mcp::McpSettings;
use crate::host::settings::CredentialStorage;
use crate::permissions::{ToolScopeRule, VaultAccess};
use crate::tools::{
    Tool, ToolConcurrency, ToolDescriptor, ToolError, ToolExecutionContext, ToolOutput,
    ToolRegistry,
};

use super::task::{
    MAX_TASK_SECRETS, VAULT_REQUEST, VAULT_SEARCH, VAULT_USE, VaultDestination, VaultTasks,
    VaultUse, env_bindings,
};
use super::{VaultItemKind, VaultItemMetadata, VaultItemSummary, VaultService};

const MAX_SEARCH_RESULTS: usize = 20;

pub(crate) struct VaultRuntime {
    pub(crate) tasks: Arc<VaultTasks>,
    pub(crate) credentials: CredentialVault,
    pub(crate) mcp: McpSettings,
    pub(crate) store: crate::storage::Store,
}

pub struct VaultTools {
    runtime: OnceLock<VaultRuntime>,
}

impl VaultTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            runtime: OnceLock::new(),
        })
    }

    pub fn register_into(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
    ) -> Result<BTreeMap<String, ToolScopeRule>, ToolError> {
        let tools = [VaultAccess::Search, VaultAccess::Use, VaultAccess::Request].map(|access| {
            Arc::new(VaultTool {
                descriptor: descriptor(access),
                access,
                owner: Arc::clone(self),
            })
        });
        for tool in &tools {
            if registry.descriptor(&tool.descriptor.name).is_ok() {
                return Err(ToolError::DuplicateTool {
                    name: tool.descriptor.name.clone(),
                });
            }
        }
        let mut scopes = BTreeMap::new();
        for tool in tools {
            scopes.insert(
                tool.descriptor.name.clone(),
                ToolScopeRule::Vault(tool.access),
            );
            registry.register(tool)?;
        }
        Ok(scopes)
    }

    pub(crate) fn bind(&self, runtime: VaultRuntime) -> Result<(), ToolError> {
        self.runtime
            .set(runtime)
            .map_err(|_| ToolError::InvalidContext {
                message: "vault tools are already bound".into(),
            })
    }
}

struct VaultTool {
    descriptor: ToolDescriptor,
    access: VaultAccess,
    owner: Arc<VaultTools>,
}

fn descriptor(access: VaultAccess) -> ToolDescriptor {
    let (name, description, input_schema, read_only) = match access {
        VaultAccess::Search => (
            VAULT_SEARCH,
            "Search the owner's credential vault by label, site, username, env name or service. Returns item metadata and vault:// references only, never secret values. The query must be nonempty; ask for what the task needs.",
            json!({"type":"object","properties":{"query":{"type":"string","minLength":1,"maxLength":256},"kinds":{"type":"array","items":{"type":"string","enum":["login","api_secret","card","identity"]},"maxItems":4,"uniqueItems":true},"limit":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_RESULTS}},"required":["query"],"additionalProperties":false}),
            true,
        ),
        VaultAccess::Use => (
            VAULT_USE,
            "After owner approval, bind one vault field reference (vault://<provider>/<id>#<field>) to a destination. env {name}: later shell commands in this task receive NAME=<value>; refer to it as $NAME and never print it. mcp_env {server, variable}: stores the value as that configured MCP server variable's native credential for its next start. http_header and form_fill are reserved and rejected. The value is never returned, and tool output containing it is redacted.",
            json!({"type":"object","properties":{"ref":{"type":"string","minLength":1,"maxLength":512},"destination":{"type":"object","properties":{"kind":{"type":"string","enum":["env","mcp_env","http_header","form_fill"]},"name":{"type":"string","minLength":1,"maxLength":128},"server":{"type":"string","minLength":1,"maxLength":20},"variable":{"type":"string","minLength":1,"maxLength":128}},"required":["kind"],"additionalProperties":false},"reason":{"type":"string","minLength":1,"maxLength":512}},"required":["ref","destination","reason"],"additionalProperties":false}),
            false,
        ),
        VaultAccess::Request => (
            VAULT_REQUEST,
            "Ask the owner to add a missing vault item. The owner enters the secret through a masked owner surface and then approves this request; the result is only the new item's vault:// reference.",
            json!({"type":"object","properties":{"kind":{"type":"string","enum":["login","api_secret","identity"]},"label":{"type":"string","minLength":1,"maxLength":128},"site":{"type":"string","minLength":1,"maxLength":253},"env_name":{"type":"string","minLength":1,"maxLength":128}},"required":["kind","label"],"additionalProperties":false}),
            false,
        ),
    };
    ToolDescriptor {
        name: name.into(),
        description: description.into(),
        input_schema,
        capabilities: Vec::new(),
        read_only,
        concurrency: if read_only {
            ToolConcurrency::Concurrent
        } else {
            ToolConcurrency::Exclusive
        },
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    #[serde(default)]
    kinds: Vec<VaultItemKind>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestArguments {
    kind: VaultItemKind,
    label: String,
    #[serde(default)]
    site: Option<String>,
    #[serde(default)]
    env_name: Option<String>,
}

#[async_trait]
impl Tool for VaultTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    async fn execute(
        &self,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let runtime = self
            .owner
            .runtime
            .get()
            .ok_or_else(|| self.refused("the vault is not available in this runtime"))?;
        context.check_cancelled()?;
        match self.access {
            VaultAccess::Search => self.search(runtime, arguments).await,
            VaultAccess::Use => self.bind(runtime, context, arguments).await,
            VaultAccess::Request => self.request(runtime, arguments).await,
        }
    }
}

impl VaultTool {
    fn refused(&self, message: impl Into<String>) -> ToolError {
        ToolError::FailedWithoutSideEffects {
            name: self.descriptor.name.clone(),
            message: message.into(),
        }
    }

    async fn search(
        &self,
        runtime: &VaultRuntime,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let arguments: SearchArguments = serde_json::from_value(arguments)
            .map_err(|_| self.refused("invalid vault_search arguments"))?;
        let query = arguments.query.trim();
        if query.is_empty() || query.chars().any(char::is_control) {
            return Err(self.refused(
                "vault_search requires a nonempty query; search for what the task needs",
            ));
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&arguments.limit) {
            return Err(self.refused("limit must be between 1 and 20"));
        }
        let items = runtime
            .tasks
            .service()
            .search(query, &arguments.kinds, arguments.limit)
            .await
            .map_err(|error| self.refused(error.to_string()))?;
        Ok(ToolOutput {
            content: json!({
                "items": items,
                "unavailable_providers": VaultService::unavailable_providers(),
            }),
            is_error: false,
        })
    }

    async fn bind(
        &self,
        runtime: &VaultRuntime,
        context: &ToolExecutionContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let vault = context
            .vault()
            .ok_or_else(|| self.refused("vault_use needs a Host task context"))?;
        let parsed = VaultUse::parse(&arguments).map_err(|reason| self.refused(reason))?;
        let item = self.item(runtime, &parsed).await?;
        let applies = match &parsed.destination {
            VaultDestination::Env { name } => {
                let store = runtime.store.clone();
                let conversation = vault.conversation_id.clone();
                let task = vault.task_id.clone();
                let bindings =
                    tokio::task::spawn_blocking(move || env_bindings(&store, &conversation, &task))
                        .await
                        .map_err(|_| self.refused("vault binding worker failed"))?
                        .map_err(|error| self.refused(error.to_string()))?;
                if bindings.len() >= MAX_TASK_SECRETS
                    && !bindings.iter().any(|binding| &binding.name == name)
                {
                    return Err(self.refused("a task can bind at most 16 vault env names"));
                }
                format!(
                    "later shell commands in this task receive ${name}; refer to it by name and never print it"
                )
            }
            VaultDestination::McpEnv { server, variable } => {
                let binding = runtime
                    .mcp
                    .servers
                    .iter()
                    .find(|candidate| &candidate.server_id == server)
                    .ok_or_else(|| self.refused("MCP server is not configured on this Host"))?
                    .environment_binding(variable)
                    .map_err(|_| {
                        self.refused("variable is not an environment reference of this MCP server")
                    })?;
                if binding.storage != CredentialStorage::Native {
                    return Err(self.refused(
                        "session-storage MCP variables are supplied only at Host startup; configure the variable with native storage",
                    ));
                }
                let secret = runtime
                    .tasks
                    .service()
                    .resolve(&parsed.reference, context.cancellation().child_token())
                    .await
                    .map_err(|error| self.refused(error.to_string()))?;
                let transfer = String::from_utf8(secret.expose().to_vec())
                    .ok()
                    .and_then(|value| SecretTransfer::new(value).ok())
                    .ok_or_else(|| {
                        self.refused(
                            "vault value is not printable text for an environment variable",
                        )
                    })?;
                runtime
                    .tasks
                    .remember(&vault.task_id, &parsed.reference, Some(Arc::new(secret)))
                    .map_err(|error| self.refused(error.to_string()))?;
                runtime
                    .credentials
                    .apply(CredentialOperation::StoreMcp {
                        binding,
                        secret: transfer,
                    })
                    .await
                    .map_err(|error| ToolError::EffectsUnknown {
                        name: self.descriptor.name.clone(),
                        message: format!("storing the MCP credential failed: {error}"),
                        cleanup_completed: true,
                    })?;
                "the MCP server receives the value at its next start (Host restart); a running server keeps its current environment".into()
            }
        };
        Ok(ToolOutput {
            content: json!({
                "bound": true,
                "ref": parsed.reference,
                "destination": parsed.destination,
                "kind": item.kind,
                "label": item.label,
                "applies": applies,
            }),
            is_error: false,
        })
    }

    async fn item(
        &self,
        runtime: &VaultRuntime,
        parsed: &VaultUse,
    ) -> Result<VaultItemSummary, ToolError> {
        let store = runtime.store.clone();
        let reference = parsed.reference.clone();
        let item = tokio::task::spawn_blocking(move || super::item_summary(&store, &reference))
            .await
            .map_err(|_| self.refused("vault lookup worker failed"))?
            .map_err(|error| self.refused(error.to_string()))?;
        let field = parsed.reference.field().unwrap_or_default();
        if item.kind == VaultItemKind::Card {
            return Err(self.refused("card references have no destination in this version"));
        }
        if !item.fields.iter().any(|existing| existing == field) {
            return Err(self.refused(format!("vault item has no {field} field")));
        }
        Ok(item)
    }

    async fn request(
        &self,
        runtime: &VaultRuntime,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let arguments: RequestArguments = serde_json::from_value(arguments)
            .map_err(|_| self.refused("invalid vault_request arguments"))?;
        let metadata = match arguments.kind {
            VaultItemKind::Login if arguments.env_name.is_none() => VaultItemMetadata::Login {
                label: arguments.label.clone(),
                sites: arguments.site.into_iter().collect(),
                username: None,
            },
            VaultItemKind::ApiSecret if arguments.site.is_none() => VaultItemMetadata::ApiSecret {
                label: arguments.label.clone(),
                env_name: arguments.env_name,
                service: None,
            },
            VaultItemKind::Identity if arguments.site.is_none() && arguments.env_name.is_none() => {
                VaultItemMetadata::Identity {
                    label: arguments.label.clone(),
                }
            }
            _ => {
                return Err(self.refused(
                    "site applies only to login, env_name only to api_secret, and cards cannot be requested",
                ));
            }
        };
        metadata
            .validate()
            .map_err(|error| self.refused(error.to_string()))?;
        let found = runtime
            .tasks
            .service()
            .search(&arguments.label, &[arguments.kind], MAX_SEARCH_RESULTS)
            .await
            .map_err(|error| self.refused(error.to_string()))?
            .into_iter()
            .find(|item| item.label == arguments.label);
        let Some(item) = found else {
            return Err(self.refused(format!(
                "no ready {} vault item labelled {:?} exists yet; the owner adds it through a masked owner surface (vault_put on the private owner pipe), then the agent can call vault_request again",
                arguments.kind.as_str(),
                arguments.label
            )));
        };
        Ok(ToolOutput {
            content: json!({ "ref": item.reference }),
            is_error: false,
        })
    }
}
