//! Host-side per-pane activity, so tabs can later show busy / exited / unseen dots.
//!
//! Nothing here renders or touches the PTY. It is a plain state model plus pure
//! predicates, so the busy rule can be exercised against a fake clock instead of
//! sleeping in tests.
//!
//! Two producers feed it:
//!
//! - **PTY agents** have no protocol signal at all. Their only evidence of work is
//!   output arriving, so busy is "output within [`BUSY_OUTPUT_WINDOW`]".
//! - **ACP agents** do have one: tool-call progress and the `__turn` marker. That
//!   is authoritative while it lasts, and it is what distinguishes an agent that
//!   is thinking silently from one that is simply idle.
//!
//! The busy predicate is deliberately pure (`last_output_at` + `now`) so callers
//! own the clock. Its consumer is the tab strip, which lands in `via:tab-render`;
//! until then the producer side records state and the predicate is exercised by
//! this module's tests.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// How long after the last output a PTY pane still counts as busy.
///
/// PTY agents emit no start/stop signal, so a short trailing window is the only
/// available proxy. One second is long enough to cover the gap between chunks of
/// a single response and short enough that an idle pane settles promptly.
// TODO(via:tab-render): consumed by the busy dot; kept here so the rule and its
// producer stay in one place.
#[allow(dead_code)]
pub const BUSY_OUTPUT_WINDOW: Duration = Duration::from_secs(1);

/// The `__turn` progress id used on the ACP wire for whole-turn start/stop.
pub const TURN_PROGRESS_ID: &str = "__turn";

/// What an ACP agent is doing, as far as the host has been told.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TurnState {
    /// A prompt is in flight but no tool has been reported yet.
    #[default]
    Thinking,
    /// One or more tool calls are open.
    ///
    /// The open calls are tracked by id, not counted. `src/acp.rs` emits
    /// `Progress { active: true }` for a `tool_call` *and* again for a later
    /// `tool_call_update` carrying the same `toolCallId`, so a counter would
    /// increment twice for one tool and never balance. Ids are idempotent:
    /// re-reporting the same open call changes nothing.
    Tools { open: BTreeSet<String> },
}

/// Activity for one pane.
///
/// Every mutator is independent of wall-clock time except [`Self::mark_output`]
/// and [`Self::is_busy`], which both take the current instant explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneActivity {
    /// When output last arrived, if it ever has.
    pub last_output_at: Option<Instant>,
    /// ACP turn state, or `None` when the agent is not working.
    pub turn: Option<TurnState>,
    /// Output arrived while this pane was not the active tab.
    pub unseen_output: bool,
    /// The pane's process has exited.
    pub exited: bool,
}

impl PaneActivity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that output was drained at `now`.
    ///
    /// `is_active_tab` decides whether the output is considered seen: work that
    /// lands on a tab the user is not looking at must raise the unseen flag.
    /// The editor is never a tab, so callers must pass `true` for it rather than
    /// letting it accumulate an unseen marker.
    pub fn mark_output(&mut self, now: Instant, is_active_tab: bool) {
        self.last_output_at = Some(now);
        if !is_active_tab {
            self.unseen_output = true;
        }
    }

    /// Apply one `AcpProgress` update.
    ///
    /// `id` is [`TURN_PROGRESS_ID`] for whole-turn markers and a tool-call id
    /// otherwise. This is the only ACP signal that reaches the host, so both
    /// directions of the turn live here.
    pub fn apply_acp_progress(&mut self, id: &str, active: bool) {
        if id == TURN_PROGRESS_ID {
            self.turn = if active {
                Some(TurnState::Thinking)
            } else {
                None
            };
            return;
        }

        if active {
            let open = match self.turn.take() {
                // Re-reporting an already-open call is idempotent.
                Some(TurnState::Tools { mut open }) => {
                    open.insert(id.to_string());
                    open
                }
                // A tool opening after the thinking phase starts the tool phase.
                _ => BTreeSet::from([id.to_string()]),
            };
            self.turn = Some(TurnState::Tools { open });
            return;
        }

        // A lone "completed" for a call that was never seen open must not end
        // the turn, so removal is conditional on the id being present. Note this
        // matches through a mutable borrow: taking `self.turn` first would turn an
        // active `Thinking` state into `None` on the way in, so a stray completion
        // would silently end a turn that a tool has not even started yet.
        let mut finished = false;
        if let Some(TurnState::Tools { open }) = &mut self.turn {
            open.remove(id);
            finished = open.is_empty();
        }
        if finished {
            self.turn = None;
        }
    }

    /// Mark the pane's process as exited. An exited pane is never busy.
    pub fn mark_exited(&mut self) {
        self.exited = true;
        self.turn = None;
    }

    /// The user is now looking at this pane, so nothing is unseen any more.
    pub fn mark_seen(&mut self) {
        self.unseen_output = false;
    }

    /// Whether this pane should show a busy indicator at `now`.
    ///
    /// Pure: the caller supplies `now`, so tests pin a fake clock instead of
    /// sleeping. An exited pane is never busy; an ACP turn is authoritative; a
    /// PTY pane is busy only while its output is recent.
    // TODO(via:tab-render): consumed by the busy dot.
    #[allow(dead_code)]
    pub fn is_busy(&self, now: Instant) -> bool {
        if self.exited {
            return false;
        }
        if self.turn.is_some() {
            return true;
        }
        self.output_is_recent(now)
    }

    /// Whether output arrived within [`BUSY_OUTPUT_WINDOW`] of `now`.
    fn output_is_recent(&self, now: Instant) -> bool {
        self.last_output_at
            .is_some_and(|last| now.saturating_duration_since(last) < BUSY_OUTPUT_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fixed instant, reused as the fake "now" for a test.
    ///
    /// Calling `Instant::now()` once per test and doing all arithmetic from that
    /// value keeps the assertions deterministic: elapsed time is always supplied
    /// explicitly (`now + window`), so no test sleeps and none depends on how long
    /// the test body takes to run.
    fn clock() -> Instant {
        Instant::now()
    }

    #[test]
    fn acp_turn_active_reports_busy() {
        let now = clock();
        let mut activity = PaneActivity::new();

        activity.apply_acp_progress(TURN_PROGRESS_ID, true);

        assert_eq!(activity.turn, Some(TurnState::Thinking));
        assert!(activity.is_busy(now), "an open ACP turn must be busy");
    }

    #[test]
    fn acp_turn_cleared_reports_idle() {
        let now = clock();
        let mut activity = PaneActivity::new();
        activity.apply_acp_progress(TURN_PROGRESS_ID, true);

        activity.apply_acp_progress(TURN_PROGRESS_ID, false);

        assert_eq!(activity.turn, None);
        assert!(
            !activity.is_busy(now),
            "a cleared turn is idle even with no clock progress"
        );
    }

    #[test]
    fn pty_recent_output_reports_busy() {
        let now = clock();
        let mut activity = PaneActivity::new();
        activity.mark_output(now, true);

        assert!(activity.is_busy(now), "output at `now` is busy");
        assert!(
            activity.is_busy(now + BUSY_OUTPUT_WINDOW / 2),
            "output inside the window is busy"
        );
    }

    #[test]
    fn pty_stale_output_reports_idle() {
        let now = clock();
        let mut activity = PaneActivity::new();
        activity.mark_output(now, true);

        assert!(
            !activity.is_busy(now + BUSY_OUTPUT_WINDOW),
            "output exactly one window old is stale"
        );
        assert!(
            !activity.is_busy(now + BUSY_OUTPUT_WINDOW * 10),
            "much older output is certainly stale"
        );
    }

    #[test]
    fn pane_with_no_activity_is_idle() {
        assert!(!PaneActivity::new().is_busy(clock()));
    }

    #[test]
    fn unseen_output_tracks_the_active_tab() {
        let now = clock();
        let mut activity = PaneActivity::new();

        // Output on the tab the user is watching is already seen.
        activity.mark_output(now, true);
        assert!(!activity.unseen_output);

        // Output on a background tab is unseen until the user activates it.
        activity.mark_output(now, false);
        assert!(activity.unseen_output);

        activity.mark_seen();
        assert!(!activity.unseen_output);
    }

    #[test]
    fn exited_pane_is_never_busy() {
        let now = clock();
        let mut activity = PaneActivity::new();
        activity.apply_acp_progress(TURN_PROGRESS_ID, true);
        activity.mark_output(now, true);

        activity.mark_exited();

        assert!(activity.exited);
        assert_eq!(activity.turn, None, "exit clears the turn");
        assert!(!activity.is_busy(now), "an exited pane is never busy");
    }

    #[test]
    fn overlapping_tool_calls_keep_the_turn_busy_until_all_finish() {
        let now = clock();
        let mut activity = PaneActivity::new();

        activity.apply_acp_progress("tool-a", true);
        activity.apply_acp_progress("tool-b", true);
        assert_eq!(open_tools(&activity), vec!["tool-a", "tool-b"]);

        // One tool finishing does not idle the pane while another still runs.
        activity.apply_acp_progress("tool-a", false);
        assert_eq!(open_tools(&activity), vec!["tool-b"]);
        assert!(activity.is_busy(now));

        activity.apply_acp_progress("tool-b", false);
        assert_eq!(activity.turn, None);
        assert!(!activity.is_busy(now));
    }

    #[test]
    fn duplicate_progress_for_one_tool_does_not_double_count() {
        let now = clock();
        let mut activity = PaneActivity::new();

        // `src/acp.rs` emits active=true for `tool_call` and again for a later
        // `tool_call_update` with the same toolCallId.
        activity.apply_acp_progress("tool-a", true);
        activity.apply_acp_progress("tool-a", true);
        activity.apply_acp_progress("tool-a", true);

        assert_eq!(
            open_tools(&activity),
            vec!["tool-a"],
            "re-reporting one open call must not stack up copies"
        );

        // A single completion therefore clears it, which a counter could not do.
        activity.apply_acp_progress("tool-a", false);
        assert_eq!(activity.turn, None);
        assert!(!activity.is_busy(now));
    }

    #[test]
    fn a_lone_completion_does_not_clear_an_in_progress_turn() {
        let now = clock();
        let mut activity = PaneActivity::new();
        activity.apply_acp_progress("tool-a", true);

        // A completion for a call we never saw open says nothing about tool-a.
        activity.apply_acp_progress("tool-unknown", false);

        assert_eq!(open_tools(&activity), vec!["tool-a"]);
        assert!(
            activity.is_busy(now),
            "an unrelated completion must not end the turn"
        );
    }

    #[test]
    fn a_lone_completion_leaves_a_thinking_turn_thinking() {
        let now = clock();
        let mut activity = PaneActivity::new();
        activity.apply_acp_progress(TURN_PROGRESS_ID, true);
        assert_eq!(activity.turn, Some(TurnState::Thinking));

        // No tool is open yet, so this completion belongs to nothing we track.
        activity.apply_acp_progress("tool-unknown", false);

        assert_eq!(
            activity.turn,
            Some(TurnState::Thinking),
            "Thinking must survive a completion for an unknown tool"
        );
        assert!(activity.is_busy(now));
    }

    #[test]
    fn turn_stop_clears_tool_state_too() {
        let mut activity = PaneActivity::new();
        activity.apply_acp_progress("tool-a", true);

        // The `__turn` stop is authoritative even if a tool never reported done.
        activity.apply_acp_progress(TURN_PROGRESS_ID, false);

        assert_eq!(activity.turn, None);
    }

    #[test]
    fn tools_reporting_after_thinking_replace_the_turn_state() {
        let mut activity = PaneActivity::new();
        activity.apply_acp_progress(TURN_PROGRESS_ID, true);
        assert_eq!(activity.turn, Some(TurnState::Thinking));

        activity.apply_acp_progress("tool-a", true);

        assert_eq!(open_tools(&activity), vec!["tool-a"]);
    }

    /// Open tool ids, sorted, for compact assertions.
    fn open_tools(activity: &PaneActivity) -> Vec<&str> {
        match &activity.turn {
            Some(TurnState::Tools { open }) => open.iter().map(String::as_str).collect(),
            other => panic!("expected an open tool set, got {other:?}"),
        }
    }
}
