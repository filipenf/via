use super::input::Key;

const SPLIT_GAP: usize = 2;

/// Require one dimension to be at least 20% larger than the other before treating the
/// window as clearly tall (`height >= width * 6/5`) or clearly wide (`width >= height * 6/5`).
const SPLIT_ASPECT_NUM: usize = 5;
const SPLIT_ASPECT_DEN: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaneRect {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) width: usize,
    pub(super) height: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneLayoutMode {
    NvimMaximized,
    Split,
    AgentMaximized,
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
        } else if clearly_wider_than_tall(width, height) {
            Self::Vertical
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
pub(super) struct SplitLayout {
    panes: Vec<PaneRect>,
}

impl SplitLayout {
    pub(super) fn for_window(
        width: usize,
        height: usize,
        pane_count: usize,
        mode: PaneLayoutMode,
        split_direction: PaneSplitDirection,
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

        if mode == PaneLayoutMode::NvimMaximized {
            return Self {
                panes: vec![
                    PaneRect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    PaneRect {
                        x: width,
                        y: 0,
                        width: 0,
                        height: 0,
                    },
                ],
            };
        }

        if mode == PaneLayoutMode::AgentMaximized {
            return Self {
                panes: vec![
                    PaneRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    },
                    PaneRect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                ],
            };
        }

        match split_direction {
            PaneSplitDirection::Vertical => vertical_split_layout(width, height),
            PaneSplitDirection::Horizontal => horizontal_split_layout(width, height),
        }
    }

    pub(super) fn pane(&self, index: usize) -> PaneRect {
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

    for key in pressed_keys {
        if shift {
            if *key == Key::Key3 {
                *mode = PaneLayoutMode::Split;
                *split_direction = split_direction.toggled();
                return true;
            }

            let Some(next_mode) = pane_layout_shortcut(*key) else {
                continue;
            };

            if next_mode == PaneLayoutMode::AgentMaximized && pane_count < 2 {
                continue;
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
    }

    false
}

fn focused_pane_for_layout(mode: PaneLayoutMode) -> Option<usize> {
    match mode {
        PaneLayoutMode::NvimMaximized => Some(0),
        PaneLayoutMode::AgentMaximized => Some(1),
        PaneLayoutMode::Split => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FocusNvimAfterReference {
    pub(super) relayout_needed: bool,
    pub(super) focus_changed: bool,
}

/// Focus the Neovim pane after navigating from a Shift+click on a file or symbol in the
/// agent pane. When the agent was fullscreen, switch to fullscreen Neovim; otherwise keep
/// the split layout and only change the active pane.
pub(super) fn focus_nvim_after_agent_reference(
    mode: &mut PaneLayoutMode,
    active_pane: &mut usize,
) -> FocusNvimAfterReference {
    let relayout_needed = *mode == PaneLayoutMode::AgentMaximized;
    if relayout_needed {
        *mode = PaneLayoutMode::NvimMaximized;
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
        Key::Key1 => Some(PaneLayoutMode::NvimMaximized),
        Key::Key2 => Some(PaneLayoutMode::AgentMaximized),
        _ => None,
    }
}

fn vertical_split_layout(width: usize, height: usize) -> SplitLayout {
    let left_width = width.saturating_sub(SPLIT_GAP) / 2;
    let right_x = left_width + SPLIT_GAP;
    let right_width = width.saturating_sub(right_x);

    SplitLayout {
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
        let mut mode = PaneLayoutMode::AgentMaximized;
        let mut active_pane = 1;

        let focus = focus_nvim_after_agent_reference(&mut mode, &mut active_pane);

        assert_eq!(mode, PaneLayoutMode::NvimMaximized);
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
}
