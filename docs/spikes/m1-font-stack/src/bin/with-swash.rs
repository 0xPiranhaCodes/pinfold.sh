//! Usage: with-swash <font> <index> <pixels> [image]

use std::env;
use std::time::Instant;

use spike_font_stack::{SAMPLE, Sheet, footprint, glyphs, map};
use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::Format;

fn main() {
    let args: Vec<String> = env::args().collect();
    let size: f32 = args[3].parse().expect("size");
    let bytes = map(&args[1]);
    let before = footprint();
    let font = FontRef::from_index(bytes, args[2].parse().expect("index")).expect("font");
    let loaded = footprint();
    let mut context = ScaleContext::new();
    let mut scaler = context.builder(font).size(size).hint(true).build();
    let mut render = Render::new(&[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline]);
    render.format(Format::Alpha);
    let charmap = font.charmap();
    let (start, mut mask_bytes) = (Instant::now(), 0);
    let mut masks = Vec::new();
    for glyph in glyphs() {
        if let Some(image) = render.render(&mut scaler, charmap.map(glyph)) {
            mask_bytes += image.data.len();
            masks.push(image);
        }
    }
    let elapsed = start.elapsed().as_secs_f64() * 1e6 / glyphs().len() as f64;
    println!("{:.2} {:.2} {:.2} {} {elapsed:.0}", before, loaded - before, footprint() - before, mask_bytes);
    if let Some(path) = args.get(4) {
        let metrics = font.metrics(&[]).scale(size);
        let cell = font.glyph_metrics(&[]).scale(size).advance_width(charmap.map('M')).round() as i32;
        let height = (metrics.ascent + metrics.descent + metrics.leading).ceil() as i32;
        let mut sheet = Sheet::new(SAMPLE.len() * cell as usize + 8, height as usize + 8);
        for (index, glyph) in SAMPLE.chars().enumerate() {
            if let Some(image) = render.render(&mut scaler, charmap.map(glyph)) {
                let top = metrics.ascent as i32 - image.placement.top;
                sheet.blit(4 + index as i32 * cell + image.placement.left, 4 + top, image.placement.width as usize, &image.data);
            }
        }
        sheet.save(path);
    }
}
