//! Turns mouse events into the reports a terminal program asked for.

use pinfold_proto::{MouseEncoding, MouseMode};
use winit::keyboard::ModifiersState;

/// A mouse event at a cell. Buttons are numbered as in reports: left, middle, right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Press(u8),
    Release(u8),
    /// The pointer reached a new cell, with this button held.
    Motion(Option<u8>),
    /// One line of wheel, up if true.
    Wheel(bool),
}

const NO_BUTTON: u8 = 3;
const MOTION: u8 = 32;
const WHEEL: u8 = 64;

fn wanted(event: Event, mode: MouseMode) -> bool {
    match (event, mode) {
        (_, MouseMode::Off) => false,
        (Event::Motion(None), mode) => mode == MouseMode::Motion,
        (Event::Motion(Some(_)), mode) => mode != MouseMode::Click,
        _ => true,
    }
}

fn button(event: Event, encoding: MouseEncoding) -> u8 {
    match event {
        Event::Press(button) => button,
        // Only SGR can say which button was released.
        Event::Release(button) if encoding == MouseEncoding::Sgr => button,
        Event::Release(_) => NO_BUTTON,
        Event::Motion(button) => button.unwrap_or(NO_BUTTON) + MOTION,
        Event::Wheel(up) => WHEEL + u8::from(!up),
    }
}

fn modifier_bits(modifiers: ModifiersState) -> u8 {
    4 * u8::from(modifiers.shift_key())
        + 8 * u8::from(modifiers.alt_key())
        + 16 * u8::from(modifiers.control_key())
}

/// Writes a code offset by 32, or returns false if the encoding cannot hold it.
fn put(out: &mut Vec<u8>, code: u32, encoding: MouseEncoding) -> bool {
    let code = code + 32;
    if encoding == MouseEncoding::Utf8 {
        let Some(character) = char::from_u32(code).filter(|_| code < 0x800) else {
            return false;
        };
        out.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
        return true;
    }
    match u8::try_from(code) {
        Ok(byte) => {
            out.push(byte);
            true
        }
        Err(_) => false,
    }
}

/// Returns the report for `event` at the zero-based `(row, column)`, or nothing if the
/// program did not ask for it or its encoding cannot reach that cell.
pub fn encode(
    event: Event,
    (row, column): (u16, u16),
    modifiers: ModifiersState,
    mode: MouseMode,
    encoding: MouseEncoding,
) -> Vec<u8> {
    if !wanted(event, mode) {
        return Vec::new();
    }
    let code = button(event, encoding) + modifier_bits(modifiers);
    let (x, y) = (u32::from(column) + 1, u32::from(row) + 1);
    if encoding == MouseEncoding::Sgr {
        let end = if matches!(event, Event::Release(_)) {
            'm'
        } else {
            'M'
        };
        return format!("\x1b[<{code};{x};{y}{end}").into_bytes();
    }
    let mut out = b"\x1b[M".to_vec();
    out.push(code + 32);
    if put(&mut out, x, encoding) && put(&mut out, y, encoding) {
        out
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifiersState = ModifiersState::empty();

    fn sgr(event: Event) -> Vec<u8> {
        encode(event, (4, 9), NONE, MouseMode::Motion, MouseEncoding::Sgr)
    }

    fn legacy(event: Event, cell: (u16, u16)) -> Vec<u8> {
        encode(event, cell, NONE, MouseMode::Click, MouseEncoding::Legacy)
    }

    #[test]
    fn sgr_reports_every_event() {
        assert_eq!(sgr(Event::Press(0)), b"\x1b[<0;10;5M");
        assert_eq!(sgr(Event::Press(2)), b"\x1b[<2;10;5M");
        assert_eq!(sgr(Event::Release(2)), b"\x1b[<2;10;5m");
        assert_eq!(sgr(Event::Motion(Some(0))), b"\x1b[<32;10;5M");
        assert_eq!(sgr(Event::Motion(None)), b"\x1b[<35;10;5M");
        assert_eq!(sgr(Event::Wheel(true)), b"\x1b[<64;10;5M");
        assert_eq!(sgr(Event::Wheel(false)), b"\x1b[<65;10;5M");
    }

    #[test]
    fn legacy_reports_are_offset_by_32() {
        assert_eq!(legacy(Event::Press(0), (4, 9)), b"\x1b[M\x20\x2a\x25");
        assert_eq!(legacy(Event::Release(0), (4, 9)), b"\x1b[M\x23\x2a\x25");
        assert_eq!(legacy(Event::Wheel(true), (0, 0)), b"\x1b[M\x60\x21\x21");
    }

    #[test]
    fn legacy_stops_where_a_byte_runs_out() {
        assert_eq!(legacy(Event::Press(0), (0, 222)), b"\x1b[M\x20\xff\x21");
        assert!(legacy(Event::Press(0), (0, 223)).is_empty());
        assert!(legacy(Event::Press(0), (223, 0)).is_empty());
    }

    #[test]
    fn utf8_reaches_further_than_legacy() {
        let report = encode(
            Event::Press(0),
            (0, 299),
            NONE,
            MouseMode::Click,
            MouseEncoding::Utf8,
        );
        let mut expected = b"\x1b[M\x20".to_vec();
        expected.extend_from_slice("\u{14c}\u{21}".as_bytes());
        assert_eq!(report, expected);
    }

    #[test]
    fn sgr_reaches_the_largest_window() {
        let report = encode(
            Event::Press(0),
            (1023, 1023),
            NONE,
            MouseMode::Click,
            MouseEncoding::Sgr,
        );
        assert_eq!(report, b"\x1b[<0;1024;1024M");
    }

    #[test]
    fn modifiers_add_their_bits() {
        let press = |modifiers| {
            encode(
                Event::Press(0),
                (0, 0),
                modifiers,
                MouseMode::Click,
                MouseEncoding::Sgr,
            )
        };
        assert_eq!(press(ModifiersState::SHIFT), b"\x1b[<4;1;1M");
        assert_eq!(press(ModifiersState::ALT), b"\x1b[<8;1;1M");
        assert_eq!(press(ModifiersState::CONTROL), b"\x1b[<16;1;1M");
    }

    #[test]
    fn programs_get_only_the_events_they_asked_for() {
        let sent = |event, mode| !encode(event, (0, 0), NONE, mode, MouseEncoding::Sgr).is_empty();
        assert!(!sent(Event::Press(0), MouseMode::Off));
        assert!(!sent(Event::Wheel(true), MouseMode::Off));
        assert!(sent(Event::Press(0), MouseMode::Click));
        assert!(sent(Event::Wheel(true), MouseMode::Click));
        assert!(!sent(Event::Motion(Some(0)), MouseMode::Click));
        assert!(sent(Event::Motion(Some(0)), MouseMode::Drag));
        assert!(!sent(Event::Motion(None), MouseMode::Drag));
        assert!(sent(Event::Motion(None), MouseMode::Motion));
    }
}
