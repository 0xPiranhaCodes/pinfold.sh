//! Glyph masks, rasterized once and held in a size-capped least-recently-used cache.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::Format;
use unicode_width::UnicodeWidthChar;

use crate::font::{Font, Fonts};

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

/// How many grid cells `character` takes.
pub(crate) fn cell_span(character: char) -> usize {
    character.width().unwrap_or(1).max(1)
}

fn render(
    context: &mut ScaleContext,
    font: &Font,
    size: f32,
    hint: bool,
    id: u16,
) -> Option<Glyph> {
    let mut scaler = context.builder(font.face()).size(size).hint(hint).build();
    let image = Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .render(&mut scaler, id)?;
    Some(Glyph {
        left: image.placement.left,
        top: image.placement.top,
        width: image.placement.width as usize,
        mask: image.data.into_boxed_slice(),
        last_used: 0,
    })
}

/// The pixels an outline covers, as `(width, height)`, without rendering it.
fn extent(
    context: &mut ScaleContext,
    font: &Font,
    size: f32,
    hint: bool,
    id: u16,
) -> Option<(f32, f32)> {
    let mut scaler = context.builder(font.face()).size(size).hint(hint).build();
    let bounds = scaler.scale_outline(id)?.bounds();
    let (min, max) = (bounds.min, bounds.max);
    Some((max.x.ceil() - min.x.floor(), max.y.ceil() - min.y.floor()))
}

/// Renders a fallback glyph inside its cells, as its font's metrics are not the primary's.
/// It is measured first, so a corrupt outline cannot ask for a huge mask. Hinting can move
/// any point, so a glyph that must shrink does it unhinted, where scaling is exact.
fn fitted(
    context: &mut ScaleContext,
    (font, id): (&Font, u16),
    size: f32,
    metrics: Metrics,
    span: usize,
) -> Option<Glyph> {
    let (width, height) = (span * metrics.cell_width, metrics.cell_height);
    let fits = |(wide, tall): (f32, f32)| wide <= width as f32 && tall <= height as f32;
    let (size, hint) = if fits(extent(context, font, size, true, id)?) {
        (size, true)
    } else {
        let (wide, tall) = extent(context, font, size, false, id)?;
        // One pixel spare, as the smaller outline may straddle one more.
        let shrink = ((width - 1).max(1) as f32 / wide).min((height - 1).max(1) as f32 / tall);
        let size = size * shrink.min(1.0);
        // A corrupt `head` can stop outlines scaling at all.
        fits(extent(context, font, size, false, id)?).then_some((size, false))?
    };
    let mut glyph = render(context, font, size, hint, id)?;
    let advance = font.face().glyph_metrics(&[]).scale(size).advance_width(id);
    let rows = glyph.mask.len().checked_div(glyph.width).unwrap_or(0) as i32;
    let centred = glyph.left + ((width as f32 - advance) / 2.0) as i32;
    glyph.left = centred.clamp(0, (width as i32 - glyph.width as i32).max(0));
    let above = (metrics.baseline - glyph.top).clamp(0, (height as i32 - rows).max(0));
    glyph.top = metrics.baseline - above;
    Some(glyph)
}

/// Rasterizes one character. A character with no outline, like a space, gives an empty mask,
/// and one that no usable font has gives the primary font's box.
pub(crate) fn rasterize(
    context: &mut ScaleContext,
    fonts: &mut Fonts,
    size: f32,
    metrics: Metrics,
    character: char,
) -> Glyph {
    let id = fonts.primary().glyph(character);
    if id == 0 {
        let span = cell_span(character);
        // Swash can panic on a corrupt font, and that must not take a shell with it.
        let fallback = catch_unwind(AssertUnwindSafe(|| {
            let found = fonts.fallback(character)?;
            fitted(context, found, size, metrics, span)
        }));
        if let Ok(Some(glyph)) = fallback {
            return glyph;
        }
    }
    render(context, fonts.primary(), size, true, id).unwrap_or(Glyph {
        left: 0,
        top: 0,
        width: 0,
        mask: Box::default(),
        last_used: 0,
    })
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
    fonts: Fonts,
    size: f32,
    metrics: Metrics,
    limit: usize,
    used: usize,
    clock: u64,
    glyphs: HashMap<char, Glyph>,
    context: ScaleContext,
}

impl GlyphCache {
    /// Creates an empty cache for one font at one size, holding at most `limit` bytes.
    #[must_use]
    pub fn new(fonts: impl Into<Fonts>, size: f32, limit: usize) -> Self {
        let fonts = fonts.into();
        Self {
            metrics: Metrics::new(fonts.primary(), size),
            fonts,
            size,
            limit,
            used: 0,
            clock: 0,
            glyphs: HashMap::new(),
            context: ScaleContext::new(),
        }
    }

    /// Switches to `size`, dropping every glyph drawn at the old one.
    pub fn set_size(&mut self, size: f32) {
        self.size = size;
        self.metrics = Metrics::new(self.fonts.primary(), size);
        self.glyphs = HashMap::new();
        self.used = 0;
    }

    pub(crate) fn metrics(&self) -> Metrics {
        self.metrics
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
            let glyph = rasterize(
                &mut self.context,
                &mut self.fonts,
                self.size,
                self.metrics,
                character,
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::fixture::{File, KANJI, kanji_font};

    const SIZE: f32 = 16.0;

    fn bundled() -> Font {
        Font::bundled().unwrap()
    }

    fn with_fallback(list: &'static [&'static str]) -> GlyphCache {
        GlyphCache::new(Fonts::with_fallbacks(bundled(), list), SIZE, CACHE_LIMIT)
    }

    /// The box the bundled font draws for a character it lacks.
    fn the_box() -> Box<[u8]> {
        let mut cache = GlyphCache::new(Fonts::with_fallbacks(bundled(), &[]), SIZE, CACHE_LIMIT);
        cache.get(KANJI).unwrap().mask.clone()
    }

    #[test]
    fn text_the_font_has_never_tries_a_fallback() {
        let mut cache = with_fallback(&["/does/not/exist"]);
        let box_drawing = '─'..='╿';
        for character in ('!'..='~').chain(box_drawing).chain("◆●✓✕○".chars()) {
            cache.get(character);
            assert_eq!(cache.fonts.untried(), 1, "{character} tried a fallback");
        }
    }

    #[test]
    fn a_missing_character_comes_from_the_fallback() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let mut cache = with_fallback(file.list);
        assert_eq!(cache.fonts.untried(), 1);
        assert_ne!(cache.get(KANJI).unwrap().mask, the_box());
        assert_eq!(cache.fonts.untried(), 0);
    }

    #[test]
    fn a_corrupt_fallback_draws_the_box() {
        let font = kanji_font().unwrap();
        for bytes in [&b"not a font"[..], &font[..12], &font[..4096]] {
            let file = File::new(bytes).unwrap();
            assert_eq!(with_fallback(file.list).get(KANJI).unwrap().mask, the_box());
        }
    }

    #[test]
    fn a_damaged_fallback_never_panics() {
        let font = kanji_font().unwrap();
        // A fixed xorshift, so a failure can be replayed.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..300 {
            let mut damaged = font.clone();
            for _ in 0..64 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // The tables at the front decide the most, so half the damage goes there.
                let end = if state & 1 == 0 { 2048 } else { damaged.len() };
                let at = state as usize % end;
                damaged[at] = (state >> 56) as u8;
            }
            let file = File::new(&damaged).unwrap();
            let mut cache = with_fallback(file.list);
            for character in (KANJI..).take(8) {
                let (cell_width, cell_height) =
                    (cache.metrics.cell_width, cache.metrics.cell_height);
                let glyph = cache.get(character).unwrap();
                let rows = glyph.mask.len().checked_div(glyph.width).unwrap_or(0);
                assert!(glyph.width <= 2 * cell_width && rows <= cell_height);
            }
        }
    }

    fn inside(glyph: &Glyph, metrics: Metrics, span: usize) -> bool {
        let rows = glyph.mask.len().checked_div(glyph.width).unwrap_or(0) as i32;
        let above = metrics.baseline - glyph.top;
        glyph.left >= 0
            && glyph.left as usize + glyph.width <= span * metrics.cell_width
            && above >= 0
            && above + rows <= metrics.cell_height as i32
    }

    #[test]
    fn a_wide_glyph_is_centred_across_two_cells() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let mut cache = with_fallback(file.list);
        let metrics = cache.metrics;
        let glyph = cache.get(KANJI).unwrap();
        assert!(inside(glyph, metrics, 2));
        // 'H' is one cell wide, so centring moves it half a cell.
        assert!(glyph.left >= metrics.cell_width as i32 / 2);
    }

    #[test]
    fn a_glyph_too_big_for_its_cells_shrinks() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let mut fonts = Fonts::with_fallbacks(bundled(), file.list);
        let metrics = Metrics {
            cell_width: 3,
            cell_height: 6,
            baseline: 5,
        };
        let mut context = ScaleContext::new();
        let glyph = rasterize(&mut context, &mut fonts, 64.0, metrics, KANJI);
        assert!(glyph.width > 0);
        assert!(inside(&glyph, metrics, 2));
    }

    /// Real fonts, on machines that have them: `FALLBACKS` is what a user gets.
    #[test]
    fn system_fonts_draw_cjk_if_installed() {
        let mut cache = GlyphCache::new(bundled(), 26.0, CACHE_LIMIT);
        let metrics = cache.metrics;
        let the_box = cache.get('\u{10ffff}').unwrap().mask.clone();
        if cache.fonts.fallback('日').is_none() {
            eprintln!("skipped: no CJK font at the known paths");
            return;
        }
        for text in ['日', 'あ', '汉', '한'] {
            let glyph = cache.get(text).unwrap();
            assert_ne!(glyph.mask, the_box, "{text} drew a box");
            assert!(inside(glyph, metrics, 2), "{text} left its cells");
            assert!(glyph.width > metrics.cell_width, "{text} is too narrow");
        }
        assert!(cache.get('\u{3000}').unwrap().mask.is_empty());
    }

    /// What fallback text costs, for the PR's memory section. Metal keeps masks in its atlas,
    /// so a limit of 0 stands for it. Run alone, with `cargo test -p pinfold-render --release
    /// fallback_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measures this process, so it must run alone"]
    fn fallback_cost() {
        let footprint = || pinfold_sys::footprint(std::process::id()).unwrap();
        let megabytes = |bytes: u64| bytes as f64 / 1e6;
        let run = |limit: usize| {
            let mut cache = GlyphCache::new(bundled(), 26.0, limit);
            for character in '!'..='~' {
                cache.get(character);
            }
            let ascii = footprint();
            cache.get('日');
            let first = footprint();
            for character in ('一'..='鿿').take(4000) {
                cache.get(character);
            }
            let full = footprint();
            println!(
                "limit {limit}: ASCII {:.2} MB, first glyph +{:.3} MB, 4,000 glyphs +{:.3} MB, \
                 {} bytes cached",
                megabytes(ascii),
                megabytes(first.saturating_sub(ascii)),
                megabytes(full.saturating_sub(ascii)),
                cache.used(),
            );
            assert!(cache.used() <= limit);
        };
        run(0);
        run(CACHE_LIMIT);
    }
}
