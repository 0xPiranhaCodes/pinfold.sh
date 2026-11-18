//! `pinfold checkpoint take` (ADR-0050 sections 1 and 2): the store of a working directory,
//! made or refused, its lock, and one checkpoint of the directory into it.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::thread;

use pinfold_sys::at;

use super::{Caps, Why, commit, io_error, walk};
use crate::git;

const BUF: usize = 64 * 1024;
/// Held with `flock` for as long as a take or a prune works in the store, so the kernel drops
/// it when that process dies, killed at `wait_s` included (ADR-0050, amendment of slice 3).
pub(crate) const LOCK: &str = "pinfold.lock";
/// Why a checkpoint is taken (ADR-0050 section 1).
const REASONS: [&str; 6] = ["launch", "done", "exit", "api", "view", "restore"];

/// What a take wrote. Its JSON line, as `pinfold checkpoint take` prints it, is
/// `{"checkpoint":"<run>/<n>","store":"<key>","files":F,"new_bytes":B,"complete":C,"changed":D}`.
#[derive(Debug, PartialEq, Eq)]
pub struct Taken {
    /// `<run>/<n>`, under `refs/pinfold/` in the store.
    pub checkpoint: String,
    /// The store's key, its directory's name under `<state>/checkpoints`.
    pub store: String,
    /// Files and links covered.
    pub files: u64,
    /// Bytes read from files whose stat changed.
    pub new_bytes: u64,
    /// False if a cap stopped the walk; a restore from it deletes nothing.
    pub complete: bool,
    /// Paths that differ from the run's launch checkpoint, 0 without one.
    pub changed: u64,
}

fn refused(why: String) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why)
}

/// Whether `run` is a run id, `r<Unix seconds>-<n>`.
pub(crate) fn is_run(run: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit());
    run.strip_prefix('r')
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(start, n)| digits(start) && digits(n))
}

/// Refuses, never repairs, a directory PinFold keeps checkpoints in that is a link, not a
/// directory, another user's or open to group or others.
pub(crate) fn private(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let why = if meta.file_type().is_symlink() {
        "is a symbolic link"
    } else if !meta.is_dir() {
        "is not a directory"
    } else if meta.uid() != pinfold_sys::effective_uid() {
        "is another user's"
    } else if meta.mode() & 0o077 != 0 {
        "is open to group or others"
    } else {
        return Ok(());
    };
    Err(refused(format!("{} {why}", path.display())))
}

/// The resolved working directory and its store's key: the first 16 hex digits of the SHA-256
/// of the resolved path, as ADR-0039's shadow. A relative path or a link is refused.
pub(crate) fn resolve(dir: &Path) -> io::Result<(PathBuf, String)> {
    if !dir.is_absolute() {
        return Err(refused(format!("{} is not absolute", dir.display())));
    }
    if fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err(refused(format!("{} is a symbolic link", dir.display())));
    }
    let resolved = dir.canonicalize()?;
    let mut key = pinfold_api::trust::sha256(resolved.as_os_str().as_bytes());
    key.truncate(16);
    Ok((resolved, key))
}

/// The store `key` under `<state>/checkpoints`, made if missing: a bare repository, mode 0700,
/// with its `stat/` and lock, built beside it and renamed into place so no take sees it half
/// made. Refused as [`private`] refuses.
fn open_store(state: &Path, key: &str) -> io::Result<PathBuf> {
    let checkpoints = state.join("checkpoints");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&checkpoints)?;
    private(&checkpoints)?;
    let store = checkpoints.join(key);
    if let Err(error) = fs::symlink_metadata(&store) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
        let new = checkpoints.join(format!(".{key}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&new);
        let made = git::create_store(&new)
            .and_then(|()| fs::DirBuilder::new().mode(0o700).create(new.join("stat")))
            .and_then(|()| {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true).mode(0o600);
                options.open(new.join(LOCK)).map(drop)
            })
            .and_then(|()| fs::rename(&new, &store));
        if made.is_err() {
            let _ = fs::remove_dir_all(&new);
        }
        // Another take may have made it first, which is as good.
        if let Err(error) = made
            && !store.exists()
        {
            return Err(error);
        }
    }
    private(&store)?;
    Ok(store)
}

/// Takes the store's lock, or refuses at once if a take or prune holds it.
pub(crate) fn lock(store: &Path) -> io::Result<File> {
    let dir = at::open_dir(store).map_err(io_error)?;
    let name = OsStr::new(LOCK);
    let file = match at::open_file_at(dir.as_fd(), name) {
        Err(at::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            at::create_at(dir.as_fd(), name).or_else(|_| at::open_file_at(dir.as_fd(), name))
        }
        other => other,
    }
    .map_err(io_error)?;
    if !file.metadata()?.is_file() {
        return Err(refused(format!(
            "{LOCK} in {} is not a file",
            store.display()
        )));
    }
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            "another checkpoint is being taken or pruned in this store",
        ),
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

/// The newest checkpoint in `store` with a stat list, as `<run>-<n>` and its tree id: the one
/// a take merges with, whichever run it is from.
fn newest(store: &Path) -> io::Result<Option<(String, String)>> {
    let format = "--format=%(refname) %(tree)";
    let refs = git::store(
        store,
        [
            "for-each-ref",
            "--sort=-committerdate",
            format,
            "refs/pinfold/",
        ],
        b"",
    )?;
    let listed = String::from_utf8_lossy(&refs.stdout);
    Ok(listed.lines().find_map(|line| {
        let (name, tree) = line.split_once(' ')?;
        let (run, n) = name.strip_prefix("refs/pinfold/")?.split_once('/')?;
        let list = format!("{run}-{n}");
        store
            .join("stat")
            .join(&list)
            .is_file()
            .then(|| (list, tree.to_owned()))
    }))
}

fn word(why: Why) -> &'static str {
    match why {
        Why::Git => "git",
        Why::Nested => "nested",
        Why::Ignored => "ignored",
        Why::Large => "large",
        Why::HardLink => "hard-link",
        Why::Special => "special",
        Why::Changed => "changed",
        Why::Unreadable => "unreadable",
        Why::Deep => "deep",
    }
}

/// Takes checkpoint `n` of `run` for `reason` of the working directory `dir` into its store
/// under `state`, at Unix time `time`, holding the store's lock throughout. The commit message
/// is one JSON object: `run`, `n`, `reason`, `time`, `files`, `new_bytes`, `complete`,
/// `skipped` (at most 1,000 `[path, why]` pairs, the path lossily UTF-8) and `more`, true if
/// more were skipped than listed.
///
/// # Errors
///
/// Refuses a relative or linked `dir`, a store or `checkpoints` directory that is a link, not a
/// directory, another user's or open to others, a store whose lock another process holds
/// ([`io::ErrorKind::WouldBlock`]), every take while the last prune could not meet
/// `max_store_mib` (`over max_store_mib`), a bad run, `n` or reason, and a checkpoint that
/// exists.
/// Otherwise a failed walk or git.
///
/// # Examples
///
/// ```no_run
/// use pinfold_tools::checkpoint::take::take;
///
/// let state = "/path/to/state".as_ref();
/// let taken = take(state, "r1700000000-0", 0, "launch", "/path/to/work".as_ref(), 1_700_000_000)?;
/// assert_eq!(taken.checkpoint, "r1700000000-0/0");
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn take(
    state: &Path,
    run: &str,
    n: u64,
    reason: &str,
    dir: &Path,
    time: u64,
) -> io::Result<Taken> {
    if !is_run(run) || !REASONS.contains(&reason) {
        let bad = format!("not a run and reason: {run} {reason}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, bad));
    }
    let (dir, key) = resolve(dir)?;
    let store = open_store(state, &key)?;
    let _lock = lock(&store)?;
    take_in(state, &store, &dir, run, n, reason, time, &Caps::default())
}

/// [`take`] of the resolved `dir` into its `store`, whose lock the caller holds, as a restore
/// holds it across its restore checkpoint and its writes.
#[expect(clippy::too_many_arguments, reason = "take's, with the store and caps")]
pub(crate) fn take_in(
    state: &Path,
    store: &Path,
    dir: &Path,
    run: &str,
    n: u64,
    reason: &str,
    time: u64,
    caps: &Caps,
) -> io::Result<Taken> {
    let key = store
        .file_name()
        .map(|key| key.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Ok(why) = fs::read_to_string(state.join("checkpoints").join(super::prune::FULL)) {
        return Err(refused(why));
    }
    let refname = format!("refs/pinfold/{run}/{n}");
    let exists = git::store(store, ["for-each-ref", "--count=1", &refname], b"")?;
    if !exists.stdout.is_empty() {
        let taken = format!("checkpoint {run}/{n} exists");
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, taken));
    }
    let base = newest(store)?;
    let open = |list: &str| {
        File::open(store.join("stat").join(list)).map(|f| BufReader::with_capacity(BUF, f))
    };
    let list = store.join(format!("stat/{run}-{n}"));

    let readers = thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let importers = (0..readers)
        .map(|_| git::import(store))
        .collect::<io::Result<Vec<_>>>()?;
    let mut previous = base.as_ref().map(|(list, _)| open(list)).transpose()?;
    let mut stat = BufWriter::with_capacity(BUF, File::create(&list)?);
    let old = previous.as_mut().map(|p| p as &mut dyn BufRead);
    let (walked, importers) = walk(dir, caps, old, &mut stat, importers)?;
    stat.into_inner().map_err(io::IntoInnerError::into_error)?;
    // The commit names blobs from every reader's pack, so they must all be in first.
    for import in importers {
        import.finish()?;
    }

    let skipped: Vec<_> = walked
        .skipped
        .iter()
        .map(|(path, why)| serde_json::json!([String::from_utf8_lossy(path), word(*why)]))
        .collect();
    let message = serde_json::json!({
        "run": run,
        "n": n,
        "reason": reason,
        "time": time,
        "files": walked.files,
        "new_bytes": walked.new_bytes,
        "complete": walked.complete,
        "more": walked.uncovered > skipped.len() as u64,
        "skipped": skipped,
    })
    .to_string();
    let mut import = git::import(store)?;
    let mut previous = base.as_ref().map(|(list, _)| open(list)).transpose()?;
    let base = base
        .as_ref()
        .map(|(_, tree)| tree.as_str())
        .zip(previous.as_mut().map(|p| p as &mut dyn BufRead));
    let mut written = BufReader::with_capacity(BUF, File::open(&list)?);
    commit(&mut import, &refname, &message, time, &mut written, base)?;
    import.finish()?;

    let launch = format!("refs/pinfold/{run}/0");
    let changed = match n {
        0 => 0,
        _ => {
            let diff = ["diff-tree", "-r", "--name-only", "-z", &launch, &refname];
            let diff = git::store(store, diff, b"")?;
            match diff.status {
                Some(status) if status.success() => {
                    diff.stdout.iter().filter(|&&c| c == 0).count() as u64
                }
                // No launch checkpoint: it took too long, or was never taken.
                _ => 0,
            }
        }
    };
    Ok(Taken {
        checkpoint: format!("{run}/{n}"),
        store: key,
        files: walked.files,
        new_bytes: walked.new_bytes,
        complete: walked.complete,
        changed,
    })
}

#[cfg(test)]
#[path = "take_tests.rs"]
pub(crate) mod tests;
