//! A long session against a real core: pane churn, output floods, window reconnects and API
//! calls, with the core's memory and an API echo round trip sampled throughout (#224). The
//! keystroke-to-echo budget in `docs/design/architecture.md` is `cargo xtask latency`'s to check.
//!
//! The driver runs in the core's first pane, so the API knows it as a caller with panes of its
//! own, and every pane it spawns is its child, which it may send to, read and close.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pinfold_api::control;
use serde_json::{Value, json};

/// Set in a soak core's environment: the run's directory, and how long the driver runs.
pub const DIRECTORY: &str = "PINFOLD_SOAK";
const SECONDS: &str = "PINFOLD_SOAK_SECONDS";

/// Most core growth after warm-up: four idle panes' budget, so one leaked pane per hour fails it.
pub const GROWTH_LIMIT: u64 = 4 * crate::mem_gate::PANE_LIMIT;
/// Most p99 per period of a round trip from `pane.send` until `pane.read` shows the line. It is
/// not `docs/design/architecture.md`'s keystroke budget (p99 16.7 ms): each poll is a new
/// connection, `hello` and a read of the pane's whole history, on top of the echo itself. 33 ms
/// is about four times the worst hour's p99 on ubuntu-latest (8.2 ms) and clears an M1's (25.2 ms).
pub const ROUND_TRIP_LIMIT: Duration = Duration::from_millis(33);
/// The first period lets scrollback reach its caps and the allocator settle.
const PERIODS: usize = 10;
/// The window is closed and reopened every this many rounds.
const RECONNECT_EVERY: u32 = 5;
const PROBES: u32 = 5;
/// Shortest round, which keeps the probes under the core's 20 sends a second to a pane.
const ROUND: Duration = Duration::from_secs(1);
/// An echo that takes longer is lost, which fails the run.
const ECHO_WAIT: Duration = Duration::from_secs(2);
/// Bounded output each second, so floods load the core without starving the machine.
const FLOOD: &str = "while :; do seq 1 50000; sleep 1; done\n";

/// One period's samples.
#[derive(Default)]
struct Period {
    rounds: u32,
    reconnects: u32,
    echoes: Vec<Duration>,
    footprint: u64,
}

/// The run's verdict: the table and every failure.
fn verdict(periods: &[Period], lost: Option<String>) -> (bool, String) {
    let mut failures: Vec<String> = lost.into_iter().collect();
    let mut table = String::from(
        "| Period | Rounds | Reconnects | Round trip p50 ms | Round trip p99 ms | Core MB |\n\
         | --- | --- | --- | --- | --- | --- |\n",
    );
    for (index, period) in periods.iter().enumerate() {
        let mut echoes = period.echoes.clone();
        echoes.sort_unstable();
        let at =
            |q: usize| echoes.get((echoes.len() * q / 100).min(echoes.len().saturating_sub(1)));
        let ms = |d: Option<&Duration>| d.map_or(f64::NAN, |d| d.as_secs_f64() * 1e3);
        if at(99).is_some_and(|p99| *p99 > ROUND_TRIP_LIMIT) {
            failures.push(format!(
                "period {} round trip p99 over {ROUND_TRIP_LIMIT:?}",
                index + 1
            ));
        }
        table.push_str(&format!(
            "| {} | {} | {} | {:.2} | {:.2} | {:.2} |\n",
            index + 1,
            period.rounds,
            period.reconnects,
            ms(at(50)),
            ms(at(99)),
            period.footprint as f64 / 1e6
        ));
    }
    if let Some((warm, rest)) = periods.split_first() {
        let peak = rest
            .iter()
            .map(|period| period.footprint)
            .max()
            .unwrap_or(0);
        let growth = peak.saturating_sub(warm.footprint);
        table.push_str(&format!(
            "\nGrowth after warm-up: {:.2} MB, limit {:.2} MB. Round trip p99 limit {ROUND_TRIP_LIMIT:?}.\n",
            growth as f64 / 1e6,
            GROWTH_LIMIT as f64 / 1e6
        ));
        if growth > GROWTH_LIMIT {
            failures.push("core memory grew past the limit".to_owned());
        }
    }
    for failure in &failures {
        table.push_str(&format!("**FAIL**: {failure}\n"));
    }
    (failures.is_empty(), table)
}

/// One call on a fresh connection, as `pinfold ctl` makes it.
fn call(method: &str, params: Value) -> Result<Value, String> {
    let socket = control::socket_path(&control::runtime_dir());
    let stream = UnixStream::connect(socket).map_err(|error| format!("{method}: {error}"))?;
    let (_, result) =
        control::ask(&stream, method, &params).map_err(|error| format!("{method}: {error}"))?;
    Ok(result)
}

fn spawn(place: &str, of: Option<u64>) -> Result<u64, String> {
    let result = call("pane.spawn", json!({"where": place, "of": of}))?;
    result["pane"]
        .as_u64()
        .ok_or_else(|| "pane.spawn: no pane".to_owned())
}

fn send(pane: u64, text: &str) -> Result<Value, String> {
    call("pane.send", json!({"pane": pane, "text": text}))
}

/// Types a unique line into `pane` over the API and times it until `pane.read` shows it.
fn echo(pane: u64, marker: &str) -> Result<Duration, String> {
    let start = Instant::now();
    send(pane, &format!(": {marker}\n"))?;
    while start.elapsed() < ECHO_WAIT {
        let read = call("pane.read", json!({"pane": pane}))?;
        if read["text"]
            .as_str()
            .is_some_and(|text| text.contains(marker))
        {
            return Ok(start.elapsed());
        }
    }
    Err(format!("no echo from pane {pane} in {ECHO_WAIT:?}"))
}

/// The core's window, if one is running.
fn window(core: u32) -> Option<String> {
    let found = Command::new("pgrep")
        .args(["-P", &core.to_string(), "-x", "pinfold-window"])
        .output()
        .ok()?;
    let pid = String::from_utf8_lossy(&found.stdout).trim().to_owned();
    (!pid.is_empty()).then_some(pid)
}

/// The shell's builtin, as a slim image has no `kill` program.
fn kill(pid: &str) {
    let _ = Command::new("/bin/sh")
        .args(["-c", &format!("kill {pid}")])
        .status();
}

/// Kills the window, as a crash would, and opens a new one.
fn reconnect(core: u32) -> Result<(), String> {
    let old = window(core);
    if let Some(pid) = &old {
        kill(pid);
    }
    for _ in 0..50 {
        // A window not yet reaped is raised instead, and answers once it ends.
        if call("window.open", json!({})).is_ok()
            && window(core).is_some_and(|pid| Some(pid) != old)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err("the window did not reopen".to_owned())
}

/// One round: churn three panes, check none is lost, and time API round trips.
fn round(
    core: u32,
    number: u32,
    kept: &[u64],
    echo_pane: u64,
    period: &mut Period,
) -> Result<(), String> {
    let start = Instant::now();
    let tab = spawn("tab", None)?;
    let right = spawn("split-right", Some(tab))?;
    let down = spawn("split-down", Some(right))?;
    for pane in [down, right, tab] {
        call("pane.close", json!({"pane": pane}))?;
    }
    if number.is_multiple_of(RECONNECT_EVERY) {
        reconnect(core)?;
        period.reconnects += 1;
    }
    let listed = call("pane.list", json!({}))?;
    let ids: Vec<u64> = listed["panes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pane| pane["id"].as_u64())
        .collect();
    if let Some(lost) = kept
        .iter()
        .chain([&echo_pane])
        .find(|pane| !ids.contains(pane))
    {
        return Err(format!("pane {lost} was lost"));
    }
    for probe in 0..PROBES {
        period
            .echoes
            .push(echo(echo_pane, &format!("soak-{number}-{probe}"))?);
    }
    period.rounds += 1;
    thread::sleep(ROUND.saturating_sub(start.elapsed()));
    Ok(())
}

fn drive(seconds: u64) -> (bool, String) {
    let core = std::os::unix::process::parent_id();
    let length = Duration::from_secs(seconds) / PERIODS as u32;
    let mut periods: Vec<Period> = Vec::new();
    let setup = || -> Result<Vec<u64>, String> {
        let socket = control::socket_path(&control::runtime_dir());
        let stream = UnixStream::connect(socket).map_err(|error| error.to_string())?;
        let (hello, _) =
            control::ask(&stream, "pane.list", &json!({})).map_err(|e| e.to_string())?;
        let own = hello["pane"].as_u64().ok_or("the driver has no pane")?;
        let floods = [spawn("split-right", None)?, spawn("tab", None)?];
        for pane in floods {
            send(pane, FLOOD)?;
        }
        call("window.open", json!({}))?;
        Ok(vec![own, floods[0], floods[1]])
    };
    let mut lost = None;
    match setup() {
        Ok(kept) => {
            let mut number = 0;
            'periods: for _ in 0..PERIODS {
                let mut period = Period::default();
                let end = Instant::now() + length;
                // A fresh echo pane each period, as `pane.read` reads a pane's whole history,
                // so a long-lived one would time the read more than the echo.
                let echo = spawn("tab", None).and_then(|echo| {
                    while Instant::now() < end {
                        number += 1;
                        round(core, number, &kept, echo, &mut period)?;
                    }
                    call("pane.close", json!({"pane": echo}))
                });
                if let Err(error) = echo {
                    lost = Some(error);
                }
                match pinfold_sys::footprint(core) {
                    Ok(bytes) => period.footprint = bytes,
                    Err(error) => lost = lost.or(Some(format!("cannot read the core: {error}"))),
                }
                periods.push(period);
                if lost.is_some() {
                    break 'periods;
                }
            }
        }
        Err(error) => lost = Some(error),
    }
    verdict(&periods, lost)
}

/// The shell of a soak core's panes: the first pane drives the run and writes its report, and
/// every other is a plain `/bin/sh`.
pub fn shell(directory: OsString) -> io::Error {
    let directory = PathBuf::from(directory);
    if fs::create_dir(directory.join("driver")).is_err() {
        return Command::new("/bin/sh").env_remove(DIRECTORY).exec();
    }
    let seconds = std::env::var(SECONDS).ok().and_then(|s| s.parse().ok());
    let (passed, table) = drive(seconds.unwrap_or(60));
    let report = format!("{}\n{table}", if passed { "pass" } else { "fail" });
    let written = fs::write(directory.join("report.part"), report);
    written
        .and_then(|()| fs::rename(directory.join("report.part"), directory.join("report")))
        .err()
        .unwrap_or_else(|| io::Error::other("the soak driver finished"))
}

/// Runs `core` for `minutes` with the driver in its first pane, prints the driver's table and
/// returns whether it passed. A core that exits early has lost the session.
pub fn run(core: &Path, minutes: u64) -> io::Result<bool> {
    let directory = std::env::temp_dir().join(format!("pinfold-soak-{}", std::process::id()));
    // Private and new, so no other user can plant the driver's directory or read its report.
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let mut command = Command::new(core);
    command
        .args(["--idle", "1"])
        .stdout(Stdio::null())
        .env("SHELL", std::env::current_exe()?)
        .env(DIRECTORY, &directory)
        .env(SECONDS, (minutes * 60).to_string());
    // Keeps the user's socket, config, session and data out of reach.
    for (variable, name) in [
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"),
    ] {
        fs::create_dir_all(directory.join(name))?;
        command.env(variable, directory.join(name));
    }
    let mut child = command.spawn()?;
    let give_up = Instant::now() + Duration::from_secs(minutes * 60 + 120);
    let report = loop {
        if let Ok(report) = fs::read_to_string(directory.join("report")) {
            break report;
        }
        if child.try_wait()?.is_some() {
            break "fail\n**FAIL**: the core exited, so the session was lost\n".to_owned();
        }
        if Instant::now() > give_up {
            break "fail\n**FAIL**: the driver did not finish\n".to_owned();
        }
        thread::sleep(Duration::from_secs(1));
    };
    // Nothing tells a window that its core was killed to exit.
    if let Some(pid) = window(child.id()) {
        kill(&pid);
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&directory);
    let (verdict, table) = report.split_once('\n').unwrap_or(("fail", ""));
    println!(
        "Soak of {minutes} min on {}.\n\n{table}",
        std::env::consts::OS
    );
    Ok(verdict == "pass")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn period(echo_ms: u64, footprint: u64) -> Period {
        Period {
            rounds: 1,
            reconnects: 0,
            echoes: vec![Duration::from_millis(echo_ms); 10],
            footprint,
        }
    }

    #[test]
    fn steady_memory_and_fast_echoes_pass() {
        let periods = [period(2, 3_000_000), period(2, 3_100_000)];
        assert!(verdict(&periods, None).0);
    }

    #[test]
    fn growth_after_warm_up_fails() {
        let grew = [
            period(2, 3_000_000),
            period(2, 3_000_000 + GROWTH_LIMIT + 1),
        ];
        assert!(!verdict(&grew, None).0);
    }

    #[test]
    fn slow_echoes_fail() {
        let periods = [period(2, 3_000_000), period(50, 3_000_000)];
        let (passed, table) = verdict(&periods, None);
        assert!(
            !passed && table.contains("period 2 round trip p99"),
            "{table}"
        );
    }

    #[test]
    fn a_lost_pane_fails() {
        let (passed, table) = verdict(&[period(2, 3_000_000)], Some("pane 3 was lost".into()));
        assert!(!passed && table.contains("pane 3 was lost"));
    }
}
