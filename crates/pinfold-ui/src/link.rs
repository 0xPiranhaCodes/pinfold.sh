//! The window's end of the link to the core (ADR-0028), with the operations of `Reactor`.
//!
//! [`Link`] sends requests, and keeps a copy of the tabs and of each pane the active tab shows,
//! so a hidden pane costs nothing here. Bytes from the core are untrusted: anything wrong ends
//! the link, and [`Link::turn`] says why as a value. The last screen stays readable after that.
//!
//! ```
//! use std::io::Write;
//! use std::os::unix::net::UnixStream;
//! use std::time::Duration;
//!
//! use pinfold_proto::link::{Notice, VERSION};
//! use pinfold_ui::link::Link;
//!
//! let (ours, mut core) = UnixStream::pair()?;
//! let mut link = Link::new(ours.into())?;
//!
//! let mut wire = Vec::new();
//! Notice::Hello { version: VERSION }.encode(&mut wire)?;
//! core.write_all(&wire)?;
//! link.turn(Some(Duration::from_secs(1)))?;
//! assert!(link.is_greeted());
//! assert!(link.screens().is_empty());
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
    ApprovalAction, Decoder, Error, Layout, MAX_DIMENSION, MAX_REQUEST, Notice, Rect, Refusal,
    Request, Selection, VERSION,
};

/// Most bytes read per turn, so a flooding core cannot hold the window's thread.
const READ_BUDGET: usize = 64 * 1024;
/// Most encoded bytes held for a core that is not reading. Two full pastes, so a resize still
/// fits behind one. A request past this is refused whole, as a cut message would end the link.
const MAX_QUEUE: usize = 2 * MAX_REQUEST;
/// Queue capacity an empty queue keeps, so one large paste does not cost memory for good.
const KEPT_QUEUE: usize = 4 * 1024;
/// The bytes of the longest request, with its length.
const LONGEST: usize = size_of::<u32>() + MAX_REQUEST;

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

/// A pane the active tab shows.
#[derive(Debug, Default)]
pub struct Screen {
    /// The pane.
    pub pane: u32,
    /// Where it goes in the space for panes.
    pub rect: Rect,
    /// The last screen received, which is empty until the first frame.
    pub frame: Frame,
    /// The core's number for the first row of `frame`.
    pub top: usize,
    /// The rows the view can reach.
    pub held: Range<usize>,
    /// Whether `frame` changed since this was last cleared.
    pub dirty: bool,
    /// Whether a full frame is on its way, so a burst of changes asks once.
    asked_full: bool,
}

/// Makes `screens` hold the panes of `shown`, in new rectangles. A pane that stays keeps what it
/// holds, one that went is freed, and a new one starts empty.
pub fn follow(screens: &mut Vec<Screen>, shown: impl Iterator<Item = (u32, Rect)>) {
    let shown: Vec<_> = shown.collect();
    screens.retain(|screen| shown.iter().any(|(pane, _)| *pane == screen.pane));
    for (pane, rect) in shown {
        match screens.iter_mut().find(|screen| screen.pane == pane) {
            Some(screen) => screen.rect = rect,
            None => screens.push(Screen {
                pane,
                rect,
                ..Screen::default()
            }),
        }
    }
}

/// A tab, as the tab strip shows it.
#[derive(Debug, Default)]
pub struct Tab {
    /// The pane with the tab's focus.
    pub pane: u32,
    /// That pane's last reported directory, as program output wrote it. Empty if none.
    pub cwd: String,
}

/// An approval copied out of the decoder's borrowed message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// Stable prompt id.
    pub id: u64,
    /// Pane asking.
    pub caller: u32,
    /// Target pane.
    pub target: u32,
    /// Cross-pane action.
    pub action: ApprovalAction,
    /// Exact batched sends, empty for reads.
    pub texts: Vec<String>,
}

impl Approval {
    /// Whether `other` is this prompt, possibly grown by more requests. Trust prompts never
    /// grow, so each is its own.
    #[must_use]
    pub fn same_batch(&self, other: &Self) -> bool {
        if self.action == ApprovalAction::Trust || other.action == ApprovalAction::Trust {
            return self.id == other.id;
        }
        (self.caller, self.target, self.action) == (other.caller, other.target, other.action)
    }
}

/// What the window knows of the session, and what it has to say.
#[derive(Default)]
struct State {
    /// Encoded requests the socket has not taken yet.
    out: Vec<u8>,
    screens: Vec<Screen>,
    tabs: Vec<Tab>,
    active: usize,
    focused: u32,
    total: usize,
    /// Whether the session's shape, a tab's name or its news changed since it was last read.
    laid: bool,
    /// Which tabs have news, one bit each.
    activity: u32,
    greeted: bool,
    /// Whether the core said no tab is left.
    over: bool,
    searched: Option<(u32, Option<Selection>)>,
    copied: Option<String>,
    /// Copies asked for and not answered.
    copying: u32,
    raised: bool,
    refused: Option<Refusal>,
    approvals: Vec<Approval>,
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
            Notice::Frame(update) => {
                // The window adds screen rows to this without checking.
                if update.top.checked_add(u16::MAX.into()).is_none() {
                    return Err(Error::OutOfBounds {
                        field: "top",
                        value: update.top as u64,
                    });
                }
                let mut shown = self.screens.iter_mut();
                let Some(screen) = shown.find(|screen| screen.pane == update.pane) else {
                    return Ok(());
                };
                if update.apply(&mut screen.frame) {
                    screen.asked_full &= !update.full;
                    screen.top = update.top;
                    screen.held = update.held;
                    screen.dirty = true;
                } else if !screen.asked_full {
                    // A message this small only misses a queue that is full, and then asks again.
                    screen.asked_full = true;
                    let pane = screen.pane;
                    let queued = self.queue(Request::FullFrame { pane });
                    if let Some(screen) = self.screens.iter_mut().find(|s| s.pane == pane) {
                        screen.asked_full = queued;
                    }
                }
            }
            Notice::Layout(layout) => self.follow(&layout),
            Notice::Cwd { pane, path } => {
                let mut tabs = self.tabs.iter_mut();
                if let Some(tab) = tabs.find(|tab| tab.pane == pane && tab.cwd != path) {
                    path.clone_into(&mut tab.cwd);
                    self.laid = true;
                }
            }
            Notice::Searched { pane, found } => self.searched = Some((pane, found)),
            // Only a key copies, so a core cannot fill the clipboard on its own.
            Notice::Copied { text, .. } if self.copying > 0 => {
                self.copying -= 1;
                self.copied = Some(text.to_owned());
            }
            Notice::Raise => self.raised = true,
            Notice::Activity { tabs } => {
                self.activity = tabs;
                self.laid = true;
            }
            Notice::Refused { reason } => self.refused = Some(reason),
            Notice::Approval(approval) => {
                let owned = Approval {
                    id: approval.id,
                    caller: approval.caller,
                    target: approval.target,
                    action: approval.action,
                    texts: approval
                        .texts
                        .iter()
                        .map(|text| (*text).to_owned())
                        .collect(),
                };
                // A batch that grew comes under a new id; it replaces the one shown.
                match self
                    .approvals
                    .iter_mut()
                    .find(|known| known.same_batch(&owned))
                {
                    Some(known) => *known = owned,
                    None => self.approvals.push(owned),
                }
            }
            Notice::Copied { .. } => {}
        }
        Ok(())
    }

    fn follow(&mut self, layout: &Layout<'_>) {
        self.active = layout.active.into();
        self.focused = layout.focused;
        self.total = layout.total.into();
        self.over = layout.is_empty();
        self.laid = true;
        follow(&mut self.screens, layout.panes());
        let mut known = std::mem::take(&mut self.tabs);
        self.tabs.extend(layout.tabs().map(|pane| {
            let same = known.iter_mut().find(|tab| tab.pane == pane);
            same.map_or(
                Tab {
                    pane,
                    cwd: String::new(),
                },
                std::mem::take,
            )
        }));
    }
}

/// A session of the core, seen through the link. A single thread drives it, as with `Reactor`.
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
    /// Takes over a connected socket, and says hello at once. The core says what to show once
    /// [`Link::resize`] says how much room there is.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be set up or polled.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let mut link = Self::silent(fd)?;
        link.greet();
        Ok(link)
    }

    /// Takes over a connected socket, and sends nothing until [`Link::greet`].
    ///
    /// # Errors
    ///
    /// Returns what [`Link::new`] returns.
    pub fn silent(fd: OwnedFd) -> io::Result<Self> {
        // The copy is close-on-exec, as ADR-0028 asks of an inherited descriptor.
        let stream = UnixStream::from(fd.try_clone()?);
        drop(fd);
        stream.set_nonblocking(true)?;
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        let interest = Interest::READABLE | Interest::WRITABLE;
        poll.registry()
            .register(&mut SourceFd(&stream.as_raw_fd()), LINK, interest)?;
        let state = State::default();
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

    /// Sends the version check. The core tells a launcher a window has opened on this.
    pub fn greet(&mut self) {
        self.state.queue(Request::Hello { version: VERSION });
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

    /// Queues a request and sends what it can. Returns false if it is over the limit of one
    /// message, the core is too far behind, or the link has ended, and then queues none.
    pub fn send(&mut self, request: Request<'_>) -> bool {
        let queued = self.stream.is_some() && self.state.queue(request);
        self.flush();
        queued
    }

    /// Returns whether [`Link::send`] has room for any request, however long. While it has not,
    /// a caller keeps its requests in order and asks again after [`Link::turn`], which returns
    /// when the socket can take more.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.state.out.len() + LONGEST <= MAX_QUEUE
    }

    /// Returns a handle that makes a blocked [`Link::turn`] return, from any thread.
    #[must_use]
    pub fn waker(&self) -> Arc<Waker> {
        Arc::clone(&self.waker)
    }

    /// Gives the space for panes, clamped to what the core accepts. Returns what [`Link::send`]
    /// returns.
    pub fn resize(&mut self, rows: u16, columns: u16) -> bool {
        self.send(Request::Resize {
            rows: rows.clamp(1, MAX_DIMENSION),
            columns: columns.clamp(1, MAX_DIMENSION),
        })
    }

    /// Asks for a selection's text, for [`Link::take_copied`]. Returns what [`Link::send`]
    /// returns.
    pub fn copy(&mut self, pane: u32, selection: Selection) -> bool {
        let queued = self.send(Request::Copy { pane, selection });
        self.state.copying = self.state.copying.saturating_add(queued.into());
        queued
    }

    /// Returns the panes the active tab shows.
    #[must_use]
    pub fn screens(&self) -> &[Screen] {
        &self.state.screens
    }

    /// Returns the panes the active tab shows, to clear what has been read.
    pub fn screens_mut(&mut self) -> &mut [Screen] {
        &mut self.state.screens
    }

    /// Returns the tabs, in order.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.state.tabs
    }

    /// Returns the index of the active tab.
    #[must_use]
    pub fn active(&self) -> usize {
        self.state.active
    }

    /// Returns the pane keys go to, which is 0 until the first layout.
    #[must_use]
    pub fn focused(&self) -> u32 {
        self.state.focused
    }

    /// Returns how many panes the session holds, shown or not.
    #[must_use]
    pub fn total(&self) -> usize {
        self.state.total
    }

    /// Returns which tabs printed something since they were shown, one bit each.
    #[must_use]
    pub fn activity(&self) -> u32 {
        self.state.activity
    }

    /// Returns whether the tabs or the panes shown changed since the last call, and clears that.
    pub fn take_laid(&mut self) -> bool {
        std::mem::take(&mut self.state.laid)
    }

    /// Returns which pane the last search ran in and how it ended, once, and nothing while none
    /// has. Its match is in view if it had one.
    pub fn take_searched(&mut self) -> Option<(u32, Option<Selection>)> {
        self.state.searched.take()
    }

    /// Returns the last command the core refused, once.
    pub fn take_refused(&mut self) -> Option<Refusal> {
        self.state.refused.take()
    }

    /// Takes the oldest new or updated approval prompt.
    pub fn take_approval(&mut self) -> Option<Approval> {
        (!self.state.approvals.is_empty()).then(|| self.state.approvals.remove(0))
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

    /// Returns whether the session ended, with no tab left, or the link did.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.over || self.ended.is_some()
    }

    /// Returns why the link ended, if it has. [`Link::is_closed`] alone cannot tell a session
    /// that ended from a core to reconnect to.
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
    use pinfold_proto::link::{
        Direction, MAX_MESSAGE, MAX_PASTE, Position, encode_frame, encode_layout,
    };

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

        fn no_tabs(&mut self) -> io::Result<()> {
            let mut wire = Vec::new();
            encode_layout(&mut wire, 0, 0, 0, &[], &[]).map_err(io::Error::other)?;
            self.stream.write_all(&wire)
        }

        /// Says hello, and that one tab shows `PANE`.
        fn greet(&mut self) -> io::Result<()> {
            self.notices(&[Notice::Hello { version: VERSION }])?;
            self.layout(0, PANE, 1, &[PANE], &[(PANE, whole())])
        }

        fn layout(
            &mut self,
            active: u16,
            focused: u32,
            total: u16,
            tabs: &[u32],
            panes: &[(u32, Rect)],
        ) -> io::Result<()> {
            let mut wire = Vec::new();
            encode_layout(&mut wire, active, focused, total, tabs, panes)
                .map_err(io::Error::other)?;
            self.stream.write_all(&wire)
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

    fn whole() -> Rect {
        Rect {
            x: 0,
            y: 0,
            columns: 3,
            rows: 2,
        }
    }

    /// Whether the screen of `PANE` changed, which clears that.
    fn take_dirty(link: &mut Link) -> bool {
        let mut screens = link.screens_mut().iter_mut();
        screens
            .find(|screen| screen.pane == PANE)
            .is_some_and(|screen| std::mem::take(&mut screen.dirty))
    }

    fn pane(link: &Link) -> &Screen {
        &link.screens()[0]
    }

    fn linked() -> io::Result<(Link, Peer)> {
        let (ours, stream) = UnixStream::pair()?;
        stream.set_read_timeout(Some(PATIENCE))?;
        let decoder = Decoder::default();
        Ok((Link::new(ours.into())?, Peer { stream, decoder }))
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
        assert_eq!(peer.requests(1), [shown(hello)]);
        assert!(!link.is_greeted());
        peer.greet().unwrap();
        assert!(turn_until(&mut link, |link| link.is_greeted()));
        assert_eq!(link.disconnected(), None);
    }

    #[test]
    fn a_silent_link_says_hello_only_when_told() {
        let (ours, stream) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut link = Link::silent(ours.into()).unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        let quiet = (&stream).read(&mut [0; 8]).unwrap_err();
        assert_eq!(quiet.kind(), ErrorKind::WouldBlock);
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        link.greet();
        let decoder = Decoder::default();
        let hello = Request::Hello { version: VERSION };
        let mut peer = Peer { stream, decoder };
        assert_eq!(peer.requests(1), [shown(hello)]);
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
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, first);
        assert_eq!((pane(&link).top, pane(&link).held.clone()), (5, 1..9));
        assert!(!take_dirty(&mut link));

        let mut second = first.clone();
        second.cells[4].character = 'b';
        peer.frame(&second, 6, Some(&first)).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, second);
        assert_eq!(pane(&link).top, 6);
    }

    #[test]
    fn what_a_budget_left_is_read_without_a_new_event() {
        let (mut link, mut peer) = linked().unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        link.buffer.truncate(8);
        peer.greet().unwrap();
        peer.frame(&screen('a'), 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
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
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, frame);
        writer.join().unwrap();
    }

    #[test]
    fn requests_reach_the_core_in_order() {
        let (mut link, mut peer) = linked().unwrap();
        peer.requests(1);
        let (from, selection) = (Position::default(), Selection::default());
        let pane = PANE;
        let expected = [
            Request::Input {
                pane,
                bytes: b"ls\n",
            },
            Request::Paste { pane, text: "a\nb" },
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
            Request::Quit,
        ];
        for request in expected {
            assert!(link.send(request));
        }
        assert!(link.resize(24, 80));
        assert!(link.copy(pane, selection));
        let mut sent = expected.map(shown).to_vec();
        sent.push(shown(Request::Resize {
            rows: 24,
            columns: 80,
        }));
        sent.push(shown(Request::Copy { pane, selection }));
        assert_eq!(peer.requests(8), sent);
    }

    #[test]
    fn a_resize_is_clamped_and_an_oversized_paste_refused() {
        let (mut link, mut peer) = linked().unwrap();
        peer.requests(1);
        assert!(!link.send(Request::Paste {
            pane: PANE,
            text: &"x".repeat(MAX_PASTE + 1),
        }));
        assert!(link.resize(0, u16::MAX));
        let clamped = Request::Resize {
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
        let paste = Request::Paste {
            pane: PANE,
            text: &text,
        };
        while link.send(paste) {
            accepted += 1;
            assert!(accepted < 16, "the queue has no limit");
        }
        assert!(accepted >= 2);
        let reader = thread::spawn(move || peer.requests(accepted + 1));
        turn_until(&mut link, |_| reader.is_finished());
        let requests = reader.join().unwrap();
        assert_eq!(requests.len(), accepted + 1);
        assert!(requests[1..].iter().all(|paste| paste.len() > MAX_PASTE));
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
        assert!(turn_until(&mut link, take_dirty));

        let mut wire = Vec::new();
        encode_frame(&mut wire, PANE, &screen('b'), 0, 1..9, None).unwrap();
        peer.stream.write_all(&wire[..wire.len() / 2]).unwrap();
        drop(peer);
        let error = Some(Disconnect::Protocol(Error::Truncated));
        assert_eq!(ended(&mut link), error);
        assert_eq!(pane(&link).frame, frame);
        assert_eq!(link.turn(None), Err(Disconnect::Protocol(Error::Truncated)));
        assert!(!link.send(Request::Quit));
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
        link.send(Request::Quit);
        link.send(Request::Quit);
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
        assert_eq!(pane(&link).frame, Frame::default());
    }

    #[test]
    fn changes_to_a_screen_of_another_size_ask_for_a_full_frame_once() {
        let (mut link, mut peer) = linked().unwrap();
        peer.requests(1);
        peer.greet().unwrap();
        let first = screen('a');
        peer.frame(&first, 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));

        // A window that lost track: this one's screen was another size.
        link.screens_mut()[0].frame.rows = 3;
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
        assert!(turn_until(&mut link, |link| link.take_laid()
            && link.tabs()[0].cwd == "/marker"));
        let quit = Request::Quit;
        assert!(link.send(quit));
        let full = Request::FullFrame { pane: PANE };
        assert_eq!(peer.requests(2), [shown(full), shown(quit)]);

        peer.frame(&second, 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, second);
        assert!(!pane(&link).asked_full);
    }

    #[test]
    fn notices_for_a_pane_not_shown_change_nothing() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (other, path) = (PANE + 1, "/other");
        let mut wire = Vec::new();
        encode_frame(&mut wire, other, &screen('a'), 0, 1..9, None).unwrap();
        peer.stream.write_all(&wire).unwrap();
        peer.notices(&[Notice::Cwd { pane: other, path }]).unwrap();
        let (pane_of_tab, path) = (PANE, "/ours");
        let cwd = Notice::Cwd {
            pane: pane_of_tab,
            path,
        };
        peer.notices(&[cwd]).unwrap();
        assert!(turn_until(&mut link, |link| link
            .tabs()
            .first()
            .is_some_and(|tab| tab.cwd == path)));
        assert_eq!(pane(&link).frame, Frame::default());
        assert!(!link.is_closed());
    }

    #[test]
    fn a_layout_keeps_panes_that_stay_and_frees_the_rest() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        peer.frame(&screen('a'), 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert!(link.take_laid());

        let (left, right) = (
            Rect {
                columns: 1,
                ..whole()
            },
            Rect {
                x: 2,
                columns: 1,
                ..whole()
            },
        );
        let tabs = [PANE, PANE + 2];
        let shown = [(PANE, left), (PANE + 1, right)];
        peer.layout(0, PANE + 1, 3, &tabs, &shown).unwrap();
        assert!(turn_until(&mut link, Link::take_laid));
        assert_eq!(link.screens().len(), 2);
        assert_eq!(pane(&link).rect, left);
        assert_eq!(pane(&link).frame, screen('a'), "what it holds stays");
        assert_eq!(link.screens()[1].frame, Frame::default());
        assert_eq!((link.focused(), link.active()), (PANE + 1, 0));
        assert_eq!((link.tabs().len(), link.total()), (2, 3));

        peer.layout(1, PANE + 2, 3, &tabs, &[(PANE + 2, whole())])
            .unwrap();
        assert!(turn_until(&mut link, Link::take_laid));
        assert_eq!(link.screens().len(), 1);
        assert_eq!(pane(&link).pane, PANE + 2);
        assert_eq!(link.active(), 1);
    }

    #[test]
    fn a_tab_keeps_its_directory_across_a_layout_until_its_pane_changes() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (pane, path) = (PANE, "/home");
        peer.notices(&[Notice::Cwd { pane, path }]).unwrap();
        assert!(turn_until(&mut link, |link| link
            .tabs()
            .first()
            .is_some_and(|tab| !tab.cwd.is_empty())));

        peer.layout(0, PANE, 2, &[PANE, PANE + 1], &[(PANE, whole())])
            .unwrap();
        assert!(turn_until(&mut link, |link| link.tabs().len() == 2));
        assert_eq!(link.tabs()[0].cwd, path);
        assert_eq!(link.tabs()[1].cwd, "");

        peer.layout(
            0,
            PANE + 2,
            2,
            &[PANE + 2, PANE + 1],
            &[(PANE + 2, whole())],
        )
        .unwrap();
        assert!(turn_until(&mut link, |link| link.tabs()[0].pane == PANE + 2));
        assert_eq!(link.tabs()[0].cwd, "");
    }

    #[test]
    fn the_rest_of_the_state_is_kept_for_the_window() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let pane = PANE;
        let found = Some(Selection::default());
        peer.notices(&[Notice::Searched { pane, found }]).unwrap();
        peer.notices(&[Notice::Refused {
            reason: Refusal::Tabs,
        }])
        .unwrap();
        assert!(turn_until(&mut link, |link| link.state.refused.is_some()));
        assert_eq!(link.take_searched(), Some((pane, found)));
        assert_eq!(link.take_searched(), None);
        assert_eq!(link.take_refused(), Some(Refusal::Tabs));
        assert_eq!(link.take_refused(), None);

        peer.no_tabs().unwrap();
        assert!(turn_until(&mut link, |link| link.is_closed()));
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
        let pane = PANE;
        let (asked, unasked) = ("asked", "unasked");
        peer.notices(&[
            Notice::Copied {
                pane,
                text: unasked,
            },
            Notice::Raise,
        ])
        .unwrap();
        assert!(turn_until(&mut link, Link::take_raised));
        assert_eq!(link.take_copied(), None);

        assert!(link.copy(pane, Selection::default()));
        peer.notices(&[
            Notice::Copied { pane, text: asked },
            Notice::Copied {
                pane,
                text: unasked,
            },
        ])
        .unwrap();
        peer.no_tabs().unwrap();
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
