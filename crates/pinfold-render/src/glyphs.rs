//! Glyph masks, rasterized once and held in a size-capped least-recently-used cache.

use std::collections::HashMap;

use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::Format;

use crate::font::Font;

/// One cache shared by every pane (`docs/design/architecture.md`).
pub const CACHE_LIMIT: usize = 1_000_000;

const MIN_SIZE: f32 = 4.0;
const MAX_SIZE: f32 = 256.0;

/// Counted for each entry besides its mask, as an estimate of the map's own cost.
const ENTRY_OVERHEAD: usize = 64;

/// An 8-bit alpha mask, placed relative to the pen on the baseline.
#[derive(Debug)]
pub struct Glyph {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: usize,
    pub(crate) mask: Box<[u8]>,
    last_used: u64,
}

impl Glyph {
    fn cost(&self) -> usize {
        self.mask.len() + ENTRY_OVERHEAD
    }
}

/// Rasterizes one character. A character with no outline, like a space, gives an empty mask.
pub(crate) fn rasterize(
    context: &mut ScaleContext,
    font: &Font,
    size: f32,
    character: char,
) -> Glyph {
    let face = font.face();
    let mut scaler = context.builder(face).size(size).hint(true).build();
    let id = face.charmap().map(character);
    let image = Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .render(&mut scaler, id);
    match image {
        Some(image) => Glyph {
            left: image.placement.left,
            top: image.placement.top,
            width: image.placement.width as usize,
            mask: image.data.into_boxed_slice(),
            last_used: 0,
        },
        None => Glyph {
            left: 0,
            top: 0,
            width: 0,
            mask: Box::default(),
            last_used: 0,
        },
    }
}

/// The size of a cell and where text sits in it, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Metrics {
    pub cell_width: usize,
    pub cell_height: usize,
    pub baseline: i32,
}

impl Metrics {
    pub(crate) fn new(font: &Font, size: f32) -> Self {
        let face = font.face();
        let metrics = face.metrics(&[]).scale(size);
        let advance = face
            .glyph_metrics(&[])
            .scale(size)
            .advance_width(face.charmap().map('M'));
        Self {
            cell_width: (advance.round() as usize).max(1),
            cell_height: ((metrics.ascent + metrics.descent + metrics.leading).ceil() as usize)
                .max(1),
            baseline: metrics.ascent.round() as i32,
        }
    }
}

/// Clamps a requested font size to a sane range.
pub(crate) fn clamp_size(size: f32) -> f32 {
    if size.is_finite() {
        size.clamp(MIN_SIZE, MAX_SIZE)
    } else {
        MIN_SIZE
    }
}

/// Rasterizes glyphs and keeps the most recently used ones.
pub struct GlyphCache {
    font: Font,
    size: f32,
    limit: usize,
    used: usize,
    clock: u64,
    glyphs: HashMap<char, Glyph>,
    context: ScaleContext,
}

impl GlyphCache {
    /// Creates an empty cache for one font at one size, holding at most `limit` bytes.
    #[must_use]
    pub fn new(font: Font, size: f32, limit: usize) -> Self {
        Self {
            font,
            size,
            limit,
            used: 0,
            clock: 0,
            glyphs: HashMap::new(),
            context: ScaleContext::new(),
        }
    }

    /// Bytes the cache counts as held.
    #[must_use]
    pub fn used(&self) -> usize {
        self.used
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, character: char) -> bool {
        self.glyphs.contains_key(&character)
    }

    fn evict_until_fits(&mut self, incoming: usize) {
        while self.used + incoming > self.limit {
            let oldest = self.glyphs.iter().min_by_key(|(_, glyph)| glyph.last_used);
            let Some((&key, _)) = oldest else { return };
            if let Some(glyph) = self.glyphs.remove(&key) {
                self.used -= glyph.cost();
            }
        }
    }

    /// Returns the mask for `character`, rasterizing it on first use.
    pub fn get(&mut self, character: char) -> Option<&Glyph> {
        self.clock += 1;
        if !self.glyphs.contains_key(&character) {
            let glyph = rasterize(&mut self.context, &self.font, self.size, character);
            if glyph.cost() > self.limit {
                return None;
            }
            self.evict_until_fits(glyph.cost());
            self.used += glyph.cost();
            self.glyphs.insert(character, glyph);
        }
        let glyph = self.glyphs.get_mut(&character)?;
        glyph.last_used = self.clock;
        Some(glyph)
    }
}
