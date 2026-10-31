//! API types, JSON Schemas and version negotiation, with no I/O.

/// One character with its colours, as `0x00RRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character. A space draws only the background.
    pub character: char,
    /// Text colour.
    pub foreground: u32,
    /// Background colour.
    pub background: u32,
}

/// A pane's visible screen, ready to draw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub columns: u16,
    /// `rows` × `columns` cells, row by row.
    pub cells: Vec<Cell>,
    /// The visible cursor, as `(row, column)`.
    pub cursor: Option<(u16, u16)>,
}
