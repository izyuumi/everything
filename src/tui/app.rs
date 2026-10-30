use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::text::Line;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use super::host::{Host, HostEvent, Readiness, Tools, TurnEvent, TurnStart, message_text};
use super::input::Input;
use crate::agent::{ConversationConfig, ProviderEndpoint, ProviderKind};
use crate::ipc::ConversationSummary;
use crate::permissions::{ApprovalRequest, ExecutionMode};
use crate::protocol::{CommandId, ConversationId, MessageContent, MessageRole, TaskId};

pub const COMMANDS: [(&str, &str); 19] = [
    ("/help", "Show commands and keys"),
    ("/new", "New conversation  [title]"),
    ("/sessions", "List conversations and switch"),
    ("/resume", "Switch conversation  <n|id-prefix>"),
    ("/model", "Pick a model for a new conversation  [model]"),
    (
        "/provider",
        "Provider for new conversations  <openai|anthropic>",
    ),
    (
        "/instructions",
        "Instructions for new conversations  [text|off]",
    ),
    (
        "/tools",
        "Native tools  <on [dir] [read-only|trusted-workspace|full-host]|off>",
    ),
    ("/approvals", "Review pending tool approvals"),
    ("/history", "Reload this conversation from the host"),
    ("/retry", "Send the last message again"),
    ("/copy", "Copy the last response to the clipboard"),
    (
        "/stop",
        "Cancel the reply in progress (ends the conversation)",
    ),
    ("/status", "Show host, conversation and tool state"),
    ("/clear", "Clear the transcript view"),
    ("/mouse", "Toggle mouse capture (off allows text selection)"),
    ("/queue", "Show queued messages"),
    ("/quit", "Exit"),
    ("/exit", "Exit"),
];

pub const KEYS: [(&str, &str); 12] = [
    ("Enter", "send"),
    ("Alt+Enter · Ctrl+J · \\ Enter", "newline"),
    ("Tab", "complete /command"),
    ("Up / Down", "input history · move between lines"),
    ("PageUp / PageDown · wheel", "scroll transcript"),
    ("End", "jump to latest"),
    ("Ctrl+C", "clear input · twice: cancel reply or exit"),
    ("Ctrl+D", "exit when input is empty"),
    ("Ctrl+L", "new conversation"),
    ("Ctrl+X", "conversation switcher"),
    ("Ctrl+A / Ctrl+E", "line start / end"),
    (
        "Ctrl+U / Ctrl+K / Ctrl+W",
        "delete to start / to end / word",
    ),
];

const NO_ARGUMENTS: [&str; 12] = [
    "/help",
    "/sessions",
    "/approvals",
    "/history",
    "/retry",
    "/copy",
    "/stop",
    "/status",
    "/clear",
    "/mouse",
    "/queue",
    "/quit",
];

#[derive(Clone, Copy)]
pub enum NoteKind {
    Info,
    Ok,
    Warn,
    Error,
    Tool,
}

pub enum Entry {
    Banner,
    User(String),
    Assistant { text: String, footer: String },
    Note(NoteKind, String),
}

pub struct Turn {
    pub started: Instant,
    pub live: String,
    pub notes: Vec<String>,
    pub conversation_id: ConversationId,
    pub command_id: CommandId,
    pub label: &'static str,
    handle: JoinHandle<()>,
}

pub enum PickerKind {
    Session,
    Model,
}

pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub value: String,
    pub current: bool,
}

pub enum Overlay {
    Approval {
        request: Box<ApprovalRequest>,
        remaining: usize,
        conversation_id: ConversationId,
        command_id: CommandId,
    },
    Picker {
        kind: PickerKind,
        items: Vec<PickerItem>,
        selected: usize,
    },
    Help,
}

pub struct Options {
    pub conversation: Option<ConversationId>,
    pub resume_latest: bool,
    pub provider: Option<ProviderKind>,
    pub model: Option<String>,
    pub endpoint: Option<ProviderEndpoint>,
    pub default_instructions: &'static str,
}

pub struct App {
    host: Host,
    events: UnboundedSender<HostEvent>,
    pub state_directory: PathBuf,
    pub conversation: Option<ConversationSummary>,
    pub provider: ProviderKind,
    pub model: Option<String>,
    endpoint: ProviderEndpoint,
    instructions: Option<String>,
    default_instructions: &'static str,
    pub tools: Option<Tools>,
    pub entries: Vec<Entry>,
    pub input: Input,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    pub turn: Option<Turn>,
    generation: u64,
    pub queued: VecDeque<String>,
    pub overlay: Option<Overlay>,
    pub scroll: usize,
    pub page: usize,
    pub completion: usize,
    pub mouse: bool,
    pub mouse_changed: bool,
    cancel_armed: Option<Instant>,
    quit_armed: Option<Instant>,
    last_user: Option<String>,
    last_reply: Option<String>,
    pub stopped: bool,
    pub should_quit: bool,
    pub rendered: Vec<Line<'static>>,
    pub rendered_width: u16,
    pub dirty: bool,
}

pub fn mode_label(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::ReadOnly => "read-only",
        ExecutionMode::TrustedWorkspace => "trusted-workspace",
        ExecutionMode::FullHost => "full-host",
        ExecutionMode::SettingsRestricted => "settings-restricted",
    }
}

pub fn provider_label(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::OpenAi => "openai",
    }
}

fn armed(at: Option<Instant>) -> bool {
    at.is_some_and(|at| at.elapsed() < Duration::from_secs(3))
}

fn task_of(command_id: CommandId) -> Option<TaskId> {
    TaskId::try_from(*command_id.as_uuid()).ok()
}

impl App {
    pub async fn new(
        host: Host,
        events: UnboundedSender<HostEvent>,
        state_directory: PathBuf,
        options: Options,
        conversations: Vec<ConversationSummary>,
    ) -> Self {
        let latest = conversations.first();
        let mut app = Self {
            host,
            events,
            state_directory,
            conversation: None,
            provider: options
                .provider
                .or(latest.map(|info| info.provider))
                .unwrap_or(ProviderKind::OpenAi),
            model: options.model.or(latest.map(|info| info.model.clone())),
            endpoint: options
                .endpoint
                .or_else(|| {
                    latest
                        .filter(|_| options.provider.is_none())
                        .map(|info| info.endpoint.clone())
                })
                .unwrap_or(ProviderEndpoint::Official),
            instructions: None,
            default_instructions: options.default_instructions,
            tools: None,
            entries: vec![Entry::Banner],
            input: Input::default(),
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            turn: None,
            generation: 0,
            queued: VecDeque::new(),
            overlay: None,
            scroll: 0,
            page: 10,
            completion: 0,
            mouse: true,
            mouse_changed: false,
            cancel_armed: None,
            quit_armed: None,
            last_user: None,
            last_reply: None,
            stopped: false,
            should_quit: false,
            rendered: Vec::new(),
            rendered_width: 0,
            dirty: true,
        };
        let target = options.conversation.or_else(|| {
            options
                .resume_latest
                .then(|| latest.map(|info| info.id))
                .flatten()
        });
        if let Some(conversation_id) = target {
            app.switch(conversation_id).await;
        }
        if app.model.is_none() {
            app.note(NoteKind::Warn, "no model yet · choose one with /model");
        }
        app
    }

    fn note(&mut self, kind: NoteKind, text: impl Into<String>) {
        self.entries.push(Entry::Note(kind, text.into()));
        self.dirty = true;
        self.scroll = 0;
    }

    pub fn completions(&self) -> Vec<(&'static str, &'static str)> {
        let text = self.input.text();
        if self.overlay.is_some() || !text.starts_with('/') || text.contains(char::is_whitespace) {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .copied()
            .filter(|(name, _)| name.starts_with(text) && *name != text)
            .collect()
    }

    pub fn status_label(&self) -> (&'static str, bool) {
        if matches!(self.overlay, Some(Overlay::Approval { .. })) {
            ("awaiting approval", true)
        } else if let Some(turn) = &self.turn {
            if turn.live.is_empty() {
                (turn.label, true)
            } else {
                ("streaming", true)
            }
        } else {
            ("ready", false)
        }
    }

    pub async fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key).await,
            Event::Paste(text) if self.overlay.is_none() => {
                self.input
                    .insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_add(3),
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(3),
                _ => {}
            },
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        if self.overlay.is_some() {
            self.on_overlay_key(key).await;
            return;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if control {
            match key.code {
                KeyCode::Char('c') => self.on_interrupt().await,
                KeyCode::Char('d') if self.input.is_empty() => self.should_quit = true,
                KeyCode::Char('l') => self.new_conversation(None).await,
                KeyCode::Char('x') => self.open_sessions().await,
                KeyCode::Char('j') => self.input.insert_char('\n'),
                KeyCode::Char('a') => self.input.home(),
                KeyCode::Char('e') => self.input.end(),
                KeyCode::Char('u') => self.input.kill_to_start(),
                KeyCode::Char('k') => self.input.kill_to_end(),
                KeyCode::Char('w') => self.input.delete_word(),
                KeyCode::Home => self.scroll = usize::MAX,
                KeyCode::End => self.scroll = 0,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Enter if alt || shift => self.input.insert_char('\n'),
            KeyCode::Enter => {
                if self.input.before_cursor().ends_with('\\') {
                    self.input.backspace();
                    self.input.insert_char('\n');
                    return;
                }
                let candidates = self.completions();
                if let Some((name, _)) =
                    candidates.get(self.completion.min(candidates.len().saturating_sub(1)))
                {
                    let name = *name;
                    self.apply_completion(name);
                    if !NO_ARGUMENTS.contains(&name) {
                        return;
                    }
                }
                self.submit().await;
            }
            KeyCode::Tab => {
                let candidates = self.completions();
                if let Some((name, _)) =
                    candidates.get(self.completion.min(candidates.len().saturating_sub(1)))
                {
                    let name = *name;
                    self.apply_completion(name);
                }
            }
            KeyCode::Up => {
                let count = self.completions().len();
                if count > 0 {
                    self.completion = (self.completion + count - 1) % count;
                } else if !self.input.on_first_line() {
                    self.input.up();
                } else {
                    self.history_previous();
                }
            }
            KeyCode::Down => {
                let count = self.completions().len();
                if count > 0 {
                    self.completion = (self.completion + 1) % count;
                } else if !self.input.on_last_line() {
                    self.input.down();
                } else {
                    self.history_next();
                }
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(self.page),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(self.page),
            KeyCode::End if self.input.is_empty() => self.scroll = 0,
            KeyCode::Esc => {
                if self.completions().is_empty() {
                    self.scroll = 0;
                } else {
                    self.input.take();
                }
            }
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace if alt => self.input.delete_word(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Char(character) => {
                self.input.insert_char(character);
                self.completion = 0;
            }
            _ => {}
        }
    }

    fn apply_completion(&mut self, name: &str) {
        if NO_ARGUMENTS.contains(&name) {
            self.input.set(name.to_owned());
        } else {
            self.input.set(format!("{name} "));
        }
        self.completion = 0;
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            None => {
                self.draft = self.input.text().to_owned();
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_index = Some(index);
        self.input.set(self.history[index].clone());
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.input.set(self.history[index + 1].clone());
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.draft);
            self.input.set(draft);
        }
    }

    async fn on_interrupt(&mut self) {
        if let Some(turn) = &self.turn {
            if armed(self.cancel_armed) {
                self.cancel_armed = None;
                let (conversation_id, command_id) = (turn.conversation_id, turn.command_id);
                self.cancel(conversation_id, command_id).await;
            } else {
                self.cancel_armed = Some(Instant::now());
                self.note(
                    NoteKind::Warn,
                    "reply still generating · Ctrl+C again to cancel it (the conversation can't continue after) · keep typing to queue",
                );
            }
        } else if !self.input.is_empty() {
            self.input.take();
            self.history_index = None;
        } else if armed(self.quit_armed) {
            self.should_quit = true;
        } else {
            self.quit_armed = Some(Instant::now());
            self.note(NoteKind::Info, "press Ctrl+C again or Ctrl+D to exit");
        }
    }

    async fn cancel(&mut self, conversation_id: ConversationId, command_id: CommandId) {
        let Some(task_id) = task_of(command_id) else {
            return;
        };
        match self.host.cancel(conversation_id, task_id).await {
            Ok(()) => {
                self.stopped = true;
                self.note(
                    NoteKind::Warn,
                    "cancel requested · your next message starts a new conversation",
                );
            }
            Err(error) => self.note(NoteKind::Error, error.to_string()),
        }
    }

    async fn on_overlay_key(&mut self, key: KeyEvent) {
        let control_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        match self.overlay.take() {
            Some(Overlay::Approval {
                request,
                remaining,
                conversation_id,
                command_id,
            }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.decide(conversation_id, command_id, request, true);
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.decide(conversation_id, command_id, request, false);
                }
                KeyCode::Esc | KeyCode::Char('s') => {
                    self.note(NoteKind::Info, "left pending · /approvals to decide later");
                }
                _ if control_c => {
                    self.note(NoteKind::Info, "left pending · /approvals to decide later");
                }
                _ => {
                    self.overlay = Some(Overlay::Approval {
                        request,
                        remaining,
                        conversation_id,
                        command_id,
                    });
                }
            },
            Some(Overlay::Picker {
                kind,
                items,
                mut selected,
            }) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected
                        .checked_sub(1)
                        .unwrap_or(items.len().saturating_sub(1));
                    self.overlay = Some(Overlay::Picker {
                        kind,
                        items,
                        selected,
                    });
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    selected = if selected + 1 >= items.len() {
                        0
                    } else {
                        selected + 1
                    };
                    self.overlay = Some(Overlay::Picker {
                        kind,
                        items,
                        selected,
                    });
                }
                KeyCode::Enter => {
                    if let Some(item) = items.get(selected) {
                        let value = item.value.clone();
                        match kind {
                            PickerKind::Session => {
                                if let Ok(conversation_id) = value.parse::<ConversationId>() {
                                    self.switch(conversation_id).await;
                                }
                            }
                            PickerKind::Model => self.choose_model(value).await,
                        }
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {}
                _ if control_c => {}
                _ => {
                    self.overlay = Some(Overlay::Picker {
                        kind,
                        items,
                        selected,
                    })
                }
            },
            Some(Overlay::Help) | None => {}
        }
        self.dirty = true;
    }

    fn decide(
        &mut self,
        conversation_id: ConversationId,
        command_id: CommandId,
        request: Box<ApprovalRequest>,
        allow: bool,
    ) {
        let tool = request.scope.tool_name.clone();
        self.note(
            if allow { NoteKind::Ok } else { NoteKind::Warn },
            format!("{} {tool}", if allow { "allowed" } else { "denied" }),
        );
        self.start_turn(
            TurnStart::Decide {
                conversation_id,
                command_id,
                request,
                allow,
            },
            conversation_id,
            command_id,
            "continuing",
        );
    }

    fn start_turn(
        &mut self,
        start: TurnStart,
        conversation_id: ConversationId,
        command_id: CommandId,
        label: &'static str,
    ) {
        self.generation += 1;
        let handle = tokio::spawn(self.host.clone().run_turn(
            self.generation,
            start,
            self.events.clone(),
        ));
        self.turn = Some(Turn {
            started: Instant::now(),
            live: String::new(),
            notes: Vec::new(),
            conversation_id,
            command_id,
            label,
            handle,
        });
        self.scroll = 0;
    }

    pub async fn on_host(&mut self, event: HostEvent) {
        if event.generation != self.generation {
            return;
        }
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        match event.event {
            TurnEvent::Live(text) => turn.live = text,
            TurnEvent::Note(note) => turn.notes.push(note),
            TurnEvent::Done(text, memories) => {
                let elapsed = turn.started.elapsed().as_secs_f32();
                self.finish_turn(false);
                if text.is_empty() {
                    self.note(NoteKind::Warn, "no response text");
                } else {
                    let model = self
                        .conversation
                        .as_ref()
                        .map(|info| info.model.clone())
                        .or_else(|| self.model.clone())
                        .unwrap_or_default();
                    self.last_reply = Some(text.clone());
                    let memory = match memories {
                        0 => String::new(),
                        1 => " · used 1 memory".to_owned(),
                        count => format!(" · used {count} memories"),
                    };
                    self.entries.push(Entry::Assistant {
                        text,
                        footer: format!("{model} · {elapsed:.1}s{memory}"),
                    });
                }
                self.refresh().await;
                if let Some(next) = self.queued.pop_front() {
                    self.send_message(next).await;
                }
            }
            TurnEvent::Approvals(requests) => {
                let (conversation_id, command_id) = (turn.conversation_id, turn.command_id);
                self.finish_turn(true);
                self.show_approvals(conversation_id, command_id, requests);
            }
            TurnEvent::Stopped(reason) => {
                self.finish_turn(true);
                self.note(NoteKind::Error, format!("stopped: {reason}"));
                self.refresh().await;
                self.flush_queue_notice();
            }
            TurnEvent::Failed(error) => {
                self.finish_turn(true);
                self.note(NoteKind::Error, error);
                self.refresh().await;
                self.flush_queue_notice();
            }
        }
    }

    fn flush_queue_notice(&mut self) {
        if !self.queued.is_empty() {
            self.note(
                NoteKind::Info,
                format!("{} queued message(s) kept · /queue to view, Enter on empty input sends the next", self.queued.len()),
            );
        }
    }

    fn finish_turn(&mut self, keep_partial: bool) {
        if let Some(turn) = self.turn.take() {
            turn.handle.abort();
            for note in turn.notes {
                self.entries.push(Entry::Note(NoteKind::Tool, note));
            }
            if keep_partial && !turn.live.is_empty() {
                self.entries.push(Entry::Assistant {
                    text: turn.live,
                    footer: "partial".into(),
                });
            }
        }
        self.cancel_armed = None;
        self.dirty = true;
    }

    fn show_approvals(
        &mut self,
        conversation_id: ConversationId,
        command_id: CommandId,
        mut requests: Vec<ApprovalRequest>,
    ) {
        if requests.is_empty() {
            self.note(
                NoteKind::Warn,
                "waiting for a permission decision, but no pending approval was found",
            );
            return;
        }
        let remaining = requests.len() - 1;
        let request = requests.swap_remove(0);
        self.overlay = Some(Overlay::Approval {
            request: Box::new(request),
            remaining,
            conversation_id,
            command_id,
        });
    }

    async fn refresh(&mut self) {
        if let Some(info) = &self.conversation {
            match self.host.conversation(info.id).await {
                Ok((info, _)) => self.conversation = Some(info),
                Err(error) => self.note(NoteKind::Error, error.to_string()),
            }
        }
        self.dirty = true;
    }

    async fn submit(&mut self) {
        let raw = self.input.take();
        self.history_index = None;
        self.completion = 0;
        let text = raw.trim().to_owned();
        if text.is_empty() {
            if self.turn.is_none()
                && let Some(next) = self.queued.pop_front()
            {
                self.send_message(next).await;
            }
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        if text.starts_with('/') && !text.contains('\n') {
            self.command(&text).await;
            return;
        }
        if self.turn.is_some() || matches!(self.overlay, Some(Overlay::Approval { .. })) {
            self.queued.push_back(text);
            self.note(
                NoteKind::Info,
                format!(
                    "queued ({}) · sends when the current reply finishes",
                    self.queued.len()
                ),
            );
            return;
        }
        self.send_message(text).await;
    }

    async fn ready_to_send(&mut self) -> bool {
        let Some(info) = self.conversation.clone() else {
            return true;
        };
        if self.stopped {
            self.leave_stopped();
            return true;
        }
        let info = match self.host.conversation(info.id).await {
            Ok((info, _)) => info,
            Err(error) => {
                self.note(NoteKind::Error, error.to_string());
                return false;
            }
        };
        self.conversation = Some(info.clone());
        match self.host.readiness(&info).await {
            Ok(Readiness::Ready) => true,
            Ok(Readiness::Stopped) => {
                self.leave_stopped();
                true
            }
            Ok(Readiness::Busy(command_id)) => {
                self.note(
                    NoteKind::Warn,
                    "a previous reply is still generating · following it, your message is queued",
                );
                self.start_turn(
                    TurnStart::Watch {
                        conversation_id: info.id,
                        command_id,
                    },
                    info.id,
                    command_id,
                    "thinking",
                );
                false
            }
            Ok(Readiness::Waiting(command_id)) => {
                match self
                    .host
                    .pending_approvals(info.id, task_of(command_id))
                    .await
                {
                    Ok(requests) => self.show_approvals(info.id, command_id, requests),
                    Err(error) => self.note(NoteKind::Error, error.to_string()),
                }
                false
            }
            Err(error) => {
                self.note(NoteKind::Error, error.to_string());
                false
            }
        }
    }

    fn leave_stopped(&mut self) {
        self.note(
            NoteKind::Warn,
            "this conversation was stopped by a cancelled reply and can't continue · starting a new one",
        );
        self.conversation = None;
        self.stopped = false;
    }

    fn configuration(&self) -> Result<ConversationConfig, String> {
        let model = self
            .model
            .clone()
            .ok_or_else(|| "choose a model first with /model".to_owned())?;
        let configuration = ConversationConfig {
            provider: self.provider,
            model,
            instructions: self
                .instructions
                .clone()
                .unwrap_or_else(|| self.default_instructions.to_owned()),
            endpoint: self.endpoint.clone(),
            max_output_tokens: 4_096,
            temperature: None,
        };
        configuration
            .validate()
            .map_err(|error| error.to_string())?;
        Ok(configuration)
    }

    async fn send_message(&mut self, text: String) {
        if !self.ready_to_send().await {
            self.queued.push_front(text);
            return;
        }
        if self.conversation.is_none() {
            let configuration = match self.configuration() {
                Ok(configuration) => configuration,
                Err(error) => {
                    self.note(NoteKind::Error, error);
                    self.input.set(text);
                    return;
                }
            };
            let title: String = text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(60)
                .collect();
            match self.host.initialize(title, configuration).await {
                Ok(info) => self.conversation = Some(info),
                Err(error) => {
                    self.note(NoteKind::Error, error.to_string());
                    self.input.set(text);
                    return;
                }
            }
        }
        let Some(conversation_id) = self.conversation.as_ref().map(|info| info.id) else {
            return;
        };
        self.last_user = Some(text.clone());
        self.entries.push(Entry::User(text.clone()));
        self.dirty = true;
        let command_id = CommandId::new();
        self.start_turn(
            TurnStart::Send {
                conversation_id,
                command_id,
                text,
                tools: self.tools.clone(),
            },
            conversation_id,
            command_id,
            "thinking",
        );
    }

    async fn new_conversation(&mut self, title: Option<String>) {
        if self.turn.is_some() {
            self.note(
                NoteKind::Warn,
                "wait for the current reply first (or Ctrl+C twice to cancel it)",
            );
            return;
        }
        self.conversation = None;
        self.stopped = false;
        self.entries = vec![Entry::Banner];
        self.dirty = true;
        self.scroll = 0;
        if let Some(title) = title {
            match self.configuration() {
                Ok(configuration) => match self.host.initialize(title, configuration).await {
                    Ok(info) => self.conversation = Some(info),
                    Err(error) => self.note(NoteKind::Error, error.to_string()),
                },
                Err(error) => self.note(NoteKind::Error, error),
            }
        }
        let model = self.model.clone().unwrap_or_else(|| "no model".into());
        self.note(
            NoteKind::Ok,
            format!(
                "new conversation · {}/{model}",
                provider_label(self.provider)
            ),
        );
    }

    async fn switch(&mut self, conversation_id: ConversationId) {
        if self.turn.is_some() {
            self.note(
                NoteKind::Warn,
                "wait for the current reply first (or Ctrl+C twice to cancel it)",
            );
            return;
        }
        let info = match self.host.conversation(conversation_id).await {
            Ok((info, _)) => info,
            Err(error) => {
                self.note(
                    NoteKind::Error,
                    format!("cannot open conversation: {error}"),
                );
                return;
            }
        };
        self.provider = info.provider;
        self.model = Some(info.model.clone());
        self.endpoint = info.endpoint.clone();
        self.conversation = Some(info.clone());
        self.stopped = false;
        self.load_transcript().await;
        match self.host.readiness(&info).await {
            Ok(Readiness::Stopped) => {
                self.stopped = true;
                self.note(NoteKind::Warn, "this conversation was stopped by a cancelled reply · your next message starts a new one");
            }
            Ok(Readiness::Busy(command_id)) => self.start_turn(
                TurnStart::Watch {
                    conversation_id,
                    command_id,
                },
                conversation_id,
                command_id,
                "thinking",
            ),
            Ok(Readiness::Waiting(command_id)) => {
                if let Ok(requests) = self
                    .host
                    .pending_approvals(conversation_id, task_of(command_id))
                    .await
                {
                    self.show_approvals(conversation_id, command_id, requests);
                }
            }
            Ok(Readiness::Ready) => {}
            Err(error) => self.note(NoteKind::Error, error.to_string()),
        }
    }

    async fn load_transcript(&mut self) {
        self.entries = vec![Entry::Banner];
        self.dirty = true;
        self.scroll = 0;
        let Some(info) = self.conversation.clone() else {
            return;
        };
        let messages = match self.host.messages(info.id, info.last_sequence).await {
            Ok(messages) => messages,
            Err(error) => {
                self.note(NoteKind::Error, error.to_string());
                return;
            }
        };
        for recorded in messages {
            let message = recorded.message;
            let text = message_text(&message);
            match message.role {
                MessageRole::User if !text.is_empty() => self.entries.push(Entry::User(text)),
                MessageRole::Assistant => {
                    if !text.is_empty() {
                        self.last_reply = Some(text.clone());
                        self.entries.push(Entry::Assistant {
                            text,
                            footer: info.model.clone(),
                        });
                    }
                    for content in &message.content {
                        if let MessageContent::ToolCall(call) = content {
                            self.entries
                                .push(Entry::Note(NoteKind::Tool, format!("⚒ {}", call.name)));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    async fn open_sessions(&mut self) {
        match self.host.conversations(100).await {
            Ok(conversations) if conversations.is_empty() => {
                self.note(NoteKind::Info, "no conversations yet")
            }
            Ok(conversations) => {
                let current = self.conversation.as_ref().map(|info| info.id);
                let selected = conversations
                    .iter()
                    .position(|info| Some(info.id) == current)
                    .unwrap_or(0);
                let items = conversations
                    .into_iter()
                    .map(|info| PickerItem {
                        label: if info.title.is_empty() {
                            "(untitled)".into()
                        } else {
                            info.title.clone()
                        },
                        detail: format!(
                            "{}/{} · {} msgs · {}",
                            provider_label(info.provider),
                            info.model,
                            info.message_count,
                            &info.id.to_string()[..8]
                        ),
                        value: info.id.to_string(),
                        current: Some(info.id) == current,
                    })
                    .collect();
                self.overlay = Some(Overlay::Picker {
                    kind: PickerKind::Session,
                    items,
                    selected,
                });
            }
            Err(error) => self.note(NoteKind::Error, error.to_string()),
        }
    }

    async fn choose_model(&mut self, model: String) {
        self.model = Some(model);
        self.new_conversation(None).await;
    }

    async fn command(&mut self, line: &str) {
        let mut parts = line.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let rest = line[name.len()..].trim();
        let arguments: Vec<&str> = parts.collect();
        match name {
            "/quit" | "/exit" | "/q" => self.should_quit = true,
            "/help" | "/?" => self.overlay = Some(Overlay::Help),
            "/new" | "/reset" => {
                self.new_conversation((!rest.is_empty()).then(|| rest.to_owned()))
                    .await;
            }
            "/sessions" | "/switch" => self.open_sessions().await,
            "/resume" => match arguments.first() {
                None => self.open_sessions().await,
                Some(choice) => self.resume(choice).await,
            },
            "/model" => match arguments.first() {
                Some(model) => self.choose_model((*model).to_owned()).await,
                None => self.pick_model().await,
            },
            "/provider" => match arguments.first().copied() {
                Some("openai") => {
                    self.provider = ProviderKind::OpenAi;
                    self.endpoint = ProviderEndpoint::Official;
                    self.note(
                        NoteKind::Ok,
                        "provider openai for new conversations · /model to pick a model",
                    );
                }
                Some("anthropic") => {
                    self.provider = ProviderKind::Anthropic;
                    self.endpoint = ProviderEndpoint::Official;
                    self.note(
                        NoteKind::Ok,
                        "provider anthropic for new conversations · /model MODEL_ID",
                    );
                }
                _ => {
                    let label = provider_label(self.provider);
                    self.note(
                        NoteKind::Info,
                        format!("provider: {label} (openai | anthropic)"),
                    );
                }
            },
            "/instructions" => {
                if rest.is_empty() {
                    let current = self
                        .instructions
                        .clone()
                        .unwrap_or_else(|| self.default_instructions.to_owned());
                    self.note(NoteKind::Info, format!("instructions: {current}"));
                } else {
                    self.instructions = (rest != "off").then(|| rest.to_owned());
                    self.note(
                        NoteKind::Ok,
                        "applies to new conversations (instructions are fixed per conversation)",
                    );
                }
            }
            "/tools" => self.tools_command(&arguments),
            "/approvals" => {
                if let Some(info) = self.conversation.clone() {
                    match self.host.pending_approvals(info.id, None).await {
                        Ok(requests) => {
                            let command_id = requests.first().and_then(|request| {
                                request.scope.task_id.parse::<CommandId>().ok()
                            });
                            match command_id {
                                Some(command_id) => {
                                    self.show_approvals(info.id, command_id, requests)
                                }
                                None => self.note(NoteKind::Info, "no pending approvals"),
                            }
                        }
                        Err(error) => self.note(NoteKind::Error, error.to_string()),
                    }
                } else {
                    self.note(NoteKind::Info, "no conversation yet");
                }
            }
            "/history" => {
                self.refresh().await;
                self.load_transcript().await;
            }
            "/retry" => match self.last_user.clone() {
                Some(text) if self.turn.is_none() => self.send_message(text).await,
                Some(_) => self.note(NoteKind::Warn, "a reply is in progress"),
                None => self.note(NoteKind::Warn, "nothing to retry"),
            },
            "/copy" => self.copy().await,
            "/stop" => match &self.turn {
                Some(turn) => {
                    let (conversation_id, command_id) = (turn.conversation_id, turn.command_id);
                    self.cancel(conversation_id, command_id).await;
                }
                None => self.note(NoteKind::Info, "nothing running"),
            },
            "/status" => self.status(),
            "/clear" => {
                self.entries = vec![Entry::Banner];
                self.dirty = true;
            }
            "/mouse" => {
                self.mouse = !self.mouse;
                self.mouse_changed = true;
                let state = if self.mouse {
                    "on (wheel scrolls)"
                } else {
                    "off (select text freely)"
                };
                self.note(NoteKind::Ok, format!("mouse capture {state}"));
            }
            "/queue" => {
                if self.queued.is_empty() {
                    self.note(NoteKind::Info, "queue is empty");
                } else {
                    let lines: Vec<String> = self
                        .queued
                        .iter()
                        .enumerate()
                        .map(|(index, text)| {
                            format!("{}. {}", index + 1, text.lines().next().unwrap_or_default())
                        })
                        .collect();
                    self.note(NoteKind::Info, lines.join("\n"));
                }
            }
            _ => self.note(NoteKind::Warn, format!("unknown command {name} · /help")),
        }
    }

    async fn resume(&mut self, choice: &str) {
        match self.host.conversations(100).await {
            Ok(conversations) => {
                let found = choice
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|index| conversations.get(index))
                    .or_else(|| {
                        let mut matches = conversations
                            .iter()
                            .filter(|info| info.id.to_string().starts_with(choice));
                        match (matches.next(), matches.next()) {
                            (Some(info), None) => Some(info),
                            _ => None,
                        }
                    })
                    .map(|info| info.id);
                match found {
                    Some(conversation_id) => self.switch(conversation_id).await,
                    None => self.note(
                        NoteKind::Warn,
                        format!("no unique conversation for {choice}"),
                    ),
                }
            }
            Err(error) => self.note(NoteKind::Error, error.to_string()),
        }
    }

    async fn pick_model(&mut self) {
        if self.provider != ProviderKind::OpenAi {
            self.note(NoteKind::Info, "type /model MODEL_ID for anthropic");
            return;
        }
        match self.host.chatgpt_models().await {
            Ok(models) if models.is_empty() => self.note(
                NoteKind::Warn,
                "the ChatGPT plan lists no models · /model MODEL_ID",
            ),
            Ok(models) => {
                let selected = models
                    .iter()
                    .position(|(slug, _)| Some(slug) == self.model.as_ref())
                    .unwrap_or(0);
                let items = models
                    .into_iter()
                    .map(|(slug, name)| PickerItem {
                        current: Some(&slug) == self.model.as_ref(),
                        label: slug.clone(),
                        detail: name,
                        value: slug,
                    })
                    .collect();
                self.overlay = Some(Overlay::Picker {
                    kind: PickerKind::Model,
                    items,
                    selected,
                });
            }
            Err(error) => self.note(NoteKind::Error, error.to_string()),
        }
    }

    fn tools_command(&mut self, arguments: &[&str]) {
        match arguments.first().copied() {
            None => {
                let text = match &self.tools {
                    Some(tools) => format!(
                        "tools {} in {}",
                        mode_label(tools.mode),
                        tools.workspace.display()
                    ),
                    None => "tools off".into(),
                };
                self.note(NoteKind::Info, text);
            }
            Some("off") => {
                self.tools = None;
                self.note(NoteKind::Ok, "tools off");
            }
            Some("on") => {
                let mut workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let mut mode = ExecutionMode::ReadOnly;
                for argument in &arguments[1..] {
                    match *argument {
                        "read-only" => mode = ExecutionMode::ReadOnly,
                        "trusted-workspace" => mode = ExecutionMode::TrustedWorkspace,
                        "full-host" => mode = ExecutionMode::FullHost,
                        path => {
                            let expanded = match path.strip_prefix("~/") {
                                Some(rest) => std::env::var_os("HOME")
                                    .map(PathBuf::from)
                                    .unwrap_or_default()
                                    .join(rest),
                                None => PathBuf::from(path),
                            };
                            workspace = if expanded.is_absolute() {
                                expanded
                            } else {
                                std::env::current_dir().unwrap_or_default().join(expanded)
                            };
                        }
                    }
                }
                match std::fs::canonicalize(&workspace) {
                    Ok(path) if path.is_dir() => {
                        self.note(
                            NoteKind::Ok,
                            format!("tools {} in {}", mode_label(mode), path.display()),
                        );
                        if mode == ExecutionMode::FullHost {
                            self.note(NoteKind::Warn, "full-host exposes a shell with full host access; every operation still needs your approval");
                        }
                        self.tools = Some(Tools {
                            workspace: path,
                            mode,
                        });
                    }
                    _ => self.note(
                        NoteKind::Error,
                        format!("not a directory: {}", workspace.display()),
                    ),
                }
            }
            Some(_) => self.note(
                NoteKind::Warn,
                "usage: /tools on [dir] [read-only|trusted-workspace|full-host] | /tools off",
            ),
        }
    }

    fn status(&mut self) {
        let mut lines = vec![format!("host state {}", self.state_directory.display())];
        match &self.conversation {
            Some(info) => {
                lines.push(format!("conversation {} · {}", info.title, info.id));
                lines.push(format!(
                    "model {}/{} · {} msgs · sequence {}",
                    provider_label(info.provider),
                    info.model,
                    info.message_count,
                    info.last_sequence
                ));
            }
            None => lines.push("conversation: new (created on first message)".into()),
        }
        lines.push(match &self.tools {
            Some(tools) => format!(
                "tools {} in {}",
                mode_label(tools.mode),
                tools.workspace.display()
            ),
            None => "tools off".into(),
        });
        self.note(NoteKind::Info, lines.join("\n"));
    }

    async fn copy(&mut self) {
        let Some(text) = self.last_reply.clone() else {
            self.note(NoteKind::Warn, "no response yet");
            return;
        };
        for (program, arguments) in [
            ("pbcopy", &[][..]),
            ("wl-copy", &[][..]),
            ("xclip", &["-selection", "clipboard"][..]),
        ] {
            let Ok(mut child) = tokio::process::Command::new(program)
                .args(arguments)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(text.as_bytes()).await;
            }
            if child.wait().await.is_ok_and(|status| status.success()) {
                self.note(NoteKind::Ok, "copied the last response");
                return;
            }
        }
        self.note(
            NoteKind::Error,
            "no clipboard tool found (pbcopy, wl-copy or xclip)",
        );
    }
}
