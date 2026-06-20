use super::input::Key;
use crate::config::{DEFAULT_AGENT_PANE_MAX_COLS, DEFAULT_AGENT_PANE_MIN_COLS};

const SPLIT_GAP: usize = 2;
/// Minimum leading (editor) pane width in columns for vertical split mode.
/// NOTE: pub so that benches can use `via::ui::ghostty::layout::...` for Criterion
/// regression testing. Not part of via's public library API.
pub const MIN_EDITOR_PANE_COLS: u16 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SplitLayoutOptions {
    pub(super) cell_width: usize,
    pub(super) agent_pane_cols: Option<(u16, u16)>,
}

impl SplitLayoutOptions {
    pub(super) fn unbounded() -> Self {
        Self {
            cell_width: 1,
            agent_pane_cols: None,
        }
    }
}

/// Require one dimension to be at least 20% larger than the other before treating the
/// window as clearly tall (`height >= width * 6/5`) or clearly wide (`width >= height * 6/5`).
const SPLIT_ASPECT_NUM: usize = 5;
const SPLIT_ASPECT_DEN: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneLayoutMode {
    Split,
    /// One specific pane (by index) is shown full-screen; others have zero size.
    PaneMaximized(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

fn clearly_taller_than_wide(width: usize, height: usize) -> bool {
    height.saturating_mul(SPLIT_ASPECT_NUM) >= width.saturating_mul(SPLIT_ASPECT_DEN)
}

fn clearly_wider_than_tall(width: usize, height: usize) -> bool {
    width.saturating_mul(SPLIT_ASPECT_NUM) >= height.saturating_mul(SPLIT_ASPECT_DEN)
}

impl PaneSplitDirection {
    /// Pick split direction for a new window from pixel size (no prior direction).
    pub(super) fn for_window(width: usize, height: usize) -> Self {
        if width == 0 || height == 0 {
            return Self::Vertical;
        }
        if clearly_taller_than_wide(width, height) {
            Self::Horizontal
        } else {
            Self::Vertical
        }
    }

    /// Update split direction after a resize, keeping the current mode until the window
    /// is clearly tall or clearly wide by the same 20% margin.
    pub(super) fn adjust_for_window_resize(self, width: usize, height: usize) -> Self {
        if width == 0 || height == 0 {
            return self;
        }
        match self {
            Self::Horizontal => {
                if clearly_wider_than_tall(width, height) {
                    Self::Vertical
                } else {
                    Self::Horizontal
                }
            }
            Self::Vertical => {
                if clearly_taller_than_wide(width, height) {
                    Self::Horizontal
                } else {
                    Self::Vertical
                }
            }
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Vertical => Self::Horizontal,
            Self::Horizontal => Self::Vertical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitLayout {
    panes: Vec<PaneRect>,
}

impl SplitLayout {
    pub(super) fn for_window(
        width: usize,
        height: usize,
        pane_count: usize,
        mode: PaneLayoutMode,
        split_direction: PaneSplitDirection,
        options: SplitLayoutOptions,
    ) -> Self {
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

        // PaneMaximized must be handled before the multi-pane split logic.
        if let PaneLayoutMode::PaneMaximized(i) = mode {
            if i < pane_count {
                let mut panes = vec![
                    PaneRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0
                    };
                    pane_count
                ];
                panes[i] = PaneRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                };
                return Self { panes };
            }
        }

        // For three or more panes we always produce a flat list of visible rects.
        // The first rect is the editor (computed via the normal two-pane rules),
        // the remaining rects evenly share the secondary area.
        if pane_count > 2 {
            // Helper: split a rect into `count` equal sub-rects.
            fn split_rect(rect: PaneRect, count: usize, horizontal: bool) -> Vec<PaneRect> {
                if count == 0 {
                    return vec![];
                }
                if count == 1 {
                    return vec![rect];
                }
                let mut out = Vec::with_capacity(count);
                if horizontal {
                    let each = rect.height / count;
                    for i in 0..count {
                        let h = if i == count - 1 {
                            rect.height.saturating_sub(i * each)
                        } else {
                            each
                        };
                        out.push(PaneRect {
                            x: rect.x,
                            y: rect.y + i * each,
                            width: rect.width,
                            height: h,
                        });
                    }
                } else {
                    let each = rect.width / count;
                    for i in 0..count {
                        let w = if i == count - 1 {
                            rect.width.saturating_sub(i * each)
                        } else {
                            each
                        };
                        out.push(PaneRect {
                            x: rect.x + i * each,
                            y: rect.y,
                            width: w,
                            height: rect.height,
                        });
                    }
                }
                out
            }

            // Compute the editor + secondary area split using the normal two-pane rules,
            // then divide the secondary area among the remaining agent panes.
            let base = match split_direction {
                PaneSplitDirection::Vertical => vertical_split_layout(
                    width,
                    height,
                    options.cell_width,
                    options.agent_pane_cols,
                ),
                PaneSplitDirection::Horizontal => horizontal_split_layout(width, height),
            };
            let editor_rect = base.panes[0];
            let agent_area = base.panes.get(1).copied().unwrap_or(PaneRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
            let agent_count = pane_count - 1;
            let agent_rects = if split_direction == PaneSplitDirection::Vertical {
                split_rect(agent_area, agent_count, /*horizontal*/ true)
            } else {
                split_rect(agent_area, agent_count, /*horizontal*/ false)
            };
            let mut all = vec![editor_rect];
            all.extend(agent_rects);
            return Self { panes: all };
        }

        match split_direction {
            PaneSplitDirection::Vertical => {
                vertical_split_layout(width, height, options.cell_width, options.agent_pane_cols)
            }
            PaneSplitDirection::Horizontal => horizontal_split_layout(width, height),
        }
    }

    pub fn pane(&self, index: usize) -> PaneRect {
        self.panes[index]
    }

    pub(super) fn pane_at(&self, x: usize, y: usize) -> Option<(usize, PaneRect)> {
        self.panes.iter().copied().enumerate().find(|(_, rect)| {
            x >= rect.x
                && x < rect.x.saturating_add(rect.width)
                && y >= rect.y
                && y < rect.y.saturating_add(rect.height)
        })
    }
}
pub(super) fn handle_layout_shortcuts(
    pressed_keys: &[Key],
    alt: bool,
    shift: bool,
    pane_count: usize,
    mode: &mut PaneLayoutMode,
    split_direction: &mut PaneSplitDirection,
    active_pane: &mut usize,
) -> bool {
    if !alt {
        return false;
    }

    // Alt+J (without Shift) toggles the split direction (replaces the old Alt+Shift+3).
    for key in pressed_keys {
        if alt && !shift && *key == Key::J {
            *mode = PaneLayoutMode::Split;
            *split_direction = split_direction.toggled();
            return true;
        }
    }

    for key in pressed_keys {
        if shift {
            let Some(next_mode) = pane_layout_shortcut(*key) else {
                continue;
            };

            if let PaneLayoutMode::PaneMaximized(i) = next_mode {
                if i >= pane_count {
                    continue;
                }
            }

            *mode = next_mode;
            if let Some(next_active_pane) = focused_pane_for_layout(next_mode) {
                *active_pane = next_active_pane;
            }
            return true;
        }

        if let Some(next_active_pane) =
            pane_focus_shortcut(*key).or_else(|| pane_navigation_shortcut(*key))
        {
            if next_active_pane < pane_count {
                *mode = PaneLayoutMode::Split;
                *active_pane = next_active_pane;
                return true;
            }
        }

        // Alt+2..9 focuses the corresponding agent pane (Alt+2 = first agent = index 1)
        if alt && !shift {
            if let Some(digit) = key_to_digit(*key) {
                if (2..=9).contains(&digit) {
                    let target = digit - 1; // Alt+2 -> 1, Alt+3 -> 2, ...
                    if target < pane_count {
                        *mode = PaneLayoutMode::Split;
                        *active_pane = target;
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn key_to_digit(key: Key) -> Option<usize> {
    match key {
        Key::Key1 => Some(1),
        Key::Key2 => Some(2),
        Key::Key3 => Some(3),
        Key::Key4 => Some(4),
        Key::Key5 => Some(5),
        Key::Key6 => Some(6),
        Key::Key7 => Some(7),
        Key::Key8 => Some(8),
        Key::Key9 => Some(9),
        _ => None,
    }
}

fn focused_pane_for_layout(mode: PaneLayoutMode) -> Option<usize> {
    match mode {
        PaneLayoutMode::PaneMaximized(i) => Some(i),
        PaneLayoutMode::Split => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusNvimAfterReference {
    pub relayout_needed: bool,
    pub focus_changed: bool,
}

/// Focus the Neovim pane after navigating from a Shift+click on a file or symbol in the
/// agent pane. When the agent was fullscreen, switch to fullscreen Neovim; otherwise keep
/// the split layout and only change the active pane.
pub fn focus_nvim_after_agent_reference(
    mode: &mut PaneLayoutMode,
    active_pane: &mut usize,
) -> FocusNvimAfterReference {
    // Any maximized agent pane counts as "agent maximized" for this transition.
    let was_agent_max = matches!(mode, PaneLayoutMode::PaneMaximized(i) if *i != 0);
    let relayout_needed = was_agent_max;
    if relayout_needed {
        *mode = PaneLayoutMode::PaneMaximized(0);
    }
    let focus_changed = *active_pane != 0;
    *active_pane = 0;
    FocusNvimAfterReference {
        relayout_needed,
        focus_changed,
    }
}

pub(super) fn pane_navigation_shortcut(key: Key) -> Option<usize> {
    match key {
        Key::Left => Some(0),
        Key::Right => Some(1),
        _ => None,
    }
}

fn pane_focus_shortcut(key: Key) -> Option<usize> {
    match key {
        Key::Key1 => Some(0),
        Key::Key2 => Some(1),
        _ => None,
    }
}

pub(super) fn pane_layout_shortcut(key: Key) -> Option<PaneLayoutMode> {
    match key {
        Key::Key1 => Some(PaneLayoutMode::PaneMaximized(0)),
        Key::Key2 => Some(PaneLayoutMode::PaneMaximized(1)),
        Key::Key3 => Some(PaneLayoutMode::PaneMaximized(2)),
        Key::Key4 => Some(PaneLayoutMode::PaneMaximized(3)),
        Key::Key5 => Some(PaneLayoutMode::PaneMaximized(4)),
        Key::Key6 => Some(PaneLayoutMode::PaneMaximized(5)),
        Key::Key7 => Some(PaneLayoutMode::PaneMaximized(6)),
        Key::Key8 => Some(PaneLayoutMode::PaneMaximized(7)),
        Key::Key9 => Some(PaneLayoutMode::PaneMaximized(8)),
        _ => None,
    }
}

pub fn vertical_split_layout(
    width: usize,
    height: usize,
    cell_width: usize,
    agent_pane_cols: Option<(u16, u16)>,
) -> SplitLayout {
    let (min_cols, max_cols) =
        agent_pane_cols.unwrap_or((DEFAULT_AGENT_PANE_MIN_COLS, DEFAULT_AGENT_PANE_MAX_COLS));
    let cell_width = cell_width.max(1);
    let trailing_cols =
        trailing_pane_cols(width, cell_width, min_cols, max_cols, MIN_EDITOR_PANE_COLS);

    let trailing_pixel_width = trailing_cols as usize * cell_width;
    let leading_width = width
        .saturating_sub(SPLIT_GAP)
        .saturating_sub(trailing_pixel_width);
    let trailing_x = leading_width + SPLIT_GAP;
    let trailing_width = width.saturating_sub(trailing_x);

    SplitLayout {
        panes: vec![
            PaneRect {
                x: 0,
                y: 0,
                width: leading_width,
                height,
            },
            PaneRect {
                x: trailing_x,
                y: 0,
                width: trailing_width,
                height,
            },
        ],
    }
}

pub fn window_col_count(width: usize, cell_width: usize) -> u16 {
    let cell_width = cell_width.max(1);
    ((width.saturating_sub(SPLIT_GAP)) / cell_width).min(u16::MAX as usize) as u16
}

/// True when the window can fit both the editor minimum and the agent minimum.
pub fn vertical_split_fits(width: usize, cell_width: usize, agent_min_cols: u16) -> bool {
    window_col_count(width, cell_width) >= MIN_EDITOR_PANE_COLS.saturating_add(agent_min_cols)
}

/// Column count for the trailing (right) pane in a vertical split. Keeps the agent at
/// `min_cols` when both minimums fit so extra width goes to the editor; shrinks the agent
/// below `min_cols` only when the window cannot satisfy `min_editor_cols` + `min_cols`.
pub fn trailing_pane_cols(
    width: usize,
    cell_width: usize,
    min_cols: u16,
    max_cols: u16,
    min_editor_cols: u16,
) -> u16 {
    let total_cols = window_col_count(width, cell_width);

    if total_cols >= min_editor_cols.saturating_add(min_cols) {
        return min_cols.min(max_cols);
    }

    total_cols.saturating_sub(min_editor_cols).min(max_cols)
}

fn horizontal_split_layout(width: usize, height: usize) -> SplitLayout {
    let top_height = height.saturating_sub(SPLIT_GAP) / 2;
    let bottom_y = top_height + SPLIT_GAP;
    let bottom_height = height.saturating_sub(bottom_y);

    SplitLayout {
        panes: vec![
            PaneRect {
                x: 0,
                y: 0,
                width,
                height: top_height,
            },
            PaneRect {
                x: 0,
                y: bottom_y,
                width,
                height: bottom_height,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_navigation_from_agent_fullscreen_maximizes_nvim() {
        let mut mode = PaneLayoutMode::PaneMaximized(1);
        let mut active_pane = 1;

        let focus = focus_nvim_after_agent_reference(&mut mode, &mut active_pane);

        assert_eq!(mode, PaneLayoutMode::PaneMaximized(0));
        assert_eq!(active_pane, 0);
        assert!(focus.relayout_needed);
        assert!(focus.focus_changed);
    }

    #[test]
    fn reference_navigation_from_split_keeps_split_and_focuses_nvim() {
        let mut mode = PaneLayoutMode::Split;
        let mut active_pane = 1;

        let focus = focus_nvim_after_agent_reference(&mut mode, &mut active_pane);

        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 0);
        assert!(!focus.relayout_needed);
        assert!(focus.focus_changed);
    }

    #[test]
    fn reference_navigation_when_nvim_already_active_in_split() {
        let mut mode = PaneLayoutMode::Split;
        let mut active_pane = 0;

        let focus = focus_nvim_after_agent_reference(&mut mode, &mut active_pane);

        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 0);
        assert!(!focus.relayout_needed);
        assert!(!focus.focus_changed);
    }

    #[test]
    fn vertical_split_gives_extra_columns_to_editor() {
        let cell_width = 10;
        let width = cell_width * 200 + SPLIT_GAP;
        let layout = vertical_split_layout(width, 50, cell_width, Some((80, 100)));

        assert_eq!(layout.pane(0).width / cell_width, 120);
        assert_eq!(layout.pane(1).width / cell_width, 80);
    }

    #[test]
    fn vertical_split_reserves_editor_minimum() {
        let cell_width = 10;
        let width = cell_width * 165 + SPLIT_GAP;
        let layout = vertical_split_layout(width, 50, cell_width, Some((80, 100)));

        assert_eq!(layout.pane(0).width / cell_width, 85);
        assert_eq!(layout.pane(1).width / cell_width, 80);
    }

    #[test]
    fn vertical_split_fits_requires_editor_and_agent_minimums() {
        assert!(!vertical_split_fits(10 * 159 + SPLIT_GAP, 10, 80));
        assert!(vertical_split_fits(10 * 160 + SPLIT_GAP, 10, 80));
    }

    #[test]
    fn trailing_pane_cols_keeps_agent_at_minimum_when_both_fit() {
        assert_eq!(
            trailing_pane_cols(10 * 150 + SPLIT_GAP, 10, 80, 100, MIN_EDITOR_PANE_COLS),
            70
        );
        assert_eq!(
            trailing_pane_cols(10 * 200 + SPLIT_GAP, 10, 80, 100, MIN_EDITOR_PANE_COLS),
            80
        );
    }

    #[test]
    fn window_col_count_is_floor_div_minus_gap() {
        assert_eq!(window_col_count(10 * 80 + SPLIT_GAP, 10), 80);
        assert_eq!(window_col_count(10 * 81 + SPLIT_GAP, 10), 81);
    }

    #[test]
    fn focus_nvim_from_agent_split_only_changes_active_pane() {
        let mut mode = PaneLayoutMode::Split;
        let mut active = 1;
        let focus = focus_nvim_after_agent_reference(&mut mode, &mut active);
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active, 0);
        assert!(!focus.relayout_needed);
        assert!(focus.focus_changed);
    }
}
