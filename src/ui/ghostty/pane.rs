use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use libghostty_vt::render::{CellIterator, RenderState, RowIterator};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{Terminal, TerminalOptions};

use tracing::{debug, warn};

use crate::nvim::FileTarget;
use crate::pty::{OutputNotifier, PtySession, TerminalSize};

use super::config::TerminalMetrics;
use super::font::FontRenderer;
use super::layout::PaneRect;
use super::links::{Osc8Tracker, file_reference_at, file_target_from_uri};
use super::render::{DamageRect, draw_pane_border, draw_screen};

const SCROLLBACK_ROWS: usize = 10_000;
const PTY_DRAIN_WARN_THRESHOLD: Duration = Duration::from_millis(20);
const PTY_DRAIN_BUDGET: Duration = Duration::from_millis(8);
const PTY_DRAIN_MIN_CHUNKS: usize = 2;

pub(super) struct TerminalPane {
    pub(super) title: &'static str,
    view: TerminalView,
    pty: Option<PtySession>,
}

impl TerminalPane {
    pub(super) fn new(
        title: &'static str,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Result<Self> {
        Ok(Self {
            title,
            view: TerminalView::new(width, height, metrics)?,
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
        self.pty = Some(PtySession::spawn_with_args(
            command,
            args,
            cwd,
            self.view.size,
            output_notifier,
        )?);
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
        if let Some(pty) = &self.pty {
            let output = pty.output().clone();
            return drain_pty_output(&output, &mut self.view);
        }

        false
    }

    pub(super) fn resize(&mut self, width: usize, height: usize) -> Option<TerminalSize> {
        let size = self.view.resize(width, height)?;

        if let Some(pty) = &mut self.pty {
            if let Err(error) = pty.resize(size) {
                debug!(pane = self.title, %error, "failed to resize PTY");
            }
        }

        Some(size)
    }

    pub(super) fn scroll_viewport(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }

        self.view.scroll_viewport(delta);
    }

    pub(super) fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let result = match &mut self.pty {
            Some(pty) => pty.write_all(bytes),
            None => Ok(()),
        };

        if let Err(error) = result {
            warn!(
                pane = self.title,
                %error,
                "terminal pane rejected input; dropping PTY session"
            );
            self.pty = None;
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

    pub(super) fn file_reference_at(
        &self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<FileTarget> {
        self.view.file_reference_at(row, column, working_directory)
    }

    pub(super) fn metrics(&self) -> TerminalMetrics {
        self.view.metrics
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
    visible_rows: Vec<String>,
    hyperlink_tracker: Osc8Tracker,
    size: TerminalSize,
    metrics: TerminalMetrics,
}

impl TerminalView {
    pub(super) fn new(width: usize, height: usize, metrics: TerminalMetrics) -> Result<Self> {
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
            visible_rows: Vec::new(),
            hyperlink_tracker: Osc8Tracker::new(size),
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

    fn process(&mut self, bytes: &[u8], follow_output: bool) {
        self.hyperlink_tracker.process(bytes);
        self.terminal.vt_write(bytes);
        if follow_output {
            self.terminal.scroll_viewport(ScrollViewport::Bottom);
        }
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
        draw_screen(
            &self.terminal,
            &mut self.render_state,
            &mut self.rows,
            &mut self.cells,
            &mut self.visible_rows,
            font_renderer,
            buffer,
            width,
            height,
            origin_x,
            origin_y,
            self.metrics,
            force_redraw,
            damage,
        )
    }

    pub(super) fn file_reference_at(
        &self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<FileTarget> {
        if let Some(target) = self.hyperlink_target_at(row, column, working_directory) {
            return Some(target);
        }

        let row = self.visible_rows.get(row)?;
        file_reference_at(row, column, working_directory)
    }

    fn hyperlink_target_at(
        &self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<FileTarget> {
        self.hyperlink_tracker
            .links()
            .get(row)?
            .iter()
            .find(|span| column >= span.start && column < span.end)
            .and_then(|span| file_target_from_uri(&span.uri, working_directory))
    }
}

fn drain_pty_output(output: &Receiver<Vec<u8>>, view: &mut TerminalView) -> bool {
    let started_at = Instant::now();
    let follow_output = view.is_viewport_at_bottom();
    let mut had_output = false;
    let mut chunks = 0usize;
    let mut bytes = 0usize;

    while let Ok(chunk) = output.try_recv() {
        had_output = true;
        chunks += 1;
        bytes += chunk.len();
        view.process(&chunk, false);

        if chunks >= PTY_DRAIN_MIN_CHUNKS && started_at.elapsed() >= PTY_DRAIN_BUDGET {
            break;
        }
    }

    if had_output && follow_output {
        view.terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    let elapsed = started_at.elapsed();
    if elapsed > PTY_DRAIN_WARN_THRESHOLD {
        warn!(?elapsed, chunks, bytes, "slow PTY drain");
    }

    had_output
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
    use super::*;

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

    fn test_view() -> TerminalView {
        TerminalView::new(
            10,
            3,
            TerminalMetrics {
                cell_width: 1,
                cell_height: 1,
                baseline: 0,
            },
        )
        .expect("test terminal view")
    }

    fn write_numbered_lines(view: &mut TerminalView, count: usize) {
        for index in 1..=count {
            view.process(format!("L{index:02}\r\n").as_bytes(), true);
        }
    }
}
