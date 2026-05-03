use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use fontdue::{Font, FontSettings, Metrics};
use libghostty_vt::render::{CellIteration, CellIterator, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::RgbColor;
use libghostty_vt::{Terminal, TerminalOptions};
use minifb::{InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window, WindowOptions};
use tracing::{debug, info};

use crate::config::Config;
use crate::event::{Event, UiEvent};
use crate::mediator::EventSender;
use crate::nvim::FileTarget;
use crate::pty::{PtySession, TerminalSize};

// This module owns the native terminal surface boundary. On Linux, Ghostty's
// exported C surface API does not currently expose a platform handle, so we use
// libghostty-vt for terminal state and keep drawing intentionally small.
const INITIAL_WIDTH: usize = 960;
const INITIAL_HEIGHT: usize = 540;
const CELL_WIDTH: usize = 10;
const CELL_HEIGHT: usize = 22;
const FONT_SIZE: f32 = 17.0;
const SCROLLBACK_ROWS: usize = 10_000;

const BLACK: u32 = 0x0c0c0c;
const WHITE: u32 = 0xd8d8d8;
const CURSOR: u32 = 0xb8bb26;
const ACTIVE_BORDER: u32 = 0x83a598;
const INACTIVE_BORDER: u32 = 0x3c3836;
const SPLIT_GAP: usize = 2;

pub struct GhosttyUi {
    config: Config,
    events: EventSender,
}

impl GhosttyUi {
    pub fn new(config: Config, events: EventSender) -> Self {
        Self { config, events }
    }

    pub fn describe_backend(&self) {
        info!(
            "native window host selected; Ghostty surface integration boundary is in ui::ghostty"
        );
    }

    pub fn run(self) -> Result<()> {
        let mut window = Window::new(
            "Spectre",
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            WindowOptions {
                resize: true,
                ..WindowOptions::default()
            },
        )
        .context("failed to create native window")?;
        window.set_target_fps(60);

        let (input_tx, input_rx) = unbounded();
        window.set_input_callback(Box::new(TextInput::new(input_tx)));

        let mut font_renderer = FontRenderer::new()?;
        let mut buffer = vec![BLACK; INITIAL_WIDTH * INITIAL_HEIGHT];
        let mut panes = self.create_panes(INITIAL_WIDTH, INITIAL_HEIGHT)?;
        let mut active_pane = 0;
        let mut layout = SplitLayout::for_window(INITIAL_WIDTH, INITIAL_HEIGHT, panes.len());
        let nvim_args = nvim_args(&self.config);

        panes[0].spawn(
            &self.config.nvim_command,
            nvim_args,
            &self.config.working_directory,
        )?;

        info!(panes = panes.len(), "native terminal window ready");
        let mut left_mouse_down = false;

        while window.is_open() {
            let (width, height) = window.get_size();
            ensure_buffer_size(&mut buffer, width, height);

            let new_layout = SplitLayout::for_window(width, height, panes.len());
            if new_layout != layout {
                layout = new_layout;

                for (index, pane) in panes.iter_mut().enumerate() {
                    let rect = layout.pane(index);
                    if let Some(size) = pane.resize(rect.width, rect.height) {
                        debug!(pane = pane.title, ?size, "resized terminal pane");
                    }
                }
            }

            for pane in &mut panes {
                pane.drain_output();
            }
            forward_text_input(&input_rx, &mut panes[active_pane])?;
            forward_special_keys(&window, &mut panes[active_pane])?;

            buffer.fill(BLACK);
            for (index, pane) in panes.iter_mut().enumerate() {
                pane.draw(
                    &mut font_renderer,
                    &mut buffer,
                    width,
                    height,
                    layout.pane(index),
                    index == active_pane,
                );
            }
            self.forward_file_reference_click(
                &window,
                &panes,
                &layout,
                &mut active_pane,
                &mut left_mouse_down,
            );
            window
                .update_with_buffer(&buffer, width, height)
                .context("failed to update native window")?;

            std::thread::sleep(Duration::from_millis(1));
        }

        Ok(())
    }

    fn create_panes(&self, width: usize, height: usize) -> Result<Vec<TerminalPane>> {
        let layout = SplitLayout::for_window(width, height, self.pane_count());
        let mut panes = vec![TerminalPane::new(
            "nvim",
            layout.pane(0).width,
            layout.pane(0).height,
        )?];

        if let Some(agent_command) = self.config.agent_command.as_deref() {
            let mut pane = TerminalPane::new("agent", layout.pane(1).width, layout.pane(1).height)?;
            pane.spawn_shell_command(agent_command, &self.config.working_directory)?;
            panes.push(pane);
        }

        Ok(panes)
    }

    fn pane_count(&self) -> usize {
        if self.config.agent_command.is_some() {
            2
        } else {
            1
        }
    }

    fn forward_file_reference_click(
        &self,
        window: &Window,
        panes: &[TerminalPane],
        layout: &SplitLayout,
        active_pane: &mut usize,
        left_mouse_down: &mut bool,
    ) {
        let is_down = window.get_mouse_down(MouseButton::Left);
        let just_pressed = is_down && !*left_mouse_down;
        *left_mouse_down = is_down;

        if !just_pressed {
            return;
        }

        let Some((x, y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) else {
            return;
        };

        let x = x as usize;
        let y = y as usize;
        let Some((pane_index, rect)) = layout.pane_at(x, y) else {
            return;
        };
        *active_pane = pane_index;

        let row = (y - rect.y) / CELL_HEIGHT;
        let column = (x - rect.x) / CELL_WIDTH;
        let Some(target) =
            panes[pane_index].file_reference_at(row, column, &self.config.working_directory)
        else {
            return;
        };

        info!(
            path = %target.path.display(),
            line = ?target.line,
            "file reference clicked"
        );
        self.events.try_send(Event::Ui(UiEvent::OpenRequested {
            path: target.path,
            line: target.line,
        }));
    }
}

struct TerminalPane {
    title: &'static str,
    view: TerminalView,
    pty: Option<PtySession>,
}

impl TerminalPane {
    fn new(title: &'static str, width: usize, height: usize) -> Result<Self> {
        Ok(Self {
            title,
            view: TerminalView::new(width, height)?,
            pty: None,
        })
    }

    fn spawn<I, S>(&mut self, command: &str, args: I, cwd: &Path) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.pty = Some(PtySession::spawn_with_args(
            command,
            args,
            cwd,
            self.view.size,
        )?);
        Ok(())
    }

    fn spawn_shell_command(&mut self, command: &str, cwd: &Path) -> Result<()> {
        self.spawn("sh", [OsString::from("-lc"), OsString::from(command)], cwd)
    }

    fn drain_output(&mut self) {
        if let Some(pty) = &self.pty {
            let output = pty.output().clone();
            drain_pty_output(&output, &mut self.view);
        }
    }

    fn resize(&mut self, width: usize, height: usize) -> Option<TerminalSize> {
        let size = self.view.resize(width, height)?;

        if let Some(pty) = &mut self.pty {
            if let Err(error) = pty.resize(size) {
                debug!(pane = self.title, %error, "failed to resize PTY");
            }
        }

        Some(size)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(pty) = &mut self.pty {
            pty.write_all(bytes)?;
        }

        Ok(())
    }

    fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        buffer_width: usize,
        buffer_height: usize,
        rect: PaneRect,
        active: bool,
    ) {
        self.view.draw(
            font_renderer,
            buffer,
            buffer_width,
            buffer_height,
            rect.x,
            rect.y,
        );
        draw_pane_border(buffer, buffer_width, buffer_height, rect, active);
    }

    fn file_reference_at(
        &self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<FileTarget> {
        self.view.file_reference_at(row, column, working_directory)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SplitLayout {
    panes: Vec<PaneRect>,
}

impl SplitLayout {
    fn for_window(width: usize, height: usize, pane_count: usize) -> Self {
        if pane_count <= 1 {
            return Self {
                panes: vec![PaneRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }],
            };
        }

        let left_width = width.saturating_sub(SPLIT_GAP) / 2;
        let right_x = left_width + SPLIT_GAP;
        let right_width = width.saturating_sub(right_x);

        Self {
            panes: vec![
                PaneRect {
                    x: 0,
                    y: 0,
                    width: left_width,
                    height,
                },
                PaneRect {
                    x: right_x,
                    y: 0,
                    width: right_width,
                    height,
                },
            ],
        }
    }

    fn pane(&self, index: usize) -> PaneRect {
        self.panes[index]
    }

    fn pane_at(&self, x: usize, y: usize) -> Option<(usize, PaneRect)> {
        self.panes.iter().copied().enumerate().find(|(_, rect)| {
            x >= rect.x
                && x < rect.x.saturating_add(rect.width)
                && y >= rect.y
                && y < rect.y.saturating_add(rect.height)
        })
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
}

impl TerminalView {
    fn new(width: usize, height: usize) -> Result<Self> {
        let size = terminal_size_for_window(width, height);
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
        })
    }

    fn resize(&mut self, width: usize, height: usize) -> Option<TerminalSize> {
        let size = terminal_size_for_window(width, height);

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

    fn process(&mut self, bytes: &[u8]) {
        self.hyperlink_tracker.process(bytes);
        self.terminal.vt_write(bytes);
    }

    fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        origin_x: usize,
        origin_y: usize,
    ) {
        self.visible_rows.clear();
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
        );
    }

    fn file_reference_at(
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

struct TextInput {
    input: Sender<char>,
}

impl TextInput {
    fn new(input: Sender<char>) -> Self {
        Self { input }
    }
}

impl InputCallback for TextInput {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(ch) = char::from_u32(uni_char) {
            let _ = self.input.send(ch);
        }
    }
}

fn drain_pty_output(output: &Receiver<Vec<u8>>, view: &mut TerminalView) {
    for chunk in output.try_iter() {
        view.process(&chunk);
    }
}

fn forward_text_input(input: &Receiver<char>, pane: &mut TerminalPane) -> Result<()> {
    for ch in input.try_iter() {
        let mut bytes = [0; 4];
        pane.write_all(ch.encode_utf8(&mut bytes).as_bytes())?;
    }

    Ok(())
}

fn forward_special_keys(window: &Window, pane: &mut TerminalPane) -> Result<()> {
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);

    for key in window.get_keys_pressed(KeyRepeat::Yes) {
        if ctrl {
            if let Some(bytes) = ctrl_sequence(key) {
                pane.write_all(&bytes)?;
                continue;
            }
        }

        if let Some(bytes) = key_sequence(key) {
            pane.write_all(bytes)?;
        }
    }

    Ok(())
}

fn ctrl_sequence(key: Key) -> Option<[u8; 1]> {
    let byte = match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        _ => return None,
    };

    Some([byte])
}

fn key_sequence(key: Key) -> Option<&'static [u8]> {
    match key {
        Key::Enter | Key::NumPadEnter => Some(b"\r"),
        Key::Backspace => Some(b"\x7f"),
        Key::Tab => Some(b"\t"),
        Key::Escape => Some(b"\x1b"),
        Key::Up => Some(b"\x1b[A"),
        Key::Down => Some(b"\x1b[B"),
        Key::Right => Some(b"\x1b[C"),
        Key::Left => Some(b"\x1b[D"),
        Key::Home => Some(b"\x1b[H"),
        Key::End => Some(b"\x1b[F"),
        Key::PageUp => Some(b"\x1b[5~"),
        Key::PageDown => Some(b"\x1b[6~"),
        Key::Insert => Some(b"\x1b[2~"),
        Key::Delete => Some(b"\x1b[3~"),
        _ => None,
    }
}

fn nvim_args(config: &Config) -> Vec<OsString> {
    vec![
        OsString::from("--listen"),
        config.nvim_socket_path.clone().into_os_string(),
        OsString::from("-c"),
        OsString::from(format!(
            "lua {}",
            nvim_context_bridge_lua(&config.editor_socket_path)
        )),
    ]
}

fn nvim_context_bridge_lua(editor_socket_path: &Path) -> String {
    format!(
        r#"do local socket={}; local uv=vim.uv or vim.loop; local pending=false; local function encode(payload) if vim.json and vim.json.encode then return vim.json.encode(payload) else return vim.fn.json_encode(payload) end end; local function notify(payload) if not socket or socket=="" or not uv then return end local pipe=uv.new_pipe(false); if not pipe then return end pipe:connect(socket,function(err) if err then pipe:close(); return end pipe:write(encode(payload).."\n",function() pipe:close() end) end) end; local function path() return vim.api.nvim_buf_get_name(0) end; local function active() local p=path(); if p=="" then return end local pos=vim.api.nvim_win_get_cursor(0); notify({{type="active_buffer_changed",path=p,line=pos[1],column=pos[2]+1}}) end; local function schedule_active() if pending then return end pending=true; vim.defer_fn(function() pending=false; active() end,75) end; local function diagnostics() local p=path(); if p=="" then return end local errors=0; local warnings=0; for _,d in ipairs(vim.diagnostic.get(0)) do if d.severity==vim.diagnostic.severity.ERROR then errors=errors+1 elseif d.severity==vim.diagnostic.severity.WARN then warnings=warnings+1 end end; notify({{type="diagnostics_changed",path=p,error_count=errors,warning_count=warnings}}) end; local group=vim.api.nvim_create_augroup("SpectreContextSync",{{clear=true}}); vim.api.nvim_create_autocmd({{"BufEnter","BufFilePost","CursorMoved","CursorMovedI"}},{{group=group,callback=schedule_active}}); vim.api.nvim_create_autocmd("DiagnosticChanged",{{group=group,callback=diagnostics}}); vim.schedule(function() active(); diagnostics() end); end"#,
        lua_string_literal(editor_socket_path)
    )
}

fn lua_string_literal(path: &Path) -> String {
    let mut quoted = String::from("\"");

    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(ch),
        }
    }

    quoted.push('"');
    quoted
}

fn terminal_size_for_window(width: usize, height: usize) -> TerminalSize {
    let cols = (width / CELL_WIDTH).max(1).min(u16::MAX as usize) as u16;
    let rows = (height / CELL_HEIGHT).max(1).min(u16::MAX as usize) as u16;

    TerminalSize {
        rows,
        cols,
        pixel_width: width.min(u16::MAX as usize) as u16,
        pixel_height: height.min(u16::MAX as usize) as u16,
    }
}

fn ensure_buffer_size(buffer: &mut Vec<u32>, width: usize, height: usize) {
    let len = width.saturating_mul(height);

    if buffer.len() != len {
        buffer.resize(len, BLACK);
    }
}

fn draw_pane_border(buffer: &mut [u32], width: usize, height: usize, rect: PaneRect, active: bool) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    let color = if active {
        ACTIVE_BORDER
    } else {
        INACTIVE_BORDER
    };

    draw_rect(buffer, width, height, rect.x, rect.y, rect.width, 1, color);
    draw_rect(
        buffer,
        width,
        height,
        rect.x,
        rect.y + rect.height.saturating_sub(1),
        rect.width,
        1,
        color,
    );
    draw_rect(buffer, width, height, rect.x, rect.y, 1, rect.height, color);
    draw_rect(
        buffer,
        width,
        height,
        rect.x + rect.width.saturating_sub(1),
        rect.y,
        1,
        rect.height,
        color,
    );
}

fn draw_screen(
    terminal: &Terminal<'static, 'static>,
    render_state: &mut RenderState<'static>,
    rows: &mut RowIterator<'static>,
    cells: &mut CellIterator<'static>,
    visible_rows: &mut Vec<String>,
    font_renderer: &mut FontRenderer,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
) {
    let Ok(snapshot) = render_state.update(terminal) else {
        return;
    };
    let cols = snapshot.cols().unwrap_or(0);
    let mut row_iter = match rows.update(&snapshot) {
        Ok(iter) => iter,
        Err(_) => return,
    };
    let mut row = 0;

    while let Some(row_ref) = row_iter.next() {
        let mut cell_iter = match cells.update(row_ref) {
            Ok(iter) => iter,
            Err(_) => return,
        };
        let y = origin_y + row as usize * CELL_HEIGHT;
        let mut row_text = String::new();
        let mut col = 0;

        while let Some(cell_ref) = cell_iter.next() {
            let x = origin_x + col as usize * CELL_WIDTH;
            let ch = draw_cell(cell_ref, font_renderer, buffer, width, height, x, y);
            row_text.push(ch.unwrap_or(' '));
            col += 1;

            if col >= cols {
                break;
            }
        }

        visible_rows.push(row_text);
        row += 1;
    }

    if snapshot.cursor_visible().unwrap_or(false) {
        if let Ok(Some(cursor)) = snapshot.cursor_viewport() {
            let cursor_color = snapshot
                .cursor_color()
                .ok()
                .flatten()
                .map(rgb_color)
                .unwrap_or(CURSOR);

            draw_rect(
                buffer,
                width,
                height,
                origin_x + cursor.x as usize * CELL_WIDTH,
                origin_y + cursor.y as usize * CELL_HEIGHT + CELL_HEIGHT - 2,
                CELL_WIDTH,
                2,
                cursor_color,
            );
        }
    }
}

fn draw_cell(
    cell: &CellIteration<'static, '_>,
    font_renderer: &mut FontRenderer,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
) -> Option<char> {
    let Ok(raw_cell) = cell.raw_cell() else {
        return None;
    };
    let is_wide_continuation = raw_cell
        .wide()
        .map(|wide| matches!(wide, CellWide::SpacerTail))
        .unwrap_or(false);

    if is_wide_continuation {
        return None;
    }

    let (fg, bg) = cell_colors(cell);
    let cell_width = if raw_cell
        .wide()
        .map(|wide| matches!(wide, CellWide::Wide | CellWide::SpacerHead))
        .unwrap_or(false)
    {
        CELL_WIDTH * 2
    } else {
        CELL_WIDTH
    };

    draw_rect(buffer, width, height, x, y, cell_width, CELL_HEIGHT, bg);

    if !raw_cell.has_text().unwrap_or(false) {
        return None;
    }

    let ch = cell
        .graphemes()
        .ok()
        .and_then(|mut graphemes| graphemes.drain(..).next())
        .unwrap_or(' ');
    font_renderer.draw_char(buffer, width, height, x, y, ch, fg);
    Some(ch)
}

struct FontRenderer {
    font: Font,
    cache: HashMap<char, GlyphBitmap>,
}

struct GlyphBitmap {
    metrics: Metrics,
    bitmap: Vec<u8>,
}

impl FontRenderer {
    fn new() -> Result<Self> {
        let font_path = font_path().context("failed to find a terminal font")?;
        let font_bytes = std::fs::read(&font_path)
            .with_context(|| format!("failed to read font {}", font_path.display()))?;
        let font = Font::from_bytes(font_bytes, FontSettings::default()).map_err(|error| {
            anyhow::anyhow!("failed to load font {}: {error}", font_path.display())
        })?;

        info!(font = %font_path.display(), "loaded terminal font");

        Ok(Self {
            font,
            cache: HashMap::new(),
        })
    }

    fn draw_char(
        &mut self,
        buffer: &mut [u32],
        width: usize,
        height: usize,
        x: usize,
        y: usize,
        ch: char,
        color: u32,
    ) {
        let glyph = self.glyph(ch);
        let baseline = y as isize + 16;
        let draw_x = x as isize + glyph.metrics.xmin as isize;
        let draw_y = baseline - glyph.metrics.ymin as isize - glyph.metrics.height as isize;

        for glyph_y in 0..glyph.metrics.height {
            for glyph_x in 0..glyph.metrics.width {
                let alpha = glyph.bitmap[glyph_y * glyph.metrics.width + glyph_x];

                if alpha == 0 {
                    continue;
                }

                blend_pixel(
                    buffer,
                    width,
                    height,
                    draw_x + glyph_x as isize,
                    draw_y + glyph_y as isize,
                    color,
                    alpha,
                );
            }
        }
    }

    fn glyph(&mut self, ch: char) -> &GlyphBitmap {
        self.cache.entry(ch).or_insert_with(|| {
            let (metrics, bitmap) = self.font.rasterize(ch, FONT_SIZE);

            GlyphBitmap { metrics, bitmap }
        })
    }
}

fn font_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SPECTRE_FONT_PATH").map(PathBuf::from) {
        return Some(path);
    }

    [
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/CaskaydiaMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    ]
    .into_iter()
    .map(Path::new)
    .find(|path| path.exists())
    .map(Path::to_path_buf)
}

fn file_reference_at(row: &str, column: usize, working_directory: &Path) -> Option<FileTarget> {
    let spans = file_reference_spans(row, working_directory);

    spans
        .into_iter()
        .find(|span| column >= span.start && column < span.end)
        .map(|span| span.target)
}

fn file_reference_spans(row: &str, working_directory: &Path) -> Vec<FileReferenceSpan> {
    let chars: Vec<char> = row.chars().collect();
    let mut spans = Vec::new();
    let mut index = 0;

    while index < chars.len() {
        while index < chars.len() && !is_file_reference_char(chars[index]) {
            index += 1;
        }

        let start = index;

        while index < chars.len() && is_file_reference_char(chars[index]) {
            index += 1;
        }

        if start == index {
            continue;
        }

        let Some((token_start, token_end, token)) = trim_file_reference(&chars[start..index])
        else {
            continue;
        };

        if !looks_like_file_reference(&token) {
            continue;
        }

        let target = FileTarget::parse(&token, working_directory);

        spans.push(FileReferenceSpan {
            start: start + token_start,
            end: start + token_end,
            target,
        });
    }

    spans
}

#[derive(Debug)]
struct FileReferenceSpan {
    start: usize,
    end: usize,
    target: FileTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkSpan {
    start: usize,
    end: usize,
    uri: String,
}

#[derive(Debug)]
struct Osc8Tracker {
    rows: Vec<Vec<LinkSpan>>,
    active_uri: Option<String>,
    size: TerminalSize,
    row: usize,
    column: usize,
    index: usize,
}

impl Osc8Tracker {
    fn new(size: TerminalSize) -> Self {
        Self {
            rows: vec![Vec::new(); size.rows as usize],
            active_uri: None,
            size,
            row: 0,
            column: 0,
            index: 0,
        }
    }

    fn resize(&mut self, size: TerminalSize) {
        self.size = size;
        self.rows.resize(size.rows as usize, Vec::new());
        self.row = self.row.min(size.rows.saturating_sub(1) as usize);
        self.column = self.column.min(size.cols.saturating_sub(1) as usize);
    }

    fn links(&self) -> &[Vec<LinkSpan>] {
        &self.rows
    }

    fn process(&mut self, bytes: &[u8]) {
        self.index = 0;

        while self.index < bytes.len() {
            match bytes[self.index] {
                b'\x1b' => self.parse_escape(bytes),
                b'\n' => self.newline(),
                b'\r' => {
                    self.column = 0;
                    self.index += 1;
                }
                byte if byte.is_ascii_control() => {
                    self.index += 1;
                }
                _ => self.print_char(bytes),
            }
        }
    }

    fn parse_escape(&mut self, bytes: &[u8]) {
        if bytes.get(self.index + 1) == Some(&b']') {
            self.parse_osc(bytes);
            return;
        }

        if bytes.get(self.index + 1) == Some(&b'[') {
            self.parse_csi(bytes);
            return;
        }

        self.index += 1;
        while self.index < bytes.len() {
            let byte = bytes[self.index];
            self.index += 1;

            if (0x40..=0x7e).contains(&byte) {
                break;
            }
        }
    }

    fn parse_csi(&mut self, bytes: &[u8]) {
        let start = self.index + 2;
        let mut cursor = start;

        while cursor < bytes.len() {
            let byte = bytes[cursor];

            if (0x40..=0x7e).contains(&byte) {
                self.apply_csi(&bytes[start..cursor], byte);
                self.index = cursor + 1;
                return;
            }

            cursor += 1;
        }

        self.index = bytes.len();
    }

    fn parse_osc(&mut self, bytes: &[u8]) {
        let payload_start = self.index + 2;
        let mut cursor = payload_start;

        while cursor < bytes.len() {
            if bytes[cursor] == b'\x07' {
                self.apply_osc(&bytes[payload_start..cursor]);
                self.index = cursor + 1;
                return;
            }

            if bytes[cursor] == b'\x1b' && bytes.get(cursor + 1) == Some(&b'\\') {
                self.apply_osc(&bytes[payload_start..cursor]);
                self.index = cursor + 2;
                return;
            }

            cursor += 1;
        }

        self.index = bytes.len();
    }

    fn apply_osc(&mut self, payload: &[u8]) {
        let Ok(payload) = std::str::from_utf8(payload) else {
            return;
        };

        let Some(rest) = payload.strip_prefix("8;") else {
            return;
        };
        let Some((_, uri)) = rest.split_once(';') else {
            return;
        };

        if uri.is_empty() {
            self.active_uri = None;
        } else {
            self.active_uri = Some(uri.to_string());
        }
    }

    fn print_char(&mut self, bytes: &[u8]) {
        let Ok(text) = std::str::from_utf8(&bytes[self.index..]) else {
            self.index += 1;
            return;
        };
        let Some(ch) = text.chars().next() else {
            self.index += 1;
            return;
        };

        self.extend_active_link();
        self.advance_column();
        self.index += ch.len_utf8();
    }

    fn newline(&mut self) {
        self.row += 1;
        if self.row >= self.size.rows as usize {
            self.rows.remove(0);
            self.rows.push(Vec::new());
            self.row = self.size.rows.saturating_sub(1) as usize;
        }
        self.column = 0;
        self.index += 1;
    }

    fn apply_csi(&mut self, params: &[u8], command: u8) {
        let params = std::str::from_utf8(params).unwrap_or_default();
        let numbers = csi_numbers(params);

        match command {
            b'A' => {
                self.row = self
                    .row
                    .saturating_sub(numbers.first().copied().unwrap_or(1))
            }
            b'B' => {
                self.row = (self.row + numbers.first().copied().unwrap_or(1))
                    .min(self.size.rows.saturating_sub(1) as usize);
            }
            b'C' => {
                self.column = (self.column + numbers.first().copied().unwrap_or(1))
                    .min(self.size.cols.saturating_sub(1) as usize);
            }
            b'D' => {
                self.column = self
                    .column
                    .saturating_sub(numbers.first().copied().unwrap_or(1))
            }
            b'H' | b'f' => {
                self.row = numbers
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.size.rows.saturating_sub(1) as usize);
                self.column = numbers
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.size.cols.saturating_sub(1) as usize);
            }
            b'J' => self.clear_screen(),
            b'K' => self.clear_current_row(),
            _ => {}
        }
    }

    fn extend_active_link(&mut self) {
        let Some(uri) = &self.active_uri else {
            return;
        };
        let Some(row) = self.rows.get_mut(self.row) else {
            return;
        };

        match row.last_mut() {
            Some(span) if span.uri == *uri && span.end == self.column => {
                span.end += 1;
            }
            _ => row.push(LinkSpan {
                start: self.column,
                end: self.column + 1,
                uri: uri.clone(),
            }),
        }
    }

    fn advance_column(&mut self) {
        self.column += 1;

        if self.column >= self.size.cols as usize {
            self.newline_without_index();
        }
    }

    fn newline_without_index(&mut self) {
        self.row += 1;
        if self.row >= self.size.rows as usize {
            self.rows.remove(0);
            self.rows.push(Vec::new());
            self.row = self.size.rows.saturating_sub(1) as usize;
        }
        self.column = 0;
    }

    fn clear_screen(&mut self) {
        self.rows.iter_mut().for_each(Vec::clear);
        self.row = 0;
        self.column = 0;
    }

    fn clear_current_row(&mut self) {
        if let Some(row) = self.rows.get_mut(self.row) {
            row.clear();
        }
    }
}

#[cfg(test)]
fn parse_vt_hyperlinks(bytes: &[u8], size: TerminalSize) -> Vec<Vec<LinkSpan>> {
    let mut parser = Osc8Tracker::new(size);

    parser.process(bytes);
    parser.rows
}

fn csi_numbers(params: &str) -> Vec<usize> {
    params
        .trim_start_matches('?')
        .split(';')
        .map(|param| param.parse::<usize>().unwrap_or(1))
        .collect()
}

fn file_target_from_uri(uri: &str, working_directory: &Path) -> Option<FileTarget> {
    let path = uri
        .strip_prefix("file://")
        .map(percent_decode)
        .or_else(|| uri.strip_prefix("file:").map(percent_decode))
        .or_else(|| {
            if uri.contains("://") {
                None
            } else {
                Some(uri.to_string())
            }
        })?;

    Some(FileTarget::parse(&path, working_directory))
}

fn percent_decode(input: &str) -> String {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;

    while index < input.len() {
        if input[index] == b'%' && index + 2 < input.len() {
            if let Ok(hex) = std::str::from_utf8(&input[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    bytes.push(byte);
                    index += 3;
                    continue;
                }
            }
        }

        bytes.push(input[index]);
        index += 1;
    }

    String::from_utf8_lossy(&bytes).into_owned()
}

fn trim_file_reference(chars: &[char]) -> Option<(usize, usize, String)> {
    let mut start = 0;
    let mut end = chars.len();

    while start < end && matches!(chars[start], '"' | '\'' | '`' | '(' | '[' | '{' | '<') {
        start += 1;
    }

    while end > start
        && matches!(
            chars[end - 1],
            '"' | '\'' | '`' | ')' | ']' | '}' | '>' | ',' | ';' | '.' | ':'
        )
    {
        end -= 1;
    }

    if start == end {
        return None;
    }

    Some((start, end, chars[start..end].iter().collect()))
}

fn looks_like_file_reference(token: &str) -> bool {
    token.contains('/')
        || token.contains('\\')
        || token.contains('.')
        || token
            .rsplit_once(':')
            .is_some_and(|(_, line)| line.parse::<u32>().is_ok())
}

fn is_file_reference_char(ch: char) -> bool {
    ch.is_alphanumeric()
        || matches!(
            ch,
            '/' | '\\'
                | '.'
                | '_'
                | '-'
                | ':'
                | '~'
                | '@'
                | '+'
                | '"'
                | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ','
                | ';'
        )
}

fn draw_rect(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
    color: u32,
) {
    let max_y = (y + rect_height).min(height);
    let max_x = (x + rect_width).min(width);

    for row in y..max_y {
        let row_start = row * width;

        for col in x..max_x {
            buffer[row_start + col] = color;
        }
    }
}

fn blend_pixel(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: isize,
    y: isize,
    color: u32,
    alpha: u8,
) {
    if x < 0 || y < 0 {
        return;
    }

    let x = x as usize;
    let y = y as usize;

    if x >= width || y >= height {
        return;
    }

    let index = y * width + x;
    let dst = buffer[index];
    let alpha = alpha as u32;
    let inv_alpha = 255 - alpha;

    let r = (((color >> 16) & 0xff) * alpha + ((dst >> 16) & 0xff) * inv_alpha) / 255;
    let g = (((color >> 8) & 0xff) * alpha + ((dst >> 8) & 0xff) * inv_alpha) / 255;
    let b = ((color & 0xff) * alpha + (dst & 0xff) * inv_alpha) / 255;

    buffer[index] = (r << 16) | (g << 8) | b;
}

fn cell_colors(cell: &CellIteration<'static, '_>) -> (u32, u32) {
    let style = cell.style().unwrap_or_default();
    let mut fg = cell
        .fg_color()
        .ok()
        .flatten()
        .map(rgb_color)
        .unwrap_or(WHITE);
    let mut bg = cell
        .bg_color()
        .ok()
        .flatten()
        .map(rgb_color)
        .unwrap_or(BLACK);

    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }

    if style.bold {
        fg = brighten(fg);
    }

    if style.faint {
        fg = dim(fg);
    }

    (fg, bg)
}

fn rgb_color(color: RgbColor) -> u32 {
    rgb(color.r, color.g, color.b)
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn brighten(color: u32) -> u32 {
    let r = (((color >> 16) & 0xff) + 40).min(255);
    let g = (((color >> 8) & 0xff) + 40).min(255);
    let b = ((color & 0xff) + 40).min(255);

    (r << 16) | (g << 8) | b
}

fn dim(color: u32) -> u32 {
    let r = ((color >> 16) & 0xff) / 2;
    let g = ((color >> 8) & 0xff) / 2;
    let b = (color & 0xff) / 2;

    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_reference_under_column() {
        let target = file_reference_at("open project.md:12 please", 7, Path::new("/repo")).unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/project.md"));
        assert_eq!(target.line, Some(12));
    }

    #[test]
    fn trims_common_surrounding_punctuation() {
        let target = file_reference_at("see `src/main.rs:8`, ok", 6, Path::new("/repo")).unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/main.rs"));
        assert_eq!(target.line, Some(8));
    }

    #[test]
    fn trims_trailing_colon_without_line_number() {
        let target =
            file_reference_at("see src/ui.rs: for details", 5, Path::new("/repo")).unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/ui.rs"));
        assert_eq!(target.line, None);
    }

    #[test]
    fn ignores_plain_words() {
        assert!(file_reference_at("no file here", 1, Path::new("/repo")).is_none());
    }

    #[test]
    fn creates_single_pane_layout_without_agent() {
        let layout = SplitLayout::for_window(100, 50, 1);

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
    }

    #[test]
    fn creates_vertical_split_layout_for_agent() {
        let layout = SplitLayout::for_window(100, 50, 2);

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 49,
                height: 50,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 51,
                y: 0,
                width: 49,
                height: 50,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
        assert_eq!(layout.pane_at(60, 10).map(|(index, _)| index), Some(1));
        assert_eq!(layout.pane_at(50, 10), None);
    }

    #[test]
    fn nvim_args_install_context_bridge() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: None,
            nvim_socket_path: PathBuf::from("/tmp/spectre-nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/spectre-editor.sock"),
            working_directory: PathBuf::from("/repo"),
        };
        let args = nvim_args(&config);
        let command = args[3].to_string_lossy();

        assert_eq!(args[0], OsString::from("--listen"));
        assert_eq!(args[1], OsString::from("/tmp/spectre-nvim.sock"));
        assert_eq!(args[2], OsString::from("-c"));
        assert!(command.contains("SpectreContextSync"));
        assert!(command.contains("/tmp/spectre-editor.sock"));
        assert!(command.contains("active_buffer_changed"));
        assert!(command.contains("diagnostics_changed"));
    }

    #[test]
    fn parses_osc8_hyperlink_spans() {
        let rows = parse_vt_hyperlinks(
            b"see \x1b]8;;file:///repo/src/main.rs:8\x1b\\main\x1b]8;;\x1b\\ now",
            test_terminal_size(),
        );

        assert_eq!(
            rows[0],
            vec![LinkSpan {
                start: 4,
                end: 8,
                uri: "file:///repo/src/main.rs:8".to_string(),
            }]
        );
    }

    #[test]
    fn parses_bel_terminated_osc8_hyperlink_spans() {
        let rows = parse_vt_hyperlinks(
            b"\x1b]8;;src/lib.rs:3\x07lib\x1b]8;;\x07",
            test_terminal_size(),
        );

        assert_eq!(
            rows[0],
            vec![LinkSpan {
                start: 0,
                end: 3,
                uri: "src/lib.rs:3".to_string(),
            }]
        );
    }

    #[test]
    fn tracks_osc8_links_after_cursor_movement() {
        let rows = parse_vt_hyperlinks(
            b"\x1b[2;5H\x1b]8;;file:///repo/src/main.rs:8\x1b\\main\x1b]8;;\x1b\\",
            test_terminal_size(),
        );

        assert_eq!(
            rows[1],
            vec![LinkSpan {
                start: 4,
                end: 8,
                uri: "file:///repo/src/main.rs:8".to_string(),
            }]
        );
    }

    #[test]
    fn converts_file_uri_to_target() {
        let target =
            file_target_from_uri("file:///repo/src/main%20file.rs:8", Path::new("/fallback"))
                .unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/main file.rs"));
        assert_eq!(target.line, Some(8));
    }

    #[test]
    fn ignores_non_file_uris() {
        assert!(
            file_target_from_uri("https://example.com/src/main.rs", Path::new("/repo")).is_none()
        );
    }

    fn test_terminal_size() -> TerminalSize {
        TerminalSize {
            rows: 5,
            cols: 40,
            pixel_width: 400,
            pixel_height: 100,
        }
    }
}
