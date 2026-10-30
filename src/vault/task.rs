use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::credentials::CredentialSecret;
use crate::permissions::{Authorizer, ExecutionAuthority};
use crate::storage::Store;
use crate::tools::ToolError;

use super::item::valid_env_name;
use super::{VaultError, VaultReference, VaultService};

pub const VAULT_SEARCH: &str = "vault_search";
pub const VAULT_USE: &str = "vault_use";
pub const VAULT_REQUEST: &str = "vault_request";
pub(crate) const MAX_TASK_SECRETS: usize = 16;
const MAX_REASON_CHARS: usize = 512;
const MAX_BINDING_ROWS: i64 = 256;
const REDACTED: &str = "[REDACTED]";
const RESERVED_ENVIRONMENT: &[&str] = &[
    "PATH", "LANG", "LC_ALL", "HOME", "SHELL", "IFS", "ENV", "BASH_ENV", "PS4", "TMPDIR",
];
const RESERVED_PREFIXES: &[&str] = &["LC_", "LD_", "DYLD_"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum VaultDestination {
    Env { name: String },
    McpEnv { server: String, variable: String },
}

impl VaultDestination {
    fn parse(value: &Value) -> Result<Self, &'static str> {
        match value.get("kind").and_then(Value::as_str) {
            Some("http_header" | "form_fill") => {
                return Err(
                    "http_header and form_fill destinations are reserved until an HTTP or browser tool exists",
                );
            }
            Some("env" | "mcp_env") => {}
            _ => return Err("destination kind must be env or mcp_env"),
        }
        let destination: Self = serde_json::from_value(value.clone())
            .map_err(|_| "env requires only name; mcp_env requires only server and variable")?;
        match &destination {
            Self::Env { name } => {
                if !valid_env_name(name)
                    || RESERVED_ENVIRONMENT.contains(&name.as_str())
                    || RESERVED_PREFIXES
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                {
                    return Err(
                        "env name must match [A-Z_][A-Z0-9_]* and must not be PATH, LANG, HOME, SHELL, IFS, ENV, BASH_ENV, PS4, TMPDIR or start with LC_, LD_ or DYLD_",
                    );
                }
            }
            Self::McpEnv { server, variable } => {
                if server.is_empty()
                    || server.len() > 20
                    || !server
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                    || variable.is_empty()
                    || variable.len() > 128
                    || variable.as_bytes()[0].is_ascii_digit()
                    || !variable
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err("invalid MCP server ID or variable name");
                }
            }
        }
        Ok(destination)
    }

    pub(crate) fn target(&self) -> String {
        match self {
            Self::Env { name } => format!("env:${name}"),
            Self::McpEnv { server, variable } => format!("mcp_env:{server}/{variable}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VaultUse {
    pub(crate) reference: VaultReference,
    pub(crate) destination: VaultDestination,
    pub(crate) reason: String,
}

impl VaultUse {
    pub(crate) fn parse(arguments: &Value) -> Result<Self, &'static str> {
        let object = arguments
            .as_object()
            .ok_or("vault_use arguments must be an object")?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "ref" | "destination" | "reason"))
        {
            return Err("vault_use accepts only ref, destination and reason");
        }
        let reference: VaultReference = object
            .get("ref")
            .and_then(Value::as_str)
            .ok_or("vault_use requires ref")?
            .parse()
            .map_err(|_| "ref must be a vault://<provider>/<id>#<field> reference")?;
        if reference.field().is_none() {
            return Err("ref must name a field, for example vault://local/<id>#value");
        }
        let destination = VaultDestination::parse(
            object
                .get("destination")
                .ok_or("vault_use requires destination")?,
        )?;
        let reason = object
            .get("reason")
            .and_then(Value::as_str)
            .ok_or("vault_use requires reason")?;
        if reason.trim().is_empty()
            || reason.chars().count() > MAX_REASON_CHARS
            || reason.chars().any(char::is_control)
        {
            return Err("reason must be 1-512 characters without control characters");
        }
        Ok(Self {
            reference,
            destination,
            reason: reason.to_owned(),
        })
    }

    pub(crate) fn same_lease(&self, other: &Self) -> bool {
        self.reference == other.reference && self.destination == other.destination
    }
}

pub(crate) struct EnvBinding {
    pub(crate) operation_id: String,
    pub(crate) name: String,
    pub(crate) reference: VaultReference,
    pub(crate) arguments: Value,
}

pub(crate) fn env_bindings(
    store: &Store,
    conversation_id: &str,
    task_id: &str,
) -> Result<Vec<EnvBinding>, VaultError> {
    let connection = store.connection()?;
    let mut statement = connection.prepare(
        "SELECT id, request FROM tool_calls WHERE conversation_id = ?1 AND task_id = ?2 AND tool_name = ?3 AND state = 'succeeded' ORDER BY created_at, id LIMIT ?4",
    )?;
    let rows = statement.query_map(
        params![conversation_id, task_id, VAULT_USE, MAX_BINDING_ROWS],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let (operation_id, request) = row?;
        let arguments = serde_json::from_str::<Value>(&request)
            .ok()
            .and_then(|mut request| request.get_mut("arguments").map(Value::take))
            .ok_or(VaultError::Malformed)?;
        let parsed = VaultUse::parse(&arguments).map_err(|_| VaultError::Malformed)?;
        if let VaultDestination::Env { name } = parsed.destination {
            bindings.insert(
                name.clone(),
                EnvBinding {
                    operation_id,
                    name,
                    reference: parsed.reference,
                    arguments,
                },
            );
        }
    }
    Ok(bindings.into_values().collect())
}

pub(crate) struct ToolVault {
    pub(crate) conversation_id: String,
    pub(crate) task_id: String,
    pub(crate) environment: Vec<(String, Arc<CredentialSecret>)>,
    pub(crate) literals: Vec<Arc<CredentialSecret>>,
}

impl fmt::Debug for ToolVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolVault")
            .field("task_id", &self.task_id)
            .field(
                "environment",
                &self
                    .environment
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("literals", &self.literals.len())
            .finish()
    }
}

type TaskSecrets = BTreeMap<VaultReference, Option<Arc<CredentialSecret>>>;

pub struct VaultTasks {
    service: VaultService,
    secrets: Mutex<BTreeMap<String, TaskSecrets>>,
}

impl VaultTasks {
    pub(crate) fn new(service: VaultService) -> Arc<Self> {
        Arc::new(Self {
            service,
            secrets: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn service(&self) -> &VaultService {
        &self.service
    }

    fn tasks(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, TaskSecrets>> {
        self.secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn remember(
        &self,
        task_id: &str,
        reference: &VaultReference,
        secret: Option<Arc<CredentialSecret>>,
    ) -> Result<(), VaultError> {
        let mut tasks = self.tasks();
        let task = tasks.entry(task_id.to_owned()).or_default();
        if !task.contains_key(reference) && task.len() >= MAX_TASK_SECRETS {
            return Err(VaultError::Capacity);
        }
        if secret.is_some() || !task.contains_key(reference) {
            task.insert(reference.clone(), secret);
        }
        Ok(())
    }

    pub(crate) fn literals(&self, task_id: &str) -> Vec<Arc<CredentialSecret>> {
        self.tasks()
            .get(task_id)
            .map(|task| task.values().flatten().cloned().collect())
            .unwrap_or_default()
    }

    fn known(&self, task_id: &str, reference: &VaultReference) -> bool {
        self.tasks()
            .get(task_id)
            .is_some_and(|task| task.contains_key(reference))
    }

    pub(crate) fn release(&self, task_id: &str) {
        let released = self.tasks().remove(task_id);
        drop(released);
    }

    pub(crate) async fn prepare(
        &self,
        store: Store,
        authorizer: Arc<Authorizer>,
        authority: &ExecutionAuthority,
        tool_name: &str,
        shell: bool,
        cancellation: &CancellationToken,
    ) -> Result<ToolVault, ToolError> {
        let failed = |message: String| ToolError::FailedWithoutSideEffects {
            name: tool_name.to_owned(),
            message,
        };
        let task_id = authority.task_id().to_owned();
        let checked = authority.clone();
        let (bindings, live) = tokio::task::spawn_blocking(move || {
            let bindings = env_bindings(&store, checked.conversation_id(), checked.task_id())
                .map_err(|error| error.to_string())?;
            let now = crate::protocol::UnixMillis::now()
                .ok()
                .and_then(|now| i64::try_from(now.get()).ok())
                .ok_or("host clock is unavailable")?;
            let live = if shell {
                bindings
                    .iter()
                    .map(|binding| {
                        authorizer.vault_binding_live(
                            &checked,
                            &binding.operation_id,
                            &binding.arguments,
                            now,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?
            } else {
                Vec::new()
            };
            Ok::<_, String>((bindings, live))
        })
        .await
        .map_err(|_| failed("vault binding worker failed".into()))?
        .map_err(|error| failed(format!("vault bindings are unavailable: {error}")))?;
        let mut environment = Vec::new();
        for (index, binding) in bindings.iter().enumerate() {
            if shell {
                if !live.get(index).copied().unwrap_or(false) {
                    return Err(failed(format!(
                        "vault binding ${} is no longer authorized because its approval or vault lease expired or was revoked; call vault_use again",
                        binding.name
                    )));
                }
                let secret = self
                    .service
                    .resolve(&binding.reference, cancellation.child_token())
                    .await
                    .map(Arc::new)
                    .map_err(|error| {
                        failed(format!(
                            "vault binding ${} cannot be resolved: {error}",
                            binding.name
                        ))
                    })?;
                self.remember(&task_id, &binding.reference, Some(Arc::clone(&secret)))
                    .map_err(|error| failed(error.to_string()))?;
                environment.push((binding.name.clone(), secret));
            } else if !self.known(&task_id, &binding.reference) {
                let secret = self
                    .service
                    .resolve(&binding.reference, cancellation.child_token())
                    .await
                    .ok()
                    .map(Arc::new);
                let _ = self.remember(&task_id, &binding.reference, secret);
            }
        }
        Ok(ToolVault {
            conversation_id: authority.conversation_id().to_owned(),
            literals: self.literals(&task_id),
            task_id,
            environment,
        })
    }
}

pub(crate) fn redact_value(value: &mut Value, literals: &[Arc<CredentialSecret>]) -> bool {
    let mut texts: Vec<&str> = literals
        .iter()
        .filter_map(|secret| std::str::from_utf8(secret.expose()).ok())
        .collect();
    texts.sort_by_key(|text| std::cmp::Reverse(text.len()));
    !texts.is_empty() && redact_json(value, &texts)
}

pub(crate) fn redact_text(text: &mut String, literals: &[Arc<CredentialSecret>]) -> bool {
    let mut value = Value::String(std::mem::take(text));
    let fired = redact_value(&mut value, literals);
    if let Value::String(redacted) = value {
        *text = redacted;
    }
    fired
}

fn redact_json(value: &mut Value, literals: &[&str]) -> bool {
    match value {
        Value::String(text) => redact_literals(text, literals),
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |fired, item| redact_json(item, literals) | fired),
        Value::Object(map) => {
            let mut fired = false;
            for (mut key, mut item) in std::mem::take(map) {
                fired |= redact_literals(&mut key, literals);
                fired |= redact_json(&mut item, literals);
                map.insert(key, item);
            }
            fired
        }
        _ => false,
    }
}

fn redact_literals(text: &mut String, literals: &[&str]) -> bool {
    let mut fired = false;
    for literal in literals {
        if text.contains(literal) {
            *text = text.replace(literal, REDACTED);
            fired = true;
        }
    }
    fired
}
