//! A tool's screen: cells of a character and a style, written to the terminal a row at a time.

use std::fmt::Write as _;

/// One of the 16 palette roles, or the terminal's own colour, so the user's theme applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(missing_docs, reason = "the names are the palette's")]
pub enum Color {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl Color {
    /// The SGR parameter for this colour in the foreground; the background's is 10 more.
    fn sgr(self) -> u8 {
        match self as u8 {
            0 => 39,
            index @ 1..=8 => 29 + index,
            index => 81 + index,
        }
    }
}

/// How a cell is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// The character's colour.
    pub fg: Color,
    /// The cell's colour.
    pub bg: Color,
    /// Bold text.
    pub bold: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    character: char,
    style: Style,
}

const BLANK: Cell = Cell {
    character: ' ',
    style: Style {
        fg: Color::Default,
        bg: Color::Default,
        bold: false,
    },
};

/// Returns `text` with nothing that could act on the terminal or reorder what is shown: C0
/// controls become `^X`, DEL `^?`, C1 and bidi controls `<U+0085>`, and invalid UTF-8 `\xNN`.
///
/// # Examples
///
/// ```
/// use pinfold_tools::screen::escape;
///
/// assert_eq!(escape(b"a\x1b[2J\x7f\xff"), "a^[[2J^?\\xFF");
/// assert_eq!(escape("cat\u{202e}gnp.exe".as_bytes()), "cat<U+202E>gnp.exe");
/// ```
pub fn escape(text: &[u8]) -> String {
    let mut escaped = String::with_capacity(text.len());
    for chunk in text.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\0'..='\x1f' => {
                    escaped.push('^');
                    escaped.push(char::from(character as u8 + 0x40));
                }
                '\x7f' => escaped.push_str("^?"),
                '\u{80}'..='\u{9f}'
                | '\u{61c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}' => {
                    let _ = write!(escaped, "<U+{:04X}>", u32::from(character));
                }
                _ => escaped.push(character),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(escaped, "\\x{byte:02X}");
        }
    }
    escaped
}

/// The cells a tool draws, and those the terminal shows, so a frame sends only changed rows.
///
/// # Examples
///
/// ```
/// use pinfold_tools::screen::{Screen, Style};
///
/// let mut screen = Screen::new(2, 10);
/// screen.put(0, 0, "hello", Style::default());
/// let mut out = Vec::new();
/// screen.frame(&mut out);
/// assert!(!out.is_empty());
/// out.clear();
/// screen.frame(&mut out);
/// assert!(out.is_empty(), "nothing changed");
/// ```
#[derive(Debug)]
pub struct Screen {
    rows: u16,
    columns: u16,
    cells: Vec<Cell>,
    /// What the terminal shows, or nothing before the first frame.
    shown: Vec<Cell>,
}

impl Screen {
    /// A blank screen of `rows` by `columns`, none of it drawn yet.
    pub fn new(rows: u16, columns: u16) -> Self {
        Self {
            rows,
            columns,
            cells: vec![BLANK; usize::from(rows) * usize::from(columns)],
            shown: Vec::new(),
        }
    }

    /// The rows and columns.
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.columns)
    }

    /// Blanks every cell.
    pub fn clear(&mut self) {
        self.cells.fill(BLANK);
    }

    /// Draws `text`, escaped by [`escape`], at `row` and `column`, cut at the right edge, and
    /// returns the column after it. This is the only way text reaches the screen. Each
    /// character takes one cell.
    pub fn put(&mut self, row: u16, column: u16, text: impl AsRef<[u8]>, style: Style) -> u16 {
        let mut column = column;
        if row >= self.rows {
            return column;
        }
        for character in escape(text.as_ref()).chars() {
            if column >= self.columns {
                break;
            }
            let index = usize::from(row) * usize::from(self.columns) + usize::from(column);
            self.cells[index] = Cell { character, style };
            column += 1;
        }
        column
    }

    /// The characters of `row`, for tests and snapshots.
    pub fn row(&self, row: u16) -> String {
        self.row_cells(&self.cells, row)
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    fn row_cells<'a>(&self, cells: &'a [Cell], row: u16) -> &'a [Cell] {
        let start = usize::from(row) * usize::from(self.columns);
        cells
            .get(start..start + usize::from(self.columns))
            .unwrap_or(&[])
    }

    /// Appends to `out` the rows that changed since the last frame, inside one synchronized
    /// update, and nothing at all if none did.
    pub fn frame(&mut self, out: &mut Vec<u8>) {
        let start = out.len();
        for row in 0..self.rows {
            let cells = self.row_cells(&self.cells, row);
            if self.row_cells(&self.shown, row) == cells {
                continue;
            }
            if out.len() == start {
                out.extend_from_slice(b"\x1b[?2026h");
            }
            let _ = write!(Bytes(out), "\x1b[{};1H", row + 1);
            let mut current = None;
            for cell in cells {
                if current != Some(cell.style) {
                    let Style { fg, bg, bold } = cell.style;
                    let bold = if bold { ";1" } else { "" };
                    let _ = write!(Bytes(out), "\x1b[0{bold};{};{}m", fg.sgr(), bg.sgr() + 10);
                    current = Some(cell.style);
                }
                let mut utf8 = [0; 4];
                out.extend_from_slice(cell.character.encode_utf8(&mut utf8).as_bytes());
            }
            out.extend_from_slice(b"\x1b[0m");
        }
        if out.len() != start {
            out.extend_from_slice(b"\x1b[?2026l");
        }
        self.shown.clone_from(&self.cells);
    }
}

/// `write!` into a byte buffer.
struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c0_controls_become_caret_letters() {
        assert_eq!(escape(b"\0\x01\t\n\r\x1b\x1f"), "^@^A^I^J^M^[^_");
    }

    #[test]
    fn del_becomes_caret_question_mark() {
        assert_eq!(escape(b"a\x7fb"), "a^?b");
    }

    #[test]
    fn c1_controls_become_code_points() {
        assert_eq!(
            escape("\u{80}\u{85}\u{9b}".as_bytes()),
            "<U+0080><U+0085><U+009B>"
        );
    }

    #[test]
    fn invalid_utf8_becomes_hex_bytes() {
        assert_eq!(escape(b"\xff\xc3a\xe2\x82"), "\\xFF\\xC3a\\xE2\\x82");
    }

    #[test]
    fn printable_text_is_unchanged() {
        assert_eq!(escape("plain ✕ ◆ é 漢".as_bytes()), "plain ✕ ◆ é 漢");
    }

    const BIDI: [u32; 12] = [
        0x61c, 0x200e, 0x200f, 0x202a, 0x202b, 0x202c, 0x202d, 0x202e, 0x2066, 0x2067, 0x2068,
        0x2069,
    ];

    #[test]
    fn bidi_controls_become_code_points() {
        for point in BIDI {
            let text = char::from_u32(point).unwrap().to_string();
            assert_eq!(escape(text.as_bytes()), format!("<U+{point:04X}>"));
        }
    }

    /// Every C0, DEL, C1 and bidi control, put into a screen, comes out of the writer only as
    /// the writer's own CSI sequences around printable text.
    #[test]
    fn no_control_or_bidi_character_reaches_the_writer() {
        let hostile: String = (0..0x20)
            .chain(0x7f..0xa0)
            .chain(BIDI)
            .filter_map(char::from_u32)
            .collect();
        let mut screen = Screen::new(1, 400);
        screen.put(0, 0, &hostile, Style::default());
        let mut out = Vec::new();
        screen.frame(&mut out);
        let out = String::from_utf8(out).unwrap();
        let mut rest = out.as_str();
        loop {
            let at = rest.find('\x1b').unwrap_or(rest.len());
            let unsafe_character = |c: char| c.is_control() || BIDI.contains(&u32::from(c));
            assert!(!rest[..at].contains(unsafe_character), "{:?}", &rest[..at]);
            let Some(sequence) = rest[at..].strip_prefix("\x1b[") else {
                break;
            };
            let end = sequence.find(|c: char| c.is_ascii_alphabetic()).unwrap();
            let parameters = &sequence[..end];
            assert!(
                parameters
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b"?;".contains(&b))
            );
            rest = &sequence[end + 1..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn an_unchanged_frame_writes_nothing_and_a_change_writes_only_its_row() {
        let mut screen = Screen::new(3, 8);
        screen.put(0, 0, "one", Style::default());
        let mut out = Vec::new();
        screen.frame(&mut out);
        let first = String::from_utf8(out.clone()).unwrap();
        assert!(
            ["\x1b[1;1H", "\x1b[2;1H", "\x1b[3;1H"]
                .iter()
                .all(|at| first.contains(at))
        );
        out.clear();
        screen.frame(&mut out);
        assert!(out.is_empty());
        let red = Style {
            fg: Color::Red,
            ..Style::default()
        };
        screen.put(1, 2, "✕", red);
        screen.frame(&mut out);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out,
            "\x1b[?2026h\x1b[2;1H\x1b[0;39;49m  \x1b[0;31;49m✕\x1b[0;39;49m     \x1b[0m\x1b[?2026l"
        );
    }

    #[test]
    fn put_cuts_at_the_edge_and_ignores_rows_past_the_end() {
        let mut screen = Screen::new(1, 4);
        assert_eq!(screen.put(0, 2, "abc", Style::default()), 4);
        assert_eq!(screen.row(0), "  ab");
        assert_eq!(screen.put(1, 0, "x", Style::default()), 0);
    }
}
