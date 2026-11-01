//! Pasted text, which must reach a program as text and nothing else (T7 in the threat model).

const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";

/// Returns the bytes to send a program for pasted `text`.
///
/// Every control character goes except tab and line ends. That covers ESC and C1 CSI, so
/// text cannot end a bracketed paste early. A program that has not asked for `bracketed`
/// paste gets line ends as the Return key sends them.
///
/// ```
/// let bytes = pinfold_core::paste::encode("ls\x1b[201~\x03\n", true);
/// assert_eq!(bytes, b"\x1b[200~ls[201~\n\x1b[201~");
/// ```
#[must_use]
pub fn encode(text: &str, bracketed: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + START.len() + END.len());
    if bracketed {
        bytes.extend_from_slice(START);
    }
    let mut previous = '\0';
    for character in text.chars() {
        match character {
            '\n' if !bracketed && previous == '\r' => {}
            '\n' if !bracketed => bytes.push(b'\r'),
            '\t' | '\n' | '\r' => bytes.push(character as u8),
            _ if character.is_control() => {}
            _ => bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes()),
        }
        previous = character;
    }
    if bracketed {
        bytes.extend_from_slice(END);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paste_cannot_end_its_brackets() {
        for text in ["a\x1b[201~rm -rf /\r", "a\u{9b}201~rm -rf /\r"] {
            let bytes = encode(text, true);
            let body = &bytes[START.len()..bytes.len() - END.len()];
            assert!(!body.contains(&0x1b), "{text:?}");
            assert!(body.ends_with(b"rm -rf /\r"));
            assert!(!std::str::from_utf8(body).unwrap().contains('\u{9b}'));
        }
    }

    #[test]
    fn control_characters_are_stripped() {
        let controls: String = (0..0x20_u8)
            .map(char::from)
            .chain('\u{7f}'..='\u{9f}')
            .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
            .collect();
        assert_eq!(encode(&format!("a{controls}b"), false), b"ab");
    }

    #[test]
    fn text_survives() {
        let text = "tab\there 漢字 é 🦀 ~[201";
        assert_eq!(encode(text, false), text.as_bytes());
    }

    #[test]
    fn line_ends_become_return_without_brackets() {
        assert_eq!(encode("a\nb\r\nc\rd", false), b"a\rb\rc\rd");
        assert_eq!(encode("a\nb\r\nc", true)[START.len()..][..6], *b"a\nb\r\nc");
    }

    #[test]
    fn output_never_outgrows_the_text_and_its_brackets() {
        let text = "\u{7f}é\n".repeat(100);
        assert!(encode(&text, true).len() <= text.len() + START.len() + END.len());
    }
}
