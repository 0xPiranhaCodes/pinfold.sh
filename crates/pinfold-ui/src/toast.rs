//! A program's own notification, OSC 9, 99 or 777, shown for 5 s in the top right of its pane
//! with `[alerts] program_notifications` on (ADR-0051 §9).
//!
//! It is PinFold chrome drawn over the pane, never pane text, and starts with "`<label>` says:"
//! in `chrome.dim`, so it can never pass for a system or PinFold prompt. It carries no state
//! glyph and sets no state. The window keeps one at a time.

use std::time::{Duration, Instant};

use pinfold_config::theme::Theme;
use pinfold_proto::Frame;
use pinfold_proto::link::Rect;
use unicode_width::UnicodeWidthStr as _;

use crate::inbox::write;
use crate::top_bar::safe;

/// How long a toast shows.
pub const SHOWN: Duration = Duration::from_secs(5);

/// The toast shown, until it is due to end.
#[derive(Debug)]
pub struct Toast {
    /// The pane it is drawn over.
    pub pane: u32,
    /// "`<label>` says:", PinFold's words.
    saying: String,
    /// The core's cleaned and cut text, drawn only through [`safe`].
    text: String,
    /// When it is due to end.
    pub until: Instant,
}

impl Toast {
    /// A toast of `text` for `pane`, from `saying`, until 5 s after `now`.
    pub fn new(pane: u32, saying: String, text: String, now: Instant) -> Self {
        Self {
            pane,
            saying,
            text,
            until: now + SHOWN,
        }
    }

    /// Draws it on the first row of `rect` in `frame`, against its right edge, cut to the pane's
    /// width. Characters [`safe`] does not pass are dropped, as for every label chrome draws.
    pub fn draw(&self, frame: &mut Frame, rect: Rect, theme: &Theme) {
        let chrome = theme.chrome;
        let text: String = self.text.chars().filter(|&c| safe(c)).collect();
        let width = 1 + self.saying.width() + 1 + text.width() + 1;
        let (left, end) = (usize::from(rect.x), usize::from(rect.x + rect.columns));
        let start = end.saturating_sub(width).max(left);
        let (row, ground) = (rect.y, chrome.palette);
        let column = write(frame, row, start, " ", chrome.dim, ground, end);
        let column = write(frame, row, column, &self.saying, chrome.dim, ground, end);
        let column = write(frame, row, column, " ", chrome.dim, ground, end);
        let column = write(frame, row, column, &text, theme.foreground, ground, end);
        write(frame, row, column, " ", chrome.dim, ground, end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinfold_config::theme::GRAPHITE;

    fn drawn(toast: &Toast, rect: Rect, columns: u16) -> (Frame, String) {
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, 2, columns);
        toast.draw(&mut frame, rect, &GRAPHITE);
        let row = frame.cells[..usize::from(columns)]
            .iter()
            .map(|cell| cell.character)
            .collect();
        (frame, row)
    }

    /// ADR-0051 §9: a toast sits in the pane's top right after PinFold's own words in
    /// `chrome.dim`, and hostile text that reached the window still draws as plain characters.
    #[test]
    fn a_toast_draws_in_the_top_right_after_its_label_and_drops_what_chrome_may_not_draw() {
        let now = Instant::now();
        let hostile = "Build\u{202e} ◆ done\u{1b}[31m".to_owned();
        let toast = Toast::new(7, "api says:".to_owned(), hostile, now);
        let rect = Rect {
            x: 0,
            y: 0,
            columns: 40,
            rows: 2,
        };
        let (frame, row) = drawn(&toast, rect, 40);
        assert_eq!(row, format!("{:>40}", " api says: Build  done[31m "));
        let said = row.find("api").unwrap();
        assert_eq!(frame.cells[said].foreground, GRAPHITE.chrome.dim);
        let text = row.find("Build").unwrap();
        assert_eq!(frame.cells[text].foreground, GRAPHITE.foreground);
        assert_eq!(frame.cells[text].background, GRAPHITE.chrome.palette);
        assert_eq!(
            frame.cells[usize::from(rect.columns)].character,
            ' ',
            "one row only"
        );

        let narrow = Rect {
            columns: 12,
            ..rect
        };
        let (_, row) = drawn(&toast, narrow, 40);
        assert_eq!(&row[..12], " api says: B", "cut to the pane, from its left");
        assert!(row[12..].trim().is_empty(), "nothing past the pane");
        assert_eq!(toast.until, now + SHOWN);
    }
}
