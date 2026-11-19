//! Measures real processes against the ADR-0021 budgets.

use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Budgets use MB of 10^6 bytes, the unit of the M0 measurements behind them.
pub const CORE_LIMIT: u64 = 4_000_000;
/// A ceiling on the core after 64 agent launches, which keeps about 1 MB more with or without
/// checkpoints (#476). Not a core allowance: it returns to [`CORE_LIMIT`] once #476 is fixed.
pub const LAUNCH_LIMIT: u64 = 5_000_000;
/// A row that is shown with no budget, never failed.
pub const INFORMATION: u64 = u64::MAX;
pub const PANE_LIMIT: u64 = 250_000;
/// The window budget on Linux for an inner size of 1920x1080.
pub const WINDOW_LIMIT_FULL_HD: u64 = 8_000_000 + 2 * 4 * 1920 * 1080;
/// The window budget on macOS for half a Retina screen, the size M0 measured.
pub const WINDOW_LIMIT_HALF_RETINA: u64 = 33_000_000 + 2 * 4 * 1512 * 1890;
/// 10k lines of typical text, on top of an idle pane of the same size.
pub const TYPICAL_TEXT_LIMIT: u64 = if cfg!(target_os = "macos") {
    1_500_000
} else {
    1_000_000
};
/// The cap on a pane's scrollback, with output that fills it.
pub const SCROLLBACK_CAP: u64 = 2_000_000;
/// ADR-0045 section 5: a tool process with nothing open.
pub const TOOL_LIMIT: u64 = 3_000_000;
/// The markdown reader's, file manager's and diff viewer's caps, which the gate fills.
pub const MARKDOWN_CAP: u64 = 5_000_000;
pub const ENTRY_CAP: u64 = 10_000;
pub const GIT_OUTPUT_CAP: u64 = 8_000_000;
/// ADR-0045 section 5: 3 bytes for each byte of the file or of git's output, 512 per entry.
pub const READER_LIMIT: u64 = TOOL_LIMIT + 3 * MARKDOWN_CAP;
pub const FILES_LIMIT: u64 = TOOL_LIMIT + 512 * ENTRY_CAP;
pub const DIFF_LIMIT: u64 = TOOL_LIMIT + 3 * GIT_OUTPUT_CAP;
/// ADR-0050: a `pinfold checkpoint take` process, its reader threads included, at any tree size.
pub const TAKE_LIMIT: u64 = 4_000_000;
/// ADR-0051 §11: a `pinfold agents peek` process, whatever the size of the session it reads.
pub const PEEK_LIMIT: u64 = 4_000_000;
/// The session file the peek row reads.
pub const PEEK_SESSION: u64 = 64 * 1024 * 1024;
/// Files in the generated tree the take rows checkpoint.
pub const TAKE_FILES: usize = 100_000;
/// Long enough for a tool to read and lay out what it opened at its cap.
pub const TOOL_SETTLE: Duration = Duration::from_secs(3);
/// How soon a core is back within its budget after a tool pane closes.
pub const CLOSED_SETTLE: Duration = Duration::from_secs(2);

const RUNS: usize = 3;
/// A webhook's worker ends a minute after its last post, and one alert to a closed port takes
/// three tries over 35 s (ADR-0051).
pub const WEBHOOK_SETTLE: Duration = Duration::from_secs(105);
/// 64 launches, each with its launch, exit and API take and its prune, and 64 lists, two at a
/// time, then the workers end (ADR-0050).
pub const REWIND_SETTLE: Duration = Duration::from_secs(90);
/// 8 launches, each printing 1,100 commands (ADR-0052).
pub const TIMELINE_SETTLE: Duration = Duration::from_secs(5);
/// The same, then 8,000 `pinfold ctl list` calls, which take about 12 s on an M1, or 100
/// `pinfold ctl timeline` reads of 1,000 entries (ADR-0052).
pub const POLLING_SETTLE: Duration = Duration::from_secs(40);
/// Long enough for a process to finish starting.
pub const SETTLE: Duration = Duration::from_secs(1);
/// A window takes seconds to fall from its startup peak, and budgets are for idle windows.
pub const WINDOW_SETTLE: Duration = Duration::from_secs(7);

/// Parts of the file names of `FALLBACKS` in `pinfold-render`, which keeps that list private.
const CJK_FONTS: [&str; 4] = ["Hiragino", "AppleSDGothicNeo", "NotoSansCJK", "wqy-zenhei"];

/// Read by `pinfold-ui`'s window, and only in a measured one.
const MENU_VARIABLE: &str = "PINFOLD_MEASURE_MENU";
/// Likewise: `open` keeps the inbox open, `closed` closes it once it has drawn its rows, and
/// `answer` shows PinFold's confirmation of an Interrupt over it (ADR-0051 §6).
const INBOX_VARIABLE: &str = "PINFOLD_MEASURE_INBOX";
/// Likewise: shows a `Rewind` prompt at its largest, 10 paths of 256 bytes, over the strip's
/// `no checkpoint` (ADR-0050).
const REWIND_VARIABLE: &str = "PINFOLD_MEASURE_REWIND";

/// ADR-0021 budgets with nothing to measure yet, and the issue that makes them measurable.
const NOT_MEASURED: [(&str, &str); 2] = [
    ("Window on Wayland", "a compositor in CI"),
    (
        "Window with the Dock menu open",
        "a way to right-click the Dock from a script",
    ),
];

/// Which process of a core that prints a pid to read.
#[derive(Clone, Copy, PartialEq)]
pub enum Side {
    Core,
    /// The printed pid's: a window, or a tool.
    Window,
    /// The core, once the printed pid, a closed tool's, has gone.
    Closed,
}

pub struct Budget<'a> {
    pub metric: &'a str,
    pub limit: u64,
    pub program: &'a Path,
    pub args: &'a [&'a str],
    /// Measures the growth per pane instead: `args`, minus these arguments, over this many panes.
    pub per_pane_over: Option<(&'a [&'a str], u64)>,
    pub settle: Duration,
    /// Starts the settle time when the process prints a line, not when it starts.
    pub wait_for_line: bool,
    /// Runs this as the shell, and checks that the window, if measured, opened a CJK font for it.
    pub screen: Option<&'a Path>,
    /// The program prints a window's pid as its line, and this says which process to read. The
    /// two are separate processes and neither count holds the other (ADR-0003).
    pub window: Option<Side>,
    /// Asks a measured window to open its right-click menu.
    pub menu: bool,
    /// Asks a measured window to show a `Rewind` prompt at its largest.
    pub rewind: bool,
    /// Asks a measured window to open its inbox, with `closed` to close it again, and with
    /// `answer` to show an answer prompt over it.
    pub inbox: Option<&'a str>,
    /// Runs the core with this folder as its config home and its `agent.sh` as every pane's
    /// shell.
    pub agents: Option<&'a Path>,
}

/// Bytes, and whether a screen's CJK text went undrawn for want of a font.
type Reading = (u64, bool);

/// Fallback fonts open on first use and stay mapped, so a mapping proves one was drawn from.
fn has_cjk_font(pid: u32) -> io::Result<bool> {
    let maps = if cfg!(target_os = "linux") {
        std::fs::read_to_string(format!("/proc/{pid}/maps"))?
    } else {
        let output = Command::new("lsof")
            .args(["-Fn", "-p", &pid.to_string()])
            .output()?;
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    Ok(CJK_FONTS.iter().any(|name| maps.contains(name)))
}

fn measure_once(budget: &Budget, args: &[&str]) -> io::Result<Reading> {
    let stdout = if budget.wait_for_line {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut command = Command::new(budget.program);
    command.args(args).stdout(stdout);
    if let Some(screen) = budget.screen {
        command.env("SHELL", screen);
    }
    if budget.menu {
        command.env(MENU_VARIABLE, "1");
    }
    if budget.rewind {
        command.env(REWIND_VARIABLE, "1");
    }
    if let Some(agents) = budget.agents {
        command.env("XDG_CONFIG_HOME", agents);
        command.env("SHELL", agents.join("agent.sh"));
        // Checkpoints go under the row's folder, never the person's state directory.
        if agents.join("state").is_dir() {
            command.env("HOME", agents.join("state"));
            command.env("XDG_STATE_HOME", agents.join("state"));
        }
        // A folder holding agents' session files is their home too (ADR-0051 §7).
        if agents.join(".claude").is_dir() {
            command.env("HOME", agents);
        }
    }
    if let Some(inbox) = budget.inbox {
        command.env(INBOX_VARIABLE, inbox);
    }
    // A core takes its session's socket, which a core the user is running holds.
    let runtime = std::env::temp_dir().join(format!("pinfold-gate-{}", std::process::id()));
    std::fs::create_dir_all(&runtime)?;
    command.env("XDG_RUNTIME_DIR", runtime);
    let mut child = command.spawn()?;
    // A process that dies closes the pipe, so this read cannot outlive it.
    let piped = child.stdout.take();
    let mut line = String::new();
    if let Some(stdout) = piped {
        BufReader::new(stdout).read_line(&mut line)?;
    }
    let window = budget.window.and_then(|_| line.trim().parse::<u32>().ok());
    let runs = |pid| pinfold_sys::footprint(pid).is_ok_and(|bytes| bytes > 0);
    let target = match budget.window {
        // A core says the pid once its window says hello, so without one nothing is linked.
        Some(_) if window.is_none() => None,
        Some(Side::Window) => window,
        // A process that is gone has no footprint to read.
        Some(Side::Closed) if window.is_some_and(runs) => None,
        _ => Some(child.id()),
    };
    thread::sleep(budget.settle);
    let exited = child.try_wait()?.is_some();
    // A core whose window or tool has gone is an idle core, not the one the row names.
    let beside = budget.window.is_some_and(|side| side != Side::Closed);
    let alone = beside && !window.is_some_and(runs);
    let measured = target
        .filter(|_| !exited && !alone)
        .ok_or_else(|| {
            io::Error::other(if beside {
                "the window or tool beside the core never started or exited early, so nothing was measured"
            } else {
                "exited before it could be measured, or a closed tool ran on"
            })
        })
        .and_then(|pid| Ok((pinfold_sys::footprint(pid)?, pid)));
    let no_font = match &measured {
        // The window draws the screen; a core beside it opens no font.
        Ok((_, pid)) if budget.screen.is_some() && budget.window != Some(Side::Core) => {
            !has_cjk_font(*pid)?
        }
        _ => false,
    };
    let _ = child.kill();
    let _ = child.wait();
    // Nothing tells a window that its core was killed to exit, as it stays to say so. A closed
    // tool's pid may be another process's by now.
    if let Some(pid) = window.filter(|_| budget.window != Some(Side::Closed)) {
        // The shell's builtin, as a slim image has no `kill` program.
        let kill = format!("kill {pid}");
        Command::new("/bin/sh").args(["-c", &kill]).status()?;
    }
    let bytes = measured.map(|(bytes, _)| bytes);
    match bytes {
        Ok(0) => Err(io::Error::other("measured zero bytes")),
        other => Ok((other?, no_font)),
    }
}

fn median(budget: &Budget, args: &[&str]) -> io::Result<Reading> {
    let mut samples = (0..RUNS)
        .map(|_| measure_once(budget, args))
        .collect::<io::Result<Vec<Reading>>>()?;
    let no_font = samples.iter().any(|sample| sample.1);
    samples.sort_unstable();
    Ok((samples[RUNS / 2].0, no_font))
}

fn measure(budget: &Budget) -> io::Result<Reading> {
    let (bytes, no_font) = median(budget, budget.args)?;
    match budget.per_pane_over {
        None => Ok((bytes, no_font)),
        Some((baseline, panes)) => {
            let growth = bytes.saturating_sub(median(budget, baseline)?.0);
            Ok((growth / panes, no_font))
        }
    }
}

fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

/// Prints the table. Returns false if a budget is exceeded or cannot be measured. A screen with
/// no CJK font is under-measured, not failed, as hosted CI may have none.
pub fn run(budgets: &[Budget]) -> bool {
    println!(
        "Median of {RUNS} runs in MB (10^6 bytes), on {}.\n",
        std::env::consts::OS
    );
    println!("| Metric | Measured | Budget | Result |");
    println!("| --- | --- | --- | --- |");
    let mut passed = true;
    for budget in budgets {
        let limit = megabytes(budget.limit);
        match measure(budget) {
            Ok((bytes, _)) if budget.limit == INFORMATION => {
                println!(
                    "| {} | {:.2} | | information only |",
                    budget.metric,
                    megabytes(bytes)
                );
            }
            Ok((bytes, true)) if bytes <= budget.limit => {
                println!(
                    "| {} | {:.2} | {limit:.2} | no CJK font, so this leaves CJK out |",
                    budget.metric,
                    megabytes(bytes)
                );
            }
            Ok((bytes, false)) if bytes <= budget.limit => {
                println!(
                    "| {} | {:.2} | {limit:.2} | pass |",
                    budget.metric,
                    megabytes(bytes)
                );
            }
            Ok((bytes, _)) => {
                passed = false;
                println!(
                    "| {} | {:.2} | {limit:.2} | **FAIL** |",
                    budget.metric,
                    megabytes(bytes)
                );
            }
            Err(error) => {
                passed = false;
                println!(
                    "| {} | error: {error} | {limit:.2} | **FAIL** |",
                    budget.metric
                );
            }
        }
    }
    for (metric, issue) in NOT_MEASURED {
        println!("| {metric} | not measured | | waits for {issue} |");
    }
    passed
}

/// Runs the next checkpoint of `tree` into a store under `state` and returns its peak, in each
/// platform's metric of the other rows. On macOS that is the take's own lifetime peak
/// `phys_footprint`, which it prints as `peak` with `PINFOLD_MEASURE` set. Linux keeps no peak
/// PSS, and the take's `VmHWM` counts shared libraries whole, so its PSS is sampled every
/// millisecond until it exits. Git's processes are not PinFold's and are not counted
/// (ADR-0050 decision 9).
fn take_peak(program: &Path, tree: &Path, state: &Path) -> io::Result<u64> {
    let take = ["checkpoint", "take", "r1-0", "api", "--"];
    let whole = |line: &serde_json::Value| {
        line["files"].as_u64() == Some(TAKE_FILES as u64) && line["complete"] == true
    };
    checkpoint_peak(program, &take, tree, state, whole)
        .map_err(|error| io::Error::other(format!("the take {error}")))
}

/// Runs `pinfold <args> <tree>` with `state` and returns its peak as [`take_peak`] does, once
/// `done` accepts its JSON line.
fn checkpoint_peak(
    program: &Path,
    args: &[&str],
    tree: &Path,
    state: &Path,
    done: impl Fn(&serde_json::Value) -> bool,
) -> io::Result<u64> {
    let mut child = Command::new(program)
        .args(args)
        .arg(tree)
        .env("HOME", state)
        .env("XDG_STATE_HOME", state)
        .env("PINFOLD_MEASURE", "1")
        .stdout(Stdio::piped())
        .spawn()?;
    let mut sampled = 0;
    while child.try_wait()?.is_none() {
        sampled = sampled.max(pinfold_sys::footprint(child.id()).unwrap_or(0));
        thread::sleep(Duration::from_millis(1));
    }
    let mut out = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        io::Read::read_to_end(&mut stdout, &mut out)?;
    }
    // A peek prints a line per directory, then the measured one.
    let last = out.trim_ascii_end().rsplit(|&byte| byte == b'\n').next();
    let line: serde_json::Value = serde_json::from_slice(last.unwrap_or_default())
        .map_err(|_| io::Error::other("printed no JSON line"))?;
    if let Some(error) = line["error"].as_str() {
        return Err(io::Error::other(format!("failed: {error}")));
    }
    if !done(&line) {
        return Err(io::Error::other(format!("did not do it all: {line}")));
    }
    // A peek may exit before the first sample, so on Linux it also gives its PSS as it ends (#505).
    let peak = if cfg!(target_os = "linux") {
        Some(sampled.max(line["now"].as_u64().unwrap_or(0)))
    } else {
        line["peak"].as_u64()
    };
    peak.filter(|&peak| peak > 0)
        .ok_or_else(|| io::Error::other("no peak was read"))
}

/// Median of [`RUNS`] of a first take of `tree`, then a take with every 100th file changed,
/// then the restore of the first over the second, its `restore` take of 100,000 files included,
/// which writes the 1,000 back.
fn take_peaks(program: &Path, tree: &Path, state: &Path) -> io::Result<[u64; 3]> {
    let (mut first, mut changed, mut restored) = (Vec::new(), Vec::new(), Vec::new());
    let restore = ["checkpoint", "restore", "r1-0", "0", "1", "--"];
    let all_back = |line: &serde_json::Value| line["restored"] == 1000 && line["skipped"] == 0;
    for run in 0..RUNS {
        let _ = std::fs::remove_dir_all(state);
        std::fs::create_dir_all(state)?;
        first.push(take_peak(program, tree, state)?);
        for file in (0..TAKE_FILES).step_by(100) {
            let path = tree.join(format!("{:03}/{file:06}.txt", file / 1000));
            std::fs::write(path, format!("changed in run {run}\n"))?;
        }
        changed.push(take_peak(program, tree, state)?);
        let peak = checkpoint_peak(program, &restore, tree, state, all_back);
        restored.push(peak.map_err(|error| io::Error::other(format!("the restore {error}")))?);
    }
    let _ = std::fs::remove_dir_all(state);
    for peaks in [&mut first, &mut changed, &mut restored] {
        peaks.sort_unstable();
    }
    Ok([first[RUNS / 2], changed[RUNS / 2], restored[RUNS / 2]])
}

/// Prints the take rows under [`run`]'s table, with `program` as `pinfold`. Returns false if a
/// row is over budget or cannot be measured.
pub fn run_takes(program: &Path, tree: &Path, state: &Path) -> bool {
    let metrics = [
        "Checkpoint take with its readers, 100,000 files",
        "Checkpoint take with its readers, 100,000 files, 1% changed",
        "Checkpoint restore of that 1%, its restore take included",
    ];
    let limit = megabytes(TAKE_LIMIT);
    match take_peaks(program, tree, state) {
        Ok(peaks) => {
            for (metric, bytes) in metrics.iter().zip(peaks) {
                let result = if bytes <= TAKE_LIMIT {
                    "pass"
                } else {
                    "**FAIL**"
                };
                println!(
                    "| {metric} | {:.2} | {limit:.2} | {result} |",
                    megabytes(bytes)
                );
            }
            peaks.iter().all(|&bytes| bytes <= TAKE_LIMIT)
        }
        Err(error) => {
            for metric in metrics {
                println!("| {metric} | error: {error} | {limit:.2} | **FAIL** |");
            }
            false
        }
    }
}

/// ADR-0051 §11: `pinfold agents peek` over a 64 MiB Claude Code session, the median of
/// [`RUNS`] peaks, against [`PEEK_LIMIT`]. `home` holds the session for `dir`. On Linux each is
/// the most of the PSS samples and the PSS the peek gives as it ends, as a peek can exit before
/// the first sample. Prints its row under [`run`]'s table and returns false if it is over or
/// cannot be measured.
pub fn run_peek(program: &Path, home: &Path, dir: &Path) -> bool {
    let metric = "`pinfold agents peek`, a 64 MiB session file";
    let limit = megabytes(PEEK_LIMIT);
    let mut peaks = Vec::new();
    for _ in 0..RUNS {
        let peak = checkpoint_peak(program, &["agents", "peek", "--"], dir, home, |_| true);
        match peak {
            Ok(peak) => peaks.push(peak),
            Err(error) => {
                println!("| {metric} | error: the peek {error} | {limit:.2} | **FAIL** |");
                return false;
            }
        }
    }
    peaks.sort_unstable();
    let bytes = peaks[RUNS / 2];
    let result = if bytes <= PEEK_LIMIT {
        "pass"
    } else {
        "**FAIL**"
    };
    println!(
        "| {metric} | {:.2} | {limit:.2} | {result} |",
        megabytes(bytes)
    );
    bytes <= PEEK_LIMIT
}

/// Waits for `path`, which a pane's script makes, while `core` runs.
fn wait_for(path: &Path, core: &mut std::process::Child) -> io::Result<()> {
    for _ in 0..1200 {
        if path.exists() {
            return Ok(());
        }
        if core.try_wait()?.is_some() {
            return Err(io::Error::other("the core exited during the calls"));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::other(format!(
        "{} never appeared",
        path.display()
    )))
}

/// One core over `agents`, whose script starts round `n` of 1,000 calls at `go<n>` and makes
/// `done<n>` at its end: the core idle, after round 1 and after round 2.
fn api_calls_once(core: &Path, agents: &Path) -> io::Result<[u64; 3]> {
    for name in ["lock", "go1", "done1", "go2", "done2"] {
        let _ = std::fs::remove_dir(agents.join(name));
        let _ = std::fs::remove_file(agents.join(name));
    }
    let runtime = std::env::temp_dir().join(format!("pinfold-gate-{}", std::process::id()));
    std::fs::create_dir_all(&runtime)?;
    let mut child = Command::new(core)
        .args(["--idle", "8"])
        .env("XDG_CONFIG_HOME", agents)
        .env("SHELL", agents.join("agent.sh"))
        .env("XDG_RUNTIME_DIR", runtime)
        .stdout(Stdio::null())
        .spawn()?;
    let mut readings = [0; 3];
    let mut measured = || -> io::Result<()> {
        thread::sleep(TOOL_SETTLE);
        readings[0] = pinfold_sys::footprint(child.id())?;
        for (round, reading) in readings.iter_mut().enumerate().skip(1) {
            std::fs::write(agents.join(format!("go{round}")), "")?;
            wait_for(&agents.join(format!("done{round}")), &mut child)?;
            // Linux trims the heap a second after the burst (#492).
            thread::sleep(TOOL_SETTLE);
            *reading = pinfold_sys::footprint(child.id())?;
        }
        Ok(())
    };
    let result = measured();
    let _ = child.kill();
    let _ = child.wait();
    result.map(|()| readings)
}

/// #502: the core with 8 idle panes, then after one of them makes 1,000 `pinfold ctl list`
/// calls, and after 1,000 more, which shows whether the calls leave a high-water mark or grow
/// with each call. The median of [`RUNS`] for each; only the first 1,000 are gated.
pub fn run_api_calls(core: &Path, agents: &Path) -> bool {
    let metrics = [
        ("Core, 8 idle panes, before the API calls", INFORMATION),
        ("Core after 1,000 API calls", CORE_LIMIT),
        ("Core after another 1,000 API calls", INFORMATION),
    ];
    let mut runs = Vec::new();
    for _ in 0..RUNS {
        match api_calls_once(core, agents) {
            Ok(readings) => runs.push(readings),
            Err(error) => {
                for (metric, _) in metrics {
                    println!("| {metric} | error: {error} | | **FAIL** |");
                }
                return false;
            }
        }
    }
    let mut passed = true;
    for (column, (metric, limit)) in metrics.into_iter().enumerate() {
        let mut readings: Vec<u64> = runs.iter().map(|run| run[column]).collect();
        readings.sort_unstable();
        let bytes = readings[RUNS / 2];
        let mb = megabytes(bytes);
        if limit == INFORMATION {
            println!("| {metric} | {mb:.2} | | information only |");
        } else {
            let result = if bytes <= limit { "pass" } else { "**FAIL**" };
            println!(
                "| {metric} | {mb:.2} | {:.2} | {result} |",
                megabytes(limit)
            );
            passed &= bytes <= limit;
        }
    }
    passed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleep(limit: u64, seconds: &'static [&'static str]) -> Budget<'static> {
        Budget {
            metric: "sleep",
            limit,
            program: Path::new("sleep"),
            args: seconds,
            per_pane_over: None,
            settle: SETTLE,
            wait_for_line: false,
            screen: None,
            window: None,
            menu: false,
            rewind: false,
            inbox: None,
            agents: None,
        }
    }

    #[test]
    fn a_real_process_under_budget_passes() {
        assert!(run(&[sleep(u64::MAX - 1, &["30"])]));
    }

    #[test]
    fn a_real_process_over_budget_fails() {
        assert!(!run(&[sleep(1, &["30"])]));
    }

    #[test]
    fn a_process_that_exits_early_fails() {
        assert!(!run(&[sleep(u64::MAX, &["0"])]));
    }

    #[test]
    fn the_tool_caps_match_the_tools() {
        assert_eq!(
            (MARKDOWN_CAP, ENTRY_CAP, GIT_OUTPUT_CAP),
            (5_000_000, 10_000, 8_000_000)
        );
        let md = include_str!("../../crates/pinfold-tools/src/md.rs");
        assert!(md.contains("MAX_BYTES: usize = 5_000_000;"));
        let files = include_str!("../../crates/pinfold-tools/src/files.rs");
        assert!(files.contains("MAX_ENTRIES: usize = 10_000;"));
        let git = include_str!("../../crates/pinfold-tools/src/git.rs");
        assert!(git.contains("MAX_OUTPUT: usize = 8_000_000;"));
    }

    #[test]
    fn a_missing_program_fails() {
        let budget = Budget {
            metric: "missing",
            limit: u64::MAX,
            program: Path::new("does/not/exist"),
            args: &[],
            per_pane_over: None,
            settle: SETTLE,
            wait_for_line: false,
            screen: None,
            window: None,
            menu: false,
            rewind: false,
            inbox: None,
            agents: None,
        };
        assert!(!run(&[budget]));
    }

    #[test]
    fn a_printed_pid_is_measured_and_killed_with_the_core() {
        // Its footprint is far from the shell's, so the reading shows which was measured.
        const SCRIPT: [&str; 2] = [
            "-c",
            "/bin/dd if=/dev/zero of=/dev/null bs=67108864 & echo $!; exec sleep 30",
        ];
        let budget = |side| Budget {
            program: Path::new("/bin/sh"),
            args: &SCRIPT,
            wait_for_line: true,
            window: Some(side),
            ..sleep(u64::MAX, &[])
        };
        let shell = measure(&budget(Side::Core)).unwrap().0;
        let window = measure(&budget(Side::Window)).unwrap().0;
        assert!(window > shell + 32_000_000, "{window} and {shell}");
    }

    #[test]
    fn a_window_that_never_says_its_pid_fails() {
        let budget = Budget {
            program: Path::new("/bin/sh"),
            args: &["-c", "echo no-pid; exec sleep 30"],
            wait_for_line: true,
            window: Some(Side::Window),
            ..sleep(u64::MAX, &[])
        };
        assert!(!run(&[budget]));
    }

    #[test]
    fn a_core_whose_window_exits_early_fails() {
        // The printed pid is gone by the reading, as a window that crashes after its hello is.
        let budget = Budget {
            program: Path::new("/bin/sh"),
            args: &["-c", "/bin/sleep 0 & echo $!; wait; exec sleep 30"],
            wait_for_line: true,
            window: Some(Side::Core),
            ..sleep(u64::MAX, &[])
        };
        let error = measure_once(&budget, budget.args).unwrap_err();
        assert!(error.to_string().contains("exited early"), "{error}");
        assert!(!run(&[budget]));
    }

    #[test]
    fn the_settle_time_can_start_from_a_line_of_output() {
        let budget = Budget {
            program: Path::new("/bin/sh"),
            args: &["-c", "sleep 2; echo ready; exec sleep 30"],
            wait_for_line: true,
            ..sleep(u64::MAX, &[])
        };
        let start = std::time::Instant::now();
        measure_once(&budget, budget.args).unwrap();
        assert!(start.elapsed() >= SETTLE + Duration::from_secs(2));
    }

    #[test]
    fn a_process_that_dies_before_its_line_fails() {
        let budget = Budget {
            program: Path::new("/bin/sh"),
            args: &["-c", "exit 0"],
            wait_for_line: true,
            ..sleep(u64::MAX, &[])
        };
        assert!(!run(&[budget]));
    }

    #[test]
    fn per_pane_growth_is_divided_by_the_panes() {
        let budget = Budget {
            per_pane_over: Some((&["30"], 2)),
            ..sleep(u64::MAX, &["30"])
        };
        assert!(measure(&budget).unwrap().0 < 1_000_000);
    }

    #[test]
    fn a_screen_with_no_cjk_font_says_so_without_failing() {
        let budget = Budget {
            screen: Some(Path::new("/bin/sh")),
            ..sleep(u64::MAX, &["30"])
        };
        assert!(measure(&budget).unwrap().1);
        assert!(run(&[budget]));
    }

    #[test]
    fn a_screen_with_no_cjk_font_still_fails_over_budget() {
        let budget = Budget {
            screen: Some(Path::new("/bin/sh")),
            ..sleep(1, &["30"])
        };
        assert!(!run(&[budget]));
    }

    #[test]
    fn the_menu_variable_matches_the_window() {
        let window = include_str!("../../crates/pinfold-ui/src/window.rs");
        assert!(window.contains(&format!("var_os(\"{MENU_VARIABLE}\")")));
        assert!(window.contains(&format!("var_os(\"{INBOX_VARIABLE}\")")));
        assert!(window.contains(&format!("var_os(\"{REWIND_VARIABLE}\")")));
    }

    #[test]
    fn the_cjk_font_names_match_the_renderer() {
        let fonts = include_str!("../../crates/pinfold-render/src/font.rs");
        assert!(CJK_FONTS.iter().all(|name| fonts.contains(name)));
    }

    #[test]
    fn the_heavy_screen_text_holds_more_glyphs_than_a_full_cache() {
        let screen = include_str!("../../tests/window/heavy-screen.txt");
        let text = ["\x1b[0m", "\x1b[1m", "\x1b[3m"]
            .iter()
            .fold(screen.to_owned(), |text, sgr| text.replace(sgr, ""));
        let glyphs: std::collections::HashSet<char> =
            text.chars().filter(|c| !c.is_whitespace()).collect();
        // Metal's atlas holds about 2,000 ASCII-sized glyphs (`metal.rs`).
        assert!(glyphs.len() > 2_000, "{} glyphs", glyphs.len());
        // full-cache.sh sizes its pages from this width, so that wrapped lines never scroll.
        let script = include_str!("../../tests/window/full-cache.sh");
        assert!(script.contains("(384 / columns + 2)"));
        for line in text.lines() {
            let width: usize = line.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
            assert_eq!(width, 384);
        }
    }

    #[test]
    fn the_heavy_screen_is_wider_and_taller_than_any_window_in_the_gate() {
        let screen = include_str!("../../tests/window/heavy-screen.txt");
        assert_eq!(screen.lines().count(), 120);
        for line in screen.lines() {
            let text = ["\x1b[0m", "\x1b[1m", "\x1b[3m"]
                .iter()
                .fold(line.to_owned(), |text, sgr| text.replace(sgr, ""));
            let width: usize = text.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
            assert!(width >= 300, "{width} columns");
        }
    }
}
