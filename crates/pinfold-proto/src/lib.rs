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

/// Which mouse events a program asked to be told about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    /// None.
    #[default]
    Off,
    /// Presses, releases and the wheel.
    Click,
    /// Those, and motion with a button held.
    Drag,
    /// Those, and all motion.
    Motion,
}

/// How a program asked for mouse reports to be written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseEncoding {
    /// One byte per value, so reports stop at column and row 223.
    #[default]
    Legacy,
    /// One UTF-8 character per value (mode 1005).
    Utf8,
    /// Decimal numbers (mode 1006).
    Sgr,
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
    /// Whether the program asked for application cursor keys.
    pub app_cursor: bool,
    /// Which mouse events the program wants.
    pub mouse: MouseMode,
    /// How the program wants them written.
    pub mouse_encoding: MouseEncoding,
    /// Whether the wheel should send arrow keys, on the alternate screen.
    pub alternate_scroll: bool,
}
