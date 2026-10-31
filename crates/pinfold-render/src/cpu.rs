//! The `Renderer` trait, and CPU drawing with `softbuffer` (ADR-0005).

use std::num::NonZeroU32;

use pinfold_proto::{Cell, Frame};
use softbuffer::{Context, Rect, Surface};

use crate::font::Font;
use crate::paint::{Canvas, Painter};

pub use softbuffer::SoftBufferError as RenderError;

/// Something that can put a frame of cells on screen.
pub trait Renderer {
    /// Returns the size of a cell in physical pixels, as `(width, height)`.
    fn cell_size(&self) -> (usize, usize);

    /// Sets the surface size in physical pixels.
    fn resize(&mut self, width: u32, height: u32);

    /// Draws `frame`, redrawing only rows that changed where the surface allows it.
    ///
    /// # Errors
    ///
    /// Returns an error if the surface cannot be drawn to or presented.
    fn draw(&mut self, frame: &Frame) -> Result<(), RenderError>;
}

/// Draws cells with the CPU into a `softbuffer` surface.
pub struct CpuRenderer<D, W> {
    surface: Surface<D, W>,
    painter: Painter,
    width: u32,
    height: u32,
    /// What the visible buffer holds, so unchanged rows can be skipped.
    drawn: Vec<Cell>,
    next: Vec<Cell>,
}

impl<D, W> CpuRenderer<D, W>
where
    D: raw_window_handle::HasDisplayHandle,
    W: raw_window_handle::HasWindowHandle,
{
    /// Creates a renderer for a window. `size` is the font size in physical pixels.
    ///
    /// # Errors
    ///
    /// Returns an error if the display connection or surface cannot be set up.
    pub fn new(display: D, window: W, font: Font, size: f32) -> Result<Self, RenderError> {
        let context = Context::new(display)?;
        Ok(Self {
            surface: Surface::new(&context, window)?,
            painter: Painter::new(font, size),
            width: 0,
            height: 0,
            drawn: Vec::new(),
            next: Vec::new(),
        })
    }
}

impl<D, W> Renderer for CpuRenderer<D, W>
where
    D: raw_window_handle::HasDisplayHandle,
    W: raw_window_handle::HasWindowHandle,
{
    fn cell_size(&self) -> (usize, usize) {
        self.painter.cell_size()
    }

    fn resize(&mut self, width: u32, height: u32) {
        (self.width, self.height) = (width, height);
    }

    fn draw(&mut self, frame: &Frame) -> Result<(), RenderError> {
        let (Some(width), Some(height)) =
            (NonZeroU32::new(self.width), NonZeroU32::new(self.height))
        else {
            return Ok(());
        };
        let columns = usize::from(frame.columns).max(1);
        self.next.clear();
        self.next.extend_from_slice(&frame.cells);
        if let Some((row, column)) = frame.cursor {
            let at = usize::from(row) * columns + usize::from(column);
            if let Some(cell) = self.next.get_mut(at) {
                std::mem::swap(&mut cell.foreground, &mut cell.background);
            }
        }
        self.surface.resize(width, height)?;
        let mut buffer = self.surface.buffer_mut()?;
        // Only a buffer that holds the previous frame can keep its unchanged rows.
        let full = buffer.age() != 1 || self.drawn.len() != self.next.len();
        let Some(mut canvas) = Canvas::new(&mut buffer, self.width as usize, self.height as usize)
        else {
            return Ok(());
        };
        if full {
            canvas.clear(frame.cells.first().map_or(0, |cell| cell.background));
        }
        let (_, cell_height) = self.painter.cell_size();
        let mut damage = Vec::new();
        for (row, cells) in self.next.chunks(columns).enumerate() {
            let old = self.drawn.chunks(columns).nth(row);
            if !full && old == Some(cells) {
                continue;
            }
            self.painter.draw_row(&mut canvas, row, cells);
            let top = u32::try_from(row * cell_height).unwrap_or(u32::MAX);
            if top < self.height {
                let rows = (self.height - top).min(u32::try_from(cell_height).unwrap_or(u32::MAX));
                if let Some(rows) = NonZeroU32::new(rows) {
                    damage.push(Rect {
                        x: 0,
                        y: top,
                        width,
                        height: rows,
                    });
                }
            }
        }
        std::mem::swap(&mut self.drawn, &mut self.next);
        if full || damage.is_empty() {
            buffer.present()
        } else {
            buffer.present_with_damage(&damage)
        }
    }
}
