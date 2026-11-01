//! Draws rows of cells into a 32-bit pixel buffer.

use std::sync::LazyLock;

use pinfold_proto::Cell;

use crate::font::{Font, Fonts};
use crate::glyphs::{CACHE_LIMIT, GlyphCache, Metrics, clamp_size};

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

    /// Fills every pixel.
    pub fn clear(&mut self, colour: u32) {
        self.pixels.fill(colour);
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

/// The steps of linear light in the tables. Each sRGB byte lands within half a step of its
/// own value, so it comes back from the round trip unchanged.
const LINEAR_MAX: u32 = 4095;

/// sRGB bytes as linear light, the decoding Metal does.
static LINEAR: LazyLock<[u16; 256]> = LazyLock::new(|| {
    std::array::from_fn(|byte| {
        let value = byte as f32 / 255.0;
        let light = if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        };
        (light * LINEAR_MAX as f32).round() as u16
    })
});

/// Linear light as sRGB bytes.
static SRGB: LazyLock<[u8; LINEAR_MAX as usize + 1]> = LazyLock::new(|| {
    std::array::from_fn(|step| {
        let light = step as f32 / LINEAR_MAX as f32;
        let value = if light <= 0.003_130_8 {
            light * 12.92
        } else {
            1.055 * light.powf(1.0 / 2.4) - 0.055
        };
        (value * 255.0).round() as u8
    })
});

/// Mixes in linear light, as coverage measures light and sRGB bytes do not. Only the colour
/// bytes of `foreground` are read, so a style in its top byte does nothing.
fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let (linear, srgb, alpha) = (&*LINEAR, &*SRGB, u32::from(alpha));
    let mix = |shift: u32| {
        let light = |colour: u32| u32::from(linear[(colour >> shift & 0xff) as usize]);
        let mixed = (light(background) * (255 - alpha) + light(foreground) * alpha + 127) / 255;
        u32::from(srgb[mixed as usize]) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// Turns cells into pixels, using one font at one size.
pub struct Painter {
    cache: GlyphCache,
}

impl Painter {
    /// Creates a painter. `size` is in physical pixels, and is clamped to a sane range.
    #[must_use]
    pub fn new(font: Font, size: f32) -> Self {
        Self::with_limit(font, size, CACHE_LIMIT)
    }

    fn with_limit(fonts: impl Into<Fonts>, size: f32, limit: usize) -> Self {
        Self {
            cache: GlyphCache::new(fonts, clamp_size(size), limit),
        }
    }

    /// Returns the size of a cell in pixels, as `(width, height)`.
    #[must_use]
    pub fn cell_size(&self) -> (usize, usize) {
        let metrics = self.cache.metrics();
        (metrics.cell_width, metrics.cell_height)
    }

    /// Sets the font size in physical pixels, dropping every glyph drawn at the old one.
    pub fn set_font_size(&mut self, size: f32) {
        self.cache.set_size(clamp_size(size));
    }

    /// Returns the glyph cache.
    #[must_use]
    pub fn cache(&self) -> &GlyphCache {
        &self.cache
    }

    /// Draws a row of cells. Anything past the canvas is clipped, and nothing is drawn
    /// outside this row, so damaged rows can be redrawn alone.
    pub fn draw_row(&mut self, canvas: &mut Canvas, row: usize, cells: &[Cell]) {
        let Metrics {
            cell_width,
            cell_height,
            baseline,
        } = self.cache.metrics();
        let top = row.saturating_mul(cell_height);
        if top >= canvas.height {
            return;
        }
        let bottom = (top + cell_height).min(canvas.height);
        // Backgrounds go first, so a wide glyph is not erased by the cell after it.
        for (column, cell) in cells.iter().enumerate() {
            let left = column.saturating_mul(cell_width);
            canvas.fill(
                left,
                top,
                left.saturating_add(cell_width),
                bottom,
                cell.background,
            );
        }
        let pen_y = i32::try_from(top)
            .unwrap_or(i32::MAX)
            .saturating_add(baseline);
        for (column, cell) in cells.iter().enumerate() {
            if cell.character == ' ' {
                continue;
            }
            let Some(glyph) = self.cache.get(cell.character, cell.style()) else {
                continue;
            };
            let left =
                i64::try_from(column * cell_width).unwrap_or(i64::MAX) + i64::from(glyph.left);
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
    use pinfold_proto::{BOLD, ITALIC};

    use super::*;
    use crate::font::fixture::{File, KANJI, kanji_font};

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
                .get('M', 0)
                .is_none()
        );
    }

    fn cost(character: char) -> usize {
        let mut cache = GlyphCache::new(Font::bundled().unwrap(), 16.0, CACHE_LIMIT);
        cache.get(character, 0);
        cache.used()
    }

    #[test]
    fn the_least_recently_used_glyph_goes_first() {
        // One byte too small for all three, so exactly one glyph has to leave.
        let limit = cost('H') + cost('I') + cost('O') - 1;
        let mut cache = GlyphCache::new(Font::bundled().unwrap(), 16.0, limit);
        cache.get('H', 0);
        cache.get('I', 0);
        cache.get('H', 0);
        cache.get('O', 0);
        assert!(cache.contains('H') && cache.contains('O'));
        assert!(!cache.contains('I'));
    }

    #[test]
    fn a_scale_change_drops_glyphs_and_changes_the_cell() {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 1, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(&mut canvas, 0, &[cell('M')]);
        assert!(painter.cache().contains('M'));
        let before = painter.cell_size();
        painter.set_font_size(32.0);
        assert_eq!(painter.cache().used(), 0);
        assert!(!painter.cache().contains('M'));
        assert!(painter.cell_size().0 > before.0 && painter.cell_size().1 > before.1);
    }

    #[test]
    fn a_wide_glyph_inks_both_of_its_cells() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let fonts = Fonts::with_fallbacks(Font::bundled().unwrap(), file.list);
        let mut painter = Painter::with_limit(fonts, 16.0, CACHE_LIMIT);
        let (mut buffer, width, height) = pixels(&painter, 2, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        painter.draw_row(&mut canvas, 0, &[cell(KANJI), cell(' ')]);
        let inked = |columns: std::ops::Range<usize>| {
            let mut rows = buffer.chunks(width);
            rows.any(|row| row[columns.clone()].iter().any(|&pixel| pixel != PAPER))
        };
        assert!(inked(0..width / 2) && inked(width / 2..width));
    }

    #[test]
    fn blending_reaches_both_ends() {
        assert_eq!(blend(PAPER, INK, 0), PAPER);
        assert_eq!(blend(PAPER, INK, 255), INK);
    }

    #[test]
    fn half_coverage_is_half_the_light() {
        // Mixing the bytes gives 0x80 both ways, so light text gains and dark text thins.
        assert_eq!(blend(0, INK, 128), 0x00bc_bcbc);
        assert_eq!(blend(INK, 0, 128), 0x00bb_bbbb);
        // `shaded_pixels_are_mixed_in_linear_light` in `pinfold_sys::metal` expects this too.
        assert_eq!(blend(PAPER, INK, 128), 0x00bc_bdbe);
    }

    #[test]
    fn every_byte_survives_the_trip_through_linear_light() {
        for byte in 0..=255_u32 {
            let colour = byte << 16 | byte << 8 | byte;
            // Text hidden by giving it the background's colour stays hidden at any coverage.
            assert_eq!(blend(colour, colour, 77), colour);
        }
    }

    #[test]
    fn a_style_in_the_foreground_is_not_a_colour() {
        assert_eq!(blend(PAPER, INK | BOLD | ITALIC, 255), INK);
    }

    fn drawn(style: u32) -> Vec<u32> {
        let mut painter = painter();
        let (mut buffer, width, height) = pixels(&painter, 3, 1);
        let mut canvas = Canvas::new(&mut buffer, width, height).unwrap();
        let styled = Cell {
            foreground: INK | style,
            ..cell('H')
        };
        painter.draw_row(&mut canvas, 0, &[cell(' '), styled, cell(' ')]);
        buffer
    }

    #[test]
    fn styles_draw_differently() {
        let regular = drawn(0);
        for style in [BOLD, ITALIC, BOLD | ITALIC] {
            assert_ne!(drawn(style), regular, "{style:#x}");
        }
        let ink = |buffer: &[u32]| buffer.iter().filter(|&&pixel| pixel != PAPER).count();
        assert!(ink(&drawn(BOLD)) > ink(&regular));
    }
}
