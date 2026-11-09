//! The markdown reader behind `pinfold md` (ADR-0045 sections 2 and 3).
//!
//! The file is laid out again for each width, as lines of styled spans that are already
//! escaped, so a line's length in characters is its length in cells. It is parsed a window of
//! whole top-level blocks at a time, which bounds the memory of the parse (#380).
//!
//! Each cap is a constant (ADR-0045 section 4), and the reader says above the file when one
//! cut it.

use std::borrow::Cow;
use std::ffi::OsString;
use std::io::{self, Read};
use std::ops::Range;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::keys::Key;
use crate::screen::{Color, Screen, Style, escape};
use crate::{Flow, Tool};

/// Bytes of a file the reader reads. A longer file shows this much, as plain text.
const MAX_BYTES: usize = 5_000_000;
/// Time parsing may take at each width, after which the rest shows as plain text.
const MAX_PARSE: Duration = Duration::from_secs(2);
/// Bytes of markdown parsed at a time. pulldown-cmark builds a tree of all it is given before
/// its first event, about 11 bytes for each byte of dense markdown (#380).
const WINDOW: usize = 32 * 1024;
/// Bytes of link reference definitions put above each window; later ones are not used across
/// windows. Two windows' worth keeps each parse within three windows, and holds about a
/// thousand definitions of a typical length.
const MAX_DEFS: usize = 64 * 1024;
/// Characters on a laid-out line. Code, table and plain-text lines are cut here.
const MAX_LINE: usize = 10_000;
/// Laid-out lines; the rest of the file is not shown.
const MAX_LINES: usize = 100_000;
/// Headings the outline lists; later ones are left out of it.
const MAX_HEADINGS: usize = 10_000;
/// Lines a search finds; later ones are not found.
const MAX_MATCHES: usize = 10_000;
/// Files Backspace goes back through; older ones are forgotten.
const MAX_BACK: usize = 100;
/// How often follow mode checks the file.
const FOLLOW: Duration = Duration::from_secs(1);

/// Columns `h` and `l` move by.
const SIDEWAYS: usize = 8;

const KEYS: [&str; 13] = [
    "j k  ↓ ↑              a line down or up",
    "h l  ← →              sideways",
    "space b  PgDn PgUp    a page down or up",
    "g G  Home End         the top or the end",
    "wheel                 three lines",
    "o                     the outline; Enter goes to a heading",
    "/                     search; Enter finds, Esc clears",
    "n N                   the next or the previous match",
    "Tab Enter  click      the next link, and follow it",
    "Backspace             back to the file before",
    "F                     follow: re-read the file when it changes",
    "?                     these keys",
    "q                     quit",
];

/// A run of laid-out text in one style and link, which starts where the span before it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    /// Where the span ends in [`Lines::text`].
    end: u32,
    /// The link's index in [`Lines::links`] plus one, or 0 for none.
    link: u32,
    style: Style,
}

// Offsets into the laid-out text fit in a u32: at most MAX_LINES lines of MAX_LINE characters
// of 4 bytes, and a banner.
const _: () = assert!(MAX_LINES * MAX_LINE * 4 + 1_000_000 <= u32::MAX as usize);

/// Laid-out lines, kept flat so that a span is 12 bytes, not allocations of its own (#380).
#[derive(Debug, Default)]
struct Lines {
    /// Each line's escaped text, one after another.
    text: String,
    spans: Vec<Span>,
    /// Each line's first span; the line in progress starts at `open`.
    starts: Vec<u32>,
    open: usize,
    links: Vec<String>,
}

impl Lines {
    /// Room for the layout of `bytes` of text, ample for most files. A freed buffer stays in the
    /// process's footprint on macOS, so growing by doubling would cost about twice the final
    /// size, while room that is never written costs no memory.
    fn with_capacity(bytes: usize) -> Self {
        Self {
            text: String::with_capacity(bytes.saturating_mul(2)),
            spans: Vec::with_capacity(bytes / 4),
            starts: Vec::with_capacity(bytes.min(MAX_LINES) + 8),
            ..Self::default()
        }
    }

    fn len(&self) -> usize {
        self.starts.len()
    }

    /// The spans of line `at`, as indexes into `spans`.
    fn range(&self, at: usize) -> Range<usize> {
        let start = self
            .starts
            .get(at)
            .map_or(self.open, |&start| start as usize);
        let end = self
            .starts
            .get(at + 1)
            .map_or(self.open, |&end| end as usize);
        start..end.max(start)
    }

    /// Where span `index` starts in `text`.
    fn offset(&self, index: usize) -> usize {
        (index.checked_sub(1).and_then(|last| self.spans.get(last)))
            .map_or(0, |span| span.end as usize)
    }

    /// The text of line `at`.
    fn text(&self, at: usize) -> &str {
        let spans = self.range(at);
        (self
            .text
            .get(self.offset(spans.start)..self.offset(spans.end)))
        .unwrap_or_default()
    }

    /// The spans of line `at`, as their text, style and link.
    fn line(&self, at: usize) -> impl Iterator<Item = (&str, Style, Option<&str>)> {
        self.range(at).filter_map(|index| {
            let span = self.spans.get(index)?;
            let text = self.text.get(self.offset(index)..span.end as usize)?;
            let link = (span.link as usize)
                .checked_sub(1)
                .and_then(|link| self.links.get(link));
            Some((text, span.style, link.map(String::as_str)))
        })
    }

    /// The line in progress has no spans yet.
    fn open_is_empty(&self) -> bool {
        self.spans.len() == self.open
    }

    /// Adds `text` to the line in progress, in its last span if that has the same style and link.
    fn append(&mut self, text: &str, style: Style, link: u32) {
        self.text.push_str(text);
        let end = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
        let open = self.open < self.spans.len();
        match self.spans.last_mut() {
            Some(last) if open && last.style == style && last.link == link => {
                last.end = end;
            }
            _ => self.spans.push(Span { end, link, style }),
        }
    }

    /// Ends the line in progress, or drops it if [`MAX_LINES`] are laid out, and says which.
    fn end_line(&mut self) -> bool {
        if self.len() < MAX_LINES {
            self.starts
                .push(u32::try_from(self.open).unwrap_or(u32::MAX));
            self.open = self.spans.len();
            return true;
        }
        let at = self.offset(self.open);
        self.text.truncate(at);
        self.spans.truncate(self.open);
        false
    }

    /// Puts `banner`, which has no links, above these lines.
    fn prepend(&mut self, banner: Lines) {
        let shift = u32::try_from(banner.text.len()).unwrap_or(u32::MAX);
        let count = u32::try_from(banner.spans.len()).unwrap_or(u32::MAX);
        self.text.reserve_exact(banner.text.len());
        self.text.insert_str(0, &banner.text);
        for span in &mut self.spans {
            span.end = span.end.saturating_add(shift);
        }
        for start in &mut self.starts {
            *start = start.saturating_add(count);
        }
        self.spans.reserve_exact(banner.spans.len());
        self.spans.splice(0..0, banner.spans);
        self.starts.reserve_exact(banner.starts.len());
        self.starts.splice(0..0, banner.starts);
        self.open += banner.open;
    }
}

/// A heading in the outline: the line it starts on, its level and its escaped text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Heading {
    line: usize,
    level: usize,
    text: String,
}

/// Reads at most [`MAX_BYTES`] of the regular file at `path`, and whether that was all of it.
pub(crate) fn read(path: &Path) -> io::Result<(Vec<u8>, bool)> {
    read_at_most(path, MAX_BYTES)
}

/// Reads at most `max` bytes of the regular file at `path`, and whether that was all of it.
/// Anything else is refused, a device or FIFO before it is opened.
pub(crate) fn read_at_most(path: &Path, max: usize) -> io::Result<(Vec<u8>, bool)> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let file = pinfold_sys::open_nonblocking(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    // The length is only a hint for the buffer: the file can change while it is read.
    let hint = usize::try_from(metadata.len()).map_or(max, |len| len.min(max));
    let mut bytes = Vec::with_capacity(hint + 1);
    // One byte past the cap says whether there is more, and nothing further is read.
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    let whole = bytes.len() <= max;
    bytes.truncate(max);
    Ok((bytes, whole))
}

/// Lays `bytes` out as the reader does at `width`, and returns each line's text.
pub(crate) fn lines(bytes: &[u8], width: u16) -> Vec<String> {
    let reader = Reader::new(Vec::new(), bytes.to_vec(), true);
    texts(&layout(&reader.text, usize::from(width), true, false, MAX_PARSE).0)
}

fn texts(lines: &Lines) -> Vec<String> {
    (0..lines.len())
        .map(|at| lines.text(at).to_owned())
        .collect()
}

/// The laid-out text, which is escaped, lowercased in ASCII only so byte offsets still match.
fn folded(text: &str) -> String {
    text.to_ascii_lowercase()
}

/// What follow mode compares: the file's modification time, length and inode.
pub(crate) type Stamp = (SystemTime, u64, u64);

pub(crate) fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len(), metadata.ino()))
}

/// The file a link points at, if it is relative or a `file:` URI, without its `#fragment`.
/// A relative link is relative to the directory of `file`.
fn local(file: &Path, target: &str) -> Option<PathBuf> {
    let target = target.split(['#', '?']).next().unwrap_or_default();
    let scheme = target.find(':').filter(|&at| {
        let scheme = &target[..at];
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    });
    let path = match scheme {
        None => target,
        Some(at) if target[..at].eq_ignore_ascii_case("file") => {
            let rest = target[at + 1..].strip_prefix("//")?;
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            rest.starts_with('/').then_some(rest)?
        }
        Some(_) => return None,
    };
    if path.is_empty() {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(unpercent(path)));
    Some(file.parent().unwrap_or(Path::new("")).join(path))
}

/// `text` with each `%XX` decoded.
fn unpercent(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let hex = bytes
            .get(at + 1..at + 3)
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match (byte, hex) {
            (b'%', Some(hex)) => {
                decoded.push(hex);
                at += 3;
            }
            _ => {
                decoded.push(byte);
                at += 1;
            }
        }
    }
    decoded
}

/// `bytes` as text, with invalid UTF-8 as `\xNN`.
fn decode(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|error| {
        let bytes = error.into_bytes();
        let mut text = String::with_capacity(bytes.len());
        for chunk in bytes.utf8_chunks() {
            text.push_str(chunk.valid());
            for byte in chunk.invalid() {
                text.push_str(&format!("\\x{byte:02X}"));
            }
        }
        text
    })
}

/// The reader: a file's text, laid out at the screen's width.
pub(crate) struct Reader {
    /// The file, which links are relative to and follow mode reads again.
    pub(crate) path: PathBuf,
    title: Vec<u8>,
    text: String,
    /// The whole file was read, so it is parsed as markdown if `markdown`.
    whole: bool,
    markdown: bool,
    width: u16,
    lines: Lines,
    widest: usize,
    headings: Vec<Heading>,
    top: usize,
    left: usize,
    page: usize,
    help: bool,
    /// The outline is open, with this heading selected.
    outline: Option<usize>,
    /// The search, as typed, and whether it is still being typed.
    query: String,
    typing: bool,
    /// The lines the search found, and the one shown.
    matches: Vec<usize>,
    current: usize,
    /// The link Tab picked, as its line and span.
    focus: Option<(usize, usize)>,
    /// A message on the last row until the next key.
    status: Option<String>,
    /// The files links came from, with their titles and where they were scrolled to.
    back: Vec<(PathBuf, Vec<u8>, usize)>,
    /// Follow mode: what the file was when last read, and when it was last checked.
    follow: Option<(Option<Stamp>, Instant)>,
}

impl Reader {
    /// A reader titled `title` for the bytes of a file, all of it if `whole`, or else its
    /// start, which shows as plain text. Invalid UTF-8 shows as `\xNN`.
    pub(crate) fn new(title: Vec<u8>, bytes: Vec<u8>, whole: bool) -> Self {
        Self {
            path: PathBuf::new(),
            title,
            text: decode(bytes),
            whole,
            markdown: true,
            width: 0,
            lines: Lines::default(),
            widest: 0,
            headings: Vec::new(),
            top: 0,
            left: 0,
            page: 1,
            help: false,
            outline: None,
            query: String::new(),
            typing: false,
            matches: Vec::new(),
            current: 0,
            focus: None,
            status: None,
            back: Vec::new(),
            follow: None,
        }
    }

    /// Shows the text as it is, not as markdown, as the file manager's pager does.
    pub(crate) fn plain(mut self) -> Self {
        self.markdown = false;
        self
    }

    /// Shows `bytes` as the file's text, laid out again at the next draw.
    fn reload(&mut self, bytes: Vec<u8>, whole: bool) {
        self.text = decode(bytes);
        self.whole = whole;
        self.width = 0;
        self.focus = None;
    }

    /// Opens the file at `path` in place of this one, scrolled to `top`.
    fn open(&mut self, path: PathBuf, title: Vec<u8>, top: usize) -> io::Result<()> {
        let (bytes, whole) = read(&path)?;
        self.reload(bytes, whole);
        (self.path, self.title, self.top, self.left) = (path, title, top, 0);
        self.query.clear();
        self.matches.clear();
        self.follow = self.follow.map(|_| (stamp(&self.path), Instant::now()));
        Ok(())
    }

    /// Opens a relative or `file:` link in the reader; shows any other link's target.
    fn follow_link(&mut self, target: &str) {
        let Some(path) = local(&self.path, target) else {
            self.status = Some(format!("Not a file, so open it from the text: {target}"));
            return;
        };
        let from = (self.path.clone(), self.title.clone(), self.top);
        let title = path.as_os_str().as_bytes().to_vec();
        match self.open(path, title, 0) {
            Ok(()) => {
                if self.back.len() == MAX_BACK {
                    self.back.remove(0);
                }
                self.back.push(from);
            }
            Err(error) => self.status = Some(format!("Cannot open {target}: {error}")),
        }
    }

    /// The first span of each link, as its line and span, in the order of the text.
    fn links(&self) -> Vec<(usize, usize)> {
        let mut starts = Vec::new();
        let mut last = None;
        for at in 0..self.lines.len() {
            for (index, (_, _, link)) in self.lines.line(at).enumerate() {
                // A link wrapped or restyled across spans is one link.
                if link.is_some() && link != last {
                    starts.push((at, index));
                }
                last = link;
            }
        }
        starts
    }

    fn link(&self, (line, span): (usize, usize)) -> Option<String> {
        Some(self.lines.line(line).nth(span)?.2?.to_owned())
    }

    /// Picks the next link after the one picked, wrapping, and scrolls to it.
    fn next_link(&mut self) {
        let links = self.links();
        let next = links
            .iter()
            .find(|&&link| match self.focus {
                Some(focus) => link > focus,
                None => link.0 >= self.top,
            })
            .or(links.first());
        self.focus = next.copied();
        if let Some((line, _)) = self.focus
            && (line < self.top || line >= self.top + self.page)
        {
            self.top = line;
        }
    }

    /// The span under a click at `row` and `column` of the screen.
    fn clicked(&self, row: u16, column: u16) -> Option<(usize, usize)> {
        let line = (self.top + usize::from(row)).checked_sub(1)?;
        let mut x = self.left + usize::from(column);
        for (index, (text, _, _)) in self.lines.line(line).enumerate() {
            let length = text.chars().count();
            if x < length {
                return Some((line, index));
            }
            x -= length;
        }
        None
    }

    fn last_top(&self) -> usize {
        self.lines.len().saturating_sub(self.page)
    }

    /// The query as it is matched: escaped like the text, and folded.
    fn needle(&self) -> String {
        folded(&escape(self.query.as_bytes()))
    }

    /// Finds the lines that hold the query, keeping the current match if it is still one.
    fn find(&mut self) {
        let needle = self.needle();
        self.matches = if needle.is_empty() {
            Vec::new()
        } else {
            (0..self.lines.len())
                .filter(|&at| folded(self.lines.text(at)).contains(&needle))
                .take(MAX_MATCHES)
                .collect()
        };
        self.current = self.current.min(self.matches.len().saturating_sub(1));
    }

    /// Scrolls to the current match.
    fn show_match(&mut self) {
        if let Some(&line) = self.matches.get(self.current) {
            self.top = line;
        }
    }

    /// Handles a key while the search is typed.
    fn type_query(&mut self, key: Key) {
        match key {
            Key::Char(character) if self.query.len() < MAX_LINE => self.query.push(character),
            Key::Backspace => {
                self.query.pop();
            }
            Key::Enter => {
                self.typing = false;
                self.find();
                // The first match from the top of the screen, wrapping to the first of all.
                let top = self.top;
                self.current = self
                    .matches
                    .iter()
                    .position(|&line| line >= top)
                    .unwrap_or(0);
                self.show_match();
            }
            Key::Esc => {
                self.typing = false;
                self.query.clear();
            }
            _ => {}
        }
    }

    /// Handles a key while the outline is open.
    fn pick_heading(&mut self, selected: usize, key: Key) {
        let last = self.headings.len().saturating_sub(1);
        match key {
            Key::Char('j') | Key::Down => self.outline = Some((selected + 1).min(last)),
            Key::Char('k') | Key::Up => self.outline = Some(selected.saturating_sub(1)),
            Key::Char('g') | Key::Home => self.outline = Some(0),
            Key::Char('G') | Key::End => self.outline = Some(last),
            Key::Enter => {
                if let Some(heading) = self.headings.get(selected) {
                    self.top = heading.line;
                }
                self.outline = None;
            }
            Key::Esc | Key::Char('o') => self.outline = None,
            _ => {}
        }
    }

    /// Draws the outline, each heading indented by its level, scrolled to show the selected one.
    fn draw_outline(&self, screen: &mut Screen, selected: usize) {
        let rows = screen.size().0;
        if self.headings.is_empty() {
            screen.put(
                2,
                2,
                "This file has no headings.",
                colour(Color::BrightBlack),
            );
        }
        let first = (selected + 1).saturating_sub(usize::from(rows.saturating_sub(1)));
        let headings = self.headings.iter().enumerate().skip(first);
        for (row, (at, heading)) in (1..rows).zip(headings) {
            let style = if at == selected {
                Style {
                    fg: Color::Black,
                    bg: Color::Blue,
                    bold: true,
                }
            } else {
                Style::default()
            };
            let indent = "  ".repeat(heading.level.saturating_sub(1));
            screen.put(row, 0, format!("{indent}{}", heading.text), style);
        }
    }

    /// Marks each visible occurrence of the search on `row`, which shows `text`.
    fn mark_matches(&self, screen: &mut Screen, row: u16, text: &str) {
        let needle = self.needle();
        let hit = Style {
            fg: Color::Black,
            bg: Color::Yellow,
            bold: false,
        };
        for (at, found) in folded(text).match_indices(&needle) {
            let start = text[..at].chars().count();
            let skip = self.left.saturating_sub(start);
            let Some((cut, _)) = text[at..at + found.len()].char_indices().nth(skip) else {
                continue;
            };
            let column = u16::try_from(start + skip - self.left).unwrap_or(u16::MAX);
            screen.put(row, column, &text[at + cut..at + found.len()], hit);
        }
    }
}

impl Tool for Reader {
    fn draw(&mut self, screen: &mut Screen) {
        let (rows, columns) = screen.size();
        if columns != self.width {
            // The old layout goes first, so the two are never in memory together.
            self.lines = Lines::default();
            (self.lines, self.widest, self.headings) = layout(
                &self.text,
                usize::from(columns),
                self.whole && self.markdown,
                !self.whole,
                MAX_PARSE,
            );
            self.width = columns;
            self.focus = None;
            self.find();
        }
        self.page = usize::from(rows.saturating_sub(1)).max(1);
        self.top = self.top.min(self.last_top());
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let dim = Style {
            fg: Color::BrightBlack,
            ..Style::default()
        };
        let mut title = b"md ".to_vec();
        title.extend_from_slice(&self.title);
        screen.put(0, 0, &title, bold);
        let found = match (self.query.is_empty() || self.typing, self.matches.len()) {
            (true, _) => String::new(),
            (false, 0) => "no match  ".to_owned(),
            (false, count) => format!("match {}/{count}  ", self.current + 1),
        };
        let follow = if self.follow.is_some() {
            "follow  "
        } else {
            ""
        };
        let place = format!(
            "{follow}{found}{}/{}  ? keys",
            self.top + 1,
            self.lines.len().max(1)
        );
        let at = columns.saturating_sub(u16::try_from(place.len()).unwrap_or(u16::MAX));
        screen.put(0, at, place, dim);
        if self.help {
            for (row, keys) in (2..rows).zip(KEYS) {
                screen.put(row, 2, keys, Style::default());
            }
            return;
        }
        if let Some(selected) = self.outline {
            self.draw_outline(screen, selected);
            return;
        }
        for (row, at) in (1..rows).zip(self.top..self.lines.len()) {
            let mut x = 0;
            for (index, (text, style, link)) in self.lines.line(at).enumerate() {
                let length = text.chars().count();
                let focused = Style {
                    fg: Color::Black,
                    bg: Color::Blue,
                    ..style
                };
                let style = if self.focus == Some((at, index)) {
                    focused
                } else {
                    style
                };
                if x + length > self.left {
                    let skip = self.left.saturating_sub(x);
                    let column = u16::try_from(x + skip - self.left).unwrap_or(u16::MAX);
                    let from = text.char_indices().nth(skip).map_or(0, |(at, _)| at);
                    let text = &text[from..];
                    match link {
                        Some(target) => {
                            screen.put_link(row, column, text, style, target.as_bytes())
                        }
                        None => screen.put(row, column, text, style),
                    };
                }
                x += length;
            }
            if !self.typing && !self.matches.is_empty() {
                self.mark_matches(screen, row, self.lines.text(at));
            }
        }
        let last = rows.saturating_sub(1);
        let pad = " ".repeat(usize::from(columns));
        if self.typing {
            let prompt = format!("/{}", self.query);
            screen.put(last, 0, prompt + &pad, Style::default());
        } else if let Some(status) = &self.status {
            screen.put(last, 0, format!("{status}{pad}"), colour(Color::Yellow));
        }
    }

    fn tick(&mut self) {
        let Some((seen, checked)) = self.follow else {
            return;
        };
        if checked.elapsed() < FOLLOW {
            return;
        }
        let now = stamp(&self.path);
        // A file that is gone or is not a regular file keeps showing what was read last.
        if now != seen
            && let Ok((bytes, whole)) = read(&self.path)
        {
            self.reload(bytes, whole);
        }
        self.follow = Some((now, Instant::now()));
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
        let page = self.page;
        self.status = None;
        let open = self.typing || self.outline.is_some();
        if self.typing {
            self.type_query(key);
        } else if let Some(selected) = self.outline {
            if key == Key::Char('q') {
                return Flow::Quit;
            }
            self.pick_heading(selected, key);
        } else if key == Key::Esc {
            self.query.clear();
            self.matches.clear();
            self.focus = None;
        }
        if open {
            self.top = self.top.min(self.last_top());
            return Flow::Continue;
        }
        let count = self.matches.len();
        match key {
            Key::Char('q') => return Flow::Quit,
            Key::Char('?') => self.help = true,
            Key::Char('o') => {
                let top = self.top;
                let before = self.headings.iter().filter(|h| h.line <= top).count();
                self.outline = Some(before.saturating_sub(1));
            }
            Key::Char('/') => {
                self.typing = true;
                self.query.clear();
                self.matches.clear();
            }
            Key::Char('n') if count > 0 => {
                self.current = (self.current + 1) % count;
                self.show_match();
            }
            Key::Char('N') if count > 0 => {
                self.current = (self.current + count - 1) % count;
                self.show_match();
            }
            Key::Tab => self.next_link(),
            Key::Enter => {
                if let Some(target) = self.focus.and_then(|focus| self.link(focus)) {
                    self.follow_link(&target);
                }
            }
            Key::Backspace => {
                if let Some((path, title, top)) = self.back.pop()
                    && let Err(error) = self.open(path, title, top)
                {
                    self.status = Some(format!("Cannot go back: {error}"));
                }
            }
            Key::Char('F') => {
                self.follow = match self.follow {
                    Some(_) => None,
                    None => Some((stamp(&self.path), Instant::now())),
                };
            }
            Key::Mouse(mouse) if mouse.pressed && mouse.button == 0 => {
                let clicked = self.clicked(mouse.row, mouse.column);
                if let Some(target) = clicked.and_then(|span| self.link(span)) {
                    self.focus = clicked;
                    self.follow_link(&target);
                }
            }
            Key::Char('j') | Key::Down => self.top += 1,
            Key::Char('k') | Key::Up => self.top = self.top.saturating_sub(1),
            Key::Char('h') | Key::Left => self.left = self.left.saturating_sub(SIDEWAYS),
            Key::Char('l') | Key::Right => self.left += SIDEWAYS,
            Key::Char(' ' | 'f') | Key::Ctrl('f') | Key::PageDown => self.top += page,
            Key::Char('b') | Key::Ctrl('b') | Key::PageUp => {
                self.top = self.top.saturating_sub(page);
            }
            Key::Char('g') | Key::Home => self.top = 0,
            Key::Char('G') | Key::End => self.top = self.last_top(),
            // The wheel, with any of Shift, Alt and Control.
            Key::Mouse(mouse) if mouse.pressed => match mouse.button & !28 {
                64 => self.top = self.top.saturating_sub(3),
                65 => self.top += 3,
                _ => {}
            },
            _ => {}
        }
        self.top = self.top.min(self.last_top());
        self.left = self
            .left
            .min(self.widest.saturating_sub(usize::from(self.width)));
        Flow::Continue
    }
}

/// A style of just the colour `fg`.
pub(crate) fn colour(fg: Color) -> Style {
    Style {
        fg,
        ..Style::default()
    }
}

/// Lays `text` out at `width` columns, as markdown if `markdown` for up to `budget` and as plain
/// text after, and returns the lines, the widest one's length and the headings. A banner above
/// says what any cap cut, the byte cap if `cut`.
fn layout(
    text: &str,
    width: usize,
    markdown: bool,
    cut: bool,
    budget: Duration,
) -> (Lines, usize, Vec<Heading>) {
    let width = width.clamp(1, MAX_LINE);
    let mut layout = Layout {
        width,
        lines: Lines::with_capacity(text.len()),
        ..Layout::default()
    };
    let mut notes = Vec::new();
    if markdown {
        let started = Instant::now();
        // shortcut: the time is checked between events, so pulldown-cmark's first pass over a
        // window and any one slow event are not cut short; a parse thread with a deadline would
        // be, if a hostile file is ever found to stall either.
        let (defs, cut_defs) = definitions(text, WINDOW, || started.elapsed() <= budget);
        if cut_defs {
            notes.push(format!(
                "Only the first {} KiB of link definitions are used.",
                MAX_DEFS / 1024
            ));
        }
        // Where the text laid out so far ends: a start's range covers its whole element.
        let mut done = 0;
        let mut late = false;
        parse(text, &defs, WINDOW, |event, range| {
            if layout.full {
                return false;
            }
            late = started.elapsed() > budget;
            if late {
                return false;
            }
            if !matches!(event, Event::Start(_)) {
                done = done.max(range.end);
            }
            layout.event(event);
            true
        });
        if late {
            notes.push(format!(
                "Parsing took over {} s, so the rest shows as plain text.",
                MAX_PARSE.as_secs()
            ));
            let at = (done..text.len())
                .find(|&at| text.is_char_boundary(at))
                .unwrap_or(text.len());
            layout.plain(&text[at..]);
        }
    } else {
        if cut {
            notes.push(format!(
                "This file is over {} MB, so its first {0} MB shows as plain text.",
                MAX_BYTES / 1_000_000
            ));
        }
        layout.plain(text);
    }
    layout.flush();
    if layout.long {
        notes.push(format!("Lines over {MAX_LINE} characters are cut."));
    }
    if layout.full {
        notes.push(format!("Only the first {MAX_LINES} lines show."));
    }
    if notes.is_empty() {
        return (layout.lines, layout.widest, layout.headings);
    }
    let mut banner = Layout {
        width,
        ..Layout::default()
    };
    for note in notes {
        banner.text(&note, colour(Color::Yellow));
        banner.newline();
    }
    banner.newline();
    for heading in &mut layout.headings {
        heading.line += banner.lines.len();
    }
    layout.lines.prepend(banner.lines);
    (
        layout.lines,
        banner.widest.max(layout.widest),
        layout.headings,
    )
}

/// Parses `text` as markdown `window` bytes at a time, with `defs` above each window, and passes
/// each event and its range in `text` to `each` until it returns false. A window ends at a line
/// end, and its last top-level block, which that end may have cut, starts the next window, so
/// each event is the one a parse of the whole text gives. A block longer than a window doubles
/// it.
///
/// shortcut: a single top-level block, such as a list, is parsed whole, so a 5 MB one still
/// costs pulldown-cmark's tree of it; cutting inside blocks would bound that, if such files
/// turn up.
fn parse(text: &str, defs: &str, window: usize, each: impl FnMut(Event<'_>, Range<usize>) -> bool) {
    windows(text, defs, window, each, |_| true);
}

/// The source of each link reference definition in `text`, a line each, as [`parse`] puts them
/// above every window so a link can use a definition in a later one, and whether
/// [`MAX_DEFS`] cut them. It stops when `more` says.
///
/// shortcut: a definition that spans lines inside a block quote keeps the `>` of its later lines,
/// so a link before its window does not find it; such definitions are rare.
fn definitions(text: &str, window: usize, more: impl Fn() -> bool) -> (String, bool) {
    let mut defs = String::new();
    let mut cut = false;
    if text.contains("]:") {
        windows(
            text,
            "",
            window,
            |_, _| more(),
            |span| {
                let def = text.get(span).unwrap_or_default();
                cut = defs.len() + def.len() + 1 > MAX_DEFS;
                if !cut {
                    defs.push_str(def);
                    defs.push('\n');
                }
                !cut && more()
            },
        );
    }
    (defs, cut)
}

/// [`parse`], which also passes `found` the span of each definition in the blocks it passed on,
/// in order, until it returns false.
fn windows(
    text: &str,
    defs: &str,
    window: usize,
    mut each: impl FnMut(Event<'_>, Range<usize>) -> bool,
    mut found: impl FnMut(Range<usize>) -> bool,
) {
    let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let (mut from, mut size) = (0, window.max(1));
    while from < text.len() {
        let end = (text.as_bytes().get(from.saturating_add(size)..))
            .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
            .map_or(text.len(), |at| from + size + at + 1);
        let last = end == text.len();
        let body = text.get(from..end).unwrap_or_default();
        // A blank line ends the definitions, so the window's first line is not a title.
        let input: Cow<'_, str> = if defs.is_empty() {
            body.into()
        } else {
            format!("{defs}\n{body}").into()
        };
        let skip = input.len() - body.len();
        let place = |range: Range<usize>| {
            from + range.start.saturating_sub(skip)..from + range.end.saturating_sub(skip)
        };
        let mut parser = Parser::new_ext(&input, options).into_offset_iter();
        // The events of the top-level block in progress, and where its line starts.
        let mut held = Vec::new();
        let mut block = from;
        let mut depth = 0_usize;
        for (event, range) in parser.by_ref() {
            let range = place(range);
            if depth == 0 && !matches!(event, Event::End(_)) {
                // A top-level block starts, so the one before it is whole.
                for (event, range) in held.drain(..) {
                    if !each(event, range) {
                        return;
                    }
                }
                let before = text.as_bytes().get(..range.start).unwrap_or_default();
                block = before
                    .iter()
                    .rposition(|&byte| byte == b'\n')
                    .map_or(0, |at| at + 1)
                    .max(from);
            }
            match event {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
            held.push((event, range));
        }
        let mut spans: Vec<_> = (parser.reference_definitions().iter())
            .filter(|(_, def)| def.span.start >= skip)
            .map(|(_, def)| place(def.span.clone()))
            .collect();
        spans.sort_by_key(|span| span.start);
        // Definitions give no events, so a file of them would never split. One that starts a
        // line ends every block before it: inside a list or quote it would be indented or quoted.
        if let Some(span) = spans
            .iter()
            .rev()
            .find(|span| span.start > block && text.as_bytes().get(span.start - 1) == Some(&b'\n'))
            && !last
        {
            for (event, range) in held.drain(..) {
                if !each(event, range) {
                    return;
                }
            }
            block = span.start;
        }
        let kept = if last { end } else { block };
        for span in spans.into_iter().filter(|span| span.start < kept) {
            if !found(span) {
                return;
            }
        }
        if last {
            for (event, range) in held {
                if !each(event, range) {
                    return;
                }
            }
            return;
        }
        if block == from {
            size = size.saturating_mul(2);
        } else {
            (from, size) = (block, window.max(1));
        }
    }
}

#[derive(Default)]
struct Layout {
    width: usize,
    lines: Lines,
    /// Characters on the line in progress, and in its prefix.
    used: usize,
    start: usize,
    widest: usize,
    /// What starts each line inside block quotes and list items.
    indents: Vec<String>,
    /// A list item's bullet or number, which replaces the last indent on its first line.
    marker: Option<String>,
    /// Each open list's next number, or `None` for bullets.
    lists: Vec<Option<u64>>,
    styles: Vec<Style>,
    /// The link text is in, as a [`Span::link`].
    link: u32,
    /// Text goes between words.
    space: bool,
    /// A blank line goes before the next block.
    gap: bool,
    code: bool,
    table: Option<Table>,
    /// A line was cut at [`MAX_LINE`].
    long: bool,
    /// [`MAX_LINES`] are laid out, and the rest are dropped.
    full: bool,
    headings: Vec<Heading>,
    /// Text goes into the last heading.
    heading: bool,
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    head: usize,
}

impl Layout {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, change: impl FnOnce(&mut Style)) {
        let mut style = self.style();
        change(&mut style);
        self.styles.push(style);
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(table) = &mut self.table {
            match event {
                Event::End(TagEnd::Table) => self.table(),
                Event::Start(Tag::TableHead | Tag::TableRow) => table.rows.push(Vec::new()),
                Event::End(TagEnd::TableHead) => table.head = table.rows.len(),
                Event::Start(Tag::TableCell) => {
                    if let Some(row) = table.rows.last_mut() {
                        row.push(String::new());
                    }
                }
                Event::Text(text) | Event::Code(text) => {
                    if let Some(cell) = table.rows.last_mut().and_then(|row| row.last_mut()) {
                        cell.push_str(&escape(text.as_bytes()));
                    }
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) if self.code => self.code_text(&text, colour(Color::Cyan)),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                self.text(&text, self.style());
            }
            Event::Code(text) => self.text(&text, colour(Color::Cyan)),
            Event::SoftBreak => self.space = true,
            Event::HardBreak => self.newline(),
            Event::Rule => {
                self.block();
                let room = self
                    .width
                    .saturating_sub(self.prefix(false).0.chars().count());
                self.word(&"─".repeat(room), colour(Color::BrightBlack));
                self.end_block();
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "[x] " } else { "[ ] " };
                self.marker = Some(marker.to_owned());
                if let Some(indent) = self.indents.last_mut() {
                    *indent = " ".repeat(marker.len());
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.block(),
            Tag::Heading { level, .. } => {
                self.block();
                self.heading = self.lines.len() < MAX_LINES && self.headings.len() < MAX_HEADINGS;
                if self.heading {
                    self.headings.push(Heading {
                        line: self.lines.len(),
                        level: level as usize,
                        text: String::new(),
                    });
                }
                self.word(&"#".repeat(level as usize), colour(Color::BrightBlack));
                self.space = true;
                self.push_style(|style| style.bold = true);
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.indents.push("│ ".to_owned());
            }
            Tag::CodeBlock(_) => {
                self.block();
                self.code = true;
            }
            Tag::List(first) => {
                if self.lists.is_empty() {
                    self.block();
                } else {
                    self.flush();
                }
                self.lists.push(first);
            }
            Tag::Item => {
                self.flush();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        *number += 1;
                        format!("{}. ", *number - 1)
                    }
                    _ => "• ".to_owned(),
                };
                self.indents.push(" ".repeat(marker.chars().count()));
                self.marker = Some(marker);
            }
            Tag::Table(_) => {
                self.block();
                self.table = Some(Table::default());
            }
            Tag::Strong => self.push_style(|style| style.bold = true),
            Tag::Link { dest_url, .. } => {
                if self.lines.links.last().map(String::as_str) != Some(&*dest_url) {
                    self.lines.links.push(dest_url.into_string());
                }
                self.link = u32::try_from(self.lines.links.len()).unwrap_or(0);
                self.push_style(|style| style.fg = Color::Blue);
            }
            Tag::Image { .. } => {
                self.push_style(|style| style.fg = Color::BrightBlack);
                self.word("[image:", self.style());
                self.space = true;
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_block(),
            TagEnd::Heading(_) => {
                self.heading = false;
                self.styles.pop();
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.indents.pop();
                self.end_block();
            }
            TagEnd::CodeBlock => {
                self.code = false;
                self.end_block();
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.flush();
                self.gap = self.lists.is_empty();
            }
            TagEnd::Item => {
                self.flush();
                self.indents.pop();
                self.marker = None;
            }
            TagEnd::Strong => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.link = 0;
                self.styles.pop();
            }
            TagEnd::Image => {
                self.space = false;
                self.word("]", self.style());
                self.styles.pop();
            }
            _ => {}
        }
    }

    /// Ends the line in progress, and puts a blank line before a block that follows another.
    fn block(&mut self) {
        self.flush();
        if self.gap && self.lines.len() > 0 {
            let (prefix, _) = self.prefix(false);
            self.lines
                .append(prefix.trim_end(), colour(Color::BrightBlack), 0);
            self.push();
        }
        self.gap = false;
    }

    fn end_block(&mut self) {
        self.flush();
        self.gap = true;
    }

    fn flush(&mut self) {
        if !self.lines.open_is_empty() {
            self.newline();
        }
    }

    fn newline(&mut self) {
        if !self.full {
            self.widest = self.widest.max(self.used);
        }
        self.push();
        self.used = 0;
        self.space = false;
    }

    fn push(&mut self) {
        if !self.lines.end_line() {
            self.full = true;
        }
    }

    /// Adds `text` as it is, line by line, outside any block.
    fn plain(&mut self, text: &str) {
        self.flush();
        self.indents.clear();
        self.marker = None;
        self.styles.clear();
        self.link = 0;
        self.code = false;
        self.table = None;
        self.code_text(text, Style::default());
    }

    /// The indents that start a line, with the list marker if `marker`, and its length. It is
    /// cut to leave a column for text, however deep the nesting.
    fn prefix(&self, marker: bool) -> (String, usize) {
        let mut prefix = String::new();
        let last = self.indents.len().saturating_sub(1);
        for (at, indent) in self.indents.iter().enumerate() {
            match &self.marker {
                Some(bullet) if marker && at == last => prefix.push_str(bullet),
                _ => prefix.push_str(indent),
            }
            // Past the width, deeper indents would be cut anyway.
            if prefix.chars().count() >= self.width {
                break;
            }
        }
        let prefix: String = prefix.chars().take(self.width - 1).collect();
        let length = prefix.chars().count();
        (prefix, length)
    }

    /// Starts a line with its prefix if nothing is on it yet.
    fn start_line(&mut self) {
        if self.lines.open_is_empty() {
            let (prefix, length) = self.prefix(true);
            self.marker = None;
            if length > 0 {
                self.lines.append(&prefix, colour(Color::BrightBlack), 0);
            }
            self.used = length;
            self.start = length;
        }
    }

    /// Adds words, wrapping at the width.
    fn text(&mut self, text: &str, style: Style) {
        if text.starts_with(char::is_whitespace) {
            self.space = true;
        }
        for word in text.split_whitespace() {
            let word = escape(word.as_bytes());
            if let Some(heading) = self.headings.last_mut().filter(|_| self.heading)
                && heading.text.len() < MAX_LINE
            {
                if self.space && !heading.text.is_empty() {
                    heading.text.push(' ');
                }
                heading.text.push_str(&word);
            }
            self.word(&word, style);
            self.space = true;
        }
        if !text.is_empty() {
            self.space = text.ends_with(char::is_whitespace);
        }
    }

    /// Adds one escaped word, on the next line if it does not fit, and split if it is wider
    /// than a line.
    fn word(&mut self, word: &str, style: Style) {
        self.start_line();
        let start = self.start;
        let length = word.chars().count();
        let space = usize::from(self.space && self.used > start);
        if self.used > start && self.used + space + length > self.width {
            self.newline();
            self.start_line();
        } else if space == 1 {
            self.append(" ", style);
        }
        self.space = false;
        let mut rest = word;
        loop {
            let room = self.width.saturating_sub(self.used).max(1);
            let at = rest
                .char_indices()
                .nth(room)
                .map_or(rest.len(), |(at, _)| at);
            self.append(&rest[..at], style);
            rest = &rest[at..];
            if rest.is_empty() {
                break;
            }
            self.newline();
            self.start_line();
        }
    }

    /// Adds a code block's text line by line, in one style, cut at the edge rather than
    /// wrapped.
    fn code_text(&mut self, text: &str, style: Style) {
        for line in text.split_inclusive('\n') {
            if self.full {
                return;
            }
            self.start_line();
            let body = line.strip_suffix('\n').unwrap_or(line);
            self.append(&escape(body.replace('\t', "    ").as_bytes()), style);
            if line.ends_with('\n') {
                self.newline();
            }
        }
    }

    fn append(&mut self, text: &str, style: Style) {
        let room = MAX_LINE.saturating_sub(self.used);
        let text = match text.char_indices().nth(room) {
            Some((at, _)) => {
                self.long = true;
                &text[..at]
            }
            None => text,
        };
        self.used += text.chars().count();
        self.lines.append(text, style, self.link);
    }

    /// Draws the table that just ended, its columns as wide as their widest cell. Rows wider
    /// than the screen are cut at the edge, and `h` and `l` scroll to the rest.
    fn table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let mut widths = Vec::new();
        for row in &table.rows {
            widths.resize(widths.len().max(row.len()), 0);
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        let dim = colour(Color::BrightBlack);
        for (at, row) in table.rows.iter().enumerate() {
            if at == table.head && at > 0 {
                self.start_line();
                let rule: Vec<String> = widths.iter().map(|width| "─".repeat(*width)).collect();
                self.append(&rule.join("─┼─"), dim);
                self.newline();
            }
            let style = Style {
                bold: at < table.head,
                ..Style::default()
            };
            self.start_line();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    self.append(" │ ", dim);
                }
                let cell = row.get(column).map_or("", String::as_str);
                let pad = width - cell.chars().count();
                self.append(&format!("{cell}{}", " ".repeat(pad)), style);
            }
            self.newline();
        }
        self.end_block();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader's screen at 80×24, as rows and style runs.
    fn shown(title: &str, markdown: impl AsRef<[u8]>) -> String {
        shown_of(title, markdown.as_ref().to_vec(), true)
    }

    fn shown_of(title: &str, markdown: Vec<u8>, whole: bool) -> String {
        let mut reader = Reader::new(title.as_bytes().to_vec(), markdown, whole);
        let mut screen = Screen::new(24, 80);
        reader.draw(&mut screen);
        screen.runs()
    }

    #[test]
    fn headings() {
        let markdown = "# Plan\n\nSome **bold** text and `code`.\n\n## Next steps\n\n### Later\n";
        insta::assert_snapshot!(shown("PLAN.md", markdown));
    }

    #[test]
    fn lists() {
        let markdown = "- one\n- two\n  - nested\n\n1. first\n2. second\n\n- [x] done\n- [ ] \
                        open\n\n> quoted\n> - inside\n";
        insta::assert_snapshot!(shown("TODO.md", markdown));
    }

    #[test]
    fn a_table() {
        let markdown = "| Crate | Licence |\n| --- | --- |\n| pulldown-cmark | MIT |\n| unicase \
                        | MIT or Apache-2.0 |\n";
        insta::assert_snapshot!(shown("deps.md", markdown));
    }

    #[test]
    fn code() {
        let markdown = "```rust\nfn main() {\n\tprintln!(\"\x1b[2J\");\n}\n```\n\n---\n";
        insta::assert_snapshot!(shown("code.md", markdown));
    }

    #[test]
    fn links() {
        let markdown = "See [the ADR](docs/adr/0045.md) and <https://pinfold.sh>.\n\n\
                        ![a diagram](d.png)\n\n[hostile](<https://a\x1b]8;;x\x07>)\n";
        insta::assert_snapshot!(shown("links.md", markdown));
    }

    #[test]
    fn a_bidi_file_name() {
        let text = "Text with \u{202e}reversed\u{202c} and \x1b[31mred\x1b[0m bytes ";
        let markdown = [text.as_bytes(), b"\xff."].concat();
        insta::assert_snapshot!(shown("cat\u{202e}dm.exe", markdown));
    }

    #[test]
    fn long_words_and_lines_wrap_at_the_width() {
        let text = format!("{} short words here", "x".repeat(25));
        let (lines, widest, _) = layout(&text, 10, true, false, MAX_PARSE);
        assert_eq!(
            texts(&lines),
            ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx", "short", "words here"]
        );
        assert_eq!(widest, 10);
    }

    #[test]
    fn deep_nesting_leaves_a_column_for_text() {
        let (lines, _, _) = layout(
            &format!("{}deep", "> ".repeat(500)),
            6,
            true,
            false,
            MAX_PARSE,
        );
        let texts = texts(&lines);
        assert_eq!(texts[0], "│ │ │d");
        assert!(texts.iter().all(|line| line.chars().count() <= 6));
    }

    #[test]
    fn keys_scroll_within_the_file_and_q_quits() {
        let markdown: String = (1..=50).map(|n| format!("line {n}\n\n")).collect();
        let mut reader = Reader::new(b"long.md".to_vec(), markdown.into_bytes(), true);
        let mut screen = Screen::new(11, 40);
        reader.draw(&mut screen);
        assert_eq!(reader.key(Key::Char('j')), Flow::Continue);
        screen.clear();
        reader.draw(&mut screen);
        assert_eq!(screen.row(1).trim_end(), "");
        assert!(screen.row(0).ends_with("2/99  ? keys"));
        reader.key(Key::Char('G'));
        reader.key(Key::Down);
        assert_eq!(reader.top, 89, "the last page stays full");
        reader.key(Key::PageUp);
        assert_eq!(reader.top, 79);
        reader.key(Key::Home);
        reader.key(Key::Up);
        assert_eq!(reader.top, 0);
        reader.key(Key::Char('?'));
        reader.draw(&mut screen);
        assert!(screen.row(2).contains("a line down"));
        assert_eq!(reader.key(Key::Esc), Flow::Continue);
        assert!(!reader.help);
        assert_eq!(reader.key(Key::Char('q')), Flow::Quit);
    }

    #[test]
    fn h_and_l_scroll_sideways_to_a_cut_line() {
        let markdown = format!("```\n{}end\n```\n", "-".repeat(20));
        let mut reader = Reader::new(b"wide.md".to_vec(), markdown.into_bytes(), true);
        let mut screen = Screen::new(3, 10);
        reader.draw(&mut screen);
        for _ in 0..5 {
            reader.key(Key::Char('l'));
        }
        assert_eq!(reader.left, 13, "no further than the widest line");
        screen.clear();
        reader.draw(&mut screen);
        assert_eq!(screen.row(1), "-------end");
    }

    #[test]
    fn a_file_is_read_up_to_the_byte_cap_and_no_further() {
        let path = std::env::temp_dir().join(format!("pinfold-md-cap-{}", std::process::id()));
        std::fs::write(&path, vec![b'x'; MAX_BYTES + 100]).unwrap();
        let (bytes, whole) = read(&path).unwrap();
        assert_eq!((bytes.len(), whole), (MAX_BYTES, false));
        std::fs::write(&path, vec![b'x'; MAX_BYTES]).unwrap();
        let (bytes, whole) = read(&path).unwrap();
        assert_eq!((bytes.len(), whole), (MAX_BYTES, true));
        std::fs::remove_file(&path).unwrap();
        assert!(read(Path::new(".")).is_err(), "a directory is not read");
    }

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let path = std::env::temp_dir().join(format!("pinfold-md-fifo-{}", std::process::id()));
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let error = read(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn the_start_of_a_file_over_the_byte_cap_shows_as_plain_text_under_a_banner() {
        let markdown = b"# Big\n\nSome **bold** text \x1b[2J.\n".to_vec();
        insta::assert_snapshot!(shown_of("big.md", markdown, false));
    }

    #[test]
    fn past_the_time_cap_the_rest_shows_as_plain_text() {
        let markdown = "Some **bold** text.\n";
        let (lines, _, _) = layout(markdown, 80, true, false, Duration::ZERO);
        assert_eq!(
            texts(&lines),
            [
                "Parsing took over 2 s, so the rest shows as plain text.",
                "",
                "Some **bold** text."
            ]
        );
        let (lines, _, _) = layout(markdown, 80, true, false, MAX_PARSE);
        assert_eq!(texts(&lines), ["Some bold text."]);
    }

    #[test]
    fn lines_past_the_line_cap_are_dropped() {
        let markdown = format!("```\n{}```\n", "x\n".repeat(MAX_LINES + 10));
        let (lines, _, _) = layout(&markdown, 80, true, false, MAX_PARSE);
        assert_eq!(lines.len(), MAX_LINES + 2);
        assert_eq!(texts(&lines)[..1], ["Only the first 100000 lines show."]);
    }

    #[test]
    fn a_line_past_the_length_cap_is_cut() {
        let markdown = format!("```\n{}\n```\n", "y".repeat(MAX_LINE + 5));
        let (lines, widest, _) = layout(&markdown, 80, true, false, MAX_PARSE);
        assert_eq!(widest, MAX_LINE);
        assert_eq!(texts(&lines)[..1], ["Lines over 10000 characters are cut."]);
        assert_eq!(texts(&lines)[2].len(), MAX_LINE);
    }

    /// A reader for `markdown` and its 80×24 screen, drawn once.
    fn opened(markdown: &str) -> (Reader, Screen) {
        let mut reader = Reader::new(b"r.md".to_vec(), markdown.as_bytes().to_vec(), true);
        let mut screen = Screen::new(24, 80);
        reader.draw(&mut screen);
        (reader, screen)
    }

    fn redraw(reader: &mut Reader, screen: &mut Screen) {
        screen.clear();
        reader.draw(screen);
    }

    fn press(reader: &mut Reader, keys: &str) {
        for character in keys.chars() {
            reader.key(Key::Char(character));
        }
    }

    #[test]
    fn the_outline_goes_to_the_heading_picked() {
        let filler = "text\n\n".repeat(30);
        let markdown = format!(
            "# Plan\n\n{filler}## Next \x1b[31msteps `now`\n\n{filler}### Later\n\n{filler}"
        );
        let (mut reader, mut screen) = opened(&markdown);
        press(&mut reader, "o");
        assert_eq!(reader.outline, Some(0));
        reader.key(Key::Down);
        redraw(&mut reader, &mut screen);
        insta::assert_snapshot!(screen.runs());
        reader.key(Key::Enter);
        assert_eq!(
            (reader.outline, reader.top),
            (None, reader.headings[1].line)
        );
        redraw(&mut reader, &mut screen);
        assert!(screen.row(1).starts_with("## Next ^[[31msteps now"));
        press(&mut reader, "oGk");
        assert_eq!(
            reader.outline,
            Some(1),
            "opens at the heading above the screen"
        );
        reader.key(Key::Esc);
        assert_eq!(
            (reader.outline, reader.top),
            (None, reader.headings[1].line)
        );
    }

    #[test]
    fn a_search_hit_is_marked() {
        let markdown = "Find the **needle** here, and NEEDLE `need` too.\n\nNo hay.\n";
        let (mut reader, mut screen) = opened(markdown);
        press(&mut reader, "/needle");
        redraw(&mut reader, &mut screen);
        assert_eq!(screen.row(23).trim_end(), "/needle", "the prompt");
        reader.key(Key::Enter);
        redraw(&mut reader, &mut screen);
        insta::assert_snapshot!(screen.runs());
    }

    #[test]
    fn n_and_shift_n_wrap_around_the_matches() {
        let markdown: String = (0..60)
            .map(|n| {
                if n % 20 == 5 {
                    "apple\n\n".to_owned()
                } else {
                    format!("line {n}\n\n")
                }
            })
            .collect();
        let (mut reader, _) = opened(&markdown);
        reader.top = 20;
        press(&mut reader, "/APPLE");
        reader.key(Key::Enter);
        assert_eq!(reader.matches, [10, 50, 90]);
        assert_eq!(
            (reader.current, reader.top),
            (1, 50),
            "the first from the top of the screen"
        );
        press(&mut reader, "n");
        assert_eq!((reader.current, reader.top), (2, 90));
        press(&mut reader, "n");
        assert_eq!(
            (reader.current, reader.top),
            (0, 10),
            "past the last, the first"
        );
        press(&mut reader, "N");
        assert_eq!(reader.current, 2, "before the first, the last");
        reader.key(Key::Esc);
        assert!(reader.matches.is_empty());
        press(&mut reader, "/pear");
        reader.key(Key::Enter);
        assert!(reader.matches.is_empty());
        press(&mut reader, "n");
        assert_eq!(reader.current, 0);
    }

    #[test]
    fn the_outline_and_search_stop_at_their_caps() {
        let (lines, _, headings) = layout(
            &"# h\n\n".repeat(MAX_HEADINGS + 5),
            80,
            true,
            false,
            MAX_PARSE,
        );
        assert_eq!(headings.len(), MAX_HEADINGS);
        assert!(lines.len() > headings[MAX_HEADINGS - 1].line + 2);
        let (mut reader, _) = opened(&"x\n\n".repeat(MAX_MATCHES + 5));
        press(&mut reader, "/x");
        reader.key(Key::Enter);
        assert_eq!(reader.matches.len(), MAX_MATCHES);
    }

    #[test]
    fn a_heading_under_the_banner_still_points_at_its_line() {
        let markdown = format!("```\n{}\n```\n\n# Title\n", "y".repeat(MAX_LINE + 5));
        let (lines, _, headings) = layout(&markdown, 80, true, false, MAX_PARSE);
        assert_eq!(headings.len(), 1);
        assert_eq!(texts(&lines)[headings[0].line], "# Title");
    }

    #[test]
    fn parsing_a_window_at_a_time_gives_the_events_of_the_whole_text() {
        let blocks = [
            "# Title\n\nSee [the ref][later], [Later] and [x]: no.\n",
            "Setext\n======\n\nAnother\n---\n",
            "| a | b |\n| - | - |\n| 1 | 2 |\nlazy row\n",
            "```\ncode\n\n# not a heading\n\n```\n",
            "    indented\n\n    still code\n",
            "- tight\n- list\n\n- now loose\n\n  more of it\n\n1. one\n2. two\n",
            "> quote\nlazy\n\n> - [x] task\n>   ```\n>   fenced\n",
            "<!-- a comment\n\n# inside it\n\n-->\n\n<div>\nhtml\n</div>\n",
            "Hard  \nbreak and `code`, **bold *both***\n\n***\n",
            "[later]: https://pinfold.sh/later \"Title\"\n[x]:\n  <spaced dest>\n",
            "[d1]: /one\n[d2]: /two\n\"titled\"\n[d3]: /three\ntext after [d1] [d4]\n\n",
            "- [y]: /in-a-list\n  [w]: /indented\n\n> [q]: /quoted\n> [d4]: /four\n\n",
        ];
        let text = blocks.concat().repeat(4);
        let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
        let whole: Vec<_> = Parser::new_ext(&text, options)
            .into_offset_iter()
            .map(|(event, range)| format!("{event:?} {range:?}"))
            .collect();
        assert!(whole.iter().any(|event| event.contains("pinfold.sh/later")));
        for window in [1, 7, 64, 300, WINDOW] {
            let (defs, _) = definitions(&text, window, || true);
            let mut events = Vec::new();
            parse(&text, &defs, window, |event, range| {
                events.push(format!("{event:?} {range:?}"));
                true
            });
            assert_eq!(events, whole, "{window}-byte windows");
        }
    }

    #[test]
    fn link_definitions_above_each_window_stop_at_their_cap() {
        let text: String = (0..20_000).map(|n| format!("[a{n}]: x\n")).collect();
        let (defs, cut) = definitions(&text, WINDOW, || true);
        assert!(cut);
        assert!(defs.len() <= MAX_DEFS && defs.len() > MAX_DEFS - 100);
        assert!(defs.starts_with("[a0]: x\n[a1]: x\n"), "the first are kept");
        let (lines, _, _) = layout(&text, 80, true, false, MAX_PARSE);
        assert_eq!(
            texts(&lines),
            ["Only the first 64 KiB of link definitions are used.", ""]
        );
        let (_, cut) = definitions("[a]: x\n", WINDOW, || true);
        assert!(!cut);
    }

    /// The median time to lay out each of `cargo xtask mem-gate`'s 5 MB files at 80 columns:
    /// `cargo test --release -p pinfold-tools layout_time -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn layout_time_at_the_byte_cap() {
        // As `write_tool_inputs` in `xtask/src/main.rs` writes them.
        let markdown = |section: usize| {
            format!(
                "## Section {section}\n\n見出し 日本語 中文 한국어\n\nSome *emphasis*, **bold**, \
                 `code` and a [link](https://pinfold.sh/{section}).\n\n- one\n- two\n\n\
                 | a | b |\n| --- | --- |\n| 1 | 2 |\n\n```\nfn main() {{}}\n```\n\n"
            )
        };
        let prose = |paragraph: usize| {
            format!(
                "Paragraph {paragraph} of plain prose, with words enough to wrap across two \
                 lines at eighty columns and no markup at all.\n\n"
            )
        };
        let defs =
            |definition: usize| format!("[a{definition}]: https://pinfold.sh/{definition}\n");
        let inputs: [(&str, &dyn Fn(usize) -> String); 3] = [
            ("markdown", &markdown),
            ("plain prose", &prose),
            ("link definitions", &defs),
        ];
        for (name, part) in inputs {
            let mut bytes = Vec::new();
            for at in 0.. {
                if bytes.len() >= MAX_BYTES {
                    break;
                }
                bytes.extend(part(at).bytes());
            }
            bytes.truncate(MAX_BYTES);
            let text = decode(bytes);
            let mut times: Vec<Duration> = (0..5)
                .map(|_| {
                    let started = Instant::now();
                    layout(&text, 80, true, false, MAX_PARSE);
                    started.elapsed()
                })
                .collect();
            times.sort();
            eprintln!("{name}: {:.1} ms", times[2].as_secs_f64() * 1000.0);
        }
    }

    /// A new empty directory for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pinfold-md-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn reader_of(path: &Path) -> (Reader, Screen) {
        let (bytes, whole) = read(path).unwrap();
        let mut reader = Reader::new(b"r.md".to_vec(), bytes, whole);
        reader.path = path.to_path_buf();
        let mut screen = Screen::new(24, 80);
        reader.draw(&mut screen);
        (reader, screen)
    }

    #[test]
    fn relative_and_file_links_are_files_and_others_are_not() {
        let file = Path::new("/docs/adr/README.md");
        for (target, want) in [
            ("0045.md", Some("/docs/adr/0045.md")),
            ("../design/x.md#caps", Some("/docs/adr/../design/x.md")),
            ("a%20b%2Fc.md?raw=1", Some("/docs/adr/a b/c.md")),
            ("%zz%4", Some("/docs/adr/%zz%4")),
            ("/etc/hosts", Some("/etc/hosts")),
            ("file:///tmp/a%20b.md", Some("/tmp/a b.md")),
            ("FILE://localhost/tmp/x", Some("/tmp/x")),
            ("file://host/x", None),
            ("file:x.md", None),
            ("#caps", None),
            ("", None),
            ("https://pinfold.sh/a.md", None),
            ("mailto:a@b", None),
            ("javascript:alert(1)", None),
        ] {
            assert_eq!(local(file, target), want.map(PathBuf::from), "{target}");
        }
    }

    #[test]
    fn a_link_opens_in_the_reader_and_backspace_goes_back() {
        let dir = scratch("links");
        let start = dir.join("a.md");
        let markdown =
            "See [the web](<https://pinfold.sh/\x1b[2J>) or [b](b.md) or [gone](c.md).\n";
        std::fs::write(&start, markdown).unwrap();
        std::fs::write(dir.join("b.md"), "# B\n").unwrap();
        let (mut reader, mut screen) = reader_of(&start);
        reader.key(Key::Tab);
        reader.key(Key::Enter);
        redraw(&mut reader, &mut screen);
        assert_eq!(
            screen.row(23).trim_end(),
            "Not a file, so open it from the text: https://pinfold.sh/^[[2J"
        );
        reader.key(Key::Tab);
        reader.key(Key::Enter);
        assert_eq!(
            (reader.text.as_str(), &reader.path),
            ("# B\n", &dir.join("b.md"))
        );
        reader.key(Key::Backspace);
        assert_eq!(reader.path, start);
        reader.key(Key::Tab);
        reader.key(Key::Tab);
        reader.key(Key::Tab);
        reader.key(Key::Enter);
        redraw(&mut reader, &mut screen);
        assert!(
            screen.row(23).starts_with("Cannot open c.md: "),
            "{}",
            screen.row(23)
        );
        assert_eq!(reader.path, start);
        reader.key(Key::Tab);
        assert_eq!(reader.focus, Some((0, 1)), "past the last link, the first");
        redraw(&mut reader, &mut screen);
        let at = screen.row(1).find(" b ").unwrap() + 1;
        let click = crate::keys::Mouse {
            button: 0,
            column: u16::try_from(screen.row(1)[..at].chars().count()).unwrap(),
            row: 1,
            pressed: true,
        };
        reader.key(Key::Mouse(click));
        assert_eq!(reader.path, dir.join("b.md"), "a click follows a link");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backspace_remembers_at_most_its_cap_of_files() {
        let dir = scratch("back");
        let path = dir.join("self.md");
        std::fs::write(&path, "[again](self.md)\n").unwrap();
        let (mut reader, _) = reader_of(&path);
        for _ in 0..MAX_BACK + 5 {
            reader.follow_link("self.md");
        }
        assert_eq!(reader.back.len(), MAX_BACK);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn follow_mode_reads_a_changed_file_again_and_keeps_the_place() {
        let dir = scratch("follow");
        let path = dir.join("log.md");
        let numbered = |count: usize| {
            (0..count)
                .map(|n| format!("line {n}\n\n"))
                .collect::<String>()
        };
        std::fs::write(&path, numbered(40)).unwrap();
        let (mut reader, mut screen) = reader_of(&path);
        reader.key(Key::Char('F'));
        reader.top = 10;
        let check = |reader: &mut Reader, screen: &mut Screen| {
            reader.follow = reader
                .follow
                .map(|(seen, _)| (seen, Instant::now() - FOLLOW));
            reader.tick();
            redraw(reader, screen);
        };
        std::fs::write(&path, numbered(60)).unwrap();
        check(&mut reader, &mut screen);
        assert_eq!(reader.lines.len(), 119, "a write");
        assert_eq!((reader.top, screen.row(1).trim_end()), (10, "line 5"));
        assert!(screen.row(0).contains("follow"));
        std::fs::write(&path, "").unwrap();
        check(&mut reader, &mut screen);
        assert_eq!((reader.lines.len(), reader.top), (0, 0), "truncated");
        let new = dir.join("new.md");
        std::fs::write(&new, "replaced\n").unwrap();
        std::fs::rename(&new, &path).unwrap();
        check(&mut reader, &mut screen);
        assert_eq!(screen.row(1).trim_end(), "replaced", "replaced by a rename");
        std::fs::remove_file(&path).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        check(&mut reader, &mut screen);
        assert_eq!(
            screen.row(1).trim_end(),
            "replaced",
            "a FIFO is not read, nor waited on"
        );
        reader.key(Key::Char('F'));
        assert!(reader.follow.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn each_escape_in_file_content_shows_escaped_as_markdown_and_as_plain_text() {
        let escapes: [(&[u8], &str); 14] = [
            (b"\x07", "^G"),
            (b"\x08", "^H"),
            (b"\x1b", "^["),
            (b"\x7f", "^?"),
            ("\u{85}".as_bytes(), "<U+0085>"),
            ("\u{9b}".as_bytes(), "<U+009B>"),
            ("\u{61c}".as_bytes(), "<U+061C>"),
            ("\u{200e}".as_bytes(), "<U+200E>"),
            ("\u{200f}".as_bytes(), "<U+200F>"),
            ("\u{202a}".as_bytes(), "<U+202A>"),
            ("\u{202e}".as_bytes(), "<U+202E>"),
            ("\u{2066}".as_bytes(), "<U+2066>"),
            ("\u{2069}".as_bytes(), "<U+2069>"),
            (b"\xff", "\\xFF"),
        ];
        for (raw, escaped) in escapes {
            let markdown = [
                b"a",
                raw,
                b"b\n\n```\nc",
                raw,
                b"d\n```\n\n| e",
                raw,
                b"f |\n| - |\n",
            ]
            .concat();
            for whole in [true, false] {
                let reader = Reader::new(Vec::new(), markdown.clone(), whole);
                let shown = texts(&layout(&reader.text, 80, whole, !whole, MAX_PARSE).0).concat();
                let raw_text = std::str::from_utf8(raw).unwrap_or("\u{fffd}");
                assert!(!shown.contains(raw_text), "{raw:?} drawn raw");
                for (before, after) in [("a", "b"), ("c", "d"), ("e", "f")] {
                    // U+0085 is whitespace, so in a paragraph it only separates words.
                    if whole && before == "a" && raw_text == "\u{85}" {
                        continue;
                    }
                    let want = format!("{before}{escaped}{after}");
                    assert!(shown.contains(&want), "{want} missing from {shown:?}");
                }
            }
        }
    }
}
