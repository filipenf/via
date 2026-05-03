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
        let mut view = TerminalView::new(INITIAL_WIDTH, INITIAL_HEIGHT)?;
        let nvim_args = [
            OsString::from("--listen"),
            self.config.nvim_socket_path.clone().into_os_string(),
        ];
        let mut pty = PtySession::spawn_with_args(
            &self.config.nvim_command,
            nvim_args,
            &self.config.working_directory,
            view.size,
        )?;

        info!(size = ?view.size, "native terminal window ready");
        let mut left_mouse_down = false;

        while window.is_open() {
            let (width, height) = window.get_size();
            ensure_buffer_size(&mut buffer, width, height);

            if let Some(size) = view.resize(width, height) {
                pty.resize(size)?;
                debug!(?size, "resized terminal");
            }

            let output = pty.output().clone();
            drain_pty_output(&output, &mut view);
            forward_text_input(&input_rx, &mut pty)?;
            forward_special_keys(&window, &mut pty)?;

            view.draw(&mut font_renderer, &mut buffer, width, height);
            self.forward_file_reference_click(&window, &view, &mut left_mouse_down);
            window
                .update_with_buffer(&buffer, width, height)
                .context("failed to update native window")?;

            std::thread::sleep(Duration::from_millis(1));
        }

        Ok(())
    }

    fn forward_file_reference_click(
        &self,
        window: &Window,
        view: &TerminalView,
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

        let row = (y as usize) / CELL_HEIGHT;
        let column = (x as usize) / CELL_WIDTH;
        let Some(target) = view.file_reference_at(row, column, &self.config.working_directory)
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

struct TerminalView {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    visible_rows: Vec<String>,
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
        Some(size)
    }

    fn process(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        width: usize,
        height: usize,
    ) {
        buffer.fill(BLACK);
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
        );
    }

    fn file_reference_at(
        &self,
        row: usize,
        column: usize,
        working_directory: &Path,
    ) -> Option<FileTarget> {
        let row = self.visible_rows.get(row)?;
        file_reference_at(row, column, working_directory)
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

fn forward_text_input(input: &Receiver<char>, pty: &mut PtySession) -> Result<()> {
    for ch in input.try_iter() {
        let mut bytes = [0; 4];
        pty.write_all(ch.encode_utf8(&mut bytes).as_bytes())?;
    }

    Ok(())
}

fn forward_special_keys(window: &Window, pty: &mut PtySession) -> Result<()> {
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);

    for key in window.get_keys_pressed(KeyRepeat::Yes) {
        if ctrl {
            if let Some(bytes) = ctrl_sequence(key) {
                pty.write_all(&bytes)?;
                continue;
            }
        }

        if let Some(bytes) = key_sequence(key) {
            pty.write_all(bytes)?;
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
        let y = row as usize * CELL_HEIGHT;
        let mut row_text = String::new();
        let mut col = 0;

        while let Some(cell_ref) = cell_iter.next() {
            let x = col as usize * CELL_WIDTH;
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
                cursor.x as usize * CELL_WIDTH,
                cursor.y as usize * CELL_HEIGHT + CELL_HEIGHT - 2,
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

fn trim_file_reference(chars: &[char]) -> Option<(usize, usize, String)> {
    let mut start = 0;
    let mut end = chars.len();

    while start < end && matches!(chars[start], '"' | '\'' | '`' | '(' | '[' | '{' | '<') {
        start += 1;
    }

    while end > start
        && matches!(
            chars[end - 1],
            '"' | '\'' | '`' | ')' | ']' | '}' | '>' | ',' | ';' | '.'
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
    fn ignores_plain_words() {
        assert!(file_reference_at("no file here", 1, Path::new("/repo")).is_none());
    }
}
