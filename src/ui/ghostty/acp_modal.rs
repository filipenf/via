use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use crate::event::{AcpModalKind, AcpPermissionOption};

pub struct AcpModalState {
    pub jsonrpc_id: serde_json::Value,
    pub title: String,
    pub message: String,
    pub options: Vec<AcpPermissionOption>,
    pub kind: AcpModalKind,
    pub focused: usize,
}

impl AcpModalState {
    pub fn new(
        jsonrpc_id: serde_json::Value,
        title: String,
        message: String,
        options: Vec<AcpPermissionOption>,
        kind: AcpModalKind,
    ) -> Self {
        Self {
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
            AcpModalKind::CursorAskQuestion { question_id } => serde_json::json!({
                "outcome": {
                    "outcome": "answered",
                    "answers": [{
                        "questionId": question_id,
                        "selectedOptionIds": [option_id],
                    }]
                }
            }),
        }
    }

    pub fn result_cancelled(&self) -> serde_json::Value {
        serde_json::json!({
            "outcome": { "outcome": "cancelled" }
        })
    }
}

/// Full-window ratatui buffer: dim background + centered dialog.
pub fn render_acp_modal_buffer(
    modal: &AcpModalState,
    cols: u16,
    rows: u16,
    background_rgb: u32,
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

    Block::default().style(Style::default().bg(dim)).render(area, &mut buf);

    let modal_w = cols.saturating_sub(8).max(36).min(78);
    let x0 = cols.saturating_sub(modal_w) / 2;
    let inner_w = modal_w.saturating_sub(4).max(8) as usize;
    let msg_lines = if inner_w > 0 {
        (modal.message.chars().count() + inner_w - 1) / inner_w
    } else {
        1
    }
    .max(1);
    let body_rows = 1u16 + msg_lines as u16 + 1 + modal.options.len().max(1) as u16 + 2;
    let modal_h = (6u16 + body_rows).min(rows.saturating_sub(4)).max(12);
    let y0 = rows.saturating_sub(modal_h) / 2;
    let modal_area = Rect::new(x0, y0, modal_w, modal_h);

    let title_line = Line::from(vec![Span::styled(
        modal.title.as_str(),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )]);
    let help = Line::from(Span::styled(
        "↑/↓ choose · Enter confirm · Esc cancel · 1–9 jump",
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
                    " Agent request ",
                    Style::default().fg(frame).add_modifier(Modifier::BOLD),
                )))
                .title_alignment(Alignment::Center)
                .border_style(Style::default().fg(frame))
                .style(Style::default().bg(Color::Indexed(0))),
        )
        .render(modal_area, &mut buf);

    buf
}
