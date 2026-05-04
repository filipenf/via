use minifb::Key;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SplitLayout {
    panes: Vec<PaneRect>,
}

impl SplitLayout {
    pub(super) fn for_window(width: usize, height: usize, pane_count: usize, mode: PaneLayoutMode) -> Self {
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
    active_pane: &mut usize,
) -> bool {
    if !alt {
        return false;
    }

    for key in pressed_keys {
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
