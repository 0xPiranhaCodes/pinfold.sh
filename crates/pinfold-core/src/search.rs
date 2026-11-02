//! Finding and copying text across a pane's history and screen.
//!
//! Both name cells by [`Position`]. A row's number is the store's number for it, and a screen
//! row counts on from the store's end, so a row keeps its number as it leaves the screen.

use std::ops::Range;

use alacritty_terminal::term::cell::{Cell, Flags};

/// Most rows one step reads, which is one or two blocks. See `benches/scrollback.rs`.
const STEP_ROWS: usize = 64;
/// Longest query, in characters, which bounds the work per cell and what a search carries.
pub const MAX_QUERY: usize = 256;
/// Most text one selection returns, which is the threat model's limit on a read.
pub const MAX_SELECTION: usize = 2_000_000;
/// Default tab stops, since history does not keep the stops in force when it was written.
const TAB_STOP: usize = 8;

const SPACERS: Flags = Flags::WIDE_CHAR_SPACER.union(Flags::LEADING_WIDE_CHAR_SPACER);

/// A cell, in numbers that survive output and eviction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// The row's number.
    pub row: usize,
    /// The column it was written at. History is not reflowed.
    pub column: u16,
}

/// The cells of a match, or of a selection in either order, both ends included.
///
/// This is a plain value: it holds nothing output can invalidate. Reading one clamps it to the
/// rows still held.
// shortcut: a screen row's number names whatever is there now, so text a program overwrites
// after selection is copied as the new text, and the alternate screen shares its numbers with
// the primary. Clear selections on damage when the window draws them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Where it started.
    pub anchor: Position,
    /// Where it ends now.
    pub focus: Position,
}

/// Which way a search runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Toward newer rows.
    Forward,
    /// Toward older rows.
    Backward,
}

/// What a step of a search came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStep {
    /// Call again.
    Running,
    /// The nearest match. Start a new search next to it to find the one after.
    Found(Selection),
    /// Every row was read.
    NotFound,
}

/// Lowers a character to one character.
// shortcut: no multi-character folds, so "ß" does not match "SS", and combining marks are not
// searched, so "e" matches "é" written as two code points. Fine for literal search in logs.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_wrapped(cells: &[Cell]) -> bool {
    cells
        .last()
        .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE))
}

/// A literal, case-insensitive search that reads a few blocks' worth of rows per step, so its
/// caller can serve PTYs between steps. A newline in the query matches the end of an unwrapped
/// line. It holds one row and a query's length of characters, which is what lets a match cross
/// a step, a soft wrap and the edge of the store.
///
/// ```
/// use pinfold_core::search::{Direction, Position, Search, SearchStep};
/// use pinfold_core::vt::{AlacrittyEngine, VtEngine};
///
/// let mut engine = AlacrittyEngine::new(4, 20);
/// engine.feed(b"a needle", &mut Vec::new());
/// let search = Search::new("NEEDLE", Position::default(), Direction::Forward);
/// let mut search = search.ok_or("the query is usable")?;
/// let SearchStep::Found(found) = engine.search(&mut search) else {
///     return Err("it is on the screen".into());
/// };
/// assert_eq!(found.anchor.column, 2);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Search {
    query: Vec<char>,
    direction: Direction,
    /// Forward, the first cell to read. Backward, the first cell not to.
    from: Position,
    /// Forward, the next row. Backward, the row after the next one.
    cursor: usize,
    /// Where a forward search ends. Output cannot keep a search running if this holds still.
    limit: Option<usize>,
    /// The characters still able to start a match.
    hay: Vec<(char, Position)>,
    /// Backward, the start of the rows read this step, then those of the step before.
    head: Vec<(char, Position)>,
    after: Vec<(char, Position)>,
    found: Option<Selection>,
}

impl Search {
    /// Prepares a search from `from`. Returns nothing for an empty query or one over
    /// [`MAX_QUERY`] characters.
    #[must_use]
    pub fn new(query: &str, from: Position, direction: Direction) -> Option<Self> {
        let query: Vec<char> = query.chars().take(MAX_QUERY + 1).map(fold).collect();
        (1..=MAX_QUERY).contains(&query.len()).then(|| Self {
            query,
            direction,
            from,
            cursor: match direction {
                Direction::Forward => from.row,
                Direction::Backward => from.row.saturating_add(1),
            },
            limit: None,
            hay: Vec::new(),
            head: Vec::new(),
            after: Vec::new(),
            found: None,
        })
    }

    /// Returns the rows of the next step, given the rows `held` now.
    pub(crate) fn next_rows(&mut self, held: &Range<usize>) -> Range<usize> {
        let limit = *self.limit.get_or_insert(held.end);
        match self.direction {
            Direction::Forward => {
                let start = self.cursor.max(held.start);
                if start != self.cursor {
                    // What was kept no longer touches what comes next.
                    self.hay.clear();
                }
                start..start.saturating_add(STEP_ROWS).min(limit)
            }
            Direction::Backward => {
                let end = self.cursor.min(limit);
                end.saturating_sub(STEP_ROWS).max(held.start)..end
            }
        }
    }

    /// Reads the next row of a step. A step's rows arrive oldest first in both directions.
    pub(crate) fn row(&mut self, number: usize, cells: &[Cell]) {
        if self.direction == Direction::Forward && self.found.is_some() {
            return;
        }
        for (column, cell) in cells.iter().enumerate() {
            if !cell.flags.intersects(SPACERS) {
                self.push(cell.c, number, column);
            }
        }
        if !is_wrapped(cells) {
            self.push('\n', number, cells.len());
        }
        self.scan();
    }

    fn push(&mut self, c: char, row: usize, column: usize) {
        let at = Position {
            row,
            column: column as u16,
        };
        let forward = self.direction == Direction::Forward;
        if forward != (at >= self.from) {
            return;
        }
        let entry = (fold(c), at);
        self.hay.push(entry);
        if !forward && self.head.len() + 1 < self.query.len() {
            self.head.push(entry);
        }
    }

    /// Looks for a match, and drops the characters too late to start one.
    fn scan(&mut self) {
        let is_match = |window: &&[(char, Position)]| {
            window
                .iter()
                .map(|entry| entry.0)
                .eq(self.query.iter().copied())
        };
        let mut windows = self.hay.windows(self.query.len());
        let hit = match self.direction {
            Direction::Forward => windows.find(is_match),
            // Rows arrive oldest first, so the last match seen is the nearest.
            Direction::Backward => windows.rfind(is_match),
        };
        if let Some((first, last)) = hit.and_then(|hit| hit.first().zip(hit.last())) {
            self.found = Some(Selection {
                anchor: first.1,
                focus: last.1,
            });
        }
        let keep = self.query.len() - 1;
        self.hay.drain(..self.hay.len().saturating_sub(keep));
    }

    /// Finishes a step over `rows`.
    pub(crate) fn end_step(&mut self, rows: Range<usize>) -> SearchStep {
        if self.direction == Direction::Backward {
            self.hay.append(&mut self.after);
            self.scan();
            self.hay.clear();
            std::mem::swap(&mut self.after, &mut self.head);
            self.cursor = rows.start;
        } else {
            self.cursor = rows.end;
        }
        match self.found {
            Some(found) => SearchStep::Found(found),
            None if rows.is_empty() => SearchStep::NotFound,
            None => SearchStep::Running,
        }
    }
}

/// Appends `selection`'s part of a row to `text`. Returns false, adding nothing, if that would
/// pass [`MAX_SELECTION`].
pub(crate) fn copy_row(
    (start, end): (Position, Position),
    number: usize,
    cells: &[Cell],
    text: &mut String,
) -> bool {
    let wrapped = is_wrapped(cells);
    let used = cells
        .iter()
        .rposition(|cell| wrapped || cell.c != ' ')
        .map_or(0, |at| at + 1);
    let mut first = if number == start.row {
        usize::from(start.column)
    } else {
        0
    };
    if cells
        .get(first)
        .is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR_SPACER))
    {
        first = first.saturating_sub(1);
    }
    let last = if number == end.row {
        used.min(usize::from(end.column) + 1)
    } else {
        used
    };
    let length = text.len();
    let mut tab = false;
    for (column, cell) in cells.iter().enumerate().take(last).skip(first) {
        // A tab fills the cells to the next stop with blanks that nobody typed.
        tab = tab && column % TAB_STOP != 0 && cell.c == ' ';
        if !tab && !cell.flags.intersects(SPACERS) {
            text.push(cell.c);
            text.extend(cell.zerowidth().unwrap_or_default());
        }
        tab |= cell.c == '\t';
    }
    if !wrapped && number != end.row {
        text.push('\n');
    }
    let fits = text.len() <= MAX_SELECTION;
    if !fits {
        text.truncate(length);
    }
    fits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_bounded() {
        let from = Position::default();
        assert!(Search::new("", from, Direction::Forward).is_none());
        let longest = "x".repeat(MAX_QUERY);
        assert!(Search::new(&longest, from, Direction::Forward).is_some());
        let long = "x".repeat(MAX_QUERY + 1);
        assert!(Search::new(&long, from, Direction::Backward).is_none());
    }

    #[test]
    fn folding_keeps_characters_without_a_lower_case() {
        assert_eq!(fold('É'), 'é');
        assert_eq!(fold('漢'), '漢');
        assert_eq!(fold('\u{130}'), 'i');
    }
}
