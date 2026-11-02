//! FFI and libc calls: the only crate allowed unsafe code.

#![allow(unsafe_code, reason = "this crate is the FFI boundary")]

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod mapped;
#[cfg(all(target_os = "macos", feature = "metal"))]
pub mod metal;
pub mod pty;
pub mod socket;

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

/// Returns the user this process acts as.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory.
    unsafe { libc::geteuid() }
}

/// Stops a same-user process from copying a PTY master out of this one (ADR-0020). Call this
/// before opening any PTY. It lasts for the process and ends at `exec`.
///
/// On macOS a process with a debugger attached dies here, and on Linux this also stops other
/// processes reading its `/proc` entries, which is why `cargo xtask mem-gate` measures a core
/// that has not called it.
///
/// # Errors
///
/// Returns an error if the system refuses. A core must not go on without this.
pub fn harden() -> io::Result<()> {
    platform::harden()
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

/// Starts `command` in a session of its own, with nothing on stdin, stdout or stderr, so it
/// outlives the caller and the caller's terminal.
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
/// use std::process::Command;
///
/// let mut child = pinfold_sys::spawn_detached(Command::new("/usr/bin/true"))?;
/// assert!(child.wait()?.success());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn spawn_detached(mut command: Command) -> io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, and the closure touches nothing else.
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    command.spawn()
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
    use std::os::fd::RawFd;

    pub fn harden() -> io::Result<()> {
        // SAFETY: PR_SET_DUMPABLE reads only its integer argument and touches no memory.
        match unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn peer_uid(socket: RawFd) -> io::Result<u32> {
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
        Ok(unsafe { peer.assume_init() }.uid)
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

    pub fn harden() -> io::Result<()> {
        // SAFETY: PT_DENY_ATTACH takes no address or data and touches no memory.
        match unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, ptr::null_mut(), 0) } {
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
        let mut child = spawn_detached(command).unwrap();
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
    fn a_time_past_any_year_is_nothing() {
        assert_eq!(local_time(i64::MAX), None);
    }
}
