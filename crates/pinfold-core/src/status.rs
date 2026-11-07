//! Per-pane agent status derived from shell marks and explicit API reports.
//!
//! An API report outranks output until the caller clears it, because output can fake a
//! "done" mark and an identified caller cannot (threat model T4). Shell marks still update the
//! fallback status while a report holds, so clearing it reveals the latest shell state.
//!
//! In a pane launched from an agent profile, the agent CLI's own OSC 7501 states and OSC 9
//! notifications outrank its shell's marks, and they alone can say "needs you" from output
//! (ADR-0045). A plain shell pane never reads them.

use std::time::{Duration, Instant};

use crate::marks::{AgentMarks, AgentSignal, ShellMarks, ShellPhase};

/// The minimum time between alerts from one pane.
///
/// ```
/// use pinfold_core::status::ALERT_INTERVAL;
///
/// assert_eq!(ALERT_INTERVAL.as_secs(), 10);
/// ```
pub const ALERT_INTERVAL: Duration = Duration::from_secs(10);

/// The most characters a `pane.status` message may have.
pub const MAX_MESSAGE: usize = 200;

/// The semantic colour used to draw a status.
///
/// ```
/// use pinfold_core::status::{ColourRole, Status};
///
/// assert_eq!(Status::NeedsYou.colour(), ColourRole::Amber);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColourRole {
    /// Attention is required.
    Amber,
    /// Work is in progress.
    Blue,
    /// Work completed successfully.
    Green,
    /// Work failed.
    Red,
    /// Nothing is happening.
    Grey,
}

/// The state shown for an agent pane.
///
/// ```
/// use pinfold_core::status::Status;
///
/// assert_eq!(Status::Running.glyph(), '●');
/// assert_eq!(Status::Running.word(), "running");
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// The agent is waiting for user action.
    NeedsYou,
    /// A command or agent is working.
    Running,
    /// Work completed successfully.
    Done,
    /// Work failed.
    Error,
    /// The pane is at rest.
    #[default]
    Idle,
}

impl Status {
    /// Returns the status glyph.
    ///
    /// ```
    /// use pinfold_core::status::Status;
    ///
    /// assert_eq!(Status::Done.glyph(), '✓');
    /// ```
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::NeedsYou => '◆',
            Self::Running => '●',
            Self::Done => '✓',
            Self::Error => '✕',
            Self::Idle => '○',
        }
    }

    /// Returns the status in words.
    ///
    /// ```
    /// use pinfold_core::status::Status;
    ///
    /// assert_eq!(Status::NeedsYou.word(), "needs you");
    /// ```
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs you",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Idle => "idle",
        }
    }

    /// Returns the status as the API names it.
    ///
    /// ```
    /// use pinfold_core::status::Status;
    ///
    /// assert_eq!(Status::NeedsYou.name(), "needs-you");
    /// assert_eq!(Status::from_name("needs-you"), Some(Status::NeedsYou));
    /// ```
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs-you",
            Self::Running => "running",
            Self::Done => "done",
            Self::Error => "error",
            Self::Idle => "idle",
        }
    }

    /// Reads a name that [`Status::name`] returns.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        [
            Self::NeedsYou,
            Self::Running,
            Self::Done,
            Self::Error,
            Self::Idle,
        ]
        .into_iter()
        .find(|status| status.name() == name)
    }

    /// Returns the semantic colour role for the status.
    ///
    /// ```
    /// use pinfold_core::status::{ColourRole, Status};
    ///
    /// assert_eq!(Status::Error.colour(), ColourRole::Red);
    /// ```
    #[must_use]
    pub const fn colour(self) -> ColourRole {
        match self {
            Self::NeedsYou => ColourRole::Amber,
            Self::Running => ColourRole::Blue,
            Self::Done => ColourRole::Green,
            Self::Error => ColourRole::Red,
            Self::Idle => ColourRole::Grey,
        }
    }
}

/// Returns a reported message without control, bidi or zero-width characters, or state
/// glyphs, so it cannot move, reorder or fake what Pinfold's chrome draws (T4, T6).
///
/// ```
/// use pinfold_core::status::sanitize;
///
/// assert_eq!(sanitize("ok\u{1b}[31m\u{202e}◆"), "ok[31m");
/// ```
#[must_use]
pub fn sanitize(message: &str) -> String {
    message
        .chars()
        .filter(|&character| {
            !matches!(
                character,
                '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{061c}'
                    | '\u{feff}'
                    | '◆'
                    | '●'
                    | '✓'
                    | '✕'
                    | '○'
            ) && !character.is_control()
        })
        .take(MAX_MESSAGE)
        .collect()
}

/// A request for the notification layer to alert about a status transition.
///
/// ```
/// use pinfold_core::status::{Status, Tracker};
/// use std::time::Instant;
///
/// let mut tracker = Tracker::default();
/// let alert = tracker.report(Status::NeedsYou, Instant::now());
/// assert_eq!(alert.map(|value| value.status()), Some(Status::NeedsYou));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alert {
    status: Status,
}

impl Alert {
    /// Returns the status that caused this alert.
    ///
    /// ```
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let mut tracker = Tracker::default();
    /// let alert = tracker.report(Status::Error, Instant::now());
    /// assert_eq!(alert.map(|value| value.status()), Some(Status::Error));
    /// ```
    #[must_use]
    pub const fn status(self) -> Status {
        self.status
    }
}

/// The pure status state machine for one pane.
///
/// ```
/// use pinfold_core::status::{Status, Tracker};
///
/// let tracker = Tracker::default();
/// assert_eq!(tracker.status(), Status::Idle);
/// ```
#[derive(Debug, Default)]
pub struct Tracker {
    status: Status,
    output_status: Status,
    reported: bool,
    observed_finished: u32,
    /// Whether output's agent signals count, which only an agent profile's pane allows.
    agent: bool,
    /// What the agent CLI last signalled, which outranks shell marks until it clears.
    agent_status: Option<Status>,
    observed_signals: u32,
    // Separate limits, so a fake alert printed by output cannot silence a real API one.
    last_api_alert: Option<Instant>,
    last_output_alert: Option<Instant>,
}

impl Tracker {
    /// Returns the status currently shown for the pane.
    ///
    /// ```
    /// use pinfold_core::status::{Status, Tracker};
    ///
    /// assert_eq!(Tracker::default().status(), Status::Idle);
    /// ```
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// Whether the shown status came from output rather than the API, so the window can
    /// label it "reported by output" (threat model T4).
    ///
    /// ```
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let mut tracker = Tracker::default();
    /// assert!(tracker.from_output());
    /// tracker.report(Status::Running, Instant::now());
    /// assert!(!tracker.from_output());
    /// ```
    #[must_use]
    pub const fn from_output(&self) -> bool {
        !self.reported
    }

    /// Applies or replaces an authoritative API report.
    ///
    /// ```
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let mut tracker = Tracker::default();
    /// tracker.report(Status::Running, Instant::now());
    /// assert_eq!(tracker.status(), Status::Running);
    /// ```
    pub fn report(&mut self, status: Status, now: Instant) -> Option<Alert> {
        self.reported = true;
        self.transition(status, now)
    }

    /// Clears API authority and immediately reveals the latest shell-derived status.
    ///
    /// ```
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let now = Instant::now();
    /// let mut tracker = Tracker::default();
    /// tracker.report(Status::Running, now);
    /// tracker.clear_report(now);
    /// assert_eq!(tracker.status(), Status::Idle);
    /// ```
    pub fn clear_report(&mut self, now: Instant) -> Option<Alert> {
        self.reported = false;
        self.transition(self.output(), now)
    }

    /// Makes [`Tracker::observe_agent`] count, for a pane launched from an agent profile
    /// (ADR-0045). Without it a pane keeps to its shell marks, whatever its output prints.
    pub fn read_agent_signals(&mut self) {
        self.agent = true;
    }

    /// Observes what an agent CLI signalled through output. Blocked and a notification say
    /// "needs you", working, done and idle say so, and clear hands back to the shell's marks.
    /// Nothing changes unless [`Tracker::read_agent_signals`] was called.
    ///
    /// ```
    /// use pinfold_core::marks::{AgentMarks, AgentSignal};
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let blocked = AgentMarks { latest: Some(AgentSignal::Blocked), signals: 1 };
    /// let mut shell = Tracker::default();
    /// shell.observe_agent(blocked, Instant::now());
    /// assert_eq!(shell.status(), Status::Idle);
    ///
    /// let mut agent = Tracker::default();
    /// agent.read_agent_signals();
    /// assert!(agent.observe_agent(blocked, Instant::now()).is_some());
    /// assert_eq!(agent.status(), Status::NeedsYou);
    /// assert!(agent.from_output());
    /// ```
    pub fn observe_agent(&mut self, marks: AgentMarks, now: Instant) -> Option<Alert> {
        if !self.agent || marks.signals == self.observed_signals {
            return None;
        }
        self.observed_signals = marks.signals;
        self.agent_status = match marks.latest? {
            AgentSignal::Blocked | AgentSignal::Notified => Some(Status::NeedsYou),
            AgentSignal::Working => Some(Status::Running),
            AgentSignal::Done => Some(Status::Done),
            AgentSignal::Idle => Some(Status::Idle),
            AgentSignal::Clear => None,
        };
        self.show(now)
    }

    /// The person typed into the pane, which answers a prompt, so "needs you" from output
    /// clears. A report from the API stays.
    ///
    /// ```
    /// use pinfold_core::marks::{AgentMarks, AgentSignal};
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let mut tracker = Tracker::default();
    /// tracker.read_agent_signals();
    /// tracker.observe_agent(AgentMarks { latest: Some(AgentSignal::Notified), signals: 1 }, Instant::now());
    /// tracker.typed(Instant::now());
    /// assert_eq!(tracker.status(), Status::Idle);
    /// ```
    pub fn typed(&mut self, now: Instant) -> Option<Alert> {
        if self.agent_status != Some(Status::NeedsYou) {
            return None;
        }
        self.agent_status = None;
        self.show(now)
    }

    /// The status output says: the agent CLI's latest signal, or else the shell's marks.
    fn output(&self) -> Status {
        self.agent_status.unwrap_or(self.output_status)
    }

    fn show(&mut self, now: Instant) -> Option<Alert> {
        if self.reported {
            None
        } else {
            self.transition(self.output(), now)
        }
    }

    /// Observes the latest OSC 133 marks and updates the output-derived fallback.
    ///
    /// A changed `finished` counter wins over the phase because shells normally emit
    /// command-finished and prompt marks together. Done or error then stays shown at the
    /// prompt until the next command runs.
    ///
    /// ```
    /// use pinfold_core::marks::{ShellMarks, ShellPhase};
    /// use pinfold_core::status::{Status, Tracker};
    /// use std::time::Instant;
    ///
    /// let mut tracker = Tracker::default();
    /// tracker.observe_shell(ShellMarks { phase: ShellPhase::Running, ..ShellMarks::default() }, Instant::now());
    /// assert_eq!(tracker.status(), Status::Running);
    /// ```
    pub fn observe_shell(&mut self, marks: ShellMarks, now: Instant) -> Option<Alert> {
        if marks.finished != self.observed_finished {
            self.observed_finished = marks.finished;
            // The command ended, so whatever its agent CLI last said no longer holds.
            self.agent_status = None;
            self.output_status = match marks.exit {
                Some(0) => Status::Done,
                Some(_) => Status::Error,
                None => Status::Idle,
            };
        } else {
            self.output_status = match marks.phase {
                // The prompt after a finish arrives in a later read, and keeps its result shown.
                ShellPhase::Prompt | ShellPhase::Input
                    if matches!(self.output_status, Status::Done | Status::Error) =>
                {
                    self.output_status
                }
                ShellPhase::Prompt | ShellPhase::Input => Status::Idle,
                ShellPhase::Running => Status::Running,
                ShellPhase::Unknown | ShellPhase::Finished => self.output_status,
            };
        }
        self.show(now)
    }

    fn transition(&mut self, status: Status, now: Instant) -> Option<Alert> {
        if self.status == status {
            return None;
        }
        self.status = status;
        if !matches!(status, Status::NeedsYou | Status::Error) {
            return None;
        }
        let last = if self.reported {
            &mut self.last_api_alert
        } else {
            &mut self.last_output_alert
        };
        if last.is_some_and(|last| now.saturating_duration_since(last) < ALERT_INTERVAL) {
            return None;
        }
        *last = Some(now);
        Some(Alert { status })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(phase: ShellPhase, finished: u32, exit: Option<u8>) -> ShellMarks {
        ShellMarks {
            phase,
            finished,
            exit,
            prompts: 0,
        }
    }

    #[test]
    fn shell_marks_transition_through_each_output_state() {
        let now = Instant::now();
        let mut tracker = Tracker::default();

        assert_eq!(tracker.status(), Status::Idle);
        assert_eq!(
            tracker.observe_shell(marks(ShellPhase::Prompt, 0, None), now),
            None
        );
        assert_eq!(
            tracker.observe_shell(marks(ShellPhase::Running, 0, None), now),
            None
        );
        assert_eq!(tracker.status(), Status::Running);
        assert_eq!(
            tracker.observe_shell(marks(ShellPhase::Prompt, 1, Some(0)), now),
            None
        );
        assert_eq!(tracker.status(), Status::Done);
        assert_eq!(
            tracker.observe_shell(marks(ShellPhase::Prompt, 2, Some(1)), now),
            Some(Alert {
                status: Status::Error
            })
        );
        assert_eq!(tracker.status(), Status::Error);
    }

    #[test]
    fn a_finish_stays_shown_at_the_next_prompt_until_a_command_runs() {
        let now = Instant::now();
        let mut tracker = Tracker::default();

        // Shells send `D` and `A` together and `B` in a later read, as recorded sessions show.
        tracker.observe_shell(marks(ShellPhase::Running, 0, None), now);
        tracker.observe_shell(marks(ShellPhase::Prompt, 1, Some(0)), now);
        tracker.observe_shell(marks(ShellPhase::Input, 1, Some(0)), now);
        assert_eq!(tracker.status(), Status::Done);
        tracker.observe_shell(marks(ShellPhase::Running, 1, Some(0)), now);
        assert_eq!(tracker.status(), Status::Running);
        tracker.observe_shell(marks(ShellPhase::Prompt, 2, Some(1)), now);
        tracker.observe_shell(marks(ShellPhase::Input, 2, Some(1)), now);
        assert_eq!(tracker.status(), Status::Error);
        // A finish without a status was never shown as done or failed.
        tracker.observe_shell(marks(ShellPhase::Running, 2, Some(1)), now);
        tracker.observe_shell(marks(ShellPhase::Prompt, 3, None), now);
        tracker.observe_shell(marks(ShellPhase::Input, 3, None), now);
        assert_eq!(tracker.status(), Status::Idle);
    }

    #[test]
    fn api_outranks_output_until_cleared() {
        let now = Instant::now();
        let mut tracker = Tracker::default();

        tracker.report(Status::NeedsYou, now);
        tracker.observe_shell(marks(ShellPhase::Running, 0, None), now);
        // A fake "done" from output an hour later still loses to the report.
        let later = now + Duration::from_secs(3600);
        tracker.observe_shell(marks(ShellPhase::Prompt, 1, Some(0)), later);
        assert_eq!(tracker.status(), Status::NeedsYou);
        assert!(!tracker.from_output());

        tracker.clear_report(later);
        assert_eq!(tracker.status(), Status::Done);
        assert!(tracker.from_output());
    }

    #[test]
    fn a_fake_error_from_output_does_not_silence_an_api_alert() {
        let now = Instant::now();
        let mut tracker = Tracker::default();

        let fake = tracker.observe_shell(marks(ShellPhase::Prompt, 1, Some(1)), now);
        let real = tracker.report(Status::NeedsYou, now + Duration::from_secs(1));

        assert!(fake.is_some());
        assert_eq!(
            real,
            Some(Alert {
                status: Status::NeedsYou
            })
        );
    }

    #[test]
    fn flapping_needs_you_fifty_times_in_one_second_raises_one_alert() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        let mut alerts: usize = 0;

        for step in 0_u32..50 {
            let at = now + Duration::from_millis(u64::from(step) * 20);
            tracker.report(Status::Running, at);
            alerts += usize::from(tracker.report(Status::NeedsYou, at).is_some());
        }

        assert_eq!(alerts, 1);
    }

    #[test]
    fn a_pane_that_needs_you_shows_diamond_in_amber_and_raises_one_alert() {
        let now = Instant::now();
        let mut tracker = Tracker::default();

        let first = tracker.report(Status::NeedsYou, now);
        let repeated = tracker.report(Status::NeedsYou, now);

        assert_eq!(tracker.status().glyph(), '◆');
        assert_eq!(tracker.status().colour(), ColourRole::Amber);
        assert_eq!(
            first,
            Some(Alert {
                status: Status::NeedsYou
            })
        );
        assert_eq!(repeated, None);
    }

    fn signal(latest: AgentSignal, signals: u32) -> AgentMarks {
        AgentMarks {
            latest: Some(latest),
            signals,
        }
    }

    #[test]
    fn an_agent_pane_needs_you_until_its_cli_moves_on_or_the_person_types() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.read_agent_signals();
        tracker.observe_shell(marks(ShellPhase::Running, 0, None), now);

        let first = tracker.observe_agent(signal(AgentSignal::Blocked, 1), now);
        assert_eq!(first.map(Alert::status), Some(Status::NeedsYou));
        assert!(tracker.from_output());
        // Shell marks of the same command do not hide it.
        tracker.observe_shell(marks(ShellPhase::Running, 0, None), now);
        assert_eq!(tracker.status(), Status::NeedsYou);
        tracker.observe_agent(signal(AgentSignal::Working, 2), now);
        assert_eq!(tracker.status(), Status::Running);
        tracker.observe_agent(signal(AgentSignal::Done, 3), now);
        assert_eq!(tracker.status(), Status::Done);

        tracker.observe_agent(signal(AgentSignal::Notified, 4), now);
        assert_eq!(tracker.status(), Status::NeedsYou);
        tracker.typed(now);
        assert_eq!(
            tracker.status(),
            Status::Running,
            "back to the shell's marks"
        );
        tracker.observe_agent(signal(AgentSignal::Blocked, 5), now);
        tracker.observe_agent(signal(AgentSignal::Clear, 6), now);
        assert_eq!(tracker.status(), Status::Running);
        // A finished command ends what its CLI said.
        tracker.observe_agent(signal(AgentSignal::Idle, 7), now);
        tracker.observe_shell(marks(ShellPhase::Prompt, 1, Some(0)), now);
        assert_eq!(tracker.status(), Status::Done);
    }

    #[test]
    fn a_shell_pane_never_needs_you_from_output_and_a_report_outranks_an_agent() {
        let now = Instant::now();
        let mut shell = Tracker::default();
        for count in 1..=1000 {
            assert_eq!(
                shell.observe_agent(signal(AgentSignal::Blocked, count), now),
                None
            );
        }
        assert_eq!(shell.status(), Status::Idle);

        let mut agent = Tracker::default();
        agent.read_agent_signals();
        agent.report(Status::Running, now);
        assert_eq!(
            agent.observe_agent(signal(AgentSignal::Blocked, 1), now),
            None
        );
        assert_eq!(agent.typed(now), None);
        assert_eq!(agent.status(), Status::Running);
        assert!(!agent.from_output());
    }

    #[test]
    fn a_flapping_agent_cli_raises_one_alert_in_ten_seconds() {
        let now = Instant::now();
        let mut tracker = Tracker::default();
        tracker.read_agent_signals();
        let mut alerts: usize = 0;
        for step in 0_u32..1000 {
            let at = now + Duration::from_millis(u64::from(step) * 5);
            let latest = [AgentSignal::Blocked, AgentSignal::Working][step as usize % 2];
            alerts += usize::from(
                tracker
                    .observe_agent(signal(latest, step + 1), at)
                    .is_some(),
            );
        }
        assert_eq!(alerts, 1);
    }

    #[test]
    fn status_presentation_matches_the_contributor_contract() {
        let cases = [
            (Status::NeedsYou, '◆', "needs you", ColourRole::Amber),
            (Status::Running, '●', "running", ColourRole::Blue),
            (Status::Done, '✓', "done", ColourRole::Green),
            (Status::Error, '✕', "error", ColourRole::Red),
            (Status::Idle, '○', "idle", ColourRole::Grey),
        ];

        for (status, glyph, word, colour) in cases {
            assert_eq!(status.glyph(), glyph);
            assert_eq!(status.word(), word);
            assert_eq!(status.colour(), colour);
            assert_eq!(Status::from_name(status.name()), Some(status));
        }
        assert_eq!(Status::from_name("needs you"), None);
    }

    #[test]
    fn a_message_loses_control_bidi_and_glyphs_and_stays_short() {
        let hostile = "a\r\n\u{1b}]0;x\u{7}b\u{202e}c\u{2066}d\u{200b}e✓f\u{9b}g";
        assert_eq!(sanitize(hostile), "a]0;xbcdefg");
        assert_eq!(sanitize(&"é".repeat(300)).chars().count(), MAX_MESSAGE);
    }
}
