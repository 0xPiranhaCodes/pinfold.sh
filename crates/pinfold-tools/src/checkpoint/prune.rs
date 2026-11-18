//! `pinfold checkpoint prune` (ADR-0050 section 5): drops checkpoints past their run's count or
//! their age, then, while all stores together are over their cap, closed runs' oldest first and
//! then live runs' but for their launch and newest; repacks a store only when that freed space
//! or it is past 32 packs or 512 MiB written since its last repack.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::take::{is_run, lock, private};
use crate::git;

/// Written by a prune that cannot bring all stores under the cap without a live run's launch
/// checkpoint; while it is there, every take is refused.
pub(crate) const FULL: &str = "full";
/// Pack bytes in a store just after its last repack.
const REPACKED: &str = "repacked";
const MAX_PACKS: usize = 32;
const MAX_SINCE_REPACK: u64 = 512 << 20;

/// When checkpoints go (`[rewind]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// All stores together, `max_store_mib`, in bytes.
    pub max_store_bytes: u64,
    /// Older checkpoints go, `keep_days`.
    pub keep_days: u64,
    /// Checkpoints a run keeps, `keep_per_run`.
    pub keep_per_run: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_store_bytes: 2048 << 20,
            keep_days: 14,
            keep_per_run: 20,
        }
    }
}

/// What a prune did. `pinfold checkpoint prune` prints it as
/// `{"deleted":D,"repacked":R,"bytes":B,"full":F,"busy":L}`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Checkpoints deleted.
    pub deleted: u64,
    /// Stores repacked.
    pub repacked: u64,
    /// Bytes all stores hold afterwards.
    pub bytes: u64,
    /// Still over the cap with only live runs' launch and newest checkpoints left.
    pub full: bool,
    /// Stores left alone as a take or another prune held their lock.
    pub busy: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Checkpoint {
    run: String,
    n: u64,
    time: u64,
}

struct Store {
    path: PathBuf,
    /// Held while this prune works in it; `None` if a take holds it, so it is only counted.
    lock: Option<File>,
    checkpoints: Vec<Checkpoint>,
    bytes: u64,
    freed: bool,
}

/// Bytes of the files under `path`, links not followed.
fn du(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map_or(0, |entries| {
        entries.flatten().map(|entry| du(&entry.path())).sum()
    })
}

fn checked(output: &git::Output, what: &str) -> io::Result<()> {
    match output.status {
        Some(status) if status.success() => Ok(()),
        _ => Err(io::Error::other(format!(
            "git {what}: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("failed")
        ))),
    }
}

fn listed(store: &Path) -> io::Result<Vec<Checkpoint>> {
    let format = "--format=%(refname) %(committerdate:unix)";
    let refs = git::store(store, ["for-each-ref", format, "refs/pinfold/"], b"")?;
    checked(&refs, "for-each-ref")?;
    let text = String::from_utf8_lossy(&refs.stdout);
    Ok(text
        .lines()
        .filter_map(|line| {
            let (name, time) = line.split_once(' ')?;
            let (run, n) = name.strip_prefix("refs/pinfold/")?.split_once('/')?;
            Some(Checkpoint {
                run: run.to_owned(),
                n: n.parse().ok()?,
                time: time.parse().ok()?,
            })
        })
        .collect())
}

impl Store {
    fn delete(&mut self, gone: &Checkpoint) -> io::Result<()> {
        let refname = format!("refs/pinfold/{}/{}", gone.run, gone.n);
        checked(
            &git::store(&self.path, ["update-ref", "-d", &refname], b"")?,
            "update-ref",
        )?;
        let list = self.path.join(format!("stat/{}-{}", gone.run, gone.n));
        match fs::remove_file(list) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        self.checkpoints.retain(|c| c != gone);
        self.freed = true;
        Ok(())
    }

    /// Drops stat lists with no checkpoint, left by a take that was killed.
    fn drop_orphans(&self) -> io::Result<()> {
        let names: HashSet<String> = self
            .checkpoints
            .iter()
            .map(|c| format!("{}-{}", c.run, c.n))
            .collect();
        for entry in fs::read_dir(self.path.join("stat"))?.flatten() {
            if !names.contains(entry.file_name().to_string_lossy().as_ref()) {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// What the store would hold once repacked: the objects its checkpoints reach, and its
    /// stat lists.
    fn projected(&self) -> io::Result<u64> {
        let args = ["rev-list", "--objects", "--all", "--disk-usage"];
        let used = git::store(&self.path, args, b"")?;
        checked(&used, "rev-list")?;
        let bytes = String::from_utf8_lossy(&used.stdout).trim().parse::<u64>();
        let bytes = bytes.map_err(|_| io::Error::other("git rev-list: no byte count"))?;
        Ok(bytes + du(&self.path.join("stat")))
    }

    fn packs(&self) -> (usize, u64) {
        let packs: Vec<u64> = fs::read_dir(self.path.join("objects/pack"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
                    .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                    .collect()
            })
            .unwrap_or_default();
        (packs.len(), packs.iter().sum())
    }

    fn repack(&mut self) -> io::Result<()> {
        checked(
            &git::store(&self.path, ["repack", "-a", "-d", "-q"], b"")?,
            "repack",
        )?;
        checked(&git::store(&self.path, ["prune"], b"")?, "prune")?;
        fs::write(self.path.join(REPACKED), self.packs().1.to_string())?;
        self.bytes = du(&self.path);
        Ok(())
    }
}

/// Whether a store with `packs` packs, `since` bytes of them written after its last repack,
/// is repacked though nothing was freed.
fn past_threshold(packs: usize, since: u64) -> bool {
    packs > MAX_PACKS || since > MAX_SINCE_REPACK
}

/// Prunes every store under `<state>/checkpoints`, as of Unix time `now`, keeping what `live`,
/// the runs still going, need. A store a take holds is counted but left alone. Writes or
/// removes `<state>/checkpoints/full`, which refuses takes while the cap cannot be met.
///
/// # Errors
///
/// Refuses a `checkpoints` directory as a take does; otherwise a failed git or file call.
///
/// # Examples
///
/// ```no_run
/// use pinfold_tools::checkpoint::prune::{prune, Limits};
///
/// let state = "/path/to/state".as_ref();
/// let pruned = prune(state, &["r1700000000-0"], &Limits::default(), 1_700_000_000)?;
/// assert!(!pruned.full);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn prune(state: &Path, live: &[&str], limits: &Limits, now: u64) -> io::Result<Pruned> {
    let checkpoints = state.join("checkpoints");
    if !checkpoints.exists() {
        return Ok(Pruned::default());
    }
    private(&checkpoints)?;
    let mut pruned = Pruned::default();
    let mut stores = Vec::new();
    for entry in fs::read_dir(&checkpoints)?.flatten() {
        let name = entry.file_name();
        let key = name.to_string_lossy();
        let path = entry.path();
        if key.len() != 16 || !key.bytes().all(|c| c.is_ascii_hexdigit()) || private(&path).is_err()
        {
            continue;
        }
        let lock = match lock(&path) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                pruned.busy += 1;
                None
            }
            Err(error) => return Err(error),
        };
        let checkpoints = match lock {
            Some(_) => listed(&path)?,
            None => Vec::new(),
        };
        let bytes = du(&path);
        stores.push(Store {
            path,
            lock,
            checkpoints,
            bytes,
            freed: false,
        });
    }

    let expired = |c: &Checkpoint| c.time.saturating_add(limits.keep_days * 86_400) < now;
    for store in stores.iter_mut().filter(|s| s.lock.is_some()) {
        let mut runs: BTreeMap<String, Vec<Checkpoint>> = BTreeMap::new();
        for c in &store.checkpoints {
            runs.entry(c.run.clone()).or_default().push(c.clone());
        }
        for (run, mut list) in runs {
            list.sort_by_key(|c| c.n);
            let after_launch: Vec<_> = list.iter().filter(|c| c.n != 0).collect();
            let over = list.len().saturating_sub(limits.keep_per_run);
            let mut gone: Vec<Checkpoint> = after_launch.into_iter().take(over).cloned().collect();
            let kept = |c: &Checkpoint| c.n == 0 && live.contains(&run.as_str());
            for c in list.iter().filter(|c| expired(c) && !kept(c)) {
                if !gone.contains(c) {
                    gone.push(c.clone());
                }
            }
            for c in &gone {
                store.delete(c)?;
                pruned.deleted += 1;
            }
        }
        store.drop_orphans()?;
        if store.freed {
            checked(&git::store(&store.path, ["prune"], b"")?, "prune")?;
        }
    }

    // Over the cap: closed runs oldest first, then live runs' but for their launch and newest.
    let mut total: u64 = stores.iter().map(|s| s.bytes).sum();
    let mut order: Vec<(bool, u64, usize, Checkpoint)> = Vec::new();
    for (at, store) in stores.iter().enumerate().filter(|(_, s)| s.lock.is_some()) {
        for c in &store.checkpoints {
            let alive = live.contains(&c.run.as_str());
            let newest = store
                .checkpoints
                .iter()
                .filter(|o| o.run == c.run)
                .all(|o| o.n <= c.n);
            if !alive || (c.n != 0 && !newest) {
                order.push((alive, c.time, at, c.clone()));
            }
        }
    }
    order.sort_by_key(|(alive, time, _, c)| (*alive, *time, c.n));
    let mut capped = HashSet::new();
    let mut order = order.into_iter();
    while total > limits.max_store_bytes {
        let Some((_, _, at, c)) = order.next() else {
            break;
        };
        let store = &mut stores[at];
        store.delete(&c)?;
        pruned.deleted += 1;
        let projected = store.projected()?;
        total = total - store.bytes + projected;
        store.bytes = projected;
        capped.insert(at);
    }
    for (at, store) in stores.iter_mut().enumerate() {
        if store.lock.is_none() {
            continue;
        }
        let (packs, bytes) = store.packs();
        let last = fs::read_to_string(store.path.join(REPACKED)).ok();
        let since = bytes.saturating_sub(last.and_then(|b| b.trim().parse().ok()).unwrap_or(0));
        if capped.contains(&at) || past_threshold(packs, since) {
            store.repack()?;
            pruned.repacked += 1;
        }
    }
    pruned.bytes = stores.iter().map(|s| s.bytes).sum();
    pruned.full = pruned.bytes > limits.max_store_bytes;
    let full = checkpoints.join(FULL);
    if pruned.full {
        let why = format!(
            "over max_store_mib: {} MiB of {} MiB is live runs' launch and newest checkpoints",
            pruned.bytes >> 20,
            limits.max_store_bytes >> 20
        );
        fs::write(full, why)?;
    } else if let Err(error) = fs::remove_file(full)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(error);
    }
    Ok(pruned)
}

/// Parses `[--live <run>]... [--max-store-mib N] [--keep-days N] [--keep-per-run N]`.
pub(crate) fn parse<'a>(mut words: &[&'a str]) -> Option<(Vec<&'a str>, Limits)> {
    let (mut live, mut limits) = (Vec::new(), Limits::default());
    while let [flag, value, rest @ ..] = words {
        let number = value.parse::<u64>().ok();
        match *flag {
            "--live" if is_run(value) => live.push(*value),
            "--max-store-mib" => limits.max_store_bytes = number?.checked_mul(1 << 20)?,
            "--keep-days" => limits.keep_days = number?,
            "--keep-per-run" => limits.keep_per_run = usize::try_from(number?).ok()?,
            _ => return None,
        }
        words = rest;
    }
    words.is_empty().then_some((live, limits))
}

#[cfg(test)]
#[path = "prune_tests.rs"]
mod tests;
