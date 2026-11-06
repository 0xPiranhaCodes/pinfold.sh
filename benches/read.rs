//! `pane.read` latency by `lines`, on a pane fed 100,000 lines, every tenth wrapped (#257).
//!
//! Run with `cargo run --release -p pinfold-benches --bin read`. It prints a Markdown table of
//! each case's p50 and p99 over [`ROUNDS`] reads and the bytes one read returns.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::error::Error;
use std::process::Command;
use std::time::{Duration, Instant};

use pinfold_core::io::Reactor;

const FED_LINES: usize = 100_000;
const ROUNDS: usize = 200;
const CASES: [Option<usize>; 6] = [
    None,
    Some(0),
    Some(1),
    Some(50),
    Some(1_000),
    Some(1_000_000),
];

fn main() -> Result<(), Box<dyn Error>> {
    let mut reactor = Reactor::new()?;
    let script = format!(
        "awk 'BEGIN {{ for (i = 1; i <= {FED_LINES}; i++) \
         if (i % 10 == 0) printf \"%06d %0300d\\n\", i, 0; else printf \"%06d line\\n\", i; \
         print \"fed\" }}'; sleep 600"
    );
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &script]);
    let pane = reactor.spawn(command, 50, 120)?;
    let start = Instant::now();
    while !reactor
        .read(pane, Some(1))
        .is_some_and(|line| line == "fed")
    {
        if start.elapsed() > Duration::from_secs(120) {
            return Err("the pane was not fed in 2 minutes".into());
        }
        reactor.turn(Some(Duration::from_millis(50)))?;
    }
    println!("| lines | p50 µs | p99 µs | bytes |\n|---|---|---|---|");
    for lines in CASES {
        let mut times = Vec::with_capacity(ROUNDS);
        let mut bytes = 0;
        for _ in 0..ROUNDS {
            let at = Instant::now();
            let text = reactor.read(pane, lines).ok_or("no such pane")?;
            times.push(at.elapsed());
            bytes = text.len();
        }
        times.sort_unstable();
        let at = |percent: usize| times[(ROUNDS - 1) * percent / 100].as_micros();
        let name = lines.map_or_else(|| "all".to_owned(), |lines| lines.to_string());
        println!("| {name} | {} | {} | {bytes} |", at(50), at(99));
    }
    Ok(())
}
