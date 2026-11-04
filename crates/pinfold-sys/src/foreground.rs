//! What a terminal's foreground processes are, for protected panes (threat model, Authorization).

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::PathBuf;

/// Most processes read from one foreground group. A larger group is an error.
pub const MAX_GROUP: usize = 256;

/// Most bytes of a command line read.
const MAX_ARGS: usize = 64 * 1024;

/// A terminal's foreground process group, and how the terminal takes input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Foreground {
    /// The group's id, which is its leader's pid.
    pub group: u32,
    /// Every live process in the group, at most [`MAX_GROUP`].
    pub members: Vec<Process>,
    /// Echo is off while input still comes in lines: how a password prompt reads. Line editors
    /// and full-screen programs turn echo off too, but take keys one at a time.
    pub echo_off: bool,
}

/// One process of a foreground group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    /// The process name the system keeps, which may be cut to 15 or 16 bytes.
    pub name: String,
    /// The command line, or nothing if this user may not read it. Linux keeps the first 64 KiB,
    /// and macOS gives nothing for a longer one.
    pub argv: Option<Vec<String>>,
    /// The user the process acts as.
    pub euid: u32,
}

/// Returns the foreground process group of the terminal whose master is `master`, with every
/// process in it.
///
/// # Errors
///
/// Returns an error if the terminal has no foreground group, the group has more than
/// [`MAX_GROUP`] processes, or one of them cannot be read. One that exits meanwhile is left
/// out, so a group whose last process just ended, before the shell takes the terminal back,
/// has none.
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
/// assert!(!found.members.is_empty());
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn foreground(master: BorrowedFd<'_>) -> io::Result<Foreground> {
    let fd = master.as_raw_fd();
    // SAFETY: tcgetpgrp takes an integer and touches no memory.
    let group = match unsafe { libc::tcgetpgrp(fd) } {
        -1 => return Err(io::Error::last_os_error()),
        group => u32::try_from(group)
            .ok()
            .filter(|&group| group > 0)
            .ok_or_else(|| io::Error::other("the terminal has no foreground group"))?,
    };
    let mut mode = MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `mode` is valid for writes of a whole termios for the call.
    if unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr succeeded, and all-zero bytes are valid for this integer struct.
    let flags = unsafe { mode.assume_init() }.c_lflag;
    let echo_off = flags & libc::ECHO == 0 && flags & libc::ICANON != 0;
    let pids = with_leader(
        group as libc::pid_t,
        platform::members(group as libc::pid_t)?,
    )?;
    if pids.len() > MAX_GROUP {
        return Err(io::Error::other("the foreground group is too large"));
    }
    let mut members = Vec::with_capacity(pids.len());
    for pid in pids {
        match process(pid) {
            Ok(member) => members.push(member),
            // It exited, or is a zombie on macOS, since it was listed, so it reads nothing.
            Err(error) if exited(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Foreground {
        group,
        members,
        echo_off,
    })
}

/// Returns the working directory of process `pid`, as the system keeps it.
///
/// # Errors
///
/// Returns an error if the process has exited or this user may not read it.
///
/// # Examples
///
/// ```
/// let cwd = pinfold_sys::foreground::cwd(std::process::id())?;
/// assert_eq!(cwd, std::env::current_dir()?.canonicalize()?);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn cwd(pid: u32) -> io::Result<PathBuf> {
    let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
    platform::cwd(pid)
}

fn process(pid: libc::pid_t) -> io::Result<Process> {
    // macOS refuses the arguments of a process while it starts or ends a program, for a
    // moment, so a few tries 1 ms apart tell that from one this user may not read.
    let mut argv = platform::argv(pid);
    for _ in 0..ARGV_TRIES {
        if argv.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        argv = platform::argv(pid);
    }
    // Read after the arguments, so both describe the same program.
    let (name, euid) = platform::name_and_euid(pid)?;
    Ok(Process { name, argv, euid })
}

/// How many more times a command line that could not be read is tried.
const ARGV_TRIES: usize = 4;

/// Whether an error says the process has exited, so it reads nothing.
fn exited(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ESRCH) || error.kind() == io::ErrorKind::NotFound
}

/// Refuses a group that still has processes but not its leader, which a scan that missed one
/// would also look like. A group with none left is one the shell is about to take back.
fn with_leader(group: libc::pid_t, pids: Vec<libc::pid_t>) -> io::Result<Vec<libc::pid_t>> {
    if pids.is_empty() || pids.contains(&group) {
        Ok(pids)
    } else {
        Err(io::Error::other(
            "the foreground group's leader was not found",
        ))
    }
}

/// The pids in `group`, from each process's `/proc/<pid>/stat`, one past the cap at most. A
/// process that exited is skipped. Any other failure, or a stat that does not parse, is an
/// error, as that process could be in the group.
#[cfg(any(target_os = "linux", test))]
fn scan(
    group: libc::pid_t,
    stats: impl Iterator<Item = (libc::pid_t, io::Result<Vec<u8>>)>,
) -> io::Result<Vec<libc::pid_t>> {
    let mut found = Vec::new();
    for (pid, stat) in stats {
        let stat = match stat {
            Ok(stat) => stat,
            Err(error) if exited(&error) => continue,
            Err(error) => return Err(error),
        };
        // The name may hold spaces and brackets, so the fields start after the last ')'.
        let rest = stat
            .rsplit(|&byte| byte == b')')
            .next()
            .filter(|_| stat.contains(&b')'));
        let pgrp = rest.and_then(|rest| {
            let rest = String::from_utf8_lossy(rest);
            rest.split_whitespace().nth(2)?.parse::<libc::pid_t>().ok()
        });
        match pgrp {
            Some(pgrp) if pgrp == group => found.push(pid),
            Some(_) => {}
            None => return Err(io::Error::other("a process stat did not parse")),
        }
        if found.len() > MAX_GROUP {
            break;
        }
    }
    Ok(found)
}

fn lossy(args: impl Iterator<Item = impl AsRef<[u8]>>) -> Vec<String> {
    args.map(|arg| String::from_utf8_lossy(arg.as_ref()).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io::{self, Read};

    /// Every pid whose `/proc` stat names `group`, stopping one past the cap.
    pub fn members(group: libc::pid_t) -> io::Result<Vec<libc::pid_t>> {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            if let Some(pid) = entry?.file_name().to_str().and_then(|n| n.parse().ok()) {
                pids.push(pid);
            }
        }
        let stat = |pid| (pid, std::fs::read(format!("/proc/{pid}/stat")));
        super::scan(group, pids.into_iter().map(stat))
    }

    pub fn cwd(pid: libc::pid_t) -> io::Result<std::path::PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
    }

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
        Some(super::lossy(args))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::io;
    use std::mem::MaybeUninit;

    /// The pids in `group`, one past the cap at most.
    pub fn members(group: libc::pid_t) -> io::Result<Vec<libc::pid_t>> {
        let mut pids: Vec<libc::pid_t> = vec![0; super::MAX_GROUP + 1];
        let bytes =
            libc::c_int::try_from(size_of_val(pids.as_slice())).map_err(io::Error::other)?;
        // SAFETY: `pids` is writable for `bytes` bytes, and the call writes whole pids into it.
        let count = unsafe { libc::proc_listpgrppids(group, pids.as_mut_ptr().cast(), bytes) };
        let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
        pids.truncate(count);
        Ok(pids)
    }

    pub fn cwd(pid: libc::pid_t) -> io::Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        // SAFETY: `info` is writable for `size` bytes, the struct PROC_PIDVNODEPATHINFO selects.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
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
        let path: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .as_flattened()
            .iter()
            .map(|&c| c as u8)
            .collect();
        let end = path
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(path.len());
        // The kernel gives an empty path for a directory it cannot name, such as a deleted one.
        let path = std::ffi::OsStr::from_bytes(&path[..end]);
        (!path.is_empty())
            .then(|| path.into())
            .ok_or_else(|| io::Error::other("the working directory has no path"))
    }

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
        Some(super::lossy(parts.take(usize::try_from(count).ok()?)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::Pty;
    use std::os::fd::AsFd;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn pty(script: &str) -> Pty {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        Pty::spawn(command, 24, 80).unwrap()
    }

    fn found_when(pty: &Pty, done: impl Fn(&Foreground) -> bool) -> Foreground {
        let start = Instant::now();
        loop {
            let found = foreground(pty.master().as_fd());
            match found {
                Ok(found) if done(&found) => return found,
                found => assert!(start.elapsed() < Duration::from_secs(5), "{found:?}"),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn named(found: &Foreground, name: &str) -> bool {
        found.members.iter().any(|member| member.name == name)
    }

    #[test]
    fn a_scan_skips_only_processes_that_exited() {
        let stat = |text: &str| Ok(text.as_bytes().to_vec());
        let stats = || {
            vec![
                (100, stat("100 (sh) S 1 100 100 34816")),
                (101, Err(io::Error::from(io::ErrorKind::NotFound))),
                (102, Err(io::Error::from_raw_os_error(libc::ESRCH))),
                (103, stat("103 (a b) c)) R 100 100 100 34816")),
                (104, stat("104 (other) S 1 7 7 0")),
                (105, Ok(b"105 (\xff) S 1 100 100 0".to_vec())),
            ]
            .into_iter()
        };
        assert_eq!(scan(100, stats()).unwrap(), vec![100, 103, 105]);

        let refused = stats().chain([(106, Err(io::Error::from(io::ErrorKind::PermissionDenied)))]);
        assert_eq!(
            scan(100, refused).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(scan(100, stats().chain([(107, stat("107 no brackets"))])).is_err());
        assert!(scan(100, stats().chain([(108, stat("108 (x) S 1"))])).is_err());
        let many = (0..1000).map(|pid| (pid, stat("1 (x) S 1 100 100 0")));
        assert_eq!(scan(100, many).unwrap().len(), MAX_GROUP + 1);
    }

    #[test]
    fn a_group_without_its_leader_is_refused_unless_it_is_empty() {
        assert_eq!(with_leader(100, vec![100, 101]).unwrap(), vec![100, 101]);
        assert!(with_leader(100, vec![101]).is_err());
        assert_eq!(
            with_leader(100, Vec::new()).unwrap(),
            Vec::<libc::pid_t>::new()
        );
    }

    #[test]
    fn the_foreground_process_has_its_name_arguments_and_user() {
        let pty = pty("exec sleep 30");
        let found = found_when(&pty, |found| named(found, "sleep"));
        assert_eq!(found.group, pty.terminal().session);
        let sleep = &found.members[0];
        assert_eq!(sleep.argv, Some(vec!["sleep".to_owned(), "30".to_owned()]));
        assert_eq!(sleep.euid, crate::effective_uid());
        assert!(!found.echo_off);
    }

    #[test]
    fn every_process_in_the_group_is_read() {
        // Without job control, background jobs stay in the shell's group.
        let pty = pty("sleep 31 & sleep 32 & wait");
        let argv = |member: &Process| member.argv.clone().unwrap_or_default().join(" ");
        // A job is read as the shell until it has started its program.
        let both = |found: &Foreground| {
            let lines: Vec<String> = found.members.iter().map(argv).collect();
            ["sleep 31", "sleep 32"]
                .iter()
                .all(|line| lines.iter().any(|l| l == line))
        };
        assert_eq!(found_when(&pty, both).members.len(), 3);
    }

    #[test]
    fn a_password_style_read_has_echo_off() {
        let echo_off = pty("stty -echo; exec sleep 30");
        found_when(&echo_off, |found| found.echo_off && named(found, "sleep"));
        let raw = pty("stty -echo -icanon; exec sleep 30");
        assert!(!found_when(&raw, |found| named(found, "sleep")).echo_off);
    }

    #[test]
    fn a_process_has_the_directory_it_moved_to() {
        let pty = pty("cd /; exec sleep 30");
        let pid = pty.terminal().session;
        let start = Instant::now();
        while cwd(pid).ok() != Some(PathBuf::from("/")) {
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}", cwd(pid));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cwd(u32::MAX).is_err());
    }

    #[test]
    fn a_terminal_whose_program_ended_has_no_foreground() {
        let pty = pty("exit 0");
        let start = Instant::now();
        let found = || foreground(pty.master().as_fd());
        while found().is_ok_and(|found| !found.members.is_empty()) {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
