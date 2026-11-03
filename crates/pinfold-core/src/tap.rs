//! A pane's output as plain text, for `until: "match"` waits and `pane.output` events. Only
//! panes something listens to are tapped, and each keeps at most [`MAX_TAPPED`] bytes until
//! taken.
//!
//! ```
//! use pinfold_core::tap::Tap;
//!
//! let mut tap = Tap::default();
//! tap.feed(b"\x1b[31mred\x1b[0m\r\n\x1b]0;title\x07done");
//! assert_eq!(tap.take(), "red\r\ndone");
//! assert_eq!(tap.take(), "");
//! ```

use alacritty_terminal::vte::{Parser, Perform};

/// Most text a tap or a match wait keeps, the newest.
pub const MAX_TAPPED: usize = 64 * 1024;

/// Turns a pane's bytes into the characters it prints, with tabs and line ends, and drops every
/// escape sequence and other control.
#[derive(Default)]
pub struct Tap {
    parser: Parser,
    text: Text,
}

#[derive(Default)]
struct Text(String);

impl Perform for Text {
    fn print(&mut self, character: char) {
        self.0.push(character);
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            self.0.push(char::from(byte));
        }
    }
}

impl Tap {
    /// Takes in bytes the pane wrote.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.text, bytes);
        keep_newest(&mut self.text.0, MAX_TAPPED);
    }

    /// Returns the text printed since the last take.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.text.0)
    }
}

/// Drops the oldest characters of `text` until it is at most `most` bytes.
pub fn keep_newest(text: &mut String, most: usize) {
    if text.len() > most {
        let mut cut = text.len() - most;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tap_keeps_the_newest_text_whole_characters_only() {
        let mut tap = Tap::default();
        tap.feed("é".repeat(MAX_TAPPED).as_bytes());
        tap.feed(b"end");
        let text = tap.take();
        assert!(text.len() <= MAX_TAPPED && text.ends_with("éend"));
        // A character split across two reads comes out whole.
        tap.feed(&[0xc3]);
        tap.feed(&[0xa9]);
        assert_eq!(tap.take(), "é");
    }
}
