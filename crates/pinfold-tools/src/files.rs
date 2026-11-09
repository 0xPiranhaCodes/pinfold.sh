//! The file manager behind `pinfold files` (ADR-0045 sections 2 and 4).
//!
//! Three columns: the parent directory, the current one and a preview of the selected entry.
//! Names and contents are untrusted, so all of them reach the screen through
//! [`Screen::put`], which escapes them, and a name that needed escaping is flagged red. Enter opens a file in the reader, executables
//! included; nothing is ever run.
//!
//! Inside a repository each entry carries its `git status` badge. Git runs on its own thread,
//! killed after 2 s, so a slow or huge repository never holds a key or a draw; once a second the
//! directory's and the index's modification times are compared, and only a change lists the
//! directory and runs git again, one call at a time. A directory where git ran over its cap
//! gets no more calls until it is left.
//!
//! Rename and trash (in [`ops`]) each wait on a confirmation drawn in the pane that
//! lists every name escaped. No API caller can answer it, as `pane.send` to a tool pane is
//! refused (ADR-0045 section 4).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use std::{io, thread};

use crate::git;
use crate::keys::Key;
use crate::md::{self, Reader, colour};
use crate::ops;
use crate::screen::{Color, Screen, Style, escape};
use crate::{Flow, Tool};

/// Entries listed per directory; the rest are counted.
const MAX_ENTRIES: usize = 10_000;
/// Bytes of a file the preview reads.
const MAX_PREVIEW: usize = 64 * 1024;
/// Bytes of a binary file the hex preview shows.
const MAX_HEX: usize = 4 * 1024;
/// Time `git status` may run before it is killed and the directory goes without badges.
pub(crate) const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the directory's modification time is checked.
const POLL: Duration = Duration::from_secs(1);
/// Matches the fuzzy jump shows.
const MAX_JUMPS: usize = 100;

const KEYS: [&str; 12] = [
    "j k  ↓ ↑        down or up",
    "h l  ← →        the parent, or into a directory",
    "Enter           into a directory, or a file in the reader",
    "/               filter by name; Enter keeps it, Esc clears it",
    "g G  Home End   the first or the last",
    "f               jump to a path under the start by fuzzy match",
    "m x  ' x        bookmark this directory as x, or go to x",
    "Space           mark or unmark; the actions below take the marked, or else the selected",
    "r               rename",
    "d               move to the trash",
    "?               these keys",
    "q               quit",
];

struct Entry {
    name: OsString,
    dir: bool,
}

/// A directory's entries, directories first and each by name, and how many past the cap.
#[derive(Default)]
struct Listing {
    entries: Vec<Entry>,
    more: usize,
}

fn list(dir: &Path) -> Result<Listing, String> {
    let read = std::fs::read_dir(dir).map_err(|error| format!("cannot read: {error}"))?;
    let mut listing = Listing::default();
    for entry in read.flatten() {
        if listing.entries.len() == MAX_ENTRIES {
            listing.more += 1;
            continue;
        }
        // A link is a directory if it points at one; `is_dir` stats it and opens nothing.
        let dir = entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()));
        let name = entry.file_name();
        listing.entries.push(Entry { name, dir });
    }
    listing
        .entries
        .sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    Ok(listing)
}

enum Preview {
    Text(String),
    Hex(Vec<u8>),
    /// Directory stats, or why there is no preview.
    Note(Vec<String>),
}

fn preview(path: &Path, dir: bool) -> Preview {
    if dir {
        return Preview::Note(match list(path) {
            Ok(listing) => {
                let dirs = listing.entries.iter().filter(|entry| entry.dir).count();
                let files = listing.entries.len() - dirs + listing.more;
                vec![
                    format!("directories  {dirs}"),
                    format!("files        {files}"),
                ]
            }
            Err(problem) => vec![problem],
        });
    }
    match md::read_at_most(path, MAX_PREVIEW) {
        Ok((bytes, _)) if text(&bytes) => {
            Preview::Text(String::from_utf8_lossy(&bytes).replace('\t', "    "))
        }
        Ok((mut bytes, _)) => {
            bytes.truncate(MAX_HEX);
            Preview::Hex(bytes)
        }
        Err(error) => Preview::Note(vec![format!("No preview: {error}")]),
    }
}

/// Text has no NUL and is UTF-8, but for a character the read cut short.
fn text(bytes: &[u8]) -> bool {
    !bytes.contains(&0)
        && std::str::from_utf8(bytes).map_or_else(|e| e.error_len().is_none(), |_| true)
}

/// Whether `name` holds a control or bidi character or invalid UTF-8: anything [`escape`]
/// changes, so the list of what is hostile lives in one place.
fn flagged(name: &[u8]) -> bool {
    std::str::from_utf8(name).map_or(true, |name| escape(name.as_bytes()) != name)
}

/// What `git status` says of an entry. A directory shows the strongest badge of what it holds,
/// but is dimmed only if it is ignored itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Badge {
    Ignored,
    Untracked,
    Added,
    Modified,
}

/// The repository root above `dir`, found by its `.git` as git finds it, so a directory
/// outside one costs no git call, and `dir`'s path from there ending in `/`, or empty.
fn repository(dir: &Path) -> Option<(PathBuf, Vec<u8>)> {
    let root = dir.ancestors().find(|up| up.join(".git").exists())?;
    let mut prefix = dir.strip_prefix(root).ok()?.as_os_str().as_bytes().to_vec();
    if !prefix.is_empty() {
        prefix.push(b'/');
    }
    Some((root.to_path_buf(), prefix))
}

/// Badges by entry name from `git status --porcelain=v1 -z`, whose entries are `XY path\0`, a
/// rename's or copy's old path in the next field, each relative to the root, so a name holding
/// a newline or a quote parses as it is.
fn badges(output: &[u8], prefix: &[u8]) -> HashMap<OsString, Badge> {
    let mut badges = HashMap::new();
    let mut fields = output.split(|&byte| byte == 0);
    while let Some(field) = fields.next() {
        let (Some(code), Some(path)) = (field.get(..2), field.get(3..)) else {
            continue;
        };
        if code.contains(&b'R') || code.contains(&b'C') {
            fields.next();
        }
        let badge = match code {
            b"!!" => Badge::Ignored,
            b"??" => Badge::Untracked,
            _ if code.contains(&b'A') => Badge::Added,
            _ => Badge::Modified,
        };
        let Some(rest) = path.strip_prefix(prefix) else {
            continue;
        };
        let (name, inside) = match rest.iter().position(|&byte| byte == b'/') {
            Some(at) => (&rest[..at], at + 1 < rest.len()),
            None => (rest, false),
        };
        if name.is_empty() || (inside && badge == Badge::Ignored) {
            continue;
        }
        let kept = badges
            .entry(OsStr::from_bytes(name).to_owned())
            .or_insert(badge);
        *kept = (*kept).max(badge);
    }
    badges
}

/// Paths under `start`, relative to it, breadth first so the cap keeps the shallow ones.
/// Links are listed but not followed, so a loop cannot hold it, and `.git` is skipped.
fn walk(start: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut queue = VecDeque::from([PathBuf::new()]);
    while let Some(dir) = queue.pop_front() {
        let Ok(read) = std::fs::read_dir(start.join(&dir)) else {
            continue;
        };
        for entry in read.flatten() {
            if paths.len() == MAX_ENTRIES {
                return paths;
            }
            let path = dir.join(entry.file_name());
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && entry.file_name() != ".git" {
                queue.push_back(path.clone());
            }
            paths.push(path);
        }
    }
    paths
}

/// How well `query` matches `path` as a subsequence, ignoring case, or `None` if it does not:
/// each character scores 1, and 10 more right after the last match or else 8 more at the start
/// of a word, so a run beats scattered word starts; a shorter path wins a tie.
fn score(query: &str, path: &str) -> Option<i64> {
    let path: Vec<char> = path.to_lowercase().chars().collect();
    let (mut score, mut last) = (0_i64, None);
    for wanted in query.to_lowercase().chars() {
        let from = last.map_or(0, |last| last + 1);
        let at = (from..path.len()).find(|&at| path[at] == wanted)?;
        score += 1;
        if last.is_some() && at == from {
            score += 10;
        } else if at == 0 || matches!(path[at - 1], '/' | '.' | '_' | '-' | ' ') {
            score += 8;
        }
        last = Some(at);
    }
    Some(score * 10_000 - i64::try_from(path.len()).unwrap_or(i64::MAX / 2))
}

/// Indices of `paths` that match `query`, best first, at most [`MAX_JUMPS`].
fn rank(query: &str, paths: &[String]) -> Vec<usize> {
    let mut ranked: Vec<(i64, usize)> = (paths.iter().enumerate())
        .filter_map(|(at, path)| Some((score(query, path)?, at)))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| paths[a.1].cmp(&paths[b.1])));
    ranked
        .into_iter()
        .take(MAX_JUMPS)
        .map(|(_, at)| at)
        .collect()
}

/// A finished `git status`, with the directory's prefix in its repository.
type Status = (Vec<u8>, io::Result<git::Output>);

/// The fuzzy jump: what was typed, the paths under the start, escaped, and the best matches.
struct Jump {
    query: String,
    paths: Vec<PathBuf>,
    shown: Vec<String>,
    ranked: Vec<usize>,
}

/// What a confirmation will do once `y` answers it.
enum Action {
    Rename(PathBuf, OsString),
    Trash(Vec<PathBuf>),
}

impl Action {
    /// The question, then the name of each entry it touches, all from one directory; every
    /// name is shown escaped.
    fn lines(&self) -> (String, &[PathBuf]) {
        match self {
            Self::Rename(from, name) => (
                format!("Rename to {}?", escape(name.as_bytes())),
                std::slice::from_ref(from),
            ),
            Self::Trash(paths) => (format!("Trash {}?", count(paths)), paths),
        }
    }
}

fn count(paths: &[PathBuf]) -> String {
    match paths.len() {
        1 => "1 entry".to_owned(),
        n => format!("{n} entries"),
    }
}

/// `name` escaped and cut to `width` characters.
fn clip(name: &[u8], width: usize) -> String {
    escape(name).chars().take(width).collect()
}

/// The file manager: Miller columns over `dir`, and the reader once a file is opened.
pub(crate) struct Files {
    /// The directory as it was given, which stands for `start` in the title.
    title: Vec<u8>,
    start: PathBuf,
    dir: PathBuf,
    listing: Result<Listing, String>,
    parent: Listing,
    /// Indices into the listing of the entries that match `filter`.
    shown: Vec<usize>,
    selected: usize,
    filter: String,
    typing: bool,
    /// The selected entry's preview, made when it is first drawn.
    preview: Option<Preview>,
    pager: Option<Reader>,
    problem: Option<String>,
    help: bool,
    /// Badges by name in `dir`, the `git status` running for a directory, and why there are none.
    badges: HashMap<OsString, Badge>,
    git: Option<(PathBuf, Receiver<Status>)>,
    git_note: Option<String>,
    /// The directory's and the index's stamps as last listed, and when they were last checked.
    stamp: Option<(Option<md::Stamp>, Option<md::Stamp>)>,
    checked: Option<Instant>,
    jump: Option<Jump>,
    /// Bookmarks for this run, and `m` or `'` waiting for a bookmark's name.
    marks: HashMap<char, PathBuf>,
    mark: Option<char>,
    /// Names marked with Space in `dir`, the new name being typed, an open confirmation and
    /// its scroll, and where trash goes.
    marked: BTreeSet<OsString>,
    naming: Option<String>,
    confirm: Option<(Action, usize)>,
    trash: Option<(PathBuf, bool)>,
}

impl Files {
    /// A file manager titled `title` for the directory `dir`.
    pub(crate) fn new(title: Vec<u8>, dir: PathBuf) -> Self {
        let mut files = Self {
            title,
            start: dir.clone(),
            dir: PathBuf::new(),
            listing: Ok(Listing::default()),
            parent: Listing::default(),
            shown: Vec::new(),
            selected: 0,
            filter: String::new(),
            typing: false,
            preview: None,
            pager: None,
            problem: None,
            help: false,
            badges: HashMap::new(),
            git: None,
            git_note: None,
            stamp: None,
            checked: None,
            jump: None,
            marks: HashMap::new(),
            mark: None,
            marked: BTreeSet::new(),
            naming: None,
            confirm: None,
            trash: ops::trash_dir(),
        };
        files.go(dir, None);
        files
    }

    /// Lists `dir`, with the entry named `select` selected if there is one.
    fn go(&mut self, dir: PathBuf, select: Option<&OsStr>) {
        self.listing = list(&dir);
        self.parent = dir
            .parent()
            .and_then(|up| list(up).ok())
            .unwrap_or_default();
        self.dir = dir;
        self.marked.clear();
        self.filter.clear();
        self.refilter();
        self.reselect(select);
        self.badges.clear();
        self.git_note = None;
        (self.stamp, self.checked) = (None, None);
    }

    fn reselect(&mut self, name: Option<&OsStr>) {
        let entries = self.entries();
        let selected = self
            .shown
            .iter()
            .position(|&at| Some(entries[at].name.as_os_str()) == name);
        self.selected = selected.unwrap_or(0);
    }

    /// Runs `git status` for `dir` on its own thread, if it is in a repository.
    fn status(&mut self) {
        let Some((_, prefix)) = repository(&self.dir) else {
            return;
        };
        let (send, receive) = mpsc::channel();
        let dir = self.dir.clone();
        thread::spawn(move || {
            let args = [
                "status",
                "--porcelain=v1",
                "-z",
                "--ignored",
                "--untracked-files=normal",
                "--",
                ".",
            ];
            let _ = send.send((prefix, git::run_within(&dir, args, STATUS_TIMEOUT)));
        });
        self.git = Some((self.dir.clone(), receive));
    }

    /// Takes in what `git status` printed for the directory at `prefix` in its repository.
    fn show(&mut self, prefix: &[u8], output: io::Result<git::Output>) {
        match output {
            Ok(output) if output.cut_short => {
                let over = STATUS_TIMEOUT.as_secs();
                self.git_note = Some(format!("git status ran over {over} s, so no badges here."));
                self.badges.clear();
            }
            Ok(output) if output.status.is_some_and(|status| status.success()) => {
                self.badges = badges(&output.stdout, prefix);
            }
            // Not a work tree, or a repository git refuses: no badges, as outside one.
            _ => self.badges.clear(),
        }
    }

    /// Takes in a finished `git status`, then once a second lists the directory again and
    /// reruns git if it or the index changed. Nothing waits on git.
    fn poll(&mut self) {
        if let Some((dir, receive)) = &self.git {
            match receive.try_recv() {
                Err(TryRecvError::Empty) => return,
                Ok((prefix, output)) if *dir == self.dir => self.show(&prefix, output),
                _ => {}
            }
            self.git = None;
        }
        if self.checked.is_some_and(|at| at.elapsed() < POLL) {
            return;
        }
        self.checked = Some(Instant::now());
        let index = repository(&self.dir).and_then(|(root, _)| md::stamp(&root.join(".git/index")));
        let now = (md::stamp(&self.dir), index);
        if self.stamp == Some(now) {
            return;
        }
        if self.stamp.is_some() {
            let name = self.entry().map(|entry| entry.name.clone());
            self.listing = list(&self.dir);
            self.refilter();
            self.reselect(name.as_deref());
        }
        self.stamp = Some(now);
        if self.git_note.is_none() {
            self.status();
        }
    }

    /// Goes to the best match: into it if it is a directory, else to it in its directory.
    fn land(&mut self) {
        let Some(jump) = self.jump.take() else { return };
        let Some(&best) = jump.ranked.first() else {
            return;
        };
        let path = self.start.join(&jump.paths[best]);
        if path.is_dir() {
            self.go(path, None);
        } else if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            self.go(dir.to_path_buf(), Some(name));
        }
    }

    fn jump_key(&mut self, key: Key) {
        let Some(jump) = &mut self.jump else { return };
        match key {
            Key::Char(character) => jump.query.push(character),
            Key::Backspace => {
                jump.query.pop();
            }
            Key::Enter => return self.land(),
            Key::Esc => {
                self.jump = None;
                return;
            }
            _ => return,
        }
        jump.ranked = rank(&jump.query, &jump.shown);
    }

    /// `m x` bookmarks this directory as `x`, and `' x` goes to it.
    fn mark_key(&mut self, kind: char, key: Key) {
        let Key::Char(name) = key else { return };
        if kind == 'm' {
            self.marks.insert(name, self.dir.clone());
        } else if let Some(dir) = self.marks.get(&name).cloned() {
            self.go(dir, None);
        } else {
            self.problem = Some(format!(
                "no bookmark {}",
                escape(name.to_string().as_bytes())
            ));
        }
    }

    fn entries(&self) -> &[Entry] {
        self.listing
            .as_ref()
            .map_or(&[], |listing| &listing.entries)
    }

    fn refilter(&mut self) {
        let filter = self.filter.to_lowercase();
        self.shown = (0..self.entries().len())
            .filter(|&at| {
                let name = escape(self.entries()[at].name.as_bytes());
                name.to_lowercase().contains(&filter)
            })
            .collect();
        self.selected = 0;
        self.preview = None;
    }

    fn entry(&self) -> Option<&Entry> {
        let at = *self.shown.get(self.selected)?;
        self.entries().get(at)
    }

    fn select(&mut self, at: usize) {
        self.selected = at.min(self.shown.len().saturating_sub(1));
        self.preview = None;
    }

    /// Into the selected directory, or the selected file in the reader.
    fn open(&mut self) {
        let Some(entry) = self.entry() else { return };
        let (path, dir, name) = (
            self.dir.join(&entry.name),
            entry.dir,
            escape(entry.name.as_bytes()),
        );
        if dir {
            self.go(path, None);
            return;
        }
        // Read, never run: an executable shows its bytes like any other file.
        match md::read(&path) {
            Ok((bytes, whole)) => {
                let title = path.as_os_str().as_bytes().to_vec();
                let mut reader = Reader::new(title, bytes, whole);
                reader.path.clone_from(&path);
                let markdown = path.extension().is_some_and(|extension| extension == "md");
                self.pager = Some(if markdown { reader } else { reader.plain() });
            }
            Err(error) => {
                self.problem = Some(format!("cannot open {name}: {error}"));
            }
        }
    }

    /// The marked entries' paths, or else the selected one's.
    fn chosen(&self) -> Vec<PathBuf> {
        if self.marked.is_empty() {
            self.entry()
                .map(|entry| vec![self.dir.join(&entry.name)])
                .unwrap_or_default()
        } else {
            self.marked.iter().map(|name| self.dir.join(name)).collect()
        }
    }

    /// Carries out a confirmed action, stopping at the first failure, then lists the directory
    /// again.
    fn act(&mut self, action: Action) {
        let done = match &action {
            Action::Rename(from, name) => ops::rename_to(from, name),
            Action::Trash(paths) => match &self.trash {
                Some((bin, xdg)) => paths
                    .iter()
                    .try_for_each(|path| ops::trash(path, (bin, *xdg))),
                None => Err("no trash: HOME is not set".to_owned()),
            },
        };
        self.problem = done.err();
        let name = match action {
            Action::Rename(_, name) => Some(name),
            _ => self.entry().map(|entry| entry.name.clone()),
        };
        self.go(self.dir.clone(), name.as_deref());
    }

    /// Takes a key while a new name is typed; Enter asks to confirm it.
    fn name_key(&mut self, key: Key) {
        let Some(name) = &mut self.naming else { return };
        match key {
            Key::Char(character) => name.push(character),
            Key::Backspace => {
                name.pop();
            }
            Key::Enter => {
                let name = OsString::from(std::mem::take(name));
                self.naming = None;
                let from = self.entry().map(|entry| self.dir.join(&entry.name));
                match (ops::check_name(&name), from) {
                    (Err(problem), _) => self.problem = Some(problem),
                    (Ok(()), Some(from)) => self.confirm = Some((Action::Rename(from, name), 0)),
                    (Ok(()), None) => {}
                }
            }
            Key::Esc => self.naming = None,
            _ => {}
        }
    }

    fn title(&self) -> Vec<u8> {
        let mut title = b"files ".to_vec();
        match self.dir.strip_prefix(&self.start) {
            Ok(rest) => {
                title.extend_from_slice(&self.title);
                if !rest.as_os_str().is_empty() {
                    if !title.ends_with(b"/") {
                        title.push(b'/');
                    }
                    title.extend_from_slice(rest.as_os_str().as_bytes());
                }
            }
            Err(_) => title.extend_from_slice(self.dir.as_os_str().as_bytes()),
        }
        title
    }
}

/// Draws `entries` in a column `width` wide from `x`, scrolled to show `selected`, with each
/// entry's badge from `badges` in its last two cells.
fn column<'a>(
    screen: &mut Screen,
    (x, width, height): (u16, usize, u16),
    entries: impl Iterator<Item = &'a Entry>,
    selected: Option<(usize, Color)>,
    badges: &HashMap<OsString, Badge>,
    marked: &BTreeSet<OsString>,
) {
    let top = selected.map_or(0, |(at, _)| (at + 1).saturating_sub(usize::from(height)));
    for (row, (at, entry)) in (1..=height).zip(entries.enumerate().skip(top)) {
        let mut name = entry.name.as_bytes().to_vec();
        if entry.dir {
            name.push(b'/');
        }
        let flag = flagged(entry.name.as_bytes());
        let badge = badges.get(&entry.name).copied();
        let fg = if flag {
            Color::Red
        } else if marked.contains(&entry.name) {
            Color::Magenta
        } else if badge == Some(Badge::Ignored) {
            Color::BrightBlack
        } else if entry.dir {
            Color::Blue
        } else {
            Color::Default
        };
        let mut style = Style {
            bold: entry.dir,
            ..colour(fg)
        };
        if let Some((_, bg)) = selected.filter(|&(selected, _)| selected == at) {
            style.fg = Color::Black;
            style.bg = if flag { Color::Red } else { bg };
        }
        let mark = match badge {
            Some(Badge::Modified) => Some(("M", Color::Yellow)),
            Some(Badge::Added) => Some(("A", Color::Green)),
            Some(Badge::Untracked) => Some(("?", Color::Cyan)),
            Some(Badge::Ignored) | None => None,
        };
        let room = width.saturating_sub(if mark.is_some() { 3 } else { 1 });
        let name = clip(&name, room);
        let at = screen.put(row, x, format!("{name:<room$}"), style);
        if let Some((mark, fg)) = mark {
            let fg = if style.fg == Color::Black {
                style.fg
            } else {
                fg
            };
            let at = screen.put(row, at, " ", style);
            screen.put(row, at, mark, Style { fg, ..style });
        }
    }
}

impl Tool for Files {
    fn tick(&mut self) {
        match &mut self.pager {
            Some(pager) => pager.tick(),
            None => self.poll(),
        }
    }

    fn draw(&mut self, screen: &mut Screen) {
        if let Some(pager) = &mut self.pager {
            pager.draw(screen);
            return;
        }
        let (rows, columns) = screen.size();
        let (dim, red) = (colour(Color::BrightBlack), colour(Color::Red));
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        screen.put(0, 0, self.title(), bold);
        let place = format!("{}/{}  ? keys", self.selected + 1, self.shown.len());
        let at = columns.saturating_sub(u16::try_from(place.len()).unwrap_or(u16::MAX));
        screen.put(0, at, place, dim);
        if self.help {
            for (row, keys) in (2..rows).zip(KEYS) {
                screen.put(row, 2, keys, Style::default());
            }
            return;
        }
        let height = rows.saturating_sub(2);
        let status = rows.saturating_sub(1);
        if let Some((action, scroll)) = &self.confirm {
            let (question, paths) = action.lines();
            screen.put(1, 0, question, bold);
            let room = usize::from(height.saturating_sub(2));
            for (row, path) in (2..=height).zip(paths.iter().skip(*scroll).take(room)) {
                let name = path.file_name().unwrap_or(path.as_os_str()).as_bytes();
                let style = if flagged(name) { red } else { Style::default() };
                screen.put(row, 2, clip(name, usize::from(columns) - 2), style);
            }
            let more = paths.len().saturating_sub(scroll + room);
            let mut answer = "y yes  any other key no".to_owned();
            if *scroll > 0 || more > 0 {
                answer.push_str(&format!("  j k scroll ({more} more below)"));
            }
            screen.put(status, 0, answer, colour(Color::Yellow));
            return;
        }
        if let Some(jump) = &self.jump {
            for (row, &at) in (1..=height).zip(&jump.ranked) {
                let style = if row == 1 {
                    colour(Color::Blue)
                } else {
                    Style::default()
                };
                let style = Style {
                    bold: row == 1,
                    ..style
                };
                screen.put(
                    row,
                    2,
                    clip(jump.shown[at].as_bytes(), usize::from(columns) - 2),
                    style,
                );
            }
            let typed = format!(
                "jump {}  ({} of {})",
                jump.query,
                jump.ranked.len(),
                jump.shown.len()
            );
            screen.put(status, 0, typed, Style::default());
            return;
        }
        let left = columns / 5;
        let middle = columns * 3 / 10;
        let right = left + middle + 2;
        let width = usize::from(columns.saturating_sub(right));
        let here = self.dir.file_name();
        let up = self
            .parent
            .entries
            .iter()
            .position(|entry| Some(entry.name.as_os_str()) == here);
        let parent = (0, usize::from(left), height);
        column(
            screen,
            parent,
            self.parent.entries.iter(),
            up.map(|at| (at, Color::BrightBlack)),
            &HashMap::new(),
            &BTreeSet::new(),
        );
        match &self.listing {
            Ok(listing) => {
                let shown = self.shown.iter().map(|&at| &listing.entries[at]);
                let current = (left + 1, usize::from(middle), height);
                let selected = Some((self.selected, Color::Blue));
                column(screen, current, shown, selected, &self.badges, &self.marked);
            }
            Err(problem) => {
                screen.put(
                    1,
                    left + 1,
                    clip(problem.as_bytes(), usize::from(middle)),
                    red,
                );
            }
        }
        if self.preview.is_none() {
            self.preview = self
                .entry()
                .map(|entry| preview(&self.dir.join(&entry.name), entry.dir));
        }
        let lines: Vec<(String, Style)> = match &self.preview {
            None => Vec::new(),
            Some(Preview::Text(text)) => (text.lines())
                .take(usize::from(height))
                .map(|line| (clip(line.as_bytes(), width), Style::default()))
                .collect(),
            Some(Preview::Hex(bytes)) => {
                let per = (width.saturating_sub(6) / 4).clamp(1, 16);
                (bytes.chunks(per).enumerate())
                    .take(usize::from(height))
                    .map(|(at, chunk)| (hex(at * per, per, chunk), Style::default()))
                    .collect()
            }
            Some(Preview::Note(lines)) => lines.iter().map(|line| (line.clone(), dim)).collect(),
        };
        for (row, (line, style)) in (1..=height).zip(lines) {
            screen.put(row, right, line, style);
        }
        if let Some(name) = &self.naming {
            screen.put(status, 0, format!("rename to: {name}"), Style::default());
        } else if self.typing || !self.filter.is_empty() {
            screen.put(status, 0, format!("/{}", self.filter), Style::default());
        } else if let Some(problem) = &self.problem {
            let at = screen.put(status, 0, "✕ error", red);
            screen.put(status, at + 1, problem, Style::default());
        } else if let Some(mark) = self.mark {
            screen.put(
                status,
                0,
                format!("{mark} bookmark: a key"),
                Style::default(),
            );
        } else if let Some(note) = &self.git_note {
            screen.put(status, 0, note, colour(Color::Yellow));
        } else if let Ok(Listing {
            more: more @ 1.., ..
        }) = &self.listing
        {
            let note = format!("{more} more entries are not listed.");
            screen.put(status, 0, note, colour(Color::Yellow));
        }
    }

    fn key(&mut self, key: Key) -> Flow {
        if let Some(pager) = &mut self.pager {
            if pager.key(key) == Flow::Quit {
                self.pager = None;
            }
            return Flow::Continue;
        }
        self.problem = None;
        if let Some((action, scroll)) = self.confirm.take() {
            match key {
                Key::Char('y') => self.act(action),
                Key::Char('j') | Key::Down => self.confirm = Some((action, scroll + 1)),
                Key::Char('k') | Key::Up => self.confirm = Some((action, scroll.saturating_sub(1))),
                _ => {}
            }
            return Flow::Continue;
        }
        if self.naming.is_some() {
            self.name_key(key);
            return Flow::Continue;
        }
        if self.jump.is_some() {
            self.jump_key(key);
            return Flow::Continue;
        }
        if let Some(kind) = self.mark.take() {
            self.mark_key(kind, key);
            return Flow::Continue;
        }
        if self.help {
            self.help = false;
            return if key == Key::Char('q') {
                Flow::Quit
            } else {
                Flow::Continue
            };
        }
        if self.typing {
            match key {
                Key::Char(character) => self.filter.push(character),
                Key::Backspace => {
                    self.filter.pop();
                }
                Key::Enter => self.typing = false,
                Key::Esc => {
                    self.typing = false;
                    self.filter.clear();
                }
                _ => return Flow::Continue,
            }
            self.refilter();
            return Flow::Continue;
        }
        match key {
            Key::Char('q') => return Flow::Quit,
            Key::Char('?') => self.help = true,
            Key::Char('/') => self.typing = true,
            Key::Char(kind @ ('m' | '\'')) => self.mark = Some(kind),
            Key::Char('f') => {
                let paths = walk(&self.start);
                let shown = (paths.iter())
                    .map(|path| escape(path.as_os_str().as_bytes()))
                    .collect();
                self.jump = Some(Jump {
                    query: String::new(),
                    ranked: (0..paths.len().min(MAX_JUMPS)).collect(),
                    paths,
                    shown,
                });
            }
            Key::Char(' ') => {
                if let Some(name) = self.entry().map(|entry| entry.name.clone()) {
                    if !self.marked.remove(&name) {
                        self.marked.insert(name);
                    }
                    self.select(self.selected + 1);
                }
            }
            Key::Char('r') if self.entry().is_some() => self.naming = Some(String::new()),
            Key::Char('d') if self.entry().is_some() => {
                self.confirm = Some((Action::Trash(self.chosen()), 0));
            }
            Key::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            Key::Char('j') | Key::Down => self.select(self.selected + 1),
            Key::Char('k') | Key::Up => self.select(self.selected.saturating_sub(1)),
            Key::Char('g') | Key::Home => self.select(0),
            Key::Char('G') | Key::End => self.select(usize::MAX),
            Key::Char('h') | Key::Left => {
                if let Some(up) = self.dir.parent() {
                    let (up, here) = (up.to_path_buf(), self.dir.file_name().map(OsStr::to_owned));
                    self.go(up, here.as_deref());
                }
            }
            Key::Char('l') | Key::Right | Key::Enter => self.open(),
            _ => {}
        }
        Flow::Continue
    }
}

/// A row of the hex preview: the offset, each byte in hex, and the printable ones as text.
fn hex(offset: usize, per: usize, bytes: &[u8]) -> String {
    let mut line = format!("{offset:04x}  ");
    for byte in bytes {
        let _ = write!(line, "{byte:02x} ");
    }
    // The text lines up on a short last row.
    line.push_str(&" ".repeat(3 * per - 3 * bytes.len()));
    line.extend(bytes.iter().map(|&byte| {
        if byte.is_ascii_graphic() || byte == b' ' {
            char::from(byte)
        } else {
            '.'
        }
    }));
    line
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A fresh `root/work` in the temp directory, so the parent column holds only `work`.
    fn work(test: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("pinfold-files-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        std::fs::create_dir_all(work.join("docs/old")).unwrap();
        std::fs::write(work.join("docs/a.md"), "# A\n").unwrap();
        std::fs::write(
            work.join("notes.txt"),
            "First line\n\tindented\nred \x1b[31mtext\n",
        )
        .unwrap();
        let binary =
            b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03\x00>\x00hello\xff";
        std::fs::write(work.join("tool.bin"), binary).unwrap();
        (root, work)
    }

    /// The screen at 80×24 after `keys`, as rows and style runs.
    fn shown(work: &Path, keys: &[Key]) -> String {
        let mut files = Files::new(b"work".to_vec(), work.to_path_buf());
        for &key in keys {
            files.key(key);
        }
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        screen.runs()
    }

    #[test]
    fn columns() {
        let (root, work) = work("columns");
        insta::assert_snapshot!(shown(&work, &[]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_text_preview() {
        let (root, work) = work("text");
        insta::assert_snapshot!(shown(&work, &[Key::Char('j')]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hex_preview() {
        let (root, work) = work("hex");
        insta::assert_snapshot!(shown(&work, &[Key::Char('G')]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_escaped_name() {
        let (root, work) = work("escaped");
        // APFS refuses invalid UTF-8 in a name, so `\xNN` is left to `escape`'s own tests.
        // Inside a directory with a bidi control and BEL in its name, so the title shows it whole.
        let dir = work.join(OsStr::from_bytes(b"\xe2\x80\xaecod\x07"));
        std::fs::create_dir(&dir).unwrap();
        let name = OsStr::from_bytes(b"a\x1b]0;x\x07\xc2\x85b\x7f");
        std::fs::write(dir.join(name), "").unwrap();
        insta::assert_snapshot!(shown(&work, &[Key::Char('j'), Key::Enter]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_newline_an_esc_and_a_bidi_override_in_a_name_are_flagged() {
        let (root, work) = work("flagged");
        for name in ["new\nline", "esc\x1b[2Jape", "cat\u{202e}gnp.exe"] {
            assert!(flagged(name.as_bytes()), "{name:?}");
            std::fs::write(work.join(name), "").unwrap();
        }
        insta::assert_snapshot!(shown(&work, &[]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_a_name_escape_changes_is_flagged() {
        for name in [
            &b"a\x7f"[..],
            b"\xc2\x85",
            "\u{61c}".as_bytes(),
            b"\xff",
            b"\0",
        ] {
            assert!(flagged(name), "{name:?}");
        }
        for name in ["notes.txt", "^[ not ESC", "<U+202E>", "é 漢 ✕"] {
            assert!(!flagged(name.as_bytes()), "{name:?}");
        }
    }

    #[test]
    fn entries_past_the_cap_are_counted_on_screen() {
        let (root, work) = work("entries");
        let many = work.join("many");
        std::fs::create_dir(&many).unwrap();
        for at in 0..MAX_ENTRIES + 3 {
            std::fs::write(many.join(format!("{at:05}")), "").unwrap();
        }
        let listing = list(&many).unwrap();
        assert_eq!((listing.entries.len(), listing.more), (10_000, 3));
        // Which entries are kept follows the directory's order, so only the first and last rows.
        let screen = shown(&many, &[]);
        let lines: Vec<&str> = screen.lines().collect();
        let (rows, runs) = lines.split_at(24);
        let mut kept = vec![rows[0], rows[23]];
        kept.extend(
            runs.iter()
                .filter(|run| run.starts_with("0:") || run.starts_with("23:")),
        );
        insta::assert_snapshot!(kept.join("\n"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_text_preview_reads_up_to_its_cap() {
        let (root, work) = work("preview-cap");
        let path = work.join("long.txt");
        std::fs::write(&path, "line\n".repeat(MAX_PREVIEW)).unwrap();
        let Preview::Text(text) = preview(&path, false) else {
            panic!("not text")
        };
        assert_eq!(text.len(), 64 * 1024);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hex_preview_shows_up_to_its_cap() {
        let (root, work) = work("hex-cap");
        let path = work.join("long.bin");
        std::fs::write(&path, vec![0; MAX_PREVIEW]).unwrap();
        let Preview::Hex(bytes) = preview(&path, false) else {
            panic!("not hex")
        };
        assert_eq!(bytes.len(), 4 * 1024);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_on_an_executable_opens_it_in_the_reader_and_never_runs_it() {
        let (root, work) = work("executable");
        let marker = root.join("ran");
        let script = work.join("run.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut files = Files::new(b"work".to_vec(), work);
        for key in [
            Key::Char('/'),
            Key::Char('r'),
            Key::Char('u'),
            Key::Enter,
            Key::Enter,
        ] {
            assert_eq!(files.key(key), Flow::Continue);
        }
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        assert_eq!(screen.row(1).trim_end(), "#!/bin/sh");
        assert!(screen.row(2).starts_with("touch "), "{}", screen.row(2));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!marker.exists(), "the script ran");
        assert_eq!(
            files.key(Key::Char('q')),
            Flow::Continue,
            "q leaves the reader"
        );
        assert_eq!(files.key(Key::Char('q')), Flow::Quit);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fifo_is_neither_previewed_nor_opened() {
        let (root, work) = work("fifo");
        let fifo = work.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let mut files = Files::new(b"work".to_vec(), work);
        files.key(Key::Char('j'));
        files.key(Key::Char('j'));
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        assert!(
            screen.row(1).contains("No preview: not a regular file"),
            "{}",
            screen.row(1)
        );
        files.key(Key::Enter);
        screen.clear();
        files.draw(&mut screen);
        assert!(
            screen.row(23).contains("not a regular file"),
            "{}",
            screen.row(23)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn h_goes_up_with_the_directory_it_left_selected() {
        let (root, work) = work("up");
        let mut files = Files::new(b"work".to_vec(), work.clone());
        files.key(Key::Enter);
        assert_eq!(files.dir, work.join("docs"));
        files.key(Key::Char('h'));
        assert_eq!((files.dir.as_path(), files.selected), (work.as_path(), 0));
        files.key(Key::Char('h'));
        assert_eq!(
            files.entry().map(|entry| entry.name.as_os_str()),
            Some(OsStr::new("work"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Git in `dir`, through the one entry point, failing the test if it fails.
    fn git_in(dir: &Path, args: &[&str]) {
        let output = git::run(dir, args).unwrap();
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.is_some_and(|status| status.success()),
            "{args:?}: {errors}"
        );
    }

    /// `work` as a repository with one of each badge: `notes.txt` modified, `added.txt` added,
    /// `new.txt` untracked, `docs/` holding a modified file and `tool.bin` ignored.
    fn repo(test: &str) -> (PathBuf, PathBuf) {
        let (root, work) = work(test);
        let me = ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c"];
        git_in(&work, &["init", "-q"]);
        std::fs::write(work.join(".gitignore"), "tool.bin\n").unwrap();
        git_in(&work, &["add", ".gitignore", "notes.txt", "docs/a.md"]);
        git_in(
            &work,
            &[&me[..], &["commit.gpgsign=false", "commit", "-qm", "x"]].concat(),
        );
        std::fs::write(work.join("notes.txt"), "changed\n").unwrap();
        std::fs::write(work.join("docs/a.md"), "# B\n").unwrap();
        std::fs::write(work.join("added.txt"), "").unwrap();
        git_in(&work, &["add", "added.txt"]);
        std::fs::write(work.join("new.txt"), "").unwrap();
        (root, work)
    }

    /// Ticks until the directory has been checked and `git status` has answered.
    fn settle(files: &mut Files) {
        for _ in 0..1_000 {
            files.tick();
            if files.git.is_none() && files.checked.is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("git status never answered");
    }

    #[test]
    fn each_badge_from_a_scratch_repository() {
        let (root, work) = repo("badges");
        let mut files = Files::new(b"work".to_vec(), work);
        settle(&mut files);
        let mut badges: Vec<_> = files.badges.iter().collect();
        badges.sort();
        let expected = [
            ("added.txt", Badge::Added),
            ("docs", Badge::Modified),
            ("new.txt", Badge::Untracked),
            ("notes.txt", Badge::Modified),
            ("tool.bin", Badge::Ignored),
        ];
        let expected: Vec<_> = expected
            .map(|(name, badge)| (OsString::from(name), badge))
            .into();
        assert_eq!(
            badges,
            expected
                .iter()
                .map(|(name, badge)| (name, badge))
                .collect::<Vec<_>>()
        );
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        insta::assert_snapshot!(screen.runs());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hostile_status_output_parses_as_it_is() {
        let output = b"R  sub/new\nname\0sub/old\0?? sub/a b\"c\0!! sub/dir/\0!! sub/in/x.log\0 M top\0AM sub/in/y\0";
        let mut found: Vec<_> = badges(output, b"sub/").into_iter().collect();
        found.sort();
        let expected = [
            ("a b\"c", Badge::Untracked),
            ("dir", Badge::Ignored),
            ("in", Badge::Added),
            ("new\nname", Badge::Modified),
        ];
        assert_eq!(
            found,
            expected.map(|(name, badge)| (OsString::from(name), badge))
        );
    }

    #[test]
    fn git_status_over_its_cap_leaves_a_note_and_no_more_calls() {
        let (root, work) = repo("slow");
        let mut files = Files::new(b"work".to_vec(), work);
        settle(&mut files);
        let killed = git::Output {
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            cut_short: true,
        };
        files.show(b"", Ok(killed));
        assert!(files.badges.is_empty());
        (files.stamp, files.checked) = (None, None);
        files.tick();
        assert!(files.git.is_none(), "git ran again");
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        assert_eq!(
            screen.row(23).trim_end(),
            "git status ran over 2 s, so no badges here."
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_added_shows_after_the_next_check_with_its_badge() {
        let (root, work) = repo("refresh");
        let mut files = Files::new(b"work".to_vec(), work.clone());
        settle(&mut files);
        std::fs::write(work.join("later.txt"), "").unwrap();
        files.tick();
        assert!(
            !files.badges.contains_key(OsStr::new("later.txt")),
            "checked within a second"
        );
        files.checked = None;
        settle(&mut files);
        let names: Vec<_> = files
            .entries()
            .iter()
            .map(|entry| entry.name.clone())
            .collect();
        assert!(names.contains(&OsString::from("later.txt")), "{names:?}");
        assert_eq!(
            files.badges.get(OsStr::new("later.txt")),
            Some(&Badge::Untracked)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fuzzy_jump_ranks_word_starts_and_runs_first_then_shorter_paths() {
        let paths = [
            "fixtures/i_l_e/s.txt",
            "src/old/files.rs",
            "src/files.rs",
            "profiles.toml",
            "docs/notes.md",
        ]
        .map(String::from);
        let ranked: Vec<&str> = rank("files", &paths)
            .iter()
            .map(|&at| paths[at].as_str())
            .collect();
        assert_eq!(
            ranked,
            [
                "src/files.rs",
                "src/old/files.rs",
                "fixtures/i_l_e/s.txt",
                "profiles.toml"
            ]
        );
        assert_eq!(
            rank("FILES", &paths),
            rank("files", &paths),
            "case is ignored"
        );
    }

    #[test]
    fn fuzzy_jump_lands_on_a_file_in_its_directory() {
        let (root, work) = work("jump");
        let mut files = Files::new(b"work".to_vec(), work.clone());
        for key in [
            Key::Char('f'),
            Key::Char('a'),
            Key::Char('.'),
            Key::Char('m'),
            Key::Enter,
        ] {
            files.key(key);
        }
        assert_eq!(files.dir, work.join("docs"));
        assert_eq!(
            files.entry().map(|entry| entry.name.as_os_str()),
            Some(OsStr::new("a.md"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_bookmark_goes_back_to_its_directory() {
        let (root, work) = work("marks");
        let mut files = Files::new(b"work".to_vec(), work.clone());
        for key in [Key::Char('m'), Key::Char('w'), Key::Enter, Key::Enter] {
            files.key(key);
        }
        assert_eq!(files.dir, work.join("docs/old"));
        files.key(Key::Char('\''));
        files.key(Key::Char('w'));
        assert_eq!(files.dir, work);
        files.key(Key::Char('\''));
        files.key(Key::Char('x'));
        assert_eq!(
            (files.dir.as_path(), files.problem.as_deref()),
            (work.as_path(), Some("no bookmark x"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Presses each key in turn.
    fn press(files: &mut Files, keys: &str) {
        for character in keys.chars() {
            let key = match character {
                '\n' => Key::Enter,
                character => Key::Char(character),
            };
            files.key(key);
        }
    }

    #[test]
    fn a_trash_confirmation_lists_a_hostile_name_escaped_and_waits_for_y() {
        let (root, work) = work("trash");
        std::fs::remove_dir_all(work.join("docs")).unwrap();
        let hostile = "evil\x1b[2J\u{202e}txt.exe";
        std::fs::write(work.join(hostile), "").unwrap();
        let mut files = Files::new(b"work".to_vec(), work.clone());
        files.trash = Some((root.join("Trash"), true));
        press(&mut files, " d");
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        insta::assert_snapshot!(screen.runs());
        press(&mut files, "n");
        assert!(work.join(hostile).exists(), "trashed without a yes");
        press(&mut files, " dy");
        assert!(!work.join(hostile).exists());
        assert!(root.join("Trash/files").join(hostile).exists());
        assert!(
            root.join("Trash/files/notes.txt").exists(),
            "the selection moved on"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rename_asks_first_and_refuses_a_name_that_leaves_the_directory() {
        let (root, work) = work("rename");
        let mut files = Files::new(b"work".to_vec(), work.clone());
        press(&mut files, "jr../out\n");
        assert_eq!(files.problem.as_deref(), Some("../out is not a name"));
        press(&mut files, "rplan.txt\n");
        assert!(files.confirm.is_some() && work.join("notes.txt").exists());
        press(&mut files, "y");
        assert!(work.join("plan.txt").exists() && !work.join("notes.txt").exists());
        assert_eq!(
            files.entry().map(|entry| entry.name.as_os_str()),
            Some(OsStr::new("plan.txt"))
        );
        press(&mut files, "rtool.bin\ny");
        let refused = files.problem.clone().unwrap_or_default();
        assert!(
            refused.ends_with("tool.bin exists; nothing was replaced"),
            "{refused}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_jump_walk_stops_at_the_entry_cap_and_skips_dot_git() {
        let (root, work) = work("walk");
        std::fs::create_dir(work.join(".git")).unwrap();
        std::fs::write(work.join(".git/HEAD"), "").unwrap();
        for at in 0..MAX_ENTRIES {
            std::fs::write(work.join("docs/old").join(format!("{at:05}")), "").unwrap();
        }
        let paths = walk(&work);
        assert_eq!(paths.len(), 10_000);
        assert!(
            !paths.contains(&PathBuf::from(".git/HEAD")),
            "went into .git"
        );
        assert!(
            paths.contains(&PathBuf::from("docs/a.md")),
            "kept the shallow paths"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
