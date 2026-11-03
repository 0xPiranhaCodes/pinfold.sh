//! One window on the core's session.
//!
//! The core owns the PTYs and the tabs, and the I/O thread here owns the link to it (ADR-0028).
//! That thread hands the window copies of the panes the active tab shows, so a window that dies
//! leaves them running, and a hidden tab costs nothing here.

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use mio::Waker;
use pinfold_config::Config;
use pinfold_proto::link::{self as proto, MAX_DIMENSION, Position, Rect, Selection};
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

use crate::flash::{self, Flash};
use crate::link::{self, Link, Screen, Tab};
use crate::outbox::{self, Inbox, Outbox, Request};
use crate::paste::Prompt;
use crate::prefix::{self, Command, Prefix};
use crate::search_bar::{Bar, Step};
use crate::selection::Selecting;
use crate::top_bar::{Clicks, Hit};
use crate::vi::{Step as ViStep, Vi, View};
use crate::{ime, input, mouse, panes, paste, reload, selection, top_bar};

const DEFAULT_SIZE: LogicalSize<u32> = LogicalSize::new(1000, 640);
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
    /// The link or poller failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

enum Notice {
    Frame,
    /// The last search in this pane ended, and its match is in view if it had one.
    Searched(u32, Option<Selection>),
    Copied(String),
    /// The core did not carry out a command.
    Refused(proto::Refusal),
    Exited,
    /// The link ended, so the panes went with the core.
    Ended,
    /// Asked to come to the front.
    Raise,
    /// The config file was saved, and this is what it now says.
    Config(Config),
}

/// What the I/O thread has for the window. A screen marked dirty here is one the window has not
/// taken yet.
#[derive(Default)]
struct Shown {
    screens: Vec<Screen>,
    tabs: Vec<Tab>,
    active: usize,
    activity: u32,
    focused: u32,
    total: usize,
    /// Whether what is above, past the screens' contents, changed since the window read it.
    laid: bool,
}

/// While `pending`, the window has not taken `shown` yet, so the I/O thread leaves the link's
/// panes dirty. That limits copies to what the window can draw.
#[derive(Default)]
struct Latest {
    shown: Mutex<Shown>,
    pending: AtomicBool,
}

/// Copies `source` over `target`. Frame's derived clone_from would allocate, and this runs on
/// every redraw.
fn copy_frame(target: &mut Frame, source: &Frame) {
    let mut cells = std::mem::take(&mut target.cells);
    cells.clone_from(&source.cells);
    *target = Frame { cells, ..*source };
}

/// Copies what changed in `link` to `shown`, and returns whether anything did. Only panes with a
/// new screen are copied, so an idle pane costs nothing per frame.
fn publish(link: &mut Link, shown: &mut Shown) -> bool {
    let mut changed = link.take_laid();
    if changed {
        let screens = link.screens();
        link::follow(
            &mut shown.screens,
            screens.iter().map(|screen| (screen.pane, screen.rect)),
        );
        shown.tabs.resize_with(link.tabs().len(), Tab::default);
        for (ours, tab) in shown.tabs.iter_mut().zip(link.tabs()) {
            ours.pane = tab.pane;
            ours.cwd.clone_from(&tab.cwd);
        }
        shown.active = link.active();
        shown.activity = link.activity();
        shown.focused = link.focused();
        shown.total = link.total();
        shown.laid = true;
    }
    for screen in link.screens_mut().iter_mut().filter(|screen| screen.dirty) {
        screen.dirty = false;
        changed = true;
        let mut ours = shown.screens.iter_mut();
        if let Some(ours) = ours.find(|ours| ours.pane == screen.pane) {
            copy_frame(&mut ours.frame, &screen.frame);
            ours.top = screen.top;
            ours.held.clone_from(&screen.held);
            ours.dirty = true;
        }
    }
    changed
}

/// Runs the link until it ends or the session does. The last screens stay in `latest`.
fn run_io(mut link: Link, requests: &Inbox, latest: &Latest, proxy: &EventLoopProxy<Notice>) {
    loop {
        // An error stays in `link`, which the end of this pass reads.
        let _ = link.turn(None);
        requests.drain(&mut link);
        let searched = link
            .take_searched()
            .map(|(pane, found)| Notice::Searched(pane, found));
        let copied = link.take_copied().map(Notice::Copied);
        let raised = link.take_raised().then_some(Notice::Raise);
        let refused = link.take_refused().map(Notice::Refused);
        for notice in [searched, copied, raised, refused].into_iter().flatten() {
            if proxy.send_event(notice).is_err() {
                return;
            }
        }
        if !latest.pending.load(Ordering::Acquire) {
            let changed = latest
                .shown
                .lock()
                .is_ok_and(|mut shown| publish(&mut link, &mut shown));
            if changed {
                latest.pending.store(true, Ordering::Release);
                if proxy.send_event(Notice::Frame).is_err() {
                    return;
                }
            }
        }
        if link.is_closed() {
            let ended = link.disconnected().is_some();
            let _ = proxy.send_event(if ended { Notice::Ended } else { Notice::Exited });
            return;
        }
    }
}

struct Open {
    window: Rc<Window>,
    renderer: Box<dyn Renderer>,
    /// Whether the window filled the screen when it last changed size.
    fullscreen: bool,
}

impl Open {
    fn padding(&self) -> Padding {
        let scale = self.window.scale_factor();
        let padding = Padding::scaled(scale);
        if self.fullscreen {
            padding.in_fullscreen(scale)
        } else {
            padding
        }
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

/// Finds what is known of `pane`, while it is shown.
fn find(panes: &[Screen], pane: u32) -> Option<&Screen> {
    panes.iter().find(|screen| screen.pane == pane)
}

/// What vi mode sees of a pane.
fn view(screen: &Screen) -> View<'_> {
    View {
        frame: &screen.frame,
        top: screen.top,
        held: &screen.held,
    }
}

struct App {
    config: Config,
    size: Option<(u32, u32)>,
    outbox: Outbox,
    latest: Arc<Latest>,
    waker: Arc<Waker>,
    open: Option<Open>,
    /// The panes the active tab shows.
    panes: Vec<Screen>,
    /// Where each of `panes` is, for drawing borders with no allocation.
    rects: Vec<Rect>,
    /// The pane keys go to. It is missing from `panes` in a window too small for it.
    focused: u32,
    /// How many panes the session holds, shown or not.
    total: usize,
    /// The space for panes, as `(rows, columns)`.
    area: (u16, u16),
    modifiers: ModifiersState,
    /// Text the input method is composing, and the byte its caret is at.
    preedit: (String, usize),
    /// The panes with their borders, the selection, `preedit` and chrome over them.
    composed: Frame,
    /// The tabs' labels, made safe to draw.
    labels: Vec<String>,
    active: usize,
    /// Which tabs have news, one bit each.
    activity: u32,
    /// The tab under the pointer.
    over_tab: Option<usize>,
    /// Whether the system says this window has the keyboard.
    has_focus: bool,
    /// When the clock in the top bar next changes.
    minute: Instant,
    /// Whether the pointer is over the top bar.
    on_bar: bool,
    /// Whether the pointer is on the top bar's title row, which moves the window.
    on_title: bool,
    clicks: Clicks,
    /// Where the input method's window was last told to go.
    ime_cursor: Option<(u16, u16)>,
    /// The cell of `area` under the pointer, as `(row, column)`. Over the top bar, its top row.
    pointer: (u16, u16),
    /// The buttons held, by their number in reports, lowest first.
    buttons: [bool; 3],
    /// Wheel movement not yet worth a whole line.
    wheel: f32,
    /// Connected on first use, so a window that never pastes or copies does not pay for it.
    clipboard: Option<Clipboard>,
    /// A question for the user. Every key goes to this until it is answered.
    prompt: Option<Prompt>,
    prefix: Prefix,
    flash: Flash,
    selecting: Selecting,
    /// The search bar, while open. Keys go to this before the program.
    bar: Option<Bar>,
    /// Vi mode, while on. Keys go to this or the bar, and none to the program.
    vi: Option<Vi>,
    /// Whether the core has gone. The last screen stays, under a row that says so.
    ended: bool,
    result: Result<(), Error>,
}

/// The scale glyphs are drawn at. The gate's macOS budget names a Retina window, so a measured
/// one draws at that scale on any display: a 1x runner would hold four times the cells.
fn font_scale(measured: Option<(u32, u32)>, scale: f64) -> f32 {
    let reference = if cfg!(target_os = "macos") && measured.is_some() {
        scale.max(2.0)
    } else {
        scale
    };
    reference as f32
}

impl App {
    fn request(&self, request: Request) {
        if self.outbox.send(request) {
            let _ = self.waker.wake();
        } else {
            self.redraw_soon();
        }
    }

    fn core(&self, request: proto::Request<'static>) {
        self.request(Request::Core(request));
    }

    fn redraw_soon(&self) {
        if let Some(open) = &self.open {
            open.window.request_redraw();
        }
    }

    fn open_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Error> {
        let attributes = Window::default_attributes().with_title("PinFold");
        let attributes = match self.size {
            Some((width, height)) => attributes.with_inner_size(PhysicalSize::new(width, height)),
            None => attributes.with_inner_size(DEFAULT_SIZE),
        };
        // Without this macOS hands over the composed character, such as ƒ for Option+f.
        #[cfg(target_os = "macos")]
        let attributes = attributes
            .with_option_as_alt(option_as_alt(&self.config))
            .with_fullsize_content_view(true)
            .with_titlebar_transparent(true)
            .with_title_hidden(true);
        let window = Rc::new(event_loop.create_window(attributes)?);
        window.set_ime_allowed(true);
        let size = self.config.font_size * font_scale(self.size, window.scale_factor());
        let renderer = renderer(&window, size, self.config.font_path.as_deref())?;
        let mut open = Open {
            window,
            renderer,
            fullscreen: false,
        };
        open.apply_background(&self.config);
        self.open = Some(open);
        Ok(())
    }

    fn resized(&mut self, size: PhysicalSize<u32>) {
        let Some(open) = &mut self.open else { return };
        open.fullscreen = open.window.fullscreen().is_some();
        let padding = open.padding();
        open.renderer.resize(size.width, size.height, padding);
        let cell = open.renderer.cell_size();
        let (rows, columns) = padding.grid(size.width, size.height, cell);
        open.window.request_redraw();
        let rows = rows
            .saturating_sub(padding.bar_rows() as u16 + CHROME_ROWS)
            .max(1);
        self.area = (rows.min(MAX_DIMENSION), columns.clamp(1, MAX_DIMENSION));
        self.request(Request::Resize(rows, columns.max(1)));
    }

    /// Redraws the font for a new display. Winit sends `Resized` after this only sometimes.
    fn scale_changed(&mut self, scale: f64) {
        let Some(open) = &mut self.open else { return };
        open.renderer
            .set_font_size(self.config.font_size * font_scale(self.size, scale));
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
        if self.config.keys.prefix != old.keys.prefix {
            self.prefix = Prefix::new(self.config.keys.prefix);
        }
        let Some(open) = &mut self.open else { return };
        #[cfg(target_os = "macos")]
        open.window.set_option_as_alt(option_as_alt(&self.config));
        open.apply_background(&self.config);
        if self.config.font_path != old.font_path {
            match Font::chosen(self.config.font_path.as_deref()) {
                Ok(font) => open.renderer.set_font(font),
                Err(error) => {
                    let _ = writeln!(io::stderr(), "pinfold: keeping the current font: {error}");
                }
            }
        }
        let scale = open.window.scale_factor();
        self.scale_changed(scale);
    }

    /// Applies a new shape: the panes shown, the tabs and the focus.
    fn lay_out(&mut self, shown: &Shown) {
        let screens = shown.screens.iter();
        link::follow(
            &mut self.panes,
            screens.map(|screen| (screen.pane, screen.rect)),
        );
        self.rects.clear();
        self.rects.extend(self.panes.iter().map(|pane| pane.rect));
        self.labels.resize_with(shown.tabs.len(), String::new);
        for (label, tab) in self.labels.iter_mut().zip(&shown.tabs) {
            top_bar::label(&tab.cwd, label);
        }
        self.active = shown.active;
        self.activity = shown.activity;
        self.total = shown.total;
        // Another pane may have closed under the question.
        if let Some(Prompt::Close { last, .. }) = &mut self.prompt {
            *last = self.total == 1;
        }
        if self.focused != shown.focused {
            let old = std::mem::replace(&mut self.focused, shown.focused);
            self.focus_moved(old);
        }
    }

    /// Ends what was going on in the pane that lost focus, as all of it belongs to one pane.
    fn focus_moved(&mut self, old: u32) {
        if self.vi.take().is_some() {
            self.core(proto::Request::Scroll {
                pane: old,
                lines: i32::MIN,
            });
            if let Some(open) = &self.open {
                open.window.set_ime_allowed(true);
            }
        }
        self.bar = None;
        self.prompt = None;
        self.selecting = Selecting::default();
        self.preedit.0.clear();
        self.ime_cursor = None;
    }

    /// Takes everything new from the I/O thread. A swap, so no copy happens under the lock.
    fn take(&mut self, shown: &mut Shown) {
        if std::mem::take(&mut shown.laid) {
            self.lay_out(shown);
        }
        for fresh in shown.screens.iter_mut().filter(|screen| screen.dirty) {
            fresh.dirty = false;
            let mut panes = self.panes.iter_mut();
            let Some(pane) = panes.find(|pane| pane.pane == fresh.pane) else {
                continue;
            };
            std::mem::swap(&mut fresh.frame, &mut pane.frame);
            std::mem::swap(&mut fresh.top, &mut pane.top);
            pane.held.clone_from(&fresh.held);
            if pane.pane == self.focused {
                self.selecting
                    .check((&fresh.frame, fresh.top), (&pane.frame, pane.top));
            }
            let writes = pane.frame.clipboard_writes;
            if fresh.frame.rows > 0 && fresh.frame.clipboard_writes != writes {
                self.flash.blocked(Instant::now());
            }
        }
        if let (Some(vi), Some(pane)) = (&mut self.vi, find(&self.panes, self.focused)) {
            vi.settle(&view(pane));
        }
    }

    fn redraw(&mut self) -> Result<(), Error> {
        if self.latest.pending.load(Ordering::Acquire) {
            let latest = self.latest.clone();
            if let Ok(mut shown) = latest.shown.lock() {
                self.take(&mut shown);
            }
            latest.pending.store(false, Ordering::Release);
            let _ = self.waker.wake();
        }
        if self.outbox.take_refused() {
            self.flash.refused(proto::Refusal::Input, Instant::now());
        }
        self.flash.tick(Instant::now());
        if let Some(open) = &mut self.open {
            let (rows, columns) = self.area;
            panes::blank(&mut self.composed, rows, columns);
            for pane in &self.panes {
                panes::blit(&mut self.composed, &pane.frame, pane.rect);
            }
            let focused = find(&self.panes, self.focused);
            panes::dividers(
                &mut self.composed,
                &self.rects,
                focused.map(|pane| pane.rect),
            );
            if let Some(pane) = focused {
                let (rect, top) = (pane.rect, pane.top);
                self.composed.cursor = panes::cursor(rect, pane.frame.cursor);
                if self.ime_cursor != self.composed.cursor {
                    self.ime_cursor = self.composed.cursor;
                    if let Some((row, column)) = self.composed.cursor {
                        let (width, height) = open.renderer.cell_size();
                        let padding = open.padding();
                        let row = usize::from(row) + padding.bar_rows();
                        let x = padding.left + usize::from(column) * width;
                        let y = padding.row_top(row, height);
                        open.window.set_ime_cursor_area(
                            PhysicalPosition::new(x as u32, y as u32),
                            PhysicalSize::new(width as u32, height as u32),
                        );
                    }
                }
                if let Some(selection) = self.selecting.current {
                    selection::overlay(&mut self.composed, rect, top, selection);
                }
                let end = rect.x + rect.columns;
                ime::overlay(&mut self.composed, end, &self.preedit.0, self.preedit.1);
                if let Some(vi) = &self.vi {
                    self.composed.cursor = panes::cursor(rect, vi.drawn(top, rect.rows));
                }
            }
            if self.ended {
                flash::ended(&mut self.composed);
            } else if let Some(prompt) = &self.prompt {
                paste::banner(&mut self.composed, prompt);
            } else if let Some(bar) = &self.bar {
                bar.draw(&mut self.composed);
            } else if let Some(vi) = self.vi.filter(|_| self.flash.until().is_none()) {
                vi.draw(&mut self.composed);
            } else {
                self.flash.banner(&mut self.composed);
            }
            let (time, minute_left) = top_bar::now();
            self.minute = Instant::now() + minute_left;
            let scale = open.window.scale_factor();
            let cell_width = open.renderer.cell_size().0;
            let left = open.padding().left;
            let inset = top_bar::inset(top_bar::LEFT_POINTS, scale, left, cell_width);
            let bar = top_bar::Bar {
                tabs: &self.labels,
                active: self.active,
                activity: self.activity,
                hovered: self.over_tab,
                focused: self.has_focus,
                title: !open.fullscreen,
                time,
                inset,
            };
            top_bar::draw(&mut self.composed, &bar);
            open.renderer.draw(&self.composed)?;
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
        if self.type_in_bar(&text) || self.vi.is_some() {
            return;
        }
        match paste::check(text) {
            Ok(text) => self.request(Request::Paste(self.focused, text)),
            Err(prompt) => {
                self.prompt = Some(prompt);
                self.redraw_soon();
            }
        }
    }

    /// Carries out what a prefix key or a Command key asked for.
    fn command(&mut self, event_loop: &ActiveEventLoop, command: Command) {
        let pane = self.focused;
        let request = match command {
            Command::NewTab => proto::Request::NewTab,
            Command::NextTab => proto::Request::StepTab { forward: true },
            Command::PreviousTab => proto::Request::StepTab { forward: false },
            Command::Tab(index) => proto::Request::SwitchTab {
                index: index as u16,
            },
            Command::SplitRight => proto::Request::Split {
                pane,
                way: proto::SplitWay::Right,
            },
            Command::SplitDown => proto::Request::Split {
                pane,
                way: proto::SplitWay::Down,
            },
            Command::Focus(side) => proto::Request::MoveFocus { side },
            Command::Resize(side, cells) => proto::Request::MoveBorder { side, cells },
            Command::Zoom => proto::Request::Zoom,
            // The session stays in the core for the next window.
            Command::Detach => return event_loop.exit(),
            // A program dies with its pane, so this asks first as tmux does.
            Command::ClosePane => {
                let last = self.total == 1;
                self.prompt = Some(Prompt::Close { pane, last });
                return self.redraw_soon();
            }
        };
        self.core(request);
    }

    /// Handles a key that belongs to the window whatever else is going on, and returns whether
    /// it did. These come before the prefix, so that an armed prefix cannot swallow a paste.
    fn window_key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) -> bool {
        let key = &event.logical_key;
        if input::is_paste(key, self.modifiers) {
            if !event.repeat {
                self.paste();
            }
            return true;
        }
        // Only this key copies, so output alone can never reach the clipboard.
        if input::is_copy(key, self.modifiers) {
            if let Some(selection) = self.selecting.current.filter(|_| !event.repeat) {
                self.request(Request::Copy(self.focused, selection));
            }
            return true;
        }
        if input::is_find(key, self.modifiers) {
            self.bar.get_or_insert_default();
            self.redraw_soon();
            return true;
        }
        if input::is_vi(key, self.modifiers) {
            if !event.repeat {
                self.toggle_vi();
            }
            return true;
        }
        let tab = input::tab_key(key, self.modifiers).filter(|_| cfg!(target_os = "macos"));
        if let Some(command) = tab {
            if !event.repeat {
                self.command(event_loop, command);
            }
            return true;
        }
        false
    }

    /// Hands a key to the search bar or vi mode, and returns whether either took it.
    fn shortcut(&mut self, event: &KeyEvent) -> bool {
        let key = &event.logical_key;
        let text = event.text.as_ref().map(|text| text.as_str());
        let bar = self.bar.as_mut().filter(|_| self.preedit.0.is_empty());
        match bar.and_then(|bar| bar.key(key, text, self.modifiers)) {
            None => return self.vi_key(event),
            Some(Step::Stay) => {}
            Some(Step::Close) => self.bar = None,
            Some(step) => self.find(step),
        }
        self.redraw_soon();
        true
    }

    /// Sends a key press where it goes: the window, then the prefix, then the search bar and vi
    /// mode, then the pane. A key pressed while composing belongs to the input method, so it
    /// neither reaches a program nor counts as the prefix.
    fn key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        if self.window_key(event, event_loop) {
            self.prefix.cancel();
            return;
        }
        let composing = !self.preedit.0.is_empty();
        if !composing {
            match self
                .prefix
                .key(&event.logical_key, self.modifiers, event.repeat)
            {
                prefix::Step::Pass => {}
                prefix::Step::Swallow => return,
                prefix::Step::Run(command) => return self.command(event_loop, command),
            }
        }
        if self.shortcut(event) || composing {
            return;
        }
        let Some(pane) = find(&self.panes, self.focused) else {
            return;
        };
        let text = event.text.as_ref().map(|text| text.as_str());
        let (app_cursor, alt) = (pane.frame.app_cursor, self.config.macos_option_as_alt);
        let bytes = input::encode(&event.logical_key, text, self.modifiers, app_cursor, alt);
        if !bytes.is_empty() {
            self.request(Request::Input(self.focused, bytes));
        }
    }

    /// Turns vi mode on at the program's cursor, or off and back to the live screen. An input
    /// method would take its keys, so that goes off with it.
    fn toggle_vi(&mut self) {
        let pane = self.focused;
        self.vi = match (self.vi, find(&self.panes, pane)) {
            (None, Some(screen)) => Some(Vi::new(&view(screen))),
            _ => None,
        };
        if self.vi.is_none() {
            let lines = i32::MIN;
            self.core(proto::Request::Scroll { pane, lines });
        }
        if let Some(open) = &self.open {
            open.window.set_ime_allowed(self.vi.is_none());
        }
        self.ime_cursor = None;
        self.redraw_soon();
    }

    /// Hands a key to vi mode. Returns false if vi mode is off.
    fn vi_key(&mut self, event: &KeyEvent) -> bool {
        let Some(vi) = &mut self.vi else {
            return false;
        };
        let Some(screen) = find(&self.panes, self.focused) else {
            return true;
        };
        // A click or new text under the selection ended it.
        if self.selecting.current.is_none() {
            vi.end_selection();
        }
        let step = vi.key(&event.logical_key, self.modifiers, &view(screen));
        let (row, selection) = (vi.row(), vi.selection());
        match step {
            ViStep::Stay => return true,
            ViStep::Moved => {
                self.selecting.current = selection.or(self.selecting.current);
                let pane = self.focused;
                self.core(proto::Request::Reveal { pane, row });
            }
            ViStep::Unselect => self.selecting.current = None,
            ViStep::Find => {
                self.bar.get_or_insert_default();
            }
            ViStep::Yank => {
                if let Some(selection) = self.selecting.current.take() {
                    self.request(Request::Copy(self.focused, selection));
                }
            }
            ViStep::Close => self.toggle_vi(),
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
            pane: self.focused,
            query,
            from,
            direction,
        });
    }

    fn searched(&mut self, pane: u32, found: Option<Selection>) {
        if let Some(bar) = self.bar.as_mut().filter(|_| pane == self.focused) {
            bar.missing = found.is_none();
            self.selecting.current = found.or(self.selecting.current);
            if let (Some(vi), Some(found)) = (&mut self.vi, found) {
                vi.jump(found.anchor);
            }
            self.redraw_soon();
        }
    }

    /// Returns where the pointer is in the focused pane, or the nearest cell there. A drag that
    /// leaves the pane stays in it.
    fn pointer_position(&self) -> Option<Position> {
        let pane = find(&self.panes, self.focused)?;
        let (row, column) = panes::local(pane.rect, self.pointer);
        Some(Position {
            row: pane.top + usize::from(row),
            column,
        })
    }

    /// Returns the pane under the pointer.
    fn hovered(&self) -> Option<&Screen> {
        let (row, column) = self.pointer;
        let mut panes = self.panes.iter().filter(|_| !self.on_bar);
        panes.find(|pane| panes::holds(pane.rect, row, column))
    }

    fn answer(&mut self, key: &Key) {
        match key {
            Key::Named(NamedKey::Enter) => match self.prompt.take() {
                Some(Prompt::Confirm { text, .. }) => {
                    self.request(Request::Paste(self.focused, text));
                }
                Some(Prompt::Close { pane, .. }) => {
                    self.core(proto::Request::ClosePane { pane });
                }
                _ => {}
            },
            Key::Named(NamedKey::Escape) => self.prompt = None,
            _ => {}
        }
        if let Some(open) = &self.open {
            open.window.request_redraw();
        }
    }

    /// Tells `pane` of `event`, at the pointer's nearest cell there.
    fn report(&self, event: mouse::Event, pane: &Screen) {
        if self.vi.is_some() || !top_bar::reaches_program(event, self.on_bar) {
            return;
        }
        let frame = &pane.frame;
        let report = mouse::encode(
            event,
            panes::local(pane.rect, self.pointer),
            self.modifiers,
            frame.mouse,
            frame.mouse_encoding,
        );
        if !report.is_empty() {
            self.request(Request::Input(pane.pane, report));
        }
    }

    fn pointer_moved(&mut self, x: f64, y: f64) {
        let Some(open) = &self.open else { return };
        let grid = self.area;
        let cell_size = open.renderer.cell_size();
        let (on_bar, pointer) = top_bar::locate(open.padding(), x, y, cell_size, grid);
        self.on_bar = on_bar;
        self.on_title = top_bar::on_title(open.padding(), y, cell_size.1);
        let over = match top_bar::hit(pointer.1, &self.labels, self.activity, self.area.1) {
            Some(Hit::Tab(tab)) if on_bar && !self.on_title => Some(tab),
            _ => None,
        };
        self.hover(over);
        if self.pointer != pointer {
            self.pointer = pointer;
            let position = self.pointer_position();
            if position.is_some_and(|at| self.selecting.drag(at)) {
                self.redraw_soon();
                return;
            }
            let held = self.buttons.iter().position(|&held| held);
            // A pane hears of a pointer passing over it only while it holds focus.
            let pane = match held {
                Some(_) => find(&self.panes, self.focused),
                None => self.hovered().filter(|pane| pane.pane == self.focused),
            };
            if let Some(pane) = pane {
                self.report(mouse::Event::Motion(held.map(|button| button as u8)), pane);
            }
        }
    }

    fn hover(&mut self, tab: Option<usize>) {
        if self.over_tab != tab {
            self.over_tab = tab;
            self.redraw_soon();
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
        if down && self.on_bar {
            if number == 0 {
                self.bar_pressed();
            }
            return;
        }
        if down {
            // Borders take no press, and the first press on a pane only focuses it.
            match self.hovered().map(|pane| pane.pane) {
                None => return,
                Some(pane) if pane != self.focused => {
                    self.core(proto::Request::Focus { pane });
                    return;
                }
                Some(_) => {}
            }
        }
        if number == 0 && self.select(down) {
            return;
        }
        // The press may have been on the bar, so the program never saw it.
        if !down && !self.buttons[usize::from(number)] {
            return;
        }
        self.buttons[usize::from(number)] = down;
        if let Some(pane) = find(&self.panes, self.focused) {
            let event = if down {
                mouse::Event::Press(number)
            } else {
                mouse::Event::Release(number)
            };
            self.report(event, pane);
        }
    }

    /// Moves or zooms the window from a left press on the top bar's title row, and takes a press
    /// on the tabs' row.
    fn bar_pressed(&mut self) {
        let Some(open) = &self.open else { return };
        if !self.on_title {
            match top_bar::hit(self.pointer.1, &self.labels, self.activity, self.area.1) {
                Some(Hit::NewTab) => self.core(proto::Request::NewTab),
                Some(Hit::Tab(tab)) => self.core(proto::Request::SwitchTab { index: tab as u16 }),
                None => {}
            }
            return;
        }
        // shortcut: always zooms in a fixed time, read the macOS double-click settings if asked.
        if cfg!(target_os = "macos") && self.clicks.press(Instant::now()) {
            open.window.set_maximized(!open.window.is_maximized());
        } else {
            let _ = open.window.drag_window();
        }
    }

    /// Takes a left press or release for a selection, and returns whether it did.
    fn select(&mut self, down: bool) -> bool {
        if !down {
            return self.selecting.release();
        }
        let Some(pane) = find(&self.panes, self.focused) else {
            return false;
        };
        let owned = selection::owns_press(pane.frame.mouse, self.modifiers);
        if let Some(at) = self.pointer_position().filter(|_| owned) {
            self.selecting.press(at);
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
        let Some(screen) = self.hovered() else { return };
        let (pane, frame) = (screen.pane, &screen.frame);
        let key = Key::Named(if lines > 0.0 {
            NamedKey::ArrowUp
        } else {
            NamedKey::ArrowDown
        });
        for _ in 0..lines.abs() as u32 {
            if frame.mouse != MouseMode::Off {
                self.report(mouse::Event::Wheel(lines > 0.0), screen);
            } else if frame.alternate_scroll && self.vi.is_none() {
                let empty = ModifiersState::empty();
                let arrow = input::encode(&key, None, empty, frame.app_cursor, false);
                self.request(Request::Input(pane, arrow));
            } else {
                let lines = if lines > 0.0 { 1 } else { -1 };
                self.core(proto::Request::Scroll { pane, lines });
            }
        }
    }

    fn ime(&mut self, event: Ime) {
        match event {
            Ime::Preedit(text, caret) => {
                // Composing is typing, and it draws at a cursor that history hides.
                // shortcut: with the bar open it still draws at the pane's cursor, not the bar's.
                if !text.is_empty() && self.bar.is_none() && self.vi.is_none() {
                    let pane = self.focused;
                    let lines = i32::MIN;
                    self.core(proto::Request::Scroll { pane, lines });
                }
                self.preedit.1 = caret.map_or(text.len(), |(start, _)| start);
                self.preedit.0 = text;
            }
            Ime::Commit(text) => {
                self.preedit.0.clear();
                if !self.type_in_bar(&text) && self.prompt.is_none() && self.vi.is_none() {
                    self.request(Request::Input(self.focused, text.into_bytes()));
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
            self.request(Request::Opened);
            self.resized(size);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, notice: Notice) {
        match notice {
            Notice::Frame => self.redraw_soon(),
            Notice::Searched(pane, found) => self.searched(pane, found),
            // Blank cells copy as nothing, which must not empty the clipboard.
            Notice::Copied(text) if text.is_empty() => {}
            Notice::Copied(text) => {
                if let Some(clipboard) = self.clipboard() {
                    clipboard.set_text(text);
                }
            }
            Notice::Refused(reason) => {
                self.flash.refused(reason, Instant::now());
                self.redraw_soon();
            }
            Notice::Exited => event_loop.exit(),
            Notice::Ended => {
                self.ended = true;
                self.redraw_soon();
            }
            Notice::Raise => {
                if let Some(open) = &self.open {
                    // Winit will not focus a minimized window.
                    open.window.set_minimized(false);
                    open.window.focus_window();
                }
            }
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
            WindowEvent::Focused(focused) => {
                self.has_focus = focused;
                self.redraw_soon();
                if !focused {
                    self.prefix.cancel();
                }
            }
            WindowEvent::CursorLeft { .. } => self.hover(None),
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
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                self.key(&event, event_loop);
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.redraw() {
                    self.result = Err(error);
                    event_loop.exit();
                }
                let wake = self
                    .flash
                    .until()
                    .map_or(self.minute, |flash| flash.min(self.minute));
                event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
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
        // shortcut: only the log hears this, so no one sees it, draw it in the chrome.
        let _ = writeln!(
            io::stderr(),
            "pinfold: using the defaults, not {}: {error}",
            path.display()
        );
        Config::default()
    })
}

/// Opens a window on the core at the other end of `link`, and returns when the window closes
/// or the session's last pane does. A core that goes away leaves the window open and saying so.
///
/// `size` is an optional inner size in physical pixels.
///
/// # Errors
///
/// Returns an error if the window, font, surface or link cannot be set up.
pub fn run(link: OwnedFd, size: Option<(u32, u32)>) -> Result<(), Error> {
    let link = Link::silent(link)?;
    let waker = link.waker();
    let (outbox, inbox) = outbox::channel();
    let latest = Arc::new(Latest::default());
    let event_loop = EventLoop::with_user_event().build()?;
    let (proxy, shared) = (event_loop.create_proxy(), latest.clone());
    thread::Builder::new()
        .name("pinfold-io".into())
        .spawn(move || run_io(link, &inbox, &shared, &proxy))?;
    // Before the file is first read, so a save between the two is not missed.
    let proxy = event_loop.create_proxy();
    reload::watch(move |config| proxy.send_event(Notice::Config(config)).is_ok())?;
    let config = config();
    let mut app = App {
        prefix: Prefix::new(config.keys.prefix),
        config,
        size,
        outbox,
        latest,
        waker,
        open: None,
        panes: Vec::new(),
        rects: Vec::new(),
        focused: 0,
        total: 0,
        area: (0, 0),
        modifiers: ModifiersState::empty(),
        preedit: (String::new(), 0),
        composed: Frame::default(),
        labels: Vec::new(),
        active: 0,
        activity: 0,
        over_tab: None,
        has_focus: true,
        on_title: false,
        minute: Instant::now(),
        on_bar: false,
        clicks: Clicks::default(),
        ime_cursor: None,
        pointer: (0, 0),
        buttons: [false; 3],
        wheel: 0.0,
        clipboard: None,
        prompt: None,
        flash: Flash::default(),
        selecting: Selecting::default(),
        bar: None,
        vi: None,
        ended: false,
        result: Ok(()),
    };
    event_loop.run_app(&mut app)?;
    app.result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp)]
    fn only_a_measured_window_on_macos_changes_its_font_scale() {
        assert_eq!(font_scale(None, 1.0), 1.0);
        assert_eq!(font_scale(None, 2.0), 2.0);
        assert_eq!(font_scale(Some((1, 1)), 3.0), 3.0);
        let measured = if cfg!(target_os = "macos") { 2.0 } else { 1.0 };
        assert_eq!(font_scale(Some((1, 1)), 1.0), measured);
    }
}
