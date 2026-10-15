use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use everything::agent::{
    Agent, AgentError, Conversation, ConversationConfig, MAX_CHAT_TEXT_BYTES, ProviderEndpoint,
    ProviderKind, TextAcceptance, TextCommand, TurnEvent,
};
use everything::model::ModelProvider;
use everything::model::anthropic::{AnthropicConfig, AnthropicProvider};
use everything::model::openai::{OpenAiConfig, OpenAiProvider};
use everything::protocol::{CommandId, ConversationId, ConversationMessage, MessageContent};
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
    let agent = Agent::open(state_dir).await?;
    if cancellation.is_cancelled() {
        return Err(AgentError::Cancelled.into());
    }
    match arguments.command {
        Command::Init(arguments) => {
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
        Command::Status(arguments) => match arguments.conversation {
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
        Command::Chat(arguments) => chat(agent, arguments, cancellation).await,
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
            TextAcceptance::Completed { message, .. } => {
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
        }
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
