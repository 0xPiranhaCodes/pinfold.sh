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
//!
//! A pane gets frames while the active tab shows it, so a hidden pane costs no frame here or in
//! the window, and its last frame is freed when it hides. Nothing is shown until the window has
//! said how much room it has. Replies reach the window for any pane.

use std::io::{self, ErrorKind, Read, Write};
use std::mem;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use mio::unix::SourceFd;
use mio::{Interest, Token};
use pinfold_proto::Frame;
use pinfold_proto::link::{
    self, Approval, ApprovalAction, ApprovalAnswer, Decoder, Notice, Refusal, Request, VERSION,
    encode_frame, encode_layout,
};
use pinfold_sys::socket;

use crate::grants::{MAX_PENDING as MAX_APPROVALS, Prompt};
use crate::io::{READ_BUDGET, Reactor};
use crate::search::{CopyStep, Copying};
use crate::status::Status;

/// Beyond any pane number, like the waker's token.
pub(crate) const LINK: Token = Token(usize::MAX - 1);

/// Room for replies beside a frame: two selections of the longest kind.
const REPLIES: usize = 4 * 1024 * 1024;
/// Most bytes waiting for a window: the largest frame, its prefix, and room for replies.
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

/// The link has ended. Why does not matter, as every cause is handled the same way.
struct Ended;

/// What the link knows about one pane, which goes when the link does.
#[derive(Default)]
struct PaneLink {
    number: usize,
    /// The last frame encoded, which the next one is a diff against.
    sent: Frame,
    /// Whether the window holds a frame to apply a diff to.
    synced: bool,
    /// Whether the active tab shows this pane.
    watched: bool,
    /// Whether this pane has its tab's focus, so its directory names the tab.
    naming: bool,
    /// The directory the window last heard of, while `naming`.
    cwd: String,
    /// Whether `cwd` needs checking without the pane having changed.
    named: bool,
    searching: bool,
    /// Whether it printed something while no window showed it.
    noisy: bool,
    /// Whether it raised a "needs you" alert while hidden, and needs you still.
    alerted: bool,
    /// Whether this window heard what the pane was running before a restore.
    told_ran: bool,
    /// The state and source the window last heard of, while `naming`.
    told: Option<(u8, bool)>,
    /// The message the window last heard of, while `naming`.
    message: String,
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
    /// In order of number, as the reactor's are.
    panes: Vec<PaneLink>,
    /// The frame being read, swapped with a pane's `sent` once encoded.
    spare: Frame,
    /// Where the next pass over panes starts, so a busy pane cannot starve the rest.
    next: usize,
    greeted: bool,
    copying: Option<CopyJob>,
    /// The reactor's revision as of the last layout sent.
    laid: Option<u64>,
    /// The tabs with news, as last sent.
    activity: u32,
    /// The tabs with a hidden pane that needs you, as last sent.
    alerts: u32,
    /// Whether those may be out of date.
    activity_changed: bool,
    /// The latest command refused. Only the last matters, so a burst of them costs nothing.
    refused: Option<Refusal>,
    /// Whether this window was told the core lacks the hardened runtime, if it does.
    told_unhardened: bool,
}

impl Session {
    fn pending(&self) -> usize {
        self.out.len().saturating_sub(self.head)
    }

    /// Queues a reply, or ends the link if a window is not reading its replies.
    fn notice(&mut self, notice: &Notice<'_>) -> Result<(), Ended> {
        let text = match notice {
            Notice::Copied { text, .. } => text.len(),
            Notice::Linked { uri, .. } => uri.len(),
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
                let taken = reactor.write(pane as usize, bytes);
                self.note_input(reactor, pane, taken);
            }
            // Never `write`: this is the call that sanitizes a paste (T7).
            Request::Paste { pane, text } => {
                let taken = reactor.paste(pane as usize, text);
                self.note_input(reactor, pane, taken);
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
                reactor.search(pane, query, from, direction);
                // One that could not start ends now, and `report` says so.
                if let Some(state) = self.state(pane) {
                    state.searching = true;
                }
            }
            // shortcut: a new copy replaces a running one, which then gets no reply, queue them
            // if a window ever needs two at once.
            Request::Copy { pane, selection } => {
                let pane = pane as usize;
                self.copying = reactor.engine(pane).map(|_| CopyJob {
                    pane,
                    copy: Copying::new(selection),
                    text: String::new(),
                });
            }
            Request::Link { pane, at } => {
                let engine = reactor.engine(pane as usize);
                let uri = engine.and_then(|engine| engine.hyperlink(at));
                let uri = uri.filter(|uri| uri.len() <= link::MAX_URI);
                self.notice(&Notice::Linked {
                    pane,
                    at,
                    uri: uri.as_deref().unwrap_or_default(),
                })?;
            }
            Request::FullFrame { pane } => {
                if let Some(state) = self.state(pane as usize).filter(|state| state.watched) {
                    state.synced = false;
                }
            }
            Request::WasRunningAnswer { pane, run } => {
                reactor.answer_was_running(pane as usize, run);
            }
            Request::ApprovalAnswer { id, answer } => {
                if reactor.approval_answers.len() < MAX_APPROVALS {
                    reactor.approval_answers.push((id, answer));
                }
            }
            Request::Quit => return Err(Ended),
            command => self.refused = reactor.command(command).err().or(self.refused),
        }
        Ok(())
    }

    /// Tells the window of input a pane had no room for. A pane that has closed is no news.
    fn note_input(&mut self, reactor: &mut Reactor, pane: u32, taken: bool) {
        if taken {
            reactor.typed(pane as usize, std::time::Instant::now());
        } else if reactor.engine(pane as usize).is_some() {
            self.refused = Some(Refusal::Input);
        }
    }

    /// Sends what finished since the last pass: searches, and a step of any copy.
    fn report(&mut self, reactor: &mut Reactor) -> Result<(), Ended> {
        for index in 0..self.panes.len() {
            let Some(state) = self.panes.get_mut(index) else {
                continue;
            };
            let pane = state.number;
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
                found,
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
        // A pane closed halfway leaves part of a copy, and the clipboard should keep what it has.
        if step.is_none() {
            job.text.clear();
        }
        if let Some(job) = self.copying.take() {
            self.notice(&Notice::Copied {
                pane: job.pane as u32,
                text: &job.text,
            })?;
        }
        Ok(())
    }

    fn state(&mut self, pane: usize) -> Option<&mut PaneLink> {
        let index = self.panes.binary_search_by_key(&pane, |state| state.number);
        self.panes.get_mut(index.ok()?)
    }

    /// Forgets panes that closed and learns new ones, which always have higher numbers.
    fn follow_panes(&mut self, reactor: &Reactor) {
        self.panes
            .retain(|state| reactor.engine(state.number).is_some());
        let last = self.panes.last().map(|state| state.number);
        let new = reactor.numbers().filter(|pane| last < Some(*pane));
        self.panes.extend(new.map(|number| PaneLink {
            number,
            ..PaneLink::default()
        }));
    }

    /// Sends a layout that changed, and applies it to what this link watches. A layout goes out
    /// only behind everything before it, so a window that stops reading holds one at most, and
    /// always gets it ahead of the frames it explains.
    fn send_layout(&mut self, reactor: &Reactor) -> Result<(), Ended> {
        if reactor.area.is_none() || self.laid == Some(reactor.revision) {
            return Ok(());
        }
        self.laid = Some(reactor.revision);
        let shown = reactor.shown();
        let tabs: Vec<u32> = (reactor.session.tabs().iter())
            .map(pinfold_mux::Tab::focused)
            .collect();
        for state in &mut self.panes {
            let watched = shown.iter().any(|(pane, _)| *pane as usize == state.number);
            if !watched {
                state.sent = Frame::default();
                state.synced = false;
            }
            state.watched = watched;
            let naming = tabs.iter().any(|pane| *pane as usize == state.number);
            if naming && !state.naming {
                state.cwd.clear();
                state.named = false;
                state.told = None;
            }
            state.naming = naming;
        }
        let active = reactor.session.active() as u16;
        let focused = reactor
            .session
            .active_tab()
            .map_or(0, pinfold_mux::Tab::focused);
        let total = reactor.pane_count() as u16;
        encode_layout(&mut self.out, active, focused, total, &tabs, &shown).map_err(|_| Ended)?;
        self.activity_changed = true;
        self.flush()
    }

    /// Says which tabs have news. The active tab has none, so showing a tab clears it. A layout
    /// numbers the tabs anew, so this always follows one.
    fn send_activity(&mut self, reactor: &Reactor) -> Result<(), Ended> {
        if self.pending() > 0 || !mem::take(&mut self.activity_changed) {
            return Ok(());
        }
        let (mut activity, mut alerts) = (0, 0);
        for (index, tab) in reactor.session.tabs().iter().enumerate() {
            let shown = index == reactor.session.active();
            let (mut noisy, mut alerted) = (false, false);
            for pane in tab.panes() {
                let number = pane as usize;
                let state = self.panes.iter_mut().find(|state| state.number == number);
                if let Some(state) = state {
                    state.noisy &= !shown;
                    noisy |= state.noisy;
                    state.alerted &= !shown;
                    alerted |= state.alerted;
                }
            }
            let bit = 1u32.checked_shl(index as u32).unwrap_or(0);
            activity |= if noisy { bit } else { 0 };
            alerts |= if alerted { bit } else { 0 };
        }
        if (activity, alerts) != (self.activity, self.alerts) {
            (self.activity, self.alerts) = (activity, alerts);
            self.notice(&Notice::Activity {
                tabs: activity,
                alerts,
            })?;
            self.flush()?;
        }
        Ok(())
    }

    /// Sends what changed, while the socket keeps up.
    fn send_panes(&mut self, reactor: &mut Reactor) -> Result<(), Ended> {
        self.flush()?;
        self.follow_panes(reactor);
        if self.pending() > 0 {
            return Ok(());
        }
        self.send_layout(reactor)?;
        if self.pending() == 0
            && let Some(reason) = self.refused.take()
        {
            self.notice(&Notice::Refused { reason })?;
            self.flush()?;
        }
        let count = self.panes.len();
        for offset in 0..count {
            let index = (self.next + offset) % count;
            if self.pending() > 0 {
                self.next = index;
                return Ok(());
            }
            self.send_pane(index, reactor)?;
        }
        self.send_activity(reactor)
    }

    fn send_pane(&mut self, index: usize, reactor: &mut Reactor) -> Result<(), Ended> {
        let Some(state) = self.panes.get_mut(index) else {
            return Ok(());
        };
        let pane = state.number;
        if !state.told_ran {
            state.told_ran = true;
            let ran = reactor.was_running.iter().find(|(at, _)| *at == pane);
            if let Some((_, command)) = ran {
                let runnable = crate::snapshot::runnable(command);
                let notice = Notice::WasRunning {
                    pane: pane as u32,
                    runnable,
                    command,
                };
                notice.encode(&mut self.out).map_err(|_| Ended)?;
            }
        }
        let dirty = reactor.take_dirty(pane);
        if dirty && !state.watched && !state.noisy {
            state.noisy = true;
            self.activity_changed = true;
        }
        // The tracker rate-limits alerts, so a flapping agent marks its tab once (T4).
        let alert = reactor.take_alert(pane);
        let status = reactor.status(pane);
        let needs_you = status.is_some_and(|(status, ..)| status == Status::NeedsYou);
        let alerted = needs_you && (state.alerted || (alert && !state.watched));
        if alerted != state.alerted {
            state.alerted = alerted;
            self.activity_changed = true;
        }
        if state.naming
            && let Some((status, reported, message)) = status
        {
            let told = Some((status as u8, reported));
            if state.told != told || state.message != message {
                state.told = told;
                message.clone_into(&mut state.message);
                let notice = Notice::Status {
                    pane: pane as u32,
                    state: status as u8,
                    reported,
                    message: &state.message,
                };
                notice.encode(&mut self.out).map_err(|_| Ended)?;
            }
        }
        if state.watched && (dirty || !state.synced) {
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
        }
        if state.naming && (dirty || !state.named) {
            state.named = true;
            let engine = reactor.engine(pane);
            // A tool pane's tab shows its title, which a window tells from a path by its lack
            // of a leading `/`.
            let cwd = engine
                .and_then(|engine| engine.cwd())
                .map(|cwd| cwd.path.as_str());
            let cwd = reactor.tool_title(pane).or(cwd).unwrap_or_default();
            if state.cwd != cwd {
                cwd.clone_into(&mut state.cwd);
                let (pane, path) = (pane as u32, state.cwd.as_str());
                let notice = Notice::Cwd { pane, path };
                notice.encode(&mut self.out).map_err(|_| Ended)?;
            }
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
                        self.decoder.next_request(&mut input).map_err(|error| {
                            if let link::Error::Version { theirs, .. } = error {
                                reactor.host.other_version = Some(theirs);
                            }
                            Ended
                        })?
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
        self.session.follow_panes(reactor);
        if self.readable {
            self.read(reactor)?;
        }
        if self.session.greeted {
            if !mem::replace(&mut self.session.told_unhardened, true)
                && let Some(checked) = reactor.unhardened
            {
                let notice = Notice::Unhardened { checked };
                // A window a version older cannot show it, and the core's log said it anyway.
                if self.decoder.version() >= Some(notice.since()) {
                    self.session.notice(&notice)?;
                }
            }
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
    dying: Vec<Child>,
    /// The link version of the last window that spoke another one, as an upgrade's does.
    other_version: Option<u16>,
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
    /// A session has one window, so while one lives this starts nothing, asks it to come to
    /// the front, and says so. The program finds the link on [`socket::LINK_FD`], and pane
    /// children never get it. A window whose link ends is killed, and this can then start
    /// another.
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
        self.spawn_window_command(Command::new(binary))
    }

    /// Does what [`Reactor::spawn_window`] does with a prepared `command`. Only the core's
    /// measuring mode uses this, as nothing from a control socket may reach a window's command.
    ///
    /// # Errors
    ///
    /// Returns what [`Reactor::spawn_window`] returns.
    pub fn spawn_window_command(&mut self, mut command: Command) -> io::Result<WindowOpen> {
        self.host.reap();
        if let Some(window) = self.host.window.as_mut() {
            // A window over the cap on replies, or one that has gone, is about to end anyway.
            let _ = window.session.notice(&Notice::Raise);
            let _ = window.session.flush();
            return Ok(WindowOpen::AlreadyOpen);
        }
        if !Path::new(command.get_program()).is_absolute() {
            return Err(io::Error::from(ErrorKind::InvalidInput));
        }
        self.host.other_version = None;
        let (ours, theirs) = socket::pair()?;
        command.stdin(Stdio::null());
        let child = socket::spawn_with_link(command, theirs)?;
        self.attach_window(ours, Some(child))?;
        Ok(WindowOpen::Started)
    }

    /// Takes the link version of a window that ended because it speaks another one than
    /// [`VERSION`]: one from a newer install, started by a core still running the old one.
    pub fn take_other_version(&mut self) -> Option<u16> {
        self.host.other_version.take()
    }

    /// Returns whether a window's link is open.
    #[must_use]
    pub fn has_window(&self) -> bool {
        self.host.window.is_some()
    }

    /// Returns whether a window on the link has said hello, which it does once it is up.
    #[must_use]
    pub fn is_window_ready(&self) -> bool {
        let window = self.host.window.as_ref();
        window.is_some_and(|window| window.session.greeted)
    }

    /// Returns whether a ready window can show a prompt for `action`: one of an older link
    /// version cannot show what came after it (ADR-0043).
    #[must_use]
    pub fn can_ask(&self, action: ApprovalAction) -> bool {
        let window = self.host.window.as_ref().filter(|w| w.session.greeted);
        window.and_then(|window| window.decoder.version()) >= Some(action.since())
    }

    /// Returns the process id of a window this reactor started, while its link is open.
    #[must_use]
    pub fn window_pid(&self) -> Option<u32> {
        self.host.window.as_ref()?.child.as_ref().map(Child::id)
    }

    /// Sends or updates a prompt over the inherited window link.
    ///
    /// Returns false when no ready window can show it or its bounded queue is full.
    pub fn ask_approval(&mut self, prompt: &Prompt) -> bool {
        if !self.can_ask(prompt.action) {
            return false;
        }
        let (Ok(caller), Ok(target)) = (u32::try_from(prompt.caller), u32::try_from(prompt.target))
        else {
            return false;
        };
        let Some(window) = self
            .host
            .window
            .as_mut()
            .filter(|window| window.session.greeted)
        else {
            return false;
        };
        let texts = prompt.texts.iter().map(String::as_str).collect();
        let notice = Notice::Approval(Approval {
            id: prompt.id,
            caller,
            target,
            action: prompt.action,
            texts,
        });
        window.session.notice(&notice).is_ok() && window.session.flush().is_ok()
    }

    /// Takes one answer that arrived over the inherited window link.
    pub fn take_approval_answer(&mut self) -> Option<(u64, ApprovalAnswer)> {
        self.approval_answers.pop()
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
                laid: None,
                activity: 0,
                alerts: 0,
                activity_changed: false,
                refused: None,
                told_unhardened: false,
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
        self.host.reap();
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

    use pinfold_proto::link::{
        MAX_INPUT, MAX_MESSAGE, MAX_PANES, MAX_TABS, OLDEST, Rect, Side, SplitWay,
    };
    use proptest::prelude::*;

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
        linked: Vec<String>,
        raises: usize,
        activity: u32,
        alerts: u32,
        /// The last status: pane, state, whether reported, and message.
        status: Option<(u32, u8, bool, String)>,
        layouts: usize,
        active: u16,
        focused: u32,
        tabs: Vec<u32>,
        shown: Vec<(u32, Rect)>,
        refused: Vec<Refusal>,
        was_running: Vec<(u32, bool, String)>,
        unhardened: Vec<bool>,
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
                linked: Vec::new(),
                raises: 0,
                activity: 0,
                alerts: 0,
                status: None,
                layouts: 0,
                active: 0,
                focused: 0,
                tabs: Vec::new(),
                shown: Vec::new(),
                refused: Vec::new(),
                was_running: Vec::new(),
                unhardened: Vec::new(),
            }
        }

        /// Attaches and says hello.
        fn greet(reactor: &mut Reactor) -> Self {
            let peer = Self::attach(reactor);
            peer.send(Request::Hello { version: VERSION });
            peer
        }

        /// Attaches, says hello and gives the size of `pane`, as a window does.
        fn watching(reactor: &mut Reactor, pane: usize) -> Self {
            let peer = Self::greet(reactor);
            peer.fit(reactor, pane);
            peer
        }

        /// Gives the size of `pane` as the space for panes, so a lone pane keeps its size.
        fn fit(&self, reactor: &Reactor, pane: usize) {
            let (rows, columns) = reactor.engine(pane).unwrap().size();
            self.send(Request::Resize { rows, columns });
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
                        Notice::Linked { uri, .. } => self.linked.push(uri.to_owned()),
                        Notice::Raise => self.raises += 1,
                        Notice::Activity { tabs, alerts } => {
                            (self.activity, self.alerts) = (tabs, alerts);
                        }
                        Notice::Status {
                            pane,
                            state,
                            reported,
                            message,
                        } => self.status = Some((pane, state, reported, message.to_owned())),
                        Notice::Layout(layout) => {
                            self.layouts += 1;
                            self.active = layout.active;
                            self.focused = layout.focused;
                            self.tabs = layout.tabs().collect();
                            self.shown = layout.panes().collect();
                        }
                        Notice::Refused { reason } => self.refused.push(reason),
                        Notice::WasRunning {
                            pane,
                            runnable,
                            command,
                        } => self.was_running.push((pane, runnable, command.to_owned())),
                        Notice::Approval(_) => {}
                        Notice::Unhardened { checked } => self.unhardened.push(checked),
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
        let pane = reactor
            .open_tab(shell("echo hi; sleep 30"), 24, 80)
            .unwrap();
        let mut peer = Peer::attach(&mut reactor);
        turn_until(&mut reactor, |r| on_screen(r, pane, "hi"));
        peer.read();
        assert_eq!(peer.hellos, 1);
        assert!(peer.frames.is_empty(), "a frame came before the hello");
        assert!(!reactor.is_window_ready());
        peer.send(Request::Hello { version: VERSION });
        peer.fit(&reactor, pane);
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
        assert!(reactor.is_window_ready());
    }

    #[test]
    fn another_version_ends_the_link_and_the_pty_lives() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::attach(&mut reactor);
        let [low, high] = (VERSION + 1).to_le_bytes();
        peer.raw(&[3, 0, 0, 0, 0, low, high]);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
        assert_eq!(reactor.take_other_version(), Some(VERSION + 1));
        assert_eq!(reactor.take_other_version(), None);
    }

    /// ADR-0043: a window one link version older is served, and never sent an owner prompt,
    /// which its version does not have.
    #[test]
    fn an_older_window_is_served_without_what_it_cannot_read() {
        // Since version 12 no notice is newer than `OLDEST`, so this checks the link stays up.
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor
            .open_tab(shell("echo hi; sleep 30"), 24, 80)
            .unwrap();
        let mut peer = Peer::attach(&mut reactor);
        peer.send(Request::Hello { version: OLDEST });
        peer.fit(&reactor, pane);
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
        assert!(reactor.is_window_ready());
        assert_eq!(reactor.take_other_version(), None);
        assert!(reactor.can_ask(ApprovalAction::Owner));
        assert!(reactor.is_window_ready(), "the link ended");
    }

    #[test]
    fn a_core_without_the_hardened_runtime_tells_each_window_once() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
        assert!(peer.unhardened.is_empty(), "a hardened core said otherwise");
        drop(peer);
        assert!(turn_until(&mut reactor, |r| !r.has_window()));
        reactor.set_unhardened(false);
        for _ in 0..2 {
            let mut peer = Peer::watching(&mut reactor, pane);
            serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
            reactor.write(pane, b"more\n");
            serve(&mut reactor, &mut peer, |r, _| {
                on_screen(r, pane, "got-more")
            });
            peer.read();
            assert_eq!(peer.unhardened, [false]);
            drop(peer);
            assert!(turn_until(&mut reactor, |r| !r.has_window()));
        }
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
            let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
            let peer = Peer::attach(&mut reactor);
            peer.raw(wire);
            assert_link_ends_and_pty_lives(&mut reactor, pane);
        }
    }

    #[test]
    fn garbage_after_a_good_hello_ends_the_link() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.raw(&[2, 0, 0, 0, 200, 1]);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn a_window_that_closes_mid_message_ends_the_link() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.raw(&[100, 0, 0, 0, 1, 0]);
        drop(peer);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn a_killed_window_is_reaped_on_a_later_turn() {
        let mut reactor = Reactor::new().unwrap();
        let mut pane = Command::new("/bin/sh");
        pane.args(["-c", ALIVE]);
        reactor.open_tab(pane, 24, 80).unwrap();
        reactor.spawn_window(Path::new("/bin/cat")).unwrap();
        let window = reactor.host.window.as_ref().unwrap();
        // `cat` may have ended already, and macOS then answers that nothing is connected.
        let _ = window.session.socket.shutdown(std::net::Shutdown::Both);
        assert!(turn_until(&mut reactor, |r| {
            !r.has_window() && r.host.dying.is_empty()
        }));
    }

    #[test]
    fn quit_ends_the_link() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let peer = Peer::greet(&mut reactor);
        peer.send(Request::Quit);
        assert_link_ends_and_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn a_stalled_window_holds_one_frame_and_catches_up_on_the_newest() {
        let mut reactor = Reactor::new().unwrap();
        // One full frame of this is past any socket's buffer, so the first send stalls.
        let (rows, columns) = (200, 400);
        let script = "yes | head -n 30000; echo finished; sleep 30";
        let flood = reactor.open_tab(shell(script), rows, columns).unwrap();
        let quiet = reactor.split(flood, shell(ALIVE), SplitWay::Right).unwrap();
        let mut peer = Peer::watching(&mut reactor, flood);

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
        assert!(capacity <= 2 * one_frame, "{capacity} bytes of buffer");
        assert_pty_lives(&mut reactor, quiet);

        serve(&mut reactor, &mut peer, |r, p| {
            p.matches(r, flood) && p.matches(r, quiet)
        });
        let session = &reactor.host.window.as_ref().unwrap().session;
        assert!(session.out.capacity() <= KEPT_OUTPUT);
    }

    #[test]
    fn a_blocked_turn_wakes_when_the_window_drains_the_socket() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell("sleep 30"), 400, 400).unwrap();
        let peer = Peer::watching(&mut reactor, pane);
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
    fn a_window_that_opens_on_no_panes_gets_them_once_they_exist() {
        let mut reactor = Reactor::new().unwrap();
        let mut peer = Peer::greet(&mut reactor);
        peer.send(Request::FullFrame { pane: 0 });
        peer.send(Request::Resize {
            rows: 24,
            columns: 80,
        });
        serve(&mut reactor, &mut peer, |_, p| p.layouts == 1);
        assert!(reactor.has_window(), "an unknown pane ended the link");

        let pane = reactor.open_tab(shell("sleep 30"), 24, 80).unwrap();
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
    }

    #[test]
    fn a_rejected_query_is_answered_and_the_link_stays() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
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
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
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
            peer.send(Request::ClosePane { pane: bad });
            peer.send(Request::Focus { pane: bad });
            peer.send(Request::Split {
                pane: bad,
                way: SplitWay::Down,
            });
        }
        peer.send(Request::SwitchTab { index: 31 });
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
            peer.send(Request::Resize { rows, columns });
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
        let pane = reactor.open_tab(shell(script), 24, 80).unwrap();
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
    fn enter_types_what_a_restored_pane_was_running_and_esc_types_nothing() {
        use crate::snapshot::{Layout, Pane, Snapshot, Tab, VERSION as SAVED, Way};
        // Stacked, so each pane is wide enough for a line.
        let split = |first, second| Layout::Split {
            direction: Way::Down,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };
        let root = split(
            Layout::Pane(0),
            split(Layout::Pane(1), split(Layout::Pane(2), Layout::Pane(3))),
        );
        let snapshot = Snapshot {
            version: SAVED,
            tabs: vec![Tab {
                root,
                focus: 0,
                zoomed: false,
            }],
            active: 0,
            panes: vec![
                Pane::new(None, "/bin/sh"),
                Pane::new(None, "rerun-me"),
                Pane::new(None, "dismiss-me"),
                Pane::new(
                    None,
                    "deploy --token=ghp_0123456789abcdefghijklmnopqrstuvwxyz",
                ),
            ],
            workspaces: Vec::new(),
        };
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = || shell(ALIVE);
        reactor.restore(&snapshot).unwrap();
        let mut peer = Peer::watching(&mut reactor, 0);
        serve(&mut reactor, &mut peer, |_, p| p.was_running.len() == 3);
        let (run, esc, redacted) = (1, 2, 3);
        // The shell pane, by its program's name, shows nothing.
        let told: Vec<_> = peer
            .was_running
            .iter()
            .map(|(pane, ok, _)| (*pane, *ok))
            .collect();
        assert_eq!(told, [(run, true), (esc, true), (redacted, false)]);
        assert_eq!(peer.was_running[0].2, "rerun-me");

        peer.send(Request::WasRunningAnswer {
            pane: esc,
            run: false,
        });
        peer.send(Request::WasRunningAnswer {
            pane: redacted,
            run: true,
        });
        peer.send(Request::WasRunningAnswer {
            pane: run,
            run: true,
        });
        assert!(turn_until(&mut reactor, |r| on_screen(
            r,
            1,
            "got-rerun-me"
        )));
        assert!(reactor.was_running.is_empty());
        for pane in [esc, redacted] {
            assert_pty_lives(&mut reactor, pane as usize);
            assert!(!on_screen(&reactor, pane as usize, "-me"));
            assert!(!on_screen(&reactor, pane as usize, "deploy"));
        }
        // Answered once: a second Enter types nothing.
        assert!(!reactor.answer_was_running(1, true));
    }

    #[test]
    fn input_reaches_the_program() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
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
        let pane = reactor.open_tab(shell(script), 10, 40).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
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
    fn a_pane_opened_later_gets_a_full_frame_once_asked() {
        let mut reactor = Reactor::new().unwrap();
        let mut peer = Peer::greet(&mut reactor);
        serve(&mut reactor, &mut peer, |_, p| p.hellos == 1);
        let script = "echo later; sleep 30";
        let pane = reactor.open_tab(shell(script), 24, 80).unwrap();
        serve(&mut reactor, &mut peer, |r, _| on_screen(r, pane, "later"));
        peer.fit(&reactor, pane);
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, pane));
    }

    #[test]
    fn the_window_learns_the_hyperlink_under_a_cell() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"printf '\033]8;;https://pinfold.sh/a\033\\here\033]8;;\033\\ plain\n'; long=$(head -c 3000 /dev/zero | tr '\0' x); printf '\033]8;;https://a/%s\033\\long\033]8;;\033\\\n' $long; while read line; do echo got-$line; done";
        let pane = reactor.open_tab(shell(script), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
        serve(&mut reactor, &mut peer, |r, _| on_screen(r, pane, "long"));
        let engine = reactor.engine(pane).unwrap();
        let (linked, plain) = (engine.position(0, 3), engine.position(0, 6));
        let long = engine.position(1, 0);
        assert!(
            engine
                .hyperlink(long)
                .is_some_and(|uri| uri.len() > link::MAX_URI)
        );
        let number = pane as u32;
        for (pane, at) in [
            (number, linked),
            (number, plain),
            (9, linked),
            (number, long),
        ] {
            peer.send(Request::Link { pane, at });
        }
        let far = link::Position {
            row: usize::MAX,
            column: u16::MAX,
        };
        peer.send(Request::Link {
            pane: number,
            at: far,
        });
        serve(&mut reactor, &mut peer, |_, p| p.linked.len() == 5);
        assert_eq!(peer.linked, ["https://pinfold.sh/a", "", "", "", ""]);
        assert_pty_lives(&mut reactor, pane);
    }

    #[test]
    fn a_hidden_pane_costs_no_frame_and_still_answers() {
        let mut reactor = Reactor::new().unwrap();
        let script = "echo needle; read line; exit 0";
        let hidden = reactor.open_tab(shell(script), 24, 80).unwrap();
        let shown = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, shown);
        serve(&mut reactor, &mut peer, |r, p| {
            on_screen(r, hidden, "needle") && p.matches(r, shown)
        });

        let number = hidden as u32;
        peer.send(Request::Search {
            pane: number,
            from: link::Position::default(),
            direction: link::Direction::Forward,
            query: "needle",
        });
        serve(&mut reactor, &mut peer, |_, p| p.searched.len() == 1);
        peer.send(Request::Copy {
            pane: number,
            selection: peer.searched[0].unwrap(),
        });
        serve(&mut reactor, &mut peer, |_, p| p.copied.len() == 1);
        assert_eq!(peer.copied, ["needle"]);
        peer.send(Request::Input {
            pane: number,
            bytes: b"\n",
        });
        serve(&mut reactor, &mut peer, |_, p| p.tabs == [shown as u32]);

        assert!(!peer.frames.contains_key(&number));
        assert!(peer.matches(&reactor, shown));
    }

    #[test]
    fn search_copy_cwd_and_exit_reach_the_window() {
        let mut reactor = Reactor::new().unwrap();
        let script = r"seq 1 200; echo needle; printf '\033]7;file:///tmp/where\033\\'; seq 1 50; read line; exit 0";
        let pane = reactor.open_tab(shell(script), 10, 40).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
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
        serve(&mut reactor, &mut peer, |_, p| {
            p.layouts > 1 && p.tabs.is_empty()
        });
        assert!(reactor.has_window(), "a pane ending is not the window");
    }

    #[test]
    fn buffers_shrink_back_after_a_large_frame() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor.open_tab(shell("sleep 30"), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
        let number = pane as u32;
        for size in [400, 24] {
            peer.send(Request::Resize {
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
        let [low, high] = VERSION.to_le_bytes();
        let hello = format!(
            r"printf '\003\000\000\000\000\{low:03o}\{high:03o}' >&3; exec cat <&3 >/dev/null"
        );
        let (binary, log) = window_script("second-open", &hello);
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
    fn a_second_open_raises_the_window_it_finds() {
        let mut reactor = Reactor::new().unwrap();
        let mut peer = Peer::greet(&mut reactor);
        let open = reactor.spawn_window(Path::new("/does/not/exist")).unwrap();
        assert_eq!(open, WindowOpen::AlreadyOpen);
        serve(&mut reactor, &mut peer, |_, p| p.raises == 1);
        assert!(reactor.has_window());
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
        let (ours, theirs) = socket::pair().unwrap();
        let ends = [ours.as_raw_fd(), theirs.as_raw_fd()];
        let mut reactor = Reactor::new().unwrap();
        reactor.attach_window(ours, None).unwrap();
        // The test opens nothing in the child, so only an inherited end can hold an end's
        // number. Listing /dev/fd opens a descriptor, which can take a free end's number when
        // parallel tests leave the ends low (#178).
        let script = format!(
            "for fd in 2 {} {}; do [ -e /dev/fd/$fd ] && echo $fd; done; echo checked; sleep 30",
            ends[0], ends[1]
        );
        let pane = reactor.open_tab(shell(&script), 24, 80).unwrap();
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "checked")));
        let engine = reactor.engine(pane).unwrap();
        let open: Vec<i32> = (0..24)
            .filter_map(|line| engine.line_text(line).parse().ok())
            .collect();
        assert!(open.contains(&2), "{open:?}");
        assert!(ends.iter().all(|end| !open.contains(end)), "{open:?}");
    }

    /// A reactor whose new panes wait for input, with one tab, and a window over it.
    fn session(rows: u16, columns: u16) -> (Reactor, Peer, u32) {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = || shell(ALIVE);
        let pane = reactor.open_tab(shell(ALIVE), rows, columns).unwrap();
        let mut peer = Peer::watching(&mut reactor, pane);
        serve(&mut reactor, &mut peer, |_, p| p.layouts >= 1);
        (reactor, peer, pane as u32)
    }

    fn right(x: u16, columns: u16, rows: u16) -> Rect {
        Rect {
            x,
            y: 0,
            columns,
            rows,
        }
    }

    #[test]
    fn a_window_needs_no_pane_number_to_learn_the_layout() {
        let (reactor, peer, pane) = session(24, 80);
        assert_eq!(peer.tabs, [pane]);
        assert_eq!((peer.active, peer.focused), (0, pane));
        assert_eq!(peer.shown, [(pane, right(0, 80, 24))]);
        assert!(peer.frames.contains_key(&pane) || reactor.has_window());
    }

    #[test]
    fn a_split_shares_the_space_and_the_programs_see_their_sizes() {
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = || shell("stty size; read line; exit 0");
        let first = reactor.open_tab(reactor_shell(), 24, 80).unwrap() as u32;
        let mut peer = Peer::watching(&mut reactor, 0);
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Right,
        });
        serve(&mut reactor, &mut peer, |_, p| p.shown.len() == 2);
        let second = peer.shown[1].0;
        assert_eq!(peer.focused, second);
        assert_eq!(
            peer.shown,
            [(first, right(0, 39, 24)), (second, right(40, 40, 24))]
        );
        assert!(turn_until(&mut reactor, |r| {
            on_screen(r, second as usize, "24 40")
        }));
        serve(&mut reactor, &mut peer, |r, p| {
            p.matches(r, first as usize) && p.matches(r, second as usize)
        });
        assert_eq!(reactor.engine(first as usize).unwrap().size(), (24, 39));
    }

    #[test]
    fn a_split_or_tab_starts_where_the_shell_is_not_where_osc_7_says() {
        let name = format!("pinfold-cwd-{}", std::process::id());
        let directory = std::env::temp_dir().join(&name);
        std::fs::create_dir_all(&directory).unwrap();
        let mut reactor = Reactor::new().unwrap();
        // The source's shell is in `directory` and its output claims /tmp (T4).
        let script = format!(
            r"cd '{}' && printf '\033]7;file:///tmp\007'; echo ready; read line",
            directory.display()
        );
        let source = reactor.open_tab(shell(&script), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, source);
        serve(&mut reactor, &mut peer, |r, _| {
            on_screen(r, source, "ready") && r.cwd(source).as_deref() == Some("/tmp")
        });
        reactor.shell = || shell(r#"echo "at-$(basename "$(pwd -P)")"; read line"#);
        let here = format!("at-{name}");

        peer.send(Request::Split {
            pane: source as u32,
            way: SplitWay::Right,
        });
        serve(&mut reactor, &mut peer, |_, p| p.shown.len() == 2);
        let split = peer.shown[1].0 as usize;
        assert!(turn_until(&mut reactor, |r| on_screen(r, split, &here)));

        // The split is focused and its shell is in `directory` too.
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.active == 1);
        let tab = peer.focused as usize;
        assert!(turn_until(&mut reactor, |r| on_screen(r, tab, &here)));

        // A directory that is gone falls back to the shell's own start, with no error.
        std::fs::remove_dir(&directory).unwrap();
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.active == 2);
        let fallback = peer.focused as usize;
        let own = std::env::current_dir().unwrap().canonicalize().unwrap();
        let own = format!("at-{}", own.file_name().unwrap().to_string_lossy());
        assert!(turn_until(&mut reactor, |r| on_screen(r, fallback, &own)));
    }

    fn reactor_shell() -> Command {
        shell("stty size; read line; exit 0")
    }

    #[test]
    fn a_resize_lays_out_every_shown_pane_and_leaves_hidden_ones() {
        let (mut reactor, mut peer, first) = session(24, 80);
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Down,
        });
        serve(&mut reactor, &mut peer, |_, p| p.shown.len() == 2);
        let second = peer.shown[1].0 as usize;
        let hidden = reactor.open_tab(shell(ALIVE), 24, 80).unwrap();
        peer.send(Request::SwitchTab { index: 0 });
        serve(&mut reactor, &mut peer, |_, p| {
            p.tabs.len() == 2 && p.active == 0
        });

        peer.send(Request::Resize {
            rows: 41,
            columns: 100,
        });
        serve(&mut reactor, &mut peer, |r, p| {
            p.frames[&first].rows == 20 && p.matches(r, second)
        });
        assert_eq!(reactor.engine(first as usize).unwrap().size(), (20, 100));
        assert_eq!(reactor.engine(second).unwrap().size(), (20, 100));
        assert_eq!(reactor.engine(hidden).unwrap().size(), (24, 80));

        peer.send(Request::StepTab { forward: false });
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, hidden));
        assert_eq!(reactor.engine(hidden).unwrap().size(), (41, 100));
    }

    #[test]
    fn a_hidden_tab_costs_no_frame_and_comes_back_whole() {
        let script = r"read line; printf '\033]7;file:///tmp/away\033\\'; echo written; read line";
        let mut reactor = Reactor::new().unwrap();
        let hidden = reactor.open_tab(shell(script), 24, 80).unwrap();
        let mut peer = Peer::watching(&mut reactor, hidden);
        let number = hidden as u32;
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, hidden));

        reactor.shell = || shell(ALIVE);
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| {
            p.shown.len() == 1 && p.shown[0].0 != number
        });
        let link = |r: &Reactor| -> (usize, usize, bool) {
            let session = &r.host.window.as_ref().unwrap().session;
            let state = &session.panes[0];
            (state.number, state.sent.cells.capacity(), state.watched)
        };
        assert_eq!(link(&reactor), (hidden, 0, false));

        let frames = peer.frames[&number].clone();
        assert!(reactor.write(hidden, b"\n"));
        serve(&mut reactor, &mut peer, |r, p| {
            on_screen(r, hidden, "written") && p.cwd == "/tmp/away"
        });
        assert_eq!(peer.frames[&number], frames, "a hidden pane sent a frame");

        peer.send(Request::SwitchTab { index: 0 });
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, hidden));
    }

    #[test]
    fn a_hidden_tab_that_prints_has_news_until_it_is_shown() {
        let mut reactor = Reactor::new().unwrap();
        let hidden = reactor
            .open_tab(shell("read line; echo news; read line"), 24, 80)
            .unwrap();
        let mut peer = Peer::watching(&mut reactor, hidden);
        serve(&mut reactor, &mut peer, |r, p| p.matches(r, hidden));

        reactor.shell = || shell(ALIVE);
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.active == 1);
        assert_eq!(peer.activity, 0, "a shown tab has no news");

        assert!(reactor.write(hidden, b"\n"));
        serve(&mut reactor, &mut peer, |_, p| p.activity == 0b01);

        peer.send(Request::SwitchTab { index: 0 });
        serve(&mut reactor, &mut peer, |_, p| {
            p.active == 0 && p.activity == 0
        });
    }

    #[test]
    fn a_hidden_pane_that_needs_you_marks_its_tab_once_and_its_status_reaches_the_window() {
        let (mut reactor, mut peer, first) = session(24, 80);
        let first = first as usize;
        serve(&mut reactor, &mut peer, |_, p| p.status.is_some());
        assert_eq!(peer.status, Some((first as u32, 4, false, String::new())));
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.active == 1);

        let now = Instant::now();
        let needs_you = Status::NeedsYou;
        reactor.report_status(first, needs_you, "ok?", now).unwrap();
        serve(&mut reactor, &mut peer, |_, p| p.alerts == 0b01);
        let told = Some((first as u32, 0, true, "ok?".to_owned()));
        serve(&mut reactor, &mut peer, |_, p| p.status == told);

        // Another alert inside the rate limit raises none, so showing the tab clears the mark.
        let running = Status::Running;
        reactor.report_status(first, running, "", now).unwrap();
        serve(&mut reactor, &mut peer, |_, p| p.alerts == 0);
        reactor.report_status(first, needs_you, "", now).unwrap();
        for _ in 0..3 {
            reactor.turn(Some(Duration::from_millis(10))).unwrap();
            peer.read();
        }
        assert_eq!(peer.alerts, 0, "rate limited");
    }

    #[test]
    fn a_new_window_finds_the_same_tabs_and_splits() {
        let (mut reactor, mut peer, first) = session(24, 80);
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Right,
        });
        peer.send(Request::NewTab);
        peer.send(Request::SwitchTab { index: 0 });
        peer.send(Request::Focus { pane: first });
        serve(&mut reactor, &mut peer, |_, p| {
            p.tabs.len() == 2 && p.active == 0 && p.focused == first
        });
        let (tabs, shown) = (peer.tabs.clone(), peer.shown.clone());
        peer.send(Request::Quit);
        assert!(turn_until(&mut reactor, |r| !r.has_window()));

        let mut peer = Peer::greet(&mut reactor);
        peer.send(Request::Resize {
            rows: 24,
            columns: 80,
        });
        serve(&mut reactor, &mut peer, |r, p| {
            p.layouts == 1 && p.matches(r, first as usize)
        });
        assert_eq!((peer.tabs, peer.shown, peer.focused), (tabs, shown, first));
    }

    #[test]
    fn focus_moves_by_pane_and_by_side_and_zoom_shows_one() {
        let (mut reactor, mut peer, first) = session(24, 80);
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Right,
        });
        serve(&mut reactor, &mut peer, |_, p| p.shown.len() == 2);
        let second = peer.shown[1].0;

        peer.send(Request::MoveFocus { side: Side::Left });
        serve(&mut reactor, &mut peer, |_, p| p.focused == first);
        peer.send(Request::MoveBorder {
            side: Side::Right,
            cells: 5,
        });
        serve(&mut reactor, &mut peer, |_, p| {
            p.shown.first().is_some_and(|(_, rect)| rect.columns == 44)
        });
        peer.send(Request::Focus { pane: second });
        serve(&mut reactor, &mut peer, |_, p| p.focused == second);
        // pane.list orders panes by when they last had focus.
        let focused = |reactor: &Reactor, pane: u32| {
            let mut listed = reactor.list();
            listed
                .find(|listed| listed.id == pane as usize)
                .map(|listed| listed.focused)
        };
        let (was, now) = (focused(&reactor, first), focused(&reactor, second));
        assert!(was > Some(0) && now > was, "{was:?} {now:?}");
        peer.send(Request::Focus { pane: second });
        peer.send(Request::Focus { pane: first });
        serve(&mut reactor, &mut peer, |_, p| p.focused == first);
        assert!(focused(&reactor, first) > focused(&reactor, second));
        peer.send(Request::Focus { pane: second });
        serve(&mut reactor, &mut peer, |_, p| p.focused == second);

        peer.send(Request::Zoom);
        serve(&mut reactor, &mut peer, |r, p| {
            p.shown == [(second, right(0, 80, 24))] && p.matches(r, second as usize)
        });
        let session = &reactor.host.window.as_ref().unwrap().session;
        let unwatched = |state: &&PaneLink| state.number == first as usize;
        let state = session.panes.iter().find(unwatched).unwrap();
        assert_eq!(state.sent.cells.capacity(), 0, "a zoom hides the others");
    }

    /// Kills a fixture's loop and removes its directory, even when the test panics.
    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Ok(pid) = fs::read_to_string(self.0.join("loop")) {
                let _ = Command::new("kill").arg(pid.trim()).status();
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn closing_ends_the_shell_frees_the_pane_and_spares_the_rest() {
        let (mut reactor, mut peer, first) = session(24, 80);
        let dir = std::env::temp_dir().join(format!("pinfold-close-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let _cleanup = Cleanup(dir.clone());
        let (pid, hup) = (dir.join("pid"), dir.join("hup"));
        // The loop outlives the hangup on purpose, so it ends once the directory goes, or
        // after 1,200 turns if the test dies before removing it.
        let script = format!(
            "echo $$ > '{}'; (trap 'echo hup > {}' HUP; n=0; while [ -d '{}' ] && [ $n -lt 1200 ]; \
             do sleep 0.05; n=$((n+1)); done) & echo $! > '{}'; echo ready; read line",
            pid.display(),
            hup.display(),
            dir.display(),
            dir.join("loop").display(),
        );
        let doomed = reactor
            .split(first as usize, shell(&script), SplitWay::Down)
            .unwrap();
        serve(&mut reactor, &mut peer, |r, p| {
            on_screen(r, doomed, "ready") && p.matches(r, doomed)
        });
        let pid = fs::read_to_string(&pid).unwrap();
        let alive = || {
            shell(&format!("kill -0 {}", pid.trim()))
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive());

        peer.send(Request::ClosePane {
            pane: doomed as u32,
        });
        serve(&mut reactor, &mut peer, |_, p| p.shown.len() == 1);
        assert!(!alive(), "the shell outlived its pane");
        assert!(
            turn_until(&mut reactor, |_| hup.exists()),
            "no hangup reached it"
        );
        assert!(reactor.engine(doomed).is_none());
        assert_eq!(reactor.pane_count(), 1);
        let session = &reactor.host.window.as_ref().unwrap().session;
        assert!(session.panes.iter().all(|state| state.number != doomed));
        assert_eq!(peer.shown, [(first, right(0, 80, 24))]);
        assert_eq!(peer.focused, first);
        assert_pty_lives(&mut reactor, first as usize);
    }

    #[test]
    fn a_program_that_exits_closes_its_pane_then_its_tab_then_the_session() {
        let (mut reactor, mut peer, first) = session(24, 80);
        reactor.shell = || shell("read line; exit 0");
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Right,
        });
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.tabs.len() == 2);
        let (split, tab) = (1, 2);
        assert_eq!(peer.tabs, [split, tab]);

        reactor.write(tab as usize, b"\n");
        serve(&mut reactor, &mut peer, |_, p| p.tabs == [split]);
        assert_eq!(peer.active, 0);
        reactor.write(split as usize, b"\n");
        serve(&mut reactor, &mut peer, |_, p| {
            p.focused == first && p.shown == [(first, right(0, 80, 24))]
        });
        reactor.write(first as usize, b"exit\n");
        peer.send(Request::ClosePane { pane: first });
        serve(&mut reactor, &mut peer, |_, p| p.tabs.is_empty());
        assert_eq!(reactor.pane_count(), 0);
        assert!(reactor.has_window(), "the window goes when it reads this");
    }

    #[test]
    fn numbers_are_not_used_again_so_late_input_reaches_nobody() {
        let (mut reactor, mut peer, first) = session(24, 80);
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.tabs.len() == 2);
        let second = peer.tabs[1];
        peer.send(Request::ClosePane { pane: second });
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| {
            p.tabs.len() == 2 && p.tabs[1] != second
        });
        let third = peer.tabs[1] as usize;
        peer.send(Request::Input {
            pane: second,
            bytes: b"late\n",
        });
        peer.send(Request::Input {
            pane: third as u32,
            bytes: b"mine\n",
        });
        assert!(turn_until(&mut reactor, |r| on_screen(
            r, third, "got-mine"
        )));
        assert!(!on_screen(&reactor, third, "late"));
        assert_pty_lives(&mut reactor, first as usize);
    }

    #[test]
    fn past_the_caps_commands_are_refused_and_the_link_stays() {
        let (mut reactor, mut peer, first) = session(128, 128);
        for _ in 0..MAX_TABS + 8 {
            peer.send(Request::NewTab);
        }
        serve(&mut reactor, &mut peer, |_, p| {
            p.refused.contains(&Refusal::Tabs)
        });
        assert_eq!(reactor.session.tabs().len(), MAX_TABS);
        assert_eq!(peer.tabs.len(), MAX_TABS);

        peer.send(Request::SwitchTab { index: 0 });
        for _ in 0..MAX_PANES {
            peer.send(Request::Split {
                pane: first,
                way: SplitWay::Down,
            });
        }
        serve(&mut reactor, &mut peer, |_, p| {
            p.refused.contains(&Refusal::Panes)
        });
        assert_eq!(reactor.pane_count(), MAX_PANES);
        assert!(reactor.has_window());
        assert_pty_lives(&mut reactor, peer.tabs[1] as usize);
    }

    #[test]
    fn input_a_pane_has_no_room_for_is_refused_and_a_closed_pane_says_nothing() {
        let (mut reactor, mut peer, first) = session(24, 80);
        reactor.shell = || shell("stty raw -echo; echo ready; sleep 30");
        peer.send(Request::Paste {
            pane: first + 1,
            text: "x",
        });
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.tabs.len() == 2);
        assert_eq!(peer.refused, []);

        let stuck = peer.tabs[1];
        let pane = stuck as usize;
        assert!(turn_until(&mut reactor, |r| on_screen(r, pane, "ready")));
        assert!(reactor.write(pane, &vec![b'x'; MAX_INPUT]));
        while reactor.write(pane, b"x") {}
        peer.send(Request::Paste {
            pane: stuck,
            text: "y",
        });
        serve(&mut reactor, &mut peer, |_, p| {
            p.refused.contains(&Refusal::Input)
        });
        assert!(reactor.has_window());
    }

    #[test]
    fn a_split_with_no_room_is_refused_and_changes_nothing() {
        let (mut reactor, mut peer, first) = session(1, 2);
        let before = reactor.session.clone();
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Right,
        });
        serve(&mut reactor, &mut peer, |_, p| p.refused == [Refusal::Room]);
        assert_eq!(reactor.session, before);
        assert_eq!(reactor.pane_count(), 1);
    }

    #[test]
    fn a_shell_that_will_not_start_is_refused_and_changes_nothing() {
        let (mut reactor, mut peer, first) = session(24, 80);
        reactor.shell = || Command::new("/does/not/exist");
        let before = reactor.session.clone();
        peer.send(Request::NewTab);
        peer.send(Request::Split {
            pane: first,
            way: SplitWay::Right,
        });
        serve(&mut reactor, &mut peer, |_, p| {
            p.refused.contains(&Refusal::Spawn)
        });
        assert_eq!(reactor.session, before);
        assert!(reactor.has_window());
    }

    #[test]
    fn a_pane_closed_during_a_copy_leaves_the_clipboard_alone() {
        let (mut reactor, mut peer, first) = session(24, 80);
        peer.send(Request::NewTab);
        serve(&mut reactor, &mut peer, |_, p| p.tabs.len() == 2);
        let second = peer.tabs[1];
        peer.send(Request::Copy {
            pane: second,
            selection: link::Selection::default(),
        });
        peer.send(Request::ClosePane { pane: second });
        serve(&mut reactor, &mut peer, |_, p| p.copied.len() == 1);
        assert_eq!(peer.copied, [""]);
        assert_pty_lives(&mut reactor, first as usize);
    }

    fn sides() -> impl Strategy<Value = Side> {
        prop::sample::select(&[Side::Left, Side::Right, Side::Up, Side::Down][..])
    }

    /// Any command, and the panes it can name, which include ones that have closed.
    fn commands() -> impl Strategy<Value = Request<'static>> {
        let way = prop::sample::select(&[SplitWay::Right, SplitWay::Down][..]);
        let pane = 0..24_u32;
        prop_oneof![
            Just(Request::NewTab),
            Just(Request::Zoom),
            (0..MAX_TABS as u16).prop_map(|index| Request::SwitchTab { index }),
            any::<bool>().prop_map(|forward| Request::StepTab { forward }),
            (pane.clone(), way).prop_map(|(pane, way)| Request::Split { pane, way }),
            pane.clone().prop_map(|pane| Request::ClosePane { pane }),
            pane.clone().prop_map(|pane| Request::Focus { pane }),
            pane.prop_map(|pane| Request::FullFrame { pane }),
            sides().prop_map(|side| Request::MoveFocus { side }),
            (sides(), 1..=link::MAX_DIMENSION)
                .prop_map(|(side, cells)| Request::MoveBorder { side, cells }),
            // Big enough to split a lot; full-size frames take most of a second each to send.
            (1..=200_u16, 1..=200_u16)
                .prop_map(|(rows, columns)| Request::Resize { rows, columns }),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn any_commands_leave_a_core_that_matches_its_session(
            commands in prop::collection::vec(commands(), 1..40),
        ) {
            let mut reactor = Reactor::new().unwrap();
            reactor.shell = || Command::new("/bin/cat");
            reactor.open_tab(Command::new("/bin/cat"), 24, 80).unwrap();
            let mut peer = Peer::greet(&mut reactor);
            peer.send(Request::Resize { rows: 24, columns: 80 });
            let at = link::Position { row: 0, column: 0 };
            for (sent, command) in commands.into_iter().enumerate() {
                peer.send(command);
                // A link reply always comes, and only after the command before it was applied.
                peer.send(Request::Link { pane: u32::MAX, at });
                serve(&mut reactor, &mut peer, |reactor, peer| {
                    let tabs = reactor.session.tabs().iter().map(pinfold_mux::Tab::focused);
                    peer.linked.len() > sent
                        && peer.tabs.iter().copied().eq(tabs)
                        && usize::from(peer.active) == reactor.session.active()
                });
            }
            prop_assert!(reactor.has_window());
            let held: Vec<usize> = reactor.numbers().collect();
            let mut tabbed: Vec<usize> = (reactor.session.tabs().iter())
                .flat_map(|tab| tab.panes().map(|pane| pane as usize))
                .collect();
            tabbed.sort_unstable();
            prop_assert_eq!(held, tabbed);
        }
    }

    #[test]
    fn the_largest_frame_fits_the_cap_with_two_selections() {
        let reply = NOTICE_HEAD + MAX_SELECTION;
        assert!(MAX_PENDING >= size_of::<u32>() + MAX_MESSAGE + 2 * reply);
    }
}
