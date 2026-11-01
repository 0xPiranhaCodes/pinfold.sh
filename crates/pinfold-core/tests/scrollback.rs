//! Scrollback must hold what a stock grid holds, cell for cell.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use pinfold_core::scrollback::{LINE_CAP, Scrollback};
use proptest::prelude::*;

const LINES: usize = 6;
const COLUMNS: usize = 20;

struct Size;

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        LINES
    }

    fn screen_lines(&self) -> usize {
        LINES
    }

    fn columns(&self) -> usize {
        COLUMNS
    }
}

fn term(history: usize) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: history,
        ..Config::default()
    };
    Term::new(config, &Size, VoidListener)
}

/// A stock grid's history, oldest first, trimmed the way scrollback trims.
fn stock_history(term: &Term<VoidListener>) -> Vec<Vec<Cell>> {
    let default = Cell::default();
    (1..=term.grid().history_size())
        .rev()
        .map(|back| {
            let row = &term.grid()[Line(-(back as i32))];
            let mut cells: Vec<Cell> = (0..COLUMNS).map(|c| row[Column(c)].clone()).collect();
            while cells.last() == Some(&default) {
                cells.pop();
            }
            cells
        })
        .collect()
}

fn assert_matches_stock(bytes: &[u8]) {
    // `grid()` is the active grid, and history is on the primary one.
    let bytes = [bytes, b"\x1b[?1049l"].concat();
    let bytes = bytes.as_slice();
    let mut stock = term(LINE_CAP * 2);
    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut stock, bytes);

    let scrollback = Arc::new(Mutex::new(Scrollback::new()));
    let mut tiered = term(0);
    tiered.grid_mut().set_history_sink(scrollback.clone());
    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut tiered, bytes);

    let expected = stock_history(&stock);
    let actual = scrollback
        .lock()
        .map(|rows| rows.rows(0, usize::MAX))
        .unwrap_or_default();
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(&expected).enumerate() {
        assert_eq!(actual, expected, "row {index}");
    }
}

#[test]
fn styled_output_matches_stock() {
    let mut bytes = Vec::new();
    for line in 0..400 {
        let text = format!(
            "\x1b[1;{}m{line}\x1b[0m \x1b[38;2;1;{};3mrgb\x1b[0m \x1b[4:3m\x1b[58;5;{}mline\x1b[0m\r\n",
            31 + line % 6,
            line % 256,
            line % 256,
        );
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(
            "漢字 e\u{301} \x1b]8;id=a;https://example.com\x1b\\link\x1b]8;;\x1b\\\r\n".as_bytes(),
        );
    }
    assert_matches_stock(&bytes);
}

/// Fragments that scroll, style and move the cursor, so random mixes reach deep states.
const FRAGMENTS: &[&str] = &[
    "\r\n",
    "\n",
    "text",
    "a much longer line that wraps past the width",
    "漢字",
    "e\u{301}",
    "\x1b[31m",
    "\x1b[0m",
    "\x1b[1;4;7m",
    "\x1b[48;2;9;8;7m",
    "\x1b[58;5;3m",
    // With an id, because links without one get a process-wide counter from the crate.
    "\x1b]8;id=a;https://example.com\x1b\\",
    "\x1b]8;;\x1b\\",
    "\x1b[2S",
    "\x1b[T",
    "\x1b[2;4r",
    "\x1b[r",
    "\x1b[H",
    "\x1b[3;3H",
    "\x1b[K",
    "\x1b[2J",
    "\x1b[1J",
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[3J",
    "\x1bc",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn random_output_matches_stock(picks in proptest::collection::vec(0..FRAGMENTS.len(), 0..600)) {
        let bytes: String = picks.iter().map(|&pick| FRAGMENTS[pick]).collect();
        assert_matches_stock(bytes.as_bytes());
    }
}
