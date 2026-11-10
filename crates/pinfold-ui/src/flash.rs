//! Short notes in the row under the pane: a blocked clipboard write, a command the core
//! refused, or, once, that the core lacks the hardened runtime and how to sign it (ADR-0044),
//! or that sandboxed agents can read secret-looking variables in other panes (ADR-0047).
//!
//! That row is outside pane text, so program output cannot fake or hide it. It says blocked
//! because the core drops OSC 52 writes, and a paste prompt in that row takes priority.

use pinfold_config::theme::Theme;
use std::time::{Duration, Instant};

use pinfold_proto::Frame;
use pinfold_proto::link::Refusal;

use crate::paste;
use crate::top_bar::{UNHARDENED, VIOLET, unhardened_word};

const SHOWN: Duration = Duration::from_secs(2);
/// Long enough to read and copy the command. The bar's marker stays after it.
const UNHARDENED_SHOWN: Duration = Duration::from_secs(30);

/// What the flash says.
#[derive(Debug, Default)]
enum Said {
    #[default]
    Blocked,
    Refused(Refusal),
    /// Whether the core could tell, and the command that signs this build.
    Unhardened(bool, String),
    /// How many secret-looking variables, and the first few names.
    Secrets(u32, String),
}

// shortcut: writes stay blocked though the threat model allows them, until the core hands
// the window their text and this says "wrote" instead.
/// Fixed in size, so a flood of writes can only move the deadline.
#[derive(Debug, Default)]
pub struct Flash {
    until: Option<Instant>,
    said: Said,
}

impl Flash {
    /// Shows that a program tried to write the clipboard.
    pub fn blocked(&mut self, now: Instant) {
        self.until = Some(now + SHOWN);
        self.said = Said::Blocked;
    }

    /// Shows why the core did not carry out a command.
    pub fn refused(&mut self, reason: Refusal, now: Instant) {
        self.until = Some(now + SHOWN);
        self.said = Said::Refused(reason);
    }

    /// Shows that the core lacks the hardened runtime, or with `checked` false that it could not
    /// tell, and `command`, which signs this build with it.
    pub fn unhardened(&mut self, checked: bool, command: String, now: Instant) {
        self.until = Some(now + UNHARDENED_SHOWN);
        self.said = Said::Unhardened(checked, command);
    }

    /// Shows that on macOS sandboxed agents can read `count` secret-looking variables in other
    /// panes, `names` first, and how to remove them. As long as the hardened runtime's notice.
    pub fn secrets(&mut self, count: u32, names: String, now: Instant) {
        self.until = Some(now + UNHARDENED_SHOWN);
        self.said = Said::Secrets(count, names);
    }

    /// Ends the flash once due.
    pub fn tick(&mut self, now: Instant) {
        self.until = self.until.filter(|until| now < *until);
    }

    /// Returns when the shown flash is due to end, or nothing if none is shown.
    #[must_use]
    pub fn until(&self) -> Option<Instant> {
        self.until
    }

    /// Adds the flash's row under the pane in `frame`, if it is shown.
    pub fn banner(&self, frame: &mut Frame, theme: &Theme) {
        let (text, states) = (theme.prompt_text(), theme.states);
        if self.until.is_none() {
            return;
        }
        match &self.said {
            Said::Refused(reason) => paste::row(
                frame,
                &[
                    (" ✕ error  ".to_owned(), states.error),
                    (reason.to_string(), text),
                ],
                theme,
            ),
            Said::Unhardened(checked, command) => paste::row(
                frame,
                &[
                    (
                        format!(" {UNHARDENED} {}  ", unhardened_word(*checked)),
                        VIOLET,
                    ),
                    (format!("Sign this build: {command}"), text),
                ],
                theme,
            ),
            Said::Secrets(count, names) => paste::row(
                frame,
                &[
                    (format!(" {UNHARDENED} secrets  "), VIOLET),
                    (
                        format!(
                            "On macOS, sandboxed agents can read {count} secret-looking variables \
                             in your other panes: {names} (strip_secrets = true removes them)"
                        ),
                        text,
                    ),
                ],
                theme,
            ),
            Said::Blocked => paste::row(
                frame,
                &[
                    (" ◆ blocked  ".to_owned(), states.needs_you),
                    ("A program tried to write the clipboard".to_owned(), text),
                ],
                theme,
            ),
        }
    }
}

/// Adds the row under the pane in `frame` that says the core has gone, and the panes with it.
pub fn ended(frame: &mut Frame, theme: &Theme) {
    let (text, states) = (theme.prompt_text(), theme.states);
    paste::row(
        frame,
        &[
            (" ✕ session ended  ".to_owned(), states.error),
            ("The panes are gone. Close this window".to_owned(), text),
        ],
        theme,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinfold_config::theme::GRAPHITE;

    #[test]
    fn a_write_shows_the_flash_until_it_is_due() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        flash.tick(start);
        assert_eq!(flash.until(), None);
        flash.blocked(start);
        assert_eq!(flash.until(), Some(start + SHOWN));
        flash.tick(start + SHOWN - Duration::from_millis(1));
        assert_eq!(flash.until(), Some(start + SHOWN));
        flash.tick(start + SHOWN);
        assert_eq!(flash.until(), None);
    }

    #[test]
    fn a_refusal_says_why() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        let mut frame = Frame {
            rows: 1,
            columns: 60,
            ..Frame::default()
        };
        flash.refused(Refusal::Tabs, start);
        flash.banner(&mut frame, &GRAPHITE);
        let text: String = frame.cells.iter().map(|cell| cell.character).collect();
        assert!(text.starts_with(" ✕ error  This session has as many tabs"));
    }

    #[test]
    fn an_unhardened_core_shows_how_to_sign_it_for_longer() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        let mut frame = Frame {
            rows: 1,
            columns: 80,
            ..Frame::default()
        };
        flash.unhardened(true, "codesign x".to_owned(), start);
        flash.tick(start + SHOWN);
        flash.banner(&mut frame, &GRAPHITE);
        let text: String = frame.cells.iter().map(|cell| cell.character).collect();
        assert!(
            text.starts_with(" ▲ unhardened  Sign this build: codesign x"),
            "{text}"
        );
        assert_eq!(frame.cells[1].foreground, VIOLET);
        flash.tick(start + UNHARDENED_SHOWN);
        assert_eq!(flash.until(), None);
    }

    #[test]
    fn exposed_secrets_are_named_with_how_to_remove_them() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        let mut frame = Frame {
            rows: 1,
            columns: 160,
            ..Frame::default()
        };
        flash.secrets(7, "AWS_PROFILE, GITHUB_TOKEN, …".to_owned(), start);
        flash.tick(start + SHOWN);
        flash.banner(&mut frame, &GRAPHITE);
        let text: String = frame.cells.iter().map(|cell| cell.character).collect();
        assert!(
            text.starts_with(
                " ▲ secrets  On macOS, sandboxed agents can read 7 secret-looking variables in \
                 your other panes: AWS_PROFILE, GITHUB_TOKEN, … (strip_secrets = true removes them)"
            ),
            "{text}"
        );
        assert_eq!(frame.cells[1].foreground, VIOLET);
        flash.tick(start + UNHARDENED_SHOWN);
        assert_eq!(flash.until(), None);
    }

    #[test]
    fn the_end_of_a_session_says_so_under_the_pane() {
        let mut frame = Frame {
            rows: 1,
            columns: 60,
            ..Frame::default()
        };
        ended(&mut frame, &GRAPHITE);
        assert_eq!(frame.rows, 2);
        let text: String = frame.cells.iter().map(|cell| cell.character).collect();
        assert!(text.starts_with(" ✕ session ended  The panes are gone"));
        assert_eq!(frame.cells[1].foreground, GRAPHITE.states.error);
    }

    #[test]
    fn the_row_is_drawn_only_while_shown() {
        let (mut flash, start) = (Flash::default(), Instant::now());
        let mut frame = Frame {
            rows: 1,
            columns: 60,
            ..Frame::default()
        };
        flash.banner(&mut frame, &GRAPHITE);
        assert_eq!(frame.rows, 1);
        flash.blocked(start);
        flash.banner(&mut frame, &GRAPHITE);
        assert_eq!(frame.rows, 2);
        let text: String = frame.cells.iter().map(|cell| cell.character).collect();
        assert!(text.starts_with(" ◆ blocked  A program tried to write the clipboard"));
        assert_eq!(frame.cells[1].foreground, GRAPHITE.states.needs_you);
    }
}
