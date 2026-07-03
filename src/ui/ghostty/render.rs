// These rendering helpers operate on a raw pixel buffer plus its geometry and
// styling parameters, which naturally exceeds the argument-count lint.
#![allow(clippy::too_many_arguments)]

use libghostty_vt::Terminal;
use libghostty_vt::render::{CellIteration, CellIterator, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::RgbColor;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use super::config::{TerminalMetrics, TerminalTheme};
use super::font::FontRenderer;
use super::layout::PaneRect;

const SELECTION_BACKGROUND: u32 = 0x4f6480;
const SELECTION_FOREGROUND: u32 = 0xfbf1c7;
/// Subtle wash toward the theme background on inactive panes (~12%).
const INACTIVE_PANE_DIM_ALPHA: u8 = 32;

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

pub(super) fn pane_focus_border_color(active: bool, theme: &TerminalTheme) -> u32 {
    if active {
        lerp_color(theme.foreground, theme.cursor, 96)
    } else {
        lerp_color(theme.foreground, theme.background, 210)
    }
}

pub(super) fn draw_pane_focus_chrome(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    rect: PaneRect,
    active: bool,
    theme: &TerminalTheme,
    apply_inactive_tint: bool,
    damage: &mut Vec<DamageRect>,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    let border_color = pane_focus_border_color(active, theme);
    draw_pane_border(buffer, width, height, rect, border_color);

    if !active && apply_inactive_tint {
        let inset = 1;
        if rect.width > inset * 2 && rect.height > inset * 2 {
            draw_pane_tint(
                buffer,
                width,
                height,
                rect.x + inset,
                rect.y + inset,
                rect.width - inset * 2,
                rect.height - inset * 2,
                theme.background,
                INACTIVE_PANE_DIM_ALPHA,
            );
        }
    }

    push_damage(
        damage,
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        width,
        height,
    );
}

pub(super) fn draw_pane_border(
    buffer: &mut [u32],
    width: usize,
    height: usize,
    rect: PaneRect,
    border_color: u32,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    draw_rect(
        buffer,
        width,
        height,
        rect.x,
        rect.y,
        rect.width,
        1,
        border_color,
    );
    draw_rect(
        buffer,
        width,
        height,
        rect.x,
        rect.y + rect.height.saturating_sub(1),
        rect.width,
        1,
        border_color,
    );
    draw_rect(
        buffer,
        width,
        height,
        rect.x,
        rect.y,
        1,
        rect.height,
        border_color,
    );
    draw_rect(
        buffer,
        width,
        height,
        rect.x + rect.width.saturating_sub(1),
        rect.y,
        1,
        rect.height,
        border_color,
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

pub(super) fn draw_ratatui_buffer(
    ratatui_buffer: &Buffer,
    font_renderer: &mut FontRenderer,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    metrics: TerminalMetrics,
    default_fg: u32,
    default_bg: u32,
    force_redraw: bool,
    damage: &mut Vec<DamageRect>,
) -> bool {
    if !force_redraw {
        return false;
    }

    let area = ratatui_buffer.area;
    for row in 0..area.height {
        let y = origin_y + row as usize * metrics.cell_height;
        let row_width = area.width as usize * metrics.cell_width;
        draw_rect(
            buffer,
            width,
            height,
            origin_x,
            y,
            row_width,
            metrics.cell_height,
            default_bg,
        );
        push_damage(
            damage,
            origin_x,
            y,
            row_width,
            metrics.cell_height,
            width,
            height,
        );

        for col in 0..area.width {
            let cell = &ratatui_buffer[(col, row)];
            if cell.skip {
                continue;
            }

            let x = origin_x + col as usize * metrics.cell_width;
            draw_ratatui_cell(
                cell,
                font_renderer,
                buffer,
                width,
                height,
                x,
                y,
                metrics,
                default_fg,
                default_bg,
            );
        }
    }

    coalesce_damage(damage);
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

    let style = cell.style().unwrap_or_default();
    let explicit_bg = cell.bg_color().ok().flatten().map(rgb_color);
    let mut fg = cell
        .fg_color()
        .ok()
        .flatten()
        .map(rgb_color)
        .unwrap_or(default_fg);
    let mut bg = explicit_bg.unwrap_or(default_bg);
    let cell_width = if raw_cell
        .wide()
        .map(|wide| matches!(wide, CellWide::Wide | CellWide::SpacerHead))
        .unwrap_or(false)
    {
        metrics.cell_width * 2
    } else {
        metrics.cell_width
    };

    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }

    if style.faint {
        fg = dim_toward(fg, bg);
    }

    if selected {
        fg = SELECTION_FOREGROUND;
        bg = SELECTION_BACKGROUND;
    }

    if explicit_bg.is_some() || selected || style.inverse {
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

    if style.invisible {
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
        font_renderer.draw_char(
            buffer,
            width,
            height,
            x,
            y,
            ch,
            fg,
            style.bold,
            style.italic,
        );
    }
    Some(ch)
}

fn draw_ratatui_cell(
    cell: &ratatui::buffer::Cell,
    font_renderer: &mut FontRenderer,
    buffer: &mut [u32],
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    metrics: TerminalMetrics,
    default_fg: u32,
    default_bg: u32,
) {
    let modifier = cell.modifier;
    let mut fg = ratatui_color(cell.fg, default_fg);
    let mut bg = ratatui_color(cell.bg, default_bg);

    if modifier.contains(Modifier::REVERSED) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if modifier.contains(Modifier::DIM) {
        fg = dim_toward(fg, bg);
    }

    draw_rect(
        buffer,
        width,
        height,
        x,
        y,
        metrics.cell_width,
        metrics.cell_height,
        bg,
    );

    if modifier.contains(Modifier::HIDDEN) {
        return;
    }

    let Some(ch) = cell.symbol().chars().next().filter(|ch| *ch != ' ') else {
        return;
    };

    if !try_draw_block_char(
        buffer,
        width,
        height,
        x,
        y,
        metrics.cell_width,
        metrics.cell_height,
        ch,
        fg,
    ) {
        font_renderer.draw_char(
            buffer,
            width,
            height,
            x,
            y,
            ch,
            fg,
            modifier.contains(Modifier::BOLD),
            modifier.contains(Modifier::ITALIC),
        );
    }

    if modifier.contains(Modifier::UNDERLINED) {
        draw_rect(
            buffer,
            width,
            height,
            x,
            y + metrics.cell_height.saturating_sub(2),
            metrics.cell_width,
            1,
            fg,
        );
    }
}

fn ratatui_color(color: Color, default: u32) -> u32 {
    match color {
        Color::Reset => default,
        Color::Black => 0x0c0c0c,
        Color::Red => 0xcc241d,
        Color::Green => 0x98971a,
        Color::Yellow => 0xd79921,
        Color::Blue => 0x458588,
        Color::Magenta => 0xb16286,
        Color::Cyan => 0x689d6a,
        Color::Gray => 0xa89984,
        Color::DarkGray => 0x928374,
        Color::LightRed => 0xfb4934,
        Color::LightGreen => 0xb8bb26,
        Color::LightYellow => 0xfabd2f,
        Color::LightBlue => 0x83a598,
        Color::LightMagenta => 0xd3869b,
        Color::LightCyan => 0x8ec07c,
        Color::White => 0xebdbb2,
        Color::Indexed(index) => ansi_color(index),
        Color::Rgb(red, green, blue) => ((red as u32) << 16) | ((green as u32) << 8) | blue as u32,
    }
}

fn ansi_color(index: u8) -> u32 {
    const ANSI_16: [u32; 16] = [
        0x0c0c0c, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984, 0x928374,
        0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
    ];

    if let Some(color) = ANSI_16.get(index as usize) {
        return *color;
    }

    let index = index.saturating_sub(16);
    if index < 216 {
        let r = index / 36;
        let g = (index % 36) / 6;
        let b = index % 6;
        return (cube_component(r) << 16) | (cube_component(g) << 8) | cube_component(b);
    }

    let gray = 8 + (index.saturating_sub(216) as u32) * 10;
    (gray << 16) | (gray << 8) | gray
}

fn cube_component(value: u8) -> u32 {
    match value {
        0 => 0,
        value => 55 + value as u32 * 40,
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

fn draw_pane_tint(
    buffer: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    x: usize,
    y: usize,
    rect_width: usize,
    rect_height: usize,
    tint: u32,
    alpha: u8,
) {
    draw_shade(
        buffer,
        buf_w,
        buf_h,
        x,
        y,
        rect_width,
        rect_height,
        tint,
        alpha,
    );
}

fn lerp_color(from: u32, to: u32, amount: u8) -> u32 {
    let amount = amount as u32;
    let inv = 255 - amount;
    let r = ((((from >> 16) & 0xff) * inv) + (((to >> 16) & 0xff) * amount)) / 255;
    let g = ((((from >> 8) & 0xff) * inv) + (((to >> 8) & 0xff) * amount)) / 255;
    let b = (((from & 0xff) * inv) + ((to & 0xff) * amount)) / 255;

    (r << 16) | (g << 8) | b
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
    let Some(pixel) = buffer.get_mut(index) else {
        return;
    };
    if alpha == 255 {
        *pixel = color;
        return;
    }

    let dst = *pixel;
    let alpha = alpha as u32;
    let inv_alpha = 255 - alpha;

    let r = (((color >> 16) & 0xff) * alpha + ((dst >> 16) & 0xff) * inv_alpha) / 255;
    let g = (((color >> 8) & 0xff) * alpha + ((dst >> 8) & 0xff) * inv_alpha) / 255;
    let b = ((color & 0xff) * alpha + (dst & 0xff) * inv_alpha) / 255;

    *pixel = (r << 16) | (g << 8) | b;
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

fn rgb_color(color: RgbColor) -> u32 {
    rgb(color.r, color.g, color.b)
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn dim_toward(color: u32, background: u32) -> u32 {
    let r = ((((color >> 16) & 0xff) + ((background >> 16) & 0xff)) / 2) & 0xff;
    let g = ((((color >> 8) & 0xff) + ((background >> 8) & 0xff)) / 2) & 0xff;
    let b = (((color & 0xff) + (background & 0xff)) / 2) & 0xff;

    (r << 16) | (g << 8) | b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ghostty::config::TerminalTheme;

    #[test]
    fn active_border_is_brighter_than_inactive_border() {
        let theme = TerminalTheme::default();
        let active = pane_focus_border_color(true, &theme);
        let inactive = pane_focus_border_color(false, &theme);

        assert_ne!(active, inactive);
        assert!(color_luminance(active) > color_luminance(inactive));
    }

    #[test]
    fn inactive_border_is_closer_to_background_than_foreground() {
        let theme = TerminalTheme::default();
        let inactive = pane_focus_border_color(false, &theme);
        let dist_to_bg = color_distance(inactive, theme.background);
        let dist_to_fg = color_distance(inactive, theme.foreground);

        assert!(dist_to_bg < dist_to_fg);
    }

    fn color_luminance(color: u32) -> u32 {
        let r = (color >> 16) & 0xff;
        let g = (color >> 8) & 0xff;
        let b = color & 0xff;
        r + g + b
    }

    fn color_distance(a: u32, b: u32) -> u32 {
        let dr = ((a >> 16) & 0xff).abs_diff((b >> 16) & 0xff);
        let dg = ((a >> 8) & 0xff).abs_diff((b >> 8) & 0xff);
        let db = (a & 0xff).abs_diff(b & 0xff);
        dr + dg + db
    }

    #[test]
    fn inactive_tint_skipped_on_chrome_only_redraw() {
        use super::super::layout::PaneRect;

        let theme = TerminalTheme::default();
        let width = 100usize;
        let height = 100usize;
        let rect = PaneRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        };
        let mut buffer = vec![0x00ff00; width * height];
        let mut damage = Vec::new();

        draw_pane_focus_chrome(
            &mut buffer,
            width,
            height,
            rect,
            false,
            &theme,
            true,
            &mut damage,
        );
        let after_content_redraw = buffer.clone();

        draw_pane_focus_chrome(
            &mut buffer,
            width,
            height,
            rect,
            false,
            &theme,
            false,
            &mut damage,
        );

        let inset = 2;
        for row in (rect.y + inset)..(rect.y + rect.height - inset) {
            for col in (rect.x + inset)..(rect.x + rect.width - inset) {
                let idx = row * width + col;
                assert_eq!(
                    buffer[idx], after_content_redraw[idx],
                    "interior pixel at ({col}, {row}) changed on chrome-only redraw"
                );
            }
        }
    }
}
