//! One window with one pane.
//!
//! The I/O thread owns the PTY and hands the window copies of the screen. M2 moves that
//! thread into the core process, and until then a window crash ends the shell.

use std::env;
use std::io;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use pinfold_config::Config;
use pinfold_core::io::{Reactor, Waker};
use pinfold_core::search::{Direction, Position, Selection};
use pinfold_proto::{Frame, MouseMode};
use pinfold_render::cpu::{CpuRenderer, Renderer};
use pinfold_render::font::{Font, FontError};
use pinfold_render::padding::Padding;
use pinfold_sys::clipboard::Clipboard;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::error::{EventLoopError, OsError};
use winit::event::{
    ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "macos")]
use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS, WindowExtMacOS};
use winit::window::{Window, WindowId};

use crate::flash::Flash;
use crate::paste::Prompt;
use crate::search_bar::{Bar, Step};
use crate::selection::Selecting;
use crate::{ime, input, mouse, paste, reload, search_bar, selection};

const DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1000, 640);
const PANE: usize = 0;
const ROWS: u16 = 24;
const COLUMNS: u16 = 80;
/// Rows under the pane that belong to Pinfold, so program output cannot reach them.
const CHROME_ROWS: u16 = 1;

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
    Paste(String),
    Resize(u16, u16),
    /// Rows to move the view into history, or back if negative.
    Scroll(i32),
    /// Replaces any search running.
    Search {
        query: String,
        from: Position,
        direction: Direction,
    },
    Copy(Selection),
}

enum Notice {
    Frame,
    /// The last search ended, and its match is in view if it had one.
    Searched(Option<Selection>),
    Copied(String),
    Exited,
    /// The config file was saved, and this is what it now says.
    Config(Config),
}

/// The latest screen. While `pending`, the window has not taken it yet, so the I/O thread
/// leaves the pane dirty. That limits copies to what the window can draw.
#[derive(Default)]
struct Latest {
    frame: Mutex<Frame>,
    /// The core's number for the first row of `frame`.
    top: AtomicUsize,
    pending: AtomicBool,
}

fn run_io(
    mut reactor: Reactor,
    requests: &Receiver<Request>,
    latest: &Latest,
    proxy: &EventLoopProxy<Notice>,
) -> io::Result<()> {
    let mut searching = false;
    loop {
        reactor.turn(None)?;
        while let Ok(request) = requests.try_recv() {
            match request {
                Request::Input(bytes) => {
                    reactor.write(PANE, &bytes);
                }
                Request::Paste(text) => {
                    reactor.paste(PANE, &text);
                }
                Request::Resize(rows, columns) => reactor.resize(PANE, rows, columns)?,
                Request::Scroll(lines) => reactor.scroll(PANE, lines),
                Request::Search {
                    query,
                    from,
                    direction,
                } => searching = reactor.search(PANE, &query, from, direction),
                Request::Copy(selection) => {
                    let mut text = String::new();
                    if let Some(engine) = reactor.engine(PANE) {
                        // shortcut: past MAX_SELECTION this copies the rows that fit without
                        // saying so, tell the user in the chrome.
                        engine.selection_text(selection, &mut text);
                    }
                    if proxy.send_event(Notice::Copied(text)).is_err() {
                        return Ok(());
                    }
                }
            }
        }
        if searching && !reactor.is_searching(PANE) {
            searching = false;
            let found = reactor.take_found(PANE);
            if let (Some(found), Some(engine)) = (found, reactor.engine(PANE)) {
                let top = engine.position(0, 0).row;
                let lines = search_bar::scroll_to(found.anchor.row, top, engine.size().0);
                reactor.scroll(PANE, lines);
            }
            if proxy.send_event(Notice::Searched(found)).is_err() {
                return Ok(());
            }
        }
        if !latest.pending.load(Ordering::Acquire) && reactor.take_dirty(PANE) {
            if let (Some(engine), Ok(mut frame)) = (reactor.engine(PANE), latest.frame.lock()) {
                engine.frame(&mut frame);
                let top = engine.position(0, 0).row;
                latest.top.store(top, Ordering::Release);
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

impl Open {
    fn padding(&self) -> Padding {
        Padding::scaled(self.window.scale_factor())
    }

    /// Applies the background settings. Winit can change a window's transparency at any time
    /// on macOS, so it stays off until the renderer says it can draw that way.
    fn apply_background(&mut self, config: &Config) {
        let translucent = self
            .renderer
            .set_background_opacity(config.background_opacity);
        self.window.set_transparent(translucent);
        // shortcut: winit fixes the radius at 80 through a private CoreGraphics call, take over
        // that call if a user needs another radius, as WezTerm's 30 is out of reach here.
        self.window.set_blur(translucent && config.background_blur);
    }
}

#[cfg(target_os = "macos")]
fn option_as_alt(config: &Config) -> OptionAsAlt {
    if config.macos_option_as_alt {
        OptionAsAlt::Both
    } else {
        OptionAsAlt::None
    }
}

/// Metal on macOS (ADR-0022), or CPU drawing if this Mac has no Metal device.
fn renderer(
    window: &Rc<Window>,
    size: f32,
    font: Option<&Path>,
) -> Result<Box<dyn Renderer>, Error> {
    #[cfg(target_os = "macos")]
    if let Some(renderer) =
        pinfold_render::metal::MetalRenderer::new(window.as_ref(), Font::chosen(font)?, size)
    {
        return Ok(Box::new(renderer));
    }
    let font = Font::chosen(font)?;
    Ok(Box::new(CpuRenderer::new(
        window.clone(),
        window.clone(),
        font,
        size,
    )?))
}

struct App {
    config: Config,
    size: Option<(u32, u32)>,
    requests: Sender<Request>,
    latest: Arc<Latest>,
    waker: Arc<Waker>,
    open: Option<Open>,
    frame: Frame,
    modifiers: ModifiersState,
    /// Text the input method is composing, and the byte its caret is at.
    preedit: (String, usize),
    /// The core's number for the first row of `frame`.
    top: usize,
    /// The screen with the selection, `preedit` and chrome over it. Empty until there are any.
    composed: Frame,
    /// Where the input method's window was last told to go.
    ime_cursor: Option<(u16, u16)>,
    /// The cell under the pointer, as `(row, column)`.
    pointer: (u16, u16),
    /// The buttons held, by their number in reports, lowest first.
    buttons: [bool; 3],
    /// Wheel movement not yet worth a whole line.
    wheel: f32,
    /// Connected on first use, so a window that never pastes or copies does not pay for it.
    clipboard: Option<Clipboard>,
    /// A paste waiting for the user. Every key goes to this until it is answered.
    prompt: Option<Prompt>,
    flash: Flash,
    selecting: Selecting,
    /// The search bar, while open. Keys go to this before the program.
    bar: Option<Bar>,
    result: Result<(), Error>,
}

impl App {
    fn request(&self, request: Request) {
        if self.requests.send(request).is_ok() {
            let _ = self.waker.wake();
        }
    }

    fn redraw_soon(&self) {
        if let Some(open) = &self.open {
            open.window.request_redraw();
        }
    }

    fn open_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Error> {
        let attributes = Window::default_attributes().with_title("Pinfold");
        let attributes = match self.size {
            Some((width, height)) => attributes.with_inner_size(PhysicalSize::new(width, height)),
            None => attributes.with_inner_size(DEFAULT_SIZE),
        };
        // Without this macOS hands over the composed character, such as ƒ for Option+f.
        // shortcut: the first row's left cells sit under the traffic lights, which padding of 12
        // does not clear, the top bar must take that row.
        #[cfg(target_os = "macos")]
        let attributes = attributes
            .with_option_as_alt(option_as_alt(&self.config))
            .with_fullsize_content_view(true)
            .with_titlebar_transparent(true)
            .with_title_hidden(true);
        let window = Rc::new(event_loop.create_window(attributes)?);
        window.set_ime_allowed(true);
        let size = self.config.font_size * window.scale_factor() as f32;
        let renderer = renderer(&window, size, self.config.font_path.as_deref())?;
        let mut open = Open { window, renderer };
        open.apply_background(&self.config);
        self.open = Some(open);
        Ok(())
    }

    fn resized(&mut self, size: PhysicalSize<u32>) {
        let Some(open) = &mut self.open else { return };
        let padding = open.padding();
        open.renderer.resize(size.width, size.height, padding);
        let cell = open.renderer.cell_size();
        let (rows, columns) = padding.grid(size.width, size.height, cell);
        open.window.request_redraw();
        let rows = rows.saturating_sub(CHROME_ROWS).max(1);
        self.request(Request::Resize(rows, columns.max(1)));
    }

    /// Redraws the font for a new display. Winit sends `Resized` after this only sometimes.
    fn scale_changed(&mut self, scale: f64) {
        let Some(open) = &mut self.open else { return };
        open.renderer
            .set_font_size(self.config.font_size * scale as f32);
        let size = open.window.inner_size();
        self.ime_cursor = None;
        self.resized(size);
    }

    /// Applies a saved config. The window keeps its size, so the grid gets new dimensions.
    fn configure(&mut self, config: Config) {
        if config == self.config {
            return;
        }
        let old = std::mem::replace(&mut self.config, config);
        let Some(open) = &mut self.open else { return };
        #[cfg(target_os = "macos")]
        open.window.set_option_as_alt(option_as_alt(&self.config));
        open.apply_background(&self.config);
        if self.config.font_path != old.font_path {
            match Font::chosen(self.config.font_path.as_deref()) {
                Ok(font) => open.renderer.set_font(font),
                Err(error) => eprintln!("pinfold: keeping the current font: {error}"),
            }
        }
        let scale = open.window.scale_factor();
        self.scale_changed(scale);
    }

    fn redraw(&mut self) -> Result<(), Error> {
        if self.latest.pending.load(Ordering::Acquire) {
            if let Ok(mut latest) = self.latest.frame.lock() {
                std::mem::swap(&mut *latest, &mut self.frame);
                let top = self.latest.top.load(Ordering::Acquire);
                self.selecting
                    .check((&latest, self.top), (&self.frame, top));
                self.top = top;
            }
            self.latest.pending.store(false, Ordering::Release);
            let _ = self.waker.wake();
        }
        self.flash
            .observe(self.frame.clipboard_writes, Instant::now());
        if let Some(open) = &mut self.open {
            if self.ime_cursor != self.frame.cursor {
                self.ime_cursor = self.frame.cursor;
                if let Some((row, column)) = self.frame.cursor {
                    let (width, height) = open.renderer.cell_size();
                    let padding = open.padding();
                    let x = padding.left + usize::from(column) * width;
                    let y = padding.top + usize::from(row) * height;
                    open.window.set_ime_cursor_area(
                        PhysicalPosition::new(x as u32, y as u32),
                        PhysicalSize::new(width as u32, height as u32),
                    );
                }
            }
            let selection = self.selecting.current;
            let plain = self.prompt.is_none()
                && self.flash.until().is_none()
                && self.bar.is_none()
                && selection.is_none();
            if self.preedit.0.is_empty() && plain {
                open.renderer.draw(&self.frame)?;
            } else {
                self.composed.clone_from(&self.frame);
                if let Some(selection) = selection {
                    selection::overlay(&mut self.composed, self.top, selection);
                }
                ime::overlay(&mut self.composed, &self.preedit.0, self.preedit.1);
                if let Some(prompt) = &self.prompt {
                    paste::banner(&mut self.composed, prompt);
                } else if let Some(bar) = &self.bar {
                    bar.draw(&mut self.composed);
                } else {
                    self.flash.banner(&mut self.composed);
                }
                open.renderer.draw(&self.composed)?;
            }
        }
        Ok(())
    }

    fn clipboard(&mut self) -> Option<&mut Clipboard> {
        let open = self.open.as_ref()?;
        if self.clipboard.is_none() {
            self.clipboard = Clipboard::new(open.window.clone());
        }
        self.clipboard.as_mut()
    }

    fn paste(&mut self) {
        let Some(text) = self.clipboard().and_then(Clipboard::text) else {
            return;
        };
        if self.type_in_bar(&text) {
            return;
        }
        match paste::check(text) {
            Ok(text) => self.request(Request::Paste(text)),
            Err(prompt) => {
                self.prompt = Some(prompt);
                self.redraw_soon();
            }
        }
    }

    /// Handles a key that is the window's, and returns whether it was.
    fn shortcut(&mut self, event: &KeyEvent) -> bool {
        let key = &event.logical_key;
        // Only this key copies, so output alone can never reach the clipboard.
        if input::is_copy(key, self.modifiers) {
            if let Some(selection) = self.selecting.current.filter(|_| !event.repeat) {
                self.request(Request::Copy(selection));
            }
            return true;
        }
        if input::is_find(key, self.modifiers) {
            self.bar.get_or_insert_default();
            self.redraw_soon();
            return true;
        }
        let text = event.text.as_ref().map(|text| text.as_str());
        let bar = self.bar.as_mut().filter(|_| self.preedit.0.is_empty());
        match bar.and_then(|bar| bar.key(key, text, self.modifiers)) {
            None => return false,
            Some(Step::Stay) => {}
            Some(Step::Close) => self.bar = None,
            Some(step) => self.find(step),
        }
        self.redraw_soon();
        true
    }

    /// Adds `text` to the search bar's query. Returns false if the bar is closed.
    fn type_in_bar(&mut self, text: &str) -> bool {
        let Some(bar) = &mut self.bar else {
            return false;
        };
        if bar.push(text) {
            self.find(Step::Typed);
            self.redraw_soon();
        }
        true
    }

    fn find(&mut self, step: Step) {
        // The old match says nothing about a new query.
        if step == Step::Typed {
            self.selecting.current = None;
        }
        let Some(bar) = &mut self.bar else { return };
        bar.missing = false;
        let (query, from, direction) = bar.search(step, self.selecting.current);
        let query = query.to_owned();
        self.request(Request::Search {
            query,
            from,
            direction,
        });
    }

    fn searched(&mut self, found: Option<Selection>) {
        if let Some(bar) = &mut self.bar {
            bar.missing = found.is_none();
            self.selecting.current = found.or(self.selecting.current);
            self.redraw_soon();
        }
    }

    fn pointer_position(&self) -> Position {
        Position {
            row: self.top + usize::from(self.pointer.0),
            column: self.pointer.1,
        }
    }

    fn answer(&mut self, key: &Key) {
        match key {
            Key::Named(NamedKey::Enter) => {
                if let Some(Prompt::Confirm { text, .. }) = self.prompt.take() {
                    self.request(Request::Paste(text));
                }
            }
            Key::Named(NamedKey::Escape) => self.prompt = None,
            _ => {}
        }
        if let Some(open) = &self.open {
            open.window.request_redraw();
        }
    }

    fn report(&self, event: mouse::Event) {
        let frame = &self.frame;
        let report = mouse::encode(
            event,
            self.pointer,
            self.modifiers,
            frame.mouse,
            frame.mouse_encoding,
        );
        if !report.is_empty() {
            self.request(Request::Input(report));
        }
    }

    fn pointer_moved(&mut self, x: f64, y: f64) {
        let Some(open) = &self.open else { return };
        let grid = (self.frame.rows, self.frame.columns);
        let pointer = open
            .padding()
            .cell_at(x, y, open.renderer.cell_size(), grid);
        if self.pointer != pointer {
            self.pointer = pointer;
            if self.selecting.drag(self.pointer_position()) {
                self.redraw_soon();
                return;
            }
            let held = self.buttons.iter().position(|&held| held);
            self.report(mouse::Event::Motion(held.map(|button| button as u8)));
        }
    }

    fn pressed(&mut self, button: MouseButton, state: ElementState) {
        let number = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        let down = state == ElementState::Pressed;
        if number == 0 && self.select(down) {
            return;
        }
        self.buttons[usize::from(number)] = down;
        self.report(if down {
            mouse::Event::Press(number)
        } else {
            mouse::Event::Release(number)
        });
    }

    /// Takes a left press or release for a selection, and returns whether it did.
    fn select(&mut self, down: bool) -> bool {
        if !down {
            return self.selecting.release();
        }
        let owned = selection::owns_press(self.frame.mouse, self.modifiers);
        if owned {
            self.selecting.press(self.pointer_position());
            self.redraw_soon();
        }
        owned
    }

    fn wheel(&mut self, delta: MouseScrollDelta) {
        let Some(open) = &self.open else { return };
        self.wheel += match delta {
            MouseScrollDelta::LineDelta(_, lines) => lines,
            MouseScrollDelta::PixelDelta(position) => {
                position.y as f32 / open.renderer.cell_size().1 as f32
            }
        };
        let lines = self.wheel.trunc();
        self.wheel -= lines;
        let key = Key::Named(if lines > 0.0 {
            NamedKey::ArrowUp
        } else {
            NamedKey::ArrowDown
        });
        for _ in 0..lines.abs() as u32 {
            if self.frame.mouse != MouseMode::Off {
                self.report(mouse::Event::Wheel(lines > 0.0));
            } else if self.frame.alternate_scroll {
                let empty = ModifiersState::empty();
                let arrow = input::encode(&key, None, empty, self.frame.app_cursor, false);
                self.request(Request::Input(arrow));
            } else {
                self.request(Request::Scroll(if lines > 0.0 { 1 } else { -1 }));
            }
        }
    }

    fn ime(&mut self, event: Ime) {
        match event {
            Ime::Preedit(text, caret) => {
                // Composing is typing, and it draws at a cursor that history hides.
                // shortcut: with the bar open it still draws at the pane's cursor, not the bar's.
                if !text.is_empty() && self.bar.is_none() {
                    self.request(Request::Scroll(i32::MIN));
                }
                self.preedit.1 = caret.map_or(text.len(), |(start, _)| start);
                self.preedit.0 = text;
            }
            Ime::Commit(text) => {
                self.preedit.0.clear();
                if !self.type_in_bar(&text) && self.prompt.is_none() {
                    self.request(Request::Input(text.into_bytes()));
                }
            }
            Ime::Disabled => self.preedit.0.clear(),
            Ime::Enabled => {}
        }
        self.redraw_soon();
    }
}

impl ApplicationHandler<Notice> for App {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            event_loop.set_control_flow(ControlFlow::Wait);
            if let Some(open) = &self.open {
                open.window.request_redraw();
            }
        }
    }

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
            Notice::Frame => self.redraw_soon(),
            Notice::Searched(found) => self.searched(found),
            // Blank cells copy as nothing, which must not empty the clipboard.
            Notice::Copied(text) if text.is_empty() => {}
            Notice::Copied(text) => {
                if let Some(clipboard) = self.clipboard() {
                    clipboard.set_text(text);
                }
            }
            Notice::Exited => event_loop.exit(),
            Notice::Config(config) => self.configure(config),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => self.resized(size),
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_changed(scale_factor);
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Ime(event) => self.ime(event),
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_moved(position.x, position.y);
            }
            WindowEvent::MouseInput { state, button, .. } => self.pressed(button, state),
            WindowEvent::MouseWheel { delta, .. } => self.wheel(delta),
            // A held key repeating must not answer a prompt it did not see.
            WindowEvent::KeyboardInput { event, .. } if self.prompt.is_some() => {
                if event.state == ElementState::Pressed && !event.repeat {
                    self.answer(&event.logical_key);
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && input::is_paste(&event.logical_key, self.modifiers) =>
            {
                if !event.repeat {
                    self.paste();
                }
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && self.shortcut(&event) => {}
            // A key pressed while composing belongs to the input method.
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && self.preedit.0.is_empty() =>
            {
                let text = event.text.as_ref().map(|text| text.as_str());
                let (app_cursor, alt) = (self.frame.app_cursor, self.config.macos_option_as_alt);
                let bytes =
                    input::encode(&event.logical_key, text, self.modifiers, app_cursor, alt);
                if !bytes.is_empty() {
                    self.request(Request::Input(bytes));
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    self.result = Err(error);
                    event_loop.exit();
                }
                event_loop.set_control_flow(
                    self.flash
                        .until()
                        .map_or(ControlFlow::Wait, ControlFlow::WaitUntil),
                );
            }
            _ => {}
        }
    }
}

/// Reads the config file. A typo must not lock anyone out of their terminal, so a bad
/// file means the defaults.
fn config() -> Config {
    let Some(path) = pinfold_config::path() else {
        return Config::default();
    };
    pinfold_config::load(&path).unwrap_or_else(|error| {
        // shortcut: stderr only, so a launch from the Dock shows nothing, draw it in the chrome.
        eprintln!(
            "pinfold: using the defaults, not {}: {error}",
            path.display()
        );
        Config::default()
    })
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
    // Before the file is first read, so a save between the two is not missed.
    let proxy = event_loop.create_proxy();
    reload::watch(move |config| proxy.send_event(Notice::Config(config)).is_ok())?;
    let mut app = App {
        config: config(),
        size,
        requests,
        latest,
        waker,
        open: None,
        frame: Frame::default(),
        modifiers: ModifiersState::empty(),
        preedit: (String::new(), 0),
        top: 0,
        composed: Frame::default(),
        ime_cursor: None,
        pointer: (0, 0),
        buttons: [false; 3],
        wheel: 0.0,
        clipboard: None,
        prompt: None,
        flash: Flash::default(),
        selecting: Selecting::default(),
        bar: None,
        result: Ok(()),
    };
    event_loop.run_app(&mut app)?;
    app.result
}
