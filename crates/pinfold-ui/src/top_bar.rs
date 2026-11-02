//! The row above the pane: a tab on the left, the date and time on the right.
//!
//! The tab's label is the last name in a path that program output wrote (OSC 7), so a
//! program chooses it. It keeps only letters, digits, ASCII punctuation and spaces, which
//! rules out control and bidi characters and every state glyph, and it is always drawn in
//! one neutral colour. A state is glyph, colour and word, so the most a label can show is
//! the word (T4 and T6 in the threat model). Only the row under the pane asks anything.

use std::ffi::OsStr;
use std::ops::Range;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pinfold_proto::{BOLD, Cell, DEFAULT_BACKGROUND, Frame};
use pinfold_render::padding::Padding;
use pinfold_sys::LocalTime;
use unicode_width::UnicodeWidthChar;

use crate::mouse;
use crate::paste::TEXT;
use crate::search_bar::write;

/// Rows above the pane that belong to Pinfold, so program output cannot reach them.
pub const ROWS: u16 = 1;
/// Points to leave on the left. On macOS the real traffic lights sit there, and elsewhere
/// the system draws its buttons outside the window.
// shortcut: stays in fullscreen, where macOS hides the lights, until the window tells us.
pub const LEFT_POINTS: f64 = if cfg!(target_os = "macos") { 78.0 } else { 0.0 };

/// The padding's colour, so the bar has no edge and a translucent window shows through it.
const CHROME: u32 = DEFAULT_BACKGROUND;
/// `Rule, quiet fill` in Foundations. The Main board's darker tab colour needs a border to
/// show, and cells have none. The accent is the blue of a running agent, so it is not used.
const ACTIVE: u32 = 0x0026_2b33;
/// The Main board's quiet label text. Foundations' secondary text is the idle state's grey.
const MUTED: u32 = 0x0085_8d98;

const FALLBACK: &str = "shell";
/// Widest label in cells, so it stays a tab and cannot crowd out the clock.
const MAX_LABEL: usize = 24;
/// Empty cells kept between the tab and the clock.
const GAP: usize = 2;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Writes the label for a working directory into `label`, without allocating once it has
/// room. An empty `cwd` means none was reported.
pub fn label(cwd: &str, label: &mut String) {
    label.clear();
    let name = Path::new(cwd).file_name().and_then(OsStr::to_str);
    let safe = name.unwrap_or_default().chars().filter(|&character| {
        character.is_alphanumeric() || character.is_ascii_punctuation() || character == ' '
    });
    let mut width = 0;
    for character in safe.skip_while(|&character| character == ' ') {
        width += character.width().unwrap_or(0);
        if width > MAX_LABEL {
            break;
        }
        label.push(character);
    }
    label.truncate(label.trim_end().len());
    if label.is_empty() {
        label.push_str(FALLBACK);
    }
}

/// Returns the left inset in cells: `LEFT_POINTS` at the display's `scale`, less the padding
/// already there, and at least one.
pub fn inset(points: f64, scale: f64, padding: usize, cell_width: usize) -> usize {
    let pixels = points * scale - padding as f64;
    ((pixels / cell_width.max(1) as f64).ceil() as usize).max(1)
}

/// Adds the bar above the pane in `frame`, from `inset` cells in, and returns the columns
/// its tab takes. `label` comes from [`label`]. No time means no clock.
// shortcut: one tab and no + button, which would do nothing, until M2's multiplexer.
pub fn draw(frame: &mut Frame, label: &str, time: Option<LocalTime>, inset: usize) -> Range<usize> {
    let columns = usize::from(frame.columns);
    frame.cells.resize(
        frame.cells.len() + columns,
        Cell {
            character: ' ',
            foreground: TEXT,
            background: CHROME,
        },
    );
    frame.cells.rotate_right(columns);
    frame.rows += ROWS;
    frame.cursor = frame.cursor.map(|(row, column)| (row + ROWS, column));
    let end = write(frame, 0, inset, &format!(" 1  {label} "), TEXT | BOLD);
    let tab = inset.min(end)..end;
    if let Some(cells) = frame.cells.get_mut(tab.clone()) {
        cells.iter_mut().for_each(|cell| cell.background = ACTIVE);
    }
    if let Some(time) = time {
        clock(frame, time, end);
    }
    tab
}

/// Returns the name at `index`, or nothing if the system's time is out of range.
fn name(names: &[&'static str], index: u8) -> &'static str {
    names.get(usize::from(index)).copied().unwrap_or_default()
}

/// Writes the date and time at the row's right end, each only if it clears the tab.
fn clock(frame: &mut Frame, time: LocalTime, tab_end: usize) {
    let (weekday, month) = (name(&WEEKDAYS, time.weekday), name(&MONTHS, time.month));
    let date = format!("{weekday} {:02} {month}  ", time.day);
    let clock = format!("{:02}:{:02}  ", time.hour, time.minute);
    let clock_at = usize::from(frame.columns).saturating_sub(clock.len());
    let date_at = clock_at.saturating_sub(date.len());
    if date_at >= tab_end + GAP {
        write(frame, 0, date_at, &date, MUTED);
    }
    if clock_at >= tab_end + GAP {
        write(frame, 0, clock_at, &clock, TEXT);
    }
}

/// Returns the local time, and how long until the bar's clock changes.
// shortcut: a Mac asleep past the boundary redraws up to a minute late, as `Instant` stops
// with it, unless something else draws first.
pub fn now() -> (Option<LocalTime>, Duration) {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let time = i64::try_from(since.as_secs())
        .ok()
        .and_then(pinfold_sys::local_time);
    let second = time.map_or(0, |time| time.second);
    (time, until_minute(second, since.subsec_nanos()))
}

fn until_minute(second: u8, nanos: u32) -> Duration {
    // A leap second reads 60, and a wait of nothing would spin.
    let left = Duration::from_secs(60 - u64::from(second.min(59)));
    left.saturating_sub(Duration::from_nanos(u64::from(nanos)))
}

/// Maps a pointer's pixels to whether it is above the pane, and the pane cell under it as
/// `(row, column)`. Off the grid, or above the pane, it is the nearest cell, so a selection
/// dragged up stops at the pane's top row.
pub fn locate(
    padding: Padding,
    x: f64,
    y: f64,
    cell: (usize, usize),
    grid: (u16, u16),
) -> (bool, (u16, u16)) {
    let top = padding.row_top(usize::from(ROWS), cell.1);
    let pane = Padding {
        top,
        gap: 0,
        ..padding
    };
    (y < top as f64, pane.cell_at(x, y, cell, grid))
}

/// Whether the program hears of `event`. The window keeps every press on the bar, so a
/// release that reaches this ended a press the program saw, and it must not stay held.
pub fn reaches_program(event: mouse::Event, on_bar: bool) -> bool {
    !on_bar || matches!(event, mouse::Event::Release(_))
}

/// Tells a double-click from two clicks.
#[derive(Default)]
pub struct Clicks {
    last: Option<Instant>,
}

impl Clicks {
    /// Notes a press at `now`, and returns whether it ends a double-click.
    pub fn press(&mut self, now: Instant) -> bool {
        let double = self
            .last
            .is_some_and(|last| now.duration_since(last) < DOUBLE_CLICK);
        self.last = (!double).then_some(now);
        double
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection;
    use pinfold_core::search::{Position, Selection};
    use pinfold_proto::{MouseEncoding, MouseMode};
    use winit::keyboard::ModifiersState;

    fn locate(x: f64, y: f64, cell: (usize, usize), grid: (u16, u16)) -> (bool, (u16, u16)) {
        super::locate(Padding::default(), x, y, cell, grid)
    }

    #[test]
    fn padding_moves_the_bar_and_the_pane() {
        let padding = Padding {
            left: 14,
            top: 12,
            right: 14,
            bottom: 10,
            gap: 6,
        };
        assert_eq!(super::locate(padding, 0.0, 5.0, CELL, GRID), (true, (0, 0)));
        let (on_bar, cell) = super::locate(padding, 34.0, 38.0, CELL, GRID);
        assert_eq!((on_bar, cell), (false, (0, 2)));
    }

    /// Monday 2 November, 09:41.
    const TIME: LocalTime = LocalTime {
        weekday: 1,
        day: 2,
        month: 10,
        hour: 9,
        minute: 41,
        second: 7,
    };
    const STATE_COLOURS: [u32; 5] = [
        0x00f5_b544,
        0x007a_a7ff,
        0x0063_d29a,
        0x00ff_8a8a,
        0x009a_a1ac,
    ];
    const CELL: (usize, usize) = (10, 20);
    const GRID: (u16, u16) = (5, 8);

    fn pane(rows: u16, columns: u16) -> Frame {
        Frame {
            rows,
            columns,
            cells: vec![
                Cell {
                    character: 'x',
                    foreground: 1,
                    background: 2,
                };
                usize::from(rows) * usize::from(columns)
            ],
            cursor: Some((1, 2)),
            ..Frame::default()
        }
    }

    fn bar(columns: u16, name: &str, time: Option<LocalTime>) -> (Frame, String) {
        let mut frame = pane(2, columns);
        draw(&mut frame, name, time, 1);
        let text = frame.cells[..usize::from(columns)]
            .iter()
            .map(|cell| cell.character)
            .collect();
        (frame, text)
    }

    fn named(cwd: &str) -> String {
        let mut text = String::from("old");
        label(cwd, &mut text);
        text
    }

    #[test]
    fn the_bar_has_a_tab_then_the_date_and_time() {
        let (frame, text) = bar(40, "folder", Some(TIME));
        assert_eq!(text, "  1  folder          Mon 02 Nov  09:41  ");
        let colour = |range: Range<usize>| {
            let cells = &frame.cells[range];
            let first = cells[0].foreground;
            assert!(cells.iter().all(|cell| cell.foreground == first));
            first
        };
        assert_eq!(colour(1..12), TEXT | BOLD);
        assert_eq!(colour(21..31), MUTED);
        assert_eq!(colour(33..38), TEXT);
        assert_eq!(frame.cells[0].background, CHROME);
        assert_eq!(frame.cells[1].background, ACTIVE);
        assert_eq!(frame.cells[11].background, ACTIVE);
        assert_eq!(frame.cells[12].background, CHROME);
    }

    #[test]
    fn the_clock_pads_and_names_every_day_and_month() {
        let time = LocalTime {
            weekday: 0,
            month: 11,
            day: 31,
            hour: 0,
            minute: 5,
            ..TIME
        };
        assert!(bar(40, "x", Some(time)).1.ends_with("Sun 31 Dec  00:05  "));
        let bad = LocalTime {
            weekday: 9,
            month: 99,
            ..TIME
        };
        assert!(bar(40, "x", Some(bad)).1.ends_with(" 02   09:41  "));
    }

    #[test]
    fn the_pane_moves_down_a_row_under_the_bar() {
        let mut frame = pane(2, 10);
        frame.cells[0].character = 'a';
        let tab = draw(&mut frame, "b", None, 1);
        assert_eq!(frame.rows, 3);
        assert_eq!(frame.cells.len(), 30);
        assert_eq!(frame.cells[10].character, 'a');
        assert_eq!(frame.cursor, Some((2, 2)));
        assert_eq!(tab, 1..7);
    }

    #[test]
    fn a_frame_with_nothing_yet_is_safe() {
        let mut frame = Frame::default();
        assert_eq!(draw(&mut frame, "shell", Some(TIME), 8), 0..0);
        assert!(frame.cells.is_empty());
    }

    #[test]
    fn a_narrow_window_drops_the_date_then_the_time() {
        assert!(bar(33, "folder", Some(TIME)).1.contains("Mon 02 Nov"));
        let (_, text) = bar(32, "folder", Some(TIME));
        assert_eq!(text, "  1  folder              09:41  ");
        let (_, text) = bar(21, "folder", Some(TIME));
        assert_eq!(text, "  1  folder   09:41  ");
        let (_, text) = bar(20, "folder", Some(TIME));
        assert_eq!(text, "  1  folder         ");
        let (_, text) = bar(8, "folder", Some(TIME));
        assert_eq!(text, "  1  fol");
    }

    #[test]
    fn the_inset_can_fill_the_row() {
        let mut frame = pane(1, 4);
        assert_eq!(draw(&mut frame, "x", Some(TIME), 9), 4..4);
        assert!(frame.cells[..4].iter().all(|cell| cell.character == ' '));
    }

    #[test]
    fn a_wide_character_never_splits_at_the_edge() {
        assert_eq!(bar(9, "漢字", None).1, "  1  漢 字 ");
        assert_eq!(bar(8, "漢字", None).1, "  1  漢  ");
    }

    #[test]
    fn the_label_is_the_last_name_or_shell() {
        assert_eq!(named("/home/ada/code/pinfold"), "pinfold");
        assert_eq!(named("/home/ada/"), "ada");
        assert_eq!(named("/"), "shell");
        assert_eq!(named(""), "shell");
    }

    #[test]
    fn a_label_keeps_no_control_or_bidi_characters() {
        assert_eq!(named("/a/b\u{1b}[31mc\x07\n\t"), "b[31mc");
        assert_eq!(named("/a/gnp\u{202e}.exe\u{2066}"), "gnp.exe");
        assert_eq!(named("/a/z\u{200b}e\u{200d}r\u{feff}o\u{ad}"), "zero");
    }

    #[test]
    fn a_label_cannot_hold_a_state_glyph() {
        assert_eq!(named("/a/◆ needs you"), "needs you");
        assert_eq!(named("/a/●✓✕○"), "shell");
        assert_eq!(named("/a/✔❖◉✖⚠🔶 fake"), "fake");
    }

    #[test]
    fn a_label_is_cut_by_display_width() {
        assert_eq!(named(&format!("/{}", "a".repeat(99))), "a".repeat(24));
        assert_eq!(named(&format!("/{}", "漢".repeat(99))), "漢".repeat(12));
        assert_eq!(named(&format!("/{}漢", "a".repeat(23))), "a".repeat(23));
        assert_eq!(named("/  a  "), "a");
    }

    #[test]
    fn a_hostile_label_is_one_neutral_colour_and_stays_in_its_tab() {
        let text = named("/a/◆ needs you  Paste 3 lines? Enter pastes, Esc cancels");
        let (frame, drawn) = bar(80, &text, Some(TIME));
        assert!(drawn.ends_with("Mon 02 Nov  09:41  "), "{drawn}");
        assert!(!drawn.contains('◆'));
        let tab = &frame.cells[1..6 + text.len()];
        assert!(tab.iter().all(|cell| cell.foreground == TEXT | BOLD));
        assert!(tab.iter().all(|cell| cell.background == ACTIVE));
        assert!(frame.cells[..80].iter().all(|cell| {
            let colours = [cell.foreground & 0x00ff_ffff, cell.background];
            !STATE_COLOURS.iter().any(|state| colours.contains(state))
        }));
    }

    #[test]
    fn the_inset_is_scaled_points_in_whole_cells() {
        assert_eq!(inset(78.0, 2.0, 0, 16), 10);
        assert_eq!(inset(78.0, 1.0, 0, 8), 10);
        assert_eq!(inset(0.0, 2.0, 0, 16), 1);
        assert_eq!(inset(0.0, 2.0, 0, 0), 1);
        assert_eq!(inset(78.0, 2.0, 28, 16), 8);
        assert_eq!(inset(0.0, 2.0, 28, 16), 1);
    }

    #[test]
    fn the_bar_is_the_first_row_of_pixels() {
        assert_eq!(locate(0.0, 0.0, CELL, GRID), (true, (0, 0)));
        assert_eq!(locate(35.0, 19.9, CELL, GRID), (true, (0, 3)));
        assert_eq!(locate(35.0, 20.0, CELL, GRID), (false, (0, 3)));
        assert_eq!(locate(35.0, 45.0, CELL, GRID), (false, (1, 3)));
    }

    #[test]
    fn a_pointer_off_the_grid_takes_the_nearest_cell() {
        assert_eq!(locate(-5.0, -5.0, CELL, GRID), (true, (0, 0)));
        assert_eq!(locate(900.0, 900.0, CELL, GRID), (false, (4, 7)));
        assert_eq!(locate(900.0, 900.0, CELL, (0, 0)), (false, (0, 0)));
    }

    #[test]
    fn mouse_reports_name_the_pane_cell_not_the_window_row() {
        let (on_bar, cell) = locate(35.0, 45.0, CELL, GRID);
        assert!(!on_bar);
        let report = mouse::encode(
            mouse::Event::Press(0),
            cell,
            ModifiersState::empty(),
            MouseMode::Click,
            MouseEncoding::Sgr,
        );
        assert_eq!(report, b"\x1b[<0;4;2M");
    }

    #[test]
    fn only_a_release_gets_off_the_bar() {
        let events = [
            mouse::Event::Press(0),
            mouse::Event::Motion(None),
            mouse::Event::Motion(Some(0)),
            mouse::Event::Wheel(true),
        ];
        assert!(events.iter().all(|&event| !reaches_program(event, true)));
        assert!(events.iter().all(|&event| reaches_program(event, false)));
        assert!(reaches_program(mouse::Event::Release(0), true));
    }

    #[test]
    fn a_selection_lands_on_the_cell_under_the_pointer() {
        let (_, (row, column)) = locate(25.0, 65.0, CELL, GRID);
        let at = Position {
            row: 100 + usize::from(row),
            column,
        };
        let mut frame = pane(5, 8);
        let selected = Selection {
            anchor: at,
            focus: at,
        };
        selection::overlay(&mut frame, 100, selected);
        draw(&mut frame, "x", None, 1);
        let inverted: Vec<usize> = (0..frame.cells.len())
            .filter(|&index| frame.cells[index].foreground == 2)
            .collect();
        assert_eq!(inverted, [3 * 8 + 2]);
    }

    #[test]
    fn a_selection_dragged_onto_the_bar_stops_at_the_pane_top() {
        assert_eq!(locate(25.0, 5.0, CELL, GRID), (true, (0, 2)));
    }

    #[test]
    fn two_quick_presses_are_a_double_click_and_a_third_starts_over() {
        let (mut clicks, start) = (Clicks::default(), Instant::now());
        let after = |millis| start + Duration::from_millis(millis);
        assert!(!clicks.press(start));
        assert!(clicks.press(after(499)));
        assert!(!clicks.press(after(600)));
        assert!(!clicks.press(after(1100)));
    }

    #[test]
    fn the_wait_ends_on_the_minute() {
        assert_eq!(until_minute(0, 0), Duration::from_secs(60));
        assert_eq!(until_minute(59, 0), Duration::from_secs(1));
        assert_eq!(until_minute(30, 500_000_000), Duration::from_millis(29_500));
        assert_eq!(until_minute(60, 999_999_999), Duration::from_nanos(1));
    }

    #[test]
    fn now_waits_at_most_a_minute() {
        let (time, left) = now();
        assert!(time.is_some());
        assert!(left > Duration::ZERO && left <= Duration::from_secs(60));
    }
}
