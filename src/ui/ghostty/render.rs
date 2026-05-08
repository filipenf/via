use std::num::NonZeroU32;

use libghostty_vt::Terminal;
use libghostty_vt::render::{CellIteration, CellIterator, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{RgbColor, StyleColor};

use super::config::{TerminalMetrics, TerminalTheme, color_from_palette};
use super::font::FontRenderer;
use super::layout::PaneRect;

const ACTIVE_BORDER: u32 = 0x83a598;
const INACTIVE_BORDER: u32 = 0x3c3836;
const SELECTION_BACKGROUND: u32 = 0x4f6480;
const SELECTION_FOREGROUND: u32 = 0xfbf1c7;

#[derive(Debug, Clone, Copy)]
pub(super) struct DamageRect {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) width: usize,
    pub(super) height: usize,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SelectionRange {
    pub(super) start_row: usize,
    pub(super) start_col: usize,
    pub(super) end_row: usize,
    pub(super) end_col: usize,
}

pub(super) fn draw_pane_border(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    rect: PaneRect,
    active: bool,
) {
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
    font_renderer: &mut FontRenderer,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    metrics: TerminalMetrics,
    selection: Option<SelectionRange>,
    force_redraw: bool,
    damage: &mut Vec<DamageRect>,
) -> bool {
    let Ok(snapshot) = render_state.update(terminal) else {
        return false;
    };
    let dirty = snapshot.dirty().unwrap_or(Dirty::Full);
    if dirty == Dirty::Clean && !force_redraw {
        return false;
    }

    let cols = snapshot.cols().unwrap_or(0);
    let colors = snapshot.colors().ok();
    let default_fg = colors
        .as_ref()
        .map(|colors| rgb_color(colors.foreground))
        .unwrap_or(font_renderer.theme.foreground);
    let default_bg = colors
        .as_ref()
        .map(|colors| rgb_color(colors.background))
        .unwrap_or(font_renderer.theme.background);
    let mut row_iter = match rows.update(&snapshot) {
        Ok(iter) => iter,
        Err(_) => return false,
    };
    let mut row = 0usize;
    // Only force all rows when explicitly requested (e.g., resize, font change).
    // For content updates (including scroll), rely on per-row dirty tracking
    // to avoid redrawing unchanged rows.
    let redraw_all_rows = force_redraw;

    while let Some(row_ref) = row_iter.next() {
        let row_dirty = redraw_all_rows || row_ref.dirty().unwrap_or(true);
        if row_dirty {
            let row_y = origin_y + row * metrics.cell_height;
            let row_width = cols as usize * metrics.cell_width;
            draw_rect(
                buffer,
                width,
                height,
                origin_x,
                row_y,
                row_width,
                metrics.cell_height,
                default_bg,
            );
            push_damage(
                damage,
                origin_x,
                row_y,
                row_width,
                metrics.cell_height,
                width,
                height,
            );
        } else {
            row += 1;
            continue;
        }

        let mut cell_iter = match cells.update(row_ref) {
            Ok(iter) => iter,
            Err(_) => return false,
        };
        let y = origin_y + row * metrics.cell_height;
        let mut col = 0;

        while let Some(cell_ref) = cell_iter.next() {
            let x = origin_x + col as usize * metrics.cell_width;
            if row_dirty {
                draw_cell(
                    cell_ref,
                    font_renderer,
                    buffer,
                    width,
                    height,
                    x,
                    y,
                    metrics,
                    is_selected_cell(row, col as usize, selection),
                    default_fg,
                    default_bg,
                );
            }
            col += 1;

            if col >= cols {
                break;
            }
        }

        if row_dirty {
            let _ = row_ref.set_dirty(false);
        }
        row += 1;
    }

    // no longer maintaining visible_rows on every frame; rebuilt lazily on click if needed

    // Coalesce vertically adjacent full-width row rects (common case for terminal rows)
    // to drastically reduce damage rect count passed to softbuffer present.
    coalesce_damage(damage);

    if snapshot.cursor_visible().unwrap_or(false) {
        if let Ok(Some(cursor)) = snapshot.cursor_viewport() {
            let cursor_x = origin_x + cursor.x as usize * metrics.cell_width;
            let cursor_y =
                origin_y + cursor.y as usize * metrics.cell_height + metrics.cell_height - 2;
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
                cursor_x,
                cursor_y,
                metrics.cell_width,
                2,
                cursor_color,
            );
            push_damage(
                damage,
                cursor_x,
                cursor_y,
                metrics.cell_width,
                2,
                width,
                height,
            );
        }
    }

    let _ = snapshot.set_dirty(Dirty::Clean);
    true
}

fn coalesce_damage(damage: &mut Vec<DamageRect>) {
    if damage.len() < 2 {
        return;
    }
    let mut write = 0;
    for read in 1..damage.len() {
        let prev = &damage[write];
        let curr = &damage[read];
        if curr.x == prev.x && curr.width == prev.width && curr.y == prev.y + prev.height {
            // extend previous
            damage[write].height += curr.height;
        } else {
            write += 1;
            if write != read {
                damage[write] = *curr;
            }
        }
    }
    damage.truncate(write + 1);
}

pub(super) fn softbuffer_rects(damage: &[DamageRect]) -> Vec<softbuffer::Rect> {
    damage
        .iter()
        .filter_map(|rect| {
            Some(softbuffer::Rect {
                x: rect.x.try_into().ok()?,
                y: rect.y.try_into().ok()?,
                width: NonZeroU32::new(rect.width.try_into().ok()?)?,
                height: NonZeroU32::new(rect.height.try_into().ok()?)?,
            })
        })
        .collect()
}

fn push_damage(
    damage: &mut Vec<DamageRect>,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
    width: usize,
    height: usize,
) {
    let max_y = (y + rect_height).min(height);
    let max_x = (x + rect_width).min(width);

    if x >= max_x || y >= max_y {
        return;
    }

    damage.push(DamageRect {
        x,
        y,
        width: max_x - x,
        height: max_y - y,
    });
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
    selected: bool,
    default_fg: u32,
    default_bg: u32,
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

    let (mut fg, mut bg) = cell_colors(cell, &font_renderer.theme, default_fg, default_bg);
    let cell_width = if raw_cell
        .wide()
        .map(|wide| matches!(wide, CellWide::Wide | CellWide::SpacerHead))
        .unwrap_or(false)
    {
        metrics.cell_width * 2
    } else {
        metrics.cell_width
    };

    if selected {
        fg = SELECTION_FOREGROUND;
        bg = SELECTION_BACKGROUND;
    }

    if bg != default_bg || selected {
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
    }

    if !raw_cell.has_text().unwrap_or(false) {
        return None;
    }

    let ch = first_grapheme(cell).unwrap_or(' ');

    if ch == ' ' {
        return Some(ch);
    }

    if !try_draw_block_char(
        buffer,
        width,
        height,
        x,
        y,
        cell_width,
        metrics.cell_height,
        ch,
        fg,
    ) {
        font_renderer.draw_char(buffer, width, height, x, y, ch, fg);
    }
    Some(ch)
}

fn first_grapheme(cell: &CellIteration<'static, '_>) -> Option<char> {
    if cell.graphemes_len().ok()? == 0 {
        return None;
    }

    let mut graphemes = ['\0'];
    cell.graphemes_buf(&mut graphemes).ok()?;
    Some(graphemes[0])
}

/// Synthetically draw Unicode block element characters (U+2580–U+259F) as exact
/// pixel rectangles so they tile seamlessly, matching what Ghostty's sprite renderer does.
/// Returns `true` if the character was handled.
fn try_draw_block_char(
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x: usize,
    y: usize,
    cell_w: usize,
    cell_h: usize,
    ch: char,
    color: u32,
) -> bool {
    match ch {
        // ▀ Upper half block
        '\u{2580}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w, cell_h / 2, color);
        }
        // ▁ Lower one eighth block
        '\u{2581}' => {
            let h = cell_h / 8;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // ▂ Lower one quarter block
        '\u{2582}' => {
            let h = cell_h / 4;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // ▃ Lower three eighths block
        '\u{2583}' => {
            let h = (cell_h * 3) / 8;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // ▄ Lower half block
        '\u{2584}' => {
            let h = cell_h / 2;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // ▅ Lower five eighths block
        '\u{2585}' => {
            let h = (cell_h * 5) / 8;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // ▆ Lower three quarters block
        '\u{2586}' => {
            let h = (cell_h * 3) / 4;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // ▇ Lower seven eighths block
        '\u{2587}' => {
            let h = (cell_h * 7) / 8;
            draw_rect(buffer, buf_w, buf_h, x, y + cell_h - h, cell_w, h, color);
        }
        // █ Full block
        '\u{2588}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w, cell_h, color);
        }
        // ▉ Left seven eighths block
        '\u{2589}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, (cell_w * 7) / 8, cell_h, color);
        }
        // ▊ Left three quarters block
        '\u{258A}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, (cell_w * 3) / 4, cell_h, color);
        }
        // ▋ Left five eighths block
        '\u{258B}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, (cell_w * 5) / 8, cell_h, color);
        }
        // ▌ Left half block
        '\u{258C}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w / 2, cell_h, color);
        }
        // ▍ Left three eighths block
        '\u{258D}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, (cell_w * 3) / 8, cell_h, color);
        }
        // ▎ Left one quarter block
        '\u{258E}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w / 4, cell_h, color);
        }
        // ▏ Left one eighth block
        '\u{258F}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w / 8, cell_h, color);
        }
        // ▐ Right half block
        '\u{2590}' => {
            let w = cell_w / 2;
            draw_rect(buffer, buf_w, buf_h, x + cell_w - w, y, w, cell_h, color);
        }
        // ░ Light shade (25%)
        '\u{2591}' => {
            draw_shade(buffer, buf_w, buf_h, x, y, cell_w, cell_h, color, 64);
        }
        // ▒ Medium shade (50%)
        '\u{2592}' => {
            draw_shade(buffer, buf_w, buf_h, x, y, cell_w, cell_h, color, 128);
        }
        // ▓ Dark shade (75%)
        '\u{2593}' => {
            draw_shade(buffer, buf_w, buf_h, x, y, cell_w, cell_h, color, 192);
        }
        // ▔ Upper one eighth block
        '\u{2594}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w, cell_h / 8, color);
        }
        // ▕ Right one eighth block
        '\u{2595}' => {
            let w = cell_w / 8;
            draw_rect(buffer, buf_w, buf_h, x + cell_w - w, y, w, cell_h, color);
        }
        // ▖ Quadrant lower left
        '\u{2596}' => {
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▗ Quadrant lower right
        '\u{2597}' => {
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▘ Quadrant upper left
        '\u{2598}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w / 2, cell_h / 2, color);
        }
        // ▙ Quadrant upper left and lower left and lower right
        '\u{2599}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w / 2, cell_h, color);
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▚ Quadrant upper left and lower right
        '\u{259A}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w / 2, cell_h / 2, color);
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▛ Quadrant upper left and upper right and lower left
        '\u{259B}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w, cell_h / 2, color);
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▜ Quadrant upper left and upper right and lower right
        '\u{259C}' => {
            draw_rect(buffer, buf_w, buf_h, x, y, cell_w, cell_h / 2, color);
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▝ Quadrant upper right
        '\u{259D}' => {
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▞ Quadrant upper right and lower left
        '\u{259E}' => {
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y,
                cell_w / 2,
                cell_h / 2,
                color,
            );
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x,
                y + cell_h / 2,
                cell_w / 2,
                cell_h / 2,
                color,
            );
        }
        // ▟ Quadrant upper right and lower left and lower right
        '\u{259F}' => {
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x + cell_w / 2,
                y,
                cell_w / 2,
                cell_h / 2,
                color,
            );
            draw_rect(
                buffer,
                buf_w,
                buf_h,
                x,
                y + cell_h / 2,
                cell_w,
                cell_h / 2,
                color,
            );
        }
        _ => return false,
    }
    true
}

fn draw_shade(
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x: usize,
    y: usize,
    cell_w: usize,
    cell_h: usize,
    color: u32,
    alpha: u8,
) {
    let max_y = (y + cell_h).min(buf_h);
    let max_x = (x + cell_w).min(buf_w);

    for row in y..max_y {
        for col in x..max_x {
            blend_pixel(
                buffer,
                buf_w,
                buf_h,
                col as isize,
                row as isize,
                color,
                alpha,
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

    if x >= max_x {
        return;
    }

    for row in y..max_y {
        let row_start = row * width;
        buffer[row_start + x..row_start + max_x].fill(color);
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
    if alpha == 255 {
        buffer[index] = color;
        return;
    }

    let dst = buffer[index];
    let alpha = alpha as u32;
    let inv_alpha = 255 - alpha;

    let r = (((color >> 16) & 0xff) * alpha + ((dst >> 16) & 0xff) * inv_alpha) / 255;
    let g = (((color >> 8) & 0xff) * alpha + ((dst >> 8) & 0xff) * inv_alpha) / 255;
    let b = ((color & 0xff) * alpha + (dst & 0xff) * inv_alpha) / 255;

    buffer[index] = (r << 16) | (g << 8) | b;
}

fn is_selected_cell(row: usize, col: usize, selection: Option<SelectionRange>) -> bool {
    let Some(selection) = selection else {
        return false;
    };

    if row < selection.start_row || row > selection.end_row {
        return false;
    }

    if selection.start_row == selection.end_row {
        return row == selection.start_row
            && col >= selection.start_col
            && col <= selection.end_col;
    }

    if row == selection.start_row {
        return col >= selection.start_col;
    }

    if row == selection.end_row {
        return col <= selection.end_col;
    }

    true
}

fn cell_colors(
    cell: &CellIteration<'static, '_>,
    theme: &TerminalTheme,
    default_fg: u32,
    default_bg: u32,
) -> (u32, u32) {
    let style = cell.style().unwrap_or_default();
    let mut fg = cell
        .fg_color()
        .ok()
        .flatten()
        .map(rgb_color)
        .unwrap_or_else(|| match style.fg_color {
            StyleColor::Palette(index) => color_from_palette(index, theme).unwrap_or(default_fg),
            StyleColor::Rgb(color) => rgb_color(color),
            StyleColor::None => default_fg,
        });
    let mut bg = cell
        .bg_color()
        .ok()
        .flatten()
        .map(rgb_color)
        .unwrap_or_else(|| match style.bg_color {
            StyleColor::Palette(index) => color_from_palette(index, theme).unwrap_or(default_bg),
            StyleColor::Rgb(color) => rgb_color(color),
            StyleColor::None => default_bg,
        });

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
