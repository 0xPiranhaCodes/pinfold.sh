//! Session replay's records (ADR-0052 sections 2 and 3, slices 1 and 2).
//!
//! Each agent run keeps a ring file of 16-byte [`Entry`] records, `<state>/timeline/r<pane>-<n>`,
//! and the core keeps only a 56-byte [`Timeline`]: the [`Summary`], the ring's head and dropped
//! count, and the file while the run is live. Each entry is one `pwrite` as its source fires, so
//! an idle core never touches the disk, and nothing is allocated per entry. The 16-byte header is
//! written when the file is made and when it closes; the core's head is the truth meanwhile.
//! The file holds no text, and is read as untrusted: a record that does not decode is skipped.

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Instant;

use pinfold_api::Log;
use pinfold_proto::link::ApprovalAction;

use crate::status::{self, Status};
use crate::vt::Closed;

/// Bytes an entry takes, in the file and in memory.
pub const SIZE: usize = 16;
const HEADER: u64 = 16;
/// Entries a run keeps once it has ended.
pub const CLOSED: u16 = 128;
/// Runs whose timelines the core keeps, as ADR-0050 keeps run records.
pub const MAX_RUNS: usize = crate::rewind::MAX_RUNS;
/// [`Summary::ended`] while the run is live.
const LIVE: u32 = u32::MAX;
/// The audit log's method names, which an `api` entry holds as an index, and whether each
/// writes or acts. Only those that do are recorded when they succeed (decision 3), so a polling
/// agent cannot flush the ring. New methods are appended, so a stored index keeps its meaning.
pub const METHODS: [(&str, bool); 24] = [
    ("hello", false),
    ("window.open", true),
    ("pane.list", false),
    ("events.subscribe", false),
    ("pane.read", false),
    ("pane.last_output", false),
    ("pane.send", true),
    ("pane.wait", false),
    ("pane.spawn", true),
    ("agent.launch", true),
    ("tool.open", true),
    ("pane.close", true),
    ("checkpoint.take", true),
    ("checkpoint.list", false),
    ("checkpoint.restore", true),
    ("pane.status", true),
    ("audit.read", false),
    ("session.save", true),
    ("workspace.open", true),
    ("grants.list", false),
    ("grants.revoke", true),
    ("workspace.trust_state", false),
    ("workspace.ask_trust", true),
    ("run.timeline", false),
];
/// Why a checkpoint was taken, as ADR-0050 names it; a `checkpoint` entry holds the index.
pub const REASONS: [&str; 6] = ["launch", "done", "exit", "api", "view", "restore"];
const ACTIONS: [ApprovalAction; 5] = [
    ApprovalAction::Read,
    ApprovalAction::Send,
    ApprovalAction::Trust,
    ApprovalAction::Owner,
    ApprovalAction::Rewind,
];
const STATUSES: [Status; 5] = [
    Status::NeedsYou,
    Status::Running,
    Status::Done,
    Status::Error,
    Status::Idle,
];

/// How a run's program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// It exited with this status.
    Code(u8),
    /// A signal ended it.
    Signal(u8),
    /// Its pane closed while it ran.
    Closed,
}

/// How the audit log recorded a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    /// It succeeded.
    Ok,
    /// Policy refused it.
    Refused,
    /// It failed while being performed.
    Error,
}

/// How a window prompt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Answer {
    /// Allowed once.
    Once,
    /// Allowed for thirty minutes.
    ThirtyMinutes,
    /// Denied.
    Denied,
    /// Nobody answered it in time, or the window closed.
    Expired,
}

/// What an entry records: a fixed set of numbers and enums, never text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// The shell's `D` mark closed a command: the stored row at its `B` mark, or `u32::MAX`
    /// without one, its run time in ms, and its exit status. Output can print marks (T4).
    Command {
        /// Stored-row number.
        row: u32,
        /// From `C` to `D`.
        ms: u32,
        /// As the mark gave it.
        exit: Option<u8>,
    },
    /// A call by or about the run's pane that wrote or acted, or was refused or failed.
    Api {
        /// The audit entry's `seq`, its low 32 bits.
        seq: u32,
        /// An index into [`METHODS`].
        method: u8,
        /// As the audit log recorded it.
        outcome: Outcome,
        /// The pane it named.
        target: Option<u16>,
    },
    /// A window prompt about the run's pane was answered or expired.
    Approval {
        /// What was asked.
        action: ApprovalAction,
        /// How it ended.
        answer: Answer,
        /// The other pane, the caller's or the target's, if it was one.
        other: Option<u16>,
        /// The prompt's id, which a lasting grant keeps.
        grant: u32,
    },
    /// The shown status changed through a report or an agent CLI's signal, never marks.
    State {
        /// The new status.
        status: Status,
        /// From `pane.status` rather than output.
        reported: bool,
    },
    /// ADR-0050 took checkpoint `n`. `changed` counts the paths that differ from the launch
    /// checkpoint; for a restore's, the paths the restore put back or deleted.
    Checkpoint {
        /// The checkpoint's number in its run.
        n: u16,
        /// An index into [`REASONS`].
        reason: u8,
        /// Whether it covered every file.
        complete: bool,
        /// See above.
        changed: u32,
    },
    /// The run's program exited or its pane closed.
    Exit(Ended),
}

/// One record: 16 bytes and no string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Milliseconds since the second the run started, saturating.
    pub at: u32,
    /// What happened.
    pub what: What,
}

const _: () = assert!(size_of::<Entry>() == SIZE);

impl Entry {
    /// The record as the file holds it: kind, two small fields, `at`, two words, little-endian.
    ///
    /// ```
    /// use pinfold_core::timeline::{Ended, Entry, What};
    ///
    /// let entry = Entry { at: 7, what: What::Exit(Ended::Signal(9)) };
    /// assert_eq!(Entry::decode(&entry.encode()), Some(entry));
    /// assert_eq!(Entry::decode(&[0xff; 16]), None);
    /// ```
    #[must_use]
    pub fn encode(&self) -> [u8; SIZE] {
        let (kind, small, flag, one, two) = match self.what {
            What::Command { row, ms, exit } => {
                (1, exit.unwrap_or(0), u16::from(exit.is_some()), row, ms)
            }
            What::Api {
                seq,
                method,
                outcome,
                target,
            } => (
                2,
                method,
                outcome as u16,
                seq,
                target.map_or(u32::MAX, u32::from),
            ),
            What::Approval {
                action,
                answer,
                other,
                grant,
            } => (
                3,
                action as u8,
                answer as u16,
                grant,
                other.map_or(u32::MAX, u32::from),
            ),
            What::State { status, reported } => (4, status as u8, u16::from(reported), 0, 0),
            What::Checkpoint {
                n,
                reason,
                complete,
                changed,
            } => (5, reason, n, changed, u32::from(complete)),
            What::Exit(Ended::Code(code)) => (6, code, 0, 0, 0),
            What::Exit(Ended::Signal(signal)) => (6, signal, 1, 0, 0),
            What::Exit(Ended::Closed) => (6, 0, 2, 0, 0),
        };
        let mut bytes = [0; SIZE];
        bytes[0] = kind;
        bytes[1] = small;
        bytes[2..4].copy_from_slice(&flag.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.at.to_le_bytes());
        bytes[8..12].copy_from_slice(&one.to_le_bytes());
        bytes[12..16].copy_from_slice(&two.to_le_bytes());
        bytes
    }

    /// The record in `bytes`, or nothing for one this core never writes.
    #[must_use]
    pub fn decode(bytes: &[u8; SIZE]) -> Option<Self> {
        let word = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let (small, flag) = (bytes[1], u16::from_le_bytes([bytes[2], bytes[3]]));
        let (one, two) = (word(8), word(12));
        let pane = |word: u32| match word {
            u32::MAX => Some(None),
            word => u16::try_from(word).ok().map(Some),
        };
        let what = match (bytes[0], flag) {
            (1, 0 | 1) if flag == 1 || small == 0 => What::Command {
                row: one,
                ms: two,
                exit: (flag == 1).then_some(small),
            },
            (2, 0..=2) if usize::from(small) < METHODS.len() => What::Api {
                seq: one,
                method: small,
                outcome: [Outcome::Ok, Outcome::Refused, Outcome::Error][usize::from(flag)],
                target: pane(two)?,
            },
            (3, 0..=3) => What::Approval {
                action: *ACTIONS.get(usize::from(small))?,
                answer: [
                    Answer::Once,
                    Answer::ThirtyMinutes,
                    Answer::Denied,
                    Answer::Expired,
                ][usize::from(flag)],
                other: pane(two)?,
                grant: one,
            },
            (5, _) if usize::from(small) < REASONS.len() && two <= 1 => What::Checkpoint {
                n: flag,
                reason: small,
                complete: two == 1,
                changed: one,
            },
            (4, 0 | 1) if one == 0 && two == 0 => What::State {
                status: *STATUSES.get(usize::from(small))?,
                reported: flag == 1,
            },
            (6, 0..=2) if one == 0 && two == 0 && (flag != 2 || small == 0) => {
                What::Exit(match flag {
                    0 => Ended::Code(small),
                    1 => Ended::Signal(small),
                    _ => Ended::Closed,
                })
            }
            _ => return None,
        };
        Some(Self { at: word(4), what })
    }
}

/// A run's counters, which eviction never lowers: what the inbox and the `was running` strip
/// say of it. All saturate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Commands closed.
    pub commands: u32,
    /// Of those, with a non-zero exit.
    pub failed: u32,
    /// Approvals answered, and of those denied.
    pub approvals: u32,
    /// See `approvals`.
    pub denied: u32,
    /// Calls that wrote or acted, were refused or failed.
    pub calls: u32,
    /// Paths the newest checkpoint changed since launch.
    pub changed: u32,
    /// `failed` plus ✕ error states.
    pub errors: u32,
    /// ◆ needs-you states.
    pub needs_you: u32,
    /// When the run started, in whole seconds since the core did.
    pub started: u32,
    /// Milliseconds from that second to its end, or `u32::MAX` while it is live.
    pub ended: u32,
}

/// One run's record in the core: 56 bytes, and its entries on disk.
#[derive(Debug)]
pub struct Timeline {
    summary: Summary,
    /// Which launch of this core it was, for its file's name.
    n: u32,
    /// Entries overwritten or cut.
    dropped: u32,
    pane: u16,
    /// The next slot, or the ring's size once it is full and nothing has dropped yet.
    head: u16,
    /// Open while the run is live.
    file: Option<File>,
}

const _: () = assert!(size_of::<Timeline>() == 56);

impl Timeline {
    /// The counters.
    #[must_use]
    pub fn summary(&self) -> Summary {
        self.summary
    }

    /// Entries overwritten by newer ones or cut when the run closed.
    #[must_use]
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    /// Which launch of this core it was, so a new run in the same pane is told apart.
    #[must_use]
    pub fn run(&self) -> u32 {
        self.n
    }

    /// Whether its program still runs.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.summary.ended == LIVE
    }

    fn name(&self) -> String {
        format!("r{}-{}", self.pane, self.n)
    }

    fn count(&self, slots: u16) -> u16 {
        if self.dropped > 0 { slots } else { self.head }
    }

    /// The slot of the entry `index` places from the oldest.
    fn slot(&self, slots: u16, index: u16) -> u64 {
        let oldest = if self.dropped > 0 {
            self.head % slots
        } else {
            0
        };
        u64::from((oldest + index) % slots)
    }

    fn push(&mut self, what: What, slots: u16, now: Instant) {
        let Some(file) = &self.file else { return };
        let elapsed = now.saturating_duration_since(status::epoch()).as_millis();
        let since = elapsed.saturating_sub(u128::from(self.summary.started) * 1000);
        let entry = Entry {
            at: u32::try_from(since).unwrap_or(u32::MAX),
            what,
        };
        let overwrites = self.dropped > 0 || self.head == slots;
        let slot = if self.head == slots { 0 } else { self.head };
        if file
            .write_all_at(&entry.encode(), HEADER + u64::from(slot) * SIZE as u64)
            .is_err()
        {
            // A full or failing disk ends the record rather than the run.
            self.file = None;
            return;
        }
        self.head = slot + 1;
        self.dropped = self.dropped.saturating_add(u32::from(overwrites));
    }

    /// Keeps the newest [`CLOSED`] entries at the start of the file, cuts it there and lets
    /// the file go.
    fn close(&mut self, slots: u16, now: Instant) {
        let since = now.saturating_duration_since(status::epoch()).as_millis();
        let since = since.saturating_sub(u128::from(self.summary.started) * 1000);
        self.summary.ended = u32::try_from(since).unwrap_or(LIVE - 1);
        let Some(file) = self.file.take() else { return };
        let count = self.count(slots);
        let keep = count.min(CLOSED);
        let mut kept = [0; SIZE * CLOSED as usize];
        for (index, bytes) in (count - keep..count).zip(kept.as_chunks_mut::<SIZE>().0) {
            // One that cannot be read stays zeros, which a reader skips.
            let _ = file.read_exact_at(bytes, HEADER + self.slot(slots, index) * SIZE as u64);
        }
        self.dropped = self.dropped.saturating_add(u32::from(count - keep));
        self.head = keep;
        let length = usize::from(keep) * SIZE;
        let _ = file.write_all_at(&kept[..length], HEADER);
        let _ = file.set_len(HEADER + length as u64);
        let _ = file.write_all_at(&header(self.head, keep, self.dropped, CLOSED), 0);
    }
}

fn header(head: u16, count: u16, dropped: u32, slots: u16) -> [u8; HEADER as usize] {
    let mut bytes = [0; HEADER as usize];
    for (at, word) in [u32::from(head), u32::from(count), dropped, u32::from(slots)]
        .into_iter()
        .enumerate()
    {
        bytes[at * 4..at * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// Whether `metadata` is a private regular file of `uid`'s.
fn ours(metadata: &Metadata, uid: u32) -> bool {
    metadata.is_file() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
}

fn refused(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("timeline file {why}"))
}

/// Opens an existing ring file, to read and with `write` to write, without following a link:
/// the path's own entry must be a private file of ours, and the opened file the same one.
fn open_checked(path: &Path, write: bool) -> io::Result<File> {
    let found = fs::symlink_metadata(path)?;
    if !ours(&found, pinfold_sys::effective_uid()) {
        return Err(refused("is a link or not a private file of ours"));
    }
    let file = OpenOptions::new().read(true).write(write).open(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (found.dev(), found.ino()) {
        return Err(refused("was swapped while it was opened"));
    }
    Ok(file)
}

/// The timelines of this core's agent runs, at most [`MAX_RUNS`], in `<state>/timeline`.
#[derive(Debug)]
pub struct Timelines {
    dir: PathBuf,
    /// Entries a live run keeps, `[timeline] entries`.
    slots: u16,
    next: u32,
    /// The audit `seq` after the newest call looked at.
    seen: u64,
    runs: Vec<Timeline>,
}

impl Timelines {
    /// Makes `dir` 0700 if it is missing, refuses it unless only this user can reach it, and
    /// empties it: a run's timeline ends with the core. A live run keeps `entries`, 64 to 4096.
    ///
    /// # Errors
    ///
    /// Returns an error if `dir` cannot be made, or is a link, someone else's or open to others.
    pub fn new(dir: PathBuf, entries: u64) -> io::Result<Self> {
        if let Some(parent) = dir.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        pinfold_api::control::checked(&dir).map_err(io::Error::other)?;
        for found in fs::read_dir(&dir)? {
            // A link goes, never what it names.
            let _ = fs::remove_file(found?.path());
        }
        Ok(Self {
            dir,
            slots: u16::try_from(entries.clamp(64, 4096)).unwrap_or(4096),
            next: 0,
            seen: 0,
            runs: Vec::new(),
        })
    }

    /// Opens a timeline for the agent run starting in `pane`, closing one live there already
    /// and, at [`MAX_RUNS`], deleting the oldest closed one. With every run live, or a file
    /// already at its name, the run gets none.
    pub fn open(&mut self, pane: usize, now: Instant) {
        self.exit(pane, None, now);
        let Ok(pane) = u16::try_from(pane) else {
            return;
        };
        let at = if self.runs.len() < MAX_RUNS {
            None
        } else {
            let oldest = (0..self.runs.len())
                .filter(|at| !self.runs[*at].is_live())
                .min_by_key(|at| self.runs[*at].n);
            let Some(oldest) = oldest else { return };
            let _ = fs::remove_file(self.dir.join(self.runs[oldest].name()));
            Some(oldest)
        };
        let mut run = Timeline {
            summary: Summary {
                started: u32::try_from(now.saturating_duration_since(status::epoch()).as_secs())
                    .unwrap_or(u32::MAX),
                ended: LIVE,
                ..Summary::default()
            },
            n: self.next,
            dropped: 0,
            pane,
            head: 0,
            file: None,
        };
        self.next = self.next.wrapping_add(1);
        // O_CREAT | O_EXCL: a link at the name is refused, never followed.
        let made = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.dir.join(run.name()));
        let file = made.and_then(|file| {
            if !ours(&file.metadata()?, pinfold_sys::effective_uid()) {
                return Err(refused("is not a private file of ours"));
            }
            file.write_all_at(&header(0, 0, 0, self.slots), 0)?;
            Ok(file)
        });
        let Ok(file) = file else { return };
        run.file = Some(file);
        match at {
            Some(at) => self.runs[at] = run,
            None => self.runs.push(run),
        }
    }

    fn live(&mut self, pane: usize) -> Option<&mut Timeline> {
        self.runs
            .iter_mut()
            .find(|run| run.is_live() && usize::from(run.pane) == pane)
    }

    /// The live run in `pane`, if it has one.
    #[must_use]
    pub fn of_pane(&self, pane: usize) -> Option<&Timeline> {
        self.runs
            .iter()
            .find(|run| run.is_live() && usize::from(run.pane) == pane)
    }

    /// The newest run that was in `pane`, live or closed.
    #[must_use]
    pub fn newest(&self, pane: usize) -> Option<&Timeline> {
        let runs = self.runs.iter().filter(|run| usize::from(run.pane) == pane);
        runs.max_by_key(|run| run.n)
    }

    /// How many entries the newest run in `pane` wrote, kept or dropped: the next one's `seq`.
    #[must_use]
    pub fn total(&self, pane: usize) -> Option<u64> {
        let run = self.newest(pane)?;
        let slots = if run.is_live() { self.slots } else { CLOSED };
        Some(u64::from(run.dropped) + u64::from(run.count(slots)))
    }

    /// Records a command the shell's marks closed in `pane`'s live run.
    pub fn command(&mut self, pane: usize, closed: Closed, now: Instant) {
        let slots = self.slots;
        let Some(run) = self.live(pane) else { return };
        let failed = u32::from(closed.exit.is_some_and(|exit| exit != 0));
        let summary = &mut run.summary;
        summary.commands = summary.commands.saturating_add(1);
        summary.failed = summary.failed.saturating_add(failed);
        summary.errors = summary.errors.saturating_add(failed);
        let row = closed.row.and_then(|row| u32::try_from(row).ok());
        let what = What::Command {
            row: row.unwrap_or(u32::MAX),
            ms: closed.ms,
            exit: closed.exit,
        };
        run.push(what, slots, now);
    }

    /// Records that `pane`'s shown status became `status`, through a report or an agent CLI's
    /// signal. A change the shell's marks made is never recorded (T4).
    pub fn state(&mut self, pane: usize, status: Status, reported: bool, now: Instant) {
        let slots = self.slots;
        let Some(run) = self.live(pane) else { return };
        let summary = &mut run.summary;
        summary.errors = summary
            .errors
            .saturating_add(u32::from(status == Status::Error));
        summary.needs_you = summary
            .needs_you
            .saturating_add(u32::from(status == Status::NeedsYou));
        run.push(What::State { status, reported }, slots, now);
    }

    /// Records each call `log` answered since the last look whose caller or target pane has a
    /// live run, if it wrote or acted, or was refused or failed. Reads that succeed, such as
    /// `pane.list` and `pane.wait`, are never recorded (decision 3). A refused or failed call
    /// from a pane goes only into the caller's run, so no pane can flush another's ring with
    /// calls that do nothing; a refused prompt shows in the target's as its approval.
    pub fn calls(&mut self, log: &Log, now: Instant) {
        let seen = self.seen;
        let new = (log.iter().rev())
            .take_while(|entry| entry.record.seq >= seen)
            .count();
        for entry in log.iter().skip(log.iter().len() - new) {
            let record = &entry.record;
            self.seen = record.seq.saturating_add(1);
            let Some((method, (_, acts))) =
                (METHODS.iter().enumerate()).find(|(_, (name, _))| *name == record.method)
            else {
                continue;
            };
            let outcome = match record.outcome {
                pinfold_api::Outcome::Ok(_) if !acts => continue,
                pinfold_api::Outcome::Ok(_) => Outcome::Ok,
                pinfold_api::Outcome::Refused(_) => Outcome::Refused,
                pinfold_api::Outcome::Error(_) => Outcome::Error,
            };
            let what = What::Api {
                seq: u32::try_from(record.seq & u64::from(u32::MAX)).unwrap_or(0),
                method: u8::try_from(method).unwrap_or(u8::MAX),
                outcome,
                target: record.target.and_then(|pane| u16::try_from(pane).ok()),
            };
            let pane = |pane: Option<u64>| pane.and_then(|pane| usize::try_from(pane).ok());
            let (caller, target) = (pane(record.caller), pane(record.target));
            let into_target = outcome == Outcome::Ok || caller.is_none();
            let target = target.filter(|target| into_target && caller != Some(*target));
            for pane in [caller, target].into_iter().flatten() {
                self.live_push(pane, what, now, |summary| {
                    summary.calls = summary.calls.saturating_add(1);
                });
            }
        }
    }

    /// Records how the window prompt `grant` ended in the runs of the panes it was about: the
    /// caller and target of a read or a send, the run of a rewind. A trust or owner prompt is
    /// about no pane.
    pub fn approval(
        &mut self,
        caller: usize,
        target: usize,
        action: ApprovalAction,
        answer: Answer,
        grant: u64,
        now: Instant,
    ) {
        let panes = match action {
            ApprovalAction::Read | ApprovalAction::Send => [Some(caller), Some(target)],
            ApprovalAction::Rewind => [None, Some(target)],
            ApprovalAction::Trust | ApprovalAction::Owner => return,
        };
        for (pane, other) in panes.into_iter().zip([target, caller]) {
            let Some(pane) = pane else { continue };
            let what = What::Approval {
                action,
                answer,
                other: (action != ApprovalAction::Rewind)
                    .then(|| u16::try_from(other).ok())
                    .flatten(),
                grant: u32::try_from(grant).unwrap_or(u32::MAX),
            };
            self.live_push(pane, what, now, |summary| {
                summary.approvals = summary.approvals.saturating_add(1);
                summary.denied = summary
                    .denied
                    .saturating_add(u32::from(answer == Answer::Denied));
            });
        }
    }

    /// Records checkpoint `n` of the newest run in `pane`, live or closed, as an exit take
    /// lands after its run closed. `changed` becomes the summary's, except a restore's, which
    /// counts what the restore undid.
    pub fn checkpoint(
        &mut self,
        pane: usize,
        n: u32,
        reason: &str,
        complete: bool,
        changed: u32,
        now: Instant,
    ) {
        let Some(reason) = REASONS.iter().position(|word| *word == reason) else {
            return;
        };
        let what = What::Checkpoint {
            n: u16::try_from(n).unwrap_or(u16::MAX),
            reason: u8::try_from(reason).unwrap_or(u8::MAX),
            complete,
            changed,
        };
        let Some(at) = (0..self.runs.len())
            .filter(|at| usize::from(self.runs[*at].pane) == pane)
            .max_by_key(|at| self.runs[*at].n)
        else {
            return;
        };
        let (path, slots) = (self.dir.join(self.runs[at].name()), self.slots);
        let run = &mut self.runs[at];
        if REASONS[reason] != "restore" {
            run.summary.changed = changed;
        }
        if run.is_live() {
            return run.push(what, slots, now);
        }
        // A closed run's file is reopened for this one record and its header kept true.
        let Ok(file) = open_checked(&path, true) else {
            return;
        };
        run.file = Some(file);
        run.push(what, CLOSED, now);
        if let Some(file) = run.file.take() {
            let head = header(run.head, run.count(CLOSED), run.dropped, CLOSED);
            let _ = file.write_all_at(&head, 0);
        }
    }

    fn live_push(&mut self, pane: usize, what: What, now: Instant, count: impl Fn(&mut Summary)) {
        let slots = self.slots;
        if let Some(run) = self.live(pane) {
            count(&mut run.summary);
            run.push(what, slots, now);
        }
    }

    /// Records how `pane`'s program ended, `None` if it still ran as its pane closed, and
    /// closes its run, whose file keeps its newest [`CLOSED`] entries.
    pub fn exit(&mut self, pane: usize, status: Option<ExitStatus>, now: Instant) {
        let slots = self.slots;
        let Some(run) = self.live(pane) else { return };
        let ended = match status {
            Some(status) => match (status.code(), status.signal()) {
                (Some(code), _) => Ended::Code(code.to_le_bytes()[0]),
                (None, Some(signal)) => Ended::Signal(signal.to_le_bytes()[0]),
                (None, None) => Ended::Closed,
            },
            None => Ended::Closed,
        };
        run.push(What::Exit(ended), slots, now);
        run.close(slots, now);
    }

    /// The entries of the newest run in `pane`, oldest first, read from its file. A record that
    /// is short or does not decode is skipped.
    ///
    /// # Errors
    ///
    /// As [`Timelines::page`].
    pub fn entries(&self, pane: usize) -> io::Result<Vec<Entry>> {
        let page = self.page(pane, None, usize::MAX, |_| true)?;
        Ok(page.into_iter().map(|(_, entry)| entry).collect())
    }

    /// For `run.timeline` (ADR-0052 section 7): the newest `count` entries of the newest run in
    /// `pane` whose `seq` is below `before` and that `keep` takes, oldest first, each with its
    /// `seq`, the count of entries the run wrote before it, which holds as older ones drop.
    ///
    /// # Errors
    ///
    /// As [`Timelines::each`].
    pub fn page(
        &self,
        pane: usize,
        before: Option<u64>,
        count: usize,
        keep: impl Fn(&Entry) -> bool,
    ) -> io::Result<Vec<(u64, Entry)>> {
        let mut page = Vec::new();
        self.each(pane, before, count, keep, |seq, entry| {
            page.push((seq, entry))
        })?;
        Ok(page)
    }

    /// Hands the entries [`Timelines::page`] would return to `each`, oldest first, holding none:
    /// it reads the file twice, a record at a time, back to where the page starts and then
    /// forward. The file is untrusted: a record that is short or does not decode is skipped, so
    /// a garbled file gives what still reads.
    ///
    /// # Errors
    ///
    /// Returns an error if a closed run's file is gone, a link or not a private file of ours.
    pub fn each(
        &self,
        pane: usize,
        before: Option<u64>,
        count: usize,
        keep: impl Fn(&Entry) -> bool,
        mut each: impl FnMut(u64, Entry),
    ) -> io::Result<()> {
        let Some(run) = self.newest(pane) else {
            return Ok(());
        };
        let (slots, reopened);
        let file = match &run.file {
            Some(file) => {
                slots = self.slots;
                file
            }
            None => {
                slots = CLOSED;
                reopened = open_checked(&self.dir.join(run.name()), false)?;
                &reopened
            }
        };
        let first = u64::from(run.dropped);
        let held = u64::from(run.count(slots));
        let end = before.map_or(held, |before| before.saturating_sub(first).min(held));
        let read = |index: u64| {
            let slot = run.slot(slots, u16::try_from(index).unwrap_or(u16::MAX));
            let mut bytes = [0; SIZE];
            let at = HEADER + slot * SIZE as u64;
            file.read_exact_at(&mut bytes, at).ok()?;
            Entry::decode(&bytes).filter(&keep)
        };
        let (mut start, mut found) = (end, 0);
        while start > 0 && found < count {
            start -= 1;
            found += usize::from(read(start).is_some());
        }
        // The file may change under it, so the second pass is held to what the first found.
        let (mut index, mut given) = (start, 0);
        while index < end && given < found {
            if let Some(entry) = read(index) {
                each(first + index, entry);
                given += 1;
            }
            index += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "timeline_tests.rs"]
mod tests;
