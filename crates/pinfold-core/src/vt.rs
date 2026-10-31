//! The terminal engine behind a trait, with `alacritty_terminal` as its implementation.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

/// Larger sizes are clamped, so a bad resize cannot allocate gigabytes of cells.
pub const MAX_DIMENSION: u16 = 1024;

/// A terminal state machine that turns program output into a grid.
///
/// The trait grows with its callers. The design also lists scrolling, search and damage,
/// and ADR-0006 expects the scrollback work to shape them.
pub trait VtEngine {
    /// Parses program output. Bytes the program asked the terminal to answer with are
    /// appended to `replies`, and the caller writes them back to the PTY.
    fn feed(&mut self, bytes: &[u8], replies: &mut Vec<u8>);

    /// Changes the grid size. Each dimension is clamped to `1..=MAX_DIMENSION`.
    fn resize(&mut self, rows: u16, columns: u16);

    /// Returns the size as `(rows, columns)`.
    fn size(&self) -> (u16, u16);

    /// Returns a visible line without trailing spaces, or nothing if `line` is off screen.
    fn line_text(&self, line: u16) -> String;
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
            columns: columns.clamp(1, MAX_DIMENSION),
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
}

impl AlacrittyEngine {
    /// Creates a blank terminal. It keeps no scrollback until #24 adds the tiered store.
    #[must_use]
    pub fn new(rows: u16, columns: u16) -> Self {
        let replies = Replies::default();
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &Size::clamped(rows, columns), replies.clone()),
            parser: Processor::new(),
            replies,
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
        assert_eq!(engine.size(), (1, 1));
        engine.resize(u16::MAX, u16::MAX);
        assert_eq!(engine.size(), (MAX_DIMENSION, MAX_DIMENSION));
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
