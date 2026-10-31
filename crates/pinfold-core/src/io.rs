//! The I/O loop: one thread polls every PTY (ADR-0007).
//!
//! Panes get a fair budget per pass, so a flooding pane cannot starve the others. What it
//! does not get read stays in the kernel's PTY buffer, and once that fills, its writer
//! blocks. That is the backpressure.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::Duration;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use pinfold_sys::pty::Pty;

use crate::vt::{AlacrittyEngine, VtEngine};

/// Most bytes read from one pane per pass.
pub const READ_BUDGET: usize = 64 * 1024;
/// Most reply bytes held for a program that is not reading them. The rest are dropped.
pub const MAX_REPLIES: usize = 4 * 1024;

const EVENT_CAPACITY: usize = 64;

/// How a pass over one pane ended.
#[derive(Debug, PartialEq)]
enum Pumped {
    /// The budget ran out, so more may be waiting. Edge-triggered polling will not repeat it.
    Budget,
    /// Nothing is left to read.
    Drained,
    /// The child has closed its end.
    Closed,
}

/// Feeds `engine` from `source` until the budget runs out or `source` stops.
fn pump(
    source: &mut impl Read,
    engine: &mut dyn VtEngine,
    replies: &mut Vec<u8>,
    buffer: &mut [u8],
) -> Pumped {
    let mut left = READ_BUDGET;
    while left > 0 {
        let size = left.min(buffer.len());
        match source.read(&mut buffer[..size]) {
            Ok(0) => return Pumped::Closed,
            Ok(count) => {
                left -= count;
                engine.feed(&buffer[..count], replies);
                replies.truncate(MAX_REPLIES);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Pumped::Drained,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            // Linux reports EIO once the child has closed its end.
            Err(_) => return Pumped::Closed,
        }
    }
    Pumped::Budget
}

/// Writes what it can without blocking, and keeps the rest for the next pass.
fn flush(sink: &mut impl Write, replies: &mut Vec<u8>) {
    while !replies.is_empty() {
        match sink.write(replies) {
            Ok(0) => return,
            Ok(count) => {
                replies.drain(..count);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

struct Pane {
    pty: Pty,
    engine: Box<dyn VtEngine + Send>,
    replies: Vec<u8>,
    readable: bool,
    closed: bool,
}

/// Every PTY and its terminal state, driven from a single thread.
pub struct Reactor {
    poll: Poll,
    events: Events,
    panes: Vec<Pane>,
    buffer: Vec<u8>,
}

impl Reactor {
    /// Creates a reactor with no panes.
    ///
    /// # Errors
    ///
    /// Returns an error if the operating system's poller cannot be created.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            poll: Poll::new()?,
            events: Events::with_capacity(EVENT_CAPACITY),
            panes: Vec::new(),
            buffer: vec![0; READ_BUDGET],
        })
    }

    /// Starts `command` on a new PTY and returns its pane number.
    ///
    /// The reactor's thread makes every spawn, which [`Pty::spawn`] requires.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be created or polled, or the program cannot start.
    pub fn spawn(&mut self, mut command: Command, rows: u16, columns: u16) -> io::Result<usize> {
        command.env("TERM", "xterm-256color");
        let pty = Pty::spawn(command, rows, columns)?;
        let pane = self.panes.len();
        let fd = pty.master().as_raw_fd();
        let interest = Interest::READABLE | Interest::WRITABLE;
        self.poll
            .registry()
            .register(&mut SourceFd(&fd), Token(pane), interest)?;
        self.panes.push(Pane {
            pty,
            engine: Box::new(AlacrittyEngine::new(rows, columns)),
            replies: Vec::new(),
            readable: true,
            closed: false,
        });
        Ok(pane)
    }

    /// Waits for PTY activity for up to `timeout`, then gives each active pane one budget.
    ///
    /// # Errors
    ///
    /// Returns an error if polling fails.
    pub fn turn(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let unfinished = self.panes.iter().any(|pane| pane.readable);
        let timeout = if unfinished {
            Some(Duration::ZERO)
        } else {
            timeout
        };
        match self.poll.poll(&mut self.events, timeout) {
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(()),
            result => result?,
        }
        for event in &self.events {
            if let Some(pane) = self.panes.get_mut(event.token().0) {
                pane.readable |= !pane.closed && (event.is_readable() || event.is_read_closed());
            }
        }
        for pane in &mut self.panes {
            if pane.readable {
                let mut master = pane.pty.master();
                let pumped = pump(
                    &mut master,
                    pane.engine.as_mut(),
                    &mut pane.replies,
                    &mut self.buffer,
                );
                pane.readable = pumped == Pumped::Budget;
                pane.closed |= pumped == Pumped::Closed;
            }
            flush(&mut pane.pty.master(), &mut pane.replies);
        }
        Ok(())
    }

    /// Returns a pane's terminal.
    #[must_use]
    pub fn engine(&self, pane: usize) -> Option<&(dyn VtEngine + Send)> {
        self.panes.get(pane).map(|pane| pane.engine.as_ref())
    }

    /// Returns whether a pane's child has closed the PTY. Unknown panes count as closed.
    #[must_use]
    pub fn is_closed(&self, pane: usize) -> bool {
        self.panes.get(pane).is_none_or(|pane| pane.closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    fn turn_until(reactor: &mut Reactor, done: impl Fn(&Reactor) -> bool) -> bool {
        let start = Instant::now();
        while !done(reactor) && start.elapsed() < Duration::from_secs(10) {
            reactor.turn(Some(Duration::from_millis(50))).unwrap();
        }
        done(reactor)
    }

    fn on_screen(reactor: &Reactor, pane: usize, text: &str) -> bool {
        let engine = reactor.engine(pane).unwrap();
        (0..24).any(|line| engine.line_text(line).contains(text))
    }

    #[test]
    fn an_endless_source_stops_at_the_budget() {
        let (mut engine, mut replies) = (AlacrittyEngine::new(4, 20), Vec::new());
        let mut buffer = vec![0; READ_BUDGET];
        let mut source = io::repeat(b'x');
        let pumped = pump(&mut source, &mut engine, &mut replies, &mut buffer);
        assert_eq!(pumped, Pumped::Budget);
    }

    #[test]
    fn replies_are_capped() {
        let (mut engine, mut replies) = (AlacrittyEngine::new(4, 20), Vec::new());
        let mut buffer = vec![0; READ_BUDGET];
        let bytes = b"\x1b[6n".repeat(10_000);
        let mut source = bytes.as_slice();
        pump(&mut source, &mut engine, &mut replies, &mut buffer);
        assert_eq!(replies.len(), MAX_REPLIES);
    }

    #[test]
    fn a_shell_runs_headlessly() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("echo $TERM; exit 0"), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| r.is_closed(pane)));
        assert!(on_screen(&reactor, pane, "xterm-256color"));
    }

    #[test]
    fn the_terminal_answers_a_program() {
        let script = r"stty raw -echo; printf '\033[6n'; dd bs=1 count=6 2>/dev/null | od -c | sed 's/^/got /'; sleep 30";
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "got")));
    }

    #[test]
    fn a_flooding_pane_does_not_starve_another() {
        let mut reactor = Reactor::new().unwrap();
        for _ in 0..3 {
            reactor.spawn(Command::new("yes"), 24, 80).unwrap();
        }
        let quiet = reactor
            .spawn(shell("sleep 0.2; echo arrived; sleep 30"), 24, 80)
            .unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, quiet, "arrived")));
    }

    #[test]
    fn an_idle_reactor_does_not_spin() {
        let mut reactor = Reactor::new().unwrap();
        reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let (start, mut turns) = (Instant::now(), 0);
        while start.elapsed() < Duration::from_millis(300) {
            reactor.turn(Some(Duration::from_millis(100))).unwrap();
            turns += 1;
        }
        assert!(turns < 20, "{turns} turns in 300 ms");
    }

    #[test]
    fn unknown_panes_are_handled() {
        let reactor = Reactor::new().unwrap();
        assert!(reactor.engine(9).is_none());
        assert!(reactor.is_closed(9));
    }
}
