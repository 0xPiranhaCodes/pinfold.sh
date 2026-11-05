//! Keystroke-to-echo latency through a real core: at rest, while a session save syncs to slow
//! storage, and under sustained API traffic (issue #218).
//!
//! `cargo xtask latency` builds the core and runs this once per case. A run is `echo --core
//! <pinfold-core> [--fsync-ms <ms>] [--api] [--seconds <s>]`. It starts the core with this binary
//! as its window, on the real link, and as its one pane's program. The window types a letter and
//! a carriage return every 20 ms and times each until the pane's first cell shows that letter.
//! The pane echoes in raw mode, as a shell's line editor does, and moves to a new directory every
//! second, so each save the core takes a minute after activity writes a new snapshot. With
//! `--api` it also keeps three API connections reading itself as fast as the core answers. With
//! `--session-save <pinfold>` it calls `pinfold ctl session save` every 5 s, which the save thread
//! writes, and the window allows each prompt.
//!
//! On macOS the core loads `slow_fsync.c`, which counts every sync and delays it by `--fsync-ms`.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::error::Error;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use pinfold_api::control;
use pinfold_proto::link::{ApprovalAction, ApprovalAnswer, Request};
use pinfold_sys::socket;
use pinfold_ui::link::Link;

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "usage: echo --core <pinfold-core> [--fsync-ms <ms>] [--api] \
                     [--session-save <pinfold>] [--seconds <s>]";
/// The run's directory, in the window and the pane.
const DIR: &str = "PINFOLD_ECHO_DIR";
/// Set in the window.
const WINDOW: &str = "PINFOLD_ECHO_WINDOW";
/// Set when the pane should keep the API busy.
const API: &str = "PINFOLD_ECHO_API";
const SECONDS: &str = "PINFOLD_ECHO_SECONDS";
/// Between keystrokes: 50 a second, faster than anyone types.
const GAP: Duration = Duration::from_millis(20);
/// Between `pinfold ctl session save` calls with `--session-save`.
const SESSION_SAVE_GAP: Duration = Duration::from_secs(5);
/// One below the API's cap of 4 connections per caller (ADR-0032).
const CONNECTIONS: usize = 3;
const PATIENCE: Duration = Duration::from_secs(30);
/// The budget in `docs/design/architecture.md`: within a 60 Hz frame at p99, no stall past 50 ms.
const P99_MS: f64 = 16.7;
const MAX_MS: f64 = 50.0;

fn main() -> Result<ExitCode> {
    if let Some(dir) = env::var_os(DIR) {
        let dir = PathBuf::from(dir);
        if env::var_os(WINDOW).is_some() {
            window(&dir)?;
        } else {
            pane(&dir)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let args: Vec<String> = env::args().skip(1).collect();
    run(&args)
}

fn run(args: &[String]) -> Result<ExitCode> {
    let (mut core, mut fsync_ms, mut api, mut seconds) = (None, 0_u64, false, 200_u64);
    let mut session_save = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--core" => core = args.next().map(PathBuf::from),
            "--fsync-ms" => fsync_ms = args.next().ok_or(USAGE)?.parse()?,
            "--seconds" => seconds = args.next().ok_or(USAGE)?.parse()?,
            "--api" => api = true,
            "--session-save" => session_save = args.next().map(PathBuf::from),
            _ => return Err(USAGE.into()),
        }
    }
    let core = core.ok_or(USAGE)?;
    let exe = env::current_exe()?;
    // A short path, as a socket's path has a low limit on macOS.
    let dir = env::temp_dir().join(format!("pe{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("run"))?;
    // The core starts the `pinfold-window` beside it.
    fs::copy(&core, dir.join("pinfold-core"))?;
    let exe = exe.display();
    script(
        &dir.join("pinfold-window"),
        &format!("exec env {WINDOW}=1 '{exe}'"),
    )?;
    // The core asks the shell for its PATH with `-i -c`, for sandboxed agents.
    let shell = format!("case $1 in -i) exec /bin/sh \"$@\";; esac\nstty raw -echo\nexec '{exe}'");
    script(&dir.join("shell"), &shell)?;
    let mut command = Command::new(dir.join("pinfold-core"));
    command
        .args(["--window", "1", "640x480"])
        .env("SHELL", dir.join("shell"))
        .env(DIR, &dir)
        .env(SECONDS, seconds.to_string())
        .stdout(Stdio::null())
        .stderr(fs::File::create(dir.join("core.log"))?);
    isolate(&mut command, &dir);
    if api {
        command.env(API, "1");
    }
    if cfg!(target_os = "macos") {
        let shim = dir.join("slow_fsync.dylib");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("slow_fsync.c");
        let built = Command::new("cc")
            .args(["-dynamiclib", "-O2", "-o"])
            .args([&shim, &source])
            .status()?;
        if !built.success() {
            return Err("cc could not build slow_fsync.c".into());
        }
        command
            .env("DYLD_INSERT_LIBRARIES", &shim)
            .env("PINFOLD_SLOW_FSYNC_MS", fsync_ms.to_string())
            .env("PINFOLD_SLOW_FSYNC_LOG", dir.join("fsyncs"));
    } else if fsync_ms > 0 {
        return Err("--fsync-ms needs macOS's DYLD_INSERT_LIBRARIES".into());
    }
    let mut child = command.spawn()?;
    let saves = Arc::new(AtomicU64::new(0));
    let asked = session_save.is_some();
    if let Some(pinfold) = session_save {
        let (dir, saves) = (dir.clone(), Arc::clone(&saves));
        thread::spawn(move || save_sessions(&pinfold, &dir, &saves));
    }
    let samples = wait_for(&dir, "samples", Duration::from_secs(seconds) + PATIENCE);
    let _ = child.kill();
    let _ = child.wait();
    let saves = saves.load(Ordering::Relaxed);
    let result = samples.and_then(|samples| report(&dir, &samples, fsync_ms, api, saves));
    // A case with no successful session.save measured nothing it was meant to.
    let result = result.map(|met| {
        if asked && saves == 0 {
            ExitCode::FAILURE
        } else {
            met
        }
    });
    let _ = fs::remove_dir_all(&dir);
    result
}

/// Points `command` at the run's own runtime, config and state directories, never the person's.
fn isolate(command: &mut Command, dir: &Path) {
    command
        .env("XDG_RUNTIME_DIR", dir.join("run"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        // The state directory, on macOS from HOME.
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("HOME", dir.join("state"));
}

/// Calls `pinfold ctl session save` every [`SESSION_SAVE_GAP`] while the window types, from a
/// terminal of its own outside every pane, as that method asks, and counts the calls that
/// succeeded. The window allows each owner prompt.
fn save_sessions(pinfold: &Path, dir: &Path, saves: &AtomicU64) {
    while !dir.join("ready").exists() {
        thread::sleep(Duration::from_millis(100));
    }
    while !dir.join("samples").exists() {
        thread::sleep(SESSION_SAVE_GAP);
        let mut command = Command::new("script");
        command
            .args(["-q", "/dev/null"])
            .arg(pinfold)
            .args(["ctl", "session", "save"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        isolate(&mut command, dir);
        if command.status().is_ok_and(|status| status.success()) {
            saves.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn script(path: &Path, body: &str) -> Result {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, format!("#!/bin/sh\n{body}\n"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Writes `text` to `path` whole: a reader sees nothing or all of it.
fn note(path: &Path, text: impl AsRef<[u8]>) -> Result {
    let part = path.with_extension("part");
    fs::write(&part, text)?;
    fs::rename(part, path)?;
    Ok(())
}

/// The run's file `name` once it is there, or an error naming the core's log.
fn wait_for(dir: &Path, name: &str, patience: Duration) -> Result<String> {
    let start = Instant::now();
    while start.elapsed() < patience {
        if let Ok(failed) = fs::read_to_string(dir.join("failed")) {
            return Err(failed.into());
        }
        match fs::read_to_string(dir.join(name)) {
            Ok(text) if !text.is_empty() => return Ok(text),
            _ => thread::sleep(Duration::from_millis(100)),
        }
    }
    let log = fs::read_to_string(dir.join("core.log")).unwrap_or_default();
    Err(format!("no {name} in time; the core said: {log}").into())
}

/// Prints the run as a row of `cargo xtask latency`'s table and says whether it met the budget.
fn report(dir: &Path, samples: &str, fsync_ms: u64, api: bool, saves: u64) -> Result<ExitCode> {
    let mut ms = samples
        .lines()
        .map(|micros| Ok(micros.parse::<f64>()? / 1000.0))
        .collect::<Result<Vec<f64>>>()?;
    ms.sort_by(f64::total_cmp);
    let at = |share: f64| ms[((ms.len() - 1) as f64 * share).round() as usize];
    let (p50, p99, max) = (at(0.5), at(0.99), at(1.0));
    let syncs = fs::read(dir.join("fsyncs")).map_or(0, |log| log.len());
    let rate = fs::read_to_string(dir.join("api")).unwrap_or_else(|_| "0".to_owned());
    let met = p99 <= P99_MS && max <= MAX_MS;
    let verdict = if met { "met" } else { "missed" };
    println!(
        "| {fsync_ms} | {} | {saves} | {} | {p50:.2} | {p99:.2} | {max:.2} | {syncs} | {} | {verdict} |",
        if api { "yes" } else { "no" },
        ms.len(),
        rate.trim()
    );
    Ok(if met {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Types into the focused pane and notes each keystroke's echo time, in microseconds.
fn window(dir: &Path) -> Result {
    let failed = dir.join("failed");
    let result = type_and_time(dir);
    if let Err(error) = &result {
        let _ = note(&failed, format!("the window: {error}"));
    }
    result
}

fn type_and_time(dir: &Path) -> Result {
    let seconds: u64 = env::var(SECONDS)?.parse()?;
    let mut link = Link::new(socket::take_link()?)?;
    if !link.resize(24, 80) {
        return Err("the link took no resize".into());
    }
    let start = Instant::now();
    while !(link.is_greeted() && dir.join("ready").exists()) {
        if start.elapsed() > PATIENCE {
            return Err("the core or the pane never got ready".into());
        }
        let _ = link.turn(Some(GAP));
    }
    let mut samples = String::new();
    let end = Instant::now() + Duration::from_secs(seconds);
    for letter in (b'a'..=b'z').cycle() {
        if Instant::now() >= end {
            break;
        }
        let pane = link.focused();
        let sent = Instant::now();
        if !link.send(Request::Input {
            pane,
            bytes: &[letter, b'\r'],
        }) {
            return Err("the link took no input".into());
        }
        while first_cell(&link, pane) != Some(char::from(letter)) {
            if sent.elapsed() > PATIENCE {
                return Err("an echo never came".into());
            }
            link.turn(Some(Duration::from_millis(5)))
                .map_err(|reason| format!("the link ended: {reason:?}"))?;
            allow_owner_prompts(&mut link);
        }
        writeln!(samples, "{}", sent.elapsed().as_micros())?;
        thread::sleep(GAP);
    }
    note(&dir.join("samples"), samples)
}

/// Allows `session.save`'s owner prompts, which are drawn by the window and so take no keystroke.
fn allow_owner_prompts(link: &mut Link) {
    while let Some(prompt) = link.take_approval() {
        if prompt.action == ApprovalAction::Owner {
            let (id, answer) = (prompt.id, ApprovalAnswer::AllowOnce);
            link.send(Request::ApprovalAnswer { id, answer });
        }
    }
}

fn first_cell(link: &Link, pane: u32) -> Option<char> {
    let screen = link.screens().iter().find(|screen| screen.pane == pane)?;
    screen.frame.cells.first().map(|cell| cell.character)
}

/// Echoes stdin, which the shell script put in raw mode, moves directory every second, and with
/// [`API`] keeps the API busy.
fn pane(dir: &Path) -> Result {
    let served = Arc::new(AtomicU64::new(0));
    if env::var_os(API).is_some() {
        for _ in 0..CONNECTIONS {
            let (served, failed) = (Arc::clone(&served), dir.join("failed"));
            thread::spawn(move || {
                if let Err(error) = read_self(&served) {
                    let _ = note(&failed, format!("the API reader: {error}"));
                }
            });
        }
    }
    let wander = dir.to_owned();
    thread::spawn(move || {
        let start = Instant::now();
        let mut moved = 0_u64;
        loop {
            moved += 1;
            let to = wander.join("cwd").join(moved.to_string());
            let _ = fs::create_dir_all(&to).and_then(|()| env::set_current_dir(&to));
            let rate = served.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64();
            let _ = note(&wander.join("api"), format!("{rate:.0}"));
            thread::sleep(Duration::from_secs(1));
        }
    });
    note(&dir.join("ready"), "yes")?;
    let (mut input, mut output) = (std::io::stdin().lock(), std::io::stdout().lock());
    let mut buffer = [0; 256];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..read])?;
        output.flush()?;
    }
}

/// Reads this pane, pane 0 of a fresh core, over and over on one connection.
fn read_self(served: &AtomicU64) -> Result {
    let mut stream = UnixStream::connect(control::socket_path(&control::runtime_dir()))?;
    let mut replies = BufReader::new(stream.try_clone()?).lines();
    writeln!(
        stream,
        r#"{{"jsonrpc":"2.0","id":0,"method":"hello","params":{{"version":2}}}}"#
    )?;
    for id in 0_u64.. {
        let reply = replies.next().ok_or("the core hung up")??;
        if reply.contains(r#""error""#) {
            return Err(reply.into());
        }
        if id > 0 {
            served.fetch_add(1, Ordering::Relaxed);
        }
        writeln!(
            stream,
            r#"{{"jsonrpc":"2.0","id":{},"method":"pane.read","params":{{"pane":0,"lines":200}}}}"#,
            id + 1
        )?;
    }
    Ok(())
}
