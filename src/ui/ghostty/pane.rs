use std::cell::RefCell;
use std::ffi::OsString;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use libghostty_vt::render::{CellIteration, CellIterator, RenderState, RowIterator};
use libghostty_vt::style::RgbColor;
use libghostty_vt::terminal::{ColorScheme, Point, PointCoordinate, ScrollViewport};
use libghostty_vt::{Terminal, TerminalOptions};

use tracing::{debug, warn};

use crate::pty::{OutputNotifier, PtySession, TerminalSize};

use super::config::{TerminalMetrics, TerminalTheme};
use super::font::FontRenderer;
use super::layout::PaneRect;
use super::links::{
    Osc8Tracker, ReferenceTarget, reference_target_from_row, reference_target_from_uri,
};
use super::render::{DamageRect, SelectionRange, draw_pane_border, draw_screen};

const SCROLLBACK_ROWS: usize = 10_000;
const PTY_DRAIN_WARN_THRESHOLD: Duration = Duration::from_millis(100);

pub(super) struct TerminalPane {
    pub(super) title: &'static str,
    view: TerminalView,
    pty: Option<Rc<RefCell<PtySession>>>,
}

impl TerminalPane {
    pub(super) fn new(
        title: &'static str,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
        theme: &TerminalTheme,
    ) -> Result<Self> {
        let mut view = TerminalView::new(width, height, metrics, title == "agent")?;
        view.apply_theme(theme);
        Ok(Self {
            title,
            view,
            pty: None,
        })
    }

    pub(super) fn spawn<I, S, N>(
        &mut self,
        command: &str,
        args: I,
        cwd: &Path,
        output_notifier: N,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
        N: OutputNotifier,
    {
        let pty = Rc::new(RefCell::new(PtySession::spawn_with_args(
            command,
            args,
            cwd,
            self.view.size,
            output_notifier,
        )?));
        self.view.set_pty_response_writer(pty.clone())?;
        self.pty = Some(pty);
        Ok(())
    }

    pub(super) fn spawn_shell_command<N>(
        &mut self,
        command: &str,
        cwd: &Path,
        output_notifier: N,
    ) -> Result<()>
    where
        N: OutputNotifier,
    {
        self.spawn(
            "sh",
            [OsString::from("-lc"), OsString::from(command)],
            cwd,
            output_notifier,
        )
    }

    pub(super) fn drain_output(&mut self) -> bool {
        !self.drain_output_chunks().is_empty()
    }

    pub(super) fn drain_output_chunks(&mut self) -> Vec<Vec<u8>> {
        if let Some(pty) = &self.pty {
            let output = pty.borrow().output().clone();
            return drain_pty_output(&output, &mut self.view);
        }

        Vec::new()
    }

    pub(super) fn resize(&mut self, width: usize, height: usize) -> Option<TerminalSize> {
        let size = self.view.resize(width, height)?;

        if let Some(pty) = &self.pty {
            if let Err(error) = pty.borrow_mut().resize(size) {
                debug!(pane = self.title, %error, "failed to resize PTY");
            }
        }

        Some(size)
    }

    pub(super) fn scroll_viewport(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }

        self.view.clear_selection();
        self.view.scroll_viewport(delta);
    }

    pub(super) fn begin_selection(&mut self, row: usize, column: usize) -> bool {
        self.view.begin_selection(row, column)
    }

    pub(super) fn update_selection(&mut self, row: usize, column: usize) -> bool {
        self.view.update_selection(row, column)
    }

    pub(super) fn copy_selection_to_clipboard(&self) -> bool {
        self.view.copy_selection_to_clipboard()
    }

    pub(super) fn clear_selection(&mut self) -> bool {
        self.view.clear_selection()
    }

    pub(super) fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let result = match &self.pty {
            Some(pty) => pty.borrow_mut().write_all(bytes),
            None => Ok(()),
        };

        if let Err(error) = result {
            warn!(
                pane = self.title,
                %error,
                "terminal pane rejected input; dropping PTY session"
            );
            self.pty = None;
            self.view.clear_pty_response_writer()?;
        }

        Ok(())
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
        if rect.width == 0 || rect.height == 0 {
            return false;
        }

        let redrawn = self.view.draw(
            font_renderer,
            buffer,
            buffer_width,
            buffer_height,
            rect.x,
            rect.y,
            force_redraw,
            damage,
        );
        if redrawn || force_redraw {
            draw_pane_border(buffer, buffer_width, buffer_height, rect, active);
        }
        redrawn
    }

    pub(super) fn reference_at(
        &mut self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<ReferenceTarget> {
        self.view.reference_at(row, column, working_directory)
    }

    pub(super) fn metrics(&self) -> TerminalMetrics {
        self.view.metrics
    }

    pub(super) fn apply_theme(&mut self, theme: &TerminalTheme) {
        self.view.apply_theme(theme);
    }

    /// Visible terminal rows in the current pane (for page-scroll step size).
    pub(super) fn viewport_rows(&self) -> usize {
        self.view.size.rows as usize
    }
}

struct TerminalView {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    hyperlink_tracker: Osc8Tracker,
    osc8_enabled: bool,
    selection_anchor: Option<(usize, usize)>,
    selection_focus: Option<(usize, usize)>,
    size: TerminalSize,
    metrics: TerminalMetrics,
}

impl TerminalView {
    fn apply_theme(&mut self, theme: &TerminalTheme) {
        let _ = self
            .terminal
            .set_default_fg_color(Some(rgb_from_u32(theme.foreground)));
        let _ = self
            .terminal
            .set_default_bg_color(Some(rgb_from_u32(theme.background)));
        let _ = self
            .terminal
            .set_default_cursor_color(Some(rgb_from_u32(theme.cursor)));
        let _ = self
            .terminal
            .set_default_color_palette(Some(theme.palette.map(rgb_from_u32)));
        let scheme = if is_light_background(theme.background) {
            ColorScheme::Light
        } else {
            ColorScheme::Dark
        };
        let _ = self.terminal.on_color_scheme(move |_term| Some(scheme));
    }

    pub(super) fn new(
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
        osc8_enabled: bool,
    ) -> Result<Self> {
        let size = terminal_size_for_window(width, height, metrics);
        let terminal = Terminal::new(TerminalOptions {
            cols: size.cols,
            rows: size.rows,
            max_scrollback: SCROLLBACK_ROWS,
        })
        .context("failed to create Ghostty terminal")?;
        let render_state = RenderState::new().context("failed to create Ghostty render state")?;
        let rows = RowIterator::new().context("failed to create Ghostty row iterator")?;
        let cells = CellIterator::new().context("failed to create Ghostty cell iterator")?;

        Ok(Self {
            terminal,
            render_state,
            rows,
            cells,
            hyperlink_tracker: Osc8Tracker::new(size),
            osc8_enabled,
            selection_anchor: None,
            selection_focus: None,
            size,
            metrics,
        })
    }

    pub(super) fn resize(&mut self, width: usize, height: usize) -> Option<TerminalSize> {
        let size = terminal_size_for_window(width, height, self.metrics);

        if size == self.size {
            return None;
        }

        if let Err(error) = self.terminal.resize(
            size.cols,
            size.rows,
            size.pixel_width as u32,
            size.pixel_height as u32,
        ) {
            debug!(%error, "failed to resize Ghostty terminal state");
        }

        self.size = size;
        self.hyperlink_tracker.resize(size);
        Some(size)
    }

    fn scroll_viewport(&mut self, delta: isize) {
        self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
    }

    fn set_pty_response_writer(&mut self, pty: Rc<RefCell<PtySession>>) -> Result<()> {
        self.terminal.on_pty_write(move |_term, data| {
            let _ = pty.borrow_mut().write_all(data);
        })?;
        Ok(())
    }

    fn clear_pty_response_writer(&mut self) -> Result<()> {
        self.terminal.on_pty_write(|_term, _data| {})?;
        Ok(())
    }

    fn process(&mut self, bytes: &[u8], follow_output: bool) {
        if self.osc8_enabled {
            self.hyperlink_tracker.process(bytes);
        }
        self.terminal.vt_write(bytes);
        if follow_output {
            self.terminal.scroll_viewport(ScrollViewport::Bottom);
        }
    }

    fn begin_selection(&mut self, row: usize, column: usize) -> bool {
        let row = row.min(self.size.rows.saturating_sub(1) as usize);
        let column = column.min(self.size.cols.saturating_sub(1) as usize);
        let point = (row, column);
        let changed = self.selection_anchor != Some(point) || self.selection_focus != Some(point);
        self.selection_anchor = Some(point);
        self.selection_focus = Some(point);
        changed
    }

    fn update_selection(&mut self, row: usize, column: usize) -> bool {
        let Some(anchor) = self.selection_anchor else {
            return false;
        };

        let row = row.min(self.size.rows.saturating_sub(1) as usize);
        let column = column.min(self.size.cols.saturating_sub(1) as usize);
        let focus = (row, column);
        let changed = self.selection_focus != Some(focus) || self.selection_anchor != Some(anchor);
        self.selection_focus = Some(focus);
        changed
    }

    fn clear_selection(&mut self) -> bool {
        let had_selection = self.selection_anchor.is_some() || self.selection_focus.is_some();
        self.selection_anchor = None;
        self.selection_focus = None;
        had_selection
    }

    fn selection_range(&self) -> Option<SelectionRange> {
        let (start, end) = self.selection_bounds()?;
        Some(SelectionRange {
            start_row: start.0,
            start_col: start.1,
            end_row: end.0,
            end_col: end.1,
        })
    }

    fn selection_bounds(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.selection_anchor?;
        let focus = self.selection_focus?;

        if anchor <= focus {
            Some((anchor, focus))
        } else {
            Some((focus, anchor))
        }
    }

    fn copy_selection_to_clipboard(&self) -> bool {
        let Some(text) = self.selection_text() else {
            return false;
        };
        if text.trim().is_empty() {
            return false;
        }

        let mut clipboard = match arboard::Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(error) => {
                warn!(%error, "clipboard open failed");
                return false;
            }
        };
        if let Err(error) = clipboard.set_text(text) {
            warn!(%error, "clipboard write failed");
            return false;
        }

        true
    }

    fn selection_text(&self) -> Option<String> {
        let ((start_row, start_col), (end_row, end_col)) = self.selection_bounds()?;
        if start_row == end_row && start_col == end_col {
            return None;
        }

        let last_column = self.size.cols.saturating_sub(1) as usize;
        let mut text = String::new();

        for row in start_row..=end_row {
            let row_start = if row == start_row { start_col } else { 0 };
            let row_end = if row == end_row { end_col } else { last_column };
            let mut row_text = String::new();

            for col in row_start..=row_end {
                let point = Point::Viewport(PointCoordinate {
                    x: col as u16,
                    y: row as u32,
                });
                let grid_ref = self.terminal.grid_ref(point).ok()?;
                let cell = grid_ref.cell().ok()?;
                let ch = if cell.has_text().ok()? {
                    cell.codepoint()
                        .ok()
                        .and_then(char::from_u32)
                        .filter(|ch| *ch != '\0')
                        .unwrap_or(' ')
                } else {
                    ' '
                };
                row_text.push(ch);
            }

            text.push_str(row_text.trim_end_matches(' '));
            if row < end_row {
                text.push('\n');
            }
        }

        Some(text)
    }

    fn is_viewport_at_bottom(&self) -> bool {
        self.terminal
            .scrollbar()
            .map(|scrollbar| scrollbar.offset.saturating_add(scrollbar.len) >= scrollbar.total)
            .unwrap_or(true)
    }

    pub(super) fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        origin_x: usize,
        origin_y: usize,
        force_redraw: bool,
        damage: &mut Vec<DamageRect>,
    ) -> bool {
        let selection = self.selection_range();
        draw_screen(
            &self.terminal,
            &mut self.render_state,
            &mut self.rows,
            &mut self.cells,
            font_renderer,
            buffer,
            width,
            height,
            origin_x,
            origin_y,
            self.metrics,
            selection,
            force_redraw,
            damage,
        )
    }

    pub(super) fn reference_at(
        &mut self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<ReferenceTarget> {
        if let Some(target) = self.hyperlink_target_at(row, column, working_directory) {
            return Some(target);
        }

        let row_text = self.row_text(row)?;
        reference_target_from_row(&row_text, column, working_directory)
    }

    fn hyperlink_target_at(
        &self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<ReferenceTarget> {
        self.hyperlink_tracker
            .links()
            .get(row)?
            .iter()
            .find(|span| column >= span.start && column < span.end)
            .and_then(|span| reference_target_from_uri(&span.uri, working_directory))
    }

    fn row_text(&mut self, row: usize) -> Option<String> {
        let snapshot = self.render_state.update(&self.terminal).ok()?;
        let cols = snapshot.cols().ok()? as usize;
        let mut row_iter = self.rows.update(&snapshot).ok()?;

        let mut row_ref = None;
        for _ in 0..=row {
            row_ref = row_iter.next();
            if row_ref.is_none() {
                return None;
            }
        }

        let mut cell_iter = self.cells.update(row_ref?).ok()?;
        let mut text = String::with_capacity(cols);
        let mut col = 0usize;

        while let Some(cell_ref) = cell_iter.next() {
            if col >= cols {
                break;
            }

            text.push(first_grapheme(&cell_ref).unwrap_or(' '));
            col += 1;
        }

        if col < cols {
            text.extend(std::iter::repeat_n(' ', cols - col));
        }

        Some(text)
    }
}

fn drain_pty_output(output: &Receiver<Vec<u8>>, view: &mut TerminalView) -> Vec<Vec<u8>> {
    let started_at = Instant::now();
    let follow_output = view.is_viewport_at_bottom();
    let mut drained_chunks = Vec::new();
    let mut bytes = 0usize;

    while let Ok(chunk) = output.try_recv() {
        bytes += chunk.len();
        view.process(&chunk, false);
        drained_chunks.push(chunk);
    }

    if !drained_chunks.is_empty() && follow_output {
        view.terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    let elapsed = started_at.elapsed();
    if elapsed > PTY_DRAIN_WARN_THRESHOLD {
        warn!(
            ?elapsed,
            chunks = drained_chunks.len(),
            bytes,
            "slow PTY drain"
        );
    }

    drained_chunks
}

fn first_grapheme(cell: &CellIteration<'static, '_>) -> Option<char> {
    if cell.graphemes_len().ok()? == 0 {
        return None;
    }

    let mut graphemes = ['\0'];
    cell.graphemes_buf(&mut graphemes).ok()?;
    Some(graphemes[0])
}

fn rgb_from_u32(color: u32) -> RgbColor {
    RgbColor {
        r: ((color >> 16) & 0xff) as u8,
        g: ((color >> 8) & 0xff) as u8,
        b: (color & 0xff) as u8,
    }
}

fn is_light_background(color: u32) -> bool {
    let r = (color >> 16) & 0xff;
    let g = (color >> 8) & 0xff;
    let b = color & 0xff;

    r * 299 + g * 587 + b * 114 > 127_500
}

fn terminal_size_for_window(width: usize, height: usize, metrics: TerminalMetrics) -> TerminalSize {
    let cols = (width / metrics.cell_width).max(1).min(u16::MAX as usize) as u16;
    let rows = (height / metrics.cell_height).max(1).min(u16::MAX as usize) as u16;

    TerminalSize {
        rows,
        cols,
        pixel_width: width.min(u16::MAX as usize) as u16,
        pixel_height: height.min(u16::MAX as usize) as u16,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::nvim::FileTarget;

    #[test]
    fn output_keeps_viewport_at_bottom_when_following() {
        let mut view = test_view();

        write_numbered_lines(&mut view, 10);

        assert!(view.is_viewport_at_bottom());
    }

    #[test]
    fn output_preserves_manual_scrollback_position() {
        let mut view = test_view();
        write_numbered_lines(&mut view, 10);
        view.scroll_viewport(-2);

        assert!(!view.is_viewport_at_bottom());

        view.process(b"L11\r\n", false);

        assert!(!view.is_viewport_at_bottom());
    }

    #[test]
    fn reference_lookup_prefers_osc8_target_over_row_parsing() {
        let mut view = test_view_with_size(40, 3);

        view.process(
            b"\x1b]8;;symbol://Foo%3A%3Abar\x1b\\src/main.rs:42\x1b]8;;\x1b\\",
            true,
        );

        assert_eq!(
            view.reference_at(0, 2, Path::new("/repo")),
            Some(ReferenceTarget::Symbol("Foo::bar".to_string()))
        );
    }

    #[test]
    fn reference_lookup_falls_back_to_row_parsing_without_osc8() {
        let mut view = test_view_with_size(40, 3);

        view.process(b"open src/lib.rs:9", true);

        assert_eq!(
            view.reference_at(0, 6, Path::new("/repo")),
            Some(ReferenceTarget::File(FileTarget {
                path: PathBuf::from("/repo/src/lib.rs"),
                line: Some(9),
            }))
        );
    }

    fn test_view() -> TerminalView {
        test_view_with_size(10, 3)
    }

    fn test_view_with_size(width: usize, height: usize) -> TerminalView {
        TerminalView::new(
            width,
            height,
            TerminalMetrics {
                cell_width: 1,
                cell_height: 1,
                baseline: 0,
            },
            true,
        )
        .expect("test terminal view")
    }

    fn write_numbered_lines(view: &mut TerminalView, count: usize) {
        for index in 1..=count {
            view.process(format!("L{index:02}\r\n").as_bytes(), true);
        }
    }
}
