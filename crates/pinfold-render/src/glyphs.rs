//! Glyph masks, rasterized once and held in a size-capped least-recently-used cache.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use pinfold_proto::{BOLD, ITALIC};
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Format, Transform};
use unicode_width::UnicodeWidthChar;

use crate::font::{Font, Fonts};

/// One cache shared by every pane (`docs/design/architecture.md`).
pub const CACHE_LIMIT: usize = 1_000_000;

const MIN_SIZE: f32 = 4.0;
const MAX_SIZE: f32 = 256.0;

/// Counted for each entry besides its mask, as an estimate of the map's own cost.
const ENTRY_OVERHEAD: usize = 64;

/// How much wider synthetic bold makes a stem, in ems, which is FreeType's default. Synthetic
/// styles need no font file, and ADR-0026 bundles one face.
const BOLD_GROWTH: f32 = 1.0 / 24.0;
/// How far synthetic italic leans, as the tangent of 12 degrees.
const ITALIC_SLANT: f32 = 0.2126;

/// How many times its cells a primary glyph may measure, which bounds a damaged font's masks.
const MAX_OVERHANG: usize = 4;
/// The size a chosen font is tried at, in pixels.
const TRIAL_SIZE: usize = 32;

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

/// Runs `work`, which reads a font. Swash can panic on a corrupt one, and that must not take
/// a shell with it.
pub(crate) fn contained<T>(work: impl FnOnce() -> Option<T>) -> Option<T> {
    catch_unwind(AssertUnwindSafe(work)).ok().flatten()
}

/// What a style does to an outline: how far to embolden it, then how to place it. Swash grows
/// an outline up and to the right by twice the strength, so the shift centres bold on the old
/// stem. Italic leans around the middle of the cell.
fn synthetic(style: u32, size: f32, metrics: Metrics) -> (f32, Transform) {
    let bold = if style & BOLD == 0 {
        0.0
    } else {
        size * BOLD_GROWTH / 2.0
    };
    let slant = if style & ITALIC == 0 {
        0.0
    } else {
        ITALIC_SLANT
    };
    let middle = metrics.baseline as f32 - metrics.cell_height as f32 / 2.0;
    let across = -bold - slant * middle;
    (bold, Transform::new(1.0, 0.0, slant, 1.0, across, -bold))
}

fn render(
    context: &mut ScaleContext,
    font: &Font,
    size: f32,
    hint: bool,
    id: u16,
    style: u32,
    metrics: Metrics,
) -> Option<Glyph> {
    let (bold, transform) = synthetic(style, size, metrics);
    let mut scaler = context.builder(font.face()).size(size).hint(hint).build();
    let image = Render::new(&[Source::Outline])
        .format(Format::Alpha)
        .embolden(bold)
        .transform(Some(transform))
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
    style: u32,
    metrics: Metrics,
) -> Option<(f32, f32)> {
    let (bold, transform) = synthetic(style, size, metrics);
    let mut scaler = context.builder(font.face()).size(size).hint(hint).build();
    let mut outline = scaler.scale_outline(id)?;
    outline.embolden(bold, bold);
    outline.transform(&transform);
    let bounds = outline.bounds();
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
    style: u32,
) -> Option<Glyph> {
    let (width, height) = (span * metrics.cell_width, metrics.cell_height);
    let fits = |(wide, tall): (f32, f32)| wide <= width as f32 && tall <= height as f32;
    let (size, hint) = if fits(extent(context, font, size, true, id, style, metrics)?) {
        (size, true)
    } else {
        let (wide, tall) = extent(context, font, size, false, id, style, metrics)?;
        // One pixel spare, as the smaller outline may straddle one more.
        let shrink = ((width - 1).max(1) as f32 / wide).min((height - 1).max(1) as f32 / tall);
        let size = size * shrink.min(1.0);
        // A corrupt `head` can stop outlines scaling at all.
        let shrunk = extent(context, font, size, false, id, style, metrics)?;
        fits(shrunk).then_some((size, false))?
    };
    let mut glyph = render(context, font, size, hint, id, style, metrics)?;
    let advance = font.face().glyph_metrics(&[]).scale(size).advance_width(id);
    let rows = glyph.mask.len().checked_div(glyph.width).unwrap_or(0) as i32;
    let centred = glyph.left + ((width as f32 - advance) / 2.0) as i32;
    glyph.left = centred.clamp(0, (width as i32 - glyph.width as i32).max(0));
    let above = (metrics.baseline - glyph.top).clamp(0, (height as i32 - rows).max(0));
    glyph.top = metrics.baseline - above;
    Some(glyph)
}

/// Renders a glyph of the primary font where the font puts it, once its size is known to be
/// sane.
fn placed(
    context: &mut ScaleContext,
    (font, id): (&Font, u16),
    size: f32,
    metrics: Metrics,
    span: usize,
    style: u32,
) -> Option<Glyph> {
    let (wide, tall) = extent(context, font, size, true, id, style, metrics)?;
    let width = MAX_OVERHANG * span * metrics.cell_width;
    let height = MAX_OVERHANG * metrics.cell_height;
    (wide <= width as f32 && tall <= height as f32).then_some(())?;
    render(context, font, size, true, id, style, metrics)
}

/// Whether a font has what the grid needs: cells of a sane shape, and ink for all of ASCII.
/// That rules out a damaged font, a subset and an emoji font with no outlines.
pub(crate) fn can_draw_text(font: &Font) -> bool {
    let size = TRIAL_SIZE as f32;
    let metrics = Metrics::new(font, size);
    let mut context = ScaleContext::new();
    metrics.cell_width <= 2 * TRIAL_SIZE
        && metrics.cell_height <= 4 * TRIAL_SIZE
        && ('!'..='~').all(|character| {
            let found = (font, font.glyph(character));
            let glyph = placed(&mut context, found, size, metrics, 1, 0);
            found.1 != 0 && glyph.is_some_and(|glyph| !glyph.mask.is_empty())
        })
}

/// Rasterizes one character. A character with no outline, like a space, gives an empty mask,
/// and one that no usable font has gives the primary font's box.
pub(crate) fn rasterize(
    context: &mut ScaleContext,
    fonts: &mut Fonts,
    size: f32,
    metrics: Metrics,
    (character, style): (char, u32),
) -> Glyph {
    let span = cell_span(character);
    let id = contained(|| Some(fonts.primary().glyph(character))).unwrap_or(0);
    if id == 0 {
        let fallback = contained(|| {
            let found = fonts.fallback(character)?;
            fitted(context, found, size, metrics, span, style)
        });
        if let Some(glyph) = fallback {
            return glyph;
        }
    }
    let found = (fonts.primary(), id);
    contained(|| placed(context, found, size, metrics, span, style)).unwrap_or(Glyph {
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
    glyphs: HashMap<(char, u32), Glyph>,
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
        self.glyphs.contains_key(&(character, 0))
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

    /// Returns the mask for `character` in `style`, as [`pinfold_proto::Cell::style`] gives
    /// it, rasterizing it on first use.
    pub fn get(&mut self, character: char, style: u32) -> Option<&Glyph> {
        self.clock += 1;
        let key = (character, style);
        if !self.glyphs.contains_key(&key) {
            let glyph = rasterize(
                &mut self.context,
                &mut self.fonts,
                self.size,
                self.metrics,
                key,
            );
            if glyph.cost() > self.limit {
                return None;
            }
            self.evict_until_fits(glyph.cost());
            self.used += glyph.cost();
            self.glyphs.insert(key, glyph);
        }
        let glyph = self.glyphs.get_mut(&key)?;
        glyph.last_used = self.clock;
        Some(glyph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::fixture::{File, KANJI, kanji_font, plain_font, table};

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
        cache.get(KANJI, 0).unwrap().mask.clone()
    }

    #[test]
    fn text_the_font_has_never_tries_a_fallback() {
        let mut cache = with_fallback(&["/does/not/exist"]);
        let box_drawing = '─'..='╿';
        for character in ('!'..='~').chain(box_drawing).chain("◆●✓✕○".chars()) {
            cache.get(character, 0);
            assert_eq!(cache.fonts.untried(), 1, "{character} tried a fallback");
        }
    }

    #[test]
    fn a_missing_character_comes_from_the_fallback() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let mut cache = with_fallback(file.list);
        assert_eq!(cache.fonts.untried(), 1);
        assert_ne!(cache.get(KANJI, 0).unwrap().mask, the_box());
        assert_eq!(cache.fonts.untried(), 0);
    }

    #[test]
    fn a_corrupt_fallback_draws_the_box() {
        let font = kanji_font().unwrap();
        for bytes in [&b"not a font"[..], &font[..12], &font[..4096]] {
            let file = File::new(bytes).unwrap();
            let mut cache = with_fallback(file.list);
            assert_eq!(cache.get(KANJI, 0).unwrap().mask, the_box());
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
                let glyph = cache.get(character, 0).unwrap();
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
        let glyph = cache.get(KANJI, 0).unwrap();
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
        let glyph = rasterize(&mut context, &mut fonts, 64.0, metrics, (KANJI, 0));
        assert!(glyph.width > 0);
        assert!(inside(&glyph, metrics, 2));
    }

    /// Real fonts, on machines that have them: `FALLBACKS` is what a user gets.
    #[test]
    fn system_fonts_draw_cjk_if_installed() {
        let mut cache = GlyphCache::new(bundled(), 26.0, CACHE_LIMIT);
        let metrics = cache.metrics;
        let the_box = cache.get('\u{10ffff}', 0).unwrap().mask.clone();
        if cache.fonts.fallback('日').is_none() {
            eprintln!("skipped: no CJK font at the known paths");
            return;
        }
        for text in ['日', 'あ', '汉', '한'] {
            let glyph = cache.get(text, 0).unwrap();
            assert_ne!(glyph.mask, the_box, "{text} drew a box");
            assert!(inside(glyph, metrics, 2), "{text} left its cells");
            assert!(glyph.width > metrics.cell_width, "{text} is too narrow");
        }
        assert!(cache.get('\u{3000}', 0).unwrap().mask.is_empty());
    }

    #[test]
    fn each_style_is_its_own_entry() {
        let mut cache = GlyphCache::new(bundled(), SIZE, CACHE_LIMIT);
        cache.get('H', 0);
        let regular = cache.used();
        let mask = |cache: &mut GlyphCache, style| cache.get('H', style).unwrap().mask.clone();
        let masks = [BOLD, ITALIC, BOLD | ITALIC].map(|style| mask(&mut cache, style));
        assert!(cache.used() > 3 * regular);
        let used = cache.used();
        assert_ne!(masks[0], masks[1]);
        assert_ne!(masks[0], masks[2]);
        assert_ne!(masks[1], masks[2]);
        assert_eq!(mask(&mut cache, BOLD), masks[0]);
        assert_eq!(cache.used(), used);
    }

    #[test]
    fn bold_widens_stems_and_italic_leans_right() {
        let mut cache = GlyphCache::new(bundled(), 64.0, CACHE_LIMIT);
        // The first and last inked column of a row, counted from the top.
        let ink = |glyph: &Glyph, row: usize| {
            let line = glyph.mask.chunks(glyph.width).nth(row).unwrap();
            let start = line.iter().position(|&alpha| alpha > 127).unwrap();
            let end = line.iter().rposition(|&alpha| alpha > 127).unwrap();
            (glyph.left + start as i32, glyph.left + end as i32)
        };
        let (left, right) = ink(cache.get('l', 0).unwrap(), 8);
        let (bold_left, bold_right) = ink(cache.get('l', BOLD).unwrap(), 8);
        assert!(bold_right - bold_left > right - left);
        let italic = cache.get('H', ITALIC).unwrap();
        let rows = italic.mask.len() / italic.width;
        assert!(ink(italic, 0).0 > ink(italic, rows - 1).0);
    }

    #[test]
    fn styles_stay_centred_on_the_regular_glyph() {
        let mut cache = GlyphCache::new(bundled(), 64.0, CACHE_LIMIT);
        let middle = |glyph: &Glyph| 2 * glyph.left + glyph.width as i32;
        let regular = middle(cache.get('H', 0).unwrap());
        for style in [BOLD, ITALIC, BOLD | ITALIC] {
            let styled = middle(cache.get('H', style).unwrap());
            assert!(
                (styled - regular).abs() <= 4,
                "{style:#x}: {styled} {regular}"
            );
        }
    }

    #[test]
    fn a_styled_fallback_stays_in_its_cells() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let mut cache = with_fallback(file.list);
        let metrics = cache.metrics;
        for style in [BOLD, ITALIC, BOLD | ITALIC] {
            assert!(inside(cache.get(KANJI, style).unwrap(), metrics, 2));
        }
    }

    #[test]
    fn a_panic_while_reading_a_font_is_contained() {
        assert_eq!(contained(|| -> Option<u8> { panic!("corrupt glyf") }), None);
        assert_eq!(contained(|| Some(1)), Some(1));
    }

    #[test]
    fn a_damaged_chosen_font_never_panics() {
        let font = plain_font();
        // A fixed xorshift, so a failure can be replayed.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let glyf = table(&font, b"glyf").unwrap();
        let mut opened = 0;
        for round in 0..100 {
            // Odd rounds spare the headers, so the damage is found only while rendering.
            let (start, hits) = if round % 2 == 0 {
                (0, 64)
            } else {
                (glyf, 4096)
            };
            let mut damaged = font.clone();
            for _ in 0..hits {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let at = start + state as usize % (damaged.len() - start);
                damaged[at] = (state >> 56) as u8;
            }
            let file = File::new(&damaged).unwrap();
            let chosen = Font::chosen(Some(std::path::Path::new(file.list[0]))).unwrap();
            opened += usize::from(!chosen.is_bundled());
            let mut cache = GlyphCache::new(Fonts::with_fallbacks(chosen, &[]), SIZE, CACHE_LIMIT);
            let (cell_width, cell_height) = (cache.metrics.cell_width, cache.metrics.cell_height);
            for character in ('!'..='~').chain(KANJI..='\u{65ff}') {
                let glyph = cache.get(character, BOLD | ITALIC).unwrap();
                let rows = glyph.mask.len().checked_div(glyph.width).unwrap_or(0);
                assert!(glyph.width <= MAX_OVERHANG * 2 * cell_width + 1);
                assert!(rows <= MAX_OVERHANG * cell_height + 1);
            }
        }
        println!("{opened} of 100 damaged fonts opened");
        assert!(opened > 0);
    }

    /// What bold and italic cost on top of regular text, for the PR's memory section. Metal
    /// keeps masks in its atlas, so `metal_style_cost` covers it. Run alone, with `cargo test
    /// -p pinfold-render --release style_cost -- --ignored --nocapture --test-threads 1`.
    #[test]
    #[ignore = "measures this process, so it must run alone"]
    fn style_cost() {
        let footprint = || pinfold_sys::footprint(std::process::id()).unwrap();
        let megabytes = |bytes: u64| bytes as f64 / 1e6;
        let mut cache = GlyphCache::new(bundled(), 26.0, CACHE_LIMIT);
        let fill = |cache: &mut GlyphCache, style| {
            for character in '!'..='~' {
                cache.get(character, style);
            }
        };
        fill(&mut cache, 0);
        let (regular, regular_used) = (footprint(), cache.used());
        for style in [BOLD, ITALIC, BOLD | ITALIC] {
            fill(&mut cache, style);
        }
        println!(
            "CPU cache, ASCII at 26 px: regular {:.2} MB with {regular_used} bytes cached, all \
             four styles +{:.3} MB with {} bytes cached",
            megabytes(regular),
            megabytes(footprint().saturating_sub(regular)),
            cache.used(),
        );
        assert!(cache.used() <= CACHE_LIMIT);
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
                cache.get(character, 0);
            }
            let ascii = footprint();
            cache.get('日', 0);
            let first = footprint();
            for character in ('一'..='鿿').take(4000) {
                cache.get(character, 0);
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
