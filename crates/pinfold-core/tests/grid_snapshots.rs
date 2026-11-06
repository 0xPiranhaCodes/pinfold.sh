//! The grid the engine shows after representative output, as text and colour runs.

use std::fmt::Write;

use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use pinfold_proto::{BOLD, Cell, Frame, ITALIC};

/// The screen after `bytes` on a fresh 4 × 10 engine: each row between bars, then each run of
/// cells whose colours or style differ from a blank cell's.
fn grid(bytes: &[u8]) -> String {
    let mut engine = AlacrittyEngine::new(4, 10);
    let mut blank = Frame::default();
    engine.frame(&mut blank);
    let mut replies = Vec::new();
    engine.feed(bytes, &mut replies);
    assert!(replies.is_empty(), "{replies:?}");
    let mut frame = Frame::default();
    engine.frame(&mut frame);

    let mut shown = format!("cursor {:?}\n", frame.cursor);
    let rows = frame.cells.chunks(usize::from(frame.columns));
    for row in rows.clone() {
        let text: String = row.iter().map(|cell| cell.character).collect();
        let _ = writeln!(shown, "|{text}|");
    }
    let plain = blank.cells[0];
    for (line, row) in rows.enumerate() {
        let mut start = 0;
        while start < row.len() {
            let same = |cell: &Cell| (cell.foreground, cell.background) == look(&row[start]);
            let end = start + row[start..].iter().take_while(|cell| same(cell)).count();
            if look(&row[start]) != look(&plain) {
                let _ = writeln!(shown, "{line}:{start}..{end} {}", describe(row[start]));
            }
            start = end;
        }
    }
    shown
}

fn look(cell: &Cell) -> (u32, u32) {
    (cell.foreground, cell.background)
}

fn describe(cell: Cell) -> String {
    let style = cell.style();
    let colour = cell.foreground & 0x00ff_ffff;
    let mut text = format!("fg {colour:06x} bg {:06x}", cell.background);
    for (bit, name) in [(BOLD, "bold"), (ITALIC, "italic")] {
        if style & bit != 0 {
            text += &format!(" {name}");
        }
    }
    text
}

#[test]
fn plain_text() {
    insta::assert_snapshot!(grid(b"hello\r\nworld"));
}

#[test]
fn a_long_line_wraps_and_a_full_screen_scrolls() {
    insta::assert_snapshot!("wraps", grid(b"0123456789abcdef\r\n1\r\n2"));
    insta::assert_snapshot!("scrolls", grid(b"0123456789abcdef\r\n1\r\n2\r\n3"));
}

#[test]
fn a_scroll_region_scrolls_only_its_rows() {
    // Rows 2 and 3 are the region; the line feeds at its foot scroll only them.
    insta::assert_snapshot!(grid(b"top\r\na\r\nb\r\nfoot\x1b[2;3r\x1b[3;1Hc\r\nd\r\ne"));
}

#[test]
fn the_alternate_screen_hides_and_restores_the_main_one() {
    let main = b"main".as_slice();
    let alternate = [main, b"\x1b[?1049h\x1b[2;3Halt"].concat();
    let back = [&alternate[..], b"\x1b[?1049l"].concat();
    insta::assert_snapshot!("alternate", grid(&alternate));
    insta::assert_snapshot!("restored", grid(&back));
}

#[test]
fn sgr_attributes_become_colours_and_style() {
    let sgr = b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[31mr\x1b[0m\x1b[42mg\x1b[0m\
                \x1b[7mv\x1b[0m\x1b[38;2;1;2;3mt\x1b[0m\x1b[38;5;196mx\x1b[0m\x1b[8mh\x1b[0m.";
    insta::assert_snapshot!(grid(sgr));
}

#[test]
fn wide_characters_take_two_cells_and_wrap_whole() {
    insta::assert_snapshot!(grid("a中b\u{1f600}\r\n123456789中".as_bytes()));
}
