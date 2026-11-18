//! The webhook sender (ADR-0051 section 5): a job on the core's worker thread, started by the
//! first alert and ending after a minute idle, that runs the system `curl` for each queued
//! record, retries, and counts what it gives up on.
//!
//! The core thread only checks the limits and queues a fixed 20-byte [`Record`]; the body and
//! `curl`'s config are built on the worker, when sent, and dropped after.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use mio::Waker;
use mio::unix::pipe;

use super::{Format, Post, Webhook};
use crate::status::Status;

/// Records the queue holds; past it the oldest is dropped and counted as given up.
pub const QUEUE: usize = 16;
/// Posts in any [`WINDOW`], in all.
pub const MOST: usize = 10;
/// The span [`MOST`] counts over.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// The least time between two posts for one pane.
pub const PER_PANE: Duration = Duration::from_secs(60);

/// The only variables `curl` gets: its proxy settings.
pub const PROXIES: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// `curl`'s exits for connect, DNS, TLS and timeout failures, which are tried again.
const RETRIED_EXITS: [i32; 6] = [6, 7, 28, 35, 52, 56];
/// The most standard output read from `curl`: `%{http_code}` is three bytes.
const MOST_OUTPUT: usize = 16;
/// How often a running `curl` is checked.
const POLL: Duration = Duration::from_millis(10);
/// The worker runs `curl` and builds one body at a time.
const STACK: usize = 64 * 1024;

/// One alert waiting to be posted: fixed fields, no text, 20 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// The pane's id.
    pub pane: u32,
    /// The tab's number, from 1.
    pub tab: u32,
    /// When it alerted, in Unix seconds.
    pub at: u32,
    /// When the pane came to the state, in Unix seconds.
    pub since: u32,
    /// The state that alerted.
    pub status: Status,
    /// Whether output, not the agent's report, set it.
    pub from_output: bool,
    /// The agent profile's index in the config's.
    pub profile: Option<u8>,
}

/// How long the worker waits on each step. Only tests shorten them.
#[derive(Clone, Copy, Debug)]
struct Times {
    /// Before the second try, and before the third.
    retries: [Duration; 2],
    /// A `curl` still running after this is killed.
    kill: Duration,
    /// The worker ends after this long with nothing queued.
    idle: Duration,
}

/// ADR-0051's.
const TIMES: Times = Times {
    retries: [Duration::from_secs(5), Duration::from_secs(30)],
    kill: Duration::from_secs(15),
    idle: Duration::from_secs(60),
};

/// Finds `curl` once, as the core starts: `/usr/bin/curl` on macOS, the first `curl` on an
/// absolute entry of `PATH` elsewhere.
#[must_use]
pub fn find_curl() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(PathBuf::from("/usr/bin/curl")).filter(|curl| executable(curl))
    } else {
        on_path(std::env::var_os("PATH").as_deref())
    }
}

/// The first executable `curl` in an absolute entry of `path`, a `PATH` value.
fn on_path(path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("curl"))
        .find(|curl| executable(curl))
}

fn executable(program: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(program)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// What the worker needs, fixed when the core starts.
struct Job {
    curl: PathBuf,
    hook: Webhook,
    format: Format,
    /// The config's agent profile names, by index.
    agents: Box<[Box<str>]>,
    times: Times,
    /// Where `curl`'s proxy variables come from.
    vars: fn(&str) -> Option<OsString>,
    /// Wakes the core when `failed` changes.
    waker: Option<Arc<Waker>>,
}

/// The records waiting, oldest first, in a fixed ring.
#[derive(Debug, Default)]
struct Queue {
    records: [Option<Record>; QUEUE],
    first: usize,
    len: usize,
    /// Whether a worker thread is running.
    running: bool,
}

impl Queue {
    /// Adds `record`, dropping the oldest when full. Returns whether one was dropped.
    fn push(&mut self, record: Record) -> bool {
        let full = self.len == QUEUE;
        if full {
            self.first = (self.first + 1) % QUEUE;
            self.len -= 1;
        }
        self.records[(self.first + self.len) % QUEUE] = Some(record);
        self.len += 1;
        full
    }

    fn pop(&mut self) -> Option<Record> {
        if self.len == 0 {
            return None;
        }
        let record = self.records[self.first].take();
        self.first = (self.first + 1) % QUEUE;
        self.len -= 1;
        record
    }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    failed: AtomicU32,
    job: Job,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // Nothing panics while holding it, but a poisoned queue is still a queue.
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one alert given up on, and tells the core.
    fn give_up(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        if let Some(waker) = &self.job.waker {
            let _ = waker.wake();
        }
    }
}

/// Sends a webhook's alerts. Owned by the core thread, which calls [`Sender::push`].
///
/// ```no_run
/// use std::time::{Instant, SystemTime};
/// use pinfold_core::status::Status;
/// use pinfold_core::webhook::{Format, Record, Sender, Webhook, find_curl};
///
/// let hook = Webhook::new("https://ntfy.sh/topic").unwrap().unwrap();
/// let mut sender = Sender::new(find_curl().unwrap(), hook, Format::Text, Box::new([]), None);
/// let record = Record {
///     pane: 7, tab: 2, at: 1_763_112_000, since: 1_763_111_958,
///     status: Status::NeedsYou, from_output: false, profile: None,
/// };
/// assert!(sender.push(record, Instant::now()));
/// ```
pub struct Sender {
    shared: Arc<Shared>,
    limits: Limits,
}

impl Sender {
    /// Sends to `hook` through `curl` in `format`, naming agents from `agents`, the config's
    /// profile names by index, and wakes `waker` when an alert is given up on. No thread starts
    /// until the first [`Sender::push`].
    #[must_use]
    pub fn new(
        curl: PathBuf,
        hook: Webhook,
        format: Format,
        agents: Box<[Box<str>]>,
        waker: Option<Arc<Waker>>,
    ) -> Self {
        let job = Job {
            curl,
            hook,
            format,
            agents,
            times: TIMES,
            vars: |name| std::env::var_os(name),
            waker,
        };
        Self::with_job(job)
    }

    fn with_job(job: Job) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue::default()),
                wake: Condvar::new(),
                failed: AtomicU32::new(0),
                job,
            }),
            limits: Limits::new(Instant::now()),
        }
    }

    /// Queues `record` unless the limits refuse it: one post a pane a minute and [`MOST`] in
    /// any ten minutes. Past [`QUEUE`] the oldest is dropped and counted as given up. Starts
    /// the worker if none runs. Returns whether it was queued.
    pub fn push(&mut self, record: Record, now: Instant) -> bool {
        if !self.limits.allow(record.pane, now) {
            return false;
        }
        let mut queue = self.shared.lock();
        if queue.push(record) {
            let _ = writeln!(
                io::stderr(),
                "pinfold-core: warn webhook to {}: the queue is full, so the oldest alert is dropped",
                self.shared.job.hook.host()
            );
            self.shared.give_up();
        }
        if !queue.running {
            let shared = Arc::clone(&self.shared);
            let started = thread::Builder::new()
                .name("pinfold-webhook".to_owned())
                .stack_size(STACK)
                .spawn(move || work(&shared));
            match started {
                Ok(_) => queue.running = true,
                // Kept queued, for the next alert to try again.
                Err(error) => {
                    let _ = writeln!(
                        io::stderr(),
                        "pinfold-core: warn webhook: cannot start its thread: {}",
                        error.kind()
                    );
                }
            }
        }
        self.shared.wake.notify_one();
        true
    }

    /// How many alerts were given up on: failed, or dropped from a full queue.
    #[must_use]
    pub fn failed(&self) -> u32 {
        self.shared.failed.load(Ordering::Relaxed)
    }
}

/// The last [`MOST`] posts' pane and second since `epoch`, oldest overwritten first. Any post
/// within [`PER_PANE`] or [`WINDOW`] is among them, as no more than [`MOST`] fit in a window.
#[derive(Debug)]
struct Limits {
    epoch: Instant,
    posts: [Option<(u32, u32)>; MOST],
    next: usize,
}

impl Limits {
    fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            posts: [None; MOST],
            next: 0,
        }
    }

    fn allow(&mut self, pane: u32, now: Instant) -> bool {
        let second =
            u32::try_from(now.saturating_duration_since(self.epoch).as_secs()).unwrap_or(u32::MAX);
        // Whole seconds, so a span is held for up to a second longer, never shorter.
        let within = |at: u32, span: Duration| u64::from(second - at.min(second)) <= span.as_secs();
        let posts = self.posts.iter().flatten();
        let window = posts.clone().filter(|(_, at)| within(*at, WINDOW)).count();
        let pane_recent = posts
            .clone()
            .any(|(posted, at)| *posted == pane && within(*at, PER_PANE));
        if window >= MOST || pane_recent {
            return false;
        }
        self.posts[self.next] = Some((pane, second));
        self.next = (self.next + 1) % MOST;
        true
    }
}

/// The worker: posts each record until the queue has stayed empty for `idle`, then ends.
fn work(shared: &Shared) {
    loop {
        let record = {
            let mut queue = shared.lock();
            loop {
                if let Some(record) = queue.pop() {
                    break record;
                }
                let (held, waited) = shared
                    .wake
                    .wait_timeout(queue, shared.job.times.idle)
                    .unwrap_or_else(PoisonError::into_inner);
                queue = held;
                if waited.timed_out() && queue.len == 0 {
                    queue.running = false;
                    return;
                }
            }
        };
        deliver(shared, record);
    }
}

/// Posts `record`, trying up to three times, and counts it given up on if none takes. The body
/// and config live only as long as this.
fn deliver(shared: &Shared, record: Record) {
    let job = &shared.job;
    let host = pinfold_sys::hostname();
    let agent = record
        .profile
        .and_then(|index| job.agents.get(usize::from(index)));
    let post = Post {
        status: record.status,
        from_output: record.from_output,
        agent: agent.map(|name| &**name),
        host: host.as_deref(),
        tab: record.tab,
        pane: record.pane,
        at: u64::from(record.at),
        since: u64::from(record.since),
    };
    let (mut body, mut config) = (String::new(), String::new());
    post.body(job.format, &mut body);
    job.hook.curl_config(&body, job.format, &mut config);
    let waits = job.times.retries.map(Some);
    for (tried, wait) in (1..).zip(waits.into_iter().chain([None])) {
        let why = match (run(job, &config), wait) {
            (Try::Done, _) => return,
            (Try::Again(_), Some(wait)) => {
                thread::sleep(wait);
                continue;
            }
            (Try::Again(why) | Try::Stop(why), _) => why,
        };
        // The host, the status and the try; never the URL, whose path can hold a secret.
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: warn webhook to {} gave up: {why}, try {tried}",
            job.hook.host()
        );
        shared.give_up();
        return;
    }
}

/// What one `curl` run came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Try {
    /// A 2xx.
    Done,
    /// A failure worth another try.
    Again(Failed),
    /// One that another try would not change.
    Stop(Failed),
}

/// Why a run failed: what `curl` said, never what it was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failed {
    Http(u16),
    Exit(i32),
    Killed,
    Signal,
    Start(io::ErrorKind),
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(code) => write!(f, "HTTP {code}"),
            Self::Exit(code) => write!(f, "curl exit {code}"),
            Self::Killed => f.write_str("curl still ran after 15 s"),
            Self::Signal => f.write_str("curl ended by a signal"),
            Self::Start(kind) => write!(f, "curl did not start: {kind}"),
        }
    }
}

/// Runs `curl --disable --config -` once with `config` on standard input: no `~/.curlrc`, and
/// the URL never in the process table. Standard error is discarded, at most [`MOST_OUTPUT`]
/// bytes of standard output are read, and a run past `times.kill` is killed.
fn run(job: &Job, config: &str) -> Try {
    let mut command = Command::new(&job.curl);
    command
        .args(["--disable", "--config", "-"])
        .env_clear()
        .envs(
            PROXIES
                .iter()
                .filter_map(|name| Some((name, (job.vars)(name)?))),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    pinfold_sys::mark_strays(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Try::Stop(Failed::Start(error.kind())),
    };
    // A few kilobytes, under any pipe's buffer, so this cannot block on a `curl` that reads
    // nothing; dropping it ends the config.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(config.as_bytes());
    }
    let mut stdout = child.stdout.take().map(pipe::Receiver::from);
    if let Some(stdout) = &stdout {
        let _ = stdout.set_nonblocking(true);
    }
    let (mut output, mut read) = ([0_u8; MOST_OUTPUT], 0);
    let deadline = Instant::now() + job.times.kill;
    let status = loop {
        if let Some(stdout) = stdout.as_mut() {
            read_some(stdout, &mut output, &mut read);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Try::Again(Failed::Killed);
            }
        }
    };
    if let Some(stdout) = stdout.as_mut() {
        read_some(stdout, &mut output, &mut read);
    }
    outcome(status, &output[..read])
}

/// Reads what is ready into the rest of `output`, never past it.
fn read_some(stdout: &mut pipe::Receiver, output: &mut [u8; MOST_OUTPUT], read: &mut usize) {
    while let Some(rest) = output.get_mut(*read..).filter(|rest| !rest.is_empty()) {
        match stdout.read(rest) {
            Ok(0) | Err(_) => return,
            Ok(count) => *read += count,
        }
    }
}

/// A 2xx is done; the retried exits, a 429 and a 5xx go again; anything else stops.
fn outcome(status: ExitStatus, output: &[u8]) -> Try {
    match status.code() {
        Some(0) => {
            let code = std::str::from_utf8(output)
                .ok()
                .and_then(|text| text.trim().parse::<u16>().ok())
                .unwrap_or(0);
            match code {
                200..=299 => Try::Done,
                429 | 500..=599 => Try::Again(Failed::Http(code)),
                _ => Try::Stop(Failed::Http(code)),
            }
        }
        Some(code) if RETRIED_EXITS.contains(&code) => Try::Again(Failed::Exit(code)),
        Some(code) => Try::Stop(Failed::Exit(code)),
        None => Try::Stop(Failed::Signal),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use regex_lite::Regex;

    use super::*;
    use crate::status::Tracker;
    use crate::vt::{AlacrittyEngine, VtEngine};

    const SHORT: Times = Times {
        retries: [Duration::from_millis(50), Duration::from_millis(150)],
        // Long enough for the stub to finish under a loaded test run.
        kill: Duration::from_secs(2),
        idle: Duration::from_millis(100),
    };

    fn record(pane: u32) -> Record {
        Record {
            pane,
            tab: 2,
            at: 1_763_112_000,
            since: 1_763_111_958,
            status: Status::NeedsYou,
            from_output: false,
            profile: Some(0),
        }
    }

    fn vars(name: &str) -> Option<OsString> {
        match name {
            "HTTPS_PROXY" => Some("http://proxy:3128".into()),
            "no_proxy" => Some("localhost".into()),
            // Set for the core, and never passed on.
            "HOME" | "PATH" | "GITHUB_TOKEN" => Some("leak".into()),
            _ => None,
        }
    }

    /// A stub `curl` in a fresh folder. Each run appends its arguments and environment to
    /// `runs`, saves its standard input as `config.N`, then answers with line N of `answers`:
    /// `http CODE`, `exit CODE` or `sleep`. Past the last line it answers `http 200`.
    fn stub(answers: &str) -> (tempfile::TempDir, Sender) {
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(dir.path().join("answers"), answers).unwrap();
        let script = format!(
            "#!/bin/sh\nenv=$(/usr/bin/env | /usr/bin/sort)\ncd '{dir}' || exit 99\n\
             n=$(($(cat count 2>/dev/null || echo 0) + 1))\necho $n > count\n\
             for arg in \"$@\"; do printf '[%s]' \"$arg\"; done >> runs\n\
             printf '\\n%s\\n' \"$env\" >> runs\ncat > config.$n\n\
             echo 'curl: noise on stderr' >&2\n\
             set -- $(sed -n \"${{n}}p\" answers)\ncase \"$1\" in\n\
             exit) exit \"$2\" ;;\nsleep) exec /bin/sleep 30 ;;\n\
             http) printf '%s' \"$2\" ;;\n*) printf 200 ;;\nesac\n",
            dir = dir.path().display()
        );
        // `cp` writes the program, so no fork of this process's other test threads can hold
        // it open for writing, which makes running it fail with ETXTBSY.
        let source = dir.path().join("curl.sh");
        std::fs::write(&source, script).unwrap();
        let copied = Command::new("/bin/cp").arg(&source).arg(&curl).status();
        assert!(copied.unwrap().success());
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hook = Webhook::new("https://ntfy.sh/secret-topic?t=1")
            .unwrap()
            .unwrap();
        let sender = Sender::with_job(Job {
            curl,
            hook,
            format: Format::Json,
            agents: Box::new(["claude".into()]),
            times: SHORT,
            vars,
            waker: None,
        });
        (dir, sender)
    }

    fn runs(dir: &tempfile::TempDir) -> usize {
        std::fs::read_to_string(dir.path().join("count"))
            .ok()
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Waits until the worker has ended, which it does after `idle` with nothing queued.
    fn settled(sender: &Sender) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while sender.shared.lock().running {
            assert!(Instant::now() < deadline, "the worker never ended");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn curl_gets_fixed_arguments_proxies_only_and_the_url_on_standard_input() {
        let (dir, mut sender) = stub("");
        assert!(sender.push(record(7), Instant::now()));
        settled(&sender);
        assert_eq!(runs(&dir), 1);
        assert_eq!(sender.failed(), 0);
        let runs = std::fs::read_to_string(dir.path().join("runs")).unwrap();
        let mut lines = runs.lines();
        assert_eq!(lines.next(), Some("[--disable][--config][-]"));
        let env: Vec<&str> = lines.filter(|line| !line.starts_with("PWD=")).collect();
        // `sh` adds its own `PWD` and, on some systems, `SHLVL` and `_`.
        let env: Vec<&str> = env
            .into_iter()
            .filter(|line| !line.starts_with("SHLVL=") && !line.starts_with("_="))
            .collect();
        assert_eq!(env, ["HTTPS_PROXY=http://proxy:3128", "no_proxy=localhost"]);
        assert!(!runs.contains("secret-topic"), "no URL in the arguments");
        let config = std::fs::read_to_string(dir.path().join("config.1")).unwrap();
        let mut expected = String::new();
        let mut body = String::new();
        let host = pinfold_sys::hostname();
        Post {
            status: Status::NeedsYou,
            from_output: false,
            agent: Some("claude"),
            host: host.as_deref(),
            tab: 2,
            pane: 7,
            at: 1_763_112_000,
            since: 1_763_111_958,
        }
        .body(Format::Json, &mut body);
        sender
            .shared
            .job
            .hook
            .curl_config(&body, Format::Json, &mut expected);
        assert_eq!(config, expected);
        assert!(config.starts_with("url = \"https://ntfy.sh/secret-topic?t=1\"\n"));
    }

    #[test]
    fn a_failure_curl_or_the_server_can_recover_from_is_tried_three_times() {
        for answers in [
            "exit 6", "exit 7", "exit 28", "exit 35", "exit 52", "exit 56", "http 429", "http 500",
            "http 503", "sleep",
        ] {
            let (dir, mut sender) = stub(&format!("{answers}\n{answers}\n{answers}\n"));
            let started = Instant::now();
            sender.push(record(7), started);
            settled(&sender);
            assert_eq!(runs(&dir), 3, "{answers}");
            assert_eq!(sender.failed(), 1, "{answers}: given up after the third");
            let waited = SHORT.retries[0] + SHORT.retries[1];
            assert!(
                started.elapsed() >= waited,
                "{answers}: waited between tries"
            );
        }
        // A recovery on the second or third try is not counted.
        for answers in ["exit 7\n", "http 503\nexit 28\n", "sleep\n"] {
            let (dir, mut sender) = stub(answers);
            sender.push(record(7), Instant::now());
            settled(&sender);
            assert_eq!(runs(&dir), answers.lines().count() + 1, "{answers:?}");
            assert_eq!(sender.failed(), 0, "{answers:?}");
        }
        assert_eq!(
            TIMES.retries,
            [Duration::from_secs(5), Duration::from_secs(30)]
        );
        assert_eq!(TIMES.kill, Duration::from_secs(15));
        assert_eq!(TIMES.idle, Duration::from_secs(60));
    }

    #[test]
    fn anything_else_is_given_up_at_once_and_counted() {
        for answers in [
            "http 400",
            "http 301",
            "http 404",
            "http 000",
            "http garbage",
            "exit 1",
            "exit 3",
            "exit 22",
            "exit 60",
        ] {
            let (dir, mut sender) = stub(answers);
            sender.push(record(7), Instant::now());
            settled(&sender);
            assert_eq!((runs(&dir), sender.failed()), (1, 1), "{answers}");
        }
        for answers in ["http 200", "http 201", "http 299"] {
            let (dir, mut sender) = stub(answers);
            sender.push(record(7), Instant::now());
            settled(&sender);
            assert_eq!((runs(&dir), sender.failed()), (1, 0), "{answers}");
        }
        // Two alerts that fail count two; a `curl` that is gone counts too.
        let (dir, mut sender) = stub("http 404\nhttp 404\n");
        let now = Instant::now();
        sender.push(record(1), now);
        sender.push(record(2), now);
        settled(&sender);
        assert_eq!((runs(&dir), sender.failed()), (2, 2));
        std::fs::remove_file(dir.path().join("curl")).unwrap();
        sender.push(record(3), now);
        settled(&sender);
        assert_eq!(sender.failed(), 3);
    }

    #[test]
    fn a_long_answer_is_read_to_sixteen_bytes_and_not_a_2xx() {
        let output: &[u8] = b"2000000000000000000000000";
        let status = |code: i32| {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(code << 8)
        };
        assert_eq!(
            outcome(status(0), &output[..MOST_OUTPUT]),
            Try::Stop(Failed::Http(0))
        );
        assert_eq!(outcome(status(0), b"204"), Try::Done);
        assert_eq!(outcome(status(7), b""), Try::Again(Failed::Exit(7)));
    }

    #[test]
    fn a_pane_posts_once_a_minute_and_all_ten_times_in_ten_minutes() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut limits = Limits::new(start);
        assert!(limits.allow(1, at(0)));
        assert!(!limits.allow(1, at(30)), "inside the pane's minute");
        assert!(limits.allow(2, at(30)), "another pane, keyed apart");
        assert!(limits.allow(1, at(61)));
        // Seven more panes fill the ten.
        for pane in 3..=9 {
            assert!(limits.allow(pane, at(100)));
        }
        assert!(!limits.allow(10, at(100)), "the eleventh in ten minutes");
        assert!(!limits.allow(10, at(600)));
        assert!(limits.allow(10, at(601)), "the first has left the window");
        // A flood of 64 panes every second for an hour posts at most ten in any ten minutes.
        let mut limits = Limits::new(start);
        let mut posted = Vec::new();
        for second in 0..3600 {
            for pane in 0..64 {
                if limits.allow(pane, at(second)) {
                    posted.push(second);
                }
            }
        }
        for (index, first) in posted.iter().enumerate() {
            let within = posted[index..].iter().take_while(|at| **at - first < 600);
            assert!(within.count() <= MOST);
        }
    }

    #[test]
    fn past_sixteen_records_the_oldest_is_dropped_and_counted() {
        assert_eq!(size_of::<Record>(), 20);
        assert_eq!(size_of::<[Option<Record>; QUEUE]>(), 320);
        let mut queue = Queue::default();
        let dropped = (0..20).filter(|&pane| queue.push(record(pane))).count();
        assert_eq!((dropped, queue.len), (4, QUEUE));
        let left: Vec<u32> = std::iter::from_fn(|| queue.pop()).map(|r| r.pane).collect();
        assert_eq!(
            left,
            (4..20).collect::<Vec<_>>(),
            "oldest first, the first four gone"
        );
        // A sender whose worker is held counts each drop as given up.
        let (_dir, mut sender) = stub("");
        sender.shared.lock().running = true;
        let start = Instant::now();
        for pane in 0..10 {
            assert!(sender.push(record(pane), start));
        }
        sender.limits = Limits::new(start);
        for pane in 10..20 {
            assert!(sender.push(record(pane), start));
        }
        assert_eq!((sender.failed(), sender.shared.lock().len), (4, QUEUE));
    }

    #[test]
    fn the_worker_starts_on_an_alert_and_ends_when_idle() {
        let (dir, mut sender) = stub("");
        assert!(
            !sender.shared.lock().running,
            "no thread before the first alert"
        );
        let now = Instant::now();
        sender.push(record(1), now);
        assert!(sender.shared.lock().running);
        settled(&sender);
        sender.push(record(2), now);
        settled(&sender);
        assert_eq!((runs(&dir), sender.failed()), (2, 0), "started again");
    }

    /// The real `curl`, where there is one, takes the whole config: a closed port is a connect
    /// failure, tried again, not a refused option or protocol.
    #[test]
    fn the_system_curl_reads_the_config_to_a_closed_port() {
        let Some(curl) = find_curl() else {
            return;
        };
        for url in [
            "https://127.0.0.1:9/t",
            "http://127.0.0.1:9/t",
            "http://[::1]:9/t",
        ] {
            let hook = Webhook::new(url).unwrap().unwrap();
            let (_dir, mut sender) = stub("");
            let job = &mut Arc::get_mut(&mut sender.shared).unwrap().job;
            (job.curl, job.hook) = (curl.clone(), hook);
            job.vars = |_| None;
            let mut config = String::new();
            job.hook.curl_config("{}", Format::Json, &mut config);
            assert_eq!(run(job, &config), Try::Again(Failed::Exit(7)), "{url}");
        }
    }

    #[test]
    fn curl_is_found_only_as_an_executable_on_an_absolute_path_entry() {
        let dir = tempfile::tempdir().unwrap();
        let (plain, bin) = (dir.path().join("plain"), dir.path().join("bin"));
        std::fs::create_dir(&plain).unwrap();
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(plain.join("curl"), "").unwrap();
        std::fs::write(bin.join("curl"), "").unwrap();
        std::fs::set_permissions(bin.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = |entries: &[&Path]| std::env::join_paths(entries).unwrap();
        assert_eq!(on_path(None), None, "no curl: webhooks are off");
        assert_eq!(on_path(Some(&path(&[&plain]))), None, "not executable");
        assert_eq!(on_path(Some(&path(&[Path::new("bin")]))), None, "relative");
        assert_eq!(
            on_path(Some(&path(&[&plain, &bin]))),
            Some(bin.join("curl"))
        );
        if cfg!(target_os = "macos") {
            assert_eq!(find_curl(), Some(PathBuf::from("/usr/bin/curl")));
        }
    }

    #[test]
    fn a_hostile_pane_never_reaches_the_posted_body() {
        let secret = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
        let mut engine = AlacrittyEngine::new(4, 20);
        let printed = format!(
            "\x1b]7;file://evil/{secret}\x07\x1b]2;{secret}\x07\x1b]9;{secret}\x07{secret}\r\n\
             url = \"https://evil/\"\n\x1b]777;notify;{secret};body\x1b\\"
        );
        engine.feed(printed.as_bytes(), &mut Vec::new());
        let mut tracker = Tracker::new(Instant::now());
        tracker.read_agent_signals(0);
        tracker.observe_agent(engine.agent_marks(), Instant::now());
        let (dir, mut sender) = stub("");
        let mut hostile = record(7);
        (hostile.status, hostile.from_output) = (tracker.status(), tracker.from_output());
        assert_eq!(
            (hostile.status, hostile.from_output),
            (Status::NeedsYou, true)
        );
        sender.push(hostile, Instant::now());
        settled(&sender);
        let config = std::fs::read_to_string(dir.path().join("config.1")).unwrap();
        assert!(
            !config.contains(secret) && !config.contains("evil"),
            "{config}"
        );
        let data = config.lines().nth(1).unwrap();
        let grammar = Regex::new(
            r#"^data = "\{\\"pinfold\\":1,\\"state\\":\\"needs-you\\",\\"source\\":\\"output\\",\\"agent\\":\\"claude\\",\\"host\\":(null|\\"[A-Za-z0-9.-]{1,64}\\"),\\"tab\\":2,\\"pane\\":7,\\"at\\":1763112000,\\"since\\":1763111958\}"$"#,
        )
        .unwrap();
        assert!(grammar.is_match(data), "{data}");
        assert_eq!(config.lines().count(), 11, "no line added");
    }
}
