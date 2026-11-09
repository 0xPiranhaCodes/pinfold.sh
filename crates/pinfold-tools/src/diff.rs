//! The diff viewer behind `pinfold diff` (ADR-0045 sections 2, 3 and 6): the files on the
//! left and the selected one's hunks on the right, unified or side by side, with the changed
//! words of each removed line and the added line beside it highlighted.
//!
//! Git's output is untrusted: names and lines come from the repository. [`parse`] takes any
//! bytes without panicking, keeps each line as a range into them, and caps each structure;
//! [`Diff::notes`] says which cap cut it, and the viewer shows them above the diff. Every name
//! and line reaches the screen through [`Screen::put`], which escapes it.

use std::collections::HashMap;
use std::iter;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffTag};

use crate::keys::Key;
use crate::md::colour;
use crate::screen::{Color, Screen, Style, escape};
use crate::{Flow, Tool, git};

/// Files kept; the rest of the diff is dropped.
const MAX_FILES: usize = 10_000;
/// Hunks kept across every file.
const MAX_HUNKS: usize = 100_000;
/// Lines kept across every file, at 16 bytes a range at most 8 MB.
const MAX_LINES: usize = 500_000;
/// Bytes kept of a line; the rest of it is cut.
const MAX_LINE: usize = 10_000;
/// Longest line, prefix included, whose changed words are highlighted.
const MAX_WORDS_LINE: usize = 1024;
/// Time `similar` may spend on one hunk's lines on one draw, after which it marks whole lines.
const WORDS_DEADLINE: Duration = Duration::from_millis(50);
/// Least time between two runs of git in watch mode.
const WATCH_EVERY: Duration = Duration::from_secs(2);
/// Times the last run's duration that watch mode waits, if longer than [`WATCH_EVERY`].
const WATCH_TIMES: u32 = 20;

/// A parsed `git diff`, which owns the bytes its ranges point into.
pub(crate) struct Diff {
    pub(crate) bytes: Vec<u8>,
    pub(crate) files: Vec<File>,
    /// One sentence per cap that cut the diff.
    pub(crate) notes: Vec<String>,
    /// Whether every line git printed was parsed, so the last hunk is whole.
    pub(crate) whole: bool,
}

/// One file's part of the diff, from its `diff --git` line.
pub(crate) struct File {
    /// Where its `diff --git` line starts.
    pub(crate) start: usize,
    /// The path before and after, the same unless the file was renamed or copied.
    pub(crate) old: Vec<u8>,
    pub(crate) new: Vec<u8>,
    /// The lines before the first hunk, such as `rename from`, `new mode` and `Binary files`.
    pub(crate) header: Vec<Range<usize>>,
    pub(crate) hunks: Vec<Hunk>,
}

pub(crate) struct Hunk {
    /// The `@@ -1,2 +1,3 @@` line.
    pub(crate) header: Range<usize>,
    /// Lines that start with ` `, `+`, `-` or `\`, or are empty.
    pub(crate) lines: Vec<Range<usize>>,
}

impl File {
    /// The path, or `old → new` for a rename or copy.
    pub(crate) fn label(&self) -> Vec<u8> {
        if self.old == self.new {
            return self.new.clone();
        }
        [&self.old[..], " → ".as_bytes(), &self.new].concat()
    }
}

const KEYS: [&str; 10] = [
    "n p                 the next or previous file",
    "s                   stage the hunk at the top, after y",
    "r                   revert the hunk at the top, after y",
    "v                   unified or side by side",
    "w                   watch: run git again every 2 s or more",
    "j k  ↓ ↑            a line down or up",
    "space b  PgDn PgUp  a page down or up",
    "g G  Home End       the top or the end",
    "?                   these keys",
    "q                   quit",
];

/// Runs `git diff` in `repo` through [`git::run`] and parses it, or says why it could not.
pub(crate) fn load(repo: &Path, staged: bool, range: Option<&str>) -> Result<Diff, String> {
    // Fixed prefixes, so `diff.noprefix` or `diff.mnemonicPrefix` cannot change the paths.
    let mut args = vec!["diff", "--src-prefix=a/", "--dst-prefix=b/"];
    args.extend(staged.then_some("--staged"));
    args.extend(range);
    args.push("--");
    let output = git::run(repo, args).map_err(|error| format!("cannot run git: {error}"))?;
    // Git cut short at the byte cap dies of SIGPIPE, which is not an error here.
    if !output.cut_short && output.status.is_some_and(|status| !status.success()) {
        let errors = String::from_utf8_lossy(&output.stderr);
        let first = errors.lines().next().unwrap_or_default();
        return Err(format!("git diff failed: {first}"));
    }
    let mut diff = parse(output.stdout);
    if output.cut_short {
        let why = match output.status {
            None => format!("git ran over {} s", git::TIMEOUT.as_secs()),
            Some(_) => format!("git printed over {} MB", git::MAX_OUTPUT / 1_000_000),
        };
        diff.notes
            .insert(0, format!("{why}, so the diff is cut short."));
        diff.whole = false;
    }
    Ok(diff)
}

/// Parses the output of `git diff`, which may be cut short anywhere.
pub(crate) fn parse(bytes: Vec<u8>) -> Diff {
    let mut files: Vec<File> = Vec::new();
    let (mut hunks, mut lines, mut long) = (0, 0, false);
    let (mut notes, mut whole) = (Vec::new(), true);
    let mut start = 0;
    while start < bytes.len() {
        let end = (bytes[start..].iter().position(|&byte| byte == b'\n'))
            .map_or(bytes.len(), |at| start + at);
        let line = &bytes[start..end];
        let range = start..cut(&bytes, start, end, &mut long);
        start = end + 1;
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            if files.len() == MAX_FILES {
                notes.push(format!("Only the first {MAX_FILES} files show."));
                whole = false;
                break;
            }
            let (old, new) = git_paths(rest);
            files.push(File {
                start: range.start,
                old,
                new,
                header: Vec::new(),
                hunks: Vec::new(),
            });
            continue;
        }
        // Anything before the first file, such as a warning, is not part of the diff.
        let Some(file) = files.last_mut() else {
            continue;
        };
        if lines == MAX_LINES {
            notes.push(format!("Only the first {MAX_LINES} lines show."));
            whole = false;
            break;
        }
        lines += 1;
        if line.starts_with(b"@@") {
            if hunks == MAX_HUNKS {
                notes.push(format!("Only the first {MAX_HUNKS} hunks show."));
                whole = false;
                break;
            }
            hunks += 1;
            let lines = Vec::new();
            file.hunks.push(Hunk {
                header: range,
                lines,
            });
            continue;
        }
        if let Some(hunk) = file.hunks.last_mut() {
            hunk.lines.push(range);
            continue;
        }
        if let Some(rest) = line
            .strip_prefix(b"--- ")
            .filter(|rest| rest != b"/dev/null")
        {
            file.old = strip(unquote(trim(rest)).0, b"a/");
        } else if let Some(rest) = line
            .strip_prefix(b"+++ ")
            .filter(|rest| rest != b"/dev/null")
        {
            file.new = strip(unquote(trim(rest)).0, b"b/");
        } else if let Some(rest) =
            (line.strip_prefix(b"rename from ")).or_else(|| line.strip_prefix(b"copy from "))
        {
            file.old = unquote(rest).0;
        } else if let Some(rest) =
            (line.strip_prefix(b"rename to ")).or_else(|| line.strip_prefix(b"copy to "))
        {
            file.new = unquote(rest).0;
        }
        file.header.push(range);
    }
    if long {
        notes.push(format!("Lines over {MAX_LINE} bytes are cut."));
    }
    Diff {
        bytes,
        files,
        notes,
        whole,
    }
}

impl Diff {
    /// A patch of file `file`'s hunk `hunk` alone: the bytes git printed from the file's
    /// `diff --git` line to its first hunk, then the hunk's, uncut. None if there is no such hunk
    /// or the diff is not whole, as the last hunk may then be cut.
    fn patch(&self, file: usize, hunk: usize) -> Option<Vec<u8>> {
        let this = self.files.get(file)?;
        let header = self
            .bytes
            .get(this.start..this.hunks.first()?.header.start)?;
        let start = this.hunks.get(hunk)?.header.start;
        let end = (this.hunks.get(hunk + 1).map(|next| next.header.start))
            .or_else(|| self.files.get(file + 1).map(|next| next.start))
            .unwrap_or(self.bytes.len());
        let body = self.bytes.get(start..end).filter(|_| self.whole)?;
        Some([header, body].concat())
    }
}

/// Where the line from `start` to `end` ends once cut at [`MAX_LINE`] bytes, at the start of
/// a character so a cut one does not show as `\xNN`.
fn cut(bytes: &[u8], start: usize, end: usize, long: &mut bool) -> usize {
    if end - start <= MAX_LINE {
        return end;
    }
    *long = true;
    let mut at = start + MAX_LINE;
    for _ in 0..3 {
        if bytes[at] & 0xC0 == 0x80 {
            at -= 1;
        }
    }
    at
}

/// The two paths of a `diff --git` line. Unquoted ones are split in the middle, which is right
/// unless the file was renamed or copied; then the lines below name both.
fn git_paths(rest: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if rest.starts_with(b"\"") {
        let (old, rest) = unquote(rest);
        let (new, _) = unquote(rest.strip_prefix(b" ").unwrap_or(rest));
        return (strip(old, b"a/"), strip(new, b"b/"));
    }
    let half = rest.len() / 2;
    match (rest.get(..half), rest.get(half), rest.get(half + 1..)) {
        (Some(old), Some(b' '), Some(new)) if old.get(2..) == new.get(2..) => {
            (strip(old.to_vec(), b"a/"), strip(new.to_vec(), b"b/"))
        }
        _ => (rest.to_vec(), rest.to_vec()),
    }
}

/// `path` without `prefix`.
fn strip(path: Vec<u8>, prefix: &[u8]) -> Vec<u8> {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.to_vec(),
        None => path,
    }
}

/// A `---` or `+++` path without the tab git adds after one that holds a space.
fn trim(path: &[u8]) -> &[u8] {
    path.strip_suffix(b"\t").unwrap_or(path)
}

/// A path as git prints it, in double quotes with C escapes if it holds anything unusual, and
/// the bytes after it. An unquoted path is all of `text`.
fn unquote(text: &[u8]) -> (Vec<u8>, &[u8]) {
    let Some(mut rest) = text.strip_prefix(b"\"") else {
        return (text.to_vec(), &[]);
    };
    let mut path = Vec::new();
    while let Some((&byte, after)) = rest.split_first() {
        rest = after;
        if byte == b'"' {
            break;
        }
        if byte != b'\\' {
            path.push(byte);
            continue;
        }
        let Some((&code, after)) = rest.split_first() else {
            break;
        };
        rest = after;
        path.push(match code {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'0'..=b'7' => {
                let mut value = code - b'0';
                for _ in 0..2 {
                    if let Some((&digit @ b'0'..=b'7', after)) = rest.split_first() {
                        value = value.wrapping_mul(8).wrapping_add(digit - b'0');
                        rest = after;
                    }
                }
                value
            }
            other => other,
        });
    }
    (path, rest)
}

/// Where a viewer's diff comes from, so watch mode can run git again.
pub(crate) struct Source {
    pub(crate) repo: PathBuf,
    pub(crate) staged: bool,
    pub(crate) range: Option<String>,
}

/// What one run of git in watch mode found, and how long it took.
type Loaded = (Result<Diff, String>, Duration);

/// Watch mode. Git runs on a thread of its own, one run at a time, so the viewer never waits.
struct Watch {
    source: Source,
    on: bool,
    /// Run git once as soon as no run is in progress, watching or not, as after a stage.
    again: bool,
    /// When git may run next.
    next: Instant,
    /// The run in progress.
    running: Option<Receiver<Loaded>>,
    /// Why the last run failed; the diff shown is the one before it.
    failed: Option<String>,
}

/// How long watch mode waits after a run of git that took `last`.
fn watch_interval(last: Duration) -> Duration {
    last.saturating_mul(WATCH_TIMES).max(WATCH_EVERY)
}

/// A hunk's lines: one context line, or removed lines and the added lines after them.
enum Block {
    Same(usize),
    Change(Range<usize>, Range<usize>),
}

fn blocks<'a>(bytes: &'a [u8], lines: &'a [Range<usize>]) -> impl Iterator<Item = Block> + 'a {
    let starts = move |at: usize, prefix: u8| {
        (lines.get(at))
            .is_some_and(|range| bytes.get(range.clone()).and_then(<[u8]>::first) == Some(&prefix))
    };
    let mut at = 0;
    iter::from_fn(move || {
        if at >= lines.len() {
            return None;
        }
        let start = at;
        while starts(at, b'-') {
            at += 1;
        }
        let middle = at;
        while starts(at, b'+') {
            at += 1;
        }
        if at == start {
            at += 1;
            return Some(Block::Same(start));
        }
        Some(Block::Change(start..middle, middle..at))
    })
}

/// One row of the selected file: a header across the view, or hunk lines, each with the hunk's
/// index and, for a change, the line it pairs with for word highlights.
enum Row {
    /// A file or hunk header.
    Full(Range<usize>, Color),
    /// A unified line and its pair.
    One(usize, Range<usize>, Option<Range<usize>>),
    /// The old and new sides; a context line is on both.
    Two(usize, Option<Range<usize>>, Option<Range<usize>>),
}

/// The changed words of `old` and `new`, a removed line and the added line beside it, as ranges
/// into each. None on a line over [`MAX_WORDS_LINE`] bytes; past `deadline`, whole lines.
fn words(old: &[u8], new: &[u8], deadline: Instant) -> [Vec<Range<usize>>; 2] {
    if old.len() > MAX_WORDS_LINE || new.len() > MAX_WORDS_LINE {
        return Default::default();
    }
    let (old_starts, new_starts) = (split_words(old), split_words(new));
    let (old_words, new_words) = (tokens(old, &old_starts), tokens(new, &new_starts));
    let ops = similar::capture_diff_slices_deadline(
        Algorithm::Myers,
        &old_words,
        &new_words,
        Some(deadline),
    );
    let (mut old_marks, mut new_marks): (Vec<Range<usize>>, _) = (Vec::new(), Vec::new());
    for op in ops {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        for (marks, starts, range) in [
            (&mut old_marks, &old_starts, old_range),
            (&mut new_marks, &new_starts, new_range),
        ] {
            let (Some(&start), Some(&end)) = (starts.get(range.start), starts.get(range.end))
            else {
                continue;
            };
            match marks.last_mut() {
                _ if start == end => {}
                Some(last) if last.end == start => last.end = end,
                _ => marks.push(start..end),
            }
        }
    }
    [old_marks, new_marks]
}

/// The words of `line` that `starts` gives.
fn tokens<'a>(line: &'a [u8], starts: &[usize]) -> Vec<&'a [u8]> {
    let tokens = starts.windows(2).map(|pair| line.get(pair[0]..pair[1]));
    tokens.map(Option::unwrap_or_default).collect()
}

/// Where each word of `line` after its `-` or `+` starts, and where the last ends: runs of
/// letters, digits, `_` and non-ASCII bytes, so a character is never split, runs of spaces, and
/// each other byte alone.
fn split_words(line: &[u8]) -> Vec<usize> {
    let class = |byte: u8| match byte {
        b'_' | 0x80.. => 0,
        _ if byte.is_ascii_alphanumeric() => 0,
        _ if byte.is_ascii_whitespace() => 1,
        _ => 2,
    };
    let mut at = line.len().min(1);
    let mut starts = vec![at];
    while let Some(&byte) = line.get(at) {
        at += 1;
        while class(byte) != 2 && line.get(at).is_some_and(|&next| class(next) == class(byte)) {
            at += 1;
        }
        starts.push(at);
    }
    starts
}

/// Draws `line` from `column` in at most `width` cells, tabs as four spaces, and the bytes in
/// `marks` in `marked`.
fn put_line(
    screen: &mut Screen,
    (row, column, width): (u16, u16, u16),
    line: &[u8],
    style: Style,
    marks: &[Range<usize>],
    marked: Style,
) {
    let end = column.saturating_add(width);
    let (mut column, mut at) = (column, 0);
    let mut parts = Vec::new();
    for mark in marks {
        parts.extend([(at..mark.start, style), (mark.clone(), marked)]);
        at = mark.end;
    }
    parts.push((at..line.len(), style));
    for (range, style) in parts {
        let Some(part) = line.get(range) else {
            continue;
        };
        let part = part
            .split(|&byte| byte == b'\t')
            .collect::<Vec<_>>()
            .join(&b"    "[..]);
        let part: String = escape(&part)
            .chars()
            .take(usize::from(end.saturating_sub(column)))
            .collect();
        column = screen.put(row, column, part, style);
    }
}

/// The diff viewer.
pub(crate) struct Viewer {
    title: Vec<u8>,
    diff: Diff,
    selected: usize,
    top: usize,
    page: usize,
    help: bool,
    side: bool,
    /// The visible lines' changed words from the last draw, by the starts of a pair's lines.
    words: HashMap<(usize, usize), [Vec<Range<usize>>; 2]>,
    watch: Option<Watch>,
    /// A stage or revert waiting for `y`.
    confirm: Option<Confirm>,
    /// Why the last stage or revert did not happen, until the next key.
    notice: Option<String>,
}

/// A stage (`git apply --cached`) or revert (`git apply -R`) of one hunk, drawn in the pane until
/// `y` applies it or another key drops it. Only the window's keys reach a tool pane, so no API
/// caller can answer it (ADR-0045 section 4).
struct Confirm {
    revert: bool,
    label: Vec<u8>,
    header: Vec<u8>,
    patch: Vec<u8>,
}

impl Viewer {
    /// A viewer titled `title`, such as `diff --staged`, for `diff`.
    pub(crate) fn new(title: Vec<u8>, diff: Diff) -> Self {
        Self {
            title,
            diff,
            selected: 0,
            top: 0,
            page: 1,
            help: false,
            side: false,
            words: HashMap::new(),
            watch: None,
            confirm: None,
            notice: None,
        }
    }

    /// This viewer, able to run git again for `source` with `w`, and watching if `on`. The
    /// first run took `took`.
    pub(crate) fn watch(mut self, source: Source, on: bool, took: Duration) -> Self {
        self.watch = Some(Watch {
            source,
            on,
            again: false,
            next: Instant::now() + watch_interval(took),
            running: None,
            failed: None,
        });
        self
    }

    /// Shows `diff`, keeping the selected file if it is still there.
    fn reload(&mut self, diff: Diff) {
        let label = self.diff.files.get(self.selected).map(File::label);
        let found =
            label.and_then(|label| diff.files.iter().position(|file| file.label() == label));
        self.selected = found
            .unwrap_or(self.selected)
            .min(diff.files.len().saturating_sub(1));
        self.diff = diff;
        self.words.clear();
    }

    /// Calls `each` with the selected file's rows in order, until it returns false.
    fn each_row(&self, mut each: impl FnMut(Row) -> bool) -> Option<()> {
        let mut emit = |row| each(row).then_some(());
        let file = self.diff.files.get(self.selected)?;
        for range in &file.header {
            emit(Row::Full(range.clone(), Color::BrightBlack))?;
        }
        for (index, hunk) in file.hunks.iter().enumerate() {
            emit(Row::Full(hunk.header.clone(), Color::Cyan))?;
            let line = |at: usize| hunk.lines.get(at).cloned();
            let pair = |lines: &Range<usize>, at: usize| {
                line(lines.start + at).filter(|_| at < lines.len())
            };
            for block in blocks(&self.diff.bytes, &hunk.lines) {
                match block {
                    Block::Same(at) if self.side => emit(Row::Two(index, line(at), line(at)))?,
                    Block::Same(at) => emit(Row::One(index, line(at)?, None))?,
                    Block::Change(old, new) if self.side => {
                        for at in 0..old.len().max(new.len()) {
                            emit(Row::Two(index, pair(&old, at), pair(&new, at)))?;
                        }
                    }
                    Block::Change(old, new) => {
                        for at in 0..old.len() {
                            emit(Row::One(index, pair(&old, at)?, pair(&new, at)))?;
                        }
                        for at in 0..new.len() {
                            emit(Row::One(index, pair(&new, at)?, pair(&old, at)))?;
                        }
                    }
                }
            }
        }
        Some(())
    }

    /// The selected file's hunk shown at the top of the view, or the first one below it.
    fn hunk_at_top(&self) -> Option<usize> {
        let (mut at, mut found) = (0, None);
        self.each_row(|row| {
            at += 1;
            if at > self.top {
                found = match row {
                    Row::One(hunk, ..) | Row::Two(hunk, ..) => Some(hunk),
                    Row::Full(..) => None,
                };
            }
            found.is_none()
        });
        found
    }

    /// Asks to stage, or revert if `revert`, the hunk at the top of the view.
    fn ask(&mut self, revert: bool) {
        let source = self.watch.as_ref().map(|watch| &watch.source);
        if !source.is_some_and(|source| !source.staged && source.range.is_none()) {
            self.notice = Some("Stage and revert work on the work tree's diff alone.".into());
            return;
        }
        let file = self.selected;
        let found = self.hunk_at_top().and_then(|hunk| {
            let header = self.diff.files.get(file)?.hunks.get(hunk)?.header.clone();
            Some((
                text(&self.diff.bytes, &header),
                self.diff.patch(file, hunk)?,
            ))
        });
        let Some((header, patch)) = found else {
            self.notice = Some(if self.diff.whole {
                "No hunk to stage or revert here.".into()
            } else {
                "The diff is cut short, so stage and revert are off.".into()
            });
            return;
        };
        self.confirm = Some(Confirm {
            revert,
            label: self
                .diff
                .files
                .get(file)
                .map(File::label)
                .unwrap_or_default(),
            header,
            patch,
        });
    }

    /// Applies `confirm` with `git apply`, then runs `git diff` again once no run is in progress.
    fn apply(&mut self, confirm: &Confirm) {
        let Some(watch) = &mut self.watch else {
            return;
        };
        match git::apply(&watch.source.repo, &confirm.patch, confirm.revert) {
            Ok(output) if output.status.is_some_and(|status| status.success()) => {}
            Ok(output) => {
                let errors = String::from_utf8_lossy(&output.stderr);
                let first = errors.lines().next().unwrap_or("git was killed");
                self.notice = Some(format!("git apply failed: {first}"));
            }
            Err(error) => self.notice = Some(format!("cannot run git: {error}")),
        }
        watch.again = true;
    }

    fn last_top(&self) -> usize {
        let mut count = 0_usize;
        self.each_row(|_| {
            count += 1;
            true
        });
        count.saturating_sub(self.page)
    }

    /// The changed words of `old` and `new`, from the last draw or worked out now within the
    /// deadline of `hunk`.
    fn marks(
        &mut self,
        last: &mut HashMap<(usize, usize), [Vec<Range<usize>>; 2]>,
        deadline: &mut Option<(usize, Instant)>,
        (hunk, old, new): (usize, &Range<usize>, &Range<usize>),
    ) -> [Vec<Range<usize>>; 2] {
        let key = (old.start, new.start);
        if let Some(marks) = self.words.get(&key) {
            return marks.clone();
        }
        let marks = last.remove(&key).unwrap_or_else(|| {
            let deadline = match *deadline {
                Some((index, at)) if index == hunk => at,
                _ => deadline.insert((hunk, Instant::now() + WORDS_DEADLINE)).1,
            };
            let bytes = &self.diff.bytes;
            let line = |range: &Range<usize>| bytes.get(range.clone()).unwrap_or_default();
            words(line(old), line(new), deadline)
        });
        self.words.insert(key, marks.clone());
        marks
    }
}

/// The bytes of `range`, or none if it is out of `bytes`.
fn text(bytes: &[u8], range: &Range<usize>) -> Vec<u8> {
    bytes.get(range.clone()).unwrap_or_default().to_vec()
}

/// The colour of a hunk line, and of its changed words.
fn line_styles(line: &[u8]) -> (Style, Style) {
    let (fg, bg) = match line.first() {
        Some(b'+') => (Color::Green, Color::Green),
        Some(b'-') => (Color::Red, Color::Red),
        Some(b'\\') => (Color::BrightBlack, Color::Default),
        _ => (Color::Default, Color::Default),
    };
    let marked = Style {
        fg: Color::Black,
        bg,
        bold: false,
    };
    (colour(fg), marked)
}

impl Tool for Viewer {
    fn tick(&mut self) {
        let Some(watch) = &mut self.watch else {
            return;
        };
        if let Some(running) = &watch.running {
            let (loaded, took) = match running.try_recv() {
                Ok(loaded) => loaded,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => (Err("git stopped".to_owned()), WATCH_EVERY),
            };
            watch.running = None;
            watch.next = Instant::now() + watch_interval(took);
            watch.failed = loaded.as_ref().err().cloned();
            if let Ok(diff) = loaded {
                self.reload(diff);
            }
            return;
        }
        if !watch.again && (!watch.on || Instant::now() < watch.next) {
            return;
        }
        watch.again = false;
        let (sender, receiver) = mpsc::channel();
        let Source {
            repo,
            staged,
            range,
        } = &watch.source;
        let (repo, staged, range) = (repo.clone(), *staged, range.clone());
        let spawned = thread::Builder::new().name("git".into()).spawn(move || {
            let started = Instant::now();
            let loaded = load(&repo, staged, range.as_deref());
            let _ = sender.send((loaded, started.elapsed()));
        });
        match spawned {
            Ok(_) => watch.running = Some(receiver),
            Err(error) => {
                watch.failed = Some(format!("cannot run git: {error}"));
                watch.next = Instant::now() + WATCH_EVERY;
            }
        }
    }

    fn draw(&mut self, screen: &mut Screen) {
        let (rows, columns) = screen.size();
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let dim = colour(Color::BrightBlack);
        screen.put(0, 0, &self.title, bold);
        let files = self.diff.files.len();
        let watching = if self.watch.as_ref().is_some_and(|watch| watch.on) {
            "watching  "
        } else {
            ""
        };
        let place = format!(
            "{watching}{}/{files}  ? keys",
            (self.selected + 1).min(files)
        );
        let at = columns.saturating_sub(u16::try_from(place.chars().count()).unwrap_or(u16::MAX));
        screen.put(0, at, place, dim);
        if self.help {
            for (row, keys) in (2..rows).zip(KEYS) {
                screen.put(row, 2, keys, Style::default());
            }
            return;
        }
        if let Some(confirm) = &self.confirm {
            let what = if confirm.revert {
                "Revert this hunk of "
            } else {
                "Stage this hunk of "
            };
            let column = screen.put(2, 2, "◆ ", colour(Color::Yellow));
            let column = screen.put(2, column, what, bold);
            let column = screen.put(2, column, &confirm.label, bold);
            screen.put(2, column, "?", bold);
            screen.put(3, 4, &confirm.header, colour(Color::Cyan));
            screen.put(5, 4, "y yes, any other key no", dim);
            return;
        }
        let mut top = 1;
        let failed = self.watch.as_ref().and_then(|watch| watch.failed.as_ref());
        for failed in self.notice.iter().chain(failed) {
            let column = screen.put(top, 0, "✕ error", colour(Color::Red));
            screen.put(top, column + 1, failed, Style::default());
            top += 1;
        }
        for note in &self.diff.notes {
            screen.put(top, 0, note, colour(Color::Yellow));
            top += 1;
        }
        if files == 0 {
            screen.put(top, 0, "No changes.", dim);
            return;
        }
        self.page = usize::from(rows.saturating_sub(top)).max(1);
        self.top = self.top.min(self.last_top());
        let left = columns / 4;
        let width = usize::from(left.saturating_sub(1));
        let first = (self.selected + 1).saturating_sub(self.page);
        for (row, (at, file)) in (top..rows).zip(self.diff.files.iter().enumerate().skip(first)) {
            let label: String = escape(&file.label()).chars().take(width).collect();
            let style = if at == self.selected {
                Style {
                    fg: Color::Black,
                    bg: Color::Blue,
                    bold: false,
                }
            } else {
                Style::default()
            };
            screen.put(row, 0, format!("{label:<width$}"), style);
        }
        let (mut skip, mut shown) = (self.top, Vec::new());
        self.each_row(|row| {
            if skip > 0 {
                skip -= 1;
                return true;
            }
            shown.push(row);
            shown.len() < self.page
        });
        let mut last = std::mem::take(&mut self.words);
        let mut deadline = None;
        let (start, half) = (left + 1, columns.saturating_sub(left + 1) / 2);
        for (row, line) in (top..rows).zip(shown) {
            match line {
                Row::Full(range, fg) => {
                    put_line(
                        screen,
                        (row, start, columns),
                        &text(&self.diff.bytes, &range),
                        colour(fg),
                        &[],
                        colour(fg),
                    );
                }
                Row::One(hunk, line, pair) => {
                    let shown = text(&self.diff.bytes, &line);
                    let (style, marked) = line_styles(&shown);
                    let marks = match pair {
                        Some(pair) if shown.first() == Some(&b'-') => {
                            self.marks(&mut last, &mut deadline, (hunk, &line, &pair))[0].clone()
                        }
                        Some(pair) => {
                            self.marks(&mut last, &mut deadline, (hunk, &pair, &line))[1].clone()
                        }
                        None => Vec::new(),
                    };
                    put_line(screen, (row, start, columns), &shown, style, &marks, marked);
                }
                Row::Two(hunk, old, new) => {
                    let marks = match (&old, &new) {
                        (Some(old), Some(new)) if old != new => {
                            self.marks(&mut last, &mut deadline, (hunk, old, new))
                        }
                        _ => Default::default(),
                    };
                    for ((side, marks), column) in
                        [old, new].into_iter().zip(marks).zip([start, start + half])
                    {
                        let shown = side
                            .map(|range| text(&self.diff.bytes, &range))
                            .unwrap_or_default();
                        let (style, marked) = line_styles(&shown);
                        put_line(
                            screen,
                            (row, column, half.saturating_sub(1)),
                            &shown,
                            style,
                            &marks,
                            marked,
                        );
                    }
                    screen.put(row, start + half - 1, "│", dim);
                }
            }
        }
    }

    fn key(&mut self, key: Key) -> Flow {
        if self.help {
            self.help = false;
            return if key == Key::Char('q') {
                Flow::Quit
            } else {
                Flow::Continue
            };
        }
        self.notice = None;
        if let Some(confirm) = self.confirm.take() {
            if key == Key::Char('y') {
                self.apply(&confirm);
            }
            return Flow::Continue;
        }
        let page = self.page;
        match key {
            Key::Char('q') => return Flow::Quit,
            Key::Char('?') => self.help = true,
            Key::Char('v') => self.side = !self.side,
            Key::Char('s') => self.ask(false),
            Key::Char('r') => self.ask(true),
            Key::Char('w') => {
                if let Some(watch) = &mut self.watch {
                    watch.on = !watch.on;
                }
            }
            Key::Char('n') => {
                self.selected = (self.selected + 1).min(self.diff.files.len().saturating_sub(1));
                self.top = 0;
            }
            Key::Char('p') => {
                self.selected = self.selected.saturating_sub(1);
                self.top = 0;
            }
            Key::Char('j') | Key::Down => self.top += 1,
            Key::Char('k') | Key::Up => self.top = self.top.saturating_sub(1),
            Key::Char(' ' | 'f') | Key::PageDown => self.top += page,
            Key::Char('b') | Key::PageUp => self.top = self.top.saturating_sub(page),
            Key::Char('g') | Key::Home => self.top = 0,
            Key::Char('G') | Key::End => self.top = self.last_top(),
            _ => {}
        }
        self.top = self.top.min(self.last_top());
        Flow::Continue
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init, reason = "marks are lists of ranges")]
mod tests {
    use super::*;

    fn text(diff: &Diff, range: &Range<usize>) -> String {
        String::from_utf8_lossy(&diff.bytes[range.clone()]).into_owned()
    }

    fn hunk_lines(diff: &Diff, file: usize) -> Vec<String> {
        let file = &diff.files[file];
        (file.hunks.iter().flat_map(|hunk| &hunk.lines))
            .map(|range| text(diff, range))
            .collect()
    }

    const CHANGE: &str = "diff --git a/src/main.rs b/src/main.rs\n\
        index 1a2b3c4..5d6e7f8 100644\n\
        --- a/src/main.rs\n\
        +++ b/src/main.rs\n\
        @@ -1,3 +1,3 @@ fn main() {\n \
        fn main() {\n\
        -    println!(\"hi\");\n\
        +    println!(\"hello\");\n \
        }\n\
        @@ -9 +9 @@\n\
        --- a/x\n\
        +@@ not a hunk\n";

    /// Every kind of file, with a tab, an escape and a bidi control in the changed lines.
    const FILES: &str = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
        diff --git a/logo.png b/logo.png\nindex 1..2 100644\n\
        Binary files a/logo.png and b/logo.png differ\n\
        diff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\n\
        diff --git \"a/x\\033[2J\" \"b/x\\033[2J\"\nnew file mode 100644\n--- /dev/null\n\
        +++ \"b/x\\033[2J\"\n@@ -0,0 +1 @@\n+\tgnp\u{202e}.exe\x1b]0;x\x07\n";

    /// The screen at 80×24 after `keys`, as rows and style runs.
    fn shown(diff: Diff, keys: &[Key]) -> String {
        let mut viewer = Viewer::new(b"diff main..HEAD".to_vec(), diff);
        for &key in keys {
            viewer.key(key);
        }
        let mut screen = Screen::new(24, 80);
        viewer.draw(&mut screen);
        viewer.draw(&mut screen);
        screen.runs()
    }

    #[test]
    fn a_unified_diff() {
        insta::assert_snapshot!(shown(parse(format!("{CHANGE}{FILES}").into()), &[]));
    }

    #[test]
    fn a_hostile_path_and_line_show_escaped() {
        let keys = [Key::Char('n'); 4];
        insta::assert_snapshot!(shown(parse(format!("{CHANGE}{FILES}").into()), &keys));
    }

    #[test]
    fn a_cut_short_diff() {
        let mut diff = parse(CHANGE.as_bytes()[..CHANGE.len() - 5].to_vec());
        diff.notes
            .push("git printed over 8 MB, so the diff is cut short.".into());
        insta::assert_snapshot!(shown(diff, &[]));
    }

    #[test]
    fn side_by_side_with_word_highlights() {
        let keys = [Key::Char('v')];
        insta::assert_snapshot!(shown(parse(format!("{CHANGE}{FILES}").into()), &keys));
    }

    /// A viewer of the work tree's diff of `repo`, not watching.
    fn work_tree(repo: &Path, diff: Diff) -> Viewer {
        let source = Source {
            repo: repo.to_owned(),
            staged: false,
            range: None,
        };
        Viewer::new(b"diff".to_vec(), diff).watch(source, false, Duration::ZERO)
    }

    #[test]
    fn a_stage_asks_first_with_the_name_escaped_and_the_hunk_header() {
        let diff = parse(format!("{CHANGE}{FILES}").into());
        let mut viewer = work_tree(Path::new("/nonexistent"), diff);
        for _ in 0..4 {
            viewer.key(Key::Char('n'));
        }
        viewer.key(Key::Char('s'));
        let mut screen = Screen::new(24, 80);
        viewer.draw(&mut screen);
        insta::assert_snapshot!(screen.runs());
        // Any key but `y` drops it, and nothing runs.
        viewer.key(Key::Char('n'));
        assert!(viewer.confirm.is_none() && viewer.notice.is_none());
    }

    #[test]
    fn a_hunk_patch_is_the_file_header_and_that_hunk_uncut() {
        let diff = parse(format!("{CHANGE}{FILES}").into());
        let patch = String::from_utf8(diff.patch(0, 1).unwrap()).unwrap();
        let header = &CHANGE[..CHANGE.find("@@").unwrap()];
        let hunk = &CHANGE[CHANGE.find("@@ -9").unwrap()..];
        assert_eq!(patch, format!("{header}{hunk}"));
        assert!(diff.patch(0, 2).is_none() && diff.patch(2, 0).is_none());
        let long = format!(
            "diff --git a/a b/a\n@@ -1 +1 @@\n+{}\n",
            "x".repeat(MAX_LINE)
        );
        let diff = parse(long.clone().into());
        assert_eq!(
            diff.patch(0, 0).unwrap(),
            long.as_bytes(),
            "a long line is whole"
        );
        let mut cut = parse(CHANGE.into());
        cut.whole = false;
        assert!(
            cut.patch(0, 1).is_none(),
            "a cut diff's last hunk may be cut"
        );
    }

    #[test]
    fn stage_and_revert_work_on_the_work_tree_diff_alone() {
        let source = Source {
            repo: "/nonexistent".into(),
            staged: true,
            range: None,
        };
        let mut viewer = Viewer::new(Vec::new(), parse(CHANGE.into()));
        viewer = viewer.watch(source, false, Duration::ZERO);
        viewer.key(Key::Char('r'));
        assert!(viewer.confirm.is_none() && viewer.notice.is_some());
    }

    #[test]
    fn stage_and_revert_apply_the_hunk_at_the_top_in_a_scratch_repository() {
        let repo = std::env::temp_dir().join(format!("pinfold-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        let numbers: String = (1..=20).map(|n| format!("{n}\n")).collect();
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), &numbers).unwrap();
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);
        let changed = numbers
            .replace("\n2\n", "\ntwo\n")
            .replace("19\n", "nineteen\n");
        std::fs::write(repo.join("a.txt"), &changed).unwrap();
        std::fs::write(repo.join("b.txt"), "B\n").unwrap();
        std::fs::write(repo.join("c.txt"), "C\n").unwrap();
        let mut viewer = work_tree(&repo, load(&repo, false, None).unwrap());
        let settle = |viewer: &mut Viewer| {
            let started = Instant::now();
            loop {
                viewer.tick();
                let watch = viewer.watch.as_ref().unwrap();
                if !watch.again && watch.running.is_none() {
                    break;
                }
                assert!(started.elapsed() < git::TIMEOUT * 2);
                thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(viewer.diff.files[0].hunks.len(), 2);
        // Four rows a page; the second hunk's header is the eleventh row.
        let mut screen = Screen::new(5, 80);
        viewer.draw(&mut screen);
        for _ in 0..10 {
            viewer.key(Key::Char('j'));
        }
        viewer.key(Key::Char('s'));
        let confirm = viewer.confirm.as_ref().unwrap();
        assert_eq!(confirm.header, b"@@ -16,5 +16,5 @@");
        viewer.key(Key::Char('y'));
        assert!(viewer.notice.is_none(), "{:?}", viewer.notice);
        let staged = git(&["diff", "--cached"]);
        assert!(
            staged.contains("+nineteen") && !staged.contains("+two"),
            "{staged}"
        );
        assert!(
            !staged.contains("b.txt") && !staged.contains("c.txt"),
            "{staged}"
        );
        settle(&mut viewer);
        assert_eq!(
            hunk_lines(&viewer.diff, 0),
            [" 1", "-2", "+two", " 3", " 4", " 5"]
        );
        // Revert b.txt, the next file; a key other than `y` first leaves it be.
        viewer.key(Key::Char('n'));
        assert_eq!(viewer.diff.files[viewer.selected].label(), b"b.txt");
        viewer.key(Key::Char('r'));
        viewer.key(Key::Char('q'));
        assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap(), "B\n");
        viewer.key(Key::Char('r'));
        viewer.key(Key::Char('y'));
        assert!(viewer.notice.is_none(), "{:?}", viewer.notice);
        assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap(), "b\n");
        assert_eq!(std::fs::read_to_string(repo.join("c.txt")).unwrap(), "C\n");
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            changed
        );
        settle(&mut viewer);
        let labels: Vec<_> = viewer.diff.files.iter().map(File::label).collect();
        assert_eq!(labels, [&b"a.txt"[..], b"c.txt"]);
        // A hunk git no longer has fails, and says why.
        viewer.selected = 0;
        viewer.top = 0;
        viewer.key(Key::Char('r'));
        std::fs::write(repo.join("a.txt"), &numbers).unwrap();
        viewer.key(Key::Char('y'));
        let notice = viewer.notice.clone().unwrap();
        assert!(notice.starts_with("git apply failed: "), "{notice}");
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn changed_words_are_marked_on_both_lines() {
        let far = Instant::now() + Duration::from_secs(60);
        let marks = words(b"-    println!(\"hi\");", b"+    println!(\"hello\");", far);
        assert_eq!(marks, [[15..17], [15..20]]);
        let marks = words("-tä x".as_bytes(), "+tö x".as_bytes(), far);
        assert_eq!(marks, [[1..4], [1..4]], "a character is never split");
    }

    #[test]
    fn a_line_past_the_word_cap_has_no_marks() {
        let far = Instant::now() + Duration::from_secs(60);
        let (old, new) = (
            format!("-{}", "a ".repeat(MAX_WORDS_LINE)),
            format!("+b {}", "a ".repeat(MAX_WORDS_LINE)),
        );
        let at = |line: &str| line.as_bytes()[..MAX_WORDS_LINE].to_vec();
        assert_ne!(words(&at(&old), &at(&new), far), [vec![], vec![]]);
        let over = |line: &str| line.as_bytes()[..=MAX_WORDS_LINE].to_vec();
        assert_eq!(words(&over(&old), &over(&new), far), [vec![], vec![]]);
    }

    #[test]
    fn word_highlights_stop_at_the_hunk_deadline() {
        // Each pair of lines shares no word, the worst case for Myers.
        let (old, new) = (
            format!("-{}\n", "(".repeat(MAX_WORDS_LINE - 1)),
            format!("+{}\n", ")".repeat(MAX_WORDS_LINE - 1)),
        );
        let rows = 100;
        let hunk = format!(
            "diff --git a/a b/a\n@@ -1 +1 @@\n{}{}",
            old.repeat(rows),
            new.repeat(rows)
        );
        let mut viewer = Viewer::new(Vec::new(), parse(hunk.into()));
        viewer.key(Key::Char('v'));
        let mut screen = Screen::new(u16::try_from(rows).unwrap(), 80);
        let started = Instant::now();
        viewer.draw(&mut screen);
        let took = started.elapsed();
        assert!(took < WORDS_DEADLINE * 10, "{took:?}");
        // Past the deadline a pair is marked whole.
        assert!(
            viewer
                .words
                .values()
                .all(|marks| *marks == [[1..MAX_WORDS_LINE], [1..MAX_WORDS_LINE]])
        );
    }

    #[test]
    fn watch_waits_2_s_or_20_times_the_last_run() {
        assert_eq!(watch_interval(Duration::ZERO), WATCH_EVERY);
        assert_eq!(
            watch_interval(Duration::from_millis(100)),
            Duration::from_secs(2)
        );
        assert_eq!(
            watch_interval(Duration::from_millis(150)),
            Duration::from_secs(3)
        );
        assert_eq!(watch_interval(Duration::MAX), Duration::MAX);
    }

    #[test]
    fn keys_scroll_within_the_diff_and_q_quits() {
        let mut viewer = Viewer::new(Vec::new(), parse(CHANGE.into()));
        let mut screen = Screen::new(5, 80);
        viewer.draw(&mut screen);
        assert_eq!(viewer.key(Key::Char('G')), Flow::Continue);
        // Eleven lines, four rows a page.
        assert_eq!(viewer.top, 7);
        viewer.key(Key::Char('j'));
        assert_eq!(viewer.top, 7);
        viewer.key(Key::Char('n'));
        assert_eq!((viewer.selected, viewer.top), (0, 0));
        assert_eq!(viewer.key(Key::Char('q')), Flow::Quit);
    }

    #[test]
    fn git_diff_runs_through_git_run_and_says_why_it_failed_or_was_cut_short() {
        let repo = std::env::temp_dir().join(format!("pinfold-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "diff.noprefix=true",
                ])
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        let diff = load(&repo, false, None).unwrap();
        assert_eq!(diff.files[0].label(), b"a.txt");
        assert_eq!(hunk_lines(&diff, 0), ["-one", "+two"]);
        // Watch mode runs git on its own thread, one run at a time, and keeps the old diff
        // until the run ends.
        let source = Source {
            repo: repo.clone(),
            staged: false,
            range: None,
        };
        let mut viewer = Viewer::new(Vec::new(), diff).watch(source, false, Duration::ZERO);
        viewer.tick();
        assert!(
            viewer.watch.as_ref().unwrap().running.is_none(),
            "watch is off"
        );
        viewer.key(Key::Char('w'));
        viewer.watch.as_mut().unwrap().next = Instant::now();
        std::fs::write(repo.join("a.txt"), "three\n").unwrap();
        viewer.tick();
        let started = Instant::now();
        while viewer.watch.as_ref().unwrap().running.is_some() {
            assert_eq!(hunk_lines(&viewer.diff, 0), ["-one", "+two"]);
            assert!(started.elapsed() < git::TIMEOUT * 2);
            thread::sleep(Duration::from_millis(10));
            viewer.tick();
        }
        assert_eq!(hunk_lines(&viewer.diff, 0), ["-one", "+three"]);
        let watch = viewer.watch.as_ref().unwrap();
        assert!(watch.next > Instant::now() + WATCH_EVERY / 2);
        assert!(watch.failed.is_none());
        viewer.tick();
        assert!(
            viewer.watch.as_ref().unwrap().running.is_none(),
            "not before the interval"
        );
        assert!(load(&repo, true, None).unwrap().files.is_empty());
        let problem = load(&repo, false, Some("nope..HEAD")).err().unwrap();
        assert!(problem.starts_with("git diff failed: fatal: "), "{problem}");
        std::fs::write(
            repo.join("big.txt"),
            format!("{}\n", "x".repeat(99)).repeat(100_000),
        )
        .unwrap();
        git(&["add", "-N", "big.txt"]);
        let diff = load(&repo, false, None).unwrap();
        assert_eq!(
            diff.notes,
            ["git printed over 8 MB, so the diff is cut short."]
        );
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn a_change_has_its_path_header_and_hunks() {
        let diff = parse(CHANGE.into());
        let [file] = &diff.files[..] else {
            panic!("one file")
        };
        assert_eq!(file.label(), b"src/main.rs");
        assert_eq!(file.header.len(), 3);
        assert_eq!(file.hunks.len(), 2);
        assert_eq!(
            text(&diff, &file.hunks[0].header),
            "@@ -1,3 +1,3 @@ fn main() {"
        );
        // A removed line that looks like a header stays in its hunk.
        assert_eq!(hunk_lines(&diff, 0)[4..], ["--- a/x", "+@@ not a hunk"]);
        assert!(diff.notes.is_empty());
    }

    #[test]
    fn a_rename_names_both_paths() {
        let diff = parse(
            "diff --git a/old name.txt b/new.txt\nsimilarity index 90%\n\
            rename from old name.txt\nrename to new.txt\n"
                .into(),
        );
        assert_eq!(diff.files[0].label(), "old name.txt → new.txt".as_bytes());
        assert_eq!(diff.files[0].header.len(), 3);
    }

    #[test]
    fn a_binary_file_shows_its_header_and_no_hunks() {
        let diff = parse(
            "diff --git a/logo.png b/logo.png\nindex 1..2 100644\n\
            Binary files a/logo.png and b/logo.png differ\n"
                .into(),
        );
        let file = &diff.files[0];
        assert_eq!(file.label(), b"logo.png");
        assert!(file.hunks.is_empty());
        let last = text(&diff, &file.header[1]);
        assert_eq!(last, "Binary files a/logo.png and b/logo.png differ");
    }

    #[test]
    fn a_mode_change_keeps_both_modes() {
        let diff = parse(
            "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
            diff --git a/gone b/gone\ndeleted file mode 100644\n--- a/gone\n+++ /dev/null\n\
            @@ -1 +0,0 @@\n-x\n"
                .into(),
        );
        let header = &diff.files[0].header;
        let header: Vec<_> = header.iter().map(|range| text(&diff, range)).collect();
        assert_eq!(header, ["old mode 100644", "new mode 100755"]);
        assert_eq!(diff.files[1].label(), b"gone", "/dev/null is not a path");
    }

    #[test]
    fn no_newline_at_end_stays_in_its_hunk() {
        let diff = parse(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+y\n"
                .into(),
        );
        let lines = hunk_lines(&diff, 0);
        assert_eq!(lines, ["-x", "\\ No newline at end of file", "+y"]);
    }

    #[test]
    fn a_diff_cut_anywhere_parses_what_is_there() {
        for at in 0..=CHANGE.len() {
            let diff = parse(CHANGE.as_bytes()[..at].to_vec());
            assert!(diff.files.len() <= 1);
        }
        let diff = parse(CHANGE.as_bytes()[..CHANGE.len() - 5].to_vec());
        assert_eq!(hunk_lines(&diff, 0).last().unwrap(), "+@@ not a ");
        // Mid-way through a quoted path or an escape.
        for cut in [
            &b"diff --git \"a/x\\"[..],
            b"diff --git \"a/\\30",
            b"--- \"a",
        ] {
            parse(cut.to_vec());
        }
    }

    #[test]
    fn quoted_paths_are_unquoted_to_their_bytes() {
        let diff = parse(
            b"diff --git \"a/t\\303\\244b\\tx\\033\" \"b/t\\303\\244b\\tx\\033\"\n\
            --- \"a/t\\303\\244b\\tx\\033\"\n+++ b/with space\t\n"
                .to_vec(),
        );
        let file = &diff.files[0];
        assert_eq!(file.old, "täb\tx\x1b".as_bytes());
        assert_eq!(file.new, b"with space");
    }

    #[test]
    fn past_the_file_cap_the_rest_is_dropped() {
        let diff = parse("diff --git a/a b/a\n".repeat(MAX_FILES + 1).into());
        assert_eq!(diff.files.len(), MAX_FILES);
        assert_eq!(
            diff.notes,
            [format!("Only the first {MAX_FILES} files show.")]
        );
    }

    #[test]
    fn past_the_hunk_cap_the_rest_is_dropped() {
        let diff = parse(
            format!(
                "diff --git a/a b/a\n{}",
                "@@ -1 +1 @@\n".repeat(MAX_HUNKS + 1)
            )
            .into(),
        );
        assert_eq!(diff.files[0].hunks.len(), MAX_HUNKS);
        assert_eq!(
            diff.notes,
            [format!("Only the first {MAX_HUNKS} hunks show.")]
        );
    }

    #[test]
    fn past_the_line_cap_the_rest_is_dropped() {
        let diff = parse(
            format!(
                "diff --git a/a b/a\n@@ -1 +1 @@\n{}",
                "+\n".repeat(MAX_LINES)
            )
            .into(),
        );
        assert_eq!(diff.files[0].hunks[0].lines.len(), MAX_LINES - 1);
        assert_eq!(
            diff.notes,
            [format!("Only the first {MAX_LINES} lines show.")]
        );
    }

    #[test]
    fn a_line_past_the_length_cap_is_cut_at_a_character() {
        let long = format!("+{}", "é".repeat(MAX_LINE));
        let diff = parse(format!("diff --git a/a b/a\n@@ -1 +1 @@\n{long}\n+short\n").into());
        let lines = hunk_lines(&diff, 0);
        assert_eq!(lines[0].len(), MAX_LINE - 1);
        assert!(!lines[0].contains('\u{FFFD}'));
        assert_eq!(lines[1], "+short");
        assert_eq!(
            diff.notes,
            [format!("Lines over {MAX_LINE} bytes are cut.")]
        );
    }
}
