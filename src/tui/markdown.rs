use std::mem;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme;

const MIN_WIDTH: u16 = 8;

pub fn render(text: &str, width: u16) -> Vec<Line<'static>> {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::new(usize::from(width.max(MIN_WIDTH)));
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

type Segment = (String, Style);

struct ListLevel {
    next: Option<u64>,
    indent: usize,
}

struct Code {
    lang: String,
    body: String,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Segment>>>,
    row: Vec<Vec<Segment>>,
    header: usize,
}

struct Renderer {
    width: usize,
    lines: Vec<Line<'static>>,
    segments: Vec<Segment>,
    styles: Vec<Style>,
    lists: Vec<ListLevel>,
    bullet: Option<String>,
    quote: usize,
    links: Vec<(String, usize)>,
    code: Option<Code>,
    table: Option<Table>,
    gap: Option<usize>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            segments: Vec::new(),
            styles: vec![theme::fg(theme::TEXT)],
            lists: Vec::new(),
            bullet: None,
            quote: 0,
            links: Vec::new(),
            code: None,
            table: None,
            gap: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        self.flush_code();
        self.flush_table();
        self.flush_bullet();
        self.lines
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => match &mut self.code {
                Some(code) => code.body.push_str(&text),
                None => {
                    let style = self.style();
                    self.segments.push((text.into_string(), style));
                }
            },
            Event::Code(code) => self.segments.push((code.into_string(), code_style())),
            Event::Html(html) | Event::InlineHtml(html) => {
                self.segments
                    .push((html.into_string(), theme::fg(theme::MUTED)));
            }
            Event::SoftBreak | Event::HardBreak => {
                let style = self.style();
                self.segments.push(("\n".to_owned(), style));
            }
            Event::Rule => self.rule(),
            Event::TaskListMarker(done) => {
                let marker = if done { "[x] " } else { "[ ] " };
                self.segments
                    .push((marker.to_owned(), theme::fg(theme::ACCENT)));
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock | Tag::Table(_) => {
                self.flush();
                if matches!(tag, Tag::Table(_)) {
                    self.table = Some(Table::default());
                }
            }
            Tag::Heading { level, .. } => {
                self.flush();
                let style = theme::bold(theme::TITLE);
                self.styles.push(if level == HeadingLevel::H1 {
                    style.add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
                self.push_modifier(Modifier::ITALIC);
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(Code {
                    lang,
                    body: String::new(),
                });
            }
            Tag::List(start) => {
                self.flush();
                self.flush_bullet();
                self.lists.push(ListLevel {
                    next: start,
                    indent: 2,
                });
            }
            Tag::Item => {
                self.flush();
                self.flush_bullet();
                let marker = match self.lists.last_mut() {
                    Some(level) => {
                        let marker = match &mut level.next {
                            Some(number) => {
                                let marker = format!("{number}. ");
                                *number = number.saturating_add(1);
                                marker
                            }
                            None => "• ".to_owned(),
                        };
                        level.indent = str_width(&marker);
                        marker
                    }
                    None => "• ".to_owned(),
                };
                self.bullet = Some(marker);
            }
            Tag::Emphasis => self.push_modifier(Modifier::ITALIC),
            Tag::Strong => self.push_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.push_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.links
                    .push((dest_url.into_string(), self.segments.len()));
                let style = self
                    .style()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::UNDERLINED);
                self.styles.push(style);
            }
            Tag::TableHead => self.styles.push(theme::bold(theme::LABEL)),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => {
                self.flush();
                self.end_block();
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.pop_style();
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.pop_style();
                self.quote = self.quote.saturating_sub(1);
                self.end_block();
            }
            TagEnd::CodeBlock => {
                self.flush_code();
                self.end_block();
            }
            TagEnd::List(_) => {
                self.flush();
                self.flush_bullet();
                self.lists.pop();
                self.end_block();
            }
            TagEnd::Item => {
                self.flush();
                self.flush_bullet();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link | TagEnd::Image => {
                self.pop_style();
                self.close_link();
            }
            TagEnd::TableCell => {
                let cell = mem::take(&mut self.segments);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                self.pop_style();
                if let Some(table) = &mut self.table {
                    table.rows.push(mem::take(&mut table.row));
                    table.header = table.rows.len();
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(mem::take(&mut table.row));
                }
            }
            TagEnd::Table => {
                self.flush_table();
                self.end_block();
            }
            _ => {}
        }
    }

    fn style(&self) -> Style {
        self.styles
            .last()
            .copied()
            .unwrap_or_else(|| theme::fg(theme::TEXT))
    }

    fn push_modifier(&mut self, modifier: Modifier) {
        let style = self.style().add_modifier(modifier);
        self.styles.push(style);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn end_block(&mut self) {
        if self.lists.is_empty() {
            self.gap = Some(self.quote);
        }
    }

    fn emit(&mut self, line: Line<'static>) {
        if let Some(depth) = self.gap.take()
            && !self.lines.is_empty()
        {
            self.lines.push(Line::from(quote_spans(depth)));
        }
        self.lines.push(line);
    }

    fn prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let indent: usize = self.lists.iter().map(|level| level.indent).sum();
        let mut first = quote_spans(self.quote);
        let mut rest = first.clone();
        match self.bullet.take() {
            Some(marker) => {
                let inner = self.lists.last().map_or(0, |level| level.indent);
                push_pad(&mut first, indent.saturating_sub(inner));
                first.push(Span::styled(marker, theme::fg(theme::ACCENT)));
            }
            None => push_pad(&mut first, indent),
        }
        push_pad(&mut rest, indent);
        (first, rest)
    }

    fn flush(&mut self) {
        if self.segments.iter().all(|(text, _)| text.trim().is_empty()) {
            self.segments.clear();
            return;
        }
        let segments = mem::take(&mut self.segments);
        let (first, rest) = self.prefixes();
        for line in wrap(segments, first, &rest, self.width) {
            self.emit(line);
        }
    }

    fn flush_bullet(&mut self) {
        if self.bullet.is_some() {
            let (first, _) = self.prefixes();
            self.emit(Line::from(first));
        }
    }

    fn close_link(&mut self) {
        let Some((url, start)) = self.links.pop() else {
            return;
        };
        let label: String = self
            .segments
            .get(start..)
            .unwrap_or_default()
            .iter()
            .map(|(text, _)| text.as_str())
            .collect();
        let label = label.trim();
        if !url.is_empty() && label != url && Some(label) != url.strip_prefix("mailto:") {
            self.segments
                .push((format!(" ({url})"), theme::fg(theme::DIM)));
        }
    }

    fn rule(&mut self) {
        self.flush();
        let (mut first, _) = self.prefixes();
        let available = self.width.saturating_sub(spans_width(&first));
        first.push(Span::styled("─".repeat(available), theme::fg(theme::MUTED)));
        self.emit(Line::from(first));
        self.end_block();
    }

    fn flush_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        let (first, rest) = self.prefixes();
        let mut prefix = Some(first);
        if !code.lang.is_empty() {
            let mut spans = prefix.take().unwrap_or_else(|| rest.clone());
            spans.push(Span::styled(code.lang, theme::fg(theme::DIM)));
            self.emit(Line::from(spans));
        }
        for raw in code.body.lines() {
            let mut spans = prefix.take().unwrap_or_else(|| rest.clone());
            let available = self.width.saturating_sub(spans_width(&spans) + 2);
            spans.push(Span::styled("│ ", theme::fg(theme::MUTED)));
            spans.push(Span::styled(
                truncate(&raw.replace('\t', "    "), available),
                code_style(),
            ));
            self.emit(Line::from(spans));
        }
    }

    fn flush_table(&mut self) {
        let Some(mut table) = self.table.take() else {
            return;
        };
        if !table.row.is_empty() {
            table.rows.push(mem::take(&mut table.row));
        }
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(segments_width(cell));
            }
        }
        let (first, rest) = self.prefixes();
        let mut prefix = Some(first);
        let separator = theme::fg(theme::MUTED);
        for (index, row) in table.rows.into_iter().enumerate() {
            let mut spans = prefix.take().unwrap_or_else(|| rest.clone());
            let mut cells = row.into_iter();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", separator));
                }
                let cell = cells.next().unwrap_or_default();
                let used = segments_width(&cell);
                spans.extend(
                    cell.into_iter()
                        .map(|(text, style)| Span::styled(text, style)),
                );
                if column + 1 < columns {
                    push_pad(&mut spans, width.saturating_sub(used));
                }
            }
            self.emit(Line::from(truncate_spans(spans, self.width)));
            if index + 1 == table.header {
                let mut spans = rest.clone();
                let rule = widths
                    .iter()
                    .map(|width| "─".repeat(*width))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                spans.push(Span::styled(rule, separator));
                self.emit(Line::from(truncate_spans(spans, self.width)));
            }
        }
    }
}

enum Token {
    Word(Vec<Segment>),
    Space(Style),
    Break,
}

fn tokenize(segments: Vec<Segment>) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut word: Vec<Segment> = Vec::new();
    for (text, style) in segments {
        for c in text.chars() {
            if c.is_whitespace() {
                if !word.is_empty() {
                    tokens.push(Token::Word(mem::take(&mut word)));
                }
                if c == '\n' {
                    tokens.push(Token::Break);
                } else if !matches!(tokens.last(), Some(Token::Space(_))) {
                    tokens.push(Token::Space(style));
                }
            } else {
                match word.last_mut() {
                    Some((piece, piece_style)) if *piece_style == style => piece.push(c),
                    _ => word.push((c.to_string(), style)),
                }
            }
        }
    }
    if !word.is_empty() {
        tokens.push(Token::Word(word));
    }
    tokens
}

struct Wrapper<'a> {
    lines: Vec<Line<'static>>,
    line: Vec<Span<'static>>,
    used: usize,
    empty: bool,
    rest: &'a [Span<'static>],
    rest_width: usize,
}

impl Wrapper<'_> {
    fn newline(&mut self) {
        let line = mem::replace(&mut self.line, self.rest.to_vec());
        self.lines.push(Line::from(line));
        self.used = self.rest_width;
        self.empty = true;
    }
}

fn wrap(
    segments: Vec<Segment>,
    first: Vec<Span<'static>>,
    rest: &[Span<'static>],
    width: usize,
) -> Vec<Line<'static>> {
    let mut wrapper = Wrapper {
        lines: Vec::new(),
        used: spans_width(&first),
        line: first,
        empty: true,
        rest,
        rest_width: spans_width(rest),
    };
    let mut space = None;
    for token in tokenize(segments) {
        match token {
            Token::Space(style) => {
                if !wrapper.empty {
                    space = Some(style);
                }
            }
            Token::Break => {
                wrapper.newline();
                space = None;
            }
            Token::Word(pieces) => {
                let word_width = segments_width(&pieces);
                if let Some(style) = space.take() {
                    if wrapper.used + 1 + word_width <= width {
                        wrapper.line.push(Span::styled(" ", style));
                        wrapper.used += 1;
                    } else {
                        wrapper.newline();
                    }
                }
                if wrapper.used + word_width > width
                    && !wrapper.empty
                    && wrapper.rest_width + word_width <= width
                {
                    wrapper.newline();
                }
                if wrapper.used + word_width <= width {
                    wrapper.line.extend(
                        pieces
                            .into_iter()
                            .map(|(text, style)| Span::styled(text, style)),
                    );
                    wrapper.used += word_width;
                } else {
                    for (text, style) in pieces {
                        let mut buffer = String::new();
                        for c in text.chars() {
                            let char_width = char_width(c);
                            if wrapper.used + char_width > width && !wrapper.empty {
                                if !buffer.is_empty() {
                                    wrapper
                                        .line
                                        .push(Span::styled(mem::take(&mut buffer), style));
                                }
                                wrapper.newline();
                            }
                            buffer.push(c);
                            wrapper.used += char_width;
                            wrapper.empty = false;
                        }
                        if !buffer.is_empty() {
                            wrapper.line.push(Span::styled(buffer, style));
                        }
                    }
                }
                wrapper.empty = false;
            }
        }
    }
    if !wrapper.empty {
        wrapper.lines.push(Line::from(wrapper.line));
    }
    wrapper.lines
}

fn code_style() -> Style {
    Style::default().fg(theme::CODE).bg(theme::CODE_BG)
}

fn quote_spans(depth: usize) -> Vec<Span<'static>> {
    (0..depth)
        .map(|_| Span::styled("▎ ", theme::fg(theme::DIM)))
        .collect()
}

fn push_pad(spans: &mut Vec<Span<'static>>, width: usize) {
    if width > 0 {
        spans.push(Span::raw(" ".repeat(width)));
    }
}

fn str_width(text: &str) -> usize {
    text.width()
}

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn segments_width(segments: &[Segment]) -> usize {
    segments.iter().map(|(text, _)| str_width(text)).sum()
}

fn truncate(text: &str, max: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|c| {
            used += char_width(*c);
            used <= max
        })
        .collect()
}

fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut remaining = max;
    let mut result = Vec::with_capacity(spans.len());
    for span in spans {
        let width = span.width();
        if width <= remaining {
            remaining -= width;
            result.push(span);
        } else {
            result.push(Span::styled(truncate(&span.content, remaining), span.style));
            break;
        }
    }
    result
}
