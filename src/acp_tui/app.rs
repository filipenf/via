//! Ratatui application state for the ACP TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::protocol::{HostToTui, TranscriptKind};

const MAX_TRANSCRIPT_ENTRIES: usize = 500;
/// Prompt bar background — slightly elevated vs default terminal black (cursor-agent-like).
const PROMPT_BG: Color = Color::Rgb(40, 40, 44);

struct TranscriptEntry {
    kind: TranscriptKind,
    text: String,
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
    provider_error: Option<String>,
    transcript: Vec<TranscriptEntry>,
    progress: Option<ProgressState>,
    prompt: String,
    show_input: bool,
    /// Offset from the live bottom: 0 = follow newest; larger = scrolled into history.
    scroll_from_bottom: u16,
    quit: bool,
}

#[derive(Debug)]
pub(super) enum InputEvent {
    None,
    Quit,
    Submit(String),
}

impl App {
    pub(super) fn new(agent_id: String, role: String, show_input: bool) -> Self {
        Self {
            agent_id,
            role,
            model: None,
            provider_error: None,
            transcript: Vec::new(),
            progress: None,
            prompt: String::new(),
            show_input,
            scroll_from_bottom: 0,
            quit: false,
        }
    }

    pub(super) fn seed_demo(&mut self) {
        self.push(
            TranscriptKind::System,
            "Demo mode — ↑/↓ scroll, type + Enter submits (JSON on stderr), q quits.".into(),
        );
        for i in 1..=24 {
            self.push(
                TranscriptKind::Agent,
                format!("Sample agent output line {i}"),
            );
        }
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
                } else {
                    self.push(kind, text);
                }
            }
            HostToTui::Progress { id, label, active } => {
                self.update_progress(id, label, active);
            }
            HostToTui::SessionStatus {
                model,
                provider_error,
                clear_provider_error,
            } => {
                if let Some(model) = model.filter(|m| !m.is_empty()) {
                    self.model = Some(model);
                }
                if clear_provider_error {
                    self.provider_error = None;
                } else if let Some(error) = provider_error.filter(|e| !e.is_empty()) {
                    self.provider_error = Some(error);
                }
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

        match key.code {
            KeyCode::Char('q')
                if self.prompt.is_empty() && !key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                InputEvent::Quit
            }
            KeyCode::Esc if self.prompt.is_empty() => InputEvent::Quit,
            KeyCode::Esc => {
                self.prompt.clear();
                InputEvent::None
            }
            KeyCode::Up | KeyCode::PageUp => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
                InputEvent::None
            }
            KeyCode::Down | KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
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
            KeyCode::Enter if self.show_input => {
                let text = self.prompt.trim().to_string();
                if text.is_empty() {
                    return InputEvent::None;
                }
                self.prompt.clear();
                self.push(TranscriptKind::User, text.clone());
                self.scroll_from_bottom = 0;
                self.update_progress("__turn".into(), "Thinking".into(), true);
                InputEvent::Submit(text)
            }
            KeyCode::Backspace if self.show_input => {
                self.prompt.pop();
                InputEvent::None
            }
            KeyCode::Char(c)
                if self.show_input && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.prompt.push(c);
                InputEvent::None
            }
            _ => InputEvent::None,
        }
    }

    pub(super) fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let constraints = if self.show_input {
            vec![
                Constraint::Length(self.header_height()),
                Constraint::Min(1),
                Constraint::Length(1),
            ]
        } else {
            vec![Constraint::Length(self.header_height()), Constraint::Min(1)]
        };
        let chunks = Layout::vertical(constraints).split(area);

        frame.render_widget(self.header_widget(chunks[1].height), chunks[0]);
        frame.render_widget(self.transcript_widget(chunks[1].height), chunks[1]);
        if self.show_input {
            frame.render_widget(self.prompt_widget(), chunks[2]);
        }
    }

    fn header_height(&self) -> u16 {
        if self.provider_error.is_some() { 3 } else { 2 }
    }

    fn header_widget(&self, transcript_rows: u16) -> Paragraph<'static> {
        let title_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(Color::DarkGray);
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

    fn prompt_widget(&self) -> Paragraph<'static> {
        // Subtle elevated bar (cursor-agent style): no box, just a shade difference.
        let bar = Style::default().bg(PROMPT_BG);
        let text = if self.prompt.is_empty() {
            Line::from(Span::styled(" Ask the agent…", bar.fg(Color::DarkGray)))
        } else {
            Line::from(Span::styled(
                format!(" {}█", self.prompt),
                bar.fg(Color::White),
            ))
        };
        Paragraph::new(text).style(bar)
    }

    fn scroll_offset(&self, viewport_rows: u16) -> u16 {
        let total = self.wrapped_lines().len() as u16;
        let max_scroll = total.saturating_sub(viewport_rows.max(1));
        self.scroll_from_bottom.min(max_scroll)
    }

    fn wrapped_lines(&self) -> Vec<Line<'static>> {
        if self.transcript.is_empty() {
            return vec![Line::from(Span::styled(
                "No transcript yet.",
                Style::default().fg(Color::DarkGray),
            ))];
        }

        let mut lines = Vec::new();
        for entry in &self.transcript {
            let (prefix, style) = match entry.kind {
                TranscriptKind::User => (
                    "you",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                TranscriptKind::Agent => ("agent", Style::default().fg(Color::Green)),
                TranscriptKind::Thought => ("thought", Style::default().fg(Color::Cyan)),
                TranscriptKind::Tool => ("tool", Style::default().fg(Color::Magenta)),
                TranscriptKind::System => ("system", Style::default().fg(Color::DarkGray)),
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
        lines
    }

    fn push(&mut self, kind: TranscriptKind, text: String) {
        self.transcript.push(TranscriptEntry { kind, text });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_clears_prompt_and_appends_user() {
        let mut app = App::new("coder".into(), "coder".into(), true);
        app.prompt = " hello ".into();
        match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            InputEvent::Submit(text) => assert_eq!(text, "hello"),
            other => panic!("expected submit, got {other:?}"),
        }
        assert!(app.prompt.is_empty());
        assert_eq!(app.transcript.last().unwrap().kind, TranscriptKind::User);
    }

    #[test]
    fn host_transcript_coalesces_same_kind() {
        let mut app = App::new("a".into(), "a".into(), true);
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
    }

    #[test]
    fn progress_activates_and_clears() {
        let mut app = App::new("a".into(), "a".into(), false);
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
    fn agent_transcript_clears_sticky_turn_progress() {
        let mut app = App::new("a".into(), "a".into(), true);
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
}
