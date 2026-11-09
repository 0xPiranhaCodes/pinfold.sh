//! The system clipboard.

use std::any::Any;
#[cfg(target_os = "macos")]
use std::io::Read;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::mpsc;
#[cfg(target_os = "macos")]
use std::time::Duration;

use copypasta::{ClipboardContext, ClipboardProvider};
use raw_window_handle::HasDisplayHandle;

/// Why [`Clipboard::text`] gave no text.
#[derive(Debug, PartialEq, Eq)]
pub enum Unread {
    /// The clipboard holds no text, or could not be read.
    Empty,
    /// The clipboard holds more text than the limit, which was not copied.
    TooLarge,
}

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

    /// Returns the clipboard's text, unless it holds none or more than `limit` bytes.
    ///
    /// # Errors
    ///
    /// Returns why there is no text.
    // AppKit reads the whole pasteboard into the reading process before its size is known
    // (`appkit_reads_a_whole_pasteboard`). So `pbpaste` reads it, and this takes no more than
    // `limit` from its pipe. That also keeps copypasta's `types().unwrap()`, which panics when
    // the pasteboard has no types (#310), out of the window.
    #[cfg(target_os = "macos")]
    pub fn text(&mut self, limit: usize) -> Result<String, Unread> {
        let mut command = Command::new("/usr/bin/pbpaste");
        command.env("LANG", "en_US.UTF-8");
        bounded(command, limit, TIMEOUT)
    }

    /// Returns the clipboard's text, unless it holds none or more than `limit` bytes.
    ///
    /// # Errors
    ///
    /// Returns why there is no text.
    #[cfg(not(target_os = "macos"))]
    pub fn text(&mut self, limit: usize) -> Result<String, Unread> {
        let text = self.provider.get_contents().map_err(|_| Unread::Empty)?;
        if text.len() > limit {
            return Err(Unread::TooLarge);
        }
        Ok(text)
    }

    /// Replaces the clipboard's text, and returns whether it did.
    pub fn set_text(&mut self, text: String) -> bool {
        self.provider.set_contents(text).is_ok()
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

/// How long a paste waits for the clipboard's owner, as copypasta waits on X11.
#[cfg(target_os = "macos")]
const TIMEOUT: Duration = Duration::from_secs(3);

/// Runs `command`, which prints the clipboard, and takes at most `limit` bytes of it, so a huge
/// clipboard costs the child and not this process. A child still writing after `timeout` is
/// killed, which ends the reading thread too.
#[cfg(target_os = "macos")]
fn bounded(mut command: Command, limit: usize, timeout: Duration) -> Result<String, Unread> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| Unread::Empty)?;
    let output = child.stdout.take().ok_or(Unread::Empty)?;
    let (send, read) = mpsc::channel();
    std::thread::spawn(move || {
        // One byte past the limit says it is over, and the capacity never has to grow.
        let mut text = Vec::with_capacity(limit + 1);
        let _ = output.take(limit as u64 + 1).read_to_end(&mut text);
        let _ = send.send(text);
    });
    let text = read.recv_timeout(timeout);
    let _ = child.kill();
    let _ = child.wait();
    let text = text.map_err(|_| Unread::Empty)?;
    if text.len() > limit {
        return Err(Unread::TooLarge);
    }
    String::from_utf8(text)
        .ok()
        .filter(|text| !text.is_empty())
        .ok_or(Unread::Empty)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn printed(script: &str, limit: usize) -> Result<String, Unread> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        bounded(command, limit, Duration::from_millis(500))
    }

    #[test]
    fn a_clipboard_over_the_limit_is_refused_in_bytes() {
        assert_eq!(printed("printf 'four'", 4), Ok("four".to_owned()));
        assert_eq!(printed("printf 'four'", 3), Err(Unread::TooLarge));
        // Two characters, four bytes.
        assert_eq!(printed("printf 'éé'", 3), Err(Unread::TooLarge));
        assert_eq!(printed("printf 'éé'", 4), Ok("éé".to_owned()));
    }

    #[test]
    fn an_empty_broken_or_silent_clipboard_gives_no_text() {
        assert_eq!(printed("true", 4), Err(Unread::Empty));
        assert_eq!(printed("printf '\\377'", 4), Err(Unread::Empty));
        let start = std::time::Instant::now();
        assert_eq!(printed("sleep 30", 4), Err(Unread::Empty));
        assert!(start.elapsed() < Duration::from_secs(5));
        let missing = bounded(Command::new("/does/not/exist"), 4, TIMEOUT);
        assert_eq!(missing, Err(Unread::Empty));
    }

    #[test]
    fn a_large_clipboard_costs_this_process_the_limit() {
        const MIB: usize = 1024 * 1024;
        let footprint = || crate::footprint(std::process::id()).unwrap_or(0) as usize;
        let before = footprint();
        let script = "head -c 67108864 /dev/zero | tr '\\0' x";
        assert_eq!(printed(script, MIB), Err(Unread::TooLarge));
        let grown = footprint().saturating_sub(before);
        eprintln!("a 64 MiB clipboard grew this process by {grown} bytes");
        assert!(grown < 4 * MIB, "{grown}");
    }

    /// What a large pasteboard that another process wrote costs the process that reads it
    /// through AppKit, before any limit can refuse it. Run with
    /// `cargo test -p pinfold-sys --features clipboard -- --ignored --nocapture appkit`.
    #[test]
    #[ignore = "measurement, prints footprints"]
    fn appkit_reads_a_whole_pasteboard() {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
        use objc2_foundation::NSString;
        const MIB: usize = 1024 * 1024;
        const NAME: &str = "PINFOLD_TEST_PASTEBOARD";
        // SAFETY: AppKit's constant string, set before any Rust code runs.
        let string = unsafe { NSPasteboardTypeString };
        if let Ok(name) = std::env::var(NAME) {
            let board = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
            board.clearContents();
            let text = NSString::from_str(&"x".repeat(64 * MIB));
            assert!(board.setString_forType(&text, string));
            return;
        }
        // A private pasteboard, written by another process, leaves the user's clipboard alone.
        let board = NSPasteboard::pasteboardWithUniqueName();
        let test = "clipboard::tests::appkit_reads_a_whole_pasteboard";
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "--ignored", test])
            .env(NAME, board.name().to_string())
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let footprint = || crate::footprint(std::process::id()).unwrap_or(0) as usize / MIB;
        let before = footprint();
        let length =
            objc2::rc::autoreleasepool(|_| board.stringForType(string).map(|text| text.length()));
        assert_eq!(length, Some(64 * MIB));
        eprintln!(
            "reading a 64 MiB pasteboard: {before} MiB before, {} MiB after",
            footprint()
        );
        // SAFETY: `releaseGlobally` takes no arguments and returns nothing.
        let () = unsafe { objc2::msg_send![&board, releaseGlobally] };
    }

    /// Reads the real clipboard, without changing it.
    #[test]
    #[ignore = "reads the user's clipboard"]
    fn pbpaste_reads_the_clipboard() {
        let mut clipboard = Clipboard {
            provider: Box::new(copypasta::nop_clipboard::NopClipboardContext),
            _display: Box::new(()),
        };
        eprintln!("{:?}", clipboard.text(1024 * 1024).map(|text| text.len()));
    }
}
