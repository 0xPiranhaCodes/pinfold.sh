//! Laying stored rows out at a pane's width, as the live grid does on a resize (ADR-0029).
//!
//! Stored rows never change. A wrapped row joins the one after it into a logical line, and that
//! line reads as rows of the current width, which this file calls shown rows. A wide character
//! that would cross the edge moves down and leaves a spacer behind, as `grid/resize.rs` does.

use alacritty_terminal::term::cell::{Cell, Flags};

/// Where a layout stands between two stored rows, which is all it needs to go on from there.
/// A width of 0 keeps rows as they were written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flow {
    /// The columns of a shown row.
    pub(crate) width: u16,
    /// The cells in the last shown row.
    column: u16,
    /// Whether the last stored row wrapped, so that the next one continues it.
    open: bool,
}

impl Flow {
    pub(crate) fn new(width: u16) -> Self {
        Self {
            width,
            ..Self::default()
        }
    }

    /// Starts a stored row of `cells` cells, and returns whether it starts a shown row. A blank
    /// row always does, so a stale wrap flag cannot swallow it and the row count can come back.
    pub(crate) fn enter(&mut self, cells: usize) -> bool {
        if self.open && cells > 0 {
            return false;
        }
        self.column = 0;
        true
    }

    /// Places `count` cells that share `flags`, calling `wrapped` for each shown row they start,
    /// with whether a wide character left the last column of the row before empty.
    pub(crate) fn place(&mut self, flags: Flags, mut count: usize, mut wrapped: impl FnMut(bool)) {
        if self.width == 0 || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            return;
        }
        let (width, mut column) = (usize::from(self.width), usize::from(self.column));
        let wide = flags.contains(Flags::WIDE_CHAR);
        while count > 0 {
            let need = if wide { 2 } else { 1 };
            // At column 0 it fits or nothing will: a width of 1 cannot make it wrap forever.
            if column > 0 && column + need > width {
                wrapped(wide && column + 1 == width);
                column = 0;
            }
            let fit = if wide {
                1
            } else {
                count.min(width - column).max(1)
            };
            column += fit;
            count -= fit;
        }
        self.column = column as u16;
    }

    /// Ends a stored row whose last cell had `flags`.
    pub(crate) fn leave(&mut self, flags: Flags) {
        self.open = self.width != 0 && flags.contains(Flags::WRAPLINE);
    }

    /// Whether the last shown row still takes cells from the next stored row.
    fn takes_more(self) -> bool {
        self.open && self.column < self.width
    }
}

/// Turns stored rows into `left` shown rows, from the `skip`th to start.
#[derive(Debug)]
pub(crate) struct Reader {
    flow: Flow,
    skip: usize,
    left: usize,
    /// The shown rows started so far.
    started: usize,
    /// The row being built, which goes unused unless it is wanted.
    row: Vec<Cell>,
    /// Whether `row` is a whole row so far, which it is not after the middle of a line.
    pending: bool,
    /// The number of the stored row being read.
    stored: usize,
    /// The number of the stored row that the first wanted row starts in.
    first: Option<usize>,
}

impl Reader {
    pub(crate) fn new(flow: Flow, skip: usize, left: usize, stored: usize) -> Self {
        Self {
            flow,
            skip,
            left,
            started: 0,
            row: Vec::new(),
            pending: false,
            stored,
            first: None,
        }
    }

    /// Returns a reader that only counts, which is what a new width needs of every block.
    pub(crate) fn counting(flow: Flow) -> Self {
        Self::new(flow, usize::MAX, 1, 0)
    }

    pub(crate) fn done(&self) -> bool {
        self.left == 0
    }

    pub(crate) fn flow(&self) -> Flow {
        self.flow
    }

    /// Returns the shown rows started since this was last called, then goes on from zero.
    pub(crate) fn take_started(&mut self) -> usize {
        std::mem::take(&mut self.started)
    }

    /// Returns the shown rows started so far.
    pub(crate) fn started(&self) -> usize {
        self.started
    }

    /// Returns the stored row that the first wanted row starts in, if it was reached.
    pub(crate) fn first_stored(&self) -> Option<usize> {
        self.first
    }

    /// Whether the last shown row is still being filled, so the row after it does not start one.
    pub(crate) fn inside_row(&self) -> bool {
        self.flow.takes_more()
    }

    /// Whether a stored row of `cells` cells may hold a wanted row, so that it needs building.
    pub(crate) fn wants(&self, cells: usize) -> bool {
        let shortest = usize::from(self.flow.width).saturating_sub(1).max(1);
        self.started
            .saturating_add(cells / shortest)
            .saturating_add(2)
            > self.skip
    }

    fn begin(&mut self, visit: &mut impl FnMut(&[Cell])) {
        self.finish(true, visit);
        self.started += 1;
        self.pending = true;
        if self.started == self.skip.saturating_add(1) {
            self.first = Some(self.stored);
        }
    }

    /// Hands over the row being built, if it is wanted. One cut short by `wrapped` says so.
    pub(crate) fn finish(&mut self, wrapped: bool, visit: &mut impl FnMut(&[Cell])) {
        if self.pending && self.started > self.skip && self.left > 0 {
            if let Some(cell) = self.row.last_mut().filter(|_| wrapped) {
                cell.flags.insert(Flags::WRAPLINE);
            }
            visit(&self.row);
            self.left -= 1;
        }
        self.pending = false;
        self.row.clear();
    }

    /// Reads a stored row, building nothing.
    pub(crate) fn skip_cells(&mut self, flags: Flags, count: usize) {
        let started = &mut self.started;
        self.flow.place(flags, count, |_| *started += 1);
    }

    /// Starts a stored row that [`Self::skip_cells`] reads.
    pub(crate) fn enter(&mut self, cells: usize) {
        self.started += usize::from(self.flow.enter(cells));
    }

    /// Ends a stored row that [`Self::skip_cells`] read.
    pub(crate) fn leave(&mut self, flags: Flags) {
        self.flow.leave(flags);
        self.stored += 1;
    }

    /// Reads a stored row of `cells`, handing over each wanted row it completes.
    pub(crate) fn row(&mut self, cells: &[Cell], visit: &mut impl FnMut(&[Cell])) {
        let last = cells.last().map_or(Flags::empty(), |cell| cell.flags);
        let wrapped = last.contains(Flags::WRAPLINE);
        let width = usize::from(self.flow.width);
        if width == 0 || (!self.flow.open && !wrapped && cells.len() <= width) {
            // Nearly every row, which needs no copy.
            self.flow.enter(cells.len());
            self.started += 1;
            if self.started == self.skip.saturating_add(1) {
                self.first = Some(self.stored);
            }
            if self.started > self.skip && self.left > 0 {
                visit(cells);
                self.left -= 1;
            }
            self.stored += 1;
            return;
        }
        if self.flow.enter(cells.len()) {
            self.begin(visit);
        }
        for cell in cells {
            let mut wrap = None;
            self.flow.place(cell.flags, 1, |pad| wrap = Some(pad));
            if let Some(pad) = wrap {
                if pad {
                    let mut spacer = Cell::default();
                    spacer.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.row.push(spacer);
                }
                self.begin(visit);
            }
            if !cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                let mut cell = cell.clone();
                cell.flags.remove(Flags::WRAPLINE);
                self.row.push(cell);
            }
        }
        self.flow.leave(last);
        self.stored += 1;
        if !wrapped {
            self.finish(false, visit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Hyperlink;

    fn cell(c: char, flags: Flags) -> Cell {
        Cell {
            c,
            flags,
            ..Cell::default()
        }
    }

    /// Stored rows from text: `>` wraps a row, `漢` is wide and `~` is a leading spacer.
    fn stored(rows: &[&str]) -> Vec<Vec<Cell>> {
        let row = |text: &&str| {
            let (text, wrapped) = match text.strip_suffix('>') {
                Some(text) => (text, true),
                None => (*text, false),
            };
            let mut cells = Vec::new();
            for c in text.chars() {
                match c {
                    '漢' => {
                        cells.push(cell(c, Flags::WIDE_CHAR));
                        cells.push(cell(' ', Flags::WIDE_CHAR_SPACER));
                    }
                    '~' => cells.push(cell(' ', Flags::LEADING_WIDE_CHAR_SPACER)),
                    _ => cells.push(cell(c, Flags::empty())),
                }
            }
            if let Some(cell) = cells.last_mut().filter(|_| wrapped) {
                cell.flags.insert(Flags::WRAPLINE);
            }
            cells
        };
        rows.iter().map(row).collect()
    }

    fn text(row: &[Cell]) -> String {
        let cells = row
            .iter()
            .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER));
        let mut text: String = cells
            .map(|c| {
                if c.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    '~'
                } else {
                    c.c
                }
            })
            .collect();
        if row
            .last()
            .is_some_and(|c| c.flags.contains(Flags::WRAPLINE))
        {
            text.push('>');
        }
        text
    }

    fn shown(rows: &[&str], width: u16) -> Vec<String> {
        let mut out = Vec::new();
        let mut visit = |row: &[Cell]| out.push(text(row));
        let mut reader = Reader::new(Flow::new(width), 0, usize::MAX, 0);
        for row in stored(rows) {
            reader.row(&row, &mut visit);
        }
        reader.finish(true, &mut visit);
        out
    }

    #[test]
    fn a_wrapped_line_joins_at_a_wider_width() {
        assert_eq!(
            shown(&["abc>", "def>", "gh", "x"], 6),
            ["abcdef>", "gh", "x"]
        );
        assert_eq!(shown(&["abc>", "def"], 9), ["abcdef"]);
    }

    #[test]
    fn a_long_row_splits_at_a_narrower_width() {
        assert_eq!(shown(&["abcdefg", "x"], 3), ["abc>", "def>", "g", "x"]);
    }

    #[test]
    fn a_wide_character_never_crosses_the_edge() {
        assert_eq!(shown(&["a漢漢"], 4), ["a漢~>", "漢"]);
        assert_eq!(shown(&["a漢~>", "漢"], 5), ["a漢漢"]);
    }

    #[test]
    fn its_own_width_gives_back_what_was_stored() {
        let rows = ["a漢~>", "漢b>", "", "abcd>", "e", ""];
        assert_eq!(shown(&rows, 4), rows);
    }

    #[test]
    fn width_one_and_empty_input_stop() {
        assert_eq!(shown(&["漢a>", "b"], 1).len(), 4);
        assert_eq!(shown(&["", ""], 1), ["", ""]);
        assert_eq!(shown(&[], 7), [""; 0]);
    }

    #[test]
    fn a_line_with_its_end_not_stored_yet_says_it_wrapped() {
        assert_eq!(shown(&["abc>"], 6), ["abc>"]);
    }

    #[test]
    fn marks_and_links_stay_on_their_cells() {
        let mut rows = stored(&["ab>", "c"]);
        rows[1][0].push_zerowidth('\u{301}');
        let link = Hyperlink::new(Some("a"), "https://pinfold.sh".to_owned());
        rows[1][0].set_hyperlink(Some(link.clone()));
        let mut out = Vec::new();
        let mut visit = |row: &[Cell]| out.push(row.to_vec());
        let mut reader = Reader::new(Flow::new(9), 0, usize::MAX, 0);
        rows.iter().for_each(|row| reader.row(row, &mut visit));
        assert_eq!(out[0][2].zerowidth(), Some(&['\u{301}'][..]));
        assert_eq!(out[0][2].hyperlink(), Some(link));
        assert_eq!(out[0][1].hyperlink(), None);
    }

    #[test]
    fn one_call_for_a_run_counts_what_a_call_per_cell_counts() {
        for width in 1..12_u16 {
            for count in 0..40 {
                let (mut run, mut each) = (Flow::new(width), Flow::new(width));
                let (mut together, mut apart) = (0, 0);
                run.place(Flags::empty(), count, |_| together += 1);
                (0..count).for_each(|_| each.place(Flags::empty(), 1, |_| apart += 1));
                assert_eq!((run, together), (each, apart), "{count} at {width}");
            }
        }
    }
}
