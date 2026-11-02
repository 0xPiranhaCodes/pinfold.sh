//! The socketpair behind a window's link, and starting a program with one end of it
//! (ADR-0028).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// The descriptor a program started by [`spawn_with_link`] finds its end of the link on.
pub const LINK_FD: RawFd = 3;

/// Highest descriptor checked before a spawn, which bounds the work in the child.
// shortcut: a descriptor above this that is not close-on-exec would reach the child, walk
// /proc/self/fd or use close_range if the core ever holds that many.
const MAX_CHECKED_FD: libc::c_long = 65_536;

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Returns a close-on-exec copy of `fd` numbered `lowest` or above.
fn duplicate(fd: RawFd, lowest: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC takes an integer and touches no memory.
    let copy = check(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, lowest) })?;
    // SAFETY: the call succeeded, so `copy` is open and owned only by this function.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// Returns a connected pair of Unix sockets, both close-on-exec.
///
/// On macOS they are briefly inheritable while they are being made, so one thread must make
/// every call that creates a child, as [`crate::pty::Pty::spawn`] also requires.
///
/// # Errors
///
/// Returns an error if the descriptors cannot be created.
///
/// # Examples
///
/// ```
/// use std::io::{Read, Write};
///
/// let (mut ours, mut theirs) = pinfold_sys::socket::pair()?;
/// ours.write_all(b"x")?;
/// let mut byte = [0];
/// theirs.read_exact(&mut byte)?;
/// assert_eq!(byte, *b"x");
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
    UnixStream::pair()
}

/// Starts `command` with `link` on [`LINK_FD`] and no other descriptor past stderr.
///
/// `link` is the only descriptor this makes inheritable, in the child alone. The caller's own
/// copy closes when this returns, so the child holds the only one.
///
/// # Errors
///
/// Returns an error if a descriptor cannot be copied or the program cannot be started.
///
/// # Examples
///
/// ```
/// use std::io::Read;
/// use std::process::Command;
///
/// let (mut ours, theirs) = pinfold_sys::socket::pair()?;
/// let mut command = Command::new("/bin/sh");
/// command.args(["-c", "echo hi >&3"]);
/// let mut child = pinfold_sys::socket::spawn_with_link(command, theirs)?;
/// let mut text = String::new();
/// ours.read_to_string(&mut text)?;
/// child.wait()?;
/// assert_eq!(text, "hi\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn spawn_with_link(mut command: Command, link: UnixStream) -> io::Result<Child> {
    // Above LINK_FD, because dup2 onto the same number would leave close-on-exec set.
    let high = duplicate(link.as_raw_fd(), LINK_FD + 1)?;
    // Fills LINK_FD if it is free, so the pipe that reports a failed exec cannot land there
    // and be overwritten in the child, which would turn a failed start into a success.
    let _hold = duplicate(link.as_raw_fd(), LINK_FD)?;
    let source = high.as_raw_fd();
    // SAFETY: sysconf reads a limit and touches no memory.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(0, MAX_CHECKED_FD) as RawFd;
    // SAFETY: the closure makes only async-signal-safe calls, on integers it owns.
    unsafe {
        command.pre_exec(move || {
            // SAFETY: both are descriptor numbers, and the copy comes without close-on-exec.
            check(libc::dup2(source, LINK_FD))?;
            for fd in LINK_FD + 1..limit {
                // Marked, not closed: the standard library's exec pipe is among them. A number
                // that is not open only fails.
                // SAFETY: F_SETFD takes an integer flag and touches no memory.
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read, Write};

    fn is_close_on_exec(fd: RawFd) -> bool {
        // SAFETY: F_GETFD takes no argument and touches no memory.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        flags & libc::FD_CLOEXEC != 0
    }

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn both_ends_are_close_on_exec() {
        let (ours, theirs) = pair().unwrap();
        assert!(is_close_on_exec(ours.as_raw_fd()));
        assert!(is_close_on_exec(theirs.as_raw_fd()));
    }

    #[test]
    fn the_child_gets_the_link_on_its_number_both_ways() {
        let (mut ours, theirs) = pair().unwrap();
        let script = "read line <&3; echo got-$line >&3";
        let mut child = spawn_with_link(shell(script), theirs).unwrap();
        ours.write_all(b"ping\n").unwrap();
        let mut text = String::new();
        ours.read_to_string(&mut text).unwrap();
        assert_eq!(text, "got-ping\n");
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn the_callers_copy_is_gone_so_the_child_exiting_closes_the_link() {
        let (mut ours, theirs) = pair().unwrap();
        let mut child = spawn_with_link(shell("exit 0"), theirs).unwrap();
        child.wait().unwrap();
        assert_eq!(ours.read(&mut [0; 8]).unwrap(), 0);
    }

    #[test]
    fn an_inheritable_descriptor_does_not_reach_the_child() {
        // Numbered high, as a plain dup can land on LINK_FD itself. No close-on-exec, on purpose.
        // SAFETY: F_DUPFD takes an integer and touches no memory.
        let leak = check(unsafe { libc::fcntl(2, libc::F_DUPFD, 20) }).unwrap();
        // SAFETY: `leak` is open and owned only by this test.
        let _leak = unsafe { OwnedFd::from_raw_fd(leak) };
        assert!(!is_close_on_exec(leak));

        // A probe in the shell would also see its own saved descriptors, so ls reports. It
        // opens up to two of its own, which take the first free numbers, 4 and 5.
        let (mut ours, theirs) = pair().unwrap();
        let script = "exec ls /dev/fd >&3";
        let mut child = spawn_with_link(shell(script), theirs).unwrap();
        let mut text = String::new();
        ours.read_to_string(&mut text).unwrap();
        child.wait().unwrap();
        let open: Vec<RawFd> = text.lines().map(|fd| fd.parse().unwrap()).collect();
        assert!(open.contains(&LINK_FD), "{open:?}");
        assert!(open.iter().all(|fd| *fd <= LINK_FD + 2), "{open:?}");
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let (_ours, theirs) = pair().unwrap();
        let result = spawn_with_link(Command::new("/does/not/exist"), theirs);
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
    }
}
