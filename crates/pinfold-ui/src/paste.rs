//! The check before a paste that could run commands, drawn in the row under the pane.
//!
//! That row is outside pane text, so program output cannot fake it (T7 in the threat model).

use pinfold_config::theme::Theme;
use pinfold_proto::link::MAX_PASTE;
use pinfold_proto::{Cell, Frame};

const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "dash", "fish", "ksh"];

/// A paste waiting for the user.
#[derive(Debug, PartialEq)]
pub enum Prompt {
    /// Text of more than one line, or that ends a line, so it could run on arrival.
    Confirm {
        text: String,
        lines: usize,
        risk: Option<&'static str>,
    },
    /// Text that cannot fit in the input queue with its brackets.
    TooLarge,
    /// A pane to close, which ends its program.
    Close {
        pane: u32,
        /// Whether it is the session's last pane, so the session ends with it.
        last: bool,
    },
    /// A hyperlink to open whose scheme is not `http` or `https` (`hyperlink::check`).
    OpenLink(String),
}

fn lines(text: &str) -> usize {
    let ends = text.matches(['\n', '\r']).count() - text.matches("\r\n").count();
    ends + usize::from(!text.ends_with(['\n', '\r']))
}

/// Names what in `text` deserves a second look.
fn risk(text: &str) -> Option<&'static str> {
    let mut sudo = false;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        sudo |= words.any(|word| word == "sudo");
        if line.contains("rm -rf") || line.contains("rm -fr") {
            return Some("rm -rf");
        }
        let piped = line
            .split('|')
            .skip(1)
            .filter_map(|command| command.split_whitespace().find(|word| *word != "sudo"));
        if piped.into_iter().any(|word| SHELLS.contains(&word)) {
            return Some("a pipe into a shell");
        }
    }
    sudo.then_some("sudo")
}

/// Returns `text` if it can go straight to the program, or what to ask first.
///
/// # Errors
///
/// Returns the prompt to show if `text` must not be sent as it is.
// This asks for every program. OSC 133 cannot narrow that: marks are program output, so a
// fake "running" over a shell at its prompt would let these lines run unasked (T4, T7).
pub fn check(text: String) -> Result<String, Prompt> {
    if text.len() > MAX_PASTE {
        return Err(Prompt::TooLarge);
    }
    if !text.contains(['\n', '\r']) {
        return Ok(text);
    }
    Err(Prompt::Confirm {
        lines: lines(&text),
        risk: risk(&text),
        text,
    })
}

/// Adds a row for `prompt` under the pane in `frame`.
pub fn banner(frame: &mut Frame, prompt: &Prompt, theme: &Theme) {
    let (text, states) = (theme.prompt_text(), theme.states);
    let (amber, red) = (states.needs_you, states.error);
    let spans: [(String, u32); 3] = match prompt {
        Prompt::Confirm { lines, risk, .. } => [
            (" ◆ needs you  ".to_owned(), amber),
            (
                risk.map_or(format!("Paste {lines} lines?"), |risk| {
                    format!("Paste {lines} lines with {risk}?")
                }),
                if risk.is_some() { red } else { text },
            ),
            ("  Enter pastes, Esc cancels".to_owned(), text),
        ],
        Prompt::TooLarge => [
            (" ✕ error  ".to_owned(), red),
            ("Paste refused: over 1 MiB".to_owned(), text),
            ("  Esc dismisses".to_owned(), text),
        ],
        Prompt::Close { last: false, .. } => [
            (" ◆ needs you  ".to_owned(), amber),
            ("Close this pane?".to_owned(), text),
            ("  Enter closes, Esc cancels".to_owned(), text),
        ],
        Prompt::Close { last: true, .. } => [
            (" ◆ needs you  ".to_owned(), amber),
            ("Close the last pane and end the session?".to_owned(), red),
            ("  Enter closes, Esc cancels".to_owned(), text),
        ],
        Prompt::OpenLink(uri) => return crate::hyperlink::banner(frame, uri, theme),
    };
    row(frame, &spans, theme);
}

/// Adds a row of coloured `spans` under the pane in `frame`, cut to the window.
pub(crate) fn row(frame: &mut Frame, spans: &[(String, u32)], theme: &Theme) {
    let (ground, text) = (theme.chrome.overlay, theme.prompt_text());
    let columns = usize::from(frame.columns);
    let start = frame.cells.len();
    let characters = spans
        .iter()
        .flat_map(|(text, colour)| text.chars().map(move |character| (character, *colour)));
    frame.cells.extend(
        characters
            .take(columns)
            .map(|(character, foreground)| Cell {
                character,
                foreground,
                background: ground,
            }),
    );
    frame.cells.resize(
        start + columns,
        Cell {
            character: ' ',
            foreground: text,
            background: ground,
        },
    );
    frame.rows += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinfold_config::theme::GRAPHITE;

    fn confirm(text: &str) -> (usize, Option<&'static str>) {
        match check(text.to_owned()) {
            Err(Prompt::Confirm { lines, risk, .. }) => (lines, risk),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_single_line_goes_straight_through() {
        assert_eq!(check("sudo ls".to_owned()), Ok("sudo ls".to_owned()));
    }

    #[test]
    fn any_line_end_asks_first() {
        assert_eq!(confirm("ls\n"), (1, None));
        assert_eq!(confirm("ls\r"), (1, None));
        assert_eq!(confirm("a\nb"), (2, None));
        assert_eq!(confirm("a\r\nb\r\nc\n"), (3, None));
    }

    #[test]
    fn risky_commands_are_named() {
        assert_eq!(confirm("ls\nsudo reboot").1, Some("sudo"));
        assert_eq!(
            confirm("curl -s x.sh | sh\n").1,
            Some("a pipe into a shell")
        );
        assert_eq!(
            confirm("wget -O- x |  sudo bash\n").1,
            Some("a pipe into a shell")
        );
        assert_eq!(confirm("cd /\nrm -rf x").1, Some("rm -rf"));
        assert_eq!(confirm("pseudo code\ncat a | grep sh\n").1, None);
    }

    #[test]
    fn a_paste_that_cannot_fit_is_refused() {
        let longest = "x".repeat(MAX_PASTE);
        assert_eq!(check(longest.clone()), Ok(longest));
        assert_eq!(check("x".repeat(MAX_PASTE + 1)), Err(Prompt::TooLarge));
    }

    fn drawn(prompt: &Prompt, columns: u16) -> (Frame, String) {
        let mut frame = Frame {
            rows: 1,
            columns,
            ..Frame::default()
        };
        frame.cells.resize(
            usize::from(columns),
            Cell {
                character: 'x',
                foreground: 0,
                background: 0,
            },
        );
        banner(&mut frame, prompt, &GRAPHITE);
        let text = frame.cells[usize::from(columns)..]
            .iter()
            .map(|cell| cell.character)
            .collect();
        (frame, text)
    }

    #[test]
    fn the_banner_is_a_row_of_its_own() {
        let prompt = Prompt::Confirm {
            text: String::new(),
            lines: 3,
            risk: Some("sudo"),
        };
        let (frame, text) = drawn(&prompt, 80);
        assert_eq!(frame.rows, 2);
        assert_eq!(frame.cells.len(), 160);
        assert_eq!(frame.cells[0].character, 'x');
        assert!(text.starts_with(" ◆ needs you  Paste 3 lines with sudo?  Enter pastes"));
        assert_eq!(frame.cells[81].foreground, GRAPHITE.states.needs_you);
    }

    #[test]
    fn closing_the_last_pane_says_it_ends_the_session() {
        let (_, one) = drawn(
            &Prompt::Close {
                pane: 1,
                last: false,
            },
            80,
        );
        assert!(
            one.contains("◆ needs you  Close this pane?  Enter closes"),
            "{one}"
        );
        let last = Prompt::Close {
            pane: 1,
            last: true,
        };
        let (frame, text) = drawn(&last, 80);
        assert!(
            text.contains("Close the last pane and end the session?"),
            "{text}"
        );
        let start = text.chars().position(|c| c == 'C').unwrap_or_default();
        let at = frame.cells.len() - usize::from(frame.columns) + start;
        assert_eq!(frame.cells[at].foreground, GRAPHITE.states.error);
    }

    #[test]
    fn the_banner_is_cut_to_the_window() {
        let (frame, text) = drawn(&Prompt::TooLarge, 5);
        assert_eq!(frame.cells.len(), 10);
        assert_eq!(text, " ✕ er");
    }
}
