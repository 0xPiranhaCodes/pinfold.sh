//! The control socket: how a launcher finds the core and asks it for a window, and how
//! `pinfold ctl` lists panes.
//!
//! It takes `hello`, `window.open` and, after `hello` version 2, `pane.list`, `pane.read`,
//! `pane.last_output`, `pane.send`, `pane.wait`, `events.subscribe`, `audit.read`,
//! `grants.list`, `grants.revoke`, `workspace.trust_state`, `workspace.ask_trust` and
//! `session.save`, as newline-delimited JSON-RPC 2.0 (ADR-0009, ADR-0028, ADR-0032).
//! `window.open` takes no arguments, so a caller cannot name a program. This only checks a
//! request's shape: the reactor answers every [`Request`], checks its scope, parks waits and
//! sends events, so nothing here reaches a pane.
//! Same-user controls are guardrails (threat model).
//!
//! ```no_run
//! use std::io::{BufRead, BufReader, Write};
//! use std::os::unix::net::UnixStream;
//!
//! use pinfold_api::control::{self, Control};
//!
//! let directory = control::runtime_dir();
//! let core = Control::bind(&directory)?;
//! std::thread::spawn(move || core.serve(|request| request.answer(Ok(serde_json::json!({})))));
//!
//! let mut launcher = UnixStream::connect(control::socket_path(&directory))?;
//! launcher.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\",\"params\":{\"version\":1}}\n")?;
//! let mut reply = String::new();
//! BufReader::new(&launcher).read_line(&mut reply)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use pinfold_sys::Terminal;
use serde_json::{Value, json};

/// The newest version `hello` may name. It adds `pane.list` and says who the caller is.
pub const VERSION: u64 = 2;
/// The oldest version `hello` may name, which allows only `window.open`. Launchers send it, so
/// a new `pinfold` still opens a window on an older core (ADR-0032 point 6).
pub const OLDEST: u64 = 1;
/// Stack for the thread that calls [`Control::serve`], which holds `serde_json`'s deepest parse.
pub const STACK: usize = 256 * 1024;

/// The session's name in its file names. There is one session until M2 adds attach.
pub const SESSION: &str = "default";
/// Most peers served at once (ADR-0032). Past it a new peer is closed unread.
pub const MAX_PEERS: usize = 16;
/// Most peers of one caller identity, where all unidentified peers count as one (ADR-0032).
pub const MAX_PER_CALLER: usize = 4;
const LISTENER: Token = Token(usize::MAX);
const WAKE: Token = Token(usize::MAX - 1);
/// Longest request: a `pane.send` of [`MAX_SEND`] bytes, each escaped as `\u0000`, fits.
const MAX_LINE: usize = 32 * 1024;
/// Most bytes one read from a peer takes, so a peer's buffer grows only with what it sent.
const READ_CHUNK: usize = 4096;
/// Most bytes of text one `pane.send` carries. More is refused, not cut (threat model, Limits).
pub const MAX_SEND: usize = 4096;
/// Most bytes queued for a peer. One that will not read past it is closed (ADR-0032 point 5).
pub const MAX_OUTPUT: usize = 2 * 1024 * 1024;
/// Most characters of a `pane.status` message. More is refused, not cut.
pub const MAX_STATUS_MESSAGE: usize = 200;
/// The states a `pane.status` may report.
pub const STATES: [&str; 5] = ["needs-you", "running", "done", "error", "idle"];
/// Most entries one `audit.read` returns.
pub const MAX_AUDIT_READ: usize = 1000;
/// Longest `pane.wait` a caller may ask for.
pub const MAX_WAIT: Duration = Duration::from_secs(600);
/// Longest a peer may take to send its first request after it connects, and to end any line
/// after its first byte, so one that trickles bytes cannot hold a slot.
const TIMEOUT: Duration = Duration::from_secs(2);
/// How long a connection stays with no request in flight, wait or subscription (ADR-0032).
const IDLE: Duration = Duration::from_secs(60);
/// Most requests handed to the reactor and not yet taken back by this thread, each holding a
/// ticket. The answer channel holds this many, so the reactor never finds it full.
const ANSWERS: usize = 4 * MAX_PEERS;
/// Most events one subscriber may have queued for this thread. One whose next event will not
/// fit is closed, and its backlog never costs another subscriber an event.
const EVENTS_EACH: usize = 64;
/// Most events queued for this thread: every peer at its own limit, so the channel never fills.
const EVENTS: usize = MAX_PEERS * EVENTS_EACH;
/// How long the core waits for a window to say hello. Past it a launcher reports the window as
/// not ready, so a hung one is heard of, and the window may still open.
pub const OPEN_WAIT: Duration = Duration::from_secs(2);
/// Longest a cross-pane request may stay parked for a window answer.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(60);
/// [`OPEN_WAIT`] for the first window of a core, which starts with nothing warm, so that a slow
/// Mac does not report failure for a window that opens (`docs/spikes/m2-cold-start`).
pub const FIRST_OPEN_WAIT: Duration = Duration::from_secs(10);
/// Longest reply a client reads: the core closes a peer whose queue reaches [`MAX_OUTPUT`].
const MAX_REPLY: u64 = MAX_OUTPUT as u64;
/// Longest a client waits for an answer, including a cross-pane approval prompt.
const REPLY_WAIT: Duration = Duration::from_secs(APPROVAL_WAIT.as_secs() + OPEN_WAIT.as_secs());
/// Longest a launcher waits for `hello` and then `window.open`, which the core answers within
/// [`FIRST_OPEN_WAIT`]. Past it the core is hung, as one is whose disk image was ejected (#279).
const OPEN_REPLY_WAIT: Duration =
    Duration::from_secs(FIRST_OPEN_WAIT.as_secs() + OPEN_WAIT.as_secs());

const PARSE_ERROR: i64 = -32700;
const NOT_A_REQUEST: i64 = -32600;
const NO_METHOD: i64 = -32601;
const BAD_PARAMS: i64 = -32602;
const BAD_VERSION: i64 = -32001;
const NO_HELLO: i64 = -32002;
const NO_WINDOW: i64 = -32003;
const REFUSED: i64 = -32004;

/// What a caller asks the reactor for, with arguments of a checked shape. Whether the caller
/// may reach `pane` is the reactor's to check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// `hello` version 2, which needs the caller's pane.
    Hello,
    /// `window.open`.
    WindowOpen,
    /// `pane.list`.
    PaneList,
    /// `pane.read`: a pane's history and screen as plain text, or its last `lines` lines.
    PaneRead {
        /// The pane's number.
        pane: usize,
        /// How many of the newest lines, or all of them.
        lines: Option<usize>,
    },
    /// `pane.last_output`: the output of the pane's last command, from OSC 133 marks.
    PaneLastOutput {
        /// The pane's number.
        pane: usize,
    },
    /// `pane.send`: text for a pane's program, at most [`MAX_SEND`] bytes.
    PaneSend {
        /// The pane's number.
        pane: usize,
        /// The text, typed as it is.
        text: String,
    },
    /// `pane.status`: the state an agent reports for its own pane.
    PaneStatus {
        /// The pane's number.
        pane: usize,
        /// One of [`STATES`].
        state: String,
        /// At most [`MAX_STATUS_MESSAGE`] characters, or empty.
        message: String,
    },
    /// `pane.wait`, its shape checked.
    PaneWait(Wait),
    /// `events.subscribe`, its shape checked.
    EventsSubscribe(Topics),
    /// `audit.read`: the newest `count` audit entries, at most [`MAX_AUDIT_READ`], of one pane
    /// or all.
    AuditRead {
        /// How many of the newest entries.
        count: usize,
        /// Only calls by or to this pane.
        pane: Option<usize>,
    },
    /// `pane.spawn`, its shape checked.
    PaneSpawn(crate::spawn::Spawn),
    /// `agent.launch`, its shape checked.
    AgentLaunch(crate::spawn::Launch),
    /// `tool.open`, its shape checked.
    ToolOpen(crate::tool::Open),
    /// `pane.close`: a pane the caller spawned.
    PaneClose {
        /// The pane's number.
        pane: usize,
    },
    /// `session.save`: write the session's snapshot now.
    SessionSave,
    /// `workspace.open`, its shape checked.
    WorkspaceOpen(crate::workspace::Open),
    /// `grants.list`, accepted only from a terminal outside every pane.
    GrantsList,
    /// `grants.revoke`, accepted only from a terminal outside every pane.
    GrantsRevoke {
        /// Stable grant id.
        id: u64,
    },
    /// `workspace.trust_state`: whether a directory is trusted with a layout's hash.
    WorkspaceTrustState(Workspace),
    /// `workspace.ask_trust`: asks the window whether to trust a layout, showing every `run` line.
    /// Accepted only from a terminal outside every pane.
    WorkspaceAskTrust(Workspace, Vec<String>),
}

/// A workspace named by `workspace.trust_state` and `workspace.ask_trust`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    /// The project directory, an absolute path the reactor makes canonical.
    pub root: String,
    /// The layout file's SHA-256, 64 lowercase hexadecimal digits.
    pub sha256: String,
}

/// Most `run` lines one trust prompt shows.
pub const MAX_RUNS: usize = 64;

/// What `pane.wait` waits for, until its timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    /// The pane named, or nothing for `self`, the caller's own.
    pub pane: Option<usize>,
    /// What ends the wait, besides the pane closing.
    pub until: Until,
    /// At most [`MAX_WAIT`].
    pub timeout: Duration,
}

/// What ends a `pane.wait`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Until {
    /// The shell finishes a command (OSC 133 `D`).
    Exit,
    /// The shell draws its next prompt (OSC 133 `A`).
    Prompt,
    /// The program writes anything.
    Output,
    /// A line printed later matches this regex, at most [`MAX_PATTERN`] bytes, which the
    /// reactor compiles.
    Match(String),
    /// The program writes nothing for this long, from 100 ms to [`MAX_WAIT`].
    Idle(Duration),
}

/// Longest `pane.wait` pattern.
pub const MAX_PATTERN: usize = 1024;

/// The events a subscriber asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Topics {
    /// `pane.opened`.
    pub opened: bool,
    /// `pane.closed`.
    pub closed: bool,
    /// `pane.status`.
    pub status: bool,
    /// `pane.output`, for the caller's own pane only.
    pub output: bool,
}

/// Why the core cannot take the control socket.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A path could be reached or changed by someone else, or holds something unexpected.
    #[error("{path} is not safe to use: {reason}")]
    Unsafe {
        /// The path.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Another core holds this session.
    #[error("a core is already running")]
    Running,
    /// The socket's other end belongs to another user, so a launcher must not talk to it.
    #[error("the core at the socket belongs to user {uid}")]
    WrongPeer {
        /// That user.
        uid: u32,
    },
    /// The core answered with an error, or with nothing a launcher can read.
    #[error("the core refused: {0}")]
    Refused(String),
    /// The core refuses this client's `hello` version, as a core older than `pinfold ctl` does.
    #[error("the running PinFold does not speak this version: quit PinFold so a new core starts")]
    Old,
    /// The system refused.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Returns the directory that holds the session's socket: `$XDG_RUNTIME_DIR/pinfold`, or a
/// directory named for this user in the temporary directory where that is unset, as on macOS.
#[must_use]
pub fn runtime_dir() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    match runtime {
        Some(runtime) => runtime.join("pinfold"),
        // It may be a shared /tmp, so the name is the user's and `checked` refuses anyone else's.
        None => std::env::temp_dir().join(format!("pinfold-{}", pinfold_sys::effective_uid())),
    }
}

/// Returns the session's socket in `directory`.
#[must_use]
pub fn socket_path(directory: &Path) -> PathBuf {
    directory.join(SESSION).with_extension("sock")
}

/// Returns the session's log in `directory`.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// let path = pinfold_api::control::log_path(Path::new("/run/user/1000/pinfold"));
/// assert_eq!(path, Path::new("/run/user/1000/pinfold/default.log"));
/// ```
#[must_use]
pub fn log_path(directory: &Path) -> PathBuf {
    directory.join(SESSION).with_extension("log")
}

/// Returns the last core's log in `directory`, which [`start_log`] keeps.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// let path = pinfold_api::control::previous_log_path(Path::new("/run/user/1000/pinfold"));
/// assert_eq!(path, Path::new("/run/user/1000/pinfold/default.log.1"));
/// ```
#[must_use]
pub fn previous_log_path(directory: &Path) -> PathBuf {
    directory.join(SESSION).with_extension("log.1")
}

/// Opens an empty log for a core that is about to start. A log with something in it, such as
/// the panic line of a core that crashed, moves to [`previous_log_path`] first, over the one
/// there. An empty one does not, so a clean run keeps the crash before it. Two generations are
/// all that bound the size.
///
/// # Errors
///
/// [`Error::Unsafe`] for a directory others can reach, and [`Error::Io`] if the system refuses,
/// as it does for a symlink at the log's path.
///
/// # Examples
///
/// ```
/// let directory = std::env::temp_dir().join(format!("pinfold-doc-{}", std::process::id()));
/// let _log = pinfold_api::control::start_log(&directory)?;
/// assert!(pinfold_api::control::log_path(&directory).exists());
/// std::fs::remove_dir_all(directory)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn start_log(directory: &Path) -> Result<File, Error> {
    checked(directory)?;
    let path = log_path(directory);
    // Not a symlink or FIFO, which `open_log` refuses. A rename moves the entry and follows
    // nothing, and the moved file keeps its mode.
    if fs::symlink_metadata(&path).is_ok_and(|found| found.is_file() && found.len() > 0) {
        // A log that cannot move is emptied, as before, rather than left without a core.
        let _ = fs::rename(&path, previous_log_path(directory));
    }
    Ok(pinfold_sys::open_log(&path)?)
}

/// Makes `directory` if it is missing, and refuses it unless only this user can reach it.
///
/// # Errors
///
/// Returns an error if it cannot be made, or is a symlink, someone else's or open to others.
pub fn checked(directory: &Path) -> Result<(), Error> {
    match DirBuilder::new().mode(0o700).create(directory) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error.into()),
        _ => {}
    }
    let refuse = |reason| {
        Err(Error::Unsafe {
            path: directory.to_owned(),
            reason,
        })
    };
    let found = fs::symlink_metadata(directory)?;
    if !found.file_type().is_dir() {
        return refuse("it is a symlink or not a directory");
    }
    if found.uid() != pinfold_sys::effective_uid() {
        return refuse("it belongs to someone else");
    }
    if found.mode() & 0o077 != 0 {
        return refuse("other users can reach it");
    }
    Ok(())
}

/// The bound socket of one session.
#[derive(Debug)]
pub struct Control {
    listener: UnixListener,
    /// Held for the core's life. The system drops it on any exit, so a crash leaves no owner.
    _lock: File,
    /// [`IDLE`], shorter in tests.
    idle: Duration,
    /// The only peer UID served: the core's own, another in tests.
    user: u32,
}

impl Control {
    /// Takes the session in `directory`, replacing a socket that a dead core left.
    ///
    /// # Errors
    ///
    /// [`Error::Unsafe`] for a directory others can reach or anything but a socket at the
    /// socket's path, [`Error::Running`] if a core holds the session, and [`Error::Io`] if the
    /// system refuses.
    pub fn bind(directory: &Path) -> Result<Self, Error> {
        checked(directory)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(directory.join(SESSION).with_extension("lock"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Running),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let path = socket_path(directory);
        match fs::symlink_metadata(&path) {
            Ok(found) if found.file_type().is_socket() => fs::remove_file(&path)?,
            Ok(_) => {
                return Err(Error::Unsafe {
                    path,
                    reason: "it is not a socket",
                });
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            Err(_) => {}
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            _lock: lock,
            idle: IDLE,
            user: pinfold_sys::effective_uid(),
        })
    }

    /// Answers up to [`MAX_PEERS`] peers at once on this thread, for as long as the process
    /// lives (ADR-0032). `ask` is handed each request for the reactor and must not block: the
    /// reactor answers the [`Request`] later, or `ask` answers that the core is busy. A peer
    /// that fails costs only its own connection.
    ///
    /// A connection takes many requests, one at a time, each line at most 32 KiB. It must send
    /// its first within 2 seconds, end each line within 2 seconds of its first byte, and is
    /// closed after 60 seconds with no request in flight, wait or subscription.
    ///
    /// # Errors
    ///
    /// Only if the system refuses to set up the poll; after that it never returns.
    pub fn serve(&self, mut ask: impl FnMut(Request)) -> io::Result<()> {
        let mut poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        let (answers, answered) = mpsc::sync_channel(ANSWERS);
        let (events_to, evented) = mpsc::sync_channel::<Event>(EVENTS);
        let tickets = Arc::new(());
        self.listener.set_nonblocking(true)?;
        let listener = self.listener.as_raw_fd();
        poll.registry()
            .register(&mut SourceFd(&listener), LISTENER, Interest::READABLE)?;
        let mut peers: Vec<Option<Peer>> = (0..MAX_PEERS).map(|_| None).collect();
        let mut generation = 0;
        // The first window of a core starts with nothing warm.
        let mut wait = FIRST_OPEN_WAIT;
        let mut events = Events::with_capacity(MAX_PEERS + 2);
        loop {
            let now = Instant::now();
            let timeout = peers.iter().flatten().filter_map(Peer::deadline).min();
            match poll.poll(
                &mut events,
                timeout.map(|at| at.saturating_duration_since(now)),
            ) {
                Err(error) if error.kind() != io::ErrorKind::Interrupted => return Err(error),
                _ => {}
            }
            for event in &events {
                match event.token() {
                    LISTENER => self.accept(&poll, &mut peers, &mut generation),
                    WAKE => {}
                    Token(slot) => {
                        if let Some(peer) = peers.get_mut(slot).and_then(Option::as_mut) {
                            peer.read();
                        }
                    }
                }
            }
            loop {
                while let Ok((peer, result, _ticket)) = answered.try_recv() {
                    if let Some(peer) = at(&mut peers, peer) {
                        peer.answered(result);
                    }
                }
                // A subscription is answered before its events, so answers are taken before
                // each event to keep that order.
                let Ok((peer, params, queued)) = evented.try_recv() else {
                    break;
                };
                queued.fetch_sub(1, Ordering::Relaxed);
                let event = json!({"jsonrpc": "2.0", "method": "event", "params": params});
                if let Some(peer) = at(&mut peers, peer) {
                    peer.queue(&event);
                }
            }
            let now = Instant::now();
            for (slot, entry) in peers.iter_mut().enumerate() {
                let Some(peer) = entry else { continue };
                peer.expire(now);
                let (generation, caller) = (peer.generation, peer.caller);
                let alive = Arc::downgrade(&peer.alive);
                peer.step(now, &mut |method| {
                    // Each ticket is at most one answer, so the answer channel cannot fill.
                    if Arc::strong_count(&tickets) > ANSWERS {
                        return None;
                    }
                    let waits = match &method {
                        Method::WindowOpen => mem::replace(&mut wait, OPEN_WAIT),
                        // The reactor answers at the timeout, so this only catches one that cannot.
                        Method::PaneWait(Wait { timeout, .. }) => *timeout + OPEN_WAIT,
                        Method::PaneRead { .. }
                        | Method::PaneLastOutput { .. }
                        | Method::PaneSend { .. }
                        | Method::WorkspaceAskTrust(..)
                        | Method::SessionSave
                        | Method::GrantsRevoke { .. }
                        | Method::WorkspaceOpen(_) => APPROVAL_WAIT + OPEN_WAIT,
                        _ => OPEN_WAIT,
                    };
                    ask(Request {
                        peer: (slot, generation),
                        caller,
                        method,
                        alive: alive.clone(),
                        until: now + waits,
                        answers: answers.clone(),
                        events: events_to.clone(),
                        ticket: Arc::clone(&tickets),
                        waker: Arc::clone(&waker),
                    });
                    Some(waits)
                });
                peer.flush();
                if peer.done(now) {
                    let fd = peer.stream.as_raw_fd();
                    let _ = poll.registry().deregister(&mut SourceFd(&fd));
                    *entry = None;
                }
            }
        }
    }

    /// Takes every waiting peer of this user while a slot is free and its identity is under its
    /// cap, and closes the rest unread. A peer's identity is read once, here.
    fn accept(&self, poll: &Poll, peers: &mut [Option<Peer>], generation: &mut u64) {
        while let Ok((stream, _)) = self.listener.accept() {
            let Some(slot) = peers.iter().position(Option::is_none) else {
                continue;
            };
            if check_peer(stream.as_fd(), self.user).is_err() {
                continue;
            }
            // Unreadable is unidentified.
            let caller = pinfold_sys::peer_terminal(stream.as_fd()).ok().flatten();
            let same = peers.iter().flatten().filter(|peer| peer.caller == caller);
            if same.count() >= MAX_PER_CALLER || stream.set_nonblocking(true).is_err() {
                continue;
            }
            let interest = Interest::READABLE | Interest::WRITABLE;
            let fd = stream.as_raw_fd();
            if poll
                .registry()
                .register(&mut SourceFd(&fd), Token(slot), interest)
                .is_ok()
            {
                *generation += 1;
                peers[slot] = Some(Peer::new(stream, *generation, caller, self.idle));
            }
        }
    }
}

/// A request for the reactor, which alone opens windows and knows panes. The caller hears
/// [`Request::answer`], or that it waited too long once [`OPEN_WAIT`] has passed.
#[derive(Debug)]
pub struct Request {
    /// The peer's slot and generation, so an answer never reaches a later peer in that slot.
    peer: (usize, u64),
    caller: Option<Terminal>,
    method: Method,
    /// Dead once the connection has closed, and true once it missed an event.
    alive: Weak<AtomicBool>,
    answers: SyncSender<Answer>,
    events: SyncSender<Event>,
    /// Counts this request until the API thread takes its answer (see [`ANSWERS`]).
    ticket: Arc<()>,
    /// When the peer stops waiting for the answer.
    until: Instant,
    waker: Arc<Waker>,
}

impl Request {
    /// Whether the peer has stopped waiting, so acting now could only repeat what a caller
    /// that retries will ask for again.
    #[must_use]
    pub fn is_late(&self) -> bool {
        Instant::now() >= self.until
    }

    /// The terminal and session the caller held when it connected, or nothing if it had no
    /// terminal. Map it to a pane on the reactor's thread for each request, so a pane that has
    /// closed since stops matching.
    #[must_use]
    pub fn caller(&self) -> Option<Terminal> {
        self.caller
    }

    /// What the caller asks for.
    #[must_use]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Answers the caller with the method's result, or a short reason, which must carry no
    /// pane contents, tokens or environment. A caller that has gone hears nothing.
    pub fn answer(self, result: Result<Value, String>) {
        // Never full while its ticket lives, and it never blocks the reactor.
        if self
            .answers
            .try_send((self.peer, result, self.ticket))
            .is_ok()
        {
            let _ = self.waker.wake();
        }
    }

    /// Whether the caller's connection has closed, so a parked request can be dropped.
    #[must_use]
    pub fn is_gone(&self) -> bool {
        gone(&self.alive)
    }

    /// Returns what sends events on this request's connection, once its subscription has been
    /// answered.
    #[must_use]
    pub fn subscriber(&self) -> Subscriber {
        Subscriber {
            peer: self.peer,
            caller: self.caller,
            alive: self.alive.clone(),
            events: self.events.clone(),
            queued: Arc::default(),
            waker: Arc::clone(&self.waker),
        }
    }
}

/// What a peer's request in flight needs once it is answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Asked {
    /// `window.open`, whose failure says no window can open.
    Open,
    /// `events.subscribe`, which keeps the connection once it succeeds.
    Subscribe,
    /// `pane.wait`, dropped if its peer hangs up.
    Wait,
    /// Anything else.
    Other,
}

/// Sends `events.subscribe` notifications to one connection. The API thread closes a peer
/// whose queue passes [`MAX_OUTPUT`], so one that stops reading costs no more than that.
#[derive(Debug)]
pub struct Subscriber {
    peer: (usize, u64),
    caller: Option<Terminal>,
    alive: Weak<AtomicBool>,
    events: SyncSender<Event>,
    /// Its events sent and not yet taken by the API thread, at most [`EVENTS_EACH`].
    queued: Arc<AtomicUsize>,
    waker: Arc<Waker>,
}

impl Subscriber {
    /// The subscriber's caller, as [`Request::caller`] gives it.
    #[must_use]
    pub fn caller(&self) -> Option<Terminal> {
        self.caller
    }

    /// Sends `{"method": "event", "params": params}`, which carries pane contents only for the
    /// caller's own pane. It never blocks: past `EVENTS_EACH` (64) queued, the connection missed
    /// an event and is closed.
    pub fn send(&self, params: Value) {
        let room = self.queued.fetch_add(1, Ordering::Relaxed) < EVENTS_EACH;
        let sent = room.then(|| {
            self.events
                .try_send((self.peer, params, Arc::clone(&self.queued)))
        });
        match sent {
            Some(Ok(())) => {}
            Some(Err(TrySendError::Disconnected(_))) => return,
            None | Some(Err(TrySendError::Full(_))) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                if let Some(missed) = self.alive.upgrade() {
                    missed.store(true, Ordering::Relaxed);
                }
            }
        }
        let _ = self.waker.wake();
    }

    /// Whether the connection has closed or missed an event, so the subscription can be dropped.
    #[must_use]
    pub fn is_gone(&self) -> bool {
        gone(&self.alive)
    }
}

/// The peer in a slot, if it is still of that generation.
fn at(peers: &mut [Option<Peer>], (slot, generation): (usize, u64)) -> Option<&mut Peer> {
    let peer = peers.get_mut(slot).and_then(Option::as_mut);
    peer.filter(|peer| peer.generation == generation)
}

/// Whether a connection has closed or is closing because it missed an event.
fn gone(alive: &Weak<AtomicBool>) -> bool {
    alive
        .upgrade()
        .is_none_or(|missed| missed.load(Ordering::Relaxed))
}

/// An answer for the peer in a slot, of a generation, with its request's ticket.
type Answer = ((usize, u64), Result<Value, String>, Arc<()>);
/// An event for the peer in a slot, of a generation.
/// An event for the peer in a slot, of a generation, with its subscriber's count of queued events.
type Event = ((usize, u64), Value, Arc<AtomicUsize>);

/// Refuses a socket whose other end is not `user`.
fn check_peer(socket: BorrowedFd<'_>, user: u32) -> Result<(), Error> {
    match pinfold_sys::peer_uid(socket)? {
        uid if uid == user => Ok(()),
        uid => Err(Error::WrongPeer { uid }),
    }
}

/// Sends one request and returns its result.
fn call(stream: &UnixStream, request: &Value) -> Result<Value, Error> {
    let mut bytes = request.to_string().into_bytes();
    bytes.push(b'\n');
    (&mut &*stream).write_all(&bytes)?;
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_REPLY)).read_until(b'\n', &mut line)?;
    result(&line)
}

/// The result in one reply line, or its error.
fn result(line: &[u8]) -> Result<Value, Error> {
    let reply: Value = serde_json::from_slice(line)
        .map_err(|_| Error::Refused("the reply is not JSON-RPC".to_owned()))?;
    match reply.get("result") {
        Some(result) => Ok(result.clone()),
        None if reply.pointer("/error/code") == Some(&json!(BAD_VERSION)) => Err(Error::Old),
        None => {
            let message = reply.pointer("/error/message").and_then(Value::as_str);
            Err(Error::Refused(message.unwrap_or("no result").to_owned()))
        }
    }
}

/// Asks the core on `stream` for a window: a launcher's whole conversation. Refuses a peer that
/// is not this user before saying anything to it, as a socket path alone proves nothing.
/// Returns the core's result, which holds an `update` when the window binary is from another
/// install than the running core, so no window opened.
///
/// # Errors
///
/// [`Error::WrongPeer`] for another user's socket, [`Error::Refused`] if the core says no, and
/// [`Error::Io`] if it does not answer within 12 seconds.
pub fn open_window(stream: &UnixStream) -> Result<Value, Error> {
    check_peer(stream.as_fd(), pinfold_sys::effective_uid())?;
    stream.set_read_timeout(Some(OPEN_REPLY_WAIT))?;
    stream.set_write_timeout(Some(OPEN_REPLY_WAIT))?;
    let hello = json!({"version": OLDEST});
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": hello});
    call(stream, &hello)?;
    call(
        stream,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "window.open"}),
    )
}

/// Asks the core on `stream` for its panes and returns the `hello` and `pane.list` results:
/// `pinfold ctl list`'s whole conversation. Refuses another user's socket, as [`open_window`]
/// does.
///
/// # Errors
///
/// [`Error::Old`] for a core that refuses `hello` version 2, and as [`open_window`] otherwise.
pub fn list_panes(stream: &UnixStream) -> Result<(Value, Value), Error> {
    ask(stream, "pane.list", &json!({}))
}

/// Says `hello` version 2 to the core on `stream`, calls `method` with `params`, and returns
/// both results: a `pinfold ctl` command's whole conversation. Errors as [`list_panes`] does.
///
/// # Errors
///
/// As [`list_panes`].
pub fn ask(stream: &UnixStream, method: &str, params: &Value) -> Result<(Value, Value), Error> {
    check_peer(stream.as_fd(), pinfold_sys::effective_uid())?;
    // `pane.wait` answers only when it ends, so its timeout comes on top.
    let waits = params.get("timeout_ms").and_then(Value::as_u64);
    let waits = waits
        .map_or(Duration::ZERO, Duration::from_millis)
        .min(MAX_WAIT);
    stream.set_read_timeout(Some(REPLY_WAIT + waits))?;
    stream.set_write_timeout(Some(REPLY_WAIT))?;
    let hello = json!({"version": VERSION});
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": hello});
    let hello = call(stream, &hello)?;
    let asked = json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params});
    Ok((hello, call(stream, &asked)?))
}

/// Says `hello` version 2 to the core on `stream`, subscribes to `topics` (`events.subscribe`'s
/// params), and returns the next event's params, or `None` once `timeout` passes with none.
/// Refuses another user's socket, as [`ask`] does.
///
/// # Errors
///
/// As [`ask`], and [`Error::Io`] if the core closes the connection before an event.
pub fn next_event(
    stream: &UnixStream,
    topics: &Value,
    timeout: Duration,
) -> Result<Option<Value>, Error> {
    check_peer(stream.as_fd(), pinfold_sys::effective_uid())?;
    stream.set_read_timeout(Some(REPLY_WAIT))?;
    stream.set_write_timeout(Some(REPLY_WAIT))?;
    call(
        stream,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": {"version": VERSION}}),
    )?;
    let subscribe =
        json!({"jsonrpc": "2.0", "id": 2, "method": "events.subscribe", "params": topics});
    let mut bytes = subscribe.to_string().into_bytes();
    bytes.push(b'\n');
    (&mut &*stream).write_all(&bytes)?;
    // One reader for the result and the events, as an event may come in the same read.
    let mut reader = BufReader::new(stream.take(MAX_REPLY));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    result(&line)?;
    let until = Instant::now() + timeout;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        reader.get_ref().get_ref().set_read_timeout(Some(left))?;
        line.clear();
        reader.get_mut().set_limit(MAX_REPLY);
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(_) if line.ends_with(b"\n") => {
                let message: Value = serde_json::from_slice(&line)
                    .map_err(|_| Error::Refused("the event is not JSON-RPC".to_owned()))?;
                if message["method"] == "event" {
                    return Ok(Some(message["params"].clone()));
                }
            }
            Ok(_) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// One connection on the API thread.
struct Peer {
    stream: UnixStream,
    generation: u64,
    /// The peer's terminal and session at accept, fixed for the connection.
    caller: Option<Terminal>,
    /// Bytes read and not yet taken as lines, at most [`MAX_LINE`].
    input: Vec<u8>,
    /// The peer ended or failed.
    ended: bool,
    /// The `hello` version, or 0 before it.
    greeted: u64,
    output: Vec<u8>,
    /// The connection ends once its output is written.
    closing: bool,
    /// When it is closed if it has no request in flight and no subscription.
    until: Instant,
    /// [`IDLE`], or shorter in tests.
    idle: Duration,
    /// When the line it has begun must have ended.
    line_by: Option<Instant>,
    /// The id of the request waiting for the reactor, what it was, and when it stops waiting.
    asked: Option<(Value, Asked, Instant)>,
    /// Held while the connection lives, so the reactor can drop its waits and subscription. Set
    /// once its subscriber misses an event, which closes it.
    alive: Arc<AtomicBool>,
    /// It has a subscription, so it stays past `until`.
    subscribed: bool,
    /// The peer closed its end or failed.
    hung_up: bool,
    /// Its queue passed [`MAX_OUTPUT`], so it is closed at once.
    gone: bool,
}

impl Peer {
    fn new(stream: UnixStream, generation: u64, caller: Option<Terminal>, idle: Duration) -> Self {
        Self {
            stream,
            generation,
            caller,
            input: Vec::new(),
            ended: false,
            greeted: 0,
            output: Vec::new(),
            closing: false,
            // Its first request is due sooner than the idle limit.
            until: Instant::now() + TIMEOUT,
            idle,
            line_by: None,
            asked: None,
            alive: Arc::new(AtomicBool::new(false)),
            subscribed: false,
            hung_up: false,
            gone: false,
        }
    }

    /// When the poll must wake for this peer, if ever.
    fn deadline(&self) -> Option<Instant> {
        match &self.asked {
            Some((_, _, at)) => Some(*at),
            None => [(!self.subscribed).then_some(self.until), self.line_by]
                .into_iter()
                .flatten()
                .min(),
        }
    }

    /// Whether it is idle past its time, or slow to end a line.
    fn late(&self, now: Instant) -> bool {
        (now >= self.until && !self.subscribed) || self.line_by.is_some_and(|at| now >= at)
    }

    /// Reads what the peer has sent while its buffer has room for a line. The rest waits in the
    /// socket until the lines before it are taken.
    fn read(&mut self) {
        let mut buffer = [0; READ_CHUNK];
        while !self.ended {
            let room = MAX_LINE.saturating_sub(self.input.len()).min(buffer.len());
            if room == 0 {
                return;
            }
            match (&mut &self.stream).read(&mut buffer[..room]) {
                Ok(0) => (self.ended, self.hung_up) = (true, true),
                Ok(read) => self.input.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => (self.ended, self.hung_up) = (true, true),
            }
        }
    }

    /// Answers each whole line, in order, until one waits for the reactor. `ask` hands it the
    /// method and returns how long the caller waits, or nothing if the core is busy.
    fn step(&mut self, now: Instant, ask: &mut impl FnMut(Method) -> Option<Duration>) {
        while self.asked.is_none() && !self.closing {
            self.read();
            let line = self
                .input
                .iter()
                .take(MAX_LINE)
                .position(|&byte| byte == b'\n');
            let Some(end) = line else {
                // A line past the limit, or a last one with no end.
                self.closing = self.input.len() >= MAX_LINE || self.ended;
                self.line_by =
                    (!self.input.is_empty()).then(|| self.line_by.unwrap_or(now + TIMEOUT));
                return;
            };
            let line: Vec<u8> = self.input.drain(..=end).take(end).collect();
            if self.input.is_empty() && self.input.capacity() > READ_CHUNK {
                self.input = Vec::new();
            }
            (self.line_by, self.until) = (None, now + self.idle);
            match answer(&line, &mut self.greeted) {
                Step::Reply(reply, go_on) => {
                    self.queue(&reply);
                    self.closing = !go_on;
                }
                Step::Ask(id, Method::EventsSubscribe(_)) if self.subscribed => {
                    self.queue(&refuse(&id, REFUSED, "one subscription per connection").0);
                    self.closing = true;
                }
                Step::Ask(id, method) => {
                    let asked = match method {
                        Method::WindowOpen => Asked::Open,
                        Method::EventsSubscribe(_) => Asked::Subscribe,
                        Method::PaneWait(_) => Asked::Wait,
                        _ => Asked::Other,
                    };
                    match ask(method) {
                        Some(waits) => self.asked = Some((id, asked, now + waits)),
                        None => self.queue(&refuse(&id, REFUSED, "the core is busy").0),
                    }
                }
            }
        }
    }

    /// Hands the waiting request its answer.
    fn answered(&mut self, result: Result<Value, String>) {
        let Some((id, asked, _)) = self.asked.take() else {
            return;
        };
        // A refusal from the reactor keeps the connection, unlike a malformed request.
        let reply = match (result, asked) {
            (Ok(result), _) => {
                self.subscribed |= asked == Asked::Subscribe;
                done(&id, &result)
            }
            (Err(reason), Asked::Open) => {
                refuse(&id, NO_WINDOW, &format!("no window can open: {reason}")).0
            }
            (Err(reason), Asked::Subscribe | Asked::Wait | Asked::Other) => {
                refuse(&id, REFUSED, &reason).0
            }
        };
        self.queue(&reply);
        self.until = Instant::now() + self.idle;
    }

    /// Tells a request that waited too long that the core did not answer in time.
    fn expire(&mut self, now: Instant) {
        let Some((_, asked, at)) = &self.asked else {
            return;
        };
        if now >= *at {
            let reason = if *asked == Asked::Open {
                "the window is not ready and may still open"
            } else {
                "the core is busy"
            };
            self.answered(Err(reason.to_owned()));
            // Its late answer could be taken for the next request's, so the connection ends.
            self.closing = true;
        }
    }

    fn queue(&mut self, reply: &Value) {
        if reply.is_null() {
            return;
        }
        let line = reply.to_string();
        if self.output.len() + line.len() >= MAX_OUTPUT {
            // A peer that will not read is closed rather than buffered for.
            (self.output, self.gone) = (Vec::new(), true);
            return;
        }
        self.output.extend(line.bytes());
        self.output.push(b'\n');
    }

    /// Writes what the socket will take now.
    fn flush(&mut self) {
        while !self.output.is_empty() {
            match (&mut &self.stream).write(&self.output) {
                Ok(written) => drop(self.output.drain(..written)),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.output.clear();
                    self.closing = true;
                }
            }
        }
        // A subscriber that caught up keeps no large queue.
        if self.output.capacity() > MAX_LINE {
            self.output = Vec::new();
        }
    }

    /// Whether the connection is over: its queue overflowed, it missed an event, it hung up on a
    /// wait, it said its last, or its time is up while it waits for nothing.
    fn done(&self, now: Instant) -> bool {
        let waits = matches!(self.asked, Some((_, Asked::Wait, _)));
        self.gone
            || self.alive.load(Ordering::Relaxed)
            || (self.hung_up && waits)
            || (self.asked.is_none()
                && ((self.closing && self.output.is_empty()) || self.late(now)))
    }
}

/// What one request leads to.
#[derive(Debug, PartialEq)]
enum Step {
    /// This reply, null for none, and whether the connection goes on.
    Reply(Value, bool),
    /// The reactor answers the request with this id.
    Ask(Value, Method),
}

fn done(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn refuse(id: &Value, code: i64, message: &str) -> (Value, bool) {
    let error = json!({"code": code, "message": message});
    (json!({"jsonrpc": "2.0", "id": id, "error": error}), false)
}

/// Returns what one request leads to. A null reply means a notification: JSON-RPC gives it no
/// answer, and no method here is one, so it does nothing.
fn answer(line: &[u8], greeted: &mut u64) -> Step {
    let refuse = |id: Option<&Value>, code, message: &str| {
        let (reply, go_on) = refuse(id.unwrap_or(&Value::Null), code, message);
        Step::Reply(reply, go_on)
    };
    let Ok(request) = serde_json::from_slice::<Value>(line) else {
        return refuse(None, PARSE_ERROR, "not JSON");
    };
    let id = request.get("id");
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !matches!(
            id,
            None | Some(Value::Null | Value::Number(_) | Value::String(_))
        )
    {
        return refuse(None, NOT_A_REQUEST, "not a JSON-RPC 2.0 request");
    }
    let Some(id) = id else {
        return Step::Reply(Value::Null, false);
    };
    match request.get("method").and_then(Value::as_str) {
        Some("hello") if *greeted > 0 => refuse(Some(id), NO_METHOD, "hello comes once"),
        Some("hello") => match request.pointer("/params/version").and_then(Value::as_u64) {
            Some(OLDEST) => {
                *greeted = OLDEST;
                let result = json!({"version": OLDEST, "capabilities": ["window.open"]});
                Step::Reply(done(id, &result), true)
            }
            // The reactor knows the caller's pane, so it answers.
            Some(VERSION) => {
                *greeted = VERSION;
                Step::Ask(id.clone(), Method::Hello)
            }
            _ => refuse(Some(id), BAD_VERSION, "unknown version"),
        },
        _ if *greeted == 0 => refuse(Some(id), NO_HELLO, "hello comes first"),
        Some("window.open") => Step::Ask(id.clone(), Method::WindowOpen),
        Some(
            method @ ("pane.list"
            | "pane.read"
            | "pane.last_output"
            | "pane.send"
            | "pane.spawn"
            | "pane.close"
            | "grants.list"
            | "grants.revoke"
            | "workspace.open"
            | "workspace.trust_state"
            | "workspace.ask_trust"
            | "session.save"
            | "agent.launch"
            | "tool.open"),
        ) if *greeted < VERSION => refuse(
            Some(id),
            NO_METHOD,
            &format!("{method} needs hello version 2"),
        ),
        Some("pane.list") => Step::Ask(id.clone(), Method::PaneList),
        Some("session.save") => Step::Ask(id.clone(), Method::SessionSave),
        Some(method @ ("pane.wait" | "events.subscribe" | "audit.read" | "pane.status"))
            if *greeted < VERSION =>
        {
            refuse(
                Some(id),
                NO_METHOD,
                &format!("{method} needs hello version 2"),
            )
        }
        Some("pane.wait") => match wait(request.get("params")) {
            Some(wait) => Step::Ask(id.clone(), Method::PaneWait(wait)),
            None => refuse(
                Some(id),
                BAD_PARAMS,
                "pane.wait takes pane, until, timeout_ms, and pattern or idle_ms",
            ),
        },
        Some("events.subscribe") => match topics(request.get("params")) {
            Some(topics) => Step::Ask(id.clone(), Method::EventsSubscribe(topics)),
            None => refuse(Some(id), BAD_PARAMS, "events.subscribe takes known events"),
        },
        Some("audit.read") => match audit_read(request.get("params")) {
            Some(method) => Step::Ask(id.clone(), method),
            None => refuse(
                Some(id),
                BAD_PARAMS,
                "audit.read takes count up to 1000 and pane",
            ),
        },
        Some("pane.spawn") => match crate::spawn::params(request.get("params")) {
            Ok(spawn) => Step::Ask(id.clone(), Method::PaneSpawn(spawn)),
            Err(reason) => refuse(Some(id), BAD_PARAMS, reason),
        },
        Some("agent.launch") => match crate::spawn::launch(request.get("params")) {
            Ok(launch) => Step::Ask(id.clone(), Method::AgentLaunch(launch)),
            Err(reason) => refuse(Some(id), BAD_PARAMS, reason),
        },
        Some("tool.open") => match crate::tool::params(request.get("params")) {
            Ok(open) => Step::Ask(id.clone(), Method::ToolOpen(open)),
            Err(reason) => refuse(Some(id), BAD_PARAMS, reason),
        },
        Some("workspace.open") => match crate::workspace::params(request.get("params")) {
            Some(open) => Step::Ask(id.clone(), Method::WorkspaceOpen(open)),
            None => refuse(
                Some(id),
                BAD_PARAMS,
                "workspace.open takes an absolute root and a layout of tabs, splits and cwds",
            ),
        },
        Some(
            method
            @ ("pane.read" | "pane.last_output" | "pane.send" | "pane.close" | "pane.status"),
        ) => match pane_method(method, request.get("params")) {
            Ok(method) => Step::Ask(id.clone(), method),
            Err(reason) => refuse(Some(id), BAD_PARAMS, reason),
        },
        Some("grants.list") => Step::Ask(id.clone(), Method::GrantsList),
        Some("grants.revoke") => match request.pointer("/params/id").and_then(Value::as_u64) {
            Some(grant @ 1..) => Step::Ask(id.clone(), Method::GrantsRevoke { id: grant }),
            None | Some(0) => refuse(
                Some(id),
                BAD_PARAMS,
                "grants.revoke takes a positive numeric id",
            ),
        },
        Some(method @ ("workspace.trust_state" | "workspace.ask_trust")) => {
            match workspace(method, request.get("params")) {
                Ok(method) => Step::Ask(id.clone(), method),
                Err(reason) => refuse(Some(id), BAD_PARAMS, reason),
            }
        }
        _ => refuse(Some(id), NO_METHOD, "unknown method"),
    }
}

/// Reads `pane.wait`'s parameters: `pane` is `"self"` or a pane number, and `timeout_ms` is
/// required and at most [`MAX_WAIT`].
fn wait(params: Option<&Value>) -> Option<Wait> {
    let params = params?;
    let pane = match params.get("pane")? {
        Value::String(pane) if pane == "self" => None,
        pane => Some(usize::try_from(pane.as_u64()?).ok()?),
    };
    let until = match params.get("until")?.as_str()? {
        "exit" => Until::Exit,
        "prompt" => Until::Prompt,
        "output" => Until::Output,
        "match" => {
            let pattern = params.get("pattern")?.as_str()?;
            (pattern.len() <= MAX_PATTERN).then(|| Until::Match(pattern.to_owned()))?
        }
        "idle" => {
            let quiet = Duration::from_millis(params.get("idle_ms")?.as_u64()?);
            let allowed = (Duration::from_millis(100)..=MAX_WAIT).contains(&quiet);
            allowed.then_some(Until::Idle(quiet))?
        }
        _ => return None,
    };
    let timeout = Duration::from_millis(params.get("timeout_ms")?.as_u64()?);
    (timeout <= MAX_WAIT).then_some(Wait {
        pane,
        until,
        timeout,
    })
}

/// Reads `events.subscribe`'s parameters: a list of at least one known event.
fn topics(params: Option<&Value>) -> Option<Topics> {
    let mut topics = Topics::default();
    for name in params?.get("events")?.as_array()? {
        match name.as_str()? {
            "pane.opened" => topics.opened = true,
            "pane.closed" => topics.closed = true,
            "pane.status" => topics.status = true,
            "pane.output" => topics.output = true,
            _ => return None,
        }
    }
    (topics != Topics::default()).then_some(topics)
}

/// Reads `audit.read`'s parameters: an optional `count`, 100 by default and at most
/// [`MAX_AUDIT_READ`], and an optional `pane`.
fn audit_read(params: Option<&Value>) -> Option<Method> {
    let field = |name| {
        params
            .and_then(|params| params.get(name))
            .filter(|v| !v.is_null())
    };
    let number = |value: &Value| usize::try_from(value.as_u64()?).ok();
    let count = field("count").map_or(Some(100), number)?;
    let pane = match field("pane") {
        Some(pane) => Some(number(pane)?),
        None => None,
    };
    (count <= MAX_AUDIT_READ).then_some(Method::AuditRead { count, pane })
}

/// Checks the shape of a pane method's `params`. Scope is the reactor's to check.
fn pane_method(method: &str, params: Option<&Value>) -> Result<Method, &'static str> {
    let field = |name| params.and_then(|params| params.get(name));
    // JSON Schema's integer takes 1.0 as well as 1.
    let count = |value: &Value| {
        let integral = || {
            value
                .as_f64()
                .filter(|n| n.fract() == 0.0 && (0.0..=1e15).contains(n))
        };
        let count = value.as_u64().or_else(|| integral().map(|n| n as u64));
        count.and_then(|count| usize::try_from(count).ok())
    };
    let pane = field("pane")
        .and_then(count)
        .ok_or("params.pane must be a pane number")?;
    match method {
        "pane.read" => {
            let lines = match field("lines") {
                None | Some(Value::Null) => None,
                Some(lines) => Some(count(lines).ok_or("params.lines must be a count")?),
            };
            Ok(Method::PaneRead { pane, lines })
        }
        "pane.send" => {
            let text = field("text")
                .and_then(Value::as_str)
                .ok_or("params.text must be a string")?;
            if text.len() > MAX_SEND {
                return Err("params.text is over 4096 bytes");
            }
            Ok(Method::PaneSend {
                pane,
                text: text.to_owned(),
            })
        }
        "pane.status" => {
            let state = field("state").and_then(Value::as_str);
            let state = state
                .filter(|state| STATES.contains(state))
                .ok_or("params.state must be needs-you, running, done, error or idle")?;
            let message = match field("message") {
                None | Some(Value::Null) => "",
                Some(message) => message.as_str().ok_or("params.message must be a string")?,
            };
            if message.chars().count() > MAX_STATUS_MESSAGE {
                return Err("params.message is over 200 characters");
            }
            Ok(Method::PaneStatus {
                pane,
                state: state.to_owned(),
                message: message.to_owned(),
            })
        }
        "pane.close" => Ok(Method::PaneClose { pane }),
        _ => Ok(Method::PaneLastOutput { pane }),
    }
}

/// Checks a workspace method's `params`: an absolute `root`, a lowercase hex `sha256`, and for
/// `workspace.ask_trust` at most [`MAX_RUNS`] `runs` strings.
fn workspace(method: &str, params: Option<&Value>) -> Result<Method, &'static str> {
    let field = |name| params.and_then(|params| params.get(name));
    let root = field("root")
        .and_then(Value::as_str)
        .filter(|root| root.starts_with('/'))
        .ok_or("params.root must be an absolute path")?;
    let sha256 = field("sha256")
        .and_then(Value::as_str)
        .filter(|hash| {
            hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
        .ok_or("params.sha256 must be 64 lowercase hexadecimal digits")?;
    let workspace = Workspace {
        root: root.to_owned(),
        sha256: sha256.to_owned(),
    };
    if method == "workspace.trust_state" {
        return Ok(Method::WorkspaceTrustState(workspace));
    }
    let runs = field("runs")
        .and_then(Value::as_array)
        .filter(|runs| runs.len() <= MAX_RUNS)
        .and_then(|runs| {
            runs.iter()
                .map(|run| Some(run.as_str()?.to_owned()))
                .collect()
        })
        .ok_or("params.runs must be at most 64 strings")?;
    Ok(Method::WorkspaceAskTrust(workspace, runs))
}

/// Feeds one bounded request line through every parser state used by a control-socket peer.
#[doc(hidden)]
pub fn fuzz_request_line(line: &[u8]) {
    let line = &line[..line.len().min(MAX_LINE)];
    for mut greeted in [0, OLDEST, VERSION] {
        let _ = answer(line, &mut greeted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    const HELLO: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":1}}"#;
    const OPEN: &[u8] = br#"{"jsonrpc":"2.0","id":2,"method":"window.open"}"#;
    const PATIENCE: Duration = Duration::from_secs(120);

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_new_log_keeps_the_last_one_with_text_and_never_an_empty_one() {
        let directory = fresh();
        writeln!(start_log(&directory).unwrap(), "panicked at here").unwrap();
        let _log = start_log(&directory).unwrap();
        let read = |path: PathBuf| fs::read_to_string(path).unwrap();
        assert_eq!(read(log_path(&directory)), "");
        assert_eq!(read(previous_log_path(&directory)), "panicked at here\n");
        let _log = start_log(&directory).unwrap();
        assert_eq!(read(previous_log_path(&directory)), "panicked at here\n");
        let mode = fs::metadata(previous_log_path(&directory)).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A new path in a fresh private directory.
    fn fresh() -> PathBuf {
        let parent = std::env::temp_dir().join(format!(
            "pc{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&parent);
        let _ = DirBuilder::new().mode(0o700).create(&parent);
        parent.join("pinfold")
    }

    fn code(reply: &Value) -> Option<i64> {
        reply.pointer("/error/code").and_then(Value::as_i64)
    }

    /// The reply to a request that wants no window.
    fn replied(line: &[u8], greeted: &mut u64) -> (Value, bool) {
        match answer(line, greeted) {
            Step::Reply(reply, go_on) => (reply, go_on),
            Step::Ask(id, method) => panic!("{id} asked the reactor for {method:?}"),
        }
    }

    /// Answers every window request at once with `result`.
    fn serving(result: Result<Value, String>) -> impl FnMut(Request) {
        move |request| request.answer(result.clone())
    }

    #[test]
    fn hello_comes_first_and_once() {
        let mut greeted = 0;
        let (reply, go_on) = replied(OPEN, &mut greeted);
        assert_eq!((code(&reply), go_on), (Some(NO_HELLO), false));
        let (reply, go_on) = replied(HELLO, &mut greeted);
        assert_eq!(reply["result"]["version"], json!(OLDEST));
        assert_eq!(reply["result"]["capabilities"], json!(["window.open"]));
        assert_eq!(reply["id"], json!(1));
        assert!(go_on);
        let (reply, go_on) = replied(HELLO, &mut greeted);
        assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false));
    }

    #[test]
    fn a_wrong_version_is_refused() {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":3}}"#;
        let mut greeted = 0;
        let (reply, go_on) = replied(line, &mut greeted);
        assert_eq!(
            (code(&reply), go_on, greeted),
            (Some(BAD_VERSION), false, 0)
        );
    }

    #[test]
    fn version_1_cannot_list_panes_and_version_2_asks_the_reactor() {
        let list = br#"{"jsonrpc":"2.0","id":2,"method":"pane.list"}"#;
        let (reply, go_on) = replied(list, &mut OLDEST.clone());
        assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false));
        assert!(
            reply.to_string().contains("needs hello version 2"),
            "{reply}"
        );

        let hello = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":2}}"#;
        let mut greeted = 0;
        assert_eq!(
            answer(hello, &mut greeted),
            Step::Ask(json!(1), Method::Hello)
        );
        assert_eq!(
            answer(list, &mut greeted),
            Step::Ask(json!(2), Method::PaneList)
        );
        let save = br#"{"jsonrpc":"2.0","id":3,"method":"session.save"}"#;
        assert_eq!(
            answer(save, &mut greeted),
            Step::Ask(json!(3), Method::SessionSave)
        );
        let (reply, _) = replied(save, &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
    }

    /// A request for a pane method with these params.
    fn pane_request(method: &str, params: &Value) -> String {
        json!({"jsonrpc": "2.0", "id": 2, "method": method, "params": params}).to_string()
    }

    #[test]
    fn pane_methods_need_version_2_and_well_formed_params() {
        let read = pane_request("pane.read", &json!({"pane": 3}));
        let (reply, _) = replied(read.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        let asked = |method, params| {
            answer(
                pane_request(method, &params).as_bytes(),
                &mut VERSION.clone(),
            )
        };
        let read = |pane, lines| Step::Ask(json!(2), Method::PaneRead { pane, lines });
        assert_eq!(asked("pane.read", json!({"pane": 3})), read(3, None));
        assert_eq!(
            asked("pane.read", json!({"pane": 3, "lines": 5})),
            read(3, Some(5))
        );
        assert_eq!(
            asked("pane.read", json!({"pane": 3.0, "lines": 5.0})),
            read(3, Some(5))
        );
        assert_eq!(
            asked("pane.last_output", json!({"pane": 0})),
            Step::Ask(json!(2), Method::PaneLastOutput { pane: 0 })
        );
        assert_eq!(
            asked("pane.close", json!({"pane": 4})),
            Step::Ask(json!(2), Method::PaneClose { pane: 4 })
        );
        let spawn = pane_request("pane.spawn", &json!({"where": "tab", "command": "sh"}));
        let (reply, _) = replied(spawn.as_bytes(), &mut VERSION.clone());
        assert_eq!(code(&reply), Some(BAD_PARAMS));
        let (reply, _) = replied(spawn.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        let launch = pane_request("agent.launch", &json!({"profile": "a", "command": ["sh"]}));
        let (reply, _) = replied(launch.as_bytes(), &mut VERSION.clone());
        assert_eq!(code(&reply), Some(BAD_PARAMS));
        let launch = pane_request("agent.launch", &json!({"profile": "a"}));
        let (reply, _) = replied(launch.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        // A refused tool never reaches the reactor, so no process starts.
        let tool = pane_request(
            "tool.open",
            &json!({"tool": "diff", "path": "/r", "range": "-p"}),
        );
        let (reply, _) = replied(tool.as_bytes(), &mut VERSION.clone());
        assert_eq!(code(&reply), Some(BAD_PARAMS));
        let tool = pane_request("tool.open", &json!({"tool": "files", "path": "/r"}));
        let (reply, _) = replied(tool.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        assert!(matches!(
            answer(tool.as_bytes(), &mut VERSION.clone()),
            Step::Ask(_, Method::ToolOpen(_))
        ));
        let bad = [
            json!(null),
            json!({"pane": -1}),
            json!({"pane": "1"}),
            json!({"pane": 1.5}),
            json!([1]),
        ];
        for params in bad {
            let line = pane_request("pane.last_output", &params);
            let (reply, go_on) = replied(line.as_bytes(), &mut VERSION.clone());
            assert_eq!((code(&reply), go_on), (Some(BAD_PARAMS), false), "{params}");
        }
        let line = pane_request("pane.read", &json!({"pane": 3, "lines": -1}));
        let (reply, _) = replied(line.as_bytes(), &mut VERSION.clone());
        assert_eq!(code(&reply), Some(BAD_PARAMS));
    }

    #[test]
    fn a_send_over_4096_bytes_is_refused_whole() {
        let send = |text: &str| pane_request("pane.send", &json!({"pane": 1, "text": text}));
        // The longest a send's line can be: every byte escaped.
        let most = "\0".repeat(MAX_SEND);
        let line = send(&most);
        assert!(line.len() <= MAX_LINE, "{}", line.len());
        assert_eq!(
            answer(line.as_bytes(), &mut VERSION.clone()),
            Step::Ask(
                json!(2),
                Method::PaneSend {
                    pane: 1,
                    text: most
                }
            )
        );
        let line = send(&"x".repeat(MAX_SEND + 1));
        let (reply, go_on) = replied(line.as_bytes(), &mut VERSION.clone());
        assert_eq!((code(&reply), go_on), (Some(BAD_PARAMS), false));
        let line = pane_request("pane.send", &json!({"pane": 1}));
        let (reply, _) = replied(line.as_bytes(), &mut VERSION.clone());
        assert_eq!(code(&reply), Some(BAD_PARAMS));
    }

    #[test]
    fn no_api_method_can_answer_an_approval_prompt() {
        let line = br#"{"jsonrpc":"2.0","id":2,"method":"approval.answer","params":{"id":1,"answer":"allow"}}"#;
        let (reply, go_on) = replied(line, &mut VERSION.clone());
        assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false));
    }

    #[test]
    fn workspace_trust_takes_an_absolute_root_a_sha256_and_bounded_runs() {
        let request = |method: &str, params: Value| {
            json!({"jsonrpc": "2.0", "id": 3, "method": method, "params": params}).to_string()
        };
        let hash = "a".repeat(64);
        let workspace = Workspace {
            root: "/p".to_owned(),
            sha256: hash.clone(),
        };
        let line = request(
            "workspace.ask_trust",
            json!({"root": "/p", "sha256": hash, "runs": ["make"]}),
        );
        let (reply, _) = replied(line.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        let asked = answer(line.as_bytes(), &mut VERSION.clone());
        let method = Method::WorkspaceAskTrust(workspace.clone(), vec!["make".to_owned()]);
        assert_eq!(asked, Step::Ask(json!(3), method));
        let line = request(
            "workspace.trust_state",
            json!({"root": "/p", "sha256": hash}),
        );
        let asked = answer(line.as_bytes(), &mut VERSION.clone());
        assert_eq!(
            asked,
            Step::Ask(json!(3), Method::WorkspaceTrustState(workspace))
        );
        for (method, params) in [
            (
                "workspace.trust_state",
                json!({"root": "p", "sha256": hash}),
            ),
            (
                "workspace.trust_state",
                json!({"root": "/p", "sha256": "A".repeat(64)}),
            ),
            (
                "workspace.trust_state",
                json!({"root": "/p", "sha256": "a"}),
            ),
            ("workspace.ask_trust", json!({"root": "/p", "sha256": hash})),
            (
                "workspace.ask_trust",
                json!({"root": "/p", "sha256": hash, "runs": [1]}),
            ),
            (
                "workspace.ask_trust",
                json!({"root": "/p", "sha256": hash, "runs": vec![""; MAX_RUNS + 1]}),
            ),
        ] {
            let (reply, _) = replied(request(method, params).as_bytes(), &mut VERSION.clone());
            assert_eq!(code(&reply), Some(BAD_PARAMS), "{method}");
        }
    }

    #[test]
    fn a_status_needs_version_2_a_known_state_and_a_short_message() {
        let status = |params: Value| pane_request("pane.status", &params);
        let most = "é".repeat(MAX_STATUS_MESSAGE);
        let line = status(json!({"pane": 1, "state": "needs-you", "message": most}));
        assert_eq!(
            answer(line.as_bytes(), &mut VERSION.clone()),
            Step::Ask(
                json!(2),
                Method::PaneStatus {
                    pane: 1,
                    state: "needs-you".to_owned(),
                    message: most
                }
            )
        );
        let line = status(json!({"pane": 1, "state": "idle"}));
        let (reply, _) = replied(line.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        let bad = [
            json!({"pane": 1, "state": "needs you"}),
            json!({"pane": 1}),
            json!({"pane": "self", "state": "done"}),
            json!({"pane": 1, "state": "done", "message": 7}),
            json!({"pane": 1, "state": "done", "message": "x".repeat(MAX_STATUS_MESSAGE + 1)}),
        ];
        for params in bad {
            let (reply, go_on) = replied(status(params.clone()).as_bytes(), &mut VERSION.clone());
            assert_eq!((code(&reply), go_on), (Some(BAD_PARAMS), false), "{params}");
        }
    }

    #[test]
    fn wait_and_subscribe_need_version_2_and_well_formed_params() {
        let request = |method: &str, params: Value| {
            json!({"jsonrpc": "2.0", "id": 3, "method": method, "params": params}).to_string()
        };
        let wait = |params| request("pane.wait", params);
        let line = wait(json!({"pane": "self", "until": "exit", "timeout_ms": 600_000}));
        let (reply, _) = replied(line.as_bytes(), &mut OLDEST.clone());
        assert_eq!(code(&reply), Some(NO_METHOD));
        let expected = Wait {
            pane: None,
            until: Until::Exit,
            timeout: MAX_WAIT,
        };
        let asked = answer(line.as_bytes(), &mut VERSION.clone());
        assert_eq!(asked, Step::Ask(json!(3), Method::PaneWait(expected)));
        let line = wait(json!({"pane": 4, "until": "output", "timeout_ms": 5}));
        let Step::Ask(_, Method::PaneWait(asked)) = answer(line.as_bytes(), &mut VERSION.clone())
        else {
            panic!("pane 4 was refused");
        };
        assert_eq!((asked.pane, asked.until), (Some(4), Until::Output));
        for (params, until) in [
            (
                json!({"until": "match", "pattern": "x".repeat(MAX_PATTERN)}),
                Until::Match("x".repeat(MAX_PATTERN)),
            ),
            (
                json!({"until": "idle", "idle_ms": 100}),
                Until::Idle(Duration::from_millis(100)),
            ),
        ] {
            let mut params = params;
            params["pane"] = json!("self");
            params["timeout_ms"] = json!(5);
            let Step::Ask(_, Method::PaneWait(asked)) =
                answer(wait(params).as_bytes(), &mut VERSION.clone())
            else {
                panic!("a well-formed wait was refused");
            };
            assert_eq!(asked.until, until);
        }

        let subscribe = |events| request("events.subscribe", json!({"events": events}));
        let line = subscribe(json!(["pane.opened", "pane.status", "pane.output"]));
        let topics = Topics {
            opened: true,
            closed: false,
            status: true,
            output: true,
        };
        let asked = answer(line.as_bytes(), &mut VERSION.clone());
        assert_eq!(asked, Step::Ask(json!(3), Method::EventsSubscribe(topics)));

        for line in [
            wait(json!({"pane": "self", "until": "exit"})),
            wait(json!({"pane": "self", "until": "exit", "timeout_ms": 600_001})),
            wait(json!({"pane": "other", "until": "exit", "timeout_ms": 1})),
            wait(json!({"pane": -1, "until": "exit", "timeout_ms": 1})),
            wait(json!({"pane": "self", "until": "never", "timeout_ms": 1})),
            wait(json!({"pane": "self", "until": "match", "timeout_ms": 1})),
            wait(
                json!({"pane": "self", "until": "match", "pattern": "x".repeat(MAX_PATTERN + 1), "timeout_ms": 1}),
            ),
            wait(json!({"pane": "self", "until": "idle", "idle_ms": 99, "timeout_ms": 1})),
            wait(json!({"pane": "self", "until": "idle", "idle_ms": 600_001, "timeout_ms": 1})),
            subscribe(json!([])),
            subscribe(json!(["pane.contents"])),
            request("events.subscribe", json!(null)),
        ] {
            let (reply, go_on) = replied(line.as_bytes(), &mut VERSION.clone());
            assert_eq!((code(&reply), go_on), (Some(BAD_PARAMS), false), "{line}");
        }
    }

    #[test]
    fn audit_read_takes_a_count_up_to_1000_and_a_pane() {
        let read = |params: Value| {
            let line = json!({"jsonrpc": "2.0", "id": 3, "method": "audit.read", "params": params});
            answer(line.to_string().as_bytes(), &mut VERSION.clone())
        };
        let asked = |count, pane| Step::Ask(json!(3), Method::AuditRead { count, pane });
        assert_eq!(read(json!({})), asked(100, None));
        assert_eq!(
            read(json!({"count": 1000, "pane": 2})),
            asked(1000, Some(2))
        );
        for params in [json!({"count": 1001}), json!({"pane": "self"})] {
            assert!(matches!(read(params), Step::Reply(..)));
        }
    }

    /// Connects to a fresh core served by `serve`, says hello version 2 and sends `line`.
    fn asking(serve: impl FnMut(Request) + Send + 'static, line: &str) -> UnixStream {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        thread::spawn(move || control.serve(serve));
        let mut stream = UnixStream::connect(socket_path(&directory)).unwrap();
        stream.set_read_timeout(Some(10 * TIMEOUT)).unwrap();
        let hello = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":2}}"#;
        writeln!(stream, "{hello}\n{line}").unwrap();
        stream
    }

    #[test]
    fn a_wait_outlives_the_peer_limit_and_is_gone_once_its_connection_closes() {
        let (asked, waits) = mpsc::channel();
        let serve = move |request: Request| match request.method() {
            Method::PaneWait(_) => asked.send(request).unwrap(),
            _ => request.answer(Ok(json!({}))),
        };
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"pane.wait","params":{"pane":"self","until":"exit","timeout_ms":60000}}"#;
        let stream = asking(serve, line);
        let wait = waits.recv_timeout(TIMEOUT).unwrap();
        thread::sleep(TIMEOUT + TIMEOUT / 4);
        assert!(!wait.is_gone(), "a parked wait was cut at the peer limit");
        drop(stream);
        let start = Instant::now();
        while !wait.is_gone() && start.elapsed() < 2 * TIMEOUT {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(wait.is_gone());
    }

    #[test]
    fn a_subscriber_hears_events_past_the_peer_limit_and_only_one_subscription() {
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"events":["pane.closed"]}}"#;
        let serve = |request: Request| {
            if let Method::EventsSubscribe(_) = request.method() {
                let subscriber = request.subscriber();
                thread::spawn(move || {
                    thread::sleep(TIMEOUT + TIMEOUT / 4);
                    subscriber.send(json!({"event": "pane.closed", "pane": 1}));
                });
            }
            request.answer(Ok(json!({})));
        };
        let mut stream = asking(serve, line);
        let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
        let mut next = || serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap();
        assert_eq!(next()["id"], json!(1));
        assert_eq!(next()["result"], json!({}));
        let event = next();
        assert_eq!(event["method"], "event");
        assert!(event.get("id").is_none());
        assert_eq!(event["params"], json!({"event": "pane.closed", "pane": 1}));
        writeln!(stream, "{}", line.replace("\"id\":2", "\"id\":3")).unwrap();
        assert_eq!(code(&next()), Some(REFUSED));
    }

    #[test]
    fn a_subscriber_that_will_not_read_is_closed_at_the_cap() {
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"events":["pane.status"]}}"#;
        let serve = |request: Request| {
            let subscriber = request.subscriber();
            let subscribed = matches!(request.method(), Method::EventsSubscribe(_));
            request.answer(Ok(json!({})));
            // Twice the cap, all queued before the API thread writes any of it, in fewer
            // events than its queue holds.
            let padding = "x".repeat(MAX_OUTPUT / 64);
            let count = if subscribed {
                2 * MAX_OUTPUT / padding.len()
            } else {
                0
            };
            for _ in 0..count {
                subscriber.send(json!({"event": "pane.status", "padding": padding}));
            }
        };
        let mut stream = asking(serve, line);
        let mut received = Vec::new();
        stream.read_to_end(&mut received).unwrap();
        assert!(received.len() < MAX_OUTPUT, "{} bytes", received.len());
    }

    #[test]
    fn ctl_hears_the_reactors_answers_and_is_told_to_restart_an_old_core() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        thread::spawn(move || {
            control.serve(|request| match request.method() {
                Method::Hello => request.answer(Ok(json!({"pane": null}))),
                _ => request.answer(Ok(json!({"panes": []}))),
            })
        });
        let stream = UnixStream::connect(socket_path(&directory)).unwrap();
        let (hello, list) = list_panes(&stream).unwrap();
        assert_eq!((hello, list), (json!({"pane": null}), json!({"panes": []})));

        let (old, core) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(&core).read_line(&mut line).unwrap();
            let refusal = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": BAD_VERSION}});
            (&core)
                .write_all(format!("{refusal}\n").as_bytes())
                .unwrap();
        });
        assert!(matches!(list_panes(&old), Err(Error::Old)));
    }

    #[test]
    fn only_hello_and_window_open_exist() {
        for method in ["pane.write", "window.close", "", "HELLO"] {
            let line = json!({"jsonrpc": "2.0", "id": 1, "method": method}).to_string();
            let mut greeted = OLDEST;
            let (reply, go_on) = replied(line.as_bytes(), &mut greeted);
            assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false), "{method}");
        }
    }

    #[test]
    fn window_open_asks_for_a_window_and_ignores_arguments() {
        let line =
            br#"{"jsonrpc":"2.0","id":2,"method":"window.open","params":{"path":"/bin/sh"}}"#;
        assert_eq!(
            answer(line, &mut OLDEST.clone()),
            Step::Ask(json!(2), Method::WindowOpen)
        );
    }

    #[test]
    fn only_json_rpc_2_requests_are_taken() {
        for line in [
            json!({"id": 1, "method": "window.open"}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "window.open"}),
            json!({"jsonrpc": "2.0", "id": [1], "method": "window.open"}),
            json!(null),
        ] {
            let line = line.to_string();
            let (reply, go_on) = replied(line.as_bytes(), &mut OLDEST.clone());
            assert_eq!(
                (code(&reply), go_on),
                (Some(NOT_A_REQUEST), false),
                "{line}"
            );
        }
    }

    #[test]
    fn a_notification_gets_no_reply_and_does_nothing() {
        let line = br#"{"jsonrpc":"2.0","method":"window.open"}"#;
        assert_eq!(
            answer(line, &mut OLDEST.clone()),
            Step::Reply(Value::Null, false)
        );
    }

    #[test]
    fn nothing_a_peer_sends_panics() {
        let nested = vec![b'['; MAX_LINE];
        let lines: [&[u8]; 6] = [
            b"",
            b"\xff\xfe",
            b"null",
            b"[]",
            b"{\"method\":7}",
            b"{\"method\":\"hello\",\"params\":3}",
        ];
        let worker = thread::Builder::new().stack_size(STACK).spawn(move || {
            for line in lines.into_iter().chain([nested.as_slice()]) {
                let (reply, go_on) = replied(line, &mut 0);
                assert!(code(&reply).is_some() && !go_on);
            }
        });
        assert!(worker.unwrap().join().is_ok());
    }

    #[test]
    fn a_new_directory_is_private_and_so_is_the_socket() {
        let directory = fresh();
        let _control = Control::bind(&directory).unwrap();
        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&directory), 0o700);
        assert_eq!(mode(&socket_path(&directory)), 0o600);
    }

    #[test]
    fn a_symlink_or_open_directory_is_refused() {
        let real = fresh();
        DirBuilder::new().mode(0o700).create(&real).unwrap();
        let link = real.with_file_name("link");
        symlink(&real, &link).unwrap();
        assert!(matches!(Control::bind(&link), Err(Error::Unsafe { .. })));

        fs::set_permissions(&real, fs::Permissions::from_mode(0o750)).unwrap();
        assert!(matches!(Control::bind(&real), Err(Error::Unsafe { .. })));

        let file = real.with_file_name("file");
        File::create(&file).unwrap();
        assert!(matches!(Control::bind(&file), Err(Error::Unsafe { .. })));
    }

    #[test]
    fn a_socket_path_too_long_for_the_system_is_an_error() {
        // Longer than `sun_path` holds on any supported system.
        let directory = fresh().with_file_name("d".repeat(120));
        match Control::bind(&directory) {
            Err(Error::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::InvalidInput),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_session_has_one_core_and_a_dead_one_leaves_no_owner() {
        let directory = fresh();
        let first = Control::bind(&directory).unwrap();
        assert!(matches!(Control::bind(&directory), Err(Error::Running)));
        drop(first);
        assert!(socket_path(&directory).exists());
        let start = Instant::now();
        loop {
            match Control::bind(&directory) {
                Ok(_) => break,
                Err(Error::Running) if start.elapsed() < PATIENCE => {
                    thread::sleep(Duration::from_millis(10));
                }
                result => panic!("the dead core kept its session: {result:?}"),
            }
        }
    }

    #[test]
    fn something_else_at_the_socket_path_is_kept() {
        let directory = fresh();
        DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let path = socket_path(&directory);
        fs::write(&path, b"keep").unwrap();
        assert!(matches!(
            Control::bind(&directory),
            Err(Error::Unsafe { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"keep");
    }

    #[test]
    fn a_slow_peer_cannot_hold_the_socket() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        thread::spawn(move || control.serve(serving(Ok(json!({})))));

        let path = socket_path(&directory);
        let mut slow = UnixStream::connect(&path).unwrap();
        thread::spawn(move || {
            // Each byte comes well inside the deadline, so only a total limit ends this.
            while slow.write_all(b"x").is_ok() {
                thread::sleep(Duration::from_millis(50));
            }
        });
        thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(2 * TIMEOUT)).unwrap();
        peer.write_all(OPEN).unwrap();
        peer.write_all(b"\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&peer).read_line(&mut reply).unwrap();
        assert!(reply.contains("hello comes first"), "{reply}");
        assert!(start.elapsed() < 2 * TIMEOUT);
    }

    #[test]
    fn a_launcher_gets_its_window_and_hears_a_refusal() {
        for (window, opened) in [(true, true), (false, false)] {
            let directory = fresh();
            let control = Control::bind(&directory).unwrap();
            let open = serving(window.then(|| json!({})).ok_or_else(|| "why".to_owned()));
            thread::spawn(move || control.serve(open));
            let stream = UnixStream::connect(socket_path(&directory)).unwrap();
            match open_window(&stream) {
                Ok(result) => assert!(opened && result == json!({})),
                Err(error) => {
                    assert!(!opened);
                    let refused = "the core refused: no window can open: why";
                    assert_eq!(error.to_string(), refused);
                }
            }
        }
    }

    #[test]
    fn a_launcher_says_nothing_to_another_users_socket() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let other = pinfold_sys::effective_uid().wrapping_add(1);
        assert!(matches!(
            check_peer(ours.as_fd(), other),
            Err(Error::WrongPeer { uid }) if uid == pinfold_sys::effective_uid()
        ));
        drop(ours);
        assert_eq!(theirs.read(&mut [0; 8]).unwrap(), 0);
    }

    #[test]
    fn the_core_closes_another_users_connection_unread() {
        let directory = fresh();
        let mut control = Control::bind(&directory).unwrap();
        control.user = pinfold_sys::effective_uid().wrapping_add(1);
        thread::spawn(move || control.serve(serving(Ok(json!({})))));
        let mut stream = UnixStream::connect(socket_path(&directory)).unwrap();
        let _ = stream.write_all(HELLO);
        let _ = stream.write_all(b"\n");
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        // Linux reports a close that left the hello unread as a reset.
        match stream.read(&mut [0; 8]) {
            Ok(0) => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
            other => panic!("the core answered another user: {other:?}"),
        }
    }

    #[test]
    fn a_connection_gets_answers_and_cannot_stay_for_ever() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        thread::spawn(move || control.serve(serving(Ok(json!({})))));

        let mut stream = UnixStream::connect(socket_path(&directory)).unwrap();
        stream.write_all(HELLO).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.write_all(OPEN).unwrap();
        stream.write_all(b"\n").unwrap();
        let mut reader = BufReader::new(&stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        let hello: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(hello["jsonrpc"], "2.0");
        assert_eq!(hello["id"], json!(1));
        assert_eq!(hello["result"]["version"], json!(OLDEST));
        reply.clear();
        reader.read_line(&mut reply).unwrap();
        let opened: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!((&opened["id"], &opened["result"]), (&json!(2), &json!({})));

        // A line with no end is cut at the limit, and the next peer is served.
        let mut flood = UnixStream::connect(socket_path(&directory)).unwrap();
        let _ = flood.write_all(&vec![b'x'; 3 * MAX_LINE]);
        let mut peer = UnixStream::connect(socket_path(&directory)).unwrap();
        peer.write_all(OPEN).unwrap();
        peer.write_all(b"\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&peer).read_line(&mut reply).unwrap();
        assert!(reply.contains("hello comes first"), "{reply}");
    }

    /// Serves `serve` on a fresh core whose connections idle for at most `idle`.
    fn core(serve: impl FnMut(Request) + Send + 'static, idle: Duration) -> PathBuf {
        let directory = fresh();
        let mut control = Control::bind(&directory).unwrap();
        control.idle = idle;
        thread::spawn(move || control.serve(serve));
        socket_path(&directory)
    }

    /// Reads to the end, and returns the lines read and how long the core took to close.
    fn until_closed(stream: &UnixStream) -> (Vec<Value>, Duration) {
        let start = Instant::now();
        let lines = BufReader::new(stream).lines().map(|line| line.unwrap());
        let lines = lines
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect();
        (lines, start.elapsed())
    }

    #[test]
    fn a_connection_takes_many_requests_and_closes_when_idle() {
        let idle = TIMEOUT / 4;
        let path = core(serving(Ok(json!({}))), idle);
        let mut stream = UnixStream::connect(path).unwrap();
        stream.set_read_timeout(Some(10 * TIMEOUT)).unwrap();
        let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
        stream.write_all(HELLO).unwrap();
        writeln!(stream).unwrap();
        assert!(lines.next().unwrap().unwrap().contains("\"id\":1"));
        // Each answer resets the idle time, so three requests outlast it.
        for id in 2..5 {
            thread::sleep(idle * 2 / 3);
            let open = json!({"jsonrpc": "2.0", "id": id, "method": "window.open"});
            writeln!(stream, "{open}").unwrap();
            let reply: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert_eq!((&reply["id"], &reply["result"]), (&json!(id), &json!({})));
        }
        let (rest, after) = until_closed(&stream);
        assert!(rest.is_empty(), "{rest:?}");
        assert!(after >= idle / 2 && after < 2 * TIMEOUT, "{after:?}");
    }

    #[test]
    fn the_first_request_and_each_line_come_soon_however_long_idle_is() {
        let path = core(serving(Ok(json!({}))), 100 * TIMEOUT);
        let silent = UnixStream::connect(&path).unwrap();
        let (lines, after) = until_closed(&silent);
        assert!(lines.is_empty() && after < 2 * TIMEOUT, "{after:?}");

        let mut slow = UnixStream::connect(&path).unwrap();
        slow.write_all(HELLO).unwrap();
        writeln!(slow).unwrap();
        let mut writer = slow.try_clone().unwrap();
        thread::spawn(move || {
            // Each byte comes well inside the deadline, but the line never ends.
            while writer.write_all(b" ").is_ok() {
                thread::sleep(Duration::from_millis(50));
            }
        });
        let (lines, after) = until_closed(&slow);
        assert_eq!(lines.len(), 1);
        assert!(after >= TIMEOUT / 2 && after < 2 * TIMEOUT, "{after:?}");
    }

    #[test]
    fn a_full_request_channel_answers_busy_and_keeps_the_connection() {
        // As the core asks its reactor, which here never takes the first request.
        let (to, _held) = mpsc::sync_channel(1);
        let serve = move |request: Request| {
            if let Err(TrySendError::Full(request)) = to.try_send(request) {
                request.answer(Err("the core is busy".to_owned()));
            }
        };
        let path = core(serve, IDLE);
        let hello = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":2}}"#;
        let mut first = UnixStream::connect(&path).unwrap();
        first.write_all(hello).unwrap();
        writeln!(first).unwrap();
        thread::sleep(Duration::from_millis(100));
        let mut second = UnixStream::connect(&path).unwrap();
        second.set_read_timeout(Some(TIMEOUT)).unwrap();
        second.write_all(hello).unwrap();
        writeln!(second, "\n{}", String::from_utf8_lossy(OPEN)).unwrap();
        let mut lines = BufReader::new(&second).lines();
        for id in [1, 2] {
            let reply: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert_eq!(reply["id"], json!(id));
            let message = reply["error"]["message"].as_str().unwrap();
            assert!(message.ends_with("the core is busy"), "{message}");
        }
    }

    #[test]
    fn a_subscriber_whose_events_overflow_is_closed() {
        let (gone, subscribers) = mpsc::channel();
        let serve = move |request: Request| {
            let subscribed = matches!(request.method(), Method::EventsSubscribe(_));
            let subscriber = request.subscriber();
            request.answer(Ok(json!({})));
            if subscribed {
                // All sent before the API thread takes any, so the last one does not fit.
                for _ in 0..=EVENTS_EACH {
                    subscriber.send(json!({"event": "pane.status"}));
                }
                gone.send(subscriber).unwrap();
            }
        };
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"events.subscribe","params":{"events":["pane.status"]}}"#;
        let stream = asking(serve, line);
        let subscriber = subscribers.recv_timeout(TIMEOUT).unwrap();
        assert!(subscriber.is_gone());
        let (lines, after) = until_closed(&stream);
        // What was queued before the missed event may still be written.
        assert!(
            lines.len() <= EVENTS_EACH + 2 && after < TIMEOUT,
            "{} {after:?}",
            lines.len()
        );
    }

    #[test]
    fn a_peer_waiting_for_a_window_holds_up_no_one() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        // Never answered, as by a core whose window hangs.
        let mut held = Vec::new();
        thread::spawn(move || control.serve(move |request| held.push(request)));

        let path = socket_path(&directory);
        let mut waiting = UnixStream::connect(&path).unwrap();
        waiting.write_all(HELLO).unwrap();
        waiting.write_all(b"\n").unwrap();
        waiting.write_all(OPEN).unwrap();
        waiting.write_all(b"\n").unwrap();
        thread::sleep(Duration::from_millis(100));

        let start = Instant::now();
        let mut peer = UnixStream::connect(&path).unwrap();
        peer.set_read_timeout(Some(TIMEOUT)).unwrap();
        peer.write_all(HELLO).unwrap();
        peer.write_all(b"\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&peer).read_line(&mut reply).unwrap();
        assert!(reply.contains("\"version\""), "{reply}");
        assert!(start.elapsed() < TIMEOUT);
    }

    #[test]
    fn past_its_identitys_cap_a_peer_is_closed_unread() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        // Each peer here is this process, so all share one identity, or are all unidentified.
        let (asked, callers) = mpsc::channel();
        thread::spawn(move || control.serve(move |request| asked.send(request).unwrap()));

        let path = socket_path(&directory);
        let mut held: Vec<UnixStream> = (0..MAX_PER_CALLER)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        held[0].write_all(HELLO).unwrap();
        held[0].write_all(b"\n").unwrap();
        held[0].write_all(OPEN).unwrap();
        held[0].write_all(b"\n").unwrap();
        let request = callers.recv_timeout(TIMEOUT).unwrap();
        let probe = UnixListener::bind(fresh()).unwrap();
        let _us = UnixStream::connect(probe.local_addr().unwrap().as_pathname().unwrap());
        let expected = pinfold_sys::peer_terminal(probe.accept().unwrap().0.as_fd()).unwrap();
        assert_eq!(request.caller(), expected);
        let mut over = UnixStream::connect(&path).unwrap();
        // macOS refuses the option with EINVAL once the core has closed it, which is the point.
        if let Err(error) = over.set_read_timeout(Some(TIMEOUT / 2)) {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{error}");
        }
        assert_eq!(over.read(&mut [0; 8]).unwrap(), 0);
        drop(held);
    }

    #[test]
    fn a_subscriber_that_hangs_up_frees_its_slot() {
        let path = core(serving(Ok(json!({}))), IDLE);
        let topics = json!({"events": ["pane.closed"]});
        // The core hears each hang-up after the next connect may arrive, so each try may be
        // closed unread at the identity's cap for a moment.
        fn eventually(mut call: impl FnMut() -> bool) {
            let until = Instant::now() + TIMEOUT;
            while !call() {
                assert!(Instant::now() < until, "slots stayed taken");
                thread::sleep(Duration::from_millis(10));
            }
        }
        for _ in 0..2 * MAX_PEERS {
            eventually(|| {
                let stream = UnixStream::connect(&path).unwrap();
                next_event(&stream, &topics, Duration::ZERO).is_ok_and(|event| event.is_none())
            });
        }
        eventually(|| {
            let stream = UnixStream::connect(&path).unwrap();
            ask(&stream, "pane.list", &json!({})).is_ok()
        });
    }
}
