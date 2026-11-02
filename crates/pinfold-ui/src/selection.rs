//! What the user has selected, drawn by inverting cells of the frame.
//!
//! A selection names cells by the core's row numbers, so it stays on its text as output
//! scrolls it, and it can reach history the view does not show.

use std::ops::Range;

use pinfold_proto::link::{Position, Rect, Selection};
use pinfold_proto::{Frame, MouseMode};
use winit::keyboard::ModifiersState;

/// The current selection, and the drag making one.
#[derive(Default)]
pub struct Selecting {
    /// What is selected, by a drag or a search.
    pub current: Option<Selection>,
    /// Where the drag in progress started.
    anchor: Option<Position>,
}

impl Selecting {
    /// Starts a drag at `at`. A click alone clears the selection.
    pub fn press(&mut self, at: Position) {
        self.anchor = Some(at);
        self.current = None;
    }

    /// Moves a drag's end to `at`. Returns false if no drag is in progress.
    pub fn drag(&mut self, at: Position) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        self.current = Some(Selection { anchor, focus: at });
        true
    }

    /// Ends a drag. Returns false if none was in progress.
    pub fn release(&mut self) -> bool {
        self.anchor.take().is_some()
    }

    /// Clears the selection if the text under it changed between two frames.
    pub fn check(&mut self, before: (&Frame, usize), after: (&Frame, usize)) {
        if self
            .current
            .is_some_and(|selection| damaged(selection, before, after))
        {
            self.current = None;
        }
    }
}

/// Whether a left press selects instead of going to the program. Shift overrides reporting.
pub fn owns_press(mode: MouseMode, modifiers: ModifiersState) -> bool {
    mode == MouseMode::Off || modifiers.shift_key()
}

/// Returns the ends of `selection`, oldest first.
pub fn ordered(selection: Selection) -> (Position, Position) {
    if selection.anchor <= selection.focus {
        (selection.anchor, selection.focus)
    } else {
        (selection.focus, selection.anchor)
    }
}

/// Returns the row numbers of `selection` in a view of `rows` rows from row `top`, each with
/// its selected columns.
fn visible(
    selection: Selection,
    top: usize,
    rows: u16,
    columns: u16,
) -> impl Iterator<Item = (usize, Range<usize>)> {
    let (start, end) = ordered(selection);
    let columns = usize::from(columns);
    let rows = start.row.max(top)..end.row.saturating_add(1).min(top + usize::from(rows));
    rows.map(move |row| {
        let first = if row == start.row {
            usize::from(start.column).min(columns)
        } else {
            0
        };
        let last = if row == end.row {
            (usize::from(end.column) + 1).min(columns)
        } else {
            columns
        };
        (row, first..last.max(first))
    })
}

/// Inverts the cells of `selection` in the pane at `rect` of `frame`, whose first row is `top`.
// shortcut: a search match ending on a wide character inverts only its first cell, since the
// frame does not say which cells are a wide character's second half.
pub fn overlay(frame: &mut Frame, rect: Rect, top: usize, selection: Selection) {
    let width = usize::from(frame.columns);
    for (row, columns) in visible(selection, top, rect.rows, rect.columns) {
        let start = (row - top + usize::from(rect.y)) * width + usize::from(rect.x);
        if let Some(cells) = frame
            .cells
            .get_mut(start + columns.start..start + columns.end)
        {
            cells.iter_mut().for_each(pinfold_proto::Cell::invert);
        }
    }
}

/// Whether text under `selection` differs between two views of the pane, in the rows both
/// show. The core's numbers name whatever is on a screen row now, so a selection cannot
/// outlive what the user selected. A new width reflows the screen, so it counts as well.
fn damaged(
    selection: Selection,
    (before, before_top): (&Frame, usize),
    (after, after_top): (&Frame, usize),
) -> bool {
    if before.columns != after.columns {
        return true;
    }
    let width = usize::from(after.columns);
    let shown = before_top..before_top + usize::from(before.rows);
    let text = |frame: &'_ Frame, line: usize, columns: &Range<usize>| {
        let cells = frame
            .cells
            .get(line * width + columns.start..line * width + columns.end);
        cells.map(|cells| cells.iter().map(|cell| cell.character).collect::<Vec<_>>())
    };
    visible(selection, after_top, after.rows, after.columns)
        .filter(|(row, _)| shown.contains(row))
        .any(|(row, columns)| {
            text(before, row - before_top, &columns) != text(after, row - after_top, &columns)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane as the whole grid.
    fn overlay(frame: &mut Frame, top: usize, selection: Selection) {
        let (rows, columns) = (frame.rows, frame.columns);
        let rect = Rect {
            x: 0,
            y: 0,
            columns,
            rows,
        };
        super::overlay(frame, rect, top, selection);
    }

    #[test]
    fn a_selection_stays_in_its_pane_on_a_shared_grid() {
        let plain = frame("abcdefghijklmnopqrstuvwx", 6);
        let mut grid = plain.clone();
        let rect = Rect {
            x: 3,
            y: 1,
            columns: 2,
            rows: 2,
        };
        super::overlay(&mut grid, rect, 10, select(at(10, 0), at(11, 500)));
        let inverted: Vec<usize> = (0..24)
            .filter(|at| grid.cells[*at] != plain.cells[*at])
            .collect();
        assert_eq!(inverted, [9, 10, 15, 16]);
    }
    use pinfold_core::vt::{AlacrittyEngine, VtEngine};
    use pinfold_proto::Cell;

    fn at(row: usize, column: u16) -> Position {
        Position { row, column }
    }

    fn select(anchor: Position, focus: Position) -> Selection {
        Selection { anchor, focus }
    }

    fn frame(text: &str, columns: u16) -> Frame {
        let cells: Vec<Cell> = text
            .chars()
            .map(|character| Cell {
                character,
                foreground: 1,
                background: 2,
            })
            .collect();
        Frame {
            rows: (cells.len() / usize::from(columns)) as u16,
            columns,
            cells,
            ..Frame::default()
        }
    }

    fn inverted(frame: &Frame) -> String {
        frame
            .cells
            .iter()
            .map(|cell| if cell.foreground == 2 { '#' } else { '.' })
            .collect()
    }

    #[test]
    fn a_click_selects_nothing_and_clears() {
        let mut selecting = Selecting {
            current: Some(Selection::default()),
            anchor: None,
        };
        assert!(!selecting.drag(at(0, 0)));
        selecting.press(at(1, 1));
        assert!(selecting.release());
        assert!(!selecting.release());
        assert_eq!(selecting.current, None);
    }

    #[test]
    fn a_drag_runs_from_the_press_to_the_pointer() {
        let mut selecting = Selecting::default();
        selecting.press(at(1, 1));
        assert!(selecting.drag(at(2, 3)));
        assert!(selecting.drag(at(0, 2)));
        assert!(selecting.release());
        assert!(!selecting.drag(at(9, 9)));
        assert_eq!(selecting.current, Some(select(at(1, 1), at(0, 2))));
    }

    #[test]
    fn the_program_keeps_the_mouse_unless_shift_is_held() {
        let none = ModifiersState::empty();
        assert!(owns_press(MouseMode::Off, none));
        assert!(!owns_press(MouseMode::Click, none));
        assert!(!owns_press(MouseMode::Motion, ModifiersState::CONTROL));
        assert!(owns_press(MouseMode::Motion, ModifiersState::SHIFT));
    }

    #[test]
    fn a_selection_covers_the_rows_between_its_ends_in_either_order() {
        let selection = select(at(10, 2), at(12, 1));
        for selection in [selection, select(selection.focus, selection.anchor)] {
            let mut view = frame("abcdefghijkl", 4);
            overlay(&mut view, 10, selection);
            assert_eq!(inverted(&view), "..######ww..".replace('w', "#"));
        }
    }

    #[test]
    fn only_the_rows_in_view_are_drawn() {
        let selection = select(at(5, 1), at(20, 1));
        let mut view = frame("abcdef", 3);
        overlay(&mut view, 10, selection);
        assert_eq!(inverted(&view), "######");
        let mut above = frame("abcdef", 3);
        overlay(&mut above, 30, selection);
        assert_eq!(inverted(&above), "......");
        let mut below = frame("abcdef", 3);
        overlay(&mut below, 0, selection);
        assert_eq!(inverted(&below), "......");
    }

    #[test]
    fn columns_past_the_row_are_cut() {
        let mut view = frame("abcd", 4);
        overlay(&mut view, 0, select(at(0, 2), at(0, 500)));
        assert_eq!(inverted(&view), "..##");
        let mut view = frame("abcd", 4);
        overlay(&mut view, 0, select(at(0, 500), at(0, u16::MAX)));
        assert_eq!(inverted(&view), "....");
        let mut empty = Frame::default();
        overlay(&mut empty, 0, select(at(0, 0), at(9, 9)));
    }

    #[test]
    fn text_changing_under_a_selection_clears_it() {
        let selection = Some(select(at(10, 1), at(10, 2)));
        let check = |after: &Frame, after_top| {
            let mut selecting = Selecting {
                current: selection,
                anchor: None,
            };
            selecting.check((&frame("abcdefgh", 4), 10), (after, after_top));
            selecting.current.is_some()
        };
        assert!(check(&frame("abcdefgh", 4), 10), "nothing changed");
        assert!(
            check(&frame("zzzzabcd", 4), 9),
            "the row moved with its text"
        );
        assert!(check(&frame("zbczefgh", 4), 10), "only other cells changed");
        assert!(check(&frame("zzzzzzzz", 4), 40), "the row is out of view");
        assert!(!check(&frame("axcdefgh", 4), 10), "the text changed");
        assert!(!check(&frame("abcdef", 3), 10), "the width changed");
    }

    #[test]
    fn scrolling_output_keeps_a_selection_on_its_text() {
        let mut selecting = Selecting {
            current: Some(select(at(11, 0), at(11, 1))),
            anchor: None,
        };
        selecting.check((&frame("abcdef", 2), 10), (&frame("cdefgh", 2), 11));
        assert!(selecting.current.is_some());
    }

    #[test]
    fn numbers_from_the_views_top_are_the_cores() {
        let mut engine = AlacrittyEngine::new(4, 20);
        for line in 0..30 {
            engine.feed(format!("row {line}\r\n").as_bytes(), &mut Vec::new());
        }
        engine.scroll(10);
        let top = engine.position(0, 0).row;
        assert_eq!(engine.position(2, 3), at(top + 2, 3));
        let mut text = String::new();
        let selection = select(at(top + 1, 4), at(top, 0));
        assert!(engine.selection_text(selection, &mut text));
        assert_eq!(text, "row 17\nrow 1");
    }
}
