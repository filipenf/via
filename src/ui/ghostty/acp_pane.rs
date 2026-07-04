use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, StatefulWidget, Widget, Wrap};
use throbber_widgets_tui::{QUADRANT_BLOCK_CRACK, Throbber, ThrobberState};
use unicode_width::UnicodeWidthChar;

use super::config::{TerminalMetrics, TerminalTheme};
use super::font::FontRenderer;
use super::input::{Key, Modifiers};
use super::layout::PaneRect;
use super::render::{DamageRect, draw_pane_focus_chrome, draw_ratatui_buffer};

const TURN_PROGRESS_ID: &str = "__turn";

/// ACP UI pane plus bus identity (orchestrator when orchestrating).
pub(super) struct AcpAgentPane {
    pub(super) pane: AcpPane,
    pub(super) id: String,
    pub(super) label: String,
    pub(super) command: Option<String>,
}

pub(super) struct AcpPane {
    metrics: TerminalMetrics,
    theme: TerminalTheme,
    size: RatatuiPaneSize,
    header_label: String,
    model: Option<String>,
    provider_error: Option<String>,
    transcript: Vec<TranscriptEntry>,
    progress: Option<ProgressState>,
    prompt: String,
    last_submitted: Option<String>,
    /// Vertical scroll offset in wrapped transcript lines (ratatui `Paragraph::scroll` y).
    transcript_scroll_y: u16,
    /// When true, keep the transcript pinned to the newest lines as content grows.
    transcript_follow_bottom: bool,
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
            header_label: "ACP".to_string(),
            model: None,
            provider_error: None,
            transcript: Vec::new(),
            progress: None,
            prompt: String::new(),
            last_submitted: None,
            transcript_scroll_y: 0,
            transcript_follow_bottom: true,
            dirty: true,
        }
    }

    pub(super) fn set_header_label(&mut self, label: &str) {
        let label = label.trim();
        if label.is_empty() || self.header_label == label {
            return;
        }
        self.header_label = label.to_string();
        self.dirty = true;
    }

    pub(super) fn apply_session_status(
        &mut self,
        model: Option<String>,
        provider_error: Option<String>,
        clear_provider_error: bool,
    ) {
        let mut changed = false;
        if let Some(model) = model.filter(|model| !model.is_empty()) {
            if self.model.as_deref() != Some(model.as_str()) {
                self.model = Some(model);
                changed = true;
            }
        }
        if clear_provider_error && self.provider_error.is_some() {
            self.provider_error = None;
            changed = true;
        } else if let Some(error) = provider_error.filter(|error| !error.is_empty()) {
            if self.provider_error.as_deref() != Some(error.as_str()) {
                self.provider_error = Some(error);
                changed = true;
            }
        }
        if changed {
            self.dirty = true;
        }
    }

    /// Scroll transcript lines. `delta_lines` matches terminal PTY semantics: positive scrolls
    /// "up" into older content (decreases the top offset), negative moves toward the live bottom.
    pub(super) fn scroll_transcript(&mut self, delta_lines: isize) -> bool {
        self.transcript_follow_bottom = false;
        let next = (self.transcript_scroll_y as i32).saturating_sub(delta_lines as i32);
        self.transcript_scroll_y = next.clamp(0, i32::from(u16::MAX)) as u16;
        self.dirty = true;
        true
    }

    pub(super) fn scroll_transcript_to_top(&mut self) -> bool {
        self.transcript_follow_bottom = false;
        self.transcript_scroll_y = 0;
        self.dirty = true;
        true
    }

    pub(super) fn scroll_transcript_to_bottom(&mut self) -> bool {
        self.transcript_follow_bottom = true;
        self.dirty = true;
        true
    }

    /// Rows available for the transcript body (ratatui cells), for page-scroll step size.
    pub(super) fn transcript_viewport_rows(&self) -> u16 {
        let prompt_h = self.prompt_height_cells();
        let header_h = self.header_height_cells();
        self.size
            .rows
            .saturating_sub(header_h)
            .saturating_sub(prompt_h)
            .max(1)
    }

    fn header_height_cells(&self) -> u16 {
        let content_lines = if self.provider_error.is_some() { 2 } else { 1 };
        // One row for the bottom border.
        content_lines + 1
    }

    fn prompt_height_cells(&self) -> u16 {
        let content_width = self.size.cols.saturating_sub(8);
        let display_text = if self.prompt.is_empty() {
            "Ask the agent..."
        } else {
            self.prompt.as_str()
        };
        let prefix_width = 2;
        let wrap_width = content_width.saturating_sub(prefix_width).max(1) as usize;
        let char_count = display_text.chars().count();
        let lines_needed = char_count.div_ceil(wrap_width);
        let h = (lines_needed as u16 + 2).max(3);
        h.min(self.size.rows.saturating_sub(6)).max(3)
    }

    pub(super) fn handle_text_input(&mut self, text: &str, modifiers: Modifiers) -> bool {
        if modifiers.ctrl || modifiers.super_key || text.is_empty() {
            return false;
        }

        self.prompt.push_str(text);
        self.dirty = true;
        true
    }

    /// Insert OS clipboard text (Super+V / Ctrl+Shift+V / Shift+Insert). Strips control
    /// characters except newline and tab so bracketed-paste escape sequences never enter the
    /// prompt.
    pub(super) fn paste_text(&mut self, text: &str) -> bool {
        let sanitized: String = text
            .chars()
            .filter(|&ch| ch == '\n' || ch == '\t' || !ch.is_control())
            .collect();
        if sanitized.is_empty() {
            return false;
        }
        self.prompt.push_str(&sanitized);
        self.dirty = true;
        true
    }

    pub(super) fn handle_key(&mut self, key: Key, modifiers: Modifiers) -> Option<String> {
        if modifiers.ctrl && key == Key::W {
            if delete_word_backward(&mut self.prompt) {
                self.dirty = true;
            }
            return None;
        }

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
                self.transcript_follow_bottom = true;
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        buffer_width: usize,
        buffer_height: usize,
        rect: PaneRect,
        active: bool,
        force_redraw: bool,
        redraw_chrome: bool,
        damage: &mut Vec<DamageRect>,
    ) -> bool {
        if rect.width == 0 || rect.height == 0 || self.size.cols == 0 || self.size.rows == 0 {
            return false;
        }

        let mut ratatui_buffer = Buffer::empty(Rect::new(0, 0, self.size.cols, self.size.rows));
        self.render_into_buffer(&mut ratatui_buffer);

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

        let chrome_drawn = if redrawn || force_redraw || redraw_chrome {
            draw_pane_focus_chrome(
                buffer,
                buffer_width,
                buffer_height,
                rect,
                active,
                &self.theme,
                redrawn || force_redraw,
                damage,
            );
            true
        } else {
            false
        };
        self.dirty = false;
        redrawn || chrome_drawn
    }

    fn render_into_buffer(&mut self, buffer: &mut Buffer) {
        let size = self.size;
        let area = Rect::new(0, 0, size.cols, size.rows);
        let prompt_height = self.prompt_height_cells();
        let header_height = self.header_height_cells();
        let chunks = Layout::vertical([
            Constraint::Length(header_height),
            Constraint::Min(3),
            Constraint::Length(prompt_height),
        ])
        .split(area);

        let title_style = Style::default()
            .fg(Color::Indexed(12))
            .add_modifier(Modifier::BOLD);
        let muted_style = Style::default().fg(Color::Indexed(8));
        let warn_style = Style::default()
            .fg(Color::Indexed(11))
            .add_modifier(Modifier::BOLD);
        let error_style = Style::default()
            .fg(Color::Indexed(9))
            .add_modifier(Modifier::BOLD);
        let user_style = Style::default()
            .fg(Color::Indexed(11))
            .add_modifier(Modifier::BOLD);
        let agent_style = Style::default()
            .fg(Color::Indexed(10))
            .add_modifier(Modifier::BOLD);
        let thought_style = Style::default().fg(Color::Indexed(14));
        let tool_style = Style::default().fg(Color::Indexed(13));
        let code_style = Style::default()
            .fg(Color::Indexed(15))
            .bg(code_block_background(self.theme.background));
        let code_fence_style = Style::default()
            .fg(Color::Indexed(8))
            .bg(code_block_background(self.theme.background));

        let mut header_lines = vec![Line::from(vec![
            Span::styled(self.header_label.as_str(), title_style),
            Span::raw("  ·  "),
            Span::styled(
                format!(
                    "model: {}",
                    self.model
                        .as_deref()
                        .filter(|model| !model.is_empty())
                        .unwrap_or("(unknown)")
                ),
                muted_style,
            ),
        ])];
        if let Some(error) = &self.provider_error {
            header_lines.push(Line::from(vec![
                Span::styled("⚠ ", warn_style),
                Span::styled(error.as_str(), error_style),
            ]));
        }

        Paragraph::new(header_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::BOTTOM))
            .render(chunks[0], buffer);

        let mut transcript = Vec::new();
        if self.transcript.is_empty() {
            transcript.push(Line::from(Span::styled(
                "Type in the prompt area and press Enter to submit.",
                muted_style,
            )));
        } else {
            for entry in &self.transcript {
                let (label, style) = match entry.kind {
                    TranscriptKind::User => ("You", user_style),
                    TranscriptKind::Agent => ("Agent", agent_style),
                    TranscriptKind::Thought => ("Thought", thought_style),
                    TranscriptKind::Tool => ("Tool", tool_style),
                    TranscriptKind::System => ("System", muted_style),
                };
                transcript.extend(format_entry(
                    label,
                    style,
                    &entry.text,
                    code_style,
                    code_fence_style,
                ));
            }
        }
        if let Some(text) = self.last_submitted.as_deref() {
            transcript.extend(format_entry(
                "Last prompt",
                muted_style,
                text,
                code_style,
                code_fence_style,
            ));
        }

        let transcript_width = chunks[1].width.max(1);
        let transcript_visible = chunks[1].height as usize;
        let total_rows = transcript_wrapped_rows(&transcript, transcript_width);
        let max_scroll = total_rows
            .saturating_sub(transcript_visible)
            .min(usize::from(u16::MAX)) as u16;

        let scroll_y = if self.transcript_follow_bottom {
            max_scroll
        } else {
            self.transcript_scroll_y.min(max_scroll)
        };
        self.transcript_scroll_y = scroll_y;
        if scroll_y >= max_scroll {
            self.transcript_follow_bottom = true;
        }

        Paragraph::new(transcript)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0))
            .block(Block::default().borders(Borders::NONE))
            .render(chunks[1], buffer);

        let prompt_text = if self.prompt.is_empty() {
            Span::styled("Ask the agent...", muted_style)
        } else {
            Span::raw(self.prompt.as_str())
        };
        let show_progress = self.progress.is_some() && chunks[2].width >= 10;
        let prompt_chunks = if show_progress {
            Layout::horizontal([Constraint::Min(1), Constraint::Length(3)])
                .spacing(1)
                .split(chunks[2])
        } else {
            Layout::horizontal([Constraint::Min(1)]).split(chunks[2])
        };
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Indexed(12))),
            prompt_text,
        ]))
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(Color::Rgb(
            ((self.theme.background >> 16) & 0xff) as u8,
            ((self.theme.background >> 8) & 0xff) as u8,
            (self.theme.background & 0xff) as u8,
        )))
        .block(Block::default().borders(Borders::ALL).title("Prompt"))
        .render(prompt_chunks[0], buffer);

        if show_progress {
            let progress = self
                .progress
                .as_mut()
                .expect("progress is present when show_progress is true");
            render_progress(buffer, prompt_chunks[1], progress, title_style);
        }
    }
}

/// Shell/readline-style backward-kill-word: drop trailing whitespace, then the last run of
/// non-whitespace characters.
fn delete_word_backward(prompt: &mut String) -> bool {
    let before = prompt.len();
    while prompt.chars().last().is_some_and(|c| c.is_whitespace()) {
        prompt.pop();
    }
    while prompt.chars().last().is_some_and(|c| !c.is_whitespace()) {
        prompt.pop();
    }
    prompt.len() != before
}

fn estimate_wrapped_line_rows(line: &Line, width: usize) -> usize {
    if width < 1 {
        return 1;
    }
    let mut row_width = 0usize;
    let mut rows = 1usize;
    let mut has_content = false;
    for span in &line.spans {
        for ch in span.content.chars() {
            has_content = true;
            let mut cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cw == 0 {
                cw = 1;
            }
            if row_width + cw > width {
                rows += 1;
                row_width = cw;
            } else {
                row_width += cw;
            }
        }
    }
    if has_content { rows } else { 1 }
}

fn transcript_wrapped_rows(lines: &[Line], width: u16) -> usize {
    let w = width.max(1) as usize;
    lines
        .iter()
        .map(|line| estimate_wrapped_line_rows(line, w))
        .sum()
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

fn code_block_background(theme_bg: u32) -> Color {
    let r = ((theme_bg >> 16) & 0xff) as i32;
    let g = ((theme_bg >> 8) & 0xff) as i32;
    let b = (theme_bg & 0xff) as i32;
    // Lighten dark themes and darken light themes for subtle contrast.
    let luminance = (r * 299 + g * 587 + b * 114) / 1000;
    let offset = if luminance > 128 { -16 } else { 16 };
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    Color::Rgb(clamp(r + offset), clamp(g + offset), clamp(b + offset))
}

/// Format one transcript entry into a sequence of `Line`s.
///
/// The speaker label is placed on its own line, body text is split on newlines
/// so paragraph breaks are preserved, and fenced code blocks (```) get a
/// distinct background style.
fn format_entry(
    label: &str,
    label_style: Style,
    text: &str,
    code_style: Style,
    code_fence_style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(label.to_string(), label_style)));

    let mut in_code_block = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::from(Span::styled(
                trimmed.to_string(),
                code_fence_style,
            )));
            continue;
        }
        if in_code_block {
            lines.push(Line::from(Span::styled(line.to_string(), code_style)));
        } else if line.is_empty() {
            lines.push(Line::from(""));
        } else {
            lines.push(Line::from(Span::raw(line.to_string())));
        }
    }

    // Separator between entries.
    lines.push(Line::from(""));
    lines
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

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier, Style};

    use super::{code_block_background, format_entry};

    fn test_style() -> Style {
        Style::default()
            .fg(Color::Indexed(10))
            .add_modifier(Modifier::BOLD)
    }

    fn code_style() -> Style {
        Style::default()
            .fg(Color::Indexed(15))
            .bg(Color::Rgb(40, 40, 40))
    }

    fn fence_style() -> Style {
        Style::default()
            .fg(Color::Indexed(8))
            .bg(Color::Rgb(40, 40, 40))
    }

    #[test]
    fn format_entry_splits_paragraphs() {
        let lines = format_entry(
            "Agent",
            test_style(),
            "First paragraph.\n\nSecond paragraph.",
            code_style(),
            fence_style(),
        );
        assert_eq!(lines.len(), 5); // label + 2 lines + blank + separator
        assert_eq!(lines[0].spans[0].content, "Agent");
        assert_eq!(lines[1].spans[0].content, "First paragraph.");
        assert!(lines[2].spans.is_empty(), "blank line between paragraphs");
        assert_eq!(lines[3].spans[0].content, "Second paragraph.");
        assert!(lines[4].spans.is_empty(), "trailing separator");
    }

    #[test]
    fn format_entry_styles_code_blocks() {
        let text = "Here is some code:\n```rust\nlet x = 1;\n```\nDone.";
        let lines = format_entry("Agent", test_style(), text, code_style(), fence_style());

        assert_eq!(lines[0].spans[0].content, "Agent");
        assert_eq!(lines[1].spans[0].content, "Here is some code:");

        // Opening fence.
        assert_eq!(lines[2].spans[0].content, "```rust");
        assert_eq!(lines[2].spans[0].style, fence_style());

        // Code body.
        assert_eq!(lines[3].spans[0].content, "let x = 1;");
        assert_eq!(lines[3].spans[0].style, code_style());

        // Closing fence.
        assert_eq!(lines[4].spans[0].content, "```");
        assert_eq!(lines[4].spans[0].style, fence_style());

        assert_eq!(lines[5].spans[0].content, "Done.");
    }

    #[test]
    fn code_block_background_contrasts_with_theme() {
        let dark = code_block_background(0x0c0c0c);
        let light = code_block_background(0xeeeeee);
        assert_ne!(dark, Color::Rgb(12, 12, 12));
        assert_ne!(light, Color::Rgb(238, 238, 238));
    }
}
