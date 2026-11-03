//! Session snapshots (ADR-0034): the layout and, per pane, its OSC 7 directory and the program it
//! started, redacted. No scrollback, no screen and no environment, so the file holds nothing
//! secret by design.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pinfold_api::control::{self, MAX_SEND};
use pinfold_api::redact;
use pinfold_mux::{LayoutTree, SessionTree, SplitDirection};
use serde::{Deserialize, Serialize};

use crate::io::Reactor;

/// The snapshot format this core writes.
pub const VERSION: u32 = 1;
/// Most bytes of a snapshot file. 64 panes with a 4,096-byte command and directory each fit.
pub const MAX_BYTES: usize = 1024 * 1024;
/// How long after a change a snapshot is taken, the period in `architecture.md`.
pub const PERIOD: Duration = Duration::from_secs(60);
/// Longest directory kept. A longer one is dropped, as a cut path names another directory.
const MAX_CWD: usize = 4096;

/// A saved session.
///
/// ```
/// let snapshot = pinfold_core::snapshot::Snapshot::default();
/// let json = serde_json::to_string(&snapshot)?;
/// assert_eq!(serde_json::from_str::<pinfold_core::snapshot::Snapshot>(&json)?, snapshot);
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// [`VERSION`].
    pub version: u32,
    /// The tabs, in order.
    pub tabs: Vec<Tab>,
    /// The position of the active tab.
    pub active: usize,
    /// Each pane, by its index in the layout.
    pub panes: Vec<Pane>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: VERSION,
            tabs: Vec::new(),
            active: 0,
            panes: Vec::new(),
        }
    }
}

/// One tab of a [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    /// Its split tree.
    pub root: Layout,
    /// The index of its focused pane.
    pub focus: usize,
    /// Whether the focused pane fills the tab.
    pub zoomed: bool,
}

/// A split tree, with panes by index.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// A pane.
    Pane(usize),
    /// Two subtrees.
    Split {
        /// `right` for side by side, `down` for stacked.
        direction: Way,
        /// The first subtree's part of the space.
        ratio: f64,
        /// The left or top subtree.
        first: Box<Layout>,
        /// The right or bottom subtree.
        second: Box<Layout>,
    },
}

/// Where a split puts its second subtree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Way {
    /// Side by side.
    Right,
    /// Stacked.
    Down,
}

/// What a snapshot keeps of a pane.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    /// The last OSC 7 directory, a hint that output can fake.
    pub cwd: Option<String>,
    /// The program the pane started, without arguments, redacted and at most 4,096 bytes.
    pub command: Option<String>,
}

impl Pane {
    /// Keeps a directory of at most 4,096 bytes and a redacted command cut to [`MAX_SEND`].
    #[must_use]
    pub fn new(cwd: Option<&str>, command: &str) -> Self {
        let command = redact(command);
        let command = &command[..command.floor_char_boundary(MAX_SEND)];
        Self {
            cwd: cwd.filter(|cwd| cwd.len() <= MAX_CWD).map(str::to_owned),
            command: Some(command)
                .filter(|command| !command.is_empty())
                .map(str::to_owned),
        }
    }
}

fn layout(tree: &LayoutTree) -> Layout {
    match tree {
        LayoutTree::Pane(index) => Layout::Pane(*index),
        LayoutTree::Split {
            direction,
            ratio,
            first,
            second,
        } => Layout::Split {
            direction: match direction {
                SplitDirection::Right => Way::Right,
                SplitDirection::Down => Way::Down,
            },
            ratio: *ratio,
            first: Box::new(layout(first)),
            second: Box::new(layout(second)),
        },
    }
}

impl Snapshot {
    /// Writes the snapshot to `sessions/<session>.json` in `state` and returns its path. The
    /// directory is made 0700 and checked as the control directory is, and the file is written
    /// 0600 to a temporary file there, synced and renamed over the old one.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory is unsafe, the snapshot is over [`MAX_BYTES`], or a
    /// write fails.
    pub fn write(&self, state: &Path, session: &str) -> io::Result<PathBuf> {
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > MAX_BYTES {
            return Err(io::Error::other("the snapshot is over 1 MiB"));
        }
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state)?;
        let dir = state.join("sessions");
        control::checked(&dir).map_err(io::Error::other)?;
        let path = dir.join(format!("{session}.json"));
        let temp = dir.join(format!(".{session}.json.tmp"));
        // Left by a crash, if anything. `create_new` follows no symlink in its place.
        let _ = fs::remove_file(&temp);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        File::open(&dir)?.sync_all()?;
        Ok(path)
    }
}

/// Writes a session's snapshots when they change, and removes its file when it has no tab.
///
/// ```
/// use std::time::Instant;
/// use pinfold_core::snapshot::{PERIOD, Saver, Snapshot};
///
/// let mut saver = Saver::new(std::env::temp_dir().join("pinfold-doc-saver"), "doc");
/// let now = Instant::now();
/// assert_eq!(saver.wait(now), None);
/// saver.touch(now);
/// assert_eq!(saver.wait(now), Some(PERIOD));
/// // A snapshot with no tab removes the file, which is not there.
/// assert!(saver.save(&Snapshot::default()).is_ok());
/// ```
#[derive(Debug)]
pub struct Saver {
    state: PathBuf,
    session: String,
    /// A hash of the snapshot last written, so an unchanged one is not written again.
    last: Option<u64>,
    /// When to take the next snapshot, set by activity only, so an idle core arms no timer.
    due: Option<Instant>,
}

impl Saver {
    /// Saves `sessions/<session>.json` in `state`.
    #[must_use]
    pub fn new(state: PathBuf, session: &str) -> Self {
        Self {
            state,
            session: session.to_owned(),
            last: None,
            due: None,
        }
    }

    /// Notes activity at `now`, which takes a snapshot [`PERIOD`] later if none is due.
    pub fn touch(&mut self, now: Instant) {
        self.due.get_or_insert(now + PERIOD);
    }

    /// Returns how long until a snapshot is due, if one is.
    #[must_use]
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Returns whether a snapshot is due at `now`, and if so, stops waiting for it.
    pub fn is_due(&mut self, now: Instant) -> bool {
        let due = self.due.is_some_and(|due| due <= now);
        if due {
            self.due = None;
        }
        due
    }

    /// Writes `snapshot` if it differs from the last one written and returns its path, or
    /// removes the file if the snapshot has no tab, as the person ended the session.
    ///
    /// # Errors
    ///
    /// Returns an error if [`Snapshot::write`] fails or the file cannot be removed.
    pub fn save(&mut self, snapshot: &Snapshot) -> io::Result<Option<PathBuf>> {
        if snapshot.tabs.is_empty() {
            self.last = None;
            let path = self
                .state
                .join("sessions")
                .join(format!("{}.json", self.session));
            return match fs::remove_file(path) {
                Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
                _ => Ok(None),
            };
        }
        let mut hasher = DefaultHasher::new();
        serde_json::to_vec(snapshot)?.hash(&mut hasher);
        let hash = hasher.finish();
        if self.last == Some(hash) {
            return Ok(None);
        }
        let path = snapshot.write(&self.state, &self.session)?;
        self.last = Some(hash);
        Ok(Some(path))
    }
}

impl Reactor {
    /// Takes a snapshot of the session. The session holds at most `MAX_PANES` panes, which
    /// bounds it.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let (tree, ids): (SessionTree, _) = self.session.to_tree();
        let panes = ids.iter().map(|id| {
            let listed = self.list().find(|pane| pane.id == *id as usize);
            listed.map_or_else(Pane::default, |pane| Pane::new(pane.cwd, pane.command))
        });
        let tabs = tree.tabs.iter().map(|tab| Tab {
            root: layout(&tab.root),
            focus: tab.focus,
            zoomed: tab.zoomed,
        });
        Snapshot {
            version: VERSION,
            tabs: tabs.collect(),
            active: tree.active,
            panes: panes.collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn sample() -> Snapshot {
        let split = Layout::Split {
            direction: Way::Right,
            ratio: 0.25,
            first: Box::new(Layout::Pane(0)),
            second: Box::new(Layout::Pane(1)),
        };
        Snapshot {
            version: VERSION,
            tabs: vec![Tab {
                root: split,
                focus: 1,
                zoomed: true,
            }],
            active: 0,
            panes: vec![
                Pane::new(
                    Some("/tmp"),
                    "deploy --token=ghp_0123456789abcdefghijklmnopqrstuvwxyz",
                ),
                Pane::new(None, &"é".repeat(MAX_SEND)),
            ],
        }
    }

    #[test]
    fn a_snapshot_survives_serde() {
        let snapshot = sample();
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(json.contains(r#""version":1"#), "{json}");
        assert!(json.contains(r#""direction":"right""#), "{json}");
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), snapshot);
    }

    #[test]
    fn a_command_is_redacted_and_capped_and_a_long_cwd_dropped() {
        let snapshot = sample();
        let command = snapshot.panes[1].command.as_deref().unwrap();
        assert_eq!(command.len(), MAX_SEND);
        let long = "/a".repeat(MAX_CWD);
        assert_eq!(Pane::new(Some(&long), "").cwd, None);
        assert_eq!(Pane::new(None, "").command, None);
    }

    #[test]
    fn an_unchanged_snapshot_is_not_rewritten_and_no_tab_removes_it() {
        let state = std::env::temp_dir().join(format!("pinfold-saver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        let mut saver = Saver::new(state.clone(), "default");
        let path = saver.save(&sample()).unwrap().unwrap();
        assert_eq!(saver.save(&sample()).unwrap(), None);
        let mut changed = sample();
        changed.panes[0].cwd = Some("/var".to_owned());
        assert_eq!(saver.save(&changed).unwrap(), Some(path.clone()));
        assert!(fs::read_to_string(&path).unwrap().contains("/var"));
        assert_eq!(saver.save(&Snapshot::default()).unwrap(), None);
        assert!(!path.exists());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_snapshot_is_due_a_period_after_the_first_activity() {
        let mut saver = Saver::new(PathBuf::new(), "default");
        let now = Instant::now();
        assert!(!saver.is_due(now + PERIOD));
        saver.touch(now);
        saver.touch(now + PERIOD / 2);
        assert_eq!(saver.wait(now), Some(PERIOD));
        assert!(!saver.is_due(now));
        assert!(saver.is_due(now + PERIOD));
        assert_eq!(saver.wait(now), None);
    }

    #[test]
    fn the_file_is_0600_in_a_0700_directory_and_holds_no_secret() {
        let state = std::env::temp_dir().join(format!("pinfold-snap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        sample().write(&state, "default").unwrap();
        // A second write replaces the first.
        let path = sample().write(&state, "default").unwrap();
        assert_eq!(path, state.join("sessions/default.json"));
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&path), mode(path.parent().unwrap())), (0o600, 0o700));
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("ghp_0123"), "{text}");
        assert!(text.contains("deploy --token="), "{text}");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(&state).unwrap();
    }
}
