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
    Serve,
    Owner(OwnerArguments),
    Service(everything::service::cli::ServiceArguments),
    Init(InitArguments),
    Chat(ChatArguments),
    Status(StatusArguments),
}

enum AgentCommand {
    Init(InitArguments),
    Chat(ChatArguments),
    Status(StatusArguments),
}

#[derive(Args)]
struct OwnerArguments {
    #[arg(long, value_name = "PATH")]
    request_file: PathBuf,
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
    Host(#[from] everything::host::runtime::HostError),
    #[error(transparent)]
    HostClient(#[from] everything::host::client::HostClientError),
    #[error("invalid owner JSON input; use a bounded query or operation object")]
    OwnerInput,
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
    let command = match arguments.command {
        Command::Serve => {
            return everything::host::runtime::HostService::start(&state_dir)
                .await?
                .run(cancellation)
                .await
                .map_err(CliError::from);
        }
        Command::Owner(arguments) => return owner_cli(state_dir, arguments, cancellation).await,
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
    let input = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::from(
            rustix::fs::open(
                arguments.request_file,
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
        serde_json::from_slice::<OwnerInput>(&bytes).map_err(|_| CliError::OwnerInput)
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
                serde_json::from_value(command.clone()).map_err(|_| CliError::OwnerInput)?;
            if serde_json::to_value(&typed).map_err(|_| CliError::Serialization)? != command {
                return Err(CliError::OwnerInput);
            }
            write_json(client.submit_domain(typed, cancellation).await?).await
        }
        OwnerInput::Operation {
            operation,
            request_id,
        } => {
            let request_id = request_id.unwrap_or_default();
            eprintln!("owner request {request_id}; reuse this ID for an explicit retry");
            write_json(client.operate(*operation, request_id, cancellation).await?).await
        }
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
    use everything::protocol::{TaskId, UnixMillis};
    let text = match (arguments.message, arguments.message_file) {
        (Some(message), None) => Some(message),
        (None, Some(path)) => Some(read_message(path).await?),
        _ => None,
    };
    let (conversation_id, mut information) = match arguments.conversation {
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
            .ok_or(AgentError::NoPendingTurn)?;
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
            let OwnerQueryResult::Approval { approval } = client
                .query(
                    OwnerQuery::Approval {
                        conversation_id,
                        lookup: everything::ipc::ApprovalLookup::Approval(id),
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
                UnixMillis::now().map_err(AgentError::from)?,
                everything::protocol::Command::ResolvePermission {
                    operation_id,
                    decision,
                },
            );
            client.submit_domain(command, cancellation.clone()).await?;
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
                information.last_sequence,
                cancellation,
            )
            .await;
        }
        command_id
    };
    let task_id = TaskId::try_from(*command_id.as_uuid()).map_err(AgentError::from)?;
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
                    information.last_sequence,
                    cancellation,
                )
                .await;
            }
            OwnerQueryResult::Task { task, .. }
                if matches!(
                    task.status,
                    everything::protocol::TaskStatus::Queued
                        | everything::protocol::TaskStatus::Running
                ) => {}
            OwnerQueryResult::Task { task, .. } => {
                eprintln!(
                    "host task {task_id} stopped at {:?}; inspect owner task/approval metadata before an explicit resume",
                    task.status
                );
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

async fn proxy_reply(
    client: &everything::host::client::HostClient,
    conversation_id: ConversationId,
    command_id: CommandId,
    through: u64,
    cancellation: CancellationToken,
) -> Result<(), CliError> {
    let messages = proxy_messages(client, conversation_id, through, cancellation).await?;
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
    print_message(&response.message).await
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
