//! The control socket: how a launcher finds the core and asks it for a window.
//!
//! Until M3 it takes only `hello` and `window.open`, as newline-delimited JSON-RPC 2.0
//! (ADR-0009, ADR-0028). `window.open` takes no arguments, so a caller cannot name a program,
//! and nothing here reaches a pane. Same-user controls are guardrails (threat model).
//!
//! ```no_run
//! use std::io::{BufRead, BufReader, Write};
//! use std::os::unix::net::UnixStream;
//!
//! use pinfold_api::control::{self, Control};
//!
//! let directory = control::runtime_dir();
//! let core = Control::bind(&directory)?;
//! std::thread::spawn(move || core.serve(|opening| opening.answer(Ok(()))));
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
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use pinfold_sys::Terminal;
use serde_json::{Value, json};

/// The version `hello` must name.
pub const VERSION: u64 = 1;
/// Stack for the thread that calls [`Control::serve`], which holds `serde_json`'s deepest parse.
pub const STACK: usize = 256 * 1024;

/// The session's name in its file names. There is one session until M2 adds attach.
const SESSION: &str = "default";
/// Most peers served at once (ADR-0032). Past it a new peer is closed unread.
pub const MAX_PEERS: usize = 16;
/// Most peers of one caller identity, where all unidentified peers count as one (ADR-0032).
pub const MAX_PER_CALLER: usize = 4;
const LISTENER: Token = Token(usize::MAX);
const WAKE: Token = Token(usize::MAX - 1);
/// Longest request, far above what `hello` and `window.open` need.
const MAX_LINE: usize = 4096;
/// Most bytes one connection may send: both requests, so it cannot stay and keep talking.
const MAX_CONNECTION: u64 = 2 * MAX_LINE as u64;
/// Longest a peer may spend sending, however it spaces its bytes, so it cannot hold a slot.
const TIMEOUT: Duration = Duration::from_secs(2);
/// How long the core waits for a window to say hello. Past it a launcher reports the window as
/// not ready, so a hung one is heard of, and the window may still open.
pub const OPEN_WAIT: Duration = Duration::from_secs(2);
/// [`OPEN_WAIT`] for the first window of a core, which starts with nothing warm, so that a slow
/// Mac does not report failure for a window that opens (`docs/spikes/m2-cold-start`).
pub const FIRST_OPEN_WAIT: Duration = Duration::from_secs(10);
/// Longest a launcher waits for each answer. It can queue behind one peer's first window, then
/// have its own.
const REPLY_WAIT: Duration = Duration::from_secs(FIRST_OPEN_WAIT.as_secs() + OPEN_WAIT.as_secs());

const PARSE_ERROR: i64 = -32700;
const NOT_A_REQUEST: i64 = -32600;
const NO_METHOD: i64 = -32601;
const BAD_VERSION: i64 = -32001;
const NO_HELLO: i64 = -32002;
const NO_WINDOW: i64 = -32003;

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

/// Opens an empty log for a core that is about to start, which is all that bounds its size.
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
    Ok(pinfold_sys::open_log(&log_path(directory))?)
}

/// Makes `directory` if it is missing, and refuses it unless only this user can reach it.
fn checked(directory: &Path) -> Result<(), Error> {
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
        })
    }

    /// Answers up to [`MAX_PEERS`] peers at once on this thread, for as long as the process
    /// lives (ADR-0032). `open` is handed each request for a window and must not block: the
    /// thread that can open one answers the [`Opening`] later. A peer that fails costs only its
    /// own connection.
    ///
    /// # Errors
    ///
    /// Only if the system refuses to set up the poll; after that it never returns.
    pub fn serve(&self, mut open: impl FnMut(Opening)) -> io::Result<()> {
        let mut poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        let (answers, answered) = mpsc::channel();
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
            let timeout = peers.iter().flatten().map(Peer::deadline).min();
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
            while let Ok(((slot, generation), result)) = answered.try_recv() {
                let peer = peers.get_mut(slot).and_then(Option::as_mut);
                if let Some(peer) = peer.filter(|peer: &&mut Peer| peer.generation == generation) {
                    peer.opened(result);
                }
            }
            let now = Instant::now();
            for (slot, entry) in peers.iter_mut().enumerate() {
                let Some(peer) = entry else { continue };
                peer.expire(now);
                let (generation, caller) = (peer.generation, peer.caller);
                peer.step(now, &mut || {
                    open(Opening {
                        peer: (slot, generation),
                        caller,
                        answers: answers.clone(),
                        waker: Arc::clone(&waker),
                    });
                    mem::replace(&mut wait, OPEN_WAIT)
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
            let ours = pinfold_sys::peer_uid(stream.as_fd())
                .is_ok_and(|uid| uid == pinfold_sys::effective_uid());
            if !ours {
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
                peers[slot] = Some(Peer::new(stream, *generation, caller));
            }
        }
    }
}

/// A request for a window, handed to the thread that can open one. The caller hears
/// [`Opening::answer`], or that the window is not ready once [`OPEN_WAIT`] has passed.
#[derive(Debug)]
pub struct Opening {
    /// The peer's slot and generation, so an answer never reaches a later peer in that slot.
    peer: (usize, u64),
    caller: Option<Terminal>,
    answers: Sender<Answer>,
    waker: Arc<Waker>,
}

impl Opening {
    /// The terminal and session the caller held when it connected, or nothing if it had no
    /// terminal. Map it to a pane on the reactor's thread for each request, so a pane that has
    /// closed since stops matching.
    #[must_use]
    pub fn caller(&self) -> Option<Terminal> {
        self.caller
    }

    /// Answers the caller: `Ok` once a window is ready, or a short reason, which must carry no
    /// pane contents, tokens or environment. A caller that has gone hears nothing.
    pub fn answer(self, result: Result<(), String>) {
        if self.answers.send((self.peer, result)).is_ok() {
            let _ = self.waker.wake();
        }
    }
}

/// An answer for the peer in a slot, of a generation.
type Answer = ((usize, u64), Result<(), String>);

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
    BufReader::new(stream.take(MAX_LINE as u64)).read_until(b'\n', &mut line)?;
    let reply: Value = serde_json::from_slice(&line)
        .map_err(|_| Error::Refused("the reply is not JSON-RPC".to_owned()))?;
    match reply.get("result") {
        Some(result) => Ok(result.clone()),
        None => {
            let message = reply.pointer("/error/message").and_then(Value::as_str);
            Err(Error::Refused(message.unwrap_or("no result").to_owned()))
        }
    }
}

/// Asks the core on `stream` for a window: a launcher's whole conversation. Refuses a peer that
/// is not this user before saying anything to it, as a socket path alone proves nothing.
///
/// # Errors
///
/// [`Error::WrongPeer`] for another user's socket, [`Error::Refused`] if the core says no, and
/// [`Error::Io`] if it does not answer.
pub fn open_window(stream: &UnixStream) -> Result<(), Error> {
    check_peer(stream.as_fd(), pinfold_sys::effective_uid())?;
    stream.set_read_timeout(Some(REPLY_WAIT))?;
    stream.set_write_timeout(Some(REPLY_WAIT))?;
    let hello = json!({"version": VERSION});
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": hello});
    call(stream, &hello)?;
    call(
        stream,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "window.open"}),
    )?;
    Ok(())
}

/// One connection on the API thread.
struct Peer {
    stream: UnixStream,
    generation: u64,
    /// The peer's terminal and session at accept, fixed for the connection.
    caller: Option<Terminal>,
    /// Bytes read and not yet taken as lines.
    input: Vec<u8>,
    /// Bytes read in all, at most [`MAX_CONNECTION`].
    taken: u64,
    /// The peer sent all it may: it ended, failed, or reached the limit.
    ended: bool,
    greeted: bool,
    output: Vec<u8>,
    /// The connection ends once its output is written.
    closing: bool,
    /// When reading stops, however the peer spaces its bytes.
    until: Instant,
    /// The id of the request waiting for a window, and when it stops waiting.
    opening: Option<(Value, Instant)>,
}

impl Peer {
    fn new(stream: UnixStream, generation: u64, caller: Option<Terminal>) -> Self {
        Self {
            stream,
            generation,
            caller,
            input: Vec::new(),
            taken: 0,
            ended: false,
            greeted: false,
            output: Vec::new(),
            closing: false,
            until: Instant::now() + TIMEOUT,
            opening: None,
        }
    }

    /// When the poll must wake for this peer.
    fn deadline(&self) -> Instant {
        self.opening.as_ref().map_or(self.until, |(_, at)| *at)
    }

    /// Reads all the peer has sent, up to its limit.
    fn read(&mut self) {
        let mut buffer = [0; MAX_LINE];
        while !self.ended {
            let room = usize::try_from(MAX_CONNECTION - self.taken).unwrap_or(0);
            if room == 0 {
                self.ended = true;
                return;
            }
            let room = room.min(buffer.len());
            match (&mut &self.stream).read(&mut buffer[..room]) {
                Ok(0) => self.ended = true,
                Ok(read) => {
                    self.input.extend_from_slice(&buffer[..read]);
                    self.taken += read as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => self.ended = true,
            }
        }
    }

    /// Answers each whole line, in order, until one waits for a window. `open` asks for a window
    /// and returns how long the caller waits for it.
    fn step(&mut self, now: Instant, open: &mut impl FnMut() -> Duration) {
        while self.opening.is_none() && !self.closing {
            let line = self
                .input
                .iter()
                .take(MAX_LINE)
                .position(|&byte| byte == b'\n');
            let Some(end) = line else {
                // A line past the limit, or a last one with no end.
                self.closing = self.input.len() >= MAX_LINE || self.ended || now >= self.until;
                return;
            };
            let line: Vec<u8> = self.input.drain(..=end).take(end).collect();
            match answer(&line, &mut self.greeted) {
                Step::Reply(reply, go_on) => {
                    self.queue(&reply);
                    self.closing = !go_on;
                }
                Step::Open(id) => self.opening = Some((id, now + open())),
            }
        }
    }

    /// Hands the waiting request its answer.
    fn opened(&mut self, result: Result<(), String>) {
        let Some((id, _)) = self.opening.take() else {
            return;
        };
        let (reply, go_on) = match result {
            Ok(()) => (done(&id, &json!({})), true),
            Err(reason) => refuse(&id, NO_WINDOW, &format!("no window can open: {reason}")),
        };
        self.queue(&reply);
        self.closing = !go_on;
    }

    /// Tells a request that waited too long that its window is not ready.
    fn expire(&mut self, now: Instant) {
        if self.opening.as_ref().is_some_and(|(_, at)| now >= *at) {
            self.opened(Err("the window is not ready and may still open".to_owned()));
        }
    }

    fn queue(&mut self, reply: &Value) {
        if !reply.is_null() {
            self.output.extend(reply.to_string().bytes());
            self.output.push(b'\n');
        }
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
    }

    /// Whether the connection is over: it said its last, or its time is up while it waits for
    /// nothing.
    fn done(&self, now: Instant) -> bool {
        self.opening.is_none() && ((self.closing && self.output.is_empty()) || now >= self.until)
    }
}

/// What one request leads to.
#[derive(Debug, PartialEq)]
enum Step {
    /// This reply, null for none, and whether the connection goes on.
    Reply(Value, bool),
    /// A window is wanted for the request with this id.
    Open(Value),
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
fn answer(line: &[u8], greeted: &mut bool) -> Step {
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
        Some("hello") if *greeted => refuse(Some(id), NO_METHOD, "hello comes once"),
        Some("hello") => {
            let version = request.pointer("/params/version").and_then(Value::as_u64);
            if version != Some(VERSION) {
                return refuse(Some(id), BAD_VERSION, "unknown version");
            }
            *greeted = true;
            let result = json!({"version": VERSION, "capabilities": ["window.open"]});
            Step::Reply(done(id, &result), true)
        }
        _ if !*greeted => refuse(Some(id), NO_HELLO, "hello comes first"),
        Some("window.open") => Step::Open(id.clone()),
        _ => refuse(Some(id), NO_METHOD, "unknown method"),
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

    static NEXT: AtomicUsize = AtomicUsize::new(0);

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
    fn replied(line: &[u8], greeted: &mut bool) -> (Value, bool) {
        match answer(line, greeted) {
            Step::Reply(reply, go_on) => (reply, go_on),
            Step::Open(id) => panic!("{id} asked for a window"),
        }
    }

    /// Answers every window request at once with `result`.
    fn serving(result: Result<(), String>) -> impl FnMut(Opening) {
        move |opening| opening.answer(result.clone())
    }

    #[test]
    fn hello_comes_first_and_once() {
        let mut greeted = false;
        let (reply, go_on) = replied(OPEN, &mut greeted);
        assert_eq!((code(&reply), go_on), (Some(NO_HELLO), false));
        let (reply, go_on) = replied(HELLO, &mut greeted);
        assert_eq!(reply["result"]["version"], json!(VERSION));
        assert_eq!(reply["result"]["capabilities"], json!(["window.open"]));
        assert_eq!(reply["id"], json!(1));
        assert!(go_on);
        let (reply, go_on) = replied(HELLO, &mut greeted);
        assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false));
    }

    #[test]
    fn a_wrong_version_is_refused() {
        let line = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":2}}"#;
        let mut greeted = false;
        let (reply, go_on) = replied(line, &mut greeted);
        assert_eq!(
            (code(&reply), go_on, greeted),
            (Some(BAD_VERSION), false, false)
        );
    }

    #[test]
    fn only_hello_and_window_open_exist() {
        for method in ["pane.read", "pane.write", "window.close", "", "HELLO"] {
            let line = json!({"jsonrpc": "2.0", "id": 1, "method": method}).to_string();
            let mut greeted = true;
            let (reply, go_on) = replied(line.as_bytes(), &mut greeted);
            assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false), "{method}");
        }
    }

    #[test]
    fn window_open_asks_for_a_window_and_ignores_arguments() {
        let line =
            br#"{"jsonrpc":"2.0","id":2,"method":"window.open","params":{"path":"/bin/sh"}}"#;
        assert_eq!(answer(line, &mut true), Step::Open(json!(2)));
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
            let (reply, go_on) = replied(line.as_bytes(), &mut true);
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
        assert_eq!(answer(line, &mut true), Step::Reply(Value::Null, false));
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
                let (reply, go_on) = replied(line, &mut false);
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
    fn a_session_has_one_core_and_a_dead_one_leaves_no_owner() {
        let directory = fresh();
        let first = Control::bind(&directory).unwrap();
        assert!(matches!(Control::bind(&directory), Err(Error::Running)));
        drop(first);
        assert!(socket_path(&directory).exists());
        assert!(Control::bind(&directory).is_ok());
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
        thread::spawn(move || control.serve(serving(Ok(()))));

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
            let open = serving(window.then_some(()).ok_or_else(|| "why".to_owned()));
            thread::spawn(move || control.serve(open));
            let stream = UnixStream::connect(socket_path(&directory)).unwrap();
            match open_window(&stream) {
                Ok(()) => assert!(opened),
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
    fn a_connection_gets_answers_and_cannot_stay_for_ever() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        thread::spawn(move || control.serve(serving(Ok(()))));

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
        assert_eq!(hello["result"]["version"], json!(VERSION));
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

    #[test]
    fn a_peer_waiting_for_a_window_holds_up_no_one() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        // Never answered, as by a core whose window hangs.
        let mut held = Vec::new();
        thread::spawn(move || control.serve(move |opening| held.push(opening)));

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
        thread::spawn(move || control.serve(move |opening| asked.send(opening).unwrap()));

        let path = socket_path(&directory);
        let mut held: Vec<UnixStream> = (0..MAX_PER_CALLER)
            .map(|_| UnixStream::connect(&path).unwrap())
            .collect();
        held[0].write_all(HELLO).unwrap();
        held[0].write_all(b"\n").unwrap();
        held[0].write_all(OPEN).unwrap();
        held[0].write_all(b"\n").unwrap();
        let opening = callers.recv_timeout(TIMEOUT).unwrap();
        let probe = UnixListener::bind(fresh()).unwrap();
        let _us = UnixStream::connect(probe.local_addr().unwrap().as_pathname().unwrap());
        let expected = pinfold_sys::peer_terminal(probe.accept().unwrap().0.as_fd()).unwrap();
        assert_eq!(opening.caller(), expected);
        let mut over = UnixStream::connect(&path).unwrap();
        over.set_read_timeout(Some(TIMEOUT / 2)).unwrap();
        assert_eq!(over.read(&mut [0; 8]).unwrap(), 0);
        drop(held);
    }
}
