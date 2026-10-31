use std::fmt::Write as _;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    GetPromptRequestParams, GetPromptResponse, GetPromptResult, Implementation, ListPromptsResult,
    ListResourcesResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::content::{self, DOCS, GUIDES, SECTIONS, Section, URI_PREFIX};

const MAX_QUERY_BYTES: usize = 512;
const MAX_RESULTS: usize = 20;
const RELEASES_URL: &str = "https://github.com/izyuumi/everything/releases";
const SETUP_PROMPT: &str = "setup_vorra";
const TROUBLESHOOTING_WORDS: &[&str] = &[
    "fail",
    "error",
    "recover",
    "repair",
    "troubleshoot",
    "blocker",
    "limit",
    "unavailable",
    "refuse",
];

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    #[schemars(description = "Words to look for in the Vorra documentation")]
    query: String,
    #[schemars(description = "Maximum results, 1 to 20 (default 8)")]
    limit: Option<u8>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PathRequest {
    #[schemars(
        description = "Embedded document path such as docs/development/cli.md, or its vorra-docs:// URI"
    )]
    path: String,
}

#[derive(Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Macos,
    Linux,
    Ios,
}

impl Os {
    fn parse(value: &str) -> Option<Self> {
        serde_json::from_value(Value::String(value.to_ascii_lowercase())).ok()
    }

    fn key(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Ios => "ios",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Macos => "macOS",
            Self::Linux => "Linux",
            Self::Ios => "iOS",
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OsRequest {
    #[schemars(description = "Target platform: macos, linux or ios")]
    os: Os,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct KeyRequest {
    #[schemars(
        description = "Setting key from list_config, for example memory.embeddings.enabled"
    )]
    key: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SymptomRequest {
    #[schemars(description = "What went wrong, for example an error message or behaviour")]
    symptom: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CommandRequest {
    #[schemars(description = "Command path from list_commands, for example \"schedule create\"")]
    name: String,
}

#[derive(Clone)]
pub struct HelpServer {
    manifest: Arc<Value>,
    tool_router: ToolRouter<Self>,
}

fn invalid(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn query_terms(query: &str) -> Result<Vec<String>, ErrorData> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(invalid(format!("query exceeds {MAX_QUERY_BYTES} bytes")));
    }
    let terms = content::terms(query);
    if terms.is_empty() {
        return Err(invalid("query has no searchable words"));
    }
    Ok(terms)
}

fn status_weight(section: &Section) -> f64 {
    if section.path() == "docs/status/implementation.md" {
        0.5
    } else {
        1.0
    }
}

fn hit(section: &Section, terms: &[String]) -> Value {
    json!({
        "path": section.path(),
        "uri": format!("{URI_PREFIX}{}", section.path()),
        "heading": section.heading,
        "snippet": content::snippet(section, terms),
    })
}

fn commands(node: &Value, prefix: &str, output: &mut Vec<Value>) {
    for child in node["subcommands"].as_array().into_iter().flatten() {
        let name = child["name"].as_str().unwrap_or_default();
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix} {name}")
        };
        output.push(json!({ "name": path, "about": child["about"] }));
        commands(child, &path, output);
    }
}

#[tool_router]
impl HelpServer {
    pub fn new(manifest: Value) -> Self {
        Self {
            manifest: Arc::new(manifest),
            tool_router: Self::tool_router(),
        }
    }

    fn descriptors(&self) -> &[Value] {
        self.manifest["settings"]["descriptors"]
            .as_array()
            .map_or(&[], Vec::as_slice)
    }

    #[tool(
        description = "Search the embedded Vorra documentation. Returns ranked sections with document paths, headings and snippets.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn search_docs(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<String, ErrorData> {
        let terms = query_terms(&request.query)?;
        let limit = usize::from(request.limit.unwrap_or(8)).clamp(1, MAX_RESULTS);
        let hits: Vec<Value> = content::search(&terms, limit, status_weight)
            .into_iter()
            .map(|section| hit(section, &terms))
            .collect();
        Ok(pretty(&Value::Array(hits)))
    }

    #[tool(
        description = "Return one embedded Vorra document in full. Only paths listed by resources/list or returned by search_docs are accepted.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn get_doc(&self, Parameters(request): Parameters<PathRequest>) -> Result<String, ErrorData> {
        content::doc(&request.path)
            .map(|doc| doc.text.to_owned())
            .ok_or_else(|| invalid("unknown document; use search_docs or resources/list"))
    }

    #[tool(
        description = "Step-by-step install and first-run guide for macos, linux or ios, assembled from the Vorra documentation.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn get_install_guide(&self, Parameters(request): Parameters<OsRequest>) -> String {
        let mut guide = format!("# Set up Vorra on {}\n", request.os.name());
        let indexes = GUIDES
            .iter()
            .find(|(os, _)| *os == request.os.key())
            .map_or(&[][..], |(_, indexes)| indexes);
        for section in indexes.iter().map(|index| &SECTIONS[*index]) {
            let _ = write!(
                guide,
                "\n## {}\n\nSource: {}\n\n{}\n",
                section.heading,
                section.path(),
                section.body()
            );
        }
        guide
    }

    #[tool(
        description = "List every typed Vorra setting key with its scope, default, agent mutability and apply behaviour. Generated from the real SettingRegistry.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn list_config(&self) -> String {
        pretty(&json!({
            "contract_version": self.manifest["settings"]["contract_version"],
            "settings": self.descriptors(),
        }))
    }

    #[tool(
        description = "Explain one Vorra setting key: its typed descriptor plus documentation sections that mention it.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn explain_config(
        &self,
        Parameters(request): Parameters<KeyRequest>,
    ) -> Result<String, ErrorData> {
        let descriptor = self
            .descriptors()
            .iter()
            .find(|descriptor| descriptor["key"] == request.key.as_str())
            .ok_or_else(|| {
                let keys: Vec<&str> = self
                    .descriptors()
                    .iter()
                    .filter_map(|descriptor| descriptor["key"].as_str())
                    .collect();
                invalid(format!(
                    "unknown setting key; known keys: {}",
                    keys.join(", ")
                ))
            })?;
        let terms = vec![request.key.to_ascii_lowercase()];
        let documentation: Vec<Value> = content::search(&terms, 5, |_| 1.0)
            .into_iter()
            .map(|section| hit(section, &terms))
            .collect();
        Ok(pretty(&json!({
            "descriptor": descriptor,
            "documentation": documentation,
        })))
    }

    #[tool(
        description = "Find documentation for a problem: failure, recovery and known-limitation sections that match the symptom.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn troubleshoot(
        &self,
        Parameters(request): Parameters<SymptomRequest>,
    ) -> Result<String, ErrorData> {
        let terms = query_terms(&request.symptom)?;
        let hits: Vec<Value> = content::search(&terms, 8, |section| {
            let heading = section.heading.to_ascii_lowercase();
            let relevant = TROUBLESHOOTING_WORDS
                .iter()
                .any(|word| heading.contains(word));
            let practical = section.path().starts_with("docs/development/");
            status_weight(section)
                * if relevant { 1.6 } else { 1.0 }
                * if practical { 1.3 } else { 1.0 }
        })
        .into_iter()
        .map(|section| hit(section, &terms))
        .collect();
        Ok(pretty(&json!({
            "matches": hits,
            "status": format!("{URI_PREFIX}docs/status/implementation.md lists what is implemented, validated and still blocked."),
        })))
    }

    #[tool(
        description = "List Vorra's documented components with a one-paragraph summary each, from the architecture documentation.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn list_features(&self) -> String {
        let features: Vec<Value> = DOCS
            .iter()
            .filter(|doc| doc.path.starts_with("docs/architecture/"))
            .map(|doc| {
                let summary = content::first_section(doc.path)
                    .map(|section| {
                        section
                            .body()
                            .split("\n\n")
                            .next()
                            .unwrap_or_default()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                json!({ "name": doc.title, "path": doc.path, "summary": summary })
            })
            .collect();
        pretty(&json!({
            "features": features,
            "maturity": format!("{URI_PREFIX}docs/status/implementation.md"),
        }))
    }

    #[tool(
        description = "List every vorra CLI command path. Generated from the real clap definitions.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn list_commands(&self) -> String {
        let mut output = Vec::new();
        commands(&self.manifest["commands"], "", &mut output);
        pretty(&Value::Array(output))
    }

    #[tool(
        description = "Explain one vorra CLI command: its arguments, flags, defaults and environment variables, plus documentation that mentions it.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn explain_command(
        &self,
        Parameters(request): Parameters<CommandRequest>,
    ) -> Result<String, ErrorData> {
        let words: Vec<&str> = request.name.split_whitespace().collect();
        let words = words.strip_prefix(&["vorra"]).unwrap_or(&words);
        let mut node = &self.manifest["commands"];
        for word in words {
            node = node["subcommands"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|child| child["name"] == *word)
                .ok_or_else(|| invalid("unknown command; use list_commands"))?;
        }
        let phrase = format!("vorra {}", words.join(" ")).trim().to_owned();
        let terms = vec![phrase.to_ascii_lowercase()];
        let documentation: Vec<Value> = SECTIONS
            .iter()
            .filter(|section| section.body().contains(&phrase))
            .take(5)
            .map(|section| hit(section, &terms))
            .collect();
        let subcommands: Vec<&Value> = node["subcommands"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|child| &child["name"])
            .collect();
        Ok(pretty(&json!({
            "command": phrase,
            "about": node["about"],
            "arguments": node["arguments"],
            "subcommands": subcommands,
            "documentation": documentation,
        })))
    }

    #[tool(
        description = "Report the Vorra version this help content was built from and how to check for the latest release.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn latest_version(&self) -> String {
        let latest = SECTIONS
            .iter()
            .find(|section| section.path() == "CHANGELOG.md" && section.heading != "Changelog")
            .map(|section| section.heading);
        pretty(&json!({
            "documented_package_version": self.manifest["version"],
            "latest_changelog_release": latest,
            "releases": RELEASES_URL,
            "check": "Run `vorra update --check` to compare the installed binary with the newest GitHub release; `vorra --version` prints the installed version.",
        }))
    }

    #[tool(
        description = "Return Vorra's CHANGELOG.md as embedded when this server was built.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    fn changelog(&self) -> String {
        let text = content::doc("CHANGELOG.md").map_or("", |doc| doc.text);
        if text.lines().filter(|line| !line.trim().is_empty()).count() <= 1 {
            format!(
                "{text}\nNo stable release notes are recorded yet. Beta release notes are on {RELEASES_URL}."
            )
        } else {
            text.to_owned()
        }
    }
}

fn setup_prompt(os: Option<Os>) -> String {
    let start = match os {
        Some(os) => format!(
            "Help me set up Vorra on {}. Call get_install_guide with os \"{}\" first and walk me through it step by step.",
            os.name(),
            os.key()
        ),
        None => "Help me set up Vorra. Ask which platform I use (macos, linux or ios), then call get_install_guide for it and walk me through it step by step.".to_owned(),
    };
    format!(
        "{start} Use list_commands and explain_command for exact CLI syntax, list_config and explain_config for settings, and troubleshoot or search_docs when something fails. Never ask me to paste API keys or other secrets into the chat."
    )
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HelpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(
            Implementation::new("vorra-help-mcp", env!("CARGO_PKG_VERSION"))
                .with_title("Vorra help"),
        )
        .with_instructions(
            "Read-only help for installing, configuring and using Vorra, a local-first native AI host. Content is the Vorra documentation embedded at build time; it cannot see or change any user's installation.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(
            DOCS.iter()
                .map(|doc| {
                    Resource::new(format!("{URI_PREFIX}{}", doc.path), doc.path)
                        .with_title(doc.title)
                        .with_mime_type("text/markdown")
                        .with_size(doc.text.len() as u64)
                })
                .collect(),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let doc = request
            .uri
            .strip_prefix(URI_PREFIX)
            .and_then(content::doc)
            .ok_or_else(|| ErrorData::resource_not_found("unknown Vorra document", None))?;
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(doc.text, request.uri.clone()).with_mime_type("text/markdown"),
        ])
        .into())
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            SETUP_PROMPT,
            Some("Help me set up Vorra"),
            Some(vec![
                PromptArgument::new("os")
                    .with_description("macos, linux or ios")
                    .with_required(false),
            ]),
        )]))
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        if request.name != SETUP_PROMPT {
            return Err(invalid("unknown prompt"));
        }
        let os = match request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("os"))
        {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if value.trim().is_empty() => None,
            Some(Value::String(value)) => Some(
                Os::parse(value.trim()).ok_or_else(|| invalid("os must be macos, linux or ios"))?,
            ),
            Some(_) => return Err(invalid("os must be a string")),
        };
        Ok(
            GetPromptResult::new(vec![PromptMessage::new_text(Role::User, setup_prompt(os))])
                .with_description("Help me set up Vorra")
                .into(),
        )
    }
}
