//! Runs a program, such as esctest, on a PTY with `pinfold-core`'s VT engine as its
//! terminal, and exits when the program does.
//!
//! esctest reads the screen with DECRQCRA, a checksum of a rectangle, which the engine does
//! not answer. The driver answers it from the engine's grid, as a test oracle, so screen
//! tests check the engine's cells and not the driver.
//!
//! ```text
//! cargo run -p pinfold-tests --bin esctest-driver -- python3 esctest.py --logfile log.txt
//! ```

use std::io::{self, ErrorKind, Read, Write};
use std::process::Command;
use std::thread;
use std::time::Duration;

use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use pinfold_proto::Frame;
use pinfold_sys::pty::Pty;

/// esctest resizes to 25 rows of 80 at the start of every test, which the engine ignores.
const ROWS: u16 = 25;
const COLUMNS: u16 = 80;

fn main() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let Some(program) = arguments.next() else {
        eprintln!("usage: esctest-driver PROGRAM [ARGUMENT...]");
        std::process::exit(2);
    };
    let mut command = Command::new(program);
    command.args(arguments);
    let mut pty = Pty::spawn(command, ROWS, COLUMNS)?;
    let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
    let (mut pending, mut replies, mut frame) = (Vec::new(), Vec::new(), Frame::default());
    let mut buffer = [0; 4096];
    loop {
        match pty.master().read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                pending.extend_from_slice(&buffer[..read]);
                let held = feed(&mut engine, &pending, &mut replies, &mut frame);
                pending.drain(..pending.len() - held);
                pty.master().write_all(&replies)?;
                replies.clear();
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if pty.try_wait()?.is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            // Linux reports EIO on the master once the program has exited.
            Err(_) if pty.try_wait()?.is_some() => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Feeds `bytes` to `engine`, answering each DECRQCRA after the bytes before it, and returns
/// how many bytes at the end it held back as the start of a sequence.
fn feed(
    engine: &mut AlacrittyEngine,
    bytes: &[u8],
    replies: &mut Vec<u8>,
    frame: &mut Frame,
) -> usize {
    let mut start = 0;
    let mut search = 0;
    while let Some(offset) = bytes[search..].iter().position(|&byte| byte == 0x1b) {
        let escape = search + offset;
        let rest = &bytes[escape..];
        let digits = rest
            .iter()
            .skip(2)
            .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
            .count();
        let tail = rest.get(2 + digits..).unwrap_or_default();
        let cut = rest.len() == 1 || (rest[1] == b'[' && tail.len() < 2 && b"*y".starts_with(tail));
        if cut {
            engine.feed(&bytes[start..escape], replies);
            return bytes.len() - escape;
        }
        if rest[1] == b'[' && tail.starts_with(b"*y") {
            engine.feed(&bytes[start..escape], replies);
            engine.frame(frame);
            let parameters = std::str::from_utf8(&rest[2..2 + digits]).unwrap_or_default();
            replies.extend_from_slice(checksum(frame, parameters).as_bytes());
            start = escape + 2 + digits + 2;
            search = start;
        } else {
            search = escape + 1;
        }
    }
    engine.feed(&bytes[start..], replies);
    0
}

/// Answers `CSI Pid ; Pp ; Pt ; Pl ; Pb ; Pr * y` with xterm's checksum before patch 279,
/// the negated sum of the characters, which is what esctest expects by default.
// shortcut: characters only, ignoring attributes and origin mode, as esctest asks one cell
// at a time with neither in play for most tests. Add them if a test needs them.
fn checksum(frame: &Frame, parameters: &str) -> String {
    let mut numbers = parameters
        .split(';')
        .map(|number| number.parse::<usize>().unwrap_or(0));
    let mut next = |default: usize| {
        numbers
            .next()
            .filter(|&number| number > 0)
            .unwrap_or(default)
    };
    let (id, _page) = (next(0), next(1));
    let (rows, columns) = (usize::from(frame.rows), usize::from(frame.columns));
    let (top, left) = (next(1), next(1));
    let (bottom, right) = (next(rows).min(rows), next(columns).min(columns));
    let mut sum: u32 = 0;
    for row in top..=bottom {
        for column in left..=right {
            let cell = frame.cells.get((row - 1) * columns + column - 1);
            sum = sum.wrapping_add(cell.map_or(0, |cell| u32::from(cell.character)));
        }
    }
    format!(
        "\x1bP{id}!~{:04X}\x1b\\",
        0x10000u32.wrapping_sub(sum) & 0xffff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checksum_request_split_across_reads_is_answered_after_the_text_before_it() {
        let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
        let (mut replies, mut frame) = (Vec::new(), Frame::default());
        let bytes = b"A\x1b[7;1;1;1;1;1*";
        assert_eq!(feed(&mut engine, bytes, &mut replies, &mut frame), 14);
        assert!(replies.is_empty());
        assert_eq!(
            feed(&mut engine, b"\x1b[7;1;1;1;1;1*y", &mut replies, &mut frame),
            0
        );
        // xterm before patch 279 sends the sum negated: 0x10000 - 'A'.
        assert_eq!(replies, b"\x1bP7!~FFBF\x1b\\");
    }

    #[test]
    fn other_sequences_reach_the_engine() {
        let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
        let (mut replies, mut frame) = (Vec::new(), Frame::default());
        assert_eq!(
            feed(&mut engine, b"\x1b[2;3H\x1b[6n", &mut replies, &mut frame),
            0
        );
        assert_eq!(replies, b"\x1b[2;3R");
    }
}
