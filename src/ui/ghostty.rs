use std::collections::HashMap;
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

#[cfg(all(unix, not(target_os = "macos")))]
use winit::platform::wayland::WindowAttributesExtWayland;

use crate::acp_tui::{
    AcpTuiBridge, HostToTui, TranscriptKind, TuiToHost, resolve_acp_tui_bin, socket_path_for_agent,
    spawn_env_and_args,
};
use crate::config::{
    AppContext, ConfigFileWatcher, PRIMARY_PTY_AGENT_ID, ReviewBackend, UserConfig,
};
use crate::event::{AcpHandshakeAction, AcpModalKind, EditorEvent, Event, UiCommand, UiEvent};
use crate::mediator::EventSender;
use crate::pty::{CoalescedOutputNotifier, OutputNotifier};
use crate::reference_index::ReferenceIndex;

use self::links::ReferenceContext;

mod acp_modal;
mod activity;
mod config;
mod font;
mod input;
pub mod layout;
pub mod links;
mod pane;
mod pane_controller;
mod render;
mod tab_strip;

use acp_modal::{AcpModalQueue, AcpModalState, render_acp_modal_buffer};
use activity::PaneActivity;
use config::{TerminalConfig, TerminalMetrics};
use font::FontRenderer;
use input::{Key, Modifiers};
use layout::{
    AgentArea, AgentAreaMode, AgentPaneShare, FrameLayout, PaneLayoutMode, PaneLayoutState,
    PaneRect, PaneSplitDirection, PaneTargets, SplitLayout, SplitLayoutOptions,
    adjust_pane_indices_after_removal, agent_region_span, chord_keeps_geometry,
    focus_nvim_after_agent_reference, handle_layout_shortcuts, layout_mode_after_sync,
    vertical_split_fits,
};
use pane::TerminalPane;
use pane_controller::{PaneCommand, PaneEventOutcome, PaneRole, TerminalPaneController};
use render::{DamageRect, draw_ratatui_buffer};
use tab_strip::{TabStripEntry, tab_strip_buffer, tab_strip_hit_boxes};

const INITIAL_WIDTH: usize = 960;
const INITIAL_HEIGHT: usize = 540;
/// Wayland app_id / X11 WM_CLASS (`WindowAttributes::with_name`). Omarchy tags
/// `TUI.*` as terminals at map time, so Super+V/C become Shift+Insert / Ctrl+Insert.
#[cfg(all(unix, not(target_os = "macos")))]
const LINUX_APP_ID: &str = "TUI.via";
const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const REPEATED_ARROW_REDRAW_INTERVAL: Duration = Duration::from_millis(24);
const INPUT_LAG_WARN_THRESHOLD: Duration = Duration::from_millis(50);
const RENDER_WARN_THRESHOLD: Duration = Duration::from_millis(30);
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(750);
const CONFIG_POLL_INTERVAL: Duration = Duration::from_millis(400);

pub struct GhosttyUi {
    app: AppContext,
    events: EventSender,
    ui_commands: TokioReceiver<UiCommand>,
    pending_agent_write: Option<PendingAgentWrite>,
}

struct PendingAgentWrite {
    ready_at: Instant,
    bytes: Vec<u8>,
}

impl GhosttyUi {
    pub fn new(
        app: AppContext,
        events: EventSender,
        ui_commands: TokioReceiver<UiCommand>,
    ) -> Self {
        Self {
            app,
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
    app: AppContext,
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
    panes: PaneStore,
    review_pane: Option<TerminalPaneController>,
    review_active: bool,
    acp_modal: AcpModalQueue,
    /// Control-plane bridges for PTY-hosted `via --acp-tui` panes (keyed by agent id).
    acp_tui_bridges: HashMap<String, AcpTuiBridge>,
    /// What the ACP TUIs were last told about the tab strip, so a change is sent once.
    acp_tui_strip_visible: bool,
    layout_state: PaneLayoutState,
    /// Which panes the agent region shows, and in what tab order.
    ///
    /// Defaults to `Split`, which reproduces the pre-tabs geometry exactly.
    agent_area: AgentArea,
    /// Split was auto-collapsed because the window is too narrow; restore on widen.
    split_collapsed_for_width: bool,
    /// The frame from just before a tab switch, when the switch can be repainted
    /// regionally.
    ///
    /// Set by a tab switch whose geometry did not move: the visible agent pane
    /// changes but the editor does not, so the repaint must stay inside the agent
    /// region. `render` feeds this to [`tab_switch_region`], the same decision
    /// `relayout_for_tab_switch` used, so the flag and the drawn region agree.
    tab_switch_from: Option<FrameLayout>,
    /// The strip cells currently on screen, so a change can be detected.
    ///
    /// Indicators move on background output (which only sets `dirty`) and on the
    /// busy window expiring (which has no event at all), so the strip cannot rely on
    /// a full redraw or a tab switch to repaint. Comparing against this is what makes
    /// a new dot show up, and what lets `draw_ratatui_buffer` keep its early return.
    last_drawn_strip_entries: Option<Vec<TabStripEntry>>,
    layout: FrameLayout,
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
    next_config_poll_at: Instant,
    config_watcher: ConfigFileWatcher,
    /// Resolved config the mediator has not accepted yet.
    pending_config_reload: Option<UserConfig>,
    error: Option<anyhow::Error>,
    file_index: ReferenceIndex,
}

/// Stable identity for one pane slot.
///
/// Panes are addressed by id rather than by their position in [`PaneStore`], so
/// spawning, terminating, or reordering panes never renumbers the survivors.
/// Ids are never reused, so a stale id cannot alias a freshly spawned pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

impl PaneId {
    /// Raw numeric value, for diagnostics and labels.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Build an id from a raw value.
    ///
    /// Test-only: production ids come from [`PaneStore::allocate_id`], which never
    /// reuses a value. This exists so tests and pure renderers can name a pane
    /// without standing up a store.
    #[cfg(test)]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

struct PaneSlot {
    id: PaneId,
    pane: AppPane,
    /// Host-side work state for this pane (busy / exited / unseen output).
    activity: PaneActivity,
}

/// Ordered pane slots plus the active pane identity.
///
/// `slots` order is the layout order (editor first, then agents) and is what the
/// positional layout layer indexes. `active` is a [`PaneId`], so removing or
/// reordering slots cannot silently move focus to a different pane.
#[derive(Default)]
struct PaneStore {
    slots: Vec<PaneSlot>,
    active: Option<PaneId>,
    next_id: u64,
}

impl PaneStore {
    fn allocate_id(&mut self) -> PaneId {
        // Start at 1 so a default `PaneId(0)` is never a live pane.
        self.next_id = self.next_id.max(1);
        let id = PaneId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Append a pane and return its new id. The first pane becomes active.
    fn push(&mut self, pane: AppPane) -> PaneId {
        let id = self.allocate_id();
        self.slots.push(PaneSlot {
            id,
            pane,
            activity: PaneActivity::new(),
        });
        if self.active.is_none() {
            self.active = Some(id);
        }
        id
    }

    /// Swap the pane inside `id`'s slot, keeping the slot and its identity.
    /// The old pane is dropped, and its activity resets because a replacement is
    /// a fresh process with no history.
    /// Returns false when `id` is unknown.
    fn replace(&mut self, id: PaneId, pane: AppPane) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        self.slots[index].pane = pane;
        self.slots[index].activity = PaneActivity::new();
        true
    }

    /// Apply an ACP progress update to the pane hosting `agent_id`.
    fn apply_acp_progress(&mut self, agent_id: &str, id: &str, active: bool) {
        let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.pane.agent_id_matches(agent_id))
        else {
            return;
        };
        slot.activity.apply_acp_progress(id, active);
    }

    /// Mark the pane with this id as seen, returning whether it changed.
    fn mark_seen(&mut self, id: PaneId) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let activity = &mut self.slots[index].activity;
        let was_unseen = activity.unseen_output;
        activity.mark_seen();
        was_unseen
    }

    /// Activity for one pane.
    fn activity(&self, id: PaneId) -> Option<&PaneActivity> {
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| &slot.activity)
    }

    /// Remove the slot at `index`. When it held the active pane, focus moves to
    /// the pane that takes its place (or the new last pane).
    fn remove_index(&mut self, index: usize) -> Option<PaneSlot> {
        if index >= self.slots.len() {
            return None;
        }
        let slot = self.slots.remove(index);
        if self.active == Some(slot.id) {
            let next = index.min(self.slots.len().saturating_sub(1));
            self.active = self.slots.get(next).map(|slot| slot.id);
        }
        Some(slot)
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Every slot id in slot order. Test-only: the layout path asks
    /// [`WinitGhosttyApp::editor_and_agent_ids`] instead, because it needs the
    /// editor separated from the agents.
    #[cfg(test)]
    fn ids(&self) -> Vec<PaneId> {
        self.slots.iter().map(|slot| slot.id).collect()
    }

    fn id_at(&self, index: usize) -> Option<PaneId> {
        self.slots.get(index).map(|slot| slot.id)
    }

    fn index_of(&self, id: PaneId) -> Option<usize> {
        self.slots.iter().position(|slot| slot.id == id)
    }

    fn get(&self, id: PaneId) -> Option<&AppPane> {
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| &slot.pane)
    }

    fn get_mut(&mut self, id: PaneId) -> Option<&mut AppPane> {
        self.slots
            .iter_mut()
            .find(|slot| slot.id == id)
            .map(|slot| &mut slot.pane)
    }

    fn first_mut(&mut self) -> Option<&mut AppPane> {
        self.slots.first_mut().map(|slot| &mut slot.pane)
    }

    fn iter(&self) -> std::slice::Iter<'_, PaneSlot> {
        self.slots.iter()
    }

    fn iter_mut(&mut self) -> std::slice::IterMut<'_, PaneSlot> {
        self.slots.iter_mut()
    }

    fn active(&self) -> Option<PaneId> {
        self.active
    }

    fn active_index(&self) -> Option<usize> {
        self.active.and_then(|id| self.index_of(id))
    }

    fn set_active(&mut self, id: PaneId) {
        if self.index_of(id).is_some() {
            self.active = Some(id);
        }
    }
}

enum AppPane {
    Terminal(TerminalPaneController),
}

impl AppPane {
    fn is_agent_pane(&self) -> bool {
        matches!(self, Self::Terminal(pane) if matches!(pane.role(), PaneRole::AgentTerminal { .. }))
    }

    fn is_agent_terminal(&self) -> bool {
        matches!(self, Self::Terminal(pane) if matches!(pane.role(), PaneRole::AgentTerminal { .. }))
    }

    fn take_new_symbol_queries(&mut self) -> Vec<String> {
        let Self::Terminal(pane) = self;
        if matches!(pane.role(), PaneRole::AgentTerminal { .. }) {
            pane.take_new_symbol_queries()
        } else {
            Vec::new()
        }
    }

    fn forget_symbol_queries(&mut self, names: &[String]) {
        let Self::Terminal(pane) = self;
        pane.forget_symbol_queries(names);
    }

    fn agent_id_matches(&self, want: &str) -> bool {
        matches!(self, Self::Terminal(pane) if matches!(pane.role(), PaneRole::AgentTerminal { id, .. } if id == want))
    }

    /// Short display label for this agent pane, used as its tab title.
    fn agent_label(&self) -> Option<String> {
        let Self::Terminal(pane) = self;
        match pane.role() {
            PaneRole::AgentTerminal { label, .. } => Some(label.clone()),
            _ => None,
        }
    }

    /// Discovery record for an agent pane, or `None` for non-agent panes.
    fn agent_record(&self) -> Option<crate::agent_bus::AgentRecord> {
        let Self::Terminal(pane) = self;
        match pane.role() {
            PaneRole::AgentTerminal {
                id, label, acp_tui, ..
            } => {
                let command = pane.command().map(str::to_string);
                Some(crate::agent_bus::AgentRecord {
                    id: id.clone(),
                    role: Some(label.clone()),
                    command: command.clone(),
                    mode: Some(if *acp_tui {
                        crate::agent_bus::AgentMode::Acp
                    } else {
                        crate::agent_bus::AgentMode::Pty
                    }),
                    primary: id == PRIMARY_PTY_AGENT_ID,
                })
            }
            _ => None,
        }
    }

    fn as_terminal_mut(&mut self) -> Option<&mut TerminalPaneController> {
        let Self::Terminal(pane) = self;
        Some(pane)
    }

    /// Bus id of this pane if it is any kind of agent pane.
    fn agent_id(&self) -> Option<String> {
        let Self::Terminal(pane) = self;
        match pane.role() {
            PaneRole::AgentTerminal { id, .. } => Some(id.clone()),
            _ => None,
        }
    }

    fn resize_with_metrics(&mut self, width: usize, height: usize, metrics: TerminalMetrics) {
        let Self::Terminal(pane) = self;
        pane.resize_with_metrics(width, height, metrics);
    }

    fn apply_theme(&mut self, theme: &config::TerminalTheme) {
        let Self::Terminal(pane) = self;
        pane.apply_theme(theme);
    }

    /// Forward the new theme to the running program inside the pane. For PTY
    /// panes this injects a DSR 997 color-scheme notification, letting agents
    /// such as OpenCode update their `system` theme without a restart.
    fn notify_theme_changed(&mut self, theme: &config::TerminalTheme) -> Result<()> {
        let Self::Terminal(pane) = self;
        pane.write_all(&theme.color_scheme_notification())
    }

    fn clear_selection(&mut self) -> bool {
        let Self::Terminal(pane) = self;
        pane.clear_selection()
    }

    fn handle_modifiers_changed(
        &mut self,
        modifiers: Modifiers,
        ctx: ReferenceContext<'_>,
    ) -> PaneEventOutcome {
        let Self::Terminal(pane) = self;
        pane.handle_modifiers_changed(modifiers, ctx)
    }

    fn drain_output(&mut self) -> bool {
        let Self::Terminal(pane) = self;
        pane.drain_output()
    }

    fn has_exited_agent(&mut self) -> bool {
        let Self::Terminal(pane) = self;
        pane.has_exited()
    }

    fn editor_has_exited(&mut self) -> bool {
        let Self::Terminal(pane) = self;
        pane.editor_has_exited()
    }

    /// Launch metadata for the primary agent pane, used when respawning after exit.
    fn primary_agent_launch(&self) -> Option<(String, Option<String>)> {
        let Self::Terminal(pane) = self;
        match pane.role() {
            PaneRole::AgentTerminal {
                id, label, command, ..
            } if id == PRIMARY_PTY_AGENT_ID => Some((label.clone(), command.clone())),
            _ => None,
        }
    }

    fn terminate_agent(&mut self) -> Result<()> {
        let Self::Terminal(pane) = self;
        pane.terminate_agent()
    }

    fn is_terminable_sub_agent(&self) -> bool {
        matches!(
            self,
            Self::Terminal(pane) if matches!(
                pane.role(),
                PaneRole::AgentTerminal { id, .. } if id != PRIMARY_PTY_AGENT_ID
            )
        )
    }

    fn drain_agent_output_chunks(&mut self) -> Option<Vec<Vec<u8>>> {
        let Self::Terminal(pane) = self;
        // ACP TUI PTY output is CSI for Ghostty VT only — do not treat as agent protocol.
        if pane.is_acp_tui() {
            return None;
        }
        if matches!(pane.role(), PaneRole::AgentTerminal { .. }) {
            Some(pane.drain_output_chunks())
        } else {
            None
        }
    }

    fn is_acp_tui_agent(&self) -> bool {
        matches!(self, Self::Terminal(pane) if pane.is_acp_tui())
    }

    fn handle_key_event(
        &mut self,
        pressed_keys: &[Key],
        key: Option<Key>,
        text: Option<&str>,
        repeat: bool,
        modifiers: Modifiers,
        suppress_input: bool,
    ) -> Result<PaneEventOutcome> {
        let Self::Terminal(pane) = self;
        pane.handle_terminal_key(pressed_keys, key, text, repeat, modifiers, suppress_input)
    }

    fn handle_text_commit(&mut self, text: &str, modifiers: Modifiers) -> Result<PaneEventOutcome> {
        let Self::Terminal(pane) = self;
        pane.handle_text_commit(text, modifiers)
    }

    fn handle_mouse_wheel(
        &mut self,
        scroll_delta: (f32, f32),
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        let Self::Terminal(pane) = self;
        pane.handle_mouse_wheel(scroll_delta, local_x, local_y, modifiers)
    }

    fn handle_mouse_input(
        &mut self,
        state: ElementState,
        button: MouseButton,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
        ctx: ReferenceContext<'_>,
    ) -> Result<PaneEventOutcome> {
        let Self::Terminal(pane) = self;
        pane.handle_mouse_input(state, button, local_x, local_y, modifiers, ctx)
    }

    fn handle_mouse_motion(
        &mut self,
        local_x: usize,
        local_y: usize,
        modifiers: Modifiers,
    ) -> Result<PaneEventOutcome> {
        let Self::Terminal(pane) = self;
        pane.handle_mouse_motion(local_x, local_y, modifiers)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        font_renderer: &mut FontRenderer,
        buffer: &mut [u32],
        buffer_width: usize,
        buffer_height: usize,
        rect: PaneRect,
        active: bool,
        force_redraw: bool,
        redraw_chrome: bool,
        damage: &mut Vec<DamageRect>,
    ) -> bool {
        let Self::Terminal(pane) = self;
        pane.draw(
            font_renderer,
            buffer,
            buffer_width,
            buffer_height,
            rect,
            active,
            force_redraw,
            redraw_chrome,
            damage,
        )
    }
}

impl WinitGhosttyApp {
    fn new(ui: GhosttyUi, event_loop_proxy: EventLoopProxy<UserEvent>) -> Result<Self> {
        let output_notifier = CoalescedOutputNotifier::new(event_loop_proxy);
        let terminal_config = TerminalConfig::load();
        let font_renderer = FontRenderer::new(&terminal_config)?;
        let pane_split_direction = PaneSplitDirection::for_window(INITIAL_WIDTH, INITIAL_HEIGHT);
        let pane_count = pane_count(&ui.app);
        let split = SplitLayout::for_window(
            INITIAL_WIDTH,
            INITIAL_HEIGHT,
            pane_count,
            PaneLayoutMode::Split,
            pane_split_direction,
            SplitLayoutOptions::unbounded(),
        );
        // No slots exist yet; the frame carries rects only and is rebuilt by relayout.
        let layout = FrameLayout::from_split(&[], split);

        let file_index = ReferenceIndex::default();
        let config_watcher = ConfigFileWatcher::new(ui.app.cli_overrides.clone());
        Ok(Self {
            app: ui.app,
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
            panes: PaneStore::default(),
            review_pane: None,
            review_active: false,
            acp_modal: AcpModalQueue::new(),
            acp_tui_bridges: HashMap::new(),
            acp_tui_strip_visible: false,
            layout_state: PaneLayoutState {
                mode: PaneLayoutMode::Split,
                split_direction: pane_split_direction,
                active_pane: 0,
                agent_share: AgentPaneShare::Auto,
            },
            agent_area: AgentArea::new(),
            split_collapsed_for_width: false,
            tab_switch_from: None,
            last_drawn_strip_entries: None,
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
            next_config_poll_at: Instant::now(),
            config_watcher,
            pending_config_reload: None,
            error: None,
            file_index,
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
        #[cfg(all(unix, not(target_os = "macos")))]
        let attributes = attributes.with_name(LINUX_APP_ID, "via");
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
            let editor =
                self.create_editor_pane(self.width, self.height, self.terminal_config.metrics)?;
            self.panes.push(editor);
            self.sync_active_pane_from_store();
            let nvim_args = nvim_args(&self.app);
            let Some(editor_pane) = self.panes.first_mut().and_then(AppPane::as_terminal_mut)
            else {
                return Err(anyhow!("editor pane is not a terminal"));
            };
            editor_pane.spawn(
                &self.app.user.nvim_command,
                nvim_args,
                &self.app.launch.working_directory,
                &[],
                self.output_notifier.clone(),
            )?;
            if let Some(agent_command) = self.app.user.agent_command.clone() {
                self.create_agent_pane(
                    PRIMARY_PTY_AGENT_ID,
                    Some("agent"),
                    Some(agent_command.as_str()),
                )?;
                if let Some(id) = self
                    .panes
                    .iter()
                    .find(|slot| slot.pane.agent_id_matches(PRIMARY_PTY_AGENT_ID))
                    .map(|slot| slot.id)
                {
                    // A narrow window may have collapsed the split onto the editor.
                    // Keys should follow the pane that is actually visible.
                    if self.layout_state.mode == PaneLayoutMode::Split {
                        self.panes.set_active(id);
                        self.sync_active_pane_from_store();
                    }
                }
            } else {
                self.relayout();
                self.write_agent_registry();
            }
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

    fn create_editor_pane(
        &self,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Result<AppPane> {
        let layout = SplitLayout::for_window(
            width,
            height,
            pane_count(&self.app),
            PaneLayoutMode::Split,
            PaneSplitDirection::for_window(width, height),
            self.split_layout_options(),
        );
        Ok(AppPane::Terminal(TerminalPaneController::new(
            PaneRole::Editor,
            TerminalPane::new(
                "nvim",
                layout.pane(0).width,
                layout.pane(0).height,
                metrics,
                &self.terminal_config.theme,
            )?,
            self.app.user.scroll_sensitivity,
        )))
    }

    fn create_agent_pane(
        &mut self,
        id: &str,
        role: Option<&str>,
        command: Option<&str>,
    ) -> Result<()> {
        // Deduplicate: if a pane with this id already exists, do nothing.
        if self.panes.iter().any(|slot| slot.pane.agent_id_matches(id)) {
            info!(%id, "create_agent_pane called for existing id – ignoring");
            return Ok(());
        }

        let metrics = self.terminal_config.metrics;
        // Use current layout dimensions to size the new pane; relayout will adjust.
        let (w, h) = (self.width, self.height);
        let pane = self.spawn_agent_terminal(id, role, command, w, h, metrics)?;
        self.panes.push(pane);

        self.relayout();
        self.write_agent_registry();
        self.dirty = true;
        self.force_redraw = true;
        Ok(())
    }

    /// Spawn an agent pane and return it without inserting it into the store.
    ///
    /// Both [`Self::create_agent_pane`] (new slot) and
    /// [`Self::respawn_primary_agent_pane`] (existing slot) go through here, so a
    /// respawn cannot drift from a first spawn.
    fn spawn_agent_terminal(
        &mut self,
        id: &str,
        role: Option<&str>,
        command: Option<&str>,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Result<AppPane> {
        let label = role.unwrap_or(id).to_string();
        let role_label = role.unwrap_or(id);
        // Prefer the command passed to SpawnAgent, then the configured agent_command,
        // finally fall back to $SHELL so the pane is at least usable.
        let cmd = command
            .map(str::to_string)
            .or_else(|| self.app.user.agent_command.clone())
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()));

        if crate::config::is_acp_command(&cmd) {
            return self
                .spawn_acp_tui_terminal(id, &label, role_label, &cmd, width, height, metrics);
        }

        // Leak a small string for the static title requirement of TerminalPane.
        let title: &'static str = Box::leak(label.clone().into_boxed_str());
        let mut pane = TerminalPane::new(
            title,
            width / 2,
            height / 2,
            metrics,
            &self.terminal_config.theme,
        )?;
        pane.spawn_shell_command(
            &cmd,
            &self.app.launch.working_directory,
            &[
                (crate::agent_bus::VIA_AGENT_ID_ENV, id),
                (crate::agent_bus::VIA_AGENT_ROLE_ENV, role_label),
            ],
            self.output_notifier.clone(),
        )?;
        Ok(AppPane::Terminal(TerminalPaneController::new(
            PaneRole::AgentTerminal {
                id: id.to_string(),
                label,
                command: Some(cmd),
                acp_tui: false,
            },
            pane,
            self.app.user.scroll_sensitivity,
        )))
    }

    /// Bind the ACP UI socket and spawn `via --acp-tui`, returning the pane uninserted.
    #[allow(clippy::too_many_arguments)]
    fn spawn_acp_tui_terminal(
        &mut self,
        id: &str,
        label: &str,
        role_label: &str,
        acp_command: &str,
        width: usize,
        height: usize,
        metrics: TerminalMetrics,
    ) -> Result<AppPane> {
        let runtime_dir = crate::config::runtime_base_dir();
        let socket_path = socket_path_for_agent(&runtime_dir, id);
        let bridge = AcpTuiBridge::bind(id, socket_path.clone())
            .with_context(|| format!("bind ACP UI socket for agent {id}"))?;
        let socket = bridge.socket_path().to_path_buf();
        let socket_str = socket
            .to_str()
            .with_context(|| format!("ACP UI socket path is not UTF-8: {}", socket.display()))?
            .to_string();

        let bin = resolve_acp_tui_bin()?;
        let bin_str = bin
            .to_str()
            .with_context(|| format!("via binary path is not UTF-8: {}", bin.display()))?
            .to_string();
        let light = self.terminal_config.theme.is_light();
        let (env, args) = spawn_env_and_args(id, role_label, &socket_str, light);

        let title: &'static str = Box::leak(label.to_string().into_boxed_str());
        let mut pane = TerminalPane::new(
            title,
            width / 2,
            height / 2,
            metrics,
            &self.terminal_config.theme,
        )?;
        pane.spawn(
            &bin_str,
            args.iter().map(|s| OsString::from(*s)),
            &self.app.launch.working_directory,
            &env,
            self.output_notifier.clone(),
        )
        .with_context(|| format!("spawn via --acp-tui for agent {id}"))?;

        self.acp_tui_bridges.insert(id.to_string(), bridge);
        info!(%id, socket = %socket.display(), bin = %bin.display(), "spawned ACP TUI pane");
        Ok(AppPane::Terminal(TerminalPaneController::new(
            PaneRole::AgentTerminal {
                id: id.to_string(),
                label: label.to_string(),
                command: Some(acp_command.to_string()),
                acp_tui: true,
            },
            pane,
            self.app.user.scroll_sensitivity,
        )))
    }

    fn terminate_agent_pane(&mut self, id: &str) -> Result<()> {
        if id == PRIMARY_PTY_AGENT_ID {
            info!(%id, "terminate_agent called for primary PTY agent – ignoring");
            return Ok(());
        }

        let Some(index) = self
            .panes
            .iter()
            .position(|slot| slot.pane.agent_id_matches(id))
        else {
            info!(%id, "terminate_agent called for unknown id – ignoring");
            return Ok(());
        };

        if !self.panes.slots[index].pane.is_terminable_sub_agent() {
            info!(%id, "terminate_agent refused for non-sub-agent pane");
            return Ok(());
        }

        self.panes.slots[index].pane.terminate_agent()?;
        self.remove_pane_at(index);
        self.close_acp_tui_bridge(id);

        self.acp_modal.remove_agent(id);

        self.relayout();
        self.write_agent_registry();
        self.dirty = true;
        self.force_redraw = true;
        info!(%id, "terminated agent pane");
        Ok(())
    }

    /// Persist the current set of agent panes to the registry so `via agent list`
    /// (and Lua's `require('via').agent.list()`) can discover them. Best-effort.
    fn write_agent_registry(&self) {
        let records: Vec<crate::agent_bus::AgentRecord> = self
            .panes
            .iter()
            .filter_map(|slot| slot.pane.agent_record())
            .collect();
        if let Err(error) = crate::agent_bus::write_registry(&self.app.paths.agents_dir, &records) {
            warn!(%error, "failed to write agent registry");
        }
    }

    /// Drop terminal agent panes whose PTY child has exited and refresh the registry.
    /// The primary agent is respawned instead of removed.
    fn prune_exited_agent_panes(&mut self) {
        let mut acp_tui_exited: Vec<String> = Vec::new();
        let mut pty_remove: Vec<PaneId> = Vec::new();
        let mut primary_respawn: Option<(PaneId, String, Option<String>)> = None;
        for slot in self.panes.iter_mut() {
            let PaneSlot { id, pane, activity } = slot;
            if !pane.has_exited_agent() {
                continue;
            }
            // Record the exit before the pane is dropped. ACP TUI panes are removed
            // by the mediator a moment later, so their slot may still be on screen.
            activity.mark_exited();
            if pane.agent_id_matches(PRIMARY_PTY_AGENT_ID) {
                if let Some((label, command)) = pane.primary_agent_launch() {
                    primary_respawn = Some((*id, label, command));
                }
                continue;
            }
            if pane.is_acp_tui_agent() {
                if let Some(id) = pane.agent_id() {
                    acp_tui_exited.push(id);
                }
            } else {
                pty_remove.push(*id);
            }
        }

        for id in acp_tui_exited {
            self.close_acp_tui_bridge(&id);
            // Tear down the mediator ACP session; UiCommand::TerminateAgent removes the pane.
            self.events
                .try_send(Event::Editor(EditorEvent::TerminateAgent { id }));
        }

        let mut removed_any = false;
        for id in pty_remove {
            if self.remove_pane_by_id(id) {
                removed_any = true;
            }
        }

        // The primary is addressed by id, so removals above cannot invalidate it.
        if let Some((id, label, command)) = primary_respawn {
            if let Err(error) = self.respawn_primary_agent_pane(id, &label, command.as_deref()) {
                error!(%error, "failed to respawn primary agent pane");
            }
            removed_any = true;
        }

        if !removed_any {
            return;
        }

        self.write_agent_registry();
        self.relayout();
        self.dirty = true;
        self.force_redraw = true;
        info!("pruned/respawned exited agent pane(s); refreshed agent registry");
    }

    /// Replace an exited primary agent pane in its own slot, keeping its [`PaneId`].
    fn respawn_primary_agent_pane(
        &mut self,
        id: PaneId,
        label: &str,
        command: Option<&str>,
    ) -> Result<()> {
        if !self
            .panes
            .get(id)
            .is_some_and(|pane| pane.agent_id_matches(PRIMARY_PTY_AGENT_ID))
        {
            return Ok(());
        }

        if self.panes.get(id).is_some_and(AppPane::is_acp_tui_agent) {
            self.close_acp_tui_bridge(PRIMARY_PTY_AGENT_ID);
        }

        let metrics = self.terminal_config.metrics;
        let (w, h) = (self.width, self.height);
        // Keep the exited slot on failure so the next drain can retry instead of
        // leaving the session without a primary agent.
        let pane =
            self.spawn_agent_terminal(PRIMARY_PTY_AGENT_ID, Some(label), command, w, h, metrics)?;

        if !self.panes.replace(id, pane) {
            warn!(?id, "primary agent pane vanished before respawn completed");
            return Ok(());
        }
        info!(?id, "respawned primary agent pane");
        Ok(())
    }

    fn editor_has_exited(&mut self) -> bool {
        self.panes
            .iter_mut()
            .any(|slot| slot.pane.editor_has_exited())
    }

    fn request_quit(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.exit();
    }

    fn close_acp_tui_bridge(&mut self, agent_id: &str) {
        if let Some(mut bridge) = self.acp_tui_bridges.remove(agent_id) {
            bridge.shutdown();
        }
    }

    fn send_acp_tui(&mut self, agent_id: &str, msg: HostToTui) -> bool {
        let Some(bridge) = self.acp_tui_bridges.get_mut(agent_id) else {
            return false;
        };
        if let Err(err) = bridge.send(msg) {
            warn!(%agent_id, %err, "failed to send ACP TUI host message");
            return false;
        }
        true
    }

    fn poll_acp_tui_bridges(&mut self) -> Result<bool> {
        let agent_ids: Vec<String> = self.acp_tui_bridges.keys().cloned().collect();
        let mut changed = false;
        for agent_id in agent_ids {
            let Some(bridge) = self.acp_tui_bridges.get_mut(&agent_id) else {
                continue;
            };
            let messages = match bridge.poll() {
                Ok(messages) => messages,
                Err(err) => {
                    warn!(%agent_id, %err, "ACP TUI bridge poll failed");
                    continue;
                }
            };
            for msg in messages {
                match msg {
                    TuiToHost::Ready { agent_id: ready_id } => {
                        debug!(%ready_id, "ACP TUI ready");
                        let light = self.terminal_config.theme.is_light();
                        if let Err(err) = bridge.send(HostToTui::Appearance { light }) {
                            warn!(%agent_id, %err, "failed to send ACP TUI appearance");
                        }
                        // A new pane starts assuming no strip; tell it if one is up.
                        if self.acp_tui_strip_visible {
                            let _ = bridge.send(HostToTui::TabStripVisible { visible: true });
                        }
                    }
                    TuiToHost::Submit { text } => {
                        changed = true;
                        self.events
                            .try_send(Event::Ui(UiEvent::AgentPromptSubmitted {
                                text,
                                agent_id: Some(agent_id.clone()),
                            }));
                    }
                    TuiToHost::SetModel { value } => {
                        changed = true;
                        self.events.try_send(Event::Ui(UiEvent::AgentModelSelected {
                            agent_id: agent_id.clone(),
                            value,
                        }));
                    }
                    TuiToHost::ShutdownAck => {
                        debug!(%agent_id, "ACP TUI shutdown ack");
                    }
                }
            }
        }
        Ok(changed)
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
        self.layout_state.split_direction = PaneSplitDirection::adjust_for_window_resize(
            self.layout_state.split_direction,
            width,
            height,
        );
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
            agent_pane_cols: self.app.user.agent_pane_col_limits(),
            agent_share: self.layout_state.agent_share,
        }
    }

    fn adjust_layout_mode_for_width(&mut self) {
        if self.panes.len() < 2 {
            return;
        }

        let fits = match (
            self.app.user.agent_pane_col_limits(),
            self.layout_state.split_direction,
        ) {
            (Some((agent_min, _)), PaneSplitDirection::Vertical) => vertical_split_fits(
                self.width,
                self.terminal_config.metrics.cell_width,
                agent_min,
            ),
            _ => true,
        };

        if self.layout_state.mode == PaneLayoutMode::Split && !fits {
            self.layout_state.mode = PaneLayoutMode::EditorFull;
            self.split_collapsed_for_width = true;
            return;
        }

        if self.layout_state.mode == PaneLayoutMode::EditorFull
            && self.split_collapsed_for_width
            && fits
        {
            self.layout_state.mode = PaneLayoutMode::Split;
            self.split_collapsed_for_width = false;
        }
    }

    /// The pane ids and editor index the layout layer needs to address tabs.
    ///
    /// The layout layer works in slot indices; tab addressing needs identity, and the
    /// editor's slot is not always 0-by-assumption, so both come from one place.
    fn pane_targets(&self) -> PaneTargets {
        let (editor, _) = self.editor_and_agent_ids();
        let slots: Vec<PaneId> = self.panes.iter().map(|slot| slot.id).collect();
        let editor_index = editor.and_then(|id| slots.iter().position(|slot| *slot == id));
        PaneTargets {
            slots,
            editor_index,
        }
    }

    /// Split the live slots into the editor id and the agent ids, in slot order.
    ///
    /// The editor is the one pane that is never a tab, so the agent region model
    /// needs it separately to keep its own invariant. It is `None` only if the
    /// slots somehow hold no editor, which the startup path rules out.
    fn editor_and_agent_ids(&self) -> (Option<PaneId>, Vec<PaneId>) {
        let mut editor = None;
        let mut agents = Vec::new();
        for slot in self.panes.iter() {
            if slot.pane.is_agent_pane() {
                agents.push(slot.id);
            } else if editor.is_none() {
                editor = Some(slot.id);
            }
        }
        (editor, agents)
    }

    fn relayout(&mut self) {
        if self.panes.is_empty() {
            return;
        }

        self.adjust_layout_mode_for_width();

        // Tabs are a pure model over pane identity: reconcile it with the live
        // panes, then ask it which panes this frame should place. In the default
        // `Split` mode this list is the editor plus every agent in slot order,
        // i.e. exactly the pre-tabs geometry.
        let (editor, agents) = self.editor_and_agent_ids();
        sync_agent_area(&mut self.agent_area, &mut self.panes, &agents, editor);
        // `sync_agent_area` may have moved keyboard focus onto the active tab. The slot
        // index has to follow, or the Alt-key path and the frame disagree about focus.
        self.sync_active_pane_from_store();
        // Closing the last agent while one was fullscreen would leave nothing to fill
        // the window, so the mode falls back to Split and the editor comes back.
        self.layout_state.mode =
            layout_mode_after_sync(self.layout_state.mode, self.agent_area.active().is_some());
        self.sync_acp_tui_strip_state();
        let ids = self
            .agent_area
            .visible_panes(editor, &agents, self.layout_state.mode);
        let split = SplitLayout::for_window(
            self.width,
            self.height,
            ids.len().max(1),
            self.layout_state.mode,
            self.layout_state.split_direction,
            self.split_layout_options(),
        );
        let new_layout = FrameLayout::from_split(&ids, split);
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
        let placed = self.layout.placed().to_vec();

        for placed in placed {
            let rect = self.effective_pane_rect(placed.id, placed.rect);
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let Some(slot) = self.panes.get_mut(placed.id) else {
                continue;
            };
            slot.resize_with_metrics(rect.width, rect.height, metrics);
        }

        if let Some(review_pane) = &mut self.review_pane {
            let rect = full_window_rect(self.width, self.height);
            if rect.width != 0 && rect.height != 0 {
                review_pane.resize_with_metrics(rect.width, rect.height, metrics);
            }
        }
    }

    fn send_acp_handshake_action(&mut self, agent_id: String, action: AcpHandshakeAction) {
        self.acp_modal.resolve();
        self.events
            .try_send(Event::Ui(UiEvent::AcpHandshakeAction { agent_id, action }));
    }

    fn handle_acp_modal_winit_key(&mut self, event: &KeyEvent) -> bool {
        if self.acp_modal.is_empty() {
            return false;
        }
        if self.modifiers.ctrl || self.modifiers.super_key {
            return false;
        }
        let PhysicalKey::Code(code) = event.physical_key else {
            return false;
        };
        if code == KeyCode::Tab {
            if self.modifiers.shift {
                self.acp_modal.prev();
            } else {
                self.acp_modal.next();
            }
            return true;
        }
        let kind = self.acp_modal.active().map(|m| m.kind.clone());
        let Some(kind) = kind else {
            return false;
        };
        if kind == AcpModalKind::HandshakeRetry {
            match code {
                KeyCode::Escape => {
                    let agent_id = self.acp_modal.active().unwrap().agent_id.clone();
                    self.send_acp_handshake_action(agent_id, AcpHandshakeAction::Dismiss);
                    true
                }
                KeyCode::Enter | KeyCode::NumpadEnter => {
                    let modal = self.acp_modal.active().unwrap();
                    let agent_id = modal.agent_id.clone();
                    let option_id = modal
                        .options
                        .get(modal.focused)
                        .map(|opt| opt.option_id.as_str())
                        .unwrap_or("retry");
                    let action = match option_id {
                        "discard" => AcpHandshakeAction::DiscardQueued,
                        _ => AcpHandshakeAction::Retry,
                    };
                    self.send_acp_handshake_action(agent_id, action);
                    true
                }
                KeyCode::ArrowUp => {
                    self.acp_modal.active_mut().unwrap().move_focus(-1);
                    true
                }
                KeyCode::ArrowDown => {
                    self.acp_modal.active_mut().unwrap().move_focus(1);
                    true
                }
                KeyCode::Digit1 | KeyCode::Numpad1 => self.acp_handshake_select_digit(0),
                KeyCode::Digit2 | KeyCode::Numpad2 => self.acp_handshake_select_digit(1),
                _ => false,
            }
        } else {
            match code {
                KeyCode::Escape => {
                    let modal = self.acp_modal.active().unwrap();
                    let agent_id = modal.agent_id.clone();
                    let id = modal.jsonrpc_id.clone();
                    let result = modal.result_cancelled();
                    self.acp_modal.resolve();
                    self.events.try_send(Event::Ui(UiEvent::AcpJsonRpcResult {
                        agent_id,
                        id,
                        result,
                    }));
                    true
                }
                KeyCode::Enter | KeyCode::NumpadEnter => {
                    let modal = self.acp_modal.active().unwrap();
                    let agent_id = modal.agent_id.clone();
                    let id = modal.jsonrpc_id.clone();
                    let focused = modal.focused;
                    let result = modal.result_for_selection(focused);
                    self.acp_modal.resolve();
                    self.events.try_send(Event::Ui(UiEvent::AcpJsonRpcResult {
                        agent_id,
                        id,
                        result,
                    }));
                    true
                }
                KeyCode::ArrowUp => {
                    self.acp_modal.active_mut().unwrap().move_focus(-1);
                    true
                }
                KeyCode::ArrowDown => {
                    self.acp_modal.active_mut().unwrap().move_focus(1);
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
    }

    fn acp_handshake_select_digit(&mut self, index: usize) -> bool {
        let Some(modal) = self.acp_modal.active() else {
            return false;
        };
        if modal.kind != AcpModalKind::HandshakeRetry {
            return false;
        }
        let Some(option) = modal.options.get(index) else {
            return false;
        };
        let agent_id = modal.agent_id.clone();
        let action = match option.option_id.as_str() {
            "discard" => AcpHandshakeAction::DiscardQueued,
            _ => AcpHandshakeAction::Retry,
        };
        self.send_acp_handshake_action(agent_id, action);
        true
    }

    fn acp_modal_select_digit(&mut self, index: usize) -> bool {
        let Some(modal) = self.acp_modal.active() else {
            return false;
        };
        if index >= modal.options.len() {
            return false;
        }
        let agent_id = modal.agent_id.clone();
        let id = modal.jsonrpc_id.clone();
        let result = modal.result_for_selection(index);
        self.acp_modal.resolve();
        self.events.try_send(Event::Ui(UiEvent::AcpJsonRpcResult {
            agent_id,
            id,
            result,
        }));
        true
    }

    fn handle_key_event(&mut self, event: KeyEvent) -> Result<()> {
        if !self.acp_modal.is_empty()
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
        let agent_span = agent_region_span(
            self.layout.split(),
            self.layout_state.split_direction,
            self.width,
            self.height,
        );
        // Snapshot both modes and the focused pane before the chord runs: entering tabs
        // or AgentFull changes geometry, and the editor's focus chrome depends on which
        // pane had focus before the switch.
        let area_mode_before = self.agent_area.mode();
        let layout_mode_before = self.layout_state.mode;
        let focus_before_shortcut = self.panes.active();
        let pane_targets = self.pane_targets();
        let consumed_chord = handle_layout_shortcuts(
            &pressed_keys,
            self.modifiers,
            event.repeat,
            self.panes.len(),
            &mut self.layout_state,
            &mut self.agent_area,
            agent_span,
            &pane_targets,
        );
        let layout_shortcut_consumed = consumed_chord.is_some();
        if let Some(chord) = consumed_chord {
            self.split_collapsed_for_width = false;
            // Shortcuts are index-based; fold the new index back into pane identity.
            self.adopt_indexed_focus();
            if chord_keeps_geometry(chord, area_mode_before, layout_mode_before) {
                // Same regional repaint a strip click uses, so an unchanged frame does
                // not repaint the editor. `relayout_for_tab_switch` owns `force_redraw`
                // here: it clears the flag `relayout` raised, and preserves a full
                // redraw that was already pending. Setting the flag afterwards would
                // undo exactly that, so it is set only on the geometry-changing path.
                self.tab_switch_from = self.relayout_for_tab_switch(focus_before_shortcut);
            } else {
                // Everything else changes geometry (mode, direction, focus) and needs
                // the full redraw.
                self.relayout();
                self.force_redraw = true;
            }
            self.dirty = true;
        }
        let Some(active_id) = self.panes.active() else {
            return Ok(());
        };
        let Some(active_pane) = self.panes.get_mut(active_id) else {
            return Ok(());
        };
        let outcome = active_pane.handle_key_event(
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
        if !pressed_keys.contains(&Key::R) {
            return Ok(false);
        }

        match self.app.user.review_backend {
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
                &self.app.launch.working_directory,
                &[],
                self.output_notifier.clone(),
            )?;
            let controller = TerminalPaneController::new(
                PaneRole::ReviewTerminal,
                pane,
                self.app.user.scroll_sensitivity,
            );
            self.review_pane = Some(controller);
        } else {
            self.reload_hunk_review();
        }

        self.review_active = true;
        Ok(())
    }

    fn reload_hunk_review(&self) {
        let repo = self.app.launch.working_directory.clone();
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
        self.layout_state.focus_maximized_editor();
        self.adopt_indexed_focus();
        self.relayout();
        self.events.try_send(Event::Ui(UiEvent::ReviewRequested));
    }

    fn handle_text_commit(&mut self, text: &str) -> Result<()> {
        if !self.acp_modal.is_empty() {
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

        let Some(active_id) = self.panes.active() else {
            return Ok(());
        };
        let Some(active_pane) = self.panes.get_mut(active_id) else {
            return Ok(());
        };
        let outcome = active_pane.handle_text_commit(text, self.modifiers)?;
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

        let Some((x, y)) = self.cursor_position else {
            return Ok(());
        };
        let Some((pane_id, local_x, local_y)) = self.pane_local_hit(x, y) else {
            return Ok(());
        };
        if let Some(pane) = self.panes.get_mut(pane_id) {
            let outcome =
                pane.handle_mouse_wheel(scroll_delta, local_x, local_y, self.modifiers)?;
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
                        ReferenceContext::new(
                            &self.app.launch.working_directory,
                            Some(&self.file_index),
                        ),
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
        // The strip owns its row, so it must be offered the click before the pane
        // hit test: otherwise a click on a tab would be routed to the agent pane
        // drawn underneath it. There is no close button, so a hit only activates.
        if state == ElementState::Pressed && self.handle_tab_strip_click(x, y) {
            return Ok(());
        }
        let Some((pane_id, local_x, local_y)) = self.pane_local_hit(x, y) else {
            return Ok(());
        };
        for slot in self.panes.iter_mut() {
            if slot.id != pane_id {
                self.dirty |= slot.pane.clear_selection();
            }
        }

        let Some(pane) = self.panes.get_mut(pane_id) else {
            return Ok(());
        };
        let outcome = pane.handle_mouse_input(
            state,
            button,
            local_x,
            local_y,
            self.modifiers,
            ReferenceContext::new(&self.app.launch.working_directory, Some(&self.file_index)),
        )?;
        let reference_navigation = Self::is_reference_navigation_command(&outcome.command);
        self.apply_pane_outcome(outcome);
        if state == ElementState::Pressed && !reference_navigation {
            self.set_active_pane(pane_id);
        }
        Ok(())
    }

    fn is_reference_navigation_command(command: &Option<PaneCommand>) -> bool {
        matches!(
            command,
            Some(
                PaneCommand::OpenRequested { .. }
                    | PaneCommand::SymbolOpenRequested { .. }
                    | PaneCommand::UrlOpenRequested { .. },
            )
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

        let Some((pane_id, local_x, local_y)) = self.pane_local_hit(x, y) else {
            return Ok(());
        };

        if self.panes.active() != Some(pane_id) {
            return Ok(());
        }

        let Some(pane) = self.panes.get_mut(pane_id) else {
            return Ok(());
        };
        let outcome = pane.handle_mouse_motion(local_x, local_y, self.modifiers)?;
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
                PaneCommand::UrlOpenRequested { url } => {
                    open_url_in_browser(&url);
                }
            }
        }
    }

    fn focus_nvim_after_reference_navigation(&mut self) {
        let focus = focus_nvim_after_agent_reference(&mut self.layout_state);
        // The helper sets a positional index; fold it back into pane identity.
        self.adopt_indexed_focus();
        if focus.relayout_needed {
            self.relayout();
        } else if focus.focus_changed {
            self.dirty = true;
            self.force_redraw = true;
        }
    }

    /// Drain PTY/UI work. Returns `Ok(true)` when Neovim has exited and via should quit.
    fn drain_background_work(&mut self) -> Result<bool> {
        // Clear the coalescing flag *before* draining so that any PTY data arriving
        // during the drain will set `pending` again and fire a new UserEvent::PtyOutput,
        // rather than being silently swallowed.
        self.output_notifier.clear();
        let now = Instant::now();
        let active_tab = self.agent_area.active();
        let mut symbol_names = Vec::new();
        let mut symbol_sources: Vec<PaneId> = Vec::new();
        for slot in self.panes.iter_mut() {
            let id = slot.id;
            let PaneSlot { pane, activity, .. } = slot;
            // The editor is never a tab, so it has no unseen state to track. Treating
            // it as an inactive tab would leave a permanent unseen dot on a pane the
            // strip never shows.
            let is_agent = pane.is_agent_pane();
            let output_is_seen = !is_agent || active_tab == Some(id);
            if let Some(chunks) = pane.drain_agent_output_chunks() {
                let drained = !chunks.is_empty();
                for chunk in &chunks {
                    if let Ok(text) = std::str::from_utf8(chunk) {
                        self.events
                            .try_send(Event::Agent(crate::event::AgentEvent::OutputChunk(
                                text.to_string(),
                            )));
                    }
                }
                self.dirty |= drained;
                if drained {
                    activity.mark_output(now, output_is_seen);
                    let before = symbol_names.len();
                    symbol_names.extend(pane.take_new_symbol_queries());
                    if symbol_names.len() != before {
                        symbol_sources.push(id);
                    }
                }
            } else {
                let drained = pane.drain_output();
                self.dirty |= drained;
                if drained {
                    activity.mark_output(now, output_is_seen);
                    let before = symbol_names.len();
                    symbol_names.extend(pane.take_new_symbol_queries());
                    if symbol_names.len() != before {
                        symbol_sources.push(id);
                    }
                }
            }
        }
        let queued: Vec<String> = symbol_names
            .iter()
            .filter(|name| !self.file_index.contains_symbol(name))
            .cloned()
            .collect();
        if !queued.is_empty()
            && !self.events.try_send(Event::Ui(UiEvent::SymbolCandidates {
                names: queued.clone(),
            }))
        {
            // The names were marked seen before the send. Put them back so the
            // next drain can try again instead of dropping them for the session.
            for id in symbol_sources {
                if let Some(pane) = self.panes.get_mut(id) {
                    pane.forget_symbol_queries(&queued);
                }
            }
        }
        if let Some(review_pane) = &mut self.review_pane {
            self.dirty |= review_pane.drain_output();
        }
        self.dirty |= self.reload_theme_if_needed()?;
        self.reload_config_if_needed();
        self.dirty |= self.forward_ui_commands()?;
        self.dirty |= self.poll_acp_tui_bridges()?;
        self.dirty |= self.flush_pending_agent_write()?;
        self.prune_exited_agent_panes();
        Ok(self.editor_has_exited())
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
        for slot in self.panes.iter_mut() {
            slot.pane.apply_theme(&self.terminal_config.theme);
            slot.pane
                .notify_theme_changed(&self.terminal_config.theme)?;
        }
        if let Some(review_pane) = &mut self.review_pane {
            review_pane.apply_theme(&self.terminal_config.theme);
            review_pane.write_all(&self.terminal_config.theme.color_scheme_notification())?;
        }
        self.broadcast_acp_tui_appearance();
        self.force_redraw = true;
        info!("terminal theme changed; reloaded Ghostty colors");
        Ok(true)
    }

    fn reload_config_if_needed(&mut self) {
        let now = Instant::now();
        if now < self.next_config_poll_at {
            return;
        }
        self.next_config_poll_at = now + CONFIG_POLL_INTERVAL;

        if let Some(user) = self.config_watcher.poll() {
            self.apply_polled_user_config(user);
            return;
        }
        if let Some(user) = self.pending_config_reload.clone() {
            self.deliver_config_reload(user);
        }
    }

    fn apply_polled_user_config(&mut self, user: UserConfig) {
        let orchestration_enabled = user.orchestration_enabled();
        let unchanged =
            user == self.app.user && orchestration_enabled == self.app.launch.orchestration_enabled;
        if unchanged {
            if self.pending_config_reload.is_some() {
                self.deliver_config_reload(user);
            }
            return;
        }

        if self.app.user.nvim_command != user.nvim_command {
            info!(
                command = %user.nvim_command,
                "nvim command changed in via.conf; restart via to launch the editor with it"
            );
        }
        if self.app.user.agent_command != user.agent_command {
            info!(
                command = user.agent_command.as_deref().unwrap_or("(unset)"),
                "agent command changed in via.conf; restart via to launch the primary pane with it"
            );
        }
        if self.app.user.scroll_sensitivity != user.scroll_sensitivity {
            let sensitivity = user.scroll_sensitivity;
            for slot in self.panes.iter_mut() {
                if let Some(pane) = slot.pane.as_terminal_mut() {
                    pane.set_scroll_sensitivity(sensitivity);
                }
            }
            if let Some(review_pane) = &mut self.review_pane {
                review_pane.set_scroll_sensitivity(sensitivity);
            }
        }

        self.app.user = user.clone();
        self.app.launch.orchestration_enabled = orchestration_enabled;
        self.relayout();
        self.deliver_config_reload(user);
    }

    fn deliver_config_reload(&mut self, user: UserConfig) {
        let event = Event::Ui(UiEvent::ConfigReloaded {
            user: Box::new(user.clone()),
        });
        if self.events.try_send(event) {
            self.pending_config_reload = None;
            info!("reloaded via.conf");
            return;
        }
        if self.pending_config_reload.is_none() {
            warn!("config reload is waiting for the mediator");
        }
        self.pending_config_reload = Some(user);
    }

    /// Tell the ACP TUIs whether a tab strip now names them, once per change.
    ///
    /// Without a strip the pane header repeats the agent id; with one it would only
    /// duplicate the tab label.
    fn sync_acp_tui_strip_state(&mut self) {
        let visible = self.tab_strip_enabled();
        if visible == self.acp_tui_strip_visible {
            return;
        }
        self.acp_tui_strip_visible = visible;
        let agent_ids: Vec<String> = self.acp_tui_bridges.keys().cloned().collect();
        for agent_id in agent_ids {
            let _ = self.send_acp_tui(&agent_id, HostToTui::TabStripVisible { visible });
        }
    }

    fn broadcast_acp_tui_appearance(&mut self) {
        let light = self.terminal_config.theme.is_light();
        let msg = HostToTui::Appearance { light };
        let agent_ids: Vec<String> = self.acp_tui_bridges.keys().cloned().collect();
        for agent_id in agent_ids {
            let _ = self.send_acp_tui(&agent_id, msg.clone());
        }
    }

    /// Focus `id` (no-op when it is already active or unknown).
    /// Focus `id`. In tabs mode the new tab also has to be laid out: the previous
    /// tab's rect is the only agent rect in the frame, so without a relayout the
    /// newly active pane would have no placement and the strip would vanish.
    ///
    /// A tab switch repaints only the agent region: `relayout` raises `force_redraw`
    /// because it cannot know why it was called, so when the geometry turns out to be
    /// unchanged the full redraw is downgraded here. Otherwise the editor - which a
    /// tab switch never touches - would be repainted on every click.
    fn set_active_pane(&mut self, id: PaneId) {
        if self.panes.active() == Some(id) {
            return;
        }
        // Decided before focus moves: `tab_activation_needs_relayout` would otherwise
        // compare `id` against itself and never report a switch.
        let needs_relayout = tab_activation_needs_relayout(&self.agent_area, id);
        // `move_focus_to` returns the pane that had focus first, so the chrome rule
        // cannot be evaluated against the post-move state by mistake. Fusing the two
        // is deliberate: a pure test cannot see caller ordering, so the ordering is
        // made unrepresentable instead.
        let focus_before = self.move_focus_to(id);
        self.dirty = true;

        if needs_relayout {
            self.tab_switch_from = self.relayout_for_tab_switch(focus_before);
        } else {
            self.force_redraw = true;
        }
    }

    /// Move focus to `id` and report the pane that had it before.
    ///
    /// The tab model and the unseen marker move with focus, exactly as
    /// [`Self::set_active_pane`] did inline. Returning the previous focus is what lets
    /// [`Self::relayout_for_tab_switch`] tell whether the editor's chrome changed,
    /// without any caller having to remember to capture it first.
    fn move_focus_to(&mut self, id: PaneId) -> Option<PaneId> {
        let before = self.panes.active();
        self.panes.set_active(id);
        self.note_focus(id);
        self.sync_active_pane_from_store();
        before
    }

    /// Relayout after a tab switch, returning the pre-switch frame when the repaint
    /// can stay regional.
    ///
    /// `focus_before` is the pane that had focus *before* the switch. Both callers must
    /// capture it before they move focus (`set_active_pane` and `adopt_indexed_focus`
    /// both do), because the chrome rule needs to know whether focus entered or left
    /// the editor.
    ///
    /// `relayout` raises `force_redraw` because it cannot know why it was called. The
    /// decision about how much to repaint is [`tab_switch_repaint`]'s, the same
    /// function `render` asks, so the flag set here and the region drawn there can
    /// never disagree. `Some(before)` means "only the agent region changed" and is
    /// handed to `render` for the next frame; `None` means the whole window must be
    /// repainted.
    ///
    /// Two things force the whole window even when no rect moved:
    ///
    /// - the editor's focus chrome changed. Its border and dim tint live outside the
    ///   agent region, so a regional repaint would leave the editor looking focused
    ///   while the agent is drawn focused too.
    /// - a full redraw was already pending. A resize or font change sets `force_redraw`
    ///   and waits for the next `RedrawRequested`, and a strip click landing first must
    ///   not downgrade that - it would clear the buffer and leave the editor unpainted.
    ///
    /// On a Full fallback the flag stays set; only a real Region clears it.
    fn relayout_for_tab_switch(&mut self, focus_before: Option<PaneId>) -> Option<FrameLayout> {
        let pending = self.force_redraw;
        let before = self.layout.clone();
        self.relayout();
        let (editor, _) = self.editor_and_agent_ids();
        let chrome_changes = editor_chrome_changes(
            self.show_pane_focus_chrome(),
            focus_before,
            self.panes.active(),
            editor,
        );
        let decision = tab_switch_repaint(
            &before,
            &self.layout,
            self.agent_region_rect(),
            chrome_changes,
        );
        self.force_redraw = force_redraw_after_tab_switch(pending, decision);
        match decision {
            TabSwitchRepaint::Region(_) => Some(before),
            TabSwitchRepaint::Full => None,
        }
    }
    /// Side effects shared by every path that changes which pane has focus.
    ///
    /// Both the mouse path and the Alt-key path route through here, so the tab
    /// model and the unseen marker cannot drift apart. See [`note_focus`].
    fn note_focus(&mut self, id: PaneId) {
        note_focus(&mut self.panes, &mut self.agent_area, id);
    }

    /// Mirror the store's active pane into the layout state's positional index.
    ///
    /// [`PaneStore`] owns focus by identity; the layout layer addresses panes by
    /// index because the Alt-key shortcuts are index-based. This is the single
    /// place the identity becomes an index again.
    fn sync_active_pane_from_store(&mut self) {
        if let Some(index) = self.panes.active_index() {
            self.layout_state.active_pane = index;
        }
    }

    /// Fold a positional focus change from the layout layer back into the store.
    ///
    /// Called after shortcut handling, which only knows indices. This must mirror
    /// [`Self::set_active_pane`] exactly - the Alt-key path is a second entry point
    /// to the same focus change, and skipping the tab model here would leave the
    /// newly focused pane stuck with `unseen_output` and `AgentArea::active` stale.
    fn adopt_indexed_focus(&mut self) {
        let Some(id) = self.panes.id_at(self.layout_state.active_pane) else {
            return;
        };
        self.panes.set_active(id);
        self.note_focus(id);
    }

    /// Remove the slot at `index`, keeping focus on a valid pane.
    fn remove_pane_at(&mut self, index: usize) -> Option<PaneSlot> {
        let removed = self.panes.remove_index(index)?;
        // `active_pane` is a slot index, so it has to be shifted; the layout mode no
        // longer carries an index of its own. The store then supplies the active
        // identity, which is index-independent, and the next relayout reconciles the
        // tab model through `sync_agent_area`.
        adjust_pane_indices_after_removal(&mut self.layout_state, index, self.panes.len());
        self.sync_active_pane_from_store();
        self.dirty = true;
        self.force_redraw = true;
        Some(removed)
    }

    fn remove_pane_by_id(&mut self, id: PaneId) -> bool {
        match self.panes.index_of(id) {
            Some(index) => self.remove_pane_at(index).is_some(),
            None => false,
        }
    }

    fn show_pane_focus_chrome(&self) -> bool {
        matches!(self.layout_state.mode, PaneLayoutMode::Split) && self.panes.len() > 1
    }

    fn first_agent_terminal_mut(&mut self) -> Option<&mut TerminalPaneController> {
        // Prefer the primary PTY agent when multiple agent panes exist.
        if let Some(i) = self
            .panes
            .iter()
            .position(|slot| slot.pane.agent_id_matches(PRIMARY_PTY_AGENT_ID))
        {
            return self.panes.slots[i].pane.as_terminal_mut();
        }
        self.panes
            .iter_mut()
            .find(|slot| slot.pane.is_agent_terminal())
            .and_then(|slot| slot.pane.as_terminal_mut())
    }

    /// Whether the tab strip reserves geometry.
    ///
    /// The strip owns a row of the agent region whenever tabs are on, whether or
    /// not it is currently drawn. Reserving it unconditionally is what keeps the
    /// terminal grid stable across a review session.
    fn tab_strip_enabled(&self) -> bool {
        tab_strip_visible(self.agent_area.mode(), self.agent_area.order().len())
    }

    /// The strip rect and the rect left to `pane_rect` below it.
    ///
    /// Returns `None` when the strip is off or that pane is not the one hosting it.
    /// Both `resize_panes` and `render` go through here, so the terminal grid and the
    /// drawn pixels agree on how much room the pane actually has.
    fn strip_split(&self, id: PaneId) -> Option<(PaneRect, PaneRect)> {
        if !self.tab_strip_enabled() {
            return None;
        }
        // The strip rides on the active tab, which in Tabs mode is the only agent
        // with a rect.
        if self.agent_area.active() != Some(id) {
            return None;
        }
        self.layout
            .strip_rect_for(id, self.terminal_config.metrics.cell_height)
    }

    /// The rect covered by every agent pane in the current frame.
    ///
    /// Union of the agents' frame rects, so it is exactly the region a tab switch
    /// can change. `None` when no agent is placed: there is then nothing to confine
    /// a repaint to, and the caller must repaint the window.
    fn agent_region_rect(&self) -> Option<PaneRect> {
        agent_region(&self.layout, |id| {
            self.panes.get(id).is_some_and(AppPane::is_agent_pane)
        })
    }

    /// The pane under `(x, y)` and the point in that pane's own coordinates.
    ///
    /// Local coordinates are measured from the rect the pane is *drawn* in, which is
    /// below the tab strip for the strip's host pane. Measuring from the frame rect
    /// instead puts every click one strip-height too low, so a ctrl+click on a link
    /// only lands when aimed above the text. A point inside the strip row belongs to
    /// no pane.
    fn pane_local_hit(&self, x: usize, y: usize) -> Option<(PaneId, usize, usize)> {
        let (id, frame_rect) = self.layout.pane_at(x, y)?;
        let drawn = self.effective_pane_rect(id, frame_rect);
        let (local_x, local_y) = pane_local_point(drawn, x, y)?;
        Some((id, local_x, local_y))
    }

    /// The rect a pane is actually drawn in / sized to.
    ///
    /// Identical to its frame rect except for the tab strip anchor, which gives up
    /// its top row to the strip.
    fn effective_pane_rect(&self, id: PaneId, rect: PaneRect) -> PaneRect {
        match self.strip_split(id) {
            Some((_, below)) => below,
            None => rect,
        }
    }

    /// Build the strip entries from the current [`AgentArea`] state.
    ///
    /// The editor is skipped by construction: `order` only ever holds agents.
    fn tab_strip_entries(&self, now: Instant) -> Vec<TabStripEntry> {
        let active = self.agent_area.active();
        self.agent_area
            .order()
            .iter()
            .filter_map(|id| {
                let activity = self.panes.activity(*id)?;
                Some(TabStripEntry {
                    id: *id,
                    label: self
                        .panes
                        .get(*id)
                        .and_then(AppPane::agent_label)
                        .unwrap_or_else(|| id.raw().to_string()),
                    active: Some(*id) == active,
                    busy: activity.is_busy(now),
                    exited: activity.exited,
                    unseen: activity.unseen_output,
                })
            })
            .collect()
    }

    /// The strip rect plus its entries, or `None` when the strip is not shown.
    fn tab_strip_overlay(&self) -> Option<(PaneRect, Vec<TabStripEntry>)> {
        let active = self.agent_area.active()?;
        let (strip_rect, _) = self.strip_split(active)?;
        Some((strip_rect, self.tab_strip_entries(Instant::now())))
    }

    /// Whether the strip cells on screen differ from what would be drawn now.
    ///
    /// Cheap enough to call from the idle poll, and the entries it compares are the
    /// same ones `render` would draw, resolved the same way: the gate must match
    /// `render`'s draw condition exactly. `tab_strip_enabled` is not that condition.
    /// It says whether the strip reserves geometry, which stays true when the active
    /// agent has no rect and nothing is drawn. Gating on it would compare `None`
    /// against non-empty entries forever and spin the idle poll.
    ///
    /// The unshown case is passed through to [`tab_strip_screen_changed`] rather than
    /// short-circuited here: that branch is where the spin lives, so it has to be the
    /// branch the idle poll actually runs.
    fn tab_strip_entries_changed(&self) -> bool {
        if self.review_active {
            return false;
        }
        let shown = self.tab_strip_overlay().map(|(_, entries)| entries);
        tab_strip_screen_changed(shown.as_deref(), self.last_drawn_strip_entries.as_deref())
    }

    /// Blit the strip buffer into `rect`.
    ///
    /// `changed` says whether the strip's own contents need repainting: true on a
    /// full redraw or a tab switch, false when nothing about the strip moved. The
    /// ACP modal passes the same kind of flag; without it every frame would present,
    /// because a Ratatui blit always reports that it drew.
    fn draw_tab_strip(&mut self, rect: PaneRect, entries: &[TabStripEntry], changed: bool) -> bool {
        let metrics = self.terminal_config.metrics;
        let cols = self.strip_columns(rect);
        if cols == 0 {
            return false;
        }
        let strip = tab_strip_buffer(entries, cols, self.terminal_config.theme.background);
        draw_ratatui_buffer(
            &strip,
            &mut self.font_renderer,
            &mut self.buffer,
            self.width,
            self.height,
            rect.x,
            rect.y,
            metrics,
            self.terminal_config.theme.foreground,
            self.terminal_config.theme.background,
            changed,
            changed,
            &mut self.damage,
        )
    }

    /// How many whole cells fit in the strip rect.
    ///
    /// Drawing and hit testing must use the same width: it decides which tabs fit,
    /// and therefore which tabs can be clicked.
    fn strip_columns(&self, rect: PaneRect) -> u16 {
        (rect.width / self.terminal_config.metrics.cell_width.max(1)).min(u16::MAX as usize) as u16
    }

    /// Activate the tab under `(x, y)` if the strip is showing there.
    ///
    /// Returns whether the click was consumed, so the caller can skip the pane hit
    /// test entirely. There is no close button: a hit only activates.
    fn handle_tab_strip_click(&mut self, x: usize, y: usize) -> bool {
        let Some((strip_rect, entries)) = self.tab_strip_overlay() else {
            return false;
        };
        if y < strip_rect.y || y >= strip_rect.y.saturating_add(strip_rect.height) {
            return false;
        }
        if x < strip_rect.x || x >= strip_rect.x.saturating_add(strip_rect.width) {
            return false;
        }

        let col = (x - strip_rect.x) / self.terminal_config.metrics.cell_width.max(1);
        let col = col.min(u16::MAX as usize) as u16;
        // Same width as the blit above, so a tab that was dropped for not fitting
        // has no hit box either: clicking the empty tail must not activate it.
        let cols = self.strip_columns(strip_rect);
        let Some((id, _, _)) = tab_strip_hit_boxes(&entries, cols)
            .into_iter()
            .find(|(_, start, end)| col >= *start && col < *end)
        else {
            // Clicked the strip row but not on a tab: consumed anyway, so the click
            // does not fall through to the pane underneath.
            return true;
        };
        self.set_active_pane(id);
        true
    }

    fn render(&mut self) -> Result<()> {
        let render_started_at = Instant::now();
        if self.width == 0 || self.height == 0 {
            return Ok(());
        }

        if self.window.is_none() {
            return Ok(());
        }

        let tab_switch_from = self.tab_switch_from.take();
        let force_full = self.force_redraw;
        if force_full {
            self.buffer.fill(self.terminal_config.theme.background);
        }
        self.damage.clear();
        let mut redrawn = force_full;
        let redraw_chrome = self.show_pane_focus_chrome() && (self.dirty || force_full);

        // The strip is part of the agent region, so it is resolved before drawing:
        // the anchor pane gives up its top row to it. Nothing here mutates the
        // layout, so this is stable frame to frame.
        let strip = if self.review_active {
            None
        } else {
            self.tab_strip_overlay()
        };
        // When the strip is not drawn its cells are gone: review paints over the
        // whole window, and with tabs off the row belongs to the pane. Forgetting the
        // baseline keeps the strip from coming back over repainted pixels and deciding
        // it has nothing to do.
        if strip.is_none() {
            self.last_drawn_strip_entries = None;
        }

        // How much to repaint is the same decision `relayout_for_tab_switch` made
        // when it cleared `force_redraw`, asked again here through one function so
        // the flag and the drawn region cannot disagree. A regional switch repaints
        // the agent region and the strip; the editor's pixels are unchanged, so it
        // is skipped. A full redraw always wins over a recorded regional switch.
        let repaint_region = tab_switch_region(
            force_full,
            tab_switch_from.as_ref(),
            &self.layout,
            self.agent_region_rect(),
        );
        let tab_switch = repaint_region.is_some();
        // Panes inside the region still need a forced paint: the pane that was hidden
        // behind the old tab is clean and would otherwise repaint nothing.
        let pane_force = pane_force_redraw(force_full, repaint_region);

        if self.review_active {
            if let Some(review_pane) = &mut self.review_pane {
                redrawn |= review_pane.draw(
                    &mut self.font_renderer,
                    &mut self.buffer,
                    self.width,
                    self.height,
                    full_window_rect(self.width, self.height),
                    true,
                    force_full,
                    false,
                    &mut self.damage,
                );
            } else {
                self.review_active = false;
            }
        } else {
            let placed = self.layout.placed().to_vec();
            let active = self.panes.active();
            for placed in placed {
                let rect = self.effective_pane_rect(placed.id, placed.rect);
                // Skip panes the repaint region does not cover, so a tab switch
                // never redraws the editor.
                if let Some(region) = repaint_region {
                    if !rects_intersect(rect, region) {
                        continue;
                    }
                }
                let Some(slot) = self.panes.get_mut(placed.id) else {
                    continue;
                };
                redrawn |= slot.draw(
                    &mut self.font_renderer,
                    &mut self.buffer,
                    self.width,
                    self.height,
                    rect,
                    Some(placed.id) == active,
                    pane_force,
                    redraw_chrome,
                    &mut self.damage,
                );
            }
            if let Some((strip_rect, entries)) = strip {
                let changed = tab_strip_changed(
                    force_full,
                    tab_switch,
                    &entries,
                    self.last_drawn_strip_entries.as_deref(),
                );
                redrawn |= self.draw_tab_strip(strip_rect, &entries, changed);
                // Record what is now on screen, including on a frame that skipped the
                // blit: the cells are unchanged either way, and this is the baseline
                // the next comparison is made against.
                self.last_drawn_strip_entries = Some(entries);
            }
        }

        if let Some(modal) = self.acp_modal.active() {
            let m = self.terminal_config.metrics;
            let cols = (self.width / m.cell_width).min(u16::MAX as usize) as u16;
            let rows = (self.height / m.cell_height).min(u16::MAX as usize) as u16;
            if cols > 0 && rows > 0 {
                let mb = render_acp_modal_buffer(
                    modal,
                    cols,
                    rows,
                    self.terminal_config.theme.background,
                    self.acp_modal.pending_count(),
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
                    force_full,
                    self.dirty,
                    &mut self.damage,
                );
            }
        }

        if !redrawn {
            self.dirty = false;
            return Ok(());
        }

        // The surface is borrowed only for the present step. Taking it earlier
        // would hold a mutable borrow of `self` across the drawing code above.
        let Some(window) = self.window.as_ref() else {
            return Ok(());
        };
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
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
                UiCommand::VisualSelectionChanged {
                    path,
                    start_line,
                    end_line,
                } => {
                    let update = format_selection_update(
                        &path,
                        start_line,
                        end_line,
                        &self.app.launch.working_directory,
                    );
                    let Some(agent_pane) = self.first_agent_terminal_mut() else {
                        continue;
                    };

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
                    let id = if let Some(ref want) = target_agent_id {
                        self.panes
                            .iter()
                            .find(|slot| slot.pane.agent_id_matches(want))
                            .map(|slot| slot.id)
                    } else {
                        None
                    };
                    // Default to the primary PTY agent
                    let id = id
                        .or_else(|| {
                            self.panes
                                .iter()
                                .find(|slot| slot.pane.agent_id_matches(PRIMARY_PTY_AGENT_ID))
                                .map(|slot| slot.id)
                        })
                        .or_else(|| {
                            self.panes
                                .iter()
                                .find(|slot| slot.pane.is_agent_pane())
                                .map(|slot| slot.id)
                        });

                    if let Some(id) = id {
                        if focus_agent {
                            // Agent-fullscreen is now the agent region's Tabs mode,
                            // not a positional layout mode.
                            if self.layout_state.mode == PaneLayoutMode::EditorFull
                                && self.agent_area.mode() == AgentAreaMode::Tabs
                            {
                                self.layout_state.mode = PaneLayoutMode::Split;
                            }
                            self.set_active_pane(id);
                        }
                        debug!(target = ?target_agent_id, pane = ?id, "forwarding input to agent pane");
                        if let Some(agent_pane) =
                            self.panes.get_mut(id).and_then(AppPane::as_terminal_mut)
                        {
                            if agent_pane.is_acp_tui() {
                                let agent_id = agent_pane.agent_id().unwrap_or("agent").to_string();
                                self.send_acp_tui(
                                    &agent_id,
                                    HostToTui::Transcript {
                                        kind: TranscriptKind::System,
                                        text: payload,
                                    },
                                );
                                self.dirty = true;
                            } else {
                                agent_pane.write_all(payload.as_bytes())?;
                            }
                        }
                    } else {
                        warn!("no agent pane available to receive input");
                    }
                    self.pending_agent_write = None;
                }
                UiCommand::AcpTranscriptChunk {
                    agent_id,
                    kind,
                    text,
                } => {
                    if self.send_acp_tui(
                        &agent_id,
                        HostToTui::Transcript {
                            kind: TranscriptKind::from_acp(&kind),
                            text: text.clone(),
                        },
                    ) {
                        debug!(agent_id, kind, "forwarding ACP transcript chunk to TUI");
                    } else {
                        warn!(
                            agent_id,
                            kind, "no ACP TUI bridge for transcript chunk; dropping"
                        );
                    }
                }
                UiCommand::AcpProgress {
                    agent_id,
                    id,
                    label,
                    active,
                } => {
                    // Keep host-side turn state in addition to forwarding, so the
                    // UI can show a busy indicator for this agent. Forwarding below
                    // is unchanged.
                    self.panes.apply_acp_progress(&agent_id, &id, active);
                    if self.send_acp_tui(
                        &agent_id,
                        HostToTui::Progress {
                            id: id.clone(),
                            label: label.clone(),
                            active,
                        },
                    ) {
                        debug!(
                            agent_id,
                            id, label, active, "forwarding ACP progress to TUI"
                        );
                    } else {
                        warn!(agent_id, id, "no ACP TUI bridge for progress; dropping");
                    }
                }
                UiCommand::AcpSessionStatus {
                    agent_id,
                    model,
                    models,
                    provider_error,
                    clear_provider_error,
                } => {
                    if self.send_acp_tui(
                        &agent_id,
                        HostToTui::SessionStatus {
                            model: model.clone(),
                            models: models.as_ref().map(|choices| {
                                choices
                                    .iter()
                                    .map(|c| crate::acp_tui::ModelChoice {
                                        value: c.value.clone(),
                                        name: c.name.clone(),
                                    })
                                    .collect()
                            }),
                            provider_error: provider_error.clone(),
                            clear_provider_error,
                        },
                    ) {
                        debug!(
                            agent_id,
                            ?model,
                            ?provider_error,
                            clear_provider_error,
                            "forwarding ACP session status to TUI"
                        );
                    } else {
                        warn!(agent_id, "no ACP TUI bridge for session status; dropping");
                    }
                }
                UiCommand::AcpModalPrompt {
                    agent_id,
                    jsonrpc_id,
                    title,
                    message,
                    options,
                    kind,
                } => {
                    self.acp_modal.push(AcpModalState::new(
                        agent_id, jsonrpc_id, title, message, options, kind,
                    ));
                }
                UiCommand::SpawnAgent { id, role, command } => {
                    info!(%id, role = ?role, command = ?command, "creating new agent pane");
                    if let Err(e) = self.create_agent_pane(&id, role.as_deref(), command.as_deref())
                    {
                        error!(%e, "failed to create agent pane");
                    }
                }
                UiCommand::TerminateAgent { id } => {
                    if let Err(e) = self.terminate_agent_pane(&id) {
                        error!(%e, "failed to terminate agent pane");
                    }
                }
                UiCommand::ReviewGateOpened { task_id, title } => {
                    info!(%task_id, %title, "review gate surfaced to UI");
                    if self.app.user.review_backend == ReviewBackend::Hunk && !self.review_active {
                        if let Err(e) = self.toggle_hunk_review() {
                            error!(%e, "failed to open hunk review");
                        } else {
                            changed = true;
                            self.dirty = true;
                            self.force_redraw = true;
                        }
                    }
                }
                UiCommand::FileIndexChanged {
                    buffers,
                    vcs_working_tree,
                    vcs_branch,
                } => {
                    self.file_index
                        .set_files(buffers, vcs_working_tree, vcs_branch);
                    if self.modifiers.ctrl {
                        self.handle_modifiers_changed();
                    }
                }
                UiCommand::SymbolIndexChanged { symbols } => {
                    self.file_index.set_symbols(symbols);
                    if self.modifiers.ctrl {
                        self.handle_modifiers_changed();
                    }
                }
                UiCommand::SymbolsDiscovered { symbols } => {
                    self.file_index.merge_cached_symbols(symbols);
                    if self.modifiers.ctrl {
                        self.handle_modifiers_changed();
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

        let pending = self
            .pending_agent_write
            .take()
            .expect("pending write exists");

        let Some(agent_pane) = self.first_agent_terminal_mut() else {
            return Ok(false);
        };
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

    fn handle_drain_result(
        &mut self,
        event_loop: &ActiveEventLoop,
        result: Result<bool>,
        on_continue: impl FnOnce(&mut Self, &ActiveEventLoop),
    ) {
        match result {
            Err(error) => self.fail(event_loop, error),
            Ok(true) => {
                info!("nvim exited; shutting down via");
                self.request_quit(event_loop);
            }
            Ok(false) => on_continue(self, event_loop),
        }
    }
}

fn open_url_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "rundll32.exe";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new(program);
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    };

    #[cfg(not(target_os = "windows"))]
    let mut command = {
        let mut command = std::process::Command::new(program);
        command.arg(url);
        command
    };

    if let Err(error) = command.spawn() {
        tracing::warn!(%error, %url, "failed to open url in system browser");
    }
}

impl ApplicationHandler<UserEvent> for WinitGhosttyApp {
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::PtyOutput => {
                let result = self.drain_background_work();
                self.handle_drain_result(event_loop, result, |this, event_loop| {
                    if this.dirty {
                        this.request_redraw();
                        this.set_wait_for_next_redraw(event_loop);
                    }
                });
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
                self.request_quit(event_loop);
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
                self.handle_modifiers_changed();
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
            if self.dirty {
                self.request_redraw();
                self.set_wait_for_next_redraw(event_loop);
            }
            return;
        }

        let result = self.drain_background_work();
        self.handle_drain_result(event_loop, result, |this, event_loop| {
            if this.dirty {
                this.request_redraw();
                this.set_wait_for_next_redraw(event_loop);
            }
        });
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let result = self.drain_background_work();
        self.handle_drain_result(event_loop, result, |this, event_loop| {
            // The strip is the one piece of chrome whose content can change with no
            // event at all: the busy dot expires when the output window elapses, and
            // nothing fires for that. This poll is already the idle tick, so compare
            // the recomputed cells against what is on screen and wake a render if
            // they differ. Without it a busy -> idle flip waits for the next full
            // redraw.
            if !this.dirty && this.tab_strip_entries_changed() {
                this.dirty = true;
            }
            if this.dirty {
                this.request_redraw();
                this.set_wait_for_next_redraw(event_loop);
            } else {
                event_loop.set_control_flow(ControlFlow::wait_duration(Duration::from_millis(50)));
            }
        });
    }
}

impl WinitGhosttyApp {
    fn handle_modifiers_changed(&mut self) {
        let mut dirty = false;
        let mut force_redraw = false;
        let ctx = ReferenceContext::new(&self.app.launch.working_directory, Some(&self.file_index));

        if self.review_active {
            if let Some(review_pane) = &mut self.review_pane {
                let outcome = review_pane.handle_modifiers_changed(self.modifiers, ctx);
                self.apply_pane_outcome(outcome);
            } else {
                self.review_active = false;
            }
            return;
        }

        for slot in self.panes.iter_mut() {
            let outcome = slot.pane.handle_modifiers_changed(self.modifiers, ctx);
            dirty |= outcome.dirty;
            force_redraw |= outcome.force_redraw;
        }
        self.dirty |= dirty;
        self.force_redraw |= force_redraw;
    }
}

/// Smallest rect covering both inputs.
fn union_rect(a: PaneRect, b: PaneRect) -> PaneRect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = a.x.saturating_add(a.width).max(b.x.saturating_add(b.width));
    let bottom =
        a.y.saturating_add(a.height)
            .max(b.y.saturating_add(b.height));
    PaneRect {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// Whether two rects overlap. Touching edges do not count.
fn rects_intersect(a: PaneRect, b: PaneRect) -> bool {
    a.x < b.x.saturating_add(b.width)
        && b.x < a.x.saturating_add(a.width)
        && a.y < b.y.saturating_add(b.height)
        && b.y < a.y.saturating_add(a.height)
}

/// Whether two frames occupy the same rects, ignoring which pane sits in each.
///
/// A tab switch in tabs mode swaps *which* agent occupies the agent region, so the
/// rects match while the occupant changes. When they do, only the agent region needs
/// repainting rather than the whole window - the editor in particular is untouched.
fn frames_share_geometry(a: &FrameLayout, b: &FrameLayout) -> bool {
    a.placed().len() == b.placed().len()
        && a.placed()
            .iter()
            .zip(b.placed())
            .all(|(left, right)| left.rect == right.rect)
}

/// The rect covered by every placed pane that `is_agent` accepts.
///
/// `None` when no such pane has geometry, which is what "there is nothing to
/// confine a repaint to" looks like.
fn agent_region(layout: &FrameLayout, is_agent: impl Fn(PaneId) -> bool) -> Option<PaneRect> {
    let mut bounds: Option<PaneRect> = None;
    for placed in layout.placed() {
        if !is_agent(placed.id) {
            continue;
        }
        bounds = Some(match bounds {
            Some(rect) => union_rect(rect, placed.rect),
            None => placed.rect,
        });
    }
    bounds
}

/// What a tab switch has to repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabSwitchRepaint {
    /// Only this region changed. Everything outside it - the editor in particular -
    /// still holds correct pixels.
    Region(PaneRect),
    /// The frame moved, so the whole window has to be repainted.
    Full,
}

/// Whether the editor's focus chrome changes as focus moves.
///
/// `draw_pane_focus_chrome` paints an active border on the focused pane and a dim tint
/// on the others, and that chrome is only shown in `Split` with 2+ panes. So when
/// focus moves to or from the editor in that configuration, the editor's own pixels
/// change - one pane's border stops being active and the other's starts. A repaint
/// confined to the agent region would leave the editor looking focused while the agent
/// is drawn focused too.
///
/// Agent-to-agent switches are unaffected: the editor is inactive before and after.
/// Both callers must capture the focused pane *before* they move focus.
fn editor_chrome_changes(
    chrome_shown: bool,
    before_focus: Option<PaneId>,
    after_focus: Option<PaneId>,
    editor: Option<PaneId>,
) -> bool {
    let Some(editor) = editor.filter(|_| chrome_shown) else {
        return false;
    };
    (before_focus == Some(editor)) != (after_focus == Some(editor))
}

/// Decide how much of the window a tab switch repaints.
///
/// This exists because two call sites need the same answer and used to compute it
/// separately: `relayout` always raises `force_redraw` (it cannot know why it ran),
/// so a switch that only swaps the occupant has to downgrade that flag, and `render`
/// has to keep the repaint inside the agent region. One function, called by both,
/// is what stops the two from drifting apart.
///
/// `agent_region` is the union of the agent rects in `after`; `None` means no agent
/// is placed and there is nothing to confine the repaint to. `focus_chrome_changes`
/// carries the one reason an unchanged frame still needs the whole window: the
/// editor's border and tint live outside the agent region.
fn tab_switch_repaint(
    before: &FrameLayout,
    after: &FrameLayout,
    agent_region: Option<PaneRect>,
    focus_chrome_changes: bool,
) -> TabSwitchRepaint {
    match agent_region {
        // Same rects, different occupant, and nothing about the editor's chrome moved:
        // the editor's pixels are still correct.
        Some(region) if !focus_chrome_changes && frames_share_geometry(before, after) => {
            TabSwitchRepaint::Region(region)
        }
        // The geometry moved, the editor's focus chrome changed, or there is no agent
        // region to stay inside.
        _ => TabSwitchRepaint::Full,
    }
}

/// The repaint region `render` should confine this frame to, if any.
///
/// Composes the two things `render` needs: the [`tab_switch_repaint`] decision, and
/// the rule that a full redraw always wins over a recorded regional switch. `None`
/// means "paint the whole window":
///
/// - `force_full` is set. The buffer has just been cleared, so confining the repaint
///   would leave everything outside the region blank. This is also what protects a
///   regional switch that a resize or font change overtook before it was rendered.
/// - no pre-switch frame was recorded, so this is not a tab-switch frame at all.
/// - the decision itself is [`TabSwitchRepaint::Full`] (geometry moved, or no agent).
///
/// `Some(region)` means exactly one regional tab switch, with `force_full` clear.
///
/// The editor-chrome rule is not repeated here: only
/// [`WinitGhosttyApp::relayout_for_tab_switch`] knows which pane had focus before the
/// switch, and it records a pre-switch frame only when chrome did not change. So a
/// recorded frame already implies "chrome unchanged", and re-deriving without that
/// fact cannot disagree.
fn tab_switch_region(
    force_full: bool,
    before: Option<&FrameLayout>,
    after: &FrameLayout,
    agent_region: Option<PaneRect>,
) -> Option<PaneRect> {
    if force_full {
        return None;
    }
    match tab_switch_repaint(before?, after, agent_region, false) {
        TabSwitchRepaint::Region(region) => Some(region),
        TabSwitchRepaint::Full => None,
    }
}

/// Whether a pane's terminal must repaint every row this frame.
///
/// A regional repaint skips the panes outside the region, but the panes inside it
/// still need a forced paint. `draw_screen` returns early on `Dirty::Clean`, and a
/// terminal that was hidden behind a tab keeps its render state - so on A -> B -> A
/// the returning pane is clean, resizes to the same size, and would paint nothing,
/// leaving B's pixels under A's strip. The first visit of a never-drawn tab only
/// looks right because it starts dirty.
///
/// So: a full redraw forces every pane, and a regional repaint forces the panes it
/// does draw. A plain incremental frame (no region) lets dirty tracking decide.
fn pane_force_redraw(force_full: bool, repaint_region: Option<PaneRect>) -> bool {
    force_full || repaint_region.is_some()
}

/// Whether the strip's cells need repainting, given what was last drawn.
///
/// Keeps the early return in `draw_ratatui_buffer` (a Ratatui blit always claims it
/// drew, so asking at all means presenting a frame) while still updating live: a
/// new busy/unseen/exited dot arrives as *background* output, which only sets
/// `dirty`, and the busy -> idle flip at the end of the output window has no event
/// at all. Both are caught by comparing the recomputed entries with the ones on
/// screen. `last` is `None` before the first draw, which must paint.
fn tab_strip_changed(
    force_full: bool,
    tab_switch: bool,
    entries: &[TabStripEntry],
    last: Option<&[TabStripEntry]>,
) -> bool {
    force_full || tab_switch || tab_strip_screen_changed(Some(entries), last)
}

/// The `force_redraw` flag to leave behind after a tab switch.
///
/// A regional switch wants to clear the flag `relayout` just raised, but only the
/// one *it* raised. A full redraw that was already pending - set by a resize or a
/// font change waiting for the next `RedrawRequested` - must survive: the strip
/// click that triggered this relayout can land before that render, and downgrading
/// it would clear the buffer while leaving the editor unpainted.
///
/// `pending` is the flag as it was before the relayout ran.
fn force_redraw_after_tab_switch(pending: bool, decision: TabSwitchRepaint) -> bool {
    match decision {
        // Keep whatever was already pending; drop only the relayout's own flag.
        TabSwitchRepaint::Region(_) => pending,
        // The switch repaints the window anyway.
        TabSwitchRepaint::Full => true,
    }
}

/// Whether the strip needs a repaint against what is currently on screen.
///
/// `shown` is the entries the strip *would* draw, or `None` when it is not on screen
/// at all. Not being on screen is never a change: the idle poll calls this to decide
/// whether to wake a render, and returning true there means redrawing nothing and
/// asking again, forever. `last` is `None` only when nothing has been drawn yet,
/// which is a change the first time the strip does appear.
fn tab_strip_screen_changed(
    shown: Option<&[TabStripEntry]>,
    last: Option<&[TabStripEntry]>,
) -> bool {
    let Some(shown) = shown else {
        return false;
    };
    last != Some(shown)
}

/// Activate `id` as the tab and report whether the frame must be rebuilt.
///
/// In tabs mode only the active agent has a rect, so switching tabs is a layout
/// change, not just a focus change: without the relayout the newly active pane has
/// no placement, `strip_rect_for` returns `None`, and the strip disappears. Split
/// mode shows every agent, so nothing needs rebuilding there.
fn tab_activation_needs_relayout(area: &AgentArea, id: PaneId) -> bool {
    area.active() != Some(id) && area.mode() == AgentAreaMode::Tabs
}

/// Whether the tab strip is shown.
///
/// Two conditions, both required: tabs mode (in split mode every agent is already
/// visible side by side, so a strip would be redundant) and more than one agent (a
/// lone tab is pure chrome).
///
/// Deliberately independent of the review pane. The strip is not *drawn* while
/// review owns the window, but its row must still be reserved, otherwise a resize
/// during review would hand the agent pane the strip's row and the terminal grid
/// would disagree with the pixels the moment review closes.
fn tab_strip_visible(mode: AgentAreaMode, agent_count: usize) -> bool {
    mode == AgentAreaMode::Tabs && agent_count > 1
}

/// Apply the side effects every focus change must have, on the two models.
///
/// Kept free so it can be tested without standing up a window. The editor is not a
/// tab, so focusing it must neither activate a tab (which would move the strip and
/// show the wrong pane) nor clear an agent's unseen marker (the user is not looking
/// at that agent). Both are asserted in the tests below.
fn note_focus(panes: &mut PaneStore, area: &mut AgentArea, id: PaneId) {
    if !panes.get(id).is_some_and(AppPane::is_agent_pane) {
        return;
    }
    area.activate(id);
    panes.mark_seen(id);
}

/// Reconcile the tab model with the live panes and clean up after a handover.
///
/// When [`AgentArea::sync`] moves `active` onto a neighbor because the previous
/// active tab was terminated, that neighbor becomes visible, so its unseen marker
/// must be cleared. Without this the pane would appear with a stale "new output"
/// dot the moment it is promoted.
///
/// It also keeps keyboard focus and the active tab pointing at the same pane. After a
/// close they can disagree: [`PaneStore::remove_index`] takes the slot that slid into
/// the removed one, while [`AgentArea::sync`] takes the tab that slid into the removed
/// tab's position. With slots `[E, a, b, c]` and order `[b, a, c]`, closing `a` leaves
/// the store on `b` and the tabs on `c`. In tabs mode the visible pane is the active
/// tab, so the store follows it; in split mode every agent is visible and the store's
/// choice stands.
fn sync_agent_area(
    area: &mut AgentArea,
    panes: &mut PaneStore,
    agents: &[PaneId],
    editor: Option<PaneId>,
) {
    let active_before = area.active();
    area.sync(agents, editor);
    if area.active() != active_before {
        if let Some(active) = area.active() {
            panes.mark_seen(active);
        }
    }

    if area.mode() != AgentAreaMode::Tabs {
        return;
    }
    let Some(active_tab) = area.active() else {
        return;
    };
    // Only an agent focus can be out of step with a tab; the editor is not a tab, so
    // focusing it is a deliberate divergence and must be left alone.
    let focused_is_agent = panes
        .active()
        .and_then(|id| panes.get(id))
        .is_some_and(AppPane::is_agent_pane);
    if focused_is_agent && panes.active() != Some(active_tab) {
        panes.set_active(active_tab);
    }
}

/// `(x, y)` relative to the rect a pane is drawn in, or `None` when the point is
/// above or left of it (the tab strip row, for the strip's host pane).
fn pane_local_point(drawn: PaneRect, x: usize, y: usize) -> Option<(usize, usize)> {
    if x < drawn.x || y < drawn.y {
        return None;
    }
    Some((x - drawn.x, y - drawn.y))
}

fn pane_count(app: &AppContext) -> usize {
    if app.user.agent_command.is_some() {
        2
    } else {
        1
    }
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

fn nvim_args(app: &AppContext) -> Vec<OsString> {
    vec![
        OsString::from("--listen"),
        app.paths.nvim_socket_path.clone().into_os_string(),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.via_editor_socket = {}",
            lua_string_literal(&app.paths.editor_socket_path)
        )),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.via_lsp_bridge_socket = {}",
            lua_string_literal(&app.paths.lsp_bridge_socket_path)
        )),
        OsString::from("--cmd"),
        OsString::from(format!(
            "lua vim.g.via_module_path = {}",
            lua_string_literal(&app.paths.nvim_via_module_path)
        )),
        OsString::from("--cmd"),
        {
            // Stable lua/ directory so require('via') works for every via session.
            let dir = crate::config::lua_dir();
            OsString::from(format!(
                "lua package.path = package.path .. ';' .. {} .. '/?.lua'",
                lua_string_literal(&dir)
            ))
        },
        OsString::from("-c"),
        OsString::from(format!(
            "luafile {}",
            vim_fnameescape(&app.paths.nvim_context_bridge_path)
        )),
    ]
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
    use layout::{PaneRect, chord_switches_tab};
    use links::{
        LinkSpan, ReferenceTarget, file_target_from_uri, parse_vt_hyperlinks,
        reference_target_from_row, reference_target_from_uri,
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
    fn emits_color_scheme_notification() {
        let mut dark = TerminalConfig::default();
        dark.apply_entry("background", "#0f0d21");
        assert_eq!(
            dark.theme.color_scheme_notification(),
            b"\x1b[?997;1n",
            "dark background sends DSR 997 ; 1"
        );

        let mut light = TerminalConfig::default();
        light.apply_entry("background", "#eff1f5");
        assert_eq!(
            light.theme.color_scheme_notification(),
            b"\x1b[?997;2n",
            "light background sends DSR 997 ; 2"
        );
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
                agent_share: AgentPaneShare::Auto,
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
            PaneLayoutMode::EditorFull,
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
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        );
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
    }

    #[test]
    fn create_nvim_maximized_layout_hides_every_agent() {
        // Agent-fullscreen was `PaneMaximized(1)`; it is now the agent region's Tabs
        // mode, so this layout mode only ever means "the editor alone".
        let layout = SplitLayout::for_window(
            100,
            50,
            3,
            PaneLayoutMode::EditorFull,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(layout.pane(0).width, 100, "the editor keeps the window");
        for index in 1..3 {
            assert_eq!(layout.pane(index).width, 0, "agent {index} is hidden");
            assert_eq!(layout.pane(index).height, 0, "agent {index} is hidden");
        }
        assert_eq!(layout.pane_at(10, 10).map(|(index, _)| index), Some(0));
    }

    #[test]
    fn editor_full_keeps_hidden_panes_zero_sized() {
        let nvim_layout = SplitLayout::for_window(
            100,
            50,
            2,
            PaneLayoutMode::EditorFull,
            PaneSplitDirection::Vertical,
            SplitLayoutOptions::unbounded(),
        );

        assert_eq!(nvim_layout.pane(1).width, 0);
        assert_eq!(nvim_layout.pane(1).height, 0);
    }

    fn shortcut_modifiers(alt: bool, shift: bool) -> Modifiers {
        Modifiers {
            alt,
            shift,
            ..Modifiers::default()
        }
    }

    /// Slot order the shortcut tests use: editor first, then agents.
    fn shortcut_targets(pane_count: usize) -> PaneTargets {
        PaneTargets {
            slots: (0..pane_count)
                .map(|index| PaneId(index as u64 + 1))
                .collect(),
            editor_index: (pane_count > 0).then_some(0),
        }
    }

    /// A `Split`-mode agent area over `pane_count` slots, matching `shortcut_targets`.
    fn shortcut_area(pane_count: usize) -> AgentArea {
        let slots: Vec<PaneId> = (0..pane_count)
            .map(|index| PaneId(index as u64 + 1))
            .collect();
        let mut area = AgentArea::new();
        area.sync(&slots[1.min(slots.len())..], slots.first().copied());
        area
    }

    fn press_layout_shortcut(
        pressed_keys: &[Key],
        modifiers: Modifiers,
        pane_count: usize,
        layout: &mut PaneLayoutState,
    ) -> bool {
        press_layout_shortcut_in(
            pressed_keys,
            modifiers,
            pane_count,
            layout,
            &mut shortcut_area(pane_count),
        )
    }

    fn press_layout_shortcut_in(
        pressed_keys: &[Key],
        modifiers: Modifiers,
        pane_count: usize,
        layout: &mut PaneLayoutState,
        area: &mut AgentArea,
    ) -> bool {
        handle_layout_shortcuts(
            pressed_keys,
            modifiers,
            false,
            pane_count,
            layout,
            area,
            layout::AgentRegionSpan::unknown(),
            &shortcut_targets(pane_count),
        )
        .is_some()
    }

    fn test_layout_state(
        mode: PaneLayoutMode,
        split_direction: PaneSplitDirection,
        active_pane: usize,
    ) -> PaneLayoutState {
        PaneLayoutState {
            mode,
            split_direction,
            active_pane,
            agent_share: AgentPaneShare::Auto,
        }
    }

    #[test]
    fn maps_alt_number_shortcuts_to_active_panes() {
        let mut layout =
            test_layout_state(PaneLayoutMode::EditorFull, PaneSplitDirection::Vertical, 1);

        assert!(press_layout_shortcut(
            &[Key::Key1],
            shortcut_modifiers(true, false),
            2,
            &mut layout
        ));
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 0);
        assert!(press_layout_shortcut(
            &[Key::Key2],
            shortcut_modifiers(true, false),
            2,
            &mut layout
        ));
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 1);
    }

    #[test]
    fn maps_alt_shift_number_shortcuts_to_editor_full_and_agent_full() {
        let mut layout = test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 1);

        // Alt+Shift+1 is the editor fullscreen, the renamed PaneMaximized(0).
        assert!(press_layout_shortcut(
            &[Key::Key1],
            shortcut_modifiers(true, true),
            2,
            &mut layout
        ));
        assert_eq!(layout.mode, PaneLayoutMode::EditorFull);
        assert_eq!(layout.active_pane, 0);

        // Alt+Shift+2 maximizes the first agent: AgentFull with its tab active.
        let mut area = shortcut_area(2);
        assert!(press_layout_shortcut_in(
            &[Key::Key2],
            shortcut_modifiers(true, true),
            2,
            &mut layout,
            &mut area
        ));
        assert_eq!(area.mode(), AgentAreaMode::Tabs);
        assert_eq!(
            area.active(),
            Some(PaneId(2)),
            "Alt+Shift+2 activates the first tab"
        );
        assert_eq!(
            layout.mode,
            PaneLayoutMode::AgentFull,
            "and fills the window with it"
        );
        assert_eq!(
            layout.active_pane, 1,
            "focus follows the activated tab to its slot"
        );
    }

    #[test]
    fn alt_arrows_are_not_layout_shortcuts() {
        // Alt+Left/Right and Alt+Shift+Left/Right belong to the editor and the agent
        // TUIs (word motion, history), so the layout layer must never claim them.
        for shift in [false, true] {
            for key in [Key::Left, Key::Right] {
                let modifiers = shortcut_modifiers(true, shift);
                assert!(layout::layout_chord(key, modifiers).is_none());

                let mut layout =
                    test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 1);
                assert!(
                    !press_layout_shortcut(&[key], modifiers, 2, &mut layout),
                    "{key:?} shift={shift} must not be consumed"
                );
                assert_eq!(layout.active_pane, 1, "and must not move focus");
                assert_eq!(layout.mode, PaneLayoutMode::Split);
            }
        }
    }

    #[test]
    fn maps_alt_j_shortcut_to_split_direction_toggle() {
        let mut layout = test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 0);

        assert!(press_layout_shortcut(
            &[Key::J],
            shortcut_modifiers(true, false),
            2,
            &mut layout
        ));
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.split_direction, PaneSplitDirection::Horizontal);
        assert_eq!(layout.active_pane, 0);
        assert!(press_layout_shortcut(
            &[Key::J],
            shortcut_modifiers(true, false),
            2,
            &mut layout
        ));
        assert_eq!(layout.split_direction, PaneSplitDirection::Vertical);
    }

    #[test]
    fn alt_j_outside_split_mode_is_consumed_and_changes_nothing() {
        // The axis only exists in split mode, so there is nothing to flip - but the
        // chord must still be consumed. Its return value is the input layer's skip
        // flag, so an unconsumed Alt+J would be typed into Neovim as a "j".
        let mut layout =
            test_layout_state(PaneLayoutMode::EditorFull, PaneSplitDirection::Vertical, 0);

        assert!(
            press_layout_shortcut(&[Key::J], shortcut_modifiers(true, false), 2, &mut layout),
            "Alt+J must be consumed even when there is no split to flip"
        );
        assert_eq!(
            layout.mode,
            PaneLayoutMode::EditorFull,
            "the mode is unchanged"
        );
        assert_eq!(
            layout.split_direction,
            PaneSplitDirection::Vertical,
            "the direction is unchanged"
        );
    }

    // --- Tabs-mode shortcuts (via:tab-keys) --------------------------------

    /// Four slots: editor at 0, three agents. Returns everything a shortcut test
    /// needs, so each test can reorder tabs without rebuilding the fixture.
    struct TabFixture {
        layout: PaneLayoutState,
        area: AgentArea,
        panes: PaneTargets,
        editor: PaneId,
        agents: [PaneId; 3],
    }

    impl TabFixture {
        fn new() -> Self {
            let editor = PaneId(1);
            let agents = [PaneId(2), PaneId(3), PaneId(4)];
            let mut area = AgentArea::new();
            area.sync(&agents, Some(editor));
            area.set_mode(AgentAreaMode::Tabs);
            Self {
                layout: test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 1),
                area,
                panes: PaneTargets {
                    slots: vec![editor, agents[0], agents[1], agents[2]],
                    editor_index: Some(0),
                },
                editor,
                agents,
            }
        }

        fn press(&mut self, key: Key, shift: bool) -> bool {
            press_layout_shortcut_in(
                &[key],
                shortcut_modifiers(true, shift),
                4,
                &mut self.layout,
                &mut self.area,
            )
        }

        /// The tab the frame would actually draw: `visible_panes` in tabs mode is the
        /// editor plus the active agent, so index 1 is that agent - never a slot index.
        fn active_visible_agent(&self) -> Option<PaneId> {
            self.area
                .visible_panes(Some(self.editor), &self.agents, PaneLayoutMode::Split)
                .get(1)
                .copied()
        }
    }

    #[test]
    fn tabs_mode_alt_number_activates_the_nth_tab_not_the_nth_pane() {
        let mut fixture = TabFixture::new();

        // Alt+2 is the first tab, order[0].
        assert!(fixture.press(Key::Key2, false));
        assert_eq!(fixture.area.active(), Some(fixture.agents[0]));
        assert_eq!(fixture.active_visible_agent(), Some(fixture.agents[0]));

        // Alt+3 is order[1]. Slot index 2 is agent[1] as well here, so reorder first
        // in the dedicated test below; this asserts the order-based mapping.
        assert!(fixture.press(Key::Key3, false));
        assert_eq!(fixture.area.active(), Some(fixture.agents[1]));

        // Alt+1 is the editor and must not move the active tab.
        let before = fixture.area.active();
        assert!(fixture.press(Key::Key1, false));
        assert_eq!(
            fixture.area.active(),
            before,
            "Alt+1 focuses the editor without changing the active tab"
        );
        assert_eq!(
            fixture.layout.active_pane, 0,
            "keyboard focus is the editor"
        );
    }

    #[test]
    fn tabs_mode_alt_four_activates_order_two_after_a_reorder() {
        // The "third tab" from the acceptance, made unambiguous: with tabs reordered
        // so that tab position and slot index disagree, Alt+4 must pick order[2].
        let mut fixture = TabFixture::new();
        // Reverse the tabs: order becomes [agents[2], agents[1], agents[0]], so
        // order[2] is agents[0] - the agent at slot index 1, not slot index 2.
        assert!(fixture.area.move_tab(fixture.agents[1], 0));
        assert!(fixture.area.move_tab(fixture.agents[2], 0));
        assert_eq!(
            fixture.area.order(),
            &[fixture.agents[2], fixture.agents[1], fixture.agents[0]],
            "premise: the tab order is the reverse of the slot order"
        );
        let expected = fixture.area.order()[2];
        assert_eq!(expected, fixture.agents[0]);
        assert_ne!(
            fixture.panes.slots.get(2),
            Some(&expected),
            "premise: order[2] is not the pane at slot index 2"
        );

        // Alt+4 is the third tab: the editor is 1, so tabs start at Alt+2.
        assert!(fixture.press(Key::Key4, false));

        assert_eq!(
            fixture.area.active(),
            Some(expected),
            "Alt+4 activates order[2]"
        );
        assert_ne!(
            fixture.layout.active_pane, 2,
            "and not the pane at slot index 2"
        );
        // The visible frame holds only the editor and the active tab, so indexing it
        // with a tab position would be out of bounds - the point of this assertion.
        let visible = fixture.area.visible_panes(
            Some(fixture.editor),
            &fixture.agents,
            PaneLayoutMode::Split,
        );
        assert_eq!(
            visible.len(),
            2,
            "tabs mode places the editor plus one agent"
        );
        assert_eq!(
            visible.get(2),
            None,
            "visible_panes[2] does not exist, so it cannot be what Alt+4 selected"
        );
        assert_eq!(fixture.active_visible_agent(), Some(expected));
    }

    #[test]
    fn tabs_mode_alt_three_activates_order_one() {
        let mut fixture = TabFixture::new();
        // Reorder so order[1] is not the pane at slot index 2.
        assert!(fixture.area.move_tab(fixture.agents[2], 1));
        let expected = fixture.area.order()[1];
        assert_eq!(expected, fixture.agents[2]);

        assert!(fixture.press(Key::Key3, false));

        assert_eq!(fixture.area.active(), Some(expected));
        assert_ne!(
            fixture.panes.slots.get(2),
            Some(&expected),
            "premise: slot index 2 holds a different pane, so this is not slot indexing"
        );
    }

    #[test]
    fn tabs_mode_alt_past_the_end_is_a_no_op() {
        let mut fixture = TabFixture::new();
        // Only three tabs exist, so Alt+5 and beyond address nothing.
        assert!(fixture.press(Key::Key2, false));
        let before = fixture.area.active();

        for key in [Key::Key5, Key::Key6, Key::Key9] {
            assert!(fixture.press(key, false), "the chord is still consumed");
            assert_eq!(
                fixture.area.active(),
                before,
                "an index past the end must not change the active tab"
            );
        }
    }

    #[test]
    fn tabs_mode_alt_shift_number_activates_that_tab_and_enters_tabs() {
        let mut fixture = TabFixture::new();
        fixture.area.set_mode(AgentAreaMode::Split);

        assert!(fixture.press(Key::Key3, true));

        assert_eq!(
            fixture.area.mode(),
            AgentAreaMode::Tabs,
            "Alt+Shift+N enters tabs mode"
        );
        assert_eq!(fixture.area.active(), Some(fixture.agents[1]));
    }

    #[test]
    fn alt_t_toggles_between_split_and_tabs() {
        let mut fixture = TabFixture::new();
        fixture.area.set_mode(AgentAreaMode::Split);

        assert!(fixture.press(Key::T, false));
        assert_eq!(fixture.area.mode(), AgentAreaMode::Tabs);
        assert_eq!(
            fixture.area.active(),
            Some(fixture.agents[0]),
            "entering tabs activates the focused agent"
        );

        // Leaving tabs restores split without moving focus or the agent share.
        let focused = fixture.layout.active_pane;
        let share = fixture.layout.agent_share;
        assert!(fixture.press(Key::T, false));
        assert_eq!(fixture.area.mode(), AgentAreaMode::Split);
        assert_eq!(fixture.layout.active_pane, focused);
        assert_eq!(fixture.layout.agent_share, share);
    }

    #[test]
    fn alt_t_entering_tabs_keeps_an_existing_active_tab() {
        let mut fixture = TabFixture::new();
        fixture.area.set_mode(AgentAreaMode::Split);
        // Focus the editor, so there is no focused agent to activate.
        fixture.layout.active_pane = 0;
        assert!(fixture.area.activate(fixture.agents[2]));

        assert!(fixture.press(Key::T, false));

        assert_eq!(
            fixture.area.active(),
            Some(fixture.agents[2]),
            "a populated tab set keeps its active tab"
        );
    }

    #[test]
    fn agent_share_refocus_does_not_run_in_tabs_mode() {
        let mut fixture = TabFixture::new();
        fixture.layout.agent_share = AgentPaneShare::Half;
        let span = layout::AgentRegionSpan {
            agent: 150,
            usable: 300,
        };

        // Re-focusing the already-active tab must not resize the (unused) split.
        assert!(fixture.area.activate(fixture.agents[0]));
        assert!(press_layout_shortcut_in_span(
            &[Key::Key2],
            shortcut_modifiers(true, false),
            4,
            &mut fixture.layout,
            &mut fixture.area,
            span,
        ));

        assert_eq!(
            fixture.layout.agent_share,
            AgentPaneShare::Half,
            "the split share is untouched while tabs are on"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn press_layout_shortcut_in_span(
        pressed_keys: &[Key],
        modifiers: Modifiers,
        pane_count: usize,
        layout: &mut PaneLayoutState,
        area: &mut AgentArea,
        span: layout::AgentRegionSpan,
    ) -> bool {
        handle_layout_shortcuts(
            pressed_keys,
            modifiers,
            false,
            pane_count,
            layout,
            area,
            span,
            &shortcut_targets(pane_count),
        )
        .is_some()
    }

    #[test]
    fn new_chords_never_reach_the_pty() {
        // The filter and the handler share one predicate, so every chord the handler
        // would consume must also be skipped by the input path.
        let alt = shortcut_modifiers(true, false);
        let alt_shift = shortcut_modifiers(true, true);

        for key in [
            Key::Key1,
            Key::Key2,
            Key::Key3,
            Key::Key4,
            Key::Key5,
            Key::Key6,
            Key::Key7,
            Key::Key8,
            Key::Key9,
            Key::J,
            Key::T,
        ] {
            assert!(
                layout::layout_chord(key, alt).is_some(),
                "{key:?} with Alt must be filtered from the PTY"
            );
        }
        for key in [Key::Key1, Key::Key2, Key::Key3, Key::Key4, Key::Key9] {
            assert!(
                layout::layout_chord(key, alt_shift).is_some(),
                "{key:?} with Alt+Shift must be filtered from the PTY"
            );
        }

        // And the filter does not swallow ordinary typing.
        assert!(layout::layout_chord(Key::Key1, shortcut_modifiers(false, false)).is_none());
        assert!(layout::layout_chord(Key::T, shortcut_modifiers(false, false)).is_none());
        assert!(layout::layout_chord(Key::Left, shortcut_modifiers(false, false)).is_none());
    }

    #[test]
    fn the_handler_consumes_exactly_what_the_filter_blocks() {
        // The input layer's skip flag *is* the handler's return value, so the two must
        // agree exactly: a chord the handler calls consumed must also be filtered, and
        // anything it does not consume must still reach the agent. Feeding the handler's
        // bool into the real payload builder is what closes the loop - asserting the
        // shared predicate twice would not have caught Alt+J typing "j" in EditorFull.
        let all_keys = [
            Key::Key1,
            Key::Key2,
            Key::Key3,
            Key::Key4,
            Key::Key5,
            Key::Key6,
            Key::Key7,
            Key::Key8,
            Key::Key9,
            Key::Left,
            Key::Right,
            Key::J,
            Key::T,
            Key::K, // not a chord, must pass through
        ];

        for shift in [false, true] {
            let modifiers = shortcut_modifiers(true, shift);

            // Split mode, Tabs mode, EditorFull, and a fixture with no editor at all.
            for mode in [
                AgentAreaMode::Split,
                AgentAreaMode::Tabs,
                AgentAreaMode::Split, // repeated with EditorFull below
            ] {
                let mut area = AgentArea::new();
                let slots = [PaneId(1), PaneId(2), PaneId(3)];
                area.sync(&slots[1..], Some(slots[0]));
                area.set_mode(mode);
                let layout_mode = if mode == AgentAreaMode::Tabs {
                    PaneLayoutMode::EditorFull
                } else {
                    PaneLayoutMode::Split
                };
                check_handler_against_filter(&mut area, layout_mode, &modifiers, &all_keys);
            }
        }

        // No editor, tabs mode: the arms that used to bail out on a missing editor.
        let mut area = AgentArea::new();
        area.sync(&[PaneId(2)], None);
        area.set_mode(AgentAreaMode::Tabs);
        let no_editor = PaneTargets {
            slots: vec![PaneId(2)],
            editor_index: None,
        };
        for shift in [false, true] {
            let modifiers = shortcut_modifiers(true, shift);
            for key in all_keys {
                let mut layout =
                    test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 0);
                let mut area = area.clone();
                let consumed = handle_layout_shortcuts(
                    &[key],
                    modifiers,
                    false,
                    no_editor.slots.len(),
                    &mut layout,
                    &mut area,
                    layout::AgentRegionSpan::unknown(),
                    &no_editor,
                )
                .is_some();
                assert_consumed_matches_filter(key, modifiers, consumed);
            }
        }
    }

    /// Drive the handler and the real PTY payload builder for every key, asserting the
    /// handler's bool is exactly what the filter needs to suppress.
    fn check_handler_against_filter(
        area: &mut AgentArea,
        layout_mode: PaneLayoutMode,
        modifiers: &Modifiers,
        keys: &[Key],
    ) {
        let panes = PaneTargets {
            slots: vec![PaneId(1), PaneId(2), PaneId(3)],
            editor_index: Some(0),
        };
        for key in keys {
            let mut layout = test_layout_state(layout_mode, PaneSplitDirection::Vertical, 1);
            let consumed = handle_layout_shortcuts(
                &[*key],
                *modifiers,
                false,
                panes.slots.len(),
                &mut layout,
                area,
                layout::AgentRegionSpan::unknown(),
                &panes,
            )
            .is_some();
            assert_consumed_matches_filter(*key, *modifiers, consumed);
        }
    }

    /// The invariant: for a recognised chord the handler consumes it, so the payload is
    /// empty; for an unrecognised key it is not consumed and may be typed.
    fn assert_consumed_matches_filter(key: Key, modifiers: Modifiers, consumed: bool) {
        let is_chord = layout::layout_chord(key, modifiers).is_some();
        assert_eq!(
            consumed, is_chord,
            "{key:?} with {modifiers:?}: handler consumed = {consumed}, but it is a chord = {is_chord}"
        );
        if is_chord {
            let payload = input::special_keys_payload(&[key], modifiers, consumed);
            assert!(
                payload.is_empty(),
                "{key:?} with {modifiers:?} was consumed yet leaked {payload:?} to the PTY"
            );
        }
    }

    #[test]
    fn alt_t_with_the_editor_focused_does_not_grab_an_agent() {
        // Decision: entering tabs must not move keyboard focus onto an agent the user
        // was not on. It must still leave EditorFull, so the active tab is visible.
        let mut fixture = TabFixture::new();
        fixture.area.set_mode(AgentAreaMode::Split);
        assert!(fixture.area.activate(fixture.agents[1]));
        fixture.layout.active_pane = 0; // the editor
        fixture.layout.mode = PaneLayoutMode::EditorFull;

        assert!(fixture.press(Key::T, false));

        assert_eq!(fixture.area.mode(), AgentAreaMode::Tabs);
        assert_eq!(
            fixture.area.active(),
            Some(fixture.agents[1]),
            "the active tab is left alone"
        );
        assert_eq!(
            fixture.layout.active_pane, 0,
            "keyboard focus stays on the editor"
        );
        assert_eq!(
            fixture.layout.mode,
            PaneLayoutMode::Split,
            "but EditorFull is left so the active tab is on screen"
        );
    }

    #[test]
    fn alt_t_from_split_mode_takes_focus_from_the_focused_agent() {
        // The other side of decision 2: when an agent does have focus, entering tabs
        // makes its tab active and focus stays on it.
        let mut fixture = TabFixture::new();
        fixture.area.set_mode(AgentAreaMode::Split);
        fixture.layout.active_pane = fixture
            .panes
            .slots
            .iter()
            .position(|id| *id == fixture.agents[2])
            .expect("agent slot");

        assert!(fixture.press(Key::T, false));

        assert_eq!(fixture.area.active(), Some(fixture.agents[2]));
        assert_eq!(fixture.layout.active_pane, 3, "focus stays on that agent");
    }

    #[test]
    fn a_recognised_chord_is_consumed_even_when_it_changes_nothing() {
        // Every arm that used to report failure: the return value feeds the input
        // layer's skip flag, so a matched chord must never be forwarded to the PTY.
        let alt = shortcut_modifiers(true, false);
        let alt_shift = shortcut_modifiers(true, true);

        // Alt+J outside split mode.
        let mut layout =
            test_layout_state(PaneLayoutMode::EditorFull, PaneSplitDirection::Vertical, 0);
        assert!(press_layout_shortcut(&[Key::J], alt, 2, &mut layout));

        // Alt+2..9 with no pane at that index.
        let mut layout = test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 0);
        assert!(press_layout_shortcut(&[Key::Key9], alt, 2, &mut layout));

        // No editor at all: Alt+1, Alt+Shift+1 and split-mode Alt+N.
        let mut layout = test_layout_state(PaneLayoutMode::Split, PaneSplitDirection::Vertical, 0);
        let mut area = AgentArea::new();
        area.sync(&[PaneId(2)], None);
        let no_editor = PaneTargets {
            slots: vec![PaneId(2)],
            editor_index: None,
        };
        for (key, modifiers) in [(Key::Key1, alt), (Key::Key1, alt_shift), (Key::Key2, alt)] {
            let consumed = handle_layout_shortcuts(
                &[key],
                modifiers,
                false,
                1,
                &mut layout,
                &mut area,
                layout::AgentRegionSpan::unknown(),
                &no_editor,
            );
            assert!(
                consumed.is_some(),
                "{key:?} with {modifiers:?} must be consumed with no editor"
            );
        }
    }

    #[test]
    fn tab_switch_chords_use_the_regional_repaint() {
        // Decision 1: only a chord that swaps the visible tab while already in tabs
        // can repaint regionally. Entering tabs changes geometry, so it cannot.
        use layout::LayoutChord;

        for chord in [LayoutChord::FocusAgent(1), LayoutChord::ActivateTab(2)] {
            assert!(
                chord_switches_tab(chord, AgentAreaMode::Tabs),
                "{chord:?} changes the visible tab in tabs mode"
            );
            assert!(
                !chord_switches_tab(chord, AgentAreaMode::Split),
                "{chord:?} in split mode changes no tab"
            );
        }

        // Everything that changes geometry in tabs mode stays a full redraw.
        for chord in [
            LayoutChord::ToggleAgentAreaMode,
            LayoutChord::EditorFull,
            LayoutChord::FocusEditor,
        ] {
            assert!(
                !chord_switches_tab(chord, AgentAreaMode::Tabs),
                "{chord:?} must keep the full redraw"
            );
        }
    }

    #[test]
    fn a_regional_chord_leaves_force_redraw_clear_and_a_full_one_sets_it() {
        // Decision 1 end to end at the flag level, which is where it can silently break:
        // `relayout_for_tab_switch` downgrades `force_redraw`, and a later unconditional
        // `force_redraw = true` would undo it, making the regional path dead code.
        use layout::LayoutChord;

        let editor = PaneId(1);
        let after = test_frame(1600, 600, &[editor, PaneId(3)]);
        let region = agent_region(&after, |id| id != editor);

        for chord in [LayoutChord::FocusAgent(1), LayoutChord::ActivateTab(2)] {
            assert!(chord_switches_tab(chord, AgentAreaMode::Tabs));
            // The downgrade this chord relies on.
            assert!(
                !force_redraw_after_tab_switch(
                    false,
                    tab_switch_repaint(
                        &test_frame(1600, 600, &[editor, PaneId(2)]),
                        &after,
                        region,
                        false,
                    )
                ),
                "{chord:?} must leave force_redraw clear so only the region repaints"
            );
        }

        // Geometry-changing chords keep the full redraw.
        for chord in [LayoutChord::ToggleAgentAreaMode, LayoutChord::EditorFull] {
            assert!(!chord_switches_tab(chord, AgentAreaMode::Tabs));
        }
        // And entering tabs from split mode is not regional, even though it activates a tab.
        assert!(!chord_switches_tab(
            LayoutChord::ActivateTab(0),
            AgentAreaMode::Split
        ));
    }

    // --- AgentFull (agent fullscreen) --------------------------------------

    /// The full-window rect of the fixture frame, so geometry assertions read clearly.
    fn full_window() -> PaneRect {
        PaneRect {
            x: 0,
            y: 0,
            width: 1600,
            height: 600,
        }
    }

    #[test]
    fn agent_full_places_only_the_active_agent_at_full_window() {
        // The geometry the review flagged: copying the EditorFull shortcut (index 0
        // gets the window) would put the EDITOR fullscreen and leave the agent with a
        // zero rect, and the strip would vanish. The agent must be the placed pane.
        let editor = PaneId(1);
        let agents = [PaneId(2), PaneId(3)];
        let mut area = AgentArea::new();
        area.sync(&agents, Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        assert!(area.activate(agents[1]));

        let ids = area.visible_panes(Some(editor), &agents, PaneLayoutMode::AgentFull);
        assert_eq!(
            ids,
            vec![agents[1]],
            "only the active agent is placed - not the editor"
        );

        let frame = test_frame(1600, 600, &ids);
        assert_eq!(
            frame.placed().len(),
            1,
            "a single entry is the single-pane case"
        );
        assert_eq!(
            frame.placed()[0].rect,
            full_window(),
            "and it gets the entire window"
        );
        assert!(
            frame.placed().iter().all(|placed| placed.id != editor),
            "the editor must not be placed at all"
        );
    }

    #[test]
    fn agent_full_keeps_the_strip_on_the_active_agents_top_row() {
        // via:tab-render draws the strip from strip_rect_for on the active agent, gated
        // on tabs mode and more than one agent. It does not care about the layout mode,
        // so this only has to prove the agent is placed with the strip's row available.
        let editor = PaneId(1);
        let agents = [PaneId(2), PaneId(3)];
        let mut area = AgentArea::new();
        area.sync(&agents, Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        assert!(area.activate(agents[1]));

        // The same gates the strip uses.
        assert!(
            tab_strip_visible(area.mode(), area.order().len()),
            "premise: the strip is enabled for this mode and agent count"
        );

        let ids = area.visible_panes(Some(editor), &agents, PaneLayoutMode::AgentFull);
        let frame = test_frame(1600, 600, &ids);
        let strip_height = 20;
        let (strip, below) = frame
            .strip_rect_for(agents[1], strip_height)
            .expect("the active agent hosts the strip");

        assert_eq!(strip.x, 0, "the strip starts at the window's left edge");
        assert_eq!(strip.y, 0, "and at its top");
        assert_eq!(strip.width, 1600, "spanning the full window width");
        assert_eq!(strip.height, strip_height);
        assert_eq!(
            below,
            PaneRect {
                x: 0,
                y: strip_height,
                width: 1600,
                height: 600 - strip_height,
            },
            "the pane below the strip is what resize_panes sizes the terminal to"
        );
        // And the hidden agents have no rect, so they cannot host a strip.
        for hidden in agents.iter().filter(|id| **id != agents[1]) {
            assert!(
                frame.strip_rect_for(*hidden, strip_height).is_none(),
                "a hidden agent must not host a strip"
            );
        }
    }

    #[test]
    fn alt_shift_three_enters_agent_full_with_the_second_tab_active() {
        let mut fixture = TabFixture::new();
        fixture.area.set_mode(AgentAreaMode::Split);

        // Alt+Shift+3 is the second tab (the editor is 1).
        assert!(fixture.press(Key::Key3, true));

        assert_eq!(fixture.area.mode(), AgentAreaMode::Tabs);
        assert_eq!(
            fixture.area.active(),
            Some(fixture.agents[1]),
            "the second tab is active"
        );
        assert_eq!(fixture.layout.mode, PaneLayoutMode::AgentFull);
        let ids = fixture.area.visible_panes(
            Some(fixture.editor),
            &fixture.agents,
            PaneLayoutMode::AgentFull,
        );
        assert_eq!(ids, vec![fixture.agents[1]], "and only it is placed");
    }

    #[test]
    fn alt_number_in_agent_full_switches_the_maximized_agent() {
        let mut fixture = TabFixture::new();
        assert!(fixture.press(Key::Key2, true));
        assert_eq!(fixture.layout.mode, PaneLayoutMode::AgentFull);

        assert!(fixture.press(Key::Key4, false), "Alt+4 is the third tab");

        assert_eq!(fixture.area.active(), Some(fixture.agents[2]));
        assert_eq!(
            fixture.layout.mode,
            PaneLayoutMode::AgentFull,
            "Alt+N in agent fullscreen stays fullscreen"
        );
        assert_eq!(
            fixture.layout.active_pane, 3,
            "and focus moves to the newly maximized agent"
        );
    }

    #[test]
    fn leaving_agent_full_restores_the_editor_rect() {
        // Alt+1 and Alt+T are the two documented exits.
        for key in [Key::Key1, Key::T] {
            let mut fixture = TabFixture::new();
            assert!(fixture.press(Key::Key2, true));
            assert_eq!(fixture.layout.mode, PaneLayoutMode::AgentFull);

            assert!(fixture.press(key, false));

            assert_eq!(
                fixture.layout.mode,
                PaneLayoutMode::Split,
                "{key:?} must leave agent fullscreen"
            );
            let ids = fixture.area.visible_panes(
                Some(fixture.editor),
                &fixture.agents,
                fixture.layout.mode,
            );
            assert!(
                ids.contains(&fixture.editor),
                "{key:?} must place the editor again, got {ids:?}"
            );
            if key == Key::Key1 {
                assert_eq!(fixture.layout.active_pane, 0, "Alt+1 focuses the editor");
            }
        }
    }

    #[test]
    fn closing_the_active_agent_full_tab_falls_back_to_split_with_no_agents() {
        // Acceptance: closing the active tab in AgentFull moves to the successor, or to
        // Split with the editor when no agents remain.
        assert_eq!(
            layout_mode_after_sync(PaneLayoutMode::AgentFull, true),
            PaneLayoutMode::AgentFull,
            "with a successor the agent stays maximized"
        );
        assert_eq!(
            layout_mode_after_sync(PaneLayoutMode::AgentFull, false),
            PaneLayoutMode::Split,
            "with no agent left the editor must come back, not a blank frame"
        );
        // The other modes are untouched.
        for mode in [PaneLayoutMode::Split, PaneLayoutMode::EditorFull] {
            assert_eq!(layout_mode_after_sync(mode, false), mode);
        }
    }

    #[test]
    fn agent_full_with_an_unknown_tab_index_is_a_no_op() {
        let mut fixture = TabFixture::new();
        assert!(fixture.press(Key::Key2, true));
        let before_tab = fixture.area.active();
        let before_mode = fixture.layout.mode;

        // Only three tabs exist.
        assert!(fixture.press(Key::Key9, true), "still consumed");

        assert_eq!(fixture.area.active(), before_tab);
        assert_eq!(fixture.layout.mode, before_mode);
    }

    #[test]
    fn agent_full_chords_pick_the_right_repaint() {
        use layout::LayoutChord;

        // A tab switch that stays in AgentFull keeps the geometry.
        assert!(
            chord_keeps_geometry(
                LayoutChord::FocusAgent(1),
                AgentAreaMode::Tabs,
                PaneLayoutMode::AgentFull
            ),
            "Alt+N swaps only the maximized pane"
        );
        // Entering AgentFull changes geometry, so it keeps the full redraw.
        assert!(
            !chord_keeps_geometry(
                LayoutChord::ActivateTab(1),
                AgentAreaMode::Split,
                PaneLayoutMode::Split
            ),
            "Alt+Shift+N enters agent fullscreen and must repaint the window"
        );
        // Leaving it changes geometry too.
        for chord in [
            LayoutChord::FocusEditor,
            LayoutChord::ToggleAgentAreaMode,
            LayoutChord::EditorFull,
        ] {
            assert!(
                !chord_keeps_geometry(chord, AgentAreaMode::Tabs, PaneLayoutMode::AgentFull),
                "{chord:?} leaves agent fullscreen"
            );
        }
    }

    // --- Editor focus chrome vs regional repaint ---------------------------

    /// The three cases the reviewer named, driven through the shared helper.
    ///
    /// `editor_chrome_changes` is the rule, `tab_switch_repaint` consumes it, and both
    /// call sites (a chord and a strip click) go through `relayout_for_tab_switch`,
    /// which is the only place that knows the previously focused pane.
    #[test]
    fn focus_moving_to_or_from_the_editor_forces_a_full_repaint() {
        let editor = PaneId(1);
        let agents = [PaneId(2), PaneId(3)];
        // Tabs + Split with 2+ panes: chrome is on, which is the buggy configuration.
        let layout_mode = PaneLayoutMode::Split;
        let chrome_shown = true;

        let mut area = AgentArea::new();
        area.sync(&agents, Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        assert!(area.activate(agents[0]));

        // Frames: editor + active agent, exactly what Tabs places.
        let before = test_frame(1600, 600, &[editor, agents[0]]);
        let after_same_agent = test_frame(1600, 600, &[editor, agents[0]]);
        let region = agent_region(&after_same_agent, |id| id != editor);

        // Premise: the frames are geometrically identical, so only the chrome rule can
        // force the full repaint here.
        assert!(frames_share_geometry(&before, &after_same_agent));
        assert!(tab_strip_visible(area.mode(), area.order().len()));

        // Editor -> agent tab: focus leaves the editor, so its active border and the
        // agent's dim tint both change. A regional repaint would leave two panes
        // looking focused.
        let chrome_changes =
            editor_chrome_changes(chrome_shown, Some(editor), Some(agents[0]), Some(editor));
        assert!(chrome_changes, "focus left the editor");
        assert_eq!(
            tab_switch_repaint(&before, &after_same_agent, region, chrome_changes),
            TabSwitchRepaint::Full,
            "editor -> agent must repaint the window, not just the agent region"
        );

        // Agent -> agent: the editor was inactive before and after, so its pixels are
        // untouched and the switch stays regional.
        let agent_to_agent =
            editor_chrome_changes(chrome_shown, Some(agents[0]), Some(agents[1]), Some(editor));
        assert!(!agent_to_agent, "the editor's chrome did not move");
        assert!(matches!(
            tab_switch_repaint(&before, &after_same_agent, region, agent_to_agent),
            TabSwitchRepaint::Region(_)
        ));

        // Agent -> editor: the same rule in the other direction.
        let back_to_editor =
            editor_chrome_changes(chrome_shown, Some(agents[1]), Some(editor), Some(editor));
        assert!(back_to_editor, "focus returned to the editor");
        assert_eq!(
            tab_switch_repaint(&before, &after_same_agent, region, back_to_editor),
            TabSwitchRepaint::Full
        );

        // Chrome off (no split chrome outside Split with 2+ panes, e.g. AgentFull):
        // the editor has no border or tint to update, so the switch stays regional.
        let in_agent_full =
            editor_chrome_changes(false, Some(editor), Some(agents[0]), Some(editor));
        assert!(
            !in_agent_full,
            "AgentFull draws no chrome, so the editor's pixels do not change"
        );
        assert!(
            matches!(
                tab_switch_repaint(&before, &after_same_agent, region, in_agent_full),
                TabSwitchRepaint::Region(_)
            ),
            "AgentFull switches stay regional"
        );
        let _ = layout_mode;
    }

    #[test]
    fn the_editor_chrome_rule_only_fires_when_chrome_is_shown() {
        let editor = Some(PaneId(1));
        let agent = Some(PaneId(2));

        // Chrome shown: crossing the editor in either direction counts.
        assert!(editor_chrome_changes(true, editor, agent, editor));
        assert!(editor_chrome_changes(true, agent, editor, editor));
        // Staying on one side does not.
        assert!(!editor_chrome_changes(true, agent, Some(PaneId(3)), editor));
        assert!(!editor_chrome_changes(true, editor, editor, editor));

        // Chrome hidden: nothing about the editor's pixels changes, whatever moves.
        assert!(!editor_chrome_changes(false, editor, agent, editor));
        assert!(!editor_chrome_changes(false, agent, editor, editor));

        // No editor placed at all: nothing to go stale.
        assert!(!editor_chrome_changes(true, agent, Some(PaneId(3)), None));
    }

    #[test]
    fn a_focus_move_off_the_editor_does_not_leave_force_redraw_clear() {
        // The flag half of the bug: a Full fallback must keep `force_redraw` set, or
        // `render` skips the editor and it keeps its active border.
        let editor = PaneId(1);
        let agents = [PaneId(2), PaneId(3)];
        let mut area = AgentArea::new();
        area.sync(&agents, Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        assert!(area.activate(agents[0]));

        let before = test_frame(1600, 600, &[editor, agents[0]]);
        let after = test_frame(1600, 600, &[editor, agents[0]]);
        let region = agent_region(&after, |id| id != editor);
        let chrome_changes =
            editor_chrome_changes(true, Some(editor), Some(agents[0]), Some(editor));

        let decision = tab_switch_repaint(&before, &after, region, chrome_changes);
        assert_eq!(decision, TabSwitchRepaint::Full);
        assert!(
            force_redraw_after_tab_switch(false, decision),
            "a Full fallback must leave force_redraw set"
        );
        // And nothing was recorded, so `render` paints the whole window.
        assert!(
            tab_switch_region(false, None, &after, region).is_none(),
            "no recorded frame means no regional confinement"
        );

        // Agent -> agent by contrast clears it.
        let regional = tab_switch_repaint(&before, &after, region, false);
        assert!(matches!(regional, TabSwitchRepaint::Region(_)));
        assert!(!force_redraw_after_tab_switch(false, regional));
    }

    #[test]
    fn a_full_fallback_records_no_frame_so_render_also_repaints_everything() {
        // The drift risk between the two users of the decision: the helper records a
        // pre-switch frame only for Region, and `render` re-derives from that frame.
        // If a Full fallback recorded one anyway, `render` would confine the repaint and
        // the editor would keep its stale border. Only the helper knows the focused
        // pane, so this is the contract that keeps them agreeing.
        let editor = PaneId(1);
        let agents = [PaneId(2), PaneId(3)];
        let mut area = AgentArea::new();
        area.sync(&agents, Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        assert!(area.activate(agents[0]));

        let before = test_frame(1600, 600, &[editor, agents[0]]);
        let after = test_frame(1600, 600, &[editor, agents[0]]);
        let region = agent_region(&after, |id| id != editor);

        // Modelled the way `relayout_for_tab_switch` does it: Full => no frame recorded.
        let chrome_changes =
            editor_chrome_changes(true, Some(editor), Some(agents[0]), Some(editor));
        let decision = tab_switch_repaint(&before, &after, region, chrome_changes);
        let recorded = match decision {
            TabSwitchRepaint::Region(_) => Some(before.clone()),
            TabSwitchRepaint::Full => None,
        };
        assert!(recorded.is_none(), "a Full fallback records nothing");
        assert!(
            tab_switch_region(false, recorded.as_ref(), &after, region).is_none(),
            "so render repaints the whole window too"
        );
    }
    #[test]
    fn pane_ids_survive_spawn_terminate_and_respawn() {
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let agent = store.push(test_app_pane("agent"));
        let coder = store.push(test_app_pane("coder"));
        let reviewer = store.push(test_app_pane("reviewer"));
        assert_eq!(store.len(), 4);
        assert_eq!(store.active(), Some(editor));

        // Spawning appends without touching the identity of existing panes.
        let spawned = store.push(test_app_pane("spawned"));
        assert_eq!(store.ids(), vec![editor, agent, coder, reviewer, spawned]);

        // Terminating a pane leaves every survivor's id intact, including the ones
        // that shift position.
        let coder_index = store.index_of(coder).expect("coder slot");
        assert!(store.remove_index(coder_index).is_some());
        assert_eq!(store.ids(), vec![editor, agent, reviewer, spawned]);
        assert!(store.get(coder).is_none());
        assert_eq!(store.active(), Some(editor));

        // Terminating the active pane repoints focus at a surviving pane.
        let agent_index = store.index_of(agent).expect("agent slot");
        store.set_active(agent);
        assert!(store.remove_index(agent_index).is_some());
        assert_eq!(store.ids(), vec![editor, reviewer, spawned]);
        assert_eq!(store.active(), Some(reviewer));

        // Respawning in place keeps the slot's id, so identity survives a restart
        // of the pane itself.
        let reviewer_before = store.ids();
        assert!(store.replace(reviewer, test_app_pane("reviewer-acp")));
        assert_eq!(store.ids(), reviewer_before);
        assert!(
            store
                .get(reviewer)
                .is_some_and(|pane| pane.agent_id().is_some_and(|id| id == "reviewer-acp"))
        );
        assert_eq!(store.active(), Some(reviewer));
    }

    #[test]
    fn pane_ids_are_never_reused_after_removal() {
        let mut store = PaneStore::default();
        let first = store.push(test_app_pane("first"));
        store.push(test_app_pane("second"));
        store.remove_index(0);
        let third = store.push(test_app_pane("third"));

        assert_ne!(first, third);
        assert!(store.index_of(first).is_none());
    }

    /// Minimal terminal pane for store tests; spawns nothing.
    fn test_app_pane(agent_id: &str) -> AppPane {
        let metrics = TerminalMetrics {
            cell_width: 10,
            cell_height: 20,
            baseline: 15,
        };
        let pane = TerminalPane::new("test", 400, 100, metrics, &config::TerminalTheme::default())
            .expect("test terminal pane");
        AppPane::Terminal(TerminalPaneController::new(
            PaneRole::AgentTerminal {
                id: agent_id.to_string(),
                label: agent_id.to_string(),
                command: None,
                acp_tui: false,
            },
            pane,
            1.0,
        ))
    }

    #[test]
    fn activity_is_keyed_by_pane_and_survives_reordering() {
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let agent = store.push(test_app_pane("agent"));
        let now = Instant::now();

        // The ACP feed addresses an agent by bus id, not by slot position.
        store.apply_acp_progress("agent", activity::TURN_PROGRESS_ID, true);
        assert_eq!(
            store.activity(agent).and_then(|a| a.turn.clone()),
            Some(activity::TurnState::Thinking)
        );
        assert!(store.activity(agent).is_some_and(|a| a.is_busy(now)));
        assert_eq!(
            store.activity(editor).and_then(|a| a.turn.clone()),
            None,
            "another pane must not pick up the turn"
        );

        // Dropping an earlier pane shifts indices but not identities, so the
        // activity stays attached to the right pane.
        store.remove_index(0);
        assert!(store.activity(agent).is_some_and(|a| a.is_busy(now)));
        assert!(store.activity(editor).is_none());
    }

    #[test]
    fn acp_progress_for_unknown_agent_is_ignored() {
        let mut store = PaneStore::default();
        store.push(test_app_pane("agent"));

        store.apply_acp_progress("nobody", activity::TURN_PROGRESS_ID, true);

        assert!(store.slots.iter().all(|slot| slot.activity.turn.is_none()));
    }

    #[test]
    fn respawn_resets_activity_because_the_process_is_new() {
        let mut store = PaneStore::default();
        let id = store.push(test_app_pane("agent"));
        let now = Instant::now();
        store.apply_acp_progress("agent", activity::TURN_PROGRESS_ID, true);
        store.mark_seen(id);
        assert!(store.activity(id).is_some_and(|a| a.is_busy(now)));

        store.replace(id, test_app_pane("agent"));

        let activity = store.activity(id).expect("slot keeps its id");
        assert_eq!(activity.turn, None);
        assert_eq!(activity.last_output_at, None);
        assert!(!activity.unseen_output);
        assert!(!activity.exited);
    }

    #[test]
    fn strip_requires_tabs_mode_and_more_than_one_agent() {
        use AgentAreaMode::{Split, Tabs};

        // Split mode already shows every agent, so no strip.
        assert!(!tab_strip_visible(Split, 1));
        assert!(!tab_strip_visible(Split, 3));

        // Tabs mode with a single agent is pure chrome.
        assert!(!tab_strip_visible(Tabs, 0));
        assert!(!tab_strip_visible(Tabs, 1));

        // The case the strip exists for.
        assert!(tab_strip_visible(Tabs, 2));
        assert!(tab_strip_visible(Tabs, 5));
    }

    #[test]
    fn tab_activation_relayouts_only_in_tabs_mode() {
        let mut area = AgentArea::new();
        let agents = [PaneId(2), PaneId(3)];
        area.sync(&agents, Some(PaneId(1)));
        area.activate(PaneId(2));

        // Split mode shows every agent, so switching tabs changes no geometry.
        assert_eq!(area.mode(), AgentAreaMode::Split);
        assert!(!tab_activation_needs_relayout(&area, PaneId(3)));

        // Tabs mode only places the active agent, so a switch needs a relayout.
        area.set_mode(AgentAreaMode::Tabs);
        assert!(tab_activation_needs_relayout(&area, PaneId(3)));

        // Re-activating the tab that is already active changes nothing.
        assert!(!tab_activation_needs_relayout(&area, PaneId(2)));
    }

    /// A frame for `ids` at the given window size, as `relayout` would build it.
    fn test_frame(width: usize, height: usize, ids: &[PaneId]) -> FrameLayout {
        FrameLayout::from_split(
            ids,
            SplitLayout::for_window(
                width,
                height,
                ids.len(),
                PaneLayoutMode::Split,
                PaneSplitDirection::Vertical,
                SplitLayoutOptions::unbounded(),
            ),
        )
    }

    #[test]
    fn mouse_points_are_relative_to_the_drawn_pane_not_the_frame_rect() {
        // The strip host pane gives up its top row, so its terminal grid starts one
        // strip-height below the frame rect. Local coordinates must start there too.
        let editor = PaneId(1);
        let agent = PaneId(2);
        let frame = test_frame(1600, 600, &[editor, agent]);
        let frame_rect = rect_of(&frame, agent);
        let (strip, below) = frame.strip_rect_for(agent, 20).expect("strip split");
        assert_eq!(
            below.y,
            frame_rect.y + strip.height,
            "premise: pane is shifted"
        );

        // The first text row sits at the top of the drawn pane.
        assert_eq!(
            pane_local_point(below, below.x + 7, below.y + 3),
            Some((7, 3)),
            "a click 3px into the drawn pane is 3px into the grid"
        );
        // The old frame-rect math would have reported strip.height + 3.
        assert_ne!(
            pane_local_point(frame_rect, below.x + 7, below.y + 3),
            Some((7, 3))
        );
        // A point in the strip row belongs to no pane.
        assert_eq!(pane_local_point(below, below.x + 7, strip.y + 1), None);
    }

    fn rect_of(frame: &FrameLayout, id: PaneId) -> PaneRect {
        frame
            .placed()
            .iter()
            .find(|placed| placed.id == id)
            .unwrap_or_else(|| panic!("{id:?} is not placed"))
            .rect
    }

    #[test]
    fn tab_switch_repaint_stays_regional_when_only_the_occupant_changes() {
        // Tabs mode places the editor plus one agent, so a switch swaps *which*
        // agent holds the agent rect and leaves both rects alone. This is the case
        // blocker 2 was about: `relayout` raises `force_redraw`, and repainting the
        // editor for it is pure waste.
        let editor = PaneId(1);
        let before = test_frame(1600, 600, &[editor, PaneId(2)]);
        let after = test_frame(1600, 600, &[editor, PaneId(3)]);
        let is_agent = |id: PaneId| id != editor;
        let region = agent_region(&after, is_agent).expect("an agent is placed");

        let decision = tab_switch_repaint(&before, &after, Some(region), false);

        // The switch is confined: `force_full` is false and the region is the agent's.
        assert_eq!(
            decision,
            TabSwitchRepaint::Region(region),
            "a geometry-preserving switch must stay regional"
        );

        let editor_rect = rect_of(&after, editor);
        let agent_rect = rect_of(&after, PaneId(3));
        assert!(
            !rects_intersect(editor_rect, region),
            "the editor must be outside the repaint region: {editor_rect:?} vs {region:?}"
        );
        assert!(
            rects_intersect(agent_rect, region),
            "the incoming agent must be inside the repaint region"
        );
        // Guard the premise: editor and agent really are disjoint in this layout, so
        // "outside the region" is a meaningful statement about the editor.
        assert!(!rects_intersect(editor_rect, agent_rect));
    }

    #[test]
    fn tab_switch_repaint_falls_back_to_the_whole_window_when_geometry_moves() {
        let editor = PaneId(1);
        let before = test_frame(1600, 600, &[editor, PaneId(2)]);
        let after = test_frame(1000, 600, &[editor, PaneId(3)]);
        let is_agent = |id: PaneId| id != editor;
        let region = agent_region(&after, is_agent).expect("an agent is still placed");

        // The rects moved, so the editor's pixels are stale too and it must be drawn.
        assert!(
            !frames_share_geometry(&before, &after),
            "premise: the resize changed the geometry"
        );
        assert_eq!(
            tab_switch_repaint(&before, &after, Some(region), false),
            TabSwitchRepaint::Full,
            "a switch that moves panes must repaint the whole window"
        );
    }

    #[test]
    fn tab_switch_repaint_falls_back_when_there_is_no_agent_region() {
        // No agent is placed, so there is no region to confine the repaint to.
        let before = test_frame(1600, 600, &[PaneId(1)]);
        let after = test_frame(1600, 600, &[PaneId(1)]);

        assert_eq!(
            tab_switch_repaint(&before, &after, None, false),
            TabSwitchRepaint::Full,
            "without an agent region the whole window is repainted"
        );
    }

    #[test]
    fn tab_switch_region_forces_the_incoming_pane_and_covers_the_editor_on_full() {
        // The same geometry-preserving switch as above, now taken all the way through
        // the function `render` actually calls. This is what blocker A was about: the
        // incoming tab is clean, so a regional repaint has to force it or nothing is
        // painted and the old tab's pixels stay under the strip.
        let editor = PaneId(1);
        let before = test_frame(1600, 600, &[editor, PaneId(2)]);
        let after = test_frame(1600, 600, &[editor, PaneId(3)]);
        let is_agent = |id: PaneId| id != editor;
        let region = agent_region(&after, is_agent);

        let repaint_region = tab_switch_region(false, Some(&before), &after, region);
        let region = repaint_region.expect("a regional switch keeps a region");

        // The decision is what forces the drawn pane, so a Region must imply it.
        assert!(
            pane_force_redraw(false, repaint_region),
            "the incoming pane must be forced under a regional switch"
        );
        // And the incoming agent is inside that region, so it is the pane drawn.
        assert!(
            rects_intersect(rect_of(&after, PaneId(3)), region),
            "the incoming agent must be the pane the region covers"
        );
        // The editor is outside it, hence skipped.
        assert!(
            !rects_intersect(rect_of(&after, editor), region),
            "the editor must stay outside a regional switch"
        );

        // A full redraw is never confined: the region is None, so nothing is skipped
        // and every pane - the editor included - is repainted.
        let full = tab_switch_region(true, Some(&before), &after, Some(region));
        assert!(full.is_none(), "a full redraw must not keep a region");

        // A frame with no recorded switch is not regional either, whatever the frames
        // look like, so it cannot accidentally skip the editor.
        assert!(
            tab_switch_region(false, None, &after, agent_region(&after, is_agent)).is_none(),
            "a non-switch frame must not be confined"
        );
        // Neither is a switch whose geometry moved.
        let resized = test_frame(1000, 600, &[editor, PaneId(3)]);
        assert!(
            tab_switch_region(
                false,
                Some(&before),
                &resized,
                agent_region(&resized, is_agent)
            )
            .is_none(),
            "a switch that moves the panes must repaint the window"
        );
    }

    #[test]
    fn a_pending_full_redraw_survives_a_regional_tab_switch() {
        // Blocker C. A resize or font change sets `force_redraw` and waits for the
        // next RedrawRequested. If a strip click renders first, its relayout would
        // clear that flag and render() would then skip the cleared buffer's editor.
        let editor = PaneId(1);
        let before = test_frame(1600, 600, &[editor, PaneId(2)]);
        let after = test_frame(1600, 600, &[editor, PaneId(3)]);
        let region = agent_region(&after, |id| id != editor);

        let regional = tab_switch_repaint(&before, &after, region, false);
        assert!(
            matches!(regional, TabSwitchRepaint::Region(_)),
            "premise: this switch is regional"
        );

        // Nothing was pending: the switch clears the relayout's own flag, so the
        // repaint stays confined.
        assert!(
            !force_redraw_after_tab_switch(false, regional),
            "a switch with nothing pending must not force the window"
        );
        // A full redraw was pending: it must not be downgraded, or `render` clears
        // the buffer and then skips the editor.
        assert!(
            force_redraw_after_tab_switch(true, regional),
            "a pending full redraw must survive a regional switch"
        );

        // And the flag then wins over the region in render's composition.
        let pending = true;
        assert!(
            tab_switch_region(
                force_redraw_after_tab_switch(pending, regional),
                Some(&before),
                &after,
                region
            )
            .is_none(),
            "a surviving full redraw must cover the editor again"
        );

        // A Full switch repaints the window regardless of what was pending.
        assert!(force_redraw_after_tab_switch(false, TabSwitchRepaint::Full));
        assert!(force_redraw_after_tab_switch(true, TabSwitchRepaint::Full));
    }

    #[test]
    fn a_plain_incremental_frame_does_not_force_the_panes() {
        // No region and no full redraw: dirty tracking decides per row, which is what
        // keeps a normal frame cheap. Only a full redraw or a regional switch forces.
        assert!(!pane_force_redraw(false, None));
        assert!(pane_force_redraw(true, None), "a full redraw forces");
        let any = PaneRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(
            pane_force_redraw(false, Some(any)),
            "a regional switch forces the panes it draws"
        );
    }

    #[test]
    fn strip_repaints_when_its_cells_change_but_not_when_they_match() {
        let entry = |id: u64, busy: bool| TabStripEntry {
            id: PaneId::from_raw(id),
            label: "agent".to_string(),
            active: id == 2,
            busy,
            exited: false,
            unseen: false,
        };

        let idle = vec![entry(2, false), entry(3, false)];
        // Never drawn yet: the first paint must happen.
        assert!(
            tab_strip_changed(false, false, &idle, None),
            "the first draw must paint"
        );

        // Identical cells, no full redraw, no switch: nothing to blit. This is the
        // early return that stops every frame from presenting.
        assert!(
            !tab_strip_changed(false, false, &idle, Some(&idle)),
            "identical entries must not repaint"
        );

        // A background pane going busy changes a dot, which arrives as plain output.
        let busy = vec![entry(2, false), entry(3, true)];
        assert!(
            tab_strip_changed(false, false, &busy, Some(&idle)),
            "a new busy dot must repaint the strip"
        );
        // The busy -> idle flip has no event at all, only a changed cell.
        assert!(
            tab_strip_changed(false, false, &idle, Some(&busy)),
            "an expiring dot must repaint the strip"
        );

        // Both override flags still work on their own.
        assert!(tab_strip_changed(true, false, &idle, Some(&idle)));
        assert!(tab_strip_changed(false, true, &idle, Some(&idle)));
    }

    #[test]
    fn an_unshown_strip_never_reports_a_change_and_spins_the_idle_poll() {
        // The idle poll's gate has to be "is a strip on screen", not "does the strip
        // reserve geometry". Tabs mode with 2 agents reserves a row, but if the active
        // agent has no rect nothing is drawn and `render` clears the baseline - so a
        // geometry-based gate would compare None against live entries forever: dirty,
        // redraw, present nothing, ask again, with no frame cap.
        //
        // Drive it from the real layout, in the same shape as
        // `strip_rect_is_absent_when_the_pane_has_no_area`: editor maximized.
        let editor = PaneId(1);
        let agent_one = PaneId(2);
        let agent_two = PaneId(3);

        let mut area = AgentArea::new();
        area.sync(&[agent_one, agent_two], Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        assert!(area.activate(agent_one), "agent_one is the active tab");
        assert_eq!(area.mode(), AgentAreaMode::Tabs);

        // The mode and agent count say "reserve a strip row"...
        assert!(
            tab_strip_visible(area.mode(), area.order().len()),
            "premise: tabs mode with two agents reserves the strip"
        );

        // ...but the editor is maximized, so the agent has no area and no strip can
        // be carved from it. This is the exact case that spun.
        let frame = FrameLayout::from_split(
            &[editor, agent_one],
            SplitLayout::for_window(
                200,
                100,
                2,
                PaneLayoutMode::EditorFull,
                PaneSplitDirection::Vertical,
                SplitLayoutOptions::unbounded(),
            ),
        );
        assert!(
            frame
                .strip_rect_for(area.active().expect("an active tab"), 10)
                .is_none(),
            "premise: the active agent has no rect, so render draws no strip"
        );

        // Nothing is on screen, so there is nothing to repaint - even though the
        // baseline is None and the entries would be non-empty.
        let entries = vec![
            TabStripEntry {
                id: agent_one,
                label: "agent".to_string(),
                active: true,
                busy: true,
                exited: false,
                unseen: false,
            },
            TabStripEntry {
                id: agent_two,
                label: "coder".to_string(),
                active: false,
                busy: false,
                exited: false,
                unseen: false,
            },
        ];
        assert!(!entries.is_empty(), "premise: there are entries to differ");

        // Derive "what would be drawn" the way the idle check does - from the frame,
        // not by writing None by hand. `strip_rect_for` returning None is what makes
        // the overlay absent, so the Option below is None *because* the rect is.
        let shown_on = |frame: &FrameLayout, id: PaneId| -> Option<Vec<TabStripEntry>> {
            frame.strip_rect_for(id, 10).map(|_| entries.clone())
        };

        let active = area.active().expect("an active tab");
        let unshown = shown_on(&frame, active);
        assert!(
            unshown.is_none(),
            "premise: no rect, so nothing is on screen to compare"
        );
        // This is the function the idle check calls, so the branch it really runs -
        // the unshown one - is the one under test.
        assert!(
            !tab_strip_screen_changed(unshown.as_deref(), None),
            "an unshown strip with no baseline must not report a change"
        );
        assert!(
            !tab_strip_screen_changed(unshown.as_deref(), Some(&entries)),
            "an unshown strip must not report a change even with a stale baseline"
        );

        // The opposite, so the gate is not a blanket false: the strip IS shown here,
        // nothing has been drawn yet, so it must report a change and paint once.
        let with_area = FrameLayout::from_split(
            &[editor, agent_one],
            SplitLayout::for_window(
                200,
                100,
                2,
                PaneLayoutMode::Split,
                PaneSplitDirection::Vertical,
                SplitLayoutOptions::unbounded(),
            ),
        );
        let visible = shown_on(&with_area, active);
        assert!(
            visible.is_some(),
            "premise: the agent has a rect, so the strip is on screen"
        );
        assert!(
            tab_strip_screen_changed(visible.as_deref(), None),
            "a shown strip with no baseline must report a change"
        );
        // And once drawn, an identical set settles: no spin in the normal case.
        assert!(!tab_strip_screen_changed(
            visible.as_deref(),
            Some(&entries)
        ));
    }

    #[test]
    fn frames_share_geometry_when_only_the_occupant_changes() {
        let frame = |ids: &[PaneId]| {
            FrameLayout::from_split(
                ids,
                SplitLayout::for_window(
                    200,
                    100,
                    ids.len(),
                    PaneLayoutMode::Split,
                    PaneSplitDirection::Vertical,
                    SplitLayoutOptions::unbounded(),
                ),
            )
        };

        // A tab switch swaps which agent holds the agent region: same rects.
        let before = frame(&[PaneId(1), PaneId(2)]);
        let after = frame(&[PaneId(1), PaneId(3)]);
        assert!(
            frames_share_geometry(&before, &after),
            "a tab switch keeps the geometry, so the repaint can stay regional"
        );

        // A different pane count really does change the geometry.
        let narrowed = frame(&[PaneId(1)]);
        assert!(
            !frames_share_geometry(&before, &narrowed),
            "a frame with fewer panes is not the same geometry"
        );
    }

    #[test]
    fn union_rect_spans_both_inputs() {
        let a = PaneRect {
            x: 10,
            y: 0,
            width: 10,
            height: 10,
        };
        let b = PaneRect {
            x: 0,
            y: 20,
            width: 10,
            height: 10,
        };

        assert_eq!(
            union_rect(a, b),
            PaneRect {
                x: 0,
                y: 0,
                width: 20,
                height: 30
            }
        );
    }

    #[test]
    fn rects_intersect_ignores_touching_edges() {
        let left = PaneRect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let touching = PaneRect {
            x: 10,
            y: 0,
            width: 10,
            height: 10,
        };
        let overlapping = PaneRect {
            x: 9,
            y: 0,
            width: 10,
            height: 10,
        };

        assert!(!rects_intersect(left, touching));
        assert!(rects_intersect(left, overlapping));
    }

    #[test]
    fn focusing_an_agent_activates_its_tab_and_clears_unseen() {
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        store.push(test_app_pane("agent"));
        let coder = store.push(test_app_pane("coder"));
        let mut area = AgentArea::new();
        let agents = store
            .ids()
            .into_iter()
            .filter(|id| *id != editor)
            .collect::<Vec<_>>();
        area.sync(&agents, Some(editor));

        // Output arrived on coder while it was not the active tab.
        mark_unseen(&mut store, coder);
        assert!(store.activity(coder).is_some_and(|a| a.unseen_output));

        // The Alt-key path calls this, mirroring the mouse path.
        note_focus(&mut store, &mut area, coder);

        assert_eq!(area.active(), Some(coder), "tab model follows focus");
        assert!(
            !store.activity(coder).is_some_and(|a| a.unseen_output),
            "focusing the pane clears its unseen marker"
        );
    }

    #[test]
    fn focusing_the_editor_neither_moves_the_tab_nor_clears_an_agent() {
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let agent = store.push(test_app_pane("agent"));
        let mut area = AgentArea::new();
        area.sync(&[agent], Some(editor));

        mark_unseen(&mut store, agent);

        // Alt+1 focuses the editor. That must not activate a tab, and must not
        // claim the agent's unseen output was seen.
        note_focus(&mut store, &mut area, editor);

        assert_eq!(area.active(), Some(agent), "editor never becomes a tab");
        assert!(
            store.activity(agent).is_some_and(|a| a.unseen_output),
            "looking at the editor does not count as looking at the agent"
        );
    }

    #[test]
    fn closing_the_active_tab_after_a_reorder_keeps_tab_and_focus_together() {
        // The reorder case from the review: slots [E, a, b, c] with tab order [b, a, c]
        // and `a` active. Closing `a` leaves PaneStore on b (the slot that slid into
        // the removed one) and the tabs on c (the tab that slid into the removed tab's
        // position). In tabs mode the visible pane is the active tab, so focus must
        // follow the tab - both must end on c, not b.
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let a = store.push(test_app_pane("a"));
        let b = store.push(test_app_pane("b"));
        let c = store.push(test_app_pane("c"));

        let mut area = AgentArea::new();
        area.sync(&[a, b, c], Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        // Reorder to [b, a, c].
        assert!(area.move_tab(b, 0));
        assert!(area.move_tab(a, 1));
        assert_eq!(area.order(), &[b, a, c], "premise: tabs are reordered");
        assert!(area.activate(a));
        store.set_active(a);

        // Close the active tab exactly as `remove_pane_at` does. The store's own pick
        // is b here, which is the divergence.
        let index = store.index_of(a).expect("a slot");
        store.remove_index(index);
        assert_eq!(
            store.active(),
            Some(b),
            "premise: the store on its own would focus b"
        );

        let remaining: Vec<PaneId> = store
            .iter()
            .filter(|slot| slot.pane.is_agent_pane())
            .map(|slot| slot.id)
            .collect();
        sync_agent_area(&mut area, &mut store, &remaining, Some(editor));

        assert_eq!(
            area.active(),
            Some(c),
            "the tab that slid into the removed tab's place takes over"
        );
        assert_eq!(
            store.active(),
            Some(c),
            "keyboard focus follows the active tab, not the next slot"
        );
        assert_ne!(store.active(), Some(b), "and not the slot-neighbor b");
    }

    #[test]
    fn closing_a_background_tab_leaves_focus_alone() {
        // The other half of the contract: closing a tab the user is not on must not
        // move keyboard focus.
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let first = store.push(test_app_pane("first"));
        let second = store.push(test_app_pane("second"));
        let mut area = AgentArea::new();
        area.sync(&[first, second], Some(editor));
        area.set_mode(AgentAreaMode::Tabs);
        area.activate(first);
        store.set_active(first);

        // Close the background tab.
        let index = store.index_of(second).expect("second slot");
        store.remove_index(index);
        sync_agent_area(&mut area, &mut store, &[first], Some(editor));

        assert_eq!(area.active(), Some(first), "the visible tab is unchanged");
        assert_eq!(store.active(), Some(first), "so is keyboard focus");
        assert_eq!(area.order(), &[first]);
    }

    #[test]
    fn ensure_contains_appends_without_disturbing_the_active_tab() {
        // `sync` used to be the only caller; this pins the direct-append contract, and
        // that the editor is refused.
        let editor = PaneId(1);
        let mut area = AgentArea::new();

        assert!(
            area.ensure_contains(PaneId(2), Some(editor)),
            "the first agent appends"
        );
        assert_eq!(area.active(), Some(PaneId(2)), "and becomes active");

        assert!(
            area.ensure_contains(PaneId(3), Some(editor)),
            "a second agent appends"
        );
        assert_eq!(area.order(), &[PaneId(2), PaneId(3)]);
        assert_eq!(
            area.active(),
            Some(PaneId(2)),
            "appending must not steal the active tab"
        );

        assert!(
            !area.ensure_contains(PaneId(2), Some(editor)),
            "re-adding an existing tab is a no-op"
        );
        assert!(
            !area.ensure_contains(editor, Some(editor)),
            "the editor is never a tab"
        );
        assert_eq!(area.order(), &[PaneId(2), PaneId(3)]);
    }

    #[test]
    fn editor_full_survives_removal_and_focus_stays_addressable() {
        // The carry-forward from the pane-ids review, updated for the index-free mode:
        // removing a slot while the editor is full must shift focus, keep the mode, and
        // leave the index in range.
        let mut store = PaneStore::default();
        let _editor = store.push(test_app_pane("editor"));
        let first = store.push(test_app_pane("first"));
        let _second = store.push(test_app_pane("second"));
        let mut layout =
            test_layout_state(PaneLayoutMode::EditorFull, PaneSplitDirection::Vertical, 2);

        // Remove the first agent while focus sits on the last slot.
        let index = store.index_of(first).expect("first slot");
        store.remove_index(index);
        adjust_pane_indices_after_removal(&mut layout, index, store.len());

        assert_eq!(layout.mode, PaneLayoutMode::EditorFull, "the mode is kept");
        assert_eq!(layout.active_pane, 1, "the focus index shifted down");
        assert!(
            layout.active_pane < store.len(),
            "the focus index still addresses a live slot"
        );
    }

    #[test]
    fn sync_clears_the_neighbor_that_takes_over_the_active_tab() {
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let first = store.push(test_app_pane("first"));
        let second = store.push(test_app_pane("second"));
        let mut area = AgentArea::new();
        area.sync(&[first, second], Some(editor));
        area.activate(first);

        // Both tabs have unseen output; then the active tab is terminated.
        mark_unseen(&mut store, first);
        mark_unseen(&mut store, second);
        store.remove_index(store.index_of(first).expect("first slot"));

        sync_agent_area(&mut area, &mut store, &[second], Some(editor));

        assert_eq!(area.active(), Some(second), "neighbor is promoted");
        assert!(
            !store.activity(second).is_some_and(|a| a.unseen_output),
            "the promoted neighbor is on screen, so its unseen marker clears"
        );
    }

    #[test]
    fn sync_leaves_unseen_alone_when_the_active_tab_survives() {
        let mut store = PaneStore::default();
        let editor = store.push(test_app_pane("editor"));
        let first = store.push(test_app_pane("first"));
        let second = store.push(test_app_pane("second"));
        let mut area = AgentArea::new();
        area.sync(&[first, second], Some(editor));
        area.activate(first);
        mark_unseen(&mut store, second);

        // A spawn does not move the active tab, so nothing should be cleared.
        sync_agent_area(&mut area, &mut store, &[first, second], Some(editor));

        assert_eq!(area.active(), Some(first));
        assert!(
            store.activity(second).is_some_and(|a| a.unseen_output),
            "a background tab keeps its unseen marker"
        );
    }

    /// Give `id` unseen output, as if it printed while not the active tab.
    fn mark_unseen(store: &mut PaneStore, id: PaneId) {
        let slot = store
            .slots
            .iter_mut()
            .find(|slot| slot.id == id)
            .expect("slot");
        slot.activity.mark_output(Instant::now(), false);
        // `mark_output` also makes the pane busy; the tests here care about unseen.
        assert!(slot.activity.unseen_output);
    }

    #[test]
    fn mark_seen_clears_unseen_and_reports_the_change() {
        let mut store = PaneStore::default();
        let id = store.push(test_app_pane("agent"));
        let now = Instant::now();

        // Output while this pane is not the active tab is unseen.
        if let Some(slot) = store.slots.iter_mut().find(|slot| slot.id == id) {
            slot.activity.mark_output(now, false);
        }
        assert!(store.activity(id).is_some_and(|a| a.unseen_output));

        assert!(store.mark_seen(id), "clearing an unseen flag reports true");
        assert!(!store.activity(id).is_some_and(|a| a.unseen_output));
        assert!(!store.mark_seen(id), "clearing twice reports false");
        assert!(!store.mark_seen(PaneId(9999)), "unknown ids report false");
    }

    #[test]
    fn nvim_args_install_context_bridge() {
        let app = AppContext {
            user: crate::config::UserConfig {
                nvim_command: "nvim".to_string(),
                agent_command: None,
                acp_agent: None,
                agent_pane_cols: crate::config::AgentPaneCols {
                    min: crate::config::DEFAULT_AGENT_PANE_MIN_COLS,
                    max: crate::config::DEFAULT_AGENT_PANE_MAX_COLS,
                },
                review_backend: ReviewBackend::Nvim,
                scroll_sensitivity: crate::config::DEFAULT_SCROLL_SENSITIVITY,
                agent_presets: crate::config::default_agent_presets(),
                auto_approve: crate::config::AutoApproveConfig::default(),
            },
            paths: crate::config::RuntimePaths {
                nvim_socket_path: PathBuf::from("/tmp/via-nvim.sock"),
                editor_socket_path: PathBuf::from("/tmp/via-editor.sock"),
                agents_dir: PathBuf::from("/tmp/via-agents"),
                nvim_context_bridge_path: PathBuf::from("/repo/nvim/context bridge.lua"),
                nvim_via_module_path: PathBuf::from("/tmp/via-module.lua"),
                lsp_bridge_socket_path: PathBuf::from("/tmp/via-lsp-bridge.sock"),
            },
            launch: crate::config::LaunchContext {
                working_directory: PathBuf::from("/repo"),
                orchestration_enabled: false,
            },
            cli_overrides: crate::config::ConfigOverrides::default(),
        };
        let args = nvim_args(&app);

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
        assert_eq!(args[8], OsString::from("--cmd"));
        assert_eq!(
            args[9],
            OsString::from(format!(
                "lua package.path = package.path .. ';' .. {} .. '/?.lua'",
                lua_string_literal(&crate::config::lua_dir())
            ))
        );
        assert_eq!(args[10], OsString::from("-c"));
        assert_eq!(
            args[11],
            OsString::from("luafile /repo/nvim/context\\ bridge.lua")
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
    fn converts_http_uri_to_url_target() {
        assert_eq!(
            reference_target_from_uri("https://example.com/src/main.rs", Path::new("/repo")),
            Some(ReferenceTarget::Url(
                "https://example.com/src/main.rs".to_string()
            ))
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
