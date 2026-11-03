//! The command that starts a sandboxed agent profile's pane (ADR-0020 point 5).
//!
//! The sandbox wraps the pane's whole program: `sandbox-exec` on macOS and bubblewrap on Linux.
//! Neither starts a session, so the agent keeps the pane's controlling terminal and session, by
//! which the API knows it. Nothing outside the sandbox reads that terminal, so TIOCSTI, which
//! only reaches the caller's own terminal, can type into sandboxed programs alone.
//!
//! A missing tool is an error: a pane that asked for a sandbox never runs without one.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The macOS rules: everything is allowed but the hidden directories and every terminal by
/// name. A pane's own terminal is already open, and `/dev/tty` still reaches it.
const PROFILE: &str = "(version 1)(allow default)\
(deny file-read* file-write* (regex #\"^/dev/ttys[0-9]+$\"))";

/// Returns the command that runs `program` with `hidden` unreadable, for
/// [`Reactor::spawn`](crate::io::Reactor::spawn).
///
/// # Errors
///
/// Returns an error if `program` is empty or starts with `-`, the system has no sandbox, or on Linux `bwrap` is not
/// on `PATH`.
///
/// # Examples
///
/// ```
/// let program = ["/bin/echo".to_owned(), "hi".to_owned()];
/// match pinfold_core::sandbox::command(&program, &[]) {
///     Ok(command) => assert_ne!(command.get_program(), "/bin/echo"),
///     // No bubblewrap here: the pane does not start.
///     Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
/// }
/// ```
pub fn command(program: &[String], hidden: &[PathBuf]) -> io::Result<Command> {
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
    let (tool, args) = if cfg!(target_os = "macos") {
        let tool = PathBuf::from("/usr/bin/sandbox-exec");
        (tool, macos_args(hidden, program))
    } else if cfg!(target_os = "linux") {
        let tool = bwrap().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "bubblewrap (bwrap) is not on PATH, so the sandboxed agent does not start",
            )
        })?;
        (tool, linux_args(hidden, program))
    } else {
        return Err(io::ErrorKind::Unsupported.into());
    };
    let mut command = Command::new(tool);
    command.args(args);
    Ok(command)
}

/// `sandbox-exec`'s arguments. Paths go in as parameters, so no path is quoted into the rules.
fn macos_args(hidden: &[PathBuf], program: &[String]) -> Vec<OsString> {
    let mut profile = PROFILE.to_owned();
    let mut args = Vec::new();
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

/// `bwrap`'s arguments: the real root, a private `/dev` and `/proc`, and an empty directory over
/// each hidden one. A missing directory is skipped, as bwrap would make it outside the sandbox.
fn linux_args(hidden: &[PathBuf], program: &[String]) -> Vec<OsString> {
    let fixed = [
        "--bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--unshare-pid",
        "--proc",
        "/proc",
    ];
    let mut args: Vec<OsString> = fixed.map(OsString::from).into();
    for path in hidden.iter().filter(|path| path.is_dir()) {
        args.extend([OsString::from("--tmpfs"), path.into()]);
    }
    args.push("--".into());
    args.extend(program.iter().map(OsString::from));
    args
}

/// The first `bwrap` on `PATH`.
fn bwrap() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("bwrap"))
        .find(|tool| Path::is_file(tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|arg| arg.to_str().unwrap()).collect()
    }

    #[test]
    fn macos_hides_each_path_by_parameter() {
        let hidden = [
            PathBuf::from("/nowhere/.ssh"),
            PathBuf::from("/nowhere/a\"b"),
        ];
        let args = macos_args(&hidden, &["agent".to_owned(), "-v".to_owned()]);
        let args = strings(&args);
        assert_eq!(args[0], "-p");
        assert!(args[1].starts_with(PROFILE));
        assert!(args[1].ends_with(
            "(deny file-read* file-write* (subpath (param \"H0\")))\
             (deny file-read* file-write* (subpath (param \"H1\")))"
        ));
        assert_eq!(
            args[2..],
            [
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
        let hidden = [root.clone(), PathBuf::from("/nowhere/.ssh")];
        let args = linux_args(&hidden, &["agent".to_owned()]);
        let args = strings(&args);
        let tmpfs = ["--tmpfs", root.to_str().unwrap(), "--", "agent"];
        assert_eq!(args[args.len() - 4..], tmpfs);
        assert!(!args.contains(&"/nowhere/.ssh"));
        assert!(!args.contains(&"--new-session"));
    }

    #[test]
    fn an_empty_program_or_an_option_is_refused() {
        for program in [&[][..], &[String::new()], &["-p".to_owned()]] {
            let error = command(program, &[]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }
}
