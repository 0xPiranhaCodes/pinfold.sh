//! Opens PTYs on a spawn thread, so the reactor waits on `grantpt` for a deadline at most
//! (ADR-0048). On macOS it can stay in the kernel forever (#392).

use std::io::{self, ErrorKind};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

/// How long the reactor waits for a PTY to open. Under pinfold-sys's `tests/open-latency.sh`
/// the slowest of 76,800 opens took 1.1 s, and 2.4 s in an earlier run; one past this is stuck.
pub(crate) const OPEN_DEADLINE: Duration = Duration::from_secs(5);
/// Most spawn threads left stuck in the kernel at once. Past it new panes are refused.
pub(crate) const MAX_ABANDONED: usize = 4;
/// The spawn thread runs the open calls and nothing else.
const OPEN_STACK: usize = 64 * 1024;

type Pair = (OwnedFd, OwnedFd);
type Open = fn() -> io::Result<Pair>;

/// A running spawn thread. Dropping it ends the thread, or, if it is stuck, closes its pair
/// when it comes.
struct Worker {
    jobs: Sender<()>,
    pairs: Receiver<io::Result<Pair>>,
    /// Held by the thread too, until it ends.
    alive: Arc<()>,
}

pub(crate) struct Opener {
    open: Open,
    deadline: Duration,
    worker: Option<Worker>,
    /// Threads that missed the deadline, each until it ends.
    abandoned: Vec<Arc<()>>,
}

impl Opener {
    /// Opens each pair with `open`, waiting `deadline` for it.
    pub(crate) fn new(open: Open, deadline: Duration) -> Self {
        Self {
            open,
            deadline,
            worker: None,
            abandoned: Vec::new(),
        }
    }

    /// Opens a pair on the spawn thread, which starts on first use. One that misses the
    /// deadline is left to the stuck thread, which closes it if it ever comes.
    ///
    /// # Errors
    ///
    /// Returns `TimedOut` past the deadline, and an error without trying once
    /// [`MAX_ABANDONED`] threads are stuck, or if the open fails.
    pub(crate) fn open(&mut self) -> io::Result<Pair> {
        self.abandoned.retain(|alive| Arc::strong_count(alive) > 1);
        if self.abandoned.len() >= MAX_ABANDONED {
            return Err(io::Error::other(
                "no new pane: PTY opens are stuck in the kernel, granting the terminal (#392)",
            ));
        }
        let worker = match self.worker.take() {
            Some(worker) => worker,
            None => self.start()?,
        };
        // A send fails only if the thread has ended, which the receive below then reports.
        let _ = worker.jobs.send(());
        match worker.pairs.recv_timeout(self.deadline) {
            Ok(pair) => {
                self.worker = Some(worker);
                pair
            }
            Err(RecvTimeoutError::Timeout) => {
                self.abandoned.push(Arc::clone(&worker.alive));
                Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!(
                        "the PTY did not open within {} s: granting it is stuck in the kernel (#392)",
                        self.deadline.as_secs_f32()
                    ),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::other("the PTY opener ended")),
        }
    }

    fn start(&self) -> io::Result<Worker> {
        let (jobs, wanted) = mpsc::channel::<()>();
        let (opened, pairs) = mpsc::channel();
        let alive = Arc::new(());
        let held = Arc::clone(&alive);
        let open = self.open;
        thread::Builder::new()
            .name("pinfold-spawn".to_owned())
            .stack_size(OPEN_STACK)
            .spawn(move || {
                let _held = held;
                // Ends once the reactor drops the worker; a pair it no longer waits for is
                // dropped here, or in the channel, and so closed.
                while wanted.recv().is_ok() {
                    if opened.send(open()).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Worker { jobs, pairs, alive })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn stuck() -> io::Result<Pair> {
        loop {
            thread::park();
        }
    }

    fn late() -> io::Result<Pair> {
        thread::sleep(Duration::from_millis(200));
        pinfold_sys::pty::open_pair()
    }

    #[test]
    fn past_the_cap_of_stuck_threads_a_pane_is_refused_without_a_thread() {
        let mut opener = Opener::new(stuck, Duration::from_millis(20));
        for _ in 0..MAX_ABANDONED {
            let error = opener.open().unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TimedOut);
        }
        let start = Instant::now();
        let error = opener.open().unwrap_err();
        assert!(start.elapsed() < Duration::from_millis(20));
        assert!(error.to_string().contains("stuck in the kernel"), "{error}");
        assert!(opener.worker.is_none());
        assert_eq!(opener.abandoned.len(), MAX_ABANDONED);
    }

    #[test]
    fn a_thread_that_comes_back_late_frees_its_place_under_the_cap() {
        let mut opener = Opener::new(late, Duration::from_millis(20));
        for _ in 0..MAX_ABANDONED {
            assert_eq!(opener.open().unwrap_err().kind(), ErrorKind::TimedOut);
        }
        let start = Instant::now();
        while opener
            .abandoned
            .iter()
            .any(|alive| Arc::strong_count(alive) > 1)
        {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "a late thread never ended"
            );
            thread::sleep(Duration::from_millis(10));
        }
        opener.deadline = OPEN_DEADLINE;
        assert!(opener.open().is_ok());
        assert!(opener.abandoned.is_empty());
    }
}
