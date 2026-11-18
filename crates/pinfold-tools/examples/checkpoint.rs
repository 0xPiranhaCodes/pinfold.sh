//! A checkpoint take for measuring, with the spike's command line so `measure.sh --real` can
//! run it: `checkpoint <dir> <store> <n>` takes `<dir>` into the store `<store>` (made if
//! missing) at `refs/pinfold/spike/<n>`, on checkpoint `<n> - 1` if the store has it, at
//! background priority, with `min(4, available CPUs)` readers each feeding its own
//! `git fast-import`. It prints one JSON line: counts, wall and walk time in milliseconds, and
//! the take's own peak memory (`footprint`, as `pinfold_sys::peak_footprint` reads it). The
//! store's lock, refusals and metadata are slice 3's `pinfold checkpoint take`.

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pinfold_tools::checkpoint::{Caps, commit, walk};
use pinfold_tools::git;

const BUF: usize = 64 * 1024;

fn main() -> Result<(), Box<dyn Error>> {
    pinfold_sys::background()?;
    let start = Instant::now();
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [dir, store, n] = &args[..] else {
        return Err("usage: checkpoint <dir> <store> <n>".into());
    };
    let n: u64 = n
        .to_str()
        .and_then(|n| n.parse().ok())
        .ok_or("n is a number")?;
    let store = std::path::absolute(store)?;
    if !store.join("HEAD").exists() {
        git::create_store(&store)?;
        fs::create_dir(store.join("stat"))?;
    }
    let list = |n: u64| store.join(format!("stat/spike-{n}"));
    let parent = n.checked_sub(1).filter(|&p| list(p).exists());
    let open = |path: PathBuf| File::open(path).map(|f| BufReader::with_capacity(BUF, f));
    let mut previous = parent.map(|p| open(list(p))).transpose()?;

    let readers = thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let importers = (0..readers)
        .map(|_| git::import(&store))
        .collect::<io::Result<Vec<_>>>()?;
    let mut stat = BufWriter::with_capacity(BUF, File::create(list(n))?);
    let old = previous.as_mut().map(|p| p as &mut dyn io::BufRead);
    let (walked, importers) = walk(Path::new(dir), &Caps::default(), old, &mut stat, importers)?;
    drop(stat);
    // The commit names blobs from every reader's pack, so they must all be in first.
    for import in importers {
        import.finish()?;
    }
    let walk_ms = start.elapsed().as_millis();

    let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let message = format!(
        "{{\"n\":{n},\"files\":{},\"complete\":{}}}",
        walked.files, walked.complete
    );
    let mut import = git::import(&store)?;
    let mut previous = parent.map(|p| open(list(p))).transpose()?;
    let from = parent.map(|p| format!("refs/pinfold/spike/{p}"));
    let parent = from
        .as_deref()
        .zip(previous.as_mut().map(|p| p as &mut dyn io::BufRead));
    let refname = format!("refs/pinfold/spike/{n}");
    commit(
        &mut import,
        &refname,
        &message,
        time,
        &mut open(list(n))?,
        parent,
    )?;
    import.finish()?;

    let footprint = pinfold_sys::peak_footprint()?;
    writeln!(
        io::stdout(),
        "{{\"n\":{n},\"ms\":{},\"walk_ms\":{walk_ms},\"readers\":{readers},\"files\":{},\"read\":{},\"reused\":{},\"new_bytes\":{},\"complete\":{},\"not_covered\":{},\"rss\":{footprint},\"footprint\":{footprint}}}",
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
