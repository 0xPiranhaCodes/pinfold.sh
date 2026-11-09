//! The one place a built-in tool starts git (threat T5, ADR-0045 section 4).
//!
//! A repository's own configuration can name commands that `git diff` and `git status` run:
//! `core.fsmonitor`, a filter driver, a diff driver, a pager. [`run`] turns each of them off,
//! so looking at a hostile repository runs nothing from it.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Bytes of standard output kept; git is cut short past them.
pub(crate) const MAX_OUTPUT: usize = 8_000_000;
/// Bytes of standard error kept, enough for any message git prints.
const MAX_ERRORS: usize = 64 * 1024;
/// Time git may run before it is killed.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);

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
    let path = env::var_os("PATH").unwrap_or_default();
    let git = find(&path).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no git"))?;
    let listed = capture(command(&git, repo, &[]).args(["config", "--list", "-z"]))?;
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
    capture(command.args(args))
}

/// The first absolute `PATH` entry with an executable `git`. PATH is the user's choice of git,
/// which a repository cannot change; a fixed path would pick Apple's `/usr/bin/git` shim over
/// Homebrew's and miss git on NixOS. A relative entry, `.` or empty, would resolve against the
/// repository, where a hostile `git` may wait, so it is skipped.
fn find(path: &OsStr) -> Option<PathBuf> {
    env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("git"))
        .find(|git| {
            git.metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
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

/// Runs `command`, keeping capped output, and kills it at the time cap.
fn capture(command: &mut Command) -> io::Result<Output> {
    let mut child = command.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // At the cap the reader drops the pipe, and git's next write ends it with SIGPIPE.
    let out = thread::spawn(move || read(stdout, MAX_OUTPUT));
    let errors = thread::spawn(move || read(stderr, MAX_ERRORS));
    let deadline = Instant::now() + TIMEOUT;
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
    fn no_other_file_in_the_crate_starts_a_process() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // shortcut: reads src/ one level deep; recurse once a module directory appears.
        for entry in fs::read_dir(src).expect("src") {
            let path = entry.expect("entry").path();
            if path.extension() == Some("rs".as_ref()) && !path.ends_with("git.rs") {
                let text = fs::read_to_string(&path).expect("read");
                // A test may make a fixture, as md.rs makes a FIFO.
                let code = text.split("#[cfg(test)]").next().unwrap_or_default();
                assert!(!code.contains("Command::new"), "{}", path.display());
            }
        }
    }

    #[test]
    fn git_is_found_only_through_absolute_path_entries() {
        let dir = env::temp_dir().join(format!("pg{}", std::process::id()));
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).expect("dir");
        let git = bin.join("git");
        fs::write(&git, "#!/bin/sh\n").expect("git");
        assert_eq!(find(bin.as_os_str()), None, "not executable");
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = env::join_paths([Path::new(""), Path::new("."), &bin]).expect("join");
        assert_eq!(find(&path), Some(git));
        // A relative entry is never searched, even when the current directory holds a git.
        assert_eq!(find(OsStr::new(".:bin:")), None);
        fs::remove_dir_all(dir).expect("clean");
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
