//! FFI and libc calls: the only crate allowed unsafe code.

#![allow(unsafe_code, reason = "this crate is the FFI boundary")]

use std::io;
use std::mem::MaybeUninit;

#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod mapped;
#[cfg(all(target_os = "macos", feature = "metal"))]
pub mod metal;
pub mod pty;

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
    fn a_time_past_any_year_is_nothing() {
        assert_eq!(local_time(i64::MAX), None);
    }
}
