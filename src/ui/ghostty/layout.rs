use super::input::Key;

const SPLIT_GAP: usize = 2;

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

impl PaneSplitDirection {
    pub(super) fn for_window(width: usize, height: usize) -> Self {
        if height > width {
            Self::Horizontal
        } else {
            Self::Vertical
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
    pane_count: usize,
    mode: &mut PaneLayoutMode,
    split_direction: &mut PaneSplitDirection,
    active_pane: &mut usize,
) -> bool {
    if !alt {
        return false;
    }

    for key in pressed_keys {
        if *key == Key::J {
            *split_direction = split_direction.toggled();
            return true;
        }

        if let Some(next_active_pane) = pane_navigation_shortcut(*key) {
            if next_active_pane < pane_count {
                *active_pane = next_active_pane;
            }
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

    false
}

fn focused_pane_for_layout(mode: PaneLayoutMode) -> Option<usize> {
    match mode {
        PaneLayoutMode::NvimMaximized => Some(0),
        PaneLayoutMode::AgentMaximized => Some(1),
        PaneLayoutMode::Split => None,
    }
}

pub(super) fn pane_navigation_shortcut(key: Key) -> Option<usize> {
    match key {
        Key::Left => Some(0),
        Key::Right => Some(1),
        _ => None,
    }
}

pub(super) fn pane_layout_shortcut(key: Key) -> Option<PaneLayoutMode> {
    match key {
        Key::Key1 => Some(PaneLayoutMode::NvimMaximized),
        Key::Key2 => Some(PaneLayoutMode::Split),
        Key::Key3 => Some(PaneLayoutMode::AgentMaximized),
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
