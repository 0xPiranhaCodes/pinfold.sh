//! One window with one pane.
//!
//! The I/O thread owns the PTY and hands the window copies of the screen. M2 moves that
//! thread into the core process, and until then a window crash ends the shell.

use std::env;
use std::io;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use pinfold_core::io::{Reactor, Waker};
use pinfold_proto::Frame;
use pinfold_render::cpu::{CpuRenderer, Renderer};
use pinfold_render::font::{Font, FontError};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::error::{EventLoopError, OsError};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::input;

/// In logical pixels.
const FONT_SIZE: f32 = 13.0;
const DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1000, 640);
const PANE: usize = 0;
const ROWS: u16 = 24;
const COLUMNS: u16 = 80;

/// Why the window could not run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The event loop failed.
    #[error(transparent)]
    EventLoop(#[from] EventLoopError),
    /// The window could not be created.
    #[error(transparent)]
    Window(#[from] OsError),
    /// The font could not be loaded.
    #[error(transparent)]
    Font(#[from] FontError),
    /// The drawing surface failed.
    #[error(transparent)]
    Render(#[from] pinfold_render::cpu::RenderError),
    /// The PTY or poller failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

enum Request {
    Input(Vec<u8>),
    Resize(u16, u16),
}

enum Notice {
    Frame,
    Exited,
}

/// The latest screen. While `pending`, the window has not taken it yet, so the I/O thread
/// leaves the pane dirty. That limits copies to what the window can draw.
#[derive(Default)]
struct Latest {
    frame: Mutex<Frame>,
    pending: AtomicBool,
}

fn run_io(
    mut reactor: Reactor,
    requests: &Receiver<Request>,
    latest: &Latest,
    proxy: &EventLoopProxy<Notice>,
) -> io::Result<()> {
    loop {
        reactor.turn(None)?;
        while let Ok(request) = requests.try_recv() {
            match request {
                Request::Input(bytes) => reactor.write(PANE, &bytes),
                Request::Resize(rows, columns) => reactor.resize(PANE, rows, columns)?,
            }
        }
        if !latest.pending.load(Ordering::Acquire) && reactor.take_dirty(PANE) {
            if let (Some(engine), Ok(mut frame)) = (reactor.engine(PANE), latest.frame.lock()) {
                engine.frame(&mut frame);
            }
            latest.pending.store(true, Ordering::Release);
            if proxy.send_event(Notice::Frame).is_err() {
                return Ok(());
            }
        }
        if reactor.is_closed(PANE) {
            let _ = proxy.send_event(Notice::Exited);
            return Ok(());
        }
    }
}

struct Open {
    window: Rc<Window>,
    renderer: Box<dyn Renderer>,
}

/// Metal on macOS (ADR-0022), or CPU drawing if this Mac has no Metal device.
fn renderer(window: &Rc<Window>, size: f32) -> Result<Box<dyn Renderer>, Error> {
    #[cfg(target_os = "macos")]
    if let Some(renderer) =
        pinfold_render::metal::MetalRenderer::new(window.as_ref(), Font::bundled()?, size)
    {
        return Ok(Box::new(renderer));
    }
    let font = Font::bundled()?;
    Ok(Box::new(CpuRenderer::new(
        window.clone(),
        window.clone(),
        font,
        size,
    )?))
}

struct App {
    size: Option<(u32, u32)>,
    requests: Sender<Request>,
    latest: Arc<Latest>,
    waker: Arc<Waker>,
    open: Option<Open>,
    frame: Frame,
    modifiers: ModifiersState,
    result: Result<(), Error>,
}

impl App {
    fn request(&self, request: Request) {
        if self.requests.send(request).is_ok() {
            let _ = self.waker.wake();
        }
    }

    fn open_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Error> {
        let attributes = Window::default_attributes().with_title("Pinfold");
        let attributes = match self.size {
            Some((width, height)) => attributes.with_inner_size(PhysicalSize::new(width, height)),
            None => attributes.with_inner_size(DEFAULT_SIZE),
        };
        let window = Rc::new(event_loop.create_window(attributes)?);
        // shortcut: fixed at the first scale factor, so moving between displays blurs, fix in #21.
        let size = FONT_SIZE * window.scale_factor() as f32;
        let renderer = renderer(&window, size)?;
        self.open = Some(Open { window, renderer });
        Ok(())
    }

    fn resized(&mut self, size: PhysicalSize<u32>) {
        let Some(open) = &mut self.open else { return };
        open.renderer.resize(size.width, size.height);
        let (width, height) = open.renderer.cell_size();
        let columns = u16::try_from(size.width as usize / width).unwrap_or(u16::MAX);
        let rows = u16::try_from(size.height as usize / height).unwrap_or(u16::MAX);
        open.window.request_redraw();
        self.request(Request::Resize(rows.max(1), columns.max(1)));
    }

    fn redraw(&mut self) -> Result<(), Error> {
        if self.latest.pending.load(Ordering::Acquire) {
            if let Ok(mut latest) = self.latest.frame.lock() {
                std::mem::swap(&mut *latest, &mut self.frame);
            }
            self.latest.pending.store(false, Ordering::Release);
            let _ = self.waker.wake();
        }
        if let Some(open) = &mut self.open {
            open.renderer.draw(&self.frame)?;
        }
        Ok(())
    }
}

impl ApplicationHandler<Notice> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.open.is_some() {
            return;
        }
        if let Err(error) = self.open_window(event_loop) {
            self.result = Err(error);
            event_loop.exit();
        }
        if let Some(size) = self.open.as_ref().map(|open| open.window.inner_size()) {
            self.resized(size);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, notice: Notice) {
        match notice {
            Notice::Frame => {
                if let Some(open) = &self.open {
                    open.window.request_redraw();
                }
            }
            Notice::Exited => event_loop.exit(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.resized(size),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let text = event.text.as_ref().map(|text| text.as_str());
                let app_cursor = self.frame.app_cursor;
                let bytes = input::encode(&event.logical_key, text, self.modifiers, app_cursor);
                if !bytes.is_empty() {
                    self.request(Request::Input(bytes));
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    self.result = Err(error);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }
}

/// Opens a window running the user's shell, and returns when either closes.
///
/// `size` is an optional inner size in physical pixels.
///
/// # Errors
///
/// Returns an error if the window, font, surface or shell cannot be set up.
pub fn run(size: Option<(u32, u32)>) -> Result<(), Error> {
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut reactor = Reactor::new()?;
    reactor.spawn(Command::new(shell), ROWS, COLUMNS)?;
    let waker = reactor.waker();
    let (requests, received) = mpsc::channel();
    let latest = Arc::new(Latest::default());
    let event_loop = EventLoop::with_user_event().build()?;
    let (proxy, shared) = (event_loop.create_proxy(), latest.clone());
    thread::Builder::new()
        .name("pinfold-io".into())
        .spawn(move || {
            if let Err(error) = run_io(reactor, &received, &shared, &proxy) {
                eprintln!("pinfold: I/O thread stopped: {error}");
                let _ = proxy.send_event(Notice::Exited);
            }
        })?;
    let mut app = App {
        size,
        requests,
        latest,
        waker,
        open: None,
        frame: Frame::default(),
        modifiers: ModifiersState::empty(),
        result: Ok(()),
    };
    event_loop.run_app(&mut app)?;
    app.result
}
