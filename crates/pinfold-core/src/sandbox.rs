//! The command that starts a sandboxed agent profile's pane (ADR-0020 point 5).
//!
//! The sandbox wraps the pane's whole program: `sandbox-exec` on macOS and bubblewrap on Linux.
//! Neither starts a session, so the agent keeps the pane's controlling terminal and session, by
//! which the API knows it. TIOCSTI, which would type into that terminal, is denied: by the rules
//! on macOS and by a seccomp filter on Linux. Nor can it open a new terminal, whose session the API
//! would take for a person's outside every pane: macOS denies every terminal by name, and Linux
//! gets an empty `/dev/pts` and no user namespace of its own to mount another in.
//!
//! The agent writes only to its [`WorkingDirectory`] and temporary directories: a fresh `/tmp` on
//! Linux, and `/private/tmp` and the user's temporary directory on macOS. Git runs what its
//! repository's hooks and config name outside the sandbox, so on Linux all of `.git` is read-only,
//! and the agent commits into a [`shadow`] git directory of its own instead (ADR-0039). On macOS
//! those files, its submodules' and anything that points git elsewhere are read-only. Of other
//! programs' Unix sockets, such as ssh-agent's, it reaches only PinFold's API socket, and on
//! Linux with Landlock ABI 6 no abstract socket bound outside the sandbox (ADR-0038).
//!
//! It cannot read the common credential stores, gets no variable whose name looks like a secret
//! but those its profile keeps, and cannot reach the clipboard: macOS denies the pasteboard
//! service, and on Linux the X11 and Wayland sockets are out of reach. The network stays open
//! (ADR-0041).
//!
//! A missing tool is an error: a pane that asked for a sandbox never runs without one.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where bubblewrap is looked for. Not `PATH`, where an agent could put its own `bwrap` for the
/// next launch.
const BWRAP: [&str; 3] = ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"];

/// The oldest bubblewrap with `--disable-userns`.
const BWRAP_OLDEST: (u32, u32) = (0, 8);

/// Variables that name other programs' sockets, which the agent does not get: agents', the
/// session bus's, and the X11 and Wayland displays', which hold the clipboard, with X11's cookie
/// file (ADR-0041).
const SOCKET_VARS: [&str; 7] = [
    "SSH_AUTH_SOCK",
    "GPG_AGENT_INFO",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
    "XAUTHORITY",
];

/// Words in the name of a variable that holds a secret, such as `GITHUB_TOKEN` or
/// `AZURE_CLIENT_SECRET`, which a sandboxed agent does not get (ADR-0041).
const SECRET_WORDS: [&str; 6] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "APIKEY",
];

/// The macOS rules: everything is allowed but the hidden directories, every terminal by name,
/// TIOCSTI (`0x80017472`), and starting or driving other programs outside the sandbox: an app
/// that LaunchServices starts (`open`) is launchd's child, not the agent's, so no rule here would
/// hold it, and Apple Events ask a running app to act. A pane's own terminal is already open,
/// and `/dev/tty` still reaches it.
const PROFILE: &str = "(version 1)(allow default)\
(deny file-read* file-write* (regex #\"^/dev/ttys[0-9]+$\"))\
(deny file-ioctl (ioctl-command 2147578994))\
(deny lsopen) (deny appleevent-send)";

/// The macOS clipboard rule (ADR-0041): no lookup of the pasteboard server's services,
/// `com.apple.pasteboard.1` and `com.apple.coreservices.uauseractivitypasteboardclient.xpc`,
/// which `/System/Library/LaunchAgents/com.apple.pboard.plist` lists for `/usr/libexec/pboard`,
/// nor of Universal Clipboard's `com.apple.coreservices.uasharedpasteboard*`, as `launchctl
/// print gui/$UID` lists them on macOS 27.
const CLIPBOARD: &str = "(deny mach-lookup (global-name \"com.apple.pasteboard.1\")\
(global-name \"com.apple.coreservices.uauseractivitypasteboardclient.xpc\")\
(global-name-prefix \"com.apple.coreservices.uasharedpasteboard\"))";

/// The macOS write rule: nowhere but the working directory `W`, the user's temporary directory
/// `T`, `/private/tmp` and `/dev`.
const WRITES: &str = "(deny file-write* (require-all (require-not (subpath (param \"W\")))\
(require-not (subpath (param \"T\"))) (require-not (subpath \"/private/tmp\"))\
(require-not (subpath \"/dev\"))))";

/// The macOS git rule for `.git` itself, `G`: it cannot be made, rewritten or moved away and
/// back. What git runs code from inside it gets a rule each.
const GIT: &str = "(deny file-write* (literal (param \"G\")))";

/// The most directories and submodules [`git_controls`] looks at before it refuses the launch.
const GIT_DIRS: usize = 256;

/// The macOS socket rule: no Unix socket but the API's `S`, as given and resolved `SR`, and the
/// system's name lookup.
const SOCKETS: &str = "(deny network-outbound (require-all (remote unix-socket)\
(require-not (remote unix-socket (path-literal (param \"S\"))))\
(require-not (remote unix-socket (path-literal (param \"SR\"))))\
(require-not (remote unix-socket (path-literal \"/private/var/run/mDNSResponder\")))))";

/// What git runs code from in a working directory, from [`git_controls`].
#[derive(Debug, Default, PartialEq)]
struct Git {
    /// Directories that must stay where they are, as the rules name what is inside them.
    kept: Vec<PathBuf>,
    /// Hooks, config and pointers to other directories, which git reads code or paths from.
    controls: Vec<PathBuf>,
    /// The `.git` of each submodule that `.gitmodules` lists, a file that points git at the
    /// submodule's repository or that repository itself.
    modules: Vec<PathBuf>,
    /// The agent's own git directory, from [`shadow`], bound writable on Linux.
    shadow: Option<PathBuf>,
}

/// A directory a sandboxed agent may write to, from [`working_directory`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDirectory(PathBuf);

impl WorkingDirectory {
    /// The directory, resolved through symlinks.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Checks `path` as a sandboxed agent's writable directory. It comes from a pane's OSC 7 report,
/// which any program can print, so it must resolve to a directory of this user's that no one else
/// can write to, under directories that only it or root can change. It must be strictly inside
/// `$HOME`, and not a dot-directory of `$HOME` such as `~/.config` or inside one, `~/bin`,
/// `~/Applications` or `~/Library` or inside them, inside a `.git` directory, an entry of
/// `user_path` or above one, inside the directory that holds an entry of `user_path` in `$HOME`,
/// such as `~/go` for `~/go/bin`, or hold or sit inside a `hidden` one. `user_path` is the user's own `PATH`, from
/// [`login_path`].
///
/// # Errors
///
/// Returns an error if `path` does not resolve, `HOME` is not set, or any rule fails. There is no
/// fallback: the agent does not start.
///
/// # Examples
///
/// ```
/// use std::os::unix::fs::DirBuilderExt;
/// use std::path::Path;
/// use pinfold_core::sandbox::working_directory;
///
/// let home = std::env::var_os("HOME").unwrap();
/// let project = Path::new(&home).join(format!("pinfold-project-{}", std::process::id()));
/// std::fs::DirBuilder::new().mode(0o700).create(&project)?;
/// let path = "/usr/bin:/bin".as_ref();
/// assert!(working_directory(&project, &[], path).is_ok());
/// assert!(working_directory(&project, &[project.join(".ssh")], path).is_err());
/// assert!(working_directory(Path::new("/"), &[], path).is_err());
/// std::fs::remove_dir(&project)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn working_directory(
    path: &Path,
    hidden: &[PathBuf],
    user_path: &OsStr,
) -> io::Result<WorkingDirectory> {
    let Some(home) = std::env::var_os("HOME") else {
        return Err(refusal(path, "cannot be checked without HOME"));
    };
    inside_home(path, hidden, user_path, &resolve(Path::new(&home)))
}

/// The error for a refused working directory, which says what to do instead.
fn refusal(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{path:?} {why}, so the sandboxed agent does not start. Start it from a project \
             directory of yours inside your home folder"
        ),
    )
}

/// [`working_directory`] with `home` resolved.
fn inside_home(
    path: &Path,
    hidden: &[PathBuf],
    user_path: &OsStr,
    home: &Path,
) -> io::Result<WorkingDirectory> {
    let refuse = |why: &str| Err(refusal(path, why));
    let dir = path.canonicalize()?;
    let meta = fs::metadata(&dir)?;
    if !meta.is_dir() {
        return refuse("is not a directory");
    }
    if meta.uid() != pinfold_sys::effective_uid() {
        return refuse("belongs to another user");
    }
    if meta.mode() & 0o022 != 0 {
        return refuse("is writable by other users");
    }
    // Outside home are package managers' prefixes the user owns, such as /opt/homebrew.
    let Some(first) = dir
        .strip_prefix(home)
        .ok()
        .and_then(|rest| rest.iter().next())
    else {
        return refuse("is not inside your home folder");
    };
    // What runs at login, from PATH or from git lives here, such as ~/.local/bin and ~/.config.
    let library = cfg!(target_os = "macos") && first == "Library";
    if library || first == "bin" || first == "Applications" {
        return refuse("is or is inside ~/bin, ~/Applications or ~/Library");
    }
    if first.as_encoded_bytes().starts_with(b".") {
        return refuse("is or is inside a dot-directory of home");
    }
    if dir.iter().any(|name| name == ".git") {
        return refuse("is inside a .git directory");
    }
    // A directory above it that another user can change could be swapped before the launch.
    for above in dir.ancestors().skip(1) {
        let meta = fs::metadata(above)?;
        let owner = meta.uid() == 0 || meta.uid() == pinfold_sys::effective_uid();
        if !owner || meta.mode() & 0o022 != 0 {
            return refuse("is under a directory another user can change");
        }
    }
    if holds_path(&dir, user_path) {
        return refuse("is or holds a directory on your PATH");
    }
    // A prefix such as ~/go or ~/miniconda3, which holds a PATH entry, holds the libraries and
    // config its programs load.
    let in_prefix = std::env::split_paths(user_path).any(|entry| {
        let entry = resolve(&entry);
        entry
            .parent()
            .filter(|prefix| prefix.starts_with(home) && *prefix != home)
            .is_some_and(|prefix| dir.starts_with(prefix))
    });
    if in_prefix {
        return refuse("is inside a package manager's directory on your PATH");
    }
    for hidden in hidden {
        let real = resolve(hidden);
        let near = |other: &PathBuf| other.starts_with(&dir) || dir.starts_with(other);
        if near(hidden) || near(&real) {
            return refuse("holds or is inside a directory the sandbox hides");
        }
    }
    Ok(WorkingDirectory(dir))
}

/// Starts the user's shell, as a pane's starts, to report the `PATH` [`login_path`] returns.
/// Call it once at start, before any agent runs. `private` is where the shell writes, a
/// directory of the core's that the sandbox hides, such as its state directory.
pub fn read_login_path(private: &Path) {
    let mut login = LOGIN.lock().unwrap_or_else(PoisonError::into_inner);
    if login.is_none() {
        *login = ask_shell(crate::shell::command(), private)
            .ok()
            .map(Login::Asked);
    }
}

/// Returns the `PATH` the user's shell sets, read once, followed by the core's own, for
/// [`working_directory`]. The core's alone is not enough: launchd starts an app with
/// `/usr/bin:/bin:/usr/sbin:/sbin`, without Homebrew's or `~/go/bin`. An agent cannot change
/// the shell's, as it cannot write the shell's startup files. It never waits for the shell, as
/// the reactor calls it.
///
/// # Errors
///
/// Returns an error of kind `WouldBlock` while the shell has not answered, and starts it if it
/// is not running. Returns the reason if the shell cannot start, took over 10 s or reported no
/// `PATH`, and asks it again for the next call.
pub fn login_path(private: &Path) -> io::Result<OsString> {
    let mut login = LOGIN.lock().unwrap_or_else(PoisonError::into_inner);
    let ask = || ask_shell(crate::shell::command(), private);
    let mut path = next_path(&mut login, &ask, LOGIN_WAIT)?;
    // An empty one would be a relative entry, which refuses every directory.
    if let Some(own) = std::env::var_os("PATH").filter(|own| !own.is_empty()) {
        path.push(":");
        path.push(own);
    }
    Ok(path)
}

/// The user's shell asked by [`ask_shell`], when, and the file it writes to.
type Asked = (Child, fs::File, Instant);

enum Login {
    Asked(Asked),
    Read(OsString),
}

static LOGIN: Mutex<Option<Login>> = Mutex::new(None);

/// How long the user's shell gets to report its `PATH`, after which it is asked again.
pub const LOGIN_WAIT: Duration = Duration::from_secs(10);

/// [`login_path`]'s refusal while the user's shell has not reported its `PATH`, which
/// `pinfold agent` waits out.
pub const READING_PATH: &str =
    "PinFold is still reading your shell's PATH; try again in a few seconds";

/// The line before the `PATH`, as the shell's startup files may print too.
const PATH_MARK: &str = "pinfold-path:";

/// [`login_path`]'s steps, with `ask` starting a shell that gets `wait` to answer.
fn next_path(
    login: &mut Option<Login>,
    ask: &dyn Fn() -> io::Result<Asked>,
    wait: Duration,
) -> io::Result<OsString> {
    let reading = || io::Error::new(io::ErrorKind::WouldBlock, READING_PATH);
    match login.take() {
        Some(Login::Read(path)) => {
            *login = Some(Login::Read(path.clone()));
            Ok(path)
        }
        Some(Login::Asked(asked)) => match reported(asked, wait) {
            Ok(Ok(path)) => {
                *login = Some(Login::Read(path.clone()));
                Ok(path)
            }
            Ok(Err(asked)) => {
                *login = Some(Login::Asked(asked));
                Err(reading())
            }
            Err(error) => {
                *login = ask().ok().map(Login::Asked);
                Err(error)
            }
        },
        None => {
            *login = Some(Login::Asked(ask()?));
            Err(reading())
        }
    }
}

/// Starts `shell`, interactive as in a pane, to print its `PATH` into an unnamed file in
/// `private`, which a program it leaves running cannot hold open as it could a pipe, and which
/// no agent can open in the moment it has a name.
fn ask_shell(mut shell: Command, private: &Path) -> io::Result<Asked> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use std::os::unix::process::CommandExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(private)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = private.join(format!(".path-{}-{nanos}", std::process::id()));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&name)?;
    fs::remove_file(&name)?;
    shell
        .args(["-i", "-c", &format!("echo {PATH_MARK}; printenv PATH")])
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(Stdio::null())
        // Not in the core's group, so it never takes a terminal the core was started from.
        .process_group(0);
    pinfold_sys::mark_strays(&mut shell);
    strip_inherited(&mut shell);
    Ok((shell.spawn()?, file, Instant::now()))
}

/// Looks once at the shell from [`ask_shell`]: the `PATH` it printed, or the shell back if it is
/// still running within `wait`. One that outlives `wait` is killed and reaped.
fn reported(
    (mut child, mut file, asked): Asked,
    wait: Duration,
) -> io::Result<Result<OsString, Asked>> {
    use std::io::{Read, Seek};
    if child.try_wait()?.is_none() {
        if asked.elapsed() <= wait {
            return Ok(Err((child, file, asked)));
        }
        let _ = child.kill();
        child.wait()?;
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "your shell took over 10 s to report its PATH",
        ));
    }
    let mut text = Vec::new();
    file.rewind()?;
    file.take(1 << 20).read_to_end(&mut text)?;
    let path = path_after_mark(&text);
    path.map(Ok)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "your shell reported no PATH"))
}

/// The line after the last [`PATH_MARK`] line in what the shell printed.
fn path_after_mark(text: &[u8]) -> Option<OsString> {
    use std::os::unix::ffi::OsStrExt;
    let mut lines = text.split(|&byte| byte == b'\n');
    let mut path = None;
    while let Some(line) = lines.next() {
        if line == PATH_MARK.as_bytes() {
            path = lines.next().filter(|path| !path.is_empty());
        }
    }
    path.map(|path| OsStr::from_bytes(path).to_owned())
}

/// Whether `dir` is or holds an entry of `path`, as given or resolved, where a program the agent
/// writes would run for the user. A relative entry counts from `dir`, as a shell there searches
/// it, and a dangling symlink as held, as the agent could make where it leads.
fn holds_path(dir: &Path, path: &OsStr) -> bool {
    std::env::split_paths(path)
        .map(|entry| dir.join(entry))
        .any(|entry| {
            let dangling = entry
                .ancestors()
                .any(|above| above.is_symlink() && !above.exists());
            dangling || entry.starts_with(dir) || resolve(&entry).starts_with(dir)
        })
}

/// `path` resolved through symlinks as far as it exists, as a hidden directory may not yet.
fn resolve(path: &Path) -> PathBuf {
    for base in path.ancestors() {
        if let Ok(real) = base.canonicalize() {
            // Not joined when nothing is left, which would add a trailing `/` that rules then miss.
            return match path.strip_prefix(base) {
                Ok(rest) if !rest.as_os_str().is_empty() => real.join(rest),
                _ => real,
            };
        }
    }
    path.to_path_buf()
}

/// Whether a variable's name looks like a secret's: it holds `TOKEN`, `SECRET`, `PASSWORD`,
/// `PASSWD`, `CREDENTIAL` or `APIKEY`, ends in `_KEY` or `_PAT`, or starts with `AWS_`, in any
/// case. A sandboxed agent does not get such a variable unless its profile keeps it.
///
/// ```
/// use pinfold_core::sandbox::secret_name;
/// for name in ["GITHUB_TOKEN", "OPENAI_API_KEY", "AWS_PROFILE", "db_password", "GH_PAT"] {
///     assert!(secret_name(name.as_ref()), "{name}");
/// }
/// for name in ["PATH", "HOME", "LANG", "KEYBOARD", "TERM"] {
///     assert!(!secret_name(name.as_ref()), "{name}");
/// }
/// ```
pub fn secret_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    name.starts_with("AWS_")
        || name.ends_with("_KEY")
        || name.ends_with("_PAT")
        || SECRET_WORDS.iter().any(|word| name.contains(word))
}

/// Most names the macOS notice lists (ADR-0047).
const LISTED: usize = 5;
/// Longest name it shows whole, in characters.
const NAME_SHOWN: usize = 40;

/// What the window's one-time notice says of `names`, the variables the core started with and
/// gives its panes, on macOS with `strip_secrets` off: how many [`secret_name`] matches, and the
/// first few, sorted, escaped and capped, ending in `…` if more. Nothing on other systems, as
/// only macOS lets a sandboxed agent read other processes' environment (ADR-0047), or when none
/// match. It takes names alone, so no value can reach the notice or the log.
///
/// ```
/// use pinfold_core::sandbox::exposed;
/// let names = ["PATH", "GITHUB_TOKEN", "AWS_PROFILE"].map(Into::into);
/// let told = exposed(false, names.clone());
/// if cfg!(target_os = "macos") {
///     assert_eq!(told, Some((2, "AWS_PROFILE, GITHUB_TOKEN".to_owned())));
/// } else {
///     assert_eq!(told, None);
/// }
/// assert_eq!(exposed(true, names), None);
/// ```
pub fn exposed(
    strip_secrets: bool,
    names: impl IntoIterator<Item = OsString>,
) -> Option<(u32, String)> {
    if !cfg!(target_os = "macos") || strip_secrets {
        return None;
    }
    let mut found: Vec<String> = names
        .into_iter()
        .filter(|name| secret_name(name))
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    found.sort_unstable();
    found.dedup();
    let mut listed = String::new();
    for name in found.iter().take(LISTED) {
        if !listed.is_empty() {
            listed.push_str(", ");
        }
        let shown: String = name.chars().take(NAME_SHOWN).collect();
        crate::tool::escape(&shown, &mut listed);
        if shown.len() < name.len() {
            listed.push('…');
        }
    }
    if found.len() > LISTED {
        listed.push_str(", …");
    }
    let count = u32::try_from(found.len()).unwrap_or(u32::MAX);
    (count > 0).then_some((count, listed))
}

/// Whether [`strip_inherited`] strips: the config's `strip_secrets`, set once at start.
static STRIP: AtomicBool = AtomicBool::new(false);

/// Makes every program the core starts, after this, go without the variables [`secret_name`]
/// matches if `on` (`strip_secrets`, ADR-0047). Not even a profile's `keep_env` brings one
/// back. Call it once at start, before any pane runs.
pub fn strip_secrets(on: bool) {
    STRIP.store(on, Ordering::Relaxed);
}

/// Removes from `command` each of `names` that [`secret_name`] matches. Names only: no value
/// is read.
///
/// ```
/// use std::process::Command;
/// use pinfold_core::sandbox::remove_secrets;
///
/// let mut command = Command::new("/usr/bin/env");
/// remove_secrets(&mut command, ["GITHUB_TOKEN".into(), "PATH".into()]);
/// let changed: Vec<_> = command.get_envs().collect();
/// assert_eq!(changed, [("GITHUB_TOKEN".as_ref(), None)]);
/// ```
pub fn remove_secrets(command: &mut Command, names: impl IntoIterator<Item = OsString>) {
    for name in names {
        if secret_name(&name) {
            command.env_remove(name);
        }
    }
}

/// Removes the secret-looking variables `command` would inherit from the core, if
/// [`strip_secrets`] turned stripping on. Every program the core starts passes through here.
pub(crate) fn strip_inherited(command: &mut Command) {
    if STRIP.load(Ordering::Relaxed) {
        remove_secrets(command, std::env::vars_os().map(|(name, _)| name));
    }
}

/// Returns the command that runs `program` with `hidden` unreadable, only `work` and temporary
/// directories writable, no variable whose [`secret_name`] is not in `keep` and, of Unix
/// sockets, only the API's at `api` reachable, for
/// [`Reactor::spawn`](crate::io::Reactor::spawn). On Linux, given a `shadow` directory and a
/// `work` with a `.git` directory, the agent's git uses [`shadow`] there. On Linux below Landlock ABI 6 abstract sockets
/// stay reachable. Spawn it once: on Linux its seccomp filter is read to the end by the first
/// launch.
///
/// # Errors
///
/// Returns an error if `program` is empty or starts with `-`, the system has no sandbox, or on
/// Linux no `bwrap` in `/usr/bin`, `/usr/local/bin` or `/bin` is root's alone or it is older than
/// 0.8, the filter has
/// no rules for this architecture, the kernel has Landlock but its probe or rules fail, or the
/// shadow cannot be made. Rules that fail in the child fail the spawn.
///
/// # Examples
///
/// ```
/// use std::os::unix::fs::DirBuilderExt;
/// use std::path::Path;
/// use pinfold_core::sandbox;
///
/// let home = std::env::var_os("HOME").unwrap();
/// let project = Path::new(&home).join(format!("pinfold-agent-{}", std::process::id()));
/// std::fs::DirBuilder::new().mode(0o700).create(&project)?;
/// let work = sandbox::working_directory(&project, &[], "/usr/bin".as_ref())?;
/// let program = ["/bin/echo".to_owned(), "hi".to_owned()];
/// let api = std::env::temp_dir().join("pinfold/pinfold.sock");
/// match sandbox::command(&program, &[], &[], &work, &api, None) {
///     Ok(command) => assert_ne!(command.get_program(), "/bin/echo"),
///     // No bubblewrap here: the pane does not start.
///     Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
/// }
/// std::fs::remove_dir(&project)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn command(
    program: &[String],
    hidden: &[PathBuf],
    keep: &[String],
    work: &WorkingDirectory,
    api: &Path,
    shadow: Option<&Path>,
) -> io::Result<Command> {
    // `sandbox-exec` would read a leading dash as its own option.
    if program
        .first()
        .is_none_or(|first| first.is_empty() || first.starts_with('-'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a sandboxed agent needs a program that does not start with -",
        ));
    }
    let mut git = git_controls(work.path())?;
    let (tool, args, filter) = if cfg!(target_os = "macos") {
        let tool = PathBuf::from("/usr/bin/sandbox-exec");
        (
            tool,
            macos_args(hidden, program, work.path(), api, &git),
            None,
        )
    } else if cfg!(target_os = "linux") {
        let tool = bwrap().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no bubblewrap (bwrap) in /usr/bin, /usr/local/bin or /bin that only root can \
                 change, so the sandboxed agent does not start",
            )
        })?;
        let mut version = Command::new(&tool);
        strip_inherited(&mut version);
        let version = version.arg("--version").output()?.stdout;
        new_enough(&String::from_utf8_lossy(&version))?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let runtime = runtime.as_deref();
        let filter = pinfold_sys::seccomp::filter()?;
        let fd = filter.as_raw_fd();
        let real = fs::symlink_metadata(work.path().join(".git")).is_ok_and(|meta| meta.is_dir());
        if let Some(dir) = shadow.filter(|_| real) {
            git.shadow = Some(self::shadow(work.path(), dir)?);
        }
        let args = linux_args(hidden, program, work.path(), api, runtime, &git, fd);
        (tool, args, Some(filter))
    } else {
        return Err(io::ErrorKind::Unsupported.into());
    };
    let mut command = Command::new(tool);
    command.args(args);
    if let Some(filter) = filter {
        pinfold_sys::inherit(&mut command, filter)?;
        // Linux only, set before bwrap runs, so it and the agent share one domain that binds no
        // socket of the host's.
        if pinfold_sys::landlock::abi()? >= pinfold_sys::landlock::SCOPED {
            pinfold_sys::landlock::scope_abstract_sockets(&mut command)?;
        }
    }
    for name in SOCKET_VARS {
        command.env_remove(name);
    }
    // Names only: a value is never read here, nor logged.
    for (name, _) in std::env::vars_os() {
        if secret_name(&name) && !keep.iter().any(|kept| name == kept.as_str()) {
            command.env_remove(name);
        }
    }
    if let Some(dir) = &git.shadow {
        command
            .env("GIT_DIR", dir)
            .env("GIT_WORK_TREE", work.path());
    }
    Ok(command)
}

/// Makes `dir` the agent's own git directory for `work`, if it does not exist yet, and returns it
/// resolved (ADR-0039). On Linux it starts at the commit and index of `work`'s `.git`, read only
/// from files really inside it; elsewhere, and without them, at an unborn `main`. It borrows the
/// objects through `objects/info/alternates`, so nothing is copied. Agent commits stay there,
/// whose hooks and config only the sandbox runs, until the user fetches them. An existing one is
/// left as it is: the core never writes where the agent has.
///
/// # Errors
///
/// Returns an error if `dir` cannot be made or written, or it exists but is a symlink, not a
/// directory, another user's, or open to others.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use pinfold_core::sandbox;
///
/// let temp = std::env::temp_dir().join(format!("pinfold-shadow-doc-{}", std::process::id()));
/// std::fs::create_dir_all(temp.join("work/.git/objects"))?;
/// std::fs::write(temp.join("work/.git/HEAD"), "ref: refs/heads/main\n")?;
/// let shadow = sandbox::shadow(&temp.join("work"), &temp.join("data/shadow"))?;
/// assert_eq!(std::fs::read_to_string(shadow.join("HEAD"))?, "ref: refs/heads/main\n");
/// assert!(shadow.join("objects/info/alternates").is_file());
/// std::fs::remove_dir_all(&temp)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn shadow(work: &Path, dir: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let git = work.canonicalize()?.join(".git");
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    if let Some(parent) = dir.parent() {
        builder.recursive(true).create(parent)?;
    }
    match builder.recursive(false).create(dir) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dir)?;
            let own = meta.uid() == pinfold_sys::effective_uid() && meta.mode() & 0o077 == 0;
            if !meta.is_dir() || !own {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{dir:?} is not a private directory of this user's, so the sandboxed \
                         agent does not start"
                    ),
                ));
            }
            return dir.canonicalize();
        }
        made => made?,
    }
    for sub in ["objects/info", "refs/heads", "refs/tags"] {
        builder.recursive(true).create(dir.join(sub))?;
    }
    // Not resolved: the agent's git and the user's fetch each resolve it as they see it.
    let mut objects = git.join("objects").into_os_string();
    objects.push("\n");
    fs::write(
        dir.join("objects/info/alternates"),
        objects.as_encoded_bytes(),
    )?;
    let head = read_inside(&git, "HEAD").unwrap_or_default();
    let branch = head.strip_prefix("ref: ").map(str::trim).filter(|name| {
        let name = Path::new(name);
        name.starts_with("refs/heads")
            && name
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
    });
    match branch {
        Some(name) => {
            fs::write(dir.join("HEAD"), format!("ref: {name}\n"))?;
            if let Some(id) = commit(&git, name) {
                if let Some(parent) = dir.join(name).parent() {
                    builder.recursive(true).create(parent)?;
                }
                fs::write(dir.join(name), format!("{id}\n"))?;
            }
        }
        // A detached HEAD starts there; anything else starts an unborn `main`.
        None => match object_id(head.trim()) {
            Some(id) => fs::write(dir.join("HEAD"), format!("{id}\n"))?,
            None => fs::write(dir.join("HEAD"), "ref: refs/heads/main\n")?,
        },
    }
    // Without it, the agent's git would see every tracked file as newly deleted.
    if let Some(mut index) = open_inside(&git, "index") {
        io::copy(&mut index, &mut fs::File::create_new(dir.join("index"))?)?;
    }
    dir.canonicalize()
}

/// `name` in `git`, opened only if it is a file that really is inside `git`, as the open file's
/// path in `/proc` shows, which no later swap of a link or directory changes. On Linux the agent
/// may have made `.git` itself, with links to hidden files whose text the shadow would show it.
fn open_inside(git: &Path, name: &str) -> Option<fs::File> {
    // Not following a last link, and not blocking on a FIFO planted there.
    let file = pinfold_sys::open_no_follow(&git.join(name)).ok()?;
    let real = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
    let plain = file.metadata().is_ok_and(|meta| meta.is_file());
    (plain && real.starts_with(git)).then_some(file)
}

/// The text of `name` in `git`, from [`open_inside`], up to 1 MiB.
fn read_inside(git: &Path, name: &str) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    let file = open_inside(git, name)?;
    file.take(1 << 20).read_to_string(&mut text).ok()?;
    Some(text)
}

/// The commit `name` points at in `git`, loose or packed.
fn commit(git: &Path, name: &str) -> Option<String> {
    if let Some(text) = read_inside(git, name) {
        return object_id(text.trim());
    }
    let packed = read_inside(git, "packed-refs")?;
    packed.lines().find_map(|line| {
        let (id, other) = line.split_once(' ')?;
        (other == name).then(|| object_id(id)).flatten()
    })
}

/// `text` if it is a SHA-1 or SHA-256 object name.
fn object_id(text: &str) -> Option<String> {
    let hex = text.bytes().all(|byte| byte.is_ascii_hexdigit());
    (hex && matches!(text.len(), 40 | 64)).then(|| text.to_owned())
}

/// `sandbox-exec`'s arguments. Paths go in as parameters, so no path is quoted into the rules.
fn macos_args(
    hidden: &[PathBuf],
    program: &[String],
    work: &Path,
    api: &Path,
    found: &Git,
) -> Vec<OsString> {
    let mut profile = format!("{PROFILE}{CLIPBOARD}{WRITES}{GIT}{SOCKETS}");
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    let git = work.join(".git");
    let mut args = Vec::new();
    let params = [
        ("W", work),
        ("T", &temp),
        ("G", &git),
        ("S", api),
        ("SR", &resolve(api)),
    ];
    for (name, path) in params {
        let mut define = OsString::from(format!("{name}="));
        define.push(path);
        args.extend([OsString::from("-D"), define]);
    }
    let kept = found.kept.iter().chain(&found.modules);
    let rules = kept.map(|path| ("K", "literal", path));
    let rules = rules.chain(found.controls.iter().map(|path| ("C", "subpath", path)));
    for (index, (name, kind, path)) in rules.enumerate() {
        profile += &format!("(deny file-write* ({kind} (param \"{name}{index}\")))");
        let mut define = OsString::from(format!("{name}{index}="));
        define.push(resolve(path));
        args.extend([OsString::from("-D"), define]);
    }
    for (index, path) in hidden.iter().enumerate() {
        profile += &format!("(deny file-read* file-write* (subpath (param \"H{index}\")))");
        // The rules match resolved paths, such as /private/var for /var, even of a missing one.
        let mut define = OsString::from(format!("H{index}="));
        define.push(resolve(path));
        args.extend([OsString::from("-D"), define]);
    }
    let mut all = vec![OsString::from("-p"), profile.into()];
    all.extend(args);
    all.extend(program.iter().map(OsString::from));
    all
}

/// `bwrap`'s arguments: the real root read-only, private `/dev` with no `/dev/pts`, `/proc`,
/// `/tmp` and IPC, an
/// empty `runtime` directory but for the API socket's, `work` writable but its `.git` and its
/// submodules' `modules`, the seccomp filter on fd `filter`, an empty directory over each
/// hidden directory and `/dev/null` over each hidden file, and the shadow git directory writable
/// over that. A missing path is skipped, as bwrap would make it outside the sandbox, and so is one
/// inside another hidden directory, which that one's mount already covers.
fn linux_args(
    hidden: &[PathBuf],
    program: &[String],
    work: &Path,
    api: &Path,
    runtime: Option<&Path>,
    git: &Git,
    filter: i32,
) -> Vec<OsString> {
    let fixed = [
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        // `/dev/ptmx` leads here: a new terminal would be a session no pane holds, which the API
        // takes for a person's terminal outside every pane. The agent's own is already open.
        "--tmpfs",
        "/dev/pts",
        // Nor may the agent make a user namespace of its own, where it could mount a new devpts.
        "--unshare-user",
        "--disable-userns",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--die-with-parent",
        "--proc",
        "/proc",
        "--tmpfs",
        "/tmp",
    ];
    let mut args: Vec<OsString> = fixed.map(OsString::from).into();
    // Where ssh-agent, the session bus and other programs keep their sockets.
    if let Some(runtime) = runtime.filter(|runtime| runtime.is_absolute()) {
        args.extend(["--tmpfs".into(), runtime.into()]);
    }
    let mut mount = |kind: &str, path: &Path| args.extend([kind.into(), path.into(), path.into()]);
    if let Some(dir) = api.parent() {
        mount("--ro-bind", dir);
    }
    mount("--bind", work);
    // Read-only whole: git makes its locks in `.git`, so a mount cannot stop the agent making a
    // `commondir` or other file that points git at its own hooks and config. Commits happen
    // outside the sandbox, or in the shadow.
    let dot = work.join(".git");
    for path in std::iter::once(&dot).chain(&git.modules) {
        if fs::symlink_metadata(path).is_ok() {
            mount("--ro-bind", path);
        }
    }
    args.extend(["--seccomp".into(), filter.to_string().into()]);
    let mounted: Vec<&PathBuf> = hidden.iter().filter(|path| path.exists()).collect();
    for path in &mounted {
        if mounted
            .iter()
            .any(|outer| outer != path && path.starts_with(outer))
        {
            continue;
        }
        if path.is_dir() {
            args.extend([OsString::from("--tmpfs"), path.into()]);
        } else {
            // A file, such as `~/.netrc`, reads as empty.
            args.extend(["--ro-bind".into(), "/dev/null".into(), path.into()]);
        }
    }
    // After the hidden directories, so none of them covers it.
    if let Some(shadow) = &git.shadow {
        args.extend(["--bind".into(), shadow.into(), shadow.into()]);
    }
    args.push("--".into());
    args.extend(program.iter().map(OsString::from));
    args
}

/// What git runs code from in `work`, for macOS to deny writing: the directories down to each
/// repository's in `.git`, which must stay where they are, the hooks, config, worktree config and
/// pointers to other directories (`commondir`, `gitdir`) of the repository and of each submodule
/// and linked worktree, whether they exist or not, and each submodule's `.git` in the working
/// tree, as `.gitmodules` lists them. Symlinks are not followed.
///
/// Refuses if the repository or a submodule has no hooks directory or no config, if any of those
/// files is a symlink, or if there are more than [`GIT_DIRS`] directories and submodules.
fn git_controls(work: &Path) -> io::Result<Git> {
    let git = work.join(".git");
    let mut found = Git::default();
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{work:?} {why}, so the sandboxed agent does not start"),
        ))
    };
    let is_dir = |path: &Path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
    // Each directory, whether git keeps a repository there and whether that is a submodule's or
    // the top one's, which need their own hooks and config.
    let mut pending = Vec::new();
    if is_dir(&git) {
        pending.push((git.clone(), true, true));
    }
    for path in submodules(work) {
        let module = work.join(path).join(".git");
        // A submodule that keeps its repository in the working tree, not in `.git/modules`.
        if is_dir(&module) {
            pending.push((module.clone(), true, true));
        }
        found.modules.push(module);
    }
    let (kept, controls) = (&mut found.kept, &mut found.controls);
    let mut seen = found.modules.len();
    while let Some((dir, repository, module)) = pending.pop() {
        seen += 1;
        if seen > GIT_DIRS {
            return refuse("holds too many git directories to check");
        }
        if repository {
            kept.push(dir.clone());
            let (hooks, config) = (dir.join("hooks"), dir.join("config"));
            if module && !(hooks.is_dir() && config.is_file()) {
                return refuse("has a repository without hooks or config");
            }
            let found = [
                hooks,
                config,
                dir.join("config.worktree"),
                dir.join("commondir"),
                dir.join("gitdir"),
            ];
            // A link could be swapped for a directory of the agent's, as `.git` stays writable.
            let link = |path: &PathBuf| {
                fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
            };
            if found.iter().any(link) {
                return refuse("links its hooks or config elsewhere");
            }
            controls.extend(found);
            pending.push((dir.join("modules"), false, true));
            if dir == git {
                pending.push((dir.join("worktrees"), false, false));
            }
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        kept.push(dir);
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let path = entry.path();
                let repository = path.join("HEAD").is_file();
                pending.push((path, repository, module));
            }
        }
    }
    Ok(found)
}

/// The paths `.gitmodules` lists, if each is a relative path inside the working directory.
fn submodules(work: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read(work.join(".gitmodules")) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&text)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            // Keys ignore case, and `;` or `#` starts a comment outside quotes.
            let value = value.trim();
            let value = match value.strip_prefix('"') {
                Some(quoted) => quoted.split('"').next()?,
                None => value.split([';', '#']).next()?.trim(),
            };
            key.trim()
                .eq_ignore_ascii_case("path")
                .then(|| PathBuf::from(value))
        })
        .filter(|path| {
            path.components().next().is_some()
                && path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        })
        .collect()
}

/// Refuses a bubblewrap older than [`BWRAP_OLDEST`], from its `--version` text, such as
/// `bubblewrap 0.11.0`, or one whose version it cannot read.
fn new_enough(version: &str) -> io::Result<()> {
    let version = version.trim();
    let mut parts = version
        .strip_prefix("bubblewrap ")
        .unwrap_or_default()
        .split('.');
    let mut part = || parts.next().and_then(|part| part.parse::<u32>().ok());
    match (part(), part()) {
        (Some(major), Some(minor)) if (major, minor) >= BWRAP_OLDEST => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "sandboxed agents need bubblewrap 0.8 or later, for --disable-userns, but this is \
                 {version:?}, so the sandboxed agent does not start"
            ),
        )),
    }
}

/// The first of [`BWRAP`] that is [`trusted`].
fn bwrap() -> Option<PathBuf> {
    BWRAP.iter().map(PathBuf::from).find(|tool| trusted(tool))
}

/// Whether `file` is a file that only root can change, as it and every directory above it are
/// [`root_only`].
fn trusted(file: &Path) -> bool {
    file.is_file() && file.ancestors().all(root_only)
}

/// Whether `path` is owned by root, writable by no one else and not a symlink.
fn root_only(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| {
        !meta.file_type().is_symlink() && meta.uid() == 0 && meta.mode() & 0o022 == 0
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The `PATH` launchd gives an app.
    const LAUNCHD: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

    #[test]
    fn only_macos_names_secret_looking_variables_and_never_a_value() {
        let value = "ghp_value_never_shown";
        let env = [
            ("GITHUB_TOKEN", value),
            ("PATH", "/bin"),
            ("OPENAI_API_KEY", value),
            ("GITHUB_TOKEN", value),
        ];
        let names = env.map(|(name, _)| OsString::from(name));
        let told = exposed(false, names.clone());
        if cfg!(target_os = "macos") {
            let (count, listed) = told.unwrap();
            assert_eq!(
                (count, listed.as_str()),
                (2, "GITHUB_TOKEN, OPENAI_API_KEY")
            );
            assert!(!listed.contains(value));
        } else {
            assert_eq!(told, None, "Linux is not affected");
        }
        assert_eq!(exposed(true, names), None, "strip_secrets removes them");
        assert_eq!(exposed(false, [OsString::from("PATH")]), None);
    }

    #[test]
    fn the_notice_caps_its_names_and_escapes_them() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let mut names: Vec<OsString> = (0..9).map(|n| format!("A{n}_TOKEN").into()).collect();
        names.push(format!("{}_TOKEN\u{202e}", "X".repeat(60)).into());
        let (count, listed) = exposed(false, names).unwrap();
        assert_eq!(count, 10);
        assert_eq!(
            listed,
            "A0_TOKEN, A1_TOKEN, A2_TOKEN, A3_TOKEN, A4_TOKEN, …"
        );
        let long = [format!("BAD\u{1b}_{}_TOKEN", "X".repeat(60)).into()];
        let (_, listed) = exposed(false, long).unwrap();
        assert!(
            listed.starts_with("BAD^[_XXX") && listed.ends_with('…'),
            "{listed}"
        );
        assert_eq!(listed.chars().count(), NAME_SHOWN + 2);
    }

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    /// A new directory in `$HOME`, where a working directory may be, with `mode`.
    fn project(name: &str, mode: u32) -> tempfile::TempDir {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let dir = tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(home)
            .unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    #[test]
    fn bubblewrap_before_0_8_or_of_no_known_version_is_refused_by_name() {
        for version in ["bubblewrap 0.8.0\n", "bubblewrap 0.11.0", "bubblewrap 1.0"] {
            assert!(new_enough(version).is_ok(), "{version}");
        }
        for version in [
            "bubblewrap 0.6.1\n",
            "bubblewrap 0.7",
            "",
            "bwrap",
            "bubblewrap x.9",
        ] {
            let error = new_enough(version).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            let message = error.to_string();
            assert!(
                message.contains("need bubblewrap 0.8 or later"),
                "{message}"
            );
            assert!(
                message.contains(&format!("{:?}", version.trim())),
                "{message}"
            );
        }
    }

    #[test]
    fn macos_hides_each_path_by_parameter() {
        let hidden = [
            PathBuf::from("/nowhere/.ssh"),
            PathBuf::from("/nowhere/a\"b"),
        ];
        let (work, api) = (Path::new("/nowhere/work"), Path::new("/nowhere/api.sock"));
        let found = Git {
            controls: vec![work.join(".git/hooks")],
            modules: vec![work.join("sub/.git")],
            ..Git::default()
        };
        let program = ["agent".to_owned(), "-v".to_owned()];
        let args = macos_args(&hidden, &program, work, api, &found);
        let args = strings(&args);
        assert_eq!(args[0], "-p");
        assert!(args[1].starts_with(&format!("{PROFILE}{CLIPBOARD}{WRITES}{GIT}{SOCKETS}")));
        assert!(args[1].ends_with(
            "(deny file-write* (literal (param \"K0\")))\
             (deny file-write* (subpath (param \"C1\")))\
             (deny file-read* file-write* (subpath (param \"H0\")))\
             (deny file-read* file-write* (subpath (param \"H1\")))"
        ));
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(
            args[2..],
            [
                "-D",
                "W=/nowhere/work",
                "-D",
                &format!("T={}", temp.display()),
                "-D",
                "G=/nowhere/work/.git",
                "-D",
                "S=/nowhere/api.sock",
                "-D",
                "SR=/nowhere/api.sock",
                "-D",
                "K0=/nowhere/work/sub/.git",
                "-D",
                "C1=/nowhere/work/.git/hooks",
                "-D",
                "H0=/nowhere/.ssh",
                "-D",
                "H1=/nowhere/a\"b",
                "agent",
                "-v"
            ]
        );
    }

    #[test]
    fn linux_covers_existing_paths_and_keeps_the_session() {
        let root = std::env::temp_dir();
        let nested = tempfile::tempdir().unwrap();
        let inner = nested.path().to_owned();
        let project = project("pinfold-git", 0o700);
        let work = project.path();
        for path in [inner.clone(), work.join(".git")] {
            fs::create_dir_all(path).unwrap();
        }
        // A hidden file, such as `~/.netrc`, outside the temporary directory.
        let netrc = work.join("netrc");
        fs::write(&netrc, "machine example.com password secret\n").unwrap();
        // A directory inside another hidden one, before or after it, needs no mount of its own.
        let hidden = [
            netrc.clone(),
            inner.clone(),
            root.clone(),
            PathBuf::from("/nowhere/.ssh"),
        ];
        let api = Path::new("/run/user/7/pinfold/pinfold.sock");
        let runtime = Some(Path::new("/run/user/7"));
        let module = work.join("sub/.git");
        let found = Git {
            modules: vec![module.clone()],
            shadow: Some(PathBuf::from("/nowhere/data/shadow")),
            ..Git::default()
        };
        fs::create_dir_all(work.join("sub")).unwrap();
        fs::write(&module, "gitdir: ../.git/modules/sub\n").unwrap();
        let args = linux_args(
            &hidden,
            &["agent".to_owned()],
            work,
            api,
            runtime,
            &found,
            7,
        );
        let args = strings(&args);
        let shadow = "/nowhere/data/shadow";
        let tmpfs = [
            "--tmpfs",
            root.to_str().unwrap(),
            "--bind",
            shadow,
            shadow,
            "--",
            "agent",
        ];
        assert_eq!(args[args.len() - 7..], tmpfs);
        // /dev/pts, /tmp, the runtime directory and the outer hidden directory.
        assert_eq!(args.iter().filter(|arg| **arg == "--tmpfs").count(), 4);
        assert!(!args.contains(&"/nowhere/.ssh"));
        assert!(!args.contains(&"--new-session"));
        assert!(!args.contains(&"--unshare-net"));
        assert_eq!(args[..3], ["--ro-bind", "/", "/"]);
        let has = |want: &[&str]| args.windows(want.len()).any(|got| got == want);
        assert!(has(&["--dev", "/dev", "--tmpfs", "/dev/pts"]));
        assert!(has(&["--seccomp", "7"]));
        let (work, git) = (work.to_str().unwrap(), work.join(".git"));
        let (git, module) = (git.to_str().unwrap(), module.to_str().unwrap());
        assert!(has(&["--ro-bind", module, module]));
        assert!(has(&["--ro-bind", "/dev/null", netrc.to_str().unwrap()]));
        assert!(has(&["--tmpfs", "/tmp"]));
        assert!(has(&["--tmpfs", "/run/user/7"]));
        let dir = "/run/user/7/pinfold";
        assert!(has(&["--ro-bind", dir, dir]));
        assert!(has(&["--bind", work, work]));
        assert!(has(&["--ro-bind", git, git]));
        for flag in [
            "--die-with-parent",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-user",
            "--disable-userns",
        ] {
            assert!(args.contains(&flag), "{flag}");
        }
    }

    #[test]
    fn git_runs_nothing_the_agent_can_change() {
        let project = project("pinfold-controls", 0o700);
        let work = project.path();
        let git = work.join(".git");
        let module = git.join("modules/libs/sub");
        let worktree = git.join("worktrees/wt");
        for dir in [git.join("hooks"), module.join("hooks"), worktree.clone()] {
            fs::create_dir_all(dir).unwrap();
        }
        for file in [
            git.join("HEAD"),
            git.join("config"),
            module.join("HEAD"),
            module.join("config"),
            worktree.join("HEAD"),
        ] {
            fs::write(file, "").unwrap();
        }
        let controls = git_controls(work);
        fs::remove_dir_all(module.join("hooks")).unwrap();
        let hookless = git_controls(work);
        std::os::unix::fs::symlink(work, module.join("hooks")).unwrap();
        let linked = git_controls(work);
        fs::remove_dir_all(&git).unwrap();
        let none = git_controls(work);
        fs::write(&git, "gitdir: /elsewhere\n").unwrap();
        let gitfile = git_controls(work);
        let Git { kept, controls, .. } = controls.unwrap();
        for dir in [
            &git,
            &git.join("modules"),
            &git.join("modules/libs"),
            &module,
        ] {
            assert!(kept.contains(dir), "{dir:?} in {kept:?}");
        }
        for path in [
            git.join("hooks"),
            git.join("config"),
            git.join("config.worktree"),
            module.join("hooks"),
            module.join("config"),
            worktree.join("config.worktree"),
            worktree.join("commondir"),
            worktree.join("gitdir"),
            git.join("commondir"),
        ] {
            assert!(controls.contains(&path), "{path:?} in {controls:?}");
        }
        assert!(hookless.is_err() && linked.is_err());
        assert_eq!(none.unwrap(), Git::default());
        assert_eq!(gitfile.unwrap(), Git::default());
    }

    #[test]
    fn each_listed_submodule_keeps_its_git() {
        let project = project("pinfold-submodules", 0o700);
        let work = project.path();
        let vendor = work.join("vendor/.git");
        fs::create_dir_all(vendor.join("hooks")).unwrap();
        fs::create_dir_all(work.join("sub")).unwrap();
        for (path, text) in [
            (work.join("sub/.git"), "gitdir: ../.git/modules/sub\n"),
            (vendor.join("config"), ""),
            (vendor.join("HEAD"), ""),
            (
                work.join(".gitmodules"),
                "[submodule \"sub\"]\n\tPath = sub ; a comment\n[submodule \"vendor\"]\n\
                 \tpath = \"vendor\" # a comment\n\
                 [submodule \"out\"]\n\tpath = ../out\n[submodule \"root\"]\n\tpath = /etc\n",
            ),
        ] {
            fs::write(path, text).unwrap();
        }
        let found = git_controls(work).unwrap();
        assert_eq!(found.modules, [work.join("sub/.git"), vendor.clone()]);
        assert!(found.kept.contains(&vendor));
        assert!(found.controls.contains(&vendor.join("hooks")));
    }

    #[test]
    fn a_path_directory_or_one_above_it_is_refused() {
        let project_dir = project("pinfold-path", 0o700);
        let work = project_dir.path();
        let bin = work.join("bin");
        let link = work.join("link");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let real = work.canonicalize().unwrap();
        // Outside the working directory, leading to where the agent could make a directory.
        let away_dir = project("pinfold-path-away", 0o700);
        let away = away_dir.path();
        let dangling = away.join("bin");
        std::os::unix::fs::symlink(real.join("later"), &dangling).unwrap();
        let path = |entries: &[&Path]| std::env::join_paths(entries).unwrap();
        let other = Path::new("/usr/bin");
        let held = [
            holds_path(&real, &path(&[other, work])),
            holds_path(&real, &path(&[&bin])),
            holds_path(&real.join("bin"), &path(&[&link])),
            holds_path(&real, &path(&[&work.join("missing/bin")])),
            holds_path(&real, &path(&[other, Path::new("")])),
            holds_path(&real, &path(&[Path::new(".")])),
            holds_path(&real, &path(&[Path::new("bin")])),
            holds_path(&real, &path(&[&dangling])),
            holds_path(&real, &path(&[&dangling.join("sub")])),
        ];
        let free = [
            holds_path(&real.join("bin"), &path(&[work])),
            holds_path(&real, &path(&[other, away])),
        ];
        assert_eq!(held, [true; 9]);
        assert_eq!(free, [false; 2]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_shadow_starts_at_the_branch_and_is_then_left_alone() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp = temp_dir.path();
        let git = temp.join("work/.git");
        fs::create_dir_all(git.join("objects")).unwrap();
        let (one, two) = ("1".repeat(40), "2".repeat(64));
        let packed = format!("# pack-refs\n{one} refs/heads/topic/x\n");
        for (path, text) in [
            (git.join("HEAD"), "ref: refs/heads/topic/x\n".to_owned()),
            (git.join("packed-refs"), packed),
            (git.join("index"), "INDEX".to_owned()),
        ] {
            fs::write(path, text).unwrap();
        }
        let dir = temp.join("data/agents/a/shadow");
        let made = shadow(&temp.join("work"), &dir).unwrap();
        let read = |path: &str| fs::read_to_string(made.join(path)).unwrap();
        let first = [read("HEAD"), read("refs/heads/topic/x"), read("index")];
        let alternates = read("objects/info/alternates");
        let mode = fs::metadata(&made).unwrap().permissions().mode() & 0o777;
        // The agent's later changes stay, and a new launch keeps them.
        fs::write(made.join("HEAD"), "mine").unwrap();
        let again = shadow(&temp.join("work"), &dir).unwrap();
        let kept = read("HEAD");
        // One the core did not make privately is refused.
        fs::set_permissions(&made, fs::Permissions::from_mode(0o755)).unwrap();
        let open = shadow(&temp.join("work"), &dir);
        let link = temp.join("data/link");
        std::os::unix::fs::symlink(&made, &link).unwrap();
        let linked = shadow(&temp.join("work"), &link);
        // A detached HEAD, and one that names no branch.
        let detached = temp.join("data/detached");
        fs::write(git.join("HEAD"), format!("{two}\n")).unwrap();
        let detached =
            fs::read_to_string(shadow(&temp.join("work"), &detached).unwrap().join("HEAD"));
        let odd = temp.join("data/odd");
        fs::write(git.join("HEAD"), "ref: refs/heads/../../x\n").unwrap();
        let odd = fs::read_to_string(shadow(&temp.join("work"), &odd).unwrap().join("HEAD"));
        // A `.git` the agent made, with links out to a hidden file, shows none of it.
        let secret = temp.join("secret");
        fs::write(&secret, format!("{one}\n")).unwrap();
        fs::remove_dir_all(&git).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::os::unix::fs::symlink(&secret, git.join("index")).unwrap();
        std::os::unix::fs::symlink(&secret, git.join("refs/heads/main")).unwrap();
        // A FIFO for `packed-refs`, which a plain open would wait on forever.
        let fifo = std::process::Command::new("mkfifo")
            .arg(git.join("packed-refs"))
            .status();
        assert!(fifo.unwrap().success());
        let planted = shadow(&temp.join("work"), &temp.join("data/planted")).unwrap();
        let leaked = [
            planted.join("index").exists(),
            planted.join("refs/heads/main").exists(),
        ];
        let objects = temp.canonicalize().unwrap().join("work/.git/objects");
        assert_eq!(leaked, [false, false]);
        assert_eq!(
            first,
            [
                "ref: refs/heads/topic/x\n".to_owned(),
                format!("{one}\n"),
                "INDEX".to_owned()
            ]
        );
        assert_eq!(alternates, format!("{}\n", objects.display()));
        assert_eq!(mode, 0o700);
        assert_eq!((again, kept), (made, "mine".to_owned()));
        assert_eq!(open.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(linked.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(detached.unwrap(), format!("{two}\n"));
        assert_eq!(odd.unwrap(), "ref: refs/heads/main\n");
    }

    #[test]
    fn a_resolved_path_has_no_trailing_slash() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(resolve(&temp).as_os_str(), temp.as_os_str());
        let missing = temp.join("pinfold-missing/x");
        assert_eq!(resolve(&missing).as_os_str(), missing.as_os_str());
    }

    #[test]
    fn only_a_file_root_alone_can_change_is_trusted() {
        // Resolved, as some systems link /usr/bin/env to a multi-call binary.
        let system = fs::canonicalize("/usr/bin/env").unwrap();
        assert!(trusted(&system), "{system:?}");
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let planted = dir.join("bwrap");
        fs::write(&planted, "").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&system, &link).unwrap();
        // Each fails on its own, not for the user's directories above it.
        let (file, symlink) = (root_only(&planted), root_only(&link));
        let whole = (trusted(&planted), trusted(&link));
        assert!(!file && !symlink);
        assert_eq!(whole, (false, false));
    }

    #[test]
    fn a_working_directory_must_be_a_private_project() {
        let project_dir = project("pinfold-work", 0o700);
        let dir = project_dir.path();
        let (open, inner, git) = (dir.join("open"), dir.join("inner"), dir.join(".git/sub"));
        let under = open.join("under");
        let dot = project(".pinfold-dot", 0o700);
        let dot = dot.path();
        for (path, mode) in [
            (&open, 0o777),
            (&under, 0o700),
            (&inner, 0o700),
            (&git, 0o700),
        ] {
            fs::create_dir_all(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
        }
        let real = dir.canonicalize().unwrap();
        let work = working_directory(dir, &[], LAUNCHD.as_ref());
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let refused = [
            working_directory(Path::new("/"), &[], LAUNCHD.as_ref()),
            working_directory(&home, &[], LAUNCHD.as_ref()),
            working_directory(home.parent().unwrap(), &[], LAUNCHD.as_ref()),
            working_directory(dir, &[dir.join(".ssh")], LAUNCHD.as_ref()),
            working_directory(&inner, &[dir.to_owned()], LAUNCHD.as_ref()),
            working_directory(&open, &[], LAUNCHD.as_ref()),
            working_directory(&under, &[], LAUNCHD.as_ref()),
            working_directory(&git, &[], LAUNCHD.as_ref()),
            working_directory(dot, &[], LAUNCHD.as_ref()),
            working_directory(&dir.join("missing"), &[], LAUNCHD.as_ref()),
        ];
        let root = working_directory(Path::new("/usr/bin"), &[], LAUNCHD.as_ref());
        let library = working_directory(&home.join("Library"), &[], LAUNCHD.as_ref());
        assert_eq!(work.unwrap().path(), real);
        for refusal in refused {
            assert!(refusal.is_err(), "{refusal:?}");
        }
        if pinfold_sys::effective_uid() != 0 {
            assert!(root.is_err());
        }
        if cfg!(target_os = "macos") {
            assert!(library.is_err());
        }
    }

    #[test]
    fn a_working_directory_is_a_project_inside_home() {
        // A home of its own inside the real one, so only the rules about home refuse.
        let home_dir = project("pinfold-home", 0o700);
        let home = home_dir.path().canonicalize().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().canonicalize().unwrap();
        let projects = ["src/app", "Projects/app", "code/app"].map(|name| home.join(name));
        let refused = [
            "Applications",
            "bin",
            "go",
            "go/src/app",
            "miniconda3",
            "miniconda3/lib",
            "tools",
            "tools/scripts",
            "tools/lib",
        ]
        .map(|name| home.join(name));
        for dir in projects.iter().chain(&refused).chain([&outside]) {
            fs::create_dir_all(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let entries = ["go/bin", "miniconda3/bin", "tools/scripts"].map(|name| home.join(name));
        let login = std::env::join_paths(entries.iter().chain([&PathBuf::from("/usr/bin")]));
        let login = login.unwrap();
        let check = |dir: &Path, path: &OsStr| inside_home(dir, &[], path, &home);
        let accepted: Vec<_> = projects.iter().map(|dir| check(dir, &login)).collect();
        let by_login: Vec<_> = refused.iter().map(|dir| check(dir, &login)).collect();
        let by_launchd = [
            &home.join("Applications"),
            &home.join("bin"),
            &home,
            &outside,
        ]
        .map(|dir| check(dir, LAUNCHD.as_ref()));
        let out = check(&outside, LAUNCHD.as_ref());
        let brew = Path::new("/opt/homebrew");
        let brew = brew
            .is_dir()
            .then(|| working_directory(brew, &[], LAUNCHD.as_ref()));
        for (work, dir) in accepted.into_iter().zip(&projects) {
            assert_eq!(work.unwrap().path(), dir);
        }
        for refusal in by_login.into_iter().chain(by_launchd) {
            assert!(refusal.is_err(), "{refusal:?}");
        }
        let error = out.unwrap_err().to_string();
        assert!(error.contains("is not inside your home folder"), "{error}");
        assert!(
            error.contains("Start it from a project directory"),
            "{error}"
        );
        if let Some(brew) = brew {
            assert!(brew.is_err(), "{brew:?}");
        }
    }

    #[test]
    fn the_path_is_the_line_after_the_mark() {
        let printed = b"motd\npinfold-path:\nfake\npinfold-path:\n/a/bin:/b\nbye\n";
        assert_eq!(path_after_mark(printed), Some("/a/bin:/b".into()));
        assert_eq!(path_after_mark(b"pinfold-path:\n"), None);
        assert_eq!(path_after_mark(b"/a/bin\n"), None);
    }

    /// Calls `next` until the shell has answered, as a launch a few seconds later would.
    fn answered(mut next: impl FnMut() -> io::Result<OsString>) -> io::Result<OsString> {
        let start = Instant::now();
        loop {
            match next() {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(start.elapsed() < Duration::from_secs(90), "{error}");
                    std::thread::sleep(Duration::from_millis(20));
                }
                done => return done,
            }
        }
    }

    #[test]
    fn the_users_shell_reports_its_path() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path();
        let ask = || ask_shell(crate::shell::command(), private);
        let mut login = None;
        // Under load the user's startup files can take over LOGIN_WAIT, which would kill the
        // shell every time (#342); the deadline has its own test below.
        let wait = Duration::from_secs(60);
        let path = answered(|| next_path(&mut login, &ask, wait)).unwrap();
        assert!(
            std::env::split_paths(&path).any(|entry| entry.ends_with("bin")),
            "{path:?}"
        );
        assert_eq!(next_path(&mut login, &ask, wait).unwrap(), path);
    }

    /// A shell that takes `seconds` to print `/slow/bin` as its `PATH`.
    fn slow_shell(seconds: u32, private: &Path) -> io::Result<Asked> {
        let mut shell = Command::new("/bin/sh");
        let script = format!("sleep {seconds}; echo {PATH_MARK}; echo /slow/bin");
        shell.args(["-c", &script, "sh"]);
        ask_shell(shell, private)
    }

    #[test]
    fn a_launch_never_waits_for_a_slow_shell() {
        let temp = tempfile::tempdir().unwrap();
        let ask = || slow_shell(3, temp.path());
        let mut login = None;
        let start = Instant::now();
        let first = next_path(&mut login, &ask, LOGIN_WAIT).unwrap_err();
        let second = next_path(&mut login, &ask, LOGIN_WAIT).unwrap_err();
        let waited = start.elapsed();
        let path = answered(|| next_path(&mut login, &ask, LOGIN_WAIT));
        let again = next_path(&mut login, &ask, LOGIN_WAIT);
        assert!(waited < Duration::from_millis(500), "{waited:?}");
        for error in [first, second] {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert_eq!(error.to_string(), READING_PATH);
        }
        assert_eq!(path.unwrap(), "/slow/bin");
        assert_eq!(again.unwrap(), "/slow/bin");
    }

    #[test]
    fn a_shell_past_its_time_is_killed_reaped_and_asked_again() {
        let temp = tempfile::tempdir().unwrap();
        let ask = || slow_shell(30, temp.path());
        let mut login = None;
        let first = next_path(&mut login, &ask, Duration::ZERO).unwrap_err();
        let Some(Login::Asked((child, ..))) = &login else {
            panic!("no shell asked");
        };
        let late = child.id().to_string();
        let start = Instant::now();
        let error = next_path(&mut login, &ask, Duration::ZERO).unwrap_err();
        let waited = start.elapsed();
        // Reaped, not a zombie: its pid is gone.
        let gone = !Command::new("/bin/sh")
            .args(["-c", "kill -0 \"$0\"", &late])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        let asked_again = matches!(login, Some(Login::Asked(_)));
        if let Some(Login::Asked((mut child, ..))) = login.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(first.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(waited < Duration::from_millis(500), "{waited:?}");
        assert!(gone && asked_again);
    }

    #[test]
    fn the_agent_gets_no_other_programs_sockets() {
        let dir = project("pinfold-env", 0o700);
        let work = working_directory(dir.path(), &[], LAUNCHD.as_ref()).unwrap();
        let result = command(
            &["agent".to_owned()],
            &[],
            &[],
            &work,
            Path::new("/nowhere/api.sock"),
            None,
        );
        // Linux without bubblewrap refuses before it builds a command.
        if let Ok(command) = result {
            let removed: Vec<_> = command
                .get_envs()
                .filter(|(_, value)| value.is_none())
                .collect();
            for name in SOCKET_VARS {
                assert!(removed.iter().any(|(key, _)| *key == name), "{name}");
            }
        }
    }

    #[test]
    fn an_empty_program_or_an_option_is_refused() {
        for program in [&[][..], &[String::new()], &["-p".to_owned()]] {
            let work = WorkingDirectory(PathBuf::from("/nowhere"));
            let error = command(
                program,
                &[],
                &[],
                &work,
                Path::new("/nowhere/api.sock"),
                None,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
