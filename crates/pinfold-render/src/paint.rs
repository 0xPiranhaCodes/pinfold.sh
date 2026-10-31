//! Draws rows of cells into a 32-bit pixel buffer.

use pinfold_proto::Cell;

use crate::font::Font;
use crate::glyphs::{CACHE_LIMIT, GlyphCache};

const MIN_SIZE: f32 = 4.0;
const MAX_SIZE: f32 = 256.0;

/// A pixel buffer of `0x00RRGGBB` values.
#[derive(Debug)]
pub struct Canvas<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
}

impl<'a> Canvas<'a> {
    /// Wraps `pixels`, or returns nothing if it does not hold `width` × `height` pixels.
    #[must_use]
    pub fn new(pixels: &'a mut [u32], width: usize, height: usize) -> Option<Self> {
        (width.checked_mul(height)? == pixels.len()).then_some(Self {
            pixels,
            width,
            height,
        })
    }

    fn fill(&mut self, left: usize, top: usize, right: usize, bottom: usize, colour: u32) {
        let right = right.min(self.width);
        for y in top..bottom.min(self.height) {
            if left < right {
                self.pixels[y * self.width + left..y * self.width + right].fill(colour);
            }
        }
    }
}

// shortcut: blends in gamma space, which thins light text a little, revisit with #21.
fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let mix = |shift: u32| {
        let (back, front) = ((background >> shift) & 0xff, (foreground >> shift) & 0xff);
        ((back * (255 - u32::from(alpha)) + front * u32::from(alpha)) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Turns cells into pixels, using one font at one size.
pub struct Painter {
    cache: GlyphCache,
    cell_width: usize,
    cell_height: usize,
    baseline: i32,
}

impl Painter {
    /// Creates a painter. `size` is in physical pixels, and is clamped to a sane range.
    #[must_use]
    pub fn new(font: Font, size: f32) -> Self {
        Self::with_limit(font, size, CACHE_LIMIT)
    }

    fn with_limit(font: Font, size: f32, limit: usize) -> Self {
        let size = if size.is_finite() {
            size.clamp(MIN_SIZE, MAX_SIZE)
        } else {
            MIN_SIZE
        };
        let face = font.face();
        let metrics = face.metrics(&[]).scale(size);
        let advance = face
            .glyph_metrics(&[])
            .scale(size)
            .advance_width(face.charmap().map('M'));
        let (cell_width, cell_height) = (
            (advance.round() as usize).max(1),
            ((metrics.ascent + metrics.descent + metrics.leading).ceil() as usize).max(1),
        );
        let baseline = metrics.ascent.round() as i32;
        Self {
            cache: GlyphCache::new(font, size, limit),
            cell_width,
            cell_height,
            baseline,
        }
    }

    /// Returns the size of a cell in pixels, as `(width, height)`.
    #[must_use]
    pub fn cell_size(&self) -> (usize, usize) {
        (self.cell_width, self.cell_height)
    }

    /// Returns the glyph cache.
    #[must_use]
    pub fn cache(&self) -> &GlyphCache {
        &self.cache
    }

    /// Draws a row of cells. Anything past the canvas is clipped, and nothing is drawn
    /// outside this row, so damaged rows can be redrawn alone.
    pub fn draw_row(&mut self, canvas: &mut Canvas, row: usize, cells: &[Cell]) {
        let top = row.saturating_mul(self.cell_height);
        if top >= canvas.height {
            return;
        }
        let bottom = (top + self.cell_height).min(canvas.height);
        // Backgrounds go first, so a wide glyph is not erased by the cell after it.
        for (column, cell) in cells.iter().enumerate() {
            let left = column.saturating_mul(self.cell_width);
            canvas.fill(
                left,
                top,
                left.saturating_add(self.cell_width),
                bottom,
                cell.background,
            );
        }
        let pen_y = i32::try_from(top)
            .unwrap_or(i32::MAX)
            .saturating_add(self.baseline);
        for (column, cell) in cells.iter().enumerate() {
            if cell.character == ' ' {
                continue;
            }
            let Some(glyph) = self.cache.get(cell.character) else {
                continue;
            };
            let left =
                i64::try_from(column * self.cell_width).unwrap_or(i64::MAX) + i64::from(glyph.left);
            let first = i64::from(pen_y) - i64::from(glyph.top);
            for (line, mask) in glyph.mask.chunks(glyph.width.max(1)).enumerate() {
                let y = first + line as i64;
                if y < top as i64 || y >= bottom as i64 {
                    continue;
                }
                for (offset, &alpha) in mask.iter().enumerate() {
                    let x = left + offset as i64;
                    if alpha == 0 || x < 0 || x >= canvas.width as i64 {
                        continue;
                    }
                    let at = y as usize * canvas.width + x as usize;
                    canvas.pixels[at] = blend(canvas.pixels[at], cell.foreground, alpha);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INK: u32 = 0x00ff_ffff;
    const PAPER: u32 = 0x0010_2030;

    fn cell(character: char) -> Cell {
        Cell {
            character,
            foreground: INK,
            background: PAPER,
        }
    }

    fn painter() -> Painter {
        Painter::new(Font::bundled().unwrap(), 16.0)
    }

    fn pixels(painter: &Painter, columns: usize, rows: usize) -> (Vec<u32>, usize, usize) {
        let (width, height) = painter.cell_size();
        (
            vec![0; width * columns * height * rows],
            width * columns,
            height * rows,
        )
    }

    #[test]
    fn a_glyph_draws_ink_on_its_background() {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 2, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(&mut canvas, 0, &[cell('M'), cell(' ')]);
        let brightest = buffer.iter().map(|pixel| pixel & 0xff).max().unwrap();
        assert!(brightest > 0xa0, "text is too faint: {brightest:#x}");
        assert!(buffer.contains(&PAPER));
        assert!(!buffer.contains(&0));
    }

    #[test]
    fn a_space_draws_only_background() {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 1, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(&mut canvas, 0, &[cell(' ')]);
        assert!(buffer.iter().all(|&pixel| pixel == PAPER));
    }

    #[test]
    fn a_row_never_touches_its_neighbours() {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 4, 3);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(
            &mut canvas,
            1,
            &[cell('g'), cell('j'), cell('Å'), cell('|')],
        );
        let row = height / 3;
        assert!(buffer[..row * width].iter().all(|&pixel| pixel == 0));
        assert!(buffer[2 * row * width..].iter().all(|&pixel| pixel == 0));
    }

    #[test]
    fn anything_past_the_canvas_is_clipped() {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 2, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(&mut canvas, 0, &[cell('M'); 500]);
        painter.draw_row(&mut canvas, 9, &[cell('M')]);
        painter.draw_row(&mut canvas, usize::MAX, &[cell('M')]);
    }

    #[test]
    fn a_wrongly_sized_buffer_is_refused() {
        assert!(Canvas::new(&mut [0; 10], 3, 3).is_none());
        assert!(Canvas::new(&mut [], usize::MAX, usize::MAX).is_none());
    }

    #[test]
    fn bad_sizes_are_clamped() {
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, 1e9] {
            let (width, height) = Painter::new(Font::bundled().unwrap(), size).cell_size();
            assert!(width > 0 && width <= 1024 && height > 0 && height <= 1024);
        }
    }

    #[test]
    fn repeated_glyphs_are_cached_once() {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 2, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(&mut canvas, 0, &[cell('M')]);
        let used = painter.cache().used();
        painter.draw_row(&mut canvas, 0, &[cell('M'), cell('M')]);
        assert_eq!(painter.cache().used(), used);
    }

    #[test]
    fn the_cache_stays_under_its_limit() {
        let limit = 2_000;
        let mut painter = Painter::with_limit(Font::bundled().unwrap(), 16.0, limit);
        let (mut buffer, width, height) = pixels(&painter, 1, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        for character in '!'..='~' {
            painter.draw_row(&mut canvas, 0, &[cell(character)]);
            assert!(painter.cache().used() <= limit);
        }
        assert!(painter.cache().used() > 0);
    }

    #[test]
    fn a_glyph_bigger_than_the_cache_is_skipped() {
        assert!(
            GlyphCache::new(Font::bundled().unwrap(), 16.0, 0)
                .get('M')
                .is_none()
        );
    }

    fn cost(character: char) -> usize {
        let mut cache = GlyphCache::new(Font::bundled().unwrap(), 16.0, CACHE_LIMIT);
        cache.get(character);
        cache.used()
    }

    #[test]
    fn the_least_recently_used_glyph_goes_first() {
        // One byte too small for all three, so exactly one glyph has to leave.
        let limit = cost('H') + cost('I') + cost('O') - 1;
        let mut cache = GlyphCache::new(Font::bundled().unwrap(), 16.0, limit);
        cache.get('H');
        cache.get('I');
        cache.get('H');
        cache.get('O');
        assert!(cache.contains('H') && cache.contains('O'));
        assert!(!cache.contains('I'));
    }

    #[test]
    fn blending_reaches_both_ends() {
        assert_eq!(blend(PAPER, INK, 0), PAPER);
        assert_eq!(blend(PAPER, INK, 255), INK);
    }
}
