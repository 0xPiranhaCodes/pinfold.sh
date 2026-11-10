//! Drawing with Metal: one 8-bit glyph atlas and instanced quads (ADR-0022).

use std::collections::HashMap;

use pinfold_proto::{Cell, DEFAULT_BACKGROUND, Frame};
use pinfold_sys::metal::{Gpu, Instance};
use raw_window_handle::HasWindowHandle;
use swash::scale::ScaleContext;

use crate::cpu::{RenderError, Renderer, cells_with_cursor};
use crate::font::{Font, Fonts};
use crate::glyphs::{Metrics, clamp_size, rasterize};
use crate::padding::Padding;
use crate::paint::{bleeds, is_cap};

/// The atlas is 1 MB of 8-bit coverage, the same cap as the CPU renderer's glyph cache.
const ATLAS_SIZE: usize = 1000;
/// Empty pixels around each glyph, so neighbours cannot bleed into each other.
const PADDING: usize = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    left: i32,
    top: i32,
}

/// Places rectangles in rows, left to right.
#[derive(Debug)]
struct Shelves {
    size: usize,
    x: usize,
    y: usize,
    row_height: usize,
}

impl Shelves {
    fn new(size: usize) -> Self {
        Self {
            size,
            x: 0,
            y: 0,
            row_height: 0,
        }
    }

    /// Returns a free position, or nothing if the atlas is full.
    fn place(&mut self, width: usize, height: usize) -> Option<(usize, usize)> {
        if self.x + width + PADDING > self.size {
            (self.x, self.y) = (0, self.y + self.row_height + PADDING);
            self.row_height = 0;
        }
        if self.x + width + PADDING > self.size || self.y + height + PADDING > self.size {
            return None;
        }
        let position = (self.x, self.y);
        self.x += width + PADDING;
        self.row_height = self.row_height.max(height);
        Some(position)
    }
}

// A glass cell goes to the GPU as it is.
const _: () = assert!(pinfold_proto::GLASS == pinfold_sys::metal::GLASS);

/// Turns cells into quads inside `padding`. `slot` finds each glyph, and `clear` is the
/// frame's background, which needs no quad.
fn instances(
    cells: &[Cell],
    columns: usize,
    metrics: Metrics,
    padding: Padding,
    clear: u32,
    mut slot: impl FnMut(char, u32) -> Option<Slot>,
    out: &mut Vec<Instance>,
) {
    out.clear();
    let (width, height) = (metrics.cell_width as f32, metrics.cell_height as f32);
    let position = |index: usize| {
        let (row, column) = (index / columns.max(1), index % columns.max(1));
        (
            padding.left as f32 + column as f32 * width,
            padding.row_top(row, metrics.cell_height) as f32,
        )
    };
    let tabs = padding.tab_row.map_or(0..0, |row| {
        let start = (row * columns).min(cells.len());
        start..(start + columns).min(cells.len())
    });
    let bleed = metrics.bleed() as f32;
    // Backgrounds go first, so a wide glyph is not covered by the cell after it.
    for (index, cell) in cells.iter().enumerate() {
        if cell.background != clear {
            let (x, y) = position(index);
            out.push(Instance {
                position: [x, y],
                size: [width, height],
                atlas: [0.0, 0.0],
                colour: cell.background,
                textured: 0,
            });
        }
    }
    for (offset, colour) in bleeds(&cells[tabs.clone()], clear).enumerate() {
        if colour != clear {
            let (x, y) = position(tabs.start + offset);
            for top in [y - bleed, y + height] {
                out.push(Instance {
                    position: [x, top],
                    size: [width, bleed],
                    atlas: [0.0, 0.0],
                    colour,
                    textured: 0,
                });
            }
        }
    }
    for (index, cell) in cells.iter().enumerate() {
        if cell.character == ' ' {
            continue;
        }
        let Some(glyph) = slot(cell.character, cell.style()).filter(|glyph| glyph.width > 0) else {
            continue;
        };
        let (x, y) = position(index);
        let drop = if tabs.contains(&index) && !is_cap(cell.character) {
            metrics.tab_text_drop
        } else {
            0
        };
        out.push(Instance {
            position: [
                x + glyph.left as f32,
                y + (metrics.baseline + drop - glyph.top) as f32,
            ],
            size: [glyph.width as f32, glyph.height as f32],
            atlas: [glyph.x as f32, glyph.y as f32],
            colour: cell.foreground,
            textured: 1,
        });
    }
}

/// Draws cells with Metal, the macOS default.
pub struct MetalRenderer {
    gpu: Gpu,
    fonts: Fonts,
    size: f32,
    metrics: Metrics,
    padding: Padding,
    context: ScaleContext,
    shelves: Shelves,
    slots: HashMap<(char, u32), Slot>,
    cells: Vec<Cell>,
    instances: Vec<Instance>,
    /// The theme's background, which is the clear and needs no quad.
    background: u32,
    cursor: Option<(u32, u32)>,
}

impl MetalRenderer {
    /// Creates a renderer. `size` is the font size in physical pixels. Returns nothing if
    /// Metal is unavailable.
    pub fn new(window: &impl HasWindowHandle, font: Font, size: f32) -> Option<Self> {
        Some(Self::with_gpu(Gpu::new(window, ATLAS_SIZE)?, font, size))
    }

    fn with_gpu(gpu: Gpu, font: Font, size: f32) -> Self {
        let size = clamp_size(size);
        let metrics = Metrics::new(&font, size);
        Self {
            gpu,
            fonts: font.into(),
            size,
            metrics,
            padding: Padding::default(),
            context: ScaleContext::new(),
            shelves: Shelves::new(ATLAS_SIZE),
            slots: HashMap::new(),
            cells: Vec::new(),
            instances: Vec::new(),
            background: DEFAULT_BACKGROUND,
            cursor: None,
        }
    }
}

impl Renderer for MetalRenderer {
    fn title_center(&self) -> f64 {
        self.metrics.title_center()
    }

    fn cell_size(&self) -> (usize, usize) {
        (self.metrics.cell_width, self.metrics.cell_height)
    }

    fn resize(&mut self, width: u32, height: u32, padding: Padding) {
        self.gpu.resize(width, height);
        self.padding = padding;
    }

    fn set_font_size(&mut self, size: f32) {
        self.size = clamp_size(size);
        self.metrics = Metrics::new(self.fonts.primary(), self.size);
        // The atlas keeps its size, and new glyphs overwrite the old ones.
        self.slots = HashMap::new();
        self.shelves = Shelves::new(ATLAS_SIZE);
    }

    fn set_font(&mut self, font: Font) {
        self.fonts = font.into();
        self.set_font_size(self.size);
    }

    fn set_background_opacity(&mut self, opacity: f32) -> bool {
        self.gpu.set_opacity(opacity)
    }

    fn set_colours(&mut self, background: u32, cursor: Option<(u32, u32)>) {
        (self.background, self.cursor) = (background, cursor);
    }

    fn draw(&mut self, frame: &Frame) -> Result<(), RenderError> {
        cells_with_cursor(frame, &mut self.cells, self.cursor);
        let metrics = self.metrics;
        let Self {
            gpu,
            fonts,
            size,
            context,
            shelves,
            slots,
            ..
        } = self;
        let slot = |character: char, style: u32| {
            let key = (character, style);
            if let Some(slot) = slots.get(&key) {
                return Some(*slot);
            }
            let glyph = rasterize(context, fonts, *size, metrics, key);
            let height = glyph.mask.len().checked_div(glyph.width).unwrap_or(0);
            let (x, y) = shelves.place(glyph.width, height).or_else(|| {
                // shortcut: a full atlas restarts, so a frame over its capacity glitches: 2,000
                // ASCII-sized glyphs, or 1,300 wide ones at 26 px. Grow it if CJK users see that.
                slots.clear();
                *shelves = Shelves::new(ATLAS_SIZE);
                shelves.place(glyph.width, height)
            })?;
            gpu.upload(x, y, glyph.width, &glyph.mask);
            let slot = Slot {
                x,
                y,
                width: glyph.width,
                height,
                left: glyph.left,
                top: glyph.top,
            };
            slots.insert(key, slot);
            Some(slot)
        };
        let columns = usize::from(frame.columns);
        instances(
            &self.cells,
            columns,
            metrics,
            self.padding,
            self.background,
            slot,
            &mut self.instances,
        );
        self.fonts.release_pages();
        self.gpu.draw(self.background, &self.instances);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use pinfold_proto::{BOLD, ITALIC};

    use super::*;

    const PAPER: u32 = 0x0010_2030;
    const METRICS: Metrics = Metrics {
        cell_width: 10,
        cell_height: 20,
        tab_text_drop: 0,
        baseline: 16,
    };

    fn cell(character: char, background: u32) -> Cell {
        Cell {
            character,
            foreground: 0x00ff_ffff,
            background,
        }
    }

    fn slot(_: char, _: u32) -> Option<Slot> {
        Some(Slot {
            x: 5,
            y: 6,
            width: 7,
            height: 9,
            left: 1,
            top: 10,
        })
    }

    fn quads(cells: &[Cell], columns: usize) -> Vec<Instance> {
        let mut out = Vec::new();
        let padding = Padding::default();
        instances(cells, columns, METRICS, padding, PAPER, slot, &mut out);
        out
    }

    #[test]
    fn blank_cells_need_no_quads() {
        assert!(quads(&[cell(' ', PAPER); 4], 2).is_empty());
    }

    #[test]
    fn a_glyph_sits_on_the_baseline_of_its_cell() {
        let out = quads(
            &[
                cell(' ', PAPER),
                cell(' ', PAPER),
                cell(' ', PAPER),
                cell('x', PAPER),
            ],
            2,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].position, [11.0, 26.0]);
        assert_eq!(out[0].size, [7.0, 9.0]);
        assert_eq!(out[0].atlas, [5.0, 6.0]);
        assert_eq!(out[0].textured, 1);
    }

    #[test]
    fn backgrounds_are_drawn_before_every_glyph() {
        let out = quads(&[cell('a', PAPER), cell(' ', 0x00ff_0000)], 2);
        assert_eq!(
            out.iter().map(|quad| quad.textured).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(out[0].colour, 0x00ff_0000);
        assert_eq!(out[0].position, [10.0, 0.0]);
    }

    #[test]
    fn a_tabs_pill_gets_strips_and_its_text_drops() {
        let metrics = Metrics {
            tab_text_drop: 2,
            ..METRICS
        };
        let padding = Padding {
            tab_row: Some(1),
            ..Padding::default()
        };
        let cells = [cell(' ', PAPER), cell('x', 0x00ff_0000), cell('y', PAPER)];
        let slot = |_, _| {
            Some(Slot {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
                left: 0,
                top: 0,
            })
        };
        let mut out = Vec::new();
        instances(&cells, 1, metrics, padding, PAPER, slot, &mut out);
        let strips: Vec<_> = out.iter().filter(|quad| quad.size[1] == 4.0).collect();
        assert_eq!(strips.iter().filter(|quad| quad.textured == 0).count(), 2);
        let glyphs: Vec<_> = out.iter().filter(|quad| quad.textured == 1).collect();
        let tops: Vec<_> = glyphs.iter().map(|quad| quad.position[1]).collect();
        let baseline = METRICS.baseline as f32;
        assert_eq!(tops, [20.0 + baseline + 2.0, 40.0 + baseline]);
    }

    #[test]
    fn padding_moves_every_quad() {
        let mut out = Vec::new();
        let padding = Padding {
            left: 14,
            top: 12,
            right: 14,
            bottom: 10,
            gap: 0,
            ..Padding::default()
        };
        let cells = [cell(' ', PAPER), cell('x', 0x00ff_0000)];
        instances(&cells, 2, METRICS, padding, PAPER, slot, &mut out);
        assert_eq!(out[0].position, [24.0, 12.0]);
        assert_eq!(out[1].position, [25.0, 18.0]);
    }

    #[test]
    fn a_coloured_first_cell_does_not_become_the_background() {
        let Some(mut renderer) = renderer() else {
            eprintln!("skipped: no Metal device");
            return;
        };
        renderer.resize(100, 100, Padding::default());
        let cells = vec![cell(' ', 0x00ff_0000), cell(' ', DEFAULT_BACKGROUND)];
        renderer.draw(&frame(2, 1, cells)).unwrap();
        let colours: Vec<u32> = renderer.instances.iter().map(|quad| quad.colour).collect();
        assert_eq!(colours, [0x00ff_0000]);
    }

    /// ADR-0049: the glass shows through a theme's background as it does through the default,
    /// and a cursor with a colour of its own is drawn in it.
    #[test]
    fn a_themes_background_needs_no_quad_and_its_cursor_is_drawn() {
        let Some(mut renderer) = renderer() else {
            eprintln!("skipped: no Metal device");
            return;
        };
        renderer.resize(100, 100, Padding::default());
        renderer.set_colours(PAPER, Some((0x00ff_0000, PAPER)));
        let cells = vec![cell(' ', PAPER), cell(' ', DEFAULT_BACKGROUND)];
        let mut screen = frame(2, 1, cells);
        let mut drawn = |screen: &Frame| {
            renderer.draw(screen).unwrap();
            let quads = renderer.instances.iter();
            quads.map(|quad| quad.colour).collect::<Vec<u32>>()
        };
        assert_eq!(drawn(&screen), [DEFAULT_BACKGROUND]);
        screen.cursor = Some((0, 0));
        assert_eq!(drawn(&screen), [0x00ff_0000, DEFAULT_BACKGROUND]);
        // A pane's OSC 11 background goes to the GPU as glass.
        screen.cells[1].background |= pinfold_proto::GLASS;
        let glass = DEFAULT_BACKGROUND | pinfold_proto::GLASS;
        assert_eq!(drawn(&screen), [0x00ff_0000, glass]);
    }

    #[test]
    fn empty_glyphs_are_skipped() {
        let mut out = Vec::new();
        let none = |_, _| {
            Some(Slot {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                left: 0,
                top: 0,
            })
        };
        let padding = Padding::default();
        instances(
            &[cell('x', PAPER)],
            1,
            METRICS,
            padding,
            PAPER,
            none,
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_quad_asks_for_its_cells_style() {
        let mut asked = Vec::new();
        let mut out = Vec::new();
        let bold = Cell {
            foreground: 0x00ff_ffff | BOLD,
            ..cell('x', PAPER)
        };
        let record = |character, style| {
            asked.push((character, style));
            slot(character, style)
        };
        let cells = [cell('x', PAPER), bold];
        let padding = Padding::default();
        instances(&cells, 2, METRICS, padding, PAPER, record, &mut out);
        assert_eq!(asked, [('x', 0), ('x', BOLD)]);
    }

    fn renderer() -> Option<MetalRenderer> {
        let gpu = Gpu::offscreen(ATLAS_SIZE)?;
        Some(MetalRenderer::with_gpu(gpu, Font::bundled().ok()?, 26.0))
    }

    fn frame(columns: u16, rows: u16, cells: Vec<Cell>) -> Frame {
        Frame {
            columns,
            rows,
            cells,
            ..Frame::default()
        }
    }

    #[test]
    fn each_style_takes_its_own_slot() {
        let Some(mut renderer) = renderer() else {
            eprintln!("skipped: no Metal device");
            return;
        };
        renderer.resize(100, 100, Padding::default());
        let styled = |style| Cell {
            foreground: 0x00ff_ffff | style,
            ..cell('x', PAPER)
        };
        let cells = [0, BOLD, ITALIC, BOLD | ITALIC].map(styled).to_vec();
        renderer.draw(&frame(4, 1, cells)).unwrap();
        assert_eq!(renderer.slots.len(), 4);
        let places: Vec<_> = renderer
            .slots
            .values()
            .map(|slot| (slot.x, slot.y))
            .collect();
        assert!((1..4).all(|index| !places[index..].contains(&places[index - 1])));
    }

    fn draw_for_a_while(renderer: &mut MetalRenderer, frame: &Frame) {
        // Enough frames to touch every drawable and let in-flight buffers go.
        for _ in 0..30 {
            renderer.draw(frame).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }

    /// What bold and italic cost on top of regular text at the budget's window size, through
    /// real drawables and a real atlas. Run alone, with `cargo test -p pinfold-render
    /// --release style_cost -- --ignored --nocapture --test-threads 1`.
    #[test]
    #[ignore = "measures this process, so it must run alone"]
    fn metal_style_cost() {
        let footprint = || pinfold_sys::footprint(std::process::id()).unwrap();
        let megabytes = |bytes: u64| bytes as f64 / 1e6;
        let mut renderer = renderer().unwrap();
        renderer.resize(1512, 1890, Padding::default());
        let (cell_width, cell_height) = renderer.cell_size();
        let (columns, rows) = (1512 / cell_width as u16, 1890 / cell_height as u16);
        let screen = |styles: &[u32]| {
            let area = usize::from(columns) * usize::from(rows);
            // Each run of ASCII takes the next style, so every pairing is on screen.
            let styled = styles
                .iter()
                .flat_map(|style| ('!'..='~').map(|c| (c, *style)));
            let cells = styled.cycle().take(area).map(|(character, style)| Cell {
                foreground: 0x00ff_ffff | style,
                ..cell(character, PAPER)
            });
            frame(columns, rows, cells.collect())
        };
        draw_for_a_while(&mut renderer, &screen(&[0]));
        let (regular, regular_slots) = (footprint(), renderer.slots.len());
        let regular_atlas = renderer.shelves.y + renderer.shelves.row_height;
        draw_for_a_while(&mut renderer, &screen(&[0, BOLD, ITALIC, BOLD | ITALIC]));
        println!(
            "Metal at 1512x1890, {columns}x{rows} cells: regular {:.2} MB, {regular_slots} \
             glyphs, {regular_atlas} atlas rows; all four styles +{:.3} MB, {} glyphs, {} \
             atlas rows of {ATLAS_SIZE}",
            megabytes(regular),
            megabytes(footprint().saturating_sub(regular)),
            renderer.slots.len(),
            renderer.shelves.y + renderer.shelves.row_height,
        );
    }

    /// What a translucent background costs at the budget's window size, through real drawables.
    /// The blur is the window server's, so this cannot see it. Run alone, with `cargo test -p
    /// pinfold-render --release glass_cost -- --ignored --nocapture --test-threads 1`.
    #[test]
    #[ignore = "measures this process, so it must run alone"]
    fn metal_glass_cost() {
        let footprint = || pinfold_sys::footprint(std::process::id()).unwrap() as f64 / 1e6;
        let mut renderer = renderer().unwrap();
        renderer.resize(1512, 1890, Padding::scaled(2.0));
        let (cell_width, cell_height) = renderer.cell_size();
        let padding = Padding::scaled(2.0);
        let (rows, columns) = padding.grid(1512, 1890, (cell_width, cell_height));
        let text = ('!'..='~').cycle().map(|c| cell(c, DEFAULT_BACKGROUND));
        let area = usize::from(rows) * usize::from(columns);
        let screen = frame(columns, rows, text.take(area).collect());
        draw_for_a_while(&mut renderer, &screen);
        let opaque = footprint();
        renderer.set_background_opacity(0.85);
        draw_for_a_while(&mut renderer, &screen);
        let glass = footprint();
        println!("Metal at 1512x1890: opaque {opaque:.2} MB, opacity 0.85 {glass:.2} MB");
    }

    #[test]
    fn shelves_never_overlap_and_report_full() {
        let mut shelves = Shelves::new(100);
        let mut placed = Vec::new();
        while let Some(position) = shelves.place(16, 35) {
            placed.push(position);
        }
        assert_eq!(placed.len(), 5 * 2);
        for (index, a) in placed.iter().enumerate() {
            for b in &placed[index + 1..] {
                assert!(a.0.abs_diff(b.0) >= 17 || a.1.abs_diff(b.1) >= 36);
            }
        }
    }

    #[test]
    fn a_glyph_bigger_than_the_atlas_is_refused() {
        assert!(Shelves::new(100).place(200, 10).is_none());
        assert!(Shelves::new(100).place(10, 200).is_none());
    }
}
