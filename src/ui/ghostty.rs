use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{debug, error, info, warn};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::config::{Config, ReviewBackend};
use crate::event::{Event, UiCommand, UiEvent};
use crate::mediator::EventSender;
use crate::pty::{CoalescedOutputNotifier, OutputNotifier};

mod acp_modal;
mod acp_pane;
mod config;
mod font;
mod input;
pub mod layout;
pub mod links;
mod pane;
mod pane_controller;
mod render;

use acp_modal::{AcpModalState, render_acp_modal_buffer};
use acp_pane::AcpPane;
use config::{TerminalConfig, TerminalMetrics};
use font::FontRenderer;
use input::{Key, Modifiers, paste_requested, read_clipboard_text};
use layout::{
    PaneLayoutMode, PaneRect, PaneSplitDirection, SplitLayout, SplitLayoutOptions,
    focus_nvim_after_agent_reference, handle_layout_shortcuts, vertical_split_fits,
};
use pane::TerminalPane;
use pane_controller::{PaneCommand, PaneEventOutcome, PaneRole, TerminalPaneController};
use render::{DamageRect, draw_ratatui_buffer};

const INITIAL_WIDTH: usize = 960;
const INITIAL_HEIGHT: usize = 540;
const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const REPEATED_ARROW_REDRAW_INTERVAL: Duration = Duration::from_millis(24);
const INPUT_LAG_WARN_THRESHOLD: Duration = Duration::from_millis(50);
const RENDER_WARN_THRESHOLD: Duration = Duration::from_millis(20);
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(750);

pub struct GhosttyUi {
    config: Config,
    events: EventSender,
    ui_commands: TokioReceiver<UiCommand>,
    pending_agent_write: Option<PendingAgentWrite>,
}

struct PendingAgentWrite {
    ready_at: Instant,
    bytes: Vec<u8>,
}

impl GhosttyUi {
    pub fn new(config: Config, events: EventSender, ui_commands: TokioReceiver<UiCommand>) -> Self {
        Self {
            config,
            events,
            ui_commands,
            pending_agent_write: None,
        }
    }

    pub fn describe_backend(&self) {
        info!(
            "native window host selected; Ghostty surface integration boundary is in ui::ghostty"
        );
    }

    pub fn run(self) -> Result<()> {
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .build()
            .context("failed to create native event loop")?;
        let mut app = WinitGhosttyApp::new(self, event_loop.create_proxy())?;

        event_loop
            .run_app(&mut app)
            .context("native event loop failed")?;

        if let Some(error) = app.error {
            return Err(error);
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    PtyOutput,
}

impl OutputNotifier for EventLoopProxy<UserEvent> {
    fn notify_output(&self) {
        let _ = self.send_event(UserEvent::PtyOutput);
    }
}

struct WinitGhosttyApp {
    config: Config,
    events: EventSender,
    ui_commands: TokioReceiver<UiCommand>,
    pending_agent_write: Option<PendingAgentWrite>,
    output_notifier: CoalescedOutputNotifier<EventLoopProxy<UserEvent>>,
    window: Option<Arc<Window>>,
    window_id: Option<WindowId>,
    softbuffer_context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    terminal_config: TerminalConfig,
    font_renderer: FontRenderer,
    buffer: Vec<u32>,
    damage: Vec<DamageRect>,
    panes: Vec<TerminalPaneController>,
    review_pane: Option<TerminalPaneController>,
    review_active: bool,
    acp_pane: Option<AcpPane>,
    acp_modal: Option<AcpModalState>,
    active_pane: usize,
    pane_layout_mode: PaneLayoutMode,
    pane_split_direction: PaneSplitDirection,
    /// Split was auto-collapsed because the window is too narrow; restore on widen.
    split_collapsed_for_width: bool,
    layout: SplitLayout,
    width: usize,
    height: usize,
    modifiers: Modifiers,
    cursor_position: Option<(usize, usize)>,
    dirty: bool,
    force_redraw: bool,
    next_redraw_at: Instant,
    arrow_repeat_redraw_deferred: bool,
    skip_background_drain_once: bool,
    last_arrow_repeat_at: Option<Instant>,
    next_theme_poll_at: Instant,
    error: Option<anyhow::Error>,
}

impl WinitGhosttyApp {
    fn new(ui: GhosttyUi, event_loop_proxy: EventLoopProxy<UserEvent>) -> Result<Self> {
        let output_notifier = CoalescedOutputNotifier::new(event_loop_proxy);
        let terminal_config = TerminalConfig::load();
        let font_renderer = FontRenderer::new(&terminal_config)?;
        let pane_split_direction = PaneSplitDirection::for_window(INITIAL_WIDTH, INITIAL_HEIGHT);
        let pane_count = pane_count(&ui.config);
        let layout = SplitLayout::for_window(
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            pane_count,
            PaneLayoutMode::Split,
            pane_split_direction,
            SplitLayoutOptions::unbounded(),
        );

        Ok(Self {
            config: ui.config,
            events: ui.events,
            ui_commands: ui.ui_commands,
            pending_agent_write: ui.pending_agent_write,
            output_notifier,
            window: None,
            window_id: None,
            softbuffer_context: None,
            surface: None,
            terminal_config: terminal_config.clone(),
            font_renderer,
            buffer: vec![terminal_config.theme.background; INITIAL_WIDTH * INITIAL_HEIGHT],
            damage: Vec::new(),
            panes: Vec::new(),
            review_pane: None,
            review_active: false,
            acp_pane: None,
            acp_modal: None,
            active_pane: 0,
            pane_layout_mode: PaneLayoutMode::Split,
            pane_split_direction,
            split_collapsed_for_width: false,
            layout,
            width: INITIAL_WIDTH,
            height: INITIAL_HEIGHT,
            modifiers: Modifiers::default(),
            cursor_position: None,
            dirty: true,
            force_redraw: true,
            next_redraw_at: Instant::now(),
            arrow_repeat_redraw_deferred: false,
            skip_background_drain_once: false,
            last_arrow_repeat_at: None,
            next_theme_poll_at: Instant::now(),
            error: None,
        })
    }

    fn initialize_window(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.window.is_some() {
            return Ok(());
        }

        let attributes = WindowAttributes::default()
            .with_title("via")
            .with_resizable(true)
            .with_inner_size(PhysicalSize::new(
                INITIAL_WIDTH as u32,
                INITIAL_HEIGHT as u32,
            ));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .context("failed to create native window")?,
        );
        window.set_ime_allowed(true);

        let context = softbuffer::Context::new(window.clone())
            .map_err(|error| anyhow!("failed to create softbuffer context: {error:?}"))?;
        let mut surface = softbuffer::Surface::new(&context, window.clone())
            .map_err(|error| anyhow!("failed to create softbuffer surface: {error:?}"))?;
        self.update_font_scale(window.scale_factor())?;
        let size = window.inner_size();
        self.resize_surface(&mut surface, size.width, size.height)?;
        self.resize_terminals(size.width as usize, size.height as usize);

        if self.panes.is_empty() {
            self.panes =
                self.create_panes(self.width, self.height, self.terminal_config.metrics)?;
            if self.config.is_acp_agent() {
                let rect = self.layout.pane(1);
                self.acp_pane = Some(AcpPane::new(
                    rect.width,
                    rect.height,
                    self.terminal_config.metrics,
                    &self.terminal_config.theme,
                ));
            }
            let nvim_args = nvim_args(&self.config);
            self.panes[0].spawn(
                &self.config.nvim_command,
                nvim_args,
                &self.config.working_directory,
                self.output_notifier.clone(),
            )?;
            self.relayout();
            info!(panes = self.panes.len(), "native terminal window ready");
        }

        self.window_id = Some(window.id());
        self.window = Some(window);
        self.softbuffer_context = Some(context);
        self.surface = Some(surface);
        self.dirty = true;
        self.request_redraw();
        Ok(())
    }

    fn create_panes(
        &self,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Result<Vec<TerminalPaneController>> {
        let layout = SplitLayout::for_window(
            width,
            height,
            pane_count(&self.config),
            PaneLayoutMode::Split,
            PaneSplitDirection::for_window(width, height),
            self.split_layout_options(),
        );
        let mut panes = vec![TerminalPaneController::new(
            PaneRole::Editor,
            TerminalPane::new(
                "nvim",
                layout.pane(0).width,
                layout.pane(0).height,
                metrics,
                &self.terminal_config.theme,
            )?,
            self.config.scroll_sensitivity,
        )];

        if self.config.agent_command.is_some() && !self.config.is_acp_agent() {
            if let Some(agent_command) = self.config.agent_command.as_deref() {
                let mut pane = TerminalPane::new(
                    "agent",
                    layout.pane(1).width,
                    layout.pane(1).height,
                    metrics,
                    &self.terminal_config.theme,
                )?;
                pane.spawn_shell_command(
                    agent_command,
                    &self.config.working_directory,
                    self.output_notifier.clone(),
                )?;
                panes.push(TerminalPaneController::new(
                    PaneRole::AgentTerminal {
                        id: "orchestrator".to_string(),
                        label: "agent".to_string(),
                    },
                    pane,
                    self.config.scroll_sensitivity,
                ));
            }
        }

        Ok(panes)
    }

    fn spawn_agent_pane(
        &mut self,
        id: &str,
        role: Option<&str>,
        command: Option<&str>,
    ) -> Result<()> {
        // Deduplicate: if a pane with this id already exists, do nothing.
        if self
            .panes
            .iter()
            .any(|p| matches!(p.role(), PaneRole::AgentTerminal { id: existing, .. } if existing == id))
        {
            info!(%id, "spawn_agent called for existing id – ignoring");
            return Ok(());
        }

        let metrics = self.terminal_config.metrics;
        // Use current layout dimensions to size the new pane; relayout will adjust.
        let (w, h) = (self.width, self.height);
        let label = role.unwrap_or(id).to_string();
        // Leak a small string for the static title requirement of TerminalPane.
        let title: &'static str = Box::leak(label.clone().into_boxed_str());
        let mut pane = TerminalPane::new(
            title,
            w / 2,
            h / 2,
            metrics,
            &self.terminal_config.theme,
        )?;
        // Prefer the command passed to SpawnAgent, then the configured agent_command,
        // finally fall back to $SHELL so the pane is at least usable.
        let cmd = command
            .map(|s| s.to_string())
            .or_else(|| self.config.agent_command.clone())
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));
        pane.spawn_shell_command(&cmd, &self.config.working_directory, self.output_notifier.clone())?;
        self.panes.push(TerminalPaneController::new(
            PaneRole::AgentTerminal {
                id: id.to_string(),
                label,
            },
            pane,
            self.config.scroll_sensitivity,
        ));
        // Focus the newly created agent pane and ensure we are in a visible split layout.
        self.active_pane = self.panes.len() - 1;
        self.pane_layout_mode = PaneLayoutMode::Split;
        self.relayout();
        self.dirty = true;
        self.force_redraw = true;
        Ok(())
    }

    fn resize_surface(
        &mut self,
        surface: &mut softbuffer::Surface<Arc<Window>, Arc<Window>>,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let Some(width) = NonZeroU32::new(width) else {
            return Ok(());
        };
        let Some(height) = NonZeroU32::new(height) else {
            return Ok(());
        };

        surface
            .resize(width, height)
            .map_err(|error| anyhow!("failed to resize softbuffer surface: {error:?}"))
    }

    fn resize_window(&mut self, width: u32, height: u32) -> Result<()> {
        if let Some(mut surface) = self.surface.take() {
            self.resize_surface(&mut surface, width, height)?;
            self.surface = Some(surface);
        }

        self.resize_terminals(width as usize, height as usize);
        self.dirty = true;
        self.force_redraw = true;
        self.request_redraw();
        Ok(())
    }

    fn update_font_scale(&mut self, scale_factor: f64) -> Result<()> {
        let previous_metrics = self.terminal_config.metrics;
        self.terminal_config
            .finalize_metrics_for_scale(scale_factor);

        if self.terminal_config.metrics == previous_metrics {
            return Ok(());
        }

        self.font_renderer = FontRenderer::new(&self.terminal_config)?;
        self.relayout();
        self.resize_panes();
        self.dirty = true;
        self.force_redraw = true;
        Ok(())
    }

    fn resize_terminals(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.pane_split_direction =
            PaneSplitDirection::adjust_for_window_resize(self.pane_split_direction, width, height);
        if ensure_buffer_size(
            &mut self.buffer,
            width,
            height,
            self.terminal_config.theme.background,
        ) {
            self.dirty = true;
            self.force_redraw = true;
        }
        self.relayout();
    }

    fn split_layout_options(&self) -> SplitLayoutOptions {
        SplitLayoutOptions {
            cell_width: self.terminal_config.metrics.cell_width,
            agent_pane_cols: self.config.agent_pane_col_limits(),
        }
    }

    fn adjust_layout_mode_for_width(&mut self) {
        if pane_count(&self.config) < 2 {
            return;
        }

        let fits = match (
            self.config.agent_pane_col_limits(),
            self.pane_split_direction,
        ) {
            (Some((agent_min, _)), PaneSplitDirection::Vertical) => vertical_split_fits(
                self.width,
                self.terminal_config.metrics.cell_width,
                agent_min,
            ),
            _ => true,
        };

        if self.pane_layout_mode == PaneLayoutMode::Split && !fits {
            self.pane_layout_mode = PaneLayoutMode::PaneMaximized(0);
            self.split_collapsed_for_width = true;
            return;
        }

        if self.pane_layout_mode == PaneLayoutMode::PaneMaximized(0)
            && self.split_collapsed_for_width
            && fits
        {
            self.pane_layout_mode = PaneLayoutMode::Split;
            self.split_collapsed_for_width = false;
        }
    }

    fn relayout(&mut self) {
        if self.panes.is_empty() {
            return;
        }

        self.adjust_layout_mode_for_width();

        let new_layout = SplitLayout::for_window(
            self.width,
            self.height,
            self.panes.len().max(1),
            self.pane_layout_mode,
            self.pane_split_direction,
            self.split_layout_options(),
        );
        if new_layout == self.layout {
            return;
        }

        self.layout = new_layout;
        self.dirty = true;
        self.force_redraw = true;
        self.resize_panes();
    }

    fn resize_panes(&mut self) {
        let metrics = self.terminal_config.metrics;

        for (index, pane) in self.panes.iter_mut().enumerate() {
            let rect = self.layout.pane(index);
            if rect.width == 0 || rect.height == 0 {
                continue;
            }

            if let Some(size) = pane.resize_with_metrics(rect.width, rect.height, metrics) {
                debug!(pane = pane.title, ?size, "resized terminal pane");
            }
        }

        if let Some(acp_pane) = &mut self.acp_pane {
            let rect = self.layout.pane(1);
            if rect.width != 0 && rect.height != 0 {
                acp_pane.resize_with_metrics(rect.width, rect.height, metrics);
            }
        }

        if let Some(review_pane) = &mut self.review_pane {
            let rect = full_window_rect(self.width, self.height);
            if rect.width != 0 && rect.height != 0 {
                review_pane.resize_with_metrics(rect.width, rect.height, metrics);
            }
        }
    }

    fn handle_acp_modal_winit_key(&mut self, event: &KeyEvent) -> bool {
        let Some(modal) = &mut self.acp_modal else {
            return false;
        };
        if self.modifiers.ctrl || self.modifiers.super_key {
            return false;
        }
        let PhysicalKey::Code(code) = event.physical_key else {
            return false;
        };
        match code {
            KeyCode::Escape => {
                let id = modal.jsonrpc_id.clone();
                let result = modal.result_cancelled();
                self.acp_modal = None;
                self.events
                    .try_send(Event::Ui(UiEvent::AcpJsonRpcResult { id, result }));
                true
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let id = modal.jsonrpc_id.clone();
                let result = modal.result_for_selection(modal.focused);
                self.acp_modal = None;
                self.events
                    .try_send(Event::Ui(UiEvent::AcpJsonRpcResult { id, result }));
                true
            }
            KeyCode::ArrowUp => {
                modal.move_focus(-1);
                true
            }
            KeyCode::ArrowDown => {
                modal.move_focus(1);
                true
            }
            KeyCode::Digit1 | KeyCode::Numpad1 => self.acp_modal_select_digit(0),
            KeyCode::Digit2 | KeyCode::Numpad2 => self.acp_modal_select_digit(1),
            KeyCode::Digit3 | KeyCode::Numpad3 => self.acp_modal_select_digit(2),
            KeyCode::Digit4 | KeyCode::Numpad4 => self.acp_modal_select_digit(3),
            KeyCode::Digit5 | KeyCode::Numpad5 => self.acp_modal_select_digit(4),
            KeyCode::Digit6 | KeyCode::Numpad6 => self.acp_modal_select_digit(5),
            KeyCode::Digit7 | KeyCode::Numpad7 => self.acp_modal_select_digit(6),
            KeyCode::Digit8 | KeyCode::Numpad8 => self.acp_modal_select_digit(7),
            KeyCode::Digit9 | KeyCode::Numpad9 => self.acp_modal_select_digit(8),
            _ => false,
        }
    }

    fn acp_modal_select_digit(&mut self, index: usize) -> bool {
        let Some(modal) = &mut self.acp_modal else {
            return false;
        };
        if index >= modal.options.len() {
            return false;
        }
        let id = modal.jsonrpc_id.clone();
        let result = modal.result_for_selection(index);
        self.acp_modal = None;
        self.events
            .try_send(Event::Ui(UiEvent::AcpJsonRpcResult { id, result }));
        true
    }

    fn handle_key_event(&mut self, event: KeyEvent) -> Result<()> {
        if self.acp_modal.is_some()
            && event.state == ElementState::Pressed
            && self.handle_acp_modal_winit_key(&event)
        {
            self.dirty = true;
            self.force_redraw = true;
            return Ok(());
        }

        if event.state != ElementState::Pressed || self.panes.is_empty() {
            return Ok(());
        }

        self.arrow_repeat_redraw_deferred = event.repeat
            && matches!(
                event.physical_key,
                PhysicalKey::Code(KeyCode::ArrowUp | KeyCode::ArrowDown)
            );
        self.skip_background_drain_once = self.arrow_repeat_redraw_deferred;
        if self.arrow_repeat_redraw_deferred {
            let now = Instant::now();
            if let Some(last_arrow_repeat_at) = self.last_arrow_repeat_at {
                let gap = now.saturating_duration_since(last_arrow_repeat_at);
                if gap > INPUT_LAG_WARN_THRESHOLD {
                    warn!(?gap, "arrow repeat input lag detected");
                }
            }
            self.last_arrow_repeat_at = Some(now);
        } else if !event.repeat {
            self.last_arrow_repeat_at = None;
        }

        let key = match event.physical_key {
            PhysicalKey::Code(code) => Key::from_key_code(code),
            PhysicalKey::Unidentified(_) => None,
        };
        let pressed_keys = key.into_iter().collect::<Vec<_>>();
        if self.handle_review_shortcut(&pressed_keys)? {
            return Ok(());
        }
        if self.review_active {
            if let Some(review_pane) = &mut self.review_pane {
                let outcome = review_pane.handle_terminal_key(
                    &pressed_keys,
                    key,
                    event.text.as_deref(),
                    event.repeat,
                    self.modifiers,
                    false,
                )?;
                self.apply_pane_outcome(outcome);
            } else {
                self.review_active = false;
            }
            return Ok(());
        }
        let layout_shortcut_consumed = handle_layout_shortcuts(
            &pressed_keys,
            self.modifiers.alt,
            self.modifiers.shift,
            self.panes.len(),
            &mut self.pane_layout_mode,
            &mut self.pane_split_direction,
            &mut self.active_pane,
        );
        if layout_shortcut_consumed {
            self.split_collapsed_for_width = false;
            self.relayout();
            self.dirty = true;
            self.force_redraw = true;
        }
        if self.active_pane >= self.panes.len() {
            if !layout_shortcut_consumed {
                if let Some(acp_pane) = &mut self.acp_pane {
                    let step = acp_pane.transcript_viewport_rows().max(1) as isize;
                    for key in pressed_keys.iter().copied() {
                        match key {
                            Key::PageUp => {
                                acp_pane.scroll_transcript(step);
                                self.dirty = true;
                                self.force_redraw = true;
                                return Ok(());
                            }
                            Key::PageDown => {
                                acp_pane.scroll_transcript(-step);
                                self.dirty = true;
                                self.force_redraw = true;
                                return Ok(());
                            }
                            Key::Home => {
                                acp_pane.scroll_transcript_to_top();
                                self.dirty = true;
                                self.force_redraw = true;
                                return Ok(());
                            }
                            Key::End => {
                                acp_pane.scroll_transcript_to_bottom();
                                self.dirty = true;
                                self.force_redraw = true;
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                }
            }

            let paste_requested = !event.repeat
                && key
                    .map(|key| paste_requested(key, self.modifiers))
                    .unwrap_or(false);
            if paste_requested && !layout_shortcut_consumed {
                if let Some(text) = read_clipboard_text() {
                    if let Some(acp_pane) = &mut self.acp_pane {
                        self.dirty |= acp_pane.paste_text(&text);
                    }
                }
                return Ok(());
            }

            self.handle_acp_key_event(key, event.text.as_deref(), layout_shortcut_consumed);
            return Ok(());
        }

        let outcome = self.panes[self.active_pane].handle_terminal_key(
            &pressed_keys,
            key,
            event.text.as_deref(),
            event.repeat,
            self.modifiers,
            layout_shortcut_consumed,
        )?;
        self.apply_pane_outcome(outcome);

        Ok(())
    }

    fn handle_review_shortcut(&mut self, pressed_keys: &[Key]) -> Result<bool> {
        if !self.modifiers.alt || self.modifiers.ctrl || self.modifiers.super_key {
            return Ok(false);
        }
        if !pressed_keys.iter().any(|key| *key == Key::R) {
            return Ok(false);
        }

        match self.config.review_backend {
            ReviewBackend::Hunk => self.toggle_hunk_review()?,
            ReviewBackend::Nvim => self.open_nvim_review(),
        }

        self.dirty = true;
        self.force_redraw = true;
        Ok(true)
    }

    fn toggle_hunk_review(&mut self) -> Result<()> {
        if self.review_active {
            self.review_active = false;
            return Ok(());
        }

        if self.review_pane.is_none() {
            let rect = full_window_rect(self.width, self.height);
            let mut pane = TerminalPane::new(
                "review",
                rect.width,
                rect.height,
                self.terminal_config.metrics,
                &self.terminal_config.theme,
            )?;
            pane.spawn_shell_command(
                hunk_review_command(),
                &self.config.working_directory,
                self.output_notifier.clone(),
            )?;
            let controller = TerminalPaneController::new(
                PaneRole::ReviewTerminal,
                pane,
                self.config.scroll_sensitivity,
            );
            self.review_pane = Some(controller);
        } else {
            self.reload_hunk_review();
        }

        self.review_active = true;
        Ok(())
    }

    fn reload_hunk_review(&self) {
        let repo = self.config.working_directory.clone();
        std::thread::spawn(move || {
            let status = Command::new("hunk")
                .args(["session", "reload", "--repo"])
                .arg(&repo)
                .args(["--", "diff"])
                .current_dir(&repo)
                .status();

            if let Err(error) = status {
                debug!(%error, "failed to reload hunk review");
            }
        });
    }

    fn open_nvim_review(&mut self) {
        self.review_active = false;
        self.pane_layout_mode = PaneLayoutMode::PaneMaximized(0);
        self.active_pane = 0;
        self.relayout();
        self.events.try_send(Event::Ui(UiEvent::ReviewRequested));
    }

    fn handle_acp_key_event(&mut self, key: Option<Key>, text: Option<&str>, suppress_input: bool) {
        if suppress_input {
            return;
        }

        let Some(acp_pane) = &mut self.acp_pane else {
            return;
        };

        if let Some(key) = key {
            if let Some(prompt) = acp_pane.handle_key(key, self.modifiers) {
                self.events
                    .try_send(Event::Ui(UiEvent::AgentPromptSubmitted { text: prompt }));
            }
        }

        if let Some(text) = text.filter(|text| text.chars().all(|ch| !ch.is_control())) {
            self.dirty |= acp_pane.handle_text_input(text, self.modifiers);
        }

        self.dirty = true;
    }

    fn handle_text_commit(&mut self, text: &str) -> Result<()> {
        if self.acp_modal.is_some() {
            return Ok(());
        }
        if self.panes.is_empty() {
            return Ok(());
        }

        if self.review_active {
            if let Some(review_pane) = &mut self.review_pane {
                let outcome = review_pane.handle_text_commit(text, self.modifiers)?;
                self.apply_pane_outcome(outcome);
            } else {
                self.review_active = false;
            }
            return Ok(());
        }

        if self.active_pane >= self.panes.len() {
            if let Some(acp_pane) = &mut self.acp_pane {
                self.dirty |= acp_pane.handle_text_input(text, self.modifiers);
            }
            return Ok(());
        }

        let outcome = self.panes[self.active_pane].handle_text_commit(text, self.modifiers)?;
        self.apply_pane_outcome(outcome);
        Ok(())
    }

    fn handle_mouse_scroll(&mut self, delta: MouseScrollDelta) -> Result<()> {
        if self.panes.is_empty() {
            return Ok(());
        }

        let scroll_delta = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x * 40.0, y * 40.0),
            MouseScrollDelta::PixelDelta(position) => (position.x as f32, position.y as f32),
        };

        if self.review_active {
            if let Some((x, y)) = self.cursor_position {
                if let Some(review_pane) = &mut self.review_pane {
                    let outcome =
                        review_pane.handle_mouse_wheel(scroll_delta, x, y, self.modifiers)?;
                    self.apply_pane_outcome(outcome);
                } else {
                    self.review_active = false;
                }
            }
            return Ok(());
        }

        if let Some(acp_pane) = &mut self.acp_pane {
            if let Some((x, y)) = self.cursor_position {
                if let Some((pane_index, _)) = self.layout.pane_at(x, y) {
                    if pane_index == 1 {
                        let (sx, sy) = scroll_delta;
                        let sy = sy + sx;
                        if sy.abs() > 1e-4 {
                            let scaled = -sy / 40.0;
                            let mut delta_y = scaled.round().clamp(-64.0, 64.0) as isize;
                            if delta_y == 0 {
                                delta_y = -sy.signum() as isize;
                            }
                            acp_pane.scroll_transcript(delta_y);
                            self.dirty = true;
                            self.force_redraw = true;
                        }
                        return Ok(());
                    }
                }
            }
        }

        let Some((x, y)) = self.cursor_position else {
            return Ok(());
        };
        let Some((pane_index, rect)) = self.layout.pane_at(x, y) else {
            return Ok(());
        };
        if let Some(pane) = self.panes.get_mut(pane_index) {
            let outcome =
                pane.handle_mouse_wheel(scroll_delta, x - rect.x, y - rect.y, self.modifiers)?;
            self.apply_pane_outcome(outcome);
        }
        Ok(())
    }

    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) -> Result<()> {
        if self.review_active {
            if let Some((x, y)) = self.cursor_position {
                if let Some(review_pane) = &mut self.review_pane {
                    let outcome = review_pane.handle_mouse_input(
                        state,
                        button,
                        x,
                        y,
                        self.modifiers,
                        &self.config.working_directory,
                    )?;
                    self.apply_pane_outcome(outcome);
                } else {
                    self.review_active = false;
                }
            }
            return Ok(());
        }

        let Some((x, y)) = self.cursor_position else {
            return Ok(());
        };
        let Some((pane_index, rect)) = self.layout.pane_at(x, y) else {
            return Ok(());
        };
        if pane_index >= self.panes.len() {
            if state == ElementState::Pressed {
                self.set_active_pane(pane_index);
            }
            return Ok(());
        }
        for (index, pane) in self.panes.iter_mut().enumerate() {
            if index != pane_index {
                self.dirty |= pane.clear_selection();
            }
        }

        let outcome = self.panes[pane_index].handle_mouse_input(
            state,
            button,
            x - rect.x,
            y - rect.y,
            self.modifiers,
            &self.config.working_directory,
        )?;
        let reference_navigation = Self::is_reference_navigation_command(&outcome.command);
        self.apply_pane_outcome(outcome);
        if state == ElementState::Pressed && !reference_navigation {
            self.set_active_pane(pane_index);
        }
        Ok(())
    }

    fn is_reference_navigation_command(command: &Option<PaneCommand>) -> bool {
        matches!(
            command,
            Some(PaneCommand::OpenRequested { .. } | PaneCommand::SymbolOpenRequested { .. })
        )
    }

    fn handle_mouse_motion(&mut self) -> Result<()> {
        let Some((x, y)) = self.cursor_position else {
            return Ok(());
        };
        if self.review_active {
            if let Some(review_pane) = &mut self.review_pane {
                let outcome = review_pane.handle_mouse_motion(x, y, self.modifiers)?;
                self.apply_pane_outcome(outcome);
            } else {
                self.review_active = false;
            }
            return Ok(());
        }

        let Some((pane_index, rect)) = self.layout.pane_at(x, y) else {
            return Ok(());
        };

        if pane_index != self.active_pane || pane_index >= self.panes.len() {
            return Ok(());
        }

        let outcome =
            self.panes[pane_index].handle_mouse_motion(x - rect.x, y - rect.y, self.modifiers)?;
        self.apply_pane_outcome(outcome);
        Ok(())
    }

    fn apply_pane_outcome(&mut self, outcome: PaneEventOutcome) {
        self.dirty |= outcome.dirty;
        self.force_redraw |= outcome.force_redraw;
        if let Some(command) = outcome.command {
            match command {
                PaneCommand::OpenRequested { path, line } => {
                    self.focus_nvim_after_reference_navigation();
                    self.events
                        .try_send(Event::Ui(UiEvent::OpenRequested { path, line }));
                }
                PaneCommand::SymbolOpenRequested { symbol } => {
                    self.focus_nvim_after_reference_navigation();
                    self.events
                        .try_send(Event::Ui(UiEvent::SymbolOpenRequested { symbol }));
                }
            }
        }
    }

    fn focus_nvim_after_reference_navigation(&mut self) {
        let focus =
            focus_nvim_after_agent_reference(&mut self.pane_layout_mode, &mut self.active_pane);
        if focus.relayout_needed {
            self.relayout();
        } else if focus.focus_changed {
            self.dirty = true;
            self.force_redraw = true;
        }
    }

    fn drain_background_work(&mut self) -> Result<()> {
        // Clear the coalescing flag *before* draining so that any PTY data arriving
        // during the drain will set `pending` again and fire a new UserEvent::PtyOutput,
        // rather than being silently swallowed.
        self.output_notifier.clear();
        for pane in self.panes.iter_mut() {
            if matches!(pane.role(), PaneRole::AgentTerminal { .. }) {
                let chunks = pane.drain_output_chunks();
                for chunk in &chunks {
                    if let Ok(text) = std::str::from_utf8(chunk) {
                        self.events
                            .try_send(Event::Agent(crate::event::AgentEvent::OutputChunk(
                                text.to_string(),
                            )));
                    }
                }
                self.dirty |= !chunks.is_empty();
            } else {
                let had_output = pane.drain_output();
                self.dirty |= had_output;
            }
        }
        if let Some(review_pane) = &mut self.review_pane {
            self.dirty |= review_pane.drain_output();
        }
        self.dirty |= self.reload_theme_if_needed()?;
        self.dirty |= self.forward_ui_commands()?;
        self.dirty |= self.flush_pending_agent_write()?;
        if let Some(acp_pane) = &mut self.acp_pane {
            self.dirty |= acp_pane.tick_progress();
        }
        Ok(())
    }

    fn reload_theme_if_needed(&mut self) -> Result<bool> {
        let now = Instant::now();
        if now < self.next_theme_poll_at {
            return Ok(false);
        }
        self.next_theme_poll_at = now + THEME_POLL_INTERVAL;

        let loaded_config = TerminalConfig::load();
        if loaded_config.theme == self.terminal_config.theme {
            return Ok(false);
        }

        self.terminal_config.theme = loaded_config.theme.clone();
        self.font_renderer.theme = loaded_config.theme;
        for pane in &mut self.panes {
            pane.apply_theme(&self.terminal_config.theme);
        }
        if let Some(review_pane) = &mut self.review_pane {
            review_pane.apply_theme(&self.terminal_config.theme);
        }
        if let Some(acp_pane) = &mut self.acp_pane {
            acp_pane.apply_theme(&self.terminal_config.theme);
        }
        self.force_redraw = true;
        info!("terminal theme changed; reloaded Ghostty colors");
        Ok(true)
    }

    fn set_active_pane(&mut self, pane_index: usize) {
        if self.active_pane != pane_index {
            self.active_pane = pane_index;
            self.dirty = true;
            self.force_redraw = true;
        }
    }

    fn show_pane_focus_chrome(&self) -> bool {
        matches!(self.pane_layout_mode, PaneLayoutMode::Split)
            && (self.panes.len() > 1 || self.acp_pane.is_some())
    }

    fn render(&mut self) -> Result<()> {
        let render_started_at = Instant::now();
        if self.width == 0 || self.height == 0 {
            return Ok(());
        }

        let Some(window) = self.window.as_ref() else {
            return Ok(());
        };

        if self.force_redraw {
            self.buffer.fill(self.terminal_config.theme.background);
        }
        self.damage.clear();
        let mut redrawn = self.force_redraw;
        let redraw_chrome = self.show_pane_focus_chrome() && (self.dirty || self.force_redraw);

        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };

        if self.review_active {
            if let Some(review_pane) = &mut self.review_pane {
                redrawn |= review_pane.draw(
                    &mut self.font_renderer,
                    &mut self.buffer,
                    self.width,
                    self.height,
                    full_window_rect(self.width, self.height),
                    true,
                    self.force_redraw,
                    false,
                    &mut self.damage,
                );
            } else {
                self.review_active = false;
            }
        } else {
            for (index, pane) in self.panes.iter_mut().enumerate() {
                redrawn |= pane.draw(
                    &mut self.font_renderer,
                    &mut self.buffer,
                    self.width,
                    self.height,
                    self.layout.pane(index),
                    index == self.active_pane,
                    self.force_redraw,
                    redraw_chrome,
                    &mut self.damage,
                );
            }
            if let Some(acp_pane) = &mut self.acp_pane {
                redrawn |= acp_pane.draw(
                    &mut self.font_renderer,
                    &mut self.buffer,
                    self.width,
                    self.height,
                    self.layout.pane(1),
                    self.active_pane == 1,
                    self.force_redraw,
                    redraw_chrome,
                    &mut self.damage,
                );
            }
        }

        if let Some(ref modal) = self.acp_modal {
            let m = self.terminal_config.metrics;
            let cols = (self.width / m.cell_width).min(u16::MAX as usize) as u16;
            let rows = (self.height / m.cell_height).min(u16::MAX as usize) as u16;
            if cols > 0 && rows > 0 {
                let mb = render_acp_modal_buffer(
                    modal,
                    cols,
                    rows,
                    self.terminal_config.theme.background,
                );
                redrawn |= draw_ratatui_buffer(
                    &mb,
                    &mut self.font_renderer,
                    &mut self.buffer,
                    self.width,
                    self.height,
                    0,
                    0,
                    m,
                    self.terminal_config.theme.foreground,
                    self.terminal_config.theme.background,
                    self.force_redraw || self.dirty,
                    &mut self.damage,
                );
            }
        }

        if !redrawn {
            self.dirty = false;
            return Ok(());
        }

        let mut surface_buffer = surface
            .buffer_mut()
            .map_err(|error| anyhow!("failed to acquire softbuffer frame: {error:?}"))?;
        copy_damage_to_surface(&self.buffer, &mut surface_buffer);
        window.pre_present_notify();
        surface_buffer
            .present()
            .map_err(|error| anyhow!("failed to present softbuffer frame: {error:?}"))?;
        let render_elapsed = render_started_at.elapsed();
        if render_elapsed > RENDER_WARN_THRESHOLD {
            warn!(
                ?render_elapsed,
                damage_rects = self.damage.len(),
                "slow render frame"
            );
        }
        self.next_redraw_at = Instant::now() + self.target_frame_interval();
        self.dirty = false;
        self.force_redraw = false;
        self.arrow_repeat_redraw_deferred = false;
        Ok(())
    }

    fn forward_ui_commands(&mut self) -> Result<bool> {
        let mut changed = false;

        while let Ok(command) = self.ui_commands.try_recv() {
            changed = true;
            match command {
                UiCommand::EditorContextChanged { path, line, column } => {
                    let Some(agent_pane) = self.panes.get_mut(1) else {
                        continue;
                    };
                    let update =
                        format_context_update(&path, line, column, &self.config.working_directory);

                    debug!(path = %path.display(), line, column, "forwarding editor context to agent");
                    agent_pane.write_all(update.as_bytes())?;
                    self.pending_agent_write = None;
                }
                UiCommand::VisualSelectionChanged {
                    path,
                    start_line,
                    end_line,
                } => {
                    let Some(agent_pane) = self.panes.get_mut(1) else {
                        continue;
                    };
                    let update = format_selection_update(
                        &path,
                        start_line,
                        end_line,
                        &self.config.working_directory,
                    );

                    debug!(path = %path.display(), start_line, end_line, "forwarding visual selection to agent");
                    agent_pane.write_all(b"\x15")?;
                    self.pending_agent_write = Some(PendingAgentWrite {
                        ready_at: Instant::now() + Duration::from_millis(30),
                        bytes: update.into_bytes(),
                    });
                }
                UiCommand::AgentInput {
                    payload,
                    focus_agent,
                    target_agent_id,
                } => {
                    // Determine target pane first (specific id or first AgentTerminal).
                    let idx = if let Some(ref want) = target_agent_id {
                        self.panes
                            .iter()
                            .position(|p| matches!(p.role(), PaneRole::AgentTerminal { id, .. } if id == want))
                    } else {
                        None
                    };
                    let idx = idx.or_else(|| {
                        self.panes
                            .iter()
                            .position(|p| matches!(p.role(), PaneRole::AgentTerminal { .. }))
                    });

                    if let Some(i) = idx {
                        if focus_agent {
                            // Focus the specific target pane (not a hardcoded one).
                            if matches!(self.pane_layout_mode, PaneLayoutMode::PaneMaximized(_)) {
                                self.pane_layout_mode = PaneLayoutMode::PaneMaximized(i);
                            }
                            self.set_active_pane(i);
                        }
                        debug!(target = ?target_agent_id, pane_index = i, "forwarding input to agent pane");
                        self.panes[i].write_all(payload.as_bytes())?;
                    } else {
                        warn!("no agent pane available to receive input");
                    }
                    self.pending_agent_write = None;
                }
                UiCommand::AcpTranscriptChunk { kind, text } => {
                    let Some(acp_pane) = &mut self.acp_pane else {
                        continue;
                    };
                    debug!(kind, "forwarding ACP transcript chunk to pane");
                    acp_pane.append_transcript_chunk(&kind, &text);
                }
                UiCommand::AcpProgress { id, label, active } => {
                    let Some(acp_pane) = &mut self.acp_pane else {
                        continue;
                    };
                    debug!(id, label, active, "forwarding ACP progress update to pane");
                    acp_pane.update_progress(id, label, active);
                }
                UiCommand::AcpModalPrompt {
                    jsonrpc_id,
                    title,
                    message,
                    options,
                    kind,
                } => {
                    self.acp_modal = Some(AcpModalState::new(
                        jsonrpc_id, title, message, options, kind,
                    ));
                }
                UiCommand::SpawnAgent { id, role, command } => {
                    info!(%id, role = ?role, command = ?command, "spawning new agent pane");
                    if let Err(e) = self.spawn_agent_pane(&id, role.as_deref(), command.as_deref()) {
                        error!(%e, "failed to spawn agent pane");
                    }
                }
            }
        }

        Ok(changed)
    }

    fn flush_pending_agent_write(&mut self) -> Result<bool> {
        let Some(pending) = &self.pending_agent_write else {
            return Ok(false);
        };

        if Instant::now() < pending.ready_at {
            return Ok(false);
        }

        let Some(agent_pane) = self.panes.get_mut(1) else {
            self.pending_agent_write = None;
            return Ok(false);
        };
        let pending = self
            .pending_agent_write
            .take()
            .expect("pending write exists");

        agent_pane.write_all(&pending.bytes)?;
        Ok(true)
    }

    fn request_redraw(&self) {
        if Instant::now() < self.next_redraw_at {
            return;
        }

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn target_frame_interval(&self) -> Duration {
        if self.arrow_repeat_redraw_deferred {
            REPEATED_ARROW_REDRAW_INTERVAL
        } else {
            TARGET_FRAME_INTERVAL
        }
    }

    fn set_wait_for_next_redraw(&self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now < self.next_redraw_at {
            event_loop.set_control_flow(ControlFlow::wait_duration(self.next_redraw_at - now));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
        event_loop.exit();
    }
}

impl ApplicationHandler<UserEvent> for WinitGhosttyApp {
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput => {
                if let Err(error) = self.drain_background_work() {
                    self.fail(event_loop, error);
                    return;
                }

                if self.dirty {
                    self.request_redraw();
                    self.set_wait_for_next_redraw(event_loop);
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.initialize_window(event_loop) {
            self.fail(event_loop, error);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if Some(window_id) != self.window_id {
            return;
        }

        let result = match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                Ok(())
            }
            WindowEvent::Resized(size) => self.resize_window(size.width, size.height),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let result = self.update_font_scale(scale_factor);
                let Some(window) = self.window.as_ref() else {
                    return;
                };
                let size = window.inner_size();
                result.and_then(|_| self.resize_window(size.width, size.height))
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key_event(event),
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                self.modifiers = Modifiers {
                    ctrl: state.control_key(),
                    shift: state.shift_key(),
                    alt: state.alt_key(),
                    super_key: state.super_key(),
                };
                Ok(())
            }
            WindowEvent::Ime(Ime::Commit(text)) => self.handle_text_commit(&text),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_position =
                    Some((position.x.max(0.0) as usize, position.y.max(0.0) as usize));
                self.handle_mouse_motion()
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_position = None;
                Ok(())
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_mouse_scroll(delta),
            WindowEvent::MouseInput { state, button, .. } => self.handle_mouse_input(state, button),
            WindowEvent::RedrawRequested => {
                self.skip_background_drain_once = false;
                self.render()
            }
            _ => Ok(()),
        };

        if let Err(error) = result {
            self.fail(event_loop, error);
            return;
        }

        if self.skip_background_drain_once {
            self.skip_background_drain_once = false;
        } else if let Err(error) = self.drain_background_work() {
            self.fail(event_loop, error);
            return;
        }

        if self.dirty {
            self.request_redraw();
            self.set_wait_for_next_redraw(event_loop);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.drain_background_work() {
            self.fail(event_loop, error);
            return;
        }

        if self.dirty {
            self.request_redraw();
            self.set_wait_for_next_redraw(event_loop);
        } else {
            event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(50)));
        }
    }
}

fn pane_count(config: &Config) -> usize {
    if config.agent_command.is_some() { 2 } else { 1 }
}

fn full_window_rect(width: usize, height: usize) -> PaneRect {
    PaneRect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn hunk_review_command() -> &'static str {
    r#"if command -v hunk >/dev/null 2>&1; then hunk diff; printf '\n[hunk exited - press Alt+R to return to via]\n'; exec "${SHELL:-sh}"; else printf 'via: hunk is not available on PATH. Set VIA_REVIEW_BACKEND=nvim or install hunk.\n'; exec "${SHELL:-sh}"; fi"#
}

fn nvim_args(config: &Config) -> Vec<OsString> {
    vec![
        OsString::from("--listen"),
        config.nvim_socket_path.clone().into_os_string(),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.via_editor_socket = {}",
            lua_string_literal(&config.editor_socket_path)
        )),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.via_lsp_bridge_socket = {}",
            lua_string_literal(&config.lsp_bridge_socket_path)
        )),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.via_module_path = {}",
            lua_string_literal(&config.nvim_via_module_path)
        )),
        OsString::from("--cmd"),
        {
            // Stable lua/ directory so require('via') works for every via session.
            let dir = crate::config::lua_dir();
            OsString::from(format!(
                "lua package.path = package.path .. ';{}/?.lua'",
                dir.to_string_lossy()
            ))
        },
        OsString::from("-c"),
        OsString::from(format!(
            "luafile {}",
            vim_fnameescape(&config.nvim_context_bridge_path)
        )),
    ]
}

fn format_context_update(
    path: &Path,
    _line: u32,
    _column: u32,
    working_directory: &Path,
) -> String {
    let display_path = path.strip_prefix(working_directory).unwrap_or(path);

    format!("@{}\n", display_path.display())
}

fn format_selection_update(
    path: &Path,
    start_line: u32,
    end_line: u32,
    working_directory: &Path,
) -> String {
    let display_path = path.strip_prefix(working_directory).unwrap_or(path);

    format!("@{}:{start_line}-{end_line}", display_path.display())
}

fn lua_string_literal(path: &Path) -> String {
    let mut quoted = String::from("\"");

    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(ch),
        }
    }

    quoted.push('"');
    quoted
}

fn vim_fnameescape(path: &Path) -> String {
    let mut escaped = String::new();

    for ch in path.to_string_lossy().chars() {
        match ch {
            '\\' | ' ' | '\t' | '\n' | '*' | '?' | '[' | '{' | '`' | '$' | '%' | '#' | '\''
            | '"' | '|' | '!' | '<' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }

    escaped
}

fn ensure_buffer_size(buffer: &mut Vec<u32>, width: usize, height: usize, fill: u32) -> bool {
    let len = width.saturating_mul(height);

    if buffer.len() != len {
        buffer.resize(len, fill);
        return true;
    }

    false
}

fn copy_damage_to_surface(buffer: &[u32], surface_buffer: &mut [u32]) {
    // Softbuffer uses a rotating chain of buffers (double/triple buffering) on Wayland.
    // Copying only the damage rects leaves the rest of the buffer out-of-sync with
    // the previous frames. Since we keep a full pristine frame in memory, we must
    // copy the entire buffer to ensure the presented frame is fully consistent.
    surface_buffer.copy_from_slice(buffer);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::pty::TerminalSize;
    use config::ghostty_config_entry;
    use layout::PaneRect;
    use links::{
        LinkSpan, ReferenceTarget, file_target_from_uri, parse_vt_hyperlinks,
        reference_target_from_row,
    };

    #[test]
    fn parses_ghostty_config_entries() {
        assert_eq!(
            ghostty_config_entry("config-file = ?\"~/.config/theme.conf\""),
            Some(("config-file", "~/.config/theme.conf"))
        );
        assert_eq!(
            ghostty_config_entry("font-family = \"JetBrainsMono Nerd Font\""),
            Some(("font-family", "JetBrainsMono Nerd Font"))
        );
    }

    #[test]
    fn parses_theme_colors() {
        let mut config = TerminalConfig::default();

        config.apply_entry("background", "#0f0d21");
        config.apply_entry("foreground", "#ffffff");
        config.apply_entry("palette", "4=#b0c3f8");

        assert_eq!(config.theme.background, 0x0f0d21);
        assert_eq!(config.theme.foreground, 0xffffff);
        assert_eq!(config.theme.palette[4], 0xb0c3f8);
    }

    #[test]
    fn scales_metrics_from_ghostty_font_size() {
        let mut config = TerminalConfig {
            font_size: 9.0,
            ..TerminalConfig::default()
        };

        config.finalize_metrics();

        assert_eq!(config.font_pixels, 12.0);
        assert_eq!(config.metrics.cell_width, 8);
        assert_eq!(config.metrics.cell_height, 17);
    }

    #[test]
    fn scales_metrics_from_window_scale_factor() {
        let mut config = TerminalConfig {
            font_size: 9.0,
            ..TerminalConfig::default()
        };

        config.finalize_metrics_for_scale(1.25);

        assert_eq!(config.font_pixels, 15.0);
        assert_eq!(config.metrics.cell_width, 9);
        assert_eq!(config.metrics.cell_height, 21);
    }

    #[test]
    fn creates_single_pane_layout_without_agent() {
        let layout = SplitLayout::for_window(
            100,
            50,
            1,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
    }

    #[test]
    fn creates_vertical_split_layout_for_agent() {
        let cell_width = 10;
        let width = cell_width * 200 + 2;
        let layout = SplitLayout::for_window(
            width,
            50,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions {
                cell_width,
                agent_pane_cols: Some((80, 100)),
            },
        );

        assert_eq!(layout.pane(0).width / cell_width, 120);
        assert_eq!(layout.pane(1).width / cell_width, 80);
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
        assert_eq!(
            layout
                .pane_at(layout.pane(1).x + 10, 10)
                .map(|(index, _)| index),
            Some(1)
        );
        assert_eq!(layout.pane_at(layout.pane(0).width + 1, 10), None);
    }

    #[test]
    fn creates_horizontal_split_layout_for_agent() {
        let layout = SplitLayout::for_window(
            50,
            100,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Horizontal,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 50,
                height: 49,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 0,
                y: 51,
                width: 50,
                height: 49,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
        assert_eq!(layout.pane_at(10, 60).map(|(index, _)| index), Some(1));
        assert_eq!(layout.pane_at(10, 50), None);
    }

    #[test]
    fn selects_initial_split_direction_from_window_shape() {
        assert_eq!(
            PaneSplitDirection::for_window(50, 100),
            PaneSplitDirection::Horizontal
        );
        assert_eq!(
            PaneSplitDirection::for_window(100, 50),
            PaneSplitDirection::Vertical
        );
        assert_eq!(
            PaneSplitDirection::for_window(100, 100),
            PaneSplitDirection::Vertical
        );
        // Below 20% height excess: treat as square-ish, default to vertical split.
        assert_eq!(
            PaneSplitDirection::for_window(100, 119),
            PaneSplitDirection::Vertical
        );
        assert_eq!(
            PaneSplitDirection::for_window(100, 120),
            PaneSplitDirection::Horizontal
        );
        assert_eq!(
            PaneSplitDirection::for_window(119, 100),
            PaneSplitDirection::Vertical
        );
        assert_eq!(
            PaneSplitDirection::for_window(120, 100),
            PaneSplitDirection::Vertical
        );
    }

    #[test]
    fn split_direction_hysteresis_keeps_mode_until_twenty_percent_margin() {
        assert_eq!(
            PaneSplitDirection::adjust_for_window_resize(PaneSplitDirection::Vertical, 100, 119),
            PaneSplitDirection::Vertical
        );
        assert_eq!(
            PaneSplitDirection::adjust_for_window_resize(PaneSplitDirection::Vertical, 100, 120),
            PaneSplitDirection::Horizontal
        );
        assert_eq!(
            PaneSplitDirection::adjust_for_window_resize(PaneSplitDirection::Horizontal, 100, 120),
            PaneSplitDirection::Horizontal
        );
        assert_eq!(
            PaneSplitDirection::adjust_for_window_resize(PaneSplitDirection::Horizontal, 100, 119),
            PaneSplitDirection::Horizontal
        );
        assert_eq!(
            PaneSplitDirection::adjust_for_window_resize(PaneSplitDirection::Horizontal, 120, 100),
            PaneSplitDirection::Vertical
        );
    }

    #[test]
    fn creates_nvim_maximized_layout() {
        let layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::NvimMaximized,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 100,
                y: 0,
                width: 0,
                height: 0,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
    }

    #[test]
    fn creates_agent_maximized_layout() {
        let layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::AgentMaximized,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 0,
                y: 0,
                width: 100,
                height: 50,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(1));
    }

    #[test]
    fn maximized_layouts_keep_hidden_panes_zero_sized() {
        let nvim_layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::NvimMaximized,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );
        let agent_layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::AgentMaximized,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(nvim_layout.pane(1).width, 0);
        assert_eq!(nvim_layout.pane(1).height, 0);
        assert_eq!(agent_layout.pane(0).width, 0);
        assert_eq!(agent_layout.pane(0).height, 0);
    }

    #[test]
    fn maps_alt_number_shortcuts_to_active_panes() {
        let mut mode = PaneLayoutMode::AgentMaximized;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 1;

        assert!(handle_layout_shortcuts(
            &[Key::Key1],
            true,
            false,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Key2],
            true,
            false,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 1);
    }

    #[test]
    fn maps_alt_shift_number_shortcuts_to_layout_modes() {
        let mut mode = PaneLayoutMode::Split;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 1;

        assert!(handle_layout_shortcuts(
            &[Key::Key1],
            true,
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::NvimMaximized);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Key2],
            true,
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::AgentMaximized);
        assert_eq!(active_pane, 1);
    }

    #[test]
    fn maps_alt_arrow_shortcuts_to_active_panes() {
        let mut mode = PaneLayoutMode::Split;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 1;

        assert!(handle_layout_shortcuts(
            &[Key::Left],
            true,
            false,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Right],
            true,
            false,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(active_pane, 1);
    }

    #[test]
    fn maps_alt_shift_3_shortcut_to_split_direction_toggle() {
        let mut mode = PaneLayoutMode::AgentMaximized;
        let mut split_direction = PaneSplitDirection::Vertical;
        let mut active_pane = 0;

        assert!(handle_layout_shortcuts(
            &[Key::Key3],
            true,
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(mode, PaneLayoutMode::Split);
        assert_eq!(split_direction, PaneSplitDirection::Horizontal);
        assert_eq!(active_pane, 0);
        assert!(handle_layout_shortcuts(
            &[Key::Key3],
            true,
            true,
            2,
            &mut mode,
            &mut split_direction,
            &mut active_pane
        ));
        assert_eq!(split_direction, PaneSplitDirection::Vertical);
    }

    #[test]
    fn nvim_args_install_context_bridge() {
        let config = Config {
            nvim_command: "nvim".to_string(),
            agent_command: None,
            agent_pane_cols: None,
            review_backend: ReviewBackend::Nvim,
            scroll_sensitivity: crate::config::DEFAULT_SCROLL_SENSITIVITY,
            nvim_socket_path: PathBuf::from("/tmp/via-nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/via-editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/repo/nvim/context bridge.lua"),
            nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
            lsp_bridge_socket_path: PathBuf::from("/tmp/via-lsp-bridge.sock"),
            working_directory: PathBuf::from("/repo"),
        };
        let args = nvim_args(&config);

        assert_eq!(args[0], OsString::from("--listen"));
        assert_eq!(args[1], OsString::from("/tmp/via-nvim.sock"));
        assert_eq!(args[2], OsString::from("--cmd"));
        assert_eq!(
            args[3],
            OsString::from(r#"lua vim.g.via_editor_socket = "/tmp/via-editor.sock""#)
        );
        assert_eq!(args[4], OsString::from("--cmd"));
        assert_eq!(
            args[5],
            OsString::from(r#"lua vim.g.via_lsp_bridge_socket = "/tmp/via-lsp-bridge.sock""#)
        );
        assert_eq!(args[6], OsString::from("-c"));
        assert_eq!(
            args[7],
            OsString::from("luafile /repo/nvim/context\\ bridge.lua")
        );
    }

    #[test]
    fn context_update_uses_relative_file_path() {
        assert_eq!(
            format_context_update(Path::new("/repo/src/main.rs"), 42, 7, Path::new("/repo")),
            "@src/main.rs\n"
        );
    }

    #[test]
    fn selection_update_uses_relative_file_path_and_line_range() {
        assert_eq!(
            format_selection_update(Path::new("/repo/src/main.rs"), 3, 8, Path::new("/repo")),
            "@src/main.rs:3-8"
        );
    }

    #[test]
    fn vim_fnameescape_escapes_command_path_characters() {
        assert_eq!(
            vim_fnameescape(Path::new("/repo/nvim/context bridge's.lua")),
            "/repo/nvim/context\\ bridge\\'s.lua"
        );
    }

    #[test]
    fn parses_osc8_hyperlink_spans() {
        let rows = parse_vt_hyperlinks(
            b"see \x1b]8;;file:///repo/src/main.rs:8\x1b\\main\x1b]8;;\x1b\\ now",
            test_terminal_size(),
        );

        assert_eq!(
            rows[0],
            vec![LinkSpan {
                start: 4,
                end: 8,
                uri: "file:///repo/src/main.rs:8".to_string(),
            }]
        );
    }

    #[test]
    fn parses_bel_terminated_osc8_hyperlink_spans() {
        let rows = parse_vt_hyperlinks(
            b"\x1b]8;;src/lib.rs:3\x07lib\x1b]8;;\x07",
            test_terminal_size(),
        );

        assert_eq!(
            rows[0],
            vec![LinkSpan {
                start: 0,
                end: 3,
                uri: "src/lib.rs:3".to_string(),
            }]
        );
    }

    #[test]
    fn tracks_osc8_links_after_cursor_movement() {
        let rows = parse_vt_hyperlinks(
            b"\x1b[2;5H\x1b]8;;file:///repo/src/main.rs:8\x1b\\main\x1b]8;;\x1b\\",
            test_terminal_size(),
        );

        assert_eq!(
            rows[1],
            vec![LinkSpan {
                start: 4,
                end: 8,
                uri: "file:///repo/src/main.rs:8".to_string(),
            }]
        );
    }

    #[test]
    fn converts_file_uri_to_target() {
        let target =
            file_target_from_uri("file:///repo/src/main%20file.rs:8", Path::new("/fallback"))
                .unwrap();

        assert_eq!(target.path, PathBuf::from("/repo/src/main file.rs"));
        assert_eq!(target.line, Some(8));
    }

    #[test]
    fn ignores_non_file_uris() {
        assert!(
            file_target_from_uri("https://example.com/src/main.rs", Path::new("/repo")).is_none()
        );
    }

    #[test]
    fn row_reference_parsing_returns_symbol_target() {
        let target = reference_target_from_row("see Foo::bar here", 5, Path::new("/repo"));

        assert_eq!(
            target,
            Some(ReferenceTarget::Symbol("Foo::bar".to_string()))
        );
    }

    fn test_terminal_size() -> TerminalSize {
        TerminalSize {
            rows: 5,
            cols: 40,
            pixel_width: 400,
            pixel_height: 100,
        }
    }
}
