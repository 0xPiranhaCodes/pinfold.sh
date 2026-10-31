//! Turns key presses into the bytes a terminal program expects.

use winit::keyboard::{Key, ModifiersState, NamedKey};

fn named(key: NamedKey) -> Option<&'static [u8]> {
    Some(match key {
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",
        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",
        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::Delete => b"\x1b[3~",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        _ => return None,
    })
}

// shortcut: covers a basic shell, and #22 adds application cursor mode, F-keys, IME and paste.
/// Returns the bytes for a key press, or nothing if the key sends none.
pub fn encode(key: &Key, text: Option<&str>, modifiers: ModifiersState) -> Vec<u8> {
    if let Key::Named(key) = key
        && let Some(bytes) = named(*key)
    {
        return bytes.to_vec();
    }
    if modifiers.control_key()
        && let Key::Character(character) = key
        && let [byte @ (b'@'..=b'_' | b'a'..=b'z')] = character.as_bytes()
    {
        return vec![byte & 0x1f];
    }
    text.map(|text| text.as_bytes().to_vec())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(text: &str, modifiers: ModifiersState) -> Vec<u8> {
        encode(&Key::Character(text.into()), Some(text), modifiers)
    }

    #[test]
    fn text_passes_through() {
        assert_eq!(press("é", ModifiersState::empty()), "é".as_bytes());
    }

    #[test]
    fn control_letters_become_control_bytes() {
        assert_eq!(press("c", ModifiersState::CONTROL), [3]);
        assert_eq!(press("C", ModifiersState::CONTROL), [3]);
        assert_eq!(press("[", ModifiersState::CONTROL), [0x1b]);
    }

    #[test]
    fn named_keys_send_sequences() {
        let up = encode(
            &Key::Named(NamedKey::ArrowUp),
            None,
            ModifiersState::empty(),
        );
        assert_eq!(up, b"\x1b[A");
        let enter = encode(
            &Key::Named(NamedKey::Enter),
            Some("\r"),
            ModifiersState::empty(),
        );
        assert_eq!(enter, b"\r");
    }

    #[test]
    fn keys_without_bytes_send_nothing() {
        let shift = encode(&Key::Named(NamedKey::Shift), None, ModifiersState::SHIFT);
        assert!(shift.is_empty());
    }
}
