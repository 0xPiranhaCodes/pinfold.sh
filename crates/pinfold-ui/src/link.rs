//! The window's end of the link to the core (ADR-0028), with the operations of `Reactor`.
//!
//! [`Link`] sends requests, and keeps a copy of the tabs and of each pane the active tab shows,
//! so a hidden pane costs nothing here. Bytes from the core are untrusted: anything wrong ends
//! the link, and [`Link::turn`] says why as a value. The last screen stays readable after that.
//!
//! ```
//! use std::io::Write;
//! use std::os::unix::net::UnixStream;
//! use std::time::Duration;
//!
//! use pinfold_proto::link::{Notice, VERSION};
//! use pinfold_ui::link::Link;
//!
//! let (ours, mut core) = UnixStream::pair()?;
//! let mut link = Link::new(ours.into())?;
//!
//! let mut wire = Vec::new();
//! Notice::Hello { version: VERSION }.encode(&mut wire)?;
//! core.write_all(&wire)?;
//! link.turn(Some(Duration::from_secs(1)))?;
//! assert!(link.is_greeted());
//! assert!(link.screens().is_empty());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::io::{self, ErrorKind, Read, Write};
use std::ops::Range;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use pinfold_proto::link::{
    ApprovalAction, Decoder, ENTRY, Error, Layout, MAX_DIMENSION, MAX_PANES, MAX_REQUEST, Notice,
    OLDEST, Position, Rect, Refusal, Request, Selection, VERSION, progress_parts,
};
use pinfold_proto::{Frame, Palette};

use crate::top_bar::{ERROR, NEEDS_YOU};

/// Most bytes read per turn, so a flooding core cannot hold the window's thread.
const READ_BUDGET: usize = 64 * 1024;
/// Most encoded bytes held for a core that is not reading. Two full pastes, so a resize still
/// fits behind one. A request past this is refused whole, as a cut message would end the link.
const MAX_QUEUE: usize = 2 * MAX_REQUEST;
/// Queue capacity an empty queue keeps, so one large paste does not cost memory for good.
const KEPT_QUEUE: usize = 4 * 1024;
/// The bytes of the longest request, with its length.
const LONGEST: usize = size_of::<u32>() + MAX_REQUEST;

/// Most panes whose OSC colours are kept. Past it the oldest goes back to the theme's, so
/// panes that closed with a colour set cannot add up.
const MAX_COLOURED: usize = 256;
/// How far apart two reports of an outside agent's file time may be and still be the same.
const SLACK: Duration = Duration::from_secs(2);

const LINK: Token = Token(0);
const WAKE: Token = Token(1);

/// Why a link ended. This holds sizes and tags, never pane contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Disconnect {
    /// The core closed the link between messages.
    #[error("the core closed the link")]
    Closed,
    /// The core sent something the protocol refuses, or closed in the middle of a message.
    #[error("the core broke the protocol: {0}")]
    Protocol(#[from] Error),
    /// The socket or its poller failed.
    #[error("the link failed: {0}")]
    Io(ErrorKind),
}

/// A pane the active tab shows.
#[derive(Debug, Default)]
pub struct Screen {
    /// The pane.
    pub pane: u32,
    /// Where it goes in the space for panes.
    pub rect: Rect,
    /// The last screen received, which is empty until the first frame.
    pub frame: Frame,
    /// The core's number for the first row of `frame`.
    pub top: usize,
    /// The rows the view can reach.
    pub held: Range<usize>,
    /// The cursor colour and the default background the pane's program set with OSC 12 and
    /// OSC 11, each nothing while the theme's shows (ADR-0049, section 4).
    pub colours: (Option<u32>, Option<u32>),
    /// Whether `frame` or `colours` changed since this was last cleared.
    pub dirty: bool,
    /// Whether a full frame is on its way, so a burst of changes asks once.
    asked_full: bool,
}

/// Makes `screens` hold the panes of `shown`, in new rectangles. A pane that stays keeps what it
/// holds, one that went is freed, and a new one starts empty.
pub fn follow(screens: &mut Vec<Screen>, shown: impl Iterator<Item = (u32, Rect)>) {
    let shown: Vec<_> = shown.collect();
    screens.retain(|screen| shown.iter().any(|(pane, _)| *pane == screen.pane));
    for (pane, rect) in shown {
        match screens.iter_mut().find(|screen| screen.pane == pane) {
            Some(screen) => screen.rect = rect,
            None => screens.push(Screen {
                pane,
                rect,
                ..Screen::default()
            }),
        }
    }
}

/// A tab, as the tab strip shows it.
#[derive(Debug, Default)]
pub struct Tab {
    /// The pane with the tab's focus.
    pub pane: u32,
    /// That pane's last reported directory, as program output wrote it. Empty if none.
    pub cwd: String,
    /// That pane's state, below [`pinfold_proto::link::STATES`], and whether its agent reported
    /// it rather than output's marks. Nothing until the core says.
    pub status: Option<(u8, bool)>,
    /// The message its agent reported, cleaned by the core. Empty if none.
    pub message: String,
    /// Its OSC 9;4 progress byte: its focused pane's, or else the furthest along of its panes,
    /// 0 for none (ADR-0051 §9).
    pub progress: u8,
}

/// The progress byte to show for a group, as a tab shows its panes' and the Dock tile its tabs':
/// the focused one's, or else the furthest along, one with no percent before none (ADR-0051 §9).
///
/// ```
/// use pinfold_proto::link::{NORMAL, progress};
/// use pinfold_ui::link::progress_of;
///
/// let (low, high) = (progress(NORMAL, 20), progress(NORMAL, 80));
/// assert_eq!(progress_of(low, [low, high].into_iter()), low);
/// assert_eq!(progress_of(0, [low, high, 0].into_iter()), high);
/// ```
#[must_use]
pub fn progress_of(focused: u8, all: impl Iterator<Item = u8>) -> u8 {
    if focused != 0 {
        return focused;
    }
    let rank = |progress: &u8| (*progress != 0, progress_parts(*progress).1);
    all.max_by_key(rank).unwrap_or(0)
}

/// A pane in the inbox (ADR-0051), as its last [`Notice::Status`] said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// The pane.
    pub pane: u32,
    /// Its state, below [`pinfold_proto::link::STATES`].
    pub state: u8,
    /// Whether its agent reported the state, rather than output's marks.
    pub reported: bool,
    /// When it came to that state, on this window's clock.
    pub since: Instant,
    /// When its program started, on this window's clock.
    pub started: Instant,
    /// The message its agent reported, cleaned by the core. Empty if none.
    pub message: String,
    /// The agent profile it was launched from, as an index into the config's profiles.
    pub profile: Option<u8>,
    /// Its git branch from the core's last [`Notice::InboxExtra`], printable ASCII. Empty while
    /// the inbox is closed, which frees it (ADR-0051 §7).
    pub branch: String,
    /// Its agent's context use, a whole percent, from the core's last [`Notice::InboxExtra`]
    /// with `[inbox] agent_files` on. None while the inbox is closed.
    pub ctx: Option<u8>,
    /// Its agent's last steps, oldest first, joined by newlines, each already clean. Empty
    /// while the inbox is closed, which frees them (ADR-0051 §7).
    pub steps: String,
    /// Its agent run's counters from the core's last [`Notice::Summary`], none for a pane with
    /// no run (ADR-0052 §6).
    pub run: Option<RunSummary>,
}

/// What the core sent the open timeline (ADR-0052 §6): numbers, and a label already held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Timeline {
    /// A page of at most [`pinfold_proto::link::MAX_PAGE`] entries, oldest first.
    Page {
        /// The run's pane.
        pane: u32,
        /// Seconds since the run started.
        started: u32,
        /// Entries the ring dropped.
        dropped: u32,
        /// Entries the run wrote.
        total: u32,
        /// The entries, as the core sent them.
        entries: Vec<[u8; ENTRY]>,
    },
    /// An entry the watched run wrote.
    Entry {
        /// The run's pane.
        pane: u32,
        /// Its `seq`.
        seq: u32,
        /// The entry, as the core sent it.
        entry: [u8; ENTRY],
    },
    /// A command's label, cleaned and cut as the overlay holds it.
    Label {
        /// The pane.
        pane: u32,
        /// The command's stored row.
        row: u32,
        /// Whether the row is still held.
        held: bool,
        /// The text, empty when not held.
        text: String,
    },
}

/// An agent run's counters, as [`Notice::Summary`] told them: fixed numbers, never text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Whether the run has ended.
    pub ended: bool,
    /// Commands closed.
    pub commands: u32,
    /// Of those, with a non-zero exit.
    pub failed: u32,
    /// Approvals answered.
    pub approvals: u32,
    /// Of those, denied.
    pub denied: u32,
    /// Paths the newest checkpoint changed since launch.
    pub changed: u32,
    /// Failed commands plus ✕ error states.
    pub errors: u32,
}

impl RunSummary {
    /// The counters in PinFold's words, a zero left out (ADR-0052 §5):
    /// `12 commands · 3 approvals · 8 files changed · 1 error`. Empty when all are zero.
    pub fn line(&self) -> String {
        let counts = [
            (self.commands, "command", "commands"),
            (self.approvals, "approval", "approvals"),
            (self.changed, "file changed", "files changed"),
            (self.errors, "error", "errors"),
        ];
        let said = counts.into_iter().filter(|(count, ..)| *count > 0);
        let said = said.map(|(count, one, many)| {
            let word = if count == 1 { one } else { many };
            format!("{count} {word}")
        });
        said.collect::<Vec<_>>().join(" · ")
    }
}

/// An agent outside PinFold that the open inbox lists after its own rows (ADR-0051 §8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outside {
    /// Its folder as its session file names it: a hint, absolute and plain, drawn escaped.
    pub dir: String,
    /// When its session file changed, on this window's clock.
    pub changed: Instant,
}

/// An approval copied out of the decoder's borrowed message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// Stable prompt id.
    pub id: u64,
    /// Pane asking.
    pub caller: u32,
    /// Target pane.
    pub target: u32,
    /// Cross-pane action.
    pub action: ApprovalAction,
    /// Exact batched sends, empty for reads.
    pub texts: Vec<String>,
}

impl Approval {
    /// Whether `other` is this prompt, possibly grown by more requests. Trust and owner prompts
    /// never grow, so each is its own.
    #[must_use]
    pub fn same_batch(&self, other: &Self) -> bool {
        if self.action.is_once() || other.action.is_once() {
            return self.id == other.id;
        }
        (self.caller, self.target, self.action) == (other.caller, other.target, other.action)
    }
}

/// What the window knows of the session, and what it has to say.
#[derive(Default)]
struct State {
    /// Encoded requests the socket has not taken yet.
    out: Vec<u8>,
    screens: Vec<Screen>,
    tabs: Vec<Tab>,
    active: usize,
    focused: u32,
    total: usize,
    /// Whether the session's shape, a tab's name or its news changed since it was last read.
    laid: bool,
    /// Which tabs have news, one bit each.
    activity: u32,
    /// Which tabs hold a pane that needs you and raised an alert while hidden, one bit each.
    alerts: u32,
    greeted: bool,
    /// Whether the core said no tab is left.
    over: bool,
    searched: Option<(u32, Option<Selection>)>,
    copied: Option<String>,
    /// The latest answer to a [`Request::Link`]: the pane, the cell and the URI, empty for none.
    linked: Option<(u32, Position, String)>,
    /// The last markdown listing: the pane, and its escaped names one a line.
    markdown: Option<(u32, String)>,
    /// What the core sent the open timeline, until the window takes it (ADR-0052 §5).
    timeline: Vec<Timeline>,
    /// Copies asked for and not answered.
    copying: u32,
    raised: bool,
    /// Set when the core lacks the hardened runtime: whether it could tell (ADR-0044).
    unhardened: Option<bool>,
    /// How many secret-looking variables a sandboxed agent can read on macOS, and the first few
    /// names (ADR-0047).
    secrets: Option<(u32, String)>,
    /// A configured webhook's host, whether `curl` was found and how many alerts were given up
    /// on, until the window takes it (ADR-0051).
    webhook: Option<(String, bool, u32)>,
    /// Whether the core is too old to take a palette, so it draws Graphite Grid (ADR-0049).
    unthemed: bool,
    /// The OSC 12 and OSC 11 colours of each pane that has either, shown or not.
    colours: Vec<(u32, Option<u32>, Option<u32>)>,
    refused: Option<Refusal>,
    /// Restored panes, whether Enter may run what each was running, that command, and its
    /// run's summary line, empty if none.
    was_running: Vec<(u32, bool, String, String)>,
    approvals: Vec<Approval>,
    /// Each pane of the session and its tab's index, from a core of 17 or later.
    members: Vec<(u32, u8)>,
    /// The inbox's panes, at most [`MAX_PANES`].
    listed: Vec<Listed>,
    /// The agents outside PinFold the open inbox lists, at most 16. Empty
    /// while it is closed.
    outside: Vec<Outside>,
    /// Whether `listed`, `members` or `outside` changed since they were last read.
    relisted: bool,
    /// Whether the window last said its inbox is open.
    inbox: bool,
    /// Listed panes that came to need you or to an error, and that state, oldest first, at
    /// most [`MAX_PANES`], for the window's desktop alerts (ADR-0051).
    alerting: Vec<(u32, u8)>,
    /// Whether a go to the next pane that needs you found none.
    none_waiting: bool,
    /// The latest program notification, pane and text, until the window takes it: one at a
    /// time, at most 480 bytes (ADR-0051 §9).
    toast: Option<(u32, String)>,
    /// Each run's pane whose strip changed and why it has no checkpoint, until the window takes
    /// it, at most [`MAX_PANES`] (ADR-0050).
    checkpoints: Vec<(u32, u8)>,
    /// Each pane with OSC 9;4 progress and its byte, never 0, at most [`MAX_PANES`] (ADR-0051
    /// §9). Empty while no pane has any.
    progress: Vec<(u32, u8)>,
    /// How the last restore the window allowed ended, until the window takes it.
    restored: Option<(u32, u32, String)>,
}

impl State {
    /// Adds a request to the queue, or returns false and leaves the queue as it was.
    fn queue(&mut self, request: Request<'_>) -> bool {
        let start = self.out.len();
        if request.encode(&mut self.out).is_err() {
            return false;
        }
        if self.out.len() > MAX_QUEUE {
            self.out.truncate(start);
            return false;
        }
        true
    }

    fn apply(&mut self, notice: Notice<'_>) -> Result<(), Error> {
        match notice {
            Notice::Hello { .. } => self.greeted = true,
            Notice::Frame(update) => {
                // The window adds screen rows to this without checking.
                if update.top.checked_add(u16::MAX.into()).is_none() {
                    return Err(Error::OutOfBounds {
                        field: "top",
                        value: update.top as u64,
                    });
                }
                let mut shown = self.screens.iter_mut();
                let Some(screen) = shown.find(|screen| screen.pane == update.pane) else {
                    return Ok(());
                };
                if update.apply(&mut screen.frame) {
                    screen.asked_full &= !update.full;
                    screen.top = update.top;
                    screen.held = update.held;
                    screen.dirty = true;
                } else if !screen.asked_full {
                    // A message this small only misses a queue that is full, and then asks again.
                    screen.asked_full = true;
                    let pane = screen.pane;
                    let queued = self.queue(Request::FullFrame { pane });
                    if let Some(screen) = self.screens.iter_mut().find(|s| s.pane == pane) {
                        screen.asked_full = queued;
                    }
                }
            }
            Notice::Layout(layout) => self.follow(&layout),
            Notice::Cwd { pane, path } => {
                let mut tabs = self.tabs.iter_mut();
                if let Some(tab) = tabs.find(|tab| tab.pane == pane && tab.cwd != path) {
                    path.clone_into(&mut tab.cwd);
                    self.laid = true;
                }
            }
            Notice::Searched { pane, found } => self.searched = Some((pane, found)),
            // Only a key copies, so a core cannot fill the clipboard on its own.
            Notice::Copied { text, .. } if self.copying > 0 => {
                self.copying -= 1;
                self.copied = Some(text.to_owned());
            }
            Notice::Linked { pane, at, uri } => self.linked = Some((pane, at, uri.to_owned())),
            Notice::Markdown { pane, names } => self.markdown = Some((pane, names.to_owned())),
            Notice::Raise => self.raised = true,
            Notice::Unhardened { checked } => self.unhardened = Some(checked),
            Notice::Secrets { count, names } => self.secrets = Some((count, names.to_owned())),
            Notice::Webhook {
                host,
                sender,
                failed,
            } => {
                self.webhook = Some((shown_host(host), sender, failed));
            }
            Notice::Colours {
                pane,
                cursor,
                background,
            } => {
                let colours = (cursor, background);
                self.colours.retain(|(other, ..)| *other != pane);
                if colours != (None, None) {
                    if self.colours.len() == MAX_COLOURED {
                        self.colours.remove(0);
                    }
                    self.colours.push((pane, cursor, background));
                }
                if let Some(screen) = self.screens.iter_mut().find(|s| s.pane == pane) {
                    (screen.colours, screen.dirty) = (colours, true);
                }
            }
            Notice::Activity { tabs, alerts } => {
                (self.activity, self.alerts) = (tabs, alerts);
                self.laid = true;
            }
            Notice::Status {
                pane,
                state,
                reported,
                naming,
                listed,
                since,
                started,
                profile,
                message,
            } => {
                let tab = self.tabs.iter_mut().find(|tab| tab.pane == pane);
                if naming && let Some(tab) = tab {
                    tab.status = Some((state, reported));
                    message.clone_into(&mut tab.message);
                    self.laid = true;
                }
                let times = (since, started);
                self.list(pane, listed.then_some((state, reported, times, message)));
                if let Some(listed) = self.listed.iter_mut().find(|listed| listed.pane == pane) {
                    listed.profile = profile;
                }
            }
            // Kept only while the inbox is open; one that comes after it closed is dropped.
            Notice::InboxExtra {
                pane,
                branch,
                ctx,
                steps,
            } if self.inbox => {
                let listed = self.listed.iter_mut().find(|listed| listed.pane == pane);
                let changed = |listed: &&mut Listed| {
                    (listed.branch.as_str(), listed.ctx, listed.steps.as_str())
                        != (branch, ctx, steps)
                };
                if let Some(listed) = listed.filter(changed) {
                    branch.clone_into(&mut listed.branch);
                    steps.clone_into(&mut listed.steps);
                    listed.ctx = ctx;
                    self.relisted = true;
                }
            }
            // Kept only while the inbox is open, as its extras are.
            Notice::Outside {
                index,
                count,
                secs,
                dir,
            } if self.inbox => {
                let (index, count) = (usize::from(index), usize::from(count));
                let now = Instant::now();
                let changed = now.checked_sub(Duration::from_secs(secs.into()));
                let changed = changed.unwrap_or(now);
                if self.outside.len() > count {
                    self.outside.truncate(count);
                    self.relisted = true;
                }
                // Whole seconds arrive up to a second late, so an unchanged file is no news.
                let same = |row: &Outside| {
                    row.dir == dir && row.changed.max(changed) - row.changed.min(changed) <= SLACK
                };
                let next = self.outside.len();
                match self.outside.get_mut(index) {
                    Some(row) if same(row) => {}
                    Some(row) => {
                        dir.clone_into(&mut row.dir);
                        row.changed = changed;
                        self.relisted = true;
                    }
                    None if index == next && index < count => {
                        let dir = dir.to_owned();
                        self.outside.push(Outside { dir, changed });
                        self.relisted = true;
                    }
                    None => {}
                }
            }
            Notice::Refused { reason } => self.refused = Some(reason),
            Notice::Toast { pane, text, .. } => self.toast = Some((pane, text.to_owned())),
            Notice::Progress { pane, progress } => {
                self.progress.retain(|(other, _)| *other != pane);
                if progress != 0 && self.progress.len() < MAX_PANES {
                    self.progress.push((pane, progress));
                }
                self.retab_progress();
            }
            Notice::Checkpoint { pane, missing } => {
                self.checkpoints.retain(|(other, _)| *other != pane);
                if self.checkpoints.len() < MAX_PANES {
                    self.checkpoints.push((pane, missing));
                }
            }
            Notice::Restored {
                undid,
                skipped,
                error,
            } => self.restored = Some((undid, skipped, error.to_owned())),
            // The core restores at most this many panes, once.
            Notice::WasRunning {
                pane,
                runnable,
                command,
                summary,
            } if self.was_running.len() < MAX_PANES => {
                // PinFold's words for the run's counters, never text from the snapshot.
                let summary = summary.map(|counts| RunSummary {
                    ended: true,
                    commands: counts.commands,
                    failed: counts.failed,
                    approvals: counts.approvals,
                    denied: counts.denied,
                    changed: counts.changed,
                    errors: counts.errors,
                });
                let line = summary.map(|run| run.line()).unwrap_or_default();
                (self.was_running).push((pane, runnable, command.to_owned(), line));
            }
            Notice::Approval(approval) => {
                let owned = Approval {
                    id: approval.id,
                    caller: approval.caller,
                    target: approval.target,
                    action: approval.action,
                    texts: approval
                        .texts
                        .iter()
                        .map(|text| (*text).to_owned())
                        .collect(),
                };
                // A batch that grew comes under a new id; it replaces the one shown.
                match self
                    .approvals
                    .iter_mut()
                    .find(|known| known.same_batch(&owned))
                {
                    Some(known) => *known = owned,
                    None => self.approvals.push(owned),
                }
            }
            // Kept on the pane's inbox row; the core lists every pane with a run first.
            Notice::Summary {
                pane,
                ended,
                commands,
                failed,
                approvals,
                denied,
                changed,
                errors,
                ..
            } => {
                let run = Some(RunSummary {
                    ended,
                    commands,
                    failed,
                    approvals,
                    denied,
                    changed,
                    errors,
                });
                let listed = self.listed.iter_mut().find(|listed| listed.pane == pane);
                if let Some(listed) = listed.filter(|listed| listed.run != run) {
                    listed.run = run;
                    self.relisted = true;
                }
            }
            // Only the open overlay asks, and it holds what comes; this keeps it only until the
            // window takes it.
            Notice::Timeline {
                pane,
                started,
                dropped,
                total,
                entries,
                ..
            } => {
                let entries = entries.as_chunks::<ENTRY>().0.to_vec();
                (self.timeline).push(Timeline::Page {
                    pane,
                    started,
                    dropped,
                    total,
                    entries,
                });
            }
            Notice::TimelineLabel {
                pane,
                row,
                held,
                text,
            } => self.timeline.push(Timeline::Label {
                pane,
                row,
                held,
                text: crate::timeline::hold(text),
            }),
            Notice::TimelineEntry { pane, seq, entry } => {
                self.timeline.push(Timeline::Entry { pane, seq, entry });
            }
            Notice::Copied { .. }
            | Notice::WasRunning { .. }
            | Notice::InboxExtra { .. }
            | Notice::Outside { .. } => {}
        }
        Ok(())
    }

    /// Keeps a pane's inbox row, or drops it for `None`. The core's seconds become an
    /// `Instant`, so the two sides share no clock.
    fn list(&mut self, pane: u32, status: Option<(u8, bool, (u32, u32), &str)>) {
        let index = self.listed.iter().position(|listed| listed.pane == pane);
        let Some((state, reported, (since, started), message)) = status else {
            if let Some(index) = index {
                self.listed.swap_remove(index);
                self.relisted = true;
            }
            return;
        };
        let old = index.map(|index| self.listed[index].state);
        let now = Instant::now();
        let ago = |seconds: u32| {
            now.checked_sub(Duration::from_secs(seconds.into()))
                .unwrap_or(now)
        };
        let (since, started) = (ago(since), ago(started));
        let index = match index {
            Some(index) => index,
            // The core never lists more, so a flood of made-up panes stops here.
            None if self.listed.len() < MAX_PANES => {
                self.listed.push(Listed {
                    pane,
                    state,
                    reported,
                    since,
                    started,
                    message: String::new(),
                    profile: None,
                    branch: String::new(),
                    ctx: None,
                    steps: String::new(),
                    run: None,
                });
                self.listed.len() - 1
            }
            None => return,
        };
        let listed = &mut self.listed[index];
        // A pane first heard of comes to its state too, as on a window's first link.
        let came = old != Some(state) && matches!(state, NEEDS_YOU | ERROR);
        if came && self.alerting.len() < MAX_PANES {
            self.alerting.push((pane, state));
        }
        (listed.state, listed.reported) = (state, reported);
        (listed.since, listed.started) = (since, started);
        message.clone_into(&mut listed.message);
        self.relisted = true;
    }

    fn follow(&mut self, layout: &Layout<'_>) {
        self.active = layout.active.into();
        self.focused = layout.focused;
        self.total = layout.total.into();
        self.over = layout.is_empty();
        self.laid = true;
        follow(&mut self.screens, layout.panes());
        self.members.clear();
        self.members.extend(layout.members());
        let members = &self.members;
        self.listed
            .retain(|listed| members.iter().any(|(pane, _)| *pane == listed.pane));
        // A pane may have moved to another tab's number.
        self.relisted = true;
        // A pane of a tab that was hidden may have set its colours then.
        for screen in &mut self.screens {
            let mut kept = self.colours.iter();
            let kept = kept.find(|(pane, ..)| *pane == screen.pane);
            screen.colours = kept.map_or((None, None), |(_, cursor, ground)| (*cursor, *ground));
        }
        let mut known = std::mem::take(&mut self.tabs);
        self.tabs.extend(layout.tabs().map(|pane| {
            let same = known.iter_mut().find(|tab| tab.pane == pane);
            same.map_or(
                Tab {
                    pane,
                    ..Tab::default()
                },
                std::mem::take,
            )
        }));
        // A pane that went takes its progress with it.
        self.progress
            .retain(|(pane, _)| members.iter().any(|(other, _)| other == pane));
        self.retab_progress();
    }

    /// Sets each tab's progress: its focused pane's, or else the furthest along of its panes'.
    /// A change is news for the tab strip.
    fn retab_progress(&mut self) {
        let of = |pane: u32| {
            let found = self.progress.iter().find(|(other, _)| *other == pane);
            found.map_or(0, |(_, progress)| *progress)
        };
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let panes = self
                .members
                .iter()
                .filter(|(_, at)| usize::from(*at) == index);
            let progress = progress_of(of(tab.pane), panes.map(|(pane, _)| of(*pane)));
            if tab.progress != progress {
                tab.progress = progress;
                self.laid = true;
            }
        }
    }
}

/// A session of the core, seen through the link. A single thread drives it, as with `Reactor`.
pub struct Link {
    /// Gone once the link ends, so the core sees it close.
    stream: Option<UnixStream>,
    poll: Poll,
    waker: Arc<Waker>,
    events: Events,
    decoder: Decoder,
    buffer: Vec<u8>,
    /// The socket may hold more than the last pass read. Polling is edge-triggered.
    readable: bool,
    /// Whether the decoder holds part of a message.
    partial: bool,
    /// Whether [`Link::greet`] waits for the core's hello to answer it.
    greeting: bool,
    /// The theme's palette, until it has followed the hello.
    palette: Option<Palette>,
    /// `[alerts] program_notifications`, until it has followed the hello.
    toasts: Option<bool>,
    /// Whether the hello has gone, so a palette no longer waits for it.
    answered: bool,
    ended: Option<Disconnect>,
    state: State,
}

impl Link {
    /// Takes over a connected socket, and says hello at once. The core says what to show once
    /// [`Link::resize`] says how much room there is.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be set up or polled.
    pub fn new(fd: OwnedFd) -> io::Result<Self> {
        let mut link = Self::silent(fd)?;
        link.greet();
        Ok(link)
    }

    /// Takes over a connected socket, and sends nothing until [`Link::greet`].
    ///
    /// # Errors
    ///
    /// Returns what [`Link::new`] returns.
    pub fn silent(fd: OwnedFd) -> io::Result<Self> {
        // The copy is close-on-exec, as ADR-0028 asks of an inherited descriptor.
        let stream = UnixStream::from(fd.try_clone()?);
        drop(fd);
        stream.set_nonblocking(true)?;
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        let interest = Interest::READABLE | Interest::WRITABLE;
        poll.registry()
            .register(&mut SourceFd(&stream.as_raw_fd()), LINK, interest)?;
        let state = State::default();
        Ok(Self {
            stream: Some(stream),
            poll,
            waker,
            events: Events::with_capacity(4),
            decoder: Decoder::default(),
            buffer: vec![0; READ_BUDGET],
            readable: true,
            partial: false,
            greeting: false,
            palette: None,
            toasts: None,
            answered: false,
            ended: None,
            state,
        })
    }

    /// Sends the version check, at the lower of this version and the core's, once the core's
    /// has come (ADR-0043). The core tells a launcher a window has opened on this.
    pub fn greet(&mut self) {
        self.greeting = true;
        self.answer();
        self.flush();
    }

    /// Sends the theme's `palette`, or keeps it to send right after the hello if that has not
    /// gone, so the latest theme is the one the core gets (ADR-0049). A core that speaks
    /// a version without it is sent none, and [`Link::take_unthemed`] then says so.
    pub fn theme(&mut self, palette: Palette) {
        if self.answered {
            // A core too old for it is sent none, and the window already draws Graphite Grid.
            self.send(Request::Palette(palette));
        } else {
            self.palette = Some(palette);
        }
    }

    /// Says whether `[alerts] program_notifications` is on, or keeps it to send right after the
    /// hello, as a palette is. A core too old for it is sent none and keeps them off.
    pub fn toasts(&mut self, on: bool) {
        if self.answered {
            self.send(Request::Toasts { on });
        } else {
            self.toasts = Some(on);
        }
    }

    /// Puts the hello, and the palette and toasts after it, ahead of any request queued while it
    /// waited for the core's.
    fn answer(&mut self) {
        if let (true, Some(version)) = (self.greeting, self.decoder.version()) {
            let mut hello = Vec::new();
            let mut encoded = Request::Hello { version }.encode(&mut hello);
            if let Some(palette) = self.palette.take().map(Request::Palette) {
                if palette.since() <= version {
                    encoded = encoded.and_then(|()| palette.encode(&mut hello));
                } else {
                    self.state.unthemed = true;
                }
            }
            let toasts = self.toasts.take().map(|on| Request::Toasts { on });
            if let Some(toasts) = toasts.filter(|toasts| toasts.since() <= version) {
                encoded = encoded.and_then(|()| toasts.encode(&mut hello));
            }
            if encoded.is_ok() {
                self.state.out.splice(..0, hello);
            }
            self.greeting = false;
            self.answered = true;
        }
    }

    /// Waits for the core for up to `timeout`, reads up to one budget and sends what it can.
    ///
    /// # Errors
    ///
    /// Returns why the link ended, at once and on every later call, so a loop cannot block on a
    /// dead link. The window can still read the last screen, and a new [`Link`] starts over.
    pub fn turn(&mut self, timeout: Option<Duration>) -> Result<(), Disconnect> {
        if let Some(reason) = self.ended {
            return Err(reason);
        }
        let timeout = if self.readable {
            Some(Duration::ZERO)
        } else {
            timeout
        };
        match self.poll.poll(&mut self.events, timeout) {
            Err(error) if error.kind() == ErrorKind::Interrupted => return Ok(()),
            Err(error) => self.end(Disconnect::Io(error.kind())),
            Ok(()) => {
                self.readable |= self.events.iter().any(|event| event.token() == LINK);
                if self.readable {
                    self.read();
                }
                self.flush();
            }
        }
        self.ended.map_or(Ok(()), Err)
    }

    fn read(&mut self) {
        let mut left = self.buffer.len();
        while left > 0 {
            let Some(mut stream) = self.stream.as_ref() else {
                return;
            };
            match stream.read(&mut self.buffer[..left]) {
                // Linux reports a reset if the core closed with requests unread.
                Ok(0) => return self.closed(),
                Err(error) if error.kind() == ErrorKind::ConnectionReset => return self.closed(),
                Ok(count) => {
                    left -= count;
                    if let Err(error) = self.decode(count) {
                        // The core logs a gap of two or more and offers a restart (#203).
                        if let (Error::Version { .. }, Some(mut stream)) =
                            (error, self.stream.as_ref())
                        {
                            let mut hello = Vec::new();
                            let _ = Request::Hello { version: VERSION }.encode(&mut hello);
                            let _ = stream.write(&hello);
                        }
                        return self.end(error.into());
                    }
                    self.answer();
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    self.readable = false;
                    return;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return self.end(Disconnect::Io(error.kind())),
            }
        }
    }

    /// Applies each message in the first `count` bytes of the buffer.
    fn decode(&mut self, count: usize) -> Result<(), Error> {
        let mut input = &self.buffer[..count];
        loop {
            let before = input.len();
            match self.decoder.next_notice(&mut input)? {
                Some(notice) => self.state.apply(notice)?,
                None => {
                    self.partial = before > 0;
                    return Ok(());
                }
            }
        }
    }

    fn closed(&mut self) {
        self.end(if self.partial {
            Error::Truncated.into()
        } else {
            Disconnect::Closed
        });
    }

    /// Closes the socket, which is how the core learns of a bad message. Only the first reason
    /// counts.
    fn end(&mut self, reason: Disconnect) {
        self.ended.get_or_insert(reason);
        self.state.out = Vec::new();
        if let Some(stream) = self.stream.take() {
            let fd = stream.as_raw_fd();
            let _ = self.poll.registry().deregister(&mut SourceFd(&fd));
        }
    }

    /// Writes what it can without blocking, and keeps the rest for the next turn.
    fn flush(&mut self) {
        if self.greeting {
            return;
        }
        // The runtime ignores SIGPIPE, so a closed core shows up here as an error.
        while let (Some(mut stream), false) = (self.stream.as_ref(), self.state.out.is_empty()) {
            match stream.write(&self.state.out) {
                Ok(0) => return,
                Ok(count) => {
                    self.state.out.drain(..count);
                }
                Err(error) => match error.kind() {
                    ErrorKind::WouldBlock => return,
                    ErrorKind::Interrupted => {}
                    // The core may have said why before it closed, so a read decides what ended.
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
                        self.state.out.clear();
                        self.readable = true;
                    }
                    kind => self.end(Disconnect::Io(kind)),
                },
            }
        }
        if self.state.out.is_empty() && self.state.out.capacity() > KEPT_QUEUE {
            self.state.out = Vec::new();
        }
    }

    /// Queues a request and sends what it can. Returns false if it is over the limit of one
    /// message, the core is too far behind, the link has ended, or the request is newer than
    /// the version the link speaks (ADR-0043), and then queues none.
    pub fn send(&mut self, request: Request<'_>) -> bool {
        if let Request::Inbox { open } = request {
            // Opening reads the rows afresh, and closing frees their branches and steps.
            (self.state.inbox, self.state.relisted) = (open, open);
            if !open {
                for listed in &mut self.state.listed {
                    (listed.branch, listed.ctx, listed.steps) =
                        (String::new(), None, String::new());
                }
                self.state.outside = Vec::new();
            }
        }
        // Before the core's hello this is the oldest version, so a newer request waits for none.
        let spoken = self.decoder.version().unwrap_or(OLDEST);
        let queued =
            self.stream.is_some() && request.since() <= spoken && self.state.queue(request);
        self.flush();
        queued
    }

    /// Returns whether [`Link::send`] has room for any request, however long. While it has not,
    /// a caller keeps its requests in order and asks again after [`Link::turn`], which returns
    /// when the socket can take more.
    #[must_use]
    pub fn has_room(&self) -> bool {
        self.state.out.len() + LONGEST <= MAX_QUEUE
    }

    /// Returns a handle that makes a blocked [`Link::turn`] return, from any thread.
    #[must_use]
    pub fn waker(&self) -> Arc<Waker> {
        Arc::clone(&self.waker)
    }

    /// Gives the space for panes, clamped to what the core accepts. Returns what [`Link::send`]
    /// returns.
    pub fn resize(&mut self, rows: u16, columns: u16) -> bool {
        self.send(Request::Resize {
            rows: rows.clamp(1, MAX_DIMENSION),
            columns: columns.clamp(1, MAX_DIMENSION),
        })
    }

    /// Asks for a selection's text, for [`Link::take_copied`]. Returns what [`Link::send`]
    /// returns.
    pub fn copy(&mut self, pane: u32, selection: Selection) -> bool {
        let queued = self.send(Request::Copy { pane, selection });
        self.state.copying = self.state.copying.saturating_add(queued.into());
        queued
    }

    /// Returns the panes the active tab shows.
    #[must_use]
    pub fn screens(&self) -> &[Screen] {
        &self.state.screens
    }

    /// Returns the panes the active tab shows, to clear what has been read.
    pub fn screens_mut(&mut self) -> &mut [Screen] {
        &mut self.state.screens
    }

    /// Returns the tabs, in order.
    #[must_use]
    pub fn tabs(&self) -> &[Tab] {
        &self.state.tabs
    }

    /// Returns each pane of the session and its tab's index, tab by tab. Empty from a core
    /// older than 17.
    #[must_use]
    pub fn members(&self) -> &[(u32, u8)] {
        &self.state.members
    }

    /// Returns the inbox's panes, in no order.
    #[must_use]
    pub fn listed(&self) -> &[Listed] {
        &self.state.listed
    }

    /// Returns the agents outside PinFold the open inbox lists, newest first.
    #[must_use]
    pub fn outside(&self) -> &[Outside] {
        &self.state.outside
    }

    /// Returns whether the inbox's panes or their tabs changed since the last call, and clears
    /// that.
    pub fn take_relisted(&mut self) -> bool {
        std::mem::take(&mut self.state.relisted)
    }

    /// Returns the next pane that needs you, and its tab's index: from a focused ◆ pane the
    /// one after it in the inbox's order, waiting longest first, and else the first (ADR-0051).
    /// Nothing if no listed pane needs you.
    #[must_use]
    pub fn next_waiting(&self) -> Option<(u8, u32)> {
        let state = &self.state;
        // As the inbox sorts its ◆ rows; the pane breaks a tie.
        let key = |listed: &Listed| {
            let members = &state.members;
            let index = members.iter().position(|(pane, _)| *pane == listed.pane)?;
            let tab = members[index].1;
            let place = members[..index].iter().filter(|(_, t)| *t == tab).count();
            Some((listed.since, tab, place, listed.pane))
        };
        let waiting = || {
            let listed = state.listed.iter();
            listed
                .filter(|listed| listed.state == NEEDS_YOU)
                .filter_map(key)
        };
        let focused = waiting().find(|(.., pane)| *pane == state.focused);
        let after = focused.and_then(|focused| waiting().filter(|key| *key > focused).min());
        let (_, tab, _, pane) = after.or_else(|| waiting().min())?;
        Some((tab, pane))
    }

    /// Goes to the next pane that needs you, as [`Link::next_waiting`] finds it, or notes that
    /// none does for [`Link::take_none_waiting`]. Returns what [`Link::send`] returns.
    pub fn go_to_next_waiting(&mut self) -> bool {
        match self.next_waiting() {
            Some((tab, pane)) => self.go_to(tab, pane),
            None => {
                self.state.none_waiting = true;
                true
            }
        }
    }

    /// Goes to `pane`, wherever its tab is: the switch to the tab, then the focus. Nothing for a
    /// pane the session no longer holds. Returns what [`Link::send`] returns.
    pub fn go_to_pane(&mut self, pane: u32) -> bool {
        match self.tab_of(pane) {
            Some(tab) => self.go_to(tab, pane),
            None => true,
        }
    }

    fn go_to(&mut self, tab: u8, pane: u32) -> bool {
        self.send(Request::SwitchTab { index: tab.into() }) && self.send(Request::Focus { pane })
    }

    /// Returns the index of `pane`'s tab, or nothing if the session does not hold it.
    fn tab_of(&self, pane: u32) -> Option<u8> {
        let mut members = self.state.members.iter();
        members.find(|(at, _)| *at == pane).map(|(_, tab)| *tab)
    }

    /// Returns whether a go to the next pane that needs you found none, once.
    pub fn take_none_waiting(&mut self) -> bool {
        std::mem::take(&mut self.state.none_waiting)
    }

    /// Takes the oldest listed pane that came to need you or to an error: the pane, the state
    /// and its tab's index, or no tab if the session no longer holds it.
    pub fn take_alert(&mut self) -> Option<(u32, u8, Option<u8>)> {
        let alerting = &mut self.state.alerting;
        let (pane, state) = (!alerting.is_empty()).then(|| alerting.remove(0))?;
        Some((pane, state, self.tab_of(pane)))
    }

    /// Takes the latest program notification: the pane, its text and its tab's index, or no tab
    /// if the session no longer holds it.
    pub fn take_toast(&mut self) -> Option<(u32, String, Option<u8>)> {
        let (pane, text) = self.state.toast.take()?;
        Some((pane, text, self.tab_of(pane)))
    }

    /// Returns whether the window's inbox is open, as the last [`Request::Inbox`] said.
    #[must_use]
    pub fn inbox_open(&self) -> bool {
        self.state.inbox
    }

    /// Returns whether the core speaks a version that carries `request`.
    #[must_use]
    pub fn speaks(&self, request: &Request<'_>) -> bool {
        self.decoder.version() >= Some(request.since())
    }

    /// Returns the index of the active tab.
    #[must_use]
    pub fn active(&self) -> usize {
        self.state.active
    }

    /// Returns the pane keys go to, which is 0 until the first layout.
    #[must_use]
    pub fn focused(&self) -> u32 {
        self.state.focused
    }

    /// Returns how many panes the session holds, shown or not.
    #[must_use]
    pub fn total(&self) -> usize {
        self.state.total
    }

    /// Returns which tabs printed something since they were shown, one bit each.
    #[must_use]
    pub fn activity(&self) -> u32 {
        self.state.activity
    }

    /// Returns which tabs hold a pane that needs you and alerted while hidden, one bit each.
    #[must_use]
    pub fn alerts(&self) -> u32 {
        self.state.alerts
    }

    /// Returns whether the tabs or the panes shown changed since the last call, and clears that.
    pub fn take_laid(&mut self) -> bool {
        std::mem::take(&mut self.state.laid)
    }

    /// Returns which pane the last search ran in and how it ended, once, and nothing while none
    /// has. Its match is in view if it had one.
    pub fn take_searched(&mut self) -> Option<(u32, Option<Selection>)> {
        self.state.searched.take()
    }

    /// Returns what restored panes were running, once each.
    pub fn take_was_running(&mut self) -> Vec<(u32, bool, String, String)> {
        std::mem::take(&mut self.state.was_running)
    }

    /// Returns the last command the core refused, once.
    pub fn take_refused(&mut self) -> Option<Refusal> {
        self.state.refused.take()
    }

    /// Takes the oldest new or updated approval prompt.
    pub fn take_approval(&mut self) -> Option<Approval> {
        (!self.state.approvals.is_empty()).then(|| self.state.approvals.remove(0))
    }

    /// Returns the text of the last copy asked for, once.
    pub fn take_copied(&mut self) -> Option<String> {
        self.state.copied.take()
    }

    /// Returns the latest answer to a [`Request::Link`], once: the pane, the cell and the URI,
    /// which is empty if the cell had no hyperlink.
    pub fn take_linked(&mut self) -> Option<(u32, Position, String)> {
        self.state.linked.take()
    }

    /// Takes what the core sent the open timeline, oldest first.
    pub fn take_timeline(&mut self) -> Vec<Timeline> {
        std::mem::take(&mut self.state.timeline)
    }

    /// Returns the markdown listing that came since the last call, if one did.
    pub fn take_markdown(&mut self) -> Option<(u32, String)> {
        self.state.markdown.take()
    }

    /// Returns whether someone asked to bring this window to the front since the last call.
    pub fn take_raised(&mut self) -> bool {
        std::mem::take(&mut self.state.raised)
    }

    /// Returns whether the core said it lacks the hardened runtime since the last call, and if
    /// so whether it could tell. A core says it once per window.
    pub fn take_unhardened(&mut self) -> Option<bool> {
        self.state.unhardened.take()
    }

    /// Returns the secret-looking variables a sandboxed agent can read on macOS, if the core
    /// said so since the last call: how many, and the first few names. It says so once a session.
    pub fn take_secrets(&mut self) -> Option<(u32, String)> {
        self.state.secrets.take()
    }

    /// Returns a configured webhook's host, whether `curl` was found and how many alerts were
    /// given up on, if the core said so since the last call: after the hello and on a change.
    pub fn take_webhook(&mut self) -> Option<(String, bool, u32)> {
        self.state.webhook.take()
    }

    /// Takes the oldest change to a run's strip: its pane and why it has no checkpoint, an index
    /// into [`pinfold_proto::link::MISSING`], 0 once it has one (ADR-0050).
    pub fn take_checkpoint(&mut self) -> Option<(u32, u8)> {
        (!self.state.checkpoints.is_empty()).then(|| self.state.checkpoints.remove(0))
    }

    /// Takes how the last restore the window allowed ended: files undone, skipped, and the
    /// core's words for a failure, empty for none (ADR-0050).
    pub fn take_restored(&mut self) -> Option<(u32, u32, String)> {
        self.state.restored.take()
    }

    /// Returns whether the core turned out too old to take the theme's palette, once. Its
    /// panes are then in Graphite Grid, and the window is to draw the same.
    pub fn take_unthemed(&mut self) -> bool {
        std::mem::take(&mut self.state.unthemed)
    }

    /// Returns whether the core has answered the version check.
    #[must_use]
    pub fn is_greeted(&self) -> bool {
        self.state.greeted
    }

    /// Returns whether the session ended, with no tab left, or the link did.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.over || self.ended.is_some()
    }

    /// Returns why the link ended, if it has. [`Link::is_closed`] alone cannot tell a session
    /// that ended from a core to reconnect to.
    #[must_use]
    pub fn disconnected(&self) -> Option<Disconnect> {
        self.ended
    }
}

/// Most characters of a webhook's host the window shows, so its notice fits 80 columns.
const SHOWN_HOST: usize = 40;

/// A webhook's host as the window shows it: only the URL rules' characters, whatever a core
/// sends, and a long one cut from the start, so the registrable domain at its end stays seen.
fn shown_host(host: &str) -> String {
    let host: Vec<char> = (host.chars())
        .filter(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(*c))
        .collect();
    match host.len().checked_sub(SHOWN_HOST) {
        Some(cut @ 1..) => std::iter::once('…')
            .chain(host[cut + 1..].iter().copied())
            .collect(),
        _ => host.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Instant;

    use pinfold_proto::Cell;
    use pinfold_proto::link::{
        Direction, MAX_MESSAGE, MAX_PASTE, Position, encode_frame, encode_layout,
    };

    use super::*;

    #[test]
    fn a_webhook_host_keeps_its_end_and_only_url_characters() {
        assert_eq!(shown_host("ntfy.sh"), "ntfy.sh");
        assert_eq!(shown_host("[::1]"), "[::1]");
        assert_eq!(shown_host("a\u{202e}b\x1b[2Jc /d"), "ab[2Jcd");
        let spoof = format!("ntfy.sh.{}.evil.com", "a".repeat(60));
        let shown = shown_host(&spoof);
        assert_eq!(shown.chars().count(), SHOWN_HOST);
        assert!(
            shown.starts_with('…') && shown.ends_with(".evil.com"),
            "{shown}"
        );
        assert_eq!(shown_host(&"h".repeat(SHOWN_HOST)), "h".repeat(SHOWN_HOST));
    }

    const PANE: u32 = 3;
    const PATIENCE: Duration = Duration::from_secs(10);

    /// Each tab's focused pane in its tab, the active tab's panes, and made-up panes in the first
    /// tab up to `total`.
    fn members(active: u16, total: u16, tabs: &[u32], panes: &[(u32, Rect)]) -> Vec<(u32, u8)> {
        let mut members: Vec<(u32, u8)> = Vec::new();
        let named = tabs
            .iter()
            .enumerate()
            .map(|(index, &pane)| (pane, index as u8));
        let shown = panes.iter().map(|&(pane, _)| (pane, active as u8));
        for (pane, tab) in named.chain(shown) {
            if members.iter().all(|(known, _)| *known != pane) {
                members.push((pane, tab));
            }
        }
        let mut spare = 9000..;
        while members.len() < usize::from(total) {
            members.extend(spare.next().map(|pane| (pane, 0)));
        }
        members
    }

    /// The core's end of the link, as a test drives it.
    struct Peer {
        stream: UnixStream,
        decoder: Decoder,
    }

    impl Peer {
        fn notices(&mut self, notices: &[Notice<'_>]) -> io::Result<()> {
            let mut wire = Vec::new();
            for notice in notices {
                notice.encode(&mut wire).map_err(io::Error::other)?;
            }
            self.stream.write_all(&wire)
        }

        fn frame(&mut self, frame: &Frame, top: usize, previous: Option<&Frame>) -> io::Result<()> {
            let mut wire = Vec::new();
            encode_frame(&mut wire, PANE, frame, top, 1..9, previous).map_err(io::Error::other)?;
            self.stream.write_all(&wire)
        }

        fn no_tabs(&mut self) -> io::Result<()> {
            let mut wire = Vec::new();
            encode_layout(&mut wire, VERSION, 0, 0, 0, &[], &[], &[]).map_err(io::Error::other)?;
            self.stream.write_all(&wire)
        }

        /// Says hello, and that one tab shows `PANE`.
        fn greet(&mut self) -> io::Result<()> {
            self.notices(&[Notice::Hello { version: VERSION }])?;
            self.layout(0, PANE, 1, &[PANE], &[(PANE, whole())])
        }

        fn layout(
            &mut self,
            active: u16,
            focused: u32,
            total: u16,
            tabs: &[u32],
            panes: &[(u32, Rect)],
        ) -> io::Result<()> {
            let mut wire = Vec::new();
            let members = members(active, total, tabs, panes);
            encode_layout(
                &mut wire, VERSION, active, focused, total, tabs, panes, &members,
            )
            .map_err(io::Error::other)?;
            self.stream.write_all(&wire)
        }

        /// Reads until `count` requests have arrived, and shows them for comparing.
        fn requests(&mut self, count: usize) -> Vec<String> {
            let (mut seen, mut chunk) = (Vec::new(), vec![0; READ_BUDGET]);
            while seen.len() < count {
                let Ok(read @ 1..) = self.stream.read(&mut chunk) else {
                    break;
                };
                let mut input = &chunk[..read];
                while let Ok(Some(request)) = self.decoder.next_request(&mut input) {
                    seen.push(format!("{request:?}"));
                }
            }
            seen
        }

        /// Whether the other end has closed, which an unread message would hide.
        fn sees_close(&mut self) -> bool {
            self.stream.read_to_end(&mut Vec::new()).is_ok()
        }
    }

    fn whole() -> Rect {
        Rect {
            x: 0,
            y: 0,
            columns: 3,
            rows: 2,
        }
    }

    /// Whether the screen of `PANE` changed, which clears that.
    fn take_dirty(link: &mut Link) -> bool {
        let mut screens = link.screens_mut().iter_mut();
        screens
            .find(|screen| screen.pane == PANE)
            .is_some_and(|screen| std::mem::take(&mut screen.dirty))
    }

    fn pane(link: &Link) -> &Screen {
        &link.screens()[0]
    }

    /// A link whose core has said hello, and the window's answer read.
    fn answered() -> io::Result<(Link, Peer)> {
        let (mut link, mut peer) = linked()?;
        peer.notices(&[Notice::Hello { version: VERSION }])?;
        turn_until(&mut link, |link| link.is_greeted());
        peer.requests(1);
        Ok((link, peer))
    }

    fn linked() -> io::Result<(Link, Peer)> {
        let (ours, stream) = UnixStream::pair()?;
        stream.set_read_timeout(Some(PATIENCE))?;
        let decoder = Decoder::default();
        Ok((Link::new(ours.into())?, Peer { stream, decoder }))
    }

    fn turn_until(link: &mut Link, mut done: impl FnMut(&mut Link) -> bool) -> bool {
        let start = Instant::now();
        while !done(link) {
            let turn = link.turn(Some(Duration::from_millis(20)));
            if turn.is_err() || start.elapsed() > PATIENCE {
                return done(link);
            }
        }
        true
    }

    fn ended(link: &mut Link) -> Option<Disconnect> {
        turn_until(link, |link| link.disconnected().is_some());
        link.disconnected()
    }

    fn screen(character: char) -> Frame {
        let cell = Cell {
            character,
            foreground: 0,
            background: 0,
        };
        Frame {
            rows: 2,
            columns: 3,
            cells: vec![cell; 6],
            cursor: Some((1, 2)),
            ..Frame::default()
        }
    }

    fn shown(request: Request<'_>) -> String {
        format!("{request:?}")
    }

    #[test]
    fn the_version_check_goes_first_each_way() {
        let (mut link, mut peer) = linked().unwrap();
        assert!(link.send(Request::Quit));
        assert!(!link.is_greeted());
        peer.greet().unwrap();
        assert!(turn_until(&mut link, |link| link.is_greeted()));
        let hello = Request::Hello { version: VERSION };
        assert_eq!(peer.requests(2), [shown(hello), shown(Request::Quit)]);
        assert_eq!(link.disconnected(), None);
    }

    /// ADR-0043: the window answers a core one version apart at the lower version.
    #[test]
    fn a_core_one_version_apart_is_answered_at_the_lower() {
        for (theirs, spoken) in [(OLDEST, OLDEST), (VERSION + 1, VERSION)] {
            let (mut link, mut peer) = linked().unwrap();
            peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
            assert!(turn_until(&mut link, |link| link.is_greeted()));
            let hello = Request::Hello { version: spoken };
            assert_eq!(peer.requests(1), [shown(hello)]);
            assert_eq!(link.disconnected(), None);
            let open = Request::ListMarkdown { pane: 1 };
            assert_eq!(link.send(open), spoken >= open.since());
        }
    }

    /// ADR-0049: the palette goes right after the hello, and a core too old for it, as
    /// version 13 was before `OLDEST` passed it, gets none.
    #[test]
    fn the_palette_follows_the_hello_unless_the_core_is_too_old_for_it() {
        let palette = Palette {
            background: 0x000d_1117,
            ..Palette::GRAPHITE_GRID
        };
        for theirs in [VERSION, OLDEST] {
            let themed = theirs >= Request::Palette(palette).since();
            let (mut link, mut peer) = linked().unwrap();
            link.theme(palette);
            assert!(link.send(Request::Quit));
            peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
            assert!(turn_until(&mut link, |link| link.is_greeted()));
            let mut sent = vec![shown(Request::Hello { version: theirs })];
            if themed {
                sent.push(shown(Request::Palette(palette)));
            }
            sent.push(shown(Request::Quit));
            assert_eq!(peer.requests(sent.len()), sent);
            assert_eq!(link.take_unthemed(), !themed);
            assert!(!link.take_unthemed(), "said once");
            // A theme saved later goes at once, and a core too old is still sent none.
            link.theme(Palette::GRAPHITE_GRID);
            assert!(link.send(Request::Quit));
            let mut sent = vec![shown(Request::Quit)];
            if themed {
                sent.insert(0, shown(Request::Palette(Palette::GRAPHITE_GRID)));
            }
            assert_eq!(peer.requests(sent.len()), sent);
        }
    }

    #[test]
    fn a_silent_link_says_hello_only_when_told() {
        let (ours, stream) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut link = Link::silent(ours.into()).unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        let quiet = (&stream).read(&mut [0; 8]).unwrap_err();
        assert_eq!(quiet.kind(), ErrorKind::WouldBlock);
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let decoder = Decoder::default();
        let mut peer = Peer { stream, decoder };
        peer.greet().unwrap();
        assert!(turn_until(&mut link, |link| link.is_greeted()));
        link.greet();
        let hello = Request::Hello { version: VERSION };
        assert_eq!(peer.requests(1), [shown(hello)]);
    }

    #[test]
    fn a_link_can_move_to_the_io_thread() {
        fn sendable<T: Send>() {}
        sendable::<Link>();
    }

    #[test]
    fn frames_are_applied_to_the_local_copy() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let first = screen('a');
        peer.frame(&first, 5, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, first);
        assert_eq!((pane(&link).top, pane(&link).held.clone()), (5, 1..9));
        assert!(!take_dirty(&mut link));

        let mut second = first.clone();
        second.cells[4].character = 'b';
        peer.frame(&second, 6, Some(&first)).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, second);
        assert_eq!(pane(&link).top, 6);
    }

    #[test]
    fn what_a_budget_left_is_read_without_a_new_event() {
        let (mut link, mut peer) = linked().unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        link.buffer.truncate(8);
        peer.greet().unwrap();
        peer.frame(&screen('a'), 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
    }

    #[test]
    fn a_frame_larger_than_a_budget_arrives_across_turns() {
        let (mut link, mut peer) = linked().unwrap();
        let mut frame = screen('a');
        frame.columns = 1024;
        frame.rows = 128;
        frame.cells = vec![frame.cells[0]; 1024 * 128];
        frame.cursor = None;
        let sent = frame.clone();
        let writer = thread::spawn(move || {
            peer.greet().unwrap();
            peer.frame(&sent, 0, None).unwrap();
            peer
        });
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, frame);
        writer.join().unwrap();
    }

    #[test]
    fn requests_reach_the_core_in_order() {
        let (mut link, mut peer) = answered().unwrap();
        let (from, selection) = (Position::default(), Selection::default());
        let pane = PANE;
        let expected = [
            Request::Input {
                pane,
                bytes: b"ls\n",
            },
            Request::Paste { pane, text: "a\nb" },
            Request::Scroll {
                pane,
                lines: i32::MIN,
            },
            Request::Reveal { pane, row: 7 },
            Request::Search {
                pane,
                from,
                direction: Direction::Backward,
                query: "x",
            },
            Request::Quit,
        ];
        for request in expected {
            assert!(link.send(request));
        }
        assert!(link.resize(24, 80));
        assert!(link.copy(pane, selection));
        let mut sent = expected.map(shown).to_vec();
        sent.push(shown(Request::Resize {
            rows: 24,
            columns: 80,
        }));
        sent.push(shown(Request::Copy { pane, selection }));
        assert_eq!(peer.requests(8), sent);
    }

    #[test]
    fn a_resize_is_clamped_and_an_oversized_paste_refused() {
        let (mut link, mut peer) = answered().unwrap();
        assert!(!link.send(Request::Paste {
            pane: PANE,
            text: &"x".repeat(MAX_PASTE + 1),
        }));
        assert!(link.resize(0, u16::MAX));
        let clamped = Request::Resize {
            rows: 1,
            columns: MAX_DIMENSION,
        };
        assert_eq!(peer.requests(1), [shown(clamped)]);
    }

    #[test]
    fn a_core_that_stops_reading_refuses_requests_whole_and_loses_none() {
        let (mut link, mut peer) = answered().unwrap();
        let text = "x".repeat(MAX_PASTE);
        let mut accepted = 0;
        let paste = Request::Paste {
            pane: PANE,
            text: &text,
        };
        while link.send(paste) {
            accepted += 1;
            assert!(accepted < 16, "the queue has no limit");
        }
        assert!(accepted >= 2);
        let reader = thread::spawn(move || peer.requests(accepted));
        turn_until(&mut link, |_| reader.is_finished());
        let requests = reader.join().unwrap();
        assert_eq!(requests.len(), accepted);
        assert!(requests.iter().all(|paste| paste.len() > MAX_PASTE));
    }

    #[test]
    fn a_core_that_closes_between_messages_is_reported() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        drop(peer);
        assert_eq!(ended(&mut link), Some(Disconnect::Closed));
        assert!(link.is_closed());
    }

    #[test]
    fn a_core_that_closes_mid_message_is_reported_and_the_screen_stays() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let frame = screen('a');
        peer.frame(&frame, 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));

        let mut wire = Vec::new();
        encode_frame(&mut wire, PANE, &screen('b'), 0, 1..9, None).unwrap();
        peer.stream.write_all(&wire[..wire.len() / 2]).unwrap();
        drop(peer);
        let error = Some(Disconnect::Protocol(Error::Truncated));
        assert_eq!(ended(&mut link), error);
        assert_eq!(pane(&link).frame, frame);
        assert_eq!(link.turn(None), Err(Disconnect::Protocol(Error::Truncated)));
        assert!(!link.send(Request::Quit));
    }

    #[test]
    fn a_bad_version_ends_the_link_and_closes_it() {
        let (mut link, mut peer) = linked().unwrap();
        let theirs = VERSION + 2;
        peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
        let error = Error::Version {
            ours: VERSION,
            theirs,
        };
        assert_eq!(ended(&mut link), Some(error.into()));
        // It still says its own version, so the core can offer a restart (#203).
        let hello = Request::Hello { version: VERSION };
        assert_eq!(peer.requests(1), [shown(hello)]);
        assert!(peer.sees_close());
        assert!(!link.is_greeted());
    }

    #[test]
    fn a_length_over_the_limit_ends_the_link_and_closes_it() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let length = u32::try_from(MAX_MESSAGE + 1).unwrap();
        peer.stream.write_all(&length.to_le_bytes()).unwrap();
        let error = Error::TooLarge {
            size: length.into(),
        };
        assert_eq!(ended(&mut link), Some(error.into()));
        assert!(peer.sees_close());
    }

    #[test]
    fn a_failed_write_does_not_hide_why_the_core_left() {
        let (mut link, mut peer) = linked().unwrap();
        let theirs = VERSION + 2;
        peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
        drop(peer);
        link.send(Request::Quit);
        link.send(Request::Quit);
        let error = Error::Version {
            ours: VERSION,
            theirs,
        };
        assert_eq!(ended(&mut link), Some(error.into()));
    }

    #[test]
    fn a_top_the_window_cannot_add_to_ends_the_link() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        peer.frame(&screen('a'), usize::MAX, None).unwrap();
        assert!(matches!(
            ended(&mut link),
            Some(Disconnect::Protocol(Error::OutOfBounds {
                field: "top",
                ..
            }))
        ));
        assert_eq!(pane(&link).frame, Frame::default());
    }

    #[test]
    fn changes_to_a_screen_of_another_size_ask_for_a_full_frame_once() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let first = screen('a');
        peer.frame(&first, 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        peer.requests(1);

        // A window that lost track: this one's screen was another size.
        link.screens_mut()[0].frame.rows = 3;
        let mut second = first.clone();
        second.cells[0].character = 'b';
        for _ in 0..2 {
            peer.frame(&second, 0, Some(&first)).unwrap();
        }
        peer.notices(&[Notice::Cwd {
            pane: PANE,
            path: "/marker",
        }])
        .unwrap();
        assert!(turn_until(&mut link, |link| link.take_laid()
            && link.tabs()[0].cwd == "/marker"));
        let quit = Request::Quit;
        assert!(link.send(quit));
        let full = Request::FullFrame { pane: PANE };
        assert_eq!(peer.requests(2), [shown(full), shown(quit)]);

        peer.frame(&second, 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert_eq!(pane(&link).frame, second);
        assert!(!pane(&link).asked_full);
    }

    #[test]
    fn notices_for_a_pane_not_shown_change_nothing() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (other, path) = (PANE + 1, "/other");
        let mut wire = Vec::new();
        encode_frame(&mut wire, other, &screen('a'), 0, 1..9, None).unwrap();
        peer.stream.write_all(&wire).unwrap();
        peer.notices(&[Notice::Cwd { pane: other, path }]).unwrap();
        let (pane_of_tab, path) = (PANE, "/ours");
        let cwd = Notice::Cwd {
            pane: pane_of_tab,
            path,
        };
        peer.notices(&[cwd]).unwrap();
        assert!(turn_until(&mut link, |link| link
            .tabs()
            .first()
            .is_some_and(|tab| tab.cwd == path)));
        assert_eq!(pane(&link).frame, Frame::default());
        assert!(!link.is_closed());
    }

    #[test]
    fn a_layout_keeps_panes_that_stay_and_frees_the_rest() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        peer.frame(&screen('a'), 0, None).unwrap();
        assert!(turn_until(&mut link, take_dirty));
        assert!(link.take_laid());

        let (left, right) = (
            Rect {
                columns: 1,
                ..whole()
            },
            Rect {
                x: 2,
                columns: 1,
                ..whole()
            },
        );
        let tabs = [PANE, PANE + 2];
        let shown = [(PANE, left), (PANE + 1, right)];
        peer.layout(0, PANE + 1, 3, &tabs, &shown).unwrap();
        assert!(turn_until(&mut link, Link::take_laid));
        assert_eq!(link.screens().len(), 2);
        assert_eq!(pane(&link).rect, left);
        assert_eq!(pane(&link).frame, screen('a'), "what it holds stays");
        assert_eq!(link.screens()[1].frame, Frame::default());
        assert_eq!((link.focused(), link.active()), (PANE + 1, 0));
        assert_eq!((link.tabs().len(), link.total()), (2, 3));

        peer.layout(1, PANE + 2, 3, &tabs, &[(PANE + 2, whole())])
            .unwrap();
        assert!(turn_until(&mut link, Link::take_laid));
        assert_eq!(link.screens().len(), 1);
        assert_eq!(pane(&link).pane, PANE + 2);
        assert_eq!(link.active(), 1);
    }

    #[test]
    fn a_tab_keeps_its_directory_across_a_layout_until_its_pane_changes() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let (pane, path) = (PANE, "/home");
        peer.notices(&[Notice::Cwd { pane, path }]).unwrap();
        assert!(turn_until(&mut link, |link| link
            .tabs()
            .first()
            .is_some_and(|tab| !tab.cwd.is_empty())));

        peer.layout(0, PANE, 2, &[PANE, PANE + 1], &[(PANE, whole())])
            .unwrap();
        assert!(turn_until(&mut link, |link| link.tabs().len() == 2));
        assert_eq!(link.tabs()[0].cwd, path);
        assert_eq!(link.tabs()[1].cwd, "");

        peer.layout(
            0,
            PANE + 2,
            2,
            &[PANE + 2, PANE + 1],
            &[(PANE + 2, whole())],
        )
        .unwrap();
        assert!(turn_until(&mut link, |link| link.tabs()[0].pane == PANE + 2));
        assert_eq!(link.tabs()[0].cwd, "");
    }

    /// ADR-0049, section 4: OSC 12 and OSC 11 colour their own pane, shown or hidden when it
    /// set them, and OSC 112 and 111 return it to the theme.
    #[test]
    fn a_panes_osc_colours_are_kept_until_they_are_reset() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let colours = |pane, cursor, background| Notice::Colours {
            pane,
            cursor,
            background,
        };
        let shows = |link: &Link, pane, colours| {
            let screen = link.screens().first();
            screen.is_some_and(|screen| (screen.pane, screen.colours) == (pane, colours))
        };
        let (set, hidden) = ((Some(1), Some(2)), (None, Some(3)));
        peer.notices(&[
            colours(PANE, set.0, set.1),
            colours(PANE + 1, None, hidden.1),
        ])
        .unwrap();
        assert!(turn_until(&mut link, |link| shows(link, PANE, set)));
        assert!(link.screens()[0].dirty, "the pane is drawn again");
        // The other tab's pane set its background while hidden, and shows it once shown.
        peer.layout(1, PANE + 1, 2, &[PANE, PANE + 1], &[(PANE + 1, whole())])
            .unwrap();
        assert!(turn_until(&mut link, |link| shows(link, PANE + 1, hidden)));
        // OSC 111 returns it to the theme.
        peer.notices(&[colours(PANE + 1, None, None)]).unwrap();
        let reset = (None, None);
        assert!(turn_until(&mut link, |link| shows(link, PANE + 1, reset)));
        assert_eq!(
            link.state.colours,
            [(PANE, set.0, set.1)],
            "only panes with one set"
        );
        // Panes that closed with a colour set cannot add up.
        let many = (0..=MAX_COLOURED as u32).map(|pane| colours(100 + pane, Some(0), None));
        peer.notices(&many.collect::<Vec<_>>()).unwrap();
        peer.notices(&[colours(PANE + 1, set.0, None)]).unwrap();
        let last = (set.0, None);
        assert!(turn_until(&mut link, |link| shows(link, PANE + 1, last)));
        assert_eq!(link.state.colours.len(), MAX_COLOURED);
    }

    /// ADR-0051: prefix `w` goes to the ◆ pane waiting longest, then on in the inbox's order,
    /// round again past the last; with none it sends nothing and says so.
    #[test]
    fn next_waiting_cycles_in_inbox_order_and_says_when_none_waits() {
        let (mut link, mut peer) = answered().unwrap();
        // Tab 0 holds 10 and 11, tab 1 holds 12 and 13; 99 is no longer in the session.
        link.state.members = vec![(10, 0), (11, 0), (12, 1), (13, 1)];
        for (pane, state, since) in [
            (12, 1, 90),
            (11, 0, 20),
            (13, 0, 50),
            (10, 0, 30),
            (99, 0, 99),
        ] {
            link.state.list(pane, Some((state, true, (since, 0), "")));
        }
        let next = |link: &mut Link, focused| {
            link.state.focused = focused;
            link.next_waiting()
        };
        assert_eq!(next(&mut link, 12), Some((1, 13)), "longest first");
        assert_eq!(next(&mut link, 13), Some((0, 10)));
        assert_eq!(next(&mut link, 10), Some((0, 11)));
        assert_eq!(next(&mut link, 11), Some((1, 13)), "round again");

        link.state.focused = 11;
        assert!(link.go_to_next_waiting());
        let expected = [Request::SwitchTab { index: 1 }, Request::Focus { pane: 13 }];
        let expected = expected.map(|request| format!("{request:?}"));
        assert_eq!(peer.requests(2), expected);
        assert!(!link.take_none_waiting());

        for pane in [10, 11, 13] {
            link.state.list(pane, Some((1, true, (0, 0), "")));
        }
        assert!(link.go_to_next_waiting());
        assert!(link.take_none_waiting() && !link.take_none_waiting());
        link.send(Request::Zoom);
        assert_eq!(
            peer.requests(1),
            [format!("{:?}", Request::Zoom)],
            "nothing before it"
        );
    }

    /// ADR-0051 §7: a listed pane's branch is kept only while the inbox is open, and closing
    /// frees it.
    #[test]
    fn a_branch_is_kept_only_while_the_inbox_is_open() {
        let (mut link, _peer) = linked().unwrap();
        link.state.list(10, Some((1, true, (5, 600), "")));
        let extra = Notice::InboxExtra {
            pane: 10,
            branch: "feat/inbox",
            ctx: Some(64),
            steps: "Read\nEdit",
        };
        link.state.apply(extra.clone()).unwrap();
        assert_eq!(link.listed()[0].branch, "", "kept while closed");
        assert_eq!(link.listed()[0].steps, "", "kept while closed");
        link.send(Request::Inbox { open: true });
        link.state.apply(extra).unwrap();
        // An unknown pane's is dropped.
        link.state
            .apply(Notice::InboxExtra {
                pane: 11,
                branch: "other",
                ctx: None,
                steps: "",
            })
            .unwrap();
        assert_eq!(link.listed().len(), 1);
        let listed = &link.listed()[0];
        assert_eq!(listed.branch, "feat/inbox");
        assert_eq!(
            (listed.ctx, listed.steps.as_str()),
            (Some(64), "Read\nEdit")
        );
        let run = listed.started.elapsed().as_secs();
        assert!((600..610).contains(&run), "{run}");
        link.send(Request::Inbox { open: false });
        let listed = &link.listed()[0];
        assert_eq!(listed.branch.capacity(), 0, "not freed on close");
        assert_eq!(
            (listed.ctx, listed.steps.capacity()),
            (None, 0),
            "not freed on close"
        );
    }

    /// ADR-0052 slice 4: a run's summary lands on its listed pane's row and an unknown pane's is
    /// dropped; pages, entries and labels decode without ending the link; and a core of 26 is
    /// never asked for a timeline, a reveal or a rewind at a checkpoint.
    #[test]
    fn a_summary_lands_on_its_row_and_the_oldest_core_is_asked() {
        use pinfold_proto::link::{ENTRY, Tool};
        let (mut link, mut peer) = answered().unwrap();
        link.state.list(10, Some((1, true, (5, 600), "")));
        let summary = |pane| Notice::Summary {
            pane,
            started: 600,
            ended: false,
            commands: 12,
            failed: 1,
            approvals: 3,
            denied: 0,
            changed: 8,
            errors: 1,
        };
        let mut entry = [0; ENTRY];
        entry[0] = 1;
        peer.notices(&[
            summary(10),
            summary(11),
            Notice::Timeline {
                pane: 10,
                run: 0,
                started: 600,
                dropped: 0,
                total: 1,
                entries: &entry,
            },
            Notice::TimelineEntry {
                pane: 10,
                seq: 1,
                entry,
            },
            Notice::TimelineLabel {
                pane: 10,
                row: 4,
                held: true,
                text: "$ make",
            },
        ])
        .unwrap();
        assert!(turn_until(&mut link, |link| link.listed()[0].run.is_some()));
        let run = RunSummary {
            ended: false,
            commands: 12,
            failed: 1,
            approvals: 3,
            denied: 0,
            changed: 8,
            errors: 1,
        };
        assert_eq!(link.listed()[0].run, Some(run));
        assert_eq!(
            link.listed().len(),
            1,
            "an unknown pane's summary is dropped"
        );
        assert_eq!(link.disconnected(), None);

        let (mut link, mut peer) = linked().unwrap();
        peer.notices(&[Notice::Hello { version: OLDEST }]).unwrap();
        assert!(turn_until(&mut link, |link| link.is_greeted()));
        for request in [
            Request::Timeline {
                pane: 1,
                watch: true,
                before: None,
                count: 64,
            },
            Request::RevealStored { pane: 1, number: 4 },
            Request::OpenTool {
                pane: 1,
                tool: Tool::Rewind,
                checkpoint: Some(2),
            },
        ] {
            // From link 28 the oldest spoken is 27, which has them; 26 is refused at its hello.
            assert!(link.speaks(&request));
        }
    }

    /// ADR-0051 §8: outside rows are kept in order only while the inbox is open, a refresh with
    /// fewer drops the rest, an unchanged file is no news, and closing frees them.
    #[test]
    fn outside_rows_are_kept_only_while_the_inbox_is_open() {
        let (mut link, _peer) = linked().unwrap();
        let row = |index, count, secs, dir| Notice::Outside {
            index,
            count,
            secs,
            dir,
        };
        link.state.apply(row(0, 1, 5, "/work")).unwrap();
        assert!(link.outside().is_empty(), "kept while closed");
        link.send(Request::Inbox { open: true });
        let _ = link.take_relisted();
        for notice in [
            row(0, 2, 5, "/work"),
            row(1, 2, 700, "/old"),
            row(3, 2, 0, "/x"),
        ] {
            link.state.apply(notice).unwrap();
        }
        let dirs: Vec<&str> = link.outside().iter().map(|row| row.dir.as_str()).collect();
        assert_eq!(dirs, ["/work", "/old"]);
        let age = link.outside()[1].changed.elapsed().as_secs();
        assert!((700..710).contains(&age), "{age}");
        assert!(link.take_relisted());
        link.state.apply(row(0, 2, 6, "/work")).unwrap();
        assert!(!link.take_relisted(), "an unchanged file redrew the inbox");
        link.state.apply(row(0, 0, 0, "")).unwrap();
        assert!(link.outside().is_empty() && link.take_relisted());
        link.state.apply(row(0, 1, 5, "/work")).unwrap();
        link.send(Request::Inbox { open: false });
        assert_eq!(link.state.outside.capacity(), 0, "not freed on close");
    }

    /// ADR-0051: a listed pane alerts when it comes to ◆ or ✕, not again while it stays, and
    /// never for another state or a pane that is not listed.
    /// ADR-0051 §9: whether toasts are on follows the hello as a palette does, and a core too old
    /// for it is sent nothing and keeps them off. The window's side keeps one toast at a time.
    #[test]
    fn toasts_follow_the_hello_and_only_the_latest_is_kept() {
        for theirs in [VERSION, OLDEST] {
            let spoken = theirs >= Request::Toasts { on: true }.since();
            let (mut link, mut peer) = linked().unwrap();
            link.toasts(true);
            assert!(link.send(Request::Quit));
            peer.notices(&[Notice::Hello { version: theirs }]).unwrap();
            assert!(turn_until(&mut link, |link| link.is_greeted()));
            let mut sent = vec![shown(Request::Hello { version: theirs })];
            if spoken {
                sent.push(shown(Request::Toasts { on: true }));
            }
            sent.push(shown(Request::Quit));
            assert_eq!(peer.requests(sent.len()), sent);
            link.toasts(false);
            assert!(link.send(Request::Quit));
            let mut sent = vec![shown(Request::Quit)];
            if spoken {
                sent.insert(0, shown(Request::Toasts { on: false }));
            }
            assert_eq!(peer.requests(sent.len()), sent);
        }
        let (mut link, _peer) = linked().unwrap();
        link.state.members = vec![(10, 1)];
        for text in ["first", "second"] {
            let toast = Notice::Toast {
                pane: 10,
                kind: pinfold_proto::link::OSC_9,
                text,
            };
            link.state.apply(toast).unwrap();
        }
        assert_eq!(link.take_toast(), Some((10, "second".to_owned(), Some(1))));
        assert_eq!(link.take_toast(), None);
    }

    /// ADR-0051 §9: a tab shows its focused pane's progress, or else the furthest along of its
    /// panes, one with no percent before none; 0 forgets a pane, and a pane that leaves the
    /// layout takes its progress with it. Each change is news for the strip.
    #[test]
    fn a_tab_shows_its_focused_panes_progress_or_its_furthest() {
        use pinfold_proto::link::{INDETERMINATE, NORMAL, progress};
        let (mut link, mut peer) = linked().unwrap();
        link.state.members = vec![(10, 0), (11, 0), (12, 1)];
        link.state.tabs = [10, 12]
            .map(|pane| Tab {
                pane,
                ..Tab::default()
            })
            .into();
        let mut set = |pane, progress| {
            link.state.laid = false;
            link.state
                .apply(Notice::Progress { pane, progress })
                .unwrap();
            let shown = link.state.tabs.iter().map(|tab| tab.progress);
            (shown.collect::<Vec<_>>(), link.state.laid)
        };
        let (low, high, busy) = (
            progress(NORMAL, 20),
            progress(NORMAL, 80),
            progress(INDETERMINATE, 0),
        );
        assert_eq!(set(11, busy), (vec![busy, 0], true), "busy beats none");
        assert_eq!(set(12, high), (vec![busy, high], true));
        assert_eq!(set(12, high), (vec![busy, high], false), "no news");
        assert_eq!(
            set(11, low),
            (vec![low, high], true),
            "a percent beats busy"
        );
        assert_eq!(set(10, high), (vec![high, high], true), "focused wins");
        assert_eq!(set(10, 0), (vec![low, high], true));
        assert_eq!(link.state.progress.len(), 2, "0 is not kept");
        peer.notices(&[Notice::Hello { version: VERSION }]).unwrap();
        peer.no_tabs().unwrap();
        assert!(turn_until(&mut link, |link| link.tabs().is_empty()));
        assert!(
            link.state.progress.is_empty(),
            "panes that left take theirs"
        );
    }

    #[test]
    fn a_listed_pane_alerts_once_on_coming_to_needs_you_or_error() {
        let (mut link, _peer) = linked().unwrap();
        link.state.members = vec![(10, 0), (11, 2)];
        let taken = |link: &mut Link| std::iter::from_fn(|| link.take_alert()).collect::<Vec<_>>();
        link.state.list(10, Some((0, true, (0, 0), "")));
        link.state.list(10, Some((0, false, (0, 0), "new message")));
        link.state.list(11, Some((1, true, (0, 0), "")));
        link.state.list(12, Some((3, true, (0, 0), "")));
        assert_eq!(taken(&mut link), [(10, 0, Some(0)), (12, 3, None)]);
        link.state.list(11, Some((2, true, (0, 0), "")));
        link.state.list(11, Some((3, true, (0, 0), "")));
        link.state.list(10, Some((4, true, (0, 0), "")));
        link.state.list(10, None);
        link.state.list(10, None);
        assert_eq!(taken(&mut link), [(11, 3, Some(2))]);
        // A flood of panes coming and going is held to the inbox's own cap.
        for step in 0..1_000 {
            link.state
                .list(step % 7, Some(((step % 2) as u8 * 3, true, (0, 0), "")));
        }
        assert_eq!(link.state.alerting.len(), MAX_PANES);
        assert_eq!(taken(&mut link).len(), MAX_PANES);
    }

    #[test]
    fn a_tab_takes_its_panes_status_and_the_alerts() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let status = Notice::Status {
            pane: PANE,
            state: 0,
            reported: true,
            naming: true,
            listed: true,
            since: 0,
            started: 0,
            profile: None,
            message: "ok?",
        };
        // A plain shell's pane names its tab but is not in the inbox.
        let other = Notice::Status {
            pane: PANE + 9,
            state: 1,
            reported: false,
            naming: true,
            listed: false,
            since: 0,
            started: 0,
            profile: None,
            message: "",
        };
        // A pane in the tab that does not name it leaves the tab's state as it is.
        let unnamed = Notice::Status {
            pane: PANE,
            state: 3,
            reported: false,
            naming: false,
            listed: true,
            since: 5,
            started: 9,
            profile: Some(2),
            message: "",
        };
        let alerts = Notice::Activity {
            tabs: 0,
            alerts: 0b1,
        };
        let start = Instant::now();
        peer.notices(&[other, status, unnamed, alerts]).unwrap();
        assert!(turn_until(&mut link, |link| link.alerts() == 0b1));
        let tab = &link.tabs()[0];
        assert_eq!((tab.status, tab.message.as_str()), (Some((0, true)), "ok?"));
        // The inbox keeps every listed pane's latest, its seconds on this clock.
        let listed = link.listed();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].pane, listed[0].state, listed[0].reported),
            (PANE, 3, false)
        );
        assert_eq!(listed[0].profile, Some(2), "for the inbox's answers");
        let five = Duration::from_secs(5);
        assert!((start - five..=Instant::now() - five).contains(&listed[0].since));
        assert!(link.take_relisted() && !link.take_relisted());
        assert_eq!(link.members(), [(PANE, 0)]);
        // A layout without the pane drops its row.
        peer.layout(0, PANE + 1, 1, &[PANE + 1], &[(PANE + 1, whole())])
            .unwrap();
        assert!(turn_until(&mut link, |link| link.listed().is_empty()));
        assert_eq!(link.members(), [(PANE + 1, 0)]);
    }

    #[test]
    fn the_rest_of_the_state_is_kept_for_the_window() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let pane = PANE;
        let found = Some(Selection::default());
        peer.notices(&[Notice::Searched { pane, found }]).unwrap();
        peer.notices(&[Notice::Refused {
            reason: Refusal::Tabs,
        }])
        .unwrap();
        assert!(turn_until(&mut link, |link| link.state.refused.is_some()));
        assert_eq!(link.take_searched(), Some((pane, found)));
        assert_eq!(link.take_searched(), None);
        assert_eq!(link.take_refused(), Some(Refusal::Tabs));
        assert_eq!(link.take_refused(), None);

        peer.no_tabs().unwrap();
        assert!(turn_until(&mut link, |link| link.is_closed()));
        assert_eq!(link.disconnected(), None, "an exit is not a disconnect");
    }

    #[test]
    fn a_raise_is_reported_once() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        peer.notices(&[Notice::Raise]).unwrap();
        assert!(turn_until(&mut link, |link| link.state.raised));
        assert!(link.take_raised());
        assert!(!link.take_raised());
    }

    #[test]
    fn a_link_answer_is_reported_once() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let uri = "https://pinfold.sh";
        let at = Position { row: 7, column: 3 };
        let notice = Notice::Linked {
            pane: PANE,
            at,
            uri,
        };
        peer.notices(&[notice]).unwrap();
        assert!(turn_until(&mut link, |link| link.state.linked.is_some()));
        assert_eq!(link.take_linked(), Some((PANE, at, uri.to_owned())));
        assert_eq!(link.take_linked(), None);
    }

    #[test]
    fn only_a_copy_asked_for_can_reach_the_clipboard() {
        let (mut link, mut peer) = linked().unwrap();
        peer.greet().unwrap();
        let pane = PANE;
        let (asked, unasked) = ("asked", "unasked");
        peer.notices(&[
            Notice::Copied {
                pane,
                text: unasked,
            },
            Notice::Raise,
        ])
        .unwrap();
        assert!(turn_until(&mut link, Link::take_raised));
        assert_eq!(link.take_copied(), None);

        assert!(link.copy(pane, Selection::default()));
        peer.notices(&[
            Notice::Copied { pane, text: asked },
            Notice::Copied {
                pane,
                text: unasked,
            },
        ])
        .unwrap();
        peer.no_tabs().unwrap();
        assert!(turn_until(&mut link, |link| link.is_closed()));
        assert_eq!(link.take_copied().as_deref(), Some(asked));
        assert_eq!(link.take_copied(), None);
    }

    #[test]
    fn a_waker_ends_a_blocked_turn() {
        let (mut link, _peer) = linked().unwrap();
        link.turn(Some(Duration::ZERO)).unwrap();
        let waker = link.waker();
        let wake = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            waker.wake().unwrap();
        });
        let start = Instant::now();
        link.turn(Some(Duration::from_secs(20))).unwrap();
        assert!(start.elapsed() < PATIENCE);
        wake.join().unwrap();
    }
}
