use super::PaneId;
use super::input::{Key, Modifiers};
use crate::config::{DEFAULT_AGENT_PANE_MAX_COLS, DEFAULT_AGENT_PANE_MIN_COLS};
use tracing::info;

const SPLIT_GAP: usize = 2;
/// Minimum leading (editor) pane width in columns for vertical split mode.
/// NOTE: pub so that benches can use `via::ui::ghostty::layout::...` for Criterion
/// regression testing. Not part of via's public library API.
pub const MIN_EDITOR_PANE_COLS: u16 = 80;

/// How the split axis is divided between the editor and the agent region.
///
/// `Auto` keeps the column range in a vertical split and half the window in a
/// horizontal split. `Half` is 50/50. `Third` gives the agent one third and the
/// editor two thirds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum AgentPaneShare {
    #[default]
    Auto,
    /// Editor and agent region each take half of the split axis.
    Half,
    /// Agent region takes one third; the editor takes two thirds.
    Third,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefocusSide {
    Editor,
    Agent,
}

impl AgentPaneShare {
    /// Repeat focus swaps between an even split and the agent at one third
    /// (editor at two thirds). `Auto` jumps to whichever of those is farther
    /// from the focused pane's current span.
    fn after_refocus(self, side: RefocusSide, span: AgentRegionSpan) -> Self {
        match (side, self) {
            (_, Self::Half) => Self::Third,
            (_, Self::Third) => Self::Half,
            (RefocusSide::Agent, Self::Auto) => {
                // Midpoint of 1/3 and 1/2 is 5/12. At or above that, shrink the agent to a third.
                if span.agent.saturating_mul(12) >= span.usable.saturating_mul(5) {
                    Self::Third
                } else {
                    Self::Half
                }
            }
            (RefocusSide::Editor, Self::Auto) => {
                let editor = span.usable.saturating_sub(span.agent);
                // Midpoint of 1/2 and 2/3 is 7/12. At or above that, move the editor to half.
                // Below that, grow the editor to two thirds.
                if editor.saturating_mul(12) >= span.usable.saturating_mul(7) {
                    Self::Half
                } else {
                    Self::Third
                }
            }
        }
    }
}

/// Agent-region size along the current split axis, gap excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AgentRegionSpan {
    pub(super) agent: usize,
    pub(super) usable: usize,
}

impl AgentRegionSpan {
    pub(super) fn unknown() -> Self {
        Self {
            agent: 0,
            usable: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SplitLayoutOptions {
    pub(super) cell_width: usize,
    pub(super) agent_pane_cols: Option<(u16, u16)>,
    pub(super) agent_share: AgentPaneShare,
}

impl SplitLayoutOptions {
    pub(super) fn unbounded() -> Self {
        Self {
            cell_width: 1,
            agent_pane_cols: None,
            agent_share: AgentPaneShare::Auto,
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
    /// Only the editor has geometry; every agent is hidden. This is the old
    /// `PaneMaximized(0)`.
    EditorFull,
    /// Only the active agent has geometry, at the full window size; the editor and the
    /// other agents get a zero rect. This is the old `PaneMaximized(i)` for `i > 0`,
    /// but it carries no index: which agent is full-screen comes from
    /// [`AgentArea::active`], so a reorder or a close cannot leave a stale index
    /// pointing at another pane's rect.
    AgentFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

/// Mutable split state: which pane is focused, how the window is divided, and
/// how much of the split the agent region takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneLayoutState {
    pub(super) mode: PaneLayoutMode,
    pub(super) split_direction: PaneSplitDirection,
    pub(super) active_pane: usize,
    pub(super) agent_share: AgentPaneShare,
}

impl PaneLayoutState {
    /// Show the editor fullscreen and focus it.
    pub(super) fn focus_maximized_editor(&mut self) {
        self.mode = PaneLayoutMode::EditorFull;
        self.active_pane = 0;
    }

    /// Build a state for the focus benchmark. Split direction and agent share stay at defaults.
    pub fn with_focus(mode: PaneLayoutMode, active_pane: usize) -> Self {
        Self {
            mode,
            split_direction: PaneSplitDirection::Vertical,
            active_pane,
            agent_share: AgentPaneShare::Auto,
        }
    }
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

        // `EditorFull` must be handled before the multi-pane split logic.
        if mode == PaneLayoutMode::EditorFull && pane_count > 0 {
            let mut panes = vec![
                PaneRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0
                };
                pane_count
            ];
            panes[0] = PaneRect {
                x: 0,
                y: 0,
                width,
                height,
            };
            return Self { panes };
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
            let base = two_pane_layout(width, height, split_direction, options);
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

        two_pane_layout(width, height, split_direction, options)
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

    pub(super) fn len(&self) -> usize {
        self.panes.len()
    }
}

/// One pane and the rect it occupies for the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedPane {
    pub id: PaneId,
    pub rect: PaneRect,
}

/// A frame's placement of panes, keyed by stable identity.
///
/// [`SplitLayout`] stays purely positional (and unchanged) so the layout math and
/// its tests do not need to know about pane identity. `FrameLayout` pairs those
/// rects with the [`PaneId`]s of the slots that produced them, so a hit test or a
/// resize addresses the pane itself instead of whatever index it happens to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameLayout {
    placed: Vec<PlacedPane>,
    split: SplitLayout,
}

impl FrameLayout {
    /// Pair `ids` (slot order) with the rects `split` computed for that order.
    /// Extra ids beyond the split's rect count are dropped; extra rects beyond
    /// `ids` are unreachable, which keeps this panic-free on any input.
    pub(super) fn from_split(ids: &[PaneId], split: SplitLayout) -> Self {
        let placed = ids
            .iter()
            .copied()
            .enumerate()
            .take(split.len())
            .map(|(index, id)| PlacedPane {
                id,
                rect: split.pane(index),
            })
            .collect();
        Self { placed, split }
    }

    /// The positional layout this frame was derived from.
    pub(super) fn split(&self) -> &SplitLayout {
        &self.split
    }

    pub fn placed(&self) -> &[PlacedPane] {
        &self.placed
    }

    /// Pane id and rect under `(x, y)`. Hidden (zero-sized) panes never match.
    pub fn pane_at(&self, x: usize, y: usize) -> Option<(PaneId, PaneRect)> {
        // Hit-testing stays owned by `SplitLayout`; this only maps the positional
        // result back to the identity of the slot that produced it.
        let (index, rect) = self.split.pane_at(x, y)?;
        let id = self.placed.get(index)?.id;
        Some((id, rect))
    }

    /// The strip rect for `id`, plus the pane rect left below it.
    ///
    /// The strip takes the top row of the agent region and the pane keeps the rest,
    /// so panes are drawn below the strip instead of under it. Pure: it reads the
    /// frame and returns two rects rather than mutating the stored layout, which
    /// matters because the render path calls it on every frame.
    ///
    /// Returns `None` when the pane has no visible area (hidden behind another tab,
    /// or fullscreened away), in which case nothing is reserved or drawn.
    pub(super) fn strip_rect_for(
        &self,
        id: PaneId,
        strip_height: usize,
    ) -> Option<(PaneRect, PaneRect)> {
        let rect = self
            .placed
            .iter()
            .find(|placed| placed.id == id)
            .map(|placed| placed.rect)?;
        if rect.width == 0 || rect.height == 0 {
            return None;
        }
        let height = strip_height.min(rect.height);
        if height == 0 {
            return None;
        }
        Some((
            PaneRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height,
            },
            PaneRect {
                x: rect.x,
                y: rect.y + height,
                width: rect.width,
                height: rect.height - height,
            },
        ))
    }
}

/// How the agent region presents its agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentAreaMode {
    /// Every agent gets geometry in the agent region (the long-standing behavior).
    #[default]
    Split,
    /// Only the active agent gets geometry; the rest are hidden behind tabs.
    Tabs,
}

/// The agent region as a group of tabs: a stable order, one active member, and
/// the mode that decides whether all of them or only the active one is shown.
///
/// This is a pure model. It owns no panes, does no rendering, and knows nothing
/// about key bindings; callers feed it the live agent ids and ask which panes a
/// frame should place.
///
/// Three invariants are maintained by every mutator:
///
/// 1. `order` is a permutation of the live agent ids - no duplicates, and none
///    missing.
/// 2. `active` is either a member of `order` or `None`.
/// 3. The editor pane is never a member of `order`; tabs only ever hold agents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentArea {
    order: Vec<PaneId>,
    active: Option<PaneId>,
    mode: AgentAreaMode,
}

impl AgentArea {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(&self) -> AgentAreaMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: AgentAreaMode) {
        self.mode = mode;
    }

    /// Tab order; a permutation of the live agent ids.
    pub fn order(&self) -> &[PaneId] {
        &self.order
    }

    pub fn active(&self) -> Option<PaneId> {
        self.active
    }

    /// Reconcile `order` with the live agent ids, keeping the existing tab order.
    ///
    /// Surviving tabs keep their relative order, newly spawned agents append, and
    /// agents that no longer exist are dropped. This is the one entry point that
    /// re-establishes the permutation invariant after a spawn or a terminate, so
    /// it also drops duplicates and refuses `editor`.
    ///
    /// When `active` is `None` and agents exist, the first tab becomes active, so
    /// a populated area always has something to show. When `active` was removed,
    /// the tab that slid into its place takes over, mirroring how pane focus
    /// behaves when a pane is closed.
    pub fn sync(&mut self, agents: &[PaneId], editor: Option<PaneId>) {
        let mut next: Vec<PaneId> = Vec::with_capacity(agents.len());
        for id in self.order.iter().copied() {
            if Some(id) == editor || next.contains(&id) || !agents.contains(&id) {
                continue;
            }
            next.push(id);
        }
        for id in agents.iter().copied() {
            if Some(id) == editor || next.contains(&id) {
                continue;
            }
            next.push(id);
        }

        // Where the active tab sat, so a removal can hand over to its neighbor.
        let previous = self
            .active
            .and_then(|active| self.order.iter().position(|id| *id == active));

        self.order = next;

        let active_is_live = self
            .active
            .is_some_and(|active| self.order.contains(&active));
        if !active_is_live {
            self.active = match previous {
                Some(index) if !self.order.is_empty() => {
                    Some(self.order[index.min(self.order.len() - 1)])
                }
                _ => self.order.first().copied(),
            };
        }
    }

    /// Add `id` as the last tab if it is not already present. Returns whether the
    /// order changed. Refuses `editor`, which never belongs to a tab set.
    pub fn ensure_contains(&mut self, id: PaneId, editor: Option<PaneId>) -> bool {
        if Some(id) == editor || self.order.contains(&id) {
            return false;
        }
        self.order.push(id);
        if self.active.is_none() {
            self.active = Some(id);
        }
        true
    }

    /// Drop `id` from the tab set. When it was the active tab, the tab that takes
    /// its place becomes active (or `None` when nothing remains).
    pub fn remove(&mut self, id: PaneId) -> bool {
        let Some(index) = self.order.iter().position(|tab| *tab == id) else {
            return false;
        };
        self.order.remove(index);
        if self.active == Some(id) {
            self.active = self
                .order
                .get(index.min(self.order.len().saturating_sub(1)))
                .copied();
        }
        true
    }

    /// Make `id` the active tab. Succeeds only for a tab that is in `order`, so
    /// activating a removed (or unknown) pane is a no-op that returns `false`.
    pub fn activate(&mut self, id: PaneId) -> bool {
        if !self.order.contains(&id) {
            return false;
        }
        self.active = Some(id);
        true
    }

    /// Move the tab `id` to `new_index`, clamped into range. Returns whether the
    /// tab exists, so an unknown id cannot disturb the order.
    pub fn move_tab(&mut self, id: PaneId, new_index: usize) -> bool {
        let Some(from) = self.order.iter().position(|tab| *tab == id) else {
            return false;
        };
        self.order.remove(from);
        let to = new_index.min(self.order.len());
        self.order.insert(to, id);
        true
    }

    /// The pane ids a frame should place, editor first.
    ///
    /// `Split` returns the editor followed by every agent in slot order, which is
    /// exactly what the geometry assumed before tabs existed. `Tabs` returns the
    /// editor plus at most the active agent, so hidden panes get no rect at all.
    ///
    /// In [`PaneLayoutMode::AgentFull`] the editor is dropped and only the active
    /// agent is placed, which [`SplitLayout`] then gives the whole window: a single
    /// entry is the single-pane case. The agent is chosen by identity, not by index,
    /// so this mode cannot point at another pane's rect.
    pub fn visible_panes(
        &self,
        editor: Option<PaneId>,
        agents: &[PaneId],
        layout_mode: PaneLayoutMode,
    ) -> Vec<PaneId> {
        let active_agent = self.active.filter(|id| agents.contains(id));
        if layout_mode == PaneLayoutMode::AgentFull {
            return active_agent.into_iter().collect();
        }
        let mut visible = Vec::with_capacity(agents.len() + 1);
        visible.extend(editor);
        match self.mode {
            AgentAreaMode::Split => visible.extend(agents.iter().copied()),
            AgentAreaMode::Tabs => visible.extend(active_agent),
        }
        visible
    }
}
/// Apply an Alt chord to the layout and agent-region state.
///
/// Returns the chord that was consumed, or `None` when no chord matched. Any
/// recognised chord is consumed even when it changes nothing: the return value is the
/// input layer's skip flag, so reporting a chord as unhandled would forward it to the
/// PTY and type it into the agent (Alt+J in `EditorFull` used to insert a "j").
///
/// Focus stays index-based in `Split` mode. In `Tabs` mode the frame is
/// editor + one agent, so a slot index would address the wrong rect - there the digit
/// keys address `AgentArea::order` instead, and `active_pane` is derived from the tab
/// that ends up active rather than used as an index into the frame.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_layout_shortcuts(
    pressed_keys: &[Key],
    modifiers: Modifiers,
    repeat: bool,
    pane_count: usize,
    layout: &mut PaneLayoutState,
    agent_area: &mut AgentArea,
    agent_span: AgentRegionSpan,
    panes: &PaneTargets,
) -> Option<LayoutChord> {
    for key in pressed_keys {
        let Some(chord) = layout_chord(*key, modifiers) else {
            continue;
        };
        apply_layout_chord(
            chord, repeat, pane_count, layout, agent_area, agent_span, panes,
        );
        return Some(chord);
    }
    None
}

/// Whether this chord swaps the visible pane without moving any rect.
///
/// Such a switch repaints only the agent region instead of the whole window.
///
/// `AgentFull` is the exception that has to be excluded by the caller: Alt+Shift+N
/// *enters* it from a split, which changes the geometry. `chord_switches_tab` therefore
/// only answers for a layout mode the chord leaves alone - see
/// [`chord_keeps_geometry`], which takes the mode from before the chord ran.
pub(super) fn chord_switches_tab(chord: LayoutChord, mode: AgentAreaMode) -> bool {
    mode == AgentAreaMode::Tabs
        && matches!(
            chord,
            LayoutChord::FocusAgent(_) | LayoutChord::ActivateTab(_)
        )
}

/// Whether this chord only swaps the visible pane, so the frame is unchanged.
///
/// `area_mode` and `layout_mode` are the modes from *before* the chord ran, because
/// the interesting cases are exactly the ones that change them:
///
/// - `ActivateTab` (Alt+Shift+N) enters `AgentFull` from a split, so it moves every
///   rect and needs the full redraw.
/// - `FocusAgent` in `AgentFull` swaps which agent fills the window: same rect, so it
///   is regional. Entering `AgentFull` that way is only reachable from `Split` mode,
///   where `area_mode` is not `Tabs`, and is excluded here.
pub(super) fn chord_keeps_geometry(
    chord: LayoutChord,
    area_mode: AgentAreaMode,
    layout_mode: PaneLayoutMode,
) -> bool {
    match layout_mode {
        // Alt+N inside a maximized agent keeps the single full-window rect.
        PaneLayoutMode::AgentFull => {
            matches!(chord, LayoutChord::FocusAgent(_))
        }
        PaneLayoutMode::Split | PaneLayoutMode::EditorFull => chord_switches_tab(chord, area_mode),
    }
}

/// The pane identities the layout layer needs to translate between indices and tabs.
///
/// The layout layer deliberately does not know about `PaneStore`; this is the small
/// window onto it that tab addressing requires.
#[derive(Debug, Clone, Default)]
pub(super) struct PaneTargets {
    /// Every pane id in slot order, so an index is a slot index.
    pub(super) slots: Vec<PaneId>,
    /// The editor's slot index, when there is one.
    pub(super) editor_index: Option<usize>,
}

impl PaneTargets {
    fn id_at(&self, index: usize) -> Option<PaneId> {
        self.slots.get(index).copied()
    }

    fn slot_of(&self, id: PaneId) -> Option<usize> {
        self.slots.iter().position(|slot| *slot == id)
    }
}

/// Apply one chord. Every arm is a no-op at worst; the caller treats a matched chord
/// as consumed, so none of them report failure.
#[allow(clippy::too_many_arguments)]
fn apply_layout_chord(
    chord: LayoutChord,
    repeat: bool,
    pane_count: usize,
    layout: &mut PaneLayoutState,
    agent_area: &mut AgentArea,
    agent_span: AgentRegionSpan,
    panes: &PaneTargets,
) {
    match chord {
        // Consumed either way; there is simply no axis to flip outside split mode.
        LayoutChord::ToggleSplitDirection => {
            if layout.mode == PaneLayoutMode::Split {
                layout.split_direction = layout.split_direction.toggled();
            }
        }
        // Always consumed. Entering tabs keeps the focused agent's tab; leaving tabs
        // returns to split without touching focus or the agent share.
        LayoutChord::ToggleAgentAreaMode => match agent_area.mode() {
            AgentAreaMode::Split => {
                agent_area.set_mode(AgentAreaMode::Tabs);
                // Only an agent that the tab set actually accepted may take focus.
                let focused_agent = panes
                    .id_at(layout.active_pane)
                    .filter(|id| agent_area.activate(*id));
                match focused_agent {
                    Some(id) => {
                        // Already focused; make sure the layout agrees it is visible.
                        if let Some(slot) = panes.slot_of(id) {
                            layout.active_pane = slot;
                        }
                    }
                    None => {
                        // The editor had focus. Keep `AgentArea.active` and the keyboard
                        // focus where they are, but leave `EditorFull` so the active tab
                        // is on screen. Do not hand focus to an agent the user was not on.
                        if agent_area.active().is_none() {
                            if let Some(first) = agent_area.order().first().copied() {
                                agent_area.activate(first);
                            }
                        }
                        if layout.mode == PaneLayoutMode::EditorFull {
                            layout.mode = PaneLayoutMode::Split;
                        }
                    }
                }
            }
            AgentAreaMode::Tabs => {
                agent_area.set_mode(AgentAreaMode::Split);
                layout.mode = PaneLayoutMode::Split;
            }
        },
        // Alt+1: focus the editor. In tabs mode this must not move the active tab,
        // so it only moves keyboard focus. The refocus cycle below is split-mode
        // only: in tabs there is no split axis to resize.
        LayoutChord::FocusEditor => {
            let Some(editor_index) = panes.editor_index.filter(|index| *index < pane_count) else {
                return;
            };
            let tabs = agent_area.mode() == AgentAreaMode::Tabs;
            if pane_count >= 2
                && !tabs
                && layout.mode == PaneLayoutMode::Split
                && layout.active_pane == editor_index
            {
                if !repeat {
                    layout.agent_share = layout
                        .agent_share
                        .after_refocus(RefocusSide::Editor, agent_span);
                }
                return;
            }
            layout.active_pane = editor_index;
            layout.mode = PaneLayoutMode::Split;
        }
        LayoutChord::FocusAgent(agent_index) => focus_agent(
            agent_index,
            repeat,
            pane_count,
            layout,
            agent_area,
            agent_span,
            panes,
        ),
        LayoutChord::EditorFull => {
            if panes.editor_index.is_some() {
                layout.focus_maximized_editor();
            }
        }
        LayoutChord::ActivateTab(agent_index) => {
            let Some(id) = agent_area.order().get(agent_index).copied() else {
                info!(
                    agent_index,
                    tabs = agent_area.order().len(),
                    "tab shortcut ignored: no tab at this index (spawn the agent first?)"
                );
                return;
            };
            agent_area.set_mode(AgentAreaMode::Tabs);
            agent_area.activate(id);
            sync_active_pane_from_tabs(layout, agent_area, panes);
            // Agent fullscreen: this tab's pane is the only one on screen. The mode
            // carries no index, so it always resolves through `AgentArea::active`.
            layout.mode = PaneLayoutMode::AgentFull;
        }
    }
}

/// Alt+2..9: focus the Nth agent.
///
/// In split mode this is the slot index (unchanged behaviour, including the
/// toggle-on-refocus agent-share cycle). In tabs mode it is `AgentArea::order[n]`,
/// because the frame holds only the active agent.
fn focus_agent(
    agent_index: usize,
    repeat: bool,
    pane_count: usize,
    layout: &mut PaneLayoutState,
    agent_area: &mut AgentArea,
    agent_span: AgentRegionSpan,
    panes: &PaneTargets,
) {
    if agent_area.mode() == AgentAreaMode::Tabs {
        let Some(id) = agent_area.order().get(agent_index).copied() else {
            info!(
                agent_index,
                tabs = agent_area.order().len(),
                "focus shortcut ignored: no tab at this index (spawn the agent first?)"
            );
            return;
        };
        agent_area.activate(id);
        // Alt+N inside a maximized agent switches which agent is maximized.
        let stay_fullscreen = layout.mode == PaneLayoutMode::AgentFull;
        sync_active_pane_from_tabs(layout, agent_area, panes);
        if stay_fullscreen {
            layout.mode = PaneLayoutMode::AgentFull;
        }
        return;
    }

    // Split mode keeps the original index-based behaviour. Agent N is slot N (the
    // editor is slot 0), which only holds while the editor is first; when it does not,
    // the index would address the wrong pane, so this is a consumed no-op.
    if panes.editor_index != Some(0) {
        return;
    }
    let target = agent_index + 1;
    if target >= pane_count {
        info!(
            digit = agent_index + 2,
            pane_count, "focus shortcut ignored: no pane at this index (spawn the agent first?)"
        );
        return;
    }
    if pane_count >= 2 && layout.mode == PaneLayoutMode::Split && layout.active_pane == target {
        if !repeat {
            layout.agent_share = layout
                .agent_share
                .after_refocus(RefocusSide::Agent, agent_span);
        }
        return;
    }
    layout.mode = PaneLayoutMode::Split;
    layout.active_pane = target;
}

/// Point `active_pane` at the slot holding the active tab.
///
/// `active_pane` is a slot index, so this is the one place the tab identity becomes
/// the focus index. Callers must never use a tab position as an index into the frame,
/// which holds only the editor and the active agent.
fn sync_active_pane_from_tabs(
    layout: &mut PaneLayoutState,
    agent_area: &AgentArea,
    panes: &PaneTargets,
) {
    let Some(active) = agent_area.active() else {
        return;
    };
    if let Some(index) = panes.slots.iter().position(|id| *id == active) {
        layout.active_pane = index;
        layout.mode = PaneLayoutMode::Split;
    }
}

/// The layout mode to keep once the tab model has been reconciled.
///
/// `AgentFull` shows only [`AgentArea::active`], so with no agent left there is
/// nothing to fill the window and every pane would be unplaced. Falling back to
/// `Split` puts the editor back on screen instead of leaving a blank frame.
pub(super) fn layout_mode_after_sync(
    mode: PaneLayoutMode,
    has_active_agent: bool,
) -> PaneLayoutMode {
    match mode {
        PaneLayoutMode::AgentFull if !has_active_agent => PaneLayoutMode::Split,
        other => other,
    }
}

/// Keep `active_pane` valid after removing a pane.
///
/// `PaneLayoutMode` no longer holds an index, so only the focus index needs shifting.
/// `EditorFull` always points at the editor, which is slot 0, and that slot only
/// disappears if the editor itself is removed.
pub(super) fn adjust_pane_indices_after_removal(
    layout: &mut PaneLayoutState,
    removed_index: usize,
    remaining_pane_count: usize,
) {
    if layout.active_pane > removed_index {
        layout.active_pane -= 1;
    }
    layout.active_pane = layout
        .active_pane
        .min(remaining_pane_count.saturating_sub(1));

    if remaining_pane_count == 0 {
        layout.mode = PaneLayoutMode::Split;
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
pub fn focus_nvim_after_agent_reference(layout: &mut PaneLayoutState) -> FocusNvimAfterReference {
    // Agent fullscreen has to give up the window for Neovim, so this is a real relayout
    // again - the same transition the old `PaneMaximized(1)` performed. In the other
    // modes only focus moves.
    let relayout_needed = layout.mode == PaneLayoutMode::AgentFull;
    let focus_changed = layout.active_pane != 0;
    if relayout_needed {
        layout.focus_maximized_editor();
    } else {
        layout.active_pane = 0;
    }
    FocusNvimAfterReference {
        relayout_needed,
        focus_changed,
    }
}

/// What an Alt chord asks the layout layer to do.
///
/// This is the single list of chords the app owns. [`layout_chord`] is the only place
/// that maps a key plus modifiers onto one of them, and both the shortcut handler and
/// the PTY input filter consult it - so a chord can never be handled in one place and
/// forwarded to the terminal in the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayoutChord {
    /// Alt+J: flip the split axis. Only meaningful while the layout mode is `Split`.
    ToggleSplitDirection,
    /// Alt+T: switch the agent region between `Split` and `Tabs`.
    ToggleAgentAreaMode,
    /// Alt+1: focus the editor. Never changes the active tab.
    FocusEditor,
    /// Alt+2..9: the Nth agent. Zero-based, so Alt+2 is agent 0.
    FocusAgent(usize),
    /// Alt+Shift+1: the editor fullscreen.
    EditorFull,
    /// Alt+Shift+2..9: enter tabs mode with the Nth agent's tab active.
    ActivateTab(usize),
}

/// The chord `key` with `modifiers` maps to, or `None`.
///
/// Shift variants are matched before plain ones so `Alt+Shift+N` cannot fall through
/// to the `Alt+N` focus case. Arrow keys are deliberately not chords: Alt+Left/Right
/// (and the Shift variants) belong to the editor and the agent TUIs.
pub(super) fn layout_chord(key: Key, modifiers: Modifiers) -> Option<LayoutChord> {
    if !modifiers.alt {
        return None;
    }

    if modifiers.shift {
        return match key {
            Key::Key1 => Some(LayoutChord::EditorFull),
            Key::Key2 => Some(LayoutChord::ActivateTab(0)),
            Key::Key3 => Some(LayoutChord::ActivateTab(1)),
            Key::Key4 => Some(LayoutChord::ActivateTab(2)),
            Key::Key5 => Some(LayoutChord::ActivateTab(3)),
            Key::Key6 => Some(LayoutChord::ActivateTab(4)),
            Key::Key7 => Some(LayoutChord::ActivateTab(5)),
            Key::Key8 => Some(LayoutChord::ActivateTab(6)),
            Key::Key9 => Some(LayoutChord::ActivateTab(7)),
            _ => None,
        };
    }

    match key {
        Key::J => Some(LayoutChord::ToggleSplitDirection),
        Key::T => Some(LayoutChord::ToggleAgentAreaMode),
        Key::Key1 => Some(LayoutChord::FocusEditor),
        Key::Key2 => Some(LayoutChord::FocusAgent(0)),
        Key::Key3 => Some(LayoutChord::FocusAgent(1)),
        Key::Key4 => Some(LayoutChord::FocusAgent(2)),
        Key::Key5 => Some(LayoutChord::FocusAgent(3)),
        Key::Key6 => Some(LayoutChord::FocusAgent(4)),
        Key::Key7 => Some(LayoutChord::FocusAgent(5)),
        Key::Key8 => Some(LayoutChord::FocusAgent(6)),
        Key::Key9 => Some(LayoutChord::FocusAgent(7)),
        _ => None,
    }
}

fn two_pane_layout(
    width: usize,
    height: usize,
    split_direction: PaneSplitDirection,
    options: SplitLayoutOptions,
) -> SplitLayout {
    match split_direction {
        PaneSplitDirection::Vertical => {
            if let Some(agent_width) = agent_share_span(width, options.agent_share) {
                return vertical_share_layout(width, height, agent_width);
            }
            vertical_split_layout(width, height, options.cell_width, options.agent_pane_cols)
        }
        PaneSplitDirection::Horizontal => {
            if let Some(agent_height) = agent_share_span(height, options.agent_share) {
                return horizontal_share_layout(width, height, agent_height);
            }
            horizontal_split_layout(width, height)
        }
    }
}

/// Agent span along `total` pixels, or `None` when the automatic layout applies.
fn agent_share_span(total: usize, share: AgentPaneShare) -> Option<usize> {
    let usable = total.saturating_sub(SPLIT_GAP);
    match share {
        AgentPaneShare::Auto => None,
        AgentPaneShare::Half => Some(usable / 2),
        AgentPaneShare::Third => Some(usable / 3),
    }
}

pub(super) fn agent_region_span(
    layout: &SplitLayout,
    direction: PaneSplitDirection,
    width: usize,
    height: usize,
) -> AgentRegionSpan {
    if layout.panes.len() < 2 {
        return AgentRegionSpan::unknown();
    }
    match direction {
        PaneSplitDirection::Vertical => AgentRegionSpan {
            agent: layout.pane(1).width,
            usable: width.saturating_sub(SPLIT_GAP),
        },
        PaneSplitDirection::Horizontal => AgentRegionSpan {
            agent: layout.pane(1).height,
            usable: height.saturating_sub(SPLIT_GAP),
        },
    }
}

fn vertical_share_layout(width: usize, height: usize, agent_width: usize) -> SplitLayout {
    let usable = width.saturating_sub(SPLIT_GAP);
    let agent_width = agent_width.min(usable);
    let leading_width = usable.saturating_sub(agent_width);
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

fn horizontal_share_layout(width: usize, height: usize, agent_height: usize) -> SplitLayout {
    let usable = height.saturating_sub(SPLIT_GAP);
    let agent_height = agent_height.min(usable);
    let top_height = usable.saturating_sub(agent_height);
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
    fn reference_navigation_from_editor_full_keeps_editor_full_and_focuses_nvim() {
        let mut layout = test_layout(PaneLayoutMode::EditorFull, 1, AgentPaneShare::Auto);

        let focus = focus_nvim_after_agent_reference(&mut layout);

        assert_eq!(layout.mode, PaneLayoutMode::EditorFull);
        assert_eq!(layout.active_pane, 0);
        assert!(
            !focus.relayout_needed,
            "the layout mode did not have to change"
        );
        assert!(focus.focus_changed);
    }

    #[test]
    fn reference_navigation_from_split_keeps_split_and_focuses_nvim() {
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Auto);

        let focus = focus_nvim_after_agent_reference(&mut layout);

        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 0);
        assert!(!focus.relayout_needed);
        assert!(focus.focus_changed);
    }

    #[test]
    fn reference_navigation_when_nvim_already_active_in_split() {
        let mut layout = test_layout(PaneLayoutMode::Split, 0, AgentPaneShare::Auto);

        let focus = focus_nvim_after_agent_reference(&mut layout);

        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 0);
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
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Auto);
        let focus = focus_nvim_after_agent_reference(&mut layout);
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 0);
        assert!(!focus.relayout_needed);
        assert!(focus.focus_changed);
    }

    #[test]
    fn adjust_pane_indices_after_removal_keeps_editor_full_and_shifts_focus() {
        // The mode no longer carries an index, so removing an earlier pane only
        // shifts the focus index - EditorFull still means "the editor".
        let mut layout = test_layout(PaneLayoutMode::EditorFull, 2, AgentPaneShare::Auto);
        adjust_pane_indices_after_removal(&mut layout, 0, 2);
        assert_eq!(layout.mode, PaneLayoutMode::EditorFull);
        assert_eq!(layout.active_pane, 1);
    }

    #[test]
    fn adjust_pane_indices_after_removal_resets_mode_when_every_pane_is_gone() {
        let mut layout = test_layout(PaneLayoutMode::EditorFull, 0, AgentPaneShare::Auto);
        adjust_pane_indices_after_removal(&mut layout, 0, 0);
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 0);
    }

    #[test]
    fn adjust_pane_indices_after_removal_shifts_active_pane_down() {
        let mut layout = test_layout(PaneLayoutMode::Split, 2, AgentPaneShare::Auto);
        adjust_pane_indices_after_removal(&mut layout, 0, 2);
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 1);
    }

    fn options_with_share(agent_share: AgentPaneShare) -> SplitLayoutOptions {
        SplitLayoutOptions {
            agent_share,
            ..SplitLayoutOptions::unbounded()
        }
    }

    #[test]
    fn vertical_half_share_splits_width_evenly() {
        let layout = SplitLayout::for_window(
            302,
            80,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
            options_with_share(AgentPaneShare::Half),
        );

        assert_eq!(layout.pane(0).width, 150);
        assert_eq!(layout.pane(1).x, 152);
        assert_eq!(layout.pane(1).width, 150);
        assert_eq!(layout.pane(1).height, 80);
    }

    #[test]
    fn vertical_third_share_gives_agent_one_third_of_width() {
        let layout = SplitLayout::for_window(
            302,
            90,
            3,
            PaneLayoutMode::Split,
            PaneSplitDirection::Vertical,
            options_with_share(AgentPaneShare::Third),
        );

        assert_eq!(layout.pane(0).width, 200);
        assert_eq!(layout.pane(1).width, 100);
        assert_eq!(layout.pane(2).width, 100);
        assert_eq!(layout.pane(1).height, 45);
        assert_eq!(layout.pane(2).height, 45);
    }

    #[test]
    fn horizontal_third_share_gives_agent_one_third_of_height() {
        let layout = SplitLayout::for_window(
            80,
            302,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Horizontal,
            options_with_share(AgentPaneShare::Third),
        );

        assert_eq!(layout.pane(0).height, 200);
        assert_eq!(layout.pane(1).y, 202);
        assert_eq!(layout.pane(1).height, 100);
        assert_eq!(layout.pane(1).width, 80);
    }

    #[test]
    fn horizontal_half_share_splits_height_evenly() {
        let layout = SplitLayout::for_window(
            40,
            202,
            2,
            PaneLayoutMode::Split,
            PaneSplitDirection::Horizontal,
            options_with_share(AgentPaneShare::Half),
        );

        assert_eq!(layout.pane(0).height, 100);
        assert_eq!(layout.pane(1).height, 100);
    }

    fn alt_held() -> Modifiers {
        Modifiers {
            alt: true,
            ..Modifiers::default()
        }
    }

    fn test_layout(
        mode: PaneLayoutMode,
        active_pane: usize,
        agent_share: AgentPaneShare,
    ) -> PaneLayoutState {
        PaneLayoutState {
            mode,
            split_direction: PaneSplitDirection::Vertical,
            active_pane,
            agent_share,
        }
    }

    /// Slot order for the three-pane fixtures below: editor, agent, agent.
    const TEST_SLOTS: [PaneId; 3] = [PaneId(1), PaneId(2), PaneId(3)];

    fn test_targets() -> PaneTargets {
        PaneTargets {
            slots: TEST_SLOTS.to_vec(),
            editor_index: Some(0),
        }
    }

    /// A `Split`-mode agent area matching `TEST_SLOTS`, so the split-mode focus tests
    /// exercise the unchanged index-based path.
    fn test_split_area() -> AgentArea {
        let mut area = AgentArea::new();
        area.sync(&TEST_SLOTS[1..], Some(PaneId(1)));
        area
    }

    fn press_agent_focus(
        key: Key,
        repeat: bool,
        layout: &mut PaneLayoutState,
        agent_span: AgentRegionSpan,
    ) -> bool {
        press_shortcut_with(
            key,
            alt_held(),
            repeat,
            3,
            layout,
            &mut test_split_area(),
            agent_span,
            &test_targets(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn press_shortcut_with(
        key: Key,
        modifiers: Modifiers,
        repeat: bool,
        pane_count: usize,
        layout: &mut PaneLayoutState,
        area: &mut AgentArea,
        agent_span: AgentRegionSpan,
        panes: &PaneTargets,
    ) -> bool {
        handle_layout_shortcuts(
            &[key],
            modifiers,
            repeat,
            pane_count,
            layout,
            area,
            agent_span,
            panes,
        )
        .is_some()
    }

    #[test]
    fn refocusing_agent_toggles_share_between_half_and_third() {
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Auto);
        let half_span = AgentRegionSpan {
            agent: 150,
            usable: 300,
        };

        assert!(press_agent_focus(Key::Key2, false, &mut layout, half_span));
        assert_eq!(layout.active_pane, 1);
        assert_eq!(layout.agent_share, AgentPaneShare::Third);

        assert!(press_agent_focus(Key::Key2, false, &mut layout, half_span));
        assert_eq!(layout.agent_share, AgentPaneShare::Half);

        assert!(press_agent_focus(Key::Key2, false, &mut layout, half_span));
        assert_eq!(layout.agent_share, AgentPaneShare::Third);
    }

    #[test]
    fn refocus_from_narrow_auto_share_grows_to_half() {
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Auto);
        let narrow = AgentRegionSpan {
            agent: 80,
            usable: 240,
        };

        assert!(press_agent_focus(Key::Key2, false, &mut layout, narrow));
        assert_eq!(layout.agent_share, AgentPaneShare::Half);
    }

    #[test]
    fn focusing_a_different_agent_does_not_change_share() {
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Third);

        assert!(press_agent_focus(
            Key::Key3,
            false,
            &mut layout,
            AgentRegionSpan::unknown()
        ));
        assert_eq!(layout.active_pane, 2);
        assert_eq!(layout.agent_share, AgentPaneShare::Third);
    }

    #[test]
    fn refocus_while_editor_full_restores_split_without_resizing() {
        let mut layout = test_layout(PaneLayoutMode::EditorFull, 1, AgentPaneShare::Auto);

        assert!(press_agent_focus(
            Key::Key2,
            false,
            &mut layout,
            AgentRegionSpan {
                agent: 300,
                usable: 300,
            }
        ));
        assert_eq!(layout.mode, PaneLayoutMode::Split);
        assert_eq!(layout.active_pane, 1);
        assert_eq!(layout.agent_share, AgentPaneShare::Auto);
    }

    #[test]
    fn key_repeat_on_focused_agent_does_not_toggle_share() {
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Half);

        assert!(press_agent_focus(
            Key::Key2,
            true,
            &mut layout,
            AgentRegionSpan::unknown()
        ));
        assert_eq!(layout.agent_share, AgentPaneShare::Half);
    }

    #[test]
    fn alt_arrows_are_ignored_and_do_not_change_agent_share() {
        let mut layout = test_layout(PaneLayoutMode::Split, 1, AgentPaneShare::Half);

        assert!(
            handle_layout_shortcuts(
                &[Key::Right],
                alt_held(),
                false,
                2,
                &mut layout,
                &mut test_split_area(),
                AgentRegionSpan::unknown(),
                &test_targets(),
            )
            .is_none(),
            "Alt+Right belongs to the focused program, not the layout"
        );
        assert_eq!(layout.active_pane, 1);
        assert_eq!(layout.agent_share, AgentPaneShare::Half);
    }

    #[test]
    fn refocusing_editor_toggles_editor_between_half_and_third() {
        let mut layout = test_layout(PaneLayoutMode::Split, 0, AgentPaneShare::Half);
        let half_span = AgentRegionSpan {
            agent: 150,
            usable: 300,
        };

        assert!(press_agent_focus(Key::Key1, false, &mut layout, half_span));
        assert_eq!(layout.active_pane, 0);
        assert_eq!(layout.agent_share, AgentPaneShare::Third);

        assert!(press_agent_focus(Key::Key1, false, &mut layout, half_span));
        assert_eq!(layout.agent_share, AgentPaneShare::Half);
    }

    #[test]
    fn refocusing_large_editor_moves_it_to_half() {
        let mut layout = test_layout(PaneLayoutMode::Split, 0, AgentPaneShare::Auto);

        assert!(press_agent_focus(
            Key::Key1,
            false,
            &mut layout,
            AgentRegionSpan {
                agent: 80,
                usable: 240,
            }
        ));
        assert_eq!(layout.agent_share, AgentPaneShare::Half);
    }

    #[test]
    fn refocusing_small_editor_grows_it_to_two_thirds() {
        let mut layout = test_layout(PaneLayoutMode::Split, 0, AgentPaneShare::Auto);

        assert!(press_agent_focus(
            Key::Key1,
            false,
            &mut layout,
            AgentRegionSpan {
                agent: 200,
                usable: 300,
            }
        ));
        assert_eq!(layout.agent_share, AgentPaneShare::Third);
    }

    // --- AgentArea (tabs model) -------------------------------------------

    const EDITOR: PaneId = PaneId(1);
    const A1: PaneId = PaneId(2);
    const A2: PaneId = PaneId(3);
    const A3: PaneId = PaneId(4);

    /// The three invariants every mutator must preserve, checked as one unit.
    fn assert_invariants(area: &AgentArea, agents: &[PaneId]) {
        // 1. order is a permutation of the live agent ids.
        assert_eq!(
            area.order().len(),
            agents.len(),
            "order must have one tab per live agent, got {:?} for {agents:?}",
            area.order()
        );
        for id in area.order() {
            assert!(
                agents.contains(id),
                "{id:?} is in order but is not a live agent"
            );
            assert_eq!(
                area.order().iter().filter(|tab| *tab == id).count(),
                1,
                "{id:?} must appear in order exactly once"
            );
        }
        for id in agents {
            assert!(
                area.order().contains(id),
                "live agent {id:?} is missing from order"
            );
        }

        // 2. active is in order or None.
        if let Some(active) = area.active() {
            assert!(
                area.order().contains(&active),
                "active {active:?} must be a member of order"
            );
        }

        // 3. the editor is never a tab.
        assert!(
            !area.order().contains(&EDITOR),
            "editor must never appear in the tab order"
        );
    }

    #[test]
    fn agent_area_defaults_to_split_with_every_agent_visible() {
        let mut area = AgentArea::new();
        assert_eq!(area.mode(), AgentAreaMode::Split);
        assert_eq!(area.order(), &[]);
        assert_eq!(area.active(), None);

        let agents = [A1, A2];
        area.sync(&agents, Some(EDITOR));
        assert_invariants(&area, &agents);

        // Split mode keeps the pre-tabs geometry: editor plus every agent.
        assert_eq!(
            area.visible_panes(Some(EDITOR), &agents, PaneLayoutMode::Split),
            vec![EDITOR, A1, A2],
            "Split must place every agent, as before tabs existed"
        );
    }

    #[test]
    fn agent_area_order_is_a_permutation_after_spawn() {
        let mut area = AgentArea::new();

        // Spawn one agent at a time; each spawn appends and keeps the rest.
        area.sync(&[A1], Some(EDITOR));
        assert_eq!(area.order(), &[A1]);
        assert_invariants(&area, &[A1]);

        area.sync(&[A1, A2], Some(EDITOR));
        assert_eq!(area.order(), &[A1, A2]);
        assert_invariants(&area, &[A1, A2]);

        area.sync(&[A1, A2, A3], Some(EDITOR));
        assert_eq!(area.order(), &[A1, A2, A3]);
        assert_invariants(&area, &[A1, A2, A3]);
    }

    #[test]
    fn agent_area_order_is_a_permutation_after_terminate() {
        let mut area = AgentArea::new();
        area.sync(&[A1, A2, A3], Some(EDITOR));
        area.activate(A2);

        // Terminating a middle tab drops only that tab, keeping relative order.
        area.sync(&[A1, A3], Some(EDITOR));
        assert_eq!(area.order(), &[A1, A3]);
        assert_invariants(&area, &[A1, A3]);
        assert_eq!(
            area.active(),
            Some(A3),
            "the tab that slides into the active slot takes over"
        );

        // Terminating the last remaining agent leaves an empty, active-less area.
        area.sync(&[], Some(EDITOR));
        assert_eq!(area.order(), &[]);
        assert_eq!(area.active(), None);
        assert_invariants(&area, &[]);
    }

    #[test]
    fn agent_area_order_is_a_permutation_after_reorder() {
        let mut area = AgentArea::new();
        let agents = [A1, A2, A3];
        area.sync(&agents, Some(EDITOR));

        assert!(area.move_tab(A3, 0));
        assert_eq!(area.order(), &[A3, A1, A2]);
        assert_invariants(&area, &agents);

        assert!(area.move_tab(A1, 2));
        assert_eq!(area.order(), &[A3, A2, A1]);
        assert_invariants(&area, &agents);

        // An out-of-range index clamps instead of panicking or losing a tab.
        assert!(area.move_tab(A3, 99));
        assert_eq!(area.order(), &[A2, A1, A3]);
        assert_invariants(&area, &agents);

        // Moving an unknown id is a no-op that reports failure.
        assert!(!area.move_tab(PaneId(99), 0));
        assert_eq!(area.order(), &[A2, A1, A3]);
        assert_invariants(&area, &agents);
    }

    #[test]
    fn agent_area_activating_a_removed_pane_is_a_no_op() {
        let mut area = AgentArea::new();
        area.sync(&[A1, A2], Some(EDITOR));
        area.activate(A2);

        // A2 is terminated; activating it again must not resurrect it or clear
        // the real active tab.
        area.sync(&[A1], Some(EDITOR));
        assert_eq!(area.active(), Some(A1));

        assert!(!area.activate(A2), "activating a removed tab must fail");
        assert_eq!(area.active(), Some(A1));
        assert_eq!(area.order(), &[A1]);
        assert_invariants(&area, &[A1]);

        // Activating a tab that never existed behaves the same way.
        assert!(!area.activate(PaneId(77)));
        assert_eq!(area.active(), Some(A1));
    }

    #[test]
    fn agent_area_never_accepts_the_editor_as_a_tab() {
        let mut area = AgentArea::new();
        area.sync(&[A1, EDITOR], Some(EDITOR));

        assert_eq!(
            area.order(),
            &[A1],
            "sync must filter the editor out of the agents list"
        );
        assert!(!area.ensure_contains(EDITOR, Some(EDITOR)));
        assert_eq!(area.order(), &[A1]);
        assert_invariants(&area, &[A1]);
    }

    #[test]
    fn agent_area_remove_clears_active_when_nothing_remains() {
        let mut area = AgentArea::new();
        area.sync(&[A1], Some(EDITOR));
        assert_eq!(area.active(), Some(A1));

        assert!(area.remove(A1));
        assert_eq!(area.order(), &[]);
        assert_eq!(area.active(), None);
        assert!(!area.remove(A1), "removing an absent tab reports failure");
    }

    #[test]
    fn agent_area_tabs_mode_hides_inactive_panes_from_geometry() {
        let frame = |visible: &[PaneId]| -> FrameLayout {
            let split = SplitLayout::for_window(
                200,
                100,
                visible.len(),
                PaneLayoutMode::Split,
                PaneSplitDirection::Vertical,
                SplitLayoutOptions::unbounded(),
            );
            FrameLayout::from_split(visible, split)
        };

        let mut area = AgentArea::new();
        let agents = [A1, A2];
        area.sync(&agents, Some(EDITOR));
        area.set_mode(AgentAreaMode::Tabs);
        area.activate(A1);

        // Only the editor and the active tab are placed.
        let visible = area.visible_panes(Some(EDITOR), &agents, PaneLayoutMode::Split);
        assert_eq!(visible, vec![EDITOR, A1]);

        // The hidden tab has no placement at all, so resize never reaches it.
        let tabbed = frame(&visible);
        assert!(
            !tabbed.placed().iter().any(|placed| placed.id == A2),
            "a tabbed-out pane must not appear in the frame"
        );
        assert_eq!(tabbed.placed().len(), 2);

        // Revealing the other tab gives it a real rect.
        area.activate(A2);
        let revealed = frame(&area.visible_panes(Some(EDITOR), &agents, PaneLayoutMode::Split));
        let rect = revealed
            .placed()
            .iter()
            .find(|placed| placed.id == A2)
            .expect("revealed tab must be placed")
            .rect;
        assert!(
            rect.width > 0 && rect.height > 0,
            "revealed tab must get a non-zero rect, got {rect:?}"
        );

        // And in Split mode both agents are placed again.
        area.set_mode(AgentAreaMode::Split);
        assert_eq!(
            frame(&area.visible_panes(Some(EDITOR), &agents, PaneLayoutMode::Split))
                .placed()
                .len(),
            3
        );
    }

    #[test]
    fn switching_tabs_rebuilds_the_frame_so_the_strip_survives() {
        // The full click path: activate a different tab, rebuild the frame from
        // `visible_panes`, and check the strip is still resolvable afterwards.
        let frame_for = |area: &AgentArea, agents: &[PaneId]| -> FrameLayout {
            let visible = area.visible_panes(Some(EDITOR), agents, PaneLayoutMode::Split);
            FrameLayout::from_split(
                &visible,
                SplitLayout::for_window(
                    200,
                    100,
                    visible.len(),
                    PaneLayoutMode::Split,
                    PaneSplitDirection::Vertical,
                    SplitLayoutOptions::unbounded(),
                ),
            )
        };

        let mut area = AgentArea::new();
        let agents = [A1, A2];
        area.sync(&agents, Some(EDITOR));
        area.set_mode(AgentAreaMode::Tabs);
        area.activate(A1);

        let before = frame_for(&area, &agents);
        assert!(before.strip_rect_for(A1, 10).is_some(), "strip under A1");

        // Click the other tab.
        assert!(area.activate(A2));
        let after = frame_for(&area, &agents);

        // The new tab is placed and hosts the strip...
        assert!(
            after.strip_rect_for(A2, 10).is_some(),
            "the newly active tab must be placed and keep the strip"
        );
        // ...the old tab is gone from the frame...
        assert!(
            !after.placed().iter().any(|placed| placed.id == A1),
            "the previous tab must leave the frame"
        );
        // ...and the editor is untouched.
        let editor_before = before
            .placed()
            .iter()
            .find(|placed| placed.id == EDITOR)
            .expect("editor placed")
            .rect;
        let editor_after = after
            .placed()
            .iter()
            .find(|placed| placed.id == EDITOR)
            .expect("editor placed")
            .rect;
        assert_eq!(
            editor_before, editor_after,
            "a tab switch must not move or resize the editor"
        );
    }

    #[test]
    fn strip_rect_sits_above_the_pane_in_both_split_directions() {
        for direction in [PaneSplitDirection::Vertical, PaneSplitDirection::Horizontal] {
            let frame = FrameLayout::from_split(
                &[EDITOR, A1],
                SplitLayout::for_window(
                    200,
                    100,
                    2,
                    PaneLayoutMode::Split,
                    direction,
                    SplitLayoutOptions::unbounded(),
                ),
            );
            let pane_rect = frame
                .placed()
                .iter()
                .find(|placed| placed.id == A1)
                .expect("agent placed")
                .rect;

            let (strip, below) = frame
                .strip_rect_for(A1, 10)
                .expect("strip fits in the agent region");

            assert_eq!(strip.y, pane_rect.y, "{direction:?}: strip at pane top");
            assert_eq!(strip.x, pane_rect.x, "{direction:?}: strip aligned");
            assert_eq!(
                strip.width, pane_rect.width,
                "{direction:?}: strip spans pane"
            );
            assert_eq!(strip.height, 10, "{direction:?}: one row tall");
            assert_eq!(
                below.y,
                pane_rect.y + 10,
                "{direction:?}: pane starts below the strip"
            );
            assert_eq!(
                below.height,
                pane_rect.height - 10,
                "{direction:?}: pane gives up exactly the strip row"
            );
        }
    }

    #[test]
    fn strip_rect_is_absent_when_the_pane_has_no_area() {
        // EditorFull: only the editor has geometry, so an agent has no rect and no
        // strip can be carved from it.
        let frame = FrameLayout::from_split(
            &[EDITOR, A1],
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
            frame.strip_rect_for(A1, 10).is_none(),
            "a pane with no area must not host a strip"
        );
    }

    #[test]
    fn strip_never_exceeds_the_pane_it_rides_on() {
        let frame = FrameLayout::from_split(
            &[EDITOR, A1],
            SplitLayout::for_window(
                200,
                100,
                2,
                PaneLayoutMode::Split,
                PaneSplitDirection::Vertical,
                SplitLayoutOptions::unbounded(),
            ),
        );
        let (strip, below) = frame
            .strip_rect_for(A1, 10_000)
            .expect("strip clamps rather than overflow");

        assert_eq!(strip.height, below.y - strip.y);
        assert_eq!(below.height, 0, "an oversized strip consumes the pane");
    }
}
