use std::ffi::OsString;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use tokio::sync::mpsc::Receiver as TokioReceiver;
use tracing::{debug, info, warn};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::config::Config;
use crate::event::{Event, UiCommand, UiEvent};
use crate::mediator::EventSender;
use crate::pty::{CoalescedOutputNotifier, OutputNotifier};

mod config;
mod font;
mod input;
mod layout;
mod links;
mod pane;
mod render;

use config::{TerminalConfig, TerminalMetrics};
use font::FontRenderer;
use input::{
    Key, Modifiers, forward_keyboard_viewport_scroll, forward_mouse_scroll, forward_special_keys,
    forward_text_input, try_clipboard_paste,
};
use layout::{PaneLayoutMode, PaneSplitDirection, SplitLayout, handle_layout_shortcuts};
use links::ReferenceTarget;
use pane::TerminalPane;
use render::DamageRect;

const INITIAL_WIDTH: usize = 960;
const INITIAL_HEIGHT: usize = 540;
const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const REPEATED_ARROW_REDRAW_INTERVAL: Duration = Duration::from_millis(24);
const INPUT_LAG_WARN_THRESHOLD: Duration = Duration::from_millis(50);
const RENDER_WARN_THRESHOLD: Duration = Duration::from_millis(20);
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(750);
const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(300);

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

#[derive(Debug, Clone, Copy)]
struct ReferenceClick {
    at: Instant,
    pane_index: usize,
    row: usize,
    column: usize,
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
    panes: Vec<TerminalPane>,
    active_pane: usize,
    pane_layout_mode: PaneLayoutMode,
    pane_split_direction: PaneSplitDirection,
    layout: SplitLayout,
    width: usize,
    height: usize,
    modifiers: Modifiers,
    cursor_position: Option<(usize, usize)>,
    left_mouse_down: bool,
    last_reference_click: Option<ReferenceClick>,
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
            active_pane: 0,
            pane_layout_mode: PaneLayoutMode::Split,
            pane_split_direction,
            layout,
            width: INITIAL_WIDTH,
            height: INITIAL_HEIGHT,
            modifiers: Modifiers::default(),
            cursor_position: None,
            left_mouse_down: false,
            last_reference_click: None,
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
    ) -> Result<Vec<TerminalPane>> {
        let layout = SplitLayout::for_window(
            width,
            height,
            pane_count(&self.config),
            PaneLayoutMode::Split,
            PaneSplitDirection::for_window(width, height),
        );
        let mut panes = vec![TerminalPane::new(
            "nvim",
            layout.pane(0).width,
            layout.pane(0).height,
            metrics,
            &self.terminal_config.theme,
        )?];

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
                panes.push(pane);
            }
        }

        Ok(panes)
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

    fn relayout(&mut self) {
        if self.panes.is_empty() {
            return;
        }

        let new_layout = SplitLayout::for_window(
            self.width,
            self.height,
            self.panes.len(),
            self.pane_layout_mode,
            self.pane_split_direction,
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
    }

    fn handle_key_event(&mut self, event: KeyEvent) -> Result<()> {
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
            self.relayout();
            self.dirty = true;
        }

        let paste_requested = !event.repeat
            && key
                .map(|key| paste_requested(key, self.modifiers))
                .unwrap_or(false);
        if paste_requested {
            try_clipboard_paste(
                true,
                &mut self.panes[self.active_pane],
                layout_shortcut_consumed,
            )?;
            return Ok(());
        }

        let copy_requested = !event.repeat
            && key
                .map(|key| copy_requested(key, self.modifiers))
                .unwrap_or(false);
        if copy_requested {
            self.dirty |= self.panes[self.active_pane].copy_selection_to_clipboard();
            return Ok(());
        }

        let keyboard_scrolled = forward_keyboard_viewport_scroll(
            &pressed_keys,
            self.modifiers,
            self.active_pane,
            &mut self.panes,
            layout_shortcut_consumed,
        );
        if keyboard_scrolled {
            self.dirty = true;
            self.force_redraw = true;
        }
        forward_special_keys(
            &pressed_keys,
            self.modifiers,
            &mut self.panes[self.active_pane],
            layout_shortcut_consumed,
        )?;

        if let Some(text) = event
            .text
            .as_deref()
            .filter(|text| text.chars().all(|ch| !ch.is_control()))
        {
            forward_text_input(
                text,
                self.modifiers,
                &mut self.panes[self.active_pane],
                layout_shortcut_consumed,
            )?;
        }

        Ok(())
    }

    fn handle_text_commit(&mut self, text: &str) -> Result<()> {
        if self.panes.is_empty() {
            return Ok(());
        }

        forward_text_input(
            text,
            self.modifiers,
            &mut self.panes[self.active_pane],
            false,
        )?;
        Ok(())
    }

    fn handle_mouse_scroll(&mut self, delta: MouseScrollDelta) {
        if self.panes.is_empty() {
            return;
        }

        let scroll_delta = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x * 40.0, y * 40.0),
            MouseScrollDelta::PixelDelta(position) => (position.x as f32, position.y as f32),
        };

        let scrolled = forward_mouse_scroll(
            scroll_delta,
            self.cursor_position,
            &self.layout,
            &mut self.panes,
            false,
        );
        if scrolled {
            self.dirty = true;
            self.force_redraw = true;
        }
    }

    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        if button != MouseButton::Left {
            return;
        }

        let is_down = state == ElementState::Pressed;
        let just_pressed = is_down && !self.left_mouse_down;
        let just_released = !is_down && self.left_mouse_down;
        self.left_mouse_down = is_down;

        if just_pressed {
            self.dirty |= self.start_mouse_selection();
            self.dirty |= self.forward_reference_click();
        }

        if just_released {
            self.dirty |= self.finalize_mouse_selection();
        }
    }

    fn start_mouse_selection(&mut self) -> bool {
        let Some((x, y)) = self.cursor_position else {
            return false;
        };
        let Some((pane_index, rect)) = self.layout.pane_at(x, y) else {
            return false;
        };

        let pane_changed = self.active_pane != pane_index;
        self.active_pane = pane_index;
        for (index, pane) in self.panes.iter_mut().enumerate() {
            if index != pane_index {
                pane.clear_selection();
            }
        }

        let metrics = self.panes[pane_index].metrics();
        let row = (y - rect.y) / metrics.cell_height;
        let column = (x - rect.x) / metrics.cell_width;
        let selection_changed = self.panes[pane_index].begin_selection(row, column);

        pane_changed || selection_changed
    }

    fn update_mouse_selection(&mut self) -> bool {
        if !self.left_mouse_down {
            return false;
        }

        let Some((x, y)) = self.cursor_position else {
            return false;
        };
        let Some((pane_index, rect)) = self.layout.pane_at(x, y) else {
            return false;
        };

        if pane_index != self.active_pane {
            return false;
        }

        let metrics = self.panes[pane_index].metrics();
        let row = (y - rect.y) / metrics.cell_height;
        let column = (x - rect.x) / metrics.cell_width;

        self.panes[pane_index].update_selection(row, column)
    }

    fn finalize_mouse_selection(&mut self) -> bool {
        if self.panes.is_empty() || self.active_pane >= self.panes.len() {
            return false;
        }

        self.panes[self.active_pane].copy_selection_to_clipboard()
    }

    fn forward_reference_click(&mut self) -> bool {
        let Some((x, y)) = self.cursor_position else {
            return false;
        };
        let Some((pane_index, rect)) = self.layout.pane_at(x, y) else {
            return false;
        };

        let pane_changed = self.active_pane != pane_index;
        self.active_pane = pane_index;

        let metrics = self.panes[pane_index].metrics();
        let row = (y - rect.y) / metrics.cell_height;
        let column = (x - rect.x) / metrics.cell_width;
        let click = ReferenceClick {
            at: Instant::now(),
            pane_index,
            row,
            column,
        };
        let is_double_click = is_double_click(
            self.last_reference_click.as_ref(),
            &click,
            DOUBLE_CLICK_THRESHOLD,
        );
        self.last_reference_click = Some(click);

        let Some(target) =
            self.panes[pane_index].reference_at(row, column, &self.config.working_directory)
        else {
            return pane_changed;
        };

        match target {
            ReferenceTarget::File(target) => {
                if !is_double_click {
                    return pane_changed;
                }

                info!(
                    path = %target.path.display(),
                    line = ?target.line,
                    "file reference double clicked"
                );
                self.events.try_send(Event::Ui(UiEvent::OpenRequested {
                    path: target.path,
                    line: target.line,
                }));
                true
            }
            ReferenceTarget::Symbol(symbol) => {
                if is_double_click {
                    return pane_changed;
                }

                info!(symbol = %symbol, "symbol reference clicked");
                self.events
                    .try_send(Event::Ui(UiEvent::SymbolOpenRequested { symbol }));
                true
            }
        }
    }

    fn drain_background_work(&mut self) -> Result<()> {
        // Clear the coalescing flag *before* draining so that any PTY data arriving
        // during the drain will set `pending` again and fire a new UserEvent::PtyOutput,
        // rather than being silently swallowed.
        self.output_notifier.clear();
        let is_agent_pane = self.panes.len() == 2;
        for (idx, pane) in self.panes.iter_mut().enumerate() {
            if is_agent_pane && idx == 1 {
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
        self.dirty |= self.reload_theme_if_needed()?;
        self.dirty |= self.forward_ui_commands()?;
        self.dirty |= self.flush_pending_agent_write()?;
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
        self.force_redraw = true;
        info!("terminal theme changed; reloaded Ghostty colors");
        Ok(true)
    }

    fn render(&mut self) -> Result<()> {
        let render_started_at = Instant::now();
        if self.width == 0 || self.height == 0 {
            return Ok(());
        }

        let Some(window) = self.window.as_ref() else {
            return Ok(());
        };
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };

        if self.force_redraw {
            self.buffer.fill(self.terminal_config.theme.background);
        }
        self.damage.clear();
        let mut redrawn = self.force_redraw;
        for (index, pane) in self.panes.iter_mut().enumerate() {
            redrawn |= pane.draw(
                &mut self.font_renderer,
                &mut self.buffer,
                self.width,
                self.height,
                self.layout.pane(index),
                index == self.active_pane,
                self.force_redraw,
                &mut self.damage,
            );
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
                UiCommand::AgentInput { payload } => {
                    let Some(agent_pane) = self.panes.get_mut(1) else {
                        continue;
                    };
                    debug!("forwarding tool response to agent");
                    agent_pane.write_all(payload.as_bytes())?;
                    self.pending_agent_write = None;
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
                self.dirty |= self.update_mouse_selection();
                Ok(())
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor_position = None;
                Ok(())
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.handle_mouse_scroll(delta);
                Ok(())
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(state, button);
                Ok(())
            }
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
    if config.agent_command.is_some() && !config.is_acp_agent() {
        2
    } else {
        1
    }
}

fn is_double_click(
    previous: Option<&ReferenceClick>,
    current: &ReferenceClick,
    threshold: Duration,
) -> bool {
    previous.is_some_and(|previous| {
        previous.pane_index == current.pane_index
            && previous.row == current.row
            && previous.column == current.column
            && current.at.saturating_duration_since(previous.at) <= threshold
    })
}

fn paste_requested(key: Key, modifiers: Modifiers) -> bool {
    (key == Key::V && (modifiers.super_key || (modifiers.ctrl && modifiers.shift)))
        || (key == Key::Insert && (modifiers.shift || modifiers.super_key))
}

fn copy_requested(key: Key, modifiers: Modifiers) -> bool {
    key == Key::C && modifiers.super_key
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
        let layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
        );

        assert_eq!(
            layout.pane(0),
            PaneRect {
                x: 0,
                y: 0,
                width: 49,
                height: 50,
            }
        );
        assert_eq!(
            layout.pane(1),
            PaneRect {
                x: 51,
                y: 0,
                width: 49,
                height: 50,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
        assert_eq!(layout.pane_at(60, 10).map(|(index, _)| index), Some(1));
        assert_eq!(layout.pane_at(50, 10), None);
    }

    #[test]
    fn creates_horizontal_split_layout_for_agent() {
        let layout = SplitLayout::for_window(
            50,
            100,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Horizontal,
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
        );
        let agent_layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::AgentMaximized,
            PaneSplitDirection::Vertical,
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
            nvim_socket_path: PathBuf::from("/tmp/via-nvim.sock"),
            editor_socket_path: PathBuf::from("/tmp/via-editor.sock"),
            nvim_context_bridge_path: PathBuf::from("/repo/nvim/context bridge.lua"),
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
    fn classifies_double_click_when_same_cell_within_threshold() {
        let first = ReferenceClick {
            at: Instant::now(),
            pane_index: 1,
            row: 3,
            column: 12,
        };
        let second = ReferenceClick {
            at: first.at + Duration::from_millis(120),
            pane_index: 1,
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
    fn rejects_double_click_when_cell_or_pane_differs() {
        let first = ReferenceClick {
            at: Instant::now(),
            pane_index: 1,
            row: 3,
            column: 12,
        };
        let different_cell = ReferenceClick {
            at: first.at + Duration::from_millis(120),
            pane_index: 1,
            row: 4,
            column: 12,
        };
        let different_pane = ReferenceClick {
            at: first.at + Duration::from_millis(120),
            pane_index: 0,
            row: 3,
            column: 12,
        };

        assert!(!is_double_click(
            Some(&first),
            &different_cell,
            Duration::from_millis(300)
        ));
        assert!(!is_double_click(
            Some(&first),
            &different_pane,
            Duration::from_millis(300)
        ));
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
