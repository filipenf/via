use std::collections::VecDeque;

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::event::{AcpModalKind, AcpPermissionOption};

pub struct AcpModalState {
    /// ACP agent whose request this modal answers (for routing the JSON-RPC result back).
    pub agent_id: String,
    pub jsonrpc_id: serde_json::Value,
    pub title: String,
    pub message: String,
    pub options: Vec<AcpPermissionOption>,
    pub kind: AcpModalKind,
    pub focused: usize,
}

impl AcpModalState {
    pub fn new(
        agent_id: String,
        jsonrpc_id: serde_json::Value,
        title: String,
        message: String,
        options: Vec<AcpPermissionOption>,
        kind: AcpModalKind,
    ) -> Self {
        Self {
            agent_id,
            jsonrpc_id,
            title,
            message,
            options,
            kind,
            focused: 0,
        }
    }

    pub fn move_focus(&mut self, delta: isize) {
        if self.options.is_empty() {
            return;
        }
        let n = self.options.len() as isize;
        let i = self.focused as isize + delta;
        let i = ((i % n) + n) % n;
        self.focused = i as usize;
    }

    pub fn result_for_selection(&self, option_index: usize) -> serde_json::Value {
        let opt = self
            .options
            .get(option_index)
            .expect("selection index in range");
        self.result_for_option_id(&opt.option_id)
    }

    pub fn result_for_option_id(&self, option_id: &str) -> serde_json::Value {
        match &self.kind {
            AcpModalKind::SessionPermission => serde_json::json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id,
                }
            }),
            AcpModalKind::AskQuestion { question_id } => serde_json::json!({
                "outcome": {
                    "outcome": "answered",
                    "answers": [{
                        "questionId": question_id,
                        "selectedOptionIds": [option_id],
                    }]
                }
            }),
            AcpModalKind::HandshakeRetry => serde_json::Value::Null,
        }
    }

    pub fn result_cancelled(&self) -> serde_json::Value {
        serde_json::json!({
            "outcome": { "outcome": "cancelled" }
        })
    }
}

/// FIFO queue of ACP modal requests; the front entry is the active modal.
pub struct AcpModalQueue {
    queue: VecDeque<AcpModalState>,
}

impl AcpModalQueue {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    pub fn push(&mut self, state: AcpModalState) {
        self.queue.push_back(state);
    }

    /// Remove and return the active (front) modal after the user resolves it.
    pub fn resolve(&mut self) -> Option<AcpModalState> {
        self.queue.pop_front()
    }

    /// Cycle to the next pending modal (Tab). No-op when len <= 1.
    pub fn next(&mut self) {
        if self.queue.len() <= 1 {
            return;
        }
        if let Some(front) = self.queue.pop_front() {
            self.queue.push_back(front);
        }
    }

    /// Cycle to the previous pending modal (Shift+Tab). No-op when len <= 1.
    pub fn prev(&mut self) {
        if self.queue.len() <= 1 {
            return;
        }
        if let Some(back) = self.queue.pop_back() {
            self.queue.push_front(back);
        }
    }

    /// Drop all modals for a terminated agent (no JSON-RPC response).
    pub fn remove_agent(&mut self, agent_id: &str) {
        self.queue.retain(|m| m.agent_id != agent_id);
    }

    pub fn pending_count(&self) -> usize {
        self.queue.len().saturating_sub(1)
    }

    pub fn active(&self) -> Option<&AcpModalState> {
        self.queue.front()
    }

    pub fn active_mut(&mut self) -> Option<&mut AcpModalState> {
        self.queue.front_mut()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl Default for AcpModalQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Layout rectangle for the modal dialog (~2-cell margin, minimum size floor).
fn acp_modal_rect(cols: u16, rows: u16) -> Rect {
    const MARGIN: u16 = 2;
    const MIN_W: u16 = 36;
    const MIN_H: u16 = 12;
    let modal_w = cols.saturating_sub(MARGIN * 2).max(MIN_W.min(cols));
    let modal_h = rows.saturating_sub(MARGIN * 2).max(MIN_H.min(rows));
    let x0 = MARGIN.min(cols.saturating_sub(modal_w));
    let y0 = MARGIN.min(rows.saturating_sub(modal_h));
    Rect::new(x0, y0, modal_w, modal_h)
}

fn acp_modal_border_title(pending_count: usize) -> String {
    if pending_count > 0 {
        format!(" Agent request (+{pending_count} pending) ")
    } else {
        " Agent request ".to_string()
    }
}

/// Full-window ratatui buffer: dim background + centered dialog.
pub fn render_acp_modal_buffer(
    modal: &AcpModalState,
    cols: u16,
    rows: u16,
    background_rgb: u32,
    pending_count: usize,
) -> Buffer {
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);
    let r = ((background_rgb >> 16) & 0xff) as u8;
    let g = ((background_rgb >> 8) & 0xff) as u8;
    let b = (background_rgb & 0xff) as u8;
    let dim = Color::Rgb(
        r.saturating_sub(25),
        g.saturating_sub(22),
        b.saturating_sub(18),
    );
    let frame = Color::Indexed(12);
    let accent = Color::Indexed(11);
    let text = Color::Indexed(7);

    Block::default()
        .style(Style::default().bg(dim))
        .render(area, &mut buf);

    let modal_area = acp_modal_rect(cols, rows);

    let title_line = Line::from(vec![Span::styled(
        modal.title.as_str(),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )]);
    let help = Line::from(Span::styled(
        "Tab next · Shift+Tab prev · ↑/↓ choose · Enter confirm · Esc cancel · 1–9 jump",
        Style::default().fg(Color::Indexed(8)),
    ));

    let mut body_lines: Vec<Line> = vec![title_line, Line::from("")];
    if !modal.message.trim().is_empty() {
        body_lines.push(Line::from(Span::styled(
            modal.message.as_str(),
            Style::default().fg(text),
        )));
        body_lines.push(Line::from(""));
    }
    for (i, opt) in modal.options.iter().enumerate() {
        let prefix = format!("{}. ", i + 1);
        let style = if i == modal.focused {
            Style::default()
                .fg(Color::Indexed(0))
                .bg(accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(text)
        };
        body_lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(opt.name.as_str(), style),
        ]));
    }
    body_lines.push(Line::from(""));
    body_lines.push(help);

    Paragraph::new(body_lines)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::from(Span::styled(
                    acp_modal_border_title(pending_count),
                    Style::default().fg(frame).add_modifier(Modifier::BOLD),
                )))
                .title_alignment(Alignment::Center)
                .border_style(Style::default().fg(frame))
                .style(Style::default().bg(Color::Indexed(0))),
        )
        .render(modal_area, &mut buf);

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modal(agent_id: &str, title: &str) -> AcpModalState {
        AcpModalState::new(
            agent_id.to_string(),
            serde_json::json!(1),
            title.to_string(),
            String::new(),
            vec![],
            AcpModalKind::SessionPermission,
        )
    }

    fn drain_titles(q: &mut AcpModalQueue) -> Vec<String> {
        let mut titles = Vec::new();
        while let Some(m) = q.resolve() {
            titles.push(m.title);
        }
        titles
    }

    #[test]
    fn push_fifo_order() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));
        assert_eq!(q.active().unwrap().title, "first");
        assert_eq!(drain_titles(&mut q), vec!["first", "second", "third"]);
    }

    #[test]
    fn resolve_advances_in_order() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));

        assert_eq!(q.resolve().unwrap().title, "first");
        assert_eq!(q.active().unwrap().title, "second");
        assert_eq!(q.resolve().unwrap().title, "second");
        assert_eq!(q.active().unwrap().title, "third");
        assert_eq!(q.resolve().unwrap().title, "third");
        assert!(q.is_empty());
    }

    #[test]
    fn next_wraparound() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));

        q.next();
        assert_eq!(q.active().unwrap().title, "second");

        q.next();
        assert_eq!(q.active().unwrap().title, "third");

        q.next();
        assert_eq!(q.active().unwrap().title, "first");
        assert_eq!(drain_titles(&mut q), vec!["first", "second", "third"]);
    }

    #[test]
    fn prev_wraparound() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));

        q.prev();
        assert_eq!(q.active().unwrap().title, "third");

        q.prev();
        assert_eq!(q.active().unwrap().title, "second");

        q.prev();
        assert_eq!(q.active().unwrap().title, "first");
        assert_eq!(drain_titles(&mut q), vec!["first", "second", "third"]);
    }

    #[test]
    fn navigation_preserves_all_entries() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));

        for _ in 0..6 {
            q.next();
        }
        assert_eq!(drain_titles(&mut q), vec!["first", "second", "third"]);

        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));
        for _ in 0..6 {
            q.prev();
        }
        assert_eq!(drain_titles(&mut q), vec!["first", "second", "third"]);
    }

    #[test]
    fn next_prev_noop_when_single() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "only"));
        q.next();
        q.prev();
        assert_eq!(q.active().unwrap().title, "only");
        assert_eq!(drain_titles(&mut q), vec!["only"]);
    }

    #[test]
    fn resolve_active_only() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "first"));
        q.push(modal("b", "second"));
        q.push(modal("c", "third"));

        assert_eq!(q.resolve().unwrap().title, "first");
        assert_eq!(q.active().unwrap().title, "second");
        assert_eq!(drain_titles(&mut q), vec!["second", "third"]);
    }

    #[test]
    fn remove_agent_keeps_other_agents() {
        let mut q = AcpModalQueue::new();
        q.push(modal("a", "a1"));
        q.push(modal("b", "b1"));
        q.push(modal("a", "a2"));
        q.push(modal("c", "c1"));

        q.remove_agent("a");
        assert_eq!(q.active().unwrap().title, "b1");
        assert_eq!(drain_titles(&mut q), vec!["b1", "c1"]);
    }

    #[test]
    fn pending_count() {
        let mut q = AcpModalQueue::new();
        assert_eq!(q.pending_count(), 0);

        q.push(modal("a", "first"));
        assert_eq!(q.pending_count(), 0);

        q.push(modal("b", "second"));
        assert_eq!(q.pending_count(), 1);

        q.push(modal("c", "third"));
        assert_eq!(q.pending_count(), 2);

        q.resolve();
        assert_eq!(q.pending_count(), 1);

        q.next();
        assert_eq!(q.pending_count(), 1);
    }

    fn render_modal(pending_count: usize, cols: u16, rows: u16) -> Buffer {
        let m = AcpModalState::new(
            "agent".to_string(),
            serde_json::json!(1),
            "Allow command?".to_string(),
            "Run `cargo test` in the project root.".to_string(),
            vec![AcpPermissionOption {
                option_id: "allow".to_string(),
                name: "Allow".to_string(),
            }],
            AcpModalKind::SessionPermission,
        );
        render_acp_modal_buffer(&m, cols, rows, 0x1e1e2e, pending_count)
    }

    fn buffer_plain_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            if y + 1 < buf.area.height {
                out.push('\n');
            }
        }
        out
    }

    fn modal_border_origin(buf: &Buffer) -> Option<(u16, u16)> {
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if buf.cell((x, y)).is_some_and(|cell| cell.symbol() == "┌") {
                    return Some((x, y));
                }
            }
        }
        None
    }

    #[test]
    fn render_shows_pending_badge_when_pending_positive() {
        let buf = render_modal(2, 120, 40);
        let text = buffer_plain_text(&buf);
        assert!(text.contains("(+2 pending)"));
        assert!(text.contains("Tab next"));
        assert!(text.contains("Shift+Tab prev"));
    }

    #[test]
    fn render_hides_pending_badge_when_zero() {
        let buf = render_modal(0, 120, 40);
        let text = buffer_plain_text(&buf);
        assert!(text.contains(" Agent request "));
        assert!(!text.contains("pending)"));
    }

    #[test]
    fn render_modal_uses_small_margins_on_large_terminal() {
        let cols = 120u16;
        let rows = 40u16;
        let expected = acp_modal_rect(cols, rows);
        let buf = render_modal(0, cols, rows);
        assert_eq!(expected, Rect::new(2, 2, 116, 36));
        assert_eq!(modal_border_origin(&buf), Some((expected.x, expected.y)));
    }

    #[test]
    fn render_modal_respects_minimum_size_on_tiny_terminal() {
        let cols = 30u16;
        let rows = 14u16;
        let expected = acp_modal_rect(cols, rows);
        let buf = render_modal(0, cols, rows);
        assert_eq!(expected.width, cols);
        assert_eq!(expected.height, 12);
        assert_eq!(modal_border_origin(&buf), Some((expected.x, expected.y)));
    }
}
