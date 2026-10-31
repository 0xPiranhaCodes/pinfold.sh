//! Measures real processes against the ADR-0021 budgets.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Budgets use MB of 10^6 bytes, the unit of the M0 measurements behind them.
pub const CORE_LIMIT: u64 = 4_000_000;

const RUNS: usize = 3;
const SETTLE: Duration = Duration::from_secs(1);

/// ADR-0021 budgets with nothing to measure yet, and the issue that makes them measurable.
const NOT_MEASURED: [(&str, &str); 6] = [
    ("Core, 8 idle panes", "#18"),
    ("Each idle pane", "#18"),
    ("10k lines of typical text", "#24"),
    ("Cold scrollback per pane", "#24"),
    ("Window on Linux", "#19"),
    ("Window on macOS", "#20"),
];

pub struct Budget<'a> {
    pub metric: &'a str,
    pub limit: u64,
    pub program: &'a Path,
    pub args: &'a [&'a str],
}

fn measure_once(budget: &Budget) -> io::Result<u64> {
    let mut child = Command::new(budget.program)
        .args(budget.args)
        .stdout(Stdio::null())
        .spawn()?;
    thread::sleep(SETTLE);
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

fn measure(budget: &Budget) -> io::Result<u64> {
    let mut samples = (0..RUNS)
        .map(|_| measure_once(budget))
        .collect::<io::Result<Vec<u64>>>()?;
    samples.sort_unstable();
    Ok(samples[RUNS / 2])
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
        };
        assert!(!run(&[budget]));
    }
}
