//! One window at a fixed physical size, drawn by one backend. See README.md.
//!
//! Usage: spike-window-memory <base|cpu|cpu-relief|metal> <width> <height> <size-file>
//! Draws 60 full-window frames, then idles so that a parent can measure it.

#[cfg(target_os = "macos")]
mod metal;

use std::num::NonZeroU32;
use std::rc::Rc;
use std::{env, fs};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const FRAMES: u32 = 60;

enum Backend {
    Base,
    Cpu(Surface<Rc<Window>, Rc<Window>>),
    #[cfg(target_os = "macos")]
    Metal(metal::Renderer),
}

struct App {
    mode: String,
    width: u32,
    height: u32,
    size_file: String,
    frame: u32,
    window: Option<Rc<Window>>,
    backend: Option<Backend>,
}

/// Asks the allocator to return freed pages, as the architecture's technique 4 proposes.
fn relieve_allocator() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut libc::c_void, goal: libc::size_t) -> libc::size_t;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }
}

impl App {
    fn draw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let size = window.inner_size();
        let shade = 0x0010_1010 * (self.frame % 16);
        match self.backend.as_mut() {
            Some(Backend::Cpu(surface)) => {
                let (Some(width), Some(height)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
                    return;
                };
                surface.resize(width, height).expect("resize");
                let mut buffer = surface.buffer_mut().expect("buffer");
                buffer.fill(shade);
                buffer.present().expect("present");
            }
            #[cfg(target_os = "macos")]
            Some(Backend::Metal(renderer)) => renderer.draw(shade),
            _ => {}
        }
        self.frame += 1;
        if self.frame < FRAMES {
            window.request_redraw();
        } else if self.frame == FRAMES {
            if self.mode == "cpu-relief" {
                relieve_allocator();
            }
            fs::write(&self.size_file, format!("{}x{}", size.width, size.height)).expect("size file");
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("pinfold window spike")
            .with_resizable(false)
            .with_inner_size(PhysicalSize::new(self.width, self.height));
        let window = Rc::new(event_loop.create_window(attributes).expect("window"));
        self.backend = Some(match self.mode.as_str() {
            "base" => Backend::Base,
            #[cfg(target_os = "macos")]
            "metal" => Backend::Metal(metal::Renderer::new(&window)),
            _ => {
                let context = Context::new(window.clone()).expect("context");
                Backend::Cpu(Surface::new(&context, window.clone()).expect("surface"))
            }
        });
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::CloseRequested => event_loop.exit(),
            _ => {}
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        mode: args[1].clone(),
        width: args[2].parse().expect("width"),
        height: args[3].parse().expect("height"),
        size_file: args[4].clone(),
        frame: 0,
        window: None,
        backend: None,
    };
    event_loop.run_app(&mut app).expect("run");
}
