//! The `pinfold-core` binary.

use std::env;
use std::io::Write;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

use pinfold_api::control::{self, Control};
use pinfold_core::io::Reactor;

/// Binds the control socket and answers it on the API thread (ADR-0007).
fn listen() -> std::io::Result<()> {
    let control = Control::bind(&control::runtime_dir()).map_err(std::io::Error::other)?;
    // shortcut: no window can open yet, as the core has no link until M2's next step.
    let serve = move || control.serve(|| false);
    thread::Builder::new()
        .name("api".to_owned())
        .stack_size(control::STACK)
        .spawn(serve)?;
    Ok(())
}

/// Runs the daemon until killed.
fn serve() -> std::io::Result<()> {
    pinfold_sys::harden()?;
    listen()?;
    let mut reactor = Reactor::new()?;
    loop {
        reactor.turn(Some(Duration::from_secs(3600)))?;
    }
}

/// Runs `panes` shells until killed. `cargo xtask mem-gate` measures this, so it is the daemon
/// without hardening, which also leaves a way to attach a debugger (ADR-0020).
fn idle(panes: usize) -> std::io::Result<()> {
    listen()?;
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut reactor = Reactor::new()?;
    for _ in 0..panes {
        reactor.spawn(Command::new(&shell), 24, 80)?;
    }
    loop {
        reactor.turn(Some(Duration::from_secs(3600)))?;
    }
}

/// Plays `file` through a real pane, says so on stdout, then idles until killed.
/// `cargo xtask mem-gate` measures this.
fn replay(file: &str) -> std::io::Result<()> {
    let mut reactor = Reactor::new()?;
    let mut cat = Command::new("cat");
    cat.arg(file);
    let pane = reactor.spawn(cat, 50, 120)?;
    while !reactor.is_closed(pane) {
        reactor.turn(Some(Duration::from_secs(1)))?;
    }
    writeln!(std::io::stdout(), "drained")?;
    loop {
        reactor.turn(Some(Duration::from_secs(3600)))?;
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let ["--idle", panes] = args[..] {
        let result = panes.parse().map_err(std::io::Error::other).and_then(idle);
        if let Err(error) = result {
            eprintln!("pinfold-core: {error}");
        }
        return ExitCode::FAILURE;
    }
    if let ["--replay", file] = args[..] {
        if let Err(error) = replay(file) {
            eprintln!("pinfold-core: {error}");
        }
        return ExitCode::FAILURE;
    }
    if !args.is_empty() {
        eprintln!("usage: pinfold-core [--idle <panes> | --replay <file>]");
        return ExitCode::FAILURE;
    }
    if let Err(error) = serve() {
        eprintln!("pinfold-core: {error}");
    }
    ExitCode::FAILURE
}
