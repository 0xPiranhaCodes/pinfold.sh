//! The window's end of the link to the core (ADR-0028), with the operations of `Reactor`.
//!
//! [`Link`] sends requests, and applies what the core sends to a copy of the screen. Bytes from
//! the core are untrusted: anything wrong ends the link, and [`Link::turn`] says why as a value.
//! The last screen stays readable after that.
//!
//! ```
//! use std::io::Write;
//! use std::os::unix::net::UnixStream;
//! use std::time::Duration;
//!
//! use pinfold_proto::Frame;
//! use pinfold_proto::link::{Notice, VERSION};
//! use pinfold_ui::link::Link;
//!
//! let (ours, mut core) = UnixStream::pair()?;
//! let mut link = Link::new(ours.into(), 0)?;
//!
//! let mut wire = Vec::new();
//! Notice::Hello { version: VERSION }.encode(&mut wire)?;
//! core.write_all(&wire)?;
//! link.turn(Some(Duration::from_secs(1)))?;
//! assert!(link.is_greeted());
//! assert_eq!(link.frame(), &Frame::default());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::io::{self, ErrorKind, Read, Write};
use std::ops::Range;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use pinfold_proto::Frame;
use pinfold_proto::link::{
    Decoder, Direction, Error, MAX_DIMENSION, MAX_REQUEST, Notice, Position, Request, Selection,
    VERSION,
};

/// Most bytes read per turn, so a flooding core cannot hold the window's thread.
const READ_BUDGET: usize = 64 * 1024;
/// Most encoded bytes held for a core that is not reading. Two full pastes, so a resize still
/// fits behind one. A request past this is refused whole, as a cut message would end the link.
const MAX_QUEUE: usize = 2 * MAX_REQUEST;
/// Queue capacity an empty queue keeps, so one large paste does not cost memory for good.
const KEPT_QUEUE: usize = 4 * 1024;

const LINK: Token = Token(0);
const WAKE: Token = Token(1);

/// Why a link ended. This holds sizes and tags, never pane contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Disconnect {
    /// The core closed the link between messages.
    #[error("the core closed the link")]
    Closed,
    /// The core sent something the protocol refuses, or closed in the middle of a message.
    #[error("the core broke the protocol: {0}")]
    Protocol(#[from] Error),
    /// The socket or its poller failed.
    #[error("the link failed: {0}")]
    Io(ErrorKind),
}

/// What the window knows of its pane, and what it has to say.
#[derive(Default)]
struct State {
    pane: u32,
    /// Encoded requests the socket has not taken yet.
    out: Vec<u8>,
    frame: Frame,
    top: usize,
    held: Range<usize>,
    cwd: String,
    dirty: bool,
    greeted: bool,
    exited: bool,
    /// Whether a full frame is on its way, so a burst of changes asks once.
    asked_full: bool,
    searched: Option<Option<Selection>>,
    copied: Option<String>,
    /// Copies asked for and not answered.
    copying: u32,
    raised: bool,
}

impl State {
    /// Adds a request to the queue, or returns false and leaves the queue as it was.
    fn queue(&mut self, request: Request<'_>) -> bool {
        let start = self.out.len();
        if request.encode(&mut self.out).is_err() {
            return false;
        }
        if self.out.len() > MAX_QUEUE {
            self.out.truncate(start);
            return false;
        }
        true
    }

    fn apply(&mut self, notice: Notice<'_>) -> Result<(), Error> {
        match notice {
            Notice::Hello { .. } => self.greeted = true,
            Notice::Frame(update) if update.pane == self.pane => {
                // The window adds screen rows to this without checking.
                if update.top.checked_add(u16::MAX.into()).is_none() {
                    return Err(Error::OutOfBounds {
                        field: "top",
                        value: update.top as u64,
                    });
                }
                if update.apply(&mut self.frame) {
                    self.asked_full &= !update.full;
                    self.top = update.top;
                    self.held = update.held;
                    self.dirty = true;
                } else if !self.asked_full {
                    let pane = self.pane;
                    self.asked_full = self.queue(Request::FullFrame { pane });
                }
            }
            Notice::Cwd { pane, path } if pane == self.pane && path != self.cwd => {
                path.clone_into(&mut self.cwd);
                self.dirty = true;
            }
            Notice::Searched { pane, found } if pane == self.pane => self.searched = Some(found),
            // Only a key copies, so a core cannot fill the clipboard on its own.
            Notice::Copied { pane, text } if pane == self.pane && self.copying > 0 => {
                self.copying -= 1;
                self.copied = Some(text.to_owned());
            }
            Notice::Exited { pane } if pane == self.pane => self.exited = true,
            Notice::Raise => self.raised = true,
            _ => {}
        }
        Ok(())
    }
}

/// One pane of the core, seen through the link. A single thread drives it, as with `Reactor`.
// shortcut: one pane per link, key `State` by pane when the window gets splits.
pub struct Link {
    /// Gone once the link ends, so the core sees it close.
    stream: Option<UnixStream>,
    poll: Poll,
    waker: Arc<Waker>,
    events: Events,
    decoder: Decoder,
    buffer: Vec<u8>,
    /// The socket may hold more than the last pass read. Polling is edge-triggered.
    readable: bool,
    /// Whether the decoder holds part of a message.
    partial: bool,
    ended: Option<Disconnect>,
    state: State,
}

impl Link {
    /// Takes over a connected socket for `pane`, and says hello at once.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be set up or polled.
    pub fn new(fd: OwnedFd, pane: u32) -> io::Result<Self> {
        let mut link = Self::silent(fd, pane)?;
        link.greet();
        Ok(link)
    }

    /// Takes over a connected socket for `pane`, and sends nothing until [`Link::greet`].
    ///
    /// # Errors
    ///
    /// Returns what [`Link::new`] returns.
    pub fn silent(fd: OwnedFd, pane: u32) -> io::Result<Self> {
        // The copy is close-on-exec, as ADR-0028 asks of an inherited descriptor.
        let stream = UnixStream::from(fd.try_clone()?);
        drop(fd);
        stream.set_nonblocking(true)?;
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        let interest = Interest::READABLE | Interest::WRITABLE;
        poll.registry()
            .register(&mut SourceFd(&stream.as_raw_fd()), LINK, interest)?;
        let state = State {
            pane,
            ..State::default()
        };
        Ok(Self {
            stream: Some(stream),
            poll,
            waker,
            events: Events::with_capacity(4),
            decoder: Decoder::default(),
            buffer: vec![0; READ_BUDGET],
            readable: true,
            partial: false,
            ended: None,
            state,
        })
    }

    /// Sends the version check and then a request for a full frame, as this window has no
    /// screen to apply changes to. The core tells a launcher a window has opened on this.
    pub fn greet(&mut self) {
        let pane = self.state.pane;
        self.state.queue(Request::Hello { version: VERSION });
        self.state.asked_full = self.state.queue(Request::FullFrame { pane });
        self.flush();
    }

    /// Waits for the core for up to `timeout`, reads up to one budget and sends what it can.
    ///
    /// # Errors
    ///
    /// Returns why the link ended, at once and on every later call, so a loop cannot block on a
    /// dead link. The window can still read the last screen, and a new [`Link`] starts over.
    pub fn turn(&mut self, timeout: Option<Duration>) -> Result<(), Disconnect> {
        if let Some(reason) = self.ended {
            return Err(reason);
        }
        let timeout = if self.readable {
            Some(Duration::ZERO)
        } else {
            timeout
        };
        match self.poll.poll(&mut self.events, timeout) {
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(()),
            Err(error) => self.end(Disconnect::Io(error.kind())),
            Ok(()) => {
                self.readable |= self.events.iter().any(|event| event.token() == LINK);
                if self.readable {
                    self.read();
                }
                self.flush();
            }
        }
        self.ended.map_or(Ok(()), Err)
    }

    fn read(&mut self) {
        let mut left = self.buffer.len();
        while left > 0 {
            let Some(mut stream) = self.stream.as_ref() else {
                return;
            };
            match stream.read(&mut self.buffer[..left]) {
                // Linux reports a reset if the core closed with requests unread.
                Ok(0) => return self.closed(),
                Err(error) if error.kind() == ErrorKind::ConnectionReset => return self.closed(),
                Ok(count) => {
                    left -= count;
                    if let Err(error) = self.decode(count) {
                        return self.end(error.into());
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    self.readable = false;
                    return;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return self.end(Disconnect::Io(error.kind())),
            }
        }
    }

    /// Applies each message in the first `count` bytes of the buffer.
    fn decode(&mut self, count: usize) -> Result<(), Error> {
        let mut input = &self.buffer[..count];
        loop {
            let before = input.len();
            match self.decoder.next_notice(&mut input)? {
                Some(notice) => self.state.apply(notice)?,
                None => {
                    self.partial = before > 0;
                    return Ok(());
                }
            }
        }
    }

    fn closed(&mut self) {
        self.end(if self.partial {
            Error::Truncated.into()
        } else {
            Disconnect::Closed
        });
    }

    /// Closes the socket, which is how the core learns of a bad message. Only the first reason
    /// counts.
    fn end(&mut self, reason: Disconnect) {
        self.ended.get_or_insert(reason);
        self.state.out = Vec::new();
        if let Some(stream) = self.stream.take() {
            let fd = stream.as_raw_fd();
            let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
        }
    }

    /// Writes what it can without blocking, and keeps the rest for the next turn.
    fn flush(&mut self) {
        // The runtime ignores SIGPIPE, so a closed core shows up here as an error.
        while let (Some(mut stream), false) = (self.stream.as_ref(), self.state.out.is_empty()) {
            match stream.write(&self.state.out) {
                Ok(0) => return,
                Ok(count) => {
                    self.state.out.drain(..count);
                }
                Err(error) => match error.kind() {
                    ErrorKind::WouldBlock => return,
                    ErrorKind::Interrupted => {}
                    // The core may have said why before it closed, so a read decides what ended.
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
                        self.state.out.clear();
                        self.readable = true;
                    }
                    kind => self.end(Disconnect::Io(kind)),
                },
            }
        }
        if self.state.out.is_empty() && self.state.out.capacity() > KEPT_QUEUE {
            self.state.out = Vec::new();
        }
    }

    /// Queues a request and sends what it can. Returns false if it was refused whole.
    fn send(&mut self, request: Request<'_>) -> bool {
        let queued = self.stream.is_some() && self.state.queue(request);
        self.flush();
        queued
    }

    /// Returns a handle that makes a blocked [`Link::turn`] return, from any thread.
    #[must_use]
    pub fn waker(&self) -> Arc<Waker> {
        Arc::clone(&self.waker)
    }

    /// Queues bytes for the pane's program. Returns false if they are over the limit of one
    /// message, the core is too far behind, or the link has ended, and then queues none.
    pub fn write(&mut self, bytes: &[u8]) -> bool {
        let pane = self.state.pane;
        self.send(Request::Input { pane, bytes })
    }

    /// Queues text to paste, which the core sanitizes. Returns what [`Link::write`] returns.
    pub fn paste(&mut self, text: &str) -> bool {
        let pane = self.state.pane;
        self.send(Request::Paste { pane, text })
    }

    /// Asks for a new size, clamped to what the core accepts. Returns what [`Link::write`]
    /// returns.
    pub fn resize(&mut self, rows: u16, columns: u16) -> bool {
        self.send(Request::Resize {
            pane: self.state.pane,
            rows: rows.clamp(1, MAX_DIMENSION),
            columns: columns.clamp(1, MAX_DIMENSION),
        })
    }

    /// Moves the view into history, or back if negative. Returns what [`Link::write`] returns.
    pub fn scroll(&mut self, lines: i32) -> bool {
        let pane = self.state.pane;
        self.send(Request::Scroll { pane, lines })
    }

    /// Scrolls just far enough to show `row`. Returns what [`Link::write`] returns.
    pub fn reveal(&mut self, row: usize) -> bool {
        let pane = self.state.pane;
        self.send(Request::Reveal { pane, row })
    }

    /// Replaces any search running. Returns what [`Link::write`] returns.
    pub fn search(&mut self, query: &str, from: Position, direction: Direction) -> bool {
        let pane = self.state.pane;
        self.send(Request::Search {
            pane,
            from,
            direction,
            query,
        })
    }

    /// Asks for a selection's text, for [`Link::take_copied`]. Returns what [`Link::write`]
    /// returns.
    pub fn copy(&mut self, selection: Selection) -> bool {
        let pane = self.state.pane;
        let queued = self.send(Request::Copy { pane, selection });
        self.state.copying = self.state.copying.saturating_add(queued.into());
        queued
    }

    /// Tells the core this window is closing. Returns what [`Link::write`] returns.
    pub fn quit(&mut self) -> bool {
        self.send(Request::Quit)
    }

    /// Returns whether the screen, its position or the directory changed since the last call,
    /// and clears that.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.state.dirty)
    }

    /// Returns the last screen received, which is empty until the first frame.
    #[must_use]
    pub fn frame(&self) -> &Frame {
        &self.state.frame
    }

    /// Returns the core's number for the first row of [`Link::frame`].
    #[must_use]
    pub fn top(&self) -> usize {
        self.state.top
    }

    /// Returns the rows the view can reach.
    #[must_use]
    pub fn held(&self) -> Range<usize> {
        self.state.held.clone()
    }

    /// Returns the last directory the pane reported, as program output wrote it. Empty if none.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.state.cwd
    }

    /// Returns how the last search ended, once, and nothing while none has. Its match is in view
    /// if it had one.
    pub fn take_searched(&mut self) -> Option<Option<Selection>> {
        self.state.searched.take()
    }

    /// Returns the text of the last copy asked for, once.
    pub fn take_copied(&mut self) -> Option<String> {
        self.state.copied.take()
    }

    /// Returns whether someone asked to bring this window to the front since the last call.
    pub fn take_raised(&mut self) -> bool {
        std::mem::take(&mut self.state.raised)
    }

    /// Returns whether the core has answered the version check.
    #[must_use]
    pub fn is_greeted(&self) -> bool {
        self.state.greeted
    }

    /// Returns whether the pane's program exited or the link ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.exited || self.ended.is_some()
    }

    /// Returns why the link ended, if it has. [`Link::is_closed`] alone cannot tell a program
    /// that exited from a core to reconnect to.
    #[must_use]
    pub fn disconnected(&self) -> Option<Disconnect> {
        self.ended
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

    use pinfold_proto::Cell;
    use pinfold_proto::link::{MAX_MESSAGE, MAX_PASTE, encode_frame};

    use super::*;

    const PANE: u32 = 3;
    const PATIENCE: Duration = Duration::from_secs(10);

    /// The core's end of the link, as a test drives it.
    struct Peer {
        stream: UnixStream,
        decoder: Decoder,
    }

    impl Peer {
        fn notices(&mut self, notices: &[Notice<'_>]) -> io::Result<()> {
            let mut wire = Vec::new();
            for notice in notices {
                notice.encode(&mut wire).map_err(io::Error::other)?;
            }
            self.stream.write_all(&wire)
        }

        fn frame(&mut self, frame: &Frame, top: usize, previous: Option<&Frame>) -> io::Result<()> {
            let mut wire = Vec::new();
            encode_frame(&mut wire, PANE, frame, top, 1..9, previous).map_err(io::Error::other)?;
            self.stream.write_all(&wire)
        }

        fn greet(&mut self) -> io::Result<()> {
            self.notices(&[Notice::Hello { version: VERSION }])
        }

        /// Reads until `count` requests have arrived, and shows them for comparing.
        fn requests(&mut self, count: usize) -> Vec<String> {
            let (mut seen, mut chunk) = (Vec::new(), vec![0; READ_BUDGET]);
            while seen.len() < count {
                let Ok(read @ 1..) = self.stream.read(&mut chunk) else {
                    break;
                };
                let mut input = &chunk[..read];
                while let Ok(Some(request)) = self.decoder.next_request(&mut input) {
                    seen.push(format!("{request:?}"));
                }
            }
            seen
        }

        /// Whether the other end has closed, which an unread message would hide.
        fn sees_close(&mut self) -> bool {
            self.stream.read_to_end(&mut Vec::new()).is_ok()
        }
    }

    fn linked() -> io::Result<(Link, Peer)> {
        let (ours, stream) = UnixStream::pair()?;
        stream.set_read_timeout(Some(PATIENCE))?;
        let decoder = Decoder::default();
        Ok((Link::new(ours.into(), PANE)?, Peer { stream, decoder }))
    }

    fn turn_until(link: &mut Link, mut done: impl FnMut(&mut Link) -> bool) -> bool {
        let start = Instant::now();
        while !done(link) {
            let turn = link.turn(Some(Duration::from_millis(20)));
            if turn.is_err() || start.elapsed() > PATIENCE {
                return done(link);
            }
        }
        true
    }

    fn ended(link: &mut Link) -> Option<Disconnect> {
        turn_until(link, |link| link.disconnected().is_some());
        link.disconnected()
    }

    fn screen(character: char) -> Frame {
        let cell = Cell {
            character,
            foreground: 0,
            background: 0,
        };
        Frame {
            rows: 2,
            columns: 3,
            cells: vec![cell; 6],
            cursor: Some((1, 2)),
            ..Frame::default()
        }
    }

    fn shown(request: Request<'_>) -> String {
        format!("{request:?}")
    }

    #[test]
    fn the_version_check_goes_first_each_way() {
        let (mut link, mut peer) = linked().unwrap();
        let hello = Request::Hello { version: VERSION };
        let full = Request::FullFrame { pane: PANE };
        assert_eq!(peer.requests(2), [shown(hello), shown(full)]);
        assert!(!link.is_greeted());
        peer.greet().unwrap();
        assert!(turn_until(&mut link, |link| link.is_greeted()));
        assert_eq!(link.disconnected(), None);
    }

    #[test]
    fn a_silent_link_says_hello_only_when_told() {
        let (ours, stream) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut link = Link::silent(ours.into(), PANE).unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        let quiet = (&stream).read(&mut [0; 8]).unwrap_err();
        assert_eq!(quiet.kind(), ErrorKind::WouldBlock);
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        link.greet();
        let decoder = Decoder::default();
        let hello = Request::Hello { version: VERSION };
        let full = Request::FullFrame { pane: PANE };
        let mut peer = Peer { stream, decoder };
        assert_eq!(peer.requests(2), [shown(hello), shown(full)]);
    }

    #[test]
    fn a_link_can_move_to_the_io_thread() {
        fn sendable<T: Send>() {}
        sendable::<Link>();
    }

    #[test]
    fn frames_are_applied_to_the_local_copy() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let first = screen('a');
        peer.frame(&first, 5, None).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
        assert_eq!(link.frame(), &first);
        assert_eq!((link.top(), link.held()), (5, 1..9));
        assert!(!link.take_dirty());

        let mut second = first.clone();
        second.cells[4].character = 'b';
        peer.frame(&second, 6, Some(&first)).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
        assert_eq!(link.frame(), &second);
        assert_eq!(link.top(), 6);
    }

    #[test]
    fn what_a_budget_left_is_read_without_a_new_event() {
        let (mut link, mut peer) = linked().unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        link.buffer.truncate(8);
        peer.greet().unwrap();
        peer.frame(&screen('a'), 0, None).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
    }

    #[test]
    fn a_frame_larger_than_a_budget_arrives_across_turns() {
        let (mut link, mut peer) = linked().unwrap();
        let mut frame = screen('a');
        frame.columns = 1024;
        frame.rows = 128;
        frame.cells = vec![frame.cells[0]; 1024 * 128];
        frame.cursor = None;
        let sent = frame.clone();
        let writer = thread::spawn(move || {
            peer.greet().unwrap();
            peer.frame(&sent, 0, None).unwrap();
            peer
        });
        assert!(turn_until(&mut link, Link::take_dirty));
        assert_eq!(link.frame(), &frame);
        writer.join().unwrap();
    }

    #[test]
    fn requests_reach_the_core_in_order() {
        let (mut link, mut peer) = linked().unwrap();
        peer.requests(2);
        let (from, selection) = (Position::default(), Selection::default());
        assert!(link.write(b"ls\n"));
        assert!(link.paste("a\nb"));
        assert!(link.resize(24, 80));
        assert!(link.scroll(i32::MIN));
        assert!(link.reveal(7));
        assert!(link.search("x", from, Direction::Backward));
        assert!(link.copy(selection));
        assert!(link.quit());
        let pane = PANE;
        let expected = [
            Request::Input {
                pane,
                bytes: b"ls\n",
            },
            Request::Paste { pane, text: "a\nb" },
            Request::Resize {
                pane,
                rows: 24,
                columns: 80,
            },
            Request::Scroll {
                pane,
                lines: i32::MIN,
            },
            Request::Reveal { pane, row: 7 },
            Request::Search {
                pane,
                from,
                direction: Direction::Backward,
                query: "x",
            },
            Request::Copy { pane, selection },
            Request::Quit,
        ];
        assert_eq!(peer.requests(8), expected.map(shown));
    }

    #[test]
    fn a_resize_is_clamped_and_an_oversized_paste_refused() {
        let (mut link, mut peer) = linked().unwrap();
        peer.requests(2);
        assert!(!link.paste(&"x".repeat(MAX_PASTE + 1)));
        assert!(link.resize(0, u16::MAX));
        let clamped = Request::Resize {
            pane: PANE,
            rows: 1,
            columns: MAX_DIMENSION,
        };
        assert_eq!(peer.requests(1), [shown(clamped)]);
    }

    #[test]
    fn a_core_that_stops_reading_refuses_requests_whole_and_loses_none() {
        let (mut link, mut peer) = linked().unwrap();
        let text = "x".repeat(MAX_PASTE);
        let mut accepted = 0;
        while link.paste(&text) {
            accepted += 1;
            assert!(accepted < 16, "the queue has no limit");
        }
        assert!(accepted >= 2);
        let reader = thread::spawn(move || peer.requests(accepted + 2));
        turn_until(&mut link, |_| reader.is_finished());
        let requests = reader.join().unwrap();
        assert_eq!(requests.len(), accepted + 2);
        assert!(requests[2..].iter().all(|paste| paste.len() > MAX_PASTE));
    }

    #[test]
    fn a_core_that_closes_between_messages_is_reported() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        drop(peer);
        assert_eq!(ended(&mut link), Some(Disconnect::Closed));
        assert!(link.is_closed());
    }

    #[test]
    fn a_core_that_closes_mid_message_is_reported_and_the_screen_stays() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let frame = screen('a');
        peer.frame(&frame, 0, None).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));

        let mut wire = Vec::new();
        encode_frame(&mut wire, PANE, &screen('b'), 0, 1..9, None).unwrap();
        peer.stream.write_all(&wire[..wire.len() / 2]).unwrap();
        drop(peer);
        let error = Some(Disconnect::Protocol(Error::Truncated));
        assert_eq!(ended(&mut link), error);
        assert_eq!(link.frame(), &frame);
        assert_eq!(link.turn(None), Err(Disconnect::Protocol(Error::Truncated)));
        assert!(!link.write(b"x"));
    }

    #[test]
    fn a_bad_version_ends_the_link_and_closes_it() {
        let (mut link, mut peer) = linked().unwrap();
        let theirs = VERSION + 1;
        peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
        let error = Error::Version {
            ours: VERSION,
            theirs,
        };
        assert_eq!(ended(&mut link), Some(error.into()));
        assert!(peer.sees_close());
        assert!(!link.is_greeted());
    }

    #[test]
    fn a_length_over_the_limit_ends_the_link_and_closes_it() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let length = u32::try_from(MAX_MESSAGE + 1).unwrap();
        peer.stream.write_all(&length.to_le_bytes()).unwrap();
        let error = Error::TooLarge {
            size: length.into(),
        };
        assert_eq!(ended(&mut link), Some(error.into()));
        assert!(peer.sees_close());
    }

    #[test]
    fn a_failed_write_does_not_hide_why_the_core_left() {
        let (mut link, mut peer) = linked().unwrap();
        let theirs = VERSION + 1;
        peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
        drop(peer);
        link.write(b"x");
        link.write(b"x");
        let error = Error::Version {
            ours: VERSION,
            theirs,
        };
        assert_eq!(ended(&mut link), Some(error.into()));
    }

    #[test]
    fn a_top_the_window_cannot_add_to_ends_the_link() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        peer.frame(&screen('a'), usize::MAX, None).unwrap();
        assert!(matches!(
            ended(&mut link),
            Some(Disconnect::Protocol(Error::OutOfBounds {
                field: "top",
                ..
            }))
        ));
        assert_eq!(link.frame(), &Frame::default());
    }

    #[test]
    fn changes_to_a_screen_of_another_size_ask_for_a_full_frame_once() {
        let (mut link, mut peer) = linked().unwrap();
        peer.requests(2);
        peer.greet().unwrap();
        let first = screen('a');
        peer.frame(&first, 0, None).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));

        // A window that lost track: this one's screen was another size.
        link.state.frame.rows = 3;
        let mut second = first.clone();
        second.cells[0].character = 'b';
        for _ in 0..2 {
            peer.frame(&second, 0, Some(&first)).unwrap();
        }
        peer.notices(&[Notice::Cwd {
            pane: PANE,
            path: "/marker",
        }])
        .unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
        assert!(link.write(b"after"));
        let (pane, bytes) = (PANE, b"after".as_slice());
        let (full, input) = (Request::FullFrame { pane }, Request::Input { pane, bytes });
        assert_eq!(peer.requests(2), [shown(full), shown(input)]);

        peer.frame(&second, 0, None).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
        assert_eq!(link.frame(), &second);
        assert!(!link.state.asked_full);
    }

    #[test]
    fn notices_for_another_pane_change_nothing() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (pane, path) = (PANE + 1, "/other");
        let mut wire = Vec::new();
        encode_frame(&mut wire, pane, &screen('a'), 0, 1..9, None).unwrap();
        peer.stream.write_all(&wire).unwrap();
        peer.notices(&[Notice::Cwd { pane, path }, Notice::Exited { pane }])
            .unwrap();
        let (pane, path) = (PANE, "/ours");
        peer.notices(&[Notice::Cwd { pane, path }]).unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
        assert_eq!(link.cwd(), path);
        assert_eq!(link.frame(), &Frame::default());
        assert!(!link.is_closed());
    }

    #[test]
    fn the_rest_of_the_state_is_kept_for_the_window() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (pane, path) = (PANE, "/home");
        let found = Some(Selection::default());
        peer.notices(&[Notice::Cwd { pane, path }, Notice::Searched { pane, found }])
            .unwrap();
        assert!(turn_until(&mut link, |link| link.state.searched.is_some()));
        assert!(link.take_dirty());
        assert_eq!(link.cwd(), path);
        assert_eq!(link.take_searched(), Some(found));
        assert_eq!(link.take_searched(), None);

        peer.notices(&[Notice::Cwd { pane, path }, Notice::Exited { pane }])
            .unwrap();
        assert!(turn_until(&mut link, |link| link.is_closed()));
        assert!(!link.take_dirty(), "the same directory changes nothing");
        assert_eq!(link.disconnected(), None, "an exit is not a disconnect");
    }

    #[test]
    fn a_raise_is_reported_once() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        peer.notices(&[Notice::Raise]).unwrap();
        assert!(turn_until(&mut link, |link| link.state.raised));
        assert!(link.take_raised());
        assert!(!link.take_raised());
    }

    #[test]
    fn only_a_copy_asked_for_can_reach_the_clipboard() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (pane, path) = (PANE, "/first");
        let (asked, unasked) = ("asked", "unasked");
        peer.notices(&[
            Notice::Copied {
                pane,
                text: unasked,
            },
            Notice::Cwd { pane, path },
        ])
        .unwrap();
        assert!(turn_until(&mut link, Link::take_dirty));
        assert_eq!(link.take_copied(), None);

        assert!(link.copy(Selection::default()));
        peer.notices(&[
            Notice::Copied { pane, text: asked },
            Notice::Copied {
                pane,
                text: unasked,
            },
            Notice::Exited { pane },
        ])
        .unwrap();
        assert!(turn_until(&mut link, |link| link.is_closed()));
        assert_eq!(link.take_copied().as_deref(), Some(asked));
        assert_eq!(link.take_copied(), None);
    }

    #[test]
    fn a_waker_ends_a_blocked_turn() {
        let (mut link, _peer) = linked().unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        let waker = link.waker();
        let wake = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            waker.wake().unwrap();
        });
        let start = Instant::now();
        link.turn(Some(Duration::from_secs(20))).unwrap();
        assert!(start.elapsed() < PATIENCE);
        wake.join().unwrap();
    }
}
