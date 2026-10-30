use std::io::{self, IsTerminal, Read};
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
    READ_ONLY_WRITES_UNAVAILABLE, ToolScopeRule,
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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

mod queries;
mod vault_cli;

const DEFAULT_INSTRUCTIONS: &str = "You are Everything, a helpful assistant.";
const VERSION: &str = match option_env!("EVERYTHING_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(version = VERSION, about = "Local-first native AI conversations")]
struct Arguments {
    #[arg(long, env = "EVERYTHING_STATE_DIR", value_name = "PATH")]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve(ServeArguments),
    Worker(everything::worker::commands::WorkerArguments),
    Schedule(everything::scheduling::commands::ScheduleArguments),
    Owner(OwnerArguments),
    Service(everything::service::cli::ServiceArguments),
    Init(InitArguments),
    Chat(ChatArguments),
    Status(StatusArguments),
    #[command(name = "chatgpt")]
    ChatGpt(ChatGptArguments),
    Tui(TuiArguments),
    Relay(everything::relay::cli::RelayArguments),
    Update(everything::update::UpdateArguments),
    Cancel(queries::CancelArguments),
    Tasks(queries::TasksArguments),
    Approvals(queries::ApprovalsArguments),
    Conversations(queries::ListArguments),
    Schedules(queries::ListArguments),
    Memory(queries::MemoryArguments),
    Vault(vault_cli::VaultArguments),
}

#[derive(Args, Default)]
struct TuiArguments {
    #[arg(long, conflicts_with = "resume_latest")]
    conversation: Option<ConversationId>,
    #[arg(short = 'c', long = "continue")]
    resume_latest: bool,
    #[arg(long, value_enum)]
    provider: Option<ProviderArgument>,
    #[arg(long)]
    model: Option<String>,
}

#[derive(Args)]
struct ChatGptArguments {
    #[command(subcommand)]
    command: ChatGptCommand,
}

#[derive(Subcommand)]
enum ChatGptCommand {
    SignIn(ChatGptSignInArguments),
    Status(ChatGptStatusArguments),
    Models(ChatGptModelsArguments),
    SignOut(ChatGptBindingArguments),
    Select(ChatGptBindingArguments),
    Deselect,
}

#[derive(Args)]
struct ChatGptBindingArguments {
    #[arg(long, requires = "storage")]
    reference: Option<uuid::Uuid>,
    #[arg(long, value_enum)]
    storage: Option<StorageArgument>,
}

#[derive(Args)]
struct ChatGptStatusArguments {
    #[command(flatten)]
    binding: ChatGptBindingArguments,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    show_email: bool,
}

#[derive(Args)]
struct ChatGptModelsArguments {
    #[command(flatten)]
    binding: ChatGptBindingArguments,
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ChatGptSignInArguments {
    #[command(flatten)]
    binding: ChatGptBindingArguments,
    #[arg(long)]
    no_browser: bool,
    #[arg(long, default_value_t = 600)]
    timeout_seconds: u64,
}

#[derive(Clone, Copy, ValueEnum)]
enum StorageArgument {
    Session,
    Native,
}

impl From<StorageArgument> for everything::host::settings::CredentialStorage {
    fn from(value: StorageArgument) -> Self {
        match value {
            StorageArgument::Session => Self::Session,
            StorageArgument::Native => Self::Native,
        }
    }
}

enum AgentCommand {
    Init(InitArguments),
    Chat(ChatArguments),
    Status(StatusArguments),
}

#[derive(Args)]
struct ServeArguments {
    #[arg(long, value_name = "PATH")]
    mcp_credential_environment_file: Option<PathBuf>,
    #[arg(long, env = "EVERYTHING_MAX_PARALLEL_TASKS", value_name = "1-4", value_parser = clap::value_parser!(u8).range(1..=4))]
    max_parallel_tasks: Option<u8>,
}

#[derive(Args)]
#[command(group(ArgGroup::new("owner_input").required(true).args(["request_file", "sensitive_stdin"])))]
struct OwnerArguments {
    #[arg(long, value_name = "PATH")]
    request_file: Option<PathBuf>,
    #[arg(long)]
    sensitive_stdin: bool,
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum OwnerInput {
    CredentialEnvironment {
        binding: everything::host::settings::ModelCredentialReference,
    },
    Query {
        query: everything::ipc::OwnerQuery,
    },
    Command {
        command: serde_json::Value,
    },
    Operation {
        operation: Box<everything::ipc::OwnerOperation>,
        request_id: Option<CommandId>,
    },
    Synchronize {
        request: everything::protocol::SyncRequest,
    },
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SensitiveOwnerInput {
    NativeSettings {
        input: everything::host::settings::native::NativeSettingsInput,
    },
    Credential {
        request_id: CommandId,
        operation: everything::host::credentials::CredentialOperation,
    },
    VaultPut {
        request_id: CommandId,
        item: everything::vault::VaultPutRequest,
    },
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
    #[arg(long)]
    no_wait: bool,
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
    WorkerCommand(#[from] everything::worker::commands::WorkerCommandError),
    #[error(
        "worker commands require --state-dir or EVERYTHING_STATE_DIR pointing to dedicated worker state"
    )]
    WorkerStateDirectory,
    #[error(transparent)]
    ScheduleCommand(#[from] everything::scheduling::commands::CommandError),
    #[error(transparent)]
    Host(#[from] everything::host::runtime::HostError),
    #[error(transparent)]
    HostClient(#[from] everything::host::client::HostClientError),
    #[error("invalid owner JSON input; use a bounded query or operation object")]
    OwnerInput,
    #[error("invalid owner JSON input: {0}")]
    OwnerJson(String),
    #[error("detached from the host task; check it with `everything tasks`")]
    Detached,
    #[error("native settings and credential input require the private stdin route")]
    SensitiveOwnerInput,
    #[error("private owner input or output was cancelled or exceeded its deadline")]
    SensitiveOwnerCancelled,
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
    #[error(
        "conversation {0} has no pending text turn to resume; its last turn already finished or was never accepted. Read the reply with `everything status --conversation {0} --history`, or start a new turn with `everything chat --conversation {0} --message TEXT`"
    )]
    NothingToResume(ConversationId),
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
    #[error("{0}")]
    ChatGpt(String),
    #[error(transparent)]
    Tui(#[from] everything::tui::TuiError),
    #[error(transparent)]
    Relay(#[from] everything::relay::cli::RelayCommandError),
    #[error(transparent)]
    Update(#[from] everything::update::UpdateError),
    #[error(transparent)]
    Vault(#[from] everything::vault::VaultError),
    #[error("{0} does not apply to {1} items")]
    VaultFlag(&'static str, String),
    #[error("{0} is not a secret field of {1} items")]
    VaultField(String, String),
    #[error("{0}")]
    VaultInput(&'static str),
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("everything: could not start async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_cli(arguments))
}

async fn run_cli(arguments: Arguments) -> ExitCode {
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
            if matches!(error, CliError::Detached)
                || matches!(error, CliError::Agent(ref error) if error.is_cancelled())
            {
                ExitCode::from(130)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

async fn execute(arguments: Arguments, cancellation: CancellationToken) -> Result<(), CliError> {
    if matches!(&arguments.command, Some(Command::Worker(_))) && arguments.state_dir.is_none() {
        return Err(CliError::WorkerStateDirectory);
    }
    let state_dir = arguments
        .state_dir
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".everything")))
        .ok_or(CliError::StateDirectory)?;
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled.into());
    }
    let command = match arguments
        .command
        .unwrap_or(Command::Tui(TuiArguments::default()))
    {
        Command::Worker(arguments) => {
            return write_json(
                everything::worker::commands::run(arguments, state_dir, cancellation).await?,
            )
            .await;
        }
        Command::Serve(arguments) => {
            let mut limits = everything::scheduler::SchedulerLimits::default();
            if let Some(maximum) = arguments.max_parallel_tasks {
                limits.max_primary_tasks = maximum.into();
            }
            return everything::host::runtime::HostService::start_with_mcp_session_bootstrap(
                &state_dir,
                arguments.mcp_credential_environment_file,
                limits,
                cancellation.clone(),
            )
            .await?
            .run(cancellation)
            .await
            .map_err(CliError::from);
        }
        Command::Schedule(arguments) => {
            return write_json(
                everything::scheduling::commands::run(arguments, state_dir, cancellation).await?,
            )
            .await;
        }
        Command::Owner(arguments) => return owner_cli(state_dir, arguments, cancellation).await,
        Command::Cancel(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return queries::cancel(&client, arguments, cancellation).await;
        }
        Command::Tasks(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return queries::tasks(&client, arguments, cancellation).await;
        }
        Command::Approvals(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return queries::approvals(&client, arguments, cancellation).await;
        }
        Command::Conversations(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return queries::conversations(&client, arguments, cancellation).await;
        }
        Command::Schedules(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return queries::schedules(&client, arguments, cancellation).await;
        }
        Command::Memory(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return queries::memory(&client, arguments, cancellation).await;
        }
        Command::Vault(arguments) => {
            let client = everything::host::client::HostClient::new(state_dir)?;
            return vault_cli::run(&client, arguments, cancellation).await;
        }
        Command::Tui(arguments) => {
            return everything::tui::run(
                state_dir,
                everything::tui::Options {
                    conversation: arguments.conversation,
                    resume_latest: arguments.resume_latest,
                    provider: arguments.provider.map(Into::into),
                    model: arguments.model,
                    endpoint: None,
                    default_instructions: DEFAULT_INSTRUCTIONS,
                },
                cancellation,
            )
            .await
            .map_err(CliError::from);
        }
        Command::Relay(arguments) => {
            return everything::relay::cli::run(arguments, state_dir, cancellation)
                .await
                .map_err(CliError::from);
        }
        Command::Update(arguments) => {
            return everything::update::run(arguments, VERSION, state_dir, cancellation)
                .await
                .map_err(CliError::from);
        }
        Command::ChatGpt(arguments) => {
            return chatgpt_cli(state_dir, arguments.command, cancellation).await;
        }
        Command::Service(arguments) => {
            return write_json(
                everything::service::cli::manage(arguments, state_dir, cancellation).await?,
            )
            .await;
        }
        Command::Init(arguments) => AgentCommand::Init(arguments),
        Command::Status(arguments) => AgentCommand::Status(arguments),
        Command::Chat(arguments) => AgentCommand::Chat(arguments),
    };
    let agent = match Agent::open(&state_dir).await {
        Ok(agent) => agent,
        Err(AgentError::Service(everything::service::ServiceError::AlreadyRunning)) => {
            return proxy_cli(state_dir, command, cancellation).await;
        }
        Err(error) => return Err(error.into()),
    };
    match command {
        AgentCommand::Init(arguments) => {
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
        AgentCommand::Status(arguments) => match arguments.conversation {
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
        },
        AgentCommand::Chat(arguments) => chat(agent, arguments, cancellation).await,
    }
}

async fn owner_cli(
    state: PathBuf,
    arguments: OwnerArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    if arguments.sensitive_stdin {
        let input_descriptor = rustix::io::fcntl_dupfd_cloexec(io::stdin(), 3)
            .map_err(|_| CliError::SensitiveOwnerInput)?;
        let output_descriptor = rustix::io::fcntl_dupfd_cloexec(io::stdout(), 3)
            .map_err(|_| CliError::SensitiveOwnerInput)?;
        let input = tokio::net::unix::pipe::Receiver::from_owned_fd(input_descriptor)
            .map_err(|_| CliError::SensitiveOwnerInput)?;
        let output = tokio::net::unix::pipe::Sender::from_owned_fd(output_descriptor)
            .map_err(|_| CliError::SensitiveOwnerInput)?;
        let input = read_sensitive_input(input, cancellation.clone()).await?;
        let client = everything::host::client::HostClient::new(state)?;
        return match input {
            SensitiveOwnerInput::NativeSettings { input } => {
                let result = client.native_settings(input, cancellation.clone()).await?;
                write_sensitive_json(output, result, cancellation).await
            }
            SensitiveOwnerInput::Credential {
                request_id,
                operation,
            } => {
                write_sensitive_json(
                    output,
                    client
                        .operate(
                            everything::ipc::OwnerOperation::Credential { operation },
                            request_id,
                            cancellation.clone(),
                        )
                        .await?,
                    cancellation,
                )
                .await
            }
            SensitiveOwnerInput::VaultPut { request_id, item } => {
                write_sensitive_json(
                    output,
                    client
                        .operate(
                            everything::ipc::OwnerOperation::VaultPut { item },
                            request_id,
                            cancellation.clone(),
                        )
                        .await?,
                    cancellation,
                )
                .await
            }
        };
    }
    let request_file = arguments.request_file.ok_or(CliError::OwnerInput)?;
    let input = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::from(
            rustix::fs::open(
                request_file,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 1_048_576 {
            return Err(CliError::OwnerInput);
        }
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(1_048_577).read_to_end(&mut bytes)?;
        if bytes.len() > 1_048_576 {
            return Err(CliError::OwnerInput);
        }
        serde_json::from_slice::<OwnerInput>(&bytes).map_err(|error| {
            CliError::OwnerJson(
                serde_json::from_slice(&bytes)
                    .ok()
                    .and_then(|mut value| {
                        everything::ipc::decode_json::<OwnerInput>(&mut value).err()
                    })
                    .map_or_else(|| error.to_string(), |error| error.to_string()),
            )
        })
    })
    .await
    .map_err(|_| CliError::Worker)??;
    let client = everything::host::client::HostClient::new(state)?;
    match input {
        OwnerInput::CredentialEnvironment { binding } => {
            let variable = match binding.provider {
                ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                ProviderKind::OpenAi => "OPENAI_API_KEY",
            };
            let value = std::env::var(variable).map_err(|_| CliError::Credential(variable))?;
            let secret = everything::host::credentials::SecretTransfer::new(value)
                .map_err(|_| CliError::Credential(variable))?;
            write_json(
                client
                    .operate(
                        everything::ipc::OwnerOperation::Credential {
                            operation: everything::host::credentials::CredentialOperation::Store {
                                binding,
                                secret,
                            },
                        },
                        CommandId::new(),
                        cancellation,
                    )
                    .await?,
            )
            .await
        }
        OwnerInput::Query { query } => write_json(client.query(query, cancellation).await?).await,
        OwnerInput::Command { command } => {
            let typed: everything::protocol::CommandEnvelope =
                everything::ipc::decode_json(&mut command.clone()).map_err(|error| {
                    CliError::OwnerJson(
                        everything::ipc::JsonFieldError {
                            path: match error.path.as_str() {
                                "" => "command".to_owned(),
                                path => format!("command.{path}"),
                            },
                            message: error.message,
                        }
                        .to_string(),
                    )
                })?;
            if serde_json::to_value(&typed).map_err(|_| CliError::Serialization)? != command {
                return Err(CliError::OwnerInput);
            }
            write_json(client.submit_domain(typed, cancellation).await?).await
        }
        OwnerInput::Operation {
            operation,
            request_id,
        } => {
            if matches!(
                &*operation,
                everything::ipc::OwnerOperation::Credential { .. }
                    | everything::ipc::OwnerOperation::VaultPut { .. }
            ) {
                return Err(CliError::SensitiveOwnerInput);
            }
            let request_id = match (&*operation, request_id) {
                (everything::ipc::OwnerOperation::SetSettingsGrant { change }, Some(original))
                    if original == change.request_id =>
                {
                    original
                }
                (everything::ipc::OwnerOperation::SetSettingsGrant { .. }, _) => {
                    return Err(CliError::OwnerInput);
                }
                (_, original) => original.unwrap_or_default(),
            };
            eprintln!("owner request {request_id}; reuse this ID for an explicit retry");
            write_json(client.operate(*operation, request_id, cancellation).await?).await
        }
        OwnerInput::Synchronize { request } => {
            request.validate().map_err(|_| CliError::OwnerInput)?;
            match client
                .request(
                    everything::transport::local::LocalRequestPayload::Synchronize { request },
                    cancellation,
                )
                .await?
            {
                response @ everything::transport::local::LocalResponsePayload::Synchronized {
                    ..
                } => write_json(response).await,
                _ => Err(CliError::OwnerInput),
            }
        }
    }
}

async fn chatgpt_cli(
    state: PathBuf,
    command: ChatGptCommand,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    use everything::host::chatgpt::{ChatGptOperation, ChatGptRevocation};
    use everything::host::settings::{ModelCredentialReference, ModelCredentialSource};
    use everything::ipc::{OwnerQuery, OwnerQueryResult};
    let client = everything::host::client::HostClient::new(state)?;
    let OwnerQueryResult::Settings { settings } = client
        .query(OwnerQuery::Settings, cancellation.clone())
        .await?
    else {
        return Err(CliError::OwnerInput);
    };
    let configured = settings
        .model_credentials
        .iter()
        .find(|binding| binding.source == ModelCredentialSource::ChatGpt)
        .cloned();
    let resolve = |arguments: &ChatGptBindingArguments, create: bool| {
        let storage = arguments.storage.map(Into::into);
        let binding = |reference, storage| ModelCredentialReference {
            provider: ProviderKind::OpenAi,
            endpoint: ProviderEndpoint::Official,
            reference,
            storage,
            source: ModelCredentialSource::ChatGpt,
        };
        match (&configured, arguments.reference, storage) {
            (Some(configured), reference, storage)
                if reference.is_none_or(|value| value == configured.reference)
                    && storage.is_none_or(|value| value == configured.storage) =>
            {
                Ok(configured.clone())
            }
            (Some(_), _, _) => Err(CliError::ChatGpt(
                "--reference/--storage differ from the ChatGPT binding selected in settings".into(),
            )),
            (None, Some(reference), Some(storage)) => Ok(binding(reference, storage)),
            (None, None, storage) if create => Ok(binding(
                uuid::Uuid::new_v4(),
                storage.unwrap_or(everything::host::settings::CredentialStorage::Native),
            )),
            (None, _, _) => Err(CliError::ChatGpt(
                "no ChatGPT binding is selected in settings; pass --reference and --storage session|native for an existing sign-in".into(),
            )),
        }
    };
    match command {
        ChatGptCommand::Status(arguments) => {
            let binding = resolve(&arguments.binding, false)?;
            let status =
                chatgpt_operate(&client, ChatGptOperation::Status { binding }, cancellation)
                    .await?;
            if arguments.json || arguments.show_email {
                write_json(status).await
            } else {
                write_json(mask_email(status)).await
            }
        }
        ChatGptCommand::Models(arguments) => {
            let binding = resolve(&arguments.binding, false)?;
            let everything::ipc::OwnerOperationResult::ChatGptModels { models, .. } = client
                .operate(
                    everything::ipc::OwnerOperation::ChatGptModels { binding },
                    CommandId::new(),
                    cancellation,
                )
                .await?
            else {
                return Err(CliError::OwnerInput);
            };
            let models: Vec<_> = models
                .into_iter()
                .filter(|model| arguments.all || model.visibility.as_deref() == Some("list"))
                .collect();
            if arguments.json {
                return write_json(models).await;
            }
            let mut lines = String::new();
            for model in &models {
                let name = model.display_name.as_deref().unwrap_or("-");
                if arguments.all {
                    let visibility = model.visibility.as_deref().unwrap_or("-");
                    lines.push_str(&format!("{}\t{name}\t{visibility}\n", model.slug));
                } else {
                    lines.push_str(&format!("{}\t{name}\n", model.slug));
                }
            }
            if models.is_empty() {
                eprintln!("the ChatGPT plan lists no models; try --all");
            }
            let mut output = tokio::io::stdout();
            output.write_all(lines.as_bytes()).await?;
            output.flush().await?;
            Ok(())
        }
        ChatGptCommand::SignOut(arguments) => {
            let binding = resolve(&arguments, false)?;
            let status =
                chatgpt_operate(&client, ChatGptOperation::SignOut { binding }, cancellation)
                    .await?;
            if status.revocation == Some(ChatGptRevocation::Unconfirmed) {
                eprintln!(
                    "signed out locally, but OpenAI did not confirm revocation; disconnect Everything in ChatGPT settings if needed"
                );
            }
            write_json(mask_email(status)).await
        }
        ChatGptCommand::Select(arguments) => {
            let binding = resolve(&arguments, false)?;
            if let Some(existing) = settings.model_credentials.iter().find(|existing| {
                existing.provider == ProviderKind::OpenAi
                    && existing.endpoint == ProviderEndpoint::Official
            }) {
                return Err(CliError::ChatGpt(if *existing == binding {
                    "ChatGPT plan usage is already selected for the official OpenAI endpoint".into()
                } else {
                    format!(
                        "the official OpenAI endpoint already uses credential reference {}; remove it from settings model_credentials before selecting ChatGPT",
                        existing.reference
                    )
                }));
            }
            let mut updated = (*settings).clone();
            updated.model_credentials.push(binding);
            chatgpt_replace_settings(&client, updated, settings.revision, cancellation).await
        }
        ChatGptCommand::Deselect => {
            if configured.is_none() {
                return Err(CliError::ChatGpt(
                    "ChatGPT plan usage is not selected in settings".into(),
                ));
            }
            let mut updated = (*settings).clone();
            updated
                .model_credentials
                .retain(|binding| binding.source != ModelCredentialSource::ChatGpt);
            chatgpt_replace_settings(&client, updated, settings.revision, cancellation).await
        }
        ChatGptCommand::SignIn(arguments) => {
            let binding = resolve(&arguments.binding, true)?;
            chatgpt_sign_in(
                &client,
                binding,
                configured.is_some(),
                arguments,
                cancellation,
            )
            .await
        }
    }
}

async fn chatgpt_operate(
    client: &everything::host::client::HostClient,
    operation: everything::host::chatgpt::ChatGptOperation,
    cancellation: CancellationToken,
) -> Result<everything::host::chatgpt::ChatGptStatus, CliError> {
    match client
        .operate(
            everything::ipc::OwnerOperation::ChatGpt { operation },
            CommandId::new(),
            cancellation,
        )
        .await?
    {
        everything::ipc::OwnerOperationResult::ChatGpt { status } => Ok(*status),
        _ => Err(CliError::OwnerInput),
    }
}

fn mask_email(
    mut status: everything::host::chatgpt::ChatGptStatus,
) -> everything::host::chatgpt::ChatGptStatus {
    status.email = status.email.map(|email| {
        email
            .rsplit_once('@')
            .map_or_else(|| "…".to_owned(), |(_, domain)| format!("…@{domain}"))
    });
    status
}

async fn chatgpt_replace_settings(
    client: &everything::host::client::HostClient,
    settings: everything::host::settings::HostSettings,
    expected_revision: u64,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let request_id = CommandId::new();
    eprintln!("owner request {request_id}; reuse this ID for an explicit retry");
    write_json(
        client
            .operate(
                everything::ipc::OwnerOperation::ReplaceSettings {
                    expected_revision,
                    settings: Box::new(settings),
                },
                request_id,
                cancellation,
            )
            .await?,
    )
    .await
}

async fn chatgpt_sign_in(
    client: &everything::host::client::HostClient,
    binding: everything::host::settings::ModelCredentialReference,
    selected: bool,
    arguments: ChatGptSignInArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    use everything::host::chatgpt::{ChatGptAttemptPhase, ChatGptOperation};
    let status = chatgpt_operate(
        client,
        ChatGptOperation::SignIn {
            binding: binding.clone(),
            timeout_seconds: arguments.timeout_seconds,
        },
        cancellation.clone(),
    )
    .await?;
    let attempt = status.attempt.ok_or(CliError::OwnerInput)?;
    let authorize_url = attempt.authorize_url.ok_or(CliError::OwnerInput)?;
    eprintln!(
        "Continue with ChatGPT in your browser:\n{authorize_url}\nWaiting for the callback on {}; press Ctrl-C to cancel.",
        attempt.redirect_uri
    );
    if !arguments.no_browser {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let opened = tokio::process::Command::new(program)
            .arg(&authorize_url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if !opened.is_ok_and(|status| status.success()) {
            eprintln!("could not open a browser; open the URL above manually");
        }
    }
    let status = loop {
        let waited = chatgpt_operate(
            client,
            ChatGptOperation::Wait {
                attempt_id: attempt.attempt_id,
                timeout_ms: 25_000,
            },
            cancellation.clone(),
        )
        .await;
        match waited {
            Ok(status)
                if status.attempt.as_ref().is_some_and(|current| {
                    current.attempt_id == attempt.attempt_id
                        && current.phase == ChatGptAttemptPhase::Pending
                }) => {}
            Ok(status) => break status,
            Err(_) if cancellation.is_cancelled() => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    chatgpt_operate(
                        client,
                        ChatGptOperation::Cancel {
                            attempt_id: attempt.attempt_id,
                        },
                        CancellationToken::new(),
                    ),
                )
                .await;
                return Err(CliError::ChatGpt(
                    "ChatGPT sign-in cancelled; the loopback listener was stopped".into(),
                ));
            }
            Err(error) => return Err(error),
        }
    };
    let phase = status
        .attempt
        .as_ref()
        .filter(|current| current.attempt_id == attempt.attempt_id)
        .map(|current| current.phase.clone());
    let plan_usage_enabled = status.plan_usage_enabled;
    write_json(mask_email(status)).await?;
    match phase {
        Some(ChatGptAttemptPhase::Completed) => {
            if !plan_usage_enabled {
                eprintln!(
                    "signed in, but ChatGPT plan usage was not granted; sign in again and allow plan usage, or keep using an API key"
                );
            }
            if !selected {
                eprintln!(
                    "ChatGPT plan usage is not selected; API-key credentials remain in use. Select it with: everything chatgpt select --reference {} --storage {}",
                    binding.reference,
                    match binding.storage {
                        everything::host::settings::CredentialStorage::Session => "session",
                        everything::host::settings::CredentialStorage::Native => "native",
                    }
                );
            }
            Ok(())
        }
        Some(ChatGptAttemptPhase::Failed { reason }) => Err(CliError::ChatGpt(format!(
            "ChatGPT sign-in did not complete: {reason}"
        ))),
        Some(ChatGptAttemptPhase::Expired) => Err(CliError::ChatGpt(
            "ChatGPT sign-in expired before the browser returned".into(),
        )),
        Some(ChatGptAttemptPhase::Cancelled) => {
            Err(CliError::ChatGpt("ChatGPT sign-in was cancelled".into()))
        }
        _ => Err(CliError::ChatGpt(
            "ChatGPT sign-in was replaced by another attempt".into(),
        )),
    }
}

async fn proxy_cli(
    state: PathBuf,
    command: AgentCommand,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    use everything::ipc::{OwnerCommand, OwnerQuery, OwnerQueryResult};
    let client = everything::host::client::HostClient::new(state)?;
    match command {
        AgentCommand::Init(arguments) => {
            let conversation_id = arguments.conversation.unwrap_or_default();
            let command_id = arguments.command_id.unwrap_or_default();
            eprintln!("conversation {conversation_id} initialization command {command_id}");
            client
                .submit(
                    OwnerCommand::Initialize {
                        conversation_id,
                        command_id,
                        title: arguments.title,
                        configuration: arguments.model.create()?,
                    },
                    cancellation.clone(),
                )
                .await?;
            match client
                .query(OwnerQuery::Conversation { conversation_id }, cancellation)
                .await?
            {
                OwnerQueryResult::Conversation { information, .. } => write_json(information).await,
                _ => Err(CliError::OwnerInput),
            }
        }
        AgentCommand::Status(arguments) => match arguments.conversation {
            None => match client
                .query(
                    OwnerQuery::Conversations {
                        limit: arguments.limit,
                    },
                    cancellation,
                )
                .await?
            {
                OwnerQueryResult::Conversations { conversations } => {
                    write_json(conversations).await
                }
                _ => Err(CliError::OwnerInput),
            },
            Some(conversation_id) => {
                let result = client
                    .query(
                        OwnerQuery::Conversation { conversation_id },
                        cancellation.clone(),
                    )
                    .await?;
                let OwnerQueryResult::Conversation { information, .. } = result else {
                    return Err(CliError::OwnerInput);
                };
                if arguments.history {
                    let messages = proxy_messages(
                        &client,
                        conversation_id,
                        information.last_sequence,
                        cancellation,
                    )
                    .await?;
                    write_json(serde_json::json!({"conversation":information,"messages":messages}))
                        .await
                } else {
                    write_json(information).await
                }
            }
        },
        AgentCommand::Chat(arguments) => proxy_chat(&client, arguments, cancellation).await,
    }
}

async fn proxy_messages(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    through: u64,
    cancellation: CancellationToken,
) -> Result<Vec<everything::agent::RecordedMessage>, CliError> {
    use everything::protocol::{ConversationEvent, SyncCursor, SyncRequest};
    use everything::transport::local::{LocalRequestPayload, LocalResponsePayload};
    let mut cursor = SyncCursor {
        conversation_id,
        after_sequence: 0,
    };
    let mut messages = Vec::new();
    let mut bytes = 0_usize;
    let mut count = 0_usize;
    while cursor.after_sequence < through {
        let response = client
            .request(
                LocalRequestPayload::Synchronize {
                    request: SyncRequest::new(vec![cursor], 256).map_err(AgentError::from)?,
                },
                cancellation.clone(),
            )
            .await?;
        let LocalResponsePayload::Synchronized { batch } = response else {
            return Err(CliError::OwnerInput);
        };
        if batch.events.is_empty() {
            return Err(CliError::OwnerInput);
        }
        cursor = batch.next_cursor();
        for event in batch
            .events
            .into_iter()
            .take_while(|event| event.sequence.get() <= through)
        {
            bytes = bytes.saturating_add(
                serde_json::to_vec(&event)
                    .map_err(|_| CliError::Serialization)?
                    .len(),
            );
            count += 1;
            if bytes > 64 * 1024 * 1024 || count > 65_536 {
                return Err(AgentError::ResourceLimit("CLI history").into());
            }
            if let ConversationEvent::MessageAppended { message } = event.payload {
                messages.push(everything::agent::RecordedMessage {
                    event_id: event.event_id,
                    sequence: event.sequence,
                    causation_id: event.causation_id,
                    message,
                });
            }
        }
    }
    Ok(messages)
}

async fn proxy_chat(
    client: &everything::host::client::HostClient,
    arguments: ChatArguments,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    use everything::ipc::{
        OwnerCommand, OwnerOperation, OwnerQuery, OwnerQueryResult, RequestedRunOptions,
    };
    use everything::protocol::TaskId;
    let text = match (arguments.message, arguments.message_file) {
        (Some(message), None) => Some(message),
        (None, Some(path)) => Some(read_message(path).await?),
        _ => None,
    };
    let (conversation_id, information) = match arguments.conversation {
        Some(conversation_id) => match client
            .query(
                OwnerQuery::Conversation { conversation_id },
                cancellation.clone(),
            )
            .await?
        {
            OwnerQueryResult::Conversation {
                information,
                configuration,
            } => {
                arguments.model.check(&configuration)?;
                (conversation_id, information)
            }
            _ => return Err(CliError::OwnerInput),
        },
        None => {
            let conversation_id = ConversationId::new();
            let command_id = CommandId::new();
            eprintln!("conversation {conversation_id} initialization command {command_id}");
            client
                .submit(
                    OwnerCommand::Initialize {
                        conversation_id,
                        command_id,
                        title: arguments.title.unwrap_or_else(|| {
                            text.as_deref()
                                .unwrap_or("New conversation")
                                .chars()
                                .take(80)
                                .collect()
                        }),
                        configuration: arguments.model.create()?,
                    },
                    cancellation.clone(),
                )
                .await?;
            let OwnerQueryResult::Conversation { information, .. } = client
                .query(
                    OwnerQuery::Conversation { conversation_id },
                    cancellation.clone(),
                )
                .await?
            else {
                return Err(CliError::OwnerInput);
            };
            (conversation_id, information)
        }
    };
    let command_id = if arguments.resume {
        let command_id = information
            .pending_command_id
            .ok_or(CliError::NothingToResume(conversation_id))?;
        if arguments.command_id.is_some_and(|id| id != command_id) {
            return Err(AgentError::CommandConflict(command_id).into());
        }
        if arguments.tools.workspace.is_some()
            || arguments.tools.execution_mode.is_some()
            || arguments.tools.context_tokens.is_some()
            || arguments.tools.max_model_attempts.is_some()
            || arguments.tools.max_tool_calls.is_some()
            || arguments.tools.max_charged_tokens.is_some()
            || arguments.tools.max_duration_seconds.is_some()
        {
            return Err(CliError::ToolOptions(
                "host resumes use the captured execution policy; omit all workspace, mode and budget overrides",
            ));
        }
        let task_id = TaskId::try_from(*command_id.as_uuid()).map_err(AgentError::from)?;
        if let Some((id, decision)) = arguments
            .tools
            .approve
            .map(|id| (id, everything::protocol::PermissionDecision::Allow))
            .or_else(|| {
                arguments
                    .tools
                    .deny
                    .map(|id| (id, everything::protocol::PermissionDecision::Deny))
            })
        {
            resolve_host_approval(
                client,
                conversation_id,
                task_id,
                id,
                decision,
                &cancellation,
            )
            .await?;
        } else {
            let operation = if arguments.tools.retry_uncertain {
                match information.native_status.clone() {
                    Some(LoopStatus::ProviderInFlight { attempt_id }) => {
                        OwnerOperation::RetryProvider {
                            task_id,
                            expected_attempt: attempt_id,
                        }
                    }
                    Some(
                        status @ LoopStatus::Paused {
                            reason:
                                everything::agent::PauseReason::Cancellation
                                | everything::agent::PauseReason::InterruptedTool,
                        },
                    ) => OwnerOperation::ResumeInterrupted {
                        task_id,
                        expected_command: command_id,
                        expected_revision: information.checkpoint_revision,
                        expected_status: status,
                    },
                    _ => {
                        return Err(CliError::ToolOptions(
                            "no exact uncertain provider attempt or known interruption is available for this explicit retry",
                        ));
                    }
                }
            } else {
                OwnerOperation::WakeTask { task_id }
            };
            let request_id = CommandId::new();
            eprintln!(
                "explicit resume request {request_id}; reuse this ID through owner IPC if its outcome is ambiguous"
            );
            client
                .operate(operation, request_id, cancellation.clone())
                .await?;
        }
        command_id
    } else {
        let command_id = arguments.command_id.unwrap_or_default();
        eprintln!(
            "conversation {conversation_id} command {command_id}; closing this CLI leaves admitted work in the host"
        );
        let workspace = arguments
            .tools
            .workspace
            .map(|path| {
                if path.is_absolute() {
                    Ok(path)
                } else {
                    std::env::current_dir().map(|current| current.join(path))
                }
            })
            .transpose()?;
        let receipt = client
            .submit(
                OwnerCommand::Text {
                    command: TextCommand {
                        conversation_id,
                        command_id,
                        text: text.ok_or(CliError::Required("--message or --message-file"))?,
                    },
                    options: RequestedRunOptions {
                        workspace,
                        execution_mode: arguments.tools.execution_mode.map(Into::into),
                        tools: Some(arguments.tools.tools),
                        context_tokens: arguments.tools.context_tokens,
                        max_model_attempts: arguments.tools.max_model_attempts,
                        max_tool_calls: arguments.tools.max_tool_calls,
                        max_charged_tokens: arguments.tools.max_charged_tokens,
                        max_duration_seconds: arguments.tools.max_duration_seconds,
                    },
                },
                cancellation.clone(),
            )
            .await?;
        if receipt
            .applied
            .as_ref()
            .is_none_or(|receipt| receipt.task_id.is_none())
        {
            let OwnerQueryResult::Conversation { information, .. } = client
                .query(
                    OwnerQuery::Conversation { conversation_id },
                    cancellation.clone(),
                )
                .await?
            else {
                return Err(CliError::OwnerInput);
            };
            return proxy_reply(
                client,
                conversation_id,
                command_id,
                &information,
                "",
                cancellation,
            )
            .await;
        }
        command_id
    };
    let task_id = TaskId::try_from(*command_id.as_uuid()).map_err(AgentError::from)?;
    let wait = !arguments.no_wait && io::stdin().is_terminal() && io::stdout().is_terminal();
    match follow_host_task(
        client,
        conversation_id,
        command_id,
        task_id,
        information,
        wait,
        cancellation.clone(),
    )
    .await
    {
        Err(_) if cancellation.is_cancelled() => {
            interrupt_host_task(client, conversation_id, task_id).await
        }
        result => result,
    }
}

async fn interrupt_host_task(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    task_id: everything::protocol::TaskId,
) -> Result<(), CliError> {
    eprintln!("\ncancelling host task {task_id}; press Ctrl-C again to detach");
    let detach = CancellationToken::new();
    tokio::select! {
        Ok(()) = shutdown_signal() => {
            eprintln!(
                "\nconversation {conversation_id} task {task_id}; closing this CLI leaves admitted work in the host"
            );
            Err(CliError::Detached)
        }
        result = queries::cancel_task(client, task_id, &detach) => {
            eprintln!("{}", queries::describe_cancellation(&result?));
            Err(AgentError::Cancelled.into())
        }
    }
}

async fn follow_host_task(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    command_id: CommandId,
    task_id: everything::protocol::TaskId,
    mut information: everything::ipc::ConversationSummary,
    wait: bool,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    use everything::ipc::{OwnerQuery, OwnerQueryResult};
    let mut lines = None;
    let mut reported = None;
    let mut streamed = None::<everything::scheduler::LiveText>;
    let mut output = tokio::io::stdout();
    loop {
        match client
            .query(OwnerQuery::Task { task_id }, cancellation.clone())
            .await?
        {
            OwnerQueryResult::Task { task, .. }
                if task.status == everything::protocol::TaskStatus::Completed =>
            {
                let OwnerQueryResult::Conversation { information, .. } = client
                    .query(
                        OwnerQuery::Conversation { conversation_id },
                        cancellation.clone(),
                    )
                    .await?
                else {
                    return Err(CliError::OwnerInput);
                };
                return proxy_reply(
                    client,
                    conversation_id,
                    command_id,
                    &information,
                    streamed.as_ref().map_or("", |live| live.text.as_str()),
                    cancellation,
                )
                .await;
            }
            OwnerQueryResult::Task { task, execution }
                if matches!(
                    task.status,
                    everything::protocol::TaskStatus::Queued
                        | everything::protocol::TaskStatus::Running
                ) || (task.status == everything::protocol::TaskStatus::WaitingForChildren
                    && !execution.cancel_requested
                    && matches!(
                        execution.waiting,
                        Some(everything::scheduler::WaitingReason::Children { .. })
                    )) =>
            {
                if execution.diagnostic != reported {
                    if let Some(diagnostic) = &execution.diagnostic {
                        eprintln!("host task {task_id} {:?}: {diagnostic}", task.status);
                    }
                    reported = execution.diagnostic;
                }
                if let Some(live) = execution.live_text {
                    if streamed
                        .as_ref()
                        .is_some_and(|previous| previous.attempt_id != live.attempt_id)
                        && streamed
                            .take()
                            .is_some_and(|previous| !previous.text.is_empty())
                    {
                        output.write_all(b"\n").await?;
                    }
                    let printed = streamed
                        .as_ref()
                        .map_or("", |previous| previous.text.as_str());
                    if let Some(rest) = live.text.strip_prefix(printed) {
                        output.write_all(rest.as_bytes()).await?;
                        output.flush().await?;
                        streamed = Some(live);
                    }
                }
            }
            OwnerQueryResult::Task { task, .. }
                if task.status == everything::protocol::TaskStatus::AwaitingPermission =>
            {
                if !wait {
                    print_host_approvals(client, conversation_id, task_id, &cancellation).await?;
                    return Err(CliError::ToolPaused);
                }
                await_host_approval(client, conversation_id, task_id, &mut lines, &cancellation)
                    .await?;
            }
            OwnerQueryResult::Task { task, execution } => {
                match &execution.waiting {
                    Some(waiting) => eprintln!(
                        "host task {task_id} stopped at {waiting:?}; inspect owner task/approval metadata before an explicit resume"
                    ),
                    None => eprintln!(
                        "host task {task_id} stopped at {:?}; inspect owner task/approval metadata before an explicit resume",
                        task.status
                    ),
                }
                if let Some(diagnostic) = &execution.diagnostic {
                    eprintln!("host task {task_id} diagnostic: {diagnostic}");
                }
                if matches!(
                    execution.waiting,
                    Some(everything::scheduler::WaitingReason::ProviderUnavailable)
                ) && let ProviderEndpoint::Loopback(endpoint) = &information.endpoint
                    && let OwnerQueryResult::Settings { settings } = client
                        .query(OwnerQuery::Settings, cancellation.clone())
                        .await?
                    && !settings.model_credentials.iter().any(|binding| {
                        binding.provider == information.provider
                            && binding.endpoint == information.endpoint
                    })
                {
                    let variable = match information.provider {
                        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                        ProviderKind::OpenAi => "OPENAI_API_KEY",
                    };
                    let binding = everything::host::settings::ModelCredentialReference {
                        provider: information.provider,
                        endpoint: information.endpoint.clone(),
                        reference: uuid::Uuid::new_v4(),
                        storage: everything::host::settings::CredentialStorage::Session,
                        source: Default::default(),
                    };
                    eprintln!(
                        "the host does not read {variable} for loopback endpoint {endpoint}; with {variable} set, run `everything owner --request-file FILE` with {} and add the same binding to settings model_credentials",
                        serde_json::json!({"kind": "credential_environment", "binding": binding})
                    );
                }
                return Err(CliError::ToolPaused);
            }
            OwnerQueryResult::LegacyForeground { .. } => {
                return Err(CliError::ToolOptions(
                    "stop the host and explicitly settle this legacy foreground task",
                ));
            }
            _ => return Err(CliError::OwnerInput),
        }
        let OwnerQueryResult::Conversation {
            information: current,
            ..
        } = client
            .query(
                OwnerQuery::Conversation { conversation_id },
                cancellation.clone(),
            )
            .await?
        else {
            return Err(CliError::OwnerInput);
        };
        information = current;
        client
            .request(
                everything::transport::local::LocalRequestPayload::Wait {
                    request: everything::ipc::ConversationWait {
                        conversation_id,
                        after_sequence: information.last_sequence,
                        timeout_ms: 1000,
                    },
                },
                cancellation.clone(),
            )
            .await?;
    }
}

async fn resolve_host_approval(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    task_id: everything::protocol::TaskId,
    id: String,
    decision: everything::protocol::PermissionDecision,
    cancellation: &CancellationToken,
) -> Result<(), CliError> {
    use everything::ipc::{ApprovalLookup, OwnerQuery, OwnerQueryResult};
    let OwnerQueryResult::Approval { approval } = client
        .query(
            OwnerQuery::Approval {
                conversation_id,
                lookup: ApprovalLookup::Approval(id),
            },
            cancellation.clone(),
        )
        .await?
    else {
        return Err(CliError::OwnerInput);
    };
    let approval = approval
        .filter(|approval| {
            approval.status == everything::permissions::ApprovalStatus::Pending
                && approval.request.scope.task_id == task_id.to_string()
        })
        .ok_or(CliError::ToolOptions(
            "approval does not belong to this pending task",
        ))?
        .request;
    let operation_id = approval
        .scope
        .operation_id
        .parse()
        .map_err(AgentError::from)?;
    let command = everything::protocol::CommandEnvelope::new(
        conversation_id,
        None,
        everything::protocol::UnixMillis::now().map_err(AgentError::from)?,
        everything::protocol::Command::ResolvePermission {
            operation_id,
            decision,
        },
    );
    client.submit_domain(command, cancellation.clone()).await?;
    Ok(())
}

async fn pending_host_approvals(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    task_id: everything::protocol::TaskId,
    cancellation: &CancellationToken,
) -> Result<Vec<everything::permissions::ApprovalRequest>, CliError> {
    use everything::ipc::{ApprovalLookup, OwnerQuery, OwnerQueryResult};
    let mut pending = Vec::new();
    let mut cursor = None;
    loop {
        let OwnerQueryResult::Approvals {
            approvals,
            next_cursor,
        } = client
            .query(
                OwnerQuery::Approvals {
                    conversation_id,
                    limit: 100,
                    cursor,
                },
                cancellation.clone(),
            )
            .await?
        else {
            return Err(CliError::OwnerInput);
        };
        for summary in approvals
            .into_iter()
            .filter(|summary| summary.task_id == task_id.to_string())
        {
            let OwnerQueryResult::Approval { approval } = client
                .query(
                    OwnerQuery::Approval {
                        conversation_id,
                        lookup: ApprovalLookup::Approval(summary.id),
                    },
                    cancellation.clone(),
                )
                .await?
            else {
                return Err(CliError::OwnerInput);
            };
            if let Some(approval) = approval.filter(|approval| {
                approval.status == everything::permissions::ApprovalStatus::Pending
            }) {
                pending.push(approval.request);
            }
        }
        match next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(pending),
        }
    }
}

async fn print_host_approvals(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    task_id: everything::protocol::TaskId,
    cancellation: &CancellationToken,
) -> Result<(), CliError> {
    for request in pending_host_approvals(client, conversation_id, task_id, cancellation).await? {
        let json = serde_json::to_string_pretty(&request).map_err(|_| CliError::Serialization)?;
        eprintln!(
            "exact operation approval required:\n{json}\n{}",
            resume_hint(conversation_id, &request.id)
        );
    }
    Ok(())
}

fn resume_hint(conversation_id: ConversationId, approval_id: &str) -> String {
    format!(
        "approve with: everything chat --conversation {conversation_id} --tools --resume --approve {approval_id}\ndeny with:    everything chat --conversation {conversation_id} --tools --resume --deny {approval_id}"
    )
}

fn terminal_lines() -> Result<tokio::sync::mpsc::Receiver<String>, CliError> {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    std::thread::Builder::new()
        .name("everything-approval-input".into())
        .spawn(move || {
            for line in io::stdin().lines() {
                let Ok(line) = line else { break };
                if sender.blocking_send(line).is_err() {
                    break;
                }
            }
        })?;
    Ok(receiver)
}

async fn await_host_approval(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    task_id: everything::protocol::TaskId,
    lines: &mut Option<tokio::sync::mpsc::Receiver<String>>,
    cancellation: &CancellationToken,
) -> Result<(), CliError> {
    use everything::ipc::{ApprovalLookup, OwnerQuery, OwnerQueryResult};
    use everything::permissions::ApprovalStatus;
    use everything::protocol::PermissionDecision;
    let Some(request) = pending_host_approvals(client, conversation_id, task_id, cancellation)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(());
    };
    let arguments =
        serde_json::to_string(&request.arguments).map_err(|_| CliError::Serialization)?;
    let arguments = match arguments.char_indices().nth(400) {
        Some((end, _)) => format!("{}…", &arguments[..end]),
        None => arguments,
    };
    let mode = serde_json::to_value(request.scope.mode)
        .ok()
        .and_then(|mode| mode.as_str().map(str::to_owned))
        .unwrap_or_default();
    eprintln!(
        "\napproval required: {} in {mode} mode\n  workspace: {}\n  arguments: {arguments}\n  approval:  {}",
        request.scope.tool_name, request.scope.workspace_root, request.id
    );
    if !request.scope.targets.is_empty() {
        eprintln!("  targets:   {}", request.scope.targets.join(", "));
    }
    let lines = match lines {
        Some(lines) => lines,
        None => lines.insert(terminal_lines()?),
    };
    loop {
        eprint!("approve? [y]es / [n]o (or decide in another client): ");
        let answer = loop {
            tokio::select! {
                () = cancellation.cancelled() => return Err(AgentError::Cancelled.into()),
                line = lines.recv() => break line,
                () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                    let OwnerQueryResult::Approval { approval } = client
                        .query(
                            OwnerQuery::Approval {
                                conversation_id,
                                lookup: ApprovalLookup::Approval(request.id.clone()),
                            },
                            cancellation.clone(),
                        )
                        .await?
                    else {
                        return Err(CliError::OwnerInput);
                    };
                    match approval.map(|approval| approval.status) {
                        Some(ApprovalStatus::Pending) => {}
                        Some(ApprovalStatus::Approved { .. }) => {
                            eprintln!("\napproved in another client; continuing");
                            return Ok(());
                        }
                        Some(status) => {
                            eprintln!("\napproval {} is no longer pending ({status:?})", request.id);
                            return Ok(());
                        }
                        None => return Ok(()),
                    }
                }
            }
        };
        let decision = match answer.map(|line| line.trim().to_ascii_lowercase()) {
            None => {
                eprintln!(
                    "\ninput closed; the approval stays pending\n{}",
                    resume_hint(conversation_id, &request.id)
                );
                return Err(CliError::ToolPaused);
            }
            Some(answer) if matches!(answer.as_str(), "y" | "yes" | "approve") => {
                PermissionDecision::Allow
            }
            Some(answer) if matches!(answer.as_str(), "n" | "no" | "deny") => {
                PermissionDecision::Deny
            }
            Some(_) => continue,
        };
        resolve_host_approval(
            client,
            conversation_id,
            task_id,
            request.id.clone(),
            decision,
            cancellation,
        )
        .await?;
        match decision {
            PermissionDecision::Allow => eprintln!("approved; continuing"),
            PermissionDecision::Deny => eprintln!("denied; continuing without running it"),
        }
        return Ok(());
    }
}

async fn proxy_reply(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    command_id: CommandId,
    information: &everything::ipc::ConversationSummary,
    streamed: &str,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let messages = proxy_messages(
        client,
        conversation_id,
        information.last_sequence,
        cancellation,
    )
    .await?;
    let response = messages
        .into_iter()
        .rev()
        .find(|message| {
            message.causation_id == Some(command_id)
                && message.message.role == everything::protocol::MessageRole::Assistant
                && !message
                    .message
                    .content
                    .iter()
                    .any(|content| matches!(content, MessageContent::ToolCall(_)))
        })
        .ok_or(CliError::ToolOptions(
            "no committed final assistant response is available",
        ))?;
    let authoritative = message_text(&response.message);
    let mut output = tokio::io::stdout();
    match authoritative.strip_prefix(streamed) {
        Some(rest) => output.write_all(rest.as_bytes()).await?,
        None => {
            output.write_all(b"\n").await?;
            eprintln!("saved response differs from provisional output; authoritative text follows");
            output.write_all(authoritative.as_bytes()).await?;
        }
    }
    output.write_all(b"\n").await?;
    output.flush().await?;
    if let Some(memory) = information
        .memory_use
        .filter(|memory| memory.command_id == command_id && memory.excerpts > 0)
    {
        eprintln!(
            "used {} {}",
            memory.excerpts,
            if memory.excerpts == 1 {
                "memory"
            } else {
                "memories"
            }
        );
    }
    eprintln!("saved message {}", response.message.id);
    Ok(())
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
            .ok_or(CliError::NothingToResume(conversation.info.id))?;
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
    let preparation_owner = agent.clone();
    let (registry, authorizer, search, authority, initial, limits, budget) =
        tokio::task::spawn_blocking(move || {
            let _retained_agent = preparation_owner;
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
    let broker = ToolBroker::start_with_owner(
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
        agent.clone(),
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
                        if !provisional.is_empty() {
                            output.write_all(b"\n").await?;
                        }
                        output.write_all(authoritative.as_bytes()).await?;
                    }
                    if !authoritative.is_empty() || !provisional.is_empty() {
                        output.write_all(b"\n").await?;
                    }
                    output.flush().await?;
                    provisional.clear();
                    eprintln!(
                        "saved assistant message {} ({:?})",
                        summary.message_id, summary.stop_reason
                    );
                }
                TurnEvent::ContextBuilt { usage, .. } => {
                    eprintln!(
                        "context input estimate {} tokens",
                        usage.estimated_input_tokens
                    );
                }
                TurnEvent::ModelAttemptStarted { attempt_id } => eprintln!("model attempt {attempt_id}"),
                TurnEvent::ToolCallRecorded { call } => eprintln!("saved tool call {} {}", call.id, call.name),
                TurnEvent::ToolResultRecorded { result } => {
                    eprintln!("saved tool result {} error={}", result.tool_call_id, result.is_error);
                    if result.is_error && result.content.contains(READ_ONLY_WRITES_UNAVAILABLE) {
                        eprintln!("writes are disabled in read-only mode; send the next message with --tools --execution-mode trusted-workspace (or full-host) to allow writes");
                    }
                }
                TurnEvent::AwaitingPermission { approval, .. } => {
                    let request = serde_json::to_string_pretty(&approval)
                        .map_err(|_| CliError::Serialization)?;
                    eprintln!(
                        "exact operation approval required:\n{request}\n{}",
                        resume_hint(conversation_id, &approval.id)
                    );
                }
                TurnEvent::Paused { reason } => eprintln!("native run paused: {reason:?}"),
                TurnEvent::ToolBlocked {
                    call_id,
                    reason,
                    diagnostic,
                } => eprintln!("tool call {call_id} blocked ({reason:?}): {diagnostic}"),
                TurnEvent::RunCompleted { run } => {
                    completed = matches!(run.status, LoopStatus::Completed);
                    eprintln!(
                        "completed task {}; charged token budget {}",
                        run.task_id, run.charged_tokens
                    );
                }
            }
            Ok::<_, CliError>(())
        }
        .await;
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

async fn read_sensitive_input(
    input: tokio::net::unix::pipe::Receiver,
    cancellation: CancellationToken,
) -> Result<SensitiveOwnerInput, CliError> {
    use everything::host::settings::native::MAX_NATIVE_SETTINGS_INPUT_BYTES;
    let mut bytes = Zeroizing::new(Vec::new());
    let mut input = input.take((MAX_NATIVE_SETTINGS_INPUT_BYTES + 1) as u64);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(CliError::SensitiveOwnerCancelled),
        result = tokio::time::timeout(std::time::Duration::from_secs(15), input.read_to_end(&mut bytes)) => {
            result.map_err(|_| CliError::SensitiveOwnerCancelled)??;
        }
    }
    if bytes.is_empty() || bytes.len() > MAX_NATIVE_SETTINGS_INPUT_BYTES {
        return Err(CliError::SensitiveOwnerInput);
    }
    serde_json::from_slice(&bytes).map_err(|_| CliError::SensitiveOwnerInput)
}

async fn write_sensitive_json(
    mut output: tokio::net::unix::pipe::Sender,
    value: impl Serialize,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let bytes = Zeroizing::new(serde_json::to_vec(&value).map_err(|_| CliError::Serialization)?);
    let write = async {
        output.write_all(&bytes).await?;
        output.write_all(b"\n").await?;
        output.flush().await
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(CliError::SensitiveOwnerCancelled),
        result = tokio::time::timeout(std::time::Duration::from_secs(15), write) => {
            result.map_err(|_| CliError::SensitiveOwnerCancelled)?.map_err(CliError::Io)
        }
    }
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
