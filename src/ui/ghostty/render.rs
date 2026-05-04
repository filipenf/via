use libghostty_vt::render::{CellIteration, CellIterator, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{RgbColor, StyleColor};
use libghostty_vt::Terminal;

use super::config::{color_from_palette, TerminalMetrics, TerminalTheme};
use super::font::FontRenderer;
use super::layout::PaneRect;

const ACTIVE_BORDER: u32 = 0x83a598;
const INACTIVE_BORDER: u32 = 0x3c3836;

pub(super) fn draw_pane_border(buffer: &mut [u32], width: usize, height: usize, rect: PaneRect, active: bool) {
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

pub(super) fn draw_screen(
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
    metrics: TerminalMetrics,
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
        let y = origin_y + row as usize * metrics.cell_height;
        let mut row_text = String::new();
        let mut col = 0;

        while let Some(cell_ref) = cell_iter.next() {
            let x = origin_x + col as usize * metrics.cell_width;
            let ch = draw_cell(
                cell_ref,
                font_renderer,
                buffer,
                width,
                height,
                x,
                y,
                metrics,
            );
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
                .unwrap_or(font_renderer.theme.cursor);

            draw_rect(
                buffer,
                width,
                height,
                origin_x + cursor.x as usize * metrics.cell_width,
                origin_y + cursor.y as usize * metrics.cell_height + metrics.cell_height - 2,
                metrics.cell_width,
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
    metrics: TerminalMetrics,
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

    let (fg, bg) = cell_colors(cell, &font_renderer.theme);
    let cell_width = if raw_cell
        .wide()
        .map(|wide| matches!(wide, CellWide::Wide | CellWide::SpacerHead))
        .unwrap_or(false)
    {
        metrics.cell_width * 2
    } else {
        metrics.cell_width
    };

    draw_rect(
        buffer,
        width,
        height,
        x,
        y,
        cell_width,
        metrics.cell_height,
        bg,
    );

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

pub(super) fn blend_pixel(
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

fn cell_colors(cell: &CellIteration<'static, '_>, theme: &TerminalTheme) -> (u32, u32) {
    let style = cell.style().unwrap_or_default();
    let mut fg = match style.fg_color {
        StyleColor::Palette(index) => color_from_palette(index, theme).unwrap_or(theme.foreground),
        StyleColor::Rgb(color) => rgb_color(color),
        StyleColor::None => cell
            .fg_color()
            .ok()
            .flatten()
            .map(rgb_color)
            .unwrap_or(theme.foreground),
    };
    let mut bg = match style.bg_color {
        StyleColor::Palette(index) => color_from_palette(index, theme).unwrap_or(theme.background),
        StyleColor::Rgb(color) => rgb_color(color),
        StyleColor::None => cell
            .bg_color()
            .ok()
            .flatten()
            .map(rgb_color)
            .unwrap_or(theme.background),
    };

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
