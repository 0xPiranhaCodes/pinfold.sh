//! Shared parts of the M0 process-split spike. See README.md.

pub mod draw;

use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, RawFd};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

pub const COLUMNS: usize = 80;
pub const ROWS: usize = 24;

pub struct Pane {
    pub term: Mutex<Term<VoidListener>>,
    master: RawFd,
}

/// Starts `/bin/sh` on a new PTY. Scrollback is off: this spike measures processes.
pub fn spawn_pane() -> Arc<Pane> {
    let mut size = libc::winsize { ws_row: ROWS as u16, ws_col: COLUMNS as u16, ws_xpixel: 0, ws_ypixel: 0 };
    let mut master = -1;
    let pid = unsafe { libc::forkpty(&mut master, ptr::null_mut(), ptr::null_mut(), (&raw mut size).cast()) };
    assert!(pid >= 0, "forkpty failed");
    if pid == 0 {
        let shell = c"/bin/sh";
        let argv = [shell.as_ptr(), ptr::null()];
        unsafe {
            libc::execv(shell.as_ptr(), argv.as_ptr());
            libc::_exit(127);
        }
    }
    unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) };
    let term = Term::new(Config { scrolling_history: 0, ..Config::default() }, &TermSize::new(COLUMNS, ROWS), VoidListener);
    Arc::new(Pane { term: Mutex::new(term), master })
}

/// The one I/O thread: polls every PTY and feeds its bytes to that pane's terminal.
pub fn spawn_io_thread(panes: Vec<Arc<Pane>>, mut on_dirty: impl FnMut() + Send + 'static) {
    thread::spawn(move || {
        let mut parsers: Vec<Processor> = panes.iter().map(|_| Processor::new()).collect();
        let mut files: Vec<File> =
            panes.iter().map(|pane| unsafe { File::from_raw_fd(pane.master) }).collect();
        let mut polls: Vec<libc::pollfd> = panes
            .iter()
            .map(|pane| libc::pollfd { fd: pane.master, events: libc::POLLIN, revents: 0 })
            .collect();
        let mut buffer = [0u8; 65536];
        loop {
            if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, -1) } < 0 {
                return;
            }
            for index in 0..polls.len() {
                if polls[index].revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                    continue;
                }
                let Ok(count) = files[index].read(&mut buffer) else {
                    polls[index].fd = -1;
                    continue;
                };
                if count == 0 {
                    polls[index].fd = -1;
                    continue;
                }
                let mut term = panes[index].term.lock().expect("lock");
                parsers[index].advance(&mut *term, &buffer[..count]);
                drop(term);
                on_dirty();
            }
        }
    });
}

/// One cell for the window: a character and a style word, eight bytes in all.
pub fn snapshot(pane: &Pane, out: &mut Vec<u64>) {
    let term = pane.term.lock().expect("lock");
    let grid = term.grid();
    for line in 0..grid.screen_lines() as i32 {
        for column in 0..grid.columns() {
            out.push(u64::from(u32::from(grid[Line(line)][Column(column)].c)));
        }
    }
}

/// An idle listener, standing in for the API thread that the core will have.
pub fn spawn_api_thread(path: &str) {
    let path = path.to_owned();
    thread::spawn(move || {
        let _ = std::fs::remove_file(&path);
        if let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) {
            for _ in listener.incoming() {}
        }
    });
}

/// PSS on Linux and phys_footprint on macOS, in bytes, as ADR-0008 requires.
#[cfg(target_os = "linux")]
pub fn footprint(pid: i32) -> usize {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).unwrap_or_default();
    let line = text.lines().find(|line| line.starts_with("Pss:")).unwrap_or("Pss: 0 kB");
    line.split_whitespace().nth(1).and_then(|kb| kb.parse().ok()).unwrap_or(0) * 1024
}

#[cfg(target_os = "macos")]
pub fn footprint(pid: i32) -> usize {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
    if result == 0 { info.ri_phys_footprint as usize } else { 0 }
}
