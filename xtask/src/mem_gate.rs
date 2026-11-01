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
pub const WINDOW_LIMIT_HALF_RETINA: u64 = 30_000_000 + 2 * 4 * 1512 * 1890;
/// 10k lines of typical text, on top of an idle pane of the same size.
pub const TYPICAL_TEXT_LIMIT: u64 = if cfg!(target_os = "macos") {
    1_500_000
} else {
    1_000_000
};
/// The cap on a pane's scrollback, with output that fills it.
pub const SCROLLBACK_CAP: u64 = 2_000_000;

const RUNS: usize = 3;
/// Long enough for a process to finish starting.
pub const SETTLE: Duration = Duration::from_secs(1);
/// A window takes seconds to fall from its startup peak, and budgets are for idle windows.
pub const WINDOW_SETTLE: Duration = Duration::from_secs(7);

/// ADR-0021 budgets with nothing to measure yet, and the issue that makes them measurable.
const NOT_MEASURED: [(&str, &str); 1] = [("Window on Wayland", "a compositor in CI")];

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
}

fn measure_once(budget: &Budget, args: &[&str]) -> io::Result<u64> {
    let stdout = if budget.wait_for_line {
        Stdio::piped()
    } else {
        Stdio::null()
    };
    let mut child = Command::new(budget.program)
        .args(args)
        .stdout(stdout)
        .spawn()?;
    // A process that dies closes the pipe, so this read cannot outlive it.
    let piped = child.stdout.take();
    if let Some(stdout) = piped {
        BufReader::new(stdout).read_line(&mut String::new())?;
    }
    thread::sleep(budget.settle);
    let exited = child.try_wait()?.is_some();
    let bytes = if exited {
        Err(io::Error::other("exited before it could be measured"))
    } else {
        pinfold_sys::footprint(child.id())
    };
    let _ = child.kill();
    let _ = child.wait();
    match bytes {
        Ok(0) => Err(io::Error::other("measured zero bytes")),
        other => other,
    }
}

fn median(budget: &Budget, args: &[&str]) -> io::Result<u64> {
    let mut samples = (0..RUNS)
        .map(|_| measure_once(budget, args))
        .collect::<io::Result<Vec<u64>>>()?;
    samples.sort_unstable();
    Ok(samples[RUNS / 2])
}

fn measure(budget: &Budget) -> io::Result<u64> {
    let bytes = median(budget, budget.args)?;
    match budget.per_pane_over {
        None => Ok(bytes),
        Some((baseline, panes)) => {
            let growth = bytes.saturating_sub(median(budget, baseline)?);
            Ok(growth / panes)
        }
    }
}

fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

/// Prints the table. Returns false if a budget is exceeded or cannot be measured.
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
            Ok(bytes) if bytes <= budget.limit => {
                println!(
                    "| {} | {:.2} | {limit:.2} | pass |",
                    budget.metric,
                    megabytes(bytes)
                );
            }
            Ok(bytes) => {
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
    fn a_missing_program_fails() {
        let budget = Budget {
            metric: "missing",
            limit: u64::MAX,
            program: Path::new("does/not/exist"),
            args: &[],
            per_pane_over: None,
            settle: SETTLE,
            wait_for_line: false,
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
        assert!(measure(&budget).unwrap() < 1_000_000);
    }
}
