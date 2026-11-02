//! The `pinfold-core` binary.

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use pinfold_api::control::{self, Control};
use pinfold_core::io::{Reactor, Waker};
use pinfold_core::shell;

const ROWS: u16 = 24;
const COLUMNS: u16 = 80;
/// How long the API thread waits on the reactor, which only blocks on a spawn.
const OPEN_WAIT: Duration = Duration::from_secs(2);
const IDLE: Duration = Duration::from_secs(3600);

/// Returns the window binary beside this one. It is resolved once, so nothing later can name
/// another program.
fn window_binary() -> io::Result<PathBuf> {
    Ok(env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-window"))
}

/// Binds the control socket and answers it on the API thread (ADR-0007). The reactor's thread
/// makes every spawn (see `pinfold_sys::socket::pair`), so `window.open` crosses to it. The
/// channel holds one, as the API thread serves one peer at a time.
fn listen(waker: Arc<Waker>) -> io::Result<Receiver<SyncSender<bool>>> {
    let control = Control::bind(&control::runtime_dir()).map_err(io::Error::other)?;
    let (ask, asked) = mpsc::sync_channel(1);
    let open = move || {
        let (reply, answer) = mpsc::sync_channel(1);
        ask.try_send(reply).is_ok()
            && waker.wake().is_ok()
            && answer.recv_timeout(OPEN_WAIT).unwrap_or(false)
    };
    let serve = move || control.serve(open);
    thread::Builder::new()
        .name("api".to_owned())
        .stack_size(control::STACK)
        .spawn(serve)?;
    Ok(asked)
}

/// Runs `panes` shells, a tab each, until every pane has gone and no window is left to tell, or
/// until killed. With a size, it opens a window for `cargo xtask mem-gate` and prints its pid.
fn daemon(panes: usize, measured: Option<&str>) -> io::Result<()> {
    let binary = window_binary()?;
    let mut reactor = Reactor::new()?;
    let asked = listen(reactor.waker())?;
    for _ in 0..panes {
        reactor
            .open_tab(shell::command(), ROWS, COLUMNS)
            .map_err(io::Error::other)?;
    }
    if let Some(size) = measured {
        let mut command = Command::new(&binary);
        command.args(["--measure", size]);
        reactor.spawn_window_command(command)?;
        let pid = reactor.window_pid().unwrap_or_default();
        writeln!(io::stdout(), "{pid}")?;
    }
    loop {
        reactor.turn(Some(IDLE))?;
        while let Ok(reply) = asked.try_recv() {
            // A timed out caller has gone, and the window it asked for still opens.
            let _ = reply.try_send(reactor.spawn_window(&binary).is_ok());
        }
        if reactor.pane_count() == 0 && !reactor.has_window() {
            return Ok(());
        }
    }
}

/// Hardens, then runs the first pane, whose shell is the user's.
fn serve() -> io::Result<()> {
    pinfold_sys::harden()?;
    daemon(1, None)
}

/// Plays `file` through a real pane, says so on stdout, then idles until killed.
/// `cargo xtask mem-gate` measures this.
fn replay(file: &str) -> io::Result<()> {
    let mut reactor = Reactor::new()?;
    let mut cat = Command::new("cat");
    cat.arg(file);
    let pane = reactor.spawn(cat, 50, 120)?;
    while !reactor.is_closed(pane) {
        reactor.turn(Some(Duration::from_secs(1)))?;
    }
    writeln!(io::stdout(), "drained")?;
    loop {
        reactor.turn(Some(IDLE))?;
    }
}

fn run(args: &[&str]) -> io::Result<()> {
    match *args {
        [] => serve(),
        // The daemon without hardening, which also leaves a way to attach a debugger
        // (ADR-0020). `cargo xtask mem-gate` measures these two, as /proc refuses a hardened one.
        ["--idle", panes] => daemon(panes.parse().map_err(io::Error::other)?, None),
        ["--window", panes, size] => daemon(panes.parse().map_err(io::Error::other)?, Some(size)),
        ["--replay", file] => replay(file),
        _ => Err(io::Error::other(
            "usage: pinfold-core [--idle <panes> | --window <panes> <width>x<height> | --replay <file>]",
        )),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold-core: {error}");
            ExitCode::FAILURE
        }
    }
}
