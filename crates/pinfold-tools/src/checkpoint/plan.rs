//! `pinfold checkpoint plan` (ADR-0050 section 6): what restoring checkpoint `C` over the view
//! checkpoint `N` would do, counted from `git diff-tree --name-status -z` in the store, for the
//! window's `Rewind` prompt. It only reads the store and takes no lock, so a prune that removes
//! either checkpoint makes it fail with "no longer exists", never count part of a diff.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::take::{is_run, private, resolve};
use crate::git;

/// Paths a plan or restore may be limited to, as `checkpoint.restore`'s `paths`.
pub const MAX_PATHS: usize = 1000;
/// Paths the prompt shows, each cut to 256 bytes.
pub(crate) const SHOWN: usize = 10;
const SHOWN_BYTES: usize = 256;
/// The longest line a `pinfold checkpoint` command prints, which the core reads whole.
pub(crate) const LINE: usize = 4096;
const BUF: usize = 64 * 1024;

/// What a restore of `C` over `N` would do. `pinfold checkpoint plan` prints it as
/// `{"restore":R,"delete":D,"bring_back":B,"not_covered":U,"later":L,"complete":C,
/// "paths":[[path,what,later],...],"more":M}`, `more` counting paths not listed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Planned {
    /// Paths both hold that the agent changed.
    pub restore: u64,
    /// Paths only `N` holds, which `C` covered and was complete: the agent added them.
    pub delete: u64,
    /// Paths only `C` holds: the agent deleted them.
    pub bring_back: u64,
    /// Paths only `N` holds that `C` did not cover, or any when `C` is incomplete: left alone.
    pub not_covered: u64,
    /// Of all of them, paths changed since the run's newest `done` or `exit` checkpoint before
    /// `N`, probably by the person: the viewer's `later` mark.
    pub later: u64,
    /// Whether `C` is complete; if not, a restore deletes nothing.
    pub complete: bool,
    /// The first 10: the path, lossily UTF-8 and cut to 256 bytes, what happens to it
    /// (`restore`, `delete`, `bring back` or `not covered`) and whether it is `later`.
    pub paths: Vec<(String, &'static str, bool)>,
    /// Why `C` was taken, one of a take's reasons, or `unknown` for any other message text.
    pub reason: &'static str,
    /// When `C` was taken, Unix seconds, or 0 if its message has none.
    pub time: u64,
}

impl Planned {
    /// Paths in the plan, listed or not.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.restore + self.delete + self.bring_back + self.not_covered
    }
}

pub(crate) fn refname(run: &str, n: u64) -> String {
    format!("refs/pinfold/{run}/{n}")
}

pub(crate) fn gone(run: &str, n: u64) -> io::Error {
    let why = format!("checkpoint {run}/{n} no longer exists");
    io::Error::new(io::ErrorKind::NotFound, why)
}

/// The message of checkpoint `n` of `run`, or `None` if it is not in the store.
pub(crate) fn message(store: &Path, run: &str, n: u64) -> io::Result<Option<serde_json::Value>> {
    let refname = refname(run, n);
    let out = git::store(
        store,
        ["for-each-ref", "--format=%(contents)", &refname],
        b"",
    )?;
    if out.status.is_none_or(|status| !status.success()) || out.cut_short {
        return Err(io::Error::other("git for-each-ref failed"));
    }
    let text = out.stdout.trim_ascii();
    if text.is_empty() {
        return Ok(None);
    }
    let broken = |_| io::Error::new(io::ErrorKind::InvalidData, "a broken checkpoint message");
    serde_json::from_slice(text).map(Some).map_err(broken)
}

pub(crate) fn exists(store: &Path, run: &str, n: u64) -> io::Result<bool> {
    message(store, run, n).map(|message| message.is_some())
}

/// The numbers of `run`'s checkpoints in the store.
pub(crate) fn numbers(store: &Path, run: &str) -> io::Result<Vec<u64>> {
    let prefix = format!("refs/pinfold/{run}/");
    let format = "--format=%(refname:lstrip=3)";
    let out = git::store(store, ["for-each-ref", format, &prefix], b"")?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().filter_map(|n| n.parse().ok()).collect())
}

/// What a restore needs from the checkpoint `C` it restores.
pub(crate) struct Origin {
    complete: bool,
    reason: &'static str,
    time: u64,
    /// More paths were skipped than the message lists.
    more: bool,
    skipped: Vec<String>,
}

impl Origin {
    pub(crate) fn read(store: &Path, run: &str, n: u64) -> io::Result<Self> {
        let message = message(store, run, n)?.ok_or_else(|| gone(run, n))?;
        let skipped = message["skipped"]
            .as_array()
            .map_or_else(Vec::new, |skipped| {
                let path = |pair: &serde_json::Value| pair[0].as_str().map(str::to_owned);
                skipped.iter().filter_map(path).collect()
            });
        // Only a take's own words, so a message an agent forged shows nothing of its own.
        let reason = message["reason"].as_str().unwrap_or_default();
        let reason = super::take::REASONS.iter().find(|known| **known == reason);
        Ok(Self {
            complete: message["complete"] == true,
            reason: reason.copied().unwrap_or("unknown"),
            time: message["time"].as_u64().unwrap_or(0),
            more: message["more"] != false,
            skipped,
        })
    }

    pub(crate) fn complete(&self) -> bool {
        self.complete
    }

    /// Whether `C` covered `path`, which it does not hold: never once it is incomplete or
    /// skipped more paths than it lists, nor when it skipped `path` or a directory above it.
    pub(crate) fn covered(&self, path: &[u8]) -> bool {
        // Lossy as the message is; a collision only covers less.
        let path = String::from_utf8_lossy(path);
        let under = |skipped: &String| {
            path.strip_prefix(skipped.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        self.complete && !self.more && !self.skipped.iter().any(under)
    }
}

/// The paths a plan or restore is limited to, all if `None`.
pub(crate) struct Selection(Option<BTreeSet<Vec<u8>>>);

impl Selection {
    pub(crate) fn new(paths: Option<Vec<Vec<u8>>>) -> io::Result<Self> {
        if paths.as_ref().is_some_and(|paths| paths.len() > MAX_PATHS) {
            let why = format!("at most {MAX_PATHS} paths");
            return Err(io::Error::new(io::ErrorKind::InvalidInput, why));
        }
        Ok(Self(paths.map(|paths| paths.into_iter().collect())))
    }

    /// Whether `path` is chosen, or is above a chosen path, so must go for it to be written.
    pub(crate) fn has(&self, path: &[u8]) -> bool {
        let Some(chosen) = &self.0 else { return true };
        let mut under = path.to_vec();
        under.push(b'/');
        chosen.contains(path)
            || chosen
                .range(under.clone()..)
                .next()
                .is_some_and(|next| next.starts_with(&under))
    }
}

/// The store of the working directory `dir` under `state`, refused as a take refuses it; never
/// made.
pub(crate) fn store_of(state: &Path, dir: &Path) -> io::Result<PathBuf> {
    store_by_key(state, &resolve(dir)?.1)
}

/// The store `key`, 16 lowercase hex digits, under `state`, refused as a take refuses it;
/// never made.
pub(crate) fn store_by_key(state: &Path, key: &str) -> io::Result<PathBuf> {
    let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
    if key.len() != 16 || !key.bytes().all(hex) {
        let bad = format!("not a store key: {key}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, bad));
    }
    let checkpoints = state.join("checkpoints");
    private(&checkpoints)?;
    let store = checkpoints.join(key);
    private(&store)?;
    Ok(store)
}

/// `git <args>`, a `diff-tree`, into a file in the store that is unlinked at once, read back
/// from its start, so a diff of any size costs no memory.
pub(crate) fn diff(store: &Path, args: &[&str]) -> io::Result<BufReader<File>> {
    let name = store.join(format!(".diff-{}", std::process::id()));
    let _ = fs::remove_file(&name);
    let mut options = fs::OpenOptions::new();
    let mut file = options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&name)?;
    fs::remove_file(&name)?;
    let out = git::store_to(store, args, file.try_clone()?)?;
    if out.status.is_none_or(|status| !status.success()) {
        let errors = String::from_utf8_lossy(&out.stderr);
        let first = errors.lines().next().unwrap_or("killed at its time cap");
        return Err(io::Error::other(format!("git diff-tree: {first}")));
    }
    file.rewind()?;
    Ok(BufReader::with_capacity(BUF, file))
}

/// Reads the next two NUL-ended fields of `-z` output into `a` and `b`, without their NULs.
/// Returns false at the end.
pub(crate) fn pair(input: &mut dyn BufRead, a: &mut Vec<u8>, b: &mut Vec<u8>) -> io::Result<bool> {
    a.clear();
    b.clear();
    if input.read_until(0, a)? == 0 {
        return Ok(false);
    }
    input.read_until(0, b)?;
    if a.pop() != Some(0) || b.pop() != Some(0) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a broken diff"));
    }
    Ok(true)
}

/// The run's newest `done` or `exit` checkpoint before `shown`.
pub(crate) fn stopped(store: &Path, run: &str, shown: u64) -> io::Result<Option<u64>> {
    let mut numbers = numbers(store, run)?;
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    for n in numbers.into_iter().filter(|&n| n < shown) {
        let reason = message(store, run, n)?.map(|m| m["reason"].clone());
        if reason
            .as_ref()
            .and_then(|r| r.as_str())
            .is_some_and(|r| r == "done" || r == "exit")
        {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

pub(crate) fn display(path: &[u8]) -> String {
    let mut path = String::from_utf8_lossy(path).into_owned();
    if path.len() > SHOWN_BYTES {
        let end = (0..=SHOWN_BYTES)
            .rev()
            .find(|&i| path.is_char_boundary(i))
            .unwrap_or(0);
        path.truncate(end);
    }
    path
}

/// Drops entries from the end of `line[list]`, adding each to `line[more]`, until `line`
/// fits the [`LINE`] the core reads.
pub(crate) fn fit(line: &mut serde_json::Value, list: &str, more: &str) {
    while line.to_string().len() > LINE {
        let Some(dropped) = line[list].as_array_mut().and_then(Vec::pop) else {
            return;
        };
        drop(dropped);
        line[more] = (line[more].as_u64().unwrap_or(0) + 1).into();
    }
}

/// Plans restoring checkpoint `c` of `run` over the view checkpoint `shown` of the working
/// directory `dir`, whose store is under `state`, limited to `paths` (relative to `dir`, at
/// most 1,000) if given. A path above a chosen one counts as chosen, as it must go for that
/// one to be written.
///
/// # Errors
///
/// Refuses a bad run, more than 1,000 paths and a store a take would refuse;
/// [`io::ErrorKind::NotFound`] with "no longer exists" if either checkpoint is not in the store,
/// a prune having removed it before or while the plan read it. Otherwise a failed git.
///
/// # Examples
///
/// ```no_run
/// use pinfold_tools::checkpoint::plan::plan;
///
/// let (state, work) = ("/path/to/state".as_ref(), "/path/to/work".as_ref());
/// let planned = plan(state, "r1700000000-0", 0, 3, work, None)?;
/// println!("restore {} · delete {} the agent added", planned.restore, planned.delete);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn plan(
    state: &Path,
    run: &str,
    c: u64,
    shown: u64,
    dir: &Path,
    paths: Option<Vec<Vec<u8>>>,
) -> io::Result<Planned> {
    if !is_run(run) {
        let bad = format!("not a run: {run}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, bad));
    }
    let chosen = Selection::new(paths)?;
    let store = store_of(state, dir)?;
    let origin = Origin::read(&store, run, c)?;
    if !exists(&store, run, shown)? {
        return Err(gone(run, shown));
    }
    let (from, to) = (refname(run, c), refname(run, shown));
    let read = || -> io::Result<_> {
        let changes = diff(
            &store,
            &["diff-tree", "-r", "--name-status", "-z", &from, &to],
        )?;
        let since = match stopped(&store, run, shown)? {
            Some(n) => {
                let stopped = refname(run, n);
                Some(diff(
                    &store,
                    &["diff-tree", "-r", "--name-only", "-z", &stopped, &to],
                )?)
            }
            None => None,
        };
        Ok((changes, since))
    };
    let (mut changes, mut since) =
        read().map_err(
            |error| match (exists(&store, run, c), exists(&store, run, shown)) {
                (Ok(false), _) => gone(run, c),
                (_, Ok(false)) => gone(run, shown),
                _ => error,
            },
        )?;

    let mut planned = Planned {
        complete: origin.complete(),
        reason: origin.reason,
        time: origin.time,
        ..Planned::default()
    };
    let (mut status, mut path) = (Vec::new(), Vec::new());
    // `since` is `--name-only`: one path a line, in the same order as `changes`.
    let mut later = Vec::new();
    let mut next_later = |later: &mut Vec<u8>| -> io::Result<bool> {
        later.clear();
        let Some(since) = since.as_mut() else {
            return Ok(false);
        };
        Ok(since.read_until(0, later)? > 0 && later.pop() == Some(0))
    };
    let mut more_later = next_later(&mut later)?;
    while pair(&mut changes, &mut status, &mut path)? {
        if !chosen.has(&path) {
            continue;
        }
        while more_later && later < path {
            more_later = next_later(&mut later)?;
        }
        let is_later = more_later && later == path;
        let what = match status.first() {
            Some(b'A') if origin.covered(&path) => {
                planned.delete += 1;
                "delete"
            }
            Some(b'A') => {
                planned.not_covered += 1;
                "not covered"
            }
            Some(b'D') => {
                planned.bring_back += 1;
                "bring back"
            }
            _ => {
                planned.restore += 1;
                "restore"
            }
        };
        planned.later += u64::from(is_later);
        if planned.paths.len() < SHOWN {
            planned.paths.push((display(&path), what, is_later));
        }
    }
    Ok(planned)
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
