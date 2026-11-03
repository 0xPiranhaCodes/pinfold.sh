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
use pinfold_mux::{Session, Size};
use pinfold_sys::Terminal;
use pinfold_sys::pty::Pty;

use crate::link::{Host, LINK};
use crate::marks::{ShellMarks, ShellPhase};
use crate::protect::Protect;
use crate::search::{
    CopyStep, Copying, Direction, MAX_SELECTION, Position, Search, SearchStep, Selection,
};
use crate::vt::{AlacrittyEngine, VtEngine};
use crate::{paste, shell};

pub use mio::Waker;

/// Most bytes read from one pane per pass.
pub const READ_BUDGET: usize = 64 * 1024;
/// Most reply bytes held for a program that is not reading them. The rest are dropped.
pub const MAX_REPLIES: usize = 4 * 1024;
/// Most input waiting for a program that is not reading it. Input past this is refused
/// whole, since a paste cut short would lose its closing bracket.
pub const MAX_INPUT: usize = pinfold_proto::link::MAX_INPUT;
/// Most text one `pane.read` or `pane.last_output` returns (threat model, Limits).
pub const MAX_READ: usize = MAX_SELECTION;
/// Most `pane.send` calls a second to one pane (threat model, Limits).
pub const SENDS_PER_SECOND: u32 = 20;
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
    /// Never used twice, so a late message cannot reach a pane that took a closed one's place.
    number: usize,
    pty: Pty,
    /// The program it started, for `pane.list`.
    program: String,
    engine: Box<dyn VtEngine + Send>,
    replies: Vec<u8>,
    input: Vec<u8>,
    readable: bool,
    closed: bool,
    dirty: bool,
    /// Counts reads that found output, wrapping, for `pane.wait`.
    output: u64,
    /// The running search, which gets one step per pass, so PTYs are served between steps.
    search: Option<Search>,
    found: Option<Selection>,
    /// When the current second of `pane.send` calls began, and how many it has had.
    sends: (Instant, u32),
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

/// What `pane.list` tells of a pane. The title, directory and status come from output, which
/// can fake them (T4).
#[derive(Debug, PartialEq, Eq)]
pub struct Listed<'a> {
    /// The pane's number.
    pub id: usize,
    /// The last name of `cwd`, as its tab shows it.
    pub title: Option<&'a str>,
    /// The last OSC 7 directory, which is only a hint.
    pub cwd: Option<&'a str>,
    /// The program the pane started, without its arguments.
    pub command: &'a str,
    /// `running`, `done`, `error` or `idle`, from OSC 133 marks, and `done` once it closed.
    pub status: &'static str,
}

/// What `pane.wait` watches in a pane, with nothing from its screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Counts reads that found output, wrapping.
    pub output: u64,
    /// The shell's OSC 133 marks, which output can fake (T4).
    pub marks: ShellMarks,
}

/// Every PTY and its terminal state, driven from a single thread.
pub struct Reactor {
    pub(crate) poll: Poll,
    waker: Arc<Waker>,
    events: Events,
    /// In order of number.
    panes: Vec<Pane>,
    pub(crate) next_pane: usize,
    /// Which panes make up which tabs. A new window finds this as the last one left it.
    pub(crate) session: Session,
    /// The space for panes, once a window has said. It outlives the window, so a pane opened
    /// between windows gets the last size.
    pub(crate) area: Option<Size>,
    /// Counts changes to what a window should show.
    pub(crate) revision: u64,
    /// Starts the shell of each pane a window opens.
    pub(crate) shell: fn() -> Command,
    buffer: Vec<u8>,
    pub(crate) host: Host,
    pub(crate) protect: Protect,
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
            next_pane: 0,
            session: Session::default(),
            area: None,
            revision: 0,
            shell: shell::command,
            buffer: vec![0; READ_BUDGET],
            host: Host::default(),
            protect: Protect::default(),
        })
    }

    /// Starts `command` on a new PTY and returns its pane number. The pane is in no tab, so it
    /// stays when its program exits, for reading. [`Reactor::open_tab`] adds one to the session.
    ///
    /// The reactor's thread makes every spawn, which [`Pty::spawn`] requires.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be created or polled, or the program cannot start.
    pub fn spawn(&mut self, mut command: Command, rows: u16, columns: u16) -> io::Result<usize> {
        command.env("TERM", "xterm-256color");
        // The program alone: its arguments can hold secrets.
        let program = command.get_program().to_string_lossy().into_owned();
        let pty = Pty::spawn(command, rows, columns)?;
        let pane = self.next_pane;
        let fd = pty.master().as_raw_fd();
        let interest = Interest::READABLE | Interest::WRITABLE;
        self.poll
            .registry()
            .register(&mut SourceFd(&fd), Token(pane), interest)?;
        self.next_pane += 1;
        self.panes.push(Pane {
            number: pane,
            pty,
            program,
            engine: Box::new(AlacrittyEngine::new(rows, columns)),
            replies: Vec::new(),
            input: Vec::new(),
            readable: true,
            closed: false,
            dirty: true,
            output: 0,
            search: None,
            found: None,
            sends: (Instant::now(), 0),
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
        let unfinished = self
            .panes
            .iter()
            .any(|pane| pane.readable || pane.search.is_some())
            || self.host.busy();
        let now = Instant::now();
        let deadline = self
            .panes
            .iter()
            .filter_map(|pane| pane.engine.sync_deadline())
            .chain(self.protect.deadline())
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
            if event.token() == LINK {
                self.host.wake();
            }
            if let Some(pane) = pane_mut(&mut self.panes, event.token().0) {
                pane.readable |= !pane.closed && (event.is_readable() || event.is_read_closed());
            }
        }
        self.send_due(Instant::now());
        for pane in &mut self.panes {
            if pane.readable {
                let mut master = pane.pty.master();
                let mut fed = false;
                let pumped = pump(
                    &mut master,
                    pane.engine.as_mut(),
                    &mut pane.replies,
                    &mut self.buffer,
                    &mut fed,
                );
                pane.dirty |= fed;
                pane.output = pane.output.wrapping_add(u64::from(fed));
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
            if let Some(search) = pane.search.as_mut() {
                let step = pane.engine.search(search);
                if step != SearchStep::Running {
                    pane.search = None;
                    pane.dirty = true;
                }
                if let SearchStep::Found(found) = step {
                    pane.found = Some(found);
                }
            }
            pane.flush();
        }
        self.reap();
        self.serve_window();
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
        let Some(pane) = pane_mut(&mut self.panes, pane) else {
            return false;
        };
        if pane.input.len() + bytes.len() > MAX_INPUT {
            return false;
        }
        pane.input.extend_from_slice(bytes);
        pane.dirty |= pane.engine.snap_to_live();
        pane.flush();
        true
    }

    /// Queues pasted text for a pane's program, sanitized. Returns what [`Reactor::write`]
    /// returns.
    pub fn paste(&mut self, pane: usize, text: &str) -> bool {
        let Some(engine) = self.engine(pane) else {
            return false;
        };
        let bracketed = engine.bracketed_paste();
        text.len() <= MAX_INPUT && self.write(pane, &paste::encode(text, bracketed))
    }

    /// Types `text` into a pane's program as it is, through the queue [`Reactor::write`] fills.
    /// Refused with a reason past [`SENDS_PER_SECOND`] calls in a second, which count refusals
    /// too, while the pane is protected or still taking a split send, or if the queue has no
    /// room for all of it.
    // shortcut: the second is per pane, which is per caller while the API has only the `self`
    // scope. Key it by caller when a caller can send to other panes.
    pub fn send(&mut self, pane: usize, text: &str, now: Instant) -> Result<(), &'static str> {
        let Some(entry) = pane_mut(&mut self.panes, pane) else {
            return Err("no such pane");
        };
        let (since, count) = &mut entry.sends;
        if *count == 0 || now.duration_since(*since) >= Duration::from_secs(1) {
            (*since, *count) = (now, 0);
        }
        if *count >= SENDS_PER_SECOND {
            return Err("rate limited: at most 20 sends a second");
        }
        *count += 1;
        if let Some(reason) = self.protection(pane) {
            return Err(reason.refusal());
        }
        if self.protect.is_sending(pane) {
            return Err("the pane is still taking an earlier send");
        }
        if self.write(pane, text.as_bytes()) {
            Ok(())
        } else {
            Err("the pane's input is full")
        }
    }

    /// Returns a pane's history and screen as plain text, newest last, or with `lines` only the
    /// last that many lines, without line ends after the last text. Past [`MAX_READ`] bytes the
    /// oldest text goes, cut at a character.
    #[must_use]
    pub fn read(&self, pane: usize, lines: Option<usize>) -> Option<String> {
        let everything = Selection {
            anchor: Position::default(),
            focus: Position {
                row: usize::MAX,
                column: u16::MAX,
            },
        };
        let mut text = newest_text(self.engine(pane)?, everything);
        if let Some(lines) = lines {
            let start = match lines.checked_sub(1) {
                None => text.len(),
                Some(skipped) => text
                    .rmatch_indices('\n')
                    .nth(skipped)
                    .map_or(0, |(at, _)| at + 1),
            };
            text.drain(..start);
        }
        Some(text)
    }

    /// Returns the output of a pane's last command, from its OSC 133 marks, as plain text with
    /// [`Reactor::read`]'s limit. Nothing if the pane is unknown or has no marks.
    #[must_use]
    pub fn last_output(&self, pane: usize) -> Option<String> {
        let engine = self.engine(pane)?;
        let output = engine.last_output()?;
        if output.focus < output.anchor {
            return Some(String::new());
        }
        Some(newest_text(engine, output))
    }

    /// Resizes a pane's terminal and tells its program.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY rejects the new size.
    pub fn resize(&mut self, pane: usize, rows: u16, columns: u16) -> io::Result<()> {
        let Some(pane) = pane_mut(&mut self.panes, pane) else {
            return Ok(());
        };
        pane.engine.resize(rows, columns);
        let (rows, columns) = pane.engine.size();
        pane.dirty = true;
        pane.pty.resize(rows, columns)
    }

    /// Moves a pane's view through history. See [`VtEngine::scroll`].
    pub fn scroll(&mut self, pane: usize, lines: i32) {
        if let Some(pane) = pane_mut(&mut self.panes, pane) {
            pane.dirty |= pane.engine.scroll(lines);
        }
    }

    /// Starts a search in a pane, replacing any it has running, and returns whether it could.
    /// [`Reactor::turn`] runs it a step at a time and marks the pane dirty when it ends.
    pub fn search(
        &mut self,
        pane: usize,
        query: &str,
        from: Position,
        direction: Direction,
    ) -> bool {
        let Some(pane) = pane_mut(&mut self.panes, pane) else {
            return false;
        };
        pane.search = Search::new(query, from, direction);
        pane.found = None;
        pane.search.is_some()
    }

    /// Returns whether a pane has a search running. Unknown panes have none.
    #[must_use]
    pub fn is_searching(&self, pane: usize) -> bool {
        self.pane(pane).is_some_and(|pane| pane.search.is_some())
    }

    /// Returns the match of a pane's last search, once, or nothing if it has not found one.
    pub fn take_found(&mut self, pane: usize) -> Option<Selection> {
        pane_mut(&mut self.panes, pane)?.found.take()
    }

    /// Returns whether a pane changed since the last call, and clears that.
    pub fn take_dirty(&mut self, pane: usize) -> bool {
        pane_mut(&mut self.panes, pane).is_some_and(|pane| std::mem::take(&mut pane.dirty))
    }

    /// Returns a pane's terminal.
    #[must_use]
    pub fn engine(&self, pane: usize) -> Option<&(dyn VtEngine + Send)> {
        self.pane(pane).map(|pane| pane.engine.as_ref())
    }

    /// Returns how many panes there are. A pane of a tab goes when its program exits or a window
    /// closes it, and its number is not used again.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Returns whether a pane's child has closed the PTY. Unknown panes count as closed.
    #[must_use]
    pub fn is_closed(&self, pane: usize) -> bool {
        self.pane(pane).is_none_or(|pane| pane.closed)
    }

    /// Returns the pane whose terminal and session a caller holds, which is who an API caller is
    /// (ADR-0032). Asked on each request, so a pane that closes, or whose program has ended,
    /// stops matching.
    #[must_use]
    pub fn pane_of(&self, caller: Terminal) -> Option<usize> {
        // The session id is the pane's child's pid. That pid cannot be reused while the pane is
        // here, because the child is only reaped when its `Pty` drops, so an exited child stays
        // a zombie holding it. Reaping earlier would need this to check the child too.
        let pane = self.panes.iter().find(|pane| pane.pty.terminal() == caller);
        pane.filter(|pane| !pane.closed).map(|pane| pane.number)
    }

    /// Describes every pane for `pane.list`, in order of number, with nothing from a screen.
    pub fn list(&self) -> impl Iterator<Item = Listed<'_>> {
        self.panes.iter().map(|pane| {
            let marks = pane.engine.shell_marks();
            let status = match (pane.closed, marks.phase) {
                (true, _) => "done",
                (_, ShellPhase::Running) => "running",
                (_, ShellPhase::Finished) if marks.exit.is_some_and(|exit| exit != 0) => "error",
                (_, ShellPhase::Finished) => "done",
                _ => "idle",
            };
            let cwd = pane.engine.cwd().map(|cwd| cwd.path.as_str());
            Listed {
                id: pane.number,
                title: cwd.and_then(|cwd| cwd.rsplit('/').next().filter(|name| !name.is_empty())),
                cwd,
                command: &pane.program,
                status,
            }
        })
    }

    /// Returns what `pane.wait` watches in a pane, or nothing once it has closed.
    #[must_use]
    pub fn progress(&self, pane: usize) -> Option<Progress> {
        let pane = self.pane(pane).filter(|pane| !pane.closed)?;
        let marks = pane.engine.shell_marks();
        Some(Progress {
            output: pane.output,
            marks,
        })
    }

    /// A pane's PTY master, for [`Reactor::protection`].
    pub(crate) fn master(&self, pane: usize) -> Option<&std::fs::File> {
        Some(self.pane(pane)?.pty.master())
    }

    fn pane(&self, number: usize) -> Option<&Pane> {
        let index = self.panes.binary_search_by_key(&number, |pane| pane.number);
        self.panes.get(index.ok()?)
    }

    /// Closes each pane of a tab whose program has exited. A pane in no tab stays, for reading.
    fn reap(&mut self) {
        for index in (0..self.panes.len()).rev() {
            let closed = self.panes.get(index).filter(|pane| pane.closed);
            if let Some(number) = closed.map(|pane| pane.number) {
                self.close_pane(number);
            }
        }
    }

    /// Returns the pane numbers, lowest first.
    pub(crate) fn numbers(&self) -> impl Iterator<Item = usize> + '_ {
        self.panes.iter().map(|pane| pane.number)
    }

    /// Stops polling a pane and drops it, which ends its program and frees its terminal.
    pub(crate) fn drop_pane(&mut self, number: usize) {
        if let Ok(index) = self.panes.binary_search_by_key(&number, |pane| pane.number) {
            let pane = self.panes.remove(index);
            let fd = pane.pty.master().as_raw_fd();
            let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
        }
    }
}

/// Copies `selection` as plain text without line ends after it, keeping its newest
/// [`MAX_READ`] bytes, cut at a character. The grid holds characters, so no escape is in it.
// shortcut: one call on the reactor's thread, bounded by the store's caps. Step it between
// turns, as a window's copy is, if reads ever hold up PTYs.
fn newest_text(engine: &dyn VtEngine, selection: Selection) -> String {
    let keep_newest = |text: &mut String| {
        let mut from = text.len().saturating_sub(MAX_READ);
        while !text.is_char_boundary(from) {
            from += 1;
        }
        text.drain(..from);
    };
    let mut copy = Copying::unlimited(selection);
    let mut text = String::new();
    while engine.copy_step(&mut copy, &mut text) == CopyStep::Running {
        if text.len() > 2 * MAX_READ {
            keep_newest(&mut text);
        }
    }
    text.truncate(text.trim_end_matches('\n').len());
    keep_newest(&mut text);
    text
}

fn pane_mut(panes: &mut [Pane], number: usize) -> Option<&mut Pane> {
    let index = panes.binary_search_by_key(&number, |pane| pane.number);
    panes.get_mut(index.ok()?)
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

    const CALLER_SOCKET: &str = "PINFOLD_TEST_CALLER_SOCKET";
    const CALLER_DETACHES: &str = "PINFOLD_TEST_CALLER_DETACHES";

    /// This test binary, run as [`identity_caller`] on the socket at `path`.
    fn caller(path: &std::path::Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "io::tests::identity_caller", "--ignored"])
            .env(CALLER_SOCKET, path);
        command
    }

    /// Turns the reactor until a caller connects, and returns it with what it holds.
    fn accept(
        reactor: &mut Reactor,
        listener: &std::os::unix::net::UnixListener,
    ) -> (std::os::unix::net::UnixStream, Option<Terminal>) {
        let start = Instant::now();
        loop {
            if let Ok((stream, _)) = listener.accept() {
                let terminal = pinfold_sys::peer_terminal(std::os::fd::AsFd::as_fd(&stream));
                return (stream, terminal.unwrap());
            }
            assert!(start.elapsed() < Duration::from_secs(10), "no caller came");
            reactor.turn(Some(Duration::from_millis(20))).unwrap();
        }
    }

    /// Not a test of its own: the caller that the identity test starts in a pane. It connects,
    /// or first leaves its session, and holds on until the test hangs up.
    #[test]
    #[ignore = "run by a_caller_is_the_pane_whose_terminal_and_session_it_holds"]
    fn identity_caller() {
        let Some(path) = std::env::var_os(CALLER_SOCKET) else {
            return;
        };
        if std::env::var_os(CALLER_DETACHES).is_some() {
            let mut command = caller(path.as_ref());
            command.env_remove(CALLER_DETACHES);
            let stderr = std::process::Stdio::null();
            pinfold_sys::spawn_detached(command, stderr)
                .unwrap()
                .wait()
                .unwrap();
            return;
        }
        let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
        let _ = stream.read(&mut [0; 1]);
    }

    #[test]
    fn a_caller_is_the_pane_whose_terminal_and_session_it_holds() {
        let directory = std::env::temp_dir().join(format!("pinfold-id-{}", std::process::id()));
        let _ = std::fs::create_dir(&directory);
        let path = directory.join("api.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut reactor = Reactor::new().unwrap();
        let b = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();

        // Pane A's caller claims to be pane B, which changes nothing.
        let mut spoofed = caller(&path);
        spoofed.env("PINFOLD_PANE_ID", b.to_string());
        let a = reactor.spawn(spoofed, 24, 80).unwrap();
        let (stream, terminal) = accept(&mut reactor, &listener);
        let terminal = terminal.unwrap();
        assert_eq!(reactor.pane_of(terminal), Some(a));

        // Pane A closes when its caller exits, and stops matching.
        drop(stream);
        assert!(turn_until(&mut reactor, |reactor| reactor.is_closed(a)));
        assert_eq!(reactor.pane_of(terminal), None);

        // A caller that leaves its pane's session has no terminal, so it is no pane.
        let mut detaches = caller(&path);
        detaches.env(CALLER_DETACHES, "1");
        reactor.spawn(detaches, 24, 80).unwrap();
        let (_stream, terminal) = accept(&mut reactor, &listener);
        assert_eq!(terminal, None);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_list_tells_program_directory_and_status_and_no_screen() {
        let mut reactor = Reactor::new().unwrap();
        let marks = r"printf '\033]7;file:///tmp/work\007\033]133;C\007secret'; sleep 30";
        let pane = reactor.spawn(shell(marks), 24, 80).unwrap();
        let listed = |reactor: &Reactor| reactor.list().any(|pane| pane.status == "running");
        assert!(turn_until(&mut reactor, listed));
        let expected = Listed {
            id: pane,
            title: Some("work"),
            cwd: Some("/tmp/work"),
            command: "/bin/sh",
            status: "running",
        };
        assert_eq!(reactor.list().collect::<Vec<_>>(), [expected]);
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
    fn a_dropped_pane_leaves_the_others_their_numbers() {
        let mut reactor = Reactor::new().unwrap();
        let panes = [(); 3].map(|()| reactor.spawn(shell("cat"), 24, 80).unwrap());
        assert_eq!(panes, [0, 1, 2]);
        reactor.drop_pane(1);
        reactor.drop_pane(9);
        assert!(reactor.is_closed(1) && reactor.engine(1).is_none());
        assert_eq!(reactor.pane_count(), 2);
        reactor.write(2, b"key\n");
        assert!(turn_until(&mut reactor, |r| on_screen(r, 2, "key")));
        assert_eq!(reactor.spawn(shell("cat"), 24, 80).unwrap(), 3);
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
    fn sends_past_20_a_second_or_a_full_queue_are_refused() {
        let mut reactor = Reactor::new().unwrap();
        let script = "stty raw -echo; echo ready; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "ready")));
        let now = Instant::now();
        for _ in 0..SENDS_PER_SECOND {
            assert_eq!(reactor.send(pane, "x", now), Ok(()));
        }
        let refused = reactor.send(pane, "x", now + Duration::from_millis(999));
        assert!(refused.unwrap_err().starts_with("rate limited"));
        let next_second = now + Duration::from_secs(1);
        assert_eq!(reactor.send(pane, "x", next_second), Ok(()));

        assert!(reactor.write(pane, &vec![b'x'; MAX_INPUT]));
        let queued = reactor.panes[pane].input.len();
        let full = reactor.send(pane, &"x".repeat(MAX_INPUT - queued + 1), next_second);
        assert_eq!(full, Err("the pane's input is full"));
        assert_eq!(reactor.panes[pane].input.len(), queued);
    }

    #[test]
    fn a_read_is_plain_text_newest_last_and_last_output_follows_the_marks() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"printf '\033[1m[one]\033[0m\ntwo\n\033]133;C\007out\nput\n\033]133;D;0\007$ '; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "$")));
        let read = |lines| reactor.read(pane, lines).unwrap();
        assert_eq!(read(None), "[one]\ntwo\nout\nput\n$");
        assert_eq!(read(Some(2)), "put\n$");
        assert_eq!(read(Some(0)), "");
        assert_eq!(read(Some(99)), read(None));
        assert_eq!(reactor.last_output(pane).unwrap(), "out\nput");
    }

    #[test]
    fn a_read_keeps_the_newest_2_mb() {
        let mut engine = AlacrittyEngine::new(24, 400);
        // About 4 MB: more than a read returns, and more rows than the store keeps.
        for line in 0..10_000 {
            engine.feed(
                format!("{line:06}{}\r\n", "x".repeat(394)).as_bytes(),
                &mut Vec::new(),
            );
        }
        let everything = Selection {
            anchor: Position::default(),
            focus: Position {
                row: usize::MAX,
                column: u16::MAX,
            },
        };
        let text = newest_text(&engine, everything);
        assert!(
            text.len() <= MAX_READ && text.len() > MAX_READ - 401,
            "{}",
            text.len()
        );
        assert!(text.ends_with(&format!("009999{}", "x".repeat(394))));
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
    fn scrolling_redraws_and_input_returns_to_the_screen() {
        let mut reactor = Reactor::new().unwrap();
        let script = "seq 1 40; read line; sleep 30";
        let pane = reactor.spawn(shell(script), 4, 20).unwrap();
        assert!(turn_until(&mut reactor, |r| {
            (0..4).any(|line| r.engine(pane).unwrap().line_text(line) == "40")
        }));
        let top = |reactor: &Reactor| {
            let mut frame = pinfold_proto::Frame::default();
            reactor.engine(pane).unwrap().frame(&mut frame);
            frame.cells[..2]
                .iter()
                .map(|cell| cell.character)
                .collect::<String>()
        };
        let live = top(&reactor);
        reactor.take_dirty(pane);
        reactor.scroll(pane, 1);
        assert!(reactor.take_dirty(pane));
        assert_ne!(top(&reactor), live);
        reactor.scroll(pane, -2);
        assert!(reactor.take_dirty(pane), "one row of that was real");
        reactor.scroll(pane, -1);
        assert!(!reactor.take_dirty(pane), "past the end nothing moves");
        reactor.scroll(pane, 3);
        reactor.take_dirty(pane);
        reactor.write(pane, b"x");
        assert!(reactor.take_dirty(pane));
        assert_eq!(top(&reactor), live);
    }

    #[test]
    fn a_search_runs_between_turns_and_reports_once() {
        let mut reactor = Reactor::new().unwrap();
        let script = "seq 1 3000; echo needle; sleep 30";
        let pane = reactor.spawn(shell(script), 4, 20).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "needle")));
        let from = Position::default();
        assert!(reactor.search(pane, "NEEDLE", from, Direction::Forward));
        assert!(reactor.is_searching(pane));
        reactor.take_dirty(pane);
        reactor.turn(Some(Duration::ZERO)).unwrap();
        assert!(
            reactor.is_searching(pane),
            "one turn must not read 3,000 rows"
        );
        assert!(turn_until(&mut reactor, |r| !r.is_searching(pane)));
        assert!(reactor.take_dirty(pane));
        let found = reactor.take_found(pane).unwrap();
        assert_eq!(found.anchor.row, 3000);
        assert_eq!(reactor.take_found(pane), None);
        let mut text = String::new();
        let engine = reactor.engine(pane).unwrap();
        assert!(engine.selection_text(found, &mut text));
        assert_eq!(text, "needle");
    }

    #[test]
    fn a_search_that_finds_nothing_ends_and_bad_ones_never_start() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 4, 20).unwrap();
        let from = Position::default();
        assert!(!reactor.search(pane, "", from, Direction::Backward));
        assert!(!reactor.search(9, "x", from, Direction::Backward));
        assert!(!reactor.is_searching(9));
        assert!(reactor.search(pane, "absent", from, Direction::Backward));
        assert!(turn_until(&mut reactor, |r| !r.is_searching(pane)));
        assert_eq!(reactor.take_found(pane), None);
        assert_eq!(reactor.take_found(9), None);
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
