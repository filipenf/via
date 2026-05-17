use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, StatefulWidget, Widget, Wrap};
use throbber_widgets_tui::{QUADRANT_BLOCK_CRACK, Throbber, ThrobberState};

use super::config::{TerminalMetrics, TerminalTheme};
use super::font::FontRenderer;
use super::input::{Key, Modifiers};
use super::layout::PaneRect;
use super::render::{DamageRect, draw_pane_border, draw_ratatui_buffer};

const TURN_PROGRESS_ID: &str = "__turn";

pub(super) struct AcpPane {
    metrics: TerminalMetrics,
    theme: TerminalTheme,
    size: RatatuiPaneSize,
    transcript: Vec<TranscriptEntry>,
    progress: Option<ProgressState>,
    prompt: String,
    last_submitted: Option<String>,
    dirty: bool,
}

struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
}

struct ProgressState {
    active_tools: Vec<ActiveTool>,
    label: String,
    throbber: ThrobberState,
}

struct ActiveTool {
    id: String,
    label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptKind {
    User,
    Agent,
    Thought,
    Tool,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RatatuiPaneSize {
    cols: u16,
    rows: u16,
}

impl AcpPane {
    pub(super) fn new(
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
        theme: &TerminalTheme,
    ) -> Self {
        Self {
            metrics,
            theme: theme.clone(),
            size: ratatui_size_for_window(width, height, metrics),
            transcript: Vec::new(),
            progress: None,
            prompt: String::new(),
            last_submitted: None,
            dirty: true,
        }
    }

    pub(super) fn handle_text_input(&mut self, text: &str, modifiers: Modifiers) -> bool {
        if modifiers.ctrl || modifiers.super_key || text.is_empty() {
            return false;
        }

        self.prompt.push_str(text);
        self.dirty = true;
        true
    }

    pub(super) fn handle_key(&mut self, key: Key, modifiers: Modifiers) -> Option<String> {
        if modifiers.ctrl || modifiers.super_key {
            return None;
        }

        match key {
            Key::Enter | Key::NumPadEnter => {
                let text = self.prompt.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.prompt.clear();
                self.last_submitted = Some(text.clone());
                self.push_transcript(TranscriptKind::User, text.clone());
                self.update_progress(TURN_PROGRESS_ID.to_string(), "Thinking".to_string(), true);
                self.dirty = true;
                Some(text)
            }
            Key::Backspace => {
                if self.prompt.pop().is_some() {
                    self.dirty = true;
                }
                None
            }
            Key::Escape => {
                if !self.prompt.is_empty() {
                    self.prompt.clear();
                    self.dirty = true;
                }
                None
            }
            _ => None,
        }
    }

    pub(super) fn append_transcript_chunk(&mut self, kind: &str, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }

        let kind = match kind {
            "user_message_chunk" => TranscriptKind::User,
            "agent_message_chunk" => TranscriptKind::Agent,
            "agent_thought_chunk" => TranscriptKind::Thought,
            "tool_call" | "tool_call_update" => TranscriptKind::Tool,
            _ => TranscriptKind::System,
        };

        if let Some(entry) = self
            .transcript
            .last_mut()
            .filter(|entry| entry.kind == kind)
        {
            entry.text.push_str(text);
        } else {
            self.push_transcript(kind, text.to_string());
        }

        self.dirty = true;
        true
    }

    pub(super) fn update_progress(&mut self, id: String, label: String, active: bool) -> bool {
        let progress = self.progress.get_or_insert_with(|| ProgressState {
            active_tools: Vec::new(),
            label: String::new(),
            throbber: ThrobberState::default(),
        });

        if active {
            if let Some(tool) = progress.active_tools.iter_mut().find(|tool| tool.id == id) {
                tool.label = label;
            } else {
                progress.active_tools.push(ActiveTool { id, label });
            }
        } else {
            progress.active_tools.retain(|tool| tool.id != id);
        }

        if progress.active_tools.is_empty() {
            self.progress = None;
        } else if let Some(tool) = progress.active_tools.last() {
            progress.label = tool.label.clone();
            progress.throbber.calc_next();
        }

        self.dirty = true;
        true
    }

    pub(super) fn tick_progress(&mut self) -> bool {
        let Some(progress) = &mut self.progress else {
            return false;
        };

        progress.throbber.calc_next();
        self.dirty = true;
        true
    }

    fn push_transcript(&mut self, kind: TranscriptKind, text: String) {
        self.transcript.push(TranscriptEntry { kind, text });
        const MAX_TRANSCRIPT_ENTRIES: usize = 100;
        if self.transcript.len() > MAX_TRANSCRIPT_ENTRIES {
            self.transcript
                .drain(..self.transcript.len() - MAX_TRANSCRIPT_ENTRIES);
        }
    }

    pub(super) fn resize_with_metrics(
        &mut self,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> bool {
        self.metrics = metrics;
        let size = ratatui_size_for_window(width, height, metrics);
        if size == self.size {
            return false;
        }

        self.size = size;
        self.dirty = true;
        true
    }

    pub(super) fn apply_theme(&mut self, theme: &TerminalTheme) {
        if self.theme == *theme {
            return;
        }

        self.theme = theme.clone();
        self.dirty = true;
    }

    pub(super) fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        buffer_width: usize,
        buffer_height: usize,
        rect: PaneRect,
        active: bool,
        force_redraw: bool,
        damage: &mut Vec<DamageRect>,
    ) -> bool {
        if rect.width == 0 || rect.height == 0 || self.size.cols == 0 || self.size.rows == 0 {
            return false;
        }

        let mut ratatui_buffer = Buffer::empty(Rect::new(0, 0, self.size.cols, self.size.rows));
        render_static_spike(
            &mut ratatui_buffer,
            self.size,
            &self.theme,
            &self.transcript,
            self.progress.as_mut(),
            &self.prompt,
            self.last_submitted.as_deref(),
        );

        let redrawn = draw_ratatui_buffer(
            &ratatui_buffer,
            font_renderer,
            buffer,
            buffer_width,
            buffer_height,
            rect.x,
            rect.y,
            self.metrics,
            self.theme.foreground,
            self.theme.background,
            force_redraw || self.dirty,
            damage,
        );

        if redrawn || force_redraw {
            draw_pane_border(buffer, buffer_width, buffer_height, rect, active);
        }
        self.dirty = false;
        redrawn
    }
}

fn render_static_spike(
    buffer: &mut Buffer,
    size: RatatuiPaneSize,
    theme: &TerminalTheme,
    transcript_entries: &[TranscriptEntry],
    progress: Option<&mut ProgressState>,
    prompt: &str,
    last_submitted: Option<&str>,
) {
    let area = Rect::new(0, 0, size.cols, size.rows);
    let prompt_height = {
        // Use conservative width (account for possible progress sidebar)
        let content_width = size.cols.saturating_sub(8);
        let display_text = if prompt.is_empty() {
            "Ask the agent..."
        } else {
            prompt
        };
        let prefix_width = 2; // "> "
        let wrap_width = content_width.saturating_sub(prefix_width).max(1) as usize;
        let char_count = display_text.len();
        let lines_needed = if wrap_width > 0 {
            (char_count + wrap_width - 1) / wrap_width
        } else {
            1
        };
        let h = (lines_needed as u16 + 2).max(3);
        h.min(size.rows.saturating_sub(6)).max(3)
    };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(prompt_height),
    ])
    .split(area);

    let title_style = Style::default()
        .fg(Color::Rgb(0x83, 0xa5, 0x98))
        .add_modifier(Modifier::BOLD);
    let muted_style = Style::default().fg(Color::Indexed(8));
    let user_style = Style::default()
        .fg(Color::Rgb(0xfe, 0x80, 0x19))
        .add_modifier(Modifier::BOLD);
    let agent_style = Style::default()
        .fg(Color::Rgb(0xb8, 0xbb, 0x26))
        .add_modifier(Modifier::BOLD);
    let thought_style = Style::default().fg(Color::Rgb(0x8e, 0xc0, 0x7c));
    let tool_style = Style::default().fg(Color::Rgb(0xd3, 0x86, 0x9b));

    Paragraph::new(vec![Line::from(vec![
        Span::styled("ACP mode", title_style),
        Span::raw("  "),
    ])])
    .block(Block::default().borders(Borders::BOTTOM))
    .render(chunks[0], buffer);

    let mut transcript = Vec::new();
    if transcript_entries.is_empty() {
        transcript.push(Line::from(Span::styled(
            "Type in the prompt area and press Enter to submit.",
            muted_style,
        )));
    } else {
        for entry in transcript_entries {
            let (label, style) = match entry.kind {
                TranscriptKind::User => ("You", user_style),
                TranscriptKind::Agent => ("Agent", agent_style),
                TranscriptKind::Thought => ("Thought", thought_style),
                TranscriptKind::Tool => ("Tool", tool_style),
                TranscriptKind::System => ("System", muted_style),
            };
            transcript.push(Line::from(vec![
                Span::styled(label, style),
                Span::raw(format!(": {}", entry.text)),
            ]));
            transcript.push(Line::from(""));
        }
    }
    if let Some(text) = last_submitted {
        transcript.push(Line::from(Span::styled(
            format!("Last prompt: {text}"),
            muted_style,
        )));
    }
    Paragraph::new(transcript)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::NONE))
        .render(chunks[1], buffer);

    let prompt_text = if prompt.is_empty() {
        Span::styled("Ask the agent...", muted_style)
    } else {
        Span::raw(prompt)
    };
    let show_progress = progress.is_some() && chunks[2].width >= 10;
    let prompt_chunks = if show_progress {
        Layout::horizontal([Constraint::Min(1), Constraint::Length(3)])
            .spacing(1)
            .split(chunks[2])
    } else {
        Layout::horizontal([Constraint::Min(1)]).split(chunks[2])
    };
    Paragraph::new(Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Rgb(0x83, 0xa5, 0x98))),
        prompt_text,
    ]))
    .wrap(Wrap { trim: false })
    .style(Style::default().bg(Color::Rgb(
        ((theme.background >> 16) & 0xff) as u8,
        ((theme.background >> 8) & 0xff) as u8,
        (theme.background & 0xff) as u8,
    )))
    .block(Block::default().borders(Borders::ALL).title("Prompt"))
    .render(prompt_chunks[0], buffer);

    if show_progress {
        let progress = progress.expect("progress is present when show_progress is true");
        render_progress(buffer, prompt_chunks[1], progress, title_style);
    }
}

fn render_progress(
    buffer: &mut Buffer,
    area: Rect,
    progress: &mut ProgressState,
    title_style: Style,
) {
    let area = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
    StatefulWidget::render(
        Throbber::default()
            .throbber_style(title_style)
            .throbber_set(QUADRANT_BLOCK_CRACK),
        area,
        buffer,
        &mut progress.throbber,
    );
}

fn ratatui_size_for_window(
    width: usize,
    height: usize,
    metrics: TerminalMetrics,
) -> RatatuiPaneSize {
    RatatuiPaneSize {
        cols: (width / metrics.cell_width).min(u16::MAX as usize) as u16,
        rows: (height / metrics.cell_height).min(u16::MAX as usize) as u16,
    }
}
