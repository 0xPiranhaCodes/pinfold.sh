//! Parked `pane.wait` requests (ADR-0032 point 4). Each waits on the reactor's thread for a
//! change in one pane, or its deadline, and holds no thread or buffer while it waits.
//!
//! ```
//! use std::time::{Duration, Instant};
//!
//! use pinfold_core::io::Reactor;
//! use pinfold_core::wait::{Outcome, Until, Waits};
//!
//! let mut reactor = Reactor::new()?;
//! let pane = reactor.spawn(std::process::Command::new("cat"), 24, 80)?;
//! let mut waits = Waits::default();
//! let now = Instant::now();
//! assert!(waits.park(&reactor, "caller", pane, Until::Exit, now).is_ok());
//! assert_eq!(waits.settle(&reactor, now), [("caller", Outcome::Timeout)]);
//! # Ok::<(), std::io::Error>(())
//! ```

use std::time::{Duration, Instant};

use regex_lite::{Regex, RegexBuilder};

use crate::io::{Progress, Reactor};
use crate::tap::{MAX_TAPPED, keep_newest};

/// Most waits parked at once. Past it a wait is refused.
pub const MAX_WAITS: usize = 64;
/// Most heap a compiled pattern may take. A pattern that needs more is refused. Matching costs
/// time per byte in proportion to this, so it bounds one try of [`MAX_TAIL`] bytes to about
/// 5 ms on a release build, even for the worst pattern that fits (#195).
const COMPILED: usize = 8 * 1024;
/// Most of a line a match tries, the newest.
const MAX_TAIL: usize = 4096;
/// Time one settle may spend trying lines across every wait. Lines past it wait for the next
/// turn, which comes at once, so one pane's output cannot stall the reactor for every pane.
pub const MATCH_BUDGET: Duration = Duration::from_millis(4);

/// What ends a wait, besides its pane closing or its deadline.
#[derive(Clone, Debug)]
pub enum Until {
    /// The shell finishes a command.
    Exit,
    /// The shell draws its next prompt.
    Prompt,
    /// The program writes anything.
    Output,
    /// A line printed after the wait began matches, as plain text without escapes. Each
    /// finished line is tried once and the line being printed once a settle if it grew, each
    /// on its newest 4 KiB. Under a flood of output a wait keeps the newest 64 KiB untried.
    Match(Regex),
    /// The program writes nothing for this long.
    Idle(Duration),
}

impl Until {
    /// Compiles `pattern` for [`Until::Match`]. Matching takes time linear in the text, and
    /// `\w`, `\d`, `\s` and case folding are ASCII only.
    ///
    /// # Errors
    ///
    /// Returns why the pattern is refused: it is not a valid pattern, or it compiles too large.
    ///
    /// # Examples
    ///
    /// ```
    /// use pinfold_core::wait::Until;
    ///
    /// assert!(Until::matching(r"^done \d+$").is_ok());
    /// assert!(Until::matching("(").is_err());
    /// assert!(Until::matching(r"\w{4000}").is_err());
    /// ```
    pub fn matching(pattern: &str) -> Result<Self, String> {
        let mut builder = RegexBuilder::new(pattern);
        builder.size_limit(COMPILED);
        builder
            .build()
            .map(Self::Match)
            .map_err(|_| "not a valid pattern, or too large".to_owned())
    }
}

/// How a wait ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A command finished, with the exit status the shell gave, if any.
    Exit(Option<u8>),
    /// A prompt was drawn.
    Prompt,
    /// The program wrote output.
    Output,
    /// This line matched.
    Matched(String),
    /// The program was quiet for the time asked.
    Idle,
    /// The pane closed.
    Closed,
    /// The deadline passed first.
    Timeout,
}

struct Parked<T> {
    ticket: T,
    pane: usize,
    until: Until,
    /// The pane when the wait began, so only a later change ends it.
    from: Progress,
    deadline: Instant,
    /// For a match, the text not yet tried (finished lines, then the line being printed),
    /// whether that last line grew since it was last tried, and a line that matched.
    line: String,
    grew: bool,
    matched: Option<String>,
    /// For idle, when the pane last wrote.
    quiet: Instant,
}

/// The parked waits, each with a ticket that says whom to answer.
pub struct Waits<T> {
    parked: Vec<Parked<T>>,
    /// The wait the next settle tries first, so each gets its turn at the budget.
    first: usize,
    /// Whether the last settle ran out of budget with lines left to try.
    behind: bool,
}

impl<T> Default for Waits<T> {
    fn default() -> Self {
        Self {
            parked: Vec::new(),
            first: 0,
            behind: false,
        }
    }
}

impl<T> Waits<T> {
    /// Parks a wait on `pane` until `deadline`.
    ///
    /// # Errors
    ///
    /// Gives `ticket` back past [`MAX_WAITS`] or if the pane has closed.
    pub fn park(
        &mut self,
        reactor: &Reactor,
        ticket: T,
        pane: usize,
        until: Until,
        deadline: Instant,
    ) -> Result<(), T> {
        match reactor.progress(pane) {
            Some(from) if self.parked.len() < MAX_WAITS => {
                self.parked.push(Parked {
                    ticket,
                    pane,
                    until,
                    from,
                    deadline,
                    line: String::new(),
                    grew: false,
                    matched: None,
                    quiet: Instant::now(),
                });
                Ok(())
            }
            _ => Err(ticket),
        }
    }

    /// Drops the waits whose tickets `keep` refuses, as for a connection that closed.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.parked.retain(|parked| keep(&parked.ticket));
    }

    /// Returns the nearest deadline, if any wait is parked. It is now while lines wait to be
    /// tried.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        if self.behind {
            return Some(Instant::now());
        }
        let due = |parked: &Parked<T>| match parked.until {
            Until::Idle(quiet) => parked.deadline.min(parked.quiet + quiet),
            _ => parked.deadline,
        };
        self.parked.iter().map(due).min()
    }

    /// Whether a wait needs the text `pane` prints, from [`Waits::feed`].
    #[must_use]
    pub fn wants_text(&self, pane: usize) -> bool {
        let matches = |parked: &&Parked<T>| matches!(parked.until, Until::Match(..));
        self.parked
            .iter()
            .filter(matches)
            .any(|parked| parked.pane == pane)
    }

    /// Hands the match waits on `pane` the text it printed. The next settle tries it.
    pub fn feed(&mut self, pane: usize, text: &str) {
        for parked in &mut self.parked {
            if let (Until::Match(_), None) = (&parked.until, &parked.matched)
                && parked.pane == pane
            {
                parked.grew |= !text.is_empty() && !text.ends_with('\n');
                parked.line.push_str(text);
                // Trimmed only past twice the cap, so a line fed a byte at a time is not copied
                // each time.
                if parked.line.len() > 2 * MAX_TAPPED {
                    keep_newest(&mut parked.line, MAX_TAPPED);
                }
            }
        }
    }

    /// Tries the lines fed to match waits for up to [`MATCH_BUDGET`], then removes and returns
    /// every wait that has ended by `now`, with how.
    pub fn settle(&mut self, reactor: &Reactor, now: Instant) -> Vec<(T, Outcome)> {
        let end = Instant::now() + MATCH_BUDGET;
        let count = self.parked.len();
        self.first = if count == 0 { 0 } else { self.first % count };
        self.behind = false;
        for step in 0..count {
            let index = (self.first + step) % count;
            if let Some(parked) = self.parked.get_mut(index)
                && !try_lines(parked, end)
            {
                // The waits after this one go first next time.
                self.behind = true;
                self.first = index + 1;
                break;
            }
        }
        let mut settled = Vec::new();
        let mut index = 0;
        while let Some(parked) = self.parked.get_mut(index) {
            match outcome(parked, reactor.progress(parked.pane), now) {
                Some(outcome) => settled.push((self.parked.swap_remove(index).ticket, outcome)),
                None => index += 1,
            }
        }
        settled
    }
}

/// Tries a match wait's finished lines in order, then its unfinished line if it grew, until one
/// matches or `end` passes. Returns false if lines are left to try.
fn try_lines<T>(parked: &mut Parked<T>, end: Instant) -> bool {
    let Until::Match(regex) = &parked.until else {
        return true;
    };
    let mut tried = 0;
    let mut done = true;
    while parked.matched.is_none() {
        if Instant::now() >= end {
            done = !parked.grew && !parked.line[tried..].contains('\n');
            break;
        }
        match parked.line[tried..].find('\n') {
            Some(at) => {
                parked.matched = check(regex, &parked.line[tried..tried + at]);
                tried += at + 1;
            }
            None => {
                if std::mem::take(&mut parked.grew) {
                    parked.matched = check(regex, &parked.line[tried..]);
                }
                break;
            }
        }
    }
    parked.line.drain(..tried);
    done
}

/// Returns `line` if its newest [`MAX_TAIL`] bytes match.
fn check(regex: &Regex, line: &str) -> Option<String> {
    // A terminal's line ends are `\r\n`, or `\r\r\n` from a program that wrote `\r\n`.
    let line = newest(line.trim_end_matches('\r'), MAX_TAIL);
    regex.is_match(line).then(|| line.to_owned())
}

/// The newest `most` bytes of `text`, or a little less, on a character boundary.
fn newest(text: &str, most: usize) -> &str {
    let mut start = text.len().saturating_sub(most);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn outcome<T>(parked: &mut Parked<T>, now_at: Option<Progress>, now: Instant) -> Option<Outcome> {
    if let Some(line) = parked.matched.take() {
        return Some(Outcome::Matched(line));
    }
    let (from, Some(at)) = (parked.from, now_at) else {
        return Some(Outcome::Closed);
    };
    match parked.until {
        Until::Idle(_) if at.output != from.output => (parked.from, parked.quiet) = (at, now),
        Until::Idle(quiet) if now >= parked.quiet + quiet => return Some(Outcome::Idle),
        Until::Exit if at.marks.finished != from.marks.finished => {
            return Some(Outcome::Exit(at.marks.exit));
        }
        Until::Prompt if at.marks.prompts != from.marks.prompts => return Some(Outcome::Prompt),
        Until::Output if at.output != from.output => return Some(Outcome::Output),
        _ => {}
    }
    (now >= parked.deadline).then_some(Outcome::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    /// Turns the reactor until every wait settles, for up to ten seconds, in ticket order.
    fn settled(reactor: &mut Reactor, waits: &mut Waits<u8>) -> Vec<(u8, Outcome)> {
        let (start, mut settled) = (Instant::now(), Vec::new());
        while waits.deadline().is_some() && start.elapsed() < Duration::from_secs(10) {
            // As the core does: tapped before the turn, so no text is missed.
            for pane in 0..4 {
                reactor.tap(pane, waits.wants_text(pane));
            }
            reactor.turn(Some(Duration::from_millis(20))).unwrap();
            for pane in 0..4 {
                if let Some(text) = reactor.take_text(pane) {
                    waits.feed(pane, &text);
                }
            }
            settled.extend(waits.settle(reactor, Instant::now()));
        }
        settled.sort_by_key(|(ticket, _)| *ticket);
        settled
    }

    #[test]
    fn a_wait_ends_on_its_mark_with_the_exit_status_and_on_close() {
        let mut reactor = Reactor::new().unwrap();
        let later = Instant::now() + Duration::from_secs(30);
        let script = r"stty -echo; read line; printf '\033]133;D;7\007\033]133;A\007'; read line";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        let mut waits = Waits::default();
        for (ticket, until) in [(0, Until::Exit), (1, Until::Prompt), (2, Until::Output)] {
            assert!(waits.park(&reactor, ticket, pane, until, later).is_ok());
        }
        assert!(reactor.write(pane, b"\n"));
        let ended = settled(&mut reactor, &mut waits);
        let expected = [Outcome::Exit(Some(7)), Outcome::Prompt, Outcome::Output];
        assert_eq!(
            ended,
            [0, 1, 2].map(|ticket| (ticket, expected[ticket as usize].clone()))
        );

        assert!(waits.park(&reactor, 3, pane, Until::Exit, later).is_ok());
        assert!(reactor.write(pane, b"\n"));
        assert_eq!(settled(&mut reactor, &mut waits), [(3, Outcome::Closed)]);
        assert!(waits.park(&reactor, 4, pane, Until::Exit, later).is_err());
    }

    #[test]
    fn a_match_takes_a_later_line_or_the_unfinished_one_and_idle_waits_for_quiet() {
        let mut reactor = Reactor::new().unwrap();
        let later = Instant::now() + Duration::from_secs(30);
        let script = r"stty -echo; echo ok 1; read line; printf 'ok 2\r\nok 3\nask> '; sleep 1";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        let mut waits = Waits::default();
        let until = |pattern| Until::matching(pattern).unwrap();
        // Printed before the waits began, "ok 1" matches neither.
        while !reactor
            .read(pane, None)
            .unwrap_or_default()
            .contains("ok 1")
        {
            reactor.turn(Some(Duration::from_millis(20))).unwrap();
        }
        assert!(
            waits
                .park(&reactor, 0, pane, until(r"^ok \d$"), later)
                .is_ok()
        );
        assert!(waits.park(&reactor, 1, pane, until("ask>"), later).is_ok());
        let idle = Until::Idle(Duration::from_millis(300));
        assert!(waits.park(&reactor, 2, pane, idle, later).is_ok());
        assert!(waits.deadline() < Some(later));
        let start = Instant::now();
        assert!(reactor.write(pane, b"\n"));
        let ended = settled(&mut reactor, &mut waits);
        let matched = |line: &str| Outcome::Matched(line.to_owned());
        assert_eq!(
            ended,
            [
                (0, matched("ok 2")),
                (1, matched("ask> ")),
                (2, Outcome::Idle)
            ]
        );
        assert!(start.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn a_pattern_too_large_is_refused_and_a_line_keeps_its_newest_bytes() {
        assert!(Until::matching(r"(?i)\w+ \d+$").is_ok());
        assert!(Until::matching(r"\w{100}x").is_ok());
        let refused = Until::matching(r"\w{200}").unwrap_err();
        assert_eq!(refused, "not a valid pattern, or too large");
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let mut waits = Waits::default();
        let later = Instant::now() + Duration::from_secs(30);
        let until = Until::matching("^b").unwrap();
        assert!(waits.park(&reactor, 0, pane, until, later).is_ok());
        waits.feed(pane, &"a".repeat(2 * MAX_TAPPED + 1));
        assert_eq!(waits.parked[0].line.len(), MAX_TAPPED);
        waits.feed(pane, "a\nb");
        let settled = waits.settle(&reactor, Instant::now());
        assert_eq!(settled, [(0, Outcome::Matched("b".to_owned()))]);
    }

    /// #195: the worst pattern the limit takes, on the most waits, fed long lines.
    #[test]
    fn matching_long_lines_keeps_each_settle_within_the_budget() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let mut waits = Waits::default();
        let later = Instant::now() + Duration::from_secs(30);
        let worst = (1..)
            .map(|n| format!(r"\w{{{n}}}x"))
            .take_while(|pattern| Until::matching(pattern).is_ok())
            .last()
            .unwrap();
        for ticket in 0..MAX_WAITS {
            let until = Until::matching(&worst).unwrap();
            assert!(waits.park(&reactor, ticket, pane, until, later).is_ok());
        }
        // One try of the newest MAX_TAIL bytes is the most a settle can run past the budget.
        let regex = Regex::new(&worst).unwrap();
        let start = Instant::now();
        assert!(check(&regex, &"a".repeat(MAX_TAIL)).is_none());
        let one = start.elapsed();
        // Slack for a loaded test machine; with no budget a settle ran all 256 tries.
        let bound = MATCH_BUDGET + 4 * one;
        let line = "a".repeat(MAX_TAPPED - 1) + "\n";
        let (mut slowest, mut turns) = (Duration::ZERO, 0);
        for _ in 0..4 {
            waits.feed(pane, &line);
        }
        loop {
            let start = Instant::now();
            assert_eq!(waits.settle(&reactor, start), []);
            slowest = slowest.max(start.elapsed());
            turns += 1;
            if waits.deadline() > Some(Instant::now()) {
                break;
            }
        }
        eprintln!("one try {one:?}, slowest settle {slowest:?} of {turns}, bound {bound:?}");
        assert!(slowest <= bound, "{slowest:?} > {bound:?}");
        assert!(turns > 1);
        // Normal lines still match once the backlog is tried.
        waits.feed(pane, &format!("{}x\n", "a".repeat(200)));
        let mut settled = 0;
        while settled < MAX_WAITS && start.elapsed() < Duration::from_secs(60) {
            settled += waits.settle(&reactor, Instant::now()).len();
        }
        assert_eq!(settled, MAX_WAITS);
    }

    #[test]
    fn a_long_unfinished_line_fed_a_byte_at_a_time_costs_little() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let mut waits = Waits::default();
        let later = Instant::now() + Duration::from_secs(30);
        let until = Until::matching("b").unwrap();
        assert!(waits.park(&reactor, 0, pane, until, later).is_ok());
        let start = Instant::now();
        for _ in 0..1024 * 1024 {
            waits.feed(pane, "a");
        }
        assert_eq!(waits.settle(&reactor, Instant::now()), []);
        // About 50 ms here; the old rescan of the whole line took minutes, even optimized.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        waits.feed(pane, "b");
        let settled = waits.settle(&reactor, Instant::now());
        assert_eq!(
            settled,
            [(0, Outcome::Matched("a".repeat(MAX_TAIL - 1) + "b"))]
        );
    }

    #[test]
    fn a_quiet_pane_times_out() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let mut waits = Waits::default();
        let soon = Instant::now() + Duration::from_millis(200);
        assert!(waits.park(&reactor, 0, pane, Until::Exit, soon).is_ok());
        assert_eq!(waits.deadline(), Some(soon));
        assert_eq!(settled(&mut reactor, &mut waits), [(0, Outcome::Timeout)]);
        assert!(Instant::now() >= soon);
        assert_eq!(waits.deadline(), None);
    }

    #[test]
    fn past_the_cap_a_wait_is_refused_until_one_is_dropped() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let mut waits = Waits::default();
        let later = Instant::now() + Duration::from_secs(30);
        for ticket in 0..MAX_WAITS {
            assert!(
                waits
                    .park(&reactor, ticket, pane, Until::Exit, later)
                    .is_ok()
            );
        }
        let refused = waits.park(&reactor, MAX_WAITS, pane, Until::Exit, later);
        assert_eq!(refused, Err(MAX_WAITS));
        // As when the first caller's connection closes.
        waits.retain(|ticket| *ticket != 0);
        assert!(
            waits
                .park(&reactor, MAX_WAITS, pane, Until::Exit, later)
                .is_ok()
        );
    }
}
