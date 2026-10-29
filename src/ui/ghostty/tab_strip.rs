//! Builds the agent-region tab strip into a Ratatui [`Buffer`].
//!
//! This module is deliberately pixel-free: it only fills cells with glyphs and
//! styles. [`super::render::draw_ratatui_buffer`] blits the result into the frame,
//! so the strip shares the same font/shaping path as the ACP modal and needs no
//! second UI toolkit.
//!
//! Keeping the buffer construction pure is what makes the strip testable: the
//! tests below assert on cells and styles without a window, a GPU, or a font.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::PaneId;

/// Glyph shown before a tab label while that pane is working.
pub const BUSY_GLYPH: &str = "\u{25cf}";
/// Glyph shown before a tab label once the pane's process has exited.
pub const EXITED_GLYPH: &str = "\u{2717}";
/// Glyph shown before a tab label when output arrived while it was not active.
pub const UNSEEN_GLYPH: &str = "\u{2022}";
/// Separator drawn between adjacent tabs.
const TAB_SEPARATOR: &str = "\u{2502}";

/// Everything the strip needs to draw one tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStripEntry {
    pub id: PaneId,
    pub label: String,
    pub active: bool,
    pub busy: bool,
    pub exited: bool,
    pub unseen: bool,
}

/// The indicator a tab should show, in precedence order.
///
/// Exited wins over everything: a dead pane is not "working" and not "unseen", it
/// is finished. Busy then beats unseen, because a pane that is still producing
/// output will refresh its unseen state anyway, while the busy dot is the thing the
/// user needs to see at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabIndicator {
    Exited,
    Busy,
    Unseen,
    None,
}

impl TabIndicator {
    pub fn for_entry(entry: &TabStripEntry) -> Self {
        if entry.exited {
            Self::Exited
        } else if entry.busy {
            Self::Busy
        } else if entry.unseen {
            Self::Unseen
        } else {
            Self::None
        }
    }

    fn glyph(self) -> Option<&'static str> {
        match self {
            Self::Exited => Some(EXITED_GLYPH),
            Self::Busy => Some(BUSY_GLYPH),
            Self::Unseen => Some(UNSEEN_GLYPH),
            Self::None => None,
        }
    }

    fn color(self, active: bool) -> Color {
        match (self, active) {
            (Self::Exited, _) => Color::Indexed(9),
            // The accent color is reserved for "there is something new here".
            (Self::Unseen, _) => Color::Indexed(11),
            (Self::Busy, true) => Color::Indexed(15),
            (Self::Busy, false) => Color::Indexed(14),
            (Self::None, _) => Color::Indexed(8),
        }
    }
}

/// Render `entries` into a strip buffer exactly `cols` wide and one row tall.
///
/// Tabs are laid out left to right and clipped at `cols`; a label never splits a
/// cell, so a tab that does not fit is dropped whole rather than half-drawn.
pub fn tab_strip_buffer(entries: &[TabStripEntry], cols: u16, background: u32) -> Buffer {
    let area = Rect::new(0, 0, cols, 1);
    let mut buf = Buffer::empty(area);
    if cols == 0 {
        return buf;
    }

    let dim = rgb_color(background);
    // Fill the whole strip first so trailing cells match the pane background
    // instead of relying on the ratatui default.
    for col in 0..cols {
        buf[(col, 0)]
            .set_symbol(" ")
            .set_style(Style::default().bg(dim));
    }

    for (index, fitted) in fitting_tabs(entries, cols).into_iter().enumerate() {
        let Fit { entry, start, .. } = fitted;
        let mut col = start;
        if index > 0 {
            buf[(col - 1, 0)]
                .set_symbol(TAB_SEPARATOR)
                .set_style(Style::default().fg(Color::Indexed(8)).bg(dim));
        }

        let indicator = TabIndicator::for_entry(entry);
        let label = tab_text(entry);

        let base = if entry.active {
            Style::default()
                .fg(Color::Indexed(0))
                .bg(Color::Indexed(7))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Indexed(7)).bg(dim)
        };

        // Only the indicator glyph carries the status color; the label keeps the
        // tab's base style so a busy or unseen tab does not read as a solid block
        // of color. The glyph sits in the first cell after the leading space.
        let glyph_background = if entry.active { Color::Indexed(7) } else { dim };

        for (offset, ch) in label.chars().enumerate() {
            let cell = &mut buf[(col, 0)];
            cell.set_symbol(&ch.to_string()).set_style(base);
            if indicator != TabIndicator::None && offset == GLYPH_OFFSET {
                cell.set_style(
                    Style::default()
                        .fg(indicator.color(entry.active))
                        .bg(glyph_background)
                        .add_modifier(Modifier::BOLD),
                );
            }
            col += 1;
        }
    }

    buf
}

/// Character offset ranges (in cells) for each tab actually drawn, so a click can
/// be mapped back to a tab.
///
/// `cols` is the strip width in cells and must be the same value given to
/// [`tab_strip_buffer`]: both walk [`fitting_tabs`], so a tab that does not fit is
/// absent here exactly as it is absent from the pixels. Without that, a click on
/// the empty tail of the strip would hit a tab that was never drawn.
pub fn tab_strip_hit_boxes(entries: &[TabStripEntry], cols: u16) -> Vec<(PaneId, u16, u16)> {
    fitting_tabs(entries, cols)
        .into_iter()
        .map(|fit| (fit.entry.id, fit.start, fit.end))
        .collect()
}

/// One tab that fits in the strip, and the cells it occupies.
struct Fit<'a> {
    entry: &'a TabStripEntry,
    /// First cell of the label (after the separator, when there is one).
    start: u16,
    /// One past the last cell of the label.
    end: u16,
}

/// Lay tabs out left to right, stopping at the first one that does not fit.
///
/// A tab is dropped whole rather than truncated: half a label would be unreadable
/// and could clip mid-glyph. Drawing and hit testing both come from this one walk,
/// so the two can never disagree about which tabs exist.
fn fitting_tabs(entries: &[TabStripEntry], cols: u16) -> Vec<Fit<'_>> {
    let mut fits = Vec::with_capacity(entries.len());
    if cols == 0 {
        return fits;
    }
    let mut col: u16 = 0;
    for (index, entry) in entries.iter().enumerate() {
        let separator_width: u16 = if index > 0 { 1 } else { 0 };
        let width = label_width(entry);
        if col.saturating_add(separator_width).saturating_add(width) > cols {
            break;
        }
        col = col.saturating_add(separator_width);
        let start = col;
        col = col.saturating_add(width);
        fits.push(Fit {
            entry,
            start,
            end: col,
        });
    }
    fits
}

/// Cell width of one tab's rendered label, including its padding.
fn label_width(entry: &TabStripEntry) -> u16 {
    tab_text(entry).chars().count().min(u16::MAX as usize) as u16
}

/// Cell offset of the indicator glyph inside [`tab_text`].
const GLYPH_OFFSET: usize = 1;

/// The text of one tab: `" <glyph> label "`. The glyph cell is always reserved
/// (a space when the pane is idle) so a tab does not change width, and the labels
/// do not shift, when its status changes.
fn tab_text(entry: &TabStripEntry) -> String {
    let glyph = TabIndicator::for_entry(entry).glyph().unwrap_or(" ");
    format!(" {glyph} {} ", entry.label)
}

fn rgb_color(rgb: u32) -> Color {
    Color::Rgb(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: u32 = 0x0011_2233;

    fn entry(id: u64, label: &str) -> TabStripEntry {
        TabStripEntry {
            id: PaneId::from_raw(id),
            label: label.to_string(),
            active: false,
            busy: false,
            exited: false,
            unseen: false,
        }
    }

    fn strip_text(buffer: &Buffer) -> String {
        let mut text = String::new();
        for col in 0..buffer.area.width {
            text.push_str(buffer[(col, 0)].symbol());
        }
        text
    }

    #[test]
    fn active_tab_is_highlighted_and_inactive_is_not() {
        let mut active = entry(2, "agent");
        active.active = true;
        let inactive = entry(3, "coder");
        let buffer = tab_strip_buffer(&[active, inactive], 40, BG);

        let text = strip_text(&buffer);
        let active_start = text.find("agent").expect("active label") as u16;
        let inactive_start = text.find("coder").expect("inactive label") as u16;

        assert_eq!(
            buffer[(active_start, 0)].bg,
            Color::Indexed(7),
            "active tab paints its background"
        );
        assert_eq!(
            buffer[(inactive_start, 0)].bg,
            rgb_color(BG),
            "inactive tab keeps the pane background"
        );
    }

    #[test]
    fn indicators_follow_exited_busy_unseen_precedence() {
        let mut exited = entry(2, "a");
        exited.exited = true;
        exited.busy = true;
        exited.unseen = true;

        let mut busy = entry(3, "b");
        busy.busy = true;
        busy.unseen = true;

        let mut unseen = entry(4, "c");
        unseen.unseen = true;

        let idle = entry(5, "d");

        assert_eq!(TabIndicator::for_entry(&exited), TabIndicator::Exited);
        assert_eq!(TabIndicator::for_entry(&busy), TabIndicator::Busy);
        assert_eq!(TabIndicator::for_entry(&unseen), TabIndicator::Unseen);
        assert_eq!(TabIndicator::for_entry(&idle), TabIndicator::None);
    }

    #[test]
    fn each_indicator_draws_its_own_glyph() {
        let mut exited = entry(2, "a");
        exited.exited = true;
        let mut busy = entry(3, "b");
        busy.busy = true;
        let mut unseen = entry(4, "c");
        unseen.unseen = true;
        let idle = entry(5, "d");

        let text = strip_text(&tab_strip_buffer(&[exited, busy, unseen, idle], 40, BG));

        assert!(
            text.contains(EXITED_GLYPH),
            "exited glyph missing: {text:?}"
        );
        assert!(text.contains(BUSY_GLYPH), "busy glyph missing: {text:?}");
        assert!(
            text.contains(UNSEEN_GLYPH),
            "unseen glyph missing: {text:?}"
        );
        assert_eq!(EXITED_GLYPH, "\u{2717}");
        assert_eq!(BUSY_GLYPH, "\u{25cf}");
        assert_eq!(UNSEEN_GLYPH, "\u{2022}");
    }

    #[test]
    fn tabs_are_clipped_without_splitting_a_label() {
        // Wide enough for the first tab only; the second must be dropped whole
        // rather than leaving half a word behind.
        let buffer = tab_strip_buffer(&[entry(2, "alpha"), entry(3, "bravo")], 9, BG);
        let text = strip_text(&buffer);

        assert_eq!(buffer.area.width, 9);
        assert!(text.contains("alpha"));
        assert!(!text.contains("brav"));
    }

    #[test]
    fn a_tab_wider_than_the_remaining_space_is_dropped_whole() {
        // "alpha" occupies 9 cells (glyph slot included). Give the strip 9 + 1 (separator) + 3, which is
        // short of "bravo"'s 9 cells but long enough to start it, so a truncating
        // implementation would emit " br" here.
        let entries = [entry(2, "alpha"), entry(3, "bravo")];
        let buffer = tab_strip_buffer(&entries, 9 + 1 + 3, BG);
        let text = strip_text(&buffer);

        assert!(text.contains("alpha"), "first tab should fit: {text:?}");
        assert!(
            !text.contains("bra"),
            "a tab that does not fit must be dropped whole, got {text:?}"
        );
        // Nothing but background after the first tab.
        assert_eq!(&text[text.len() - 4..], "    ", "got {text:?}");
    }

    #[test]
    fn only_the_indicator_glyph_is_colored() {
        let mut busy = entry(2, "agent");
        busy.busy = true;
        busy.active = true;

        let buffer = tab_strip_buffer(&[busy], 40, BG);

        // Locate the glyph and assert the label cells kept the base style.
        let glyph_pos = (0..buffer.area.width)
            .find(|col| buffer[(*col, 0)].symbol() == BUSY_GLYPH)
            .expect("busy glyph rendered");
        assert_eq!(
            buffer[(glyph_pos, 0)].fg,
            TabIndicator::Busy.color(true),
            "the glyph carries the accent color"
        );

        // Every other non-space cell is label text and must not be accent-colored.
        for col in 0..buffer.area.width {
            let cell = &buffer[(col, 0)];
            if cell.symbol() == BUSY_GLYPH || cell.symbol() == " " {
                continue;
            }
            assert_eq!(
                cell.fg,
                Color::Indexed(0),
                "label cell {col} ({:?}) must keep the base foreground",
                cell.symbol()
            );
        }
    }

    #[test]
    fn inactive_tabs_color_their_glyph_too() {
        let mut unseen = entry(2, "coder");
        unseen.unseen = true;
        unseen.active = false;

        let buffer = tab_strip_buffer(&[unseen], 40, BG);
        let glyph_pos = (0..buffer.area.width)
            .find(|col| buffer[(*col, 0)].symbol() == UNSEEN_GLYPH)
            .expect("unseen glyph rendered");

        assert_eq!(
            buffer[(glyph_pos, 0)].fg,
            TabIndicator::Unseen.color(false),
            "inactive tabs still color the indicator"
        );
    }

    #[test]
    fn hit_boxes_line_up_with_rendered_labels() {
        let mut active = entry(2, "agent");
        active.busy = true;
        let entries = vec![active, entry(3, "coder"), entry(4, "reviewer")];

        let cols = 60;
        let buffer = tab_strip_buffer(&entries, cols, BG);

        for (id, start, end) in tab_strip_hit_boxes(&entries, cols) {
            // Collect the cells in this hit box, one symbol per column, so multibyte
            // glyphs like the busy dot do not shift the comparison.
            let end = end.min(buffer.area.width);
            let rendered: String = (start..end).map(|col| buffer[(col, 0)].symbol()).collect();
            let expected = entries.iter().find(|entry| entry.id == id).expect("entry");
            assert!(
                rendered.contains(expected.label.as_str()),
                "hit box {start}..{end} for {:?} rendered {rendered:?}",
                expected.label
            );
        }
    }

    #[test]
    fn a_dropped_tab_has_no_hit_box() {
        // The exact widths of `a_tab_wider_than_the_remaining_space_is_dropped_whole`:
        // "alpha" is 9 cells, then 1 of separator and 3 more, which is enough room to
        // start "bravo" but not to finish it. The second tab is therefore not drawn,
        // so a click on the empty tail must not activate it.
        let entries = [entry(2, "alpha"), entry(3, "bravo")];
        let cols = 9 + 1 + 3;

        let text = strip_text(&tab_strip_buffer(&entries, cols, BG));
        assert!(text.contains("alpha"), "first tab should fit: {text:?}");
        assert!(
            !text.contains("bra"),
            "second tab must be dropped: {text:?}"
        );

        let boxes = tab_strip_hit_boxes(&entries, cols);
        assert_eq!(
            boxes.len(),
            1,
            "only the drawn tab may be clickable, got {boxes:?}"
        );
        assert_eq!(boxes[0].0, PaneId::from_raw(2));
        assert!(
            boxes.iter().all(|(id, _, _)| *id != PaneId::from_raw(3)),
            "the dropped tab must have no hit box"
        );
        // Every clickable column is inside the strip and inside the drawn tab.
        for (_, start, end) in &boxes {
            assert!(*end <= cols, "hit box {start}..{end} exceeds {cols} cells");
        }
    }

    #[test]
    fn a_click_past_the_last_drawn_tab_hits_nothing() {
        // Same drop rule, expressed as the click the bug actually was: a column in
        // the empty tail of the strip. It must fall outside every hit box.
        let entries = [entry(2, "alpha"), entry(3, "bravo")];
        let cols = 9 + 1 + 3;
        let boxes = tab_strip_hit_boxes(&entries, cols);

        let tail_col = 11; // inside the strip, past "alpha"
        assert!(tail_col < cols, "premise: the column is inside the strip");
        assert!(
            !boxes
                .iter()
                .any(|(_, start, end)| tail_col >= *start && tail_col < *end),
            "an empty tail cell must hit no tab, boxes {boxes:?}"
        );
    }

    #[test]
    fn indicator_sits_left_of_the_label_and_idle_tabs_keep_the_slot() {
        let mut busy = entry(2, "agent");
        busy.busy = true;
        let idle = entry(3, "coder");

        let buffer = tab_strip_buffer(&[busy.clone(), idle.clone()], 40, BG);
        let text = strip_text(&buffer);
        assert!(
            text.starts_with(&format!(" {BUSY_GLYPH} agent ")),
            "glyph must precede the label: {text:?}"
        );

        // Idle shows a blank in the glyph slot, so the width does not depend on status.
        let idle_text = strip_text(&tab_strip_buffer(std::slice::from_ref(&idle), 40, BG));
        assert!(idle_text.starts_with("   coder "), "got {idle_text:?}");
        let mut busy_coder = idle.clone();
        busy_coder.busy = true;
        assert_eq!(label_width(&idle), label_width(&busy_coder));
    }

    #[test]
    fn zero_width_strip_renders_nothing() {
        let buffer = tab_strip_buffer(&[entry(2, "agent")], 0, BG);
        assert_eq!(buffer.area.width, 0);
    }
}
