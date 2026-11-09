//! Measures real processes against the ADR-0021 budgets.

use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Budgets use MB of 10^6 bytes, the unit of the M0 measurements behind them.
pub const CORE_LIMIT: u64 = 4_000_000;
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
/// The reader's row, a known miss.
pub const READER: &str = "Markdown reader, a 5 MB file";
/// The reader measured 91.32 MB on macOS (#380). This ceiling, 10% over that, keeps it from
/// getting worse; #380's fix deletes it, and the row must then pass `READER_LIMIT`.
const READER_CEILING: u64 = 100_500_000;
/// Rows over budget with an open issue, which pass up to their ceiling and fail above it.
const KNOWN_MISSES: [(&str, u64, &str); 1] = [(READER, READER_CEILING, "#380")];
/// Long enough for a tool to read and lay out what it opened at its cap.
pub const TOOL_SETTLE: Duration = Duration::from_secs(3);
/// How soon a core is back within its budget after a tool pane closes.
pub const CLOSED_SETTLE: Duration = Duration::from_secs(2);

const RUNS: usize = 3;
/// Long enough for a process to finish starting.
pub const SETTLE: Duration = Duration::from_secs(1);
/// A window takes seconds to fall from its startup peak, and budgets are for idle windows.
pub const WINDOW_SETTLE: Duration = Duration::from_secs(7);

/// Parts of the file names of `FALLBACKS` in `pinfold-render`, which keeps that list private.
const CJK_FONTS: [&str; 4] = ["Hiragino", "AppleSDGothicNeo", "NotoSansCJK", "wqy-zenhei"];

/// Read by `pinfold-ui`'s window, and only in a measured one.
const MENU_VARIABLE: &str = "PINFOLD_MEASURE_MENU";

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
    /// Runs this as the shell, and checks that the window opened a CJK font for it.
    pub screen: Option<&'a Path>,
    /// The program prints a window's pid as its line, and this says which process to read. The
    /// two are separate processes and neither count holds the other (ADR-0003).
    pub window: Option<Side>,
    /// Asks a measured window to open its right-click menu.
    pub menu: bool,
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
    let target = match budget.window {
        Some(Side::Window) => window,
        // A process that is gone has no footprint to read.
        Some(Side::Closed) if window.is_none_or(|pid| pinfold_sys::footprint(pid).is_ok()) => None,
        _ => Some(child.id()),
    };
    thread::sleep(budget.settle);
    let exited = child.try_wait()?.is_some();
    let measured = target
        .filter(|_| !exited)
        .ok_or_else(|| {
            io::Error::other("exited before it could be measured, or a closed tool ran on")
        })
        .and_then(|pid| Ok((pinfold_sys::footprint(pid)?, pid)));
    let no_font = match &measured {
        Ok((_, pid)) if budget.screen.is_some() => !has_cjk_font(*pid)?,
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
        let miss = KNOWN_MISSES
            .into_iter()
            .find(|miss| miss.0 == budget.metric);
        match measure(budget) {
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
            Ok((bytes, _)) if miss.is_some_and(|miss| bytes <= miss.1) => {
                let (_, ceiling, issue) = miss.unwrap_or_default();
                println!(
                    "| {} | {:.2} | {limit:.2} | **missed**, see {issue}; fails over {:.2} |",
                    budget.metric,
                    megabytes(bytes),
                    megabytes(ceiling)
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
        }
    }

    #[test]
    fn a_real_process_under_budget_passes() {
        assert!(run(&[sleep(u64::MAX, &["30"])]));
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
    fn a_known_miss_passes_up_to_its_ceiling() {
        let miss = Budget {
            metric: READER,
            ..sleep(1, &["30"])
        };
        assert!(run(&[miss]));
        assert!(!run(&[sleep(1, &["30"])]));
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
