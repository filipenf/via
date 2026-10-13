use std::ops::{Deref, DerefMut};

use anyhow::Result;
use tracing::info;
use winit::event::{ElementState, MouseButton};

use super::input::{
    Key, Modifiers, copy_requested, forward_special_keys, forward_text_input, paste_requested,
    try_clipboard_paste,
};
use super::links::{ReferenceContext, ReferenceTarget};
use super::pane::{PaneMouseAction, PaneMouseButton, TerminalPane};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PaneRole {
    Editor,
    AgentTerminal {
        id: String,
        label: String,
        /// Launch / ACP command recorded in the agent registry (not the TUI binary argv).
        command: Option<String>,
        /// PTY child is `via --acp-tui`; mediator owns the ACP session.
        acp_tui: bool,
    },
    ReviewTerminal,
}

#[derive(Debug, Default)]
pub(super) struct PaneEventOutcome {
    pub(super) dirty: bool,
    pub(super) force_redraw: bool,
    pub(super) command: Option<PaneCommand>,
}

#[derive(Debug)]
#[allow(clippy::enum_variant_names)] // Open / SymbolOpen / UrlOpen share a deliberate Requested suffix
pub(super) enum PaneCommand {
    OpenRequested {
        path: std::path::PathBuf,
        line: Option<u32>,
    },
    SymbolOpenRequested {
        symbol: String,
    },
    UrlOpenRequested {
        url: String,
    },
}

#[derive(Default)]
struct MouseState {
    held_button: Option<PaneMouseButton>,
    /// Owner latched at press: App means the gesture was claimed by the
    /// program (forwarded), Local means via-local selection/reference.
    /// Motion/release follow the latched owner instead of re-deciding per
    /// event, so mid-gesture DECSET/mode changes and Shift/Ctrl flips cannot
    /// split a press from its release or steal a drag.
    drag_owner: Option<DragOwner>,
    /// Carried-over fractional scroll input. Pixel-precise devices (touchpads, hi-res wheels) send
    /// many small deltas per notch; we accumulate them and only emit whole wheel steps so programs
    /// (and local scrollback) move one line per notch instead of a burst per event.
    scroll_accumulator: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragOwner {
    App,
    Local,
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

    pub(super) fn set_scroll_sensitivity(&mut self, scroll_sensitivity: f32) {
        if scroll_sensitivity.is_finite() && scroll_sensitivity > 0.0 {
            self.scroll_sensitivity = scroll_sensitivity;
        }
    }

    pub(super) fn role(&self) -> &PaneRole {
        &self.role
    }

    pub(super) fn has_exited(&mut self) -> bool {
        matches!(self.role, PaneRole::AgentTerminal { .. }) && self.pane.child_has_exited()
    }

    pub(super) fn editor_has_exited(&mut self) -> bool {
        matches!(self.role, PaneRole::Editor) && self.pane.child_has_exited()
    }

    pub(super) fn terminate_agent(&mut self) -> Result<()> {
        if matches!(self.role, PaneRole::AgentTerminal { .. }) {
            self.pane.terminate_child()?;
        }
        Ok(())
    }

    pub(super) fn command(&self) -> Option<&str> {
        match &self.role {
            PaneRole::AgentTerminal { command, .. } => command.as_deref(),
            _ => None,
        }
    }

    pub(super) fn is_acp_tui(&self) -> bool {
        matches!(self.role, PaneRole::AgentTerminal { acp_tui: true, .. })
    }

    pub(super) fn agent_id(&self) -> Option<&str> {
        match &self.role {
            PaneRole::AgentTerminal { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    #[cfg(test)]
    fn held_button_for_test(&self) -> Option<PaneMouseButton> {
        self.mouse.held_button
    }

    #[cfg(test)]
    fn drag_owner_for_test(&self) -> Option<DragOwner> {
        self.mouse.drag_owner
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

    pub(super) fn handle_modifiers_changed(
        &mut self,
        modifiers: Modifiers,
        ctx: ReferenceContext<'_>,
    ) -> PaneEventOutcome {
        if matches!(self.role, PaneRole::Editor) {
            return PaneEventOutcome::default();
        }
        let dirty = self.pane.set_reference_cues_enabled(modifiers.ctrl, ctx);
        PaneEventOutcome {
            dirty,
            force_redraw: dirty,
            command: None,
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
        ctx: ReferenceContext<'_>,
    ) -> Result<PaneEventOutcome> {
        match self.role {
            PaneRole::ReviewTerminal => {
                self.forward_terminal_mouse_input(state, button, local_x, local_y, modifiers)
            }
            PaneRole::Editor | PaneRole::AgentTerminal { .. } => {
                self.handle_app_aware_mouse_input(state, button, local_x, local_y, modifiers, ctx)
            }
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
                    modifiers,
                    true,
                )?;
                Ok(PaneEventOutcome {
                    dirty,
                    force_redraw: false,
                    command: None,
                })
            }
            PaneRole::Editor | PaneRole::AgentTerminal { .. } => {
                let Some(button) = self.mouse.held_button else {
                    return Ok(PaneEventOutcome::default());
                };
                match self.mouse.drag_owner {
                    Some(DragOwner::App) => {
                        let forwarded = self.pane.forward_mouse_event(
                            PaneMouseAction::Motion,
                            Some(button),
                            local_x,
                            local_y,
                            modifiers,
                            true,
                        )?;
                        // App-owned drag stays App even if the program turned
                        // mouse reporting off mid-gesture: there is no local
                        // anchor (press was forwarded), so there is nothing to
                        // fall back to.
                        return Ok(PaneEventOutcome {
                            dirty: forwarded,
                            force_redraw: false,
                            command: None,
                        });
                    }
                    Some(DragOwner::Local) => {
                        // Local drag stays local even if the program enabled
                        // mouse reporting mid-gesture, so the selection is
                        // never stolen and the release still copies.
                    }
                    None => {
                        // Legacy state (should not happen): keep the old
                        // forward-then-fallback shape, minus the Shift bypass
                        // which is now latched at press.
                        let forwarded = self.pane.forward_mouse_event(
                            PaneMouseAction::Motion,
                            Some(button),
                            local_x,
                            local_y,
                            modifiers,
                            true,
                        )?;
                        if forwarded {
                            return Ok(PaneEventOutcome {
                                dirty: true,
                                force_redraw: false,
                                command: None,
                            });
                        }
                    }
                }
                if button != PaneMouseButton::Left {
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
            modifiers,
            self.mouse.held_button.is_some(),
        )?;

        Ok(PaneEventOutcome {
            dirty,
            force_redraw: false,
            command: None,
        })
    }

    /// Editor and agent panes forward mouse events to the running program
    /// (Neovim tabs, agent TUI clicks, ...) whenever it enabled mouse
    /// reporting (e.g. `DECSET 1000` + SGR `1006`, same signal the wheel path
    /// already uses). Only when the program did not claim the event do we fall
    /// back to via-local selection / Ctrl-click reference handling.
    ///
    /// The gesture owner is latched at press: motion and release follow
    /// `drag_owner` instead of re-deciding per event, so Shift/Ctrl flips or
    /// DECSET changes mid-gesture cannot orphan a release or steal a drag.
    /// Shift bypasses application mouse so users can always drag-select; Ctrl
    /// on an agent pane still prefers reference navigation when a reference
    /// exists under the cursor.
    fn handle_app_aware_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
        ctx: ReferenceContext<'_>,
    ) -> Result<PaneEventOutcome> {
        let Some(pane_button) = pane_mouse_button(button) else {
            return Ok(PaneEventOutcome::default());
        };
        let is_down = state == ElementState::Pressed;

        if !is_down {
            return self.handle_app_aware_mouse_release(pane_button, local_x, local_y, modifiers);
        }

        // Press: latch the owner.
        if modifiers.shift {
            return Ok(self.local_press(pane_button, local_x, local_y, modifiers, ctx));
        }

        // Ctrl-click reference navigation wins when a reference exists; the
        // single lookup feeds the command directly so release/motion stay
        // Local via the latched owner. Otherwise the click still belongs to
        // the application (or to local selection).
        if pane_button == PaneMouseButton::Left
            && matches!(self.role, PaneRole::AgentTerminal { .. })
            && modifiers.ctrl
        {
            let metrics = self.pane.metrics();
            if let Some(target) = self.pane.reference_at(
                local_y / metrics.cell_height,
                local_x / metrics.cell_width,
                ctx,
            ) {
                self.mouse.held_button = Some(PaneMouseButton::Left);
                self.mouse.drag_owner = Some(DragOwner::Local);
                let mut outcome = PaneEventOutcome::default();
                outcome.dirty = true;
                outcome.command = Some(command_from_reference_target(target));
                return Ok(outcome);
            }
        }

        let forwarded = self.pane.forward_mouse_event(
            PaneMouseAction::Press,
            Some(pane_button),
            local_x,
            local_y,
            modifiers,
            true,
        )?;
        if forwarded {
            self.mouse.held_button = Some(pane_button);
            self.mouse.drag_owner = Some(DragOwner::App);
            let mut dirty = true;
            dirty |= self.pane.clear_selection();
            return Ok(PaneEventOutcome {
                dirty,
                force_redraw: false,
                command: None,
            });
        }

        Ok(self.local_press(pane_button, local_x, local_y, modifiers, ctx))
    }

    /// Release follows the owner latched at press. App-owned releases are
    /// always forwarded (even with Shift held, so the program sees button-up)
    /// and clear the gesture even when the program already turned mouse
    /// reporting off. Local-owned releases copy the selection. Releases with
    /// no latched gesture are spurious and send nothing.
    fn handle_app_aware_mouse_release(
        &mut self,
        button: PaneMouseButton,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        let Some(held) = self.mouse.held_button else {
            return Ok(PaneEventOutcome::default());
        };
        if held != button {
            return Ok(PaneEventOutcome::default());
        }
        match self.mouse.drag_owner {
            Some(DragOwner::App) => {
                let forwarded = self.pane.forward_mouse_event(
                    PaneMouseAction::Release,
                    Some(button),
                    local_x,
                    local_y,
                    modifiers,
                    false,
                )?;
                self.mouse.held_button = None;
                self.mouse.drag_owner = None;
                let mut dirty = forwarded;
                dirty |= self.pane.clear_selection();
                Ok(PaneEventOutcome {
                    dirty,
                    force_redraw: false,
                    command: None,
                })
            }
            Some(DragOwner::Local) => {
                self.mouse.held_button = None;
                self.mouse.drag_owner = None;
                Ok(PaneEventOutcome {
                    dirty: if button == PaneMouseButton::Left {
                        self.pane.copy_selection_to_clipboard()
                    } else {
                        false
                    },
                    force_redraw: false,
                    command: None,
                })
            }
            None => {
                // No owner (should not happen): fall back to a forward attempt
                // so Review-style callers still deliver button-up, then clear.
                let forwarded = self.pane.forward_mouse_event(
                    PaneMouseAction::Release,
                    Some(button),
                    local_x,
                    local_y,
                    modifiers,
                    false,
                )?;
                self.mouse.held_button = None;
                Ok(PaneEventOutcome {
                    dirty: forwarded,
                    force_redraw: false,
                    command: None,
                })
            }
        }
    }

    /// Local press path: latches `Local`, starts a selection unless this is a
    /// Ctrl reference click (which resolves the reference with one lookup).
    /// Non-Left presses are ignored without touching the latched gesture so a
    /// stray button cannot clobber an in-progress Left drag.
    fn local_press(
        &mut self,
        button: PaneMouseButton,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
        ctx: ReferenceContext<'_>,
    ) -> PaneEventOutcome {
        if button != PaneMouseButton::Left {
            return PaneEventOutcome::default();
        }
        // A repeated press without an intervening release carries no new
        // gesture; keep the latched owner.
        if self.mouse.held_button == Some(PaneMouseButton::Left) {
            return PaneEventOutcome::default();
        }
        self.mouse.held_button = Some(PaneMouseButton::Left);
        self.mouse.drag_owner = Some(DragOwner::Local);

        let mut outcome = PaneEventOutcome::default();
        let is_reference_click =
            matches!(self.role, PaneRole::AgentTerminal { .. }) && modifiers.ctrl;
        if !is_reference_click {
            outcome.dirty |= self.start_selection(local_x, local_y);
        }
        if is_reference_click {
            outcome.dirty |=
                self.forward_reference_click(local_x, local_y, ctx, &mut outcome.command);
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
        ctx: ReferenceContext<'_>,
        command: &mut Option<PaneCommand>,
    ) -> bool {
        // Single lookup per click: the press path that already resolved the
        // target builds the command via `command_from_reference_target`
        // instead of calling back here.
        let metrics = self.pane.metrics();
        let row = local_y / metrics.cell_height;
        let column = local_x / metrics.cell_width;

        let Some(target) = self.pane.reference_at(row, column, ctx) else {
            return false;
        };

        match target {
            ReferenceTarget::File(target) => {
                info!(
                    path = %target.path.display(),
                    line = ?target.line,
                    "file reference ctrl-clicked"
                );
                *command = Some(PaneCommand::OpenRequested {
                    path: target.path,
                    line: target.line,
                });
                true
            }
            ReferenceTarget::Symbol(symbol) => {
                info!(symbol = %symbol, "symbol reference ctrl-clicked");
                *command = Some(PaneCommand::SymbolOpenRequested { symbol });
                true
            }
            ReferenceTarget::Url(url) => {
                info!(%url, "url reference ctrl-clicked");
                *command = Some(PaneCommand::UrlOpenRequested { url });
                true
            }
        }
    }
}

fn command_from_reference_target(target: ReferenceTarget) -> PaneCommand {
    match target {
        ReferenceTarget::File(target) => {
            info!(
                path = %target.path.display(),
                line = ?target.line,
                "file reference ctrl-clicked"
            );
            PaneCommand::OpenRequested {
                path: target.path,
                line: target.line,
            }
        }
        ReferenceTarget::Symbol(symbol) => {
            info!(symbol = %symbol, "symbol reference ctrl-clicked");
            PaneCommand::SymbolOpenRequested { symbol }
        }
        ReferenceTarget::Url(url) => {
            info!(%url, "url reference ctrl-clicked");
            PaneCommand::UrlOpenRequested { url }
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::config::{TerminalMetrics, TerminalTheme};
    use super::*;

    fn ctrl_modifiers() -> Modifiers {
        Modifiers {
            ctrl: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn agent_intercepts_page_keys_for_viewport_scroll() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
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
            command: None,
            acp_tui: false,
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
            command: None,
            acp_tui: false,
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
            command: None,
            acp_tui: false,
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
            command: None,
            acp_tui: false,
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
                command: None,
                acp_tui: false,
            },
            0.5,
        );
        // Half sensitivity needs two notches for one step.
        assert_eq!(pane.accumulate_wheel_steps((0.0, 40.0)), 0);
        assert_eq!(pane.accumulate_wheel_steps((0.0, 40.0)), 1);
    }

    #[test]
    fn agent_role_opens_symbol_references_on_ctrl_click() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"see Foo::bar here", true);

        let without_ctrl = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                5 * test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(without_ctrl.command.is_none());
        pane.handle_mouse_input(
            ElementState::Released,
            MouseButton::Left,
            5 * test_metrics().cell_width,
            0,
            Modifiers::default(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                5 * test_metrics().cell_width,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
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
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(outcome.command.is_none());
    }

    #[test]
    fn review_role_updates_reference_cues_on_modifier_change() {
        let mut pane = test_controller(PaneRole::ReviewTerminal);
        pane.process_for_test(b"open src/main.rs:42", true);

        let outcome = pane.handle_modifiers_changed(
            ctrl_modifiers(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        );
        assert!(outcome.dirty);
        assert!(outcome.force_redraw);

        let outcome = pane.handle_modifiers_changed(
            Modifiers::default(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        );
        assert!(outcome.dirty);
        assert!(outcome.force_redraw);
    }

    #[test]
    fn agent_role_opens_file_references_on_ctrl_click() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"open src/main.rs:42", true);
        let x = 6 * test_metrics().cell_width;

        let without_ctrl = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(without_ctrl.command.is_none());
        pane.handle_mouse_input(
            ElementState::Released,
            MouseButton::Left,
            x,
            0,
            Modifiers::default(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(matches!(
            outcome.command,
            Some(PaneCommand::OpenRequested { ref path, line })
                if path == Path::new("/repo/src/main.rs") && line == Some(42)
        ));
    }

    #[test]
    fn agent_role_opens_http_url_from_row_text_on_ctrl_click() {
        let mut pane = TerminalPaneController::new(
            PaneRole::AgentTerminal {
                id: "agent".to_string(),
                label: "agent".to_string(),
                command: None,
                acp_tui: false,
            },
            TerminalPane::new("agent", 400, 100, test_metrics(), &TerminalTheme::default())
                .unwrap(),
            1.0,
        );
        pane.process_for_test(b"see https://example.com/path for info", true);

        let col = "see https://example.com/path for info"
            .find("example")
            .unwrap()
            * test_metrics().cell_width;

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                col,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(matches!(
            outcome.command,
            Some(PaneCommand::UrlOpenRequested { ref url })
                if url == "https://example.com/path"
        ));
    }

    #[test]
    fn agent_role_opens_http_osc8_links_on_ctrl_click() {
        let mut pane = TerminalPaneController::new(
            PaneRole::AgentTerminal {
                id: "agent".to_string(),
                label: "agent".to_string(),
                command: None,
                acp_tui: false,
            },
            TerminalPane::new("agent", 400, 100, test_metrics(), &TerminalTheme::default())
                .unwrap(),
            1.0,
        );
        pane.process_for_test(
            b"\x1b]8;;https://example.com/path\x1b\\link\x1b]8;;\x1b\\",
            true,
        );

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(matches!(
            outcome.command,
            Some(PaneCommand::UrlOpenRequested { ref url })
                if url == "https://example.com/path"
        ));
    }

    #[test]
    fn agent_role_ignores_shift_click_references() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"open src/main.rs:42", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                6 * test_metrics().cell_width,
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(outcome.command.is_none());
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
            ReferenceContext::cwd_only(Path::new("/repo")),
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
            ReferenceContext::cwd_only(Path::new("/repo")),
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
            ReferenceContext::cwd_only(Path::new("/repo")),
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
                command: None,
                acp_tui: false,
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
                command: None,
                acp_tui: false,
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

    #[test]
    fn editor_forwards_click_when_mouse_reporting_is_enabled() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(outcome.dirty);
        assert!(outcome.command.is_none());
        assert_eq!(pane.held_button_for_test(), Some(PaneMouseButton::Left));
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));

        let release = pane
            .handle_mouse_input(
                ElementState::Released,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(release.dirty);
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
    }

    #[test]
    fn editor_falls_back_to_selection_without_mouse_reporting() {
        let mut pane = test_controller(PaneRole::Editor);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(outcome.dirty);
        assert_eq!(pane.held_button_for_test(), Some(PaneMouseButton::Left));
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));

        // Drag without mouse reporting updates the local selection.
        let motion = pane
            .handle_mouse_motion(3 * test_metrics().cell_width, 0, Modifiers::default())
            .unwrap();
        assert!(motion.dirty);
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));
    }

    #[test]
    fn agent_forwards_click_when_mouse_reporting_is_enabled() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(outcome.dirty);
        assert!(outcome.command.is_none());
        // Owner proves the program claimed the press; local selection is also dirty.
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));
    }

    #[test]
    fn right_click_forwards_and_release_clears_when_mouse_reporting_is_enabled() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        let press = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Right,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(press.dirty);
        assert_eq!(pane.held_button_for_test(), Some(PaneMouseButton::Right));
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));

        let release = pane
            .handle_mouse_input(
                ElementState::Released,
                MouseButton::Right,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(release.dirty);
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
    }

    #[test]
    fn non_left_press_without_mouse_reporting_leaves_gesture_alone() {
        let mut pane = test_controller(PaneRole::Editor);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Right,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(!outcome.dirty);
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
    }

    #[test]
    fn spurious_release_sends_nothing() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        let release = pane
            .handle_mouse_input(
                ElementState::Released,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(!release.dirty);
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
    }

    #[test]
    fn shift_click_bypasses_mouse_reporting_for_selection() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        // Shift means local selection even though the program owns the mouse.
        assert!(outcome.dirty);
        assert!(outcome.command.is_none());
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));

        let motion = pane
            .handle_mouse_motion(
                3 * test_metrics().cell_width,
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
            )
            .unwrap();
        assert!(motion.dirty);
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));
    }

    #[test]
    fn app_press_then_shift_release_still_delivers_button_up() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            test_metrics().cell_width,
            0,
            Modifiers::default(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));

        // Owner is latched at press, so the release still goes to the program
        // (with Shift as a modifier) instead of switching to local copy.
        let release = pane
            .handle_mouse_input(
                ElementState::Released,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(release.dirty);
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
    }

    #[test]
    fn shift_press_then_plain_release_stays_local() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            test_metrics().cell_width,
            0,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));

        // No orphan release is forwarded to the program; the gesture stays
        // local even though Shift was lifted before button-up. A forwarded
        // release is dirty; the Local point-selection copy is not.
        let release = pane
            .handle_mouse_input(
                ElementState::Released,
                MouseButton::Left,
                3 * test_metrics().cell_width,
                0,
                Modifiers::default(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
        assert!(release.command.is_none());
        assert!(!release.dirty);
    }

    #[test]
    fn ctrl_reference_click_wins_over_mouse_reporting() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"open src/main.rs:42", true);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);
        let x = 6 * test_metrics().cell_width;

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                x,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(matches!(
            outcome.command,
            Some(PaneCommand::OpenRequested { .. })
        ));
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));
    }

    #[test]
    fn ctrl_reference_release_does_not_forward_orphan() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"open src/main.rs:42", true);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h", true);
        let x = 6 * test_metrics().cell_width;

        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            x,
            0,
            ctrl_modifiers(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));

        // Motion and release stay Local: no press was ever forwarded, so the
        // program must not see motion/button-up for this gesture. No anchor
        // was set (reference clicks don't select), so Local motion/release
        // are not dirty while a 1002 forward would be.
        let motion = pane.handle_mouse_motion(x, 0, ctrl_modifiers()).unwrap();
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));
        assert!(!motion.dirty);

        let release = pane
            .handle_mouse_input(
                ElementState::Released,
                MouseButton::Left,
                x,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();
        assert!(release.command.is_none());
        assert!(!release.dirty);
        assert_eq!(pane.held_button_for_test(), None);
        assert_eq!(pane.drag_owner_for_test(), None);
    }

    #[test]
    fn ctrl_click_without_reference_forwards_when_mouse_reporting_is_enabled() {
        let mut pane = test_controller(PaneRole::AgentTerminal {
            id: "agent".to_string(),
            label: "agent".to_string(),
            command: None,
            acp_tui: false,
        });
        pane.process_for_test(b"plain text here", true);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1006h", true);

        let outcome = pane
            .handle_mouse_input(
                ElementState::Pressed,
                MouseButton::Left,
                test_metrics().cell_width,
                0,
                ctrl_modifiers(),
                ReferenceContext::cwd_only(Path::new("/repo")),
            )
            .unwrap();

        assert!(outcome.command.is_none());
        assert!(outcome.dirty);
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));
    }

    #[test]
    fn drag_motion_forwards_when_mouse_reporting_is_enabled() {
        let mut pane = test_controller(PaneRole::Editor);
        pane.process_for_test(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h", true);
        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            0,
            0,
            Modifiers::default(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));

        let motion = pane
            .handle_mouse_motion(10, 10, Modifiers::default())
            .unwrap();
        assert!(motion.dirty);
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::App));
    }

    #[test]
    fn local_drag_stays_local_when_mouse_reporting_turns_on_mid_gesture() {
        let mut pane = test_controller(PaneRole::Editor);

        pane.handle_mouse_input(
            ElementState::Pressed,
            MouseButton::Left,
            0,
            0,
            Modifiers::default(),
            ReferenceContext::cwd_only(Path::new("/repo")),
        )
        .unwrap();
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));

        // Program enables drag reporting mid-gesture: the latched Local owner
        // wins, so the event is never forwarded. A same-cell motion is not
        // dirty on the Local path (anchor == focus) but is dirty as a 1002
        // forward, so !dirty proves the gesture stayed local.
        pane.process_for_test(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h", true);
        let motion = pane
            .handle_mouse_motion(0, 0, Modifiers::default())
            .unwrap();
        assert!(!motion.dirty);
        assert_eq!(pane.drag_owner_for_test(), Some(DragOwner::Local));
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
