//! The way from the window's thread to the I/O thread, with a limit in bytes.
//!
//! While the core lags, requests wait here in order until the link has room, so input after a
//! paste cannot overtake it. The window's thread never waits: past [`MAX_HELD`] a request is
//! refused, and the window says so. With the link's own queue of two full requests, a stalled
//! core makes a window hold about 3 MiB, not counting the socket's buffer.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use pinfold_proto::link::{self as proto, Direction, MAX_INPUT, Position, Selection};

use crate::link::Link;

/// Most bytes waiting here: a full paste, and room to type behind it.
const MAX_HELD: usize = MAX_INPUT + 64 * 1024;
/// What a request costs before its text: a channel slot, its state and a share of its block.
const SLOT: usize = size_of::<Request>() + 16;

/// What the window asks of the I/O thread.
pub(crate) enum Request {
    /// The window is on screen, so the link can say hello, which a launcher is waiting on.
    Opened,
    Input(u32, Vec<u8>),
    Paste(u32, String),
    Resize(u16, u16),
    /// Replaces any search running in the pane.
    Search {
        pane: u32,
        query: String,
        from: Position,
        direction: Direction,
    },
    Copy(u32, Selection),
    /// Anything that owns all it says.
    Core(proto::Request<'static>),
}

impl Request {
    fn cost(&self) -> usize {
        SLOT + match self {
            Self::Input(_, bytes) => bytes.capacity(),
            Self::Paste(_, text) | Self::Search { query: text, .. } => text.capacity(),
            _ => 0,
        }
    }

    /// Hands this to `link`, which has room. A request the link still turns down can never be
    /// sent, as `paste::check` keeps such pastes out, or the link has ended and the window says
    /// so. It goes, so that it cannot hold up the requests behind it.
    fn deliver(self, link: &mut Link) {
        let _ = match self {
            Self::Opened => {
                link.greet();
                true
            }
            Self::Input(pane, bytes) => link.send(proto::Request::Input {
                pane,
                bytes: &bytes,
            }),
            Self::Paste(pane, text) => link.send(proto::Request::Paste { pane, text: &text }),
            Self::Resize(rows, columns) => link.resize(rows, columns),
            Self::Search {
                pane,
                query,
                from,
                direction,
            } => link.send(proto::Request::Search {
                pane,
                from,
                direction,
                query: &query,
            }),
            Self::Copy(pane, selection) => link.copy(pane, selection),
            Self::Core(request) => link.send(request),
        };
    }
}

/// The window's end.
pub(crate) struct Outbox {
    requests: Sender<Request>,
    held: Arc<AtomicUsize>,
    refused: Cell<bool>,
}

impl Outbox {
    /// Passes `request` on without waiting, and returns whether it did. A refused request is
    /// gone, and [`Outbox::take_refused`] says so.
    pub(crate) fn send(&self, request: Request) -> bool {
        let cost = request.cost();
        if self.held.fetch_add(cost, Ordering::Relaxed) + cost > MAX_HELD {
            self.held.fetch_sub(cost, Ordering::Relaxed);
            self.refused.set(true);
            return false;
        }
        // Only the end of the I/O thread stops this, and the window then says the session ended.
        if self.requests.send(request).is_err() {
            self.held.fetch_sub(cost, Ordering::Relaxed);
        }
        true
    }

    /// Returns whether a request was refused since the last call.
    pub(crate) fn take_refused(&self) -> bool {
        self.refused.take()
    }
}

/// The I/O thread's end.
pub(crate) struct Inbox {
    requests: Receiver<Request>,
    held: Arc<AtomicUsize>,
}

impl Inbox {
    /// Moves requests to `link` while it has room, and leaves the rest for after the next turn.
    /// A request for a pane that closed goes like any other, and the core ignores it.
    pub(crate) fn drain(&self, link: &mut Link) {
        while link.has_room() {
            let Ok(request) = self.requests.try_recv() else {
                return;
            };
            self.held.fetch_sub(request.cost(), Ordering::Relaxed);
            request.deliver(link);
        }
    }

    /// Takes every request waiting, for a test to look at.
    #[cfg(test)]
    pub(crate) fn take(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }
}

/// Makes the two ends.
pub(crate) fn channel() -> (Outbox, Inbox) {
    let (sender, requests) = mpsc::channel();
    let held = Arc::new(AtomicUsize::new(0));
    let outbox = Outbox {
        requests: sender,
        held: Arc::clone(&held),
        refused: Cell::new(false),
    };
    (outbox, Inbox { requests, held })
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::{Duration, Instant};

    use pinfold_proto::link::{Decoder, MAX_PASTE, Notice, VERSION, encode_layout};

    use super::*;

    const PANE: u32 = 3;
    const PATIENCE: Duration = Duration::from_secs(20);

    /// A link whose core has said hello, which the window answers on its next turn.
    fn linked() -> (Link, UnixStream) {
        let (ours, mut core) = UnixStream::pair().unwrap();
        core.set_read_timeout(Some(PATIENCE)).unwrap();
        let mut wire = Vec::new();
        Notice::Hello { version: VERSION }
            .encode(&mut wire)
            .unwrap();
        std::io::Write::write_all(&mut core, &wire).unwrap();
        (Link::new(ours.into()).unwrap(), core)
    }

    fn paste(letter: &str) -> Request {
        Request::Paste(PANE, letter.repeat(MAX_PASTE))
    }

    /// Reads `count` requests, each shown as its kind, length and first letter.
    fn read(mut core: UnixStream, count: usize) -> Vec<(char, usize, char)> {
        let (mut decoder, mut chunk, mut seen) = (Decoder::default(), vec![0; 64 * 1024], vec![]);
        while seen.len() < count {
            let read = core.read(&mut chunk).unwrap();
            assert!(read > 0, "the link closed after {} requests", seen.len());
            let mut input = &chunk[..read];
            while let Some(request) = decoder.next_request(&mut input).unwrap() {
                seen.push(match request {
                    proto::Request::Paste { text, .. } => {
                        ('p', text.len(), text.chars().next().unwrap())
                    }
                    proto::Request::Input { bytes, .. } => ('i', bytes.len(), char::from(bytes[0])),
                    _ => ('o', 0, ' '),
                });
            }
        }
        seen
    }

    /// Turns and drains until the core has read all it expects. Each turn can wait as long as the
    /// test does, so only the socket's own wake-ups, as in `run_io`, move this along.
    fn deliver_to(
        link: &mut Link,
        inbox: &Inbox,
        core: thread::JoinHandle<Vec<(char, usize, char)>>,
    ) -> Vec<(char, usize, char)> {
        let start = Instant::now();
        while !core.is_finished() && start.elapsed() < PATIENCE {
            let _ = link.turn(Some(PATIENCE));
            inbox.drain(link);
        }
        core.join().unwrap()
    }

    #[test]
    fn a_stalled_core_loses_no_paste_and_nothing_overtakes_one() {
        let (mut link, core) = linked();
        let (outbox, inbox) = channel();
        for letter in ["a", "b", "c"] {
            assert!(outbox.send(paste(letter)));
            inbox.drain(&mut link);
        }
        assert!(outbox.send(Request::Input(PANE, b"k".to_vec())));
        inbox.drain(&mut link);
        assert!(!link.has_room());
        assert!(
            outbox.held.load(Ordering::Relaxed) > 0,
            "nothing is waiting"
        );

        let reader = thread::spawn(move || read(core, 5));
        let seen = deliver_to(&mut link, &inbox, reader);
        let full = |letter| ('p', MAX_PASTE, letter);
        let hello = ('o', 0, ' ');
        let key = ('i', 1, 'k');
        assert_eq!(seen, [hello, full('a'), full('b'), full('c'), key]);
        assert_eq!(outbox.held.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn what_waits_has_a_limit_and_past_it_the_window_is_told() {
        let (mut link, core) = linked();
        let (outbox, inbox) = channel();
        // Fills the link, so nothing leaves the channel.
        for letter in ["a", "b"] {
            assert!(outbox.send(paste(letter)));
            inbox.drain(&mut link);
        }
        assert!(!link.has_room());
        assert!(!outbox.take_refused());

        let mut held = 0;
        while outbox.send(Request::Core(proto::Request::NewTab)) {
            held += 1;
            assert!(held <= MAX_HELD / SLOT, "the channel has no limit");
        }
        assert_eq!(held, MAX_HELD / SLOT);
        assert!(outbox.take_refused());
        assert!(!outbox.take_refused());
        assert!(!outbox.send(paste("c")));

        drop(core);
    }

    #[test]
    fn a_request_for_a_closed_pane_holds_nothing_up() {
        let (mut link, mut core) = linked();
        let (outbox, inbox) = channel();
        let mut wire = Vec::new();
        encode_layout(&mut wire, 0, PANE + 1, 1, &[PANE + 1], &[]).unwrap();
        std::io::Write::write_all(&mut core, &wire).unwrap();
        let start = Instant::now();
        while link.focused() != PANE + 1 && start.elapsed() < PATIENCE {
            link.turn(Some(Duration::from_millis(20))).unwrap();
        }
        assert_eq!(link.focused(), PANE + 1);

        assert!(outbox.send(Request::Input(PANE, b"late".to_vec())));
        assert!(outbox.send(Request::Input(PANE + 1, b"mine".to_vec())));
        inbox.drain(&mut link);
        let seen = read(core, 3);
        assert_eq!(seen[1..], [('i', 4, 'l'), ('i', 4, 'm')]);
    }

    #[test]
    fn requests_the_link_can_never_take_do_not_wedge_the_queue() {
        let (mut link, core) = linked();
        let (outbox, inbox) = channel();
        assert!(outbox.send(Request::Paste(PANE, "x".repeat(MAX_PASTE + 1))));
        assert!(outbox.send(Request::Input(PANE, b"k".to_vec())));
        inbox.drain(&mut link);
        assert_eq!(outbox.held.load(Ordering::Relaxed), 0);
        drop(core);

        let start = Instant::now();
        while link.disconnected().is_none() && start.elapsed() < PATIENCE {
            let _ = link.turn(Some(Duration::from_millis(20)));
        }
        assert!(link.disconnected().is_some());
        assert!(outbox.send(Request::Input(PANE, b"late".to_vec())));
        inbox.drain(&mut link);
        assert_eq!(outbox.held.load(Ordering::Relaxed), 0, "a dead link wedged");
    }

    #[test]
    fn a_gone_io_thread_is_no_refusal() {
        let (outbox, inbox) = channel();
        drop(inbox);
        assert!(outbox.send(paste("a")));
        assert!(!outbox.take_refused());
        assert_eq!(outbox.held.load(Ordering::Relaxed), 0);
    }
}
