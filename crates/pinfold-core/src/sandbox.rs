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
//! and on macOS those files, its submodules' and anything that points git elsewhere are. Of other
//! programs' Unix sockets, such as ssh-agent's, it reaches only PinFold's API socket.
//!
//! A missing tool is an error: a pane that asked for a sandbox never runs without one.

use std::ffi::OsString;
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
/// inside it, inside a `.git` directory, or hold or sit inside a `hidden` one.
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
    for hidden in hidden {
        let real = resolve(hidden);
        let near = |other: &PathBuf| other.starts_with(&dir) || dir.starts_with(other);
        if near(hidden) || near(&real) {
            return refuse("holds or is inside a directory the sandbox hides");
        }
    }
    Ok(WorkingDirectory(dir))
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
/// [`Reactor::spawn`](crate::io::Reactor::spawn). Spawn it once: on Linux its seccomp filter is
/// read to the end by the first launch.
///
/// # Errors
///
/// Returns an error if `program` is empty or starts with `-`, the system has no sandbox, or on
/// Linux no `bwrap` in `/usr/bin`, `/usr/local/bin` or `/bin` is root's alone, or the filter has
/// no rules for this architecture.
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
/// match sandbox::command(&program, &[], &work, &api) {
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
    let git = git_controls(work.path())?;
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
        let args = linux_args(hidden, program, work.path(), api, runtime, &git.modules, fd);
        (tool, args, Some(filter))
    } else {
        return Err(io::ErrorKind::Unsupported.into());
    };
    let mut command = Command::new(tool);
    command.args(args);
    if let Some(filter) = filter {
        pinfold_sys::inherit(&mut command, filter)?;
    }
    for name in SOCKET_VARS {
        command.env_remove(name);
    }
    Ok(command)
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
/// submodules' `modules`, the seccomp filter on fd `filter`, and an empty directory over each
/// hidden one. A missing directory is skipped, as bwrap would make it outside the sandbox, and so
/// is one inside another hidden directory, which that one's mount already covers.
fn linux_args(
    hidden: &[PathBuf],
    program: &[String],
    work: &Path,
    api: &Path,
    runtime: Option<&Path>,
    modules: &[PathBuf],
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
    // outside the sandbox.
    let git = work.join(".git");
    for git in std::iter::once(&git).chain(modules) {
        if fs::symlink_metadata(git).is_ok() {
            mount("--ro-bind", git);
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
        let modules = [module.clone()];
        fs::create_dir_all(work.join("sub")).unwrap();
        fs::write(&module, "gitdir: ../.git/modules/sub\n").unwrap();
        let args = linux_args(
            &hidden,
            &["agent".to_owned()],
            &work,
            api,
            runtime,
            &modules,
            7,
        );
        fs::remove_dir(&inner).unwrap();
        fs::remove_dir_all(&work).unwrap();
        let args = strings(&args);
        let tmpfs = ["--tmpfs", root.to_str().unwrap(), "--", "agent"];
        assert_eq!(args[args.len() - 4..], tmpfs);
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
            let error = command(program, &[], &work, Path::new("/nowhere/api.sock")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
