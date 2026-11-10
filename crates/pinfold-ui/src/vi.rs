//! Vi mode: a keyboard cursor over the pane's history and screen.
//!
//! The cursor is a core row number and a column, so it costs the same however long history is.
//! Motions read only the frame in view. Moving past it asks the core to scroll, so walking the
//! whole store costs a view per row, as the wheel does, and never a copy of the store. The label
//! sits in the row under the pane, where program output cannot fake it.

use pinfold_config::theme::Theme;
use std::ops::Range;

use pinfold_proto::link::{Position, Selection};
use pinfold_proto::{Cell, Frame};
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::paste;

/// What a key asks the window to do. Every key is vi mode's: none reaches the program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do.
    Stay,
    /// The cursor or the selection changed.
    Moved,
    /// Drop the selection.
    Unselect,
    /// Open the search bar.
    Find,
    /// Copy the selection, then drop it.
    Yank,
    /// Leave vi mode.
    Close,
}

/// What a selection in progress covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Visual {
    Cells,
    Lines,
}

/// What of the pane a motion can see.
pub struct View<'a> {
    /// The rows in view.
    pub frame: &'a Frame,
    /// The core's number for the first row of `frame`.
    pub top: usize,
    /// The rows the view can reach.
    pub held: &'a Range<usize>,
}

impl View<'_> {
    fn row(&self, row: usize) -> Option<&[Cell]> {
        let columns = usize::from(self.frame.columns).max(1);
        let line = row.checked_sub(self.top)?;
        self.frame.cells.chunks_exact(columns).nth(line)
    }

    /// Returns where `at` is in the frame's cells, if it is in view.
    fn index(&self, at: Position) -> Option<usize> {
        self.row(at.row)?;
        let line = at.row - self.top;
        Some(line * usize::from(self.frame.columns) + usize::from(at.column))
    }

    /// Returns the cell at `index`, which a word motion may have walked off the frame, or
    /// `from` if the rows held stop there.
    fn place(&self, index: usize, from: Position) -> Position {
        let columns = usize::from(self.frame.columns).max(1);
        let (row, column) = if index == usize::MAX {
            let last = self.frame.columns.saturating_sub(1);
            (self.top.checked_sub(1), last)
        } else {
            let row = self.top.checked_add(index / columns);
            (row, (index % columns) as u16)
        };
        row.filter(|row| self.held.contains(row))
            .map_or(from, |row| Position { row, column })
    }

    fn clamped(&self, at: Position) -> Position {
        let (held, last) = (self.held, self.frame.columns.saturating_sub(1));
        Position {
            row: at.row.min(held.end.saturating_sub(1)).max(held.start),
            column: at.column.min(last),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Blank,
    Word,
    Mark,
}

fn kind(cell: Option<&Cell>) -> Option<Kind> {
    cell.map(|cell| match cell.character {
        character if character.is_whitespace() => Kind::Blank,
        character if character.is_alphanumeric() || character == '_' => Kind::Word,
        _ => Kind::Mark,
    })
}

/// Returns the start of the word after `from`, or the frame's end if none is in view.
// shortcut: a word that wraps counts as two, and a motion past the view's edge stops on the row
// beyond it, as the frame is all a motion reads. Read rows from the core if people miss it.
fn next_word(cells: &[Cell], from: usize) -> usize {
    let mut at = from;
    let start = kind(cells.get(at));
    while start != Some(Kind::Blank) && kind(cells.get(at)) == start {
        at += 1;
    }
    while kind(cells.get(at)) == Some(Kind::Blank) {
        at += 1;
    }
    at
}

/// Returns the far end of the next word in the direction of `step`, which `e` and `b` share.
/// Off the frame's start that is `usize::MAX`, and off its end the frame's length.
fn word_end(cells: &[Cell], from: usize, step: isize) -> usize {
    let mut at = from.wrapping_add_signed(step);
    while kind(cells.get(at)) == Some(Kind::Blank) {
        at = at.wrapping_add_signed(step);
    }
    let word = kind(cells.get(at));
    while word.is_some() && kind(cells.get(at.wrapping_add_signed(step))) == word {
        at = at.wrapping_add_signed(step);
    }
    at
}

/// The name of what a key does, or nothing if it does nothing.
fn command(key: &Key, modifiers: ModifiersState) -> Option<&str> {
    if modifiers.super_key() || modifiers.alt_key() {
        return None;
    }
    match key {
        Key::Named(NamedKey::ArrowLeft) => Some("h"),
        Key::Named(NamedKey::ArrowDown) => Some("j"),
        Key::Named(NamedKey::ArrowUp) => Some("k"),
        Key::Named(NamedKey::ArrowRight) => Some("l"),
        Key::Named(NamedKey::Home) => Some("0"),
        Key::Named(NamedKey::End) => Some("$"),
        Key::Named(NamedKey::PageUp) => Some("page up"),
        Key::Named(NamedKey::PageDown) => Some("page down"),
        Key::Named(NamedKey::Escape) => Some("escape"),
        Key::Character(text) if modifiers.control_key() => match text.as_str() {
            "b" => Some("page up"),
            "f" => Some("page down"),
            "u" => Some("half up"),
            "d" => Some("half down"),
            _ => None,
        },
        Key::Character(text) => Some(text.as_str()),
        _ => None,
    }
}

/// The cursor, and the selection it is making.
// shortcut: no counts, `?`, `n` or `N`, and `/` searches from the newest output, stepping with
// the bar's Enter and Shift+Enter. Add them once a query can outlive the bar.
#[derive(Clone, Copy, Debug)]
pub struct Vi {
    cursor: Position,
    /// Where the selection in progress started.
    visual: Option<(Position, Visual)>,
    /// Whether the last key was the first `g` of `gg`.
    after_g: bool,
}

impl Vi {
    /// Starts at the program's cursor, or at the last row in view if none is shown.
    #[must_use]
    pub fn new(view: &View) -> Self {
        let last = view.frame.rows.saturating_sub(1);
        let (row, column) = view.frame.cursor.unwrap_or((last, 0));
        Self {
            cursor: Position {
                row: view.top.saturating_add(usize::from(row)),
                column,
            },
            visual: None,
            after_g: false,
        }
    }

    /// Applies a key press.
    pub fn key(&mut self, key: &Key, modifiers: ModifiersState, view: &View) -> Step {
        let after_g = std::mem::take(&mut self.after_g);
        let Some(command) = command(key, modifiers) else {
            return Step::Stay;
        };
        let at = self.cursor;
        let rows = view.frame.rows as isize;
        let by = |lines: isize| Position {
            row: at.row.saturating_add_signed(lines),
            ..at
        };
        let row = |row: usize| Position { row, ..at };
        let column = |column: u16| Position { column, ..at };
        let text = |cell: &Cell| !cell.character.is_whitespace();
        let cells = view.frame.cells.as_slice();
        let word = |index: Option<usize>| index.map_or(at, |index| view.place(index, at));
        let from = view.index(at);
        let target = match command {
            "h" => column(at.column.saturating_sub(1)),
            "l" => column(at.column.saturating_add(1)),
            "j" => by(1),
            "k" => by(-1),
            "0" => column(0),
            "^" => {
                let found = view.row(at.row).and_then(|row| row.iter().position(text));
                column(found.unwrap_or(0) as u16)
            }
            "$" => {
                let found = view.row(at.row).and_then(|row| row.iter().rposition(text));
                column(found.unwrap_or(0) as u16)
            }
            "w" => word(from.map(|from| next_word(cells, from))),
            "e" => word(from.map(|from| word_end(cells, from, 1))),
            "b" => word(from.map(|from| word_end(cells, from, -1))),
            "H" => row(view.top),
            "M" => row(view.top.saturating_add_signed(rows / 2)),
            "L" => row(view.top.saturating_add_signed(rows - 1)),
            "G" => row(usize::MAX),
            "g" if after_g => row(0),
            "g" => {
                self.after_g = true;
                return Step::Stay;
            }
            "half up" => by(-(rows / 2)),
            "half down" => by(rows / 2),
            "page up" => by(-rows),
            "page down" => by(rows),
            "v" => return self.toggle(Visual::Cells),
            "V" => return self.toggle(Visual::Lines),
            "y" => {
                self.visual = None;
                return Step::Yank;
            }
            "/" => return Step::Find,
            "q" => return Step::Close,
            "escape" if self.visual.take().is_some() => return Step::Unselect,
            "escape" => return Step::Close,
            _ => return Step::Stay,
        };
        self.cursor = view.clamped(target);
        Step::Moved
    }

    fn toggle(&mut self, kind: Visual) -> Step {
        self.visual = match self.visual {
            Some((_, old)) if old == kind => None,
            Some((anchor, _)) => Some((anchor, kind)),
            None => Some((self.cursor, kind)),
        };
        if self.visual.is_some() {
            Step::Moved
        } else {
            Step::Unselect
        }
    }

    /// Returns the row the cursor is on, which the view must show.
    #[must_use]
    pub fn row(&self) -> usize {
        self.cursor.row
    }

    /// Returns the selection being made, if any.
    #[must_use]
    pub fn selection(&self) -> Option<Selection> {
        let (anchor, kind) = self.visual?;
        let focus = self.cursor;
        if kind == Visual::Cells {
            return Some(Selection { anchor, focus });
        }
        let (first, last) = if anchor.row <= focus.row {
            (0, u16::MAX)
        } else {
            (u16::MAX, 0)
        };
        Some(Selection {
            anchor: Position {
                column: first,
                ..anchor
            },
            focus: Position {
                column: last,
                ..focus
            },
        })
    }

    /// Stops selecting, as when a click or new text under it ended the selection.
    pub fn end_selection(&mut self) {
        self.visual = None;
    }

    /// Moves the cursor to `at`, such as a search match, and stops selecting.
    pub fn jump(&mut self, at: Position) {
        self.cursor = at;
        self.visual = None;
    }

    /// Keeps the cursor on a cell, as eviction or a resize can take its own away.
    pub fn settle(&mut self, view: &View) {
        self.cursor = view.clamped(self.cursor);
    }

    /// Returns the cursor as a frame's `(row, column)`, if it is in a view of `rows` rows from
    /// row `top`. Output can scroll it away, and the next motion brings it back.
    #[must_use]
    pub fn drawn(&self, top: usize, rows: u16) -> Option<(u16, u16)> {
        let row = u16::try_from(self.cursor.row.checked_sub(top)?).ok()?;
        (row < rows).then_some((row, self.cursor.column))
    }

    /// Adds the mode's row under the pane in `frame`.
    pub fn draw(&self, frame: &mut Frame, theme: &Theme) {
        let text = theme.prompt_text();
        let (label, help) = if self.visual.is_some() {
            (" VISUAL  ", "Move to extend, y copies, Esc cancels")
        } else {
            (
                " VI  ",
                "h j k l move, v selects, y copies, / finds, Esc leaves",
            )
        };
        paste::row(
            frame,
            &[(label.to_owned(), text), (help.to_owned(), text)],
            theme,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_bar::scroll_to;
    use crate::selection;
    use pinfold_config::theme::GRAPHITE;
    use pinfold_core::vt::{AlacrittyEngine, VtEngine};

    const NONE: ModifiersState = ModifiersState::empty();
    const HELD: Range<usize> = 0..100;

    fn at(row: usize, column: u16) -> Position {
        Position { row, column }
    }

    fn frame(lines: &[&str], columns: u16) -> Frame {
        let cell = |character| Cell {
            character,
            foreground: 1,
            background: 2,
        };
        let width = usize::from(columns);
        let row = |line: &&str| {
            let padded = line.chars().chain(std::iter::repeat(' ')).take(width);
            padded.map(cell).collect::<Vec<_>>()
        };
        Frame {
            rows: lines.len() as u16,
            columns,
            cells: lines.iter().flat_map(row).collect(),
            ..Frame::default()
        }
    }

    fn view<'a>(frame: &'a Frame, top: usize, held: &'a Range<usize>) -> View<'a> {
        View { frame, top, held }
    }

    fn vi(cursor: Position) -> Vi {
        Vi {
            cursor,
            visual: None,
            after_g: false,
        }
    }

    fn letter(key: &str) -> Key {
        Key::Character(key.into())
    }

    /// Presses `keys`, each one character, over `lines` from row 10.
    fn pressed(vi: &mut Vi, lines: &[&str], keys: &str) -> Step {
        let (frame, held) = (frame(lines, 12), HELD);
        let view = view(&frame, 10, &held);
        keys.chars().fold(Step::Stay, |_, key| {
            vi.key(&letter(&key.to_string()), NONE, &view)
        })
    }

    fn walk(from: Position, lines: &[&str], keys: &str) -> Position {
        let mut vi = vi(from);
        pressed(&mut vi, lines, keys);
        vi.cursor
    }

    #[test]
    fn it_starts_at_the_programs_cursor_or_the_last_row_in_view() {
        let (mut shown, held) = (frame(&["a", "b"], 12), HELD);
        shown.cursor = Some((1, 4));
        assert_eq!(Vi::new(&view(&shown, 20, &held)).cursor, at(21, 4));
        shown.cursor = None;
        assert_eq!(Vi::new(&view(&shown, 20, &held)).cursor, at(21, 0));
        assert_eq!(Vi::new(&view(&Frame::default(), 0, &held)).cursor, at(0, 0));
    }

    #[test]
    fn hjkl_move_a_cell_and_stop_at_the_edges() {
        let lines = &["", ""];
        assert_eq!(walk(at(11, 3), lines, "h"), at(11, 2));
        assert_eq!(walk(at(11, 3), lines, "j"), at(12, 3));
        assert_eq!(walk(at(11, 3), lines, "k"), at(10, 3));
        assert_eq!(walk(at(11, 3), lines, "l"), at(11, 4));
        assert_eq!(walk(at(11, 0), lines, "h"), at(11, 0));
        assert_eq!(walk(at(11, 11), lines, "l"), at(11, 11));
        assert_eq!(walk(at(0, 0), lines, "k"), at(0, 0));
        assert_eq!(walk(at(99, 0), lines, "j"), at(99, 0));
    }

    #[test]
    fn arrows_and_home_and_end_match_their_letters() {
        let (frame, held) = (frame(&["  ab  "], 12), HELD);
        let view = view(&frame, 10, &held);
        let mut cursor = vi(at(10, 4));
        for (key, expected) in [
            (NamedKey::ArrowLeft, at(10, 3)),
            (NamedKey::ArrowRight, at(10, 4)),
            (NamedKey::ArrowUp, at(9, 4)),
            (NamedKey::ArrowDown, at(10, 4)),
            (NamedKey::Home, at(10, 0)),
            (NamedKey::End, at(10, 3)),
        ] {
            assert_eq!(cursor.key(&Key::Named(key), NONE, &view), Step::Moved);
            assert_eq!(cursor.cursor, expected);
        }
    }

    #[test]
    fn row_ends_go_by_the_text_on_the_row() {
        let lines = &["  ab cd  ", "     "];
        assert_eq!(walk(at(10, 5), lines, "0"), at(10, 0));
        assert_eq!(walk(at(10, 5), lines, "^"), at(10, 2));
        assert_eq!(walk(at(10, 0), lines, "$"), at(10, 6));
        assert_eq!(walk(at(11, 3), lines, "$"), at(11, 0));
    }

    #[test]
    fn word_motions_stop_at_words_and_marks() {
        let lines = &["foo.bar baz", "  next"];
        assert_eq!(walk(at(10, 0), lines, "w"), at(10, 3));
        assert_eq!(walk(at(10, 0), lines, "ww"), at(10, 4));
        assert_eq!(walk(at(10, 0), lines, "www"), at(10, 8));
        assert_eq!(walk(at(10, 0), lines, "e"), at(10, 2));
        assert_eq!(walk(at(10, 2), lines, "e"), at(10, 3));
        assert_eq!(walk(at(10, 9), lines, "b"), at(10, 8));
        assert_eq!(walk(at(10, 8), lines, "b"), at(10, 4));
    }

    #[test]
    fn word_motions_cross_rows() {
        let lines = &["foo.bar baz", "  next"];
        assert_eq!(walk(at(10, 8), lines, "w"), at(11, 2));
        assert_eq!(walk(at(10, 8), lines, "ee"), at(11, 5));
        assert_eq!(walk(at(11, 2), lines, "b"), at(10, 8));
    }

    #[test]
    fn word_motions_leave_the_view_one_row_at_a_time() {
        let lines = &["foo", "bar"];
        assert_eq!(walk(at(11, 0), lines, "w"), at(12, 0));
        assert_eq!(walk(at(10, 0), lines, "b"), at(9, 11));
        assert_eq!(walk(at(11, 0), lines, "e"), at(11, 2));
        assert_eq!(walk(at(11, 2), lines, "e"), at(12, 0));
    }

    #[test]
    fn word_motions_stop_where_the_rows_held_stop() {
        let frame = frame(&["   "], 12);
        let held = 10..11;
        for key in ["w", "e", "b"] {
            let mut cursor = vi(at(10, 1));
            cursor.key(&letter(key), NONE, &view(&frame, 10, &held));
            assert_eq!(cursor.cursor, at(10, 1), "{key}");
        }
        let held = 0..1;
        let mut cursor = vi(at(0, 0));
        cursor.key(&letter("b"), NONE, &view(&frame, 0, &held));
        assert_eq!(cursor.cursor, at(0, 0));
    }

    #[test]
    fn a_word_motion_on_a_row_out_of_view_stays_put() {
        for keys in ["w", "e", "b"] {
            assert_eq!(walk(at(40, 3), &["foo"], keys), at(40, 3), "{keys}");
        }
    }

    #[test]
    fn rows_jump_within_the_view_and_the_rows_held() {
        let lines = &["", "", "", ""];
        assert_eq!(walk(at(12, 1), lines, "H"), at(10, 1));
        assert_eq!(walk(at(10, 1), lines, "M"), at(12, 1));
        assert_eq!(walk(at(10, 1), lines, "L"), at(13, 1));
        assert_eq!(walk(at(12, 1), lines, "G"), at(99, 1));
        assert_eq!(walk(at(12, 1), lines, "gg"), at(0, 1));
    }

    #[test]
    fn g_needs_a_second_g_right_after_it() {
        assert_eq!(walk(at(12, 1), &[""], "g"), at(12, 1));
        assert_eq!(walk(at(12, 1), &[""], "gjg"), at(13, 1));
    }

    #[test]
    fn pages_move_by_the_view() {
        let (frame, held) = (frame(&["", "", "", ""], 12), HELD);
        let view = view(&frame, 10, &held);
        let control = ModifiersState::CONTROL;
        for (key, row) in [("b", 46), ("f", 54), ("u", 48), ("d", 52)] {
            let mut cursor = vi(at(50, 0));
            cursor.key(&letter(key), control, &view);
            assert_eq!(cursor.cursor.row, row, "{key}");
        }
        for (key, row) in [(NamedKey::PageUp, 46), (NamedKey::PageDown, 54)] {
            let mut cursor = vi(at(50, 0));
            cursor.key(&Key::Named(key), NONE, &view);
            assert_eq!(cursor.cursor.row, row);
        }
    }

    #[test]
    fn other_keys_do_nothing() {
        let (frame, held) = (frame(&[""], 12), HELD);
        let view = view(&frame, 10, &held);
        let mut cursor = vi(at(10, 1));
        for (key, modifiers) in [
            (letter("z"), NONE),
            (letter("c"), ModifiersState::CONTROL),
            (letter("j"), ModifiersState::SUPER),
            (letter("j"), ModifiersState::ALT),
            (Key::Named(NamedKey::Enter), NONE),
            (Key::Named(NamedKey::Tab), NONE),
        ] {
            assert_eq!(cursor.key(&key, modifiers, &view), Step::Stay);
        }
        assert_eq!(cursor.cursor, at(10, 1));
    }

    #[test]
    fn v_selects_from_where_it_was_pressed() {
        let mut cursor = vi(at(10, 2));
        assert_eq!(pressed(&mut cursor, &["", ""], "v"), Step::Moved);
        pressed(&mut cursor, &["", ""], "jl");
        let selection = Selection {
            anchor: at(10, 2),
            focus: at(11, 3),
        };
        assert_eq!(cursor.selection(), Some(selection));
        assert_eq!(pressed(&mut cursor, &["", ""], "v"), Step::Unselect);
        assert_eq!(cursor.selection(), None);
    }

    #[test]
    fn shift_v_selects_whole_rows_in_either_direction() {
        let lines = &["", "", ""];
        let mut cursor = vi(at(11, 2));
        pressed(&mut cursor, lines, "Vj");
        let down = cursor.selection().map(selection::ordered);
        assert_eq!(down, Some((at(11, 0), at(12, u16::MAX))));
        pressed(&mut cursor, lines, "kk");
        let up = cursor.selection().map(selection::ordered);
        assert_eq!(up, Some((at(10, 0), at(11, u16::MAX))));
    }

    #[test]
    fn v_and_shift_v_switch_without_moving_the_start() {
        let mut cursor = vi(at(10, 2));
        pressed(&mut cursor, &["", ""], "Vlv");
        let anchor = cursor.selection().map(|selection| selection.anchor);
        assert_eq!(anchor, Some(at(10, 2)));
    }

    #[test]
    fn escape_drops_a_selection_before_it_leaves() {
        let (frame, held) = (frame(&[""], 12), HELD);
        let view = view(&frame, 10, &held);
        let escape = Key::Named(NamedKey::Escape);
        let mut cursor = vi(at(10, 2));
        cursor.key(&letter("v"), NONE, &view);
        assert_eq!(cursor.key(&escape, NONE, &view), Step::Unselect);
        assert_eq!(cursor.key(&escape, NONE, &view), Step::Close);
    }

    #[test]
    fn some_keys_ask_the_window() {
        let mut cursor = vi(at(10, 2));
        assert_eq!(pressed(&mut cursor, &[""], "/"), Step::Find);
        assert_eq!(pressed(&mut cursor, &[""], "q"), Step::Close);
        pressed(&mut cursor, &[""], "v");
        assert_eq!(pressed(&mut cursor, &[""], "y"), Step::Yank);
        assert_eq!(cursor.selection(), None);
    }

    #[test]
    fn a_search_match_moves_the_cursor_and_ends_the_selection() {
        let mut cursor = vi(at(10, 2));
        pressed(&mut cursor, &[""], "v");
        cursor.jump(at(3, 4));
        assert_eq!(cursor.cursor, at(3, 4));
        assert_eq!(cursor.selection(), None);
    }

    #[test]
    fn a_selection_that_ended_elsewhere_ends_here_too() {
        let mut cursor = vi(at(10, 2));
        pressed(&mut cursor, &[""], "v");
        cursor.end_selection();
        assert_eq!(cursor.selection(), None);
    }

    #[test]
    fn eviction_and_a_resize_keep_the_cursor_on_a_cell() {
        let frame = frame(&[""], 12);
        let held = 20..40;
        let mut cursor = vi(at(5, 500));
        cursor.settle(&view(&frame, 30, &held));
        assert_eq!(cursor.cursor, at(20, 11));
        let (empty, nothing) = (Frame::default(), 0..0);
        cursor.settle(&view(&empty, 0, &nothing));
        assert_eq!(cursor.cursor, at(0, 0));
    }

    #[test]
    fn the_cursor_is_drawn_only_in_view() {
        let cursor = vi(at(12, 3));
        assert_eq!(cursor.drawn(10, 4), Some((2, 3)));
        assert_eq!(cursor.drawn(13, 4), None);
        assert_eq!(cursor.drawn(5, 4), None);
        assert_eq!(cursor.drawn(0, 0), None);
    }

    #[test]
    fn the_label_is_a_row_of_its_own_and_says_when_selecting() {
        let row = |cursor: &Vi| {
            let mut frame = frame(&["x"], 40);
            cursor.draw(&mut frame, &GRAPHITE);
            assert_eq!(frame.rows, 2);
            let cells = frame.cells[40..].iter();
            cells.map(|cell| cell.character).collect::<String>()
        };
        let mut cursor = vi(at(10, 2));
        assert!(row(&cursor).starts_with(" VI  h j k l"));
        pressed(&mut cursor, &[""], "v");
        assert!(row(&cursor).starts_with(" VISUAL  Move"));
    }

    #[test]
    fn the_state_stays_small() {
        assert!(size_of::<Vi>() <= 64, "{}", size_of::<Vi>());
    }

    fn filled() -> AlacrittyEngine {
        let mut engine = AlacrittyEngine::new(4, 20);
        for line in 0..1000 {
            engine.feed(format!("row {line}\r\n").as_bytes(), &mut Vec::new());
        }
        engine
    }

    /// Presses a key the way the window and I/O thread do: over a frame, then a scroll.
    fn step(engine: &mut AlacrittyEngine, cursor: &mut Vi, key: &str) {
        let mut frame = Frame::default();
        engine.frame(&mut frame);
        let (top, held) = (engine.position(0, 0).row, engine.view_rows());
        cursor.key(&letter(key), NONE, &view(&frame, top, &held));
        engine.scroll(scroll_to(cursor.row(), top, engine.size().0));
    }

    #[test]
    fn the_cursor_walks_the_whole_store_and_stays_in_view() {
        let mut engine = filled();
        let held = engine.view_rows();
        assert!(held.len() > 900, "{held:?}");
        let mut cursor = vi(engine.position(3, 0));
        let start = cursor.row();
        for _ in 0..held.len() + 5 {
            step(&mut engine, &mut cursor, "k");
            assert!(cursor.drawn(engine.position(0, 0).row, 4).is_some());
        }
        assert_eq!(cursor.row(), held.start);
        assert_eq!(engine.position(0, 0).row, held.start);
        for _ in held.start..start {
            step(&mut engine, &mut cursor, "j");
            assert!(cursor.drawn(engine.position(0, 0).row, 4).is_some());
        }
        assert_eq!(cursor.row(), start);
    }

    #[test]
    fn a_selection_copies_from_the_screen_into_cold_history() {
        let mut engine = filled();
        let mut cursor = vi(engine.position(3, 0));
        for key in ["k", "V", "g", "g"] {
            step(&mut engine, &mut cursor, key);
        }
        let selection = cursor.selection().unwrap_or_default();
        assert_eq!(selection.focus.row, engine.view_rows().start);
        let mut copy = String::new();
        assert!(engine.selection_text(selection, &mut copy));
        assert_eq!(copy.lines().next(), Some("row 0"));
        assert_eq!(copy.lines().last(), Some("row 999"));
    }

    #[test]
    fn with_no_history_to_reach_the_cursor_keeps_to_the_screen() {
        let mut engine = filled();
        engine.feed(b"\x1b[?1049h", &mut Vec::new());
        let mut cursor = vi(engine.position(0, 0));
        let top = cursor.row();
        step(&mut engine, &mut cursor, "g");
        step(&mut engine, &mut cursor, "g");
        assert_eq!(cursor.row(), top);
        step(&mut engine, &mut cursor, "G");
        assert_eq!(cursor.row(), top + 3);
    }
}
