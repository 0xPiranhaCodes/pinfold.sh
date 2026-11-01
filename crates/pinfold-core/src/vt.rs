//! The terminal engine behind a trait, with `alacritty_terminal` as its implementation.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell as GridCell, Flags};
use alacritty_terminal::term::color;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};
use pinfold_proto::{Cell, Frame, MouseEncoding, MouseMode};

use crate::marks::{Marks, ShellMarks, WorkingDirectory};
use crate::scrollback::Scrollback;

/// The slot Alacritty's colour table gives the default background.
const BACKGROUND_SLOT: usize = 257;

/// Larger sizes are clamped, so a bad resize cannot allocate gigabytes of cells.
pub const MAX_DIMENSION: u16 = 1024;
/// Longest OSC string that reaches the parser, whose own buffer has no limit.
pub const MAX_SEQUENCE: usize = 4096;
/// Most output a synchronized update holds back. Past this it is shown early, which can
/// tear a frame. The parser's own limit is 2 MiB per pane, outside the scrollback cap.
pub const MAX_HELD: usize = 256 * 1024;
/// A wide character cannot fit in fewer columns: reflowing one wraps forever.
pub const MIN_COLUMNS: u16 = 2;

/// The modes that show the program's screen whatever the view, and keep the wheel from it.
const NO_HISTORY: TermMode = TermMode::ALT_SCREEN.union(TermMode::MOUSE_MODE);

/// A terminal state machine that turns program output into a grid.
///
/// The trait grows with its callers. The design also lists scrolling, search and damage,
/// and ADR-0006 expects the scrollback work to shape them.
pub trait VtEngine {
    /// Parses program output. Bytes the program asked the terminal to answer with are
    /// appended to `replies`, and the caller writes them back to the PTY.
    fn feed(&mut self, bytes: &[u8], replies: &mut Vec<u8>);

    /// Changes the grid size, clamped to at least one row and `MIN_COLUMNS` columns and at
    /// most `MAX_DIMENSION` of each.
    fn resize(&mut self, rows: u16, columns: u16);

    /// Returns the size as `(rows, columns)`.
    fn size(&self) -> (u16, u16);

    /// Returns a visible line without trailing spaces, or nothing if `line` is off screen.
    fn line_text(&self, line: u16) -> String;

    /// Copies the view into `frame`, reusing its allocation. That is the visible screen, or
    /// history above it while scrolled back, when no cursor is shown.
    fn frame(&self, frame: &mut Frame);

    /// Moves the view `lines` rows into history, or back toward the screen if negative, and
    /// returns whether it moved. It stops at both ends. The alternate screen has no history
    /// and a program with mouse reporting owns the wheel, so neither scrolls.
    fn scroll(&mut self, lines: i32) -> bool;

    /// Returns the view to the live screen, and returns whether it was scrolled back.
    fn snap_to_live(&mut self) -> bool;

    /// Returns whether the program has asked for pastes wrapped in brackets.
    fn bracketed_paste(&self) -> bool;

    /// Returns when output held back by a synchronized update must be shown, if any is.
    fn sync_deadline(&self) -> Option<Instant>;

    /// Shows what a synchronized update held back, as if the program had ended it.
    fn end_sync(&mut self, replies: &mut Vec<u8>);

    /// Returns what the shell last reported through OSC 133. Output can fake all of it.
    fn shell_marks(&self) -> ShellMarks;

    /// Returns the last valid OSC 7 directory, which is a hint that output can fake.
    fn cwd(&self) -> Option<&WorkingDirectory>;
}

/// The "Graphite Grid" ground and text colours (`docs/design/ui/`).
const BACKGROUND: u32 = 0x000e_1014;
const FOREGROUND: u32 = 0x00e2_e5ea;

// shortcut: the design has no ANSI palette, so this reuses its tokens, and magenta is a stand-in.
const ANSI: [u32; 16] = [
    0x0012_151a,
    0x00ff_8a8a,
    0x0063_d29a,
    0x00f5_b544,
    0x007a_a7ff,
    0x00d9_a7e0,
    0x009c_c0ff,
    0x00c9_cdd4,
    0x0034_3a44,
    0x00ff_8a8a,
    0x0063_d29a,
    0x00f5_b544,
    0x009c_c0ff,
    0x00d9_a7e0,
    0x00c4_d9ff,
    0x00e2_e5ea,
];

/// The standard 256-colour table: 16 ANSI colours, a 6×6×6 cube and a grey ramp.
fn indexed(index: usize) -> u32 {
    let level = |step: usize| if step == 0 { 0 } else { 55 + 40 * step as u32 };
    match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let offset = index - 16;
            level(offset / 36) << 16 | level(offset / 6 % 6) << 8 | level(offset % 6)
        }
        232..=255 => (8 + 10 * (index as u32 - 232)) * 0x0001_0101,
        _ => FOREGROUND,
    }
}

#[derive(Clone, Copy)]
struct Size {
    rows: u16,
    columns: u16,
}

impl Size {
    fn clamped(rows: u16, columns: u16) -> Self {
        Self {
            rows: rows.clamp(1, MAX_DIMENSION),
            columns: columns.clamp(MIN_COLUMNS, MAX_DIMENSION),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.columns)
    }
}

/// Whether `reply` is a CSI sequence of numbers, with nothing a program could have chosen.
///
/// Replies are written into the shell's input, so one that echoed program text would run
/// it (T3 in the threat model).
fn is_fixed_format(reply: &str) -> bool {
    let Some(body) = reply.strip_prefix("\x1b[") else {
        return false;
    };
    let Some(end) = body.chars().last().filter(char::is_ascii_alphabetic) else {
        return false;
    };
    let parameters = &body[..body.len() - end.len_utf8()];
    let parameters = parameters.strip_suffix('$').unwrap_or(parameters);
    reply.len() <= 32
        && parameters
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b';' | b'?' | b'>'))
}

/// Collects what the terminal wants to send back to the program.
///
/// Every other event is dropped, which keeps OSC 52 clipboard reads and writes and colour
/// queries unanswered. Clipboard writes need the status-bar flash from the threat model.
#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && is_fixed_format(&text)
            && let Ok(mut pending) = self.0.lock()
        {
            pending.extend_from_slice(text.as_bytes());
        }
    }
}

/// Where a read left the scan for OSC strings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Scan {
    #[default]
    Ground,
    Escape,
    /// Inside an OSC string, this many bytes in.
    Osc(usize),
}

impl Scan {
    /// Calls `pass` with `bytes`, minus OSC string bytes past `MAX_SEQUENCE`, and gives
    /// `marks` every OSC string that fits. The parser drops the ones `marks` wants.
    ///
    /// This follows the parser exactly: only `ESC ]` opens a string, with controls allowed
    /// between the two, and BEL, CAN, SUB or ESC ends one. A string cut short is still
    /// dispatched when it ends.
    // shortcut: marks apply on read, ahead of output a synchronized update holds back, fix
    // this if marks ever need a position in the grid.
    fn filter(&mut self, bytes: &[u8], marks: &mut Marks, mut pass: impl FnMut(&[u8])) {
        let ends = |byte: &u8| matches!(byte, 0x07 | 0x18 | 0x1a | 0x1b);
        let (mut start, mut index) = (0, 0);
        while index < bytes.len() {
            match *self {
                Self::Ground => match bytes[index..].iter().position(|&byte| byte == 0x1b) {
                    Some(offset) => {
                        index += offset + 1;
                        *self = Self::Escape;
                    }
                    None => index = bytes.len(),
                },
                Self::Escape => {
                    *self = match bytes[index] {
                        b']' => {
                            marks.start();
                            Self::Osc(0)
                        }
                        0x18 | 0x1a => Self::Ground,
                        0x00..=0x1f | 0x7f..=0xff => Self::Escape,
                        _ => Self::Ground,
                    };
                    index += 1;
                }
                Self::Osc(length) if length >= MAX_SEQUENCE => {
                    pass(&bytes[start..index]);
                    index += bytes[index..]
                        .iter()
                        .position(ends)
                        .unwrap_or(bytes.len() - index);
                    start = index;
                    if let Some(byte) = bytes.get(index) {
                        *self = if *byte == 0x1b {
                            Self::Escape
                        } else {
                            Self::Ground
                        };
                        index += 1;
                    }
                }
                Self::Osc(length) => {
                    let byte = bytes[index];
                    marks.byte(byte);
                    *self = match byte {
                        0x1b => Self::Escape,
                        _ if ends(&byte) => Self::Ground,
                        _ => Self::Osc(length + 1),
                    };
                    index += 1;
                }
            }
        }
        pass(&bytes[start..]);
    }
}

/// [`VtEngine`] over `alacritty_terminal`.
pub struct AlacrittyEngine {
    term: Term<Replies>,
    parser: Processor,
    scan: Scan,
    marks: Marks,
    replies: Replies,
    scrollback: Arc<Mutex<Scrollback>>,
    /// The store's number for the view's top row, or nothing at the live screen. A number
    /// stays on its row as output arrives, so the view holds still without being told.
    view: Option<usize>,
}

impl AlacrittyEngine {
    /// Creates a blank terminal. Rows leaving the primary screen go to its scrollback store.
    #[must_use]
    pub fn new(rows: u16, columns: u16) -> Self {
        let replies = Replies::default();
        let config = Config {
            scrolling_history: 0,
            ..Config::default()
        };
        let mut term = Term::new(config, &Size::clamped(rows, columns), replies.clone());
        let scrollback = Arc::new(Mutex::new(Scrollback::new()));
        term.grid_mut().set_history_sink(scrollback.clone());
        Self {
            term,
            parser: Processor::new(),
            scan: Scan::default(),
            marks: Marks::default(),
            replies,
            scrollback,
            view: None,
        }
    }

    /// Returns the store of rows that have left the screen.
    #[must_use]
    pub fn scrollback(&self) -> &Mutex<Scrollback> {
        &self.scrollback
    }

    /// Returns how many rows above the live screen the view starts.
    fn rows_back(&self, store: &Scrollback) -> usize {
        self.view
            .filter(|_| !self.term.mode().intersects(NO_HISTORY))
            .and_then(|number| store.index(number))
            .map_or(0, |index| store.len() - index)
    }

    /// Copies the view's rows of history into `frame`, and returns how many there are.
    fn frame_history(&self, frame: &mut Frame) -> usize {
        let Ok(mut store) = self.scrollback.lock() else {
            return 0;
        };
        let back = self.rows_back(&store);
        let first = store.len() - back;
        let blank = self.cell(&GridCell::default());
        let columns = usize::from(frame.columns);
        // shortcut: history keeps the width it was written at, until cold reflow in M2.
        store.read_rows(first, back.min(usize::from(frame.rows)), |row| {
            let end = frame.cells.len() + columns;
            let cells = row.iter().take(columns);
            frame.cells.extend(cells.map(|cell| self.cell(cell)));
            frame.cells.resize(end, blank);
        });
        frame.cells.len() / columns.max(1)
    }

    fn take_replies(&self, replies: &mut Vec<u8>) {
        if let Ok(mut pending) = self.replies.0.lock() {
            replies.append(&mut pending);
        }
    }
}

impl AlacrittyEngine {
    fn colour(&self, colour: Color) -> u32 {
        let index = match colour {
            Color::Spec(rgb) => {
                return u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b);
            }
            Color::Indexed(index) => usize::from(index),
            Color::Named(name) => name as usize,
        };
        match self.term.colors()[index.min(color::COUNT - 1)] {
            Some(rgb) => u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b),
            None if index == BACKGROUND_SLOT => BACKGROUND,
            None => indexed(index),
        }
    }

    fn cell(&self, cell: &GridCell) -> Cell {
        let (mut foreground, mut background) = (self.colour(cell.fg), self.colour(cell.bg));
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            foreground = background;
        }
        let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
        Cell {
            character: if spacer { ' ' } else { cell.c },
            foreground,
            background,
        }
    }
}

impl VtEngine for AlacrittyEngine {
    fn feed(&mut self, bytes: &[u8], replies: &mut Vec<u8>) {
        let (parser, term) = (&mut self.parser, &mut self.term);
        self.scan
            .filter(bytes, &mut self.marks, |bytes| parser.advance(term, bytes));
        if parser.sync_bytes_count() > MAX_HELD {
            parser.stop_sync(term);
        }
        self.take_replies(replies);
    }

    fn resize(&mut self, rows: u16, columns: u16) {
        self.term.resize(Size::clamped(rows, columns));
    }

    fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        let rows = u16::try_from(grid.screen_lines()).unwrap_or(MAX_DIMENSION);
        let columns = u16::try_from(grid.columns()).unwrap_or(MAX_DIMENSION);
        (rows, columns)
    }

    fn line_text(&self, line: u16) -> String {
        let grid = self.term.grid();
        if usize::from(line) >= grid.screen_lines() {
            return String::new();
        }
        let row = &grid[Line(i32::from(line))];
        let text: String = (0..grid.columns())
            .map(|column| &row[Column(column)])
            .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect();
        text.trim_end().to_owned()
    }

    fn frame(&self, frame: &mut Frame) {
        let grid = self.term.grid();
        (frame.rows, frame.columns) = self.size();
        frame.cells.clear();
        let history = self.frame_history(frame);
        for line in 0..frame.rows.saturating_sub(history as u16) {
            let row = &grid[Line(i32::from(line))];
            frame
                .cells
                .extend((0..grid.columns()).map(|column| self.cell(&row[Column(column)])));
        }
        let point = grid.cursor.point;
        // Input lands at the live cursor, so none is shown over history.
        let visible = self.term.mode().contains(TermMode::SHOW_CURSOR) && history == 0;
        let position = u16::try_from(point.line.0)
            .ok()
            .zip(u16::try_from(point.column.0).ok());
        frame.cursor = position.filter(|_| visible);
        let mode = self.term.mode();
        frame.app_cursor = mode.contains(TermMode::APP_CURSOR);
        frame.mouse = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseMode::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseMode::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseMode::Click
        } else {
            MouseMode::Off
        };
        frame.mouse_encoding = if mode.contains(TermMode::SGR_MOUSE) {
            MouseEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseEncoding::Utf8
        } else {
            MouseEncoding::Legacy
        };
        frame.alternate_scroll = mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL);
    }

    fn scroll(&mut self, lines: i32) -> bool {
        let Ok(store) = self.scrollback.lock() else {
            return false;
        };
        if self.term.mode().intersects(NO_HISTORY) {
            return false;
        }
        let back = self.rows_back(&store);
        let target = back.saturating_add_signed(lines as isize).min(store.len());
        self.view = (target > 0).then(|| store.end() - target);
        target != back
    }

    fn snap_to_live(&mut self) -> bool {
        self.view.take().is_some()
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    fn end_sync(&mut self, replies: &mut Vec<u8>) {
        self.parser.stop_sync(&mut self.term);
        self.take_replies(replies);
    }

    fn shell_marks(&self) -> ShellMarks {
        self.marks.shell
    }

    fn cwd(&self) -> Option<&WorkingDirectory> {
        self.marks.cwd.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(engine: &mut AlacrittyEngine, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();
        engine.feed(bytes, &mut replies);
        replies
    }

    #[test]
    fn the_engine_can_move_to_the_io_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<AlacrittyEngine>();
    }

    #[test]
    fn text_reaches_the_grid() {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, b"hello\r\nworld");
        assert_eq!(engine.line_text(0), "hello");
        assert_eq!(engine.line_text(1), "world");
        assert_eq!(engine.line_text(2), "");
    }

    #[test]
    fn wide_characters_appear_once() {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, "漢字!".as_bytes());
        assert_eq!(engine.line_text(0), "漢字!");
    }

    #[test]
    fn utf8_split_across_reads_survives() {
        let mut engine = AlacrittyEngine::new(4, 20);
        let bytes = "é".as_bytes();
        feed(&mut engine, &bytes[..1]);
        feed(&mut engine, &bytes[1..]);
        assert_eq!(engine.line_text(0), "é");
    }

    #[test]
    fn cursor_position_requests_get_a_reply() {
        let mut engine = AlacrittyEngine::new(4, 20);
        assert_eq!(feed(&mut engine, b"ab\x1b[6n"), b"\x1b[1;3R");
        assert!(feed(&mut engine, b"more").is_empty());
    }

    #[test]
    fn sequences_that_echo_strings_get_no_reply() {
        let hostile: [&[u8]; 9] = [
            b"\x1b]2;rm -rf\x07\x1b[21t",
            b"\x1b]1;rm -rf\x07\x1b[20t",
            b"\x1bP$qm\x1b\\",
            b"\x1bP+q544e\x1b\\",
            b"\x1b]52;c;?\x07",
            b"\x1b]52;c;aGVsbG8=\x07",
            b"\x1b]4;1;?\x07",
            b"\x1b]10;?\x07",
            b"\x1b[14t",
        ];
        for bytes in hostile {
            let mut engine = AlacrittyEngine::new(4, 20);
            assert!(feed(&mut engine, bytes).is_empty(), "{bytes:?}");
        }
    }

    #[test]
    fn replies_are_only_fixed_format_csi() {
        for reply in [
            "\x1b[1;3R",
            "\x1b[0n",
            "\x1b[?6c",
            "\x1b[>0;3400;1c",
            "\x1b[?1;2$y",
            "\x1b[?0u",
            "\x1b[8;24;80t",
        ] {
            assert!(is_fixed_format(reply), "{reply:?}");
        }
        for reply in [
            "",
            "\x1b[",
            "\x1b]l$(reboot)\x1b\\",
            "\x1bPtext",
            "\x1b[rm -rf /\rt",
            "\x1b[1;\r1R",
            "\x1b[1é",
            "\x1b[$$y",
            "text",
        ] {
            assert!(!is_fixed_format(reply), "{reply:?}");
        }
        assert!(!is_fixed_format(&format!("\x1b[{}R", "1".repeat(64))));
    }

    fn filtered(scan: &mut Scan, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        scan.filter(bytes, &mut Marks::default(), |bytes| {
            out.extend_from_slice(bytes);
        });
        out
    }

    #[test]
    fn an_unending_osc_string_stops_at_the_limit() {
        let mut scan = Scan::default();
        let mut passed = filtered(&mut scan, b"\x1b]2;").len();
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..1024 {
            passed += filtered(&mut scan, &chunk).len();
        }
        assert_eq!(passed, 2 + MAX_SEQUENCE);
        assert_eq!(filtered(&mut scan, b"\x07after"), b"\x07after");
        assert_eq!(scan, Scan::Ground);
    }

    #[test]
    fn output_outside_osc_strings_is_untouched() {
        let bytes = [b"\x1b[31mred\x1b(B".as_slice(), &[b'x'; 10_000]].concat();
        assert_eq!(filtered(&mut Scan::default(), &bytes), bytes);
    }

    #[test]
    fn every_terminator_ends_a_string() {
        for end in [0x07, 0x18, 0x1a, 0x1b] {
            let mut scan = Scan::Osc(MAX_SEQUENCE);
            let bytes = [b"cut".as_slice(), &[end, b'x']].concat();
            assert_eq!(filtered(&mut scan, &bytes), [end, b'x']);
            assert_eq!(scan, Scan::Ground);
        }
    }

    #[test]
    fn an_escape_split_across_reads_still_counts() {
        let mut scan = Scan::default();
        filtered(&mut scan, b"\x1b");
        filtered(&mut scan, b"]");
        assert_eq!(scan, Scan::Osc(0));
    }

    #[test]
    fn controls_after_an_escape_do_not_hide_a_string_from_the_limit() {
        for byte in [0x00, 0x07, 0x1c, 0x7f, 0x80, 0xff] {
            let mut scan = Scan::default();
            filtered(&mut scan, &[0x1b, byte, b']']);
            assert_eq!(scan, Scan::Osc(0), "{byte:#x}");
        }
        for byte in [0x18, 0x1a, b'c'] {
            let mut scan = Scan::default();
            filtered(&mut scan, &[0x1b, byte, b']']);
            assert_eq!(scan, Scan::Ground, "{byte:#x}");
        }
    }

    #[test]
    fn marks_hold_no_more_than_one_string() {
        let mut engine = AlacrittyEngine::new(4, 20);
        let title = [b"\x1b]2;".as_slice(), &[b'a'; 10_000], b"\x07"].concat();
        feed(&mut engine, &title);
        assert!(engine.marks.text.capacity() <= 8);
        let url = [b"\x1b]7;file:///".as_slice(), &[b'a'; 10_000], b"\x07"].concat();
        for _ in 0..1000 {
            feed(&mut engine, &url);
            feed(&mut engine, b"\x1b]133;D;1\x07");
        }
        assert!(engine.marks.text.capacity() <= MAX_SEQUENCE);
        assert_eq!(engine.cwd(), None);
    }

    #[test]
    fn text_after_a_long_string_still_reaches_the_screen() {
        let mut engine = AlacrittyEngine::new(4, 20);
        let mut replies = Vec::new();
        engine.feed(b"\x1b]2;", &mut replies);
        engine.feed(&vec![b'a'; 1_000_000], &mut replies);
        engine.feed(b"\x07ok", &mut replies);
        assert_eq!(engine.line_text(0), "ok");
    }

    #[test]
    fn a_finished_update_appears_at_once() {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, b"\x1b[?2026hheld");
        assert_eq!(engine.line_text(0), "");
        assert!(engine.sync_deadline().is_some());
        feed(&mut engine, b"\x1b[?2026l");
        assert_eq!(engine.line_text(0), "held");
        assert!(engine.sync_deadline().is_none());
    }

    #[test]
    fn ending_an_update_shows_and_answers_what_it_held() {
        let mut engine = AlacrittyEngine::new(4, 20);
        assert!(feed(&mut engine, b"\x1b[?2026hheld\x1b[6n").is_empty());
        let mut replies = Vec::new();
        engine.end_sync(&mut replies);
        assert_eq!(engine.line_text(0), "held");
        assert_eq!(replies, b"\x1b[1;5R");
        assert!(engine.sync_deadline().is_none());
    }

    #[test]
    fn an_update_stops_holding_output_at_the_limit() {
        let mut engine = AlacrittyEngine::new(4, 20);
        feed(&mut engine, b"\x1b[?2026h");
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..16 {
            feed(&mut engine, &chunk);
            assert!(engine.parser.sync_bytes_count() <= MAX_HELD);
        }
        assert_eq!(engine.line_text(0).len(), 20);
    }

    #[test]
    fn resize_changes_the_size() {
        let mut engine = AlacrittyEngine::new(4, 20);
        engine.resize(10, 40);
        assert_eq!(engine.size(), (10, 40));
    }

    #[test]
    fn sizes_are_clamped() {
        let mut engine = AlacrittyEngine::new(0, 0);
        assert_eq!(engine.size(), (1, MIN_COLUMNS));
        engine.resize(u16::MAX, u16::MAX);
        assert_eq!(engine.size(), (MAX_DIMENSION, MAX_DIMENSION));
    }

    #[test]
    fn a_wide_character_survives_the_narrowest_window() {
        let mut engine = AlacrittyEngine::new(8, 249);
        feed(&mut engine, "䤬".as_bytes());
        engine.resize(4, 1);
        assert_eq!(engine.size(), (4, MIN_COLUMNS));
        assert_eq!(engine.line_text(0), "䤬");
    }

    fn screen(bytes: &[u8]) -> Frame {
        let (mut engine, mut frame) = (AlacrittyEngine::new(4, 20), Frame::default());
        feed(&mut engine, bytes);
        engine.frame(&mut frame);
        frame
    }

    #[test]
    fn a_frame_holds_every_cell() {
        let frame = screen(b"hi");
        assert_eq!((frame.rows, frame.columns), (4, 20));
        assert_eq!(frame.cells.len(), 80);
        assert_eq!(frame.cells[1].character, 'i');
        assert_eq!(frame.cells[0].foreground, FOREGROUND);
        assert_eq!(frame.cells[0].background, BACKGROUND);
    }

    #[test]
    fn colours_resolve() {
        assert_eq!(screen(b"\x1b[31mx").cells[0].foreground, ANSI[1]);
        assert_eq!(screen(b"\x1b[38;5;196mx").cells[0].foreground, 0x00ff_0000);
        assert_eq!(screen(b"\x1b[38;5;232mx").cells[0].foreground, 0x0008_0808);
        assert_eq!(
            screen(b"\x1b[48;2;1;2;3mx").cells[0].background,
            0x0001_0203
        );
    }

    #[test]
    fn programs_can_change_the_palette() {
        assert_eq!(
            screen(b"\x1b]4;1;rgb:01/02/03\x07\x1b[31mx").cells[0].foreground,
            0x0001_0203
        );
    }

    #[test]
    fn inverse_and_hidden_text_apply() {
        let inverse = screen(b"\x1b[7mx").cells[0];
        assert_eq!(
            (inverse.foreground, inverse.background),
            (BACKGROUND, FOREGROUND)
        );
        let hidden = screen(b"\x1b[8mx").cells[0];
        assert_eq!(hidden.foreground, hidden.background);
    }

    #[test]
    fn wide_spacers_are_blank() {
        let frame = screen("漢".as_bytes());
        assert_eq!(frame.cells[0].character, '漢');
        assert_eq!(frame.cells[1].character, ' ');
    }

    #[test]
    fn the_cursor_follows_the_program() {
        assert_eq!(screen(b"ab\r\nc").cursor, Some((1, 1)));
        assert_eq!(screen(b"\x1b[?25l").cursor, None);
    }

    #[test]
    fn frames_carry_application_cursor_mode() {
        assert!(!screen(b"").app_cursor);
        assert!(screen(b"\x1b[?1h").app_cursor);
        assert!(!screen(b"\x1b[?1h\x1b[?1l").app_cursor);
    }

    #[test]
    fn the_program_chooses_bracketed_paste() {
        let mut engine = AlacrittyEngine::new(4, 20);
        assert!(!engine.bracketed_paste());
        feed(&mut engine, b"\x1b[?2004h");
        assert!(engine.bracketed_paste());
    }

    #[test]
    fn frames_carry_the_mouse_modes() {
        assert_eq!(screen(b"").mouse, MouseMode::Off);
        assert_eq!(screen(b"\x1b[?1000h").mouse, MouseMode::Click);
        assert_eq!(screen(b"\x1b[?1002h").mouse, MouseMode::Drag);
        assert_eq!(screen(b"\x1b[?1003h").mouse, MouseMode::Motion);
        assert_eq!(screen(b"\x1b[?1003h\x1b[?1003l").mouse, MouseMode::Off);
        assert_eq!(screen(b"").mouse_encoding, MouseEncoding::Legacy);
        assert_eq!(screen(b"\x1b[?1005h").mouse_encoding, MouseEncoding::Utf8);
        assert_eq!(screen(b"\x1b[?1006h").mouse_encoding, MouseEncoding::Sgr);
    }

    #[test]
    fn the_wheel_sends_arrows_only_on_the_alternate_screen() {
        assert!(!screen(b"").alternate_scroll);
        assert!(screen(b"\x1b[?1049h").alternate_scroll);
        assert!(!screen(b"\x1b[?1049h\x1b[?1007l").alternate_scroll);
    }

    #[test]
    fn every_colour_index_is_safe() {
        for index in 0..=300 {
            let _ = indexed(index);
        }
        for byte in 0..=255 {
            screen(format!("\x1b[38;5;{byte}m\x1b[48;5;{byte}mx").as_bytes());
        }
    }

    fn history(engine: &AlacrittyEngine) -> Vec<String> {
        let store = engine.scrollback().lock().unwrap();
        store
            .rows(0, store.len())
            .iter()
            .map(|row| row.iter().map(|cell| cell.c).collect::<String>())
            .map(|text| text.trim_end().to_owned())
            .collect()
    }

    #[test]
    fn rows_leaving_the_screen_reach_the_store() {
        let mut engine = AlacrittyEngine::new(2, 20);
        feed(&mut engine, b"one\r\ntwo\r\nthree\r\nfour");
        assert_eq!(history(&engine), ["one", "two"]);
        assert_eq!(engine.line_text(0), "three");
    }

    #[test]
    fn the_alternate_screen_stays_out_of_the_store() {
        let mut engine = AlacrittyEngine::new(2, 20);
        feed(&mut engine, b"\x1b[?1049hone\r\ntwo\r\nthree");
        assert!(history(&engine).is_empty());
    }

    #[test]
    fn clearing_scrollback_empties_the_store() {
        let mut engine = AlacrittyEngine::new(2, 20);
        feed(&mut engine, b"one\r\ntwo\r\nthree\x1b[3J");
        assert!(history(&engine).is_empty());
    }

    #[test]
    fn hostile_output_through_a_pane_stays_under_the_cap() {
        let mut engine = AlacrittyEngine::new(2, 120);
        for line in 0..20_000_u32 {
            let colour = line.wrapping_mul(2_654_435_761);
            let [r, g, b, _] = colour.to_le_bytes();
            let text = format!("\x1b[38;2;{r};{g};{b}m{colour:0>119}\r\n");
            feed(&mut engine, text.as_bytes());
            let bytes = engine.scrollback().lock().unwrap().bytes();
            assert!(bytes <= crate::scrollback::BYTE_CAP);
        }
    }

    #[test]
    fn off_screen_lines_are_empty() {
        assert_eq!(AlacrittyEngine::new(4, 20).line_text(u16::MAX), "");
    }

    #[test]
    fn every_byte_value_is_survivable() {
        let mut engine = AlacrittyEngine::new(4, 20);
        for _ in 0..3 {
            for byte in 0..=u8::MAX {
                feed(&mut engine, &[byte, b'[', byte, b'm']);
            }
        }
        feed(&mut engine, b"\x1bc");
        feed(&mut engine, b"ok");
        assert_eq!(engine.line_text(0), "ok");
    }
}
