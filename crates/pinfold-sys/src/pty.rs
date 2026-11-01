//! Pseudo-terminals: a child process on a PTY, with the master end held by the caller.

use std::fs::File;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::ptr;

/// A child process attached to a PTY. Dropping it kills the child and closes the master.
#[derive(Debug)]
pub struct Pty {
    master: ManuallyDrop<File>,
    child: Child,
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

impl Pty {
    /// Starts `command` as a session leader with a new PTY as its controlling terminal
    /// and its stdin, stdout and stderr.
    ///
    /// The master is non-blocking and close-on-exec. It is briefly inheritable while it
    /// is being opened, so one thread must make every call that creates a child, or
    /// another pane's child could inherit this master.
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
    pub fn spawn(mut command: Command, rows: u16, columns: u16) -> io::Result<Self> {
        let (mut master, mut slave) = (-1, -1);
        let mut size = window_size(rows, columns);
        // SAFETY: both fd pointers are writable, null name and termios are allowed, and
        // `size` outlives the call.
        check(unsafe {
            libc::openpty(
                &raw mut master,
                &raw mut slave,
                ptr::null_mut(),
                ptr::null_mut(),
                (&raw mut size).cast(),
            )
        })?;
        // SAFETY: openpty succeeded, so each fd is open and owned only by this function.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        for fd in [&master, &slave] {
            // SAFETY: `fd` is open, and F_SETFD takes an integer flag.
            check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        }
        // SAFETY: `master` is open, and F_SETFL takes an integer flag.
        check(unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) })?;

        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
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
            child,
        })
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

    #[test]
    fn a_missing_program_is_an_error() {
        let result = Pty::spawn(Command::new("/does/not/exist"), 24, 80);
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotFound);
    }
}
