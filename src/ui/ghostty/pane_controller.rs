use std::ops::{Deref, DerefMut};
use std::path::Path;

use anyhow::Result;
use tracing::info;
use winit::event::{ElementState, MouseButton};

use super::input::{
    Key, Modifiers, copy_requested, forward_special_keys, forward_text_input, paste_requested,
    try_clipboard_paste,
};
use super::links::ReferenceTarget;
use super::pane::{PaneMouseAction, PaneMouseButton, PaneMouseModifiers, TerminalPane};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PaneRole {
    Editor,
    AgentTerminal { id: String, label: String },
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

#[derive(Default)]
struct MouseState {
    held_button: Option<PaneMouseButton>,
    /// Carried-over fractional scroll input. Pixel-precise devices (touchpads, hi-res wheels) send
    /// many small deltas per notch; we accumulate them and only emit whole wheel steps so programs
    /// (and local scrollback) move one line per notch instead of a burst per event.
    scroll_accumulator: f32,
}

/// Raw scroll units that make up one wheel step (one line / one notch). Winit `LineDelta` is scaled
/// to this in [`super::ghostty`], so a single notch is exactly one step.
const WHEEL_STEP_UNITS: f32 = 40.0;

pub(super) struct TerminalPaneController {
    role: PaneRole,
    pane: TerminalPane,
    mouse: MouseState,
    scroll_sensitivity: f32,
}

impl TerminalPaneController {
    pub(super) fn new(role: PaneRole, pane: TerminalPane, scroll_sensitivity: f32) -> Self {
        let scroll_sensitivity = if scroll_sensitivity.is_finite() && scroll_sensitivity > 0.0 {
            scroll_sensitivity
        } else {
            1.0
        };
        Self {
            role,
            pane,
            mouse: MouseState::default(),
            scroll_sensitivity,
        }
    }

    pub(super) fn role(&self) -> &PaneRole {
        &self.role
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
        let steps = self.accumulate_wheel_steps(scroll_delta);
        if steps == 0 {
            return Ok(PaneEventOutcome::default());
        }

        match self.role {
            PaneRole::ReviewTerminal => {
                self.forward_terminal_wheel_steps(steps, local_x, local_y, modifiers)
            }
            PaneRole::Editor | PaneRole::AgentTerminal { .. } => {
                let forwarded =
                    self.forward_terminal_wheel_steps(steps, local_x, local_y, modifiers)?;
                if forwarded.dirty {
                    return Ok(forwarded);
                }

                self.pane.scroll_viewport(-steps);
                Ok(PaneEventOutcome {
                    dirty: true,
                    force_redraw: true,
                    command: None,
                })
            }
        }
    }

    /// Fold raw scroll deltas into whole wheel steps. Positive result means scroll up (matching
    /// winit's positive `y`). Fractional input is carried in `scroll_accumulator` and a direction
    /// reversal drops the carry so the gesture feels responsive.
    fn accumulate_wheel_steps(&mut self, scroll_delta: (f32, f32)) -> isize {
        let (sx, sy) = scroll_delta;
        let raw = (sy + sx) * self.scroll_sensitivity;
        if raw.abs() <= 1e-4 {
            return 0;
        }

        if self.mouse.scroll_accumulator != 0.0
            && self.mouse.scroll_accumulator.signum() != raw.signum()
        {
            self.mouse.scroll_accumulator = 0.0;
        }
        self.mouse.scroll_accumulator += raw;

        let steps = (self.mouse.scroll_accumulator / WHEEL_STEP_UNITS).trunc();
        if steps != 0.0 {
            self.mouse.scroll_accumulator -= steps * WHEEL_STEP_UNITS;
        }
        steps.clamp(-64.0, 64.0) as isize
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
            PaneRole::Editor | PaneRole::AgentTerminal { .. } => Ok(self.handle_local_mouse_input(
                state,
                button,
                local_x,
                local_y,
                modifiers,
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
            PaneRole::Editor | PaneRole::AgentTerminal { .. } => {
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

    fn forward_terminal_wheel_steps(
        &mut self,
        steps: isize,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        if steps == 0 {
            return Ok(PaneEventOutcome::default());
        }

        let button = if steps > 0 {
            PaneMouseButton::WheelUp
        } else {
            PaneMouseButton::WheelDown
        };
        let count = steps.unsigned_abs().min(64);
        let modifiers = pane_mouse_modifiers(modifiers);
        let mut dirty = false;

        for _ in 0..count {
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
        modifiers: Modifiers,
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
            let is_reference_click =
                matches!(self.role, PaneRole::AgentTerminal { .. }) && modifiers.shift;
            if !is_reference_click {
                outcome.dirty |= self.start_selection(local_x, local_y);
            }
            if is_reference_click {
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

        let Some(target) = self.pane.reference_at(row, column, working_directory) else {
            return false;
        };

        match target {
            ReferenceTarget::File(target) => {
                info!(
                    path = %target.path.display(),
                    line = ?target.line,
                    "file reference shift-clicked"
                );
                *command = Some(PaneCommand::OpenRequested {
                    path: target.path,
                    line: target.line,
                });
                true
            }
            ReferenceTarget::Symbol(symbol) => {
                info!(symbol = %symbol, "symbol reference shift-clicked");
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
    /// Agent panes use Page/Home/End for scrollback while on the primary screen; once a full-screen
    /// program takes over the alternate screen (e.g. opencode) those keys are forwarded so the
    /// program does its own paging. Editor/review panes forward those keys to the running program
    /// (e.g. Neovim) while pinned to the live bottom; after the user scrolls into history, the same
    /// keys move the viewport again.
    fn intercepts_viewport_navigation_keys(&self) -> bool {
        match self.role {
            PaneRole::AgentTerminal { .. } => !self.pane.is_alt_screen(),
            PaneRole::ReviewTerminal => false,
            PaneRole::Editor => !self.pane.is_viewport_at_bottom(),
        }
    }
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::config::{TerminalMetrics, TerminalTheme};
    use super::*;

    fn shift_modifiers() -> Modifiers {
        Modifiers {
            shift: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn agent_intercepts_page_keys_for_viewport_scroll() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
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
    fn agent_forwards_page_keys_on_alt_screen() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
        pane.process_for_test(b"\x1b[?1049h", true);
        assert!(pane.is_alt_screen());

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
    fn single_notch_scrolls_one_line() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
        assert_eq!(pane.accumulate_wheel_steps((0.0, 40.0)), 1);
        assert_eq!(pane.accumulate_wheel_steps((0.0, -40.0)), -1);
        assert_eq!(pane.accumulate_wheel_steps((0.0, 0.0)), 0);
    }

    #[test]
    fn small_pixel_deltas_accumulate_into_one_step() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
        // Four sub-threshold pixel events that together cross one step.
        assert_eq!(pane.accumulate_wheel_steps((0.0, 10.0)), 0);
        assert_eq!(pane.accumulate_wheel_steps((0.0, 10.0)), 0);
        assert_eq!(pane.accumulate_wheel_steps((0.0, 10.0)), 0);
        assert_eq!(pane.accumulate_wheel_steps((0.0, 10.0)), 1);
    }

    #[test]
    fn direction_reversal_drops_carry() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
        assert_eq!(pane.accumulate_wheel_steps((0.0, 30.0)), 0);
        // Reversing direction should not have to "pay back" the prior carry.
        assert_eq!(pane.accumulate_wheel_steps((0.0, -40.0)), -1);
    }

    #[test]
    fn scroll_sensitivity_scales_steps() {
        let mut pane = test_controller_with_sensitivity(
            PaneRole::AgentTerminal {
                id: "agent".to_string(),
                label: "agent".to_string(),
            },
            0.5,
        );
        // Half sensitivity needs two notches for one step.
        assert_eq!(pane.accumulate_wheel_steps((0.0, 40.0)), 0);
        assert_eq!(pane.accumulate_wheel_steps((0.0, 40.0)), 1);
    }

    #[test]
    fn agent_role_opens_symbol_references_on_shift_click() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
        pane.process_for_test(b"see Foo::bar here", true);

        let without_shift = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                5 * test_metrics().cell_width,
                0,
                Modifiers::default(),
                Path::new("/repo"),
            )
            .unwrap();
        assert!(without_shift.command.is_none());
        pane.handle_mouse_input(
            ElementState::Released,
            MouseButton::Left,
            5 * test_metrics().cell_width,
            0,
            Modifiers::default(),
            Path::new("/repo"),
        )
        .unwrap();

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                5 * test_metrics().cell_width,
                0,
                shift_modifiers(),
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
    fn agent_role_opens_file_references_on_shift_click() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
        });
        pane.process_for_test(b"open src/main.rs:42", true);
        let x = 6 * test_metrics().cell_width;

        let without_shift = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                Modifiers::default(),
                Path::new("/repo"),
            )
            .unwrap();
        assert!(without_shift.command.is_none());
        pane.handle_mouse_input(
            ElementState::Released,
            MouseButton::Left,
            x,
            0,
            Modifiers::default(),
            Path::new("/repo"),
        )
        .unwrap();

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                shift_modifiers(),
                Path::new("/repo"),
            )
            .unwrap();

        assert!(matches!(
            outcome.command,
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
        for role in [
            PaneRole::Editor,
            PaneRole::AgentTerminal {
                id: "agent".to_string(),
                label: "agent".to_string(),
            },
        ] {
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
        for role in [
            PaneRole::Editor,
            PaneRole::AgentTerminal {
                id: "agent".to_string(),
                label: "agent".to_string(),
            },
        ] {
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
        test_controller_with_sensitivity(role, 1.0)
    }

    fn test_controller_with_sensitivity(
        role: PaneRole,
        scroll_sensitivity: f32,
    ) -> TerminalPaneController {
        TerminalPaneController::new(
            role,
            TerminalPane::new("test", 400, 100, test_metrics(), &TerminalTheme::default()).unwrap(),
            scroll_sensitivity,
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
