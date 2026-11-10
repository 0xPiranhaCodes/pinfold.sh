//! The search bar, drawn in the row under the pane so program output cannot fake it.

use pinfold_config::theme::Theme;
use pinfold_proto::link::{Direction, MAX_QUERY, Position, Selection};
use pinfold_proto::{Cell, Frame};
use unicode_width::UnicodeWidthChar;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::selection;

/// Past the newest row, so a backward search starts at the newest output.
const NEWEST: Position = Position {
    row: usize::MAX,
    column: u16::MAX,
};

/// What a key did to the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do.
    Stay,
    /// Close the bar.
    Close,
    /// The query changed, so search again from the newest output.
    Typed,
    /// Find the match before the selection.
    Older,
    /// Find the match after the selection.
    Newer,
}

/// A search in progress.
#[derive(Default)]
pub struct Bar {
    query: String,
    /// Whether the last search found nothing.
    pub missing: bool,
}

impl Bar {
    /// Adds text to the query, without control characters and up to the core's limit.
    /// Returns whether the query changed.
    pub fn push(&mut self, text: &str) -> bool {
        let room = MAX_QUERY.saturating_sub(self.query.chars().count());
        let length = self.query.len();
        let text = text.chars().filter(|character| !character.is_control());
        self.query.extend(text.take(room));
        self.query.len() != length
    }

    /// Applies a key, or returns nothing if it is not the bar's. The program still gets Ctrl
    /// keys, so Ctrl+C can stop it while the bar is open.
    pub fn key(
        &mut self,
        key: &Key,
        text: Option<&str>,
        modifiers: ModifiersState,
    ) -> Option<Step> {
        if modifiers.control_key() || modifiers.super_key() {
            return None;
        }
        let changed = |changed| if changed { Step::Typed } else { Step::Stay };
        Some(match key {
            Key::Named(NamedKey::Escape) => Step::Close,
            // Next is older: a search starts at the newest output and walks into history.
            Key::Named(NamedKey::Enter) if modifiers.shift_key() => Step::Newer,
            Key::Named(NamedKey::Enter) => Step::Older,
            Key::Named(NamedKey::Backspace) => changed(self.query.pop().is_some()),
            _ => changed(self.push(text.unwrap_or_default())),
        })
    }

    /// Returns the query, and where to start looking for `step` given the current `selection`.
    // shortcut: no wrap at either end of history, the bar says "no match", wrap if people miss it.
    pub fn search(&self, step: Step, selection: Option<Selection>) -> (&str, Position, Direction) {
        let ends = selection.map(selection::ordered);
        let (from, direction) = match (step, ends) {
            (Step::Older, Some((start, _))) => (start, Direction::Backward),
            (Step::Newer, Some((_, end))) => {
                let column = end.column.saturating_add(1);
                (Position { column, ..end }, Direction::Forward)
            }
            (Step::Newer, None) => (Position::default(), Direction::Forward),
            _ => (NEWEST, Direction::Backward),
        };
        (&self.query, from, direction)
    }

    /// Adds a row for the bar under the pane in `frame`, with the cursor at the query's end.
    // shortcut: a query wider than the window runs off its end, scroll it if that shows up.
    pub fn draw(&self, frame: &mut Frame, theme: &Theme) {
        let (ground, text) = (theme.chrome.overlay, theme.prompt_text());
        let columns = usize::from(frame.columns);
        let start = frame.cells.len();
        frame.cells.resize(
            start + columns,
            Cell {
                character: ' ',
                foreground: text,
                background: ground,
            },
        );
        let mut column = write(frame, start, 0, " Find: ", text);
        column = write(frame, start, column, &self.query, text);
        let cursor = column.min(columns.saturating_sub(1));
        column += 1;
        if self.missing {
            column = write(frame, start, column, " no match", theme.states.error);
        }
        write(
            frame,
            start,
            column,
            "  Enter older, Shift+Enter newer, Esc closes",
            theme.chrome.dim,
        );
        frame.cursor = Some((frame.rows, cursor as u16));
        frame.rows += 1;
    }
}

/// Writes `text` into the row at cell `row` from `column`, and returns the column after it.
pub(crate) fn write(
    frame: &mut Frame,
    row: usize,
    mut column: usize,
    text: &str,
    colour: u32,
) -> usize {
    let columns = usize::from(frame.columns);
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if column + width > columns {
            return columns;
        }
        if let Some(cell) = frame.cells.get_mut(row + column) {
            cell.character = character;
            cell.foreground = colour;
        }
        column += width;
    }
    column
}

/// Returns the rows to scroll a view of `rows` rows from row `top` to show row `row`, which
/// is nothing if the view shows it already. The core does this for the window, so only the
/// tests here that drive an engine directly need it.
#[cfg(test)]
pub fn scroll_to(row: usize, top: usize, rows: u16) -> i32 {
    let bottom = top.saturating_add(usize::from(rows).saturating_sub(1));
    let distance = |rows: usize| i32::try_from(rows).unwrap_or(i32::MAX);
    if row < top {
        distance(top - row)
    } else if row > bottom {
        -distance(row - bottom)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinfold_config::theme::GRAPHITE;
    use pinfold_core::search::{Search, SearchStep};
    use pinfold_core::vt::{AlacrittyEngine, VtEngine};

    const NONE: ModifiersState = ModifiersState::empty();

    fn typed(bar: &mut Bar, text: &str) -> Option<Step> {
        bar.key(&Key::Character(text.into()), Some(text), NONE)
    }

    fn named(bar: &mut Bar, key: NamedKey, modifiers: ModifiersState) -> Option<Step> {
        bar.key(&Key::Named(key), None, modifiers)
    }

    fn row(bar: &Bar, columns: u16) -> (Frame, String) {
        let mut frame = Frame {
            rows: 1,
            columns,
            cells: vec![
                Cell {
                    character: 'x',
                    foreground: 0,
                    background: 0,
                };
                usize::from(columns)
            ],
            ..Frame::default()
        };
        bar.draw(&mut frame, &GRAPHITE);
        let text = frame.cells[usize::from(columns)..]
            .iter()
            .map(|cell| cell.character)
            .collect();
        (frame, text)
    }

    #[test]
    fn keys_edit_the_query_and_steer_the_search() {
        let mut bar = Bar::default();
        assert_eq!(typed(&mut bar, "a"), Some(Step::Typed));
        assert_eq!(typed(&mut bar, "é"), Some(Step::Typed));
        assert_eq!(bar.query, "aé");
        assert_eq!(
            named(&mut bar, NamedKey::Backspace, NONE),
            Some(Step::Typed)
        );
        assert_eq!(bar.query, "a");
        assert_eq!(named(&mut bar, NamedKey::Enter, NONE), Some(Step::Older));
        let shift = ModifiersState::SHIFT;
        assert_eq!(named(&mut bar, NamedKey::Enter, shift), Some(Step::Newer));
        assert_eq!(named(&mut bar, NamedKey::Escape, NONE), Some(Step::Close));
    }

    #[test]
    fn keys_that_change_nothing_stay() {
        let mut bar = Bar::default();
        assert_eq!(named(&mut bar, NamedKey::Backspace, NONE), Some(Step::Stay));
        assert_eq!(named(&mut bar, NamedKey::ArrowUp, NONE), Some(Step::Stay));
        assert_eq!(
            bar.key(&Key::Named(NamedKey::Tab), Some("\t"), NONE),
            Some(Step::Stay)
        );
    }

    #[test]
    fn ctrl_and_super_keys_are_not_the_bars() {
        let mut bar = Bar::default();
        let key = Key::Character("c".into());
        assert_eq!(bar.key(&key, Some("c"), ModifiersState::CONTROL), None);
        assert_eq!(bar.key(&key, Some("c"), ModifiersState::SUPER), None);
        assert_eq!(bar.query, "");
    }

    #[test]
    fn the_query_holds_no_control_characters_and_stops_at_the_limit() {
        let mut bar = Bar::default();
        assert!(bar.push("a\nb\u{1b}[31m"));
        assert_eq!(bar.query, "ab[31m");
        assert!(bar.push(&"é".repeat(MAX_QUERY * 2)));
        assert_eq!(bar.query.chars().count(), MAX_QUERY);
        assert!(!bar.push("x"));
        assert!(Search::new(&bar.query, NEWEST, Direction::Backward).is_some());
    }

    #[test]
    fn a_search_starts_where_its_step_says() {
        let bar = Bar::default();
        let at = |row, column| Position { row, column };
        let selection = Some(Selection {
            anchor: at(7, 9),
            focus: at(5, 2),
        });
        let from = |step, selection| {
            let (_, from, direction) = bar.search(step, selection);
            (from, direction)
        };
        assert_eq!(from(Step::Typed, selection), (NEWEST, Direction::Backward));
        assert_eq!(from(Step::Older, None), (NEWEST, Direction::Backward));
        assert_eq!(
            from(Step::Older, selection),
            (at(5, 2), Direction::Backward)
        );
        assert_eq!(
            from(Step::Newer, selection),
            (at(7, 10), Direction::Forward)
        );
        assert_eq!(from(Step::Newer, None), (at(0, 0), Direction::Forward));
    }

    #[test]
    fn the_bar_is_a_row_of_its_own_with_the_cursor_after_the_query() {
        let mut bar = Bar::default();
        bar.push("漢a");
        let (frame, text) = row(&bar, 80);
        assert_eq!(frame.rows, 2);
        assert_eq!(frame.cells.len(), 160);
        assert_eq!(frame.cells[0].character, 'x');
        assert!(text.starts_with(" Find: 漢 a   Enter older"), "{text}");
        assert_eq!(frame.cursor, Some((1, 10)));
        assert_eq!(frame.cells[80].background, GRAPHITE.chrome.overlay);
    }

    #[test]
    fn a_failed_search_says_so() {
        let mut bar = Bar::default();
        bar.push("a");
        bar.missing = true;
        let (frame, text) = row(&bar, 80);
        assert!(text.starts_with(" Find: a  no match  Enter"), "{text}");
        assert_eq!(frame.cells[90].foreground, GRAPHITE.states.error);
    }

    #[test]
    fn the_bar_is_cut_to_the_window() {
        let mut bar = Bar::default();
        bar.push("abcdefgh");
        let (frame, text) = row(&bar, 10);
        assert_eq!(frame.cells.len(), 20);
        assert_eq!(text, " Find: abc");
        assert_eq!(frame.cursor, Some((1, 9)));
        let (frame, _) = row(&Bar::default(), 0);
        assert_eq!(frame.cursor, Some((1, 0)));
    }

    #[test]
    fn a_row_scrolls_into_view_only_if_it_is_out_of_view() {
        assert_eq!(scroll_to(10, 10, 4), 0);
        assert_eq!(scroll_to(13, 10, 4), 0);
        assert_eq!(scroll_to(4, 10, 4), 6);
        assert_eq!(scroll_to(16, 10, 4), -3);
        assert_eq!(scroll_to(0, usize::MAX, 4), i32::MAX);
    }

    fn find(engine: &AlacrittyEngine, query: &str, step: Step, at: Option<Selection>) -> Selection {
        let bar = Bar {
            query: query.to_owned(),
            missing: false,
        };
        let (query, from, direction) = bar.search(step, at);
        let mut search = Search::new(query, from, direction).unwrap();
        loop {
            match engine.search(&mut search) {
                SearchStep::Running => {}
                SearchStep::Found(found) => return found,
                SearchStep::NotFound => panic!("{query} not found"),
            }
        }
    }

    #[test]
    fn a_match_scrolls_into_view_and_enter_walks_through_history() {
        let mut engine = AlacrittyEngine::new(4, 20);
        for line in 0..40 {
            let text = if line % 10 == 3 { "needle" } else { "hay" };
            engine.feed(format!("{text} {line}\r\n").as_bytes(), &mut Vec::new());
        }
        // Returns the row the match landed on, and the characters drawn inverted there.
        let shown = |engine: &mut AlacrittyEngine, found: Selection| {
            let top = engine.position(0, 0).row;
            engine.scroll(scroll_to(found.anchor.row, top, 4));
            let mut view = Frame::default();
            engine.frame(&mut view);
            let plain = view.clone();
            let rect = pinfold_proto::link::Rect {
                x: 0,
                y: 0,
                columns: view.columns,
                rows: view.rows,
            };
            selection::overlay(&mut view, rect, engine.position(0, 0).row, found, None);
            let cells = plain.cells.iter().zip(&view.cells);
            let picked = cells.filter(|(plain, drawn)| plain != drawn);
            let inverted: String = picked.map(|(_, drawn)| drawn.character).collect();
            let line = found.anchor.row - engine.position(0, 0).row;
            let row = plain.cells.chunks(20).nth(line).unwrap_or_default();
            let row: String = row.iter().map(|cell| cell.character).collect();
            (row.trim_end().to_owned(), inverted)
        };
        let newest = find(&engine, "needle", Step::Typed, None);
        assert_eq!(
            shown(&mut engine, newest),
            ("needle 33".into(), "needle".into())
        );
        let older = find(&engine, "needle", Step::Older, Some(newest));
        assert_eq!(
            shown(&mut engine, older),
            ("needle 23".into(), "needle".into())
        );
        let back = find(&engine, "needle", Step::Newer, Some(older));
        assert_eq!(back, newest);
        assert_eq!(
            shown(&mut engine, back),
            ("needle 33".into(), "needle".into())
        );
    }
}
