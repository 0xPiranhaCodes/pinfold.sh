//! Scrollback size, feed speed and read latency on the M0 spike's corpora.
//!
//! Run with `cargo run --release -p pinfold-benches --bin scrollback`.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use pinfold_core::scrollback::Scrollback;

const COLUMNS: usize = 120;
const SCREEN: usize = 50;
const FED_LINES: usize = 12_000;
const CORPORA: [&str; 7] = [
    "build-log",
    "plain-log",
    "git-diff",
    "dense",
    "random",
    "truecolor",
    "cjk-links",
];
const WORDS: [&str; 16] = [
    "compile", "error", "warning", "finished", "request", "response", "handler", "config",
    "thread", "buffer", "session", "update", "failed", "passed", "module", "cache",
];

struct Size;

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        SCREEN
    }

    fn screen_lines(&self) -> usize {
        SCREEN
    }

    fn columns(&self) -> usize {
        COLUMNS
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    fn word(&mut self) -> &'static str {
        WORDS[self.below(WORDS.len())]
    }
}

/// One line of deterministic output. `dense`, `random` and `truecolor` are worst cases.
fn line(name: &str, rng: &mut Rng, number: usize, out: &mut Vec<u8>) -> std::io::Result<()> {
    match name {
        "build-log" => {
            let (word, colour) = (rng.word(), 31 + rng.below(6));
            write!(
                out,
                "\x1b[1;{colour}m{word:>12}\x1b[0m crate-{} v0.{}.{} ",
                rng.below(40),
                rng.below(9),
                rng.below(30)
            )?;
            write!(
                out,
                "(/home/dev/project/crates/{} in {}.{:02}s)",
                rng.word(),
                rng.below(9),
                rng.below(99)
            )?;
        }
        "plain-log" => {
            write!(
                out,
                "2026-10-31T{:02}:{:02}:{:02}.{:03}Z INFO req={:016x} ",
                number / 3600 % 24,
                number / 60 % 60,
                number % 60,
                rng.below(1000),
                rng.next()
            )?;
            write!(
                out,
                "{} {} status={} ms={}",
                rng.word(),
                rng.word(),
                200 + rng.below(4) * 100,
                rng.below(900)
            )?;
        }
        "git-diff" => {
            let (mark, colour) = [("+", 32), ("-", 31), (" ", 39)][rng.below(3)];
            write!(
                out,
                "\x1b[{colour}m{mark}    let {} = self.{}(&{}, {});\x1b[0m",
                rng.word(),
                rng.word(),
                rng.word(),
                rng.below(4096)
            )?;
        }
        "dense" => {
            while out.len() < COLUMNS - 12 {
                write!(out, "{} ", rng.word())?;
            }
        }
        "random" => out.extend((0..COLUMNS - 1).map(|_| b'!' + rng.below(90) as u8)),
        "truecolor" => {
            for _ in 0..COLUMNS - 1 {
                write!(
                    out,
                    "\x1b[38;2;{};{};{}m{}",
                    rng.below(256),
                    rng.below(256),
                    rng.below(256),
                    char::from(b'a' + rng.below(26) as u8)
                )?;
            }
            out.extend_from_slice(b"\x1b[0m");
        }
        _ => {
            write!(
                out,
                "\x1b]8;id=l{};https://example.com/{}\x1b\\link\x1b]8;;\x1b\\ \x1b[4:3m\x1b[58;5;{}m下線\x1b[0m e\u{301} ",
                number % 5,
                number % 7,
                rng.below(256)
            )?;
            for _ in 0..30 {
                out.extend_from_slice(["漢", "字", "表", "示"][rng.below(4)].as_bytes());
            }
        }
    }
    Ok(())
}

fn corpus(name: &str) -> std::io::Result<Vec<u8>> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut out = Vec::new();
    let mut one = Vec::new();
    for number in 0..FED_LINES {
        one.clear();
        line(name, &mut rng, number, &mut one)?;
        one.extend_from_slice(b"\r\n");
        out.extend_from_slice(&one);
    }
    Ok(out)
}

/// Returns megabytes per second for the whole corpus, parsed in 64 KB reads.
fn feed(bytes: &[u8], sink: Option<&Arc<Mutex<Scrollback>>>) -> f64 {
    let config = Config {
        scrolling_history: 0,
        ..Config::default()
    };
    let mut term = Term::new(config, &Size, VoidListener);
    if let Some(sink) = sink {
        term.grid_mut().set_history_sink(sink.clone());
    }
    let mut parser = Processor::<StdSyncHandler>::new();
    let start = Instant::now();
    for read in bytes.chunks(64 * 1024) {
        parser.advance(&mut term, read);
    }
    bytes.len() as f64 / 1e6 / start.elapsed().as_secs_f64()
}

fn main() -> std::io::Result<()> {
    println!("{COLUMNS} columns, {SCREEN}-row screen, {FED_LINES} lines fed.\n");
    println!(
        "| Corpus | Rows held | Store MB | MB/s, no store | MB/s, store | Read 50 rows p99 ms |"
    );
    println!("| --- | --- | --- | --- | --- | --- |");
    for name in CORPORA {
        let bytes = corpus(name)?;
        let plain = feed(&bytes, None);
        let sink = Arc::new(Mutex::new(Scrollback::new()));
        let stored = feed(&bytes, Some(&sink));
        let store = sink.lock().map_err(|_| std::io::Error::other("poisoned"))?;
        let mut rng = Rng(7);
        let mut reads: Vec<f64> = (0..2000)
            .map(|_| {
                let first = rng.below(store.len().max(1));
                let start = Instant::now();
                std::hint::black_box(store.rows(first, SCREEN));
                start.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        reads.sort_by(f64::total_cmp);
        let p99 = reads.get(reads.len() * 99 / 100).copied().unwrap_or(0.0);
        let held = store.bytes() as f64 / 1e6;
        println!(
            "| {name} | {} | {held:.2} | {plain:.0} | {stored:.0} | {p99:.2} |",
            store.len()
        );
    }
    Ok(())
}
