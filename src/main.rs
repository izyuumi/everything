use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use everything::agent::{
    Agent, AgentError, Conversation, ConversationConfig, LoopLimits, LoopStatus,
    MAX_CHAT_TEXT_BYTES, ProviderEndpoint, ProviderKind, ProviderResume, TextAcceptance,
    TextCommand, ToolRunSetup, TurnEvent,
};
use everything::context::{ContextBudget, TaskContext};
use everything::model::ModelProvider;
use everything::model::anthropic::{AnthropicConfig, AnthropicProvider};
use everything::model::openai::{OpenAiConfig, OpenAiProvider};
use everything::permissions::{
    ApprovalChoice, ApprovalResolution, Authorizer, ExecutionAuthority, ExecutionMode,
    ToolScopeRule,
};
use everything::protocol::{CommandId, ConversationId, ConversationMessage, MessageContent};
use everything::selector::{SEARCH_TOOLS_NAME, SearchTools, ToolSelector};
use everything::tools::{
    ToolRegistry,
    broker::{BrokerLimits, ToolBroker},
    builtin::{BuiltinConfig, BuiltinTools},
};
use futures_util::StreamExt;
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

const DEFAULT_INSTRUCTIONS: &str = "You are Everything, a helpful assistant.";

#[derive(Parser)]
#[command(version, about = "Local-first native AI conversations")]
struct Arguments {
    #[arg(long, env = "EVERYTHING_STATE_DIR", value_name = "PATH")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Service(everything::service::cli::ServiceArguments),
    Init(InitArguments),
    Chat(ChatArguments),
    Status(StatusArguments),
}

#[derive(Clone, Copy, ValueEnum)]
enum ProviderArgument {
    Anthropic,
    Openai,
}

impl From<ProviderArgument> for ProviderKind {
    fn from(value: ProviderArgument) -> Self {
        match value {
            ProviderArgument::Anthropic => Self::Anthropic,
            ProviderArgument::Openai => Self::OpenAi,
        }
    }
}

#[derive(Args)]
struct ModelArguments {
    #[arg(long, value_enum)]
    provider: Option<ProviderArgument>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    instructions: Option<String>,
    #[arg(long, value_name = "URL")]
    loopback_endpoint: Option<String>,
    #[arg(long)]
    max_output_tokens: Option<u32>,
    #[arg(long)]
    temperature: Option<f32>,
}

impl ModelArguments {
    fn create(self) -> Result<ConversationConfig, CliError> {
        let provider = self
            .provider
            .ok_or(CliError::Required("--provider"))?
            .into();
        let model = self.model.ok_or(CliError::Required("--model"))?;
        let endpoint = match self.loopback_endpoint {
            Some(endpoint) => ProviderEndpoint::loopback(&endpoint)?,
            None => ProviderEndpoint::Official,
        };
        let config = ConversationConfig {
            provider,
            model,
            instructions: self
                .instructions
                .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.to_owned()),
            endpoint,
            max_output_tokens: self.max_output_tokens.unwrap_or(4_096),
            temperature: self.temperature,
        };
        config.validate()?;
        Ok(config)
    }

    fn check(&self, config: &ConversationConfig) -> Result<(), CliError> {
        let endpoint = self
            .loopback_endpoint
            .as_deref()
            .map(ProviderEndpoint::loopback)
            .transpose()?;
        if self
            .provider
            .is_some_and(|provider| ProviderKind::from(provider) != config.provider)
            || self
                .model
                .as_ref()
                .is_some_and(|model| *model != config.model)
            || self
                .instructions
                .as_ref()
                .is_some_and(|instructions| *instructions != config.instructions)
            || endpoint
                .as_ref()
                .is_some_and(|value| *value != config.endpoint)
            || self
                .max_output_tokens
                .is_some_and(|value| value != config.max_output_tokens)
            || self
                .temperature
                .is_some_and(|value| Some(value) != config.temperature)
        {
            return Err(AgentError::ConfigurationMismatch.into());
        }
        Ok(())
    }
}

#[derive(Args)]
struct InitArguments {
    #[arg(long)]
    conversation: Option<ConversationId>,
    #[arg(long)]
    command_id: Option<CommandId>,
    #[arg(long, default_value = "New conversation")]
    title: String,
    #[command(flatten)]
    model: ModelArguments,
}

#[derive(Args)]
#[command(group(ArgGroup::new("input").required(true).args(["message", "message_file", "resume"])))]
struct ChatArguments {
    #[arg(long)]
    conversation: Option<ConversationId>,
    #[arg(long)]
    command_id: Option<CommandId>,
    #[arg(long)]
    message: Option<String>,
    #[arg(long, value_name = "PATH")]
    message_file: Option<PathBuf>,
    #[arg(long, requires = "conversation")]
    resume: bool,
    #[arg(long, conflicts_with = "conversation")]
    title: Option<String>,
    #[command(flatten)]
    model: ModelArguments,
    #[command(flatten)]
    tools: ToolArguments,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArgument {
    ReadOnly,
    TrustedWorkspace,
    FullHost,
}

impl From<ModeArgument> for ExecutionMode {
    fn from(value: ModeArgument) -> Self {
        match value {
            ModeArgument::ReadOnly => Self::ReadOnly,
            ModeArgument::TrustedWorkspace => Self::TrustedWorkspace,
            ModeArgument::FullHost => Self::FullHost,
        }
    }
}

#[derive(Args)]
struct ToolArguments {
    #[arg(long)]
    tools: bool,
    #[arg(long, requires = "tools")]
    workspace: Option<PathBuf>,
    #[arg(long, value_enum, requires = "tools")]
    execution_mode: Option<ModeArgument>,
    #[arg(long, requires = "tools")]
    context_tokens: Option<u64>,
    #[arg(long, requires = "tools")]
    max_model_attempts: Option<u32>,
    #[arg(long, requires = "tools")]
    max_tool_calls: Option<u32>,
    #[arg(long, requires = "tools")]
    max_charged_tokens: Option<u64>,
    #[arg(long, requires = "tools")]
    max_duration_seconds: Option<u64>,
    #[arg(long, requires_all = ["tools", "resume"])]
    retry_uncertain: bool,
    #[arg(long, requires_all = ["tools", "resume"], conflicts_with = "deny")]
    approve: Option<String>,
    #[arg(long, requires_all = ["tools", "resume"])]
    deny: Option<String>,
}

#[derive(Args)]
struct StatusArguments {
    #[arg(long)]
    conversation: Option<ConversationId>,
    #[arg(long, requires = "conversation")]
    history: bool,
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Service(#[from] everything::service::ServiceError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Model(#[from] everything::model::ModelError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0} is required when creating a conversation")]
    Required(&'static str),
    #[error("{0} must contain a valid provider credential; accepted messages remain pending")]
    Credential(&'static str),
    #[error("HOME is unavailable; pass an absolute --state-dir")]
    StateDirectory,
    #[error("command {0} is already pending; use --resume --conversation with its conversation ID")]
    ResumeRequired(CommandId),
    #[error("message files require at most 131072 bytes of UTF-8 text")]
    MessageFile,
    #[error("CLI output worker failed")]
    Worker,
    #[error("could not serialize CLI output")]
    Serialization,
    #[error(transparent)]
    Tool(#[from] everything::tools::ToolError),
    #[error(transparent)]
    Permission(#[from] everything::permissions::PermissionError),
    #[error(transparent)]
    Selector(#[from] everything::selector::SelectorError),
    #[error(transparent)]
    Broker(#[from] everything::tools::broker::BrokerError),
    #[error("tool run stopped at a durable boundary; inspect status before resuming")]
    ToolPaused,
    #[error("invalid tool options: {0}")]
    ToolOptions(&'static str),
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let cancellation = CancellationToken::new();
    let operation = execute(arguments, cancellation.clone());
    tokio::pin!(operation);
    let result = tokio::select! {
        result = &mut operation => result,
        signal = shutdown_signal() => {
            cancellation.cancel();
            let result = operation.await;
            match signal {
                Ok(()) => result,
                Err(error) => Err(CliError::Io(error)),
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("everything: {error}");
            if matches!(error, CliError::Agent(ref error) if error.is_cancelled()) {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

async fn execute(arguments: Arguments, cancellation: CancellationToken) -> Result<(), CliError> {
    let state_dir = arguments
        .state_dir
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".everything")))
        .ok_or(CliError::StateDirectory)?;
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled.into());
    }
    match arguments.command {
        Command::Service(arguments) => {
            write_json(everything::service::cli::manage(arguments, state_dir, cancellation).await?)
                .await
        }
        Command::Init(arguments) => {
            let agent = Agent::open(state_dir).await?;
            let conversation = agent
                .create_conversation(
                    arguments.conversation.unwrap_or_default(),
                    arguments.command_id.unwrap_or_default(),
                    arguments.title,
                    arguments.model.create()?,
                )
                .await?;
            write_json(conversation.info).await
        }
        Command::Status(arguments) => {
            let agent = Agent::open(state_dir).await?;
            match arguments.conversation {
                Some(id) => {
                    let conversation = agent.conversation(id).await?;
                    if arguments.history {
                        #[derive(Serialize)]
                        struct Status {
                            conversation: everything::agent::ConversationInfo,
                            messages: Vec<everything::agent::RecordedMessage>,
                        }
                        write_json(Status {
                            conversation: conversation.info,
                            messages: conversation.messages,
                        })
                        .await
                    } else {
                        write_json(conversation.info).await
                    }
                }
                None => write_json(agent.conversations(arguments.limit).await?).await,
            }
        }
        Command::Chat(arguments) => {
            chat(Agent::open(state_dir).await?, arguments, cancellation).await
        }
    }
}

async fn chat(
    agent: Agent,
    arguments: ChatArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let text = match (arguments.message, arguments.message_file) {
        (Some(message), None) => Some(message),
        (None, Some(path)) => Some(read_message(path).await?),
        _ => None,
    };
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled.into());
    }
    let conversation = match arguments.conversation {
        Some(id) => {
            let conversation = agent.conversation(id).await?;
            arguments.model.check(&conversation.config)?;
            conversation
        }
        None => {
            let title = arguments.title.unwrap_or_else(|| {
                text.as_deref()
                    .unwrap_or("New conversation")
                    .chars()
                    .take(80)
                    .collect()
            });
            agent
                .create_conversation(
                    ConversationId::new(),
                    CommandId::new(),
                    title,
                    arguments.model.create()?,
                )
                .await?
        }
    };
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled.into());
    }
    let command_id = if arguments.resume {
        let pending = conversation
            .info
            .pending_command_id
            .ok_or(AgentError::NoPendingTurn)?;
        if arguments.command_id.is_some_and(|id| id != pending) {
            return Err(
                AgentError::CommandConflict(arguments.command_id.unwrap_or(pending)).into(),
            );
        }
        pending
    } else {
        let command = TextCommand {
            conversation_id: conversation.info.id,
            command_id: arguments.command_id.unwrap_or_default(),
            text: text.ok_or(CliError::Required("--message or --message-file"))?,
        };
        match agent.accept_text(command).await? {
            TextAcceptance::Completed {
                command_id,
                message,
            } => {
                agent
                    .reconcile_foreground_task(conversation.info.id, command_id)
                    .await?;
                eprintln!(
                    "replaying saved response for conversation {}",
                    conversation.info.id
                );
                print_message(&message.message).await?;
                return Ok(());
            }
            TextAcceptance::Pending {
                command_id,
                duplicate: true,
                ..
            } => {
                return Err(CliError::ResumeRequired(command_id));
            }
            TextAcceptance::Pending { command_id, .. } => command_id,
        }
    };
    eprintln!(
        "conversation {} command {}; streamed text is provisional until saved",
        conversation.info.id, command_id
    );
    if arguments.tools.tools {
        return tool_chat(
            agent,
            conversation.info.id,
            command_id,
            arguments.tools,
            cancellation,
        )
        .await;
    }
    let provider = native_provider(&conversation)?;
    let mut events = agent
        .run_pending(conversation.info.id, command_id, provider, cancellation)
        .await?;
    let mut output = tokio::io::stdout();
    let mut provisional = String::new();
    while let Some(event) = events.next().await {
        match event? {
            TurnEvent::ProvisionalText { delta, .. } => {
                provisional.push_str(&delta);
                output.write_all(delta.as_bytes()).await?;
                output.flush().await?;
            }
            TurnEvent::Committed { message, summary } => {
                let authoritative = message_text(&message);
                if authoritative != provisional {
                    if !provisional.is_empty() {
                        output.write_all(b"\n").await?;
                        eprintln!(
                            "saved response differs from provisional output; authoritative text follows"
                        );
                    }
                    output.write_all(authoritative.as_bytes()).await?;
                }
                output.write_all(b"\n").await?;
                output.flush().await?;
                eprintln!(
                    "saved message {} ({:?})",
                    summary.message_id, summary.stop_reason
                );
            }
            TurnEvent::ContextBuilt { .. }
            | TurnEvent::ModelAttemptStarted { .. }
            | TurnEvent::ToolCallRecorded { .. }
            | TurnEvent::ToolResultRecorded { .. }
            | TurnEvent::AwaitingPermission { .. }
            | TurnEvent::Paused { .. }
            | TurnEvent::RunCompleted { .. }
            | TurnEvent::ToolBlocked { .. } => {
                return Err(AgentError::NativeSetupRequired.into());
            }
        }
    }
    agent
        .reconcile_foreground_task(conversation.info.id, command_id)
        .await?;
    Ok(())
}

async fn tool_chat(
    agent: Agent,
    conversation_id: ConversationId,
    command_id: CommandId,
    options: ToolArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let conversation = agent.conversation(conversation_id).await?;
    let provider = native_provider(&conversation)?;
    let task = agent
        .prepare_foreground_task(conversation_id, command_id)
        .await?;
    let result = run_tool_chat(
        &agent,
        conversation,
        command_id,
        provider,
        &task,
        options,
        cancellation,
    )
    .await;
    let settled = agent
        .settle_foreground_task(conversation_id, command_id)
        .await;
    match (result, settled) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(settle_error)) => {
            eprintln!("foreground task remains in its previous durable state: {settle_error}");
            Err(error)
        }
    }
}

async fn run_tool_chat(
    agent: &Agent,
    conversation: Conversation,
    command_id: CommandId,
    provider: Arc<dyn ModelProvider>,
    task: &everything::protocol::TaskRecord,
    options: ToolArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let active = conversation
        .native_run()
        .filter(|run| run.command_id == command_id);
    if active.is_some()
        && (options.workspace.is_some()
            || options.execution_mode.is_some()
            || options.context_tokens.is_some()
            || options.max_model_attempts.is_some()
            || options.max_tool_calls.is_some()
            || options.max_charged_tokens.is_some()
            || options.max_duration_seconds.is_some())
    {
        return Err(CliError::ToolOptions(
            "an active native turn must use its saved setup; omit workspace, mode and budget overrides",
        ));
    }
    if active.as_ref().is_some_and(|run| run.task_id != task.id) {
        return Err(AgentError::ConfigurationMismatch.into());
    }
    let store = agent.store();
    let task_id = task.id;
    let conversation_id = conversation.info.id;
    let instruction = task.instruction.clone();
    let output_tokens = conversation.config.max_output_tokens;
    let prepare_store = store.clone();
    let approval = options
        .approve
        .clone()
        .map(|id| (id, ApprovalChoice::Allow))
        .or_else(|| options.deny.clone().map(|id| (id, ApprovalChoice::Deny)));
    let provider_resume = if options.retry_uncertain {
        ProviderResume::RetryUncertain
    } else {
        ProviderResume::OnlyUnstarted
    };
    let (registry, authorizer, search, authority, initial, limits, budget) =
        tokio::task::spawn_blocking(move || {
            let workspace = match &active {
                Some(run) => run.workspace_root.clone(),
                None => options.workspace.unwrap_or(std::env::current_dir()?),
            };
            let mode = active.as_ref().map(|run| run.mode).unwrap_or_else(|| {
                options
                    .execution_mode
                    .map(ExecutionMode::from)
                    .unwrap_or(ExecutionMode::ReadOnly)
            });
            let mut authority = ExecutionAuthority::new(
                conversation_id.to_string(),
                task_id.to_string(),
                workspace,
                mode,
            )?;
            if let Some(run) = &active {
                authority =
                    authority.restrict_tools(run.allowed_tools.iter().cloned().collect())?;
            }
            let redact_literals = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
                .into_iter()
                .filter_map(|name| std::env::var(name).ok())
                .filter(|value| !value.is_empty())
                .collect();
            let builtins = BuiltinTools::new(BuiltinConfig { redact_literals })?;
            let mut registry = ToolRegistry::new();
            let mut scopes = builtins.register_into(&mut registry)?;
            let selection_authorizer = Arc::new(Authorizer::new(
                prepare_store.clone(),
                Arc::new(registry.clone()),
                scopes.clone(),
            )?);
            let selector = ToolSelector::new(&registry, selection_authorizer)?;
            let search = Arc::new(SearchTools::new(selector.clone()));
            registry.register(search.clone())?;
            scopes.insert(SEARCH_TOOLS_NAME.to_owned(), ToolScopeRule::LocalReadOnly);
            let registry = Arc::new(registry);
            let authorizer = Arc::new(Authorizer::new(prepare_store, registry.clone(), scopes)?);
            if let Some((id, choice)) = approval {
                let now = i64::try_from(
                    everything::protocol::UnixMillis::now()
                        .map_err(AgentError::from)?
                        .get(),
                )
                .map_err(|_| CliError::ToolOptions("timestamp overflow"))?;
                let existing = authorizer.approval(&id, now)?;
                if existing.request.scope.conversation_id != conversation_id.to_string()
                    || existing.request.scope.task_id != task_id.to_string()
                {
                    return Err(CliError::ToolOptions(
                        "approval belongs to a different task",
                    ));
                }
                authorizer.resolve(
                    &id,
                    &ApprovalResolution {
                        choice,
                        expires_at: None,
                    },
                    now,
                )?;
            }
            let query: String = instruction
                .chars()
                .scan(0_usize, |bytes, character| {
                    *bytes += character.len_utf8();
                    (*bytes <= 4096).then_some(character)
                })
                .collect();
            let initial = selector.select_local(&authority, &query, 8)?.definitions();
            let mut limits = LoopLimits::default();
            if let Some(value) = options.max_model_attempts {
                limits.max_model_attempts = value;
            }
            if let Some(value) = options.max_tool_calls {
                limits.max_tool_calls = value;
            }
            if let Some(value) = options.max_charged_tokens {
                limits.max_charged_tokens = value;
            }
            if let Some(value) = options.max_duration_seconds {
                limits.max_duration_ms = value
                    .checked_mul(1000)
                    .ok_or(CliError::ToolOptions("duration overflow"))?;
            }
            limits.validate()?;
            let budget = ContextBudget {
                max_context_tokens: options.context_tokens.unwrap_or(128_000),
                reserved_output_tokens: output_tokens,
                safety_margin_tokens: 1024,
                max_request_bytes: 32 * 1024 * 1024,
                max_compaction_bytes: 16 * 1024,
            };
            Ok::<_, CliError>((
                registry, authorizer, search, authority, initial, limits, budget,
            ))
        })
        .await
        .map_err(|_| CliError::Worker)??;
    let broker = ToolBroker::start(
        store,
        registry.clone(),
        authorizer,
        BrokerLimits {
            max_concurrent: 8,
            max_in_flight: 32,
            queue_timeout: std::time::Duration::from_secs(30),
            execution_timeout: std::time::Duration::from_secs(120),
            cancellation_grace: std::time::Duration::from_secs(5),
        },
    )
    .await?;
    if broker.is_quarantined() {
        eprintln!(
            "tool broker requires owner cleanup reconciliation; operations will remain blocked"
        );
    }
    let _discovery = search.bind_task(authority.clone())?;
    let scoped = broker.scoped(authority);
    let setup = if conversation
        .native_run()
        .is_some_and(|run| run.command_id == command_id)
    {
        ToolRunSetup::resume(scoped, registry, &conversation, provider_resume)?
    } else {
        ToolRunSetup {
            broker: scoped,
            registry,
            task: TaskContext {
                objective: task.instruction.clone(),
                constraints: vec!["Use only the host-authorized tool capabilities. Ask for exact operation approval when required.".into()],
                state: "Foreground task admitted by the local owner.".into(),
            },
            budget,
            memory: Vec::new(),
            selected_tools: initial,
            limits,
            provider_resume,
        }
    };
    let mut events = agent
        .run_tools(
            conversation_id,
            command_id,
            provider,
            setup,
            cancellation.clone(),
        )
        .await?;
    let mut output = tokio::io::stdout();
    let mut provisional = String::new();
    let mut failure = None;
    let mut completed = false;
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                if failure.is_none() {
                    failure = Some(CliError::Agent(error));
                }
                cancellation.cancel();
                continue;
            }
        };
        if failure.is_some() {
            continue;
        }
        let rendered = async {
            match event {
                TurnEvent::ProvisionalText { delta, .. } => {
                    provisional.push_str(&delta);
                    output.write_all(delta.as_bytes()).await?;
                    output.flush().await?;
                }
                TurnEvent::Committed { message, summary } => {
                    let authoritative = message_text(&message);
                    if authoritative != provisional {
                        if !provisional.is_empty() { output.write_all(b"\n").await?; }
                        output.write_all(authoritative.as_bytes()).await?;
                    }
                    if !authoritative.is_empty() || !provisional.is_empty() { output.write_all(b"\n").await?; }
                    output.flush().await?;
                    provisional.clear();
                    eprintln!("saved assistant message {} ({:?})", summary.message_id, summary.stop_reason);
                }
                TurnEvent::ContextBuilt { usage, .. } => {
                    eprintln!("context input estimate {} tokens", usage.estimated_input_tokens);
                }
                TurnEvent::ModelAttemptStarted { attempt_id } => eprintln!("model attempt {attempt_id}"),
                TurnEvent::ToolCallRecorded { call } => eprintln!("saved tool call {} {}", call.id, call.name),
                TurnEvent::ToolResultRecorded { result } => eprintln!("saved tool result {} error={}", result.tool_call_id, result.is_error),
                TurnEvent::AwaitingPermission { approval, .. } => {
                    let request = serde_json::to_string_pretty(&approval).map_err(|_| CliError::Serialization)?;
                    eprintln!("exact operation approval required:\n{request}\nresume with --tools --approve {} or --deny {}", approval.id, approval.id);
                }
                TurnEvent::Paused { reason } => eprintln!("native run paused: {reason:?}"),
                TurnEvent::ToolBlocked { call_id, reason, diagnostic } => eprintln!("tool call {call_id} blocked ({reason:?}): {diagnostic}"),
                TurnEvent::RunCompleted { run } => {
                    completed = matches!(run.status, LoopStatus::Completed);
                    eprintln!("completed task {}; charged token budget {}", run.task_id, run.charged_tokens);
                }
            }
            Ok::<_, CliError>(())
        }.await;
        if let Err(error) = rendered {
            failure = Some(error);
            cancellation.cancel();
        }
    }
    drop(events);
    if let Some(error) = failure {
        return Err(error);
    }
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled.into());
    }
    if !completed {
        return Err(CliError::ToolPaused);
    }
    Ok(())
}

fn native_provider(conversation: &Conversation) -> Result<Arc<dyn ModelProvider>, CliError> {
    match conversation.config.provider {
        ProviderKind::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| CliError::Credential("ANTHROPIC_API_KEY"))?;
            let mut config = AnthropicConfig::new(key)?;
            if let ProviderEndpoint::Loopback(endpoint) = &conversation.config.endpoint {
                config = config.with_loopback_endpoint(endpoint)?;
            }
            Ok(Arc::new(AnthropicProvider::with_config(config)?))
        }
        ProviderKind::OpenAi => {
            let key = Zeroizing::new(
                std::env::var("OPENAI_API_KEY")
                    .map_err(|_| CliError::Credential("OPENAI_API_KEY"))?,
            );
            let mut config = OpenAiConfig::new(key.as_str())?;
            if let ProviderEndpoint::Loopback(endpoint) = &conversation.config.endpoint {
                config = config.with_loopback_endpoint(endpoint)?;
            }
            Ok(Arc::new(OpenAiProvider::with_config(config)?))
        }
    }
}

async fn read_message(path: PathBuf) -> Result<String, CliError> {
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
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
        #[cfg(not(unix))]
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_CHAT_TEXT_BYTES as u64 {
            return Err(CliError::MessageFile);
        }
        let mut bytes = Vec::new();
        file.take((MAX_CHAT_TEXT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_CHAT_TEXT_BYTES {
            return Err(CliError::MessageFile);
        }
        String::from_utf8(bytes).map_err(|_| CliError::MessageFile)
    })
    .await
    .map_err(|_| CliError::Worker)?
}

async fn write_json(value: impl Serialize + Send + 'static) -> Result<(), CliError> {
    let bytes = tokio::task::spawn_blocking(move || {
        serde_json::to_vec_pretty(&value).map_err(|_| CliError::Serialization)
    })
    .await
    .map_err(|_| CliError::Worker)??;
    let mut output = tokio::io::stdout();
    output.write_all(&bytes).await?;
    output.write_all(b"\n").await?;
    output.flush().await?;
    Ok(())
}

async fn print_message(message: &ConversationMessage) -> Result<(), CliError> {
    let mut output = tokio::io::stdout();
    output.write_all(message_text(message).as_bytes()).await?;
    output.write_all(b"\n").await?;
    output.flush().await?;
    Ok(())
}

fn message_text(message: &ConversationMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
