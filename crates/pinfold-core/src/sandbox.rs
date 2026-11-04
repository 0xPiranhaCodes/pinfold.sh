//! The command that starts a sandboxed agent profile's pane (ADR-0020 point 5).
//!
//! The sandbox wraps the pane's whole program: `sandbox-exec` on macOS and bubblewrap on Linux.
//! Neither starts a session, so the agent keeps the pane's controlling terminal and session, by
//! which the API knows it. TIOCSTI, which would type into that terminal, is denied: by the rules
//! on macOS and by a seccomp filter on Linux.
//!
//! The agent writes only to its [`WorkingDirectory`] and temporary directories: a fresh `/tmp` on
//! Linux, and `/private/tmp` and the user's temporary directory on macOS. Git runs what its
//! repository's hooks and config name outside the sandbox, so on Linux all of `.git` is read-only,
//! and the agent commits into a [`shadow`] git directory of its own instead (ADR-0039). On macOS
//! those files, its submodules' and anything that points git elsewhere are read-only. Of other
//! programs' Unix sockets, such as ssh-agent's, it reaches only PinFold's API socket, and on
//! Linux with Landlock ABI 6 no abstract socket bound outside the sandbox (ADR-0038).
//!
//! A missing tool is an error: a pane that asked for a sandbox never runs without one.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Where bubblewrap is looked for. Not `PATH`, where an agent could put its own `bwrap` for the
/// next launch.
const BWRAP: [&str; 3] = ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"];

/// Variables that name other programs' sockets, which the agent does not get.
const SOCKET_VARS: [&str; 3] = [
    "SSH_AUTH_SOCK",
    "GPG_AGENT_INFO",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// The macOS rules: everything is allowed but the hidden directories, every terminal by name and
/// TIOCSTI (`0x80017472`). A pane's own terminal is already open, and `/dev/tty` still reaches it.
const PROFILE: &str = "(version 1)(allow default)\
(deny file-read* file-write* (regex #\"^/dev/ttys[0-9]+$\"))\
(deny file-ioctl (ioctl-command 2147578994))";

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
/// can write to, under directories that only it or root can change. It must not be `/`, `$HOME`
/// or above it, a dot-directory of `$HOME` such as `~/.config` or inside one, `~/Library` or
/// inside it, inside a `.git` directory, a `PATH` directory or above one, or hold or sit inside a
/// `hidden` one.
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
/// assert!(working_directory(&project, &[]).is_ok());
/// assert!(working_directory(&project, &[project.join(".ssh")]).is_err());
/// assert!(working_directory(Path::new("/"), &[]).is_err());
/// std::fs::remove_dir(&project)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn working_directory(path: &Path, hidden: &[PathBuf]) -> io::Result<WorkingDirectory> {
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{path:?} {why}, so the sandboxed agent does not start"),
        ))
    };
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
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return refuse("cannot be checked without HOME");
    };
    let home = resolve(&home);
    if home.starts_with(&dir) {
        return refuse("is the home directory or above it");
    }
    // What runs at login, from PATH or from git lives here, such as ~/.local/bin and ~/.config.
    let first = dir
        .strip_prefix(&home)
        .ok()
        .and_then(|rest| rest.iter().next());
    let library = cfg!(target_os = "macos") && first.is_some_and(|name| name == "Library");
    if library || first.is_some_and(|name| name.as_encoded_bytes().starts_with(b".")) {
        return refuse("is or is inside a dot-directory of home or ~/Library");
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
    if std::env::var_os("PATH").is_some_and(|path| holds_path(&dir, &path)) {
        return refuse("is or holds a PATH directory");
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

/// Returns the command that runs `program` with `hidden` unreadable, only `work` and temporary
/// directories writable and, of Unix sockets, only the API's at `api` reachable, for
/// [`Reactor::spawn`](crate::io::Reactor::spawn). On Linux, given a `shadow` directory and a
/// `work` with a `.git` directory, the agent's git uses [`shadow`] there. On Linux below Landlock ABI 6 abstract sockets
/// stay reachable. Spawn it once: on Linux its seccomp filter is read to the end by the first
/// launch.
///
/// # Errors
///
/// Returns an error if `program` is empty or starts with `-`, the system has no sandbox, or on
/// Linux no `bwrap` in `/usr/bin`, `/usr/local/bin` or `/bin` is root's alone, the filter has
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
/// let work = sandbox::working_directory(&project, &[])?;
/// let program = ["/bin/echo".to_owned(), "hi".to_owned()];
/// let api = std::env::temp_dir().join("pinfold/pinfold.sock");
/// match sandbox::command(&program, &[], &work, &api, None) {
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
    let mut profile = format!("{PROFILE}{WRITES}{GIT}{SOCKETS}");
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
        // The rules match resolved paths, such as /private/var for /var.
        let path = path.canonicalize().unwrap_or_else(|_| path.clone());
        let mut define = OsString::from(format!("H{index}="));
        define.push(path);
        args.extend([OsString::from("-D"), define]);
    }
    let mut all = vec![OsString::from("-p"), profile.into()];
    all.extend(args);
    all.extend(program.iter().map(OsString::from));
    all
}

/// `bwrap`'s arguments: the real root read-only, private `/dev`, `/proc`, `/tmp` and IPC, an
/// empty `runtime` directory but for the API socket's, `work` writable but its `.git` and its
/// submodules' `modules`, the seccomp filter on fd `filter`, an empty directory over each
/// hidden one, and the shadow git directory writable over that. A missing directory is skipped, as bwrap would make it outside the sandbox, and so
/// is one inside another hidden directory, which that one's mount already covers.
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
    let mounted: Vec<&PathBuf> = hidden.iter().filter(|path| path.is_dir()).collect();
    for path in &mounted {
        if mounted
            .iter()
            .any(|outer| outer != path && path.starts_with(outer))
        {
            continue;
        }
        args.extend([OsString::from("--tmpfs"), path.into()]);
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

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    /// A new directory in `$HOME`, where a working directory may be, with `mode`.
    fn project(name: &str, mode: u32) -> PathBuf {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let dir = home.join(format!("{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
        dir
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
        assert!(args[1].starts_with(&format!("{PROFILE}{WRITES}{GIT}{SOCKETS}")));
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
        let inner = root.join(format!("pinfold-nested-{}", std::process::id()));
        let work = project("pinfold-git", 0o700);
        for path in [inner.clone(), work.join(".git")] {
            fs::create_dir_all(path).unwrap();
        }
        // A directory inside another hidden one, before or after it, needs no mount of its own.
        let hidden = [inner.clone(), root.clone(), PathBuf::from("/nowhere/.ssh")];
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
            &work,
            api,
            runtime,
            &found,
            7,
        );
        fs::remove_dir(&inner).unwrap();
        fs::remove_dir_all(&work).unwrap();
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
        // /tmp, the runtime directory and the outer hidden directory.
        assert_eq!(args.iter().filter(|arg| **arg == "--tmpfs").count(), 3);
        assert!(!args.contains(&"/nowhere/.ssh"));
        assert!(!args.contains(&"--new-session"));
        assert!(!args.contains(&"--unshare-net"));
        assert_eq!(args[..3], ["--ro-bind", "/", "/"]);
        let has = |want: &[&str]| args.windows(want.len()).any(|got| got == want);
        assert!(has(&["--seccomp", "7"]));
        let (work, git) = (work.to_str().unwrap(), work.join(".git"));
        let (git, module) = (git.to_str().unwrap(), module.to_str().unwrap());
        assert!(has(&["--ro-bind", module, module]));
        assert!(has(&["--tmpfs", "/tmp"]));
        assert!(has(&["--tmpfs", "/run/user/7"]));
        let dir = "/run/user/7/pinfold";
        assert!(has(&["--ro-bind", dir, dir]));
        assert!(has(&["--bind", work, work]));
        assert!(has(&["--ro-bind", git, git]));
        for flag in ["--die-with-parent", "--unshare-ipc", "--unshare-uts"] {
            assert!(args.contains(&flag), "{flag}");
        }
    }

    #[test]
    fn git_runs_nothing_the_agent_can_change() {
        let work = project("pinfold-controls", 0o700);
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
        let controls = git_controls(&work);
        fs::remove_dir_all(module.join("hooks")).unwrap();
        let hookless = git_controls(&work);
        std::os::unix::fs::symlink(&work, module.join("hooks")).unwrap();
        let linked = git_controls(&work);
        fs::remove_dir_all(&git).unwrap();
        let none = git_controls(&work);
        fs::write(&git, "gitdir: /elsewhere\n").unwrap();
        let gitfile = git_controls(&work);
        fs::remove_dir_all(&work).unwrap();
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
        let work = project("pinfold-submodules", 0o700);
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
        let found = git_controls(&work);
        fs::remove_dir_all(&work).unwrap();
        let found = found.unwrap();
        assert_eq!(found.modules, [work.join("sub/.git"), vendor.clone()]);
        assert!(found.kept.contains(&vendor));
        assert!(found.controls.contains(&vendor.join("hooks")));
    }

    #[test]
    fn a_path_directory_or_one_above_it_is_refused() {
        let work = project("pinfold-path", 0o700);
        let bin = work.join("bin");
        let link = work.join("link");
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let real = work.canonicalize().unwrap();
        // Outside the working directory, leading to where the agent could make a directory.
        let away = project("pinfold-path-away", 0o700);
        let dangling = away.join("bin");
        std::os::unix::fs::symlink(real.join("later"), &dangling).unwrap();
        let path = |entries: &[&Path]| std::env::join_paths(entries).unwrap();
        let other = Path::new("/usr/bin");
        let held = [
            holds_path(&real, &path(&[other, &work])),
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
            holds_path(&real.join("bin"), &path(&[&work])),
            holds_path(&real, &path(&[other, &away])),
        ];
        fs::remove_dir_all(&work).unwrap();
        fs::remove_dir_all(&away).unwrap();
        assert_eq!(held, [true; 9]);
        assert_eq!(free, [false; 2]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_shadow_starts_at_the_branch_and_is_then_left_alone() {
        let temp = std::env::temp_dir().join(format!("pinfold-shadow-{}", std::process::id()));
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
        fs::remove_dir_all(&temp).unwrap();
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
        let dir = std::env::temp_dir().join(format!("pinfold-planted-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let planted = dir.join("bwrap");
        fs::write(&planted, "").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&system, &link).unwrap();
        // Each fails on its own, not for the user's directories above it.
        let (file, symlink) = (root_only(&planted), root_only(&link));
        let whole = (trusted(&planted), trusted(&link));
        fs::remove_dir_all(&dir).unwrap();
        assert!(!file && !symlink);
        assert_eq!(whole, (false, false));
    }

    #[test]
    fn a_working_directory_must_be_a_private_project() {
        let dir = project("pinfold-work", 0o700);
        let (open, inner, git) = (dir.join("open"), dir.join("inner"), dir.join(".git/sub"));
        let under = open.join("under");
        let dot = project(".pinfold-dot", 0o700);
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
        let work = working_directory(&dir, &[]);
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let refused = [
            working_directory(Path::new("/"), &[]),
            working_directory(&home, &[]),
            working_directory(home.parent().unwrap(), &[]),
            working_directory(&dir, &[dir.join(".ssh")]),
            working_directory(&inner, std::slice::from_ref(&dir)),
            working_directory(&open, &[]),
            working_directory(&under, &[]),
            working_directory(&git, &[]),
            working_directory(&dot, &[]),
            working_directory(&dir.join("missing"), &[]),
        ];
        let root = working_directory(Path::new("/usr/bin"), &[]);
        let library = working_directory(&home.join("Library"), &[]);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&dot).unwrap();
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
    fn the_agent_gets_no_other_programs_sockets() {
        let dir = project("pinfold-env", 0o700);
        let work = working_directory(&dir, &[]).unwrap();
        let result = command(
            &["agent".to_owned()],
            &[],
            &work,
            Path::new("/nowhere/api.sock"),
            None,
        );
        fs::remove_dir(&dir).unwrap();
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
            let error =
                command(program, &[], &work, Path::new("/nowhere/api.sock"), None).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
