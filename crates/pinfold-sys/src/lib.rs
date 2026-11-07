//! FFI and libc calls: the only crate allowed unsafe code.

#![allow(unsafe_code, reason = "this crate is the FFI boundary")]

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod foreground;
pub mod landlock;
pub mod mapped;
#[cfg(all(target_os = "macos", feature = "menu"))]
pub mod menu;
#[cfg(all(target_os = "macos", feature = "metal"))]
pub mod metal;
#[cfg(all(target_os = "macos", feature = "menu"))]
pub mod notify;
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

/// Sends SIGTERM to the process at the other end of a connected Unix socket, found as
/// [`peer_terminal`] finds it. Where Linux has `SO_PEERPIDFD` the signal goes through that
/// descriptor, so a reused pid cannot take it. Elsewhere the caller keeps the socket open, as
/// that keeps a peer that holds its end from going.
///
/// # Errors
///
/// Returns an error if `socket` is not a connected Unix socket or the signal cannot be sent.
///
/// # Examples
///
/// ```no_run
/// use std::os::fd::AsFd;
/// use std::os::unix::net::UnixStream;
///
/// let core = UnixStream::connect("/tmp/pinfold-501/default.sock")?;
/// pinfold_sys::terminate_peer(core.as_fd())?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn terminate_peer(socket: BorrowedFd<'_>) -> io::Result<()> {
    platform::signal_peer(socket.as_raw_fd(), libc::SIGTERM)
}

/// Sends SIGKILL to the process at the other end of a connected Unix socket, as
/// [`terminate_peer`] sends SIGTERM: for a peer too hung to handle that, such as a core whose
/// program was on a disk image that was ejected under it.
///
/// # Errors
///
/// Returns an error if `socket` is not a connected Unix socket or the signal cannot be sent.
///
/// # Examples
///
/// ```no_run
/// use std::os::fd::AsFd;
/// use std::os::unix::net::UnixStream;
///
/// let core = UnixStream::connect("/tmp/pinfold-501/default.sock")?;
/// pinfold_sys::kill_peer(core.as_fd())?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn kill_peer(socket: BorrowedFd<'_>) -> io::Result<()> {
    platform::signal_peer(socket.as_raw_fd(), libc::SIGKILL)
}

/// Returns the path of the program running at the other end of a connected Unix socket, found
/// as [`peer_terminal`] finds the process. On macOS a program whose file has gone, as when its
/// disk image was ejected, gives an error. On Linux it gives the path with ` (deleted)` added.
///
/// # Errors
///
/// Returns an error if `socket` is not a connected Unix socket or the path cannot be read.
///
/// # Examples
///
/// ```
/// use std::os::fd::AsFd;
/// use std::os::unix::net::{UnixListener, UnixStream};
///
/// let path = std::env::temp_dir().join(format!("pinfold-exe-{}", std::process::id()));
/// let listener = UnixListener::bind(&path)?;
/// let ours = UnixStream::connect(&path)?;
/// let program = pinfold_sys::peer_executable(ours.as_fd())?;
/// assert_eq!(program.canonicalize()?, std::env::current_exe()?.canonicalize()?);
/// # drop(listener);
/// std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn peer_executable(socket: BorrowedFd<'_>) -> io::Result<PathBuf> {
    platform::peer_executable(socket.as_raw_fd())
}

/// Returns the kernel's name for the process at the other end of a connected Unix socket: its
/// program's file name, cut to 15 bytes, which stays known after the file has gone.
///
/// # Errors
///
/// Returns an error if `socket` is not a connected Unix socket or the peer has gone.
///
/// # Examples
///
/// ```
/// use std::os::fd::AsFd;
/// use std::os::unix::net::{UnixListener, UnixStream};
///
/// let path = std::env::temp_dir().join(format!("pinfold-name-{}", std::process::id()));
/// let listener = UnixListener::bind(&path)?;
/// let ours = UnixStream::connect(&path)?;
/// let program = std::env::current_exe()?;
/// let name = program.file_name().unwrap().to_string_lossy();
/// assert!(name.starts_with(&pinfold_sys::peer_name(ours.as_fd())?));
/// # drop(listener);
/// std::fs::remove_file(&path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn peer_name(socket: BorrowedFd<'_>) -> io::Result<String> {
    platform::peer_name(socket.as_raw_fd())
}

/// Whether `path` is on a file system mounted read-only, as a disk image and macOS's App
/// Translocation mount of a quarantined app both are.
///
/// # Errors
///
/// Returns an error if `path` does not exist or holds a NUL byte.
///
/// # Examples
///
/// ```
/// assert!(!pinfold_sys::read_only(&std::env::temp_dir())?);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn read_only(path: &Path) -> io::Result<bool> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let mut found = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and `found` is valid for writes of a statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), found.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs succeeded, so it wrote a whole statvfs.
    let found = unsafe { found.assume_init() };
    Ok(found.f_flag & libc::ST_RDONLY != 0)
}

/// `pid` if it names one process: kill takes 0 and below for groups or every process.
fn one_process(pid: libc::pid_t) -> io::Result<libc::pid_t> {
    if pid > 0 {
        Ok(pid)
    } else {
        Err(io::Error::from(io::ErrorKind::NotFound))
    }
}

/// Highest descriptor marked in a child where the system has no call to mark them all, which
/// bounds the work there. [`mark_strays`] keeps the open-file limit at or below it, so no
/// descriptor can be numbered past it. Launched from the Dock the limit is 256, well below.
const MAX_CHECKED_FD: libc::rlim_t = 10_240;

/// The soft open-file limit before [`mark_strays`] lowered it, for children to get back, or 0.
static SOFT_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Set in a child once its strays are marked. Only children write it, each in its own copy of
/// memory, so it reads false in every parent.
static STRAYS_MARKED: AtomicBool = AtomicBool::new(false);

/// Makes every descriptor past stderr close-on-exec in the child `command` starts, so it gets
/// only those handed to it on purpose after this, or by [`inherit`]. Another thread can make a
/// descriptor that is inheritable for a moment, such as a connection accepted on macOS, and a
/// fork in that moment copies it. Every function here that starts a child calls this, and so
/// must any other code in the core that starts one.
///
/// The child marks descriptors only up to a bound, so this also keeps this process's soft
/// open-file limit at or below it, and marks close-on-exec any descriptor already open past it.
/// The child gets the limit back as it was.
///
/// # Examples
///
/// ```
/// use std::process::Command;
///
/// let mut command = Command::new("/usr/bin/true");
/// pinfold_sys::mark_strays(&mut command);
/// assert!(command.status()?.success());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn mark_strays(command: &mut Command) {
    let restore = cap_open_files();
    // SAFETY: sysconf reads a limit and touches no memory.
    let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) }.clamp(0, MAX_CHECKED_FD as _) as RawFd;
    // SAFETY: the closure makes only system calls that allocate nothing (fcntl, close_range and
    // setrlimit), on values it owns, and writes only an atomic in the child's own memory.
    unsafe {
        command.pre_exec(move || {
            // Once, so a later closure cannot undo what an earlier one handed over.
            if STRAYS_MARKED.swap(true, Ordering::Relaxed) {
                return Ok(());
            }
            if let Some(restore) = restore {
                // A program that cannot have the limit back still starts, with the bound. First,
                // as close_range returns early, and nothing here opens a descriptor.
                // SAFETY: setrlimit is a system call stub that reads only `restore`.
                libc::setrlimit(libc::RLIMIT_NOFILE, &restore);
            }
            #[cfg(target_os = "linux")]
            {
                // SAFETY: close_range takes integers and touches no memory. Kernels before
                // 5.11 refuse the flag, and the loop below does the same work.
                let all = libc::syscall(
                    libc::SYS_close_range,
                    3,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                );
                if all == 0 {
                    return Ok(());
                }
            }
            for fd in 3..limit {
                // Marked, not closed: the standard library's exec pipe is among them. A number
                // that is not open only fails.
                // SAFETY: F_SETFD takes an integer flag and touches no memory.
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
}

/// Lowers this process's soft open-file limit to [`MAX_CHECKED_FD`] if it is above, so no new
/// descriptor can be numbered past what a child marks, then marks those already past it. Returns
/// the limit a child should get back, if it was lowered at any time.
fn cap_open_files() -> Option<libc::rlimit> {
    let mut now = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only `now`.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut now) } != 0 {
        return None;
    }
    if now.rlim_cur > MAX_CHECKED_FD {
        let capped = libc::rlimit {
            rlim_cur: MAX_CHECKED_FD,
            rlim_max: now.rlim_max,
        };
        // SAFETY: setrlimit reads only `capped`. Lowering a soft limit is always allowed.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &capped) } == 0 {
            SOFT_LIMIT.store(now.rlim_cur, Ordering::Relaxed);
            // Lowered first, so nothing can open past the bound while this walks.
            for entry in std::fs::read_dir("/dev/fd").into_iter().flatten().flatten() {
                match entry.file_name().to_str().map(str::parse::<RawFd>) {
                    Some(Ok(fd)) if fd as libc::rlim_t >= MAX_CHECKED_FD => {
                        // SAFETY: F_SETFD takes an integer flag and touches no memory.
                        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                    }
                    _ => {}
                }
            }
        }
    }
    match SOFT_LIMIT.load(Ordering::Relaxed) {
        0 => None,
        soft => Some(libc::rlimit {
            rlim_cur: soft,
            rlim_max: now.rlim_max,
        }),
    }
}

/// Starts `command` in a session of its own, with nothing on stdin or stdout and `stderr` as its
/// stderr, so it outlives the caller and the caller's terminal. It gets no other descriptor
/// past stderr.
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
    mark_strays(&mut command);
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
    // First, so a spawn here that marks strays later leaves this one alone.
    mark_strays(command);
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

    pub fn peer_executable(socket: RawFd) -> io::Result<std::path::PathBuf> {
        std::fs::read_link(format!("/proc/{}/exe", credentials(socket)?.pid))
    }

    pub fn peer_name(socket: RawFd) -> io::Result<String> {
        let name = std::fs::read_to_string(format!("/proc/{}/comm", credentials(socket)?.pid))?;
        Ok(name.trim_end_matches('\n').to_owned())
    }

    pub fn signal_peer(socket: RawFd, signal: libc::c_int) -> io::Result<()> {
        let sent = match peer_pidfd(socket) {
            // SAFETY: pidfd_send_signal takes integers and a null siginfo, and touches no memory.
            Ok(pidfd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            },
            // An older kernel: the pid alone, as ADR-0020 allows.
            Err(error) if error.raw_os_error() == Some(libc::ENOPROTOOPT) => {
                let pid = crate::one_process(credentials(socket)?.pid)?;
                // SAFETY: kill takes integers and touches no memory.
                libc::c_long::from(unsafe { libc::kill(pid, signal) })
            }
            Err(error) => return Err(error),
        };
        match sent {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
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

    fn peer_pid(socket: RawFd) -> io::Result<libc::pid_t> {
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
        match result {
            0 => Ok(pid),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn signal_peer(socket: RawFd, signal: libc::c_int) -> io::Result<()> {
        let pid = crate::one_process(peer_pid(socket)?)?;
        // SAFETY: kill takes integers and touches no memory.
        match unsafe { libc::kill(pid, signal) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn peer_executable(socket: RawFd) -> io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;
        let pid = peer_pid(socket)?;
        let mut path = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let size = u32::try_from(path.len()).map_err(io::Error::other)?;
        // SAFETY: `path` is writable for `size` bytes.
        let length = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), size) };
        let length = usize::try_from(length).map_err(io::Error::other)?;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        path.truncate(length);
        Ok(std::ffi::OsString::from_vec(path).into())
    }

    pub fn peer_name(socket: RawFd) -> io::Result<String> {
        let pid = peer_pid(socket)?;
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
        if read != size {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        // SAFETY: all-zero bytes are valid for this integer struct, and the call filled it.
        let name = unsafe { info.assume_init() }.pbi_comm;
        let bytes: Vec<u8> = name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn peer_terminal(socket: RawFd) -> io::Result<Option<Terminal>> {
        let pid = peer_pid(socket)?;
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
    #[cfg(target_os = "macos")]
    fn the_sealed_system_volume_is_read_only() {
        assert!(read_only(Path::new("/System")).unwrap());
        assert!(read_only(Path::new("/no/such/path")).is_err());
    }

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
