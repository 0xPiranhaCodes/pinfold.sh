//! What Open Link does with a target that program output chose (T3, OSC 8 in the threat model).
//!
//! `http` and `https` go to the platform's opener as one argument, never through a shell. Any
//! other scheme asks first, in rows drawn over muted pane text with the keys on the chrome row,
//! which output cannot reach, and with the URI in full and anything outside printable ASCII
//! escaped.

use pinfold_proto::link::MAX_URI;
use pinfold_proto::{Cell, Frame};

use crate::approval::{MUTED, visible, wrap};
use crate::paste::{self, AMBER, GROUND, TEXT};

/// What Open Link does with a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// An `http` or `https` link with a host, all printable ASCII, so it opens straight away.
    Web,
    /// Anything else, which opens only once the prompt is answered.
    Ask,
}

/// Returns what Open Link does with `uri`, or nothing if it is too long or no URI: a scheme as
/// RFC 3986 spells it, a colon, and more, with no space or control character anywhere.
pub fn check(uri: &str) -> Option<Target> {
    if uri.len() > MAX_URI || uri.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, rest) = uri.split_once(':')?;
    let mut letters = scheme.chars();
    let first = letters.next().is_some_and(|c| c.is_ascii_alphabetic());
    if !first || rest.is_empty() || !letters.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Some(Target::Ask);
    }
    let host = rest.strip_prefix("//")?.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    // A look-alike letter from another script could pass for a known host, so it asks.
    Some(if uri.is_ascii() {
        Target::Web
    } else {
        Target::Ask
    })
}

/// The prompt's rows for a pane area `columns` wide.
fn lines(uri: &str, columns: usize) -> Vec<String> {
    let border = format!("{:─<columns$}", "── PinFold link ");
    let mut lines = vec![border.chars().take(columns.max(1)).collect()];
    let head = format!(" ◆ needs you  Open this link? {}", visible(uri));
    lines.extend(wrap(&head, columns));
    lines
}

/// Whether the whole prompt for `uri` fits in a pane area of `rows` × `columns`. Enter opens
/// nothing while it does not.
pub fn fits(uri: &str, rows: u16, columns: u16) -> bool {
    lines(uri, usize::from(columns)).len() <= usize::from(rows)
}

/// Mutes the panes in `frame`, draws the prompt over the bottom of them, and its keys on a new
/// chrome row under them.
pub fn banner(frame: &mut Frame, uri: &str) {
    let (rows, columns) = (usize::from(frame.rows), usize::from(frame.columns));
    for cell in &mut frame.cells {
        cell.foreground = MUTED;
    }
    let lines = lines(uri, columns);
    let shown = lines.len().min(rows);
    for (offset, line) in lines.iter().take(shown).enumerate() {
        let start = (rows - shown + offset) * columns;
        let Some(cells) = frame.cells.get_mut(start..start + columns) else {
            break;
        };
        let mut characters = line.chars();
        for cell in cells {
            *cell = Cell {
                character: characters.next().unwrap_or(' '),
                foreground: if offset <= 1 { AMBER } else { TEXT },
                background: GROUND,
            };
        }
    }
    let legend = if lines.len() <= rows {
        format!(" Enter opens · Esc cancels · the {shown} rows above are this prompt")
    } else {
        " Too long to show whole, so it cannot open · Esc cancels".to_owned()
    };
    paste::row(frame, &[(legend, TEXT)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_links_open_and_other_schemes_ask() {
        for uri in [
            "https://pinfold.sh",
            "HTTP://a.b/c?d#e",
            "http://[::1]:8080/",
        ] {
            assert_eq!(check(uri), Some(Target::Web), "{uri}");
        }
        for uri in [
            "file:///etc/passwd",
            "ssh://host",
            "x-man-page://ls",
            "mailto:a@b.c",
            "javascript:alert(1)",
            "https://аpple.com",
        ] {
            assert_eq!(check(uri), Some(Target::Ask), "{uri}");
        }
    }

    #[test]
    fn what_is_no_uri_or_too_long_is_refused() {
        let long = format!("https://a.b/{}", "x".repeat(MAX_URI));
        for uri in [
            "",
            "pinfold.sh",
            ":x",
            "1http://a",
            "ht tp://a",
            "https:",
            "https:pinfold.sh",
            "https:///path",
            "https://a.b/\u{7}",
            "https://a.b/\x1b[2J",
            "-a:b",
            "file:/x\n",
            &long,
        ] {
            assert_eq!(check(uri), None, "{uri:?}");
        }
        let longest = format!("https://a/{}", "x".repeat(MAX_URI - 10));
        assert_eq!(check(&longest), Some(Target::Web));
    }

    fn drawn(uri: &str, rows: u16, columns: u16) -> Vec<String> {
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, rows, columns);
        banner(&mut frame, uri);
        frame
            .cells
            .chunks(usize::from(columns))
            .map(|row| row.iter().map(|cell| cell.character).collect())
            .collect()
    }

    #[test]
    fn the_prompt_shows_the_whole_uri_escaped() {
        let rows = drawn("file:///a\u{202e}b", 4, 40);
        assert_eq!(rows.len(), 5, "the keys get a chrome row of their own");
        assert_eq!(rows[1], format!("{:─<40}", "── PinFold link "));
        assert_eq!(rows[2], " ◆ needs you  Open this link? file:///a<");
        assert_eq!(rows[3].trim_end(), "U+202E>b");
        assert!(rows[4].starts_with(" Enter opens · Esc cancels · the 3 rows"));
        assert!(fits("file:///a\u{202e}b", 4, 40));
        assert!(!rows.concat().contains('\u{202e}'));
    }

    #[test]
    fn a_prompt_too_long_to_show_cannot_open() {
        let uri = format!("file:///{}", "x".repeat(200));
        assert!(!fits(&uri, 3, 40));
        let rows = drawn(&uri, 3, 40);
        assert!(
            rows[3].starts_with(" Too long to show whole"),
            "{}",
            rows[3]
        );
    }
}
