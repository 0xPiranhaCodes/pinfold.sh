//! Minimal CPU window: softbuffer, with a block per cell instead of a glyph.
//!
//! There are no fonts in this spike because fonts cost the same in both designs.

use std::num::NonZeroU32;
use std::rc::Rc;

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::{COLUMNS, ROWS};

const GROUND: u32 = 0x0012_151A;
const TEXT: u32 = 0x00E2_E5EA;

pub type Grids = dyn FnMut(&mut Vec<u64>);

struct App {
    panes: usize,
    grids: Box<Grids>,
    cells: Vec<u64>,
    surface: Option<(Rc<Window>, Surface<Rc<Window>, Rc<Window>>)>,
}

impl App {
    fn redraw(&mut self) {
        self.cells.clear();
        (self.grids)(&mut self.cells);
        let Some((window, surface)) = &mut self.surface else { return };
        let size = window.inner_size();
        let (Some(width), Some(height)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        surface.resize(width, height).expect("resize");
        let mut buffer = surface.buffer_mut().expect("buffer");
        buffer.fill(GROUND);
        let (width, height) = (size.width as usize, size.height as usize);
        let across = self.panes.min(4);
        let down = self.panes.div_ceil(across);
        let cell_width = (width / across / COLUMNS).max(1);
        let cell_height = (height / down / ROWS).max(1);
        for (index, grid) in self.cells.chunks(COLUMNS * ROWS).enumerate() {
            let (left, top) = ((index % across) * width / across, (index / across) * height / down);
            for (cell, &value) in grid.iter().enumerate() {
                if value == u64::from(u32::from(' ')) {
                    continue;
                }
                let x = left + (cell % COLUMNS) * cell_width;
                let y = top + (cell / COLUMNS) * cell_height;
                for row in y..(y + cell_height - 1).min(height) {
                    let end = (row * width + x + cell_width - 1).min(buffer.len());
                    buffer[row * width + x..end].fill(TEXT);
                }
            }
        }
        buffer.present().expect("present");
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.surface.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("pinfold spike")
            .with_inner_size(LogicalSize::new(1280, 800));
        let window = Rc::new(event_loop.create_window(attributes).expect("window"));
        let context = Context::new(window.clone()).expect("context");
        let surface = Surface::new(&context, window.clone()).expect("surface");
        self.surface = Some((window, surface));
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        if let Some((window, _)) = &self.surface {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
    }
}

/// Creates the event loop, hands its proxy to `start`, then runs until the window closes.
pub fn run(panes: usize, start: impl FnOnce(EventLoopProxy<()>) -> Box<Grids>) {
    let event_loop = EventLoop::<()>::with_user_event().build().expect("event loop");
    let grids = start(event_loop.create_proxy());
    let mut app = App { panes, grids, cells: Vec::new(), surface: None };
    event_loop.run_app(&mut app).expect("run");
}
