use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::{
    App, COMMANDS, Entry, KEYS, NoteKind, Overlay, PickerKind, mode_label, provider_label,
};
use super::markdown;
use super::theme::{
    ACCENT, AGENT, BAR_BG, BORDER, DIM, ERROR, GLYPH, LABEL, LOGO_GRADIENT, MUTED, OK, SELECT_BG,
    TEXT, TITLE, WARN, bold, fg,
};

const LOGO: [&str; 6] = [
    "███████╗██╗   ██╗███████╗██████╗ ██╗   ██╗████████╗██╗  ██╗██╗███╗   ██╗ ██████╗",
    "██╔════╝██║   ██║██╔════╝██╔══██╗╚██╗ ██╔╝╚══██╔══╝██║  ██║██║████╗  ██║██╔════╝",
    "█████╗  ██║   ██║█████╗  ██████╔╝ ╚████╔╝    ██║   ███████║██║██╔██╗ ██║██║  ███╗",
    "██╔══╝  ╚██╗ ██╔╝██╔══╝  ██╔══██╗  ╚██╔╝     ██║   ██╔══██║██║██║╚██╗██║██║   ██║",
    "███████╗ ╚████╔╝ ███████╗██║  ██║   ██║      ██║   ██║  ██║██║██║ ╚████║╚██████╔╝",
    "╚══════╝  ╚═══╝  ╚══════╝╚═╝  ╚═╝   ╚═╝      ╚═╝   ╚═╝  ╚═╝╚═╝╚═╝  ╚═══╝ ╚═════╝",
];

const SMALL_LOGO: [&str; 4] = [
    " _____   _____ _____   _______ _  _ ___ _  _  ___",
    "| __\\ \\ / / __| _ \\ \\ / /_   _| || |_ _| \\| |/ __|",
    "| _| \\ V /| _||   /\\ V /  | | | __ || || .` | (_ |",
    "|___| \\_/ |___|_|_\\ |_|   |_| |_||_|___|_|\\_|\\___|",
];

const FACES: [&str; 12] = [
    "(｡•́︿•̀｡)",
    "(◕‿◕✿)",
    "( ˘▽˘)っ",
    "(ﾉ◕ヮ◕)ﾉ",
    "ʕ•ᴥ•ʔ",
    "(¬‿¬)",
    "(⌐■_■)",
    "(•̀ᴗ•́)و",
    "ᕕ( ᐛ )ᕗ",
    "(=^･ω･^=)",
    "(づ｡◕‿‿◕｡)づ",
    "( •_•)>⌐■",
];

const VERBS: [&str; 12] = [
    "pondering",
    "contemplating",
    "musing",
    "cogitating",
    "ruminating",
    "deliberating",
    "mulling",
    "reflecting",
    "reasoning",
    "weaving",
    "brewing",
    "untangling",
];

const SPIN: [char; 6] = ['◜', '◠', '◝', '◞', '◡', '◟'];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let width = area.width.saturating_sub(2).max(1) as usize;
    let layout = app.input.layout(width);
    let input_height = (layout.rows.len().min(8) as u16) + 1;
    let [transcript, input, status] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_transcript(frame, app, transcript);
    draw_input(frame, app, input, &layout);
    draw_status(frame, app, status);
    draw_completions(frame, app, input);
    match &app.overlay {
        Some(Overlay::Approval { .. }) => draw_approval(frame, app, area),
        Some(Overlay::Picker { .. }) => draw_picker(frame, app, area),
        Some(Overlay::Help) => draw_help(frame, area),
        Some(Overlay::Secret {
            label,
            value,
            overflow,
        }) => draw_secret(frame, area, label, value.chars().count(), *overflow),
        None => {}
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut row = String::new();
        let mut row_width = 0;
        for word in line.split_inclusive(' ') {
            let word_width = word.width();
            if row_width + word_width.saturating_sub(usize::from(word.ends_with(' '))) > width
                && !row.is_empty()
            {
                rows.push(std::mem::take(&mut row).trim_end().to_owned());
                row_width = 0;
            }
            if word_width > width {
                for character in word.chars() {
                    let character_width = character.width().unwrap_or(0);
                    if row_width + character_width > width && !row.is_empty() {
                        rows.push(std::mem::take(&mut row));
                        row_width = 0;
                    }
                    row.push(character);
                    row_width += character_width;
                }
            } else {
                row.push_str(word);
                row_width += word_width;
            }
        }
        rows.push(row.trim_end().to_owned());
    }
    rows
}

fn indent(lines: Vec<Line<'static>>, prefix: &'static str) -> impl Iterator<Item = Line<'static>> {
    lines.into_iter().map(move |mut line| {
        line.spans.insert(0, Span::raw(prefix));
        line
    })
}

fn boxed_row(content: Vec<Span<'static>>, inner: usize) -> Line<'static> {
    let used: usize = content.iter().map(|span| span.content.width()).sum();
    let mut spans = vec![Span::styled("│ ", fg(BORDER))];
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
    spans.push(Span::styled(" │", fg(BORDER)));
    Line::from(spans)
}

fn banner(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let logo: &[&str] = if width >= 84 {
        &LOGO
    } else if width >= 54 {
        &SMALL_LOGO
    } else {
        &[]
    };
    for (index, row) in logo.iter().enumerate() {
        let color =
            LOGO_GRADIENT[(index * LOGO_GRADIENT.len() / logo.len()).min(LOGO_GRADIENT.len() - 1)];
        lines.push(Line::from(Span::styled((*row).to_owned(), bold(color))));
    }
    if !logo.is_empty() {
        lines.push(Line::default());
    }
    let box_width = width.clamp(20, 100);
    let inner = box_width.saturating_sub(4);
    let title = format!(" {AGENT} v{} ", crate::VERSION);
    let top_fill = box_width.saturating_sub(title.width() + 3);
    lines.push(Line::from(vec![
        Span::styled("╭─", fg(BORDER)),
        Span::styled(title, bold(TITLE)),
        Span::styled(format!("{}╮", "─".repeat(top_fill)), fg(BORDER)),
    ]));
    let model = match (&app.conversation, &app.model) {
        (Some(info), _) => format!("{}/{}", provider_label(info.provider), info.model),
        (None, Some(model)) => format!("{}/{model}", provider_label(app.provider)),
        (None, None) => "none · /model to choose".into(),
    };
    let conversation = match &app.conversation {
        Some(info) => format!(
            "{} · {} · {} msgs",
            if info.title.is_empty() {
                "untitled"
            } else {
                info.title.as_str()
            },
            &info.id.to_string()[..8],
            info.message_count
        ),
        None => "new (created on first message)".into(),
    };
    let tools = match &app.tools {
        Some(tools) => format!("{} · {}", mode_label(tools.mode), tools.workspace.display()),
        None => "off".into(),
    };
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let rows = [
        ("agent", app.agent_label.clone()),
        ("model", model),
        ("conversation", conversation),
        ("tools", tools),
        (
            "host",
            format!("● connected · {}", app.state_directory.display()),
        ),
        ("cwd", cwd),
    ];
    lines.push(boxed_row(Vec::new(), inner));
    for (label, value) in rows {
        let value_width = inner.saturating_sub(14);
        let value: String = value.chars().take(value_width).collect();
        lines.push(boxed_row(
            vec![
                Span::styled(format!(" {label:<13}"), fg(LABEL)),
                Span::styled(value, fg(TEXT)),
            ],
            inner,
        ));
    }
    lines.push(boxed_row(Vec::new(), inner));
    for tip in [
        "/sessions or Ctrl+X to switch · /new for a fresh chat · /model to pick a model",
        "/tools on to let it work in this directory · /help for everything else",
    ] {
        let tip: String = tip.chars().take(inner.saturating_sub(4)).collect();
        lines.push(boxed_row(
            vec![Span::styled("  ▸ ", fg(ACCENT)), Span::styled(tip, fg(DIM))],
            inner,
        ));
    }
    lines.push(boxed_row(Vec::new(), inner));
    let subtitle = " local-first AI host ";
    let bottom_fill = box_width.saturating_sub(subtitle.width() + 4);
    lines.push(Line::from(vec![
        Span::styled(format!("╰{}", "─".repeat(bottom_fill)), fg(BORDER)),
        Span::styled(subtitle, fg(DIM)),
        Span::styled("─╯", fg(BORDER)),
    ]));
    lines.push(Line::from(Span::styled(
        "Welcome to Vorra! Type your message or /help for commands.",
        fg(DIM),
    )));
    lines
}

fn note_lines(kind: NoteKind, text: &str, width: usize) -> Vec<Line<'static>> {
    let (symbol, color) = match kind {
        NoteKind::Info => ("·", DIM),
        NoteKind::Ok => ("✓", OK),
        NoteKind::Warn => ("⚠", WARN),
        NoteKind::Error => ("✗", ERROR),
        NoteKind::Tool => ("┊", DIM),
    };
    wrap(text, width.saturating_sub(4))
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 {
                        format!("  {symbol} ")
                    } else {
                        "    ".into()
                    },
                    fg(color),
                ),
                Span::styled(
                    row,
                    if matches!(kind, NoteKind::Tool) {
                        fg(MUTED)
                    } else {
                        fg(color)
                    },
                ),
            ])
        })
        .collect()
}

fn entry_lines(app: &App, entry: &Entry, width: usize) -> Vec<Line<'static>> {
    match entry {
        Entry::Banner => banner(app, width),
        Entry::User(text) => {
            let mut lines = vec![Line::default()];
            for (index, row) in wrap(text, width.saturating_sub(2)).into_iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(if index == 0 { "❯ " } else { "  " }, bold(ACCENT)),
                    Span::styled(row, fg(TEXT)),
                ]));
            }
            lines
        }
        Entry::Assistant { text, footer } => {
            let mut lines = vec![
                Line::default(),
                Line::from(Span::styled(AGENT, bold(TITLE))),
            ];
            lines.extend(indent(
                markdown::render(text, (width.saturating_sub(2)) as u16),
                "  ",
            ));
            lines.push(Line::from(Span::styled(format!("  ─ {footer}"), fg(MUTED))));
            lines
        }
        Entry::Note(kind, text) => note_lines(*kind, text, width),
    }
}

fn spinner(elapsed: f32, label: &str) -> Line<'static> {
    let frame = SPIN[((elapsed * 10.0) as usize) % SPIN.len()];
    let slot = (elapsed / 2.5) as usize;
    let face = FACES[(slot * 7) % FACES.len()];
    let verb = if label == "thinking" {
        VERBS[(slot * 5) % VERBS.len()]
    } else {
        label
    };
    Line::from(vec![
        Span::styled(format!("{frame} "), fg(ACCENT)),
        Span::styled(format!("{face} "), fg(TITLE)),
        Span::styled(format!("{verb}..."), fg(DIM)),
        Span::styled(
            format!(" ({elapsed:.1}s)"),
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            "   type to queue · Ctrl+C twice cancels",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ),
    ])
}

fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width as usize;
    if app.dirty || app.rendered_width != area.width {
        let mut rendered = Vec::new();
        for entry in &app.entries {
            rendered.extend(entry_lines(app, entry, width));
        }
        app.rendered = rendered;
        app.rendered_width = area.width;
        app.dirty = false;
    }
    let mut live = Vec::new();
    if let Some(turn) = &app.turn {
        for note in &turn.notes {
            live.extend(note_lines(NoteKind::Tool, note, width));
        }
        if !turn.live.is_empty() {
            live.push(Line::default());
            live.push(Line::from(Span::styled(AGENT, bold(TITLE))));
            live.extend(indent(
                markdown::render(&turn.live, (width.saturating_sub(2)) as u16),
                "  ",
            ));
        }
        live.push(Line::default());
        live.push(spinner(turn.started.elapsed().as_secs_f32(), turn.label));
    }
    for (index, text) in app.queued.iter().enumerate() {
        let first = text.lines().next().unwrap_or_default();
        let preview: String = first.chars().take(width.saturating_sub(16)).collect();
        live.push(Line::from(Span::styled(
            format!("  ⧗ queued {}: {preview}", index + 1),
            fg(MUTED),
        )));
    }
    let height = area.height as usize;
    app.page = height.saturating_sub(2).max(1);
    let total = app.rendered.len() + live.len();
    let maximum = total.saturating_sub(height);
    app.scroll = app.scroll.min(maximum);
    let start = maximum - app.scroll;
    let visible: Vec<Line<'static>> = app
        .rendered
        .iter()
        .chain(live.iter())
        .skip(start)
        .take(height)
        .cloned()
        .collect();
    frame.render_widget(Paragraph::new(visible), area);
    if app.scroll > 0 {
        let hint = format!(" ↓ {} more lines · End ", app.scroll);
        let hint_width = hint.width() as u16;
        let rect = Rect {
            x: area.right().saturating_sub(hint_width + 1),
            y: area.bottom().saturating_sub(1),
            width: hint_width.min(area.width),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(BAR_BG).bg(ACCENT))),
            rect,
        );
    }
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect, layout: &super::input::Layout) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(fg(BORDER))
        .title(
            Line::from(Span::styled(
                " Enter send · Alt+Enter newline · / commands ",
                fg(MUTED),
            ))
            .alignment(Alignment::Right),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible = inner.height as usize;
    let top = layout.cursor_row.saturating_sub(visible.saturating_sub(1));
    let lines: Vec<Line> = if app.input.is_empty() {
        let placeholder = if app.turn.is_some() {
            "keep typing · messages queue until the reply finishes"
        } else {
            "Message Vorra…"
        };
        vec![Line::from(vec![
            Span::styled("❯ ", bold(ACCENT)),
            Span::styled(
                placeholder,
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
        ])]
    } else {
        layout
            .rows
            .iter()
            .enumerate()
            .skip(top)
            .take(visible)
            .map(|(index, row)| {
                Line::from(vec![
                    Span::styled(if index == 0 { "❯ " } else { "  " }, bold(ACCENT)),
                    Span::styled(row.clone(), fg(TEXT)),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), inner);
    if app.overlay.is_none() {
        frame.set_cursor_position((
            inner.x + 2 + layout.cursor_column as u16,
            inner.y + (layout.cursor_row - top) as u16,
        ));
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let bar = Style::default().bg(BAR_BG);
    let separator = Span::styled(" │ ", bar.fg(DIM));
    let (model, provider, title, messages) = match &app.conversation {
        Some(info) => (
            info.model.clone(),
            provider_label(info.provider),
            if info.title.is_empty() {
                "untitled".to_owned()
            } else {
                info.title.clone()
            },
            info.message_count,
        ),
        None => (
            app.model.clone().unwrap_or_else(|| "no model".into()),
            provider_label(app.provider),
            "new conversation".into(),
            0,
        ),
    };
    let width = area.width as usize;
    let mut spans = vec![
        Span::styled(
            format!(" {GLYPH} {model}"),
            bar.fg(TITLE).add_modifier(Modifier::BOLD),
        ),
        separator.clone(),
        Span::styled(provider, bar.fg(TEXT)),
        separator.clone(),
        Span::styled(format!("agent {}", app.agent_label), bar.fg(TEXT)),
    ];
    if width >= 60 {
        spans.push(separator.clone());
        spans.push(Span::styled(
            title.chars().take(28).collect::<String>(),
            bar.fg(TEXT),
        ));
    }
    if width >= 84 {
        spans.push(separator.clone());
        spans.push(Span::styled(format!("{messages} msgs"), bar.fg(TEXT)));
        spans.push(separator.clone());
        spans.push(match &app.tools {
            Some(tools) => Span::styled(
                format!("⚒ {}", mode_label(tools.mode)),
                bar.fg(
                    if matches!(tools.mode, crate::permissions::ExecutionMode::FullHost) {
                        ERROR
                    } else {
                        WARN
                    },
                ),
            ),
            None => Span::styled("tools off", bar.fg(DIM)),
        });
    }
    let (label, busy) = app.status_label();
    spans.push(separator.clone());
    spans.push(Span::styled(label, bar.fg(if busy { ACCENT } else { OK })));
    if let Some(turn) = &app.turn {
        spans.push(Span::styled(
            format!(" ⏱ {:.0}s", turn.started.elapsed().as_secs_f32()),
            bar.fg(DIM),
        ));
    }
    if !app.queued.is_empty() {
        spans.push(Span::styled(
            format!(" ▶ {}", app.queued.len()),
            bar.fg(ACCENT),
        ));
    }
    if app.stopped {
        spans.push(Span::styled(" ⚠ stopped", bar.fg(WARN)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

fn draw_completions(frame: &mut Frame, app: &App, input: Rect) {
    let candidates = app.completions();
    if candidates.is_empty() {
        return;
    }
    let shown = candidates.len().min(10);
    let selected = app.completion.min(candidates.len() - 1);
    let first = selected.saturating_sub(shown - 1);
    let height = shown as u16 + 2;
    let width = input.width.min(76);
    let rect = Rect {
        x: input.x,
        y: input.y.saturating_sub(height),
        width,
        height: height.min(input.y),
    };
    let lines: Vec<Line> = candidates
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(index, (name, description))| {
            let base = if index == selected {
                Style::default().bg(SELECT_BG)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(
                    format!(" {name:<14}"),
                    base.fg(if index == selected { TITLE } else { ACCENT })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{description} "),
                    base.fg(if index == selected { TEXT } else { DIM }),
                ),
            ])
        })
        .collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(fg(BORDER)),
        ),
        rect,
    );
}

pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width
        .min(area.width.saturating_sub(2))
        .max(10.min(area.width));
    let height = height
        .min(area.height.saturating_sub(2))
        .max(3.min(area.height));
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

pub(super) fn overlay_block(
    title: String,
    color: ratatui::style::Color,
    footer: &'static str,
) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(fg(color))
        .title(Span::styled(title, bold(color)))
        .title_bottom(Line::from(Span::styled(footer, fg(MUTED))).alignment(Alignment::Right))
}

fn draw_approval(frame: &mut Frame, app: &App, area: Rect) {
    let Some(Overlay::Approval {
        request, remaining, ..
    }) = &app.overlay
    else {
        return;
    };
    let width = area.width.saturating_sub(4).min(96);
    let inner_width = width.saturating_sub(4) as usize;
    let scope = &request.scope;
    let minutes = (request.expires_at - chrono::Utc::now().timestamp_millis()).max(0) / 60_000;
    let mut lines = Vec::new();
    let mut row = |label: &str, value: String, style: Style| {
        for (index, part) in wrap(&value, inner_width.saturating_sub(14))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(vec![
                Span::styled(
                    if index == 0 {
                        format!("{label:<14}")
                    } else {
                        " ".repeat(14)
                    },
                    fg(LABEL),
                ),
                Span::styled(part, style),
            ]));
        }
    };
    row("tool", scope.tool_name.clone(), bold(WARN));
    row("mode", mode_label(scope.mode).to_owned(), fg(TEXT));
    row("workspace", scope.workspace_root.clone(), fg(TEXT));
    if !scope.capabilities.is_empty() {
        row("capabilities", scope.capabilities.join(", "), fg(TEXT));
    }
    if !scope.targets.is_empty() {
        row("targets", scope.targets.join(", "), fg(TEXT));
    }
    row("expires", format!("in {minutes} min"), fg(TEXT));
    if *remaining > 0 {
        row("also pending", format!("{remaining} more"), fg(DIM));
    }
    lines.push(Line::from(Span::styled("arguments", fg(LABEL))));
    let arguments = serde_json::to_string_pretty(&request.arguments).unwrap_or_default();
    let budget = area.height.saturating_sub(lines.len() as u16 + 8) as usize;
    let mut argument_lines: Vec<String> = arguments
        .lines()
        .flat_map(|line| wrap(line, inner_width.saturating_sub(2)))
        .collect();
    if argument_lines.len() > budget.max(3) {
        argument_lines.truncate(budget.max(3));
        argument_lines.push("…".into());
    }
    for line in argument_lines {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            fg(super::theme::CODE),
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(
            " y ",
            Style::default()
                .fg(BAR_BG)
                .bg(OK)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" allow   ", fg(TEXT)),
        Span::styled(
            " n ",
            Style::default()
                .fg(BAR_BG)
                .bg(ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" deny   ", fg(TEXT)),
        Span::styled(
            " esc ",
            Style::default()
                .fg(BAR_BG)
                .bg(MUTED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" decide later", fg(TEXT)),
    ]));
    let rect = centered(area, width, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(overlay_block(" ⚠ approval required ".into(), WARN, "")),
        rect,
    );
}

fn draw_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(Overlay::Picker {
        kind,
        items,
        selected,
    }) = &app.overlay
    else {
        return;
    };
    let title = match kind {
        PickerKind::Session => " ◈ conversations ",
        PickerKind::Model => " ◈ models · starts a new conversation ",
        PickerKind::Agent => " ◈ agents · starts a new conversation ",
        PickerKind::Skill => " ◈ skills · ◈ marks skills assigned to the current agent ",
    };
    let footer = match kind {
        PickerKind::Skill => " ↑↓ move · Enter details · a toggle · Esc close ",
        _ => " ↑↓ move · Enter choose · Esc close ",
    };
    let width = area.width.saturating_sub(4).min(100);
    let visible = (area.height.saturating_sub(6) as usize)
        .clamp(1, 20)
        .min(items.len().max(1));
    let first = selected.saturating_sub(visible - 1);
    let label_width = (width as usize / 2).saturating_sub(4);
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(index, item)| {
            let base = if index == *selected {
                Style::default().bg(SELECT_BG)
            } else {
                Style::default()
            };
            let marker = if item.current {
                format!(" {GLYPH}")
            } else {
                String::new()
            };
            let label: String = item.label.chars().take(label_width).collect();
            let pad = label_width.saturating_sub(label.width() + marker.width());
            Line::from(vec![
                Span::styled(format!(" {:>2} ", index + 1), base.fg(ACCENT)),
                Span::styled(
                    label,
                    base.fg(if index == *selected { TITLE } else { TEXT })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(marker, base.fg(TITLE)),
                Span::styled(" ".repeat(pad + 2), base),
                Span::styled(item.detail.clone(), base.fg(DIM)),
            ])
        })
        .collect();
    let rect = centered(area, width, visible as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(overlay_block(title.into(), BORDER, footer)),
        rect,
    );
}

fn draw_secret(frame: &mut Frame, area: Rect, label: &str, length: usize, overflow: bool) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(format!("{:<8}", "label"), fg(LABEL)),
            Span::styled(label.to_owned(), bold(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<8}", "kind"), fg(LABEL)),
            Span::styled("api_secret · field value", fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<8}", "secret"), fg(LABEL)),
            Span::styled("•".repeat(length.min(64)), fg(TEXT)),
            Span::styled("▏", fg(TITLE)),
        ]),
        Line::default(),
        Line::from(Span::styled(
            "Stored in the system keychain; never shown, kept in history or sent to the model.",
            fg(DIM),
        )),
    ];
    if overflow {
        lines.push(Line::from(Span::styled(
            "longer than 8192 bytes · Backspace or Esc",
            fg(ERROR),
        )));
    }
    let rect = centered(area, 88, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(overlay_block(
                " ◈ add to the local vault ".into(),
                BORDER,
                " Enter save · Esc cancel ",
            )),
        rect,
    );
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let mut lines = vec![Line::from(Span::styled(
        "(^_^)? Available Commands",
        bold(TITLE),
    ))];
    for (name, description) in COMMANDS.iter().filter(|(name, _)| *name != "/exit") {
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<15}"), bold(ACCENT)),
            Span::styled(*description, fg(TEXT)),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Keys", bold(TITLE))));
    for (key, description) in KEYS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<30}"), bold(LABEL)),
            Span::styled(description, fg(TEXT)),
        ]));
    }
    let rect = centered(area, 96, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(overlay_block(
            format!(" {AGENT} help "),
            BORDER,
            " any key to close ",
        )),
        rect,
    );
}
