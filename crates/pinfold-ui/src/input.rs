//! Turns key presses into the bytes a terminal program expects, the way xterm does.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// How a key that has no text is written.
#[derive(Clone, Copy)]
enum Sequence {
    /// `ESC [ <letter>`, or `ESC O <letter>` in application cursor mode.
    Cursor(u8),
    /// `ESC O <letter>`, the first four function keys.
    Function(u8),
    /// `ESC [ <number> ~`.
    Tilde(u8),
}

fn sequence(key: NamedKey) -> Option<Sequence> {
    use Sequence::{Cursor, Function, Tilde};
    Some(match key {
        NamedKey::ArrowUp => Cursor(b'A'),
        NamedKey::ArrowDown => Cursor(b'B'),
        NamedKey::ArrowRight => Cursor(b'C'),
        NamedKey::ArrowLeft => Cursor(b'D'),
        NamedKey::Home => Cursor(b'H'),
        NamedKey::End => Cursor(b'F'),
        NamedKey::F1 => Function(b'P'),
        NamedKey::F2 => Function(b'Q'),
        NamedKey::F3 => Function(b'R'),
        NamedKey::F4 => Function(b'S'),
        NamedKey::Insert => Tilde(2),
        NamedKey::Delete => Tilde(3),
        NamedKey::PageUp => Tilde(5),
        NamedKey::PageDown => Tilde(6),
        NamedKey::F5 => Tilde(15),
        NamedKey::F6 => Tilde(17),
        NamedKey::F7 => Tilde(18),
        NamedKey::F8 => Tilde(19),
        NamedKey::F9 => Tilde(20),
        NamedKey::F10 => Tilde(21),
        NamedKey::F11 => Tilde(23),
        NamedKey::F12 => Tilde(24),
        _ => return None,
    })
}

/// Whether this is the paste shortcut: Command+V on macOS, Ctrl+Shift+V elsewhere.
pub fn is_paste(key: &Key, modifiers: ModifiersState) -> bool {
    let held = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL | ModifiersState::SHIFT
    };
    modifiers == held && matches!(key, Key::Character(text) if text.eq_ignore_ascii_case("v"))
}

/// xterm's modifier parameter, or nothing without modifiers.
fn parameter(modifiers: ModifiersState) -> Option<u8> {
    let bits = u8::from(modifiers.shift_key())
        + 2 * u8::from(modifiers.alt_key())
        + 4 * u8::from(modifiers.control_key());
    (bits > 0).then_some(bits + 1)
}

fn write(sequence: Sequence, modifiers: ModifiersState, app_cursor: bool) -> Vec<u8> {
    let plain = |letter: u8, ss3: bool| vec![0x1b, if ss3 { b'O' } else { b'[' }, letter];
    match (sequence, parameter(modifiers)) {
        (Sequence::Cursor(letter), None) => plain(letter, app_cursor),
        (Sequence::Function(letter), None) => plain(letter, true),
        (Sequence::Tilde(number), None) => format!("\x1b[{number}~").into_bytes(),
        (Sequence::Tilde(number), Some(modifier)) => {
            format!("\x1b[{number};{modifier}~").into_bytes()
        }
        (Sequence::Cursor(letter) | Sequence::Function(letter), Some(modifier)) => {
            format!("\x1b[1;{modifier}{}", char::from(letter)).into_bytes()
        }
    }
}

/// A key that writes one byte, before Alt.
fn single(key: NamedKey, modifiers: ModifiersState) -> Option<u8> {
    Some(match key {
        NamedKey::Enter => b'\r',
        NamedKey::Backspace if modifiers.control_key() => 0x08,
        NamedKey::Backspace => 0x7f,
        NamedKey::Tab => b'\t',
        NamedKey::Escape => 0x1b,
        NamedKey::Space if modifiers.control_key() => 0,
        _ => return None,
    })
}

/// The control byte for Ctrl and `character`, as xterm maps them.
fn control(character: &str) -> Option<u8> {
    let [byte] = character.as_bytes() else {
        return None;
    };
    Some(match byte {
        b'@'..=b'_' | b'a'..=b'z' => byte & 0x1f,
        b'2' | b' ' => 0,
        b'3'..=b'7' => byte - b'3' + 0x1b,
        b'8' | b'?' => 0x7f,
        b'/' => 0x1f,
        _ => return None,
    })
}

/// Prefixes ESC when Alt is held, which is how terminals have always sent Meta.
fn with_alt(modifiers: ModifiersState, bytes: &[u8]) -> Vec<u8> {
    let prefix = modifiers.alt_key().then_some(0x1b);
    prefix.into_iter().chain(bytes.iter().copied()).collect()
}

/// Returns the bytes for a key press, or nothing if the key sends none.
///
/// `app_cursor` is the program's application cursor mode.
pub fn encode(
    key: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
    app_cursor: bool,
) -> Vec<u8> {
    // Super belongs to the desktop and to Pinfold's own shortcuts, such as Cmd+C.
    if modifiers.super_key() {
        return Vec::new();
    }
    if let Key::Named(named) = key {
        if let Some(sequence) = sequence(*named) {
            return write(sequence, modifiers, app_cursor);
        }
        if *named == NamedKey::Tab && modifiers.shift_key() {
            return b"\x1b[Z".to_vec();
        }
        if let Some(byte) = single(*named, modifiers) {
            return with_alt(modifiers, &[byte]);
        }
    }
    if modifiers.control_key()
        && let Key::Character(character) = key
        && let Some(byte) = control(character)
    {
        return with_alt(modifiers, &[byte]);
    }
    let text = text.unwrap_or_default().as_bytes();
    // shortcut: Option composes text on macOS, as in Terminal.app, make it a setting in #23.
    if cfg!(target_os = "macos") {
        text.to_vec()
    } else {
        with_alt(modifiers, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifiersState = ModifiersState::empty();
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const ALT: ModifiersState = ModifiersState::ALT;
    const CONTROL: ModifiersState = ModifiersState::CONTROL;

    fn press(text: &str, modifiers: ModifiersState) -> Vec<u8> {
        encode(&Key::Character(text.into()), Some(text), modifiers, false)
    }

    fn named(key: NamedKey, modifiers: ModifiersState) -> Vec<u8> {
        encode(&Key::Named(key), None, modifiers, false)
    }

    #[test]
    fn text_passes_through() {
        assert_eq!(press("é", NONE), "é".as_bytes());
    }

    #[test]
    fn control_letters_become_control_bytes() {
        assert_eq!(press("c", CONTROL), [3]);
        assert_eq!(press("C", CONTROL), [3]);
        assert_eq!(press("[", CONTROL), [0x1b]);
    }

    #[test]
    fn control_symbols_follow_xterm() {
        assert_eq!(press("2", CONTROL), [0]);
        assert_eq!(press("3", CONTROL), [0x1b]);
        assert_eq!(press("7", CONTROL), [0x1f]);
        assert_eq!(press("8", CONTROL), [0x7f]);
        assert_eq!(press("/", CONTROL), [0x1f]);
        assert_eq!(press("?", CONTROL), [0x7f]);
        assert_eq!(press("é", CONTROL), "é".as_bytes());
    }

    #[test]
    fn named_keys_send_sequences() {
        assert_eq!(named(NamedKey::ArrowUp, NONE), b"\x1b[A");
        assert_eq!(named(NamedKey::Enter, NONE), b"\r");
        assert_eq!(named(NamedKey::Insert, NONE), b"\x1b[2~");
        assert_eq!(named(NamedKey::PageDown, NONE), b"\x1b[6~");
    }

    #[test]
    fn function_keys_follow_xterm() {
        assert_eq!(named(NamedKey::F1, NONE), b"\x1bOP");
        assert_eq!(named(NamedKey::F4, NONE), b"\x1bOS");
        assert_eq!(named(NamedKey::F5, NONE), b"\x1b[15~");
        assert_eq!(named(NamedKey::F10, NONE), b"\x1b[21~");
        assert_eq!(named(NamedKey::F12, NONE), b"\x1b[24~");
        assert_eq!(named(NamedKey::F1, SHIFT), b"\x1b[1;2P");
        assert_eq!(named(NamedKey::F5, CONTROL), b"\x1b[15;5~");
    }

    #[test]
    fn application_cursor_mode_changes_only_cursor_keys() {
        let press = |key, modifiers| encode(&Key::Named(key), None, modifiers, true);
        assert_eq!(press(NamedKey::ArrowUp, NONE), b"\x1bOA");
        assert_eq!(press(NamedKey::Home, NONE), b"\x1bOH");
        assert_eq!(press(NamedKey::ArrowUp, CONTROL), b"\x1b[1;5A");
        assert_eq!(press(NamedKey::PageUp, NONE), b"\x1b[5~");
    }

    #[test]
    fn modifiers_become_a_parameter() {
        assert_eq!(named(NamedKey::ArrowLeft, SHIFT), b"\x1b[1;2D");
        assert_eq!(named(NamedKey::ArrowLeft, ALT), b"\x1b[1;3D");
        assert_eq!(named(NamedKey::ArrowLeft, CONTROL), b"\x1b[1;5D");
        assert_eq!(
            named(NamedKey::ArrowLeft, SHIFT | ALT | CONTROL),
            b"\x1b[1;8D"
        );
        assert_eq!(named(NamedKey::Delete, ALT), b"\x1b[3;3~");
    }

    #[test]
    fn editing_keys_follow_their_modifiers() {
        assert_eq!(named(NamedKey::Tab, SHIFT), b"\x1b[Z");
        assert_eq!(named(NamedKey::Backspace, CONTROL), [0x08]);
        assert_eq!(named(NamedKey::Backspace, ALT), [0x1b, 0x7f]);
        assert_eq!(named(NamedKey::Enter, ALT), b"\x1b\r");
        assert_eq!(
            encode(&Key::Named(NamedKey::Space), Some(" "), CONTROL, false),
            [0]
        );
    }

    #[test]
    fn alt_prefixes_escape_except_where_option_composes() {
        assert_eq!(press("c", ALT | CONTROL), [0x1b, 3]);
        let expected: &[u8] = if cfg!(target_os = "macos") {
            b"b"
        } else {
            b"\x1bb"
        };
        assert_eq!(press("b", ALT), expected);
    }

    #[test]
    fn super_sends_nothing() {
        assert!(press("c", ModifiersState::SUPER).is_empty());
        assert!(named(NamedKey::ArrowUp, ModifiersState::SUPER).is_empty());
    }

    #[test]
    fn keys_without_bytes_send_nothing() {
        assert!(named(NamedKey::Shift, SHIFT).is_empty());
        assert!(named(NamedKey::CapsLock, NONE).is_empty());
    }
}
