//! The fork's contract: every row leaving the primary screen reaches the sink, and only those.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, HistorySink, Row};
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

#[derive(Clone, Copy)]
struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.0
    }

    fn screen_lines(&self) -> usize {
        self.0
    }

    fn columns(&self) -> usize {
        self.1
    }
}

#[derive(Debug, Default)]
struct Rows(Vec<String>);

impl HistorySink<Cell> for Rows {
    fn push(&mut self, row: &Row<Cell>) {
        let text: String = (0..row.len()).map(|column| row[Column(column)].c).collect();
        self.0.push(text.trim_end().to_owned());
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

struct Pane {
    term: Term<VoidListener>,
    parser: Processor,
    rows: Arc<Mutex<Rows>>,
}

impl Pane {
    fn new(lines: usize, columns: usize) -> Self {
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let mut term = Term::new(config, &Size(lines, columns), VoidListener);
        let rows = Arc::new(Mutex::new(Rows::default()));
        term.grid_mut().set_history_sink(rows.clone());
        Self {
            term,
            parser: Processor::new(),
            rows,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn history(&self) -> Vec<String> {
        self.rows
            .lock()
            .map(|rows| rows.0.clone())
            .unwrap_or_default()
    }
}

#[test]
fn rows_arrive_oldest_first() {
    let mut pane = Pane::new(2, 10);
    pane.feed(b"one\r\ntwo\r\nthree\r\nfour");
    assert_eq!(pane.history(), ["one", "two"]);
}

#[test]
fn scrolling_many_rows_at_once_keeps_them_in_order() {
    let mut pane = Pane::new(3, 10);
    pane.feed(b"one\r\ntwo\r\nthree\x1b[2S");
    assert_eq!(pane.history(), ["one", "two"]);
}

#[test]
fn a_scroll_region_below_the_top_keeps_rows_out_of_history() {
    let mut pane = Pane::new(4, 10);
    pane.feed(b"one\x1b[2;4r\x1b[4;1H\r\ntwo\r\nthree");
    assert!(pane.history().is_empty());
}

#[test]
fn the_alternate_screen_never_reaches_history() {
    let mut pane = Pane::new(2, 10);
    pane.feed(b"\x1b[?1049hone\r\ntwo\r\nthree\r\nfour\x1b[?1049l");
    assert!(pane.history().is_empty());
}

#[test]
fn clearing_history_reaches_the_sink() {
    let mut pane = Pane::new(2, 10);
    pane.feed(b"secret\r\ntwo\r\nthree");
    assert_eq!(pane.history(), ["secret"]);
    pane.feed(b"\x1b[3J");
    assert!(pane.history().is_empty());
}

#[test]
fn resetting_the_terminal_clears_history_from_the_alternate_screen_too() {
    let mut pane = Pane::new(2, 10);
    pane.feed(b"secret\r\ntwo\r\nthree\x1b[?1049h\x1bc");
    assert!(pane.history().is_empty());
}

#[test]
fn rows_pushed_off_by_a_shorter_window_are_kept() {
    let mut pane = Pane::new(3, 10);
    pane.feed(b"one\r\ntwo\r\nthree");
    pane.term.resize(Size(1, 10));
    assert_eq!(pane.history(), ["one", "two"]);
}

#[test]
fn a_taller_window_adds_nothing_to_history() {
    let mut pane = Pane::new(2, 10);
    pane.feed(b"one\r\ntwo\r\nthree");
    pane.term.resize(Size(5, 10));
    assert_eq!(pane.history(), ["one"]);
}

#[test]
fn rows_reflowed_off_a_narrower_window_are_kept() {
    let mut pane = Pane::new(2, 6);
    pane.feed(b"abcdef\r\nghijkl");
    pane.term.resize(Size(2, 3));
    assert_eq!(pane.history(), ["abc", "def"]);
}
