//! Draws text an input method is still composing, at the cursor.
//!
//! The text is not the program's yet, so it goes over a copy of the screen.

use pinfold_proto::Frame;
use unicode_width::UnicodeWidthChar;

/// Draws `text` in inverse from the cursor, and moves the cursor to byte `caret` of `text`.
/// Text past the end of the row is cut off, and a hidden cursor draws nothing.
pub fn overlay(frame: &mut Frame, text: &str, caret: usize) {
    let Some((row, mut column)) = frame.cursor else {
        return;
    };
    let columns = usize::from(frame.columns);
    let start = usize::from(row) * columns;
    let mut cursor = None;
    for (index, character) in text.char_indices() {
        if index == caret {
            cursor = Some(column);
        }
        // shortcut: combining marks are not drawn, because a cell holds one character.
        let width = character.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if usize::from(column) + width > columns {
            break;
        }
        for offset in 0..width {
            if let Some(cell) = frame.cells.get_mut(start + usize::from(column) + offset) {
                cell.invert();
                cell.character = if offset == 0 { character } else { ' ' };
            }
        }
        column += width as u16;
    }
    frame.cursor = Some((row, cursor.unwrap_or(column)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinfold_proto::Cell;

    fn frame() -> Frame {
        let cell = Cell {
            character: 'x',
            foreground: 1,
            background: 2,
        };
        Frame {
            rows: 2,
            columns: 4,
            cells: vec![cell; 8],
            cursor: Some((1, 1)),
            ..Frame::default()
        }
    }

    fn text(frame: &Frame) -> String {
        frame.cells.iter().map(|cell| cell.character).collect()
    }

    #[test]
    fn text_goes_at_the_cursor_in_inverse() {
        let mut frame = frame();
        overlay(&mut frame, "ab", 2);
        assert_eq!(text(&frame), "xxxxxabx");
        assert_eq!(frame.cells[5].foreground, 2);
        assert_eq!(frame.cells[7].foreground, 1);
        assert_eq!(frame.cursor, Some((1, 3)));
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut frame = frame();
        overlay(&mut frame, "漢", 3);
        assert_eq!(text(&frame), "xxxxx漢 x");
        assert_eq!(frame.cursor, Some((1, 3)));
    }

    #[test]
    fn the_caret_can_sit_inside_the_text() {
        let mut frame = frame();
        overlay(&mut frame, "漢a", 3);
        assert_eq!(frame.cursor, Some((1, 3)));
        overlay(&mut frame, "ab", 0);
        assert_eq!(frame.cursor, Some((1, 3)));
    }

    #[test]
    fn text_past_the_row_is_cut_off() {
        let mut frame = frame();
        overlay(&mut frame, "abcdef", 6);
        assert_eq!(text(&frame), "xxxxxabc");
        assert_eq!(frame.cursor, Some((1, 4)));
        let mut wide = self::frame();
        overlay(&mut wide, "ab漢", 5);
        assert_eq!(text(&wide), "xxxxxabx");
    }

    #[test]
    fn a_hidden_cursor_draws_nothing() {
        let mut frame = frame();
        frame.cursor = None;
        overlay(&mut frame, "ab", 2);
        assert_eq!(text(&frame), "xxxxxxxx");
        assert_eq!(frame.cursor, None);
    }

    #[test]
    fn combining_marks_and_empty_text_are_safe() {
        let mut frame = frame();
        overlay(&mut frame, "", 0);
        assert_eq!(text(&frame), "xxxxxxxx");
        overlay(&mut frame, "e\u{301}", 3);
        assert_eq!(text(&frame), "xxxxxexx");
    }
}
