//! The empty pixels between the window's edge and the grid.

/// Padding on each side in logical pixels, as `(left, top, right, bottom)`. The top puts the
/// first row level with the macOS traffic lights, while wider sides give the terminal room to breathe.
const LOGICAL: (f64, f64, f64, f64) = (16.0, 8.0, 16.0, 16.0);
/// Logical pixels under the top bar, so it stands clear of the pane.
const BAR_GAP: f64 = 18.0;
/// Logical pixels between the title and the tabs.
const TITLE_GAP: f64 = 16.0;
/// Logical pixels above the tabs in fullscreen, where there is no title.
const FULLSCREEN_TOP: f64 = 14.0;

/// Rows of the top bar: a title, then the tab strip. The gap comes after them.
pub const BAR_ROWS: usize = 2;

/// Physical pixels around the grid. Only the default background is drawn here, and window
/// chrome can use these numbers to place itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Padding {
    /// Pixels left of the first column.
    pub left: usize,
    /// Pixels above the first row.
    pub top: usize,
    /// Pixels right of the last column, at least.
    pub right: usize,
    /// Pixels below the last row, at least.
    pub bottom: usize,
    /// Pixels between the top bar and the rows below it.
    pub gap: usize,
    /// Pixels between the top bar's title and its tabs.
    pub title_gap: usize,
    /// Whether the top bar has lost its title row, as a fullscreen window does.
    pub fullscreen: bool,
    /// The row of tabs, whose pills stand taller than the row and whose text is centred in them.
    pub tab_row: Option<usize>,
}

impl Padding {
    /// Aligns the grid's left edge and title capitals with native window buttons. All
    /// inputs are physical pixels. A large font stays inside the window even when its
    /// title is taller than the native title bar. Fullscreen keeps its own padding.
    #[must_use]
    pub fn aligned_title(self, left: f64, center_y: f64, text_center: f64) -> Self {
        if self.fullscreen {
            return self;
        }
        Self {
            left: left.max(0.0).round() as usize,
            top: (center_y - text_center).max(0.0).round() as usize,
            ..self
        }
    }

    /// Returns the padding of a window with this ratio of physical to logical pixels.
    ///
    /// ```
    /// use pinfold_render::padding::Padding;
    ///
    /// let padding = Padding::scaled(2.0);
    /// assert_eq!((padding.left, padding.top), (32, 16));
    /// ```
    #[must_use]
    pub fn scaled(scale: f64) -> Self {
        let pixels = |logical: f64| (logical * scale).round() as usize;
        Self {
            left: pixels(LOGICAL.0),
            top: pixels(LOGICAL.1),
            right: pixels(LOGICAL.2),
            bottom: pixels(LOGICAL.3),
            gap: pixels(BAR_GAP),
            title_gap: pixels(TITLE_GAP),
            fullscreen: false,
            tab_row: Some(BAR_ROWS - 1),
        }
    }

    /// Returns this padding for a fullscreen window at the same `scale`.
    #[must_use]
    pub fn in_fullscreen(self, scale: f64) -> Self {
        Self {
            top: (FULLSCREEN_TOP * scale).round() as usize,
            fullscreen: true,
            tab_row: Some(0),
            ..self
        }
    }

    /// Returns how many rows the top bar takes.
    #[must_use]
    pub fn bar_rows(self) -> usize {
        if self.fullscreen { 1 } else { BAR_ROWS }
    }

    fn title_gap(self) -> usize {
        if self.fullscreen { 0 } else { self.title_gap }
    }

    /// Returns the pixel a row starts at, for cells `cell_height` tall.
    #[must_use]
    pub fn row_top(self, row: usize, cell_height: usize) -> usize {
        let gap = if row < self.bar_rows() { 0 } else { self.gap };
        let title = if row == 0 { 0 } else { self.title_gap() };
        self.top
            .saturating_add(row.saturating_mul(cell_height))
            .saturating_add(gap)
            .saturating_add(title)
    }

    /// Returns how many whole `(width, height)` cells fit in a surface, as `(rows, columns)`.
    /// A surface too small for one cell gives zero.
    #[must_use]
    pub fn grid(self, width: u32, height: u32, cell: (usize, usize)) -> (u16, u16) {
        let fit = |pixels: u32, padding: usize, size: usize| {
            let room = (pixels as usize).saturating_sub(padding);
            u16::try_from(room / size).unwrap_or(u16::MAX)
        };
        (
            fit(
                height,
                self.top
                    .saturating_add(self.bottom)
                    .saturating_add(self.gap)
                    .saturating_add(self.title_gap()),
                cell.1,
            ),
            fit(width, self.left.saturating_add(self.right), cell.0),
        )
    }

    /// Returns the cell under a point on the surface, as `(row, column)`, for a grid of
    /// `(rows, columns)`. A point in the padding belongs to the nearest cell.
    #[must_use]
    pub fn cell_at(self, x: f64, y: f64, cell: (usize, usize), grid: (u16, u16)) -> (u16, u16) {
        let index = |pixels: f64, padding: usize, size: usize, cells: u16| {
            let last = usize::from(cells.max(1)) - 1;
            ((pixels - padding as f64).max(0.0) as usize / size).min(last) as u16
        };
        (
            index(y, self.top, cell.1, grid.0),
            index(x, self.left, cell.0, grid.1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (usize, usize) = (10, 20);

    fn padding() -> Padding {
        Padding {
            left: 14,
            top: 12,
            right: 14,
            bottom: 10,
            gap: 0,
            ..Padding::default()
        }
    }

    #[test]
    fn a_fullscreen_window_has_no_title_row() {
        let padding = Padding::scaled(1.0);
        assert_eq!(padding.row_top(1, 20), 8 + 20 + 16);
        assert_eq!(padding.row_top(2, 20), 8 + 40 + 16 + 18);
        let full = padding.in_fullscreen(1.0);
        assert_eq!(full.bar_rows(), 1);
        assert_eq!(full.row_top(0, 20), 14);
        assert_eq!(full.row_top(1, 20), 14 + 20 + 18);
        assert_eq!((padding.tab_row, full.tab_row), (Some(1), Some(0)));
        let rows = |padding: Padding| padding.grid(100, 200, (10, 20)).0;
        assert_eq!(rows(padding), (200 - 8 - 16 - 18 - 16) / 20);
        assert_eq!(rows(full), (200 - 14 - 16 - 18) / 20);
        assert_eq!(full.aligned_title(8.0, 16.0, 9.0), full);
        assert_eq!(padding.aligned_title(8.0, 16.0, 40.0).top, 0);
    }

    #[test]
    fn padding_follows_the_scale_factor() {
        let expected = Padding {
            left: 16,
            top: 8,
            right: 16,
            bottom: 16,
            gap: 18,
            title_gap: 16,
            tab_row: Some(1),
            ..padding()
        };
        assert_eq!(Padding::scaled(1.0), expected);
        let scaled = Padding::scaled(1.5);
        assert_eq!(
            (scaled.left, scaled.top, scaled.right, scaled.bottom),
            (24, 12, 24, 24)
        );
    }

    #[test]
    fn the_grid_loses_the_padding() {
        assert_eq!(padding().grid(128, 62, CELL), (2, 10));
        // One pixel short of the next row and column.
        assert_eq!(padding().grid(127, 61, CELL), (1, 9));
        assert_eq!(Padding::default().grid(128, 62, CELL), (3, 12));
    }

    #[test]
    fn only_rows_after_the_bar_sit_below_the_gap() {
        let padding = Padding {
            gap: 6,
            ..padding()
        };
        assert_eq!(padding.row_top(0, 20), 12);
        assert_eq!(padding.row_top(1, 20), 32);
        assert_eq!(padding.row_top(2, 20), 58);
        assert_eq!(padding.grid(128, 68, CELL), (2, 10));
        assert_eq!(padding.grid(128, 67, CELL), (1, 10));
    }

    #[test]
    fn a_surface_smaller_than_the_padding_has_no_grid() {
        assert_eq!(padding().grid(5, 5, CELL), (0, 0));
        assert_eq!(Padding::scaled(1e300).grid(5, 5, CELL), (0, 0));
    }

    #[test]
    fn a_huge_surface_saturates() {
        assert_eq!(
            Padding::default().grid(u32::MAX, u32::MAX, (1, 1)),
            (u16::MAX, u16::MAX)
        );
    }

    #[test]
    fn a_point_maps_to_the_cell_inside_the_padding() {
        let grid = (5, 5);
        assert_eq!(padding().cell_at(14.0, 12.0, CELL, grid), (0, 0));
        assert_eq!(padding().cell_at(23.9, 31.9, CELL, grid), (0, 0));
        assert_eq!(padding().cell_at(24.0, 32.0, CELL, grid), (1, 1));
    }

    #[test]
    fn a_point_in_the_padding_takes_the_nearest_cell() {
        let grid = (5, 5);
        assert_eq!(padding().cell_at(0.0, 0.0, CELL, grid), (0, 0));
        assert_eq!(padding().cell_at(-3.0, -3.0, CELL, grid), (0, 0));
        assert_eq!(padding().cell_at(1e9, 1e9, CELL, grid), (4, 4));
        assert_eq!(padding().cell_at(9.0, 9.0, CELL, (0, 0)), (0, 0));
    }
}
