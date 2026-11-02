//! Turns key presses into the bytes a terminal program expects, the way xterm does.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::prefix::Command;

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

/// Whether this is a shortcut for `letter`: Command on macOS, Ctrl+Shift elsewhere.
fn is_shortcut(key: &Key, modifiers: ModifiersState, letter: &str) -> bool {
    let held = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL | ModifiersState::SHIFT
    };
    modifiers == held && matches!(key, Key::Character(text) if text.eq_ignore_ascii_case(letter))
}

/// Whether this is the paste shortcut: Command+V on macOS, Ctrl+Shift+V elsewhere.
pub fn is_paste(key: &Key, modifiers: ModifiersState) -> bool {
    is_shortcut(key, modifiers, "v")
}

/// Whether this is the copy shortcut, which plain Ctrl+C is not.
pub fn is_copy(key: &Key, modifiers: ModifiersState) -> bool {
    is_shortcut(key, modifiers, "c")
}

/// Whether this is the shortcut that opens the search bar.
pub fn is_find(key: &Key, modifiers: ModifiersState) -> bool {
    is_shortcut(key, modifiers, "f")
}

/// Whether this is the shortcut that turns vi mode on and off.
pub fn is_vi(key: &Key, modifiers: ModifiersState) -> bool {
    is_shortcut(key, modifiers, "x")
}

/// The Mac's own keys for tabs: Command+T, and Command with a digit from 1, whatever the tab
/// strip's width. Command is not a modifier programs hear of, so no program loses these.
pub fn tab_key(key: &Key, modifiers: ModifiersState) -> Option<Command> {
    let Key::Character(text) = key else {
        return None;
    };
    if modifiers != ModifiersState::SUPER {
        return None;
    }
    match text.as_bytes() {
        [b't' | b'T'] => Some(Command::NewTab),
        [digit @ b'1'..=b'9'] => Some(Command::Tab(usize::from(digit - b'1'))),
        _ => None,
    }
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
/// `app_cursor` is the program's application cursor mode. `option_as_alt` is the
/// `macos_option_as_alt` setting.
pub fn encode(
    key: &Key,
    text: Option<&str>,
    modifiers: ModifiersState,
    app_cursor: bool,
    option_as_alt: bool,
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
    // Option composes text on macOS by default, as in Terminal.app.
    if cfg!(target_os = "macos") && !option_as_alt {
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
        let key = Key::Character(text.into());
        encode(&key, Some(text), modifiers, false, false)
    }

    fn named(key: NamedKey, modifiers: ModifiersState) -> Vec<u8> {
        encode(&Key::Named(key), None, modifiers, false, false)
    }

    #[test]
    fn command_opens_and_picks_tabs_and_nothing_else_does() {
        let key = |text: &str| Key::Character(text.into());
        let command = ModifiersState::SUPER;
        assert_eq!(tab_key(&key("t"), command), Some(Command::NewTab));
        assert_eq!(tab_key(&key("1"), command), Some(Command::Tab(0)));
        assert_eq!(tab_key(&key("9"), command), Some(Command::Tab(8)));
        assert_eq!(tab_key(&key("0"), command), None);
        assert_eq!(tab_key(&key("n"), command), None);
        assert_eq!(tab_key(&key("t"), ModifiersState::empty()), None);
        assert_eq!(tab_key(&key("t"), ModifiersState::CONTROL), None);
        assert_eq!(tab_key(&key("t"), command | ModifiersState::SHIFT), None);
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
        let press = |key, modifiers| encode(&Key::Named(key), None, modifiers, true, false);
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
            encode(
                &Key::Named(NamedKey::Space),
                Some(" "),
                CONTROL,
                false,
                false
            ),
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
    fn option_as_alt_prefixes_escape_everywhere() {
        let key = Key::Character("b".into());
        assert_eq!(encode(&key, Some("b"), ALT, false, true), b"\x1bb");
        assert_eq!(encode(&key, Some("b"), NONE, false, true), b"b");
    }

    #[test]
    fn super_sends_nothing() {
        assert!(press("c", ModifiersState::SUPER).is_empty());
        assert!(named(NamedKey::ArrowUp, ModifiersState::SUPER).is_empty());
    }

    #[test]
    fn shortcuts_need_exactly_their_modifiers() {
        let held = if cfg!(target_os = "macos") {
            ModifiersState::SUPER
        } else {
            CONTROL | SHIFT
        };
        let key = |letter: &str| Key::Character(letter.into());
        assert!(is_copy(&key("c"), held));
        assert!(is_copy(&key("C"), held));
        assert!(is_find(&key("f"), held));
        assert!(is_paste(&key("v"), held));
        assert!(is_vi(&key("x"), held));
        assert!(!is_vi(&key("x"), CONTROL));
        assert!(!is_copy(&key("x"), held));
        assert!(!is_copy(&key("c"), CONTROL));
        assert!(!is_find(&key("f"), CONTROL));
        assert!(!is_copy(&key("c"), held | ALT));
    }

    #[test]
    fn keys_without_bytes_send_nothing() {
        assert!(named(NamedKey::Shift, SHIFT).is_empty());
        assert!(named(NamedKey::CapsLock, NONE).is_empty());
    }
}
