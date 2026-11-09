//! The markdown reader behind `pinfold md` (ADR-0045 sections 2 and 3).
//!
//! The file is laid out again for each width, as lines of styled spans that are already
//! escaped, so a line's length in characters is its length in cells.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::keys::Key;
use crate::screen::{Color, Screen, Style, escape};
use crate::{Flow, Tool};

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

/// The reader: a file's text, laid out at the screen's width.
pub(crate) struct Reader {
    title: Vec<u8>,
    text: String,
    width: u16,
    lines: Vec<Line>,
    widest: usize,
    top: usize,
    left: usize,
    page: usize,
    help: bool,
}

impl Reader {
    /// A reader titled `title` for the bytes of a file; invalid UTF-8 shows as `\xNN`.
    pub(crate) fn new(title: Vec<u8>, bytes: &[u8]) -> Self {
        let mut text = String::with_capacity(bytes.len());
        for chunk in bytes.utf8_chunks() {
            text.push_str(chunk.valid());
            for byte in chunk.invalid() {
                text.push_str(&format!("\\x{byte:02X}"));
            }
        }
        Self {
            title,
            text,
            width: 0,
            lines: Vec::new(),
            widest: 0,
            top: 0,
            left: 0,
            page: 1,
            help: false,
        }
    }

    fn last_top(&self) -> usize {
        self.lines.len().saturating_sub(self.page)
    }
}

impl Tool for Reader {
    fn draw(&mut self, screen: &mut Screen) {
        let (rows, columns) = screen.size();
        if columns != self.width {
            (self.lines, self.widest) = layout(&self.text, usize::from(columns));
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

/// Styles the layout uses.
fn colour(fg: Color) -> Style {
    Style {
        fg,
        ..Style::default()
    }
}

/// Lays `text` out at `width` columns, and returns the lines and the widest one's length.
fn layout(text: &str, width: usize) -> (Vec<Line>, usize) {
    let options = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let mut layout = Layout {
        width: width.max(1),
        ..Layout::default()
    };
    for event in Parser::new_ext(text, options) {
        layout.event(event);
    }
    layout.flush();
    (layout.lines, layout.widest)
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
            Event::Text(text) if self.code => self.code_text(&text),
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
            self.lines
                .push(vec![span(prefix.trim_end(), colour(Color::BrightBlack))]);
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
        self.widest = self.widest.max(self.used);
        self.lines.push(std::mem::take(&mut self.line));
        self.used = 0;
        self.space = false;
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

    /// Adds a code block's text line by line, in one colour, cut at the edge rather than
    /// wrapped.
    fn code_text(&mut self, text: &str) {
        for line in text.split_inclusive('\n') {
            self.start_line();
            let body = line.strip_suffix('\n').unwrap_or(line);
            self.append(
                &escape(body.replace('\t', "    ").as_bytes()),
                colour(Color::Cyan),
            );
            if line.ends_with('\n') {
                self.newline();
            }
        }
    }

    fn append(&mut self, text: &str, style: Style) {
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
        let mut reader = Reader::new(title.as_bytes().to_vec(), markdown.as_ref());
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
        let (lines, widest) = layout(&format!("{} short words here", "x".repeat(25)), 10);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert_eq!(
            text,
            ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx", "short", "words here"]
        );
        assert_eq!(widest, 10);
    }

    #[test]
    fn deep_nesting_leaves_a_column_for_text() {
        let (lines, _) = layout(&format!("{}deep", "> ".repeat(500)), 6);
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
        let mut reader = Reader::new(b"long.md".to_vec(), markdown.as_bytes());
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
        let mut reader = Reader::new(b"wide.md".to_vec(), markdown.as_bytes());
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
}
