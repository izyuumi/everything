use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use tokio_util::sync::CancellationToken;
use vorra::agent::{ProviderEndpoint, ProviderKind};
use vorra::host::agents::{
    AgentDefinition, AgentId, AgentModel, AgentName, AgentRepository, AgentTools, AgentsFile,
    MAX_PERSONA_BYTES, MemoryAccess, Messaging, SkillAssignment, SkillName, ToolAllow, ToolRef,
    TraitKey,
};
use vorra::host::client::HostClient;
use vorra::ipc::{OwnerOperation, OwnerOperationResult, OwnerQuery, OwnerQueryResult};
use vorra::permissions::ExecutionMode;
use vorra::protocol::CommandId;
use vorra::service::{ServiceError, StateLease};

use crate::{CliError, ProviderArgument, write_json};

#[derive(Args)]
#[command(about = "Create, inspect, edit, remove and choose agents")]
pub struct AgentsArguments {
    #[command(subcommand)]
    command: AgentsCommand,
}

#[derive(Subcommand)]
enum AgentsCommand {
    #[command(about = "List agents; * marks the default")]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Show one agent")]
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Create an agent; new agents start with tools off")]
    Create(Box<CreateArguments>),
    #[command(about = "Edit an agent; existing conversations keep their model and persona")]
    Edit(Box<EditArguments>),
    #[command(about = "Remove an agent; the current default cannot be removed")]
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    #[command(about = "Show the default agent, or make NAME the default")]
    Default { name: Option<String> },
}

#[derive(Args)]
struct DefinitionArguments {
    #[arg(long)]
    description: Option<String>,
    #[arg(long, conflicts_with = "persona_file")]
    persona: Option<String>,
    #[arg(long, value_name = "PATH")]
    persona_file: Option<PathBuf>,
    #[arg(long, value_enum)]
    provider: Option<ProviderArgument>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_name = "URL")]
    loopback_endpoint: Option<String>,
    #[arg(long)]
    temperature: Option<f32>,
    #[arg(long)]
    max_output_tokens: Option<u32>,
    #[arg(long = "trait", value_name = "KEY=VALUE")]
    traits: Vec<String>,
    #[arg(long, value_enum)]
    tools: Option<ToolsArgument>,
    #[arg(long = "allow-tool", value_name = "NAME")]
    allow_tools: Vec<String>,
    #[arg(long = "allow-mcp", value_name = "SERVER_ID")]
    allow_mcp: Vec<String>,
    #[arg(long, conflicts_with_all = ["allow_tools", "allow_mcp"])]
    allow_all_tools: bool,
    #[arg(long, value_name = "PATH")]
    workspace: Option<PathBuf>,
    #[arg(long, value_enum)]
    memory: Option<MemoryArgument>,
    #[arg(long, value_enum, conflicts_with = "skill")]
    skills: Option<SkillsArgument>,
    #[arg(long, value_name = "NAME")]
    skill: Vec<String>,
    #[arg(long = "may-message", value_name = "NAME")]
    may_message: Vec<String>,
}

#[derive(Args)]
struct CreateArguments {
    name: String,
    #[command(flatten)]
    definition: DefinitionArguments,
}

#[derive(Args)]
struct EditArguments {
    name: String,
    #[command(flatten)]
    definition: DefinitionArguments,
    #[arg(long, value_name = "NEW")]
    rename: Option<String>,
    #[arg(long, conflicts_with_all = ["persona", "persona_file"])]
    clear_persona: bool,
    #[arg(long, conflicts_with_all = ["provider", "model", "loopback_endpoint", "temperature", "max_output_tokens"])]
    clear_model: bool,
    #[arg(long, conflicts_with = "workspace")]
    clear_workspace: bool,
    #[arg(long = "remove-tool", value_name = "NAME")]
    remove_tools: Vec<String>,
    #[arg(long = "remove-trait", value_name = "KEY")]
    remove_traits: Vec<String>,
    #[arg(long, conflicts_with = "may_message")]
    no_messaging: bool,
    #[arg(long, value_name = "N")]
    expect_revision: Option<u64>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ToolsArgument {
    Off,
    ReadOnly,
    TrustedWorkspace,
    FullHost,
}

#[derive(Clone, Copy, ValueEnum)]
enum MemoryArgument {
    Shared,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum SkillsArgument {
    None,
    All,
}

fn invalid(message: impl Into<String>) -> CliError {
    CliError::AgentInput(message.into())
}

enum Backend {
    Host(HostClient),
    Offline(AgentRepository),
}

impl Backend {
    async fn open(state: PathBuf) -> Result<Self, CliError> {
        match StateLease::acquire(&state).await {
            Ok(lease) => Ok(Self::Offline(AgentRepository::open(lease))),
            Err(ServiceError::AlreadyRunning) => Ok(Self::Host(HostClient::new(state)?)),
            Err(error) => Err(error.into()),
        }
    }

    async fn agents(&self, cancellation: CancellationToken) -> Result<AgentsFile, CliError> {
        match self {
            Self::Host(client) => match client.query(OwnerQuery::Agents, cancellation).await? {
                OwnerQueryResult::Agents { agents } => Ok(*agents),
                _ => Err(CliError::OwnerInput),
            },
            Self::Offline(repository) => {
                let repository = repository.clone();
                Ok(tokio::task::spawn_blocking(move || repository.snapshot())
                    .await
                    .map_err(|_| CliError::Worker)??)
            }
        }
    }

    async fn unavailable_tools(
        &self,
        id: AgentId,
        cancellation: CancellationToken,
    ) -> Result<Option<Vec<String>>, CliError> {
        match self {
            Self::Host(client) => match client.query(OwnerQuery::Agent { id }, cancellation).await?
            {
                OwnerQueryResult::Agent {
                    unavailable_tools, ..
                } => Ok(unavailable_tools),
                _ => Err(CliError::OwnerInput),
            },
            Self::Offline(_) => Ok(None),
        }
    }

    async fn operate(
        &self,
        operation: OwnerOperation,
        cancellation: CancellationToken,
    ) -> Result<AgentsFile, CliError> {
        match self {
            Self::Host(client) => match client
                .operate(operation, CommandId::new(), cancellation)
                .await?
            {
                OwnerOperationResult::Agents { agents } => Ok(*agents),
                _ => Err(CliError::OwnerInput),
            },
            Self::Offline(repository) => {
                let repository = repository.clone();
                tokio::task::spawn_blocking(move || match operation {
                    OwnerOperation::AgentPut {
                        expected_revision,
                        definition,
                    } => Ok(repository.put(expected_revision, *definition)?),
                    OwnerOperation::AgentRemove {
                        id,
                        expected_revision,
                    } => Ok(repository.remove(id, expected_revision)?),
                    OwnerOperation::AgentSetDefault {
                        id,
                        expected_revision,
                    } => Ok(repository.set_default(id, expected_revision)?),
                    _ => Err(CliError::OwnerInput),
                })
                .await
                .map_err(|_| CliError::Worker)?
            }
        }
    }
}

pub fn find<'a>(agents: &'a AgentsFile, name: &str) -> Result<&'a AgentDefinition, CliError> {
    agents
        .by_name(name)
        .ok_or_else(|| invalid(format!("no agent named {name}; see `vorra agents list`")))
}

pub async fn run(
    state: PathBuf,
    arguments: AgentsArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let backend = Backend::open(state).await?;
    let agents = backend.agents(cancellation.clone()).await?;
    match arguments.command {
        AgentsCommand::List { json } => {
            if json {
                return write_json(agents).await;
            }
            for agent in &agents.agents {
                let marker = if agent.id == agents.default_agent {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{marker} {}\t{}\t{}\t{}",
                    agent.name.as_str(),
                    model_label(agent),
                    agent.tools,
                    agent.description
                );
            }
            Ok(())
        }
        AgentsCommand::Show { name, json } => {
            let agent = find(&agents, &name)?;
            let unavailable = backend.unavailable_tools(agent.id, cancellation).await?;
            if json {
                return write_json(serde_json::json!({
                    "agent": agent,
                    "default": agent.id == agents.default_agent,
                    "unavailable_tools": unavailable,
                }))
                .await;
            }
            print_agent(&agents, agent, unavailable.as_deref());
            Ok(())
        }
        AgentsCommand::Create(arguments) => {
            let CreateArguments { name, definition } = *arguments;
            if definition.description.is_none() {
                return Err(invalid("--description is required to create an agent"));
            }
            let mut agent = AgentDefinition {
                id: AgentId::new(),
                name: AgentName::try_from(name)?,
                description: String::new(),
                persona: None,
                model: None,
                traits: Default::default(),
                tools: AgentTools::Off,
                workspace: None,
                memory: MemoryAccess::Shared,
                skills: SkillAssignment::All,
                messaging: Messaging::Closed,
                revision: 0,
            };
            definition.apply(&mut agent, &agents).await?;
            let id = agent.id;
            let saved = backend
                .operate(
                    OwnerOperation::AgentPut {
                        expected_revision: None,
                        definition: Box::new(agent),
                    },
                    cancellation.clone(),
                )
                .await?;
            print_saved(&backend, &saved, id, cancellation).await
        }
        AgentsCommand::Edit(arguments) => {
            let EditArguments {
                name,
                definition,
                rename,
                clear_persona,
                clear_model,
                clear_workspace,
                remove_tools,
                remove_traits,
                no_messaging,
                expect_revision,
            } = *arguments;
            let mut agent = find(&agents, &name)?.clone();
            let expected_revision = expect_revision.unwrap_or(agent.revision);
            if let Some(rename) = rename {
                agent.name = AgentName::try_from(rename)?;
            }
            if clear_persona {
                agent.persona = None;
            }
            if clear_model {
                agent.model = None;
            }
            if clear_workspace {
                agent.workspace = None;
            }
            if no_messaging {
                agent.messaging = Messaging::Closed;
            }
            for key in remove_traits {
                if agent
                    .traits
                    .remove(&TraitKey::try_from(key.clone())?)
                    .is_none()
                {
                    return Err(invalid(format!("agent has no trait {key}")));
                }
            }
            for tool in remove_tools {
                let AgentTools::On {
                    allow: ToolAllow::Only(references),
                    ..
                } = &mut agent.tools
                else {
                    return Err(invalid(
                        "--remove-tool needs an agent that lists its tools; use --tools off or --allow-tool instead",
                    ));
                };
                let removed = references.remove(&ToolRef::Tool(tool.clone()))
                    | references.remove(&ToolRef::McpServer(tool.clone()));
                if !removed {
                    return Err(invalid(format!("agent does not list tool {tool}")));
                }
            }
            definition.apply(&mut agent, &agents).await?;
            let id = agent.id;
            let saved = backend
                .operate(
                    OwnerOperation::AgentPut {
                        expected_revision: Some(expected_revision),
                        definition: Box::new(agent),
                    },
                    cancellation.clone(),
                )
                .await?;
            print_saved(&backend, &saved, id, cancellation).await
        }
        AgentsCommand::Remove { name, yes } => {
            let agent = find(&agents, &name)?;
            if agent.id == agents.default_agent {
                return Err(invalid(format!(
                    "{name} is the default agent; choose another with `vorra agents default NAME` first"
                )));
            }
            if !yes && !confirm(&format!("remove agent {}? [y/N] ", agent.name.as_str()))? {
                return Err(invalid("not removed; pass --yes to remove without asking"));
            }
            backend
                .operate(
                    OwnerOperation::AgentRemove {
                        id: agent.id,
                        expected_revision: agent.revision,
                    },
                    cancellation,
                )
                .await?;
            println!("removed agent {}", agent.name.as_str());
            Ok(())
        }
        AgentsCommand::Default { name: None } => {
            println!("{}", agents.resolve(None)?.name.as_str());
            Ok(())
        }
        AgentsCommand::Default { name: Some(name) } => {
            let agent = find(&agents, &name)?;
            backend
                .operate(
                    OwnerOperation::AgentSetDefault {
                        id: agent.id,
                        expected_revision: agent.revision,
                    },
                    cancellation,
                )
                .await?;
            println!("default agent: {}", agent.name.as_str());
            Ok(())
        }
    }
}

impl DefinitionArguments {
    async fn apply(self, agent: &mut AgentDefinition, agents: &AgentsFile) -> Result<(), CliError> {
        if let Some(description) = self.description {
            agent.description = description;
        }
        if let Some(persona) = self.persona {
            agent.persona = Some(persona);
        }
        if let Some(path) = self.persona_file {
            agent.persona = Some(read_persona(path).await?);
        }
        if self.provider.is_some() || self.model.is_some() {
            let current = agent.model.take();
            let provider = self
                .provider
                .map(ProviderKind::from)
                .or(current.as_ref().map(|model| model.provider))
                .ok_or_else(|| invalid("--provider is required to give an agent a model"))?;
            let name = self
                .model
                .or(current.as_ref().map(|model| model.model.clone()))
                .ok_or_else(|| invalid("--model is required to give an agent a model"))?;
            let same_provider = current
                .as_ref()
                .is_some_and(|model| model.provider == provider);
            agent.model = Some(AgentModel {
                provider,
                model: name,
                endpoint: current
                    .as_ref()
                    .filter(|_| same_provider)
                    .map_or(ProviderEndpoint::Official, |model| model.endpoint.clone()),
                max_output_tokens: current.as_ref().and_then(|model| model.max_output_tokens),
                temperature: current.as_ref().and_then(|model| model.temperature),
            });
        }
        if self.loopback_endpoint.is_some()
            || self.temperature.is_some()
            || self.max_output_tokens.is_some()
        {
            let model = agent.model.as_mut().ok_or_else(|| {
                invalid("--loopback-endpoint, --temperature and --max-output-tokens need an agent model; add --provider and --model")
            })?;
            if let Some(endpoint) = &self.loopback_endpoint {
                model.endpoint = ProviderEndpoint::loopback(endpoint)?;
            }
            if let Some(temperature) = self.temperature {
                model.temperature = Some(temperature);
            }
            if let Some(tokens) = self.max_output_tokens {
                model.max_output_tokens = Some(tokens);
            }
        }
        for entry in self.traits {
            let (key, value) = entry
                .split_once('=')
                .ok_or_else(|| invalid(format!("--trait {entry} must be KEY=VALUE")))?;
            agent
                .traits
                .insert(TraitKey::try_from(key.to_owned())?, value.to_owned());
        }
        let allow_flags =
            self.allow_all_tools || !self.allow_tools.is_empty() || !self.allow_mcp.is_empty();
        match self.tools {
            Some(ToolsArgument::Off) if allow_flags => {
                return Err(invalid(
                    "--tools off cannot be combined with --allow-tool, --allow-mcp or --allow-all-tools",
                ));
            }
            Some(ToolsArgument::Off) => agent.tools = AgentTools::Off,
            Some(mode) => {
                let max_mode = match mode {
                    ToolsArgument::ReadOnly => ExecutionMode::ReadOnly,
                    ToolsArgument::TrustedWorkspace => ExecutionMode::TrustedWorkspace,
                    _ => ExecutionMode::FullHost,
                };
                let allow = match &agent.tools {
                    AgentTools::On { allow, .. } => allow.clone(),
                    AgentTools::Off => ToolAllow::All,
                };
                agent.tools = AgentTools::On { max_mode, allow };
            }
            None => {}
        }
        if allow_flags {
            let AgentTools::On { allow, .. } = &mut agent.tools else {
                return Err(invalid(
                    "the agent's tools are off; add --tools read-only|trusted-workspace|full-host",
                ));
            };
            if self.allow_all_tools {
                *allow = ToolAllow::All;
            } else {
                let mut references = match allow {
                    ToolAllow::Only(references) => std::mem::take(references),
                    ToolAllow::All => BTreeSet::new(),
                };
                references.extend(self.allow_tools.into_iter().map(ToolRef::Tool));
                references.extend(self.allow_mcp.into_iter().map(ToolRef::McpServer));
                *allow = ToolAllow::Only(references);
            }
        }
        if let Some(path) = self.workspace {
            let path = tokio::fs::canonicalize(&path)
                .await
                .map_err(|error| invalid(format!("workspace {}: {error}", path.display())))?;
            if !tokio::fs::metadata(&path).await?.is_dir() {
                return Err(invalid(format!(
                    "workspace {} is not a directory",
                    path.display()
                )));
            }
            agent.workspace = Some(path);
        }
        if let Some(memory) = self.memory {
            agent.memory = match memory {
                MemoryArgument::Shared => MemoryAccess::Shared,
                MemoryArgument::Off => MemoryAccess::Off,
            };
        }
        if let Some(skills) = self.skills {
            agent.skills = match skills {
                SkillsArgument::None => SkillAssignment::None,
                SkillsArgument::All => SkillAssignment::All,
            };
        }
        if !self.skill.is_empty() {
            agent.skills = SkillAssignment::Only(
                self.skill
                    .iter()
                    .map(|name| {
                        SkillName::parse(name)
                            .map_err(|issue| invalid(format!("skill {name}: {issue}")))
                    })
                    .collect::<Result<_, _>>()?,
            );
        }
        if !self.may_message.is_empty() {
            let mut targets = match std::mem::replace(&mut agent.messaging, Messaging::Closed) {
                Messaging::May(targets) => targets,
                Messaging::Closed => BTreeSet::new(),
            };
            for name in &self.may_message {
                let target = find(agents, name)?;
                if target.id == agent.id {
                    return Err(invalid("an agent cannot list itself in --may-message"));
                }
                targets.insert(target.id);
            }
            agent.messaging = Messaging::May(targets);
        }
        Ok(())
    }
}

async fn read_persona(path: PathBuf) -> Result<String, CliError> {
    let display = path.display().to_string();
    let result = tokio::task::spawn_blocking(move || -> Result<Option<String>, io::Error> {
        let file = std::fs::File::from(
            rustix::fs::open(
                path,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_PERSONA_BYTES as u64 {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.take((MAX_PERSONA_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_PERSONA_BYTES {
            return Ok(None);
        }
        Ok(String::from_utf8(bytes).ok())
    })
    .await
    .map_err(|_| CliError::Worker)?
    .map_err(|error| invalid(format!("persona file {display}: {error}")))?;
    result.ok_or_else(|| {
        invalid(format!(
            "persona file {display} must be a regular UTF-8 file of at most 32768 bytes"
        ))
    })
}

fn confirm(prompt: &str) -> Result<bool, CliError> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

async fn print_saved(
    backend: &Backend,
    agents: &AgentsFile,
    id: AgentId,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let agent = agents
        .get(id)
        .ok_or_else(|| invalid("saved agent is missing"))?;
    let unavailable = backend.unavailable_tools(id, cancellation).await?;
    print_agent(agents, agent, unavailable.as_deref());
    Ok(())
}

pub fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenAi => "openai",
    }
}

fn model_label(agent: &AgentDefinition) -> String {
    agent.model.as_ref().map_or_else(
        || "host default model".to_owned(),
        |model| format!("{}/{}", provider_label(model.provider), model.model),
    )
}

fn print_agent(agents: &AgentsFile, agent: &AgentDefinition, unavailable: Option<&[String]>) {
    let name_of = |id: &AgentId| {
        agents
            .get(*id)
            .map_or_else(|| id.to_string(), |agent| agent.name.as_str().to_owned())
    };
    println!("name: {}", agent.name.as_str());
    println!("id: {}", agent.id);
    println!("revision: {}", agent.revision);
    println!(
        "default: {}",
        if agent.id == agents.default_agent {
            "yes"
        } else {
            "no"
        }
    );
    println!("description: {}", agent.description);
    match &agent.persona {
        Some(persona) => println!(
            "persona: {} ({} bytes)",
            persona
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect::<String>(),
            persona.len()
        ),
        None => println!("persona: host default instructions"),
    }
    match &agent.model {
        Some(model) => {
            let mut line = format!("model: {}/{}", provider_label(model.provider), model.model);
            if let ProviderEndpoint::Loopback(url) = &model.endpoint {
                line.push_str(&format!(" via {url}"));
            }
            if let Some(temperature) = model.temperature {
                line.push_str(&format!(" · temperature {temperature}"));
            }
            if let Some(tokens) = model.max_output_tokens {
                line.push_str(&format!(" · max output tokens {tokens}"));
            }
            println!("{line}");
        }
        None => println!("model: host default model"),
    }
    for (key, value) in &agent.traits {
        println!("trait: {}={value}", key.as_str());
    }
    println!("tools: {}", agent.tools);
    match unavailable {
        Some([]) => {}
        Some(names) => println!("unavailable tools: {}", names.join(", ")),
        None if matches!(
            agent.tools,
            AgentTools::On {
                allow: ToolAllow::Only(_),
                ..
            }
        ) =>
        {
            println!("unavailable tools: not checked without a running Host")
        }
        None => {}
    }
    println!(
        "workspace: {}",
        agent.workspace.as_ref().map_or_else(
            || "host setting".to_owned(),
            |path| path.display().to_string()
        )
    );
    println!(
        "memory: {}",
        match agent.memory {
            MemoryAccess::Shared => "shared",
            MemoryAccess::Off => "off",
        }
    );
    println!(
        "skills: {}",
        match &agent.skills {
            SkillAssignment::None => "none".to_owned(),
            SkillAssignment::All => "all".to_owned(),
            SkillAssignment::Only(names) => names
                .iter()
                .map(SkillName::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        }
    );
    println!(
        "messaging: {}",
        match &agent.messaging {
            Messaging::Closed => "closed".to_owned(),
            Messaging::May(targets) => format!(
                "may message {}",
                targets.iter().map(name_of).collect::<Vec<_>>().join(", ")
            ),
        }
    );
}
