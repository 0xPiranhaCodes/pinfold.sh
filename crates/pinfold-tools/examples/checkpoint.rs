//! Measures the checkpoint walk alone: `checkpoint <dir> <stat> [<previous stat>]` walks `<dir>`
//! with `min(4, available CPUs)` readers whose blobs go nowhere, writes the stat list to
//! `<stat>`, and prints one JSON line of counts and wall time. Run it under `/usr/bin/time -l`
//! (macOS) or `-v` (Linux) for the walk's peak memory.

use std::error::Error;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::thread;
use std::time::Instant;

use pinfold_tools::checkpoint::{Caps, walk};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [dir, stat, previous @ ..] = &args[..] else {
        return Err("usage: checkpoint <dir> <stat> [<previous stat>]".into());
    };
    let start = Instant::now();
    let readers = thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let mut previous = match previous.first() {
        Some(path) => Some(BufReader::with_capacity(64 * 1024, File::open(path)?)),
        None => None,
    };
    let mut out = BufWriter::with_capacity(64 * 1024, File::create(stat)?);
    let previous = previous.as_mut().map(|p| p as &mut dyn io::BufRead);
    let sinks = (0..readers).map(|_| io::sink()).collect();
    let (walked, _) = walk(Path::new(dir), &Caps::default(), previous, &mut out, sinks)?;
    writeln!(
        io::stdout(),
        "{{\"ms\":{},\"readers\":{readers},\"files\":{},\"read\":{},\"reused\":{},\"new_bytes\":{},\"complete\":{},\"uncovered\":{}}}",
        start.elapsed().as_millis(),
        walked.files,
        walked.read,
        walked.reused,
        walked.new_bytes,
        walked.complete,
        walked.uncovered,
    )?;
    Ok(())
}
