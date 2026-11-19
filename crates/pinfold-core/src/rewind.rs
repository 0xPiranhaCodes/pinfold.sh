//! Agent runs and their checkpoints (ADR-0050 sections 4 and 7, amendment of slice 5).
//!
//! The core keeps a fixed 48-byte [`Run`] per `agent.launch`, at most [`MAX_RUNS`], and no
//! checkpoint list: `pinfold checkpoint take|list|prune`, the sibling binary, do the work and
//! read the store. At most two of them run at once, each waited on by a worker thread of its
//! own that kills it at its bound and hands back one JSON line of at most 4 KiB, so a slow
//! disk or a crashed child never stalls the event loop or touches a PTY. Launches and API calls
//! wait in a queue of [`QUEUE`]; done and exit takes wait as flags in their run's record.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use mio::Waker;
use mio::unix::pipe;
use serde_json::Value;

/// Runs whose records the core keeps.
pub const MAX_RUNS: usize = 64;
/// Launches and API calls waiting for a worker.
pub const QUEUE: usize = 16;
const WORKERS: usize = 2;
/// A done or exit take still running after this is killed.
pub const TAKE_BOUND: Duration = Duration::from_secs(120);
/// An API take, whose caller waits 62 s for its answer.
pub const API_BOUND: Duration = Duration::from_secs(60);
/// The prune after a take; its `repack` and `prune` have 300 s each inside.
pub const PRUNE_BOUND: Duration = Duration::from_secs(900);
/// A list or a restore's plan still running after this is killed.
pub const LIST_BOUND: Duration = Duration::from_secs(10);
/// Seconds between two done takes of a run, and two API takes.
const DONE_EVERY: u32 = 60;
const API_EVERY: u32 = 30;
/// The longest line read from a child.
const LINE: usize = 4096;
const POLL: Duration = Duration::from_millis(10);
const STACK: usize = 64 * 1024;
/// A field of a [`Run`] with nothing in it.
const NONE: u32 = u32::MAX;
/// What a child killed at its bound answers.
pub const KILLED: &str = "pinfold checkpoint was killed at its bound";
/// Why an API call is refused while the queue is full.
pub const BUSY: &str = "the core is busy taking checkpoints; try again";

const CLOSED: u8 = 1;
const WANT_DONE: u8 = 2;
const WANT_EXIT: u8 = 4;
const WAS_DONE: u8 = 8;

/// Why a run has no checkpoint, for the strip's `no checkpoint` (slice 6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Missing {
    /// It has one.
    #[default]
    None,
    /// The launch take passed `wait_s`.
    TookOver,
    /// No git, or on macOS no Command Line Tools.
    NoGit,
    /// The take failed or was refused.
    Failed,
    /// 64 runs were live.
    TooManyRuns,
    /// The queue was full.
    Busy,
}

impl Missing {
    /// The strip's words after `no checkpoint ·`, or empty.
    #[must_use]
    pub fn word(self) -> &'static str {
        pinfold_proto::link::MISSING[self as usize]
    }
}

/// One run: 48 bytes, and nothing else kept for it (ADR-0050 section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// The core's start, Unix seconds: the id is `r<start>-<n>`.
    start: u64,
    /// The store's key, 16 hex digits.
    key: [u8; 8],
    n: u32,
    /// The agent's pane, or [`NONE`] until it starts.
    pane: u32,
    /// The pane that launched it, which reaches it once it closed.
    launcher: u32,
    /// The newest `done` or `exit` checkpoint's `n`, or [`NONE`].
    newest: u32,
    /// The second after the last done take, and after the last API take, or 0.
    done_at: u32,
    api_at: u32,
    missing: Missing,
    flags: u8,
}

const _: () = assert!(size_of::<Run>() == 48);

impl Run {
    /// `r<start>-<n>`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("r{}-{}", self.start, self.n)
    }

    /// The agent's pane, once it started.
    #[must_use]
    pub fn pane(&self) -> Option<usize> {
        (self.pane != NONE).then_some(self.pane as usize)
    }

    /// The pane that launched it.
    #[must_use]
    pub fn launcher(&self) -> usize {
        self.launcher as usize
    }

    /// The newest `done` or `exit` checkpoint's `n`.
    #[must_use]
    pub fn newest(&self) -> Option<u32> {
        (self.newest != NONE).then_some(self.newest)
    }

    /// Why it has no checkpoint.
    #[must_use]
    pub fn missing(&self) -> Missing {
        self.missing
    }

    /// Whether its pane closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.flags & CLOSED != 0
    }

    /// The store's key, as `pinfold checkpoint` names it.
    #[must_use]
    pub fn key(&self) -> String {
        self.key.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// `[rewind]`'s values the core passes on, read when it starts. Plain numbers, so a core with
/// no run allocates nothing for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Take a checkpoint when a run's status becomes done.
    pub on_done: bool,
    /// How long a launch waits for its checkpoint.
    pub wait: Duration,
    /// `take`'s caps.
    pub max_files: u64,
    /// In MiB.
    pub max_file_mib: u64,
    /// In MiB.
    pub max_new_mib: u64,
    /// Cover what `.gitignore` files ignore too.
    pub include_ignored: bool,
    /// `prune`'s limits, in MiB.
    pub max_store_mib: u64,
    /// Days.
    pub keep_days: u64,
    /// Checkpoints a run keeps.
    pub keep_per_run: u64,
}

impl Settings {
    fn take(&self) -> Vec<OsString> {
        let (files, file, new) = (self.max_files, self.max_file_mib, self.max_new_mib);
        let mut args = vec![
            "--max-files".into(),
            files.to_string().into(),
            "--max-file-mib".into(),
            file.to_string().into(),
            "--max-new-mib".into(),
            new.to_string().into(),
        ];
        if self.include_ignored {
            args.push("--include-ignored".into());
        }
        args
    }

    fn prune(&self) -> [OsString; 6] {
        let (store, days, kept) = (self.max_store_mib, self.keep_days, self.keep_per_run);
        [
            "--max-store-mib".into(),
            store.to_string().into(),
            "--keep-days".into(),
            days.to_string().into(),
            "--keep-per-run".into(),
            kept.to_string().into(),
        ]
    }
}

/// What a worker does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Launch,
    Take(&'static str),
    List,
    Plan,
    Restore,
}

impl Work {
    /// Whether it takes the store's lock, and a prune follows it.
    fn locks(self) -> bool {
        !matches!(self, Self::List | Self::Plan)
    }
}

/// A launch or API call waiting for a worker.
struct Queued<T> {
    run: usize,
    work: Work,
    payload: T,
    /// A launch's directory and when it gives up.
    launch: Option<(PathBuf, Instant)>,
    /// A plan's or restore's arguments after the run: `<c> <shown> [-- <path>...]`.
    args: Vec<OsString>,
}

/// A worker's job, as the core tracks it.
struct Slot<T> {
    run: usize,
    key: [u8; 8],
    work: Work,
    payload: Option<T>,
    /// Its take answered and its prune runs, which may lock any store.
    pruning: bool,
}

enum Note {
    Answer(usize, Result<Value, String>),
    Free(usize),
}

/// A launch or API call whose work ended: its payload, its run, and the child's line or why
/// there is none.
#[derive(Debug)]
pub struct Finished<T> {
    /// What the caller parked.
    pub payload: T,
    /// The run's index.
    pub run: usize,
    /// The child's JSON line, or why there is none ([`KILLED`] at its bound).
    pub result: Result<Value, String>,
}

/// The runs of this core and their checkpoint work. Nothing is allocated until a launch.
pub struct Rewind<T> {
    program: PathBuf,
    settings: Settings,
    start: u64,
    epoch: Instant,
    next: u32,
    runs: Vec<Run>,
    slots: [Option<Slot<T>>; WORKERS],
    queue: VecDeque<Queued<T>>,
    notes: Option<(Sender<Note>, Receiver<Note>)>,
    waker: Option<Arc<Waker>>,
    /// One bit a record: the window was told its strip as it is now.
    told: u64,
}

impl<T> Rewind<T> {
    /// Runs `program checkpoint ...` with `settings`, naming runs after `start`, the core's start
    /// in Unix seconds, and wakes `waker` when a child answers.
    #[must_use]
    pub fn new(
        program: PathBuf,
        settings: Settings,
        start: u64,
        waker: Option<Arc<Waker>>,
    ) -> Self {
        Self {
            program,
            settings,
            start,
            epoch: Instant::now(),
            next: 0,
            runs: Vec::new(),
            slots: [None, None],
            queue: VecDeque::new(),
            notes: None,
            waker,
            told: 0,
        }
    }

    /// The run at `run`.
    #[must_use]
    pub fn run(&self, run: usize) -> Option<&Run> {
        self.runs.get(run)
    }

    /// The run whose agent is `pane`.
    #[must_use]
    pub fn of_pane(&self, pane: usize) -> Option<usize> {
        self.runs.iter().position(|run| run.pane() == Some(pane))
    }

    /// The run whose id is `id`, `r<start>-<n>`.
    #[must_use]
    pub fn of_id(&self, id: &str) -> Option<usize> {
        self.runs.iter().position(|run| run.id() == id)
    }

    /// Whether a queued or running call's payload is `parked`.
    pub fn any(&self, mut parked: impl FnMut(&T) -> bool) -> bool {
        self.queue.iter().any(|queued| parked(&queued.payload))
            || (self.slots.iter().flatten())
                .any(|slot| slot.payload.as_ref().is_some_and(&mut parked))
    }

    /// The first live run whose strip the window was not told, with its pane and why it has no
    /// checkpoint, for [`Rewind::told`].
    #[must_use]
    pub fn untold(&self) -> Option<(usize, usize, Missing)> {
        self.runs.iter().enumerate().find_map(|(at, run)| {
            let pane = run
                .pane()
                .filter(|_| !run.is_closed() && self.told & 1 << at == 0)?;
            Some((at, pane, run.missing))
        })
    }

    /// Notes that the window was told `run`'s strip.
    pub fn told(&mut self, run: usize) {
        self.told |= 1_u64
            .checked_shl(u32::try_from(run).unwrap_or(u32::MAX))
            .unwrap_or(0);
    }

    /// Forgets what any window was told, as a new one knows nothing.
    pub fn untell(&mut self) {
        self.told = 0;
    }

    /// Queues `pinfold checkpoint plan` of `run` from `c` over the view checkpoint `shown`,
    /// limited to `paths` if any, for `checkpoint.restore`'s prompt.
    ///
    /// # Errors
    ///
    /// Gives `payload` back with [`BUSY`].
    pub fn plan(
        &mut self,
        run: usize,
        (c, shown): (u32, u32),
        paths: Option<&[String]>,
        payload: T,
    ) -> Result<(), (T, &'static str)> {
        self.restore_work(Work::Plan, run, (c, shown), paths, payload)
    }

    /// Queues `pinfold checkpoint restore`, as [`Rewind::plan`], once the window allowed it.
    ///
    /// # Errors
    ///
    /// Gives `payload` back with [`BUSY`].
    pub fn restore(
        &mut self,
        run: usize,
        (c, shown): (u32, u32),
        paths: Option<&[String]>,
        payload: T,
    ) -> Result<(), (T, &'static str)> {
        self.restore_work(Work::Restore, run, (c, shown), paths, payload)
    }

    fn restore_work(
        &mut self,
        work: Work,
        run: usize,
        (c, shown): (u32, u32),
        paths: Option<&[String]>,
        payload: T,
    ) -> Result<(), (T, &'static str)> {
        let mut args: Vec<OsString> = vec![c.to_string().into(), shown.to_string().into()];
        // An absent list means all, so none is never sent as an empty one.
        if let Some(paths) = paths.filter(|paths| !paths.is_empty()) {
            args.push("--".into());
            args.extend(paths.iter().map(OsString::from));
        }
        if self.queue.len() >= QUEUE || run >= self.runs.len() {
            return Err((payload, BUSY));
        }
        self.queue.push_back(Queued {
            run,
            work,
            payload,
            launch: None,
            args,
        });
        Ok(())
    }

    /// Records a new run launched from `launcher` in the working directory `dir`, reusing the
    /// oldest closed run's record at [`MAX_RUNS`].
    ///
    /// # Errors
    ///
    /// [`Missing::TooManyRuns`] with every record live, and [`Missing::Failed`] if `dir` does
    /// not resolve.
    pub fn open(&mut self, launcher: usize, dir: &Path) -> Result<usize, Missing> {
        let resolved = dir.canonicalize().map_err(|_| Missing::Failed)?;
        let hash = pinfold_api::trust::sha256(resolved.as_os_str().as_encoded_bytes());
        let mut key = [0; 8];
        for (byte, pair) in key.iter_mut().zip(hash.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).unwrap_or_default();
            *byte = u8::from_str_radix(pair, 16).unwrap_or_default();
        }
        let run = Run {
            start: self.start,
            key,
            n: self.next,
            pane: NONE,
            launcher: u32::try_from(launcher).unwrap_or(NONE),
            newest: NONE,
            done_at: 0,
            api_at: 0,
            missing: Missing::None,
            flags: 0,
        };
        let at = if self.runs.len() < MAX_RUNS {
            self.runs.push(run);
            self.runs.len() - 1
        } else {
            let free = |at: &usize| {
                let record = &self.runs[*at];
                record.is_closed()
                    && record.flags & (WANT_DONE | WANT_EXIT) == 0
                    && !self.queue.iter().any(|queued| queued.run == *at)
                    && !self.slots.iter().flatten().any(|slot| slot.run == *at)
            };
            let oldest = (0..self.runs.len())
                .filter(free)
                .min_by_key(|at| self.runs[*at].n);
            let at = oldest.ok_or(Missing::TooManyRuns)?;
            self.runs[at] = run;
            at
        };
        self.next = self.next.wrapping_add(1);
        Ok(at)
    }

    /// Queues `run`'s launch checkpoint of `dir`, given up at `wait_s` from `now`.
    ///
    /// # Errors
    ///
    /// Gives `payload` back with [`Missing::Busy`] when the queue is full; the agent then
    /// starts at once.
    pub fn launch(
        &mut self,
        run: usize,
        dir: PathBuf,
        payload: T,
        now: Instant,
    ) -> Result<(), (T, Missing)> {
        let by = now + self.settings.wait;
        self.enqueue(run, Work::Launch, payload, Some((dir, by)))
            .map_err(|payload| {
                self.set_missing(run, Missing::Busy);
                (payload, Missing::Busy)
            })
    }

    /// Records that `run`'s agent started in `pane`, or with `None` that it did not.
    pub fn started(&mut self, run: usize, pane: Option<usize>) {
        if let Some(record) = self.runs.get_mut(run) {
            match pane.and_then(|pane| u32::try_from(pane).ok()) {
                Some(pane) => record.pane = pane,
                None => record.flags |= CLOSED,
            }
        }
    }

    /// Queues `checkpoint.take` for `run`, once in 30 s.
    ///
    /// # Errors
    ///
    /// Gives `payload` back with why: the rate limit, or [`BUSY`].
    pub fn take(&mut self, run: usize, payload: T, now: Instant) -> Result<(), (T, &'static str)> {
        let second = self.second(now);
        let Some(record) = self.runs.get_mut(run) else {
            return Err((payload, "no such run"));
        };
        if record.api_at != 0 && second - record.api_at < API_EVERY {
            return Err((payload, "checkpoint.take runs once in 30 s per run"));
        }
        self.enqueue(run, Work::Take("api"), payload, None)
            .map_err(|payload| (payload, BUSY))?;
        self.runs[run].api_at = second;
        Ok(())
    }

    /// Queues the `view` checkpoint of `run` that a diff viewer of it shows (ADR-0050 section
    /// 6). With `limited`, for an API caller, it shares `checkpoint.take`'s once in 30 s, so no
    /// caller takes checkpoints faster through `tool.open`; the window's, the person's, is not.
    ///
    /// # Errors
    ///
    /// Gives `payload` back with why: the rate limit, or [`BUSY`].
    pub fn view(
        &mut self,
        run: usize,
        payload: T,
        limited: Option<Instant>,
    ) -> Result<(), (T, &'static str)> {
        let second = limited.map(|now| self.second(now));
        let Some(record) = self.runs.get(run) else {
            return Err((payload, "no such run"));
        };
        if let Some(second) = second
            && record.api_at != 0
            && second - record.api_at < API_EVERY
        {
            return Err((payload, "checkpoint.take runs once in 30 s per run"));
        }
        self.enqueue(run, Work::Take("view"), payload, None)
            .map_err(|payload| (payload, BUSY))?;
        if let Some(second) = second {
            self.runs[run].api_at = second;
        }
        Ok(())
    }

    /// Queues `checkpoint.list` for `run`.
    ///
    /// # Errors
    ///
    /// Gives `payload` back with [`BUSY`].
    pub fn list(&mut self, run: usize, payload: T) -> Result<(), (T, &'static str)> {
        self.enqueue(run, Work::List, payload, None)
            .map_err(|payload| (payload, BUSY))
    }

    fn enqueue(
        &mut self,
        run: usize,
        work: Work,
        payload: T,
        launch: Option<(PathBuf, Instant)>,
    ) -> Result<(), T> {
        if self.queue.len() >= QUEUE || run >= self.runs.len() {
            return Err(payload);
        }
        self.queue.push_back(Queued {
            run,
            work,
            payload,
            launch,
            args: Vec::new(),
        });
        Ok(())
    }

    /// Looks at each live run's pane: `state(pane)` says whether it closed and whether its
    /// status is done. A pane that closed gets an exit take; one that became done, a done take
    /// at most once in 60 s if `on_done`.
    pub fn observe(&mut self, now: Instant, mut state: impl FnMut(usize) -> (bool, bool)) {
        let second = self.second(now);
        for run in &mut self.runs {
            let Some(pane) = run.pane().filter(|_| !run.is_closed()) else {
                continue;
            };
            let (closed, done) = state(pane);
            if closed {
                run.flags = (run.flags | CLOSED | WANT_EXIT) & !WANT_DONE;
                continue;
            }
            let due = run.done_at == 0 || second - run.done_at >= DONE_EVERY;
            if done && run.flags & WAS_DONE == 0 && self.settings.on_done && due {
                run.flags |= WANT_DONE;
                run.done_at = second;
            }
            run.flags = if done {
                run.flags | WAS_DONE
            } else {
                run.flags & !WAS_DONE
            };
        }
    }

    /// When the first queued launch gives up, for the event loop's wait.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.queue
            .iter()
            .filter_map(|queued| Some(queued.launch.as_ref()?.1))
            .min()
    }

    /// Takes in what the workers answered, gives up queued launches past `wait_s`, starts what
    /// can start, and returns each launch and API call that ended.
    pub fn poll(&mut self, now: Instant) -> Vec<Finished<T>> {
        let mut finished = Vec::new();
        while let Some(note) = self
            .notes
            .as_ref()
            .and_then(|(_, notes)| notes.try_recv().ok())
        {
            match note {
                Note::Answer(slot, result) => {
                    if let Some(done) = self.answered(slot, result) {
                        finished.push(done);
                    }
                }
                Note::Free(slot) => self.slots[slot] = None,
            }
        }
        let mut kept = VecDeque::with_capacity(0);
        for queued in self.queue.drain(..) {
            match queued.launch {
                Some((_, by)) if by <= now => finished.push(Finished {
                    payload: queued.payload,
                    run: queued.run,
                    result: Err(KILLED.to_owned()),
                }),
                _ => kept.push_back(queued),
            }
        }
        for gave_up in &finished {
            if gave_up.result.as_ref().is_err_and(|why| why == KILLED) {
                self.set_missing(gave_up.run, Missing::TookOver);
            }
        }
        self.queue = kept;
        while let Some(slot) = self.slots.iter().position(Option::is_none) {
            if !self.start_next(slot, now) {
                break;
            }
        }
        finished
    }

    fn set_missing(&mut self, run: usize, missing: Missing) {
        if let Some(record) = self.runs.get_mut(run) {
            if record.missing != missing {
                self.told &= !(1_u64
                    .checked_shl(u32::try_from(run).unwrap_or(u32::MAX))
                    .unwrap_or(0));
            }
            record.missing = missing;
        }
    }

    fn answered(&mut self, slot: usize, result: Result<Value, String>) -> Option<Finished<T>> {
        let busy = self.slots[slot].as_mut()?;
        let run = busy.run;
        let work = busy.work;
        busy.pruning = work.locks();
        let payload = busy.payload.take();
        if let Some(record) = self.runs.get_mut(run) {
            let n = result.as_ref().ok().and_then(|line| {
                let checkpoint = line["checkpoint"].as_str()?;
                checkpoint.rsplit_once('/')?.1.parse::<u32>().ok()
            });
            let missing = match (work, n, &result) {
                (Work::List | Work::Plan | Work::Restore, ..) => record.missing,
                (Work::Take("done" | "exit"), Some(n), _) => {
                    record.newest = n;
                    Missing::None
                }
                (_, Some(_), _) => Missing::None,
                (Work::Launch, None, Err(why)) if why == KILLED => Missing::TookOver,
                (Work::Launch, None, Err(why)) if why.contains("no git") => Missing::NoGit,
                (Work::Launch, None, _) => Missing::Failed,
                // A later take that fails leaves the strip as it was.
                (Work::Take(_), None, _) => record.missing,
            };
            self.set_missing(run, missing);
        }
        payload.map(|payload| Finished {
            payload,
            run,
            result,
        })
    }

    /// Whether a take of the store `key` would meet another worker's lock.
    fn locked(&self, key: [u8; 8]) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|slot| slot.pruning || (slot.key == key && slot.work.locks()))
    }

    /// Starts in `slot` the first queued launch that can start, else a done or exit take, so
    /// API calls never starve them, else the first queued call.
    fn start_next(&mut self, slot: usize, now: Instant) -> bool {
        let key = |run: usize| self.runs[run].key;
        let can = |queued: &Queued<T>| !queued.work.locks() || !self.locked(key(queued.run));
        let launch = self
            .queue
            .iter()
            .position(|queued| queued.work == Work::Launch && can(queued));
        let wanted = (0..self.runs.len()).find(|&run| {
            self.runs[run].flags & (WANT_DONE | WANT_EXIT) != 0 && !self.locked(key(run))
        });
        let queued = match (launch, wanted) {
            (Some(at), _) => Some(at),
            (None, Some(_)) => None,
            (None, None) => self.queue.iter().position(can),
        };
        if let Some(queued) = queued.and_then(|at| self.queue.remove(at)) {
            let id = self.runs[queued.run].id();
            let (args, bound) = match (queued.work, &queued.launch) {
                (Work::List, _) => {
                    let key = self.runs[queued.run].key();
                    (args(&["list", &id, "--key", &key]), LIST_BOUND)
                }
                (Work::Plan | Work::Restore, _) => {
                    let (word, bound) = match queued.work {
                        Work::Plan => ("plan", LIST_BOUND),
                        _ => ("restore", API_BOUND),
                    };
                    let mut args = args(&[word, &id]);
                    let (numbers, paths) = queued.args.split_at(queued.args.len().min(2));
                    args.extend(numbers.iter().cloned());
                    args.extend(["--key".into(), self.runs[queued.run].key().into()]);
                    args.extend(paths.iter().cloned());
                    (args, bound)
                }
                (Work::Launch, Some((dir, by))) => {
                    let mut args = args(&["take", &id, "launch"]);
                    args.extend(self.settings.take());
                    args.extend([OsString::from("--"), dir.clone().into_os_string()]);
                    (args, by.saturating_duration_since(now))
                }
                (work, _) => (self.take_args(queued.run, work), API_BOUND),
            };
            self.begin(
                slot,
                queued.run,
                queued.work,
                Some(queued.payload),
                args,
                bound,
            );
            return true;
        }
        let Some(run) = wanted else {
            return false;
        };
        let record = &mut self.runs[run];
        let reason = if record.flags & WANT_EXIT != 0 {
            "exit"
        } else {
            "done"
        };
        // An exit take covers a done take still waiting.
        record.flags &= !(WANT_DONE | WANT_EXIT);
        let args = self.take_args(run, Work::Take(reason));
        self.begin(slot, run, Work::Take(reason), None, args, TAKE_BOUND);
        true
    }

    fn take_args(&self, run: usize, work: Work) -> Vec<OsString> {
        let reason = match work {
            Work::Take(reason) => reason,
            _ => "api",
        };
        let record = &self.runs[run];
        let mut args = args(&["take", &record.id(), reason]);
        args.extend(self.settings.take());
        args.extend(["--key".into(), record.key().into()]);
        args
    }

    /// Hands `args` to a worker thread in `slot`, then the prune for a take.
    fn begin(
        &mut self,
        slot: usize,
        run: usize,
        work: Work,
        payload: Option<T>,
        take: Vec<OsString>,
        bound: Duration,
    ) {
        let prune = work.locks().then(|| {
            let mut prune = args(&["prune"]);
            for live in self.runs.iter().filter(|run| !run.is_closed()) {
                prune.extend(["--live".into(), live.id().into()]);
            }
            prune.extend(self.settings.prune());
            prune
        });
        self.slots[slot] = Some(Slot {
            run,
            key: self.runs[run].key,
            work,
            payload,
            pruning: false,
        });
        let (notes, _) = self.notes.get_or_insert_with(mpsc::channel);
        let (notes, waker, program) = (notes.clone(), self.waker.clone(), self.program.clone());
        let fallback = notes.clone();
        let started = thread::Builder::new()
            .name("pinfold-checkpoint".to_owned())
            .stack_size(STACK)
            .spawn(move || {
                let tell = |note| {
                    let _ = notes.send(note);
                    if let Some(waker) = &waker {
                        let _ = waker.wake();
                    }
                };
                tell(Note::Answer(slot, child(&program, &take, bound)));
                if let Some(prune) = prune {
                    let _ = child(&program, &prune, PRUNE_BOUND);
                }
                tell(Note::Free(slot));
            });
        if let Err(error) = started {
            let why = format!("cannot start a checkpoint worker: {}", error.kind());
            let _ = fallback.send(Note::Answer(slot, Err(why)));
            let _ = fallback.send(Note::Free(slot));
        }
    }

    fn second(&self, now: Instant) -> u32 {
        let since = now.saturating_duration_since(self.epoch).as_secs();
        // Plus one, so 0 is never.
        u32::try_from(since).unwrap_or(u32::MAX - 1) + 1
    }
}

/// The texts of the window's `Rewind` prompt (ADR-0050 section 6), from `plan`, the line
/// `pinfold checkpoint plan` printed from the store: the directory, when and why the checkpoint
/// was taken, as long ago from `now` (Unix seconds), who asks (`tool`, the caller's tool pane,
/// or a terminal outside every pane), the counts, notes, each path shown with `! ` before one
/// changed later and `  ` before the rest, and how many more. Every text that came from a file
/// name or the store is escaped and cut, so nothing in it can pass for PinFold's own words.
#[must_use]
pub fn prompt_texts(plan: &Value, tool: Option<usize>, running: bool, now: u64) -> Vec<String> {
    let count = |name: &str| plan[name].as_u64().unwrap_or(0);
    let shown = |text: &str| {
        let mut out = String::new();
        crate::tool::escape(text, &mut out);
        out.chars().take(SHOWN).collect::<String>()
    };
    let ago = now.saturating_sub(count("time"));
    let ago = match ago {
        0..60 => format!("{ago} s"),
        60..3600 => format!("{} min", ago / 60),
        3600..86_400 => format!("{} h", ago / 3600),
        _ => format!("{} d", ago / 86_400),
    };
    let reason = plan["reason"].as_str().unwrap_or("unknown");
    let caller = match tool {
        Some(pane) => format!("asked by the tool in pane {pane}"),
        None => "asked by a terminal outside every pane".to_owned(),
    };
    let mut counts = format!(
        "restore {} · delete {} the agent added · bring back {} it deleted",
        count("restore"),
        count("delete"),
        count("bring_back")
    );
    for (name, words) in [
        ("later", "changed later"),
        ("not_covered", "not covered, left alone"),
    ] {
        if count(name) > 0 {
            counts.push_str(&format!(" · {} {words}", count(name)));
        }
    }
    let mut texts = vec![
        shown(plan["dir"].as_str().unwrap_or_default()),
        format!("since {ago} ago ({})", shown(reason)),
        caller,
        counts,
    ];
    if plan["complete"] != true {
        texts.push("the checkpoint is incomplete, so nothing is deleted".to_owned());
    }
    if running {
        texts.push("the agent is still running, and may change these files again".to_owned());
    }
    for path in plan["paths"].as_array().into_iter().flatten() {
        let mark = if path[2] == true { "!" } else { " " };
        let what = path[1].as_str().unwrap_or_default();
        let path = path[0].as_str().unwrap_or_default();
        texts.push(format!("{mark} {} {}", shown(what), shown(path)));
    }
    if count("more") > 0 {
        texts.push(format!("and {} more", count("more")));
    }
    texts
}

/// The most characters of one shown text: a path cut to 256 bytes stays whole unless escaping
/// grew it (ADR-0050's memory row: at most 10 paths of 256).
const SHOWN: usize = 256;

/// The flash's words for a restore that failed: a fixed phrase for each reason a restore gives,
/// never the child's own text, which could name a path or carry a store's forged message.
#[must_use]
pub fn restore_error(why: &str) -> &'static str {
    if why == KILLED {
        "the restore took over 60 s"
    } else if why.contains("no longer exists") {
        "the checkpoint no longer exists"
    } else if why.contains("another checkpoint is being taken") {
        "another checkpoint is being taken or pruned"
    } else if why == BUSY {
        "the core is busy taking checkpoints"
    } else {
        "the restore was refused or failed"
    }
}

fn args(words: &[&str]) -> Vec<OsString> {
    std::iter::once("checkpoint")
        .chain(words.iter().copied())
        .map(OsString::from)
        .collect()
}

/// Runs `program args`, reads at most [`LINE`] bytes of its standard output, and kills it at
/// `bound`. Its line, or its `error`, or why there is neither.
fn child(program: &Path, args: &[OsString], bound: Duration) -> Result<Value, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    pinfold_sys::mark_strays(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start pinfold checkpoint: {}", error.kind()))?;
    let mut stdout = child.stdout.take().map(pipe::Receiver::from);
    if let Some(stdout) = &stdout {
        let _ = stdout.set_nonblocking(true);
    }
    let (mut line, mut read) = ([0_u8; LINE], 0);
    let deadline = Instant::now() + bound;
    loop {
        if let Some(stdout) = stdout.as_mut() {
            read_some(stdout, &mut line, &mut read);
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(KILLED.to_owned());
            }
        }
    }
    if let Some(stdout) = stdout.as_mut() {
        read_some(stdout, &mut line, &mut read);
    }
    let line: Value = serde_json::from_slice(line[..read].trim_ascii())
        .map_err(|_| "pinfold checkpoint gave no answer".to_owned())?;
    match line.get("error") {
        Some(error) => Err(error.as_str().unwrap_or("failed").to_owned()),
        None => Ok(line),
    }
}

fn read_some(stdout: &mut pipe::Receiver, line: &mut [u8; LINE], read: &mut usize) {
    while let Some(rest) = line.get_mut(*read..).filter(|rest| !rest.is_empty()) {
        match stdout.read(rest) {
            Ok(0) | Err(_) => return,
            Ok(count) => *read += count,
        }
    }
}

#[cfg(test)]
#[path = "rewind_tests.rs"]
mod tests;
