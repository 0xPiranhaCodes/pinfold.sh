//! The VT conformance matrix: escape sequences fed to a fresh 4 × 10 engine, each with the
//! result a reference terminal documents, and what PinFold shows. The matrix in
//! `docs/design/vt-conformance.md` is written from this table, and the test fails when that
//! file is stale, a passing case regresses, or a case fails without a known-gap issue.
//! `PINFOLD_UPDATE_CONFORMANCE=1 cargo test -p pinfold-core --test vt_conformance` rewrites it.

use std::fmt::Write;

use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use pinfold_proto::{BOLD, Frame, ITALIC};

const MATRIX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/design/vt-conformance.md"
);

struct Case {
    area: &'static str,
    name: &'static str,
    bytes: &'static [u8],
    observe: fn(&mut AlacrittyEngine) -> String,
    expected: &'static str,
    /// A key of `REFERENCES`, then the page or section it documents the case in.
    reference: &'static str,
    gap: Option<u32>,
}

macro_rules! cases {
    (@gap) => { None };
    (@gap $issue:literal) => { Some($issue) };
    ($($area:literal $name:literal $bytes:expr => $observe:expr, $expected:literal, $reference:literal $(, gap $issue:literal)?;)*) => {
        [$(Case { area: $area, name: $name, bytes: $bytes, observe: $observe, expected: $expected,
            reference: $reference, gap: cases!(@gap $($issue)?) },)*]
    };
}

const REFERENCES: [(&str, &str); 11] = [
    ("pinfold", "../adr/"),
    ("uax11", "https://www.unicode.org/reports/tr11/"),
    ("unicode", "https://www.unicode.org/versions/latest/"),
    ("vt100", "https://vt100.net/emu/dec_ansi_parser"),
    ("ghostty", "https://ghostty.org/docs/vt/"),
    (
        "xterm",
        "https://invisible-island.net/xterm/ctlseqs/ctlseqs.html",
    ),
    ("vttest", "https://invisible-island.net/vttest/"),
    (
        "ecma48",
        "https://ecma-international.org/publications-and-standards/standards/ecma-48/",
    ),
    (
        "osc8",
        "https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda",
    ),
    (
        "osc133",
        "https://gitlab.freedesktop.org/Per_Bothner/specifications/blob/master/proposals/semantic-prompts.md",
    ),
    (
        "sync",
        "https://contour-terminal.org/vt-extensions/synchronized-output/",
    ),
];

fn frame(engine: &AlacrittyEngine) -> Frame {
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    frame
}

/// Each row of the view as the window draws it, without trailing spaces, between bars.
fn screen(engine: &mut AlacrittyEngine) -> String {
    let frame = frame(engine);
    let rows = frame.cells.chunks(usize::from(frame.columns));
    let text = rows.map(|row| row.iter().map(|cell| cell.character).collect::<String>());
    text.map(|row| row.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("|")
}

/// The cursor as `row,column`, counted from 0.
fn cursor(engine: &mut AlacrittyEngine) -> String {
    frame(engine)
        .cursor
        .map_or("hidden".into(), |(row, column)| format!("{row},{column}"))
}

fn screen_cursor(engine: &mut AlacrittyEngine) -> String {
    format!("{} @{}", screen(engine), cursor(engine))
}

/// The first cell's colours and style, with a fresh engine's colours named.
fn cell(engine: &mut AlacrittyEngine) -> String {
    let blank = frame(&AlacrittyEngine::new(4, 10)).cells[0];
    let cell = frame(engine).cells[0];
    let name = |colour: u32| match colour & 0x00ff_ffff {
        c if c == blank.foreground & 0x00ff_ffff => "default-fg".into(),
        c if c == blank.background & 0x00ff_ffff => "default-bg".into(),
        c => format!("{c:06x}"),
    };
    let mut shown = format!("{} on {}", name(cell.foreground), name(cell.background));
    for (bit, word) in [(BOLD, " bold"), (ITALIC, " italic")] {
        if cell.style() & bit != 0 {
            shown += word;
        }
    }
    shown
}

fn modes(engine: &mut AlacrittyEngine) -> String {
    let frame = frame(engine);
    format!(
        "mouse {:?} {:?}, paste {}, app-cursor {}, alt-scroll {}",
        frame.mouse,
        frame.mouse_encoding,
        engine.bracketed_paste(),
        frame.app_cursor,
        frame.alternate_scroll
    )
}

fn link(engine: &mut AlacrittyEngine) -> String {
    let at = |column| engine.hyperlink(engine.position(0, column));
    format!("{:?} {:?}", at(0), at(1))
}

fn cwd(engine: &mut AlacrittyEngine) -> String {
    engine
        .cwd()
        .map_or("none".into(), |cwd| format!("{}:{}", cwd.host, cwd.path))
}

fn marks(engine: &mut AlacrittyEngine) -> String {
    let marks = engine.shell_marks();
    format!(
        "{:?} exit {:?}, {} finished, {} prompts",
        marks.phase, marks.exit, marks.finished, marks.prompts
    )
}

fn osc_colours(engine: &mut AlacrittyEngine) -> String {
    let (cursor, background) = engine.colours();
    let hex = |colour: Option<u32>| colour.map_or("none".into(), |c| format!("{c:06x}"));
    format!("cursor {}, background {}", hex(cursor), hex(background))
}

fn widen(engine: &mut AlacrittyEngine) -> String {
    engine.resize(4, 20);
    screen(engine)
}

/// The cursor's row stays put, so the line's start moves into history; this looks one row up.
fn narrow(engine: &mut AlacrittyEngine) -> String {
    engine.resize(4, 5);
    engine.scroll(1);
    screen(engine)
}

fn shrink_to_nothing(engine: &mut AlacrittyEngine) -> String {
    engine.resize(0, 0);
    format!("{:?} {}", engine.size(), screen(engine))
}

fn ended_sync(engine: &mut AlacrittyEngine) -> String {
    let held = screen(engine);
    engine.end_sync(&mut Vec::new());
    format!("held {held:?}, then {:?}", screen(engine))
}

#[rustfmt::skip]
const CASES: [Case; 89] = cases! {
    "Cursor" "CUP moves to row;column" b"\x1b[2;3H" => cursor, "1,2", "ghostty csi/cup";
    "Cursor" "CUP with no parameters goes home" b"ab\x1b[H" => cursor, "0,0", "ghostty csi/cup";
    "Cursor" "CUP clamps to the screen" b"\x1b[99;99H" => cursor, "3,9", "ghostty csi/cup";
    "Cursor" "CUU stops at the top" b"\x1b[3;3H\x1b[5A" => cursor, "0,2", "ghostty csi/cuu";
    "Cursor" "CUU 0 moves one" b"\x1b[3;1H\x1b[0A" => cursor, "1,0", "ghostty csi/cuu";
    "Cursor" "CUD moves down" b"\x1b[2B" => cursor, "2,0", "ghostty csi/cud";
    "Cursor" "CUF stops at the right margin" b"\x1b[20C" => cursor, "0,9", "ghostty csi/cuf";
    "Cursor" "CUB moves left" b"\x1b[1;5H\x1b[2D" => cursor, "0,2", "ghostty csi/cub";
    "Cursor" "CHA sets the column" b"\x1b[5G" => cursor, "0,4", "xterm CHA";
    "Cursor" "HPA sets the column" b"\x1b[7`" => cursor, "0,6", "ghostty csi/hpa";
    "Cursor" "VPA sets the row" b"\x1b[3d" => cursor, "2,0", "ghostty csi/vpa";
    "Cursor" "CNL goes to the start of a later row" b"ab\x1b[2E" => cursor, "2,0", "ghostty csi/cnl";
    "Cursor" "CPL goes to the start of an earlier row" b"\x1b[3;5H\x1b[2F" => cursor, "0,0", "ghostty csi/cpl";
    "Cursor" "BS stops at the left margin" b"\x08\x08" => cursor, "0,0", "ghostty control/bs";
    "Cursor" "CR and LF" b"ab\r\ncd" => screen_cursor, "ab|cd|| @1,2", "ghostty control/lf";
    "Cursor" "DECSC and DECRC" b"\x1b[2;3H\x1b7\x1b[H\x1b8" => cursor, "1,2", "ghostty esc/decrc";
    "Cursor" "the last column leaves a wrap pending" b"0123456789" => cursor, "0,9", "vttest 1";
    "Cursor" "CR clears a pending wrap" b"0123456789\rX" => screen_cursor, "X123456789||| @0,1", "vttest 1";
    "Erase" "ED 0 erases to the end of the screen" b"abc\r\ndef\x1b[1;2H\x1b[J" => screen, "a|||", "ghostty csi/ed";
    "Erase" "ED 1 erases to the cursor" b"abc\r\ndef\x1b[2;2H\x1b[1J" => screen, "|  f||", "ghostty csi/ed";
    "Erase" "ED 2 erases the screen, not moving the cursor" b"abc\x1b[2J" => screen_cursor, "||| @0,3", "ghostty csi/ed";
    "Erase" "EL 0 erases to the end of the line" b"abcdef\x1b[1;3H\x1b[K" => screen, "ab|||", "ghostty csi/el";
    "Erase" "EL 1 erases to the cursor" b"abcdef\x1b[1;3H\x1b[1K" => screen, "   def|||", "ghostty csi/el";
    "Erase" "EL 2 erases the line" b"abcdef\x1b[2K" => screen, "|||", "ghostty csi/el";
    "Erase" "ECH erases characters in place" b"abcdef\x1b[1;2H\x1b[2X" => screen_cursor, "a  def||| @0,1", "ghostty csi/ech";
    "Erase" "DECALN fills the screen with E" b"ab\x1b#8" => screen_cursor, "EEEEEEEEEE|EEEEEEEEEE|EEEEEEEEEE|EEEEEEEEEE @0,0", "ghostty esc/decaln";
    "Erase" "DECALN resets the scroll region" b"\x1b[1;2r\x1b#8\x1b[2;1H\nX" => screen, "EEEEEEEEEE|EEEEEEEEEE|XEEEEEEEEE|EEEEEEEEEE", "xterm DECALN";
    "Scroll" "LF at the foot scrolls the screen" b"1\r\n2\r\n3\r\n4\r\n5" => screen, "2|3|4|5", "ghostty control/lf";
    "Scroll" "DECSTBM homes the cursor" b"ab\x1b[2;3r" => cursor, "0,0", "ghostty csi/decstbm";
    "Scroll" "LF at a region's foot scrolls only the region" b"top\r\na\r\nb\r\nfoot\x1b[2;3r\x1b[3;1Hc\r\nd" => screen, "top|c|d|foot", "vttest 2";
    "Scroll" "SU scrolls up" b"a\r\nb\r\nc\r\nd\x1b[S" => screen, "b|c|d|", "ghostty csi/su";
    "Scroll" "SD scrolls down" b"a\r\nb\r\nc\r\nd\x1b[T" => screen, "|a|b|c", "ghostty csi/sd";
    "Scroll" "RI at the top scrolls down" b"a\r\nb\x1b[H\x1bM" => screen, "|a|b|", "ghostty esc/ri";
    "Scroll" "IND at the foot scrolls up" b"a\r\nb\r\nc\r\nd\x1bD" => screen, "b|c|d|", "ghostty esc/ind";
    "Scroll" "NEL is CR and LF" b"ab\x1bEc" => screen_cursor, "ab|c|| @1,1", "xterm NEL";
    "Insert/delete" "ICH inserts blanks" b"abcd\x1b[1;2H\x1b[2@" => screen, "a  bcd|||", "ghostty csi/ich";
    "Insert/delete" "DCH deletes characters" b"abcdef\x1b[1;2H\x1b[2P" => screen, "adef|||", "ghostty csi/dch";
    "Insert/delete" "IL inserts lines" b"a\r\nb\r\nc\x1b[2;1H\x1b[L" => screen, "a||b|c", "ghostty csi/il";
    "Insert/delete" "DL deletes lines" b"a\r\nb\r\nc\x1b[1;1H\x1b[M" => screen, "b|c||", "ghostty csi/dl";
    "Insert/delete" "IL outside the scroll region does nothing" b"a\r\nb\x1b[2;3r\x1b[4;1H\x1b[L" => screen, "a|b||", "ghostty csi/il";
    "Insert/delete" "IRM inserts text" b"abc\x1b[1;1H\x1b[4hX" => screen, "Xabc|||", "xterm IRM";
    "Insert/delete" "REP repeats the last character" b"a\x1b[3b" => screen, "aaaa|||", "ghostty csi/rep";
    "Tabs" "HT goes to the next stop of eight" b"\tx" => screen_cursor, "        x||| @0,9", "ecma48 HT";
    "Tabs" "HT stops at the right margin" b"\t\t\t" => cursor, "0,9", "ecma48 HT";
    "Tabs" "HTS sets a stop" b"\x1b[1;4H\x1bH\x1b[H\tx" => screen, "   x|||", "xterm HTS";
    "Tabs" "TBC 3 clears every stop" b"\x1b[3g\tx" => screen, "         x|||", "ghostty csi/tbc";
    "Tabs" "CBT goes back a stop" b"\x1b[1;10H\x1b[Z" => cursor, "0,8", "ghostty csi/cbt";
    "Tabs" "CHT goes forward stops" b"\x1b[2I" => cursor, "0,9", "ghostty csi/cht";
    "SGR" "38;2 sets a truecolor foreground" b"\x1b[38;2;1;2;3mX" => cell, "010203 on default-bg", "xterm SGR";
    "SGR" "38:2 with colons sets a truecolor foreground" b"\x1b[38:2::1:2:3mX" => cell, "010203 on default-bg", "xterm SGR";
    "SGR" "48;2 sets a truecolor background" b"\x1b[48;2;250;0;9mX" => cell, "default-fg on fa0009", "xterm SGR";
    "SGR" "38;5 picks from the 6x6x6 cube" b"\x1b[38;5;196mX" => cell, "ff0000 on default-bg", "xterm SGR";
    "SGR" "48;5 picks from the grey ramp" b"\x1b[48;5;232mX" => cell, "default-fg on 080808", "xterm SGR";
    "SGR" "1 is bold" b"\x1b[1mX" => cell, "default-fg on default-bg bold", "xterm SGR";
    "SGR" "3 is italic" b"\x1b[3mX" => cell, "default-fg on default-bg italic", "xterm SGR";
    "SGR" "7 swaps the colours" b"\x1b[7mX" => cell, "default-bg on default-fg", "xterm SGR";
    "SGR" "0 resets everything" b"\x1b[1;3;38;5;196m\x1b[0mX" => cell, "default-fg on default-bg", "xterm SGR";
    "SGR" "39 resets the foreground" b"\x1b[38;5;196;39mX" => cell, "default-fg on default-bg", "xterm SGR";
    "SGR" "OSC 4 changes a palette slot" b"\x1b]4;1;rgb:01/02/03\x07\x1b[31mX" => cell, "010203 on default-bg", "ghostty osc/4";
    "Unicode" "a wide character takes two columns" "中a".as_bytes() => screen_cursor, "中 a||| @0,3", "uax11";
    "Unicode" "a wide character at the last column wraps" "\x1b[1;10H中".as_bytes() => screen_cursor, "|中|| @1,2", "uax11";
    "Unicode" "a combining mark joins the cell before it" "e\u{301}x".as_bytes() => screen_cursor, "e\u{301}x||| @0,2", "unicode 2.11 Combining Characters", gap 446;
    "Wrap" "autowrap continues on the next row" b"0123456789ab" => screen_cursor, "0123456789|ab|| @1,2", "vttest 1";
    "Wrap" "DECAWM off overwrites the last column" b"\x1b[?7l0123456789ab" => screen, "012345678b|||", "xterm DECAWM";
    "Wrap" "a wrapped line reflows when widened" b"0123456789ab" => widen, "0123456789ab|||", "pinfold 0029-history-reflows-at-read-time.md";
    "Wrap" "a line reflows when narrowed, keeping its text" b"abcdefgh" => narrow, "abcde|fgh||", "pinfold 0029-history-reflows-at-read-time.md";
    "Alternate screen" "1049 shows a clear alternate screen, keeping the cursor" b"main\x1b[?1049halt" => screen, "    alt|||", "xterm Alternate Screen Buffer";
    "Alternate screen" "1049 reset restores the screen and cursor" b"main\x1b[?1049halt\x1b[?1049l" => screen_cursor, "main||| @0,4", "xterm Alternate Screen Buffer";
    "OSC" "OSC 7 reports the working directory" b"\x1b]7;file://host/tmp/a%20b\x07" => cwd, "host:/tmp/a b", "ghostty osc/7";
    "OSC" "OSC 7 refuses a URL that is not file:" b"\x1b]7;http://host/tmp\x07" => cwd, "none", "ghostty osc/7";
    "OSC" "OSC 8 links the text between its marks" b"\x1b]8;;https://e.x\x1b\\L\x1b]8;;\x1b\\M" => link, "Some(\"https://e.x\") None", "osc8";
    "OSC" "OSC 133 A B C D;2 marks a finished command" b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;2\x07" => marks, "Finished exit Some(2), 1 finished, 1 prompts", "osc133";
    "OSC" "OSC 0 sets the title, not the screen" b"\x1b]0;title\x07x" => screen, "x|||", "ghostty osc/0";
    "OSC" "OSC 11 and 12 set the background and cursor colours" b"\x1b]11;rgb:12/34/56\x07\x1b]12;#abcdef\x07" => osc_colours, "cursor abcdef, background 123456", "xterm OSC";
    "Modes" "DECTCEM hides the cursor" b"\x1b[?25l" => cursor, "hidden", "xterm DECTCEM";
    "Modes" "DECOM counts rows from the region" b"\x1b[2;3r\x1b[?6h\x1b[1;1H" => cursor, "1,0", "vttest 1";
    "Modes" "DECCKM and 2004" b"\x1b[?1h\x1b[?2004h" => modes, "mouse Off Legacy, paste true, app-cursor true, alt-scroll false", "xterm DECSET";
    "Modes" "1000 reports clicks" b"\x1b[?1000h" => modes, "mouse Click Legacy, paste false, app-cursor false, alt-scroll false", "xterm Mouse Tracking";
    "Modes" "1002 with 1006 reports drags as SGR" b"\x1b[?1002h\x1b[?1006h" => modes, "mouse Drag Sgr, paste false, app-cursor false, alt-scroll false", "xterm Mouse Tracking";
    "Modes" "1003 with 1005 reports motion as UTF-8" b"\x1b[?1003h\x1b[?1005h" => modes, "mouse Motion Utf8, paste false, app-cursor false, alt-scroll false", "xterm Mouse Tracking";
    "Modes" "1007 reset stops the wheel sending arrows" b"\x1b[?1049h\x1b[?1007l" => modes, "mouse Off Legacy, paste false, app-cursor false, alt-scroll false", "xterm Alt and Meta Keys";
    "Modes" "2026 holds output until the update ends" b"\x1b[?2026hX" => ended_sync, "held \"|||\", then \"X|||\"", "sync";
    "Hostile" "a parameter past u16 clamps" b"\x1b[3;3H\x1b[99999999999999999999A" => cursor, "0,2", "vt100";
    "Hostile" "REP of 65535 writes 65535 more" b"a\x1b[65535b" => screen, "aaaaaaaaaa|aaaaaaaaaa|aaaaaaaaaa|aaaaaa", "ghostty csi/rep";
    "Hostile" "invalid UTF-8 shows U+FFFD" b"\xffa" => screen, "\u{fffd}a|||", "unicode 3.9 U+FFFD Substitution";
    "Hostile" "CAN aborts a sequence" b"\x1b[2\x18J" => screen, "J|||", "vt100";
    "Hostile" "an inverted scroll region is ignored" b"a\r\nb\r\nc\r\nd\x1b[3;2r\r\ne" => screen, "b|c|d|e", "ghostty csi/decstbm";
    "Hostile" "a DCS string shows nothing" b"\x1bPq#0;2;0;0;0~-\x1b\\ok" => screen, "ok|||", "vt100";
    "Hostile" "a resize to 0 x 0 clamps to 1 x 2" b"abc" => shrink_to_nothing, "(1, 2) c", "pinfold 0006-vt-engine.md";
};

/// Text as itself, ESC by name, and other controls and invalid UTF-8 as escapes.
fn bytes_shown(bytes: &[u8]) -> String {
    let mut shown = String::new();
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\x1b' => shown += "ESC",
                c if c.is_control() => shown.extend(c.escape_default()),
                c => shown.push(c),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(shown, "\\x{byte:02x}");
        }
    }
    shown
}

fn reference(key: &str) -> Option<String> {
    let (source, page) = key.split_once(' ').unwrap_or((key, ""));
    let (_, base) = REFERENCES.iter().find(|(name, _)| *name == source)?;
    Some(match source {
        "ghostty" => format!("[Ghostty {page}]({base}{page})"),
        "pinfold" => format!("[{}]({base}{page})", &page[..8]),
        _ if page.is_empty() => format!("[{source}]({base})"),
        _ => format!("[{source}]({base}): {page}"),
    })
}

fn cell_text(text: &str) -> String {
    format!(
        "`{}`",
        text.replace('|', "\\|")
            .replace('`', "'")
            .replace('\t', "\\t")
    )
}

#[test]
fn matrix() {
    let (mut passed, mut gaps, mut failures) = (0, 0, Vec::new());
    let mut rows = String::new();
    for case in &CASES {
        let mut engine = AlacrittyEngine::new(4, 10);
        engine.feed(case.bytes, &mut Vec::new());
        let shown = (case.observe)(&mut engine);
        let status = match (shown == case.expected, case.gap) {
            (true, None) => {
                passed += 1;
                "pass".to_string()
            }
            (false, Some(issue)) => {
                gaps += 1;
                format!(
                    "gap [#{issue}](https://github.com/0xPiranhaCodes/pinfold.sh/issues/{issue})"
                )
            }
            (true, Some(issue)) => {
                failures.push(format!("{}: passes now; drop gap #{issue}", case.name));
                continue;
            }
            (false, None) => {
                failures.push(format!(
                    "{}: expected {:?}, got {shown:?}",
                    case.name, case.expected
                ));
                continue;
            }
        };
        let _ = writeln!(
            rows,
            "| {} | {} | {} | {} | {} | {status} | {} |",
            case.area,
            case.name,
            cell_text(&bytes_shown(case.bytes)),
            cell_text(case.expected),
            cell_text(&shown),
            reference(case.reference).unwrap_or_else(|| panic!("no {:?}", case.reference))
        );
    }
    assert!(failures.is_empty(), "{failures:#?}");

    let matrix = format!(
        "# VT conformance matrix\n\n\
         Generated by `crates/pinfold-core/tests/vt_conformance.rs`; do not edit by hand. Each case\n\
         feeds its bytes to a fresh 4 × 10 engine and compares what PinFold shows with the result\n\
         the reference documents. `cargo test` fails when this file is stale, a passing case\n\
         regresses, or a case fails without a known-gap issue. To rewrite it, run\n\
         `PINFOLD_UPDATE_CONFORMANCE=1 cargo test -p pinfold-core --test vt_conformance`.\n\n\
         Screens are the rows the window draws, without trailing spaces, between bars; a wide\n\
         character's second cell is a space. Cursors are `row,column` from 0.\n\
         esctest2 runs as well, through `tests/conformance/esctest.sh`.\n\n\
         **{passed} pass, {gaps} known gaps, {} cases.**\n\n\
         | Area | Case | Bytes | Expected | PinFold | Status | Reference |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n{rows}",
        CASES.len()
    );
    if std::env::var_os("PINFOLD_UPDATE_CONFORMANCE").is_some() {
        std::fs::write(MATRIX, &matrix).unwrap();
    }
    let committed = std::fs::read_to_string(MATRIX).unwrap_or_default();
    assert!(
        committed == matrix,
        "docs/design/vt-conformance.md is stale; rerun with PINFOLD_UPDATE_CONFORMANCE=1"
    );
}
