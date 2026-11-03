//! Session snapshots (ADR-0034): the layout and, per pane, its OSC 7 directory and the program it
//! started, redacted. No scrollback, no screen and no environment, so the file holds nothing
//! secret by design.

use std::env;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pinfold_api::control::{self, MAX_SEND};
use pinfold_api::redact;
use pinfold_mux::{LayoutTree, PaneId, Session, SessionTree, SplitDirection, TabTree};
use serde::{Deserialize, Serialize};

use crate::io::Reactor;
use crate::tabs::FIRST_SIZE;

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
    /// The workspaces `pinfold up` opened, so a restore can run their `run` lines again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspaces: Vec<Workspace>,
}

/// A workspace of a [`Snapshot`]. It names no command: `run` lines are read from the layout
/// file again, and only if it still has this hash and is trusted with it (ADR-0034).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// The canonical project directory.
    pub root: String,
    /// The layout file's SHA-256 when it opened.
    pub sha256: String,
    /// Each layout pane, in the file's order, by its index in the snapshot, or none once closed.
    pub panes: Vec<Option<usize>>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: VERSION,
            tabs: Vec::new(),
            active: 0,
            panes: Vec::new(),
            workspaces: Vec::new(),
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

fn tree(layout: &Layout) -> LayoutTree {
    match layout {
        Layout::Pane(index) => LayoutTree::Pane(*index),
        Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutTree::Split {
            direction: match direction {
                Way::Right => SplitDirection::Right,
                Way::Down => SplitDirection::Down,
            },
            ratio: *ratio,
            first: Box::new(tree(first)),
            second: Box::new(tree(second)),
        },
    }
}

/// An error that names neither the file nor anything in it.
fn invalid(why: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, why)
}

/// Parses and checks a snapshot: this [`VERSION`], and a layout [`Session::from_tree`] takes.
/// serde_json's nesting limit of 128 bounds the recursion.
///
/// ```
/// use pinfold_core::snapshot;
///
/// assert!(snapshot::parse(br#"{"version":1,"tabs":[],"active":0,"panes":[]}"#).is_err());
/// let one = br#"{"version":1,"tabs":[{"root":{"pane":0},"focus":0,"zoomed":false}],"active":0,"panes":[]}"#;
/// assert_eq!(snapshot::parse(one)?.tabs.len(), 1);
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Errors
///
/// An [`ErrorKind::InvalidData`] error, whose text is a fixed sentence, if the bytes fail.
pub fn parse(bytes: &[u8]) -> io::Result<Snapshot> {
    let snapshot: Snapshot =
        serde_json::from_slice(bytes).map_err(|_| invalid("the snapshot does not parse"))?;
    if snapshot.version != VERSION {
        return Err(invalid("the snapshot's version is unknown"));
    }
    Session::from_tree(&snapshot.tree(), 0).map_err(|_| invalid("the snapshot's layout is bad"))?;
    Ok(snapshot)
}

/// Reads and checks `sessions/<session>.json` in `state`, if there is one. The file is opened
/// without following a symlink, must be a regular file of at most [`MAX_BYTES`], and must
/// [`parse`]. One that fails is renamed to `<session>.json.bad`, replacing an older one, so it
/// does not fail every start.
///
/// # Errors
///
/// Returns why the file failed, in words that name neither the file nor anything in it.
pub fn load(state: &Path, session: &str) -> io::Result<Option<Snapshot>> {
    let dir = state.join("sessions");
    let path = dir.join(format!("{session}.json"));
    let read = || {
        let file = pinfold_sys::open_no_follow(&path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("the snapshot is not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("the snapshot is over 1 MiB"));
        }
        parse(&bytes)
    };
    match read() {
        Ok(snapshot) => Ok(Some(snapshot)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            // Renames the link itself, if it is one. A rename that fails leaves the next start
            // to fail the same way.
            let _ = fs::rename(&path, dir.join(format!("{session}.json.bad")));
            Err(error)
        }
    }
}

/// Whether `path` and each directory above it is a real directory, not a symlink, owned by
/// this user or root and writable by no one else. The saved directory comes from OSC 7, which
/// any program in a pane can print, and a shell started in a planted directory can run its code
/// (a direnv `.envrc`, git hooks or fsmonitor in a prompt). A sticky `/tmp` is refused too.
/// A restore starts a pane in `$HOME` instead.
///
/// ```
/// assert!(!pinfold_core::snapshot::is_private(std::path::Path::new("relative")));
/// ```
#[must_use]
pub fn is_private(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    let me = pinfold_sys::effective_uid();
    let plain = |part: Component<'_>| matches!(part, Component::RootDir | Component::Normal(_));
    path.is_absolute()
        && path.components().all(plain)
        && path.ancestors().all(|directory| {
            fs::symlink_metadata(directory).is_ok_and(|meta| {
                meta.file_type().is_dir()
                    && (meta.uid() == me || meta.uid() == 0)
                    && meta.mode() & 0o022 == 0
            })
        })
}

/// Where a restored pane starts: its saved directory if [`is_private`], else `$HOME`.
fn start_in(pane: Option<&Pane>) -> Option<PathBuf> {
    let saved = pane.and_then(|pane| pane.cwd.as_deref()).map(Path::new);
    let saved = saved.filter(|cwd| is_private(cwd));
    saved
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
}

impl Snapshot {
    /// The layout, as [`Session::from_tree`] takes it.
    #[must_use]
    pub fn tree(&self) -> SessionTree {
        let tabs = self.tabs.iter().map(|tab| TabTree {
            root: tree(&tab.root),
            focus: tab.focus,
            zoomed: tab.zoomed,
        });
        SessionTree {
            tabs: tabs.collect(),
            active: self.active,
        }
    }

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
            workspaces: Vec::new(),
        }
    }
}

impl Reactor {
    /// Rebuilds the layout `snapshot` saved, with a new shell in each pane, started in the
    /// pane's saved directory if that still is one, else in `$HOME`. A saved command never runs
    /// from here (ADR-0034): one that is not the shell is kept for a window to show, and runs
    /// only through [`Reactor::answer_was_running`]. Call it before any tab opens, as it replaces
    /// the session.
    ///
    /// # Errors
    ///
    /// Returns an error if the layout is bad or a shell cannot start, and then keeps no pane.
    pub fn restore(&mut self, snapshot: &Snapshot) -> io::Result<()> {
        let first = PaneId::try_from(self.next_pane).map_err(|_| invalid("no pane ids left"))?;
        let (session, ids) = Session::from_tree(&snapshot.tree(), first)
            .map_err(|_| invalid("the snapshot's layout is bad"))?;
        // Panes are numbered in order from `first`, so pane `index` gets the id it was given.
        let mut was_running = Vec::new();
        for index in 0..ids.len() {
            let mut command = (self.shell)();
            let saved = snapshot.panes.get(index);
            if let Some(directory) = start_in(saved) {
                command.current_dir(directory);
            }
            // shortcut: a pane records only its program (#145), so a shell pane shows nothing.
            // OSC 133 carries no command line, so recording one needs the shell to report it.
            let shell = Path::new(command.get_program())
                .file_name()
                .map(ToOwned::to_owned);
            let ran = saved.and_then(|pane| pane.command.clone());
            let ran = ran.filter(|ran| Path::new(ran).file_name() != shell.as_deref());
            match self.spawn(command, FIRST_SIZE.rows, FIRST_SIZE.cols) {
                Ok(pane) => was_running.extend(ran.map(|ran| (pane, ran))),
                Err(error) => {
                    for started in first as usize..self.next_pane {
                        self.drop_pane(started);
                    }
                    return Err(error);
                }
            }
        }
        self.was_running = was_running;
        self.session = session;
        self.lay_out();
        Ok(())
    }
}

/// Whether a saved command may be typed: not changed by redaction, and with no control
/// character, which could end the line early or edit it.
pub(crate) fn runnable(command: &str) -> bool {
    !command.contains("«redacted:") && !command.contains(char::is_control)
}

impl Reactor {
    /// Answers what a restored pane was running, once: with `run`, types the command and a
    /// newline into the pane's input queue, unless redaction changed it or it holds a control
    /// character. Returns whether it typed it.
    pub fn answer_was_running(&mut self, pane: usize, run: bool) -> bool {
        let Some(index) = self.was_running.iter().position(|(at, _)| *at == pane) else {
            return false;
        };
        let (_, command) = self.was_running.swap_remove(index);
        run && runnable(&command) && self.write(pane, format!("{command}\n").as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

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
            workspaces: vec![Workspace {
                root: "/p".to_owned(),
                sha256: "0".repeat(64),
                panes: vec![Some(1), None],
            }],
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
    fn a_bad_file_is_set_aside_and_a_fresh_session_starts() {
        let state = std::env::temp_dir().join(format!("pinfold-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        assert_eq!(load(&state, "default").unwrap(), None);
        let path = sample().write(&state, "default").unwrap();
        let bad = state.join("sessions/default.json.bad");
        let big = format!("[{}0]", "0,".repeat(MAX_BYTES / 2));
        for text in ["{", r#"{"version":2}"#, &big] {
            fs::write(&path, text).unwrap();
            assert!(load(&state, "default").is_err());
            assert!(!path.exists());
            // A newer bad file replaces the older one.
            assert_eq!(fs::read_to_string(&bad).unwrap(), text);
            assert_eq!(load(&state, "default").unwrap(), None);
        }
        // A symlink to a good snapshot is refused, and moved aside as a link.
        let other = state.join("elsewhere.json");
        fs::write(&other, serde_json::to_vec(&sample()).unwrap()).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(load(&state, "default").is_err());
        assert!(fs::symlink_metadata(&bad).unwrap().is_symlink());

        let mut reactor = Reactor::new().unwrap();
        reactor.shell = || Command::new("cat");
        let mut layout = sample();
        layout.tabs[0].root = Layout::Pane(2);
        assert!(reactor.restore(&layout).is_err());
        assert_eq!(reactor.pane_count(), 0);
        assert!(reactor.open_tab(Command::new("cat"), 24, 80).is_ok());
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_restore_starts_a_shell_per_pane_in_the_saved_layout() {
        let state = std::env::temp_dir().join(format!("pinfold-load-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        sample().write(&state, "default").unwrap();
        let snapshot = load(&state, "default").unwrap().unwrap();
        let mut reactor = Reactor::new().unwrap();
        reactor.shell = || Command::new("cat");
        reactor.restore(&snapshot).unwrap();
        assert_eq!(reactor.pane_count(), 2);
        assert_eq!(reactor.session.to_tree().0, snapshot.tree());
        // The saved command is never the program.
        assert!(reactor.list().all(|pane| pane.command == "cat"));
        let home = env::var_os("HOME").map(PathBuf::from);
        assert_eq!(start_in(Some(&Pane::new(Some("/no/such"), ""))), home);
        assert_eq!(start_in(Some(&Pane::new(Some("/"), ""))), Some("/".into()));
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn only_a_directory_no_one_else_can_write_or_redirect_is_kept() {
        // On macOS the temporary directory is 0700 under root's 0755 `/private/var/folders`.
        // Linux's `/tmp` is sticky and world-writable, so there the tree goes in the crate.
        let bases = [env::temp_dir(), PathBuf::from(env!("CARGO_MANIFEST_DIR"))];
        let base = bases.iter().filter_map(|base| base.canonicalize().ok());
        let base = base.into_iter().find(|base| is_private(base)).unwrap();
        let root = base.join(format!("pinfold-cwd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mode = |path: &Path, mode| fs::set_permissions(path, fs::Permissions::from_mode(mode));
        let kept = root.join("kept");
        fs::create_dir_all(&kept).unwrap();
        mode(&root, 0o755).unwrap();
        mode(&kept, 0o755).unwrap();
        let open = root.join("open");
        fs::create_dir(&open).unwrap();
        mode(&open, 0o777).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&kept, &link).unwrap();
        fs::create_dir(kept.join("below")).unwrap();

        let start = |cwd: &Path| start_in(Some(&Pane::new(cwd.to_str(), "")));
        let home = env::var_os("HOME").map(PathBuf::from);
        assert_eq!(start(&kept), Some(kept.clone()));
        assert_eq!(start(&open), home);
        assert_eq!(start(&link), home);
        assert_eq!(start(&link.join("below")), home);
        assert_eq!(start(&kept.join("../open")), home);
        fs::remove_dir_all(&root).unwrap();
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
