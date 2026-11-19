//! The diff viewer on a checkpoint, `pinfold diff --checkpoint` (ADR-0050 section 6): what the
//! run changed from checkpoint `C` to the view checkpoint `N` the core took as the viewer opened,
//! read from the run's store through [`git::store`] and never from the working directory.
//!
//! The viewer never writes a file. `u` and `U` ask `checkpoint.restore`, which plans from the
//! store and waits on the window's `Rewind` prompt, so PinFold decides, not the viewer. A view
//! checkpoint no restore used is deleted when the viewer quits.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, ErrorKind};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use pinfold_api::control::{self, MAX_RESTORE_PATHS};
use serde_json::{Value, json};

use crate::checkpoint::plan::{self, refname};
use crate::checkpoint::take;
use crate::diff::{self, Diff};
use crate::git;

/// How long quitting waits for the store's lock to delete the view checkpoint.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// A viewer's checkpoints, and the restore it asked for.
pub(crate) struct View {
    /// The agent's pane, which `checkpoint.restore` names.
    pane: usize,
    /// `C` and `N`, `<run>/<n>`.
    from: String,
    shown: String,
    store: PathBuf,
    /// Whether each file of the diff changed after the agent stopped: its content in `N`
    /// differs from the run's newest `done` or `exit` checkpoint before `N`.
    pub(crate) later: Vec<bool>,
    /// Whether [`View::mark_later`] read the store, so an unmarked file did not change later.
    pub(crate) marked: bool,
    /// Nested repositories and submodules `C` or `N` did not cover, as their messages list them.
    pub(crate) nested: Vec<String>,
    /// The restore waiting on the window, and what became of the last one.
    restoring: Option<Receiver<Result<Value, String>>>,
    pub(crate) done: Option<Result<String, String>>,
    /// A restore was asked for, so `N` is kept: it is what the restore compared against.
    used: bool,
}

/// Splits `<run>/<n>`.
fn split(id: &str) -> Option<(&str, u64)> {
    let (run, n) = id.rsplit_once('/')?;
    Some((run, n.parse().ok()?)).filter(|(run, _)| take::is_run(run))
}

/// Loads the diff from `from` to `shown`, both checkpoints of one run in the store `key` under
/// `state`, for the agent's `pane`, or says why it could not.
pub(crate) fn load(
    state: &Path,
    key: &str,
    (from, shown): (&str, &str),
    pane: usize,
) -> Result<(Diff, View), String> {
    let (Some((run, c)), Some((same, n))) = (split(from), split(shown)) else {
        return Err("not two checkpoints of one run".to_owned());
    };
    if run != same {
        return Err("not two checkpoints of one run".to_owned());
    }
    let store = plan::store_by_key(state, key).map_err(|error| error.to_string())?;
    let mut view = View {
        pane,
        from: from.to_owned(),
        shown: shown.to_owned(),
        later: Vec::new(),
        marked: false,
        nested: Vec::new(),
        store,
        restoring: None,
        done: None,
        used: false,
    };
    let (c_ref, n_ref) = (refname(run, c), refname(run, n));
    if !plan::exists(&view.store, run, c).unwrap_or(false) {
        view.finish();
        return Err(format!("checkpoint {from} no longer exists"));
    }
    // Fixed prefixes, and no renames, so each file is one path a restore can name.
    let args = [
        "diff",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        &c_ref,
        &n_ref,
        "--",
    ];
    let output =
        git::store(&view.store, args, b"").map_err(|error| format!("cannot run git: {error}"));
    let diff = match output.and_then(diff::loaded) {
        Ok(diff) => diff,
        Err(problem) => {
            view.finish();
            return Err(problem);
        }
    };
    view.later = vec![false; diff.files.len()];
    // Read from the store alone; failing, `U` is refused rather than undoing unmarked files.
    view.marked = view.mark_later(&diff, run, n).is_ok();
    for at in [c, n] {
        let message = plan::message(&view.store, run, at).ok().flatten();
        let skipped = message
            .as_ref()
            .and_then(|message| message["skipped"].as_array());
        for pair in skipped.into_iter().flatten() {
            if let (Some(path), Some("nested")) = (pair[0].as_str(), pair[1].as_str())
                && !view.nested.iter().any(|known| known == path)
            {
                view.nested.push(path.to_owned());
            }
        }
    }
    Ok((diff, view))
}

impl View {
    /// Marks each file of `diff` that `diff-tree` lists between the run's newest `done` or `exit`
    /// checkpoint before `n` and `n`, streamed from a file in the store.
    fn mark_later(&mut self, diff: &Diff, run: &str, n: u64) -> std::io::Result<()> {
        let Some(stopped) = plan::stopped(&self.store, run, n)? else {
            return Ok(());
        };
        let (stopped, n) = (refname(run, stopped), refname(run, n));
        let mut since = plan::diff(
            &self.store,
            &["diff-tree", "-r", "--name-only", "-z", &stopped, &n],
        )?;
        let files: HashMap<&[u8], usize> = (diff.files.iter().enumerate())
            .map(|(at, file)| (file.new.as_slice(), at))
            .collect();
        let mut path = Vec::new();
        while since.read_until(0, &mut path)? > 0 {
            if path.pop() == Some(0)
                && let Some(&at) = files.get(path.as_slice())
            {
                self.later[at] = true;
            }
            path.clear();
        }
        Ok(())
    }

    /// Asks `checkpoint.restore` for `file`'s path, or with `None` for every file not marked
    /// `later`: all of them, with no list, only when none is and the diff is whole, else the
    /// shown files by name. Says why not instead, or that a restore is already waiting.
    pub(crate) fn ask(&mut self, diff: &Diff, file: Option<usize>) -> Result<(), String> {
        if self.restoring.is_some() {
            return Err("A restore already waits for the window.".to_owned());
        }
        let params = self.params(diff, file)?;
        let (send, receive) = mpsc::channel();
        thread::spawn(move || send.send(restore(&params)));
        (self.restoring, self.used, self.done) = (Some(receive), true, None);
        Ok(())
    }

    /// `checkpoint.restore`'s parameters for [`View::ask`].
    fn params(&self, diff: &Diff, file: Option<usize>) -> Result<Value, String> {
        let name = |at: usize| {
            let path = diff.files.get(at).map(|file| file.new.clone());
            path.and_then(|path| String::from_utf8(path).ok())
                .ok_or_else(|| "A name that is not UTF-8 cannot be restored alone.".to_owned())
        };
        let paths = match file {
            Some(at) => Some(vec![name(at)?]),
            None if !self.marked => {
                return Err(
                    "Files changed later cannot be told; u undoes them one at a time.".to_owned(),
                );
            }
            // An absent list means all, so none is never sent as an empty one, and only for a
            // diff that shows every file.
            None if !self.later.contains(&true) && diff.whole && diff.notes.is_empty() => None,
            None => {
                let kept: Vec<usize> = (0..diff.files.len())
                    .filter(|at| !self.later[*at])
                    .collect();
                if kept.is_empty() {
                    return Err("Every file changed later; u undoes them one at a time.".to_owned());
                }
                if kept.len() > MAX_RESTORE_PATHS {
                    return Err(format!(
                        "Over {MAX_RESTORE_PATHS} files besides those changed later; u undoes them one at a time."
                    ));
                }
                Some(kept.into_iter().map(name).collect::<Result<_, _>>()?)
            }
        };
        let mut params = json!({"pane": self.pane, "checkpoint": self.from, "shown": self.shown});
        if let Some(paths) = paths {
            params["paths"] = json!(paths);
        }
        Ok(params)
    }

    /// Whether a restore waits on the window.
    pub(crate) fn waiting(&self) -> bool {
        self.restoring.is_some()
    }

    /// Takes in the restore's answer, if it came.
    pub(crate) fn tick(&mut self) {
        let Some(restoring) = &self.restoring else {
            return;
        };
        let answer = match restoring.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(answer) => answer,
            Err(TryRecvError::Disconnected) => Err("the restore stopped".to_owned()),
        };
        self.restoring = None;
        self.done = Some(answer.map(|line| {
            let count = |name: &str| line[name].as_u64().unwrap_or(0);
            let undid = count("restored") + count("deleted");
            format!(
                "✓ done · undid {undid} files, {} skipped · the diff shows the tree as it was when opened",
                count("skipped")
            )
        }));
    }

    /// Deletes the view checkpoint `N` unless a restore used it, under the store's lock. The
    /// prune after each take holds it briefly, so this waits up to [`LOCK_WAIT`]; a take or
    /// prune holding it longer leaves `N` to `keep_per_run`.
    pub(crate) fn finish(&self) {
        let Some((run, n)) = split(&self.shown).filter(|_| !self.used) else {
            return;
        };
        let started = Instant::now();
        let _lock = loop {
            match take::lock(&self.store) {
                Ok(lock) => break lock,
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock && started.elapsed() < LOCK_WAIT =>
                {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return,
            }
        };
        let deleted = git::store(&self.store, ["update-ref", "-d", &refname(run, n)], b"");
        if deleted.is_ok_and(|out| out.status.is_some_and(|status| status.success())) {
            let _ = fs::remove_file(self.store.join("stat").join(format!("{run}-{n}")));
        }
    }
}

/// `checkpoint.restore` with `params`, waiting while the window asks the person.
fn restore(params: &Value) -> Result<Value, String> {
    let socket = control::socket_path(&control::runtime_dir());
    let stream = UnixStream::connect(&socket).map_err(|_| "PinFold is not running".to_owned())?;
    let (_, result) =
        control::ask(&stream, "checkpoint.restore", params).map_err(|error| error.to_string())?;
    Ok(result)
}

#[cfg(test)]
#[path = "rewind_tests.rs"]
mod tests;
