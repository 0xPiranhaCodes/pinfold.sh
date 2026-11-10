//! One window on the core's session.
//!
//! The core owns the PTYs and the tabs, and the I/O thread here owns the link to it (ADR-0028).
//! That thread hands the window copies of the panes the active tab shows, so a window that dies
//! leaves them running, and a hidden tab costs nothing here.

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use mio::Waker;
use pinfold_config::theme::{self, GRAPHITE, Theme};
use pinfold_config::{Config, MAX_FONT_SIZE, MIN_FONT_SIZE};
use pinfold_proto::link::{self as proto, MAX_DIMENSION, MAX_PASTE, Position, Rect, Selection};
use pinfold_proto::{Frame, MouseMode};
use pinfold_render::cpu::{CpuRenderer, Renderer};
use pinfold_render::font::{Font, FontError};
use pinfold_render::padding::Padding;
use pinfold_sys::clipboard::{Clipboard, Unread};
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

use crate::context_menu::{self as menu, Choice, Menu};
#[cfg(target_os = "macos")]
use crate::dock_menu;
use crate::flash::{self, Flash};
use crate::hyperlink::{self, Target};
use crate::link::{self, Approval, Link, Screen, Tab};
use crate::notify;
use crate::outbox::{self, Inbox, Outbox, Request};
use crate::palette::{self, Palette};
use crate::paste::Prompt;
use crate::prefix::{self, Command, Prefix};
use crate::search_bar::{Bar, Step};
use crate::selection::Selecting;
use crate::top_bar::{Clicks, Hit};
use crate::vi::{Step as ViStep, Vi, View};
use crate::{
    approval, ime, input, menu_bar, mouse, panes, paste, reload, selection, top_bar, was_running,
};

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
    /// The hyperlink's target at a pane's cell, empty if it has none.
    Linked(u32, Position, String),
    /// A pane's `*.md` names, escaped, one a line.
    Markdown(u32, String),
    /// The core did not carry out a command.
    Refused(proto::Refusal),
    Approval(Approval),
    Exited,
    /// The link ended, so the panes went with the core.
    Ended,
    /// Asked to come to the front.
    Raise,
    /// The core lacks the hardened runtime, and whether it could tell (ADR-0044).
    Unhardened(bool),
    /// The core is too old to take the theme's palette (ADR-0049).
    Unthemed,
    /// The config file or the theme file in use was saved, and this is the theme now.
    Theme(Theme),
    /// The config file was saved, and this is what it now says.
    Config(Config),
    /// A menu item was chosen.
    Command(Command),
    /// A restored pane, whether the core would run what it was running, and that command.
    WasRunning(u32, bool, String),
}

/// What the I/O thread has for the window. A screen marked dirty here is one the window has not
/// taken yet.
#[derive(Default)]
struct Shown {
    screens: Vec<Screen>,
    tabs: Vec<Tab>,
    active: usize,
    activity: u32,
    alerts: u32,
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
            ours.status = tab.status;
            ours.message.clone_from(&tab.message);
        }
        shown.active = link.active();
        shown.activity = link.activity();
        shown.alerts = link.alerts();
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
            ours.colours = screen.colours;
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
        let linked = link
            .take_linked()
            .map(|(pane, at, uri)| Notice::Linked(pane, at, uri));
        let markdown = (link.take_markdown()).map(|(pane, names)| Notice::Markdown(pane, names));
        let raised = link.take_raised().then_some(Notice::Raise);
        let unhardened = link.take_unhardened().map(Notice::Unhardened);
        let refused = link.take_refused().map(Notice::Refused);
        let unthemed = link.take_unthemed().then_some(Notice::Unthemed);
        while let Some(approval) = link.take_approval() {
            if proxy.send_event(Notice::Approval(approval)).is_err() {
                return;
            }
        }
        let ran = link.take_was_running().into_iter();
        let ran = ran.map(|(pane, runnable, command)| Notice::WasRunning(pane, runnable, command));
        let notices = [
            searched, copied, linked, markdown, raised, unhardened, refused, unthemed,
        ]
        .into_iter()
        .flatten();
        for notice in notices.chain(ran) {
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
    /// The cursor colours the renderer last got, which follow the focused pane.
    colours: Option<(u32, u32)>,
}

impl Open {
    fn padding(&self) -> Padding {
        let scale = self.window.scale_factor();
        let padding = Padding::scaled(scale);
        if self.fullscreen {
            padding.in_fullscreen(scale)
        } else {
            #[cfg(target_os = "macos")]
            if let Some(controls) = pinfold_sys::window::controls(&*self.window) {
                return padding.aligned_title(
                    controls.left * scale,
                    controls.center_y * scale,
                    self.renderer.title_center(),
                );
            }
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

    /// Hands the renderer the theme's background, for the clear, the padding and the glass,
    /// and its cursor.
    fn apply_theme(&mut self, theme: &Theme) {
        self.colours = cursor_of(theme, None);
        self.renderer.set_colours(theme.background, self.colours);
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

/// What `theme` asks of its text and background drawn the other way round, a cursor or a
/// selection: its `ground` and `text`, or none where they are the theme's foreground and
/// background, which inverts each cell as before themes (ADR-0049, section 1).
fn over(theme: &Theme, ground: u32, text: u32) -> Option<(u32, u32)> {
    ((ground, text) != (theme.foreground, theme.background)).then_some((ground, text))
}

/// The cursor's background and text: `set`, a pane's OSC 12 colour, under the theme's cursor
/// text, or else the theme's own, which is none where it inverts (ADR-0049, section 4).
fn cursor_of(theme: &Theme, set: Option<u32>) -> Option<(u32, u32)> {
    set.map(|colour| (colour, theme.cursor_text))
        .or_else(|| over(theme, theme.cursor, theme.cursor_text))
}

/// The 18 colours of `theme` the core resolves pane colours through.
fn palette_of(theme: &Theme) -> pinfold_proto::Palette {
    pinfold_proto::Palette {
        background: theme.background,
        foreground: theme.foreground,
        ansi: [
            theme.black,
            theme.red,
            theme.green,
            theme.yellow,
            theme.blue,
            theme.magenta,
            theme.cyan,
            theme.white,
            theme.bright_black,
            theme.bright_red,
            theme.bright_green,
            theme.bright_yellow,
            theme.bright_blue,
            theme.bright_magenta,
            theme.bright_cyan,
            theme.bright_white,
        ],
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
    /// The one theme the window draws from (ADR-0049).
    theme: Theme,
    /// Whether the core is too old for a palette, so the theme stays Graphite Grid.
    unthemed: bool,
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
    /// Which tabs have a mark, news or a state, one bit each.
    activity: u32,
    /// Each tab's state, as its pill shows it.
    states: Vec<u8>,
    /// The active tab's state, whether its agent reported it, and its message.
    status: Option<(u8, bool, String)>,
    /// Set for the session once the core says it lacks the hardened runtime: whether it could
    /// tell. The bar keeps a marker while it is set.
    unhardened: Option<bool>,
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
    /// What restored panes were running, until answered. The focused one's strip takes the next
    /// key.
    was_running: was_running::Strips,
    /// Cross-pane prompts, oldest first. Updates replace a batch in place.
    approvals: approval::Queue,
    /// The right-click menu, while open. Every key and press goes to it.
    menu: Option<Menu>,
    /// The command palette, while open. Every key goes to it.
    palette: Option<Palette>,
    /// The pane whose `*.md` files the open palette lists, if it lists them.
    picking: Option<u32>,
    /// The pane and cell the open menu asked the core for a hyperlink at.
    asking: Option<(u32, Position)>,
    prefix: Prefix,
    flash: Flash,
    selecting: Selecting,
    /// The search bar, while open. Keys go to this before the program.
    bar: Option<Bar>,
    /// Vi mode, while on. Keys go to this or the bar, and none to the program.
    vi: Option<Vi>,
    /// Whether the core has gone. The last screen stays, under a row that says so.
    ended: bool,
    menu_bar: menu_bar::Bar,
    #[cfg(target_os = "macos")]
    dock: dock_menu::Dock,
    notifier: notify::Notifier,
    /// Points added to the config's font size by Bigger and Smaller, until the next save.
    font_step: f32,
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
    // Linux has no Dock, so one fewer.
    #[cfg_attr(
        target_os = "macos",
        expect(clippy::too_many_arguments, reason = "the parts the event loop builds")
    )]
    fn new(
        config: Config,
        size: Option<(u32, u32)>,
        outbox: Outbox,
        latest: Arc<Latest>,
        waker: Arc<Waker>,
        menu_bar: menu_bar::Bar,
        #[cfg(target_os = "macos")] dock: dock_menu::Dock,
        notifier: notify::Notifier,
    ) -> Self {
        Self {
            prefix: Prefix::new(config.keys.prefix),
            config,
            theme: GRAPHITE,
            unthemed: false,
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
            states: Vec::new(),
            status: None,
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
            was_running: was_running::Strips::default(),
            approvals: {
                // Matches `has_focus` below; `Focused` events keep them in step.
                let mut queue = approval::Queue::default();
                queue.focus(true);
                queue
            },
            menu: None,
            palette: None,
            picking: None,
            asking: None,
            flash: Flash::default(),
            unhardened: None,
            selecting: Selecting::default(),
            bar: None,
            vi: None,
            ended: false,
            menu_bar,
            #[cfg(target_os = "macos")]
            dock,
            notifier,
            font_step: 0.0,
            result: Ok(()),
        }
    }

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
        let size = self.font_size() * font_scale(self.size, window.scale_factor());
        let renderer = renderer(&window, size, self.config.font_path.as_deref())?;
        let mut open = Open {
            window,
            renderer,
            fullscreen: false,
            colours: None,
        };
        open.apply_background(&self.config);
        open.apply_theme(&self.theme);
        self.open = Some(open);
        Ok(())
    }

    /// Draws everything in Graphite Grid, as the panes of a core too old for a palette are
    /// (ADR-0049, section 3).
    fn untheme(&mut self) {
        self.unthemed = true;
        if self.theme == GRAPHITE {
            return;
        }
        let _ = writeln!(
            io::stderr(),
            "pinfold: drawing Graphite Grid, not the theme: themes need the core updated"
        );
        self.theme = GRAPHITE;
        if let Some(open) = &mut self.open {
            open.apply_theme(&self.theme);
        }
        self.redraw_soon();
    }

    /// Draws from `theme`, saved while the window was open, and sends the core its palette
    /// (ADR-0049, section 2).
    fn retheme(&mut self, theme: Theme) {
        if self.unthemed || theme == self.theme {
            return;
        }
        self.theme = theme;
        let palette = proto::Request::Palette(palette_of(&self.theme));
        self.outbox.send(Request::Core(palette));
        if let Some(open) = &mut self.open {
            open.apply_theme(&self.theme);
        }
        self.redraw_soon();
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
        // `cargo xtask mem-gate` reads a measured window with its right-click menu open.
        if self.size.is_some() && std::env::var_os("PINFOLD_MEASURE_MENU").is_some() {
            let middle = (self.area.0 / 2, self.area.1 / 2);
            self.menu = Some(Menu::open(middle, self.area, None));
        }
    }

    /// Redraws the font for a new display. Winit sends `Resized` after this only sometimes.
    fn scale_changed(&mut self, scale: f64) {
        let points = self.font_size() * font_scale(self.size, scale);
        let Some(open) = &mut self.open else { return };
        open.renderer.set_font_size(points);
        let size = open.window.inner_size();
        self.ime_cursor = None;
        self.resized(size);
    }

    fn font_size(&self) -> f32 {
        (self.config.font_size + self.font_step).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// Applies a saved config. The window keeps its size, so the grid gets new dimensions.
    fn configure(&mut self, config: Config) {
        if config == self.config {
            return;
        }
        self.font_step = 0.0;
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
        let tabs = shown.tabs.iter().enumerate();
        let old = std::mem::take(&mut self.states);
        self.states
            .extend(tabs.map(|(index, tab)| top_bar::tab_state(tab.status, shown.alerts, index)));
        let tabs = notify::Tabs {
            old: &old,
            new: &self.states,
            labels: &self.labels,
            active: self.active,
            focused: self.has_focus,
        };
        let pane = |index: usize| shown.tabs.get(index).map_or(u32::MAX, |tab| tab.pane);
        self.notifier.changed(tabs, pane, Instant::now());
        self.activity = shown.activity | top_bar::marked(&self.states);
        #[cfg(target_os = "macos")]
        self.dock.update(&self.labels, &self.states);
        let active = shown.tabs.get(shown.active);
        self.status =
            active.and_then(|tab| Some((tab.status?.0, tab.status?.1, tab.message.clone())));
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
        self.menu = None;
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
            pane.colours = fresh.colours;
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
            let theme = &self.theme;
            panes::clear(&mut self.composed, rows, columns, theme.background);
            for pane in &self.panes {
                panes::blit(&mut self.composed, &pane.frame, pane.rect);
                if let Some(ground) = pane.colours.1 {
                    panes::glaze(&mut self.composed, pane.rect, ground);
                }
            }
            let focused = find(&self.panes, self.focused);
            // The cursor is the focused pane's, so its OSC 12 colour draws it.
            let cursor = cursor_of(theme, focused.and_then(|pane| pane.colours.0));
            if open.colours != cursor {
                open.colours = cursor;
                open.renderer.set_colours(theme.background, cursor);
            }
            panes::dividers(
                &mut self.composed,
                &self.rects,
                focused.map(|pane| pane.rect),
                theme,
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
                    let colours = over(
                        theme,
                        theme.selection_background,
                        theme.selection_foreground,
                    );
                    selection::overlay(&mut self.composed, rect, top, selection, colours);
                }
                let end = rect.x + rect.columns;
                ime::overlay(&mut self.composed, end, &self.preedit.0, self.preedit.1);
                if let Some(vi) = &self.vi {
                    self.composed.cursor = panes::cursor(rect, vi.drawn(top, rect.rows));
                }
            }
            if let Some(menu) = &self.menu {
                menu.draw(&mut self.composed, theme);
                self.composed.cursor = None;
            }
            if let Some(palette) = &self.palette {
                palette.draw(&mut self.composed, theme);
            }
            if self.ended {
                flash::ended(&mut self.composed, theme);
            } else if !self.approvals.is_empty() {
                self.approvals
                    .draw(&mut self.composed, Instant::now(), theme);
                self.was_running.hide();
            } else if let Some(prompt) = &self.prompt {
                paste::banner(&mut self.composed, prompt, theme);
                self.was_running.hide();
            } else if (self.was_running).draw(
                &mut self.composed,
                self.focused,
                Instant::now(),
                theme,
            ) {
                // The strip is drawn.
            } else if let Some(bar) = &self.bar {
                bar.draw(&mut self.composed, theme);
            } else if let Some(vi) = self.vi.filter(|_| self.flash.until().is_none()) {
                vi.draw(&mut self.composed, theme);
            } else {
                self.flash.banner(&mut self.composed, theme);
            }
            let (time, minute_left) = top_bar::now();
            self.minute = Instant::now() + minute_left;
            let scale = open.window.scale_factor();
            let cell_width = open.renderer.cell_size().0;
            let left = open.padding().left;
            let title_left = top_bar::LEFT_POINTS;
            #[cfg(target_os = "macos")]
            let title_left = pinfold_sys::window::controls(&*open.window)
                .map_or(title_left, |controls| controls.right + 12.0);
            let inset = top_bar::inset(title_left, scale, left, cell_width);
            let bar = top_bar::Bar {
                tabs: &self.labels,
                active: self.active,
                activity: self.activity,
                states: &self.states,
                status: (self.status.as_ref())
                    .map(|(state, reported, message)| (*state, *reported, message.as_str())),
                hovered: self.over_tab,
                focused: self.has_focus,
                title: !open.fullscreen,
                time,
                inset,
                unhardened: self.unhardened,
                theme,
            };
            top_bar::draw(&mut self.composed, &bar);
            open.renderer.draw(&self.composed)?;
        }
        let size = self.font_size();
        self.menu_bar.update(menu_bar::State {
            selection: self.selecting.current.is_some(),
            tabs: self.labels.len(),
            panes: self.panes.len(),
            bigger: size < MAX_FONT_SIZE,
            smaller: size > MIN_FONT_SIZE,
            resized: self.font_step != 0.0,
        });
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
        // The limit bounds the read itself, so a huge clipboard costs the window nothing.
        let text = match self.clipboard().map(|clipboard| clipboard.text(MAX_PASTE)) {
            Some(Ok(text)) => text,
            Some(Err(Unread::TooLarge)) => {
                self.prompt = Some(Prompt::TooLarge);
                return self.redraw_soon();
            }
            Some(Err(Unread::Empty)) | None => return,
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
        // The session stays in the core for the next window.
        if command == Command::Detach {
            return event_loop.exit();
        }
        self.run(command);
    }

    /// [`App::command`] for every command but detach, which needs the event loop.
    fn run(&mut self, command: Command) {
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
            Command::OpenFiles => proto::Request::OpenTool {
                pane,
                tool: proto::Tool::Files,
            },
            Command::OpenDiff => proto::Request::OpenTool {
                pane,
                tool: proto::Tool::Diff,
            },
            // The picker opens when the core answers with the names.
            Command::OpenMarkdown => proto::Request::ListMarkdown { pane },
            Command::Detach => return,
            // A program dies with its pane, so this asks first as tmux does.
            Command::ClosePane => {
                let last = self.total == 1;
                self.prompt = Some(Prompt::Close { pane, last });
                return self.redraw_soon();
            }
            // Only this copies, so output alone can never reach the clipboard.
            Command::Copy => {
                if let Some(selection) = self.selecting.current {
                    self.request(Request::Copy(pane, selection));
                }
                return;
            }
            Command::Paste => return self.paste(),
            Command::SelectAll => return self.select_all(),
            Command::Find => {
                self.bar.get_or_insert_default();
                return self.redraw_soon();
            }
            Command::FontBigger | Command::FontSmaller | Command::FontReset => {
                return self.step_font(command);
            }
            Command::OpenConfig => return open_config(),
            Command::OpenDocs => return launch(DOCS, false),
        };
        self.core(request);
    }

    fn select_all(&mut self) {
        let Some(screen) = find(&self.panes, self.focused) else {
            return;
        };
        let (held, last) = (&screen.held, screen.frame.columns.saturating_sub(1));
        self.selecting.current = Some(Selection {
            anchor: Position {
                row: held.start,
                column: 0,
            },
            focus: Position {
                row: held.end.saturating_sub(1).max(held.start),
                column: last,
            },
        });
        self.redraw_soon();
    }

    fn step_font(&mut self, command: Command) {
        let size = self.font_size();
        self.font_step = match command {
            Command::FontBigger => self.font_step + 1.0,
            Command::FontSmaller => self.font_step - 1.0,
            _ => 0.0,
        };
        // A step past a limit stays at it.
        self.font_step = self.font_size() - self.config.font_size;
        if self.font_size() != size
            && let Some(open) = &self.open
        {
            let scale = open.window.scale_factor();
            self.scale_changed(scale);
        }
    }

    /// Handles a key that belongs to the window whatever else is going on, and returns whether
    /// it did. These come before the prefix, so that an armed prefix cannot swallow a paste.
    fn window_key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) -> bool {
        let key = &event.logical_key;
        if input::is_palette(key, self.modifiers) {
            self.menu = None;
            self.palette = Some(Palette::commands());
            self.picking = None;
            self.redraw_soon();
            return true;
        }
        if input::is_vi(key, self.modifiers) {
            if !event.repeat {
                self.toggle_vi();
            }
            return true;
        }
        // On macOS these are the menu bar's key equivalents, so they must not fire twice.
        let command = if cfg!(target_os = "macos") {
            None
        } else if input::is_paste(key, self.modifiers) {
            Some(Command::Paste)
        } else if input::is_copy(key, self.modifiers) {
            Some(Command::Copy)
        } else if input::is_find(key, self.modifiers) {
            Some(Command::Find)
        } else {
            None
        };
        let Some(command) = command else {
            return false;
        };
        if !event.repeat {
            self.command(event_loop, command);
        }
        true
    }

    /// Opens the markdown picker on the names the core listed for `pane`: only for the pane
    /// still focused, and never over a prompt or another palette.
    fn pick_markdown(&mut self, pane: u32, names: &str) {
        let free = self.prompt.is_none() && self.approvals.is_empty();
        if pane == self.focused && free && self.palette.is_none() {
            self.menu = None;
            self.palette = Some(Palette::markdown(names));
            self.picking = Some(pane);
            self.redraw_soon();
        }
    }

    /// Hands a key to the open palette, and returns the command it chose. Ctrl+Shift+P again
    /// closes it.
    fn palette_key(&mut self, key: &Key, text: Option<&str>) -> Option<Command> {
        let palette = self.palette.as_mut()?;
        let step = if input::is_palette(key, self.modifiers) {
            palette::Step::Close
        } else {
            palette.key(key, text, self.modifiers)
        };
        self.redraw_soon();
        match step {
            palette::Step::Stay => None,
            palette::Step::Close => {
                self.palette = None;
                None
            }
            palette::Step::Chose(index) => {
                let palette = self.palette.take()?;
                let Some(pane) = self.picking.take() else {
                    return palette::COMMANDS.get(index).map(|&(_, command)| command);
                };
                let name = palette.label(index)?.to_owned();
                self.request(Request::OpenMarkdown(pane, name));
                None
            }
        }
    }

    /// Hands a key to the search bar or vi mode, and returns whether either took it.
    fn shortcut(&mut self, key: &Key, text: Option<&str>) -> bool {
        let bar = self.bar.as_mut().filter(|_| self.preedit.0.is_empty());
        match bar.and_then(|bar| bar.key(key, text, self.modifiers)) {
            None => return self.vi_key(key),
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
        if self.answer_strip(event) {
            return;
        }
        if self.window_key(event, event_loop) {
            self.prefix.cancel();
            return;
        }
        let text = event.text.as_ref().map(|text| text.as_str());
        if let Some(command) = self.pane_key(&event.logical_key, text, event.repeat) {
            self.command(event_loop, command);
        }
    }

    /// [`App::key`] from the prefix on, and the command the prefix ran. Every pane, a tool's
    /// too, gets only what the prefix, the search bar and vi mode pass on (ADR-0045).
    fn pane_key(&mut self, key: &Key, text: Option<&str>, repeat: bool) -> Option<Command> {
        let composing = !self.preedit.0.is_empty();
        if !composing {
            match self.prefix.key(key, self.modifiers, repeat) {
                prefix::Step::Pass => {}
                prefix::Step::Swallow => return None,
                prefix::Step::Run(command) => return Some(command),
            }
        }
        if self.shortcut(key, text) || composing {
            return None;
        }
        let pane = find(&self.panes, self.focused)?;
        let (app_cursor, alt) = (pane.frame.app_cursor, self.config.macos_option_as_alt);
        let bytes = input::encode(key, text, self.modifiers, app_cursor, alt);
        if !bytes.is_empty() {
            self.request(Request::Input(self.focused, bytes));
        }
        None
    }

    /// Answers the focused pane's strip with any key: Enter, once armed and if offered, runs
    /// what it was running, and anything else dismisses it. Returns whether the key was used up,
    /// as Enter and Esc are; any other goes on to the pane.
    fn answer_strip(&mut self, event: &KeyEvent) -> bool {
        let key = &event.logical_key;
        let now = Instant::now();
        match (self.was_running).key(self.focused, key, event.repeat, now, self.area.1) {
            None => false,
            Some(was_running::Answer::Held) => true,
            Some(was_running::Answer::Answered { pane, run, used }) => {
                self.core(proto::Request::WasRunningAnswer { pane, run });
                self.redraw_soon();
                used
            }
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
    fn vi_key(&mut self, key: &Key) -> bool {
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
        let step = vi.key(key, self.modifiers, &view(screen));
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
                Some(Prompt::OpenLink(uri)) => {
                    let (rows, columns) = self.area;
                    if hyperlink::fits(&uri, rows, columns) {
                        launch(&uri, false);
                    } else {
                        self.prompt = Some(Prompt::OpenLink(uri));
                    }
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

    /// Gives a key to the prompt on screen, and returns whether one was up to take it.
    fn prompted(&mut self, key: &Key, state: ElementState, repeat: bool) -> bool {
        // A held key repeating must not answer a prompt it did not see.
        let fresh = state == ElementState::Pressed && !repeat;
        if !self.approvals.is_empty() {
            if fresh {
                self.answer_approval(key);
            }
        } else if self.prompt.is_some() {
            if fresh {
                self.answer(key);
            }
        } else {
            return false;
        }
        true
    }

    fn answer_approval(&mut self, key: &Key) {
        let (rows, columns) = self.area;
        let Some((id, answer)) = self.approvals.key(key, Instant::now(), rows, columns) else {
            return;
        };
        self.core(proto::Request::ApprovalAnswer { id, answer });
        self.redraw_soon();
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
        let columns = top_bar::tab_columns(self.area.1, self.unhardened);
        let over = match top_bar::hit(pointer.1, &self.labels, self.activity, columns) {
            Some(Hit::Tab(tab)) if on_bar && !self.on_title => Some(tab),
            _ => None,
        };
        self.hover(over);
        if self.pointer != pointer {
            self.pointer = pointer;
            if let Some(menu) = &mut self.menu {
                if !on_bar && menu.hover(pointer) {
                    self.redraw_soon();
                }
                return;
            }
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

    /// Carries out what a key or press did to the open menu.
    fn menu_step(&mut self, step: menu::Step, event_loop: &ActiveEventLoop) {
        match step {
            menu::Step::Stay => {}
            menu::Step::Close => self.menu = None,
            menu::Step::Chose(choice) => {
                self.menu = None;
                match choice {
                    Choice::Run(command) => self.command(event_loop, command),
                    Choice::OpenLink(uri) => match hyperlink::check(&uri) {
                        Some(Target::Web) => launch(&uri, false),
                        Some(Target::Ask) => self.prompt = Some(Prompt::OpenLink(uri)),
                        None => {}
                    },
                    Choice::CopyLink(uri) => {
                        if let Some(clipboard) = self.clipboard() {
                            clipboard.set_text(uri);
                        }
                    }
                }
            }
        }
        self.redraw_soon();
    }

    /// Takes a press or release, and returns the step for an open menu it clicked.
    fn pressed(&mut self, button: MouseButton, state: ElementState) -> Option<menu::Step> {
        if !self.approvals.is_empty() {
            // A click cannot answer a prompt, and goes nowhere else while one is up.
            return None;
        }
        let number = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return None,
        };
        let down = state == ElementState::Pressed;
        // shortcut: a press anywhere closes the palette, as Esc does; choose by click if missed.
        if self.palette.is_some() {
            if down {
                self.palette = None;
                self.redraw_soon();
            }
            return None;
        }
        if let Some(menu) = self.menu.as_mut().filter(|_| down) {
            return Some(menu.click((!self.on_bar).then_some(self.pointer)));
        }
        if down && self.on_bar {
            if number == 0 {
                self.bar_pressed();
            }
            return None;
        }
        if down {
            // Borders take no press, and the first press on a pane only focuses it.
            match self.hovered().map(|pane| pane.pane) {
                None => return None,
                Some(pane) if pane != self.focused => {
                    self.core(proto::Request::Focus { pane });
                    return None;
                }
                Some(_) => {}
            }
        }
        if number == 0 && self.select(down) {
            return None;
        }
        // With mouse reporting on, the program gets the press unless Shift is held.
        let mode = find(&self.panes, self.focused).map(|pane| pane.frame.mouse);
        // A prompt keeps every key until it is answered, so no menu opens over one.
        if down
            && number == 2
            && self.prompt.is_none()
            && mode.is_some_and(|mode| selection::owns_press(mode, self.modifiers))
        {
            // The link items join when the core answers, so a slow core cannot hold the menu.
            self.asking = self.pointer_position().map(|at| (self.focused, at));
            if let Some((pane, at)) = self.asking {
                self.core(proto::Request::Link { pane, at });
            }
            self.menu = Some(Menu::open(self.pointer, self.area, None));
            self.redraw_soon();
            return None;
        }
        // The press may have been on the bar, so the program never saw it.
        if !down && !self.buttons[usize::from(number)] {
            return None;
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
        None
    }

    /// Moves or zooms the window from a left press on the top bar's title row, and takes a press
    /// on the tabs' row.
    fn bar_pressed(&mut self) {
        let Some(open) = &self.open else { return };
        if !self.on_title {
            let columns = top_bar::tab_columns(self.area.1, self.unhardened);
            match top_bar::hit(self.pointer.1, &self.labels, self.activity, columns) {
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
                // Like a click, text cannot answer a prompt and goes nowhere else while one is up.
                let prompted = !self.approvals.is_empty() || self.prompt.is_some();
                if !prompted && !self.type_in_bar(&text) && self.vi.is_none() {
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
            Notice::Linked(pane, at, uri) => {
                let asked = self.asking == Some((pane, at));
                let menu = self.menu.as_mut().filter(|_| asked);
                // Copy Link copies only what Open Link would take.
                if let Some(menu) = menu.filter(|_| hyperlink::check(&uri).is_some()) {
                    menu.link(uri);
                    self.redraw_soon();
                }
            }
            Notice::Markdown(pane, names) => self.pick_markdown(pane, &names),
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
            Notice::Unhardened(checked) => {
                self.unhardened = Some(checked);
                self.flash
                    .unhardened(checked, pinfold_sys::sign_command(), Instant::now());
                self.redraw_soon();
            }
            Notice::Approval(approval) => {
                if let Some(id) = self.approvals.add(approval) {
                    let answer = proto::ApprovalAnswer::Deny;
                    self.core(proto::Request::ApprovalAnswer { id, answer });
                }
                self.prefix.cancel();
                self.menu = None;
                self.palette = None;
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
            Notice::Unthemed => self.untheme(),
            Notice::Theme(theme) => self.retheme(theme),
            Notice::Config(config) => self.configure(config),
            Notice::WasRunning(pane, runnable, command) => {
                self.was_running.add(pane, runnable, command);
                self.redraw_soon();
            }
            // A prompt keeps every key until it is answered, menu keys too; only Quit, which
            // leaves the session running, gets past it.
            Notice::Command(command) if self.prompt.is_none() || command == Command::Detach => {
                self.prefix.cancel();
                self.menu = None;
                self.palette = None;
                self.command(event_loop, command);
            }
            Notice::Command(_) => {}
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
                self.was_running.focus(focused);
                self.approvals.focus(focused);
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
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(step) = self.pressed(button, state) {
                    self.menu_step(step, event_loop);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.wheel(delta),
            WindowEvent::KeyboardInput { event, .. } if self.menu.is_some() => {
                if let Some(menu) = self.menu.as_mut().filter(|_| event.state.is_pressed()) {
                    let step = menu.key(&event.logical_key);
                    self.menu_step(step, event_loop);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if self.palette.is_some() => {
                if event.state.is_pressed() {
                    let text = event.text.as_ref().map(|text| text.as_str());
                    if let Some(command) = self.palette_key(&event.logical_key, text) {
                        self.command(event_loop, command);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let prompted = self.prompted(&event.logical_key, event.state, event.repeat);
                if !prompted && event.state == ElementState::Pressed {
                    self.key(&event, event_loop);
                }
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

/// Where Help sends the browser.
const DOCS: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/blob/main/docs/configuration.md"
);

/// Where the opener may live, tried in order. Never found through `PATH`, which whoever
/// started PinFold controls (#298). NixOS has no `/usr/bin/xdg-open`; its system profile
/// links programs from `/run/current-system/sw/bin`, which only root can change.
#[cfg(target_os = "macos")]
const OPENERS: &[&str] = &["/usr/bin/open"];
#[cfg(not(target_os = "macos"))]
const OPENERS: &[&str] = &[
    "/usr/bin/xdg-open",
    "/usr/local/bin/xdg-open",
    "/run/current-system/sw/bin/xdg-open",
];

/// The first of `candidates` that is a regular, executable file once symlinks are followed.
fn opener<'a>(candidates: &[&'a str]) -> Result<&'a str, String> {
    use std::os::unix::fs::PermissionsExt;
    candidates
        .iter()
        .copied()
        .find(|path| {
            std::fs::metadata(path)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
        .ok_or_else(|| format!("no executable opener at {}", candidates.join(", ")))
}

/// The command that opens `target` with the first opener in `candidates`.
fn opener_command(
    candidates: &[&str],
    target: &str,
    text: bool,
) -> Result<std::process::Command, String> {
    let mut command = std::process::Command::new(opener(candidates)?);
    if text && cfg!(target_os = "macos") {
        command.arg("-t");
    }
    // The environment is kept: openers need the session's DISPLAY, WAYLAND_DISPLAY,
    // DBUS_SESSION_BUS_ADDRESS and XDG_* variables to reach the desktop.
    command
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

/// Opens `target`, a file or URL, in the app the desktop picks for it, without waiting.
/// `text` asks macOS for the text editor, as a `.toml` file may have no app of its own.
fn launch(target: &str, text: bool) {
    let spawned = opener_command(OPENERS, target, text)
        .and_then(|mut command| command.spawn().map_err(|error| error.to_string()));
    match spawned {
        // Reaped on a thread of its own, so it leaves no zombie and the window does not wait.
        Ok(mut child) => drop(thread::spawn(move || child.wait())),
        Err(error) => {
            // Not the target, which may be a link that program output chose.
            let _ = writeln!(io::stderr(), "pinfold: cannot start the opener: {error}");
        }
    }
}

/// Opens the config file, made empty first if there is none, so there is something to edit.
fn open_config() {
    let Some(path) = pinfold_config::path() else {
        return;
    };
    let made = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
        });
    match made {
        Ok(_) => launch(&path.to_string_lossy(), true),
        Err(error) => {
            let _ = writeln!(
                io::stderr(),
                "pinfold: cannot make {}: {error}",
                path.display()
            );
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

/// Reads the theme the config file at `path` chooses, with its `[colors]` on top. A theme that
/// cannot be used means Graphite Grid, and the log says why (ADR-0049, section 2).
fn theme_at(path: &Path) -> Theme {
    let (theme, notes) = theme::load(path)
        .unwrap_or_else(|error| (GRAPHITE, vec![format!("using Graphite Grid: {error}")]));
    for note in notes {
        let _ = writeln!(io::stderr(), "pinfold: theme: {note}");
    }
    theme
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
    let mut link = Link::silent(link)?;
    let waker = link.waker();
    let (outbox, inbox) = outbox::channel();
    let latest = Arc::new(Latest::default());
    let mut builder = EventLoop::with_user_event();
    // The menu bar below replaces winit's.
    #[cfg(target_os = "macos")]
    winit::platform::macos::EventLoopBuilderExtMacOS::with_default_menu(&mut builder, false);
    let event_loop = builder.build()?;
    // Before either file is first read, so a save between the two is not missed.
    let proxy = event_loop.create_proxy();
    reload::watch(move |saved| {
        let notice = match saved {
            reload::Saved::Config(config) => Notice::Config(config),
            reload::Saved::Theme(theme) => Notice::Theme(theme),
        };
        proxy.send_event(notice).is_ok()
    })?;
    let theme = pinfold_config::path().map_or(GRAPHITE, |path| theme_at(&path));
    link.theme(palette_of(&theme));
    let (proxy, shared) = (event_loop.create_proxy(), latest.clone());
    thread::Builder::new()
        .name("pinfold-io".into())
        .spawn(move || run_io(link, &inbox, &shared, &proxy))?;
    let proxy = event_loop.create_proxy();
    let menu_bar = menu_bar::Bar::install(move |command| {
        let _ = proxy.send_event(Notice::Command(command));
    });
    #[cfg(target_os = "macos")]
    let dock = {
        let proxy = event_loop.create_proxy();
        dock_menu::Dock::install(move |command| {
            let _ = proxy.send_event(Notice::Command(command));
        })
    };
    let proxy = event_loop.create_proxy();
    let notifier = notify::Notifier::new(move |tab| {
        let _ = proxy.send_event(Notice::Command(Command::Tab(tab)));
    });
    let mut app = App::new(
        config(),
        size,
        outbox,
        latest,
        waker,
        menu_bar,
        #[cfg(target_os = "macos")]
        dock,
        notifier,
    );
    app.theme = theme;
    event_loop.run_app(&mut app)?;
    app.result
}

#[cfg(test)]
mod tests {
    use mio::{Poll, Token};
    use pinfold_proto::link::{ApprovalAction, ApprovalAnswer};

    use super::*;

    /// A fresh folder for an opener test.
    fn opener_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pinfold-opener-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes an executable script at `path` that writes `marker` when run.
    fn script(path: &std::path::Path, marker: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_opener_is_never_found_through_path() {
        let dir = opener_dir("path");
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let (fake, real) = (dir.join("fake-ran"), dir.join("real-ran"));
        let name = OPENERS[0].rsplit('/').next().unwrap();
        script(&bin.join(name), &fake);
        let opener = dir.join("opener");
        script(&opener, &real);
        let status = opener_command(&[opener.to_str().unwrap()], "https://example.com", false)
            .unwrap()
            .env("PATH", &bin)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(real.exists());
        assert!(!fake.exists());
        let command = opener_command(OPENERS, "x", false);
        if let Ok(command) = command {
            assert!(std::path::Path::new(command.get_program()).is_absolute());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_opener_is_the_first_regular_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = opener_dir("pick");
        let missing = dir.join("missing");
        let folder = dir.join("folder");
        std::fs::create_dir(&folder).unwrap();
        let plain = dir.join("plain");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (good, later) = (dir.join("good"), dir.join("later"));
        script(&good, &dir.join("unused"));
        script(&later, &dir.join("unused"));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&folder, &link).unwrap();
        let paths =
            [&missing, &folder, &link, &plain, &good, &later].map(|path| path.to_str().unwrap());
        assert_eq!(opener(&paths), Ok(paths[4]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn no_opener_names_every_path_tried() {
        let error = opener(&["/nonexistent/a/xdg-open", "/nonexistent/b/xdg-open"]).unwrap_err();
        assert_eq!(
            error,
            "no executable opener at /nonexistent/a/xdg-open, /nonexistent/b/xdg-open"
        );
        let Err(error) = opener_command(&["/nonexistent/xdg-open"], "x", false) else {
            panic!("a missing opener gave a command");
        };
        assert!(error.contains("/nonexistent/xdg-open"));
    }

    /// A window with one pane, 1, focused, and no event loop. The poll keeps the waker's queue.
    fn app() -> (App, Inbox, Poll) {
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(0)).unwrap());
        let (outbox, inbox) = outbox::channel();
        let mut app = App::new(
            Config::default(),
            None,
            outbox,
            Arc::default(),
            waker,
            menu_bar::Bar::default(),
            #[cfg(target_os = "macos")]
            dock_menu::Dock::default(),
            notify::Notifier::new(|_| ()),
        );
        let mut screen = Screen::default();
        (screen.pane, screen.rect) = (
            1,
            Rect {
                x: 0,
                y: 0,
                columns: 80,
                rows: 24,
            },
        );
        app.panes.push(screen);
        app.area = (24, 80);
        app.focused = 1;
        (app, inbox, poll)
    }

    fn approval(app: &mut App) {
        let approval = Approval {
            id: 7,
            caller: 2,
            target: 1,
            action: ApprovalAction::Read,
            texts: Vec::new(),
        };
        assert_eq!(app.approvals.add(approval), None);
    }

    #[test]
    fn a_click_behind_an_approval_goes_nowhere() {
        let (mut app, inbox, _poll) = app();
        app.focused = 2;
        approval(&mut app);
        assert!(
            app.pressed(MouseButton::Left, ElementState::Pressed)
                .is_none()
        );
        assert!(inbox.take().is_empty());
        // With no prompt up, the same press focuses the pane under it.
        let (mut app, inbox, _poll) = self::app();
        app.focused = 2;
        assert!(
            app.pressed(MouseButton::Left, ElementState::Pressed)
                .is_none()
        );
        let sent = inbox.take();
        assert!(
            matches!(sent[..], [Request::Core(proto::Request::Focus { pane: 1 })]),
            "{}",
            sent.len()
        );
    }

    #[test]
    fn keys_go_to_an_approval_and_a_held_one_answers_nothing() {
        let (mut app, inbox, _poll) = app();
        assert!(!app.prompted(&Key::Named(NamedKey::Escape), ElementState::Pressed, false));
        approval(&mut app);
        let escape = Key::Named(NamedKey::Escape);
        assert!(app.prompted(&escape, ElementState::Pressed, true));
        assert!(app.prompted(&Key::Character("x".into()), ElementState::Pressed, false));
        assert!(inbox.take().is_empty());
        assert!(app.prompted(&escape, ElementState::Pressed, false));
        let sent = inbox.take();
        let deny = ApprovalAnswer::Deny;
        assert!(
            matches!(
                sent[..],
                [Request::Core(proto::Request::ApprovalAnswer { id: 7, answer })] if answer == deny
            ),
            "{}",
            sent.len()
        );
        assert!(app.approvals.is_empty());
    }

    #[test]
    fn keys_go_to_a_paste_prompt_and_enter_pastes_into_the_focused_pane() {
        let (mut app, inbox, _poll) = app();
        let text = "ls\nrm -rf ~\n".to_owned();
        app.prompt = Some(Prompt::Confirm {
            text,
            lines: 2,
            risk: None,
        });
        let enter = Key::Named(NamedKey::Enter);
        assert!(app.prompted(&Key::Character("y".into()), ElementState::Pressed, false));
        assert!(app.prompted(&enter, ElementState::Released, false));
        assert!(inbox.take().is_empty());
        assert!(app.prompted(&enter, ElementState::Pressed, false));
        let sent = inbox.take();
        assert!(
            matches!(&sent[..], [Request::Paste(1, text)] if text == "ls\nrm -rf ~\n"),
            "{}",
            sent.len()
        );
        assert!(app.prompt.is_none());
    }

    #[test]
    fn input_method_text_behind_a_prompt_goes_nowhere() {
        let (mut app, inbox, _poll) = app();
        approval(&mut app);
        app.ime(Ime::Commit("y".into()));
        app.approvals = approval::Queue::default();
        app.prompt = Some(Prompt::TooLarge);
        app.ime(Ime::Commit("y".into()));
        assert!(inbox.take().is_empty());
        app.prompt = None;
        app.ime(Ime::Commit("é".into()));
        let sent = inbox.take();
        assert!(
            matches!(&sent[..], [Request::Input(1, bytes)] if bytes == "é".as_bytes()),
            "{}",
            sent.len()
        );
    }

    #[test]
    fn the_palette_opens_files_and_diff_and_runs_any_command() {
        let (mut app, inbox, _poll) = app();
        let text = |text: &str| Key::Character(text.into());
        let enter = Key::Named(NamedKey::Enter);
        for (query, tool) in [("files", proto::Tool::Files), ("diff", proto::Tool::Diff)] {
            app.palette = Some(Palette::commands());
            assert_eq!(app.palette_key(&text(query), Some(query)), None);
            let command = app.palette_key(&enter, None).unwrap();
            assert!(app.palette.is_none());
            app.run(command);
            let sent = inbox.take();
            assert!(
                matches!(
                    sent[..],
                    [Request::Core(proto::Request::OpenTool { pane: 1, tool: sent })] if sent == tool
                ),
                "{query}"
            );
        }
        app.palette = Some(Palette::commands());
        app.palette_key(&text("split d"), Some("split d"));
        let command = app.palette_key(&enter, None).unwrap();
        app.run(command);
        let sent = inbox.take();
        assert!(matches!(
            sent[..],
            [Request::Core(proto::Request::Split {
                pane: 1,
                way: proto::SplitWay::Down
            })]
        ));
        // Ctrl+Shift+P again closes it, and nothing reaches the pane.
        app.palette = Some(Palette::commands());
        app.modifiers = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert_eq!(app.palette_key(&text("P"), Some("\u{10}")), None);
        assert!(app.palette.is_none());
        assert!(inbox.take().is_empty());
    }

    #[test]
    fn prefix_m_and_open_markdown_list_then_the_pick_goes_back_by_its_listed_name() {
        let (mut app, inbox, _poll) = app();
        let text = |text: &str| Key::Character(text.into());
        let enter = Key::Named(NamedKey::Enter);
        app.modifiers = ModifiersState::CONTROL;
        assert_eq!(app.pane_key(&text("a"), None, false), None);
        app.modifiers = ModifiersState::empty();
        let command = app.pane_key(&text("m"), None, false).unwrap();
        app.run(command);
        app.palette = Some(Palette::commands());
        app.palette_key(&text("markdown"), Some("markdown"));
        let command = app.palette_key(&enter, None).unwrap();
        app.run(command);
        let sent = inbox.take();
        let list = |request: &Request| {
            matches!(
                request,
                Request::Core(proto::Request::ListMarkdown { pane: 1 })
            )
        };
        assert!(sent.len() == 2 && sent.iter().all(list));
        // A listing for a pane no longer focused opens nothing.
        app.pick_markdown(2, "a.md");
        assert!(app.palette.is_none());
        app.pick_markdown(1, "README.md\nevil<U+202E>dm.md");
        app.palette_key(&text("evil"), Some("evil"));
        assert_eq!(app.palette_key(&enter, None), None);
        assert!(app.palette.is_none() && app.picking.is_none());
        let sent = inbox.take();
        assert!(
            matches!(&sent[..], [Request::OpenMarkdown(1, name)] if name == "evil<U+202E>dm.md")
        );
    }

    #[test]
    fn a_tool_pane_gets_no_window_key_and_f_and_g_open_tools() {
        // Pane 1 runs a tool: keys route as for any pane, so the window's are taken first.
        let (mut app, inbox, _poll) = app();
        let text = |text: &str| Key::Character(text.into());
        let prefixed = |app: &mut App, key: &Key, modifiers| {
            app.modifiers = ModifiersState::CONTROL;
            assert_eq!(app.pane_key(&text("a"), None, false), None);
            app.modifiers = modifiers;
            app.pane_key(key, None, false)
        };
        let left = Key::Named(NamedKey::ArrowLeft);
        let resize = prefixed(&mut app, &left, ModifiersState::ALT);
        assert_eq!(resize, Some(Command::Resize(proto::Side::Left, 5)));
        assert!(inbox.take().is_empty());
        for (key, tool) in [("f", proto::Tool::Files), ("g", proto::Tool::Diff)] {
            let command = prefixed(&mut app, &text(key), ModifiersState::empty()).unwrap();
            app.run(command);
            let sent = inbox.take();
            assert!(
                matches!(
                    sent[..],
                    [Request::Core(proto::Request::OpenTool { pane: 1, tool: sent })] if sent == tool
                ),
                "{key}"
            );
        }
        // Everything else is the tool's, as terminal input.
        assert_eq!(app.pane_key(&text("q"), Some("q"), false), None);
        let sent = inbox.take();
        assert!(matches!(&sent[..], [Request::Input(1, bytes)] if bytes == b"q"));
    }

    /// ADR-0049: a theme set in `pinfold.toml` at start colours the padding, the cursor and the
    /// palette the core is sent, and a core too old for a palette means Graphite Grid.
    #[test]
    fn a_theme_in_the_config_colours_the_ground_the_cursor_and_the_palette() {
        use std::os::unix::fs::PermissionsExt;
        let path = opener_dir("theme").join("pinfold.toml");
        let write = |text: &str| {
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            theme_at(&path)
        };
        let theme = write("[colors]\nbackground = \"#0d1117\"\ncursor = \"#58a6ff\"\n");
        let palette = palette_of(&theme);
        assert_eq!(palette.background, 0x000d_1117);
        assert_eq!(palette.ansi, pinfold_proto::Palette::GRAPHITE_GRID.ansi);
        let cursor = over(&theme, theme.cursor, theme.cursor_text);
        // `[colors]` sits on Graphite Grid, whose cursor text this is.
        assert_eq!(cursor, Some((0x0058_a6ff, GRAPHITE.cursor_text)));

        let (mut app, inbox, _poll) = app();
        // A save while the window is open draws from the new theme and tells the core, once.
        app.retheme(theme);
        app.retheme(theme);
        assert_eq!(app.theme, theme);
        let sent = inbox.take();
        assert!(
            matches!(&sent[..], [Request::Core(proto::Request::Palette(sent))] if *sent == palette)
        );
        app.untheme();
        assert_eq!(app.theme, GRAPHITE);
        // A core too old for a palette keeps Graphite Grid whatever is saved.
        app.retheme(theme);
        assert_eq!(app.theme, GRAPHITE);
        assert!(inbox.take().is_empty());
        // The default's cursor and selection invert each cell, as before themes.
        assert_eq!(over(&GRAPHITE, GRAPHITE.cursor, GRAPHITE.cursor_text), None);
        // A pane's OSC 12 colour draws its cursor under any theme, and OSC 112 gives it back.
        assert_eq!(cursor_of(&theme, None), cursor);
        let set = Some((0x00ff_0000, GRAPHITE.cursor_text));
        assert_eq!(cursor_of(&theme, Some(0x00ff_0000)), set);
        assert_eq!(cursor_of(&GRAPHITE, Some(0x00ff_0000)), set);
        assert_eq!(cursor_of(&GRAPHITE, None), None);
        let (ground, text) = (GRAPHITE.selection_background, GRAPHITE.selection_foreground);
        assert_eq!(over(&GRAPHITE, ground, text), None);
        assert_eq!(palette_of(&GRAPHITE), pinfold_proto::Palette::GRAPHITE_GRID);
        // No theme has a name that is a path, so this one is refused.
        assert_eq!(write("theme = \"../other\""), GRAPHITE);
    }

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
