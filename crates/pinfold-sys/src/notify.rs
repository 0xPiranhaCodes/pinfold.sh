//! macOS notifications that a tab needs you (ADR-0045).
//!
//! `NSUserNotificationCenter` is deprecated, but it is in the Foundation binding the window
//! already links and needs no entitlement. It has no default center outside an app bundle, such
//! as in a `cargo` build, so then the Dock icon asks for attention instead.

#![allow(deprecated)]

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSRequestUserAttentionType};
use objc2_foundation::{
    NSObject, NSString, NSUserNotification, NSUserNotificationCenter,
    NSUserNotificationCenterDelegate,
};

/// What a click on a notification calls, with the number it was raised with.
struct Clicked {
    run: Box<dyn Fn(usize)>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PinFoldNotificationDelegate"]
    #[ivars = Clicked]
    struct Delegate;

    // SAFETY: NSObject already conforms.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: both methods below have the protocol's signatures, and the rest are optional.
    unsafe impl NSUserNotificationCenterDelegate for Delegate {
        // SAFETY: the signature is NSUserNotificationCenterDelegate's.
        #[unsafe(method(userNotificationCenter:didActivateNotification:))]
        fn activated(&self, _center: &NSUserNotificationCenter, note: &NSUserNotification) {
            let number = note.identifier().and_then(|id| id.to_string().parse().ok());
            if let Some(number) = number {
                (self.ivars().run)(number);
            }
        }

        // SAFETY: as above. Shown even while the app is frontmost, as the tab is hidden then.
        #[unsafe(method(userNotificationCenter:shouldPresentNotification:))]
        fn present(&self, _center: &NSUserNotificationCenter, _note: &NSUserNotification) -> bool {
            true
        }
    }
);

/// Raises notifications, or Dock attention where the app has no notification center.
pub struct Notifier {
    mtm: MainThreadMarker,
    center: Option<(Retained<NSUserNotificationCenter>, Retained<Delegate>)>,
}

impl Notifier {
    /// Sets up notifications on the main thread, and returns nothing off it. `clicked` gets the
    /// number a notification was raised with when it is clicked.
    pub fn new(clicked: impl Fn(usize) + 'static) -> Option<Self> {
        let mtm = MainThreadMarker::new()?;
        // SAFETY: the method takes no arguments and returns the shared center, or nil outside
        // a bundle, which the binding's non-null type would not allow.
        let center: Option<Retained<NSUserNotificationCenter>> = unsafe {
            msg_send![
                NSUserNotificationCenter::class(),
                defaultUserNotificationCenter
            ]
        };
        let center = center.map(|center| {
            let delegate = Delegate::alloc(mtm).set_ivars(Clicked {
                run: Box::new(clicked),
            });
            // SAFETY: NSObject's init takes no arguments and returns the initialised object.
            let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
            // SAFETY: the center holds its delegate weakly, and `Notifier` keeps it alive.
            unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
            (center, delegate)
        });
        Some(Self { mtm, center })
    }

    /// Shows `title`, which replaces an earlier notification with the same `number`. Without a
    /// center it bounces the Dock icon once, which shows no text.
    pub fn notify(&self, title: &str, number: usize) {
        let Some((center, _)) = &self.center else {
            let app = NSApplication::sharedApplication(self.mtm);
            app.requestUserAttention(NSRequestUserAttentionType::InformationalRequest);
            return;
        };
        let note = NSUserNotification::new();
        note.setTitle(Some(&NSString::from_str(title)));
        note.setIdentifier(Some(&NSString::from_str(&number.to_string())));
        center.deliverNotification(&note);
    }
}
