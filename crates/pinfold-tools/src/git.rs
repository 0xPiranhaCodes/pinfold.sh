//! The one place a built-in tool starts git (threat T5, ADR-0045 section 4).
//!
//! A repository's own configuration can name commands that `git diff` and `git status` run:
//! `core.fsmonitor`, a filter driver, a diff driver, a pager. [`run`] turns each of them off,
//! so looking at a hostile repository runs nothing from it.
//!
//! A checkpoint store (ADR-0050) is reached only through [`store`] and [`import`], which read
//! no configuration but the store's own and never see a work tree. On macOS, Apple's
//! `/usr/bin/git` stub runs only once the Command Line Tools are found, so no install dialog
//! ever opens.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Bytes of standard output kept; git is cut short past them.
pub(crate) const MAX_OUTPUT: usize = 8_000_000;
/// Bytes of standard error kept, enough for any message git prints.
const MAX_ERRORS: usize = 64 * 1024;
/// Time git may run before it is killed.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
/// Time `repack` and `prune` may run in a store, at background priority off the launch path:
/// `repack` took 10.5 to 15.5 s on a Linux-sized one (ADR-0050 amendment).
const STORE_UPKEEP: Duration = Duration::from_secs(300);
/// Apple's stub, which offers to install the Command Line Tools in a dialog when they are
/// missing (ADR-0050 decision 5).
const APPLE_STUB: Option<&str> = if cfg!(target_os = "macos") {
    Some("/usr/bin/git")
} else {
    None
};
/// The store's configuration, written by PinFold: no command, and zlib level 1, as deltas
/// come later from `repack` (ADR-0050 section 2).
const STORE_CONFIG: &str =
    "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n\tcompression = 1\n";

/// Overrides on every call. `-c` beats every configuration file. With every transport refused,
/// `core.sshCommand`, credential helpers and `core.askPass` cannot be reached; they are emptied
/// as well, and an empty `credential.helper` clears the list.
const OVERRIDES: [&str; 14] = [
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.pager=cat",
    // A submodule's own filters are not in this repository's list (below), so git never looks
    // inside one. `submodule.<name>.ignore` beats this; `--ignore-submodules=all` beats both.
    "diff.ignoreSubmodules=all",
    "core.sshCommand=",
    "core.askPass=",
    "credential.helper=",
    "protocol.allow=never",
    // A repository's `protocol.<name>.allow` beats `protocol.allow`.
    "protocol.ext.allow=never",
    "protocol.file.allow=never",
    "protocol.git.allow=never",
    "protocol.ssh.allow=never",
    "protocol.http.allow=never",
    "protocol.https.allow=never",
];

/// What git printed, and how it ended.
#[derive(Debug)]
pub struct Output {
    /// The exit status, or `None` if git was killed at the time cap.
    pub status: Option<ExitStatus>,
    /// Standard output, at most 8 MB.
    pub stdout: Vec<u8>,
    /// Standard error, at most 64 KiB.
    pub stderr: Vec<u8>,
    /// Whether git ran past 10 s or printed more than 8 MB, so `stdout` is not all of it.
    pub cut_short: bool,
}

/// Runs `git <args>` in `repo` with every command the repository could name turned off.
///
/// Every inherited `GIT_*` variable is cleared, so neither `GIT_EXTERNAL_DIFF` nor `GIT_DIR`
/// reaches git, and `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0` and `GIT_NO_LAZY_FETCH=1`
/// are set. Git gets `--no-pager` and the `-c` overrides above; `git config --list -z` (which runs
/// nothing) first names the filter drivers and protocols to turn off. `diff` gets
/// `--no-ext-diff --no-textconv --no-color`, and `diff` and `status` get
/// `--ignore-submodules=all`, so a submodule's changes do not show. Another subcommand that
/// prints a diff, such as `show`, must pass those itself. `safe.directory` is left to the user's configuration.
///
/// `args` are the caller's own: a value from outside, such as a path, goes after `--`, and a
/// revision range is checked first, as `tool.open` does.
///
/// # Errors
///
/// Returns an error if no `git` is on `PATH`, it cannot start, or it cannot list the
/// repository's configuration.
///
/// # Examples
///
/// ```no_run
/// let output = pinfold_tools::git::run("/path/to/repo".as_ref(), ["diff", "--", "src"])?;
/// assert!(!output.cut_short);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn run<I, S>(repo: &Path, args: I) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_within(repo, args, TIMEOUT)
}

/// Runs `git apply` in `repo` through [`run`]'s hardening, with `patch` on its standard input
/// and no file written: `--cached` stages it, or `-R` reverts it in the work tree. There is never
/// `--unsafe-paths`, so git refuses a path outside the repository or behind a symbolic link.
///
/// # Errors
///
/// As [`run`].
///
/// # Examples
///
/// ```no_run
/// let patch = b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n";
/// let output = pinfold_tools::git::apply("/path/to/repo".as_ref(), patch, false)?;
/// assert!(output.status.is_some_and(|status| status.success()));
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn apply(repo: &Path, patch: &[u8], revert: bool) -> io::Result<Output> {
    let how = if revert { "-R" } else { "--cached" };
    spawn(repo, ["apply", how], TIMEOUT, patch)
}

/// [`run`], with git and its configuration listing killed once `timeout` has passed in all.
pub(crate) fn run_within<I, S>(repo: &Path, args: I, timeout: Duration) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    spawn(repo, args, timeout, &[])
}

/// [`run_within`], with `input` on git's standard input if there is any.
fn spawn<I, S>(repo: &Path, args: I, timeout: Duration, input: &[u8]) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let deadline = Instant::now() + timeout;
    let git = locate()?;
    let listed = capture(
        command(&git, repo, &[]).args(["config", "--list", "-z"]),
        deadline,
        &[],
    )?;
    if listed.cut_short {
        return Ok(Output {
            stdout: Vec::new(),
            ..listed
        });
    }
    if listed.status.is_none_or(|status| !status.success()) {
        let errors = String::from_utf8_lossy(&listed.stderr);
        return Err(io::Error::other(format!(
            "git config --list: {}",
            errors.trim()
        )));
    }
    let mut args = args.into_iter().map(|arg| arg.as_ref().to_owned());
    let mut command = command(&git, repo, &overrides(&listed.stdout));
    if let Some(subcommand) = args.next() {
        command.arg(&subcommand);
        if subcommand == "diff" {
            command.args(["--no-ext-diff", "--no-textconv", "--no-color"]);
        }
        if subcommand == "diff" || subcommand == "status" {
            command.arg("--ignore-submodules=all");
        }
    }
    if !input.is_empty() {
        command.stdin(Stdio::piped());
    }
    capture(command.args(args), deadline, input)
}

/// The first absolute `PATH` entry with an executable `name`, such as `git`. PATH is the user's choice of git,
/// which a repository cannot change; a fixed path would pick Apple's `/usr/bin/git` shim over
/// Homebrew's and miss git on NixOS. A relative entry, `.` or empty, would resolve against the
/// repository, where a hostile `git` may wait, so it is skipped.
pub(crate) fn find(path: &OsStr, name: &str) -> Option<PathBuf> {
    env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|git| {
            git.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

/// The git to run: [`find`]'s, unless that is Apple's stub and the Command Line Tools are
/// missing, when git counts as missing. The check runs once a process.
fn locate() -> io::Result<PathBuf> {
    static TOOLS: OnceLock<bool> = OnceLock::new();
    let path = env::var_os("PATH").unwrap_or_default();
    let tools = || *TOOLS.get_or_init(|| command_line_tools("/usr/bin/xcode-select".as_ref()));
    lookup(&path, APPLE_STUB.map(Path::new), tools)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no git"))
}

/// The first git on `path`, refused if it is `stub` and `tools` says it would not run.
fn lookup(path: &OsStr, stub: Option<&Path>, tools: impl FnOnce() -> bool) -> Option<PathBuf> {
    let git = find(path, "git")?;
    (stub != Some(git.as_path()) || tools()).then_some(git)
}

/// Whether `xcode-select -p` names a developer directory holding `usr/bin/git`, so Apple's stub
/// runs git rather than offering to install it. `-p` only prints; it opens nothing.
fn command_line_tools(xcode_select: &Path) -> bool {
    let mut command = Command::new(xcode_select);
    command
        .arg("-p")
        .env_clear()
        .envs(inherited(env::vars_os()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let deadline = Instant::now() + crate::files::STATUS_TIMEOUT;
    let Ok(printed) = capture(&mut command, deadline, &[]) else {
        return false;
    };
    let dir = Path::new(OsStr::from_bytes(printed.stdout.trim_ascii_end()));
    printed.status.is_some_and(|status| status.success())
        && dir.is_absolute()
        && dir.join("usr/bin/git").is_file()
}

/// Makes a new checkpoint store at `store`: a bare repository, mode 0700, whose files PinFold
/// writes itself, as neither `init` nor `config` runs there. Its configuration names no command.
/// Refusing a store that is a link, another user's or open to others is the caller's.
///
/// # Errors
///
/// Returns an error if anything is already at `store` or a file cannot be written.
///
/// # Examples
///
/// ```no_run
/// pinfold_tools::git::create_store("/path/to/state/checkpoints/0123456789abcdef".as_ref())?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn create_store(store: &Path) -> io::Result<()> {
    let mut dirs = fs::DirBuilder::new();
    dirs.mode(0o700).create(store)?;
    for dir in [
        "objects",
        "objects/info",
        "objects/pack",
        "refs",
        "refs/heads",
        "refs/tags",
    ] {
        dirs.create(store.join(dir))?;
    }
    fs::write(store.join("HEAD"), "ref: refs/heads/main\n")?;
    fs::write(store.join("config"), STORE_CONFIG)
}

/// Runs `git <args>` in the checkpoint store `store` (ADR-0050 section 2): `GIT_DIR` is the
/// store, `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` leave only its own
/// configuration, which PinFold wrote, and no other variable is passed, so there is no work
/// tree and nothing the person's environment names. [`run`]'s overrides, absolute-`PATH`
/// lookup, output cap and 10 s timeout hold; `repack` and `prune` get 300 s.
///
/// Only these run: `diff-tree`, `diff` (given `--no-ext-diff --no-textconv --no-color`),
/// `cat-file --batch`, `for-each-ref`, `update-ref -d`, `repack -a -d -q`, `prune` and
/// `rev-list --objects --all --disk-usage`, which gets 300 s too.
/// `fast-import` is [`import`].
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] for any other subcommand, otherwise as [`run`].
///
/// # Examples
///
/// ```no_run
/// let store = "/path/to/state/checkpoints/0123456789abcdef".as_ref();
/// let refs = pinfold_tools::git::store(store, ["for-each-ref", "refs/pinfold/"], b"")?;
/// assert!(!refs.cut_short);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn store<I, S>(store: &Path, args: I, input: &[u8]) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    store_with(store, args, input, None)
}

/// [`store`], with git's standard output written to `out` rather than kept, so a diff of any
/// size costs no memory; the returned `stdout` is empty and `cut_short` means only the timeout.
///
/// # Errors
///
/// As [`store`].
///
/// # Examples
///
/// ```no_run
/// let store = "/path/to/state/checkpoints/0123456789abcdef".as_ref();
/// let out = std::fs::File::create("/path/to/diff")?;
/// pinfold_tools::git::store_to(store, ["diff-tree", "-r", "-z", "refs/pinfold/r1-0/0", "refs/pinfold/r1-0/1"], out)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn store_to<I, S>(store: &Path, args: I, out: fs::File) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    store_with(store, args, b"", Some(out))
}

fn store_with<I, S>(
    store: &Path,
    args: I,
    input: &[u8],
    out: Option<fs::File>,
) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let words: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
    let timeout = match words.as_slice() {
        [b"diff-tree" | b"diff" | b"for-each-ref", ..] | [b"cat-file", b"--batch"] => TIMEOUT,
        [b"update-ref", b"-d", ..] => TIMEOUT,
        [b"repack", b"-a", b"-d", b"-q"] | [b"prune", ..] => STORE_UPKEEP,
        [b"rev-list", b"--objects", b"--all", b"--disk-usage"] => STORE_UPKEEP,
        _ => {
            let refused = "not a subcommand a checkpoint store runs";
            return Err(io::Error::new(io::ErrorKind::InvalidInput, refused));
        }
    };
    let mut command = store_command(&locate()?, store);
    command.arg(&args[0]);
    if words[0] == b"diff" {
        command.args(["--no-ext-diff", "--no-textconv", "--no-color"]);
    }
    if !input.is_empty() {
        command.stdin(Stdio::piped());
    }
    if let Some(out) = out {
        command.stdout(out);
    }
    capture(command.args(&args[1..]), Instant::now() + timeout, input)
}

/// A `git fast-import --quiet --depth=0` writing into a checkpoint store, run as [`store`]
/// runs git. Its input is written through [`Write`] and buffered; it has no timeout of its own,
/// as it waits on its writer. Input ends only with [`Import::finish`]: dropped before that, or
/// killed with the take, the importer stops at the end of input without `done` and leaves no
/// partial commit.
#[derive(Debug)]
pub struct Import {
    child: Child,
    input: Option<io::BufWriter<ChildStdin>>,
    errors: Option<JoinHandle<Vec<u8>>>,
}

/// Starts a `git fast-import` into `store`, after `feature done`.
///
/// # Errors
///
/// Returns an error if no git is found or it cannot start.
///
/// # Examples
///
/// ```no_run
/// use std::io::Write;
///
/// let store = "/path/to/state/checkpoints/0123456789abcdef".as_ref();
/// let mut import = pinfold_tools::git::import(store)?;
/// import.write_all(b"blob\ndata 6\nhello\n\n")?;
/// import.finish()?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn import(store: &Path) -> io::Result<Import> {
    let mut command = store_command(&locate()?, store);
    command
        .args(["fast-import", "--quiet", "--depth=0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    let mut child = command.spawn()?;
    let stderr = child.stderr.take();
    let errors = Some(thread::spawn(move || read(stderr, MAX_ERRORS)));
    let input = child
        .stdin
        .take()
        .map(|stdin| io::BufWriter::with_capacity(64 * 1024, stdin));
    let mut import = Import {
        child,
        input,
        errors,
    };
    import.write_all(b"feature done\n")?;
    Ok(import)
}

impl Import {
    /// Ends the input with `done` and waits for git to write its pack and refs.
    ///
    /// # Errors
    ///
    /// Returns an error if writing fails or git exits with a failure, with git's first line.
    pub fn finish(mut self) -> io::Result<()> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| io::Error::other("no input"))?;
        input.write_all(b"done\n")?;
        input.flush()?;
        drop(input);
        let status = self.child.wait()?;
        let errors = self.errors.take().map(|e| e.join().unwrap_or_default());
        if status.success() {
            return Ok(());
        }
        let errors = errors.unwrap_or_default();
        let first = String::from_utf8_lossy(&errors);
        let first = first.lines().next().unwrap_or("no message");
        Err(io::Error::other(format!("git fast-import: {first}")))
    }
}

impl Write for Import {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.input {
            Some(input) => input.write(bytes),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.input.as_mut().map_or(Ok(()), Write::flush)
    }
}

impl Drop for Import {
    fn drop(&mut self) {
        if self.input.take().is_some() {
            // Not finished: input ends without `done`, so git keeps nothing; reap it.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Git in the checkpoint store `store`, with the fixed environment [`store`] describes.
fn store_command(git: &Path, store: &Path) -> Command {
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("GIT_DIR", store)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LC_ALL", "C")
        .current_dir(store)
        .arg("--no-pager");
    for setting in OVERRIDES {
        command.arg("-c").arg(setting);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Git in `repo` with a scrubbed environment, the fixed overrides and `extra` ones.
fn command(git: &Path, repo: &Path, extra: &[(OsString, &str)]) -> Command {
    let mut command = Command::new(git);
    command
        .env_clear()
        .envs(inherited(env::vars_os()))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        // `-c` would split a key such as `filter.a=b.clean` at its `=`; these variables cannot.
        .env("GIT_CONFIG_COUNT", extra.len().to_string());
    for (n, (key, value)) in extra.iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{n}"), key)
            .env(format!("GIT_CONFIG_VALUE_{n}"), value);
    }
    command.current_dir(repo).arg("--no-pager");
    for setting in OVERRIDES {
        command.arg("-c").arg(setting);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The environment without any `GIT_*` variable.
fn inherited(
    vars: impl Iterator<Item = (OsString, OsString)>,
) -> impl Iterator<Item = (OsString, OsString)> {
    vars.filter(|(key, _)| !key.as_bytes().starts_with(b"GIT_"))
}

/// An override for each filter command and protocol in `git config --list -z` output, whose
/// entries are `key\nvalue\0`. Git lower-cases the section and the name, not the driver.
fn overrides(listed: &[u8]) -> Vec<(OsString, &'static str)> {
    listed
        .split(|&byte| byte == 0)
        .filter_map(|entry| {
            let key = entry.split(|&byte| byte == b'\n').next()?;
            let value = if key.starts_with(b"filter.") {
                [".clean", ".smudge", ".process"]
                    .iter()
                    .any(|name| key.ends_with(name.as_bytes()))
                    .then_some("")
                    .or_else(|| key.ends_with(b".required").then_some("false"))
            } else {
                (key.starts_with(b"protocol.") && key.ends_with(b".allow")).then_some("never")
            }?;
            Some((OsStr::from_bytes(key).to_owned(), value))
        })
        .collect()
}

/// Runs `command` with `input` on its piped standard input, keeping capped output, and kills it
/// at `deadline`.
fn capture(command: &mut Command, deadline: Instant, input: &[u8]) -> io::Result<Output> {
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    let input = input.to_vec();
    // Dropping the pipe at the end ends git's input; a write to a git that died fails, unseen.
    let writer = thread::spawn(move || stdin.map(|mut stdin| stdin.write_all(&input)));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // At the cap the reader drops the pipe, and git's next write ends it with SIGPIPE.
    let out = thread::spawn(move || read(stdout, MAX_OUTPUT));
    let errors = thread::spawn(move || read(stderr, MAX_ERRORS));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            break None;
        }
        thread::sleep(Duration::from_millis(5));
    };
    // shortcut: a process git started that keeps the pipe open holds this join; kill git's
    // process group if a tool ever hangs here.
    let _ = writer.join();
    let mut stdout = out.join().unwrap_or_default();
    let cut_short = status.is_none() || stdout.len() > MAX_OUTPUT;
    stdout.truncate(MAX_OUTPUT);
    let mut stderr = errors.join().unwrap_or_default();
    stderr.truncate(MAX_ERRORS);
    Ok(Output {
        status,
        stdout,
        stderr,
        cut_short,
    })
}

/// Up to `cap` bytes and one more from `pipe`, so a longer stream shows as longer.
fn read<P: Read>(pipe: Option<P>, cap: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe {
        let _ = pipe.take(cap as u64 + 1).read_to_end(&mut bytes);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn no_other_file_but_the_editors_starts_a_process() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // shortcut: reads src/ one level deep; recurse once a module directory appears.
        for entry in fs::read_dir(src).expect("src") {
            let path = entry.expect("entry").path();
            // editor.rs starts the user's editor, and nothing else (ADR-0045 section 4).
            let allowed = path.ends_with("git.rs") || path.ends_with("editor.rs");
            if path.extension() == Some("rs".as_ref()) && !allowed {
                let text = fs::read_to_string(&path).expect("read");
                // A test may make a fixture, as md.rs makes a FIFO.
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                assert!(!code.contains("Command::new"), "{}", path.display());
            }
        }
    }

    #[test]
    fn a_call_is_killed_at_its_deadline() {
        let start = Instant::now();
        let mut sleep = Command::new("sleep");
        sleep.arg("30");
        let deadline = start + crate::files::STATUS_TIMEOUT;
        let output = capture(&mut sleep, deadline, &[]).expect("sleep");
        let took = start.elapsed();
        assert!(output.cut_short && output.status.is_none());
        assert!((2.0..5.0).contains(&took.as_secs_f64()), "{took:?}");
    }

    #[test]
    fn git_is_found_only_through_absolute_path_entries() {
        let dir = env::temp_dir().join(format!("pg{}", std::process::id()));
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).expect("dir");
        let git = bin.join("git");
        fs::write(&git, "#!/bin/sh\n").expect("git");
        assert_eq!(find(bin.as_os_str(), "git"), None, "not executable");
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = env::join_paths([Path::new(""), Path::new("."), &bin]).expect("join");
        assert_eq!(find(&path, "git"), Some(git));
        // A relative entry is never searched, even when the current directory holds a git.
        assert_eq!(find(OsStr::new(".:bin:"), "git"), None);
        fs::remove_dir_all(dir).expect("clean");
    }

    #[test]
    fn apples_stub_runs_only_once_the_command_line_tools_are_found() {
        let dir = env::temp_dir().join(format!("pinfold-clt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let bin = dir.join("bin");
        fs::create_dir_all(dir.join("tools/usr/bin")).expect("dir");
        fs::create_dir_all(dir.join("empty")).expect("dir");
        fs::create_dir_all(&bin).expect("dir");
        let script = |path: PathBuf, text: String| {
            fs::write(&path, format!("#!/bin/sh\n{text}\n")).expect("write");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        };
        let ran = dir.join("ran");
        let stub = script(bin.join("git"), format!("touch {}", ran.display()));
        script(dir.join("tools/usr/bin/git"), String::new());
        // As `xcode-select -p` answers with no developer directory chosen.
        let missing = script(
            dir.join("missing"),
            "echo 'xcode-select: error' >&2; exit 2".into(),
        );
        let empty = script(
            dir.join("chosen-empty"),
            format!("echo {}", dir.join("empty").display()),
        );
        let found = script(
            dir.join("found"),
            format!("echo {}", dir.join("tools").display()),
        );
        let path = bin.as_os_str();
        assert_eq!(
            lookup(path, Some(&stub), || command_line_tools(&missing)),
            None
        );
        assert_eq!(
            lookup(path, Some(&stub), || command_line_tools(&empty)),
            None
        );
        assert_eq!(
            lookup(path, Some(&stub), || command_line_tools(&found)),
            Some(stub.clone())
        );
        let other = Some(Path::new("/usr/bin/git"));
        assert_eq!(
            lookup(path, other, || unreachable!("not asked")),
            Some(stub)
        );
        assert!(!ran.exists(), "the stub never ran");
        fs::remove_dir_all(dir).expect("clean");
    }

    #[test]
    fn a_store_runs_only_its_subcommands() {
        let store = Path::new("/nonexistent");
        let refused: [&[&str]; 9] = [
            &["init"],
            &["config", "core.hooksPath", "x"],
            &["fast-import"],
            &["update-ref", "refs/x", "HEAD"],
            &["repack", "-A"],
            &["repack", "-a", "-d", "-q", "--window=1000"],
            &["cat-file", "-p", "HEAD"],
            &["log"],
            &[],
        ];
        for args in refused {
            let error = super::store(store, args, b"").expect_err("refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{args:?}");
        }
    }

    #[test]
    fn every_git_variable_is_dropped() {
        let vars = [
            "GIT_EXTERNAL_DIFF",
            "GIT_DIR",
            "GIT_CONFIG_COUNT",
            "PATH",
            "HOME",
        ]
        .map(|key| (OsString::from(key), OsString::from("x")));
        let kept: Vec<_> = inherited(vars.into_iter()).map(|(key, _)| key).collect();
        assert_eq!(kept, ["PATH", "HOME"]);
    }

    #[test]
    fn filter_commands_and_protocols_found_in_the_configuration_are_overridden() {
        let listed = b"filter.lfs.clean\ngit-lfs clean\0filter.a=b.process\nx\0\
            filter.lfs.required\ntrue\0protocol.ext.allow\nalways\0diff.x.textconv\nx\0core.bare\0";
        let overrides = overrides(listed);
        let expected = [
            ("filter.lfs.clean", ""),
            ("filter.a=b.process", ""),
            ("filter.lfs.required", "false"),
            ("protocol.ext.allow", "never"),
        ]
        .map(|(key, value)| (OsString::from(key), value));
        assert_eq!(overrides, expected);
    }
}
