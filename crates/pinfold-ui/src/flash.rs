//! The flash when a program asks to write the clipboard, drawn in the row under the pane.
//!
//! That row is outside pane text, so program output cannot fake or hide it. It says blocked
//! because the core drops OSC 52 writes, and a paste prompt in that row takes priority.

use std::time::{Duration, Instant};

use pinfold_proto::Frame;

use crate::paste::{self, AMBER, TEXT};

const SHOWN: Duration = Duration::from_secs(2);

// shortcut: writes stay blocked though the threat model allows them, until the core hands
// the window their text and this says "wrote" instead.
/// Fixed in size, so a flood of writes can only move the deadline.
#[derive(Debug, Default)]
pub struct Flash {
    seen: u32,
    until: Option<Instant>,
}

impl Flash {
    /// Shows the flash if `writes`, the count in the latest frame, moved. Ends it once due.
    pub fn observe(&mut self, writes: u32, now: Instant) {
        if writes != self.seen {
            self.seen = writes;
            self.until = Some(now + SHOWN);
        }
        self.until = self.until.filter(|until| now < *until);
    }

    /// Returns when the shown flash is due to end, or nothing if none is shown.
    #[must_use]
    pub fn until(&self) -> Option<Instant> {
        self.until
    }

    /// Adds the flash's row under the pane in `frame`, if it is shown.
    pub fn banner(&self, frame: &mut Frame) {
        if self.until.is_some() {
            paste::row(
                frame,
                &[
                    (" ◆ blocked  ".to_owned(), AMBER),
                    ("A program tried to write the clipboard".to_owned(), TEXT),
                ],
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_shows_the_flash_until_it_is_due() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        flash.observe(0, start);
        assert_eq!(flash.until(), None);
        flash.observe(1, start);
        assert_eq!(flash.until(), Some(start + SHOWN));
        flash.observe(1, start + SHOWN - Duration::from_millis(1));
        assert_eq!(flash.until(), Some(start + SHOWN));
        flash.observe(1, start + SHOWN);
        assert_eq!(flash.until(), None);
    }

    #[test]
    fn a_flood_keeps_one_deadline() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        for writes in 1..=1000 {
            let now = start + Duration::from_millis(u64::from(writes));
            flash.observe(writes, now);
            assert_eq!(flash.until(), Some(now + SHOWN));
        }
        flash.observe(1000, start + Duration::from_secs(3));
        assert_eq!(flash.until(), None);
    }

    #[test]
    fn a_count_that_wraps_is_still_a_write() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        flash.observe(u32::MAX, start);
        flash.observe(u32::MAX, start + SHOWN);
        flash.observe(0, start + SHOWN);
        assert!(flash.until().is_some());
    }

    #[test]
    fn the_row_is_drawn_only_while_shown() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        let mut frame = Frame {
            rows: 1,
            columns: 60,
            ..Frame::default()
        };
        flash.banner(&mut frame);
        assert_eq!(frame.rows, 1);
        flash.observe(1, start);
        flash.banner(&mut frame);
        assert_eq!(frame.rows, 2);
        let text: String = frame.cells.iter().map(|cell| cell.character).collect();
        assert!(text.starts_with(" ◆ blocked  A program tried to write the clipboard"));
        assert_eq!(frame.cells[1].foreground, AMBER);
    }
}
