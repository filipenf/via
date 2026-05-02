use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender, unbounded};
use font8x8::{BASIC_FONTS, UnicodeFonts};
use minifb::{InputCallback, Key, KeyRepeat, Window, WindowOptions};
use tracing::{debug, info};
use vt100::{Cell, Color, Parser, Screen};

use crate::config::Config;
use crate::pty::{PtySession, TerminalSize};

// This module owns the native terminal surface boundary. The current renderer is
// intentionally small so the PTY/window path works before full libghostty surface
// bindings are wired in.
const INITIAL_WIDTH: usize = 960;
const INITIAL_HEIGHT: usize = 540;
const CELL_WIDTH: usize = 8;
const CELL_HEIGHT: usize = 16;
const FONT_SCALE_Y: usize = 2;
const SCROLLBACK_ROWS: usize = 10_000;

const BLACK: u32 = 0x0c0c0c;
const WHITE: u32 = 0xd8d8d8;
const CURSOR: u32 = 0xb8bb26;

pub struct GhosttyUi {
    config: Config,
}

impl GhosttyUi {
    pub fn new(config: Config) -> Self {
        Self { config }
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

        let mut buffer = vec![BLACK; INITIAL_WIDTH * INITIAL_HEIGHT];
        let mut view = TerminalView::new(INITIAL_WIDTH, INITIAL_HEIGHT);
        let mut pty = PtySession::spawn(&self.config.nvim_command, view.size)?;

        info!(size = ?view.size, "native terminal window ready");

        while window.is_open() {
            let (width, height) = window.get_size();
            ensure_buffer_size(&mut buffer, width, height);

            if let Some(size) = view.resize(width, height) {
                pty.resize(size)?;
                debug!(?size, "resized terminal");
            }

            drain_pty_output(pty.output(), &mut view.parser);
            forward_text_input(&input_rx, &mut pty)?;
            forward_special_keys(&window, &mut pty)?;

            view.draw(&mut buffer, width, height);
            window
                .update_with_buffer(&buffer, width, height)
                .context("failed to update native window")?;

            std::thread::sleep(Duration::from_millis(1));
        }

        Ok(())
    }
}

struct TerminalView {
    parser: Parser,
    size: TerminalSize,
}

impl TerminalView {
    fn new(width: usize, height: usize) -> Self {
        let size = terminal_size_for_window(width, height);
        let parser = Parser::new(size.rows, size.cols, SCROLLBACK_ROWS);

        Self { parser, size }
    }

    fn resize(&mut self, width: usize, height: usize) -> Option<TerminalSize> {
        let size = terminal_size_for_window(width, height);

        if size == self.size {
            return None;
        }

        self.parser.screen_mut().set_size(size.rows, size.cols);
        self.size = size;
        Some(size)
    }

    fn draw(&self, buffer: &mut [u32], width: usize, height: usize) {
        buffer.fill(BLACK);
        draw_screen(self.parser.screen(), buffer, width, height);
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

fn drain_pty_output(output: &Receiver<Vec<u8>>, parser: &mut Parser) {
    for chunk in output.try_iter() {
        parser.process(&chunk);
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

fn draw_screen(screen: &Screen, buffer: &mut [u32], width: usize, height: usize) {
    let (rows, cols) = screen.size();

    for row in 0..rows {
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };

            let x = col as usize * CELL_WIDTH;
            let y = row as usize * CELL_HEIGHT;
            draw_cell(cell, buffer, width, height, x, y);
        }
    }

    if !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        draw_rect(
            buffer,
            width,
            height,
            col as usize * CELL_WIDTH,
            row as usize * CELL_HEIGHT + CELL_HEIGHT - 2,
            CELL_WIDTH,
            2,
            CURSOR,
        );
    }
}

fn draw_cell(cell: &Cell, buffer: &mut [u32], width: usize, height: usize, x: usize, y: usize) {
    let (fg, bg) = cell_colors(cell);
    draw_rect(buffer, width, height, x, y, CELL_WIDTH, CELL_HEIGHT, bg);

    if !cell.has_contents() || cell.is_wide_continuation() {
        return;
    }

    let ch = cell.contents().chars().next().unwrap_or(' ');
    draw_char(buffer, width, height, x, y, ch, fg);
}

fn draw_char(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    ch: char,
    color: u32,
) {
    let glyph = BASIC_FONTS.get(ch).or_else(|| BASIC_FONTS.get('?'));

    let Some(glyph) = glyph else {
        return;
    };

    for (glyph_y, row) in glyph.iter().enumerate() {
        for glyph_x in 0..8 {
            if (row >> glyph_x) & 1 == 0 {
                continue;
            }

            draw_rect(
                buffer,
                width,
                height,
                x + glyph_x,
                y + glyph_y * FONT_SCALE_Y,
                1,
                FONT_SCALE_Y,
                color,
            );
        }
    }
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

fn cell_colors(cell: &Cell) -> (u32, u32) {
    let mut fg = terminal_color(cell.fgcolor(), WHITE);
    let mut bg = terminal_color(cell.bgcolor(), BLACK);

    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }

    if cell.bold() {
        fg = brighten(fg);
    }

    if cell.dim() {
        fg = dim(fg);
    }

    (fg, bg)
}

fn terminal_color(color: Color, default: u32) -> u32 {
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => rgb(r, g, b),
        Color::Idx(index) => ANSI_COLORS.get(index as usize).copied().unwrap_or(default),
    }
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

const ANSI_COLORS: [u32; 16] = [
    0x1d2021, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, 0x928374,
    0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
];
