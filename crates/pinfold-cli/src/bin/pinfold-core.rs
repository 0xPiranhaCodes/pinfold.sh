//! The `pinfold-core` binary.

use std::env;
use std::io::Write;
use std::process::{Command, ExitCode};
use std::time::Duration;

use pinfold_core::io::Reactor;

/// Runs `panes` shells until killed. `cargo xtask mem-gate` measures this.
fn idle(panes: usize) -> std::io::Result<()> {
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

#[allow(clippy::print_stdout, reason = "binary main")]
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
    println!(
        "pinfold-core {} (pre-alpha, not functional yet)",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::SUCCESS
}
