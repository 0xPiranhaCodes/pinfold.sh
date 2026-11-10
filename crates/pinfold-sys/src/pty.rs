//! Pseudo-terminals: a child process on a PTY, with the master end held by the caller.

use std::fs::File;
use std::io;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};

use crate::Terminal;

/// A child process attached to a PTY. Dropping it kills the child and closes the master.
#[derive(Debug)]
pub struct Pty {
    master: ManuallyDrop<File>,
    child: Child,
    terminal: Terminal,
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn window_size(rows: u16, columns: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    // In macOS since 10.13.4, which the `libc` crate does not declare.
    fn ptsname_r(fd: libc::c_int, buf: *mut libc::c_char, buflen: libc::size_t) -> libc::c_int;
}
#[cfg(not(target_os = "macos"))]
use libc::ptsname_r;

/// Opens a PTY's master and slave, each close-on-exec from the moment it exists, so a child
/// that another thread forks meanwhile inherits neither. A master a stray child holds would
/// keep a killed pane's program from ending on macOS, as it waits for its output to drain.
///
/// On macOS `grantpt` can stay in the kernel forever, so a caller that must not stall opens
/// the pair on another thread and gives [`Pty::spawn_on`] what it returns (ADR-0048).
///
/// # Errors
///
/// Returns an error if the PTY cannot be opened, granted, unlocked or named.
///
/// # Examples
///
/// ```
/// let (master, slave) = pinfold_sys::pty::open_pair()?;
/// # drop((master, slave));
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn open_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: posix_openpt takes integer flags and returns a new fd or -1.
    let open = || check(unsafe { libc::posix_openpt(flags) });
    let master = open_master(open)?;
    // SAFETY: posix_openpt succeeded, so the fd is open and owned only by this function.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    // SAFETY: `master` is an open PTY master.
    check(unsafe { libc::grantpt(master.as_raw_fd()) })?;
    // SAFETY: `master` is an open PTY master.
    check(unsafe { libc::unlockpt(master.as_raw_fd()) })?;
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: `name` is writable for its whole length, which is the length passed.
    let named = unsafe { ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    if named != 0 {
        // Linux returns the error, macOS -1 with errno.
        return Err(match named {
            -1 => io::Error::last_os_error(),
            error => io::Error::from_raw_os_error(error),
        });
    }
    // SAFETY: ptsname_r succeeded, so `name` holds a NUL-terminated path.
    let slave = check(unsafe { libc::open(name.as_ptr(), flags) })?;
    // SAFETY: open succeeded, so the fd is open and owned only by this function.
    Ok((master, unsafe { OwnedFd::from_raw_fd(slave) }))
}

/// How many times in all a PTY master is opened while it loses the race for a slot. Under
/// `tests/pty-loop.sh` two tries still lost once in 384,000 opens, and four never did.
const OPEN_TRIES: usize = 4;

/// errno -6, private to the macOS kernel: an open of `/dev/ptmx` that lost the race for a slot
/// to another open ten times in a row, which the kernel returns instead of trying again.
const EREDRIVEOPEN: i32 = -6;

/// Opens a master with `open`, again while it loses the race for a slot. macOS refuses with
/// ENXIO when another PTY closes while this one opens with every slot in use: the kernel picks
/// the slot past the end, then finds one free and grows nothing. It refuses with EREDRIVEOPEN
/// when other opens took the slot it picked. A later try takes a slot; a table truly full
/// refuses each time. The kernel's private code is reported as a busy resource, not an errno.
fn open_master(mut open: impl FnMut() -> io::Result<libc::c_int>) -> io::Result<libc::c_int> {
    let raced =
        |error: &io::Error| matches!(error.raw_os_error(), Some(libc::ENXIO | EREDRIVEOPEN));
    let mut tries = 1;
    loop {
        match open() {
            Err(error) if raced(&error) && tries < OPEN_TRIES => tries += 1,
            Err(error) if error.raw_os_error() == Some(EREDRIVEOPEN) => {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "other PTYs opening at once took every slot this one tried",
                ));
            }
            opened => return opened,
        }
    }
}

impl Pty {
    /// Starts `command` as a session leader with a new PTY as its controlling terminal
    /// and its stdin, stdout and stderr.
    ///
    /// The master is non-blocking, and both ends are close-on-exec from the moment they are
    /// opened, so a child another thread starts at the same time inherits neither.
    /// The child gets no other descriptor past stderr, unless it is handed over with
    /// [`crate::inherit`].
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be created or the program cannot be started.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::process::Command;
    ///
    /// let mut command = Command::new("/bin/sh");
    /// command.args(["-c", "exit 0"]);
    /// let pty = pinfold_sys::pty::Pty::spawn(command, 24, 80)?;
    /// # drop(pty);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn spawn(command: Command, rows: u16, columns: u16) -> io::Result<Self> {
        Self::spawn_on(open_pair()?, command, rows, columns)
    }

    /// [`Pty::spawn`] on a master and slave that [`open_pair`] opened, perhaps on another
    /// thread: on macOS `grantpt` can stay in the kernel forever (ADR-0048).
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY cannot be set up or the program cannot be started.
    pub fn spawn_on(
        (master, slave): (OwnedFd, OwnedFd),
        mut command: Command,
        rows: u16,
        columns: u16,
    ) -> io::Result<Self> {
        let size = window_size(rows, columns);
        // SAFETY: `master` is open, and `size` is a valid winsize for TIOCSWINSZ.
        check(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &raw const size) })?;
        // SAFETY: `master` is open, and F_SETFL takes an integer flag.
        check(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) })?;
        let mut found = MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: `slave` is open, and `found` is valid for writes of a whole stat for the call.
        check(unsafe { libc::fstat(slave.as_raw_fd(), found.as_mut_ptr()) })?;
        // SAFETY: fstat succeeded, and all-zero bytes are valid for this integer struct.
        let device = unsafe { found.assume_init() }.st_rdev as u32;

        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        crate::mark_strays(&mut command);
        // SAFETY: the closure makes only async-signal-safe calls, and runs after the
        // standard library has put the PTY on fd 0.
        unsafe {
            command.pre_exec(|| {
                // SAFETY: setsid takes no arguments.
                check(libc::setsid())?;
                // SAFETY: fd 0 is the PTY, and this request takes an integer argument.
                check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        Ok(Self {
            master: ManuallyDrop::new(File::from(master)),
            // The child leads its session, so the session's id is its pid.
            terminal: Terminal {
                device,
                session: child.id(),
            },
            child,
        })
    }

    /// The PTY and the session the child leads: a caller that holds both is in this pane.
    #[must_use]
    pub fn terminal(&self) -> Terminal {
        self.terminal
    }

    /// The non-blocking master. Read and write through `&File`, and register it for polling.
    #[must_use]
    pub fn master(&self) -> &File {
        &self.master
    }

    /// Tells the PTY its new size, which sends `SIGWINCH` to the foreground process group.
    ///
    /// # Errors
    ///
    /// Returns an error if the PTY rejects the request.
    pub fn resize(&self, rows: u16, columns: u16) -> io::Result<()> {
        let size = window_size(rows, columns);
        // SAFETY: the master is open, and `size` is a valid winsize for TIOCSWINSZ.
        check(unsafe {
            libc::ioctl(
                self.master.as_raw_fd(),
                libc::TIOCSWINSZ as _,
                &raw const size,
            )
        })?;
        Ok(())
    }

    /// Returns the child's exit status if it has exited, and reaps it.
    ///
    /// # Errors
    ///
    /// Returns an error if the child's state cannot be read.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // macOS holds a dying child in close until its output drains, so waiting with the
        // master open and unread can block forever.
        // SAFETY: this is the only drop of `master`, and nothing uses it afterwards.
        unsafe { ManuallyDrop::drop(&mut self.master) };
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read, Write};
    use std::thread;
    use std::time::{Duration, Instant};

    fn shell(script: &str) -> Pty {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        Pty::spawn(command, 24, 80).unwrap()
    }

    #[test]
    fn both_ends_are_close_on_exec_from_the_start() {
        let (master, slave) = open_pair().unwrap();
        for fd in [master, slave] {
            // SAFETY: `fd` is open, and F_GETFD takes no argument.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        }
    }

    /// Reads until `wanted` appears or five seconds pass.
    fn read_until(pty: &Pty, wanted: &str) -> String {
        let (mut text, mut buffer) = (String::new(), [0u8; 4096]);
        let start = Instant::now();
        while !text.contains(wanted) && start.elapsed() < Duration::from_secs(5) {
            match pty.master().read(&mut buffer) {
                Ok(count) => text.push_str(&String::from_utf8_lossy(&buffer[..count])),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                // Linux reports EIO once the child has closed its end.
                Err(_) => break,
            }
        }
        text
    }

    #[test]
    fn dropping_a_pty_with_unread_output_returns() {
        let pty = shell("yes");
        thread::sleep(Duration::from_millis(200));
        let (done, finished) = std::sync::mpsc::channel();
        thread::spawn(move || {
            drop(pty);
            done.send(()).unwrap();
        });
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn the_child_sees_the_requested_size() {
        assert!(read_until(&shell("stty size"), "24 80").contains("24 80"));
    }

    #[test]
    fn the_pty_is_the_controlling_terminal() {
        // Opening /dev/tty fails without a controlling terminal.
        let output = read_until(&shell("echo found > /dev/tty"), "found");
        assert!(output.contains("found"), "{output}");
    }

    #[test]
    fn resize_reaches_the_child() {
        let pty = shell("read line; stty size");
        pty.resize(30, 100).unwrap();
        pty.master().write_all(b"go\n").unwrap();
        assert!(read_until(&pty, "30 100").contains("30 100"));
    }

    #[test]
    fn the_master_never_blocks() {
        let pty = shell("sleep 30");
        let result = pty.master().read(&mut [0u8; 16]);
        assert_eq!(result.unwrap_err().kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn drop_kills_the_child() {
        let pty = shell("echo $$; sleep 30");
        let output = read_until(&pty, "\n");
        let pid = output.trim().to_owned();
        drop(pty);
        let alive = Command::new("/bin/sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .unwrap();
        assert!(!alive.success(), "process {pid} survived");
    }

    /// The descriptors `command` finds open, read from /dev/fd by ls, which opens only its own,
    /// numbered low.
    fn open_in_child(mut command: Command) -> Vec<i32> {
        command.env_remove("CLICOLOR").env_remove("CLICOLOR_FORCE");
        let pty = Pty::spawn(command, 24, 80).unwrap();
        let text = read_until(&pty, "end");
        text.lines()
            .map_while(|line| line.trim().parse().ok())
            .collect()
    }

    #[test]
    fn a_descriptor_inheritable_on_another_thread_does_not_reach_the_child() {
        // As an accept on macOS leaves a connection for a moment before marking it. No
        // close-on-exec, on purpose, and numbered high so it cannot be one ls opens.
        // SAFETY: F_DUPFD takes an integer and touches no memory.
        let made = thread::spawn(|| unsafe { libc::fcntl(2, libc::F_DUPFD, 40) });
        let leak = check(made.join().unwrap()).unwrap();
        // SAFETY: `leak` is open and owned only by this test.
        let _leak = unsafe { OwnedFd::from_raw_fd(leak) };
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "ls -1 /dev/fd; echo end"]);
        let open = open_in_child(command);
        assert!(open.contains(&0), "{open:?}");
        assert!(open.iter().all(|fd| *fd < 10), "{open:?}");
    }

    #[test]
    fn a_descriptor_handed_on_purpose_still_reaches_the_child() {
        let fd = OwnedFd::from(File::open("/dev/null").unwrap());
        let number = fd.as_raw_fd();
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!("test -e /dev/fd/{number} && echo found; echo end"),
        ]);
        crate::inherit(&mut command, fd).unwrap();
        let pty = Pty::spawn(command, 24, 80).unwrap();
        assert!(read_until(&pty, "end").contains("found"));
    }

    /// The time a pane spawn takes, to the child's exit: `cargo test -p pinfold-sys --release
    /// --lib spawn_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn spawn_cost() {
        let runs = 200;
        let start = Instant::now();
        for _ in 0..runs {
            let mut pty = Pty::spawn(Command::new("/usr/bin/true"), 24, 80).unwrap();
            while pty.try_wait().unwrap().is_none() {
                thread::sleep(Duration::from_micros(50));
            }
        }
        let each = start.elapsed() / runs;
        // SAFETY: sysconf reads a limit and touches no memory.
        let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        eprintln!("{runs} spawns, {each:?} each, open-files limit {limit}");
    }

    /// Prints how long opens take with 16 threads opening 300 each, for the core's deadline
    /// (ADR-0048). `tests/open-latency.sh` runs copies at once. The replica closes first, so
    /// no `grantpt` gets stuck (#392) and every open counts.
    #[test]
    #[ignore = "a measurement, run by tests/open-latency.sh"]
    fn open_latency() {
        let threads: Vec<_> = (0..16)
            .map(|_| {
                thread::spawn(|| {
                    (0..300)
                        .map(|_| {
                            let start = Instant::now();
                            let pair = open_pair();
                            let took = start.elapsed();
                            pair.map(|(master, slave)| drop((slave, master)))
                                .map(|()| took)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let tries: Vec<_> = threads
            .into_iter()
            .flat_map(|t| t.join().unwrap())
            .collect();
        let mut took: Vec<_> = tries
            .iter()
            .filter_map(|tried| tried.as_ref().ok())
            .collect();
        took.sort();
        let at = |share: f64| took[((took.len() - 1) as f64 * share) as usize].as_millis();
        println!(
            "opens={} failed={} p50_ms={} p99_ms={} max_ms={}",
            took.len(),
            tries.len() - took.len(),
            at(0.5),
            at(0.99),
            at(1.0)
        );
    }

    /// Many PTYs opened at once, as `tests/pty-loop.sh` runs it from several processes (#383).
    /// Each closes its replica first: a master closed first can leave another process's grantpt
    /// restarting forever on macOS, which the loop counts apart as a hang.
    #[test]
    #[ignore = "a load for tests/pty-loop.sh, not a check on its own"]
    fn many_ptys_open_at_once() {
        let open = || open_pair().map(|(master, slave)| drop((slave, master)));
        let threads: Vec<_> = (0..16)
            .map(|_| thread::spawn(move || (0..1000).try_for_each(|_| open())))
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
    }

    #[test]
    fn a_master_that_lost_the_race_for_a_slot_is_opened_again() {
        let failing = |codes: Vec<i32>| {
            let mut codes = codes.into_iter();
            move || {
                codes
                    .next()
                    .map_or(Ok(7), |code| Err(io::Error::from_raw_os_error(code)))
            }
        };
        assert_eq!(
            open_master(failing(vec![EREDRIVEOPEN, libc::ENXIO, EREDRIVEOPEN])).unwrap(),
            7
        );
        let refused = open_master(failing(vec![EREDRIVEOPEN; OPEN_TRIES])).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::ResourceBusy);
        assert_eq!(refused.raw_os_error(), None);
        let full = open_master(failing(vec![libc::ENXIO; OPEN_TRIES])).unwrap_err();
        assert_eq!(full.raw_os_error(), Some(libc::ENXIO));
        let denied = open_master(failing(vec![libc::EACCES, EREDRIVEOPEN])).unwrap_err();
        assert_eq!(denied.raw_os_error(), Some(libc::EACCES));
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let result = Pty::spawn(Command::new("/does/not/exist"), 24, 80);
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
    }
}
