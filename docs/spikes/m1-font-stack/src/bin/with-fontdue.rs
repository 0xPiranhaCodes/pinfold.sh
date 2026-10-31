//! Usage: with-fontdue <font> <index> <pixels> [image]

use std::env;
use std::time::Instant;

use fontdue::{Font, FontSettings};
use spike_font_stack::{SAMPLE, Sheet, footprint, glyphs, map};

fn main() {
    let args: Vec<String> = env::args().collect();
    let size: f32 = args[3].parse().expect("size");
    let bytes = map(&args[1]);
    let before = footprint();
    let settings = FontSettings { collection_index: args[2].parse().expect("index"), ..FontSettings::default() };
    let font = Font::from_bytes(bytes, settings).expect("font");
    let loaded = footprint();
    let (start, mut mask_bytes) = (Instant::now(), 0);
    let mut masks = Vec::new();
    for glyph in glyphs() {
        let (metrics, mask) = font.rasterize(glyph, size);
        mask_bytes += mask.len();
        masks.push((metrics, mask));
    }
    let elapsed = start.elapsed().as_secs_f64() * 1e6 / glyphs().len() as f64;
    println!("{:.2} {:.2} {:.2} {} {elapsed:.0}", before, loaded - before, footprint() - before, mask_bytes);
    if let Some(path) = args.get(4) {
        let (cell, height) = (font.rasterize('M', size).0.advance_width.round() as i32, (size * 1.4) as i32);
        let mut sheet = Sheet::new(SAMPLE.len() * cell as usize + 8, height as usize + 8);
        for (index, glyph) in SAMPLE.chars().enumerate() {
            let (metrics, mask) = font.rasterize(glyph, size);
            let top = height - (size * 0.3) as i32 - metrics.height as i32 - metrics.ymin;
            sheet.blit(4 + index as i32 * cell + metrics.xmin, 4 + top, metrics.width, &mask);
        }
        sheet.save(path);
    }
}
