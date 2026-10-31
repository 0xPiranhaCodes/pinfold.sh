//! M0 spike for bet 3. See README.md.
//!
//! Each mode runs in a fresh process so that heap and footprint numbers do not mix.

mod store;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use store::Store;

const COLUMNS: usize = 120;
const SCREEN: usize = 50;
const HISTORY: usize = 10_000;
const FED_LINES: usize = 12_000;
const SCROLL_SAMPLES: usize = 2_000;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// PSS on Linux and phys_footprint on macOS, in bytes, as ADR-0008 requires.
#[cfg(target_os = "linux")]
fn footprint() -> usize {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let line = text.lines().find(|line| line.starts_with("Pss:")).unwrap_or("Pss: 0 kB");
    line.split_whitespace().nth(1).and_then(|kb| kb.parse().ok()).unwrap_or(0) * 1024
}

#[cfg(target_os = "macos")]
fn footprint() -> usize {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, (&raw mut info).cast())
    };
    if result == 0 { info.ri_phys_footprint as usize } else { 0 }
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
}

const WORDS: [&str; 16] = [
    "compile", "error", "warning", "finished", "request", "response", "handler", "config",
    "thread", "buffer", "session", "update", "failed", "passed", "module", "cache",
];

/// Deterministic terminal output. `dense` and `random` are deliberate worst cases.
fn corpus(name: &str, rng: &mut Rng, line: usize, out: &mut Vec<u8>) {
    match name {
        "build-log" => {
            let word = WORDS[rng.below(16)];
            let colour = 31 + rng.below(6);
            write!(out, "\x1b[1;{colour}m{word:>12}\x1b[0m crate-{} v0.{}.{} ", rng.below(40), rng.below(9), rng.below(30)).ok();
            write!(out, "(/home/dev/project/crates/{} in {}.{:02}s)", WORDS[rng.below(16)], rng.below(9), rng.below(99)).ok();
        }
        "plain-log" => {
            write!(out, "2026-10-31T{:02}:{:02}:{:02}.{:03}Z INFO req={:016x} ", line / 3600 % 24, line / 60 % 60, line % 60, rng.below(1000), rng.next()).ok();
            write!(out, "{} {} status={} ms={}", WORDS[rng.below(16)], WORDS[rng.below(16)], 200 + rng.below(4) * 100, rng.below(900)).ok();
        }
        "git-diff" => {
            let (mark, colour) = [("+", 32), ("-", 31), (" ", 39)][rng.below(3)];
            write!(out, "\x1b[{colour}m{mark}    let {} = self.{}(&{}, {});\x1b[0m", WORDS[rng.below(16)], WORDS[rng.below(16)], WORDS[rng.below(16)], rng.below(4096)).ok();
        }
        "dense" => {
            while out.len() < COLUMNS - 12 {
                write!(out, "{} ", WORDS[rng.below(16)]).ok();
            }
        }
        "random" => {
            for _ in 0..COLUMNS - 1 {
                out.push(b'!' + rng.below(90) as u8);
            }
        }
        "truecolor" => {
            for _ in 0..(COLUMNS - 1) {
                write!(out, "\x1b[38;2;{};{};{}m{}", rng.below(256), rng.below(256), rng.below(256), (b'a' + rng.below(26) as u8) as char).ok();
            }
            out.extend_from_slice(b"\x1b[0m");
        }
        "cjk-links" => {
            write!(out, "\x1b]8;id=l{};https://example.com/{}\x1b\\link\x1b]8;;\x1b\\ \x1b[4:3m\x1b[58;5;{}m下線\x1b[0m e\u{301} ", line % 5, line % 7, rng.below(256)).ok();
            for _ in 0..30 {
                out.extend_from_slice(["漢", "字", "表", "示"][rng.below(4)].as_bytes());
            }
        }
        other => panic!("unknown corpus {other}"),
    }
}

const CORPORA: [&str; 7] = ["build-log", "plain-log", "git-diff", "dense", "random", "truecolor", "cjk-links"];

fn new_term(history: usize) -> Term<VoidListener> {
    let config = Config { scrolling_history: history, ..Config::default() };
    Term::new(config, &TermSize::new(COLUMNS, SCREEN), VoidListener)
}

/// Feeds the corpus, handing each row that leaves the grid to `store` if there is one.
fn feed(name: &str, term: &mut Term<VoidListener>, mut store: Option<&mut Store>) {
    let mut parser: Processor = Processor::new();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut line = Vec::with_capacity(4096);
    for number in 0..FED_LINES {
        line.clear();
        corpus(name, &mut rng, number, &mut line);
        line.extend_from_slice(b"\r\n");
        parser.advance(term, &line);
        match store.as_deref_mut() {
            Some(store) => term.grid_mut().evicted.drain(..).for_each(|row| store.push(&row)),
            None => term.grid_mut().evicted.clear(),
        }
    }
}

fn run_tiered(name: &str) -> (Term<VoidListener>, Store, f64) {
    let mut term = new_term(0);
    let mut store = Store::default();
    let start = Instant::now();
    feed(name, &mut term, Some(&mut store));
    (term, store, start.elapsed().as_secs_f64())
}

fn measure(mode: &str, name: &str) {
    let before_heap = LIVE.load(Ordering::Relaxed);
    let before_foot = footprint();
    match mode {
        "baseline" => {
            let mut term = new_term(0);
            feed(name, &mut term, None);
            term.grid_mut().evicted.clear();
            report(mode, name, before_heap, before_foot, 0.0, 0.0, 0.0, 0);
        }
        "stock" => {
            let mut term = new_term(HISTORY);
            let start = Instant::now();
            feed(name, &mut term, None);
            let seconds = start.elapsed().as_secs_f64();
            report(mode, name, before_heap, before_foot, seconds, 0.0, 0.0, term.history_size());
        }
        _ => {
            let (_term, store, seconds) = run_tiered(name);
            let settled_heap = LIVE.load(Ordering::Relaxed) - before_heap;
            let settled_foot = footprint().saturating_sub(before_foot);
            let mut rng = Rng(0x1234_5678_9abc_def1);
            let cold = store.len() - store::HOT_LINES;
            let mut samples = Vec::with_capacity(SCROLL_SAMPLES);
            for _ in 0..SCROLL_SAMPLES {
                let first = rng.below(cold);
                let start = Instant::now();
                let rows = store.rows(first, SCREEN);
                samples.push(start.elapsed().as_secs_f64() * 1e3);
                assert_eq!(rows.len(), SCREEN);
            }
            samples.sort_by(f64::total_cmp);
            let p99 = samples[SCROLL_SAMPLES * 99 / 100];
            let max = samples[SCROLL_SAMPLES - 1];
            println!("{mode} {name} {settled_heap} {settled_foot} {seconds:.4} {p99:.3} {max:.3} {}", store.len());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report(mode: &str, name: &str, heap: usize, foot: usize, seconds: f64, p99: f64, max: f64, lines: usize) {
    let live = LIVE.load(Ordering::Relaxed) - heap;
    let grown = footprint().saturating_sub(foot);
    println!("{mode} {name} {live} {grown} {seconds:.4} {p99:.3} {max:.3} {lines}");
}

fn normalized(cells: impl Iterator<Item = Cell>) -> Vec<Cell> {
    let mut row: Vec<Cell> = cells.collect();
    while row.last().is_some_and(|cell| *cell == Cell::default()) {
        row.pop();
    }
    row
}

/// Proves the store holds exactly what a stock grid holds, cell for cell.
fn verify(name: &str) {
    let mut stock = new_term(HISTORY);
    feed(name, &mut stock, None);
    let (tiered, store, _) = run_tiered(name);
    let tiered_grid = tiered.grid();
    let grid = stock.grid();
    let lines = store.len().min(grid.history_size());
    let stored = store.rows(store.len() - lines, lines);
    let mut mismatched = 0;
    for (offset, row) in stored.iter().enumerate() {
        let line = Line(offset as i32 - lines as i32);
        let want = normalized((0..COLUMNS).map(|column| grid[line][Column(column)].clone()));
        if normalized(row.iter().cloned()) != want {
            mismatched += 1;
        }
    }
    for screen in 0..SCREEN as i32 {
        let want = normalized((0..COLUMNS).map(|c| grid[Line(screen)][Column(c)].clone()));
        let got = normalized((0..COLUMNS).map(|c| tiered_grid[Line(screen)][Column(c)].clone()));
        if want != got {
            mismatched += 1;
        }
    }
    println!("verify {name} {lines} {mismatched}");
    assert!(lines >= HISTORY - store::BLOCK_LINES, "too few lines compared: {lines}");
}

fn capture(args: &[&str]) -> String {
    let exe = std::env::current_exe().expect("exe");
    let output = Command::new(exe).args(args).output().expect("child");
    assert!(output.status.success(), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn row(mode: &str, name: &str) -> Vec<f64> {
    capture(&[mode, name]).split_whitespace().skip(2).filter_map(|value| value.parse().ok()).collect()
}

fn mb(bytes: f64) -> f64 {
    bytes / 1e6
}

fn fed_bytes(name: &str) -> usize {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut line = Vec::with_capacity(4096);
    (0..FED_LINES)
        .map(|number| {
            line.clear();
            corpus(name, &mut rng, number, &mut line);
            line.len() + 2
        })
        .sum()
}

fn all() {
    println!("# Platform: {} {}\n", std::env::consts::OS, std::env::consts::ARCH);
    println!("{COLUMNS} columns, {SCREEN}-row screen, {FED_LINES} lines fed, {HISTORY} kept. Each size is growth in MB (10^6 bytes) over the same corpus fed with no history, read before scroll sampling. Footprint is PSS on Linux and phys_footprint on macOS. Throughput excludes rendering.\n");
    println!("| Corpus | Stock heap | Tiered heap | Tiered footprint | Reduction | Scroll p99 ms | Scroll max ms | Stock MB/s | Tiered MB/s | Mismatched rows | ≤ 1 MB heap | ≤ 1 MB footprint |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut scroll_ok = true;
    let mut exact = true;
    for name in CORPORA {
        let base = row("baseline", name);
        let stock = row("stock", name);
        let tiered = row("tiered", name);
        let check: Vec<String> = capture(&["verify", name]).split_whitespace().map(str::to_owned).collect();
        let bad: usize = check[3].parse().expect("count");
        let heap = mb(tiered[0] - base[0]);
        let foot = mb((tiered[1] - base[1]).max(0.0));
        let size = mb(fed_bytes(name) as f64);
        scroll_ok &= tiered[4] < 5.0;
        exact &= bad == 0;
        println!(
            "| {name} | {:.2} | {heap:.2} | {foot:.2} | {:.1}x | {:.2} | {:.2} | {:.0} | {:.0} | {bad} of {} | {} | {} |",
            mb(stock[0] - base[0]), (stock[0] - base[0]) / (tiered[0] - base[0]), tiered[3], tiered[4],
            size / stock[2], size / tiered[2], check[2],
            if heap <= 1.0 { "yes" } else { "**no**" }, if foot <= 1.0 { "yes" } else { "**no**" },
        );
    }
    println!("\nScroll under 5 ms on every corpus: {scroll_ok}. Every row exact on every corpus: {exact}.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("verify") => verify(&args[2]),
        Some(mode @ ("baseline" | "stock" | "tiered")) => measure(mode, &args[2]),
        _ => all(),
    }
}
