//! The macOS menu bar, built once at startup, and the Dock menu, built each time it is shown
//! (ADR-0033).
//!
//! Items either run a value of the caller's, through `run`, or are one of AppKit's own, which
//! go to the key window or the app. AppKit asks `enabled` each time it shows a menu or matches
//! a key equivalent, so a greyed item's key reaches the window as a plain key press.

use std::cell::RefCell;
use std::ffi::CStr;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Imp, NSObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, ffi, msg_send, sel};
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
use objc2_foundation::NSString;

/// What an item does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action<T> {
    /// Hands this to `run`.
    Run(T),
    /// The standard About panel.
    About,
    /// Hides the app.
    Hide,
    /// Minimizes the key window.
    Minimize,
    /// Turns full screen on or off for the key window.
    FullScreen,
    /// A line between groups. Its title and key are ignored.
    Separator,
}

/// An item: its title, its key equivalent with Command, and its action. An uppercase key adds
/// Shift, as AppKit reads it, and an empty one has no key.
pub type Item<T> = (&'static str, &'static str, Action<T>);

/// A menu's title and items.
pub type Menu<T> = (&'static str, &'static [Item<T>]);

/// Keeps the menu bar's target alive, which items hold only weakly.
pub struct MenuBar {
    _target: Retained<Target>,
}

/// The menu bar's callbacks, by item tag.
struct Callbacks {
    run: Box<dyn Fn(isize)>,
    enabled: Box<dyn Fn(isize) -> bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Target implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PinFoldMenuTarget"]
    #[ivars = Callbacks]
    // Receives every item that runs a caller's value.
    struct Target;

    impl Target {
        // SAFETY: an action takes its sender, which for these is always an NSMenuItem.
        #[unsafe(method(run:))]
        fn run(&self, item: &NSMenuItem) {
            (self.ivars().run)(item.tag());
        }

        // SAFETY: the signature is NSMenuItemValidation's.
        #[unsafe(method(validateMenuItem:))]
        fn validate(&self, item: &NSMenuItem) -> bool {
            item.action() != Some(sel!(run:)) || (self.ivars().enabled)(item.tag())
        }
    }
);

fn item(
    mtm: MainThreadMarker,
    title: &str,
    key: &str,
    action: Option<Sel>,
) -> Retained<NSMenuItem> {
    let (title, key) = (NSString::from_str(title), NSString::from_str(key));
    // SAFETY: every selector passed here is a valid action that takes a sender.
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &title, action, &key)
    }
}

/// The selector of an item with `action`, or none for a separator.
fn selector<T>(action: &Action<T>) -> Option<Sel> {
    match action {
        Action::Run(_) => Some(sel!(run:)),
        Action::About => Some(sel!(orderFrontStandardAboutPanel:)),
        Action::Hide => Some(sel!(hide:)),
        Action::Minimize => Some(sel!(performMiniaturize:)),
        Action::FullScreen => Some(sel!(toggleFullScreen:)),
        Action::Separator => None,
    }
}

fn target(mtm: MainThreadMarker, callbacks: Callbacks) -> Retained<Target> {
    let target = Target::alloc(mtm).set_ivars(callbacks);
    // SAFETY: NSObject's init takes no arguments and returns the initialised object.
    unsafe { msg_send![super(target), init] }
}

/// Builds `menus` as the app's menu bar. `run` gets an item's value when it is chosen, and
/// `enabled` says whether it can be. Returns nothing off the main thread.
///
/// Call it after the app has launched, or AppKit's own setup may replace it.
pub fn install<T: Copy + 'static>(
    menus: &[Menu<T>],
    run: impl Fn(T) + 'static,
    enabled: impl Fn(T) -> bool + 'static,
) -> Option<MenuBar> {
    let mtm = MainThreadMarker::new()?;
    let values: Vec<T> = menus
        .iter()
        .flat_map(|(_, items)| items.iter())
        .filter_map(|(_, _, action)| match action {
            Action::Run(value) => Some(*value),
            _ => None,
        })
        .collect();
    let values: std::rc::Rc<[T]> = values.into();
    let shared = values.clone();
    let callbacks = Callbacks {
        run: Box::new(move |tag| {
            if let Some(value) = usize::try_from(tag).ok().and_then(|tag| values.get(tag)) {
                run(*value);
            }
        }),
        enabled: Box::new(move |tag| {
            usize::try_from(tag)
                .ok()
                .and_then(|tag| shared.get(tag))
                .is_some_and(|value| enabled(*value))
        }),
    };
    let target = target(mtm, callbacks);
    let bar = NSMenu::new(mtm);
    let mut tag = 0;
    for (title, items) in menus {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for &(title, key, action) in *items {
            let Some(selector) = selector(&action) else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let entry = item(mtm, title, key, Some(selector));
            if let Action::Run(_) = action {
                entry.setTag(tag);
                tag += 1;
                let target: &AnyObject = &target;
                // SAFETY: the target answers `run:`, and MenuBar keeps it alive.
                unsafe { entry.setTarget(Some(target)) };
            }
            menu.addItem(&entry);
        }
        let holder = item(mtm, title, "", None);
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
    }
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&bar));
    Some(MenuBar { _target: target })
}

/// winit 0.30's app delegate class, which AppKit asks for the Dock menu.
const DELEGATE: &CStr = c"WinitApplicationDelegate";

/// Why the Dock menu was not installed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoDock {
    /// It was not asked for on the main thread.
    OffMainThread,
    /// winit's app delegate class is not registered, so winit has changed.
    NoDelegate,
    /// The delegate already answers `applicationDockMenu:`, which is never overwritten.
    Answered,
}

/// Builds the Dock menu.
type Build = Box<dyn Fn() -> Retained<NSMenu>>;

thread_local! {
    /// Set on the main thread, where AppKit asks for the Dock menu.
    static DOCK: RefCell<Option<Build>> = RefCell::new(None);
}

/// Answers `applicationDockMenu:` for winit's delegate. AppKit calls it on the main thread
/// each time the Dock icon is right-clicked; anywhere else it answers nil, AppKit's default.
extern "C-unwind" fn dock_menu(_this: &AnyObject, _cmd: Sel, _app: &AnyObject) -> *mut NSMenu {
    DOCK.with(|dock| {
        let dock = dock.try_borrow().ok();
        let menu = dock
            .as_ref()
            .and_then(|dock| dock.as_ref())
            .map(|build| build());
        menu.map_or(std::ptr::null_mut(), Retained::autorelease_return)
    })
}

/// Gives the app a Dock menu of `items`, asked for afresh each time the Dock icon is
/// right-clicked, so they are always current. `run` gets an item's value when it is chosen.
///
/// winit owns the app delegate and has no Dock menu API, so this adds `applicationDockMenu:`
/// to winit's delegate class (spike m3-dock-menu). Call it once, on the main thread, after
/// the event loop is built.
///
/// # Errors
///
/// Returns why nothing was installed: off the main thread, winit's delegate class missing,
/// or a delegate that already answers, which is left alone.
pub fn install_dock<T: Copy + 'static>(
    items: impl Fn() -> Vec<(String, Action<T>)> + 'static,
    run: impl Fn(T) + 'static,
) -> Result<(), NoDock> {
    let mtm = MainThreadMarker::new().ok_or(NoDock::OffMainThread)?;
    let class = AnyClass::get(DELEGATE).ok_or(NoDock::NoDelegate)?;
    let values: Rc<RefCell<Vec<T>>> = Rc::default();
    let shown = values.clone();
    let callbacks = Callbacks {
        run: Box::new(move |tag| {
            let value = usize::try_from(tag)
                .ok()
                .and_then(|tag| shown.try_borrow().ok()?.get(tag).copied());
            if let Some(value) = value {
                run(value);
            }
        }),
        enabled: Box::new(|_| true),
    };
    let target = target(mtm, callbacks);
    let build = move || {
        let menu = NSMenu::new(mtm);
        let mut tags = Vec::new();
        for (title, action) in items() {
            let Some(selector) = selector(&action) else {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            };
            let entry = item(mtm, &title, "", Some(selector));
            if let Action::Run(value) = action {
                entry.setTag(isize::try_from(tags.len()).unwrap_or(isize::MAX));
                tags.push(value);
                let target: &AnyObject = &target;
                // SAFETY: the target answers `run:`, and the builder, which owns it, lives in
                // DOCK for the rest of the process.
                unsafe { entry.setTarget(Some(target)) };
            }
            menu.addItem(&entry);
        }
        if let Ok(mut values) = values.try_borrow_mut() {
            *values = tags;
        }
        menu
    };
    let function: extern "C-unwind" fn(&AnyObject, Sel, &AnyObject) -> *mut NSMenu = dock_menu;
    // SAFETY: an IMP is any function pointer called with the method's arguments, and these
    // match the type encoding below: an object back, then self, _cmd and an object.
    let imp: Imp = unsafe { std::mem::transmute(function) };
    let types = c"@@:@";
    // SAFETY: the class is registered, the selector and types are valid, and
    // `class_addMethod` returns false instead of replacing a method the class already has.
    let added = unsafe {
        ffi::class_addMethod(
            std::ptr::from_ref(class).cast_mut(),
            sel!(applicationDockMenu:),
            imp,
            types.as_ptr(),
        )
    };
    if !added.as_bool() {
        return Err(NoDock::Answered);
    }
    // AppKit asks on this thread, and only once the event loop runs.
    DOCK.with(|dock| dock.replace(Some(Box::new(build))));
    Ok(())
}
