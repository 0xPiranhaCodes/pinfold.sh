//! History at a new width must read like output written at that width.

use alacritty_terminal::term::cell::{Cell, Flags};
use pinfold_core::scrollback::BYTE_CAP;
use pinfold_core::search::{Direction, Position, Search, SearchStep, Selection};
use pinfold_core::vt::{AlacrittyEngine, MIN_COLUMNS, VtEngine};
use pinfold_proto::Frame;
use proptest::prelude::*;

const ROWS: u16 = 4;

fn feed(engine: &mut AlacrittyEngine, bytes: &[u8]) {
    engine.feed(bytes, &mut Vec::new());
}

/// Every history row after a read has brought the layout up to date, trailing blanks dropped.
fn history(engine: &AlacrittyEngine) -> Vec<Vec<Cell>> {
    let mut rows = engine.scrollback().lock().map_or(Vec::new(), |mut store| {
        store.sync();
        store.rows(0, usize::MAX)
    });
    while rows.last().is_some_and(Vec::is_empty) {
        rows.pop();
    }
    rows
}

fn text(row: &[Cell]) -> String {
    row.iter()
        .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
        .map(|cell| cell.c)
        .collect()
}

/// A cell's spacer takes no style from the text around it on a resize, as in the grid.
fn plain(row: &[Cell]) -> Vec<Cell> {
    let mut row = row.to_vec();
    for cell in &mut row {
        if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            let flags = cell.flags;
            *cell = Cell::default();
            cell.flags = flags;
        }
    }
    row
}

fn screen(engine: &AlacrittyEngine) -> Vec<String> {
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    frame
        .cells
        .chunks(usize::from(frame.columns))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.character).collect();
            text.trim_end().to_owned()
        })
        .collect()
}

fn pane(columns: u16, output: &str) -> AlacrittyEngine {
    let mut engine = AlacrittyEngine::new(ROWS, columns);
    feed(&mut engine, output.as_bytes());
    engine
}

/// Output whose every line has left the screen, so no line crosses from history to the grid.
fn flushed(output: &str) -> String {
    format!("{output}{}", "\r\n".repeat(usize::from(ROWS) + 1))
}

#[test]
fn a_wrapped_line_reads_at_a_wider_width() {
    let output = flushed(&format!("{}\r\nshort\r\n", "ab".repeat(15)));
    let mut engine = pane(10, &output);
    assert_eq!(history(&engine).len(), 4);
    engine.resize(ROWS, 20);
    let rows = history(&engine);
    let rows: Vec<String> = rows.iter().map(|row| text(row)).collect();
    assert_eq!(
        rows[..3],
        ["ab".repeat(10), "ab".repeat(5), "short".to_owned()]
    );
}

#[test]
fn a_view_in_history_stays_on_its_text() {
    let mut output = String::new();
    for line in 0..600 {
        output += &format!("line {line:04} {}\r\n", "x".repeat(40));
    }
    let mut engine = pane(30, &output);
    engine.scroll(600);
    while !screen(&engine)[0].starts_with("line ") {
        engine.scroll(1);
    }
    let top = screen(&engine)[0].clone();
    for columns in [50, 11, 7, 80, MIN_COLUMNS, 30] {
        engine.resize(ROWS, columns);
        let joined = screen(&engine).concat();
        let wanted: String = top.chars().take(4).collect();
        assert!(joined.starts_with(&wanted), "{columns}: {joined}");
    }
    assert_eq!(screen(&engine)[0], top);
}

#[test]
fn the_wheel_moves_in_rows_of_the_new_width() {
    let output = flushed(&format!("{}\r\n", "abcd".repeat(40)));
    let mut engine = pane(8, &output);
    engine.resize(ROWS, 4);
    assert!(engine.scroll(i32::MAX));
    assert_eq!(screen(&engine), ["abcd"; 4]);
    let top = engine.position(0, 0).row;
    assert!(engine.scroll(-3));
    assert_eq!(engine.position(0, 0).row, top + 3);
    let held = engine.view_rows();
    assert_eq!(held.start, top, "the oldest row can be reached");
}

#[test]
fn numbers_read_back_what_they_name_at_the_new_width() {
    let output = flushed(&format!("{}NEEDLE{}\r\n", "x".repeat(25), "y".repeat(25)));
    let mut engine = pane(10, &output);
    engine.resize(ROWS, 7);
    let mut search = Search::new("needle", Position::default(), Direction::Forward)
        .expect("the query is usable");
    let found = loop {
        match engine.search(&mut search) {
            SearchStep::Found(found) => break found,
            SearchStep::Running => {}
            SearchStep::NotFound => panic!("not found"),
        }
    };
    assert_eq!(found.anchor.column, 4, "25 cells in at seven a row");
    assert_eq!(found.focus.row, found.anchor.row + 1, "it crosses a wrap");
    let mut copied = String::new();
    assert!(engine.selection_text(found, &mut copied));
    assert_eq!(copied, "NEEDLE");
    let start = Position {
        row: found.anchor.row - 3,
        column: 0,
    };
    let whole = Selection {
        anchor: start,
        focus: Position {
            row: found.focus.row + 4,
            column: u16::MAX,
        },
    };
    copied.clear();
    assert!(engine.selection_text(whole, &mut copied));
    assert!(copied.contains(&format!("{}NEEDLE{}", "x".repeat(25), "y".repeat(25))));
}

#[test]
fn a_line_crossing_into_the_screen_still_copies_as_one() {
    let mut engine = pane(10, &"ab".repeat(15));
    assert_eq!(history(&engine).len(), 0);
    feed(&mut engine, &"cd".repeat(10).into_bytes());
    assert_eq!(history(&engine).len(), 1);
    engine.resize(ROWS, 15);
    let end = engine.scrollback().lock().unwrap().end();
    let selection = Selection {
        anchor: Position { row: 0, column: 0 },
        focus: Position {
            row: end + 2,
            column: u16::MAX,
        },
    };
    let mut copied = String::new();
    engine.selection_text(selection, &mut copied);
    assert_eq!(copied.trim_end(), "ab".repeat(15) + &"cd".repeat(10));
}

#[test]
fn the_narrowest_width_holds_wide_characters_and_a_drag_does_not_evict() {
    let output = flushed(&format!("{}\r\n", "漢字".repeat(100)));
    let mut engine = pane(40, &output);
    let before = history(&engine);
    for columns in (MIN_COLUMNS..40).rev().chain(MIN_COLUMNS..=40) {
        engine.resize(ROWS, columns);
        let store = engine.scrollback().lock().unwrap();
        assert!(store.bytes() <= BYTE_CAP);
    }
    assert_eq!(history(&engine), before);
    engine.resize(ROWS, 1);
    let rows = history(&engine);
    assert_eq!(rows.len(), 200, "one wide character a row");
}

fn fragment() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "abc",
        "wxyz0123456789",
        "a much longer line that wraps past the width",
        "漢字",
        "一二三四五六七八九十",
        "e\u{301}x",
        "\x1b[31m",
        "\x1b[0m",
        "\x1b[1;4m",
        // No background: a line feed fills a row with it to the width of the moment.
        "\x1b[38;2;9;8;7m",
        "\x1b]8;id=a;https://example.com\x1b\\",
        "\x1b]8;;\x1b\\",
        "\x1b[0m\r\n",
        "\x1b[0m\r\n\r\n",
    ])
}

fn output() -> impl Strategy<Value = String> {
    prop::collection::vec(fragment(), 0..200).prop_map(|parts| {
        let line = parts.concat();
        flushed(&format!("{line}\x1b[0m\x1b]8;;\x1b\\"))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn reflowed_history_matches_a_fresh_render(
        output in output(),
        written in 2..50_u16,
        target in 2..50_u16,
    ) {
        let mut engine = pane(written, &output);
        engine.resize(ROWS, target);
        let fresh = pane(target, &output);
        let (rows, fresh) = (history(&engine), history(&fresh));
        prop_assert_eq!(rows.len(), fresh.len());
        for (index, (row, fresh)) in rows.iter().zip(&fresh).enumerate() {
            prop_assert_eq!(plain(row), plain(fresh), "row {}", index);
        }
    }

    #[test]
    fn any_walk_of_widths_comes_back_to_the_rows_it_left(
        output in output(),
        written in 2..50_u16,
        widths in prop::collection::vec(1..200_u16, 0..8),
    ) {
        let mut engine = pane(written, &output);
        let before = history(&engine);
        for columns in widths {
            engine.resize(ROWS, columns);
            let fresh = pane(engine.size().1, &output);
            let rows: Vec<_> = history(&engine).iter().map(|row| plain(row)).collect();
            let fresh: Vec<_> = history(&fresh).iter().map(|row| plain(row)).collect();
            prop_assert_eq!(rows, fresh, "at {} columns", columns);
        }
        engine.resize(ROWS, written);
        prop_assert_eq!(history(&engine), before);
    }

    #[test]
    fn no_output_and_resizes_break_a_pane(
        steps in prop::collection::vec(
            (output(), 0..1100_u16, 0..40_u16, any::<i16>()),
            1..6,
        ),
    ) {
        let mut engine = pane(10, "");
        let mut frame = Frame::default();
        for (output, columns, rows, lines) in steps {
            feed(&mut engine, output.as_bytes());
            engine.resize(rows, columns);
            engine.scroll(i32::from(lines));
            engine.frame(&mut frame);
            let (rows, columns) = engine.size();
            prop_assert_eq!(frame.cells.len(), usize::from(rows) * usize::from(columns));
            let held = engine.view_rows();
            prop_assert!(held.start <= held.end);
            let position = engine.position(0, 0);
            prop_assert!(position.row <= held.end);
            let everything = Selection {
                anchor: Position::default(),
                focus: Position { row: usize::MAX, column: u16::MAX },
            };
            engine.selection_text(everything, &mut String::new());
            feed(&mut engine, b"\x1b[?1049h");
            engine.resize(rows.wrapping_add(1), columns.wrapping_add(1));
            feed(&mut engine, b"\x1b[?1049l");
        }
    }
}

#[test]
fn a_synced_store_reads_every_row_it_counts() {
    // Found by the `vt_input` fuzz target: base64 LwD1ZczMCg==.
    let mut engine = AlacrittyEngine::new(47, 0);
    let (mut replies, mut frame) = (Vec::new(), Frame::default());
    engine.feed(b"", &mut replies);
    engine.frame(&mut frame);
    engine.resize(0, 1);
    engine.feed(b"e\xcc\xcc\n", &mut replies);
    engine.frame(&mut frame);
    engine.resize(4, 101);
    let mut store = engine.scrollback().lock().unwrap();
    store.sync();
    assert_eq!(store.rows(0, store.len()).len(), store.len());
}
