//! Native window geometry, kept at the FFI boundary.

use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, msg_send};
use objc2_foundation::NSRect;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// The native window buttons in logical pixels relative to the content's top-left.
#[derive(Clone, Copy, Debug)]
pub struct Controls {
    /// Left edge of the close button.
    pub left: f64,
    /// Right edge of the zoom button.
    pub right: f64,
    /// Vertical centre of the close button.
    pub center_y: f64,
}

/// Reads the native buttons on the main thread. Hidden buttons, a detached view, or a
/// non-AppKit handle return nothing. The handle keeps the view alive during these calls.
pub fn controls(window: &impl HasWindowHandle) -> Option<Controls> {
    let _main = MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    let view = handle.ns_view.cast::<AnyObject>().as_ptr();
    // SAFETY: the live AppKit handle is an NSView, and all calls are on the main thread.
    // NSWindowButton values 0 and 2 are close and zoom; the returned NSButtons are NSViews.
    // convertRect:fromView: converts their bounds into the content view's coordinates.
    unsafe {
        let native: *mut AnyObject = msg_send![view, window];
        if native.is_null() {
            return None;
        }
        let close: *mut AnyObject = msg_send![native, standardWindowButton: 0_usize];
        let zoom: *mut AnyObject = msg_send![native, standardWindowButton: 2_usize];
        if close.is_null() || zoom.is_null() {
            return None;
        }
        let hidden: bool = msg_send![close, isHidden];
        if hidden {
            return None;
        }
        let close_bounds: NSRect = msg_send![close, bounds];
        let zoom_bounds: NSRect = msg_send![zoom, bounds];
        let first: NSRect = msg_send![view, convertRect: close_bounds, fromView: close];
        let last: NSRect = msg_send![view, convertRect: zoom_bounds, fromView: zoom];
        let bounds: NSRect = msg_send![view, bounds];
        let flipped: bool = msg_send![view, isFlipped];
        let center = first.origin.y + first.size.height / 2.0;
        Some(Controls {
            left: first.origin.x - bounds.origin.x,
            right: last.origin.x + last.size.width - bounds.origin.x,
            center_y: if flipped {
                center - bounds.origin.y
            } else {
                bounds.origin.y + bounds.size.height - center
            },
        })
    }
}
