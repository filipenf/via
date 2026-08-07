//! Ratatui application state for the ACP TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use xai_grok_markdown::{MarkdownStyle, Syntect, render_markdown_ratatui};
use xai_ratatui_textarea::{TextArea, TextAreaState};

use super::protocol::{HostToTui, ModelChoice, TranscriptKind};

const MAX_TRANSCRIPT_ENTRIES: usize = 500;
/// Cap multiline prompt growth so the transcript keeps most of the pane.
const MAX_PROMPT_HEIGHT: u16 = 6;
/// Fallback PageUp/PageDown step before the first draw records a viewport height.
const DEFAULT_PAGE_SCROLL: u16 = 10;

/// Baked syntect themes for light/dark polarity.
///
/// Full Ghostty palette → token color mapping is future work; for now we only
/// switch between these two TextMate themes based on background luminance.
const TM_THEME_DARK: &[u8] = include_bytes!("assets/tokyo-night.tmTheme");
const TM_THEME_LIGHT: &[u8] = include_bytes!("assets/grok-day.tmTheme");

struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
    /// Cached markdown lines for agent/thought; `None` means dirty / not yet rendered.
    md_lines: Option<Vec<Line<'static>>>,
}

struct ProgressState {
    active_tools: Vec<(String, String)>,
    label: String,
    frame: u8,
}

pub(super) struct App {
    agent_id: String,
    role: String,
    model: Option<String>,
    models: Vec<ModelChoice>,
    /// When set, the model picker overlay is open at this index.
    model_picker: Option<usize>,
    provider_error: Option<String>,
    transcript: Vec<TranscriptEntry>,
    progress: Option<ProgressState>,
    prompt: TextArea,
    prompt_state: TextAreaState,
    show_input: bool,
    /// Offset from the live bottom: 0 = follow newest; larger = scrolled into history.
    scroll_from_bottom: u16,
    /// Last drawn transcript body height; drives PageUp/PageDown step size.
    transcript_viewport_rows: u16,
    quit: bool,
    /// Light appearance (Ghostty background luminance / host [`HostToTui::Appearance`]).
    light: bool,
    /// Syntax highlighter for fenced code blocks (shared across renders).
    syntect: Syntect,
    md_style: MarkdownStyle,
}

#[derive(Debug)]
pub(super) enum InputEvent {
    None,
    Quit,
    Submit(String),
    SetModel(String),
}

impl App {
    pub(super) fn new(agent_id: String, role: String, show_input: bool, light: bool) -> Self {
        let mut app = Self {
            agent_id,
            role,
            model: None,
            models: Vec::new(),
            model_picker: None,
            provider_error: None,
            transcript: Vec::new(),
            progress: None,
            prompt: TextArea::new(),
            prompt_state: TextAreaState::default(),
            show_input,
            scroll_from_bottom: 0,
            transcript_viewport_rows: 0,
            quit: false,
            light,
            syntect: Syntect::new(if light { TM_THEME_LIGHT } else { TM_THEME_DARK }),
            md_style: MarkdownStyle::default(),
        };
        app.restyle_prompt();
        app
    }

    fn set_appearance(&mut self, light: bool) {
        if self.light == light {
            return;
        }
        self.light = light;
        self.syntect = Syntect::new(if light { TM_THEME_LIGHT } else { TM_THEME_DARK });
        self.restyle_prompt();
        for entry in &mut self.transcript {
            entry.md_lines = None;
        }
    }

    fn restyle_prompt(&mut self) {
        let prompt_bg = self.prompt_bg();
        if self.light {
            self.prompt.selection_style = Style::default()
                .bg(Color::Rgb(180, 200, 240))
                .fg(Color::Black);
            self.prompt.scrollbar_track_style = Style::default().bg(prompt_bg);
            self.prompt.scrollbar_thumb_style =
                Style::default().fg(Color::Rgb(140, 140, 150)).bg(prompt_bg);
        } else {
            self.prompt.selection_style = Style::default()
                .bg(Color::Rgb(49, 62, 115))
                .fg(Color::Rgb(192, 202, 245));
            self.prompt.scrollbar_track_style = Style::default().bg(Color::Rgb(32, 35, 53));
            self.prompt.scrollbar_thumb_style = Style::default()
                .fg(Color::Rgb(42, 46, 65))
                .bg(Color::Rgb(32, 35, 53));
        }
    }

    fn prompt_bg(&self) -> Color {
        if self.light {
            Color::Rgb(230, 230, 234)
        } else {
            Color::Rgb(40, 40, 44)
        }
    }

    fn picker_bg(&self) -> Color {
        if self.light {
            Color::Rgb(245, 245, 248)
        } else {
            Color::Rgb(20, 20, 24)
        }
    }

    fn picker_fg(&self) -> Color {
        if self.light {
            Color::Black
        } else {
            Color::White
        }
    }

    fn muted_fg(&self) -> Color {
        if self.light {
            Color::Gray
        } else {
            Color::DarkGray
        }
    }

    pub(super) fn seed_demo(&mut self) {
        self.push(
            TranscriptKind::System,
            "Demo — Enter send · Shift+Enter newline · /model · PgUp/PgDn scroll · q quit.".into(),
        );
        for i in 1..=8 {
            self.push(
                TranscriptKind::Agent,
                format!("Sample agent output line {i}"),
            );
        }
        self.push(
            TranscriptKind::Agent,
            "# Markdown demo\n\nHello **bold** and `code`.\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n".into(),
        );
        self.push(
            TranscriptKind::Thought,
            "Thinking about the next step…".into(),
        );
        self.update_progress("demo".into(), "working".into(), true);
    }

    pub(super) fn push_system(&mut self, text: String) {
        self.push(TranscriptKind::System, text);
    }

    pub(super) fn should_quit(&self) -> bool {
        self.quit
    }

    pub(super) fn request_quit(&mut self) {
        self.quit = true;
    }

    pub(super) fn apply_host(&mut self, msg: HostToTui) {
        match msg {
            HostToTui::Transcript { kind, text } => {
                if text.is_empty() {
                    return;
                }
                // Clear local thinking spinner once agent output arrives (avoids sticky `__turn`
                // when the TUI armed it on submit before the host echoed progress).
                if matches!(kind, TranscriptKind::Agent) {
                    self.update_progress("__turn".into(), String::new(), false);
                }
                if let Some(entry) = self
                    .transcript
                    .last_mut()
                    .filter(|entry| entry.kind == kind)
                {
                    entry.text.push_str(&text);
                    entry.md_lines = None;
                } else {
                    self.push(kind, text);
                }
            }
            HostToTui::Progress { id, label, active } => {
                self.update_progress(id, label, active);
            }
            HostToTui::SessionStatus {
                model,
                models,
                provider_error,
                clear_provider_error,
            } => {
                if let Some(model) = model.filter(|m| !m.is_empty()) {
                    self.model = Some(model);
                }
                if let Some(mut models) = models {
                    sort_model_choices(&mut models);
                    self.models = models;
                    if self.model_picker.is_some() {
                        self.model_picker = self.current_model_index().or({
                            if self.models.is_empty() {
                                None
                            } else {
                                Some(0)
                            }
                        });
                    }
                }
                if clear_provider_error {
                    self.provider_error = None;
                } else if let Some(error) = provider_error.filter(|e| !e.is_empty()) {
                    self.provider_error = Some(error);
                }
            }
            HostToTui::Appearance { light } => {
                self.set_appearance(light);
            }
            HostToTui::Shutdown => {
                self.quit = true;
            }
        }
    }

    pub(super) fn tick_progress(&mut self) {
        if let Some(progress) = &mut self.progress {
            progress.frame = progress.frame.wrapping_add(1);
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> InputEvent {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return InputEvent::Quit;
        }

        if self.model_picker.is_some() {
            return self.handle_model_picker_key(key);
        }

        let prompt_empty = self.prompt.is_empty();

        match key.code {
            KeyCode::Char('q') if prompt_empty && !key.modifiers.contains(KeyModifiers::SHIFT) => {
                InputEvent::Quit
            }
            KeyCode::Esc if prompt_empty => InputEvent::Quit,
            KeyCode::Esc => {
                self.prompt.set_text("");
                InputEvent::None
            }
            // Transcript scroll: Page*/Home/End always; Up/Down only when prompt empty
            // so multiline editing can move the caret.
            KeyCode::PageUp => {
                let step = self.page_scroll_step();
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(step);
                InputEvent::None
            }
            KeyCode::PageDown => {
                let step = self.page_scroll_step();
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(step);
                InputEvent::None
            }
            KeyCode::Home => {
                self.scroll_from_bottom = u16::MAX;
                InputEvent::None
            }
            KeyCode::End => {
                self.scroll_from_bottom = 0;
                InputEvent::None
            }
            KeyCode::Up if prompt_empty => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
                InputEvent::None
            }
            KeyCode::Down if prompt_empty => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
                InputEvent::None
            }
            // Plain Enter submits; Shift/Alt+Enter insert a newline.
            KeyCode::Enter if self.show_input => {
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.prompt
                        .input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    return InputEvent::None;
                }
                let text = self.prompt.text().trim().to_string();
                if text.is_empty() {
                    return InputEvent::None;
                }
                self.prompt.set_text("");
                if let Some(event) = self.try_handle_slash_command(&text) {
                    self.scroll_from_bottom = 0;
                    return event;
                }
                self.push(TranscriptKind::User, text.clone());
                self.scroll_from_bottom = 0;
                self.update_progress("__turn".into(), "Thinking".into(), true);
                InputEvent::Submit(text)
            }
            _ if self.show_input => {
                self.prompt.input(key);
                InputEvent::None
            }
            _ => InputEvent::None,
        }
    }

    /// Local slash commands (not forwarded to the ACP agent).
    ///
    /// - `/model` — open the picker
    /// - `/model <slug>` — set by value/name match
    fn try_handle_slash_command(&mut self, text: &str) -> Option<InputEvent> {
        let trimmed = text.trim();
        let (cmd, rest) = match trimmed.strip_prefix('/') {
            Some(rest) => rest
                .split_once(char::is_whitespace)
                .map(|(c, r)| (c, r.trim()))
                .unwrap_or((rest, "")),
            None => return None,
        };
        if !cmd.eq_ignore_ascii_case("model") {
            return None;
        }

        self.push(TranscriptKind::User, trimmed.to_string());

        if rest.is_empty() {
            return Some(self.open_model_picker());
        }
        Some(self.set_model_from_query(rest))
    }

    fn open_model_picker(&mut self) -> InputEvent {
        if self.models.is_empty() {
            self.push_system(
                "No model list from this agent (config option \"model\" unavailable).".into(),
            );
            return InputEvent::None;
        }
        self.model_picker = Some(self.current_model_index().unwrap_or(0));
        InputEvent::None
    }

    fn current_model_index(&self) -> Option<usize> {
        self.model
            .as_ref()
            .and_then(|current| self.models.iter().position(|m| &m.value == current))
    }

    fn set_model_from_query(&mut self, query: &str) -> InputEvent {
        if self.models.is_empty() {
            self.push_system(
                "No model list from this agent (config option \"model\" unavailable).".into(),
            );
            return InputEvent::None;
        }
        match resolve_model_choice(&self.models, query) {
            Some(choice) => {
                let value = choice.value.clone();
                let label = choice
                    .name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .unwrap_or(value.as_str());
                self.push_system(format!("Switching model to {label}…"));
                InputEvent::SetModel(value)
            }
            None => {
                let available = self
                    .models
                    .iter()
                    .map(|m| {
                        m.name
                            .as_deref()
                            .filter(|n| !n.is_empty())
                            .unwrap_or(m.value.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                self.push_system(format!(
                    "Unknown model '{query}'. Try `/model` to pick, or: {available}"
                ));
                InputEvent::None
            }
        }
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> InputEvent {
        let Some(idx) = self.model_picker else {
            return InputEvent::None;
        };
        let len = self.models.len();
        if len == 0 {
            self.model_picker = None;
            return InputEvent::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.model_picker = None;
                InputEvent::None
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.model_picker = Some(idx.saturating_sub(1));
                InputEvent::None
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.model_picker = Some((idx + 1).min(len - 1));
                InputEvent::None
            }
            KeyCode::Home => {
                self.model_picker = Some(0);
                InputEvent::None
            }
            KeyCode::End => {
                self.model_picker = Some(len - 1);
                InputEvent::None
            }
            KeyCode::Enter => {
                let value = self.models[idx].value.clone();
                self.model_picker = None;
                InputEvent::SetModel(value)
            }
            _ => InputEvent::None,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame<'_>) {
        self.refresh_markdown_caches();

        let area = frame.area();
        let prompt_height = if self.show_input {
            let width = area.width.max(1);
            self.prompt
                .desired_height(width)
                .clamp(1, MAX_PROMPT_HEIGHT)
        } else {
            0
        };
        let constraints = if self.show_input {
            vec![
                Constraint::Length(self.header_height()),
                Constraint::Min(1),
                Constraint::Length(prompt_height),
            ]
        } else {
            vec![Constraint::Length(self.header_height()), Constraint::Min(1)]
        };
        let chunks = Layout::vertical(constraints).split(area);
        self.transcript_viewport_rows = chunks[1].height;

        frame.render_widget(self.header_widget(chunks[1].height), chunks[0]);
        frame.render_widget(self.transcript_widget(chunks[1].height), chunks[1]);
        if self.show_input {
            self.draw_prompt(frame, chunks[2]);
        }
        if self.model_picker.is_some() {
            self.draw_model_picker(frame, area);
        }
    }

    /// Rows to jump on PageUp/PageDown: nearly one viewport, with one row of overlap.
    fn page_scroll_step(&self) -> u16 {
        match self.transcript_viewport_rows {
            0 => DEFAULT_PAGE_SCROLL,
            n => n.saturating_sub(1).max(1),
        }
    }

    fn draw_model_picker(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(selected) = self.model_picker else {
            return;
        };
        let width = area.width.saturating_sub(4).clamp(24, 56);
        let height = (self.models.len() as u16)
            .saturating_add(2)
            .min(area.height.saturating_sub(2))
            .max(3);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let dialog = Rect {
            x,
            y,
            width,
            height,
        };

        // Clear first so transcript cells under the dialog cannot show through.
        frame.render_widget(Clear, dialog);

        let picker_bg = self.picker_bg();
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let title_style = Style::default()
            .fg(Color::Cyan)
            .bg(picker_bg)
            .add_modifier(Modifier::BOLD);
        let row_style = Style::default().fg(self.picker_fg()).bg(picker_bg);
        let selected_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);

        let mut lines = vec![Line::from(Span::styled(
            pad_picker_row(" /model — Enter · Esc", inner_width),
            title_style,
        ))];
        let visible = height.saturating_sub(2) as usize;
        let start = selected.saturating_sub(visible.saturating_sub(1) / 2);
        let end = (start + visible).min(self.models.len());
        for (i, choice) in self.models[start..end].iter().enumerate() {
            let idx = start + i;
            let label = model_choice_label(choice);
            let marker = if idx == selected { "› " } else { "  " };
            let style = if idx == selected {
                selected_style
            } else {
                row_style
            };
            lines.push(Line::from(Span::styled(
                pad_picker_row(&format!("{marker}{label}"), inner_width),
                style,
            )));
        }
        // Pad remaining rows so the dialog body is fully opaque.
        while lines.len() < visible + 1 {
            lines.push(Line::from(Span::styled(
                pad_picker_row("", inner_width),
                row_style,
            )));
        }

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan).bg(picker_bg))
                .style(Style::default().bg(picker_bg)),
        );
        frame.render_widget(widget, dialog);
    }

    fn draw_prompt(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let prompt_bg = self.prompt_bg();
        // Elevated background behind the textarea.
        let bar = Block::default().style(Style::default().bg(prompt_bg));
        frame.render_widget(bar, area);

        if self.prompt.is_empty() {
            let placeholder = Paragraph::new(Line::from(Span::styled(
                " Ask the agent… (Enter send · Shift+Enter newline · /model)",
                Style::default().fg(self.muted_fg()).bg(prompt_bg),
            )));
            frame.render_widget(placeholder, area);
            return;
        }

        frame.render_stateful_widget_ref(&self.prompt, area, &mut self.prompt_state);
    }

    fn header_height(&self) -> u16 {
        if self.provider_error.is_some() { 3 } else { 2 }
    }

    fn header_widget(&self, transcript_rows: u16) -> Paragraph<'static> {
        let title_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(self.muted_fg());
        let warn = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);

        let model = self
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
            .unwrap_or("(unknown)");
        let progress = self
            .progress
            .as_ref()
            .map(|p| {
                let spin = match p.frame % 4 {
                    0 => '|',
                    1 => '/',
                    2 => '-',
                    _ => '\\',
                };
                format!("  {spin} {}", p.label)
            })
            .unwrap_or_default();

        let from_bottom = self.scroll_offset(transcript_rows.max(1));
        let scroll_hint = if from_bottom == 0 {
            String::new()
        } else {
            format!("  ↑{from_bottom}")
        };

        let mut lines = vec![Line::from(vec![
            Span::styled(self.role.clone(), title_style),
            Span::styled(format!("  ({})", self.agent_id), muted),
            Span::raw("  ·  "),
            Span::styled(format!("model: {model}"), muted),
            if self.models.is_empty() {
                Span::raw("")
            } else {
                Span::styled(" · /model", muted)
            },
            Span::styled(progress, Style::default().fg(Color::Magenta)),
            Span::styled(scroll_hint, muted),
        ])];
        if let Some(error) = &self.provider_error {
            lines.push(Line::from(Span::styled(format!("! {error}"), warn)));
        }

        // Bottom border is the only chrome between header and transcript (no transcript box).
        Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM))
    }

    fn transcript_widget(&self, viewport_rows: u16) -> Paragraph<'static> {
        let body_rows = viewport_rows.max(1);
        let lines = self.wrapped_lines();
        let max_scroll = (lines.len() as u16).saturating_sub(body_rows);
        let from_bottom = self.scroll_from_bottom.min(max_scroll);
        let scroll_y = max_scroll.saturating_sub(from_bottom);

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0))
    }

    fn scroll_offset(&self, viewport_rows: u16) -> u16 {
        let total = self.wrapped_lines().len() as u16;
        let max_scroll = total.saturating_sub(viewport_rows.max(1));
        self.scroll_from_bottom.min(max_scroll)
    }

    fn refresh_markdown_caches(&mut self) {
        for entry in &mut self.transcript {
            if !matches!(entry.kind, TranscriptKind::Agent | TranscriptKind::Thought) {
                continue;
            }
            if entry.md_lines.is_some() {
                continue;
            }
            let (lines, _) =
                render_markdown_ratatui(&entry.text, self.md_style, true, Some(&self.syntect));
            entry.md_lines = Some(lines);
        }
    }

    fn wrapped_lines(&self) -> Vec<Line<'static>> {
        if self.transcript.is_empty() {
            return vec![Line::from(Span::styled(
                "No transcript yet.",
                Style::default().fg(self.muted_fg()),
            ))];
        }

        let mut lines = Vec::new();
        for entry in &self.transcript {
            match entry.kind {
                TranscriptKind::Agent | TranscriptKind::Thought => {
                    let label = match entry.kind {
                        TranscriptKind::Thought => "thought",
                        _ => "agent",
                    };
                    let label_style = match entry.kind {
                        TranscriptKind::Thought => Style::default().fg(Color::Cyan),
                        _ => Style::default().fg(Color::Green),
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{label}:"),
                        label_style.add_modifier(Modifier::BOLD),
                    )));
                    if let Some(md) = &entry.md_lines {
                        lines.extend(md.iter().cloned());
                    } else {
                        // Cache not warm yet (shouldn't happen after refresh); plain fallback.
                        for part in entry.text.split('\n') {
                            lines.push(Line::from(Span::styled(part.to_string(), label_style)));
                        }
                    }
                    lines.push(Line::from(""));
                }
                kind => {
                    let (prefix, style) = match kind {
                        TranscriptKind::User => (
                            "you",
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        TranscriptKind::Tool => ("tool", Style::default().fg(Color::Magenta)),
                        TranscriptKind::System => ("system", Style::default().fg(self.muted_fg())),
                        TranscriptKind::Agent | TranscriptKind::Thought => unreachable!(),
                    };
                    for (idx, part) in entry.text.split('\n').enumerate() {
                        if idx == 0 {
                            lines.push(Line::from(vec![
                                Span::styled(format!("{prefix}: "), style),
                                Span::styled(part.to_string(), style),
                            ]));
                        } else {
                            lines.push(Line::from(Span::styled(part.to_string(), style)));
                        }
                    }
                }
            }
        }
        lines
    }

    fn push(&mut self, kind: TranscriptKind, text: String) {
        self.transcript.push(TranscriptEntry {
            kind,
            text,
            md_lines: None,
        });
        if self.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            let drain = self.transcript.len() - MAX_TRANSCRIPT_ENTRIES;
            self.transcript.drain(..drain);
        }
    }

    fn update_progress(&mut self, id: String, label: String, active: bool) {
        let progress = self.progress.get_or_insert_with(|| ProgressState {
            active_tools: Vec::new(),
            label: String::new(),
            frame: 0,
        });
        if active {
            if let Some((_, existing)) = progress
                .active_tools
                .iter_mut()
                .find(|(existing_id, _)| *existing_id == id)
            {
                *existing = label;
            } else {
                progress.active_tools.push((id, label));
            }
        } else {
            progress
                .active_tools
                .retain(|(existing_id, _)| existing_id != &id);
        }
        if progress.active_tools.is_empty() {
            self.progress = None;
        } else if let Some((_, label)) = progress.active_tools.last() {
            progress.label = label.clone();
        }
    }
}

fn model_choice_label(choice: &ModelChoice) -> &str {
    choice
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or(choice.value.as_str())
}

fn sort_model_choices(models: &mut [ModelChoice]) {
    models.sort_by(|a, b| {
        model_choice_label(a)
            .to_ascii_lowercase()
            .cmp(&model_choice_label(b).to_ascii_lowercase())
            .then_with(|| a.value.cmp(&b.value))
    });
}

fn pad_picker_row(text: &str, width: usize) -> String {
    let mut out = text.chars().take(width).collect::<String>();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

/// Resolve `/model <query>` against ACP choices (exact value/name, then unique prefix).
fn resolve_model_choice<'a>(models: &'a [ModelChoice], query: &str) -> Option<&'a ModelChoice> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    if let Some(choice) = models.iter().find(|m| m.value == q) {
        return Some(choice);
    }
    if let Some(choice) = models.iter().find(|m| m.name.as_deref() == Some(q)) {
        return Some(choice);
    }
    let q_lower = q.to_ascii_lowercase();
    if let Some(choice) = models.iter().find(|m| {
        m.value.to_ascii_lowercase() == q_lower
            || m.name
                .as_ref()
                .is_some_and(|n| n.to_ascii_lowercase() == q_lower)
    }) {
        return Some(choice);
    }
    let prefix_hits: Vec<_> = models
        .iter()
        .filter(|m| {
            m.value.to_ascii_lowercase().starts_with(&q_lower)
                || m.name
                    .as_ref()
                    .is_some_and(|n| n.to_ascii_lowercase().starts_with(&q_lower))
        })
        .collect();
    if prefix_hits.len() == 1 {
        return Some(prefix_hits[0]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_up_down_scroll_by_viewport_not_one_row() {
        let mut app = App::new("a".into(), "a".into(), true, false);
        app.transcript_viewport_rows = 12;
        assert_eq!(app.page_scroll_step(), 11);

        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll_from_bottom, 11);
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll_from_bottom, 22);
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.scroll_from_bottom, 11);

        // Before first draw, fall back to a usable default (not 1).
        app.transcript_viewport_rows = 0;
        assert_eq!(app.page_scroll_step(), DEFAULT_PAGE_SCROLL);
    }

    #[test]
    fn submit_clears_prompt_and_appends_user() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.prompt.set_text(" hello ");
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            InputEvent::Submit(text) => assert_eq!(text, "hello"),
            other => panic!("expected submit, got {other:?}"),
        }
        assert!(app.prompt.is_empty());
        assert_eq!(app.transcript.last().unwrap().kind, TranscriptKind::User);
    }

    #[test]
    fn shift_enter_inserts_newline_without_submit() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.prompt.set_text("line1");
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)) {
            InputEvent::None => {}
            other => panic!("expected none, got {other:?}"),
        }
        assert!(app.prompt.text().contains('\n'));
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn host_transcript_coalesces_same_kind() {
        let mut app = App::new("a".into(), "a".into(), true, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "hel".into(),
        });
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "lo".into(),
        });
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.transcript[0].text, "hello");
        assert!(app.transcript[0].md_lines.is_none());
    }

    #[test]
    fn agent_markdown_cache_renders_on_draw_refresh() {
        let mut app = App::new("a".into(), "a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "# Title\n\n**bold**".into(),
        });
        app.refresh_markdown_caches();
        let lines = app.wrapped_lines();
        let joined: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Title") || joined.contains("bold") || joined.contains("agent:"));
        assert!(app.transcript[0].md_lines.is_some());
    }

    #[test]
    fn appearance_switches_theme_and_invalidates_markdown_cache() {
        let mut app = App::new("a".into(), "a".into(), false, false);
        assert!(!app.light);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "```rust\nfn main() {}\n```".into(),
        });
        app.refresh_markdown_caches();
        assert!(app.transcript[0].md_lines.is_some());

        app.apply_host(HostToTui::Appearance { light: true });
        assert!(app.light);
        assert_eq!(app.prompt.selection_style.fg, Some(Color::Black));
        assert!(app.transcript[0].md_lines.is_none());

        app.apply_host(HostToTui::Appearance { light: true });
        assert!(app.light);

        app.apply_host(HostToTui::Appearance { light: false });
        assert!(!app.light);
    }

    #[test]
    fn progress_activates_and_clears() {
        let mut app = App::new("a".into(), "a".into(), false, false);
        app.apply_host(HostToTui::Progress {
            id: "t".into(),
            label: "edit".into(),
            active: true,
        });
        assert!(app.progress.is_some());
        app.apply_host(HostToTui::Progress {
            id: "t".into(),
            label: "edit".into(),
            active: false,
        });
        assert!(app.progress.is_none());
    }

    #[test]
    fn session_status_updates_header_model() {
        let mut app = App::new("coder".into(), "coder".into(), false, false);
        app.apply_host(HostToTui::SessionStatus {
            model: Some("composer-2.5".into()),
            models: Some(vec![
                ModelChoice {
                    value: "composer-2.5".into(),
                    name: Some("Composer".into()),
                },
                ModelChoice {
                    value: "opus".into(),
                    name: Some("Opus".into()),
                },
            ]),
            provider_error: None,
            clear_provider_error: true,
        });
        assert_eq!(app.model.as_deref(), Some("composer-2.5"));
        assert_eq!(app.models.len(), 2);
    }

    #[test]
    fn slash_model_opens_picker() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.apply_host(HostToTui::SessionStatus {
            model: Some("a".into()),
            models: Some(vec![
                ModelChoice {
                    value: "a".into(),
                    name: Some("A".into()),
                },
                ModelChoice {
                    value: "b".into(),
                    name: Some("B".into()),
                },
            ]),
            provider_error: None,
            clear_provider_error: false,
        });
        app.prompt.set_text("/model");
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            InputEvent::None
        ));
        assert_eq!(app.model_picker, Some(0));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            InputEvent::None
        ));
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            InputEvent::SetModel(value) => assert_eq!(value, "b"),
            other => panic!("expected SetModel, got {other:?}"),
        }
        assert_eq!(app.model.as_deref(), Some("a"));
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn slash_model_query_sets_directly() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.apply_host(HostToTui::SessionStatus {
            model: Some("a".into()),
            models: Some(vec![
                ModelChoice {
                    value: "composer-2.5".into(),
                    name: Some("Composer".into()),
                },
                ModelChoice {
                    value: "opus".into(),
                    name: Some("Opus".into()),
                },
            ]),
            provider_error: None,
            clear_provider_error: false,
        });
        app.prompt.set_text("/model opus");
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            InputEvent::SetModel(value) => assert_eq!(value, "opus"),
            other => panic!("expected SetModel, got {other:?}"),
        }
        assert_eq!(app.model.as_deref(), Some("a"));
        assert!(app.model_picker.is_none());
    }

    #[test]
    fn resolve_model_choice_prefers_unique_prefix() {
        let models = vec![
            ModelChoice {
                value: "composer-2.5".into(),
                name: Some("Composer".into()),
            },
            ModelChoice {
                value: "opus".into(),
                name: Some("Opus".into()),
            },
        ];
        assert_eq!(
            resolve_model_choice(&models, "comp").map(|m| m.value.as_str()),
            Some("composer-2.5")
        );
        assert_eq!(resolve_model_choice(&models, "nope"), None);
    }

    #[test]
    fn model_choices_sorted_by_label() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.apply_host(HostToTui::SessionStatus {
            model: None,
            models: Some(vec![
                ModelChoice {
                    value: "z-last".into(),
                    name: Some("Zebra".into()),
                },
                ModelChoice {
                    value: "a-first".into(),
                    name: Some("Alpha".into()),
                },
                ModelChoice {
                    value: "m-mid".into(),
                    name: None,
                },
            ]),
            provider_error: None,
            clear_provider_error: false,
        });
        let labels: Vec<_> = app.models.iter().map(model_choice_label).collect();
        assert_eq!(labels, ["Alpha", "m-mid", "Zebra"]);
    }

    #[test]
    fn session_status_without_models_leaves_picker_list_unchanged() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.apply_host(HostToTui::SessionStatus {
            model: Some("a".into()),
            models: Some(vec![
                ModelChoice {
                    value: "a".into(),
                    name: Some("A".into()),
                },
                ModelChoice {
                    value: "b".into(),
                    name: Some("B".into()),
                },
            ]),
            provider_error: None,
            clear_provider_error: false,
        });
        let before = app.models.clone();
        app.apply_host(HostToTui::SessionStatus {
            model: Some("b".into()),
            models: None,
            provider_error: None,
            clear_provider_error: false,
        });
        assert_eq!(app.models, before);
        assert_eq!(app.model.as_deref(), Some("b"));
    }

    #[test]
    fn model_change_waits_for_session_status() {
        let mut app = App::new("coder".into(), "coder".into(), true, false);
        app.apply_host(HostToTui::SessionStatus {
            model: Some("a".into()),
            models: Some(vec![
                ModelChoice {
                    value: "a".into(),
                    name: Some("A".into()),
                },
                ModelChoice {
                    value: "b".into(),
                    name: Some("B".into()),
                },
            ]),
            provider_error: None,
            clear_provider_error: false,
        });
        app.prompt.set_text("/model b");
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            InputEvent::SetModel(value) => assert_eq!(value, "b"),
            other => panic!("expected SetModel, got {other:?}"),
        }
        assert_eq!(app.model.as_deref(), Some("a"));
        app.apply_host(HostToTui::SessionStatus {
            model: Some("b".into()),
            models: None,
            provider_error: None,
            clear_provider_error: false,
        });
        assert_eq!(app.model.as_deref(), Some("b"));
    }

    #[test]
    fn agent_transcript_clears_sticky_turn_progress() {
        let mut app = App::new("a".into(), "a".into(), true, false);
        app.apply_host(HostToTui::Progress {
            id: "__turn".into(),
            label: "Thinking".into(),
            active: true,
        });
        assert!(app.progress.is_some());
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "hello".into(),
        });
        assert!(app.progress.is_none());
    }

    /// Compile-time proof that grok leaf UI crates share via's ratatui 0.29 types.
    #[test]
    fn grok_leaf_crates_type_unify_with_via_ratatui() {
        use ratatui::widgets::Paragraph;

        let mut ta = TextArea::new();
        ta.insert_str("prompt");
        assert_eq!(ta.text(), "prompt");

        let syntect = Syntect::new(TM_THEME_DARK);
        let (lines, _) = render_markdown_ratatui(
            "# title\n\n```rust\nfn main() {}\n```",
            MarkdownStyle::default(),
            true,
            Some(&syntect),
        );
        assert!(!lines.is_empty());
        let _ = Paragraph::new(lines);

        let _ =
            std::any::type_name::<xai_ratatui_inline::Terminal<ratatui::backend::TestBackend>>();
    }
}
