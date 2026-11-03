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

use std::time::Instant;

use crate::io::{Progress, Reactor};

/// Most waits parked at once. Past it a wait is refused.
pub const MAX_WAITS: usize = 64;

/// What ends a wait, besides its pane closing or its deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    /// The shell finishes a command.
    Exit,
    /// The shell draws its next prompt.
    Prompt,
    /// The program writes anything.
    Output,
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A command finished, with the exit status the shell gave, if any.
    Exit(Option<u8>),
    /// A prompt was drawn.
    Prompt,
    /// The program wrote output.
    Output,
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
        self.parked.iter().map(|parked| parked.deadline).min()
    }

    /// Removes and returns every wait that has ended by `now`, with how.
    pub fn settle(&mut self, reactor: &Reactor, now: Instant) -> Vec<(T, Outcome)> {
        let mut settled = Vec::new();
        let mut index = 0;
        while let Some(parked) = self.parked.get(index) {
            match outcome(parked, reactor.progress(parked.pane), now) {
                Some(outcome) => settled.push((self.parked.swap_remove(index).ticket, outcome)),
                None => index += 1,
            }
        }
        settled
    }
}

fn outcome<T>(parked: &Parked<T>, now_at: Option<Progress>, now: Instant) -> Option<Outcome> {
    let (from, Some(at)) = (parked.from, now_at) else {
        return Some(Outcome::Closed);
    };
    match parked.until {
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
            reactor.turn(Some(Duration::from_millis(20))).unwrap();
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
            [0, 1, 2].map(|ticket| (ticket, expected[ticket as usize]))
        );

        assert!(waits.park(&reactor, 3, pane, Until::Exit, later).is_ok());
        assert!(reactor.write(pane, b"\n"));
        assert_eq!(settled(&mut reactor, &mut waits), [(3, Outcome::Closed)]);
        assert!(waits.park(&reactor, 4, pane, Until::Exit, later).is_err());
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
