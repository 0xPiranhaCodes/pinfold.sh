//! The system clipboard.

use std::any::Any;

use copypasta::{ClipboardContext, ClipboardProvider};
use raw_window_handle::HasDisplayHandle;

/// A connection to the clipboard.
pub struct Clipboard {
    provider: Box<dyn ClipboardProvider>,
    // Declared after `provider`, so a Wayland display outlives what was built on it.
    _display: Box<dyn Any>,
}

impl Clipboard {
    /// Connects to the clipboard of `display`, which it keeps until dropped.
    #[must_use]
    pub fn new<D: HasDisplayHandle + 'static>(display: D) -> Option<Self> {
        let provider = wayland(&display).or_else(|| {
            let context = ClipboardContext::new().ok()?;
            Some(Box::new(context) as Box<dyn ClipboardProvider>)
        })?;
        Some(Self {
            provider,
            _display: Box::new(display),
        })
    }

    /// Returns the clipboard's text, if it holds any.
    pub fn text(&mut self) -> Option<String> {
        self.provider.get_contents().ok()
    }
}

#[cfg(not(target_os = "macos"))]
fn wayland(display: &impl HasDisplayHandle) -> Option<Box<dyn ClipboardProvider>> {
    use raw_window_handle::RawDisplayHandle;
    let RawDisplayHandle::Wayland(handle) = display.display_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: `HasDisplayHandle` promises a valid display for as long as `display` lives,
    // and `Clipboard` keeps it for as long as this provider.
    let (_, clipboard) = unsafe {
        copypasta::wayland_clipboard::create_clipboards_from_external(handle.display.as_ptr())
    };
    Some(Box::new(clipboard))
}

#[cfg(target_os = "macos")]
fn wayland(_: &impl HasDisplayHandle) -> Option<Box<dyn ClipboardProvider>> {
    None
}
