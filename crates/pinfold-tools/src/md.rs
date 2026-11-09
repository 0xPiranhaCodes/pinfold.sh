//! The markdown reader behind `pinfold md` (ADR-0045 sections 2 and 3).
//!
//! The file is laid out again for each width, as lines of styled spans that are already
//! escaped, so a line's length in characters is its length in cells.
//!
//! Each cap is a constant (ADR-0045 section 4), and the reader says above the file when one
//! cut it.

use std::ffi::OsString;
use std::io::{self, Read};
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    text: String,
    style: Style,
    link: Option<String>,
}

type Line = Vec<Span>;

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

fn texts(lines: &[Line]) -> Vec<String> {
    lines.iter().map(text).collect()
}

fn text(line: &Line) -> String {
    line.iter().map(|span| span.text.as_str()).collect()
}

/// The laid-out text, which is escaped, lowercased in ASCII only so byte offsets still match.
fn folded(text: &str) -> String {
    text.to_ascii_lowercase()
}

/// What follow mode compares: the file's modification time, length and inode.
type Stamp = (SystemTime, u64, u64);

fn stamp(path: &Path) -> Option<Stamp> {
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
    lines: Vec<Line>,
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
            lines: Vec::new(),
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
        for (at, line) in self.lines.iter().enumerate() {
            for (index, span) in line.iter().enumerate() {
                // A link wrapped or restyled across spans is one link.
                if span.link.is_some() && span.link.as_ref() != last {
                    starts.push((at, index));
                }
                last = span.link.as_ref();
            }
        }
        starts
    }

    fn link(&self, (line, span): (usize, usize)) -> Option<String> {
        self.lines.get(line)?.get(span)?.link.clone()
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
        for (index, span) in self.lines.get(line)?.iter().enumerate() {
            let length = span.text.chars().count();
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
            (self.lines.iter().enumerate())
                .filter(|(_, line)| folded(&text(line)).contains(&needle))
                .map(|(at, _)| at)
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

    /// Marks each visible occurrence of the search on `row`, which shows `line`.
    fn mark_matches(&self, screen: &mut Screen, row: u16, line: &Line) {
        let needle = self.needle();
        let text = text(line);
        let hit = Style {
            fg: Color::Black,
            bg: Color::Yellow,
            bold: false,
        };
        for (at, found) in folded(&text).match_indices(&needle) {
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
        let lines = self.lines.iter().enumerate().skip(self.top);
        for (row, (at, line)) in (1..rows).zip(lines) {
            let mut x = 0;
            for (index, span) in line.iter().enumerate() {
                let length = span.text.chars().count();
                let focused = Style {
                    fg: Color::Black,
                    bg: Color::Blue,
                    ..span.style
                };
                let style = if self.focus == Some((at, index)) {
                    focused
                } else {
                    span.style
                };
                if x + length > self.left {
                    let skip = self.left.saturating_sub(x);
                    let column = u16::try_from(x + skip - self.left).unwrap_or(u16::MAX);
                    let from = span.text.char_indices().nth(skip).map_or(0, |(at, _)| at);
                    let text = &span.text[from..];
                    match &span.link {
                        Some(target) => {
                            screen.put_link(row, column, text, style, target.as_bytes())
                        }
                        None => screen.put(row, column, text, style),
                    };
                }
                x += length;
            }
            if !self.typing && !self.matches.is_empty() {
                self.mark_matches(screen, row, line);
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
) -> (Vec<Line>, usize, Vec<Heading>) {
    let width = width.clamp(1, MAX_LINE);
    let mut layout = Layout {
        width,
        ..Layout::default()
    };
    let mut notes = Vec::new();
    if markdown {
        let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
        let started = Instant::now();
        // shortcut: the time is checked between events, so pulldown-cmark's first pass over the
        // blocks and any one slow event are not cut short; a parse thread with a deadline would
        // be, if a hostile file is ever found to stall either.
        // Where the text laid out so far ends: a start's range covers its whole element.
        let mut done = 0;
        for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
            if layout.full {
                break;
            }
            if started.elapsed() > budget {
                notes.push(format!(
                    "Parsing took over {} s, so the rest shows as plain text.",
                    MAX_PARSE.as_secs()
                ));
                let at = (done..text.len())
                    .find(|&at| text.is_char_boundary(at))
                    .unwrap_or(text.len());
                layout.plain(&text[at..]);
                break;
            }
            if !matches!(event, Event::Start(_)) {
                done = done.max(range.end);
            }
            layout.event(event);
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
    banner.lines.append(&mut layout.lines);
    (
        banner.lines,
        banner.widest.max(layout.widest),
        layout.headings,
    )
}

#[derive(Default)]
struct Layout {
    width: usize,
    lines: Vec<Line>,
    line: Line,
    /// Characters on `line`, and in its prefix.
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
    link: Option<String>,
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
                self.link = Some(dest_url.into_string());
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
                self.link = None;
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
        if self.gap && !self.lines.is_empty() {
            let (prefix, _) = self.prefix(false);
            self.push(vec![span(prefix.trim_end(), colour(Color::BrightBlack))]);
        }
        self.gap = false;
    }

    fn end_block(&mut self) {
        self.flush();
        self.gap = true;
    }

    fn flush(&mut self) {
        if !self.line.is_empty() {
            self.newline();
        }
    }

    fn newline(&mut self) {
        let line = std::mem::take(&mut self.line);
        if !self.full {
            self.widest = self.widest.max(self.used);
        }
        self.push(line);
        self.used = 0;
        self.space = false;
    }

    fn push(&mut self, line: Line) {
        if self.lines.len() < MAX_LINES {
            self.lines.push(line);
        } else {
            self.full = true;
        }
    }

    /// Adds `text` as it is, line by line, outside any block.
    fn plain(&mut self, text: &str) {
        self.flush();
        self.indents.clear();
        self.marker = None;
        self.styles.clear();
        self.link = None;
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
        if self.line.is_empty() {
            let (prefix, length) = self.prefix(true);
            self.marker = None;
            if length > 0 {
                self.line.push(span(&prefix, colour(Color::BrightBlack)));
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
        match self.line.last_mut() {
            Some(last) if last.style == style && last.link == self.link => {
                last.text.push_str(text);
            }
            _ => self.line.push(Span {
                text: text.to_owned(),
                style,
                link: self.link.clone(),
            }),
        }
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

fn span(text: &str, style: Style) -> Span {
    Span {
        text: text.to_owned(),
        style,
        link: None,
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
        let first: String = lines[0].iter().map(|span| span.text.as_str()).collect();
        assert_eq!(first, "│ │ │d");
        assert!(
            lines
                .iter()
                .all(|line| line.iter().map(|s| s.text.chars().count()).sum::<usize>() <= 6)
        );
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
        assert_eq!(texts(&lines[..1]), ["Only the first 100000 lines show."]);
    }

    #[test]
    fn a_line_past_the_length_cap_is_cut() {
        let markdown = format!("```\n{}\n```\n", "y".repeat(MAX_LINE + 5));
        let (lines, widest, _) = layout(&markdown, 80, true, false, MAX_PARSE);
        assert_eq!(widest, MAX_LINE);
        assert_eq!(texts(&lines[..1]), ["Lines over 10000 characters are cut."]);
        assert_eq!(texts(&lines[2..])[0].len(), MAX_LINE);
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
