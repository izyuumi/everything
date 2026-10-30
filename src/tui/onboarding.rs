use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::host::{Host, TuiError};
use super::render::{centered, overlay_block};
use super::theme::{ACCENT, BORDER, DIM, ERROR, MUTED, OK, SELECT_BG, TEXT, TITLE, WARN, bold, fg};
use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::host::chatgpt::{ChatGptAttemptPhase, ChatGptOperation};
use crate::host::credentials::{CredentialOperation, SecretTransfer};
use crate::host::providers::api_key_provider;
use crate::host::settings::{CredentialStorage, ModelCredentialReference, ModelCredentialSource};
use crate::ipc::{OwnerOperation, OwnerOperationResult};
use crate::model::{Message, MessageRole, ModelEvent, ModelRequest};
use crate::service::ServiceError;
use crate::service::cli::{ServiceArguments, ServiceCommand, ServiceReport, manage};
use crate::vault::{VaultItemMetadata, VaultPutRequest};

const MAX_INPUT: usize = 8_192;
const OPENAI_MODEL: &str = "gpt-5";
const ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

enum Key {
    Up,
    Down,
    Enter,
    Back,
    Erase,
    Char(char),
    Paste(Zeroizing<String>),
    Other,
}

enum Prompt<'a> {
    Choose(&'a [String], usize),
    Input {
        label: &'a str,
        value: &'a str,
        masked: bool,
    },
    Wait(Instant),
}

pub struct Screen<'a> {
    terminal: &'a mut DefaultTerminal,
    events: &'a mut EventStream,
    cancellation: CancellationToken,
}

impl<'a> Screen<'a> {
    pub fn new(
        terminal: &'a mut DefaultTerminal,
        events: &'a mut EventStream,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            terminal,
            events,
            cancellation,
        }
    }

    fn draw(
        &mut self,
        title: &str,
        body: &[Line<'static>],
        prompt: Prompt,
    ) -> Result<(), TuiError> {
        let mut lines = body.to_vec();
        lines.push(Line::default());
        let footer = match prompt {
            Prompt::Choose(options, selected) => {
                for (index, option) in options.iter().enumerate() {
                    let style = if index == selected {
                        Style::default().bg(SELECT_BG).fg(TITLE)
                    } else {
                        fg(TEXT)
                    };
                    let marker = if index == selected { "❯" } else { " " };
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {marker} {}. ", index + 1), style.fg(ACCENT)),
                        Span::styled(option.clone(), style),
                    ]));
                }
                " ↑↓ move · Enter choose · Esc back · Ctrl+C quit "
            }
            Prompt::Input {
                label,
                value,
                masked,
            } => {
                let shown = if masked {
                    "•".repeat(value.chars().count().min(64))
                } else {
                    value.to_owned()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{label} "), bold(ACCENT)),
                    Span::styled(shown, fg(TEXT)),
                    Span::styled("▏", fg(TITLE)),
                ]));
                " Enter confirm · Esc back · Ctrl+C quit "
            }
            Prompt::Wait(started) => {
                let elapsed = started.elapsed().as_secs_f32();
                let ticks = usize::try_from(started.elapsed().as_millis() / 100).unwrap_or(0);
                let frame = SPIN[ticks % SPIN.len()];
                lines.push(Line::from(vec![
                    Span::styled(format!("{frame} "), fg(ACCENT)),
                    Span::styled(format!("working… ({elapsed:.1}s)"), fg(DIM)),
                ]));
                " Esc cancel · Ctrl+C quit "
            }
        };
        self.terminal.draw(|frame| {
            let area = frame.area();
            let width = area.width.saturating_sub(4).min(88);
            let inner = usize::from(width.saturating_sub(2)).max(1);
            let height: usize = lines
                .iter()
                .map(|line| line.width().max(1).div_ceil(inner))
                .sum();
            let rect = centered(area, width, u16::try_from(height + 2).unwrap_or(u16::MAX));
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(overlay_block(format!(" ◈ {title} "), BORDER, footer)),
                rect,
            );
        })?;
        Ok(())
    }

    async fn key(&mut self) -> Result<Key, TuiError> {
        loop {
            tokio::select! {
                () = self.cancellation.cancelled() => return Err(TuiError::Interrupted),
                event = self.events.next() => match event {
                    None => return Err(TuiError::Interrupted),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok(Event::Paste(text))) => return Ok(Key::Paste(Zeroizing::new(text))),
                    Some(Ok(Event::Resize(..))) => return Ok(Key::Other),
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        let control = key.modifiers.contains(KeyModifiers::CONTROL);
                        return match key.code {
                            KeyCode::Char('c') if control => Err(TuiError::Interrupted),
                            KeyCode::Char(character) if !control => Ok(Key::Char(character)),
                            KeyCode::Up | KeyCode::BackTab => Ok(Key::Up),
                            KeyCode::Down | KeyCode::Tab => Ok(Key::Down),
                            KeyCode::Enter => Ok(Key::Enter),
                            KeyCode::Esc => Ok(Key::Back),
                            KeyCode::Backspace => Ok(Key::Erase),
                            _ => Ok(Key::Other),
                        };
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }

    pub async fn choose(
        &mut self,
        title: &str,
        body: &[Line<'static>],
        options: &[String],
        default: usize,
    ) -> Result<Option<usize>, TuiError> {
        if options.is_empty() {
            return Ok(None);
        }
        let mut selected = default.min(options.len() - 1);
        loop {
            self.draw(title, body, Prompt::Choose(options, selected))?;
            match self.key().await? {
                Key::Up => selected = selected.checked_sub(1).unwrap_or(options.len() - 1),
                Key::Down => selected = (selected + 1) % options.len(),
                Key::Enter => return Ok(Some(selected)),
                Key::Back => return Ok(None),
                Key::Char(character) => {
                    if let Some(index) = character
                        .to_digit(10)
                        .and_then(|digit| (digit as usize).checked_sub(1))
                        .filter(|index| *index < options.len())
                    {
                        return Ok(Some(index));
                    }
                }
                _ => {}
            }
        }
    }

    pub async fn input(
        &mut self,
        title: &str,
        body: &[Line<'static>],
        label: &str,
        initial: &str,
        masked: bool,
    ) -> Result<Option<Zeroizing<String>>, TuiError> {
        let mut value = Zeroizing::new(String::with_capacity(MAX_INPUT));
        let push = |value: &mut String, character: char| {
            if !character.is_control() && value.len() + character.len_utf8() <= MAX_INPUT {
                value.push(character);
            }
        };
        initial
            .chars()
            .for_each(|character| push(&mut value, character));
        loop {
            self.draw(
                title,
                body,
                Prompt::Input {
                    label,
                    value: &value,
                    masked,
                },
            )?;
            match self.key().await? {
                Key::Char(character) => push(&mut value, character),
                Key::Paste(text) => text
                    .chars()
                    .for_each(|character| push(&mut value, character)),
                Key::Erase => {
                    value.pop();
                }
                Key::Enter if !value.trim().is_empty() => return Ok(Some(value)),
                Key::Back => return Ok(None),
                _ => {}
            }
        }
    }

    pub async fn wait<T>(
        &mut self,
        title: &str,
        body: &[Line<'static>],
        work: impl Future<Output = T>,
    ) -> Result<Option<T>, TuiError> {
        tokio::pin!(work);
        let started = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            self.draw(title, body, Prompt::Wait(started))?;
            tokio::select! {
                result = &mut work => return Ok(Some(result)),
                _ = tick.tick() => {}
                key = self.key() => if let Key::Back = key? {
                    return Ok(None);
                },
            }
        }
    }
}

fn text(value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(value.into(), fg(TEXT)))
}

fn dim(value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(value.into(), fg(MUTED)))
}

fn colored(color: ratatui::style::Color, value: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(value.into(), fg(color)))
}

pub struct SessionHost(Child);

impl Drop for SessionHost {
    fn drop(&mut self) {
        if let Some(pid) = self
            .0
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }
}

async fn ready(host: &Host, mut child: Option<&mut Child>) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if host.conversations(1).await.is_ok() {
            return Ok(());
        }
        if let Some(child) = &mut child
            && let Ok(Some(status)) = child.try_wait()
        {
            return Err(format!(
                "the host exited ({status}); run `everything serve` to see why"
            ));
        }
        if Instant::now() > deadline {
            return Err("the host did not answer within 30 seconds".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn service(
    state: &Path,
    command: ServiceCommand,
    cancellation: CancellationToken,
) -> Result<ServiceReport, ServiceError> {
    manage(
        ServiceArguments {
            launch_agents_dir: None,
            command,
        },
        state.to_path_buf(),
        cancellation,
    )
    .await
}

async fn install_and_start(
    state: &Path,
    cancellation: CancellationToken,
) -> Result<(), ServiceError> {
    service(state, ServiceCommand::Install, cancellation.clone()).await?;
    match service(state, ServiceCommand::Start, cancellation.clone()).await {
        Err(ServiceError::Disabled) => {
            service(state, ServiceCommand::Enable, cancellation.clone()).await?;
            service(state, ServiceCommand::Start, cancellation).await?;
        }
        result => {
            result?;
        }
    }
    Ok(())
}

pub async fn ensure_host(
    screen: &mut Screen<'_>,
    host: &Host,
    state: &Path,
) -> Result<Option<SessionHost>, TuiError> {
    match host.conversations(1).await {
        Ok(_) => return Ok(None),
        Err(TuiError::Host(_)) => {}
        Err(error) => return Err(error),
    }
    let mut options = vec!["Start the host in the background until this window closes".to_owned()];
    if cfg!(target_os = "macos") {
        options.push("Install and start the login service (LaunchAgent)".to_owned());
    }
    options.push("Quit".to_owned());
    let mut notice = None;
    loop {
        let mut body = vec![
            text(format!(
                "No Everything host is running for {}.",
                state.display()
            )),
            dim("The host owns conversations, tools and credentials; this window talks to it."),
        ];
        if options.len() == 3 {
            body.push(dim(
                "The login service replaces any existing Everything LaunchAgent.",
            ));
        }
        if let Some(notice) = notice.take() {
            body.push(Line::default());
            body.push(colored(ERROR, notice));
        }
        let choice = screen.choose("Start the host", &body, &options, 0).await?;
        match choice {
            Some(0) => {
                let child = tokio::process::Command::new(std::env::current_exe()?)
                    .arg("--state-dir")
                    .arg(state)
                    .arg("serve")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .spawn()?;
                let mut session = SessionHost(child);
                match screen
                    .wait(
                        "Starting the host",
                        &body,
                        ready(host, Some(&mut session.0)),
                    )
                    .await?
                {
                    Some(Ok(())) => return Ok(Some(session)),
                    Some(Err(message)) => notice = Some(message),
                    None => notice = Some("start cancelled".into()),
                }
            }
            Some(1) if options.len() == 3 => {
                let cancellation = CancellationToken::new();
                let work = async {
                    install_and_start(state, cancellation.clone())
                        .await
                        .map_err(|error| error.to_string())?;
                    ready(host, None).await
                };
                match screen
                    .wait("Starting the login service", &body, work)
                    .await?
                {
                    Some(Ok(())) => return Ok(None),
                    Some(Err(message)) => notice = Some(message),
                    None => {
                        cancellation.cancel();
                        notice = Some("start cancelled".into());
                    }
                }
            }
            _ => return Err(TuiError::Unavailable(state.to_path_buf())),
        }
    }
}

pub struct Found {
    openai: bool,
    anthropic: bool,
    chatgpt: Option<ModelCredentialReference>,
}

fn environment(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| !value.is_empty())
}

pub async fn scan(host: &Host, fresh: bool) -> Result<Option<Found>, TuiError> {
    let settings = host.settings().await?;
    if settings
        .model_credentials
        .iter()
        .any(|binding| binding.source == ModelCredentialSource::ApiKey)
    {
        return Ok(None);
    }
    let chatgpt = settings
        .model_credentials
        .into_iter()
        .find(|binding| binding.source == ModelCredentialSource::ChatGpt);
    match &chatgpt {
        Some(binding) => {
            let usable = host
                .chatgpt(ChatGptOperation::Status {
                    binding: binding.clone(),
                })
                .await
                .is_ok_and(|status| status.signed_in && status.plan_usage_enabled);
            if usable {
                return Ok(None);
            }
        }
        None if !fresh => return Ok(None),
        None => {}
    }
    Ok(Some(Found {
        openai: environment("OPENAI_API_KEY"),
        anthropic: environment("ANTHROPIC_API_KEY"),
        chatgpt,
    }))
}

pub struct Route {
    pub provider: ProviderKind,
    pub endpoint: ProviderEndpoint,
    pub model: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    Provider,
    Verify,
    Save,
    Service,
    Vault,
}

const STEPS: [Step; 6] = [
    Step::Welcome,
    Step::Provider,
    Step::Verify,
    Step::Save,
    Step::Service,
    Step::Vault,
];

enum Flow {
    Next,
    Goto(Step),
    Skip,
}

enum Credential {
    ApiKey(Zeroizing<String>),
    ChatGpt(ModelCredentialReference),
}

struct Choice {
    configuration: ConversationConfig,
    storage: CredentialStorage,
    credential: Credential,
}

enum Source {
    ChatGpt,
    Environment(ProviderKind),
    Typed(ProviderKind),
}

struct Wizard<'a, 'b> {
    screen: &'a mut Screen<'b>,
    host: &'a Host,
    state: &'a Path,
    instructions: &'static str,
    session: bool,
    found: Found,
    quick: bool,
    choice: Option<Choice>,
    saved: Option<Route>,
}

fn variable(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => "OPENAI_API_KEY",
        ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
    }
}

fn provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Anthropic => "Anthropic",
    }
}

fn default_model(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => OPENAI_MODEL,
        ProviderKind::Anthropic => ANTHROPIC_MODEL,
    }
}

pub async fn run(
    screen: &mut Screen<'_>,
    host: &Host,
    state: &Path,
    found: Found,
    instructions: &'static str,
    session: bool,
) -> Result<Option<Route>, TuiError> {
    let mut wizard = Wizard {
        screen,
        host,
        state,
        instructions,
        session,
        found,
        quick: true,
        choice: None,
        saved: None,
    };
    let mut index = 0;
    while let Some(step) = STEPS.get(index).copied() {
        index = match wizard.step(step).await? {
            Flow::Next => index + 1,
            Flow::Goto(target) => STEPS
                .iter()
                .position(|step| *step == target)
                .unwrap_or(STEPS.len()),
            Flow::Skip if step == Step::Vault => break,
            Flow::Skip => STEPS
                .iter()
                .position(|step| *step == Step::Vault)
                .unwrap_or(STEPS.len()),
        };
    }
    Ok(wizard.saved)
}

impl Wizard<'_, '_> {
    async fn step(&mut self, step: Step) -> Result<Flow, TuiError> {
        match step {
            Step::Welcome => self.welcome().await,
            Step::Provider => self.provider().await,
            Step::Verify => self.verify().await,
            Step::Save => self.save().await,
            Step::Service => self.service().await,
            Step::Vault => self.vault().await,
        }
    }

    async fn welcome(&mut self) -> Result<Flow, TuiError> {
        let body = [
            colored(TITLE, "Let's connect a model so you can start chatting."),
            Line::default(),
            colored(
                WARN,
                "Security: credentials stay on this machine in the system keychain; the model provider sees your conversation content.",
            ),
        ];
        let options = [
            "Quick start · OpenAI, keychain storage, default model".to_owned(),
            "Custom · choose provider, model and storage".to_owned(),
            "Skip for now · settings stay unchanged".to_owned(),
        ];
        Ok(
            match self
                .screen
                .choose("Welcome to Everything", &body, &options, 0)
                .await?
            {
                Some(0) => {
                    self.quick = true;
                    Flow::Next
                }
                Some(1) => {
                    self.quick = false;
                    Flow::Next
                }
                _ => Flow::Skip,
            },
        )
    }

    fn found_lines(&self) -> Vec<Line<'static>> {
        let mark = |found: bool, name: &str| {
            if found {
                colored(OK, format!("  ✓ {name} is set in this environment"))
            } else {
                dim(format!("  · {name} is not set"))
            }
        };
        vec![
            text("Found on this machine (values are never shown):"),
            mark(self.found.openai, "OPENAI_API_KEY"),
            mark(self.found.anthropic, "ANTHROPIC_API_KEY"),
            match &self.found.chatgpt {
                Some(_) => colored(
                    WARN,
                    "  ! ChatGPT sign-in in settings needs signing in again",
                ),
                None => dim("  · no ChatGPT sign-in in settings"),
            },
        ]
    }

    async fn provider(&mut self) -> Result<Flow, TuiError> {
        self.choice = None;
        let mut sources = vec![(Source::ChatGpt, "Sign in with ChatGPT (browser)".to_owned())];
        let providers: &[ProviderKind] = if self.quick {
            &[ProviderKind::OpenAi]
        } else {
            &[ProviderKind::OpenAi, ProviderKind::Anthropic]
        };
        for provider in providers {
            let found = match provider {
                ProviderKind::OpenAi => self.found.openai,
                ProviderKind::Anthropic => self.found.anthropic,
            };
            if found {
                sources.push((
                    Source::Environment(*provider),
                    format!("Use {} from this environment", variable(*provider)),
                ));
            }
            sources.push((
                Source::Typed(*provider),
                format!("Enter {} API key", provider_name(*provider)),
            ));
        }
        let mut options: Vec<String> = sources.iter().map(|(_, label)| label.clone()).collect();
        options.push("Skip for now · settings stay unchanged".to_owned());
        let body = self.found_lines();
        let Some(index) = self
            .screen
            .choose("Choose how to connect", &body, &options, 0)
            .await?
        else {
            return Ok(Flow::Goto(Step::Welcome));
        };
        let Some((source, _)) = sources.into_iter().nth(index) else {
            return Ok(Flow::Skip);
        };
        let choice = match source {
            Source::ChatGpt => self.chatgpt().await?,
            Source::Environment(provider) => match std::env::var(variable(provider)) {
                Ok(value) => self.api_key(provider, Some(Zeroizing::new(value))).await?,
                Err(_) => None,
            },
            Source::Typed(provider) => self.api_key(provider, None).await?,
        };
        Ok(match choice {
            Some(choice) => {
                self.choice = Some(choice);
                Flow::Next
            }
            None => Flow::Goto(Step::Provider),
        })
    }

    async fn storage(&mut self, title: &str) -> Result<Option<CredentialStorage>, TuiError> {
        if self.quick {
            return Ok(Some(CredentialStorage::Native));
        }
        let options = [
            "System keychain (kept across restarts)".to_owned(),
            "Host memory only (forgotten when the host stops)".to_owned(),
        ];
        let body = [text("Where should the credential be stored?")];
        Ok(self
            .screen
            .choose(title, &body, &options, 0)
            .await?
            .map(|index| {
                if index == 0 {
                    CredentialStorage::Native
                } else {
                    CredentialStorage::Session
                }
            }))
    }

    async fn api_key(
        &mut self,
        provider: ProviderKind,
        environment: Option<Zeroizing<String>>,
    ) -> Result<Option<Choice>, TuiError> {
        let title = format!("{} API key", provider_name(provider));
        let mut notice: Option<String> = None;
        let (endpoint, model) = if self.quick {
            (
                ProviderEndpoint::Official,
                default_model(provider).to_owned(),
            )
        } else {
            let options = [
                format!("{} API (official)", provider_name(provider)),
                "A compatible server on this machine (loopback URL)".to_owned(),
            ];
            let body = [text("Which endpoint should Everything call?")];
            let endpoint = match self.screen.choose(&title, &body, &options, 0).await? {
                None => return Ok(None),
                Some(0) => ProviderEndpoint::Official,
                Some(_) => loop {
                    let mut body = vec![dim(match provider {
                        ProviderKind::OpenAi => "For example http://127.0.0.1:8080/v1/responses",
                        ProviderKind::Anthropic => "For example http://127.0.0.1:8080/v1/messages",
                    })];
                    if let Some(notice) = notice.take() {
                        body.push(colored(ERROR, notice));
                    }
                    let Some(url) = self
                        .screen
                        .input(&title, &body, "Endpoint URL:", "", false)
                        .await?
                    else {
                        return Ok(None);
                    };
                    match ProviderEndpoint::loopback(url.trim()) {
                        Ok(endpoint) => break endpoint,
                        Err(error) => notice = Some(error.to_string()),
                    }
                },
            };
            let body = [dim("Model ID sent to the provider.")];
            let Some(model) = self
                .screen
                .input(&title, &body, "Model:", default_model(provider), false)
                .await?
            else {
                return Ok(None);
            };
            (endpoint, model.trim().to_owned())
        };
        let configuration = ConversationConfig {
            provider,
            model,
            instructions: self.instructions.to_owned(),
            endpoint,
            max_output_tokens: 4_096,
            temperature: None,
        };
        if let Err(error) = configuration.validate() {
            let body = [colored(ERROR, error.to_string())];
            self.screen
                .choose(&title, &body, &["Back".to_owned()], 0)
                .await?;
            return Ok(None);
        }
        let Some(storage) = self.storage(&title).await? else {
            return Ok(None);
        };
        let key = if let Some(key) = environment {
            key
        } else {
            let body = [
                text(format!(
                    "Paste or type your {} API key. It is never shown.",
                    provider_name(provider)
                )),
                dim("It is tested before anything is saved."),
            ];
            let Some(key) = self
                .screen
                .input(&title, &body, "API key:", "", true)
                .await?
            else {
                return Ok(None);
            };
            key
        };
        Ok(Some(Choice {
            configuration,
            storage,
            credential: Credential::ApiKey(key),
        }))
    }

    async fn chatgpt(&mut self) -> Result<Option<Choice>, TuiError> {
        let title = "Sign in with ChatGPT";
        let binding = if let Some(binding) = &self.found.chatgpt {
            binding.clone()
        } else {
            let Some(storage) = self.storage(title).await? else {
                return Ok(None);
            };
            ModelCredentialReference {
                provider: ProviderKind::OpenAi,
                endpoint: ProviderEndpoint::Official,
                reference: uuid::Uuid::new_v4(),
                storage,
                source: ModelCredentialSource::ChatGpt,
            }
        };
        let failure = match self.sign_in(title, binding.clone()).await? {
            Ok(()) => {
                return Ok(Some(Choice {
                    configuration: ConversationConfig {
                        provider: ProviderKind::OpenAi,
                        model: OPENAI_MODEL.to_owned(),
                        instructions: self.instructions.to_owned(),
                        endpoint: ProviderEndpoint::Official,
                        max_output_tokens: 4_096,
                        temperature: None,
                    },
                    storage: binding.storage,
                    credential: Credential::ChatGpt(binding),
                }));
            }
            Err(failure) => failure,
        };
        let body = [colored(ERROR, failure)];
        self.screen
            .choose(title, &body, &["Back".to_owned()], 0)
            .await?;
        Ok(None)
    }

    async fn sign_in(
        &mut self,
        title: &str,
        binding: ModelCredentialReference,
    ) -> Result<Result<(), String>, TuiError> {
        let started = self
            .host
            .chatgpt(ChatGptOperation::SignIn {
                binding,
                timeout_seconds: 600,
            })
            .await;
        let attempt = match started.map(|status| status.attempt) {
            Ok(Some(attempt)) => attempt,
            Ok(None) => return Ok(Err("the host did not start a sign-in attempt".into())),
            Err(error) => return Ok(Err(error.to_string())),
        };
        let Some(url) = attempt.authorize_url.clone() else {
            return Ok(Err("the host returned no sign-in URL".into()));
        };
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let opened = tokio::process::Command::new(program)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
        let body = [
            text(if opened {
                "Continue in your browser. If it did not open, visit:"
            } else {
                "Open this address in your browser:"
            }),
            colored(ACCENT, url),
            dim(format!(
                "Waiting for the callback on {}.",
                attempt.redirect_uri
            )),
        ];
        let host = self.host.clone();
        let attempt_id = attempt.attempt_id;
        let work = async move {
            loop {
                let status = host
                    .chatgpt(ChatGptOperation::Wait {
                        attempt_id,
                        timeout_ms: 25_000,
                    })
                    .await?;
                if !status.attempt.as_ref().is_some_and(|current| {
                    current.attempt_id == attempt_id
                        && current.phase == ChatGptAttemptPhase::Pending
                }) {
                    return Ok::<_, TuiError>(status);
                }
            }
        };
        let waited = self.screen.wait(title, &body, work).await;
        if !matches!(waited, Ok(Some(_))) {
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                self.host.chatgpt(ChatGptOperation::Cancel { attempt_id }),
            )
            .await;
        }
        let status = match waited? {
            None => return Ok(Err("sign-in cancelled".into())),
            Some(Err(error)) => return Ok(Err(error.to_string())),
            Some(Ok(status)) => status,
        };
        let phase = status
            .attempt
            .as_ref()
            .filter(|current| current.attempt_id == attempt_id)
            .map(|current| current.phase.clone());
        Ok(match phase {
            Some(ChatGptAttemptPhase::Completed) if status.plan_usage_enabled => Ok(()),
            Some(ChatGptAttemptPhase::Completed) => Err(
                "signed in, but ChatGPT plan usage was not granted; sign in again and allow it"
                    .into(),
            ),
            Some(ChatGptAttemptPhase::Failed { reason }) => {
                Err(format!("sign-in did not complete: {reason}"))
            }
            Some(ChatGptAttemptPhase::Expired) => Err("sign-in expired".into()),
            Some(ChatGptAttemptPhase::Cancelled) => Err("sign-in was cancelled".into()),
            _ => Err("sign-in was replaced by another attempt".into()),
        })
    }

    async fn verify(&mut self) -> Result<Flow, TuiError> {
        let Some(choice) = self.choice.as_mut() else {
            return Ok(Flow::Goto(Step::Provider));
        };
        let title = "Testing the connection";
        let label = format!(
            "Sending a tiny test prompt to {}/{}…",
            provider_name(choice.configuration.provider),
            choice.configuration.model
        );
        let result = match &choice.credential {
            Credential::ApiKey(key) => {
                let body = [text(label)];
                let work = test_turn(&choice.configuration, key);
                self.screen.wait(title, &body, work).await?
            }
            Credential::ChatGpt(binding) => {
                let body = [text("Asking ChatGPT which models your plan can use…")];
                let work = self.host.plan_models(binding.clone());
                match self.screen.wait(title, &body, work).await? {
                    Some(Ok(models)) if models.is_empty() => {
                        Some(Err("the ChatGPT plan lists no models".to_owned()))
                    }
                    Some(Ok(models)) => {
                        let model = if self.quick {
                            models.first().map(|(slug, _)| slug.clone())
                        } else {
                            let options: Vec<String> = models
                                .iter()
                                .map(|(slug, name)| format!("{slug}  {name}"))
                                .collect();
                            let body = [text("Pick the model for new conversations.")];
                            self.screen
                                .choose("Choose a model", &body, &options, 0)
                                .await?
                                .and_then(|index| models.get(index))
                                .map(|(slug, _)| slug.clone())
                        };
                        match model {
                            Some(model) => {
                                choice.configuration.model = model;
                                Some(Ok(()))
                            }
                            None => None,
                        }
                    }
                    Some(Err(error)) => Some(Err(error.to_string())),
                    None => None,
                }
            }
        };
        let failure = match result {
            Some(Ok(())) => return Ok(Flow::Next),
            Some(Err(failure)) => failure,
            None => "test cancelled".to_owned(),
        };
        let body = [
            colored(ERROR, format!("The test failed: {failure}")),
            dim("Nothing has been saved."),
        ];
        let options = [
            "Retry".to_owned(),
            "Pick another provider".to_owned(),
            "Skip for now · settings stay unchanged".to_owned(),
        ];
        Ok(
            match self
                .screen
                .choose("Connection test failed", &body, &options, 0)
                .await?
            {
                Some(0) => Flow::Goto(Step::Verify),
                Some(1) | None => Flow::Goto(Step::Provider),
                _ => Flow::Skip,
            },
        )
    }

    async fn save(&mut self) -> Result<Flow, TuiError> {
        let Some(choice) = self.choice.take() else {
            return Ok(Flow::Goto(Step::Provider));
        };
        let work = store_route(self.host, &choice);
        let body = [text("Saving the verified connection…")];
        let result = self.screen.wait("Saving", &body, work).await?;
        let route = Route {
            provider: choice.configuration.provider,
            endpoint: choice.configuration.endpoint.clone(),
            model: choice.configuration.model.clone(),
        };
        let failure = match result {
            Some(Ok(restart)) => {
                let mut body = vec![colored(
                    OK,
                    format!(
                        "Connected. New conversations use {}/{}.",
                        provider_name(route.provider).to_lowercase(),
                        route.model
                    ),
                )];
                if restart {
                    body.push(colored(
                        WARN,
                        "The host reports that some settings apply after a restart.",
                    ));
                }
                self.screen
                    .choose("Saved", &body, &["Continue".to_owned()], 0)
                    .await?;
                self.saved = Some(route);
                return Ok(Flow::Next);
            }
            Some(Err(failure)) => failure,
            None => "saving was interrupted; check settings before retrying".to_owned(),
        };
        self.choice = Some(choice);
        let body = [colored(ERROR, format!("Could not save: {failure}"))];
        let options = ["Retry".to_owned(), "Skip for now".to_owned()];
        Ok(
            match self
                .screen
                .choose("Saving failed", &body, &options, 0)
                .await?
            {
                Some(0) => Flow::Goto(Step::Save),
                _ => Flow::Skip,
            },
        )
    }

    async fn service(&mut self) -> Result<Flow, TuiError> {
        if !cfg!(target_os = "macos") {
            return Ok(Flow::Next);
        }
        let cancellation = CancellationToken::new();
        if let Ok(ServiceReport::Status { status }) =
            service(self.state, ServiceCommand::Status, cancellation.clone()).await
            && status.installed
        {
            return Ok(Flow::Next);
        }
        let body = [
            text("Run the host automatically when you log in?"),
            dim(
                "Installs a LaunchAgent for this state directory, replacing any existing Everything LaunchAgent.",
            ),
        ];
        let options = ["Not now".to_owned(), "Install the login service".to_owned()];
        if self
            .screen
            .choose("Background service", &body, &options, 0)
            .await?
            != Some(1)
        {
            return Ok(Flow::Next);
        }
        let work = service(self.state, ServiceCommand::Install, cancellation.clone());
        let body = [text("Installing the login service…")];
        let message = match self.screen.wait("Background service", &body, work).await? {
            Some(Ok(ServiceReport::Installed { path })) => colored(
                OK,
                if self.session {
                    format!(
                        "Installed {}. After closing this window, start it with `everything service start`.",
                        path.display()
                    )
                } else {
                    format!(
                        "Installed {}. It starts at your next login or with `everything service start` once the running host stops.",
                        path.display()
                    )
                },
            ),
            Some(Ok(_)) => colored(OK, "Installed."),
            Some(Err(error)) => colored(ERROR, format!("Could not install: {error}")),
            None => {
                cancellation.cancel();
                colored(WARN, "Installation cancelled.")
            }
        };
        self.screen
            .choose(
                "Background service",
                &[message],
                &["Continue".to_owned()],
                0,
            )
            .await?;
        Ok(Flow::Next)
    }
}

impl Wizard<'_, '_> {
    async fn vault(&mut self) -> Result<Flow, TuiError> {
        let title = "Connect a password manager";
        let body = [
            text("Optional: keep logins and API secrets in a vault on this machine."),
            dim(
                "Tasks can use vault items once the vault tools land (#219); the model will only see names and vault:// references, never values.",
            ),
        ];
        let options = [
            "Skip".to_owned(),
            "1Password".to_owned(),
            "Proton Pass".to_owned(),
            "Add an API secret to the local vault (system keychain)".to_owned(),
        ];
        let (name, issue) = match self.screen.choose(title, &body, &options, 0).await? {
            Some(1) => ("1Password", 220),
            Some(2) => ("Proton Pass", 221),
            Some(3) => return self.local_secret().await,
            _ => return Ok(Flow::Next),
        };
        let body = [
            colored(
                WARN,
                format!(
                    "{name} is not available yet; it is tracked in https://github.com/izyuumi/everything/issues/{issue}.",
                ),
            ),
            dim("Nothing was connected. You can add local secrets with `everything vault add`."),
        ];
        self.screen
            .choose(title, &body, &["Continue".to_owned()], 0)
            .await?;
        Ok(Flow::Next)
    }

    async fn local_secret(&mut self) -> Result<Flow, TuiError> {
        let title = "Add an API secret";
        let body = [dim(
            "A name you and the model will see, for example GitHub token.",
        )];
        let Some(label) = self.screen.input(title, &body, "Label:", "", false).await? else {
            return Ok(Flow::Goto(Step::Vault));
        };
        let metadata = VaultItemMetadata::ApiSecret {
            label: label.trim().to_owned(),
            env_name: None,
            service: None,
        };
        let message = if let Err(error) = metadata.validate() {
            colored(ERROR, error.to_string())
        } else {
            let body = [
                text(format!(
                    "Paste or type the secret for {}. It is never shown.",
                    metadata.label()
                )),
                dim("It goes straight to the host's keychain-backed vault."),
            ];
            let Some(mut value) = self.screen.input(title, &body, "Secret:", "", true).await?
            else {
                return Ok(Flow::Goto(Step::Vault));
            };
            match SecretTransfer::new(std::mem::take(&mut *value)) {
                Err(_) => colored(
                    ERROR,
                    "the secret must be printable text; nothing was saved",
                ),
                Ok(secret) => {
                    let item = VaultPutRequest {
                        metadata,
                        secrets: [("value".to_owned(), secret)].into(),
                    };
                    let work = self.host.operate(OwnerOperation::VaultPut { item });
                    let body = [text("Saving to the local vault…")];
                    match self.screen.wait(title, &body, work).await? {
                        Some(Ok(OwnerOperationResult::VaultItemAdded { reference })) => {
                            colored(OK, format!("Saved as {reference}."))
                        }
                        Some(Ok(_)) => colored(ERROR, "unexpected vault response"),
                        Some(Err(error)) => colored(ERROR, format!("Could not save: {error}")),
                        None => colored(
                            WARN,
                            "Saving was interrupted; check `everything vault list`.",
                        ),
                    }
                }
            }
        };
        self.screen
            .choose(title, &[message], &["Continue".to_owned()], 0)
            .await?;
        Ok(Flow::Next)
    }
}

async fn store_route(host: &Host, choice: &Choice) -> Result<bool, String> {
    let configuration = &choice.configuration;
    let (binding, stored) = match &choice.credential {
        Credential::ApiKey(key) => {
            let binding = ModelCredentialReference {
                provider: configuration.provider,
                endpoint: configuration.endpoint.clone(),
                reference: uuid::Uuid::new_v4(),
                storage: choice.storage,
                source: ModelCredentialSource::ApiKey,
            };
            let secret =
                SecretTransfer::new(key.trim().to_owned()).map_err(|error| error.to_string())?;
            host.operate(OwnerOperation::Credential {
                operation: CredentialOperation::Store {
                    binding: binding.clone(),
                    secret,
                },
            })
            .await
            .map_err(|error| error.to_string())?;
            (binding, true)
        }
        Credential::ChatGpt(binding) => (binding.clone(), false),
    };
    let saved = replace_settings(host, &binding).await;
    if saved.is_err() && stored {
        let _ = host
            .operate(OwnerOperation::Credential {
                operation: CredentialOperation::Remove { binding },
            })
            .await;
    }
    saved
}

async fn replace_settings(host: &Host, binding: &ModelCredentialReference) -> Result<bool, String> {
    let same = |existing: &&ModelCredentialReference| {
        existing.provider == binding.provider && existing.endpoint == binding.endpoint
    };
    let settings = host.settings().await.map_err(|error| error.to_string())?;
    if settings.model_credentials.iter().filter(same).eq([binding]) {
        return Ok(false);
    }
    let mut updated = settings.clone();
    updated
        .model_credentials
        .retain(|existing| !same(&existing));
    updated.model_credentials.push(binding.clone());
    let (OwnerOperationResult::SettingsReceipt {
        restart_required: restart,
        ..
    }
    | OwnerOperationResult::Settings {
        restart_required: restart,
        ..
    }) = host
        .operate(OwnerOperation::ReplaceSettings {
            expected_revision: settings.revision,
            settings: Box::new(updated),
        })
        .await
        .map_err(|error| error.to_string())?
    else {
        return Err("unexpected settings response".into());
    };
    let applied = host.settings().await.map_err(|error| error.to_string())?;
    if applied.model_credentials.iter().filter(same).eq([binding]) {
        Ok(restart)
    } else {
        Err("the host did not apply the settings change".into())
    }
}

async fn test_turn(configuration: &ConversationConfig, key: &str) -> Result<(), String> {
    let work = async {
        let provider =
            api_key_provider(configuration, key.trim()).map_err(|error| error.to_string())?;
        let mut request = ModelRequest::new(
            configuration.model.clone(),
            vec![
                Message::text(MessageRole::User, "Reply with the single word: ready")
                    .map_err(|error| error.to_string())?,
            ],
        )
        .map_err(|error| error.to_string())?;
        request.max_output_tokens = Some(256);
        let mut stream = provider
            .stream(request, CancellationToken::new())
            .await
            .map_err(|error| error.to_string())?;
        while let Some(event) = stream.next().await {
            if let ModelEvent::Completed { .. } = event.map_err(|error| error.to_string())? {
                return Ok(());
            }
        }
        Err("the provider closed the response early".to_owned())
    };
    tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .map_err(|_| "no answer within 60 seconds".to_owned())?
}
