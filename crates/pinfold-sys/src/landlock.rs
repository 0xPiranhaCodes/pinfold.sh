//! Landlock for a sandboxed agent on Linux (ADR-0038): from ABI 6 (Linux 6.12) it cannot
//! connect to an abstract Unix socket bound outside its sandbox, such as X11's
//! `@/tmp/.X11-unix/X0` or a session bus on `unix:abstract=`. Seccomp cannot see the address
//! passed to `connect`, and bubblewrap's `--unshare-net` would also take the network away.

use std::io;
use std::process::Command;

/// The first Landlock ABI that scopes abstract Unix sockets.
pub const SCOPED: u32 = 6;

/// Returns the kernel's Landlock ABI version: 0 if it has no Landlock or it is turned off, and
/// always 0 off Linux.
///
/// # Errors
///
/// Returns an error if the kernel answers the probe with any other failure, so a launch that
/// needs the answer is refused.
///
/// # Examples
///
/// ```
/// let abi = pinfold_sys::landlock::abi()?;
/// if cfg!(not(target_os = "linux")) {
///     assert_eq!(abi, 0);
/// }
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn abi() -> io::Result<u32> {
    #[cfg(target_os = "linux")]
    {
        linux::abi()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(0)
    }
}

/// Makes the program `command` starts, and everything it starts, unable to connect to an
/// abstract Unix socket bound outside it. The child sets `no_new_privs` first, as Landlock
/// needs, so a setuid program it runs gains nothing. Pathname sockets, such as the API's, and
/// socket pairs are not affected.
///
/// # Errors
///
/// Returns an error if the kernel cannot make the rules, which needs ABI [`SCOPED`], or this is
/// not Linux. The child fails to start, and spawning it returns the error, if they cannot be
/// applied there.
///
/// # Examples
///
/// ```
/// use std::process::Command;
///
/// let mut command = Command::new("/usr/bin/true");
/// if pinfold_sys::landlock::abi()? >= pinfold_sys::landlock::SCOPED {
///     pinfold_sys::landlock::scope_abstract_sockets(&mut command)?;
/// }
/// assert!(command.status()?.success());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn scope_abstract_sockets(command: &mut Command) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::scope_abstract_sockets(command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = command;
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use libc::{SYS_landlock_create_ruleset, SYS_landlock_restrict_self, c_long};

    // From linux/landlock.h.
    const CREATE_RULESET_VERSION: c_long = 1;
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1;

    /// `struct landlock_ruleset_attr` as of ABI 6.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    pub(super) fn abi() -> io::Result<u32> {
        let (attr, size) = (std::ptr::null::<RulesetAttr>(), 0usize);
        // SAFETY: with a null attribute, a size of 0 and the version flag, the kernel reads no
        // memory and returns the ABI version or -1.
        let result = unsafe {
            libc::syscall(
                SYS_landlock_create_ruleset,
                attr,
                size,
                CREATE_RULESET_VERSION,
            )
        };
        version(result, io::Error::last_os_error())
    }

    /// The probe's answer, with `error` read straight after it. `ENOSYS` means the kernel has no
    /// Landlock, and `EOPNOTSUPP` that it was turned off at boot.
    fn version(result: c_long, error: io::Error) -> io::Result<u32> {
        if result == -1 {
            return match error.raw_os_error() {
                Some(libc::ENOSYS | libc::EOPNOTSUPP) => Ok(0),
                _ => Err(error),
            };
        }
        u32::try_from(result).map_err(|_| io::Error::other("Landlock gave no ABI version"))
    }

    pub(super) fn scope_abstract_sockets(command: &mut Command) -> io::Result<()> {
        let attr = RulesetAttr {
            handled_access_fs: 0,
            handled_access_net: 0,
            scoped: SCOPE_ABSTRACT_UNIX_SOCKET,
        };
        let size = size_of::<RulesetAttr>();
        // SAFETY: `attr` is a live `landlock_ruleset_attr` of `size` bytes, which the kernel only
        // reads during the call.
        let fd = unsafe { libc::syscall(SYS_landlock_create_ruleset, &raw const attr, size, 0) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        let fd = i32::try_from(fd).map_err(|_| io::Error::other("Landlock gave no ruleset"))?;
        // SAFETY: the kernel just returned `fd`, open and close-on-exec, and nothing else owns it.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: prctl and landlock_restrict_self are async-signal-safe system calls, and
        // `ruleset` is open until the closure, and so `command`, is dropped.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let fd = c_long::from(ruleset.as_raw_fd());
                match libc::syscall(SYS_landlock_restrict_self, fd, 0) {
                    -1 => Err(io::Error::last_os_error()),
                    _ => Ok(()),
                }
            });
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_probe_reads_a_version_or_no_landlock() {
            let errno = io::Error::from_raw_os_error;
            assert_eq!(version(6, errno(0)).unwrap(), 6);
            assert_eq!(version(1, errno(0)).unwrap(), 1);
            assert_eq!(version(-1, errno(libc::ENOSYS)).unwrap(), 0);
            assert_eq!(version(-1, errno(libc::EOPNOTSUPP)).unwrap(), 0);
            // Anything else, such as a filter refusing the call, is not a missing Landlock.
            let refused = version(-1, errno(libc::EPERM)).unwrap_err();
            assert_eq!(refused.raw_os_error(), Some(libc::EPERM));
            assert!(version(-2, errno(0)).is_err());
            assert!(version(c_long::from(u32::MAX) + 1, errno(0)).is_err());
        }

        #[test]
        fn a_scoped_child_still_runs() {
            if abi().unwrap() < super::super::SCOPED {
                return;
            }
            let mut command = Command::new("/usr/bin/true");
            scope_abstract_sockets(&mut command).unwrap();
            assert!(command.status().unwrap().success());
        }
    }
}
