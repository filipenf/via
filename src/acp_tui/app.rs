//! Ratatui application state for the ACP TUI.

use std::collections::HashSet;

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
/// Visible body rows for an expanded thought block; longer blocks scroll inside.
const THOUGHT_BODY_ROWS: usize = 5;

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
    /// A finished thought block must not absorb thought chunks from a later phase.
    sealed: bool,
    /// Row offset inside a thought block's five-row body window.
    thought_scroll: usize,
    /// Show the newest body rows (streaming block, or a closed block's tail).
    thought_follow_tail: bool,
    /// Wrapped body rows measured by the last layout pass (thought blocks only).
    thought_body_rows: usize,
}

/// One transcript entry's position in content-row space.
struct EntryLayout {
    entry: usize,
    /// First rendered row of the entry, counted from the top of the transcript.
    top: usize,
    /// Total rendered rows, including the trailing spacer.
    height: usize,
}

struct TranscriptLayout {
    entries: Vec<EntryLayout>,
    total_rows: usize,
}

struct ProgressState {
    active_tools: Vec<(String, String)>,
    label: String,
    frame: u8,
}

pub(super) struct App {
    agent_id: String,
    /// The host draws a tab strip above this pane, so the header need not name the agent.
    tab_strip_visible: bool,
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
    /// Global collapse state for every thought block.
    thoughts_collapsed: bool,
    /// Tool ids already reported this turn; a repeated update must not seal again.
    seen_tool_ids: HashSet<String>,
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
    pub(super) fn new(agent_id: String, show_input: bool, light: bool) -> Self {
        let mut app = Self {
            agent_id,
            tab_strip_visible: false,
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
            thoughts_collapsed: false,
            seen_tool_ids: HashSet::new(),
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
            "Demo — Enter send · Shift+Enter newline · /model · PgUp/PgDn scroll · Ctrl+T thoughts · Ctrl+↑/↓ thought scroll · q quit.".into(),
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
            (1..=12)
                .map(|i| format!("- Reasoning step {i}: checking the next step…"))
                .collect::<Vec<_>>()
                .join("\n"),
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
                    .filter(|entry| entry.kind == kind && !entry.sealed)
                {
                    entry.text.push_str(&text);
                    entry.md_lines = None;
                } else {
                    self.push(kind, text);
                }
            }
            HostToTui::Progress { id, label, active } => {
                if id == "__turn" {
                    // A finished turn ends the current reasoning phase. Tool ids
                    // are scoped to a turn, so the next turn starts clean.
                    if !active {
                        self.seal_active_thought();
                    }
                    self.seen_tool_ids.clear();
                } else if self.seen_tool_ids.insert(id.clone()) {
                    // First report for this tool starts a new reasoning phase,
                    // even when the tool arrives already completed. Later
                    // `tool_call_update` events for the same id must not split
                    // the thought that followed the first report.
                    self.seal_active_thought();
                }
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
            HostToTui::TabStripVisible { visible } => {
                self.tab_strip_visible = visible;
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
            // Thought blocks: global collapse, plus scrolling the newest block body.
            // Ctrl+T (not plain `t`) so a prompt can still start with "t".
            KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.thoughts_collapsed = !self.thoughts_collapsed;
                InputEvent::None
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_thought(-1);
                InputEvent::None
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_thought(1);
                InputEvent::None
            }
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(entry) = self.newest_thought_mut() {
                    entry.thought_scroll = 0;
                    entry.thought_follow_tail = false;
                }
                InputEvent::None
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(entry) = self.newest_thought_mut() {
                    entry.thought_follow_tail = true;
                }
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
                self.seen_tool_ids.clear();
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

        let layout = self.layout_transcript(chunks[1].width);
        // Home can overshoot a short transcript; store the clamped offset so a
        // following Down/PageDown moves the view instead of decrementing a
        // saturated sentinel.
        let max_scroll = layout
            .total_rows
            .saturating_sub(usize::from(chunks[1].height.max(1)));
        self.scroll_from_bottom = self.scroll_from_bottom.min(to_u16(max_scroll));
        frame.render_widget(self.header_widget(&layout, chunks[1].height), chunks[0]);
        self.draw_transcript(frame, chunks[1], &layout);
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

    fn header_widget(&self, layout: &TranscriptLayout, transcript_rows: u16) -> Paragraph<'static> {
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

        let max_scroll = layout
            .total_rows
            .saturating_sub(usize::from(transcript_rows.max(1)));
        let from_bottom = usize::from(self.scroll_from_bottom).min(max_scroll);
        let scroll_hint = if from_bottom == 0 {
            String::new()
        } else {
            format!("  ↑{from_bottom}")
        };

        // The model and what the agent is doing. The agent id is shown only when the
        // host has no tab strip above this pane to name it.
        let identity = if self.tab_strip_visible {
            Span::raw("")
        } else {
            Span::styled(format!("{}  ·  ", self.agent_id), muted)
        };
        let mut lines = vec![Line::from(vec![
            identity,
            Span::styled(model.to_string(), title_style),
            Span::styled(progress, Style::default().fg(Color::Magenta)),
            Span::styled(scroll_hint, muted),
        ])];
        if let Some(error) = &self.provider_error {
            lines.push(Line::from(Span::styled(format!("! {error}"), warn)));
        }

        // Bottom border is the only chrome between header and transcript (no transcript box).
        Paragraph::new(lines).block(Block::default().borders(Borders::BOTTOM))
    }

    /// Measure every entry once per draw.
    ///
    /// `Paragraph` wraps at render time, so heights must be measured with
    /// [`Paragraph::line_count`]; otherwise transcript scrolling is wrong for
    /// long lines and thought blocks cannot be capped at a fixed row count.
    fn layout_transcript(&mut self, width: u16) -> TranscriptLayout {
        let muted = self.muted_fg();
        let mut entries = Vec::with_capacity(self.transcript.len());
        let mut top = 0usize;

        for (idx, entry) in self.transcript.iter_mut().enumerate() {
            let height = match entry.kind {
                TranscriptKind::Thought => {
                    entry.thought_body_rows = thought_body_height(entry, width);
                    let visible = if self.thoughts_collapsed {
                        0
                    } else {
                        entry.thought_body_rows.min(THOUGHT_BODY_ROWS)
                    };
                    1 + visible + 1
                }
                _ => {
                    let lines = entry_text_lines(entry, muted);
                    wrapped_row_count(lines, width)
                }
            };
            entries.push(EntryLayout {
                entry: idx,
                top,
                height,
            });
            top = top.saturating_add(height);
        }

        TranscriptLayout {
            entries,
            total_rows: top,
        }
    }

    fn draw_transcript(&self, frame: &mut Frame<'_>, area: Rect, layout: &TranscriptLayout) {
        if self.transcript.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "No transcript yet.",
                    Style::default().fg(self.muted_fg()),
                ))),
                area,
            );
            return;
        }

        let max_scroll = layout
            .total_rows
            .saturating_sub(usize::from(area.height.max(1)));
        let top = max_scroll.saturating_sub(usize::from(self.scroll_from_bottom));
        let view_bottom = top.saturating_add(usize::from(area.height));

        for geom in &layout.entries {
            let entry_top = geom.top;
            let entry_bottom = entry_top.saturating_add(geom.height);
            if entry_bottom <= top {
                continue;
            }
            if entry_top >= view_bottom {
                break;
            }

            let entry = &self.transcript[geom.entry];
            if entry.kind == TranscriptKind::Thought {
                self.draw_thought(frame, area, top, view_bottom, geom);
                continue;
            }

            let visible_start = entry_top.max(top);
            let visible_end = entry_bottom.min(view_bottom);
            let rect = Rect::new(
                area.x,
                area.y + to_u16(visible_start - top),
                area.width,
                to_u16(visible_end - visible_start),
            );
            let lines = entry_text_lines(entry, self.muted_fg());
            let (start, row) = row_slice(&lines, area.width, visible_start - entry_top);
            if start >= lines.len() {
                continue;
            }
            frame.render_widget(
                Paragraph::new(lines[start..].to_vec())
                    .wrap(Wrap { trim: false })
                    .scroll((safe_scroll(row, rect.height), 0)),
                rect,
            );
        }
    }

    fn draw_thought(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        top: usize,
        view_bottom: usize,
        geom: &EntryLayout,
    ) {
        let entry = &self.transcript[geom.entry];
        let body_rows = entry.thought_body_rows;
        let body_scroll = thought_offset(entry);

        if geom.top >= top && geom.top < view_bottom {
            let rect = Rect::new(area.x, area.y + to_u16(geom.top - top), area.width, 1);
            frame.render_widget(
                Paragraph::new(self.thought_header_line(entry, body_scroll)),
                rect,
            );
        }

        if self.thoughts_collapsed || body_rows == 0 {
            return;
        }

        let body_top = geom.top.saturating_add(1);
        let body_height = body_rows.min(THOUGHT_BODY_ROWS);
        let body_bottom = body_top.saturating_add(body_height);
        let visible_start = body_top.max(top);
        let visible_end = body_bottom.min(view_bottom);
        if visible_end <= visible_start {
            return;
        }

        let rect = Rect::new(
            area.x,
            area.y + to_u16(visible_start - top),
            area.width,
            to_u16(visible_end - visible_start),
        );
        let skip = (visible_start - body_top).saturating_add(body_scroll);
        let lines = thought_body_lines(entry);
        let (start, row) = row_slice(&lines, area.width.saturating_sub(1), skip);
        if start >= lines.len() {
            return;
        }
        frame.render_widget(
            Paragraph::new(lines[start..].to_vec())
                .style(Style::default().fg(self.muted_fg()))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(self.muted_fg())),
                )
                .scroll((safe_scroll(row, rect.height), 0)),
            rect,
        );
    }

    fn thought_header_line(&self, entry: &TranscriptEntry, body_scroll: usize) -> Line<'static> {
        let marker = if self.thoughts_collapsed {
            "▸"
        } else {
            "▾"
        };
        let body_rows = entry.thought_body_rows;
        let mut text = format!("{marker} thought");
        if body_rows > 0 {
            text.push_str(&format!(" · {body_rows} lines"));
        }
        if !self.thoughts_collapsed && body_rows > THOUGHT_BODY_ROWS {
            let start = body_scroll.saturating_add(1);
            let end = body_scroll.saturating_add(THOUGHT_BODY_ROWS).min(body_rows);
            text.push_str(&format!(" · {start}–{end}"));
        }
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
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

    fn push(&mut self, kind: TranscriptKind, text: String) {
        if kind != TranscriptKind::Thought {
            self.seal_active_thought();
        }
        self.transcript.push(TranscriptEntry {
            kind,
            text,
            md_lines: None,
            sealed: false,
            thought_scroll: 0,
            // A new thought block starts expanded and follows the newest rows.
            thought_follow_tail: kind == TranscriptKind::Thought,
            thought_body_rows: 0,
        });
        if self.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            let drain = self.transcript.len() - MAX_TRANSCRIPT_ENTRIES;
            self.transcript.drain(..drain);
        }
    }

    /// Mark the current thought block finished so later chunks open a new block.
    ///
    /// Sealing is idempotent: repeated tool updates must not move a block the
    /// user already scrolled.
    fn seal_active_thought(&mut self) {
        let Some(entry) = self.transcript.last_mut() else {
            return;
        };
        if entry.kind != TranscriptKind::Thought || entry.sealed {
            return;
        }
        entry.sealed = true;
        // Keep the most recent reasoning in view once the block closes.
        entry.thought_follow_tail = true;
    }

    fn newest_thought_mut(&mut self) -> Option<&mut TranscriptEntry> {
        self.transcript
            .iter_mut()
            .rev()
            .find(|entry| entry.kind == TranscriptKind::Thought)
    }

    /// Move the newest thought block's body window by `delta` rows.
    fn scroll_thought(&mut self, delta: i16) {
        let Some(entry) = self.newest_thought_mut() else {
            return;
        };
        let max = thought_max_scroll(entry);
        let current = thought_offset(entry);
        let next = if delta < 0 {
            current.saturating_sub(usize::from(delta.unsigned_abs()))
        } else {
            current.saturating_add(delta as usize).min(max)
        };
        entry.thought_scroll = next;
        entry.thought_follow_tail = next >= max;
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

/// Rendered lines for a non-thought entry (agent markdown, user/tool/system text).
fn entry_text_lines(entry: &TranscriptEntry, muted: Color) -> Vec<Line<'static>> {
    match entry.kind {
        TranscriptKind::Agent => {
            let label_style = Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD);
            let mut lines = vec![Line::from(Span::styled("agent:", label_style))];
            if let Some(md) = &entry.md_lines {
                lines.extend(md.iter().cloned());
            } else {
                // Cache not warm yet (shouldn't happen after refresh); plain fallback.
                for part in entry.text.split('\n') {
                    lines.push(Line::from(Span::styled(part.to_string(), label_style)));
                }
            }
            lines.push(Line::from(""));
            lines
        }
        TranscriptKind::Thought => Vec::new(),
        kind => {
            let (prefix, style) = match kind {
                TranscriptKind::User => (
                    "you",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                TranscriptKind::Tool => ("tool", Style::default().fg(Color::Magenta)),
                TranscriptKind::System => ("system", Style::default().fg(muted)),
                TranscriptKind::Agent | TranscriptKind::Thought => unreachable!(),
            };
            let mut lines = Vec::new();
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
            lines
        }
    }
}

fn thought_body_lines(entry: &TranscriptEntry) -> Vec<Line<'static>> {
    if let Some(md) = &entry.md_lines {
        return md.clone();
    }
    entry
        .text
        .split('\n')
        .map(|part| Line::from(part.to_string()))
        .collect()
}

/// Wrapped rows for a thought body. The body renders inside a left-border block,
/// so it has one fewer column than the transcript.
fn thought_body_height(entry: &TranscriptEntry, width: u16) -> usize {
    let lines = thought_body_lines(entry);
    if lines.is_empty() {
        return 0;
    }
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width.saturating_sub(1))
}

fn wrapped_row_count(lines: Vec<Line<'static>>, width: u16) -> usize {
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn thought_max_scroll(entry: &TranscriptEntry) -> usize {
    entry.thought_body_rows.saturating_sub(THOUGHT_BODY_ROWS)
}

fn thought_offset(entry: &TranscriptEntry) -> usize {
    let max = thought_max_scroll(entry);
    if entry.thought_follow_tail {
        max
    } else {
        entry.thought_scroll.min(max)
    }
}

/// Locate the first line and row offset that cover `skip` wrapped rows.
///
/// `Paragraph::scroll` is `u16`, so entries taller than that cannot be
/// rendered by scrolling alone. Rendering starts at the matching line instead.
fn row_slice(lines: &[Line<'static>], width: u16, skip: usize) -> (usize, usize) {
    if skip == 0 || width == 0 {
        return (0, 0);
    }
    let mut remaining = skip;
    for (idx, line) in lines.iter().enumerate() {
        let height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1);
        if remaining < height {
            return (idx, remaining);
        }
        remaining -= height;
    }
    (lines.len(), 0)
}

/// Keep `Paragraph`'s internal `scroll.y + area.height` inside `u16`.
fn safe_scroll(row: usize, height: u16) -> u16 {
    to_u16(row).min(u16::MAX.saturating_sub(height))
}

fn to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
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
        let mut app = App::new("a".into(), true, false);
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
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("a".into(), true, false);
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
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "# Title\n\n**bold**".into(),
        });
        app.refresh_markdown_caches();
        let lines = entry_text_lines(&app.transcript[0], app.muted_fg());
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

    fn transcript_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn long_thought(lines: usize) -> String {
        // A markdown list keeps one rendered row per item; consecutive plain
        // lines would collapse into a single wrapped paragraph.
        (1..=lines)
            .map(|i| format!("- think {i:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn thought_body_is_capped_at_five_rows_and_follows_the_tail() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: long_thought(20),
        });

        let layout = app.layout_transcript(60);
        let geom = layout.entries.last().expect("thought layout");
        assert_eq!(app.transcript[0].thought_body_rows, 20);
        assert_eq!(geom.height, 1 + 5 + 1);

        let rows = transcript_rows(&mut app, 60, 20);
        let body: Vec<&String> = rows.iter().filter(|row| row.contains("think ")).collect();
        assert_eq!(
            body.len(),
            5,
            "body must render exactly five rows: {rows:?}"
        );
        assert!(
            body.iter().any(|row| row.contains("think 20")),
            "streaming block should follow the tail: {body:?}"
        );
        assert!(
            !body.iter().any(|row| row.contains("think 15")),
            "rows above the five-row window must stay hidden: {body:?}"
        );
    }

    #[test]
    fn thought_toggle_collapses_all_blocks() {
        let mut app = App::new("a".into(), true, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "one\ntwo".into(),
        });
        let layout = app.layout_transcript(60);
        assert_eq!(layout.entries.last().unwrap().height, 1 + 2 + 1);

        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            InputEvent::None
        ));
        assert!(app.thoughts_collapsed);
        let layout = app.layout_transcript(60);
        assert_eq!(layout.entries.last().unwrap().height, 2);
    }

    #[test]
    fn tool_progress_seals_thought_blocks() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "first".into(),
        });
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: true,
        });
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "second".into(),
        });

        assert_eq!(app.transcript.len(), 2);
        assert!(app.transcript[0].sealed);
        assert!(!app.transcript[1].sealed);
    }

    #[test]
    fn repeated_tool_progress_does_not_split_the_thought() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: long_thought(20),
        });
        // tool_call starts the phase.
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: true,
        });
        app.layout_transcript(60);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        let scrolled = app.transcript[0].thought_scroll;

        // Every later tool_call_update for the same id must not seal again.
        for active in [true, true, false, false] {
            app.apply_host(HostToTui::Progress {
                id: "tool-1".into(),
                label: "edit".into(),
                active,
            });
        }
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.transcript[0].thought_scroll, scrolled);

        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "second".into(),
        });
        assert_eq!(app.transcript.len(), 2);
        assert!(app.transcript[0].sealed);
        assert!(!app.transcript[1].sealed);
    }

    #[test]
    fn tool_ids_reset_between_turns() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "first".into(),
        });
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: true,
        });
        app.apply_host(HostToTui::Progress {
            id: "__turn".into(),
            label: String::new(),
            active: false,
        });
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "second".into(),
        });
        // The same id in a new turn is a new tool call and seals the block.
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: true,
        });
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "third".into(),
        });

        assert_eq!(app.transcript.len(), 3);
        assert!(app.transcript[1].sealed);
        assert!(!app.transcript[2].sealed);
    }

    #[test]
    fn completed_tool_event_seals_the_thought() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "first".into(),
        });
        // A tool that arrives already completed still ends the phase.
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: false,
        });
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "second".into(),
        });

        assert_eq!(app.transcript.len(), 2);
        assert!(app.transcript[0].sealed);
    }

    #[test]
    fn agent_text_does_not_reset_a_scrolled_thought() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: long_thought(20),
        });
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: true,
        });
        app.layout_transcript(60);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        let scrolled = app.transcript[0].thought_scroll;

        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Agent,
            text: "answer".into(),
        });

        assert!(app.transcript[0].sealed);
        assert_eq!(app.transcript[0].thought_scroll, scrolled);
        assert!(!app.transcript[0].thought_follow_tail);
    }

    #[test]
    fn thought_scroll_keys_move_the_newest_block() {
        let mut app = App::new("a".into(), true, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: long_thought(20),
        });
        app.layout_transcript(60);
        assert_eq!(app.transcript[0].thought_body_rows, 20);

        // The live window is at the tail (rows 16..20), so one row up is 14.
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.transcript[0].thought_scroll, 14);
        assert!(!app.transcript[0].thought_follow_tail);

        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL));
        assert_eq!(app.transcript[0].thought_scroll, 0);
        assert!(!app.transcript[0].thought_follow_tail);

        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert!(app.transcript[0].thought_follow_tail);
    }

    #[test]
    fn sealing_a_thought_keeps_its_tail_visible() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: long_thought(20),
        });
        app.apply_host(HostToTui::Progress {
            id: "__turn".into(),
            label: String::new(),
            active: false,
        });
        assert!(app.transcript[0].sealed);
        assert!(app.transcript[0].thought_follow_tail);

        let rows = transcript_rows(&mut app, 60, 20);
        assert!(rows.iter().any(|row| row.contains("think 20")));
        assert!(!rows.iter().any(|row| row.contains("think 01")));
    }

    #[test]
    fn older_thought_blocks_keep_their_tail_window() {
        let mut app = App::new("a".into(), false, false);
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: long_thought(20),
        });
        app.apply_host(HostToTui::Progress {
            id: "tool-1".into(),
            label: "edit".into(),
            active: true,
        });
        app.apply_host(HostToTui::Transcript {
            kind: TranscriptKind::Thought,
            text: "second".into(),
        });

        let rows = transcript_rows(&mut app, 60, 40);
        assert!(
            rows.iter().any(|row| row.contains("think 20")),
            "closed block should keep its newest rows reachable: {rows:?}"
        );
        assert!(!rows.iter().any(|row| row.contains("think 01")));
    }

    #[test]
    fn home_clamps_transcript_scroll_before_down() {
        let mut app = App::new("a".into(), false, false);
        for i in 0..40 {
            app.push(TranscriptKind::System, format!("line {i}"));
        }
        transcript_rows(&mut app, 40, 12);
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        transcript_rows(&mut app, 40, 12);

        let layout = app.layout_transcript(40);
        let max = layout
            .total_rows
            .saturating_sub(usize::from(app.transcript_viewport_rows));
        assert_eq!(usize::from(app.scroll_from_bottom), max);

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(usize::from(app.scroll_from_bottom), max - 1);
    }

    #[test]
    fn row_slice_matches_paragraph_scroll() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let lines = vec![
            Line::from("one two three four five six seven eight nine ten"),
            Line::from(""),
            Line::from("short"),
            Line::from("a b c d e f g h i j k l m n o p"),
        ];
        let width = 12;
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);

        for skip in 0..total {
            let (start, row) = row_slice(&lines, width, skip);
            let mut expected = Terminal::new(TestBackend::new(width, 4)).expect("terminal");
            expected
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(lines.clone())
                            .wrap(Wrap { trim: false })
                            .scroll((to_u16(skip), 0)),
                        frame.area(),
                    );
                })
                .expect("draw");

            let mut actual = Terminal::new(TestBackend::new(width, 4)).expect("terminal");
            actual
                .draw(|frame| {
                    frame.render_widget(
                        Paragraph::new(lines[start..].to_vec())
                            .wrap(Wrap { trim: false })
                            .scroll((to_u16(row), 0)),
                        frame.area(),
                    );
                })
                .expect("draw");

            assert_eq!(
                expected.backend().buffer(),
                actual.backend().buffer(),
                "skip {skip} resolved to line {start}, row {row}"
            );
        }
    }

    #[test]
    fn huge_entry_does_not_hide_later_entries() {
        let mut app = App::new("a".into(), false, false);
        let huge = (0..70_000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push(TranscriptKind::Tool, huge);
        app.push(TranscriptKind::System, "after".into());

        let layout = app.layout_transcript(80);
        assert!(layout.total_rows > usize::from(u16::MAX));
        assert!(layout.entries.last().unwrap().top > usize::from(u16::MAX));

        let rows = transcript_rows(&mut app, 80, 20);
        assert!(
            rows.iter().any(|row| row.contains("line 69999")),
            "the huge entry's tail must render: {rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("after")));
    }

    #[test]
    fn typing_t_in_the_prompt_does_not_toggle_thoughts() {
        let mut app = App::new("a".into(), true, false);
        app.prompt.set_text("a");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(app.prompt.text().len(), 2);
        assert!(app.prompt.text().contains('t'));
        assert!(!app.thoughts_collapsed);

        // Ctrl+T is the toggle, including while composing.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.thoughts_collapsed);
    }

    #[test]
    fn appearance_switches_theme_and_invalidates_markdown_cache() {
        let mut app = App::new("a".into(), false, false);
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
        let mut app = App::new("a".into(), false, false);
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
        let mut app = App::new("coder".into(), false, false);
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

    fn header_row(app: &mut App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(60, 6)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.width)
            .map(|col| buffer[(col, 0)].symbol())
            .collect()
    }

    fn header_app() -> App {
        let mut app = App::new("coder".into(), false, false);
        app.model = Some("composer-2.5".into());
        app.models = vec![ModelChoice {
            value: "composer-2.5".into(),
            name: None,
        }];
        app.update_progress("t1".into(), "edit".into(), true);
        app
    }

    #[test]
    fn header_shows_only_model_and_status_when_a_tab_strip_names_the_agent() {
        let mut app = header_app();
        app.apply_host(HostToTui::TabStripVisible { visible: true });
        let row = header_row(&mut app);

        assert!(row.starts_with("composer-2.5"), "got {row:?}");
        assert!(row.contains("edit"), "status missing: {row:?}");
        for noise in ["coder", "model:", "/model"] {
            assert!(!row.contains(noise), "{noise:?} should be gone: {row:?}");
        }
    }

    #[test]
    fn header_names_the_agent_when_no_tab_strip_is_visible() {
        let mut app = header_app();
        let row = header_row(&mut app);
        assert!(row.starts_with("coder  ·  composer-2.5"), "got {row:?}");
        assert!(row.contains("edit"), "status missing: {row:?}");

        // The strip appears, then goes away again.
        app.apply_host(HostToTui::TabStripVisible { visible: true });
        assert!(!header_row(&mut app).contains("coder"));
        app.apply_host(HostToTui::TabStripVisible { visible: false });
        assert!(header_row(&mut app).starts_with("coder  ·  "));
    }

    #[test]
    fn slash_model_opens_picker() {
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("coder".into(), true, false);
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
        let mut app = App::new("a".into(), true, false);
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
