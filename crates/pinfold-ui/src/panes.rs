//! Puts the panes of a tab on one grid of cells, with cell-drawn dividers between them.
//!
//! A border belongs to no pane, so a program cannot reach it. The one around the focused pane
//! takes the focus accent, as Foundations sets. Panes have no header yet, so no number either.

use pinfold_proto::link::Rect;
use pinfold_proto::{Cell, DEFAULT_BACKGROUND, Frame};

/// `Focus accent` in Foundations.
const FOCUS: u32 = 0x005f_d4d0;
/// `Rule, quiet fill` in Foundations.
const RULE: u32 = 0x0026_2b33;

/// Empties `frame` to `rows` by `columns`.
pub fn blank(frame: &mut Frame, rows: u16, columns: u16) {
    let cell = Cell {
        character: ' ',
        foreground: DEFAULT_BACKGROUND,
        background: DEFAULT_BACKGROUND,
    };
    frame.rows = rows;
    frame.columns = columns;
    frame.cursor = None;
    frame.cells.clear();
    frame
        .cells
        .resize(usize::from(rows) * usize::from(columns), cell);
}

/// Copies `pane` to `rect` of `target`. Between a layout and the frame that follows it the two
/// can differ, so what does not fit is cut off, and what is missing stays blank.
pub fn blit(target: &mut Frame, pane: &Frame, rect: Rect) {
    let width = usize::from(target.columns);
    let (x, columns) = (usize::from(rect.x), usize::from(pane.columns));
    let shown = columns
        .min(usize::from(rect.columns))
        .min(width.saturating_sub(x));
    if shown == 0 {
        return;
    }
    let rows = pane
        .cells
        .chunks_exact(columns)
        .take(usize::from(rect.rows));
    for (row, cells) in (usize::from(rect.y)..usize::from(target.rows)).zip(rows) {
        let start = row * width + x;
        if let (Some(to), Some(from)) = (
            target.cells.get_mut(start..start + shown),
            cells.get(..shown),
        ) {
            to.copy_from_slice(from);
        }
    }
}

/// Whether `rect` holds the cell at `row` and `column` of the grid.
pub fn holds(rect: Rect, row: u16, column: u16) -> bool {
    (rect.y..rect.y + rect.rows).contains(&row) && (rect.x..rect.x + rect.columns).contains(&column)
}

/// Returns the grid's cell `(row, column)` as the nearest cell of the pane at `rect`.
pub fn local(rect: Rect, (row, column): (u16, u16)) -> (u16, u16) {
    (
        row.saturating_sub(rect.y).min(rect.rows.saturating_sub(1)),
        column
            .saturating_sub(rect.x)
            .min(rect.columns.saturating_sub(1)),
    )
}

/// Returns a pane's cursor as a cell of the grid, or nothing if the pane at `rect` is too small
/// for it, as it can be until the frame that follows a layout.
pub fn cursor(rect: Rect, cursor: Option<(u16, u16)>) -> Option<(u16, u16)> {
    let (row, column) =
        cursor.filter(|(row, column)| *row < rect.rows && *column < rect.columns)?;
    Some((row + rect.y, column + rect.x))
}

/// Whether a cell of a grid of `rows` by `columns` is on a border.
fn on_border(rects: &[Rect], rows: u16, columns: u16, row: i32, column: i32) -> bool {
    let (Ok(row), Ok(column)) = (u16::try_from(row), u16::try_from(column)) else {
        return false;
    };
    row < rows && column < columns && !rects.iter().any(|rect| holds(*rect, row, column))
}

/// The box-drawing character for a border cell, given which neighbours are border too.
const fn glyph(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (false, true, true, true) => '┬',
        (true, false, true, true) => '┴',
        (false, true, false, true) => '┌',
        (false, true, true, false) => '┐',
        (true, false, false, true) => '└',
        (true, false, true, false) => '┘',
        (_, _, false, false) => '│',
        _ => '─',
    }
}

/// Draws the borders of the panes at `rects`, on the cells no pane holds along their right and
/// bottom edges. Every border is one of those, as a layout divides the grid with no gaps.
pub fn dividers(frame: &mut Frame, rects: &[Rect], focused: Option<Rect>) {
    let (rows, columns) = (frame.rows, frame.columns);
    let width = usize::from(columns);
    let near = |at: u16, start: u16, length: u16| at + 1 >= start && at <= start + length;
    let touches = |row: u16, column: u16| {
        focused
            .is_some_and(|rect| near(row, rect.y, rect.rows) && near(column, rect.x, rect.columns))
    };
    let mut draw = |row: u16, column: u16| {
        let (y, x) = (i32::from(row), i32::from(column));
        let gap = |row, column| on_border(rects, rows, columns, row, column);
        if !gap(y, x) {
            return;
        }
        let at = usize::from(row) * width + usize::from(column);
        if let Some(cell) = frame.cells.get_mut(at) {
            cell.character = glyph(gap(y - 1, x), gap(y + 1, x), gap(y, x - 1), gap(y, x + 1));
            cell.foreground = if touches(row, column) { FOCUS } else { RULE };
        }
    };
    for rect in rects {
        let (bottom, right) = (rect.y + rect.rows, rect.x + rect.columns);
        for row in rect.y..=bottom {
            draw(row, right);
        }
        for column in rect.x..=right {
            draw(bottom, column);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, columns: u16, rows: u16) -> Rect {
        Rect {
            x,
            y,
            columns,
            rows,
        }
    }

    fn pane(character: char, rows: u16, columns: u16) -> Frame {
        Frame {
            rows,
            columns,
            cells: vec![
                Cell {
                    character,
                    foreground: 1,
                    background: 2,
                };
                usize::from(rows) * usize::from(columns)
            ],
            ..Frame::default()
        }
    }

    fn lines(frame: &Frame) -> Vec<String> {
        frame
            .cells
            .chunks_exact(usize::from(frame.columns))
            .map(|row| row.iter().map(|cell| cell.character).collect())
            .collect()
    }

    /// Three panes: a tall one on the left, and two stacked on the right.
    fn three() -> (Frame, [Rect; 3]) {
        let rects = [rect(0, 0, 2, 5), rect(3, 0, 4, 2), rect(3, 3, 4, 2)];
        let mut grid = Frame::default();
        blank(&mut grid, 5, 7);
        blit(&mut grid, &pane('a', 5, 2), rects[0]);
        blit(&mut grid, &pane('b', 2, 4), rects[1]);
        blit(&mut grid, &pane('c', 2, 4), rects[2]);
        (grid, rects)
    }

    #[test]
    fn panes_land_in_their_rectangles_with_borders_between() {
        let (mut grid, rects) = three();
        dividers(&mut grid, &rects, None);
        assert_eq!(
            lines(&grid),
            ["aa│bbbb", "aa│bbbb", "aa├────", "aa│cccc", "aa│cccc"]
        );
    }

    #[test]
    fn four_panes_meet_at_a_cross() {
        let rects = [
            rect(0, 0, 2, 1),
            rect(3, 0, 2, 1),
            rect(0, 2, 2, 1),
            rect(3, 2, 2, 1),
        ];
        let mut grid = Frame::default();
        blank(&mut grid, 3, 5);
        dividers(&mut grid, &rects, None);
        assert_eq!(lines(&grid), ["  │  ", "──┼──", "  │  "]);
    }

    #[test]
    fn a_pane_of_the_whole_grid_has_no_border() {
        let whole = rect(0, 0, 3, 2);
        let mut grid = Frame::default();
        blank(&mut grid, 2, 3);
        blit(&mut grid, &pane('a', 2, 3), whole);
        dividers(&mut grid, &[whole], Some(whole));
        assert_eq!(lines(&grid), ["aaa", "aaa"]);
    }

    #[test]
    fn only_the_border_around_the_focused_pane_takes_the_accent() {
        let (mut grid, rects) = three();
        dividers(&mut grid, &rects, Some(rects[1]));
        let colour = |row: usize, column: usize| grid.cells[row * 7 + column].foreground;
        assert_eq!(colour(0, 2), FOCUS);
        assert_eq!(colour(2, 2), FOCUS);
        assert_eq!(colour(2, 6), FOCUS);
        assert_eq!(colour(4, 2), RULE);
        assert_eq!(colour(3, 2), RULE);
    }

    #[test]
    fn a_grid_cell_maps_to_the_nearest_cell_of_a_pane() {
        let pane = rect(3, 1, 4, 2);
        assert_eq!(local(pane, (2, 5)), (1, 2));
        assert_eq!(local(pane, (0, 0)), (0, 0));
        assert_eq!(local(pane, (9, 9)), (1, 3));
    }

    #[test]
    fn a_cursor_moves_with_its_pane_and_stays_in_it() {
        let pane = rect(3, 1, 4, 2);
        assert_eq!(cursor(pane, Some((1, 3))), Some((2, 6)));
        assert_eq!(cursor(pane, Some((2, 0))), None);
        assert_eq!(cursor(pane, None), None);
    }

    #[test]
    fn a_frame_of_another_size_is_cut_to_its_rectangle() {
        let mut grid = Frame::default();
        blank(&mut grid, 2, 4);
        blit(&mut grid, &pane('a', 9, 9), rect(1, 1, 2, 1));
        assert_eq!(lines(&grid), ["    ", " aa "]);
        blit(&mut grid, &pane('b', 1, 1), rect(0, 0, 2, 2));
        assert_eq!(lines(&grid), ["b   ", " aa "]);
    }

    #[test]
    fn a_rectangle_off_the_grid_draws_nothing_and_does_not_panic() {
        let mut grid = Frame::default();
        blank(&mut grid, 2, 2);
        let far = rect(1000, 1000, 24, 24);
        blit(&mut grid, &pane('a', 24, 24), far);
        blit(&mut grid, &Frame::default(), rect(0, 0, 2, 2));
        dividers(&mut grid, &[far], Some(far));
        assert_eq!(lines(&grid), ["  ", "  "]);
    }
}
