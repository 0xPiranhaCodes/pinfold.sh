//! The macOS menu bar, built once at startup (ADR-0033).
//!
//! Items either run a value of the caller's, through `run`, or are one of AppKit's own, which
//! go to the key window or the app. AppKit asks `enabled` each time it shows a menu or matches
//! a key equivalent, so a greyed item's key reaches the window as a plain key press.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
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
    let target = Target::alloc(mtm).set_ivars(callbacks);
    // SAFETY: NSObject's init takes no arguments and returns the initialised object.
    let target: Retained<Target> = unsafe { msg_send![super(target), init] };
    let bar = NSMenu::new(mtm);
    let mut tag = 0;
    for (title, items) in menus {
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
        for &(title, key, action) in *items {
            let selector = match action {
                Action::Run(_) => sel!(run:),
                Action::About => sel!(orderFrontStandardAboutPanel:),
                Action::Hide => sel!(hide:),
                Action::Minimize => sel!(performMiniaturize:),
                Action::FullScreen => sel!(toggleFullScreen:),
                Action::Separator => {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                    continue;
                }
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
