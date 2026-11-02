//! The core's end of a window's link (ADR-0028): one window, served from the reactor's thread.
//!
//! Everything a window sends is untrusted. Any bad message, or a window that goes away, ends
//! the link and leaves every PTY running.
//!
//! A window that stops reading costs a fixed amount. A frame is only encoded once all earlier
//! bytes have reached the socket, so while it is stalled panes stay dirty and nothing queues,
//! and when it drains it gets one frame of the newest state. That frame is still a diff, as
//! nothing it was promised was lost. Replies cannot be merged like that, so one that would
//! pass [`MAX_PENDING`] ends the link.

use std::io::{self, ErrorKind, Read, Write};
use std::mem;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use mio::unix::SourceFd;
use mio::{Interest, Token};
use pinfold_proto::Frame;
use pinfold_proto::link::{self, Decoder, Notice, Request, VERSION, encode_frame};
use pinfold_sys::socket;

use crate::io::{READ_BUDGET, Reactor};
use crate::search::{CopyStep, Copying, Direction, Position, Selection};

/// Beyond any pane number, like the waker's token.
pub(crate) const LINK: Token = Token(usize::MAX - 1);

/// Room for replies beside a frame: two selections of the longest kind.
const REPLIES: usize = 4 * 1024 * 1024;
/// Most bytes waiting for a window: the largest frame, its prefix, and [`REPLIES`].
pub const MAX_PENDING: usize = size_of::<u32>() + link::MAX_MESSAGE + REPLIES;
/// Room for a notice around its text, which is well past the largest head.
const NOTICE_HEAD: usize = 64;
/// Send buffer an idle link keeps. This holds a full frame of 200x80 cells.
const KEPT_OUTPUT: usize = 256 * 1024;
/// Cells the spare frame keeps, so one huge pane does not cost memory for good.
const KEPT_CELLS: usize = 16 * 1024;
/// One read from the socket, which lives on the stack and costs nothing when idle.
const READ_CHUNK: usize = 16 * 1024;

/// What [`Reactor::spawn_window`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowOpen {
    /// A new window process started.
    Started,
    /// One is already open, so no process started.
    AlreadyOpen,
}

impl From<link::Position> for Position {
    fn from(position: link::Position) -> Self {
        Self {
            row: position.row,
            column: position.column,
        }
    }
}

impl From<link::Selection> for Selection {
    fn from(selection: link::Selection) -> Self {
        Self {
            anchor: selection.anchor.into(),
            focus: selection.focus.into(),
        }
    }
}

impl From<Selection> for link::Selection {
    fn from(selection: Selection) -> Self {
        let position = |at: Position| link::Position {
            row: at.row,
            column: at.column,
        };
        Self {
            anchor: position(selection.anchor),
            focus: position(selection.focus),
        }
    }
}

impl From<link::Direction> for Direction {
    fn from(direction: link::Direction) -> Self {
        match direction {
            link::Direction::Forward => Self::Forward,
            link::Direction::Backward => Self::Backward,
        }
    }
}

/// The link has ended. Why does not matter, as every cause is handled the same way.
struct Ended;

/// What the link knows about one pane, which goes when the link does.
#[derive(Default)]
struct PaneLink {
    /// The last frame encoded, which the next one is a diff against.
    sent: Frame,
    /// Whether the window holds a frame to apply a diff to.
    synced: bool,
    cwd: String,
    searching: bool,
    exited: bool,
}

/// A copy, which takes a step per pass so a window cannot hold up PTYs with long ones.
struct CopyJob {
    pane: usize,
    copy: Copying,
    text: String,
}

/// The send side, and what a request can change.
struct Session {
    socket: UnixStream,
    out: Vec<u8>,
    /// How much of `out` the socket has taken.
    head: usize,
    panes: Vec<PaneLink>,
    /// The frame being read, swapped with a pane's `sent` once encoded.
    spare: Frame,
    /// Where the next pass over panes starts, so a busy pane cannot starve the rest.
    next: usize,
    greeted: bool,
    copying: Option<CopyJob>,
}

impl Session {
    fn pending(&self) -> usize {
        self.out.len().saturating_sub(self.head)
    }

    /// Queues a reply, or ends the link if a window is not reading its replies.
    fn notice(&mut self, notice: &Notice<'_>) -> Result<(), Ended> {
        let text = match notice {
            Notice::Copied { text, .. } => text.len(),
            _ => 0,
        };
        if self.pending() + NOTICE_HEAD + text > MAX_PENDING {
            return Err(Ended);
        }
        self.out.drain(..self.head);
        self.head = 0;
        notice.encode(&mut self.out).map_err(|_| Ended)
    }

    /// Writes what the socket will take, and keeps the rest for its next wake-up.
    fn flush(&mut self) -> Result<(), Ended> {
        while let Some(bytes) = self.out.get(self.head..).filter(|bytes| !bytes.is_empty()) {
            match (&self.socket).write(bytes) {
                Ok(0) => return Err(Ended),
                Ok(count) => self.head += count,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(Ended),
            }
        }
        self.out.clear();
        self.head = 0;
        self.out.shrink_to(KEPT_OUTPUT);
        Ok(())
    }

    /// Carries out a request. An unknown pane does nothing, as the reactor's calls refuse it
    /// and `get_mut` finds nothing here.
    fn apply(&mut self, request: Request<'_>, reactor: &mut Reactor) -> Result<(), Ended> {
        match request {
            Request::Hello { .. } => self.greeted = true,
            Request::Input { pane, bytes } => {
                reactor.write(pane as usize, bytes);
            }
            // Never `write`: this is the call that sanitizes a paste (T7).
            Request::Paste { pane, text } => {
                reactor.paste(pane as usize, text);
            }
            // A PTY that refuses a size is the pane's problem, not a reason to drop the link.
            Request::Resize {
                pane,
                rows,
                columns,
            } => {
                let _ = reactor.resize(pane as usize, rows, columns);
            }
            Request::Scroll { pane, lines } => reactor.scroll(pane as usize, lines),
            Request::Reveal { pane, row } => reveal(reactor, pane as usize, row),
            Request::Search {
                pane,
                from,
                direction,
                query,
            } => {
                let pane = pane as usize;
                reactor.search(pane, query, from.into(), direction.into());
                // One that could not start ends now, and `report` says so.
                if let Some(state) = self.panes.get_mut(pane) {
                    state.searching = true;
                }
            }
            // shortcut: a new copy replaces a running one, which then gets no reply, queue them
            // if a window ever needs two at once.
            Request::Copy { pane, selection } => {
                let pane = pane as usize;
                self.copying = self.panes.get(pane).map(|_| CopyJob {
                    pane,
                    copy: Copying::new(selection.into()),
                    text: String::new(),
                });
            }
            Request::FullFrame { pane } => {
                if let Some(state) = self.panes.get_mut(pane as usize) {
                    state.synced = false;
                }
            }
            Request::Quit => return Err(Ended),
        }
        Ok(())
    }

    /// Sends what finished since the last pass: searches, and a step of any copy.
    fn report(&mut self, reactor: &mut Reactor) -> Result<(), Ended> {
        for pane in 0..self.panes.len() {
            let Some(state) = self.panes.get_mut(pane) else {
                continue;
            };
            if !state.searching || reactor.is_searching(pane) {
                continue;
            }
            state.searching = false;
            let found = reactor.take_found(pane);
            if let Some(found) = found {
                reveal(reactor, pane, found.anchor.row);
            }
            self.notice(&Notice::Searched {
                pane: pane as u32,
                found: found.map(Into::into),
            })?;
        }
        let Some(job) = self.copying.as_mut() else {
            return Ok(());
        };
        let step = reactor
            .engine(job.pane)
            .map(|engine| engine.copy_step(&mut job.copy, &mut job.text));
        if step == Some(CopyStep::Running) {
            return Ok(());
        }
        if let Some(job) = self.copying.take() {
            self.notice(&Notice::Copied {
                pane: job.pane as u32,
                text: &job.text,
            })?;
        }
        Ok(())
    }

    /// Sends each pane that changed, while the socket keeps up.
    fn send_panes(&mut self, reactor: &mut Reactor) -> Result<(), Ended> {
        self.flush()?;
        let count = self.panes.len();
        for offset in 0..count {
            let pane = (self.next + offset) % count;
            if self.pending() > 0 {
                self.next = pane;
                return Ok(());
            }
            self.send_pane(pane, reactor)?;
        }
        Ok(())
    }

    fn send_pane(&mut self, pane: usize, reactor: &mut Reactor) -> Result<(), Ended> {
        let Some(state) = self.panes.get_mut(pane).filter(|state| !state.exited) else {
            return Ok(());
        };
        let dirty = reactor.take_dirty(pane);
        if dirty || !state.synced {
            let Some(engine) = reactor.engine(pane) else {
                return Ok(());
            };
            engine.frame(&mut self.spare);
            let previous = state.synced.then_some(&state.sent);
            let (top, held) = (engine.position(0, 0).row, engine.view_rows());
            let number = pane as u32;
            encode_frame(&mut self.out, number, &self.spare, top, held, previous)
                .map_err(|_| Ended)?;
            mem::swap(&mut state.sent, &mut self.spare);
            state.sent.cells.shrink_to_fit();
            self.spare.cells.clear();
            self.spare.cells.shrink_to(KEPT_CELLS);
            state.synced = true;

            let cwd = engine.cwd().map_or("", |cwd| cwd.path.as_str());
            if state.cwd != cwd {
                cwd.clone_into(&mut state.cwd);
                let path = &state.cwd;
                let notice = Notice::Cwd { pane: number, path };
                notice.encode(&mut self.out).map_err(|_| Ended)?;
            }
        }
        if reactor.is_closed(pane) {
            state.exited = true;
            state.sent = Frame::default();
            self.notice(&Notice::Exited { pane: pane as u32 })?;
        }
        self.flush()
    }
}

/// Scrolls a pane's view just far enough to show `row`, which can be any number.
fn reveal(reactor: &mut Reactor, pane: usize, row: usize) {
    let Some(engine) = reactor.engine(pane) else {
        return;
    };
    let top = engine.position(0, 0).row;
    let bottom = top.saturating_add(usize::from(engine.size().0).saturating_sub(1));
    let distance = |rows: usize| i32::try_from(rows).unwrap_or(i32::MAX);
    let lines = if row < top {
        distance(top - row)
    } else if row > bottom {
        -distance(row - bottom)
    } else {
        0
    };
    reactor.scroll(pane, lines);
}

/// One window's link, and the process on the other end if the core started it.
pub(crate) struct Window {
    child: Option<Child>,
    decoder: Decoder,
    /// Whether the socket may hold more, as polling is edge-triggered.
    readable: bool,
    session: Session,
}

impl Window {
    fn read(&mut self, reactor: &mut Reactor) -> Result<(), Ended> {
        let mut buffer = [0; READ_CHUNK];
        let mut left = READ_BUDGET;
        while left > 0 {
            match (&self.session.socket).read(&mut buffer) {
                Ok(0) => return Err(Ended),
                Ok(count) => {
                    left = left.saturating_sub(count);
                    let mut input = buffer.get(..count).unwrap_or_default();
                    while let Some(request) =
                        self.decoder.next_request(&mut input).map_err(|_| Ended)?
                    {
                        self.session.apply(request, reactor)?;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    self.readable = false;
                    return Ok(());
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(Ended),
            }
        }
        Ok(())
    }

    /// One pass: reads requests, then sends what they and the panes produced.
    fn serve(&mut self, reactor: &mut Reactor) -> Result<(), Ended> {
        let count = reactor.pane_count();
        self.session.panes.resize_with(count, PaneLink::default);
        if self.readable {
            self.read(reactor)?;
        }
        if self.session.greeted {
            self.session.report(reactor)?;
            self.session.send_panes(reactor)
        } else {
            self.session.flush()
        }
    }
}

/// The session's window, and windows killed but not yet reaped.
#[derive(Default)]
pub(crate) struct Host {
    window: Option<Window>,
    /// shortcut: reaped only when a link ends or a window opens, so one stays a zombie until
    /// then. Reap each turn if windows ever end often.
    dying: Vec<Child>,
}

impl Host {
    /// Whether the next turn should not wait.
    pub(crate) fn busy(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.readable || window.session.copying.is_some())
    }

    /// Notes that the socket has news.
    pub(crate) fn wake(&mut self) {
        if let Some(window) = self.window.as_mut() {
            window.readable = true;
        }
    }

    fn retire(&mut self, mut window: Window) {
        if let Some(mut child) = window.child.take() {
            let _ = child.kill();
            self.dying.push(child);
        }
        self.reap();
    }

    /// Never waits, so a window that is slow to die cannot stall the reactor.
    fn reap(&mut self) {
        self.dying
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let window = self
            .window
            .as_mut()
            .and_then(|window| window.child.as_mut());
        if let Some(child) = window {
            let _ = child.kill();
        }
    }
}

impl Reactor {
    /// Starts `binary` as this session's window, with no arguments, on a new link.
    ///
    /// A session has one window, so while one lives this starts nothing and says so. The
    /// program finds the link on [`socket::LINK_FD`], and pane children never get it. A window
    /// whose link ends is killed, and this can then start another.
    ///
    /// # Errors
    ///
    /// Returns an error if `binary` is not an absolute path, as a search of `PATH` could find
    /// another program, or if it cannot start.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    ///
    /// use pinfold_core::io::Reactor;
    /// use pinfold_core::link::WindowOpen;
    ///
    /// let mut reactor = Reactor::new()?;
    /// let binary = Path::new("/bin/cat");
    /// assert_eq!(reactor.spawn_window(binary)?, WindowOpen::Started);
    /// assert_eq!(reactor.spawn_window(binary)?, WindowOpen::AlreadyOpen);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn spawn_window(&mut self, binary: &Path) -> io::Result<WindowOpen> {
        self.host.reap();
        if self.host.window.is_some() {
            return Ok(WindowOpen::AlreadyOpen);
        }
        if !binary.is_absolute() {
            return Err(io::Error::from(ErrorKind::InvalidInput));
        }
        let (ours, theirs) = socket::pair()?;
        let mut command = Command::new(binary);
        command.stdin(Stdio::null());
        let child = socket::spawn_with_link(command, theirs)?;
        self.attach_window(ours, Some(child))?;
        Ok(WindowOpen::Started)
    }

    /// Returns whether a window's link is open.
    #[must_use]
    pub fn has_window(&self) -> bool {
        self.host.window.is_some()
    }

    /// Serves `socket` as the window, and sends the version check.
    fn attach_window(&mut self, socket: UnixStream, child: Option<Child>) -> io::Result<()> {
        let fd = socket.as_raw_fd();
        let mut window = Window {
            child,
            decoder: Decoder::default(),
            // Anything already sent raises no edge, so the first pass looks.
            readable: true,
            session: Session {
                socket,
                out: Vec::new(),
                head: 0,
                panes: Vec::new(),
                spare: Frame::default(),
                next: 0,
                greeted: false,
                copying: None,
            },
        };
        let interest = Interest::READABLE | Interest::WRITABLE;
        let session = &mut window.session;
        let opened = session.socket.set_nonblocking(true).and_then(|()| {
            let hello = Notice::Hello { version: VERSION };
            hello.encode(&mut session.out).map_err(io::Error::other)?;
            self.poll
                .registry()
                .register(&mut SourceFd(&fd), LINK, interest)
        });
        if let Err(error) = opened {
            self.host.retire(window);
            return Err(error);
        }
        // A window already gone shows up as the end of its stream on the first pass.
        let _ = window.session.flush();
        self.host.window = Some(window);
        Ok(())
    }

    /// Runs a pass of the link, and ends it if that says to. Called at the end of each turn.
    pub(crate) fn serve_window(&mut self) {
        let Some(mut window) = self.host.window.take() else {
            return;
        };
        if window.serve(self).is_ok() {
            self.host.window = Some(window);
            return;
        }
        let fd = window.session.socket.as_raw_fd();
        let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
        self.host.retire(window);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use pinfold_proto::link::MAX_MESSAGE;

    use super::*;
    use crate::search::MAX_SELECTION;

    const ALIVE: &str = "while read line; do echo got-$line; done";

    /// A window in the same thread as the reactor, over a real socketpair.
    struct Peer {
        socket: UnixStream,
        decoder: Decoder,
        hellos: usize,
        frames: HashMap<u32, Frame>,
        tops: HashMap<u32, usize>,
        cwd: String,
        searched: Vec<Option<link::Selection>>,
        copied: Vec<String>,
        exited: Vec<u32>,
    }

    impl Peer {
        fn attach(reactor: &mut Reactor) -> Self {
            let (ours, socket) = socket::pair().unwrap();
            socket.set_nonblocking(true).unwrap();
            reactor.attach_window(ours, None).unwrap();
            Self {
                socket,
                decoder: Decoder::default(),
                hellos: 0,
                frames: HashMap::new(),
                tops: HashMap::new(),
                cwd: String::new(),
                searched: Vec::new(),
                copied: Vec::new(),
                exited: Vec::new(),
            }
        }

        /// Attaches and says hello.
        fn greet(reactor: &mut Reactor) -> Self {
            let peer = Self::attach(reactor);
            peer.send(Request::Hello { version: VERSION });
            peer
        }

        fn send(&self, request: Request<'_>) {
            let mut wire = Vec::new();
            request.encode(&mut wire).unwrap();
            self.raw(&wire);
        }

        fn raw(&self, wire: &[u8]) {
            (&self.socket).write_all(wire).unwrap();
        }

        /// Takes everything waiting.
        fn read(&mut self) {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let count = match (&self.socket).read(&mut buffer) {
                    Ok(0) => return,
                    Ok(count) => count,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                    Err(error) if error.kind() == ErrorKind::ConnectionReset => return,
                    Err(error) => panic!("{error}"),
                };
                let mut input = &buffer[..count];
                while let Some(notice) = self.decoder.next_notice(&mut input).unwrap() {
                    match notice {
                        Notice::Hello { .. } => self.hellos += 1,
                        Notice::Frame(update) => {
                            let frame = self.frames.entry(update.pane).or_default();
                            assert!(update.apply(frame), "a diff with no frame under it");
                            self.tops.insert(update.pane, update.top);
                        }
                        Notice::Cwd { path, .. } => path.clone_into(&mut self.cwd),
                        Notice::Searched { found, .. } => self.searched.push(found),
                        Notice::Copied { text, .. } => self.copied.push(text.to_owned()),
                        Notice::Exited { pane } => self.exited.push(pane),
                    }
                }
            }
        }

        /// Whether this window draws what the pane's terminal holds.
        fn matches(&self, reactor: &Reactor, pane: usize) -> bool {
            let mut frame = Frame::default();
            reactor.engine(pane).unwrap().frame(&mut frame);
            self.frames.get(&(pane as u32)) == Some(&frame)
        }
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    fn turn_until(reactor: &mut Reactor, mut done: impl FnMut(&mut Reactor) -> bool) -> bool {
        let start = Instant::now();
        while !done(reactor) && start.elapsed() < Duration::from_secs(20) {
            reactor.turn(Some(Duration::from_millis(5))).unwrap();
        }
        done(reactor)
    }

    /// Turns the reactor with the window reading, until `done`.
    fn serve(reactor: &mut Reactor, peer: &mut Peer, done: impl Fn(&Reactor, &Peer) -> bool) {
        let served = turn_until(reactor, |reactor| {
            peer.read();
            done(reactor, peer)
        });
        assert!(served, "the link did not get there");
    }

    fn on_screen(reactor: &Reactor, pane: usize, text: &str) -> bool {
        let engine = reactor.engine(pane).unwrap();
        (0..engine.size().0).any(|line| engine.line_text(line).contains(text))
    }

    fn assert_pty_lives(reactor: &mut Reactor, pane: usize) {
        assert!(reactor.write(pane, b"still-here\n"));
        let wanted = "got-still-here";
        assert!(turn_until(reactor, |r| on_screen(r, pane, wanted)));
    }

    fn assert_link_ends_and_pty_lives(reactor: &mut Reactor, pane: usize) {
        assert!(turn_until(reactor, |r| !r.has_window()));
        assert_pty_lives(reactor, pane);
    }

    #[test]
    fn hello_goes_first_each_way_and_frames_wait_for_it() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("echo hi; sleep 30"), 24, 80).unwrap();
        let mut peer = Peer::attach(&mut reactor);
        turn_until(&mut reactor, |r| on_screen(r, pane, "hi"));
        peer.read();
        assert_eq!(peer.hellos, 1);
        assert!(peer.frames.is_empty(), "a frame came before the hello");
        peer.send(Request::Hello { version: VERSION });
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
    }

    #[test]
    fn another_version_ends_the_link_and_the_pty_lives() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::attach(&mut reactor);
        peer.raw(&[3, 0, 0, 0, 0, 9, 0]);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn garbage_ends_the_link_and_the_pty_lives() {
        let wires: [&[u8]; 4] = [
            b"GET / HTTP/1.1\r\n\r\n",
            &[0xff; 4096],
            &[1, 0, 0, 0, 9],
            &[0, 0, 0, 0],
        ];
        for wire in wires {
            let mut reactor = Reactor::new().unwrap();
            let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
            let peer = Peer::attach(&mut reactor);
            peer.raw(wire);
            assert_link_ends_and_pty_lives(&mut reactor, pane);
        }
    }

    #[test]
    fn garbage_after_a_good_hello_ends_the_link() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.raw(&[2, 0, 0, 0, 200, 1]);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn a_window_that_closes_mid_message_ends_the_link() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.raw(&[100, 0, 0, 0, 1, 0]);
        drop(peer);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn quit_ends_the_link() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.send(Request::Quit);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn a_stalled_window_holds_one_frame_and_catches_up_on_the_newest() {
        let mut reactor = Reactor::new().unwrap();
        let (rows, columns) = (40, 120);
        let script = "yes | head -n 3000000; echo finished; sleep 30";
        let flood = reactor.spawn(shell(script), rows, columns).unwrap();
        let quiet = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let mut peer = Peer::greet(&mut reactor);

        let one_frame = 64 + usize::from(rows) * (2 + usize::from(columns) * 12);
        let (mut most, mut capacity) = (0, 0);
        let flooded = turn_until(&mut reactor, |r| {
            let session = &r.host.window.as_ref().unwrap().session;
            most = most.max(session.pending());
            capacity = capacity.max(session.out.capacity());
            on_screen(r, flood, "finished")
        });
        assert!(flooded);
        assert!(reactor.has_window());
        assert!(most > 0, "the socket took everything, so nothing stalled");
        assert!(most <= one_frame, "{most} bytes waiting");
        assert!(capacity <= KEPT_OUTPUT, "{capacity} bytes of buffer");
        assert_pty_lives(&mut reactor, quiet);

        serve(&mut reactor, &mut peer, |r, p| {
            p.matches(r, flood) && p.matches(r, quiet)
        });
    }

    #[test]
    fn a_blocked_turn_wakes_when_the_window_drains_the_socket() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 400, 400).unwrap();
        let peer = Peer::greet(&mut reactor);
        turn_until(&mut reactor, |r| {
            r.host
                .window
                .as_ref()
                .is_some_and(|w| w.session.pending() > 0)
        });
        assert!(reactor.has_window() && reactor.engine(pane).is_some());

        peer.socket.set_nonblocking(false).unwrap();
        let mut socket = peer.socket.try_clone().unwrap();
        let reader = std::thread::spawn(move || io::copy(&mut socket, &mut io::sink()));
        let start = Instant::now();
        while reactor.host.window.as_ref().unwrap().session.pending() > 0 {
            reactor.turn(Some(Duration::from_secs(5))).unwrap();
            assert!(start.elapsed() < Duration::from_secs(4), "a turn slept");
        }
        peer.socket.shutdown(std::net::Shutdown::Both).unwrap();
        reader.join().unwrap().unwrap();
    }

    #[test]
    fn the_clients_opening_works_with_or_without_a_pane_0() {
        let mut reactor = Reactor::new().unwrap();
        let mut peer = Peer::greet(&mut reactor);
        peer.send(Request::FullFrame { pane: 0 });
        serve(&mut reactor, &mut peer, |_, p| p.hellos == 1);
        assert!(reactor.has_window(), "an unknown pane ended the link");

        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        assert_eq!(pane, 0);
        peer.send(Request::FullFrame { pane: 0 });
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, 0));
    }

    #[test]
    fn a_rejected_query_is_answered_and_the_link_stays() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let mut peer = Peer::greet(&mut reactor);
        let long = "x".repeat(crate::search::MAX_QUERY + 1);
        for (asked, query) in ["", long.as_str()].into_iter().enumerate() {
            peer.send(Request::Search {
                pane: pane as u32,
                from: link::Position::default(),
                direction: link::Direction::Forward,
                query,
            });
            serve(&mut reactor, &mut peer, |_, p| {
                p.searched.len() == asked + 1
            });
            assert_eq!(peer.searched[asked], None);
        }
        assert!(reactor.has_window());
        assert_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn replies_a_window_does_not_read_end_the_link_at_the_cap() {
        let mut reactor = Reactor::new().unwrap();
        let _peer = Peer::attach(&mut reactor);
        let text = "x".repeat(MAX_SELECTION);
        let session = &mut reactor.host.window.as_mut().unwrap().session;
        let notice = Notice::Copied {
            pane: 0,
            text: &text,
        };
        let mut sent = 0;
        while session.notice(&notice).is_ok() {
            sent += 1;
            assert!(session.pending() <= MAX_PENDING);
            assert!(sent < 100, "nothing stopped them");
        }
        assert!(sent >= 2, "two whole selections must fit");
    }

    #[test]
    fn bad_pane_numbers_and_wild_values_change_nothing() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let mut peer = Peer::greet(&mut reactor);
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));

        let far = link::Position {
            row: usize::MAX,
            column: u16::MAX,
        };
        let selection = link::Selection {
            anchor: far,
            focus: far,
        };
        let search = |pane, direction, query| Request::Search {
            pane,
            from: far,
            direction,
            query,
        };
        for bad in [1, 9, u32::MAX] {
            peer.send(Request::Input {
                pane: bad,
                bytes: b"x",
            });
            peer.send(Request::Paste {
                pane: bad,
                text: "x",
            });
            peer.send(Request::Resize {
                pane: bad,
                rows: 2,
                columns: 2,
            });
            peer.send(Request::Scroll {
                pane: bad,
                lines: 1,
            });
            peer.send(Request::Reveal { pane: bad, row: 1 });
            peer.send(Request::Copy {
                pane: bad,
                selection,
            });
            peer.send(Request::FullFrame { pane: bad });
            peer.send(search(bad, link::Direction::Forward, "x"));
        }
        let number = pane as u32;
        for lines in [i32::MIN, i32::MAX] {
            peer.send(Request::Scroll {
                pane: number,
                lines,
            });
        }
        for row in [0, usize::MAX] {
            peer.send(Request::Reveal { pane: number, row });
        }
        peer.send(search(number, link::Direction::Forward, "x"));
        peer.send(search(number, link::Direction::Backward, "x"));
        peer.send(search(number, link::Direction::Forward, ""));
        peer.send(Request::Copy {
            pane: number,
            selection,
        });
        for (rows, columns) in [(1, 1), (24, 80)] {
            peer.send(Request::Resize {
                pane: number,
                rows,
                columns,
            });
        }

        serve(&mut reactor, &mut peer, |_, p| p.copied.len() == 1);
        assert!(reactor.has_window());
        assert_pty_lives(&mut reactor, pane);
        assert!(peer.frames.keys().all(|known| *known == number));
    }

    #[test]
    fn a_paste_arrives_cleaned() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"stty raw -echo; printf '\033[?2004h'; dd bs=1 count=20 2>/dev/null | od -An -c | tr -d ' \n' | sed 's/^/got-/'; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        turn_until(&mut reactor, |r| r.engine(pane).unwrap().bracketed_paste());
        peer.send(Request::Paste {
            pane: pane as u32,
            text: "ab\x1b[201~c",
        });
        let expected = r"got-033[200~ab[201~c033[201~";
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, expected)));
    }

    #[test]
    fn input_reaches_the_program() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.send(Request::Input {
            pane: pane as u32,
            bytes: b"key\n",
        });
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "got-key")));
    }

    #[test]
    fn frames_decode_to_the_same_frame_through_diffs_a_resize_and_history() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"seq 1 100; printf '\033[1;31mred\033[0m'; read line; echo more; sleep 30";
        let pane = reactor.spawn(shell(script), 10, 40).unwrap();
        let mut peer = Peer::greet(&mut reactor);
        let number = pane as u32;
        serve(&mut reactor, &mut peer, |r, p| {
            on_screen(r, pane, "red") && p.matches(r, pane)
        });

        peer.send(Request::Scroll {
            pane: number,
            lines: 5,
        });
        serve(&mut reactor, &mut peer, |r, p| {
            r.engine(pane).unwrap().position(0, 0).row == p.tops[&number]
                && p.frames[&number].cursor.is_none()
                && p.matches(r, pane)
        });

        peer.send(Request::Resize {
            pane: number,
            rows: 6,
            columns: 30,
        });
        serve(&mut reactor, &mut peer, |r, p| {
            p.frames[&number].rows == 6 && p.matches(r, pane)
        });

        peer.send(Request::Input {
            pane: number,
            bytes: b"\n",
        });
        serve(&mut reactor, &mut peer, |r, p| {
            on_screen(r, pane, "more") && p.matches(r, pane)
        });

        peer.frames.clear();
        peer.send(Request::FullFrame { pane: number });
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
    }

    #[test]
    fn a_pane_opened_later_gets_a_full_frame() {
        let mut reactor = Reactor::new().unwrap();
        let mut peer = Peer::greet(&mut reactor);
        serve(&mut reactor, &mut peer, |_, p| p.hellos == 1);
        let script = "echo later; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        serve(&mut reactor, &mut peer, |r, p| {
            on_screen(r, pane, "later") && p.matches(r, pane)
        });
    }

    #[test]
    fn search_copy_cwd_and_exit_reach_the_window() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"seq 1 200; echo needle; printf '\033]7;file:///tmp/where\033\\'; seq 1 50; read line; exit 0";
        let pane = reactor.spawn(shell(script), 10, 40).unwrap();
        let mut peer = Peer::greet(&mut reactor);
        let number = pane as u32;
        serve(&mut reactor, &mut peer, |_, p| p.cwd == "/tmp/where");

        peer.send(Request::Search {
            pane: number,
            from: link::Position {
                row: usize::MAX / 2,
                column: 0,
            },
            direction: link::Direction::Backward,
            query: "needle",
        });
        serve(&mut reactor, &mut peer, |_, p| p.searched.len() == 1);
        let found = peer.searched[0].unwrap();
        let top = reactor.engine(pane).unwrap().position(0, 0).row;
        assert!((top..top + 10).contains(&found.anchor.row), "not in view");

        peer.send(Request::Copy {
            pane: number,
            selection: found,
        });
        serve(&mut reactor, &mut peer, |_, p| p.copied.len() == 1);
        assert_eq!(peer.copied, ["needle"]);

        peer.send(Request::Search {
            pane: number,
            from: link::Position::default(),
            direction: link::Direction::Forward,
            query: "",
        });
        serve(&mut reactor, &mut peer, |_, p| p.searched.len() == 2);
        assert_eq!(peer.searched[1], None);

        peer.send(Request::Input {
            pane: number,
            bytes: b"\n",
        });
        serve(&mut reactor, &mut peer, |_, p| p.exited == [number]);
        assert!(reactor.has_window(), "a pane ending is not the window");
    }

    #[test]
    fn buffers_shrink_back_after_a_large_frame() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.spawn(shell("sleep 30"), 24, 80).unwrap();
        let mut peer = Peer::greet(&mut reactor);
        let number = pane as u32;
        for size in [400, 24] {
            peer.send(Request::Resize {
                pane: number,
                rows: size,
                columns: 80,
            });
            serve(&mut reactor, &mut peer, |r, p| {
                p.frames.get(&number).is_some_and(|f| f.rows == size) && p.matches(r, pane)
            });
        }
        let session = &reactor.host.window.as_ref().unwrap().session;
        assert!(session.out.capacity() <= KEPT_OUTPUT);
        assert!(session.spare.cells.capacity() <= KEPT_CELLS);
        assert_eq!(session.panes[pane].sent.cells.capacity(), 24 * 80);
    }

    /// Writes a window program that logs its argument count to a file beside it.
    fn window_script(name: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pinfold-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (path, log) = (dir.join("window"), dir.join("log"));
        let script = format!("#!/bin/sh\necho \"$#\" >> '{}'\n{body}\n", log.display());
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (path, log)
    }

    /// Linux refuses to run a file another thread's fork still holds open for writing.
    fn spawn_window(reactor: &mut Reactor, binary: &Path) -> WindowOpen {
        for _ in 0..100 {
            match reactor.spawn_window(binary) {
                Err(error) if error.kind() == ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => return result.unwrap(),
            }
        }
        panic!("the script stayed busy");
    }

    fn starts(log: &Path) -> String {
        fs::read_to_string(log).unwrap_or_default()
    }

    #[test]
    fn a_second_open_starts_nothing_until_the_first_has_gone() {
        let hello = r"printf '\003\000\000\000\000\001\000' >&3; exec cat <&3 >/dev/null";
        let (binary, log) = window_script("second-open", hello);
        let mut reactor = Reactor::new().unwrap();
        assert_eq!(spawn_window(&mut reactor, &binary), WindowOpen::Started);
        let greeted = |r: &mut Reactor| r.host.window.as_ref().is_some_and(|w| w.session.greeted);
        assert!(turn_until(&mut reactor, greeted), "fd 3 did not work");
        assert_eq!(spawn_window(&mut reactor, &binary), WindowOpen::AlreadyOpen);
        assert_eq!(starts(&log), "0\n", "one process, with no arguments");

        let window = reactor.host.window.as_mut().unwrap();
        window.child.as_mut().unwrap().kill().unwrap();
        assert!(turn_until(&mut reactor, |r| !r.has_window()));
        assert_eq!(spawn_window(&mut reactor, &binary), WindowOpen::Started);
        assert!(turn_until(&mut reactor, |_| starts(&log) == "0\n0\n"));
        drop(reactor);
        fs::remove_dir_all(binary.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_window_that_breaks_the_link_is_killed_and_reaped() {
        let (binary, log) = window_script("killed", "echo not-the-protocol >&3; exec sleep 300");
        let mut reactor = Reactor::new().unwrap();
        spawn_window(&mut reactor, &binary);
        assert!(turn_until(&mut reactor, |r| !r.has_window()));
        assert_eq!(starts(&log), "0\n");
        assert!(turn_until(&mut reactor, |r| {
            r.host.reap();
            r.host.dying.is_empty()
        }));
        fs::remove_dir_all(binary.parent().unwrap()).unwrap();
    }

    #[test]
    fn only_a_program_that_starts_counts_as_a_window() {
        let mut reactor = Reactor::new().unwrap();
        let error = reactor.spawn_window(Path::new("cat")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(reactor.spawn_window(Path::new("/does/not/exist")).is_err());
        assert!(!reactor.has_window());
    }

    #[test]
    fn a_pane_child_does_not_inherit_the_link() {
        // These take the lowest free numbers, so neither end shares one with the two that ls
        // opens for itself, which are the lowest free in the child.
        let _low = [(); 2].map(|()| fs::File::open("/dev/null").unwrap());
        let (ours, theirs) = socket::pair().unwrap();
        let ends = [ours.as_raw_fd(), theirs.as_raw_fd()];
        let mut reactor = Reactor::new().unwrap();
        reactor.attach_window(ours, None).unwrap();
        let script = "ls -1 /dev/fd; echo checked; sleep 30";
        let pane = reactor.spawn(shell(script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "checked")));
        let engine = reactor.engine(pane).unwrap();
        let open: Vec<i32> = (0..24)
            .filter_map(|line| engine.line_text(line).parse().ok())
            .collect();
        assert!(open.contains(&2), "{open:?}");
        assert!(ends.iter().all(|end| !open.contains(end)), "{open:?}");
    }

    #[test]
    fn the_largest_frame_fits_the_cap_with_two_selections() {
        let reply = NOTICE_HEAD + MAX_SELECTION;
        assert!(MAX_PENDING >= size_of::<u32>() + MAX_MESSAGE + 2 * reply);
    }
}
