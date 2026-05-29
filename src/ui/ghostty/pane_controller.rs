use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::info;
use winit::event::{ElementState, MouseButton};

use super::input::{
    copy_requested, forward_special_keys, forward_text_input, paste_requested, try_clipboard_paste,
    Key, Modifiers,
};
use super::links::ReferenceTarget;
use super::pane::{PaneMouseAction, PaneMouseButton, PaneMouseModifiers, TerminalPane};

const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneRole {
    Editor,
    AgentTerminal,
    ReviewTerminal,
}

#[derive(Debug, Default)]
pub(super) struct PaneEventOutcome {
    pub(super) dirty: bool,
    pub(super) force_redraw: bool,
    pub(super) command: Option<PaneCommand>,
}

#[derive(Debug)]
pub(super) enum PaneCommand {
    OpenRequested {
        path: std::path::PathBuf,
        line: Option<u32>,
    },
    SymbolOpenRequested {
        symbol: String,
    },
}

#[derive(Debug, Clone, Copy)]
struct ReferenceClick {
    at: Instant,
    row: usize,
    column: usize,
}

#[derive(Default)]
struct MouseState {
    held_button: Option<PaneMouseButton>,
    last_reference_click: Option<ReferenceClick>,
}

pub(super) struct TerminalPaneController {
    role: PaneRole,
    pane: TerminalPane,
    mouse: MouseState,
}

impl TerminalPaneController {
    pub(super) fn new(role: PaneRole, pane: TerminalPane) -> Self {
        Self {
            role,
            pane,
            mouse: MouseState::default(),
        }
    }

    pub(super) fn role(&self) -> PaneRole {
        self.role
    }

    #[cfg(test)]
    fn held_button_for_test(&self) -> Option<PaneMouseButton> {
        self.mouse.held_button
    }

    pub(super) fn handle_terminal_key(
        &mut self,
        pressed_keys: &[Key],
        key: Option<Key>,
        text: Option<&str>,
        repeat: bool,
        modifiers: Modifiers,
        suppress_input: bool,
    ) -> Result<PaneEventOutcome> {
        let mut outcome = PaneEventOutcome::default();

        let paste_requested = !repeat
            && key
                .map(|key| paste_requested(key, modifiers))
                .unwrap_or(false);
        if paste_requested {
            outcome.dirty |= try_clipboard_paste(true, &mut self.pane, suppress_input)?;
            return Ok(outcome);
        }

        let copy_requested = !repeat
            && key
                .map(|key| copy_requested(key, modifiers))
                .unwrap_or(false);
        if copy_requested {
            outcome.dirty |= self.pane.copy_selection_to_clipboard();
            return Ok(outcome);
        }

        if !suppress_input && self.intercepts_viewport_navigation_keys() {
            let step = self.pane.viewport_rows().max(1) as isize;
            for key in pressed_keys.iter().copied() {
                match key {
                    Key::PageUp => {
                        self.pane.scroll_viewport(-step);
                        outcome.dirty = true;
                        outcome.force_redraw = true;
                        return Ok(outcome);
                    }
                    Key::PageDown => {
                        self.pane.scroll_viewport(step);
                        outcome.dirty = true;
                        outcome.force_redraw = true;
                        return Ok(outcome);
                    }
                    Key::Home => {
                        self.pane.scroll_viewport_to_top();
                        outcome.dirty = true;
                        outcome.force_redraw = true;
                        return Ok(outcome);
                    }
                    Key::End => {
                        self.pane.scroll_viewport_to_bottom();
                        outcome.dirty = true;
                        outcome.force_redraw = true;
                        return Ok(outcome);
                    }
                    _ => {}
                }
            }
        }

        outcome.dirty |=
            forward_special_keys(pressed_keys, modifiers, &mut self.pane, suppress_input)?;

        if let Some(text) = text.filter(|text| text.chars().all(|ch| !ch.is_control())) {
            outcome.dirty |= forward_text_input(text, modifiers, &mut self.pane, suppress_input)?;
        }

        Ok(outcome)
    }

    pub(super) fn handle_text_commit(
        &mut self,
        text: &str,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        Ok(PaneEventOutcome {
            dirty: forward_text_input(text, modifiers, &mut self.pane, false)?,
            force_redraw: false,
            command: None,
        })
    }

    pub(super) fn handle_mouse_wheel(
        &mut self,
        scroll_delta: (f32, f32),
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        match self.role {
            PaneRole::ReviewTerminal => {
                self.forward_terminal_mouse_wheel(scroll_delta, local_x, local_y, modifiers)
            }
            PaneRole::Editor | PaneRole::AgentTerminal => {
                let forwarded =
                    self.forward_terminal_mouse_wheel(scroll_delta, local_x, local_y, modifiers)?;
                if forwarded.dirty {
                    return Ok(forwarded);
                }

                let Some(delta_y) = viewport_scroll_delta(scroll_delta) else {
                    return Ok(PaneEventOutcome::default());
                };
                self.pane.scroll_viewport(delta_y);
                Ok(PaneEventOutcome {
                    dirty: true,
                    force_redraw: true,
                    command: None,
                })
            }
        }
    }

    pub(super) fn handle_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
        working_directory: &Path,
    ) -> Result<PaneEventOutcome> {
        match self.role {
            PaneRole::ReviewTerminal => {
                self.forward_terminal_mouse_input(state, button, local_x, local_y, modifiers)
            }
            PaneRole::Editor | PaneRole::AgentTerminal => Ok(self.handle_local_mouse_input(
                state,
                button,
                local_x,
                local_y,
                working_directory,
            )),
        }
    }

    pub(super) fn handle_mouse_motion(
        &mut self,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        match self.role {
            PaneRole::ReviewTerminal => {
                let Some(button) = self.mouse.held_button else {
                    return Ok(PaneEventOutcome::default());
                };
                let dirty = self.pane.forward_mouse_event(
                    PaneMouseAction::Motion,
                    Some(button),
                    local_x,
                    local_y,
                    pane_mouse_modifiers(modifiers),
                    true,
                )?;
                Ok(PaneEventOutcome {
                    dirty,
                    force_redraw: false,
                    command: None,
                })
            }
            PaneRole::Editor | PaneRole::AgentTerminal => {
                if self.mouse.held_button != Some(PaneMouseButton::Left) {
                    return Ok(PaneEventOutcome::default());
                }
                let metrics = self.pane.metrics();
                Ok(PaneEventOutcome {
                    dirty: self.pane.update_selection(
                        local_y / metrics.cell_height,
                        local_x / metrics.cell_width,
                    ),
                    force_redraw: false,
                    command: None,
                })
            }
        }
    }

    fn forward_terminal_mouse_wheel(
        &mut self,
        scroll_delta: (f32, f32),
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        let (sx, sy) = scroll_delta;
        let sy = sy + sx;
        if sy.abs() <= 1e-4 {
            return Ok(PaneEventOutcome::default());
        }

        let scaled = (sy / 40.0).round();
        let steps = if scaled == 0.0 {
            1
        } else {
            scaled.abs().min(64.0) as usize
        };
        let button = if sy > 0.0 {
            PaneMouseButton::WheelUp
        } else {
            PaneMouseButton::WheelDown
        };
        let modifiers = pane_mouse_modifiers(modifiers);
        let mut dirty = false;

        for _ in 0..steps {
            dirty |= self.pane.forward_mouse_event(
                PaneMouseAction::Press,
                Some(button),
                local_x,
                local_y,
                modifiers,
                self.mouse.held_button.is_some(),
            )?;
            dirty |= self.pane.forward_mouse_event(
                PaneMouseAction::Release,
                Some(button),
                local_x,
                local_y,
                modifiers,
                self.mouse.held_button.is_some(),
            )?;
        }

        Ok(PaneEventOutcome {
            dirty,
            force_redraw: false,
            command: None,
        })
    }

    fn forward_terminal_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        let Some(button) = pane_mouse_button(button) else {
            return Ok(PaneEventOutcome::default());
        };

        let action = if state == ElementState::Pressed {
            self.mouse.held_button = Some(button);
            PaneMouseAction::Press
        } else {
            if self.mouse.held_button == Some(button) {
                self.mouse.held_button = None;
            }
            PaneMouseAction::Release
        };

        let dirty = self.pane.forward_mouse_event(
            action,
            Some(button),
            local_x,
            local_y,
            pane_mouse_modifiers(modifiers),
            self.mouse.held_button.is_some(),
        )?;

        Ok(PaneEventOutcome {
            dirty,
            force_redraw: false,
            command: None,
        })
    }

    fn handle_local_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        local_x: usize,
        local_y: usize,
        working_directory: &Path,
    ) -> PaneEventOutcome {
        if button != MouseButton::Left {
            return PaneEventOutcome::default();
        }

        let is_down = state == ElementState::Pressed;
        let just_pressed = is_down && self.mouse.held_button != Some(PaneMouseButton::Left);
        let just_released = !is_down && self.mouse.held_button == Some(PaneMouseButton::Left);
        if is_down {
            self.mouse.held_button = Some(PaneMouseButton::Left);
        } else {
            self.mouse.held_button = None;
        }

        let mut outcome = PaneEventOutcome::default();
        if just_pressed {
            outcome.dirty |= self.start_selection(local_x, local_y);
            if self.role == PaneRole::AgentTerminal {
                outcome.dirty |= self.forward_reference_click(
                    local_x,
                    local_y,
                    working_directory,
                    &mut outcome.command,
                );
            }
        }
        if just_released {
            outcome.dirty |= self.pane.copy_selection_to_clipboard();
        }

        outcome
    }

    fn start_selection(&mut self, local_x: usize, local_y: usize) -> bool {
        let metrics = self.pane.metrics();
        self.pane
            .begin_selection(local_y / metrics.cell_height, local_x / metrics.cell_width)
    }

    fn forward_reference_click(
        &mut self,
        local_x: usize,
        local_y: usize,
        working_directory: &Path,
        command: &mut Option<PaneCommand>,
    ) -> bool {
        let metrics = self.pane.metrics();
        let row = local_y / metrics.cell_height;
        let column = local_x / metrics.cell_width;
        let click = ReferenceClick {
            at: Instant::now(),
            row,
            column,
        };
        let is_double_click = is_double_click(
            self.mouse.last_reference_click.as_ref(),
            &click,
            DOUBLE_CLICK_THRESHOLD,
        );
        self.mouse.last_reference_click = Some(click);

        let Some(target) = self.pane.reference_at(row, column, working_directory) else {
            return false;
        };

        match target {
            ReferenceTarget::File(target) => {
                if !is_double_click {
                    return false;
                }

                info!(
                    path = %target.path.display(),
                    line = ?target.line,
                    "file reference double clicked"
                );
                *command = Some(PaneCommand::OpenRequested {
                    path: target.path,
                    line: target.line,
                });
                true
            }
            ReferenceTarget::Symbol(symbol) => {
                if is_double_click {
                    return false;
                }

                info!(symbol = %symbol, "symbol reference clicked");
                *command = Some(PaneCommand::SymbolOpenRequested { symbol });
                true
            }
        }
    }
}

impl Deref for TerminalPaneController {
    type Target = TerminalPane;

    fn deref(&self) -> &Self::Target {
        &self.pane
    }
}

impl DerefMut for TerminalPaneController {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pane
    }
}

impl TerminalPaneController {
    /// Agent panes always use Page/Home/End for scrollback. Editor/review panes forward those
    /// keys to the running program (e.g. Neovim) while pinned to the live bottom; after the user
    /// scrolls into history, the same keys move the viewport again.
    fn intercepts_viewport_navigation_keys(&self) -> bool {
        match self.role {
            PaneRole::AgentTerminal => true,
            PaneRole::ReviewTerminal => false,
            PaneRole::Editor => !self.pane.is_viewport_at_bottom(),
        }
    }
}

fn viewport_scroll_delta(scroll_delta: (f32, f32)) -> Option<isize> {
    let (sx, sy) = scroll_delta;
    let sy = sy + sx;

    if sy.abs() <= 1e-4 {
        return None;
    }

    let scaled = -sy / 40.0;
    let mut delta_y = scaled.round().clamp(-64.0, 64.0) as isize;
    if delta_y == 0 {
        delta_y = -sy.signum() as isize;
    }
    Some(delta_y)
}

fn pane_mouse_button(button: MouseButton) -> Option<PaneMouseButton> {
    match button {
        MouseButton::Left => Some(PaneMouseButton::Left),
        MouseButton::Middle => Some(PaneMouseButton::Middle),
        MouseButton::Right => Some(PaneMouseButton::Right),
        _ => None,
    }
}

fn pane_mouse_modifiers(modifiers: Modifiers) -> PaneMouseModifiers {
    PaneMouseModifiers {
        ctrl: modifiers.ctrl,
        shift: modifiers.shift,
        alt: modifiers.alt,
        super_key: modifiers.super_key,
    }
}

fn is_double_click(
    previous: Option<&ReferenceClick>,
    current: &ReferenceClick,
    threshold: Duration,
) -> bool {
    previous.is_some_and(|previous| {
        previous.row == current.row
            && previous.column == current.column
            && current.at.saturating_duration_since(previous.at) <= threshold
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::config::{TerminalMetrics, TerminalTheme};
    use super::*;

    #[test]
    fn classifies_double_click_when_same_cell_within_threshold() {
        let first = ReferenceClick {
            at: Instant::now(),
            row: 3,
            column: 12,
        };
        let second = ReferenceClick {
            at: first.at + Duration::from_millis(120),
            row: 3,
            column: 12,
        };

        assert!(is_double_click(
            Some(&first),
            &second,
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn rejects_double_click_when_cell_differs() {
        let first = ReferenceClick {
            at: Instant::now(),
            row: 3,
            column: 12,
        };
        let different_cell = ReferenceClick {
            at: first.at + Duration::from_millis(120),
            row: 4,
            column: 12,
        };

        assert!(!is_double_click(
            Some(&first),
            &different_cell,
            Duration::from_millis(300)
        ));
    }

    #[test]
    fn agent_intercepts_page_keys_for_viewport_scroll() {
        let mut pane = test_controller(PaneRole::AgentTerminal);
        let outcome = pane
            .handle_terminal_key(
                &[Key::PageUp],
                Some(Key::PageUp),
                None,
                false,
                Modifiers::default(),
                false,
            )
            .unwrap();
        assert!(outcome.force_redraw);
    }

    #[test]
    fn editor_forwards_page_keys_at_live_bottom() {
        let mut pane = test_controller(PaneRole::Editor);
        let outcome = pane
            .handle_terminal_key(
                &[Key::PageUp],
                Some(Key::PageUp),
                None,
                false,
                Modifiers::default(),
                false,
            )
            .unwrap();
        assert!(outcome.dirty);
        assert!(!outcome.force_redraw);
    }

    #[test]
    fn editor_intercepts_page_keys_when_scrolled_into_scrollback() {
        let mut pane = test_controller(PaneRole::Editor);
        for index in 1..=10 {
            pane.process_for_test(format!("L{index:02}\r\n").as_bytes(), true);
        }
        pane.scroll_viewport(-2);
        assert!(!pane.is_viewport_at_bottom());

        let outcome = pane
            .handle_terminal_key(
                &[Key::PageUp],
                Some(Key::PageUp),
                None,
                false,
                Modifiers::default(),
                false,
            )
            .unwrap();
        assert!(outcome.force_redraw);
    }

    #[test]
    fn converts_scroll_wheel_to_viewport_delta() {
        assert_eq!(viewport_scroll_delta((0.0, 40.0)), Some(-1));
        assert_eq!(viewport_scroll_delta((0.0, -40.0)), Some(1));
        assert_eq!(viewport_scroll_delta((0.0, 0.0)), None);
    }

    #[test]
    fn agent_role_opens_symbol_references() {
        let mut pane = test_controller(PaneRole::AgentTerminal);
        pane.process_for_test(b"see Foo::bar here", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                5 * test_metrics().cell_width,
                0,
                Modifiers::default(),
                Path::new("/repo"),
            )
            .unwrap();

        assert!(matches!(
            outcome.command,
            Some(PaneCommand::SymbolOpenRequested { ref symbol }) if symbol == "Foo::bar"
        ));
    }

    #[test]
    fn editor_role_ignores_symbol_references() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"see Foo::bar here", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                5 * test_metrics().cell_width,
                0,
                Modifiers::default(),
                Path::new("/repo"),
            )
            .unwrap();

        assert!(outcome.command.is_none());
    }

    #[test]
    fn agent_role_opens_file_references_on_double_click() {
        let mut pane = test_controller(PaneRole::AgentTerminal);
        pane.process_for_test(b"open src/main.rs:42", true);
        let x = 6 * test_metrics().cell_width;

        let first = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                Modifiers::default(),
                Path::new("/repo"),
            )
            .unwrap();
        pane.handle_mouse_input(
            ElementState::Released,
            MouseButton::Left,
            x,
            0,
            Modifiers::default(),
            Path::new("/repo"),
        )
        .unwrap();
        let second = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                Modifiers::default(),
                Path::new("/repo"),
            )
            .unwrap();

        assert!(first.command.is_none());
        assert!(matches!(
            second.command,
            Some(PaneCommand::OpenRequested { ref path, line })
                if path == Path::new("/repo/src/main.rs") && line == Some(42)
        ));
    }

    #[test]
    fn review_role_tracks_drag_button_until_release() {
        let mut pane = test_controller(PaneRole::ReviewTerminal);

        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            0,
            0,
            Modifiers::default(),
            Path::new("/repo"),
        )
        .unwrap();
        assert_eq!(pane.held_button_for_test(), Some(PaneMouseButton::Left));

        pane.handle_mouse_motion(10, 10, Modifiers::default())
            .unwrap();
        assert_eq!(pane.held_button_for_test(), Some(PaneMouseButton::Left));

        pane.handle_mouse_input(
            ElementState::Released,
            MouseButton::Left,
            10,
            10,
            Modifiers::default(),
            Path::new("/repo"),
        )
        .unwrap();
        assert_eq!(pane.held_button_for_test(), None);
    }

    #[test]
    fn review_wheel_does_not_clear_held_drag_button() {
        let mut pane = test_controller(PaneRole::ReviewTerminal);

        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            0,
            0,
            Modifiers::default(),
            Path::new("/repo"),
        )
        .unwrap();
        pane.handle_mouse_wheel((0.0, 40.0), 0, 0, Modifiers::default())
            .unwrap();

        assert_eq!(pane.held_button_for_test(), Some(PaneMouseButton::Left));
    }

    #[test]
    fn local_wheel_falls_back_to_viewport_without_mouse_reporting() {
        for role in [PaneRole::Editor, PaneRole::AgentTerminal] {
            let mut pane = test_controller(role);

            let outcome = pane
                .handle_mouse_wheel((0.0, 40.0), 0, 0, Modifiers::default())
                .unwrap();

            assert!(outcome.dirty);
            assert!(outcome.force_redraw);
        }
    }

    #[test]
    fn local_wheel_forwards_when_mouse_reporting_is_enabled() {
        for role in [PaneRole::Editor, PaneRole::AgentTerminal] {
            let mut pane = test_controller(role);
            pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

            let outcome = pane
                .handle_mouse_wheel((0.0, 40.0), 0, 0, Modifiers::default())
                .unwrap();

            assert!(outcome.dirty);
            assert!(!outcome.force_redraw);
        }
    }

    fn test_controller(role: PaneRole) -> TerminalPaneController {
        TerminalPaneController::new(
            role,
            TerminalPane::new("test", 400, 100, test_metrics(), &TerminalTheme::default()).unwrap(),
        )
    }

    fn test_metrics() -> TerminalMetrics {
        TerminalMetrics {
            cell_width: 10,
            cell_height: 20,
            baseline: 15,
        }
    }
}
