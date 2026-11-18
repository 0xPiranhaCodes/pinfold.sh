//! `pinfold checkpoint restore` (ADR-0050 section 6 and its slice 4 amendment): under the
//! store's lock, takes the `restore` checkpoint, then makes each path that differs between the
//! checkpoint `C` and the view checkpoint `N` match `C` again, through [`pinfold_sys::at`]
//! only. A path that differs from its entry in `N` was changed since shown and is skipped.
//! Deletions run first: a path only `N` holds goes only if `C` covered it and is complete.
//! Then each write is a new file in its parent, renamed over the name, so a link or hard link
//! there is replaced and what it points to is never written.

use std::ffi::{OsStr, OsString};
use std::fs::Permissions;
use std::io::{self, BufWriter, Seek};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use pinfold_sys::at::{self, Kind};

use super::plan::{self, Origin, Selection, display, exists, gone, numbers, refname};
use super::take::{is_run, lock, resolve, take_in};
use super::{BUF, Caps, blob, hex};
use crate::git;

const TREE: u32 = 0o040000;
const LINK: u32 = 0o120000;
const EXECUTABLE: u32 = 0o100755;
/// The longest link target a restore writes.
const MAX_LINK: usize = 4096;

/// What a restore did. `pinfold checkpoint restore` prints it as `{"restored":R,"deleted":D,
/// "skipped":S,"skipped_paths":[[path,why],...],"restore_checkpoint":"<run>/<n>"}`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Restored {
    /// Paths written back as `C` holds them.
    pub restored: u64,
    /// Paths deleted that the agent added.
    pub deleted: u64,
    /// Paths left as they are: changed since shown, or failed.
    pub skipped: u64,
    /// The first 10 of them: the path, lossily UTF-8 and cut to 256 bytes, and `changed` or
    /// `failed`.
    pub skipped_paths: Vec<(String, &'static str)>,
    /// `<run>/<n>`, the checkpoint taken first, which restores the state before this restore.
    pub restore_checkpoint: String,
}

/// One entry of `diff-tree -r -t -z` raw output: modes, 0 for none, and blob ids.
struct Change {
    from: u32,
    to: u32,
    from_id: String,
    to_id: String,
}

fn change(header: &[u8]) -> io::Result<Change> {
    let header = std::str::from_utf8(header).unwrap_or_default();
    let mut fields = header.strip_prefix(':').unwrap_or_default().split(' ');
    let mut mode = || u32::from_str_radix(fields.next()?, 8).ok();
    let (from, to) = (mode(), mode());
    let mut id = || fields.next().map(str::to_owned);
    match (from, to, id(), id()) {
        (Some(from), Some(to), Some(from_id), Some(to_id)) => Ok(Change {
            from,
            to,
            from_id,
            to_id,
        }),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "a broken diff")),
    }
}

fn missing(error: &at::Error) -> bool {
    matches!(error, at::Error::Io(e) if e.kind() == io::ErrorKind::NotFound)
}

/// A parent that is a link or not a directory: what is under it cannot be reached without
/// following a link, so is not there.
fn blocked(error: &at::Error) -> bool {
    matches!(error, at::Error::Link)
        || matches!(error, at::Error::Io(e) if e.kind() == io::ErrorKind::NotADirectory)
}

struct Restore {
    root: OwnedFd,
    blobs: git::Blobs,
    mask: u32,
    buf: Vec<u8>,
    done: Restored,
}

impl Restore {
    fn skip(&mut self, path: &[u8], why: &'static str) {
        self.done.skipped += 1;
        if self.done.skipped_paths.len() < plan::SHOWN {
            self.done.skipped_paths.push((display(path), why));
        }
    }

    /// The parent of `path` opened from the root without following a link, and its last
    /// component; `None` if a parent is missing, a link or not a directory.
    fn parent<'p>(&self, path: &'p [u8]) -> io::Result<Option<(OwnedFd, &'p OsStr)>> {
        let mut parts = path.split(|&c| c == b'/');
        let name = OsStr::from_bytes(parts.next_back().unwrap_or_default());
        let mut dir = self.root.try_clone()?;
        for part in parts {
            match at::open_dir_at(dir.as_fd(), OsStr::from_bytes(part)) {
                Ok(next) => dir = next,
                Err(error) if missing(&error) || blocked(&error) => return Ok(None),
                Err(error) => return Err(super::io_error(error)),
            }
        }
        Ok(Some((dir, name)))
    }

    /// Whether what is at `path` now is `N`'s entry, `mode` and blob `id`, or is missing as
    /// `N`'s is when `mode` is 0: its kind and mode, then its blob id, never its stat alone.
    fn matches(&mut self, path: &[u8], mode: u32, id: &str) -> io::Result<bool> {
        let Some((dir, name)) = self.parent(path)? else {
            return Ok(mode == 0);
        };
        let stat = match at::stat_at(dir.as_fd(), name) {
            Ok(stat) => stat,
            Err(error) if missing(&error) => return Ok(mode == 0),
            Err(error) => return Err(super::io_error(error)),
        };
        let found = match stat.kind {
            Kind::Link if mode == LINK => {
                let mut target = [0; MAX_LINK];
                let target =
                    at::read_link_at(dir.as_fd(), name, &mut target).map_err(super::io_error)?;
                let size = target.len() as u64;
                blob(&mut io::sink(), &mut &target[..], size, &mut self.buf)?
            }
            Kind::File if (stat.mode & 0o111 != 0) == (mode == EXECUTABLE) && mode != LINK => {
                let mut file = at::open_file_at(dir.as_fd(), name).map_err(super::io_error)?;
                let opened = file.metadata()?;
                if !opened.is_file() || opened.len() != stat.size {
                    return Ok(false);
                }
                blob(&mut io::sink(), &mut file, stat.size, &mut self.buf)?
            }
            _ => return Ok(false),
        };
        Ok(found.1 && hex(&found.0) == id.as_bytes())
    }

    fn delete(&mut self, path: &[u8]) -> io::Result<()> {
        let (dir, name) = self.parent(path)?.ok_or(io::ErrorKind::NotFound)?;
        at::unlink_at(dir.as_fd(), name).map_err(super::io_error)
    }

    /// Removes the directory `path` if it is empty.
    fn remove_dir(&self, path: &[u8]) {
        if let Ok(Some((dir, name))) = self.parent(path) {
            let _ = at::remove_dir_at(dir.as_fd(), name);
        }
    }

    /// Writes `C`'s entry, `mode` and blob `id`, at `path`: a new file or link in its parent,
    /// renamed over the name. Missing parents are made; one that is a link or a file is not
    /// removed here, so the path is skipped (`Ok(false)`).
    fn write(&mut self, path: &[u8], mode: u32, id: &str) -> io::Result<bool> {
        let mut parts = path.split(|&c| c == b'/');
        let name = OsStr::from_bytes(parts.next_back().unwrap_or_default());
        let mut dir = self.root.try_clone()?;
        for part in parts.map(OsStr::from_bytes) {
            dir = match at::open_dir_at(dir.as_fd(), part) {
                Ok(next) => next,
                Err(error) if missing(&error) => {
                    at::mkdir_at(dir.as_fd(), part, 0o777).map_err(super::io_error)?;
                    at::open_dir_at(dir.as_fd(), part).map_err(super::io_error)?
                }
                Err(error) if blocked(&error) => return Ok(false),
                Err(error) => return Err(super::io_error(error)),
            };
        }
        let new = OsString::from(format!(".pinfold-restore-{}", std::process::id()));
        let made = if mode == LINK {
            let mut target = Vec::new();
            self.blobs.copy(id, MAX_LINK as u64, &mut target)?;
            at::symlink_at(OsStr::from_bytes(&target), dir.as_fd(), &new).map_err(super::io_error)
        } else {
            let file = at::create_at(dir.as_fd(), &new).map_err(super::io_error)?;
            let mut out = BufWriter::with_capacity(BUF, file);
            let bits = if mode == EXECUTABLE { 0o755 } else { 0o644 };
            self.blobs
                .copy(id, u64::MAX, &mut out)
                .and_then(|_| out.into_inner().map_err(io::IntoInnerError::into_error))
                .and_then(|file| file.set_permissions(Permissions::from_mode(bits & !self.mask)))
        };
        let renamed = made.and_then(|()| {
            at::rename_at(dir.as_fd(), &new, dir.as_fd(), name).map_err(super::io_error)
        });
        if renamed.is_err() {
            let _ = at::unlink_at(dir.as_fd(), &new);
        }
        renamed.map(|()| true)
    }
}

fn under(path: &[u8], dir: &[u8]) -> bool {
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// Restores checkpoint `c` of `run` over the view checkpoint `shown` in the working directory
/// `dir`, whose store is under `state`, limited to `paths` (relative to `dir`, at most 1,000)
/// if given, at Unix time `time`. It holds the store's lock throughout, takes the `restore`
/// checkpoint, numbered after the run's newest, then deletes and writes as the module says.
/// A path `C` did not cover is never written or deleted, and nothing is deleted when `C` is
/// incomplete. A directory left empty that only `N` held is removed.
///
/// # Errors
///
/// Refuses as [`plan::plan`] does, and as a take refuses, its lock included
/// ([`io::ErrorKind::WouldBlock`]); nothing is written then. A path that cannot be written is
/// skipped as `failed`; the restore goes on.
///
/// # Examples
///
/// ```no_run
/// use pinfold_tools::checkpoint::restore::restore;
///
/// let (state, work) = ("/path/to/state".as_ref(), "/path/to/work".as_ref());
/// let restored = restore(state, "r1700000000-0", 0, 3, work, None, 1_700_000_100)?;
/// println!("undid {} files, {} skipped", restored.restored + restored.deleted, restored.skipped);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn restore(
    state: &Path,
    run: &str,
    c: u64,
    shown: u64,
    dir: &Path,
    paths: Option<Vec<Vec<u8>>>,
    time: u64,
) -> io::Result<Restored> {
    if !is_run(run) {
        let bad = format!("not a run: {run}");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, bad));
    }
    let chosen = Selection::new(paths)?;
    // Before the take starts its readers.
    let mask = pinfold_sys::umask();
    let (dir, _) = resolve(dir)?;
    let store = plan::store_of(state, &dir)?;
    let _lock = lock(&store)?;
    let origin = Origin::read(&store, run, c)?;
    if !exists(&store, run, shown)? {
        return Err(gone(run, shown));
    }
    let n = numbers(&store, run)?.into_iter().max().map_or(0, |n| n + 1);
    let taken = take_in(
        state,
        &store,
        &dir,
        run,
        n,
        "restore",
        time,
        &Caps::default(),
    )?;
    let (from, to) = (refname(run, c), refname(run, shown));
    let mut changes = plan::diff(&store, &["diff-tree", "-r", "-t", "-z", &from, &to])?;
    let mut restore = Restore {
        root: at::open_dir(&dir).map_err(super::io_error)?,
        blobs: git::blobs(&store)?,
        mask,
        buf: vec![0; BUF],
        done: Restored {
            restore_checkpoint: taken.checkpoint,
            ..Restored::default()
        },
    };
    let (mut header, mut path) = (Vec::new(), Vec::new());

    // Deletions: paths only N holds, deepest last, then the directories only N held, emptied.
    let mut dirs: Vec<Vec<u8>> = Vec::new();
    while plan::pair(&mut changes, &mut header, &mut path)? {
        let change = change(&header)?;
        while dirs.last().is_some_and(|dir| !under(&path, dir)) {
            dirs.pop().inspect(|dir| restore.remove_dir(dir));
        }
        if change.from != 0 || !chosen.has(&path) || !origin.covered(&path) {
            continue;
        }
        if change.to == TREE {
            dirs.push(path.clone());
            continue;
        }
        match restore.matches(&path, change.to, &change.to_id) {
            Ok(true) => match restore.delete(&path) {
                Ok(()) => restore.done.deleted += 1,
                Err(_) => restore.skip(&path, "failed"),
            },
            Ok(false) => restore.skip(&path, "changed"),
            Err(_) => restore.skip(&path, "failed"),
        }
    }
    while let Some(dir) = dirs.pop() {
        restore.remove_dir(&dir);
    }

    // Writes: every path C holds that differs in N.
    changes.rewind()?;
    while plan::pair(&mut changes, &mut header, &mut path)? {
        let change = change(&header)?;
        if matches!(change.from, 0 | TREE) || !chosen.has(&path) {
            continue;
        }
        let written = match restore.matches(&path, change.to, &change.to_id) {
            Ok(true) => restore.write(&path, change.from, &change.from_id),
            other => other,
        };
        match written {
            Ok(true) => restore.done.restored += 1,
            Ok(false) => restore.skip(&path, "changed"),
            Err(_) => restore.skip(&path, "failed"),
        }
    }
    Ok(restore.done)
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
