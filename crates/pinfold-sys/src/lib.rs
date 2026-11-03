//! FFI and libc calls: the only crate allowed unsafe code.

#![allow(unsafe_code, reason = "this crate is the FFI boundary")]

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod foreground;
pub mod landlock;
pub mod mapped;
#[cfg(all(target_os = "macos", feature = "menu"))]
pub mod menu;
#[cfg(all(target_os = "macos", feature = "metal"))]
pub mod metal;
pub mod pty;
pub mod seccomp;
pub mod socket;
#[cfg(all(target_os = "macos", feature = "metal"))]
pub mod window;

/// Returns a process's memory footprint in bytes: PSS on Linux and `phys_footprint` on
/// macOS, the two metrics ADR-0021 budgets are written in.
///
/// # Errors
///
/// Returns an error if the process has gone or its memory cannot be read.
///
/// # Examples
///
/// ```
/// let bytes = pinfold_sys::footprint(std::process::id())?;
/// assert!(bytes > 0);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn footprint(pid: u32) -> io::Result<u64> {
    platform::footprint(pid)
}

/// Writes a byte to `pipe` on each SIGTERM and SIGHUP, the self-pipe the core polls to save its
/// session before it exits. Neither signal ends the process after this, so the reader must.
///
/// # Errors
///
/// Returns an error if the descriptor cannot be copied or the system refuses a handler.
pub fn on_terminate(pipe: &OwnedFd) -> io::Result<()> {
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        signal_hook::low_level::pipe::register_raw(signal, pipe.try_clone()?)?;
    }
    Ok(())
}

/// Returns the user this process acts as.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// Stops a same-user process from debugging this one, which on Linux stops it copying a PTY
/// master out too (ADR-0020). Call this before opening any PTY. It lasts for the process, holds
/// when called again and ends at `exec`, so a pane's programs can still be debugged.
///
/// On macOS this stops `ptrace` alone, and a process that tries gets `SIGSEGV`, which is what
/// ends a debugger's server when `lldb -p` tries. A task port is
/// not stopped: only a hardened runtime signature does that. Under a debugger a process exits
/// here with status 45, and `pinfold-core --idle 1` is the way to debug a core. On Linux this
/// also stops other processes reading its `/proc` entries, which is why `cargo xtask mem-gate`
/// measures a core that has not called it, and a tracer already attached stays attached.
///
/// # Errors
///
/// Returns an error if the system refuses. A core must not go on without this.
pub fn harden() -> io::Result<()> {
    platform::harden()
}

/// Tries to attach to `pid` as a debugger and detaches again, to prove [`harden`] held.
///
/// The caller is left alone: macOS answers an attach to a hardened process with `SIGSEGV` as well
/// as an error, so this ignores that signal for the call, and is not for a process that can fault.
///
/// # Errors
///
/// Returns [`io::ErrorKind::ResourceBusy`] for a hardened process. Any other refusal is one
/// that any process gets without the debugger entitlement, hardened or not.
///
/// # Examples
///
/// ```
/// let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn()?;
/// let refused = pinfold_sys::probe_attach(child.id()).unwrap_err();
/// assert_ne!(refused.kind(), std::io::ErrorKind::ResourceBusy);
/// child.kill()?;
/// child.wait()?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(target_os = "macos")]
pub fn probe_attach(pid: u32) -> io::Result<()> {
    platform::probe_attach(pid)
}

/// Returns the user at the other end of a connected Unix socket.
///
/// # Errors
///
/// Returns an error if `socket` is not a connected Unix socket.
///
/// # Examples
///
/// ```
/// use std::os::fd::AsFd;
///
/// let (ours, _theirs) = std::os::unix::net::UnixStream::pair()?;
/// assert_eq!(pinfold_sys::peer_uid(ours.as_fd())?, pinfold_sys::effective_uid());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn peer_uid(socket: BorrowedFd<'_>) -> io::Result<u32> {
    platform::peer_uid(socket.as_raw_fd())
}

/// A controlling terminal and the session that holds it: what makes a process part of a pane
/// (ADR-0004). A pane's comes from [`pty::Pty::terminal`] and a caller's from [`peer_terminal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Terminal {
    /// The terminal's device number, in the 32-bit form both systems report for a process.
    pub device: u32,
    /// The session's id, which is its leader's pid.
    pub session: u32,
}

/// Returns the controlling terminal and session of the process at the other end of a connected
/// Unix socket, or nothing if it has no terminal or has gone (ADR-0020, ADR-0032).
///
/// The pid comes from `SO_PEERCRED` on Linux and `LOCAL_PEERPID` on macOS. Where Linux has
/// `SO_PEERPIDFD`, that descriptor is only used to check the peer outlived the lookup, so a
/// reused pid gives nothing. macOS and older Linux kernels have no such check.
///
/// # Errors
///
/// Returns an error if `socket` is not a connected Unix socket.
///
/// # Examples
///
/// ```
/// use std::os::fd::AsFd;
/// use std::os::unix::net::{UnixListener, UnixStream};
///
/// let path = std::env::temp_dir().join(format!("pinfold-peer-{}", std::process::id()));
/// let listener = UnixListener::bind(&path)?;
/// let _caller = UnixStream::connect(&path)?;
/// let (ours, _) = listener.accept()?;
/// // The peer is this process, which may or may not have a terminal.
/// let _terminal = pinfold_sys::peer_terminal(ours.as_fd())?;
/// std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn peer_terminal(socket: BorrowedFd<'_>) -> io::Result<Option<Terminal>> {
    platform::peer_terminal(socket.as_raw_fd())
}

/// Starts `command` in a session of its own, with nothing on stdin or stdout and `stderr` as its
/// stderr, so it outlives the caller and the caller's terminal.
///
/// The caller exits soon after, and the system reaps the child from then on.
///
/// # Errors
///
/// Returns an error if the program cannot be started.
///
/// # Examples
///
/// ```
/// use std::process::{Command, Stdio};
///
/// let mut child = pinfold_sys::spawn_detached(Command::new("/usr/bin/true"), Stdio::null())?;
/// assert!(child.wait()?.success());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn spawn_detached(mut command: Command, stderr: Stdio) -> io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe, and the closure touches nothing else.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    command.spawn()
}

/// Hands `fd` to the program `command` starts, at the same number, as bubblewrap's `--seccomp`
/// needs. The fd stays close-on-exec in this process, so no other child gets it, and closes
/// here when `command` is dropped.
///
/// # Errors
///
/// Returns an error if `fd` is 0, 1 or 2, where the child's stdio would land over it.
///
/// # Examples
///
/// ```
/// use std::os::fd::{AsRawFd, OwnedFd};
/// use std::process::Command;
///
/// let fd = OwnedFd::from(std::fs::File::open("/dev/null")?);
/// let mut command = Command::new("/bin/sh");
/// command.args(["-c", &format!("test -e /dev/fd/{}", fd.as_raw_fd())]);
/// pinfold_sys::inherit(&mut command, fd)?;
/// assert!(command.status()?.success());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn inherit(command: &mut Command, fd: OwnedFd) -> io::Result<()> {
    if fd.as_raw_fd() < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a descriptor handed to a child must be 3 or above",
        ));
    }
    // SAFETY: fcntl is async-signal-safe, and `fd` is open until the closure, and so `command`,
    // is dropped.
    unsafe {
        command.pre_exec(
            move || match libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, 0) {
                -1 => Err(io::Error::last_os_error()),
                _ => Ok(()),
            },
        );
    }
    Ok(())
}

/// Opens the log at `path` empty, for appending: mode 0600 if it is new, and refused if it is a
/// symlink. Appending means that a file emptied under a running writer stays free of holes.
///
/// # Errors
///
/// Returns an error if the file cannot be opened.
///
/// # Examples
///
/// ```
/// use std::io::Write;
///
/// let path = std::env::temp_dir().join(format!("pinfold-log-{}", std::process::id()));
/// writeln!(pinfold_sys::open_log(&path)?, "first")?;
/// assert_eq!(pinfold_sys::open_log(&path)?.metadata()?.len(), 0);
/// std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn open_log(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        // O_NONBLOCK, so that a FIFO put here fails at once and cannot hang a launcher.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    // Not O_TRUNC, which the standard library refuses beside append.
    file.set_len(0)?;
    Ok(file)
}

/// Opens the file at `path` for reading, refused if it is a symlink. The caller checks that it
/// got a regular file: O_NONBLOCK, so that a FIFO put here cannot hang the open.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or is a symlink.
///
/// # Examples
///
/// ```
/// let path = std::env::temp_dir().join(format!("pinfold-read-{}", std::process::id()));
/// std::fs::write(&path, "saved")?;
/// assert!(pinfold_sys::open_no_follow(&path)?.metadata()?.is_file());
/// std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn open_no_follow(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// A moment on the local clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    /// Days since Sunday, from 0.
    pub weekday: u8,
    /// Day of the month, from 1.
    pub day: u8,
    /// Months since January, from 0.
    pub month: u8,
    /// Hour of the day, from 0.
    pub hour: u8,
    /// Minute of the hour, from 0.
    pub minute: u8,
    /// Second of the minute, from 0 and up to 60 for a leap second.
    pub second: u8,
}

/// Returns `seconds` since the Unix epoch on the local clock, or nothing if it is out of range.
///
/// # Examples
///
/// ```
/// let time = pinfold_sys::local_time(1_000_000_000).unwrap();
/// assert_eq!(time.second, 40);
/// ```
#[must_use]
pub fn local_time(seconds: i64) -> Option<LocalTime> {
    let seconds = seconds as libc::time_t;
    let mut local = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: both pointers are to live values of the right types, and neither is kept.
    let result = unsafe { libc::localtime_r(&raw const seconds, local.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    // SAFETY: the call succeeded, so it filled in the struct it was given.
    let local = unsafe { local.assume_init() };
    let field = |value: libc::c_int| u8::try_from(value).ok();
    Some(LocalTime {
        weekday: field(local.tm_wday)?,
        day: field(local.tm_mday)?,
        month: field(local.tm_mon)?,
        hour: field(local.tm_hour)?,
        minute: field(local.tm_min)?,
        second: field(local.tm_sec)?,
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    use crate::Terminal;

    pub fn harden() -> io::Result<()> {
        // SAFETY: PR_SET_DUMPABLE reads only its integer argument and touches no memory.
        match unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn credentials(socket: RawFd) -> io::Result<libc::ucred> {
        let mut peer = MaybeUninit::<libc::ucred>::uninit();
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `peer` and `length` are valid for writes for the call, and `length` holds
        // the size of `peer`.
        let result = unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                peer.as_mut_ptr().cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getsockopt succeeded, so it wrote a whole `ucred`.
        Ok(unsafe { peer.assume_init() })
    }

    pub fn peer_uid(socket: RawFd) -> io::Result<u32> {
        Ok(credentials(socket)?.uid)
    }

    /// A descriptor that pins the peer process, from kernels with `SO_PEERPIDFD` (6.5 on).
    fn peer_pidfd(socket: RawFd) -> io::Result<OwnedFd> {
        let mut pidfd: libc::c_int = -1;
        let mut length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `pidfd` and `length` are valid for writes for the call, and `length` holds
        // the size of `pidfd`.
        let result = unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&raw mut pidfd).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getsockopt succeeded, so `pidfd` is open and owned only by this function.
        Ok(unsafe { OwnedFd::from_raw_fd(pidfd) })
    }

    /// Whether the process a pidfd pins has exited, which makes the pidfd readable.
    fn exited(pidfd: &OwnedFd) -> io::Result<bool> {
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` is one valid pollfd for the call, which does not wait.
        match unsafe { libc::poll(&raw mut poll, 1, 0) } {
            -1 => Err(io::Error::last_os_error()),
            ready => Ok(ready > 0),
        }
    }

    /// The terminal and session `/proc` gives for `pid`, or nothing if it has none or has gone.
    fn terminal_of(pid: libc::pid_t) -> Option<Terminal> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may hold spaces and brackets, so the fields start after the last ')'.
        let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
        let session = fields.nth(3)?.parse().ok()?;
        let device = fields.next()?.parse().ok()?;
        (device != 0 && session != 0).then_some(Terminal { device, session })
    }

    pub fn peer_terminal(socket: RawFd) -> io::Result<Option<Terminal>> {
        let pid = credentials(socket)?.pid;
        let pidfd = match peer_pidfd(socket) {
            Ok(pidfd) => Some(pidfd),
            // An older kernel: the lookup goes unguarded, as ADR-0020 allows.
            Err(error) if error.raw_os_error() == Some(libc::ENOPROTOOPT) => None,
            // The peer has gone.
            Err(_) => return Ok(None),
        };
        let found = terminal_of(pid);
        match pidfd {
            Some(pidfd) if exited(&pidfd)? => Ok(None),
            _ => Ok(found),
        }
    }

    pub fn footprint(pid: u32) -> io::Result<u64> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))?;
        text.lines()
            .find_map(|line| line.strip_prefix("Pss:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kilobytes| kilobytes.parse::<u64>().ok())
            .map(|kilobytes| kilobytes * 1024)
            .ok_or_else(|| io::Error::other("no Pss line in smaps_rollup"))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::RawFd;
    use std::ptr;

    use crate::Terminal;

    pub fn harden() -> io::Result<()> {
        // SAFETY: PT_DENY_ATTACH takes no address or data and touches no memory.
        match unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, ptr::null_mut(), 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn probe_attach(pid: u32) -> io::Result<()> {
        let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
        // SAFETY: SIG_IGN is a valid disposition, and the old one goes straight back below.
        let previous = unsafe { libc::signal(libc::SIGSEGV, libc::SIG_IGN) };
        // SAFETY: PT_ATTACH takes no address or data and touches no memory.
        let attached = unsafe { libc::ptrace(libc::PT_ATTACH, pid, ptr::null_mut(), 0) };
        let error = io::Error::last_os_error();
        // SAFETY: `previous` came from signal for this signal.
        unsafe { libc::signal(libc::SIGSEGV, previous) };
        if attached != 0 {
            return Err(error);
        }
        let mut status = 0;
        // SAFETY: `status` is valid for writes for the call.
        unsafe { libc::waitpid(pid, &raw mut status, libc::WUNTRACED) };
        // SAFETY: the kernel reads PT_DETACH's address as a value: 1 means carry on from here.
        match unsafe { libc::ptrace(libc::PT_DETACH, pid, ptr::without_provenance_mut(1), 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn peer_uid(socket: RawFd) -> io::Result<u32> {
        let (mut user, mut group) = (0, 0);
        // SAFETY: `user` and `group` are valid for writes for the call.
        match unsafe { libc::getpeereid(socket, &raw mut user, &raw mut group) } {
            0 => Ok(user),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn peer_terminal(socket: RawFd) -> io::Result<Option<Terminal>> {
        let mut pid: libc::pid_t = 0;
        let mut length = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid` and `length` are valid for writes for the call, and `length` holds the
        // size of `pid`.
        let result = unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is writable for `size` bytes, the struct PROC_PIDTBSDINFO selects.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        // SAFETY: getsid takes an integer and touches no memory.
        let session = unsafe { libc::getsid(pid) };
        if read != size {
            return Ok(None);
        }
        // SAFETY: all-zero bytes are valid for this integer struct, and the call filled it.
        let device = unsafe { info.assume_init() }.e_tdev;
        let Ok(session) = u32::try_from(session) else {
            return Ok(None);
        };
        // NODEV, all ones, is what a process with no terminal has.
        let none = device == 0 || device == u32::MAX || session == 0;
        Ok((!none).then_some(Terminal { device, session }))
    }

    pub fn footprint(pid: u32) -> io::Result<u64> {
        let pid = libc::c_int::try_from(pid).map_err(io::Error::other)?;
        let mut info = MaybeUninit::<libc::rusage_info_v4>::zeroed();
        let buffer = info.as_mut_ptr().cast();
        // SAFETY: `buffer` is a writable rusage_info_v4, the struct RUSAGE_INFO_V4 selects.
        let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, buffer) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, and all-zero bytes are valid for this integer struct.
        Ok(unsafe { info.assume_init() }.ri_phys_footprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_time_is_a_real_time_of_day() {
        let time = local_time(1_000_000_000).unwrap();
        assert_eq!(time.second, 40);
        assert!(time.weekday < 7 && time.month < 12 && time.hour < 24 && time.minute < 60);
        assert!((1..=31).contains(&time.day));
        let later = local_time(1_000_000_060).unwrap();
        assert_eq!((later.minute + 60 - time.minute) % 60, 1);
    }

    #[test]
    fn a_detached_child_leads_its_own_session() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let mut child = spawn_detached(command, Stdio::null()).unwrap();
        let pid = child.id() as libc::pid_t;
        // The session is set before exec, so it holds from the moment spawn returns.
        // SAFETY: getsid takes an integer and touches no memory.
        let session = unsafe { libc::getsid(pid) };
        // SAFETY: getsid takes an integer and touches no memory.
        let ours = unsafe { libc::getsid(0) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(session, pid);
        assert_ne!(session, ours);
    }

    #[test]
    fn a_log_is_private_and_starts_empty_and_a_symlink_is_refused() {
        use std::fs;
        use std::io::Write;
        use std::os::unix::fs::{MetadataExt, symlink};

        let path = std::env::temp_dir().join(format!("pinfold-log-test-{}", std::process::id()));
        let link = path.with_extension("link");
        let (_, _) = (fs::remove_file(&path), fs::remove_file(&link));
        let mut log = open_log(&path).unwrap();
        log.write_all(b"old").unwrap();
        assert_eq!(log.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(open_log(&path).unwrap().metadata().unwrap().len(), 0);

        fs::write(&path, b"keep").unwrap();
        symlink(&path, &link).unwrap();
        assert_eq!(
            open_log(&link).unwrap_err().raw_os_error(),
            Some(libc::ELOOP)
        );
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        let (_, _) = (fs::remove_file(&path), fs::remove_file(&link));
    }

    #[test]
    fn a_time_past_any_year_is_nothing() {
        assert_eq!(local_time(i64::MAX), None);
    }
}
