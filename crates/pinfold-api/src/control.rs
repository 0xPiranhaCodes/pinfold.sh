//! The control socket: how a launcher finds the core and asks it for a window.
//!
//! Until M3 it takes only `hello` and `window.open`, as newline-delimited JSON-RPC 2.0
//! (ADR-0009, ADR-0028). `window.open` takes no arguments, so a caller cannot name a program,
//! and nothing here reaches a pane. Same-user controls are guardrails (threat model).

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

/// The version `hello` must name.
pub const VERSION: u64 = 1;
/// Stack for the thread that calls [`Control::serve`], which holds `serde_json`'s deepest parse.
pub const STACK: usize = 256 * 1024;

/// The session's name in its file names. There is one session until M2 adds attach.
const SESSION: &str = "default";
/// Longest request, far above what `hello` and `window.open` need.
const MAX_LINE: usize = 4096;
/// Most bytes one connection may send: both requests, so it cannot stay and keep talking.
const MAX_CONNECTION: u64 = 2 * MAX_LINE as u64;
/// Longest wait on one peer, as this thread serves them one at a time.
const TIMEOUT: Duration = Duration::from_secs(2);

const PARSE_ERROR: i64 = -32700;
const NO_METHOD: i64 = -32601;
const BAD_VERSION: i64 = -32001;
const NO_HELLO: i64 = -32002;
const NO_WINDOW: i64 = -32003;

/// Why the core cannot take the control socket.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The directory could be reached or changed by someone else.
    #[error("{path} is not a safe directory: {reason}")]
    Unsafe {
        /// The directory.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Another core holds this session.
    #[error("a core is already running")]
    Running,
    /// The system refused.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Returns the directory that holds the session's socket: `$XDG_RUNTIME_DIR/pinfold`, or under
/// the per-user temporary directory where that is unset, as on macOS.
#[must_use]
pub fn runtime_dir() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    match runtime {
        Some(runtime) => runtime.join("pinfold"),
        None if cfg!(target_os = "macos") => std::env::temp_dir().join("pinfold"),
        // /tmp is shared, so the name is the user's and `checked` refuses anyone else's.
        None => std::env::temp_dir().join(format!("pinfold-{}", pinfold_sys::effective_uid())),
    }
}

/// Returns the session's socket in `directory`.
#[must_use]
pub fn socket_path(directory: &Path) -> PathBuf {
    directory.join(SESSION).with_extension("sock")
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
    /// [`Error::Unsafe`] for a directory others can reach, [`Error::Running`] if a core holds
    /// the session, and [`Error::Io`] if the system refuses.
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
        match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            _lock: lock,
        })
    }

    /// Answers peers one at a time, for as long as the process lives. `open` asks for a window
    /// and says if one is on its way. A peer that fails costs only its own connection.
    pub fn serve(&self, mut open: impl FnMut() -> bool) {
        for stream in self.listener.incoming().flatten() {
            let _ = serve_peer(&stream, &mut open);
        }
    }
}

/// Takes one connection from a peer of this user, until it fails or ends.
fn serve_peer(stream: &UnixStream, open: &mut impl FnMut() -> bool) -> io::Result<()> {
    if pinfold_sys::peer_uid(stream.as_fd())? != pinfold_sys::effective_uid() {
        return Ok(());
    }
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut lines = BufReader::new(stream.take(MAX_CONNECTION));
    let mut greeted = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        (&mut lines)
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut line)?;
        if line.pop() != Some(b'\n') {
            return Ok(());
        }
        let (reply, go_on) = answer(&line, &mut greeted, open);
        let mut bytes = reply.to_string().into_bytes();
        bytes.push(b'\n');
        (&mut &*stream).write_all(&bytes)?;
        if !go_on {
            return Ok(());
        }
    }
}

/// Returns the reply to one request, and whether the connection goes on.
fn answer(line: &[u8], greeted: &mut bool, open: &mut impl FnMut() -> bool) -> (Value, bool) {
    let refuse = |id: Option<&Value>, code, message: &str| {
        let id = id.cloned().unwrap_or(Value::Null);
        let error = json!({"code": code, "message": message});
        (json!({"jsonrpc": "2.0", "id": id, "error": error}), false)
    };
    let Ok(request) = serde_json::from_slice::<Value>(line) else {
        return refuse(None, PARSE_ERROR, "not JSON");
    };
    let id = request.get("id");
    let done = |result: Value| (json!({"jsonrpc": "2.0", "id": id, "result": result}), true);
    match request.get("method").and_then(Value::as_str) {
        Some("hello") if *greeted => refuse(id, NO_METHOD, "hello comes once"),
        Some("hello") => {
            let version = request.pointer("/params/version").and_then(Value::as_u64);
            if version != Some(VERSION) {
                return refuse(id, BAD_VERSION, "unknown version");
            }
            *greeted = true;
            done(json!({"version": VERSION}))
        }
        _ if !*greeted => refuse(id, NO_HELLO, "hello comes first"),
        Some("window.open") if open() => done(json!({})),
        Some("window.open") => refuse(id, NO_WINDOW, "no window can open"),
        _ => refuse(id, NO_METHOD, "unknown method"),
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

    #[test]
    fn hello_comes_first_and_once() {
        let mut open = || true;
        let mut greeted = false;
        let (reply, go_on) = answer(OPEN, &mut greeted, &mut open);
        assert_eq!((code(&reply), go_on), (Some(NO_HELLO), false));
        let (reply, go_on) = answer(HELLO, &mut greeted, &mut open);
        assert_eq!(reply["result"]["version"], json!(VERSION));
        assert_eq!(reply["id"], json!(1));
        assert!(go_on);
        let (reply, go_on) = answer(HELLO, &mut greeted, &mut open);
        assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false));
    }

    #[test]
    fn a_wrong_version_is_refused() {
        let line = br#"{"id":1,"method":"hello","params":{"version":2}}"#;
        let mut greeted = false;
        let (reply, go_on) = answer(line, &mut greeted, &mut || true);
        assert_eq!(
            (code(&reply), go_on, greeted),
            (Some(BAD_VERSION), false, false)
        );
    }

    #[test]
    fn only_hello_and_window_open_exist() {
        for method in ["pane.read", "pane.write", "window.close", "", "HELLO"] {
            let line = json!({"id": 1, "method": method}).to_string();
            let mut greeted = true;
            let (reply, go_on) = answer(line.as_bytes(), &mut greeted, &mut || true);
            assert_eq!((code(&reply), go_on), (Some(NO_METHOD), false), "{method}");
        }
    }

    #[test]
    fn window_open_reports_what_happened_and_ignores_arguments() {
        let line = br#"{"id":2,"method":"window.open","params":{"path":"/bin/sh"}}"#;
        let mut asked = 0;
        let mut greeted = true;
        let (reply, go_on) = answer(line, &mut greeted, &mut || {
            asked += 1;
            true
        });
        assert_eq!((reply["result"].clone(), go_on), (json!({}), true));
        let (reply, _) = answer(line, &mut greeted, &mut || false);
        assert_eq!(code(&reply), Some(NO_WINDOW));
        assert_eq!(asked, 1);
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
                let (reply, go_on) = answer(line, &mut false, &mut || true);
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
    fn a_connection_gets_answers_and_cannot_stay_for_ever() {
        let directory = fresh();
        let control = Control::bind(&directory).unwrap();
        thread::spawn(move || control.serve(|| true));

        let mut stream = UnixStream::connect(socket_path(&directory)).unwrap();
        stream.write_all(HELLO).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.write_all(OPEN).unwrap();
        stream.write_all(b"\n").unwrap();
        let mut reader = BufReader::new(&stream);
        let mut reply = String::new();
        reader.read_line(&mut reply).unwrap();
        assert!(reply.contains("\"version\":1"), "{reply}");
        reply.clear();
        reader.read_line(&mut reply).unwrap();
        assert!(reply.contains("\"result\":{}"), "{reply}");

        // A line with no end is cut at the limit, and the next peer is served.
        let mut flood = UnixStream::connect(socket_path(&directory)).unwrap();
        let _ = flood.write_all(&vec![b'x'; 3 * MAX_LINE]);
        let mut peer = UnixStream::connect(socket_path(&directory)).unwrap();
        peer.write_all(b"null\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&peer).read_line(&mut reply).unwrap();
        assert!(reply.contains("hello comes first"), "{reply}");
    }
}
