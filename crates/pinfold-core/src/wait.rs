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

use regex::{Regex, RegexBuilder};

use crate::io::{Progress, Reactor};
use crate::tap::{MAX_TAPPED, keep_newest};

/// Most waits parked at once. Past it a wait is refused.
pub const MAX_WAITS: usize = 64;
/// Most heap a compiled pattern, and its search cache, may take. A pattern that needs more is
/// refused, so 64 match waits hold at most a few megabytes.
const COMPILED: usize = 64 * 1024;

/// What ends a wait, besides its pane closing or its deadline.
#[derive(Clone, Debug)]
pub enum Until {
    /// The shell finishes a command.
    Exit,
    /// The shell draws its next prompt.
    Prompt,
    /// The program writes anything.
    Output,
    /// A line printed after the wait began matches, as plain text without escapes. The line
    /// being printed is tried as it grows, and at most its newest 64 KiB are kept.
    Match(Regex),
    /// The program writes nothing for this long.
    Idle(Duration),
}

impl Until {
    /// Compiles `pattern` for [`Until::Match`]. Matching takes time linear in the text.
    ///
    /// # Errors
    ///
    /// Returns why the pattern is refused: it is not a regex, or it compiles too large.
    ///
    /// # Examples
    ///
    /// ```
    /// use pinfold_core::wait::Until;
    ///
    /// assert!(Until::matching(r"^done \d+$").is_ok());
    /// assert!(Until::matching("(").is_err());
    /// assert!(Until::matching(r"\w{1000}").is_err());
    /// ```
    pub fn matching(pattern: &str) -> Result<Self, String> {
        let mut builder = RegexBuilder::new(pattern);
        builder.size_limit(COMPILED).dfa_size_limit(COMPILED);
        builder
            .build()
            .map(Self::Match)
            .map_err(|error| match error {
                regex::Error::CompiledTooBig(_) => "the pattern compiles too large".to_owned(),
                _ => "the pattern is not a valid regex".to_owned(),
            })
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
    /// For a match, the line being printed, and a line that matched.
    line: String,
    matched: Option<String>,
    /// For idle, when the pane last wrote.
    quiet: Instant,
}

/// The parked waits, each with a ticket that says whom to answer.
pub struct Waits<T> {
    parked: Vec<Parked<T>>,
}

impl<T> Default for Waits<T> {
    fn default() -> Self {
        Self { parked: Vec::new() }
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

    /// Returns the nearest deadline, if any wait is parked.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        let due = |parked: &Parked<T>| match parked.until {
            Until::Idle(quiet) => parked.deadline.min(parked.quiet + quiet),
            _ => parked.deadline,
        };
        self.parked.iter().map(due).min()
    }

    /// Whether a wait needs the text `pane` prints, from [`Waits::feed`].
    #[must_use]
    pub fn wants_text(&self, pane: usize) -> bool {
        let matches = |parked: &&Parked<T>| matches!(parked.until, Until::Match(_));
        self.parked
            .iter()
            .filter(matches)
            .any(|parked| parked.pane == pane)
    }

    /// Hands the match waits on `pane` the text it printed. A match is settled next.
    pub fn feed(&mut self, pane: usize, text: &str) {
        for parked in &mut self.parked {
            if let (Until::Match(regex), None) = (&parked.until, &parked.matched)
                && parked.pane == pane
            {
                parked.matched = find(regex, &mut parked.line, text);
            }
        }
    }

    /// Removes and returns every wait that has ended by `now`, with how.
    pub fn settle(&mut self, reactor: &Reactor, now: Instant) -> Vec<(T, Outcome)> {
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

/// Adds `text` to the unmatched `line` so far, and returns the first line that matches, the
/// unfinished one last. Keeps the unfinished line's newest [`MAX_TAPPED`] bytes.
fn find(regex: &Regex, line: &mut String, text: &str) -> Option<String> {
    line.push_str(text);
    let unfinished = line.rfind('\n').map_or(0, |end| end + 1);
    let (done, rest) = line.split_at(unfinished);
    let done = done.split_terminator('\n');
    let rest = (!rest.is_empty()).then_some(rest);
    // A terminal's line ends are `\r\n`, or `\r\r\n` from a program that wrote `\r\n`.
    let mut lines = done.chain(rest).map(|each| each.trim_end_matches('\r'));
    let found = lines.find(|each| regex.is_match(each)).map(str::to_owned);
    line.drain(..unfinished);
    keep_newest(line, MAX_TAPPED);
    found
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
        assert!(Until::matching(r"\w+ \d+").is_ok());
        let refused = Until::matching(r"\w{500}").unwrap_err();
        assert_eq!(refused, "the pattern compiles too large");
        let regex = Regex::new("^b").unwrap();
        let mut line = String::new();
        assert_eq!(find(&regex, &mut line, &"a".repeat(2 * MAX_TAPPED)), None);
        assert_eq!(line.len(), MAX_TAPPED);
        assert_eq!(find(&regex, &mut line, "a\nb"), Some("b".to_owned()));
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
