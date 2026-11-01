//! The terminal engine behind a trait, with `alacritty_terminal` as its implementation.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell as GridCell, Flags};
use alacritty_terminal::term::color;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};
use pinfold_proto::{Cell, Frame};

use crate::scrollback::Scrollback;

/// The slot Alacritty's colour table gives the default background.
const BACKGROUND_SLOT: usize = 257;

/// Larger sizes are clamped, so a bad resize cannot allocate gigabytes of cells.
pub const MAX_DIMENSION: u16 = 1024;
/// A wide character cannot fit in fewer columns: reflowing one wraps forever.
pub const MIN_COLUMNS: u16 = 2;

/// A terminal state machine that turns program output into a grid.
///
/// The trait grows with its callers. The design also lists scrolling, search and damage,
/// and ADR-0006 expects the scrollback work to shape them.
pub trait VtEngine {
    /// Parses program output. Bytes the program asked the terminal to answer with are
    /// appended to `replies`, and the caller writes them back to the PTY.
    fn feed(&mut self, bytes: &[u8], replies: &mut Vec<u8>);

    /// Changes the grid size, clamped to at least one row and `MIN_COLUMNS` columns and at
    /// most `MAX_DIMENSION` of each.
    fn resize(&mut self, rows: u16, columns: u16);

    /// Returns the size as `(rows, columns)`.
    fn size(&self) -> (u16, u16);

    /// Returns a visible line without trailing spaces, or nothing if `line` is off screen.
    fn line_text(&self, line: u16) -> String;

    /// Copies the visible screen into `frame`, reusing its allocation.
    fn frame(&self, frame: &mut Frame);
}

/// The "Graphite Grid" ground and text colours (`docs/design/ui/`).
const BACKGROUND: u32 = 0x000e_1014;
const FOREGROUND: u32 = 0x00e2_e5ea;

// shortcut: the design has no ANSI palette, so this reuses its tokens, and magenta is a stand-in.
const ANSI: [u32; 16] = [
    0x0012_151a,
    0x00ff_8a8a,
    0x0063_d29a,
    0x00f5_b544,
    0x007a_a7ff,
    0x00d9_a7e0,
    0x009c_c0ff,
    0x00c9_cdd4,
    0x0034_3a44,
    0x00ff_8a8a,
    0x0063_d29a,
    0x00f5_b544,
    0x009c_c0ff,
    0x00d9_a7e0,
    0x00c4_d9ff,
    0x00e2_e5ea,
];

/// The standard 256-colour table: 16 ANSI colours, a 6×6×6 cube and a grey ramp.
fn indexed(index: usize) -> u32 {
    let level = |step: usize| if step == 0 { 0 } else { 55 + 40 * step as u32 };
    match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let offset = index - 16;
            level(offset / 36) << 16 | level(offset / 6 % 6) << 8 | level(offset % 6)
        }
        232..=255 => (8 + 10 * (index as u32 - 232)) * 0x0001_0101,
        _ => FOREGROUND,
    }
}

#[derive(Clone, Copy)]
struct Size {
    rows: u16,
    columns: u16,
}

impl Size {
    fn clamped(rows: u16, columns: u16) -> Self {
        Self {
            rows: rows.clamp(1, MAX_DIMENSION),
            columns: columns.clamp(MIN_COLUMNS, MAX_DIMENSION),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

/// Collects what the terminal wants to send back to the program.
///
/// Alacritty's other events are dropped for now. They include clipboard writes, which
/// must stay off until the escape-sequence policy in #27 decides them.
#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && let Ok(mut pending) = self.0.lock()
        {
            pending.extend_from_slice(text.as_bytes());
        }
    }
}

/// [`VtEngine`] over `alacritty_terminal`.
pub struct AlacrittyEngine {
    term: Term<Replies>,
    parser: Processor,
    replies: Replies,
    scrollback: Arc<Mutex<Scrollback>>,
}

impl AlacrittyEngine {
    /// Creates a blank terminal. Rows leaving the primary screen go to its scrollback store.
    #[must_use]
    pub fn new(rows: u16, columns: u16) -> Self {
        let replies = Replies::default();
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let mut term = Term::new(config, &Size::clamped(rows, columns), replies.clone());
        let scrollback = Arc::new(Mutex::new(Scrollback::new()));
        term.grid_mut().set_history_sink(scrollback.clone());
        Self {
            term,
            parser: Processor::new(),
            replies,
            scrollback,
        }
    }

    /// Returns the store of rows that have left the screen.
    #[must_use]
    pub fn scrollback(&self) -> &Mutex<Scrollback> {
        &self.scrollback
    }
}

impl AlacrittyEngine {
    fn colour(&self, colour: Color) -> u32 {
        let index = match colour {
            Color::Spec(rgb) => {
                return u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b);
            }
            Color::Indexed(index) => usize::from(index),
            Color::Named(name) => name as usize,
        };
        match self.term.colors()[index.min(color::COUNT - 1)] {
            Some(rgb) => u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b),
            None if index == BACKGROUND_SLOT => BACKGROUND,
            None => indexed(index),
        }
    }

    fn cell(&self, cell: &GridCell) -> Cell {
        let (mut foreground, mut background) = (self.colour(cell.fg), self.colour(cell.bg));
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            foreground = background;
        }
        let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
        Cell {
            character: if spacer { ' ' } else { cell.c },
            foreground,
            background,
        }
    }
}

impl VtEngine for AlacrittyEngine {
    fn feed(&mut self, bytes: &[u8], replies: &mut Vec<u8>) {
        self.parser.advance(&mut self.term, bytes);
        if let Ok(mut pending) = self.replies.0.lock() {
            replies.append(&mut pending);
        }
    }

    fn resize(&mut self, rows: u16, columns: u16) {
        self.term.resize(Size::clamped(rows, columns));
    }

    fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        let rows = u16::try_from(grid.screen_lines()).unwrap_or(MAX_DIMENSION);
        let columns = u16::try_from(grid.columns()).unwrap_or(MAX_DIMENSION);
        (rows, columns)
    }

    fn line_text(&self, line: u16) -> String {
        let grid = self.term.grid();
        if usize::from(line) >= grid.screen_lines() {
            return String::new();
        }
        let row = &grid[Line(i32::from(line))];
        let text: String = (0..grid.columns())
            .map(|column| &row[Column(column)])
            .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect();
        text.trim_end().to_owned()
    }

    fn frame(&self, frame: &mut Frame) {
        let grid = self.term.grid();
        (frame.rows, frame.columns) = self.size();
        frame.cells.clear();
        for line in 0..frame.rows {
            let row = &grid[Line(i32::from(line))];
            frame
                .cells
                .extend((0..grid.columns()).map(|column| self.cell(&row[Column(column)])));
        }
        let point = grid.cursor.point;
        let visible = self.term.mode().contains(TermMode::SHOW_CURSOR);
        let position = u16::try_from(point.line.0)
            .ok()
            .zip(u16::try_from(point.column.0).ok());
        frame.cursor = position.filter(|_| visible);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(engine: &mut AlacrittyEngine, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        engine.feed(bytes, &mut replies);
        replies
    }

    #[test]
    fn the_engine_can_move_to_the_io_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<AlacrittyEngine>();
    }

    #[test]
    fn text_reaches_the_grid() {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, b"hello\r\nworld");
        assert_eq!(engine.line_text(0), "hello");
        assert_eq!(engine.line_text(1), "world");
        assert_eq!(engine.line_text(2), "");
    }

    #[test]
    fn wide_characters_appear_once() {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, "漢字!".as_bytes());
        assert_eq!(engine.line_text(0), "漢字!");
    }

    #[test]
    fn utf8_split_across_reads_survives() {
        let mut engine = AlacrittyEngine::new(4, 20);
        let bytes = "é".as_bytes();
        feed(&mut engine, &bytes[..1]);
        feed(&mut engine, &bytes[1..]);
        assert_eq!(engine.line_text(0), "é");
    }

    #[test]
    fn cursor_position_requests_get_a_reply() {
        let mut engine = AlacrittyEngine::new(4, 20);
        assert_eq!(feed(&mut engine, b"ab\x1b[6n"), b"\x1b[1;3R");
        assert!(feed(&mut engine, b"more").is_empty());
    }

    #[test]
    fn clipboard_writes_are_ignored() {
        let mut engine = AlacrittyEngine::new(4, 20);
        assert!(feed(&mut engine, b"\x1b]52;c;aGVsbG8=\x07").is_empty());
    }

    #[test]
    fn resize_changes_the_size() {
        let mut engine = AlacrittyEngine::new(4, 20);
        engine.resize(10, 40);
        assert_eq!(engine.size(), (10, 40));
    }

    #[test]
    fn sizes_are_clamped() {
        let mut engine = AlacrittyEngine::new(0, 0);
        assert_eq!(engine.size(), (1, MIN_COLUMNS));
        engine.resize(u16::MAX, u16::MAX);
        assert_eq!(engine.size(), (MAX_DIMENSION, MAX_DIMENSION));
    }

    #[test]
    fn a_wide_character_survives_the_narrowest_window() {
        let mut engine = AlacrittyEngine::new(8, 249);
        feed(&mut engine, "䤬".as_bytes());
        engine.resize(4, 1);
        assert_eq!(engine.size(), (4, MIN_COLUMNS));
        assert_eq!(engine.line_text(0), "䤬");
    }

    fn screen(bytes: &[u8]) -> Frame {
        let (mut engine, mut frame) = (AlacrittyEngine::new(4, 20), Frame::default());
        feed(&mut engine, bytes);
        engine.frame(&mut frame);
        frame
    }

    #[test]
    fn a_frame_holds_every_cell() {
        let frame = screen(b"hi");
        assert_eq!((frame.rows, frame.columns), (4, 20));
        assert_eq!(frame.cells.len(), 80);
        assert_eq!(frame.cells[1].character, 'i');
        assert_eq!(frame.cells[0].foreground, FOREGROUND);
        assert_eq!(frame.cells[0].background, BACKGROUND);
    }

    #[test]
    fn colours_resolve() {
        assert_eq!(screen(b"\x1b[31mx").cells[0].foreground, ANSI[1]);
        assert_eq!(screen(b"\x1b[38;5;196mx").cells[0].foreground, 0x00ff_0000);
        assert_eq!(screen(b"\x1b[38;5;232mx").cells[0].foreground, 0x0008_0808);
        assert_eq!(
            screen(b"\x1b[48;2;1;2;3mx").cells[0].background,
            0x0001_0203
        );
    }

    #[test]
    fn programs_can_change_the_palette() {
        assert_eq!(
            screen(b"\x1b]4;1;rgb:01/02/03\x07\x1b[31mx").cells[0].foreground,
            0x0001_0203
        );
    }

    #[test]
    fn inverse_and_hidden_text_apply() {
        let inverse = screen(b"\x1b[7mx").cells[0];
        assert_eq!(
            (inverse.foreground, inverse.background),
            (BACKGROUND, FOREGROUND)
        );
        let hidden = screen(b"\x1b[8mx").cells[0];
        assert_eq!(hidden.foreground, hidden.background);
    }

    #[test]
    fn wide_spacers_are_blank() {
        let frame = screen("漢".as_bytes());
        assert_eq!(frame.cells[0].character, '漢');
        assert_eq!(frame.cells[1].character, ' ');
    }

    #[test]
    fn the_cursor_follows_the_program() {
        assert_eq!(screen(b"ab\r\nc").cursor, Some((1, 1)));
        assert_eq!(screen(b"\x1b[?25l").cursor, None);
    }

    #[test]
    fn every_colour_index_is_safe() {
        for index in 0..=300 {
            let _ = indexed(index);
        }
        for byte in 0..=255 {
            screen(format!("\x1b[38;5;{byte}m\x1b[48;5;{byte}mx").as_bytes());
        }
    }

    fn history(engine: &AlacrittyEngine) -> Vec<String> {
        let store = engine.scrollback().lock().unwrap();
        store
            .rows(0, store.len())
            .iter()
            .map(|row| row.iter().map(|cell| cell.c).collect::<String>())
            .map(|text| text.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn rows_leaving_the_screen_reach_the_store() {
        let mut engine = AlacrittyEngine::new(2, 20);
        feed(&mut engine, b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(history(&engine), ["one", "two"]);
        assert_eq!(engine.line_text(0), "three");
    }

    #[test]
    fn the_alternate_screen_stays_out_of_the_store() {
        let mut engine = AlacrittyEngine::new(2, 20);
        feed(&mut engine, b"\x1b[?1049hone\r\ntwo\r\nthree");
        assert!(history(&engine).is_empty());
    }

    #[test]
    fn clearing_scrollback_empties_the_store() {
        let mut engine = AlacrittyEngine::new(2, 20);
        feed(&mut engine, b"one\r\ntwo\r\nthree\x1b[3J");
        assert!(history(&engine).is_empty());
    }

    #[test]
    fn hostile_output_through_a_pane_stays_under_the_cap() {
        let mut engine = AlacrittyEngine::new(2, 120);
        for line in 0..20_000_u32 {
            let colour = line.wrapping_mul(2_654_435_761);
            let [r, g, b, _] = colour.to_le_bytes();
            let text = format!("\x1b[38;2;{r};{g};{b}m{colour:0>119}\r\n");
            feed(&mut engine, text.as_bytes());
            let bytes = engine.scrollback().lock().unwrap().bytes();
            assert!(bytes <= crate::scrollback::BYTE_CAP);
        }
    }

    #[test]
    fn off_screen_lines_are_empty() {
        assert_eq!(AlacrittyEngine::new(4, 20).line_text(u16::MAX), "");
    }

    #[test]
    fn every_byte_value_is_survivable() {
        let mut engine = AlacrittyEngine::new(4, 20);
        for _ in 0..3 {
            for byte in 0..=u8::MAX {
                feed(&mut engine, &[byte, b'[', byte, b'm']);
            }
        }
        feed(&mut engine, b"\x1bc");
        feed(&mut engine, b"ok");
        assert_eq!(engine.line_text(0), "ok");
    }
}
