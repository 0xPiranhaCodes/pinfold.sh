//! What a terminal's foreground process is, for protected panes (threat model, Authorization).

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd};

/// The foreground process of a terminal, and how the terminal takes input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Foreground {
    /// The process name the system keeps, which may be cut to 15 or 16 bytes.
    pub name: String,
    /// The command line, or nothing if this user may not read it. Linux keeps the first 64 KiB,
    /// and macOS gives nothing for a longer one.
    pub argv: Option<Vec<String>>,
    /// The user the process acts as.
    pub euid: u32,
    /// Echo is off while input still comes in lines: how a password prompt reads. Line editors
    /// and full-screen programs turn echo off too, but take keys one at a time.
    pub echo_off: bool,
}

/// Returns the foreground process group leader of the terminal whose master is `master`, or
/// nothing if the terminal has no foreground group.
///
/// # Errors
///
/// Returns an error if the process has gone or cannot be read.
///
/// # Examples
///
/// ```
/// use std::os::fd::AsFd;
/// use std::process::Command;
///
/// let mut command = Command::new("/bin/sh");
/// command.args(["-c", "sleep 5"]);
/// let pty = pinfold_sys::pty::Pty::spawn(command, 24, 80)?;
/// let found = pinfold_sys::foreground::foreground(pty.master().as_fd())?;
/// assert!(found.is_none_or(|found| !found.name.is_empty()));
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn foreground(master: BorrowedFd<'_>) -> io::Result<Option<Foreground>> {
    let fd = master.as_raw_fd();
    // SAFETY: tcgetpgrp takes an integer and touches no memory.
    let group = unsafe { libc::tcgetpgrp(fd) };
    if group <= 0 {
        return Ok(None);
    }
    let mut mode = MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `mode` is valid for writes of a whole termios for the call.
    if unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr succeeded, and all-zero bytes are valid for this integer struct.
    let flags = unsafe { mode.assume_init() }.c_lflag;
    let echo_off = flags & libc::ECHO == 0 && flags & libc::ICANON != 0;
    let (name, euid) = platform::name_and_euid(group)?;
    Ok(Some(Foreground {
        name,
        argv: platform::argv(group),
        euid,
        echo_off,
    }))
}

/// Most bytes of a command line read.
const MAX_ARGS: usize = 64 * 1024;

#[cfg(target_os = "linux")]
mod platform {
    use std::io::{self, Read};

    pub fn name_and_euid(pid: libc::pid_t) -> io::Result<(String, u32)> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let field = |key: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .map(str::trim)
        };
        let name = field("Name:").unwrap_or_default().to_owned();
        // Real, effective, saved and filesystem ids, in that order.
        let euid = field("Uid:")
            .and_then(|ids| ids.split_whitespace().nth(1))
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| io::Error::other("no Uid line in status"))?;
        Ok((name, euid))
    }

    pub fn argv(pid: libc::pid_t) -> Option<Vec<String>> {
        let file = std::fs::File::open(format!("/proc/{pid}/cmdline")).ok()?;
        let mut bytes = Vec::new();
        file.take(super::MAX_ARGS as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        let args = bytes.split(|&byte| byte == 0).filter(|arg| !arg.is_empty());
        Some(
            args.map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect(),
        )
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::mem::MaybeUninit;

    pub fn name_and_euid(pid: libc::pid_t) -> io::Result<(String, u32)> {
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
            return Err(io::Error::last_os_error());
        }
        // SAFETY: all-zero bytes are valid for this integer struct, and the call filled it.
        let info = unsafe { info.assume_init() };
        let comm = info.pbi_comm.map(|c| c as u8);
        let end = comm
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(comm.len());
        Ok((
            String::from_utf8_lossy(&comm[..end]).into_owned(),
            info.pbi_uid,
        ))
    }

    /// The arguments from `KERN_PROCARGS2`: a count, the executable's path, padding, then
    /// the arguments and the environment, which is never read.
    pub fn argv(pid: libc::pid_t) -> Option<Vec<String>> {
        let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut buffer = vec![0u8; super::MAX_ARGS];
        let mut length = buffer.len();
        // SAFETY: `name` holds three integers, and `buffer` is writable for `length` bytes.
        let result = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return None;
        }
        let bytes = buffer.get(..length)?;
        let count = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
        let mut parts = bytes[4..]
            .split(|&byte| byte == 0)
            .filter(|part| !part.is_empty());
        parts.next()?;
        let args = parts.take(usize::try_from(count).ok()?);
        Some(
            args.map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::Pty;
    use std::os::fd::AsFd;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn found_when(script: &str, done: impl Fn(&Foreground) -> bool) -> Foreground {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        let pty = Pty::spawn(command, 24, 80).unwrap();
        let start = Instant::now();
        loop {
            let found = foreground(pty.master().as_fd()).unwrap();
            match found {
                Some(found) if done(&found) => return found,
                found => assert!(start.elapsed() < Duration::from_secs(5), "{found:?}"),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_foreground_process_has_its_name_arguments_and_user() {
        let found = found_when("exec sleep 30", |found| found.name == "sleep");
        assert_eq!(found.argv, Some(vec!["sleep".to_owned(), "30".to_owned()]));
        assert_eq!(found.euid, crate::effective_uid());
        assert!(!found.echo_off);
    }

    #[test]
    fn a_password_style_read_has_echo_off() {
        found_when("stty -echo; exec sleep 30", |found| {
            found.echo_off && found.name == "sleep"
        });
        let raw = found_when("stty -echo -icanon; exec sleep 30", |found| {
            found.name == "sleep"
        });
        assert!(!raw.echo_off);
    }
}
