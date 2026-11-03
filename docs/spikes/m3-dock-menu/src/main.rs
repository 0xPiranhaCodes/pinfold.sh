//! Spike for #114: add `applicationDockMenu:` to winit's app delegate class at runtime.
//!
//! `cargo run` opens a window; right-click the Dock icon. Keys in the window cycle tab 0's
//! agent state. `cargo run -- --self-test` asks the delegate for the menu itself, prints the
//! items, "clicks" each one and exits (everything except the Dock's own right-click).

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    mac::run();
}

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::RefCell;
    use std::ffi::c_char;
    use std::sync::{Mutex, OnceLock};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{MainThreadMarker, MainThreadOnly, define_class, ffi, msg_send, sel};
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::{NSObject, NSString};
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::window::{Window, WindowId};

    #[derive(Debug, Clone, Copy)]
    enum Ev {
        NewTab,
        SplitRight,
        SplitDown,
        Detach,
        GoTo(usize),
    }

    /// Tab titles and agent state index into `STATES` (glyph and word, per CLAUDE.md UI).
    static TABS: Mutex<[(&str, usize); 3]> = Mutex::new([("api", 1), ("docs", 0), ("tests", 3)]);
    const STATES: [(&str, &str); 5] = [
        ("◆", "needs you"),
        ("●", "running"),
        ("✓", "done"),
        ("✕", "error"),
        ("○", "idle"),
    ];
    static PROXY: OnceLock<EventLoopProxy<Ev>> = OnceLock::new();

    thread_local! {
        // NSMenuItem holds its target unretained; keep the target alive here.
        static TARGET: RefCell<Option<Retained<Target>>> = const { RefCell::new(None) };
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "SpikeDockTarget"]
        struct Target;

        impl Target {
            #[unsafe(method(dockItem:))]
            fn dock_item(&self, item: &NSMenuItem) {
                let ev = match item.tag() {
                    0 => Ev::NewTab,
                    1 => Ev::SplitRight,
                    2 => Ev::SplitDown,
                    3 => Ev::Detach,
                    n => Ev::GoTo((n - 100) as usize),
                };
                if let Some(p) = PROXY.get() {
                    let _ = p.send_event(ev);
                }
            }
        }
    );

    fn item(mtm: MainThreadMarker, t: &Target, title: &str, tag: isize) -> Retained<NSMenuItem> {
        // SAFETY: `dockItem:` exists on `Target`; the item is main-thread only and just created.
        unsafe {
            let it = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(sel!(dockItem:)),
                &NSString::new(),
            );
            it.setTarget(Some(t));
            it.setTag(tag);
            it
        }
    }

    /// The method added to winit's delegate class. AppKit calls it on the main thread each
    /// time the Dock menu is shown, so tab state is always current.
    extern "C-unwind" fn dock_menu(_this: &AnyObject, _cmd: Sel, _app: &AnyObject) -> *mut NSMenu {
        // SAFETY: AppKit calls delegate methods on the main thread.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let target: Retained<Target> = unsafe { msg_send![mtm.alloc::<Target>(), init] };
        let menu = NSMenu::new(mtm);
        for (i, t) in ["New Tab", "Split Right", "Split Down", "Detach"]
            .iter()
            .enumerate()
        {
            menu.addItem(&item(mtm, &target, t, i as isize));
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for (i, (name, st)) in TABS.lock().unwrap().iter().enumerate() {
            let (g, w) = STATES[*st];
            menu.addItem(&item(
                mtm,
                &target,
                &format!("{g} {name}  {w}"),
                100 + i as isize,
            ));
        }
        TARGET.with(|c| *c.borrow_mut() = Some(target));
        Retained::autorelease_return(menu)
    }

    /// Adds `applicationDockMenu:` to the class winit registered for its delegate.
    fn install() -> bool {
        let cls = AnyClass::get(c"WinitApplicationDelegate").expect("winit delegate class");
        let sel = sel!(applicationDockMenu:);
        // SAFETY: the IMP's signature matches the type encoding "@@:@" (id, self, _cmd, id).
        // winit's delegate does not implement this selector, and class_addMethod returns
        // false rather than overwriting if a later winit does.
        unsafe {
            let imp: Imp = std::mem::transmute(
                dock_menu as extern "C-unwind" fn(&AnyObject, Sel, &AnyObject) -> *mut NSMenu,
            );
            let types: *const c_char = c"@@:@".as_ptr();
            ffi::class_addMethod(cls as *const AnyClass as *mut AnyClass, sel, imp, types).as_bool()
        }
    }

    struct App {
        window: Option<Window>,
        self_test: bool,
    }

    impl ApplicationHandler<Ev> for App {
        fn resumed(&mut self, el: &ActiveEventLoop) {
            let attrs = Window::default_attributes().with_title("dock-menu spike");
            self.window = Some(el.create_window(attrs).unwrap());
            if self.self_test {
                // Ask the delegate exactly as AppKit does, then click every item.
                let mtm = MainThreadMarker::new().unwrap();
                let app = NSApplication::sharedApplication(mtm);
                let del = app.delegate().expect("delegate");
                // SAFETY: selector was added by `install`; returns NSMenu or nil.
                let menu: Option<Retained<NSMenu>> =
                    unsafe { msg_send![&*del, applicationDockMenu: &*app] };
                let menu = menu.expect("delegate did not answer applicationDockMenu:");
                for i in 0..menu.numberOfItems() {
                    if let Some(it) = menu.itemAtIndex(i) {
                        println!("item {i}: {}", it.title());
                        if !it.isSeparatorItem() {
                            menu.performActionForItemAtIndex(i);
                        }
                    }
                }
            }
        }

        fn user_event(&mut self, el: &ActiveEventLoop, ev: Ev) {
            println!("FIRED {ev:?}");
            if self.self_test && matches!(ev, Ev::GoTo(2)) {
                el.exit();
            }
        }

        fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
            match ev {
                WindowEvent::CloseRequested => el.exit(),
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed =>
                {
                    let mut t = TABS.lock().unwrap();
                    t[0].1 = (t[0].1 + 1) % STATES.len();
                }
                _ => {}
            }
        }
    }

    pub fn run() {
        let el = EventLoop::<Ev>::with_user_event().build().unwrap();
        PROXY.set(el.create_proxy()).ok();
        println!("class_addMethod ok: {}", install());
        let mut app = App {
            window: None,
            self_test: std::env::args().any(|a| a == "--self-test"),
        };
        el.run_app(&mut app).unwrap();
    }
}
