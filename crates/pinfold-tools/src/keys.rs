//! Turns the bytes a terminal sends into keys and SGR mouse reports.

/// Longest escape sequence read; a longer one is dropped.
const MAX_SEQUENCE: usize = 32;

/// A key, or a mouse report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs, reason = "the names are the keys'")]
pub enum Key {
    /// A printable character, such as `j`, `/`, `?` or `q`.
    Char(char),
    /// Control and a letter or one of `@[\]^_`, lowercase.
    Ctrl(char),
    Enter,
    Tab,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Mouse(Mouse),
}

/// An SGR mouse report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    /// The xterm button code: 0 to 2 for the buttons, 64 and 65 for the wheel, plus 32 for
    /// motion and 4, 8 and 16 for Shift, Alt and Control.
    pub button: u16,
    /// From 0 at the left.
    pub column: u16,
    /// From 0 at the top.
    pub row: u16,
    /// A press rather than a release.
    pub pressed: bool,
}

enum Step {
    Key(Key, usize),
    /// A bracketed paste starts after this many bytes.
    Paste(usize),
    Skip(usize),
    Wait,
}

/// A decoder that keeps a sequence split across reads until the rest arrives.
///
/// # Examples
///
/// ```
/// use pinfold_tools::keys::{Key, Keys};
///
/// let mut keys = Keys::default();
/// keys.push(b"j\x1b[");
/// assert_eq!(keys.next(false), Some(Key::Char('j')));
/// assert_eq!(keys.next(false), None, "the arrow is not complete");
/// keys.push(b"A");
/// assert_eq!(keys.next(false), Some(Key::Up));
/// ```
#[derive(Debug, Default)]
pub struct Keys {
    pending: Vec<u8>,
    /// Inside a bracketed paste, whose bytes are dropped: pasted text never answers a tool's
    /// confirmation (ADR-0045 section 2).
    pasting: bool,
}

/// Ends a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

impl Keys {
    /// Adds bytes read from the terminal.
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next key, or `None` once the bytes are used up or end in part of a sequence. With
    /// `quiet`, input has gone quiet, so a lone ESC is the Esc key and other parts are dropped.
    pub fn next(&mut self, quiet: bool) -> Option<Key> {
        loop {
            if self.pasting {
                let end = self
                    .pending
                    .windows(PASTE_END.len())
                    .position(|w| w == PASTE_END);
                let Some(end) = end else {
                    // Keep what may be the start of the end marker.
                    let keep = self.pending.len().saturating_sub(PASTE_END.len() - 1);
                    self.pending.drain(..keep);
                    return None;
                };
                self.pending.drain(..end + PASTE_END.len());
                self.pasting = false;
            }
            match step(&self.pending) {
                Step::Key(key, length) => {
                    self.pending.drain(..length);
                    return Some(key);
                }
                Step::Skip(length) => drop(self.pending.drain(..length)),
                Step::Paste(length) => {
                    self.pending.drain(..length);
                    self.pasting = true;
                }
                Step::Wait if quiet && !self.pending.is_empty() => {
                    if self.pending.remove(0) == 0x1b {
                        return Some(Key::Esc);
                    }
                }
                Step::Wait => return None,
            }
        }
    }
}

fn step(input: &[u8]) -> Step {
    let Some(&first) = input.first() else {
        return Step::Wait;
    };
    let key = match first {
        0x1b => return escape(input),
        b'\r' | b'\n' => Key::Enter,
        b'\t' => Key::Tab,
        0x08 | 0x7f => Key::Backspace,
        0x00..=0x1f => Key::Ctrl(char::from(first | 0x40).to_ascii_lowercase()),
        _ => return character(input),
    };
    Step::Key(key, 1)
}

fn character(input: &[u8]) -> Step {
    let length = match input[0] {
        0x20..=0x7e => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Step::Skip(1),
    };
    match input.get(..length).map(std::str::from_utf8) {
        Some(Ok(text)) => text
            .chars()
            .next()
            .map_or(Step::Skip(1), |c| Step::Key(Key::Char(c), length)),
        Some(Err(_)) => Step::Skip(1),
        None if input[1..].iter().all(|byte| byte & 0xc0 == 0x80) => Step::Wait,
        None => Step::Skip(1),
    }
}

fn escape(input: &[u8]) -> Step {
    match input.get(1) {
        None => Step::Wait,
        Some(b'[') => csi(input),
        Some(b'O') => match input.get(2) {
            None => Step::Wait,
            Some(&end) => cursor(end).map_or(Step::Skip(3), |key| Step::Key(key, 3)),
        },
        // Alt and a key: the Esc, then the key.
        Some(_) => Step::Key(Key::Esc, 1),
    }
}

fn cursor(end: u8) -> Option<Key> {
    Some(match end {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => return None,
    })
}

fn csi(input: &[u8]) -> Step {
    let mut end = 2;
    loop {
        if end == MAX_SEQUENCE {
            return Step::Skip(end);
        }
        match input.get(end) {
            None => return Step::Wait,
            Some(0x30..=0x3f) => end += 1,
            Some(0x40..=0x7e) => break,
            Some(_) => return Step::Skip(end),
        }
    }
    // Only bytes 0x30 to 0x3f, so ASCII.
    let parameters = std::str::from_utf8(&input[2..end]).unwrap_or_default();
    let key = match (input[end], parameters) {
        (b'M' | b'm', parameters) if parameters.starts_with('<') => {
            mouse(&parameters[1..], input[end] == b'M')
        }
        (b'~', "200") => return Step::Paste(end + 1),
        (b'~', parameters) => match parameters.split(';').next() {
            Some("1" | "7") => Some(Key::Home),
            Some("4" | "8") => Some(Key::End),
            Some("3") => Some(Key::Delete),
            Some("5") => Some(Key::PageUp),
            Some("6") => Some(Key::PageDown),
            _ => None,
        },
        (final_byte, _) => cursor(final_byte),
    };
    key.map_or(Step::Skip(end + 1), |key| Step::Key(key, end + 1))
}

fn mouse(parameters: &str, pressed: bool) -> Option<Key> {
    let mut numbers = parameters.split(';').map(str::parse::<u16>);
    let (Some(Ok(button)), Some(Ok(column)), Some(Ok(row)), None) = (
        numbers.next(),
        numbers.next(),
        numbers.next(),
        numbers.next(),
    ) else {
        return None;
    };
    Some(Key::Mouse(Mouse {
        button,
        column: column.saturating_sub(1),
        row: row.saturating_sub(1),
        pressed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Key> {
        let mut keys = Keys::default();
        keys.push(bytes);
        std::iter::from_fn(|| keys.next(true)).collect()
    }

    #[test]
    fn pasted_text_is_dropped_even_split_across_reads() {
        assert_eq!(
            decode(b"j\x1b[200~ dy\r\x1b[201~k"),
            [Key::Char('j'), Key::Char('k')]
        );
        let mut keys = Keys::default();
        keys.push(b"\x1b[200~y\x1b[20");
        assert_eq!(keys.next(true), None);
        keys.push(b"1~q");
        assert_eq!(keys.next(true), Some(Key::Char('q')));
    }

    #[test]
    fn the_keys_every_tool_uses() {
        let keys = decode(b"hjkl/?q\x1b[A\x1b[B\x1b[C\x1b[D\x1bOA\r\x7f");
        let chars = "hjkl/?q".chars().map(Key::Char);
        let rest = [
            Key::Up,
            Key::Down,
            Key::Right,
            Key::Left,
            Key::Up,
            Key::Enter,
            Key::Backspace,
        ];
        assert_eq!(keys, chars.chain(rest).collect::<Vec<_>>());
    }

    #[test]
    fn control_keys_utf8_and_tilde_sequences() {
        let keys = decode("\x03é\x1b[5~\x1b[6~\x1b[3~\x1b[1;5A".as_bytes());
        let expected = [
            Key::Ctrl('c'),
            Key::Char('é'),
            Key::PageUp,
            Key::PageDown,
            Key::Delete,
            Key::Up,
        ];
        assert_eq!(keys, expected);
    }

    #[test]
    fn sgr_mouse_reports() {
        let keys = decode(b"\x1b[<0;10;5M\x1b[<65;1;1m");
        let press = Mouse {
            button: 0,
            column: 9,
            row: 4,
            pressed: true,
        };
        let wheel = Mouse {
            button: 65,
            column: 0,
            row: 0,
            pressed: false,
        };
        assert_eq!(keys, [Key::Mouse(press), Key::Mouse(wheel)]);
    }

    #[test]
    fn a_lone_esc_waits_until_input_goes_quiet() {
        let mut keys = Keys::default();
        keys.push(b"\x1b");
        assert_eq!(keys.next(false), None);
        assert_eq!(keys.next(true), Some(Key::Esc));
        assert_eq!(keys.next(true), None);
    }

    #[test]
    fn junk_is_dropped_without_losing_what_follows() {
        let long = [b"\x1b[".as_slice(), &[b'1'; 30], b"q"].concat();
        assert_eq!(decode(&long), [Key::Char('q')]);
        assert_eq!(decode(b"\xff\x1b[99Zq\x1b[<1;2M"), [Key::Char('q')]);
    }
}
