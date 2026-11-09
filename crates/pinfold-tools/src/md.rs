//! The markdown reader behind `pinfold md` (ADR-0045 sections 2 and 3).
//!
//! The file is laid out again for each width, as lines of styled spans that are already
//! escaped, so a line's length in characters is its length in cells.
//!
//! Each cap is a constant (ADR-0045 section 4), and the reader says above the file when one
//! cut it.

use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// Columns `h` and `l` move by.
const SIDEWAYS: usize = 8;

const KEYS: [&str; 7] = [
    "j k  ↓ ↑              a line down or up",
    "h l  ← →              sideways",
    "space b  PgDn PgUp    a page down or up",
    "g G  Home End         the top or the end",
    "wheel                 three lines",
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
    lines
        .iter()
        .map(|line| line.iter().map(|span| span.text.as_str()).collect())
        .collect()
}

/// The reader: a file's text, laid out at the screen's width.
pub(crate) struct Reader {
    title: Vec<u8>,
    text: String,
    /// The whole file was read, so it is parsed as markdown if `markdown`.
    whole: bool,
    markdown: bool,
    width: u16,
    lines: Vec<Line>,
    widest: usize,
    top: usize,
    left: usize,
    page: usize,
    help: bool,
}

impl Reader {
    /// A reader titled `title` for the bytes of a file, all of it if `whole`, or else its
    /// start, which shows as plain text. Invalid UTF-8 shows as `\xNN`.
    pub(crate) fn new(title: Vec<u8>, bytes: Vec<u8>, whole: bool) -> Self {
        let text = String::from_utf8(bytes).unwrap_or_else(|error| {
            let bytes = error.into_bytes();
            let mut text = String::with_capacity(bytes.len());
            for chunk in bytes.utf8_chunks() {
                text.push_str(chunk.valid());
                for byte in chunk.invalid() {
                    text.push_str(&format!("\\x{byte:02X}"));
                }
            }
            text
        });
        Self {
            title,
            text,
            whole,
            markdown: true,
            width: 0,
            lines: Vec::new(),
            widest: 0,
            top: 0,
            left: 0,
            page: 1,
            help: false,
        }
    }

    /// Shows the text as it is, not as markdown, as the file manager's pager does.
    pub(crate) fn plain(mut self) -> Self {
        self.markdown = false;
        self
    }

    fn last_top(&self) -> usize {
        self.lines.len().saturating_sub(self.page)
    }
}

impl Tool for Reader {
    fn draw(&mut self, screen: &mut Screen) {
        let (rows, columns) = screen.size();
        if columns != self.width {
            (self.lines, self.widest) = layout(
                &self.text,
                usize::from(columns),
                self.whole && self.markdown,
                !self.whole,
                MAX_PARSE,
            );
            self.width = columns;
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
        let place = format!("{}/{}  ? keys", self.top + 1, self.lines.len().max(1));
        let at = columns.saturating_sub(u16::try_from(place.len()).unwrap_or(u16::MAX));
        screen.put(0, at, place, dim);
        if self.help {
            for (row, keys) in (2..rows).zip(KEYS) {
                screen.put(row, 2, keys, Style::default());
            }
            return;
        }
        for (row, line) in (1..rows).zip(self.lines.iter().skip(self.top)) {
            let mut x = 0;
            for span in line {
                let length = span.text.chars().count();
                if x + length > self.left {
                    let skip = self.left.saturating_sub(x);
                    let column = u16::try_from(x + skip - self.left).unwrap_or(u16::MAX);
                    let at = span.text.char_indices().nth(skip).map_or(0, |(at, _)| at);
                    let text = &span.text[at..];
                    match &span.link {
                        Some(target) => {
                            screen.put_link(row, column, text, span.style, target.as_bytes())
                        }
                        None => screen.put(row, column, text, span.style),
                    };
                }
                x += length;
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
        let page = self.page;
        match key {
            Key::Char('q') => return Flow::Quit,
            Key::Char('?') => self.help = true,
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
/// text after, and returns the lines and the widest one's length. A banner above says what
/// any cap cut, the byte cap if `cut`.
fn layout(
    text: &str,
    width: usize,
    markdown: bool,
    cut: bool,
    budget: Duration,
) -> (Vec<Line>, usize) {
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
        return (layout.lines, layout.widest);
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
    banner.lines.append(&mut layout.lines);
    (banner.lines, banner.widest.max(layout.widest))
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
            self.word(&escape(word.as_bytes()), style);
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
        let (lines, widest) = layout(&text, 10, true, false, MAX_PARSE);
        assert_eq!(
            texts(&lines),
            ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx", "short", "words here"]
        );
        assert_eq!(widest, 10);
    }

    #[test]
    fn deep_nesting_leaves_a_column_for_text() {
        let (lines, _) = layout(
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
        let (lines, _) = layout(markdown, 80, true, false, Duration::ZERO);
        assert_eq!(
            texts(&lines),
            [
                "Parsing took over 2 s, so the rest shows as plain text.",
                "",
                "Some **bold** text."
            ]
        );
        let (lines, _) = layout(markdown, 80, true, false, MAX_PARSE);
        assert_eq!(texts(&lines), ["Some bold text."]);
    }

    #[test]
    fn lines_past_the_line_cap_are_dropped() {
        let markdown = format!("```\n{}```\n", "x\n".repeat(MAX_LINES + 10));
        let (lines, _) = layout(&markdown, 80, true, false, MAX_PARSE);
        assert_eq!(lines.len(), MAX_LINES + 2);
        assert_eq!(texts(&lines[..1]), ["Only the first 100000 lines show."]);
    }

    #[test]
    fn a_line_past_the_length_cap_is_cut() {
        let markdown = format!("```\n{}\n```\n", "y".repeat(MAX_LINE + 5));
        let (lines, widest) = layout(&markdown, 80, true, false, MAX_PARSE);
        assert_eq!(widest, MAX_LINE);
        assert_eq!(texts(&lines[..1]), ["Lines over 10000 characters are cut."]);
        assert_eq!(texts(&lines[2..])[0].len(), MAX_LINE);
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
