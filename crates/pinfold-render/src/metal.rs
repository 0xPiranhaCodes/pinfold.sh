//! Drawing with Metal: one 8-bit glyph atlas and instanced quads (ADR-0022).

use std::collections::HashMap;

use pinfold_proto::{Cell, Frame};
use pinfold_sys::metal::{Gpu, Instance};
use raw_window_handle::HasWindowHandle;
use swash::scale::ScaleContext;

use crate::cpu::{RenderError, Renderer, cells_with_cursor};
use crate::font::{Font, Fonts};
use crate::glyphs::{Metrics, clamp_size, rasterize};

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

/// Turns cells into quads. `slot` finds each glyph, and `clear` is the frame's background,
/// which needs no quad.
fn instances(
    cells: &[Cell],
    columns: usize,
    metrics: Metrics,
    clear: u32,
    mut slot: impl FnMut(char) -> Option<Slot>,
    out: &mut Vec<Instance>,
) {
    out.clear();
    let (width, height) = (metrics.cell_width as f32, metrics.cell_height as f32);
    let position = |index: usize| {
        let (row, column) = (index / columns.max(1), index % columns.max(1));
        (column as f32 * width, row as f32 * height)
    };
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
    for (index, cell) in cells.iter().enumerate() {
        if cell.character == ' ' {
            continue;
        }
        let Some(glyph) = slot(cell.character).filter(|glyph| glyph.width > 0) else {
            continue;
        };
        let (x, y) = position(index);
        out.push(Instance {
            position: [
                x + glyph.left as f32,
                y + (metrics.baseline - glyph.top) as f32,
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
    context: ScaleContext,
    shelves: Shelves,
    slots: HashMap<char, Slot>,
    cells: Vec<Cell>,
    instances: Vec<Instance>,
}

impl MetalRenderer {
    /// Creates a renderer. `size` is the font size in physical pixels. Returns nothing if
    /// Metal is unavailable.
    pub fn new(window: &impl HasWindowHandle, font: Font, size: f32) -> Option<Self> {
        let size = clamp_size(size);
        let metrics = Metrics::new(&font, size);
        Some(Self {
            gpu: Gpu::new(window, ATLAS_SIZE)?,
            fonts: font.into(),
            size,
            metrics,
            context: ScaleContext::new(),
            shelves: Shelves::new(ATLAS_SIZE),
            slots: HashMap::new(),
            cells: Vec::new(),
            instances: Vec::new(),
        })
    }
}

impl Renderer for MetalRenderer {
    fn cell_size(&self) -> (usize, usize) {
        (self.metrics.cell_width, self.metrics.cell_height)
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.gpu.resize(width, height);
    }

    fn set_font_size(&mut self, size: f32) {
        self.size = clamp_size(size);
        self.metrics = Metrics::new(self.fonts.primary(), self.size);
        // The atlas keeps its size, and new glyphs overwrite the old ones.
        self.slots = HashMap::new();
        self.shelves = Shelves::new(ATLAS_SIZE);
    }

    fn draw(&mut self, frame: &Frame) -> Result<(), RenderError> {
        cells_with_cursor(frame, &mut self.cells);
        let clear = frame.cells.first().map_or(0, |cell| cell.background);
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
        let slot = |character: char| {
            if let Some(slot) = slots.get(&character) {
                return Some(*slot);
            }
            let glyph = rasterize(context, fonts, *size, metrics, character);
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
            slots.insert(character, slot);
            Some(slot)
        };
        let columns = usize::from(frame.columns);
        instances(
            &self.cells,
            columns,
            metrics,
            clear,
            slot,
            &mut self.instances,
        );
        self.gpu.draw(clear, &self.instances);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAPER: u32 = 0x0010_2030;
    const METRICS: Metrics = Metrics {
        cell_width: 10,
        cell_height: 20,
        baseline: 16,
    };

    fn cell(character: char, background: u32) -> Cell {
        Cell {
            character,
            foreground: 0x00ff_ffff,
            background,
        }
    }

    fn slot(_: char) -> Option<Slot> {
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
        instances(cells, columns, METRICS, PAPER, slot, &mut out);
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
    fn empty_glyphs_are_skipped() {
        let mut out = Vec::new();
        let none = |_| {
            Some(Slot {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                left: 0,
                top: 0,
            })
        };
        instances(&[cell('x', PAPER)], 1, METRICS, PAPER, none, &mut out);
        assert!(out.is_empty());
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
