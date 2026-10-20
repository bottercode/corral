//! Agent state detection.
//!
//! Each pane has a [`Detector`]: a small state machine fed with output and
//! input timestamps, the verdict of the screen rules, focus changes and
//! hook reports. All time comes in as arguments so tests can drive it with a
//! fake clock.

pub mod agents;
pub mod rules;

use std::time::{Duration, Instant};

use crate::protocol::AgentState;

/// Output within this window means the agent is working.
pub const WORKING_WINDOW: Duration = Duration::from_millis(1500);
/// Output this long after a hook report means the hook is stale.
pub const HOOK_OUTPUT_EXPIRY: Duration = Duration::from_secs(60);
/// A `working` hook with no output for this long is stale (e.g. the user
/// interrupted the agent and no Stop hook fired).
pub const HOOK_WORKING_QUIET_EXPIRY: Duration = Duration::from_secs(10);
/// Output right after a resize is a repaint, not work.
pub const QUIET_AFTER_RESIZE: Duration = Duration::from_millis(1000);
/// Output right after input is mostly echo, not work.
pub const QUIET_AFTER_INPUT: Duration = Duration::from_millis(500);

/// What the screen rules say about the visible screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Verdict {
    #[default]
    None,
    Working,
    Blocked,
}

#[derive(Debug, Clone, Copy)]
struct Hook {
    state: AgentState,
    at: Instant,
}

#[derive(Debug, Clone)]
pub struct Detector {
    state: AgentState,
    last_output: Option<Instant>,
    quiet_until: Option<Instant>,
    hook: Option<Hook>,
    verdict: Verdict,
    focused: bool,
    exited: bool,
    /// Working (or blocked) since the agent last went quiet.
    active: bool,
    /// Finished work the user hasn't looked at yet.
    unseen: bool,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Self {
        Detector {
            state: AgentState::Idle,
            last_output: None,
            quiet_until: None,
            hook: None,
            verdict: Verdict::None,
            focused: false,
            exited: false,
            active: false,
            unseen: false,
        }
    }

    pub fn state(&self) -> AgentState {
        self.state
    }

    pub fn on_output(&mut self, now: Instant) {
        if self.quiet_until.is_none_or(|q| now >= q) {
            self.last_output = Some(now);
        }
        if let Some(h) = self.hook
            && now.saturating_duration_since(h.at) >= HOOK_OUTPUT_EXPIRY
        {
            self.hook = None;
        }
    }

    pub fn on_input(&mut self, now: Instant) {
        self.extend_quiet(now + QUIET_AFTER_INPUT);
        // Typing into a pane a hook marked blocked means the user answered.
        if self.hook.is_some_and(|h| h.state == AgentState::Blocked) {
            self.hook = None;
        }
    }

    pub fn on_resize(&mut self, now: Instant) {
        self.extend_quiet(now + QUIET_AFTER_RESIZE);
    }

    fn extend_quiet(&mut self, until: Instant) {
        self.quiet_until = Some(self.quiet_until.map_or(until, |q| q.max(until)));
    }

    /// A hook report; authoritative until the next report or until it goes stale.
    pub fn report(&mut self, state: AgentState, now: Instant) {
        if state == AgentState::Exited {
            return;
        }
        self.hook = Some(Hook { state, at: now });
        match state {
            AgentState::Done => {
                self.active = false;
                self.unseen = !self.focused;
            }
            AgentState::Idle => {
                self.active = false;
                self.unseen = false;
            }
            _ => {}
        }
    }

    pub fn set_verdict(&mut self, verdict: Verdict) {
        self.verdict = verdict;
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused {
            self.unseen = false;
        }
    }

    pub fn set_exited(&mut self) {
        self.exited = true;
    }

    fn hook_state(&mut self, now: Instant) -> Option<AgentState> {
        let h = self.hook?;
        if h.state == AgentState::Working {
            let last = self.last_output.map_or(h.at, |o| o.max(h.at));
            if now.saturating_duration_since(last) >= HOOK_WORKING_QUIET_EXPIRY {
                self.hook = None;
                return None;
            }
        }
        Some(h.state)
    }

    /// Recompute the state. Returns `Some((old, new))` when it changed.
    pub fn evaluate(&mut self, now: Instant) -> Option<(AgentState, AgentState)> {
        let next = self.compute(now);
        let old = self.state;
        self.state = next;
        (old != next).then_some((old, next))
    }

    fn compute(&mut self, now: Instant) -> AgentState {
        if self.exited {
            return AgentState::Exited;
        }
        let raw = match self.hook_state(now) {
            Some(s) => s,
            None => match self.verdict {
                Verdict::Blocked => AgentState::Blocked,
                Verdict::Working => AgentState::Working,
                Verdict::None => {
                    let recent = self
                        .last_output
                        .is_some_and(|t| now.saturating_duration_since(t) < WORKING_WINDOW);
                    if recent {
                        AgentState::Working
                    } else {
                        AgentState::Idle
                    }
                }
            },
        };
        match raw {
            AgentState::Working | AgentState::Blocked => {
                self.active = true;
                raw
            }
            AgentState::Done | AgentState::Idle => {
                if self.active {
                    self.active = false;
                    self.unseen = !self.focused;
                }
                if self.unseen {
                    AgentState::Done
                } else {
                    AgentState::Idle
                }
            }
            AgentState::Exited => AgentState::Exited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentState::*;

    struct Clock(Instant);
    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }
        fn at(&self, ms: u64) -> Instant {
            self.0 + Duration::from_millis(ms)
        }
    }

    #[test]
    fn starts_idle() {
        let c = Clock::new();
        let mut d = Detector::new();
        assert_eq!(d.evaluate(c.at(0)), None);
        assert_eq!(d.state(), Idle);
    }

    #[test]
    fn output_means_working_then_done_when_unfocused() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.on_output(c.at(0));
        assert_eq!(d.evaluate(c.at(100)), Some((Idle, Working)));
        d.on_output(c.at(1000));
        assert_eq!(d.evaluate(c.at(2000)), None);
        assert_eq!(d.evaluate(c.at(2600)), Some((Working, Done)));
        // Stays done until viewed.
        assert_eq!(d.evaluate(c.at(60_000)), None);
        d.set_focused(true);
        assert_eq!(d.evaluate(c.at(60_100)), Some((Done, Idle)));
        // Unfocusing again doesn't resurrect "done".
        d.set_focused(false);
        assert_eq!(d.evaluate(c.at(60_200)), None);
    }

    #[test]
    fn finishing_while_focused_goes_idle() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.set_focused(true);
        d.on_output(c.at(0));
        d.evaluate(c.at(10));
        assert_eq!(d.evaluate(c.at(2000)), Some((Working, Idle)));
    }

    #[test]
    fn screen_verdicts() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.set_verdict(Verdict::Blocked);
        assert_eq!(d.evaluate(c.at(0)), Some((Idle, Blocked)));
        // A spinner keeps it working with no output at all.
        d.set_verdict(Verdict::Working);
        assert_eq!(d.evaluate(c.at(10_000)), Some((Blocked, Working)));
        d.set_verdict(Verdict::None);
        assert_eq!(d.evaluate(c.at(20_000)), Some((Working, Done)));
    }

    #[test]
    fn echo_and_resize_repaints_are_not_work() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.on_input(c.at(0));
        d.on_output(c.at(100));
        assert_eq!(d.evaluate(c.at(150)), None);
        d.on_resize(c.at(1000));
        d.on_output(c.at(1500));
        assert_eq!(d.evaluate(c.at(1600)), None);
        d.on_output(c.at(2100));
        assert_eq!(d.evaluate(c.at(2150)), Some((Idle, Working)));
    }

    #[test]
    fn hooks_override_heuristics() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.report(Blocked, c.at(0));
        d.on_output(c.at(100));
        d.set_verdict(Verdict::Working);
        assert_eq!(d.evaluate(c.at(200)), Some((Idle, Blocked)));
        d.report(Done, c.at(300));
        assert_eq!(d.evaluate(c.at(400)), Some((Blocked, Done)));
        d.report(Working, c.at(500));
        assert_eq!(d.evaluate(c.at(600)), Some((Done, Working)));
    }

    #[test]
    fn hook_expires_after_a_minute_of_output() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.report(Blocked, c.at(0));
        d.on_output(c.at(30_000));
        assert_eq!(d.evaluate(c.at(30_001)), Some((Idle, Blocked)));
        d.on_output(c.at(61_000));
        // Heuristics are back: recent output → working.
        assert_eq!(d.evaluate(c.at(61_001)), Some((Blocked, Working)));
    }

    #[test]
    fn working_hook_goes_stale_when_quiet() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.report(Working, c.at(0));
        assert_eq!(d.evaluate(c.at(5_000)), Some((Idle, Working)));
        assert_eq!(d.evaluate(c.at(10_001)), Some((Working, Done)));
    }

    #[test]
    fn typing_answers_a_blocked_hook() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.report(Blocked, c.at(0));
        d.evaluate(c.at(1));
        d.on_input(c.at(1000));
        // No more hook; output from the answer arrives after the echo window.
        d.on_output(c.at(1600));
        assert_eq!(d.evaluate(c.at(1700)), Some((Blocked, Working)));
    }

    #[test]
    fn done_hook_while_focused_is_idle() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.set_focused(true);
        d.report(Working, c.at(0));
        d.evaluate(c.at(1));
        d.report(Done, c.at(100));
        assert_eq!(d.evaluate(c.at(101)), Some((Working, Idle)));
    }

    #[test]
    fn exit_is_final() {
        let c = Clock::new();
        let mut d = Detector::new();
        d.report(Blocked, c.at(0));
        d.set_exited();
        assert_eq!(d.evaluate(c.at(1)), Some((Idle, Exited)));
        d.report(Working, c.at(2));
        assert_eq!(d.evaluate(c.at(3)), None);
    }
}
