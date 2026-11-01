//! The I/O loop: one thread polls every PTY (ADR-0007).
//!
//! Panes get a fair budget per pass, so a flooding pane cannot starve the others. What it
//! does not get read stays in the kernel's PTY buffer, and once that fills, its writer
//! blocks. That is the backpressure.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use pinfold_sys::pty::Pty;

use crate::paste;
use crate::vt::{AlacrittyEngine, VtEngine};

pub use mio::Waker;

/// Most bytes read from one pane per pass.
pub const READ_BUDGET: usize = 64 * 1024;
/// Most reply bytes held for a program that is not reading them. The rest are dropped.
pub const MAX_REPLIES: usize = 4 * 1024;
/// Most input waiting for a program that is not reading it. Input past this is refused
/// whole, since a paste cut short would lose its closing bracket.
pub const MAX_INPUT: usize = 1024 * 1024;
/// Queue capacity an empty pane keeps, so one large paste does not cost memory for good.
const KEPT_INPUT: usize = 4 * 1024;

const EVENT_CAPACITY: usize = 64;
/// Beyond any pane number, so a wake-up is never mistaken for PTY activity.
const WAKE: Token = Token(usize::MAX);

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
    fed: &mut bool,
) -> Pumped {
    let mut left = READ_BUDGET;
    while left > 0 {
        let size = left.min(buffer.len());
        match source.read(&mut buffer[..size]) {
            Ok(0) => return Pumped::Closed,
            Ok(count) => {
                left -= count;
                *fed = true;
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
    input: Vec<u8>,
    readable: bool,
    closed: bool,
    dirty: bool,
}

impl Pane {
    fn flush(&mut self) {
        flush(&mut self.pty.master(), &mut self.replies);
        flush(&mut self.pty.master(), &mut self.input);
        if self.input.is_empty() && self.input.capacity() > KEPT_INPUT {
            self.input = Vec::new();
        }
    }
}

/// Every PTY and its terminal state, driven from a single thread.
pub struct Reactor {
    poll: Poll,
    waker: Arc<Waker>,
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
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        Ok(Self {
            poll,
            waker,
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
            input: Vec::new(),
            readable: true,
            closed: false,
            dirty: true,
        });
        Ok(pane)
    }

    /// Waits for PTY activity for up to `timeout`, then gives each active pane one budget.
    ///
    /// A synchronized update that outlives its deadline ends the wait early and is shown.
    ///
    /// # Errors
    ///
    /// Returns an error if polling fails.
    pub fn turn(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let unfinished = self.panes.iter().any(|pane| pane.readable);
        let now = Instant::now();
        let deadline = self
            .panes
            .iter()
            .filter_map(|pane| pane.engine.sync_deadline())
            .min()
            .map(|deadline| deadline.saturating_duration_since(now));
        let timeout = if unfinished {
            Some(Duration::ZERO)
        } else {
            timeout.into_iter().chain(deadline).min()
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
                    &mut pane.dirty,
                );
                pane.readable = pumped == Pumped::Budget;
                pane.closed |= pumped == Pumped::Closed;
            }
            if pane
                .engine
                .sync_deadline()
                .is_some_and(|due| due <= Instant::now())
            {
                pane.engine.end_sync(&mut pane.replies);
                pane.replies.truncate(MAX_REPLIES);
                pane.dirty = true;
            }
            pane.flush();
        }
        Ok(())
    }

    /// Returns a handle that makes a blocked [`Reactor::turn`] return, from any thread.
    #[must_use]
    pub fn waker(&self) -> Arc<Waker> {
        Arc::clone(&self.waker)
    }

    /// Queues bytes for a pane's program, such as keyboard input. Returns false if the queue
    /// has no room for all of them, or the pane is unknown, and then queues none.
    pub fn write(&mut self, pane: usize, bytes: &[u8]) -> bool {
        let Some(pane) = self.panes.get_mut(pane) else {
            return false;
        };
        if pane.input.len() + bytes.len() > MAX_INPUT {
            return false;
        }
        pane.input.extend_from_slice(bytes);
        pane.flush();
        true
    }

    /// Queues pasted text for a pane's program, sanitized. Returns what [`Reactor::write`]
    /// returns.
    pub fn paste(&mut self, pane: usize, text: &str) -> bool {
        let Some(engine) = self.panes.get(pane).map(|pane| &pane.engine) else {
            return false;
        };
        let bracketed = engine.bracketed_paste();
        text.len() <= MAX_INPUT && self.write(pane, &paste::encode(text, bracketed))
    }

    /// Resizes a pane's terminal and tells its program.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY rejects the new size.
    pub fn resize(&mut self, pane: usize, rows: u16, columns: u16) -> io::Result<()> {
        let Some(pane) = self.panes.get_mut(pane) else {
            return Ok(());
        };
        pane.engine.resize(rows, columns);
        let (rows, columns) = pane.engine.size();
        pane.dirty = true;
        pane.pty.resize(rows, columns)
    }

    /// Returns whether a pane changed since the last call, and clears that.
    pub fn take_dirty(&mut self, pane: usize) -> bool {
        self.panes
            .get_mut(pane)
            .is_some_and(|pane| std::mem::take(&mut pane.dirty))
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
        let mut fed = false;
        let pumped = pump(
            &mut source,
            &mut engine,
            &mut replies,
            &mut buffer,
            &mut fed,
        );
        assert_eq!(pumped, Pumped::Budget);
        assert!(fed);
    }

    #[test]
    fn replies_are_capped() {
        let (mut engine, mut replies) = (AlacrittyEngine::new(4, 20), Vec::new());
        let mut buffer = vec![0; READ_BUDGET];
        let bytes = b"\x1b[6n".repeat(10_000);
        let mut source = bytes.as_slice();
        pump(
            &mut source,
            &mut engine,
            &mut replies,
            &mut buffer,
            &mut false,
        );
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
        let mut reactor = Reactor::new().unwrap();
        assert!(reactor.engine(9).is_none());
        assert!(reactor.is_closed(9));
        assert!(!reactor.take_dirty(9));
        reactor.write(9, b"x");
        reactor.resize(9, 24, 80).unwrap();
    }

    #[test]
    fn input_reaches_the_program() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor
            .spawn(shell("read line; echo got-$line; sleep 30"), 24, 80)
            .unwrap();
        reactor.write(pane, b"key\n");
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "got-key")));
    }

    #[test]
    fn a_large_write_reaches_the_program_whole() {
        let mut reactor = Reactor::new().unwrap();
        let script = "stty raw -echo; echo ready; head -c 300000 | wc -c | sed 's/ //g; s/^/got-/'; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "ready")));
        assert!(reactor.write(pane, &vec![b'x'; 300_000]));
        assert!(turn_until(&mut reactor, |r| on_screen(
            r,
            pane,
            "got-300000"
        )));
        assert_eq!(reactor.panes[pane].input.capacity(), 0);
    }

    #[test]
    fn input_past_the_limit_is_refused_whole() {
        let mut reactor = Reactor::new().unwrap();
        let script = "stty raw -echo; echo ready; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "ready")));
        assert!(reactor.write(pane, &vec![b'x'; MAX_INPUT]));
        let queued = reactor.panes[pane].input.len();
        assert!(queued > 0, "the kernel took all of it");
        let room = MAX_INPUT - queued;
        assert!(!reactor.write(pane, &vec![b'x'; room + 1]));
        assert_eq!(reactor.panes[pane].input.len(), queued);
        assert!(!reactor.paste(pane, &"x".repeat(MAX_INPUT + 1)));
    }

    #[test]
    fn a_paste_is_sanitized_and_bracketed_as_the_program_asked() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"stty raw -echo; printf '\033[?2004h'; dd bs=1 count=20 2>/dev/null | od -An -c | tr -d ' \n' | sed 's/^/got-/'; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        turn_until(&mut reactor, |r| r.engine(pane).unwrap().bracketed_paste());
        assert!(reactor.paste(pane, "ab\x1b[201~c"));
        let expected = r"got-033[200~ab[201~c033[201~";
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, expected)));
    }

    #[test]
    fn resize_reaches_the_terminal_and_program() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor
            .spawn(shell("read line; stty size; sleep 30"), 24, 80)
            .unwrap();
        reactor.resize(pane, 30, 100).unwrap();
        assert_eq!(reactor.engine(pane).unwrap().size(), (30, 100));
        reactor.write(pane, b"\n");
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "30 100")));
    }

    #[test]
    fn output_marks_a_pane_dirty_once() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor
            .spawn(shell("read line; echo hello; sleep 30"), 24, 80)
            .unwrap();
        assert!(reactor.take_dirty(pane));
        assert!(!reactor.take_dirty(pane));
        reactor.write(pane, b"\n");
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "hello")));
        assert!(reactor.take_dirty(pane));
        assert!(!reactor.take_dirty(pane));
    }

    #[test]
    fn a_synchronized_update_that_never_ends_still_shows_up() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"printf '\033[?2026hhidden'; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        reactor.take_dirty(pane);
        let start = Instant::now();
        while !on_screen(&reactor, pane, "hidden") && start.elapsed() < Duration::from_secs(10) {
            reactor.turn(Some(Duration::from_secs(20))).unwrap();
        }
        assert!(on_screen(&reactor, pane, "hidden"));
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(reactor.take_dirty(pane));
    }

    #[test]
    fn a_waker_ends_a_blocked_turn() {
        let mut reactor = Reactor::new().unwrap();
        reactor.turn(Some(Duration::ZERO)).unwrap();
        let waker = reactor.waker();
        let wake = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker.wake().unwrap();
        });
        let start = Instant::now();
        reactor.turn(Some(Duration::from_secs(20))).unwrap();
        assert!(start.elapsed() < Duration::from_secs(10));
        wake.join().unwrap();
    }
}
