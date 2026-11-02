//! The empty pixels between the window's edge and the grid.

/// Padding on each side in logical pixels, as `(left, top, right, bottom)`.
const LOGICAL: (f64, f64, f64, f64) = (14.0, 12.0, 14.0, 10.0);

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
}

impl Padding {
    /// Returns the padding of a window with this ratio of physical to logical pixels.
    ///
    /// ```
    /// use pinfold_render::padding::Padding;
    ///
    /// let padding = Padding::scaled(2.0);
    /// assert_eq!((padding.left, padding.top), (28, 24));
    /// ```
    #[must_use]
    pub fn scaled(scale: f64) -> Self {
        let pixels = |logical: f64| (logical * scale).round() as usize;
        Self {
            left: pixels(LOGICAL.0),
            top: pixels(LOGICAL.1),
            right: pixels(LOGICAL.2),
            bottom: pixels(LOGICAL.3),
        }
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
            fit(height, self.top.saturating_add(self.bottom), cell.1),
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
        }
    }

    #[test]
    fn padding_follows_the_scale_factor() {
        assert_eq!(Padding::scaled(1.0), padding());
        let scaled = Padding::scaled(1.5);
        assert_eq!(
            (scaled.left, scaled.top, scaled.right, scaled.bottom),
            (21, 18, 21, 15)
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
