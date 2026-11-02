//! Scrollback size, feed speed, read, wheel, vi, search, selection and resize latency on the M0
//! spike's corpora.
//!
//! Run with `cargo run --release -p pinfold-benches --bin scrollback`. Adding
//! `-- --write <dir>` saves the corpora instead, which is how `cargo xtask mem-gate` gets them.

#![allow(clippy::print_stdout, reason = "command-line output")]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use pinfold_core::scrollback::Scrollback;
use pinfold_core::search::{
    CopyStep, Copying, Direction, MAX_QUERY, Position, Search, SearchStep, Selection,
};
use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use pinfold_proto::Frame;

const COLUMNS: usize = 120;
const SCREEN: usize = 50;
const FED_LINES: usize = 12_000;
/// Searches each way, and whole-store copies.
const ROUNDS: usize = 20;
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

/// Calls `take` with a corpus in reads of about 64 KB, as a PTY would.
fn reads(name: &str, mut take: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut read = Vec::new();
    let mut one = Vec::new();
    for number in 0..FED_LINES {
        one.clear();
        line(name, &mut rng, number, &mut one)?;
        one.extend_from_slice(b"\r\n");
        read.extend_from_slice(&one);
        if read.len() >= 64 * 1024 {
            take(&read);
            read.clear();
        }
    }
    take(&read);
    Ok(())
}

fn corpus(name: &str) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reads(name, |read| out.extend_from_slice(read))?;
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

/// A pane that has been fed a corpus, and the rows its store holds. The corpus is never held
/// whole, because a later copy could land in its freed pages and hide what that copy costs.
fn filled(name: &str) -> std::io::Result<(AlacrittyEngine, usize)> {
    let mut engine = AlacrittyEngine::new(SCREEN as u16, COLUMNS as u16);
    let mut replies = Vec::new();
    reads(name, |read| engine.feed(read, &mut replies))?;
    let held = engine.scrollback().lock().map_or(0, |store| store.len());
    Ok((engine, held))
}

/// Returns p50, p99 and the maximum of `times`, which it sorts.
fn percentiles(times: &mut [f64]) -> (f64, f64, f64) {
    times.sort_by(f64::total_cmp);
    let at = |share: usize| times.get(times.len() * share / 100).copied().unwrap_or(0.0);
    (at(50), at(99), times.last().copied().unwrap_or(0.0))
}

/// Runs a search that misses over the whole store, both ways, which is the longest one runs.
/// Returns the time of each step, which is one call on the reactor's thread, and of each search.
fn misses(engine: &AlacrittyEngine, query: &str) -> (Vec<f64>, Vec<f64>) {
    let (mut steps, mut wholes) = (vec![0.0; 64 * 1024], vec![0.0; ROUNDS * 2]);
    steps.clear();
    wholes.clear();
    for round in 0..ROUNDS * 2 {
        let (from, direction) = if round % 2 == 0 {
            (Position::default(), Direction::Forward)
        } else {
            let row = usize::MAX;
            (Position { row, column: 0 }, Direction::Backward)
        };
        let Some(mut search) = Search::new(query, from, direction) else {
            continue;
        };
        let whole = Instant::now();
        loop {
            let start = Instant::now();
            let step = engine.search(&mut search);
            steps.push(start.elapsed().as_secs_f64() * 1e3);
            if step != SearchStep::Running {
                break;
            }
        }
        wholes.push(whole.elapsed().as_secs_f64() * 1e3);
    }
    (steps, wholes)
}

/// Prints a table row from `misses`.
fn print_misses(name: &str, call: &str, held: usize, mut times: (Vec<f64>, Vec<f64>), grown: f64) {
    let calls = times.0.len() / times.1.len().max(1);
    let (p50, p99, most) = percentiles(&mut times.0);
    let (whole, _, _) = percentiles(&mut times.1);
    println!(
        "| {name} | {call} | {held} | {calls} | {p50:.3} | {p99:.3} | {most:.3} | {whole:.2} | | {grown:.3} |"
    );
}

/// The most work a query can make per cell: over rows of `a`, every window compares in full.
fn worst_query() -> std::io::Result<()> {
    let mut engine = AlacrittyEngine::new(SCREEN as u16, COLUMNS as u16);
    let line = "a".repeat(COLUMNS - 1) + "\r\n";
    engine.feed(line.repeat(FED_LINES).as_bytes(), &mut Vec::new());
    let held = engine.scrollback().lock().map_or(0, |store| store.len());
    let query = "a".repeat(MAX_QUERY - 1) + "b";
    print_misses(
        "all a",
        "search, worst query",
        held,
        misses(&engine, &query),
        0.0,
    );
    Ok(())
}

/// Prints table rows for searching a whole store and for copying all of it. Footprint growth
/// leaves out what `bytes()` grew by and adds up from row to row, so "wheel too" is what search
/// and scrolling cost together, and each copy row holds what came before it. Held is the most
/// seen while a copy's text was alive, and left is what stays once it has gone. A copy in steps
/// drops each step's text, as a caller that passes it on would.
fn finds(name: &str) -> std::io::Result<()> {
    let (mut engine, held) = filled(name)?;
    let pid = std::process::id();
    let bytes = |engine: &AlacrittyEngine| {
        let store = engine.scrollback().lock();
        store.map_or(0, |store| store.bytes()) as u64
    };
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    let mut times = vec![0.0; held * 2];
    let (first_bytes, before) = (bytes(&engine), pinfold_sys::footprint(pid)?);
    let grown = |engine: &AlacrittyEngine| -> std::io::Result<f64> {
        let total = pinfold_sys::footprint(pid)?.saturating_sub(before);
        Ok(total.saturating_sub(bytes(engine) - first_bytes) as f64 / 1e6)
    };
    let searches = misses(&engine, "absent-needle");
    let growth = grown(&engine)?;
    print_misses(name, "search", held, searches, growth);
    wheel(&mut engine, &mut frame, &mut times);
    let growth = grown(&engine)?;
    println!("| {name} | wheel too | {held} | | | | | | | {growth:.3} |");

    let selection = Selection {
        anchor: Position::default(),
        focus: Position {
            row: usize::MAX,
            column: u16::MAX,
        },
    };
    // `times` was written by the wheel, so these pages do not count as growth.
    times.clear();
    let mut wholes = vec![0.0; ROUNDS];
    wholes.clear();
    let (mut piece, mut size, mut peak) = (String::new(), 0, 0.0_f64);
    for _ in 0..ROUNDS {
        let mut copy = Copying::new(selection);
        let whole = Instant::now();
        size = 0;
        loop {
            piece.clear();
            let start = Instant::now();
            let step = engine.copy_step(&mut copy, &mut piece);
            times.push(start.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(&piece);
            size += piece.len();
            if step != CopyStep::Running {
                break;
            }
        }
        wholes.push(whole.elapsed().as_secs_f64() * 1e3);
        peak = peak.max(grown(&engine)?);
    }
    drop(piece);
    let call = format!("copy {:.2} MB in steps", size as f64 / 1e6);
    print_copy(
        name,
        &call,
        held,
        (&mut times, &mut wholes),
        peak,
        grown(&engine)?,
    );

    times.clear();
    wholes.clear();
    let (mut size, mut peak) = (0, 0.0_f64);
    for _ in 0..ROUNDS {
        let mut text = String::new();
        let start = Instant::now();
        std::hint::black_box(engine.selection_text(selection, &mut text));
        times.push(start.elapsed().as_secs_f64() * 1e3);
        wholes.push(start.elapsed().as_secs_f64() * 1e3);
        std::hint::black_box(&text);
        size = text.len();
        peak = peak.max(grown(&engine)?);
    }
    let left = grown(&engine)?;
    let call = format!("copy {:.2} MB in one String", size as f64 / 1e6);
    print_copy(name, &call, held, (&mut times, &mut wholes), peak, left);
    Ok(())
}

/// Prints a table row for repeated copies of a whole store.
fn print_copy(
    name: &str,
    call: &str,
    held: usize,
    times: (&mut [f64], &mut [f64]),
    peak: f64,
    left: f64,
) {
    let calls = times.0.len() / times.1.len().max(1);
    let (p50, p99, most) = percentiles(times.0);
    let (whole, _, _) = percentiles(times.1);
    println!(
        "| {name} | {call} | {held} | {calls} | {p50:.3} | {p99:.3} | {most:.3} | {whole:.2} | {peak:.3} | {left:.3} |"
    );
}

/// Moves the view through the whole store and back, timing each notch into `times`.
fn wheel(engine: &mut AlacrittyEngine, frame: &mut Frame, times: &mut Vec<f64>) {
    times.clear();
    for lines in [1, -1] {
        loop {
            let start = Instant::now();
            let moved = engine.scroll(lines);
            engine.frame(frame);
            std::hint::black_box(&*frame);
            if !moved {
                break;
            }
            times.push(start.elapsed().as_secs_f64() * 1e3);
        }
    }
}

/// Prints a table row for one wheel notch: a scroll and the frame it needs, over a pane's whole
/// store and back. Each corpus runs in its own process, so footprint growth is its own.
fn notches(name: &str) -> std::io::Result<()> {
    let (mut engine, held) = filled(name)?;
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    // Written before the first reading, so these pages do not count as growth.
    let mut times = vec![0.0; held * 2];
    let before = pinfold_sys::footprint(std::process::id())?;
    wheel(&mut engine, &mut frame, &mut times);
    let grown = pinfold_sys::footprint(std::process::id())?.saturating_sub(before);
    let (p50, p99, most) = percentiles(&mut times);
    let grown = grown as f64 / 1e6;
    println!("| {name} | {held} | {p50:.3} | {p99:.3} | {most:.3} | {grown:.3} |");
    Ok(())
}

/// Prints a table row for vi mode over a pane's whole store and back: a step past the view's
/// edge, which is what `Request::Reveal` does for `j` and `k`, then the jumps of `gg` and `G`.
fn motions(name: &str) -> std::io::Result<()> {
    let (mut engine, held) = filled(name)?;
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    // Written before the first reading, so these pages do not count as growth.
    let mut times = vec![0.0; held * 2];
    let before = pinfold_sys::footprint(std::process::id())?;
    times.clear();
    for lines in [1, -1] {
        loop {
            let start = Instant::now();
            std::hint::black_box(engine.position(0, 0));
            let moved = engine.scroll(lines);
            engine.frame(&mut frame);
            std::hint::black_box((&frame, engine.view_rows()));
            if !moved {
                break;
            }
            times.push(start.elapsed().as_secs_f64() * 1e3);
        }
    }
    let grown = pinfold_sys::footprint(std::process::id())?.saturating_sub(before);
    let mut jump = |lines| {
        let start = Instant::now();
        engine.scroll(lines);
        engine.frame(&mut frame);
        std::hint::black_box(&frame);
        start.elapsed().as_secs_f64() * 1e3
    };
    let (top, bottom) = (jump(i32::MAX), jump(i32::MIN));
    let (p50, p99, most) = percentiles(&mut times);
    let grown = grown as f64 / 1e6;
    println!(
        "| {name} | {held} | {p50:.3} | {p99:.3} | {most:.3} | {top:.3} | {bottom:.3} | {grown:.3} |"
    );
    Ok(())
}

/// A pane holding one line that wraps across every row of the store, or one of rows of long
/// links that compress well, which is the most a store can make a new width inflate.
fn worst_case(name: &str) -> (AlacrittyEngine, usize) {
    let (columns, bytes) = if name == "one-line" {
        (COLUMNS, b"x".repeat(COLUMNS * 9_000))
    } else {
        let link = "u".repeat(2000);
        let cell = format!("\x1b]8;id=a;{link}\x1b\\x\x1b]8;;\x1b\\");
        (1024, cell.repeat(1024 * 2000).into_bytes())
    };
    let mut engine = AlacrittyEngine::new(SCREEN as u16, columns as u16);
    engine.feed(&bytes, &mut Vec::new());
    let held = engine.scrollback().lock().map_or(0, |store| store.len());
    (engine, held)
}

/// Prints a table row for changing width: a step of a window drag at the live screen, the first
/// read of history after one, a step with the view in history, and a wheel notch once reflowed.
fn resizes(name: &str) -> std::io::Result<()> {
    let (mut engine, held) = if name.starts_with("worst-") {
        worst_case(name.trim_start_matches("worst-"))
    } else {
        filled(name)?
    };
    let (rows, columns) = engine.size();
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    let before = pinfold_sys::footprint(std::process::id())?;
    let width = |step: u16| columns - 1 - step % 40;
    let drag = |engine: &mut AlacrittyEngine, frame: &mut Frame| {
        let mut times: Vec<f64> = (0..200)
            .map(|step| {
                let start = Instant::now();
                engine.resize(rows, width(step));
                engine.frame(frame);
                std::hint::black_box((engine.position(0, 0), engine.view_rows()));
                start.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        percentiles(&mut times)
    };
    let (live_p50, live_p99, _) = drag(&mut engine, &mut frame);
    let start = Instant::now();
    engine.scroll(SCREEN as i32 * 2);
    engine.frame(&mut frame);
    let first = start.elapsed().as_secs_f64() * 1e3;
    let (view_p50, view_p99, _) = drag(&mut engine, &mut frame);
    engine.scroll(i32::MIN);
    engine.resize(rows, columns * 2 / 3);
    let mut times: Vec<f64> = (0..2000)
        .map(|_| {
            let start = Instant::now();
            engine.scroll(1);
            engine.frame(&mut frame);
            start.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    let (notch_p50, notch_p99, _) = percentiles(&mut times);
    let grown = pinfold_sys::footprint(std::process::id())?.saturating_sub(before) as f64 / 1e6;
    println!(
        "| {name} | {held} | {live_p50:.3} | {live_p99:.3} | {first:.3} | {view_p50:.3} | {view_p99:.3} | {notch_p50:.3} | {notch_p99:.3} | {grown:.3} |"
    );
    Ok(())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, dir] = args.as_slice()
        && flag == "--write"
    {
        for name in CORPORA {
            std::fs::write(std::path::Path::new(dir).join(name), corpus(name)?)?;
        }
        return Ok(());
    }
    if let [flag, name] = args.as_slice()
        && flag == "--notches"
    {
        return notches(name);
    }
    if let [flag, name] = args.as_slice()
        && flag == "--motions"
    {
        return motions(name);
    }
    if let [flag, name] = args.as_slice()
        && flag == "--resizes"
    {
        return resizes(name);
    }
    if let [flag] = args.as_slice()
        && flag == "--worst-query"
    {
        return worst_query();
    }
    if let [flag, name] = args.as_slice()
        && flag == "--finds"
    {
        return finds(name);
    }
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
    println!("\n| Corpus | Rows held | Notch p50 ms | p99 ms | Max ms | Footprint growth MB |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for name in CORPORA {
        std::process::Command::new(std::env::current_exe()?)
            .args(["--notches", name])
            .status()?;
    }
    println!(
        "\n| Corpus | Rows held | Vi step p50 ms | p99 ms | Max ms | gg ms | G ms | Footprint growth MB |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- |");
    for name in CORPORA {
        std::process::Command::new(std::env::current_exe()?)
            .args(["--motions", name])
            .status()?;
    }
    println!(
        "\n| Corpus | Rows held | Live step p50 ms | p99 ms | First read ms | View step p50 ms | p99 ms | Notch p50 ms | p99 ms | Footprint growth MB |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let worst = ["worst-one-line", "worst-huge-links"];
    for name in CORPORA.iter().chain(&worst) {
        std::process::Command::new(std::env::current_exe()?)
            .args(["--resizes", name])
            .status()?;
    }
    println!(
        "\n| Corpus | Call | Rows held | Calls | p50 ms | p99 ms | Max ms | Whole ms | Held MB | Left MB |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for name in CORPORA {
        std::process::Command::new(std::env::current_exe()?)
            .args(["--finds", name])
            .status()?;
    }
    std::process::Command::new(std::env::current_exe()?)
        .arg("--worst-query")
        .status()?;
    Ok(())
}
