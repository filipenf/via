use std::cell::RefCell;
use std::ffi::OsString;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use libghostty_vt::render::{CellIteration, CellIterator, RenderState, RowIterator};
use libghostty_vt::screen::Screen;
use libghostty_vt::style::RgbColor;
use libghostty_vt::terminal::{ColorScheme, Point, PointCoordinate, ScrollViewport};
use libghostty_vt::{Terminal, TerminalOptions, key as vt_key, mouse};

use tracing::{debug, warn};

use crate::pty::{OutputNotifier, PtySession, TerminalSize};

use super::config::{TerminalMetrics, TerminalTheme};
use super::font::FontRenderer;
use super::layout::PaneRect;
use super::links::{
    Osc8Tracker, ReferenceContext, ReferenceTarget, reference_spans_from_row_ctx,
    reference_target_from_row_ctx, reference_target_from_uri,
};
use super::render::{CueSpan, DamageRect, SelectionRange, draw_pane_focus_chrome, draw_screen};

const SCROLLBACK_ROWS: usize = 10_000;
const PTY_DRAIN_WARN_THRESHOLD: Duration = Duration::from_millis(100);

pub(super) struct TerminalPane {
    pub(super) title: &'static str,
    view: TerminalView,
    pty: Option<Rc<RefCell<PtySession>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneMouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneMouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaneMouseModifiers {
    pub(super) ctrl: bool,
    pub(super) shift: bool,
    pub(super) alt: bool,
    pub(super) super_key: bool,
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
        extra_env: &[(&str, &str)],
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
            extra_env,
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
        extra_env: &[(&str, &str)],
        output_notifier: N,
    ) -> Result<()>
    where
        N: OutputNotifier,
    {
        self.spawn(
            "sh",
            [OsString::from("-lc"), OsString::from(command)],
            cwd,
            extra_env,
            output_notifier,
        )
    }

    pub(super) fn drain_output(&mut self) -> bool {
        !self.drain_output_chunks().is_empty()
    }

    pub(super) fn drain_output_chunks(&mut self) -> Vec<Vec<u8>> {
        if let Some(pty) = &self.pty {
            let output = pty.borrow().output().clone();
            return drain_pty_output(&output, &mut self.view, |response| {
                if let Err(error) = pty.borrow_mut().write_all(response) {
                    debug!(%error, "failed to write OSC color query response");
                }
            });
        }

        Vec::new()
    }

    pub(super) fn resize_with_metrics(
        &mut self,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Option<TerminalSize> {
        let size = self.view.resize_with_metrics(width, height, metrics)?;

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

    pub(super) fn scroll_viewport_to_top(&mut self) {
        self.view.clear_selection();
        self.view.scroll_viewport_to_top();
    }

    pub(super) fn scroll_viewport_to_bottom(&mut self) {
        self.view.clear_selection();
        self.view.scroll_viewport_to_bottom();
    }

    pub(super) fn forward_mouse_event(
        &mut self,
        action: PaneMouseAction,
        button: Option<PaneMouseButton>,
        x: usize,
        y: usize,
        modifiers: PaneMouseModifiers,
        any_button_pressed: bool,
    ) -> Result<bool> {
        let payload =
            self.view
                .mouse_event_payload(action, button, x, y, modifiers, any_button_pressed)?;
        if payload.is_empty() {
            return Ok(false);
        }

        self.write_all(&payload)?;
        Ok(true)
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

    pub(super) fn child_has_exited(&mut self) -> bool {
        match &self.pty {
            None => true,
            Some(pty) => pty.borrow_mut().has_exited(),
        }
    }

    pub(super) fn terminate_child(&mut self) -> Result<()> {
        self.view.clear_pty_response_writer()?;
        self.pty = None;
        Ok(())
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
        let chrome_drawn = if redrawn || force_redraw || redraw_chrome {
            draw_pane_focus_chrome(
                buffer,
                buffer_width,
                buffer_height,
                rect,
                active,
                &self.view.theme,
                redrawn || force_redraw,
                damage,
            );
            true
        } else {
            false
        };
        redrawn || chrome_drawn
    }

    pub(super) fn reference_at(
        &mut self,
        row: usize,
        column: usize,
        ctx: ReferenceContext<'_>,
    ) -> Option<ReferenceTarget> {
        self.view.reference_at(row, column, ctx)
    }

    pub(super) fn set_reference_cues_enabled(
        &mut self,
        enabled: bool,
        ctx: ReferenceContext<'_>,
    ) -> bool {
        self.view.set_reference_cues_enabled(enabled, ctx)
    }

    pub(super) fn metrics(&self) -> TerminalMetrics {
        self.view.metrics
    }

    pub(super) fn apply_theme(&mut self, theme: &TerminalTheme) {
        self.view.apply_theme(theme);
    }

    #[cfg(test)]
    pub(super) fn process_for_test(&mut self, bytes: &[u8], follow_output: bool) {
        self.view.process(bytes, follow_output);
    }

    /// Visible terminal rows in the current pane (for page-scroll step size).
    pub(super) fn viewport_rows(&self) -> usize {
        self.view.size.rows as usize
    }

    pub(super) fn is_viewport_at_bottom(&self) -> bool {
        self.view.is_viewport_at_bottom()
    }

    pub(super) fn is_alt_screen(&self) -> bool {
        self.view.is_alt_screen()
    }
}

struct TerminalView {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    hyperlink_tracker: Osc8Tracker,
    color_query_responder: OscColorQueryResponder,
    osc8_enabled: bool,
    selection_anchor: Option<(usize, usize)>,
    selection_focus: Option<(usize, usize)>,
    cue_spans: Vec<CueSpan>,
    size: TerminalSize,
    metrics: TerminalMetrics,
    theme: TerminalTheme,
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
        self.theme = theme.clone();
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
            color_query_responder: OscColorQueryResponder::default(),
            osc8_enabled,
            selection_anchor: None,
            selection_focus: None,
            cue_spans: Vec::new(),
            size,
            metrics,
            theme: TerminalTheme::default(),
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

    fn resize_with_metrics(
        &mut self,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Option<TerminalSize> {
        self.metrics = metrics;
        self.resize(width, height)
    }

    fn scroll_viewport(&mut self, delta: isize) {
        self.terminal.scroll_viewport(ScrollViewport::Delta(delta));
    }

    fn scroll_viewport_to_top(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Top);
    }

    fn scroll_viewport_to_bottom(&mut self) {
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
    }

    fn mouse_event_payload(
        &mut self,
        action: PaneMouseAction,
        button: Option<PaneMouseButton>,
        x: usize,
        y: usize,
        modifiers: PaneMouseModifiers,
        any_button_pressed: bool,
    ) -> Result<Vec<u8>> {
        let mut encoder = mouse::Encoder::new().context("failed to create mouse encoder")?;
        encoder
            .set_options_from_terminal(&self.terminal)
            .set_size(mouse::EncoderSize {
                screen_width: self.size.pixel_width as u32,
                screen_height: self.size.pixel_height as u32,
                cell_width: self.metrics.cell_width as u32,
                cell_height: self.metrics.cell_height as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_any_button_pressed(any_button_pressed);

        let mut event = mouse::Event::new().context("failed to create mouse event")?;
        event
            .set_action(match action {
                PaneMouseAction::Press => mouse::Action::Press,
                PaneMouseAction::Release => mouse::Action::Release,
                PaneMouseAction::Motion => mouse::Action::Motion,
            })
            .set_button(button.map(|button| match button {
                PaneMouseButton::Left => mouse::Button::Left,
                PaneMouseButton::Middle => mouse::Button::Middle,
                PaneMouseButton::Right => mouse::Button::Right,
                PaneMouseButton::WheelUp => mouse::Button::Four,
                PaneMouseButton::WheelDown => mouse::Button::Five,
            }))
            .set_mods(mouse_modifiers(modifiers))
            .set_position(mouse::Position {
                x: x.min(self.size.pixel_width.saturating_sub(1) as usize) as f32,
                y: y.min(self.size.pixel_height.saturating_sub(1) as usize) as f32,
            });

        let mut payload = Vec::with_capacity(32);
        encoder
            .encode_to_vec(&event, &mut payload)
            .context("failed to encode mouse event")?;
        Ok(payload)
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

    fn set_reference_cues_enabled(&mut self, enabled: bool, ctx: ReferenceContext<'_>) -> bool {
        let spans = if enabled {
            self.visible_reference_spans(ctx)
        } else {
            Vec::new()
        };
        if self.cue_spans == spans {
            return false;
        }
        self.cue_spans = spans;
        true
    }

    fn visible_reference_spans(&mut self, ctx: ReferenceContext<'_>) -> Vec<CueSpan> {
        let mut spans = Vec::new();
        for row in 0..self.size.rows as usize {
            for span in self
                .hyperlink_tracker
                .links()
                .get(row)
                .into_iter()
                .flatten()
            {
                if reference_target_from_uri(&span.uri, ctx.working_directory).is_some() {
                    spans.push(CueSpan {
                        row,
                        start_col: span.start,
                        end_col: span.end,
                    });
                }
            }

            let Some(row_text) = self.row_text(row) else {
                continue;
            };
            for span in reference_spans_from_row_ctx(&row_text, ctx) {
                let cue = CueSpan {
                    row,
                    start_col: span.start,
                    end_col: span.end,
                };
                if !spans.iter().any(|existing| contains_span(*existing, cue)) {
                    spans.retain(|existing| !contains_span(cue, *existing));
                    spans.push(cue);
                }
            }
        }
        spans.sort_by_key(|span| (span.row, span.start_col, span.end_col));
        spans.dedup();
        spans
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

    fn is_alt_screen(&self) -> bool {
        self.terminal
            .active_screen()
            .map(|screen| screen == Screen::Alternate)
            .unwrap_or(false)
    }

    #[allow(clippy::too_many_arguments)]
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
            &self.cue_spans,
            force_redraw,
            damage,
        )
    }

    pub(super) fn reference_at(
        &mut self,
        row: usize,
        column: usize,
        ctx: ReferenceContext<'_>,
    ) -> Option<ReferenceTarget> {
        if let Some(target) = self.hyperlink_target_at(row, column, ctx.working_directory) {
            return Some(target);
        }

        let row_text = self.row_text(row)?;
        reference_target_from_row_ctx(&row_text, column, ctx)
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
            row_ref?;
        }

        let mut cell_iter = self.cells.update(row_ref?).ok()?;
        let mut text = String::with_capacity(cols);
        let mut col = 0usize;

        while let Some(cell_ref) = cell_iter.next() {
            if col >= cols {
                break;
            }

            text.push(first_grapheme(cell_ref).unwrap_or(' '));
            col += 1;
        }

        if col < cols {
            text.extend(std::iter::repeat_n(' ', cols - col));
        }

        Some(text)
    }
}

fn contains_span(container: CueSpan, contained: CueSpan) -> bool {
    container.row == contained.row
        && container.start_col <= contained.start_col
        && container.end_col >= contained.end_col
}

fn drain_pty_output(
    output: &Receiver<Vec<u8>>,
    view: &mut TerminalView,
    mut write_response: impl FnMut(&[u8]),
) -> Vec<Vec<u8>> {
    let started_at = Instant::now();
    let follow_output = view.is_viewport_at_bottom();
    let mut drained_chunks = Vec::new();
    let mut bytes = 0usize;

    while let Ok(chunk) = output.try_recv() {
        bytes += chunk.len();
        view.color_query_responder
            .process(&chunk, &view.theme, &mut write_response);
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

fn mouse_modifiers(modifiers: PaneMouseModifiers) -> vt_key::Mods {
    let mut mods = vt_key::Mods::empty();
    if modifiers.ctrl {
        mods |= vt_key::Mods::CTRL;
    }
    if modifiers.shift {
        mods |= vt_key::Mods::SHIFT;
    }
    if modifiers.alt {
        mods |= vt_key::Mods::ALT;
    }
    if modifiers.super_key {
        mods |= vt_key::Mods::SUPER;
    }
    mods
}

#[derive(Default)]
struct OscColorQueryResponder {
    state: OscColorQueryState,
}

#[derive(Default)]
enum OscColorQueryState {
    #[default]
    Ground,
    Escape,
    Osc {
        payload: Vec<u8>,
        pending_escape: bool,
    },
}

#[derive(Clone, Copy)]
enum OscTerminator {
    Bell,
    St,
}

impl OscColorQueryResponder {
    fn process(
        &mut self,
        bytes: &[u8],
        theme: &TerminalTheme,
        mut write_response: impl FnMut(&[u8]),
    ) {
        const MAX_OSC_BYTES: usize = 4096;

        for &byte in bytes {
            match &mut self.state {
                OscColorQueryState::Ground => {
                    if byte == b'\x1b' {
                        self.state = OscColorQueryState::Escape;
                    }
                }
                OscColorQueryState::Escape => {
                    self.state = if byte == b']' {
                        OscColorQueryState::Osc {
                            payload: Vec::new(),
                            pending_escape: false,
                        }
                    } else if byte == b'\x1b' {
                        OscColorQueryState::Escape
                    } else {
                        OscColorQueryState::Ground
                    };
                }
                OscColorQueryState::Osc {
                    payload,
                    pending_escape,
                } => {
                    if *pending_escape {
                        *pending_escape = false;
                        if byte == b'\\' {
                            if let Some(response) =
                                osc_color_query_response(payload, OscTerminator::St, theme)
                            {
                                write_response(&response);
                            }
                            self.state = OscColorQueryState::Ground;
                            continue;
                        }

                        payload.push(b'\x1b');
                    }

                    match byte {
                        b'\x07' => {
                            if let Some(response) =
                                osc_color_query_response(payload, OscTerminator::Bell, theme)
                            {
                                write_response(&response);
                            }
                            self.state = OscColorQueryState::Ground;
                        }
                        b'\x1b' => *pending_escape = true,
                        _ if payload.len() < MAX_OSC_BYTES => payload.push(byte),
                        _ => self.state = OscColorQueryState::Ground,
                    }
                }
            }
        }
    }
}

fn osc_color_query_response(
    payload: &[u8],
    terminator: OscTerminator,
    theme: &TerminalTheme,
) -> Option<Vec<u8>> {
    let payload = std::str::from_utf8(payload).ok()?;
    let (target, query) = payload.split_once(';')?;
    if query != "?" {
        return None;
    }

    let color = match target {
        "10" => theme.foreground,
        "11" => theme.background,
        "12" => theme.cursor,
        _ => return None,
    };

    let r = ((color >> 16) & 0xff) * 257;
    let g = ((color >> 8) & 0xff) * 257;
    let b = (color & 0xff) * 257;
    let mut response = format!("\x1b]{target};rgb:{r:04x}/{g:04x}/{b:04x}").into_bytes();
    response.extend_from_slice(terminator.bytes());
    Some(response)
}

impl OscTerminator {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Bell => b"\x07",
            Self::St => b"\x1b\\",
        }
    }
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
            view.reference_at(0, 2, ReferenceContext::cwd_only(Path::new("/repo"))),
            Some(ReferenceTarget::Symbol("Foo::bar".to_string()))
        );
    }

    #[test]
    fn reference_lookup_falls_back_to_row_parsing_without_osc8() {
        let mut view = test_view_with_size(40, 3);

        view.process(b"open src/lib.rs:9", true);

        assert_eq!(
            view.reference_at(0, 6, ReferenceContext::cwd_only(Path::new("/repo"))),
            Some(ReferenceTarget::File(FileTarget {
                path: PathBuf::from("/repo/src/lib.rs"),
                line: Some(9),
            }))
        );
    }

    #[test]
    fn reference_lookup_returns_http_url_from_row_text() {
        let mut view = test_view_with_size(80, 3);

        view.process(b"see https://example.com/path for info", true);

        let col = "see https://example.com/path for info"
            .find("example")
            .unwrap();

        assert_eq!(
            view.reference_at(0, col, ReferenceContext::cwd_only(Path::new("/repo"))),
            Some(ReferenceTarget::Url("https://example.com/path".to_string()))
        );
    }

    #[test]
    fn reference_lookup_returns_http_osc8_url() {
        let mut view = test_view_with_size(40, 3);

        view.process(
            b"\x1b]8;;https://example.com/path\x1b\\link\x1b]8;;\x1b\\",
            true,
        );

        assert_eq!(
            view.reference_at(0, 0, ReferenceContext::cwd_only(Path::new("/repo"))),
            Some(ReferenceTarget::Url("https://example.com/path".to_string()))
        );
    }

    #[test]
    fn ctrl_cues_include_osc8_and_fallback_reference_spans() {
        let mut view = test_view_with_size(40, 3);

        view.process(
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ src/lib.rs:9",
            true,
        );

        assert!(
            view.set_reference_cues_enabled(true, ReferenceContext::cwd_only(Path::new("/repo")))
        );
        assert_eq!(
            view.cue_spans,
            vec![
                CueSpan {
                    row: 0,
                    start_col: 0,
                    end_col: 4,
                },
                CueSpan {
                    row: 0,
                    start_col: 5,
                    end_col: 17,
                },
            ]
        );
        assert!(
            view.set_reference_cues_enabled(false, ReferenceContext::cwd_only(Path::new("/repo")))
        );
        assert!(view.cue_spans.is_empty());
    }

    #[test]
    fn osc_color_query_responder_answers_background_query() {
        let mut responder = OscColorQueryResponder::default();
        let theme = test_theme();
        let mut responses = Vec::new();

        responder.process(b"\x1b]11;?\x07", &theme, |response| {
            responses.push(response.to_vec());
        });

        assert_eq!(responses, [b"\x1b]11;rgb:eeee/ffff/dddd\x07".to_vec()]);
    }

    #[test]
    fn osc_color_query_responder_handles_split_st_query() {
        let mut responder = OscColorQueryResponder::default();
        let theme = test_theme();
        let mut responses = Vec::new();

        responder.process(b"\x1b]10", &theme, |response| {
            responses.push(response.to_vec());
        });
        responder.process(b";?\x1b\\", &theme, |response| {
            responses.push(response.to_vec());
        });

        assert_eq!(responses, [b"\x1b]10;rgb:1111/2222/3333\x1b\\".to_vec()]);
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

    fn test_theme() -> TerminalTheme {
        TerminalTheme {
            foreground: 0x112233,
            background: 0xeeffdd,
            cursor: 0x445566,
            palette: [0; 256],
        }
    }
}
