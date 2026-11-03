//! The Dock menu's delegate method, on the main thread, which AppKit needs: hence no harness.

#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "calls the added method as AppKit does")]
#[allow(clippy::unwrap_used, reason = "a test, without the harness")]
fn main() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use objc2::rc::Retained;
    use objc2::runtime::{ClassBuilder, NSObject};
    use objc2::{ClassType, MainThreadMarker, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSMenu};
    use pinfold_sys::menu::{Action, NoDock, install_dock};

    let off = std::thread::spawn(|| install_dock(Vec::new, |_: u8| {}));
    assert_eq!(off.join().ok(), Some(Err(NoDock::OffMainThread)));
    assert_eq!(install_dock(Vec::new, |_: u8| {}), Err(NoDock::NoDelegate));

    // Stands in for winit's delegate class, which answers nothing about the Dock.
    let builder = ClassBuilder::new(c"WinitApplicationDelegate", NSObject::class());
    let class = builder.map(ClassBuilder::register).unwrap();
    let selector = sel!(applicationDockMenu:);
    assert!(!class.responds_to(selector));

    let state = Rc::new(RefCell::new("●"));
    let ran = Rc::new(RefCell::new(Vec::new()));
    let (shown, record) = (state.clone(), ran.clone());
    let items = move || {
        vec![
            ("New Tab".to_owned(), Action::Run(1)),
            (String::new(), Action::Separator),
            (format!("{} api", shown.borrow()), Action::Run(2)),
        ]
    };
    install_dock(items, move |value: u8| record.borrow_mut().push(value)).unwrap();
    assert!(class.responds_to(selector));
    assert_eq!(
        install_dock(Vec::new, |_: u8| {}),
        Err(NoDock::Answered),
        "an answered selector is never overwritten"
    );

    // SAFETY: the class is a registered NSObject subclass with no ivars, so `new` is NSObject's.
    let delegate: Retained<NSObject> = unsafe { msg_send![class, new] };
    // Items send their action through the app.
    let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
    let ask = || -> Retained<NSMenu> {
        // SAFETY: the method was added above, takes the app (any object) and returns a menu.
        let menu: Option<Retained<NSMenu>> =
            unsafe { msg_send![&delegate, applicationDockMenu: &*app] };
        menu.unwrap()
    };
    let menu = ask();
    let titles: Vec<String> = (0..menu.numberOfItems())
        .filter_map(|index| menu.itemAtIndex(index))
        .map(|item| item.title().to_string())
        .collect();
    assert_eq!(titles, ["New Tab", "", "● api"]);
    for index in [0, 2] {
        menu.performActionForItemAtIndex(index);
    }
    assert_eq!(*ran.borrow(), [1, 2]);

    *state.borrow_mut() = "✓";
    let menu = ask();
    let item = menu.itemAtIndex(2).unwrap();
    assert_eq!(
        item.title().to_string(),
        "✓ api",
        "each request is built afresh"
    );
}

#[cfg(not(target_os = "macos"))]
fn main() {}
