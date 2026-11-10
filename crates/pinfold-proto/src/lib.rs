//! API types, JSON Schemas and version negotiation, with no I/O.

pub mod link;

/// Bold text, set in the top byte of [`Cell::foreground`].
pub const BOLD: u32 = 1 << 24;
/// Italic text, set in the top byte of [`Cell::foreground`].
pub const ITALIC: u32 = 1 << 25;
/// On [`LEFT_CAP`] or [`RIGHT_CAP`], the end of a tab, which stands taller than its cell. Only
/// the window's own top bar sets this.
pub const TALL: u32 = 1 << 26;
/// The colour of a cell no program has coloured, and of the window around the grid.
///
/// shortcut: a copy of the core's own default, so after OSC 11 a program's new default counts
/// as a cell colour and stays opaque, carry the live colour in `Frame` when the core next changes.
pub const DEFAULT_BACKGROUND: u32 = 0x0012_1212;

/// The colours a pane's named and indexed colours resolve through, as `0x00RRGGBB` (ADR-0049).
/// The 256-colour cube and the grey ramp are computed, and are not here.
///
/// ```
/// use pinfold_proto::{DEFAULT_BACKGROUND, Palette};
///
/// assert_eq!(Palette::GRAPHITE_GRID.background, DEFAULT_BACKGROUND);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// The colour of a cell no program has coloured.
    pub background: u32,
    /// The colour of text no program has coloured.
    pub foreground: u32,
    /// Black, red, green, yellow, blue, magenta, cyan and white, then the bright eight.
    pub ansi: [u32; 16],
}

impl Palette {
    /// "Graphite Grid", the default (`docs/design/ui/`), which the core uses until a window
    /// sends another.
    pub const GRAPHITE_GRID: Self = Self {
        background: DEFAULT_BACKGROUND,
        foreground: 0x00e2_e5ea,
        ansi: [
            0x0012_151a,
            0x00ff_8a8a,
            0x0063_d29a,
            0x00f5_b544,
            0x007a_a7ff,
            0x00d9_a7e0,
            0x009c_c0ff,
            0x00c9_cdd4,
            0x0034_3a44,
            0x00ff_8a8a,
            0x0063_d29a,
            0x00f5_b544,
            0x009c_c0ff,
            0x00d9_a7e0,
            0x00c4_d9ff,
            0x00e2_e5ea,
        ],
    };
}

/// The round left end of a pill, which the renderer draws to fill its cell in the foreground
/// colour, as a font would not fit it to the grid.
pub const LEFT_CAP: char = '\u{e0b6}';
/// The round right end of a pill.
pub const RIGHT_CAP: char = '\u{e0b4}';

/// One character with its colours, as `0x00RRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The character. A space draws only the background.
    pub character: char,
    /// Text colour, with any of [`BOLD`] and [`ITALIC`] in the top byte, which keeps a cell
    /// at 12 bytes.
    pub foreground: u32,
    /// Background colour.
    pub background: u32,
}

impl Cell {
    /// The style bits of [`Cell::foreground`], 0 for regular text.
    ///
    /// ```
    /// use pinfold_proto::{BOLD, Cell};
    ///
    /// let cell = Cell { character: 'a', foreground: 0x00ff_ffff | BOLD, background: 0 };
    /// assert_eq!(cell.style(), BOLD);
    /// ```
    #[must_use]
    pub fn style(&self) -> u32 {
        self.foreground & (BOLD | ITALIC | TALL)
    }

    /// Swaps text and background colours. The style stays with the text.
    pub fn invert(&mut self) {
        let style = self.style();
        std::mem::swap(&mut self.foreground, &mut self.background);
        self.foreground |= style;
        self.background &= !style;
    }
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
    /// How many OSC 52 clipboard writes the program has asked for, wrapping.
    pub clipboard_writes: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverting_swaps_the_colours_and_keeps_the_style_on_the_text() {
        let cell = Cell {
            character: 'a',
            foreground: 0x00ab_cdef | BOLD | ITALIC,
            background: 0x0012_3456,
        };
        let mut inverted = cell;
        inverted.invert();
        assert_eq!(inverted.foreground, 0x0012_3456 | BOLD | ITALIC);
        assert_eq!(inverted.background, 0x00ab_cdef);
        inverted.invert();
        assert_eq!(inverted, cell);
    }
}
