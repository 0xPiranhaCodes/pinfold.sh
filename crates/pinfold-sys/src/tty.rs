//! Raw mode and the size of the terminal a full-screen tool runs in (ADR-0045).

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd};

/// A terminal in raw mode, back in its old mode when dropped. It borrows the descriptor, so it
/// cannot outlive it and restore a terminal that later took the same number.
///
/// ```compile_fail,E0597
/// use std::os::fd::AsFd;
///
/// let raw = {
///     let terminal = std::fs::File::open("/dev/tty")?;
///     pinfold_sys::tty::raw(terminal.as_fd())?
/// }; // `terminal` closes here, while `raw` still holds it: refused
/// # drop(raw);
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct Raw<'fd> {
    fd: BorrowedFd<'fd>,
    saved: libc::termios,
}

/// Puts the terminal `fd` in raw mode: keys arrive one byte at a time, unechoed and with no
/// signals, and a read waits at most 100 ms, so a tool can poll between keys.
///
/// # Errors
///
/// Returns an error if `fd` is not a terminal.
///
/// # Examples
///
/// ```no_run
/// use std::os::fd::AsFd;
///
/// let stdin = std::io::stdin();
/// let raw = pinfold_sys::tty::raw(stdin.as_fd())?;
/// drop(raw); // the old mode is back
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn raw(terminal: BorrowedFd<'_>) -> io::Result<Raw<'_>> {
    let fd = terminal.as_raw_fd();
    let mut mode = MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `mode` is valid for writes of a whole termios for the call.
    if unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr succeeded, and all-zero bytes are valid for this integer struct.
    let saved = unsafe { mode.assume_init() };
    let mut raw = saved;
    // SAFETY: `raw` is a valid termios that cfmakeraw only changes in place.
    unsafe { libc::cfmakeraw(&raw mut raw) };
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 1;
    // SAFETY: `raw` is a valid termios, read for the call.
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw const raw) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Raw {
        fd: terminal,
        saved,
    })
}

impl Drop for Raw<'_> {
    fn drop(&mut self) {
        // SAFETY: the borrow keeps `fd` open, and `saved` is the valid termios tcgetattr gave.
        unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSAFLUSH, &raw const self.saved) };
    }
}

/// Returns the rows and columns of the terminal `fd`.
///
/// # Errors
///
/// Returns an error if `fd` is not a terminal.
///
/// # Examples
///
/// ```no_run
/// use std::os::fd::AsFd;
///
/// let (rows, columns) = pinfold_sys::tty::size(std::io::stdout().as_fd())?;
/// assert!(rows > 0 && columns > 0);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn size(fd: BorrowedFd<'_>) -> io::Result<(u16, u16)> {
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: `size` is valid for writes of a whole winsize, which TIOCGWINSZ fills.
    if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ as _, size.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the ioctl succeeded, and all-zero bytes are valid for this integer struct.
    let size = unsafe { size.assume_init() };
    Ok((size.ws_row, size.ws_col))
}

#[cfg(test)]
mod tests {
    use std::mem::MaybeUninit;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

    fn mode(fd: BorrowedFd<'_>) -> libc::termios {
        let mut mode = MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: `mode` is valid for writes of a whole termios for the call.
        let got = unsafe { libc::tcgetattr(fd.as_raw_fd(), mode.as_mut_ptr()) };
        assert_eq!(got, 0);
        // SAFETY: tcgetattr succeeded, and all-zero bytes are valid for this integer struct.
        unsafe { mode.assume_init() }
    }

    #[test]
    fn raw_mode_takes_single_keys_and_dropping_it_restores_the_old_mode() {
        let (_master, slave) = crate::pty::open_pair().unwrap();
        let before = mode(slave.as_fd());
        assert_ne!(before.c_lflag & libc::ICANON, 0);
        let raw = super::raw(slave.as_fd()).unwrap();
        let during = mode(slave.as_fd());
        assert_eq!(during.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
        assert_eq!((during.c_cc[libc::VMIN], during.c_cc[libc::VTIME]), (0, 1));
        drop(raw);
        let after = mode(slave.as_fd());
        assert_eq!(
            (after.c_lflag, after.c_iflag),
            (before.c_lflag, before.c_iflag)
        );
    }

    #[test]
    fn size_reads_what_the_master_set() {
        let (master, slave) = crate::pty::open_pair().unwrap();
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `master` is open, and `size` is a valid winsize for TIOCSWINSZ.
        let set =
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &raw const size) };
        assert_eq!(set, 0);
        assert_eq!(super::size(slave.as_fd()).unwrap(), (30, 100));
    }

    #[test]
    fn a_file_is_not_a_terminal() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(super::raw(file.as_fd()).is_err());
        assert!(super::size(file.as_fd()).is_err());
    }
}
