//! The rows above the pane: a title with the date and time, then a row of pill tabs. Fullscreen
//! has no title row, as the system hides the title bar it lines up with.
//!
//! A tab's label is the last name in a path that program output wrote (OSC 7), so a
//! program chooses it. It keeps only letters, digits, ASCII punctuation and spaces, which
//! rules out control and bidi characters and every state glyph, and it is always drawn in
//! one neutral colour. A state is glyph, colour and word, so the most a label can show is
//! the word (T4 and T6 in the threat model). Only the row under the pane asks anything.
//!
//! A tab whose pane is not idle shows its state's glyph in its colour, as the design's tabs
//! do, and the title row gives the active tab's state as glyph, colour and word, with the
//! agent's message or "from output" when marks, which output can fake, gave it.

use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pinfold_proto::{BOLD, Cell, DEFAULT_BACKGROUND, Frame, LEFT_CAP, RIGHT_CAP, TALL};
use pinfold_render::padding::Padding;
use pinfold_sys::LocalTime;
use unicode_width::UnicodeWidthChar;

use crate::mouse;
use crate::paste::TEXT;
use crate::search_bar::write;

/// Points to leave on the left. On macOS the real traffic lights sit there, and elsewhere
/// the system draws its buttons outside the window.
pub const LEFT_POINTS: f64 = if cfg!(target_os = "macos") { 86.0 } else { 0.0 };

/// The padding's colour, so the bar has no edge and a translucent window shows through it.
const CHROME: u32 = DEFAULT_BACKGROUND;
/// The active tab. The default blue is a running agent's colour, and a state is glyph, colour
/// and word, so a tab must not look like one.
const ACTIVE: u32 = 0x0025_3c3b;
const ACCENT: u32 = 0x005c_d6cb;
/// An inactive tab or the new tab button.
const QUIET: u32 = 0x001c_2026;
const HOVER: u32 = 0x002b_323b;
const QUIET_TEXT: u32 = 0x00b0_b8c4;
const HOVER_TEXT: u32 = 0x00e2_e5ea;
/// The number on a tab that is not active.
const KEY_TEXT: u32 = 0x0090_9ba9;
const NEW_TAB_TEXT: u32 = 0x00b0_b8c4;
const TITLE: u32 = 0x009b_a5b3;
const CLOCK_TEXT: u32 = 0x00e2_e5ea;
/// The title and date of an unfocused window.
const UNFOCUSED_TITLE: u32 = 0x0087_909e;
/// The time and the cursor of an unfocused window.
const UNFOCUSED: u32 = 0x009b_a5b3;

const FALLBACK: &str = "shell";
/// Widest label in cells, so it stays a tab and cannot crowd out the clock.
const MAX_LABEL: usize = 24;
/// Empty cells kept between the title and the clock.
const GAP: usize = 2;
/// The new tab button's cells: an end, the plus, an end.
const NEW_TAB: usize = 3;
/// Cells before a tab's label: its end and a space.
const LEADING: usize = 2;
/// The mark on a tab with news, with the space after it. It is not a state's glyph, and
/// [`label`] keeps it out of labels.
const ACTIVITY_MARK: char = '•';
const ACTIVITY: usize = 2;
/// What a tab's number is pressed with.
const KEY: &str = if cfg!(target_os = "macos") { "⌘" } else { "" };
/// How many tabs have a number.
const KEYED: usize = 9;
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// Each agent state by its number on the link: glyph, word and colour, from
/// `docs/design/ui/Foundations.dc.html` (CLAUDE.md, UI).
pub const STATES: [(char, &str, u32); 5] = [
    ('◆', "needs you", 0x00f5_b544),
    ('●', "running", 0x007a_a7ff),
    ('✓', "done", 0x0063_d29a),
    ('✕', "error", 0x00ff_8a8a),
    ('○', "idle", 0x009a_a1ac),
];
pub(crate) const NEEDS_YOU: u8 = 0;
const IDLE: u8 = 4;
/// Widest message in cells, so it cannot crowd out the clock for long.
const MAX_MESSAGE: usize = 48;

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Whether chrome may draw a character from a program or an agent: letters, digits, ASCII
/// punctuation and spaces, which rules out control, bidi and format characters and every state
/// glyph (T4, T6).
fn safe(character: char) -> bool {
    character.is_alphanumeric() || character.is_ascii_punctuation() || character == ' '
}

/// Writes the label for a working directory into `label`, without allocating once it has
/// room. An empty `cwd` means none was reported.
pub fn label(cwd: &str, label: &mut String) {
    label.clear();
    let name = Path::new(cwd).file_name().and_then(OsStr::to_str);
    let safe = name
        .unwrap_or_default()
        .chars()
        .filter(|&character| safe(character));
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

/// Returns the state a tab's pill shows: needs you if it alerted while hidden, else that of the
/// pane that names it, and idle before the core says.
pub fn tab_state(status: Option<(u8, bool)>, alerts: u32, index: usize) -> u8 {
    if has_news(alerts, index) {
        NEEDS_YOU
    } else {
        status.map_or(IDLE, |(state, _)| state.min(IDLE))
    }
}

/// Returns a bit for each tab whose state is not idle, which takes a mark as news does.
pub fn marked(states: &[u8]) -> u32 {
    let busy = states
        .iter()
        .enumerate()
        .filter(|(_, state)| **state != IDLE);
    busy.fold(0, |bits, (index, _)| {
        bits | u32::try_from(index)
            .ok()
            .and_then(|index| 1u32.checked_shl(index))
            .unwrap_or(0)
    })
}

/// Returns the left inset in cells: `LEFT_POINTS` at the display's `scale`, less the padding
/// already there, and at least one.
pub fn inset(points: f64, scale: f64, padding: usize, cell_width: usize) -> usize {
    let pixels = points * scale - padding as f64;
    ((pixels / cell_width.max(1) as f64).ceil() as usize).max(1)
}

/// What the bar shows.
pub struct Bar<'a, L> {
    /// Labels from [`label`].
    pub tabs: &'a [L],
    /// The index of the active tab.
    pub active: usize,
    /// Which tabs have a mark, news or a state, one bit each, lowest first.
    pub activity: u32,
    /// Each tab's state from [`tab_state`], which its mark shows unless idle.
    pub states: &'a [u8],
    /// The active tab's state, whether its agent reported it, and the message, for the title.
    pub status: Option<(u8, bool, &'a str)>,
    /// The tab under the pointer.
    pub hovered: Option<usize>,
    /// Whether the window has the keyboard.
    pub focused: bool,
    /// Whether the title row shows.
    pub title: bool,
    /// The clock. No time means no clock.
    pub time: Option<LocalTime>,
    /// Cells left of the title.
    pub inset: usize,
}

/// Adds the bar above the pane in `frame`. The title is the active tab's label.
pub fn draw(frame: &mut Frame, bar: &Bar<'_, impl AsRef<str>>) {
    if !bar.focused
        && let Some((row, column)) = frame.cursor
    {
        let at = usize::from(row) * usize::from(frame.columns) + usize::from(column);
        if let Some(cell) = frame.cells.get_mut(at) {
            cell.foreground = UNFOCUSED;
        }
    }
    let rows = if bar.title { 2 } else { 1 };
    let cells = usize::from(rows) * usize::from(frame.columns);
    frame.cells.resize(
        frame.cells.len() + cells,
        Cell {
            character: ' ',
            foreground: TEXT,
            background: CHROME,
        },
    );
    frame.cells.rotate_right(cells);
    frame.rows += rows;
    frame.cursor = frame.cursor.map(|(row, column)| (row + rows, column));
    if bar.title {
        let title = bar.tabs.get(bar.active).map_or("", AsRef::as_ref);
        let colour = if bar.focused { TITLE } else { UNFOCUSED_TITLE };
        let mut end = write(frame, 0, bar.inset, title, colour);
        if let Some(status) = bar.status {
            end = state_line(frame, end + GAP, status, colour);
        }
        if let Some(time) = bar.time {
            clock(frame, time, end, bar.focused);
        }
    }
    strip(frame, bar, cells - usize::from(frame.columns));
}

/// Writes a state as glyph and word in its colour from `column` of the title row, then where it
/// came from in `colour`: the agent's message, or "from output" for marks. Nothing for an idle
/// shell. Returns the column after it.
fn state_line(frame: &mut Frame, column: usize, status: (u8, bool, &str), colour: u32) -> usize {
    let (state, reported, message) = status;
    let Some(&(glyph, word, tint)) = STATES.get(usize::from(state)) else {
        return column;
    };
    if state == IDLE && !reported {
        return column;
    }
    let end = write(frame, 0, column, &format!("{glyph} {word}"), tint | BOLD);
    // The core cleaned it too, but the window draws only what it allows itself.
    let message: String = message
        .chars()
        .filter(|&character| safe(character))
        .collect();
    let source = if !reported {
        "from output"
    } else if message.trim().is_empty() {
        return end;
    } else {
        message.trim()
    };
    write(
        frame,
        0,
        end,
        &format!(" · {}", fit(source, MAX_MESSAGE)),
        colour,
    )
}

/// Returns how many cells `text` takes.
fn width(text: &str) -> usize {
    text.chars().filter_map(UnicodeWidthChar::width).sum()
}

/// Returns whether `activity` holds the tab at `index`.
fn has_news(activity: u32, index: usize) -> bool {
    u32::try_from(index)
        .ok()
        .and_then(|index| activity.checked_shr(index))
        .is_some_and(|bits| bits & 1 == 1)
}

/// Returns the cells of a tab's number: a space, then a small pill. Past [`KEYED`], just a space.
fn key_cells(index: usize) -> usize {
    if index < KEYED { width(KEY) + 4 } else { 1 }
}

/// Returns the cells a tab takes with `label` cut to `room`.
fn tab_cells(label: &str, room: usize, index: usize, activity: u32) -> usize {
    let mark = if has_news(activity, index) {
        ACTIVITY
    } else {
        0
    };
    LEADING + mark + width(label).min(room) + key_cells(index) + 1
}

/// Where one tab sits on the tabs' row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    index: usize,
    start: usize,
    end: usize,
    /// Cells its label may take, or nothing for a tab with no room for its ends and number.
    room: Option<usize>,
}

/// Returns where the tabs go: from the left at their own widths, a cell apart, with the longest
/// labels cut first. Tabs that do not fit even so share the row equally.
fn spans<'a>(
    tabs: &'a [impl AsRef<str>],
    activity: u32,
    columns: usize,
) -> impl Iterator<Item = Span> + 'a {
    let row = |room: usize| {
        let each = tabs.iter().enumerate();
        each.map(|(index, label)| tab_cells(label.as_ref(), room, index, activity) + 1)
            .sum::<usize>()
    };
    let room = (1..=MAX_LABEL)
        .rev()
        .find(|&room| row(room) + NEW_TAB <= columns);
    let equal = columns.saturating_sub(NEW_TAB + 1) / tabs.len().max(1);
    let mut start = 0;
    tabs.iter()
        .enumerate()
        .map(move |(index, label)| {
            let cells = room.map_or(equal.saturating_sub(1), |room| {
                tab_cells(label.as_ref(), room, index, activity)
            });
            let span = Span {
                index,
                start,
                end: start + cells,
                room,
            };
            start = span.end + 1;
            span
        })
        .filter(|span| span.end > span.start)
}

/// What a press on the tabs' row landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The tab at this index.
    Tab(usize),
    /// The new tab button.
    NewTab,
}

/// Returns what `column` of the tabs' row holds, in a window `columns` wide.
pub fn hit(column: u16, tabs: &[impl AsRef<str>], activity: u32, columns: u16) -> Option<Hit> {
    let column = usize::from(column);
    let mut button = 0;
    for span in spans(tabs, activity, usize::from(columns)) {
        if (span.start..span.end).contains(&column) {
            return Some(Hit::Tab(span.index));
        }
        button = span.end + 1;
    }
    (button..button + NEW_TAB)
        .contains(&column)
        .then_some(Hit::NewTab)
}

/// Returns a tab's fill, label colour and number colour.
fn look<L>(index: usize, bar: &Bar<'_, L>) -> (u32, u32, u32) {
    match (index == bar.active, bar.focused) {
        (true, true) => (ACTIVE, ACCENT | BOLD, QUIET_TEXT),
        (true, false) => (HOVER, HOVER_TEXT | BOLD, KEY_TEXT),
        _ if bar.hovered == Some(index) => (HOVER, HOVER_TEXT, QUIET_TEXT),
        _ => (QUIET, QUIET_TEXT, KEY_TEXT),
    }
}

/// Writes the tabs' row, which starts at cell `row`: pills from the left, then the button.
fn strip(frame: &mut Frame, bar: &Bar<'_, impl AsRef<str>>, row: usize) {
    let columns = usize::from(frame.columns);
    let mut button = 0;
    for span in spans(bar.tabs, bar.activity, columns) {
        button = span.end + 1;
        let (fill, text, key) = look(span.index, bar);
        pill(frame, row, span.start..span.end, fill | TALL, CHROME);
        let label = bar.tabs.get(span.index).map_or("", AsRef::as_ref);
        let Some(room) = span.room else {
            let shown = fit(label, (span.end - span.start).saturating_sub(2));
            write(frame, row, span.start + 1, &shown, text);
            continue;
        };
        let mut at = span.start + LEADING;
        if has_news(bar.activity, span.index) {
            let state = bar.states.get(span.index).copied().unwrap_or(IDLE);
            let (mark, tint) = match STATES.get(usize::from(state)) {
                Some(&(glyph, _, tint)) if state != IDLE => (glyph, tint),
                _ => (ACTIVITY_MARK, ACCENT),
            };
            put(frame, row, at, mark, tint, fill);
            at += ACTIVITY;
        }
        write(frame, row, at, &fit(label, room), text);
        if span.index < KEYED {
            let left = span.end - key_cells(span.index);
            pill(frame, row, left..span.end - 1, CHROME, fill);
            let number = format!("{KEY}{}", span.index + 1);
            write(frame, row, left + 1, &number, key);
        }
    }
    pill(frame, row, button..button + NEW_TAB, QUIET | TALL, CHROME);
    write(frame, row, button + 1, "+", NEW_TAB_TEXT);
}

/// Draws a pill over a span of the tabs' row, on `outside`. `inside` can carry [`TALL`].
fn pill(frame: &mut Frame, row: usize, span: std::ops::Range<usize>, inside: u32, outside: u32) {
    fill(frame, row, span.clone(), inside & !TALL);
    put(frame, row, span.start, LEFT_CAP, inside, outside);
    put(frame, row, span.end - 1, RIGHT_CAP, inside, outside);
}

/// Sets one cell of a row, unless it is past the edge.
fn put(
    frame: &mut Frame,
    row: usize,
    column: usize,
    character: char,
    foreground: u32,
    background: u32,
) {
    if column < usize::from(frame.columns)
        && let Some(cell) = frame.cells.get_mut(row + column)
    {
        *cell = Cell {
            character,
            foreground,
            background,
        };
    }
}

/// Paints a span of a row.
fn fill(frame: &mut Frame, row: usize, span: std::ops::Range<usize>, background: u32) {
    let end = span.end.min(usize::from(frame.columns));
    if let Some(cells) = frame.cells.get_mut(row + span.start.min(end)..row + end) {
        cells
            .iter_mut()
            .for_each(|cell| cell.background = background);
    }
}

/// Returns `label` cut to `room` cells, marked by an ellipsis if anything went.
fn fit(label: &str, room: usize) -> String {
    if width(label) <= room {
        return label.to_owned();
    }
    let mut shown = String::new();
    let mut used = 1;
    for character in label.chars() {
        used += character.width().unwrap_or(0);
        if used > room {
            break;
        }
        shown.push(character);
    }
    if room > 0 {
        shown.push('…');
    }
    shown
}

/// Returns the name at `index`, or nothing if the system's time is out of range.
fn name(names: &[&'static str], index: u8) -> &'static str {
    names.get(usize::from(index)).copied().unwrap_or_default()
}

/// Writes the date and time at the row's right end, each only if it clears the title.
fn clock(frame: &mut Frame, time: LocalTime, title_end: usize, focused: bool) {
    let (weekday, month) = (name(&WEEKDAYS, time.weekday), name(&MONTHS, time.month));
    let date = format!("{weekday} {:02} {month}  ", time.day);
    let clock = format!("{:02}:{:02}", time.hour, time.minute);
    let clock_at = usize::from(frame.columns).saturating_sub(clock.len());
    let date_at = clock_at.saturating_sub(date.len());
    let (date_colour, clock_colour) = if focused {
        (TITLE, CLOCK_TEXT)
    } else {
        (UNFOCUSED_TITLE, UNFOCUSED)
    };
    if date_at >= title_end + GAP {
        write(frame, 0, date_at, &date, date_colour);
    }
    if clock_at >= title_end + GAP {
        write(frame, 0, clock_at, &clock, clock_colour);
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
    let top = padding.row_top(padding.bar_rows(), cell.1);
    let pane = Padding {
        top,
        gap: 0,
        ..padding
    };
    (y < top as f64, pane.cell_at(x, y, cell, grid))
}

/// Whether a pointer at pixel `y` is on the title row, which moves the window.
pub fn on_title(padding: Padding, y: f64, cell_height: usize) -> bool {
    !padding.fullscreen && y < padding.row_top(1, cell_height) as f64
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
    use pinfold_proto::link::MAX_TABS;
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
            ..Padding::default()
        };
        assert_eq!(super::locate(padding, 0.0, 5.0, CELL, GRID), (true, (0, 0)));
        let (on_bar, cell) = super::locate(padding, 34.0, 58.0, CELL, GRID);
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

    fn text(frame: &Frame, row: usize) -> String {
        let columns = usize::from(frame.columns);
        frame.cells[row * columns..(row + 1) * columns]
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    fn shown<'a>(names: &'a [&'a str], active: usize) -> Bar<'a, &'a str> {
        Bar {
            tabs: names,
            active,
            activity: 0,
            states: &[],
            status: None,
            hovered: None,
            focused: true,
            title: true,
            time: None,
            inset: 1,
        }
    }

    #[test]
    fn states_match_the_core_and_the_contributor_rule() {
        use pinfold_core::status::Status;
        let all = [
            Status::NeedsYou,
            Status::Running,
            Status::Done,
            Status::Error,
            Status::Idle,
        ];
        assert_eq!(STATES.len(), usize::from(pinfold_proto::link::STATES));
        for status in all {
            let (glyph, word, _) = STATES[status as usize];
            assert_eq!((glyph, word), (status.glyph(), status.word()));
        }
        assert_eq!(STATES[usize::from(NEEDS_YOU)].2, 0x00f5_b544, "amber");
        assert_eq!(STATES[usize::from(IDLE)].0, '○');
    }

    #[test]
    fn a_tab_shows_its_state_and_a_hidden_alert_shows_needs_you() {
        let status = [Some((1, false)), None, Some((3, true)), Some((9, true))];
        let states: Vec<u8> = (0..4).map(|i| tab_state(status[i], 0b10, i)).collect();
        assert_eq!(states, [1, NEEDS_YOU, 3, IDLE]);
        assert_eq!(marked(&states), 0b111);

        let names = ["a", "b", "c", "d"];
        let mut frame = pane(2, 60);
        let bar = Bar {
            activity: marked(&states),
            states: &states,
            ..shown(&names, 3)
        };
        draw(&mut frame, &bar);
        let strip = strip_of(&frame);
        for (index, state) in [(0, 1), (1, NEEDS_YOU), (2, 3)] {
            let start = spans(&names, bar.activity, 60).nth(index).unwrap().start;
            let mark = strip[start + LEADING];
            let (glyph, _, tint) = STATES[usize::from(state)];
            assert_eq!(
                (mark.character, mark.foreground),
                (glyph, tint),
                "tab {index}"
            );
        }
    }

    #[test]
    fn the_title_gives_glyph_colour_word_and_where_it_came_from() {
        let title = |status| {
            let mut frame = pane(2, 70);
            draw(
                &mut frame,
                &Bar {
                    status,
                    ..shown(&["work"], 0)
                },
            );
            let glyph = frame.cells.iter().find(|cell| cell.character == '◆');
            let tint = glyph.map(|cell| cell.foreground);
            (text(&frame, 0).trim_end().to_owned(), tint)
        };
        let (line, tint) = title(Some((NEEDS_YOU, true, "approve the diff?")));
        assert_eq!(line, " work  ◆ needs you · approve the diff?");
        assert_eq!(tint, Some(STATES[0].2 | BOLD));
        assert_eq!(title(Some((2, false, ""))).0, " work  ✓ done · from output");
        assert_eq!(title(Some((IDLE, false, ""))).0, " work");
        assert_eq!(title(Some((IDLE, true, ""))).0, " work  ○ idle");
        // A hostile message keeps no control, bidi or glyph, so it cannot fake a state.
        let hostile = "ok\u{202e} \u{1b}[31m◆ needs you\u{2066}";
        let (line, _) = title(Some((1, true, hostile)));
        assert_eq!(line, " work  ● running · ok [31m needs you");
        assert!(
            !line.contains(['\u{202e}', '\u{1b}', '\u{2066}', '◆']),
            "{line}"
        );
        let long = "x".repeat(200);
        let (line, _) = title(Some((1, true, long.as_str())));
        assert!(line.ends_with('…') && line.len() < 80, "{line}");
    }

    fn bar(columns: u16, name: &str, time: Option<LocalTime>) -> (Frame, String) {
        let mut frame = pane(2, columns);
        draw(
            &mut frame,
            &Bar {
                time,
                ..shown(&[name], 0)
            },
        );
        let title = text(&frame, 0);
        (frame, title)
    }

    fn tabs(columns: u16, names: &[&str], active: usize) -> Frame {
        let mut frame = pane(2, columns);
        draw(&mut frame, &shown(names, active));
        frame
    }

    /// Returns the cells of the tabs' row of `frame`.
    fn strip_of(frame: &Frame) -> &[Cell] {
        let columns = usize::from(frame.columns);
        &frame.cells[columns..columns * 2]
    }

    /// Returns where the tab named `name` starts on the row, and its cell count.
    fn tab_of(names: &[&str], index: usize, columns: usize) -> (usize, usize) {
        let span = spans(names, 0, columns).nth(index).unwrap();
        (span.start, span.end - span.start)
    }

    /// Where a tab's number sits, whichever key this system uses.
    fn key(number: usize) -> String {
        format!("{KEY}{number}")
    }

    fn named(cwd: &str) -> String {
        let mut text = String::from("old");
        label(cwd, &mut text);
        text
    }

    #[test]
    fn the_title_is_the_active_tab_then_the_date_and_time() {
        let (frame, text) = bar(40, "folder", Some(TIME));
        assert_eq!(text, " folder                Mon 02 Nov  09:41");
        assert_eq!(frame.cells[1].foreground, TITLE);
        assert_eq!(frame.cells[37].foreground, CLOCK_TEXT);
        assert_eq!(frame.cells[23].foreground, TITLE);
        assert!(
            frame.cells[..40]
                .iter()
                .all(|cell| cell.background == CHROME)
        );
    }

    #[test]
    fn one_focused_tab_has_a_tinted_pill_with_its_key_then_the_button() {
        let frame = tabs(60, &["agent"], 0);
        let row = strip_of(&frame);
        let cells = LEADING + "agent".len() + key_cells(0) + 1;
        assert_eq!(row[0].character, LEFT_CAP);
        assert_eq!(
            (row[0].foreground, row[0].background),
            (ACTIVE | TALL, CHROME)
        );
        assert_eq!(row[2].character, 'a');
        assert_eq!(
            (row[2].foreground, row[2].background),
            (ACCENT | BOLD, ACTIVE)
        );
        assert_eq!(row[cells - 1].character, RIGHT_CAP);
        assert_eq!(row[cells - 1].foreground, ACTIVE | TALL);
        let key_at = cells - key_cells(0);
        assert_eq!(row[key_at].character, LEFT_CAP);
        assert_eq!(
            (row[key_at].foreground, row[key_at].background),
            (CHROME, ACTIVE)
        );
        assert_eq!(row[key_at + 1].background, CHROME);
        let digit = key_at + 1 + width(KEY);
        assert_eq!(row[digit].character, '1');
        assert_eq!(row[digit].foreground, QUIET_TEXT);
        assert_eq!(row[cells].background, CHROME, "a gap before the button");
        assert_eq!(row[cells + 1].character, LEFT_CAP);
        assert_eq!(row[cells + 2].character, '+');
        assert_eq!(row[cells + 2].foreground, NEW_TAB_TEXT);
        assert_eq!(row[cells + 3].character, RIGHT_CAP);
        assert_eq!(row[cells + 3].foreground, QUIET | TALL);
        assert_eq!(
            row[cells + 4].background,
            CHROME,
            "the button is not at the far end"
        );
    }

    #[test]
    fn a_tab_that_is_not_active_is_quiet() {
        let frame = tabs(60, &["a", "b"], 0);
        let row = strip_of(&frame);
        let (start, cells) = tab_of(&["a", "b"], 1, 60);
        assert_eq!(row[start].foreground, QUIET | TALL);
        assert_eq!(row[start + 2].character, 'b');
        assert_eq!(
            (row[start + 2].foreground, row[start + 2].background),
            (QUIET_TEXT, QUIET)
        );
        let digit = start + cells - 3;
        assert_eq!(row[digit].character, '2');
        assert_eq!(row[digit].foreground, KEY_TEXT);
    }

    #[test]
    fn hovering_lights_a_quiet_tab_and_not_the_active_one() {
        let names = ["a", "b"];
        let mut frame = pane(2, 60);
        draw(
            &mut frame,
            &Bar {
                hovered: Some(1),
                ..shown(&names, 0)
            },
        );
        let row = strip_of(&frame);
        let (start, cells) = tab_of(&names, 1, 60);
        assert_eq!(row[start + 2].background, HOVER);
        assert_eq!(row[start + 2].foreground, HOVER_TEXT);
        assert_eq!(row[start + cells - 3].foreground, QUIET_TEXT);
        let mut frame = pane(2, 60);
        draw(
            &mut frame,
            &Bar {
                hovered: Some(0),
                ..shown(&names, 0)
            },
        );
        assert_eq!(strip_of(&frame)[2].background, ACTIVE);
    }

    #[test]
    fn a_tab_with_news_has_a_teal_mark_before_its_label() {
        let names = ["a", "b"];
        let mut frame = pane(2, 60);
        draw(
            &mut frame,
            &Bar {
                activity: 0b10,
                ..shown(&names, 0)
            },
        );
        let row = strip_of(&frame);
        let start = spans(&names, 0b10, 60).nth(1).unwrap().start;
        assert_eq!(row[start + 2].character, ACTIVITY_MARK);
        assert_eq!(row[start + 2].foreground, ACCENT);
        assert_eq!(row[start + 4].character, 'b');
        assert!(
            spans(&names, 0b10, 60).nth(1).unwrap().end > spans(&names, 0, 60).nth(1).unwrap().end
        );
        assert!(!has_news(u32::MAX, 32), "past the last bit");
        assert!(!named("/a/•b").contains(ACTIVITY_MARK));
    }

    #[test]
    fn fullscreen_has_only_the_tabs() {
        let names = ["a", "b"];
        let mut frame = pane(2, 60);
        draw(
            &mut frame,
            &Bar {
                title: false,
                time: Some(TIME),
                ..shown(&names, 0)
            },
        );
        assert_eq!(frame.rows, 3);
        assert_eq!(frame.cells[0].character, LEFT_CAP);
        assert_eq!(frame.cursor, Some((2, 2)));
        assert!(!text(&frame, 0).contains("Mon"));
    }

    #[test]
    fn an_unfocused_window_goes_quiet() {
        let names = ["a", "b"];
        let mut frame = pane(2, 60);
        let (at, _) = (60 * 3 + 2, 0);
        draw(
            &mut frame,
            &Bar {
                focused: false,
                time: Some(TIME),
                ..shown(&names, 0)
            },
        );
        assert_eq!(frame.cells[1].foreground, UNFOCUSED_TITLE);
        assert_eq!(frame.cells[43].foreground, UNFOCUSED_TITLE);
        assert_eq!(frame.cells[57].foreground, UNFOCUSED);
        let row = strip_of(&frame);
        assert_eq!(row[0].foreground, HOVER | TALL);
        assert_eq!(
            (row[2].foreground, row[2].background),
            (HOVER_TEXT | BOLD, HOVER)
        );
        assert_eq!(frame.cursor, Some((3, 2)));
        assert_eq!(
            frame.cells[at].foreground, UNFOCUSED,
            "the cursor inverts this"
        );
    }

    #[test]
    fn a_press_lands_on_what_the_strip_drew() {
        let names = ["one", "two"];
        let (first, cells) = tab_of(&names, 0, 60);
        let (second, _) = tab_of(&names, 1, 60);
        let at = |column: usize| hit(column as u16, &names, 0, 60);
        assert_eq!(at(first), Some(Hit::Tab(0)));
        assert_eq!(at(first + cells - 1), Some(Hit::Tab(0)));
        assert_eq!(at(first + cells), None, "the gap");
        assert_eq!(at(second), Some(Hit::Tab(1)));
        let button = second + cells + 1;
        assert_eq!(at(button - 1), None, "the gap");
        for column in button..button + NEW_TAB {
            assert_eq!(at(column), Some(Hit::NewTab));
        }
        assert_eq!(at(button + NEW_TAB), None, "past the button");
    }

    #[test]
    fn labels_shrink_before_tabs_do() {
        let long = "a".repeat(24);
        let names = [long.as_str(), long.as_str()];
        let frame = tabs(40, &names, 0);
        let drawn = text(&frame, 1);
        assert!(drawn.contains('…'), "{drawn}");
        assert!(drawn.contains('+'), "{drawn}");
        let button = (0..40).any(|column| hit(column, &names, 0, 40) == Some(Hit::NewTab));
        assert!(button);
    }

    #[test]
    fn a_strip_too_narrow_for_its_tabs_still_has_a_button() {
        let names = vec!["a"; MAX_TABS];
        assert_eq!(hit(18, &names, 0, 70), Some(Hit::Tab(9)));
        let button = (0..70).find(|&column| hit(column, &names, 0, 70) == Some(Hit::NewTab));
        assert!(button.is_some());
        assert_eq!(hit(0, &names, 0, 20), Some(Hit::NewTab));
    }

    #[test]
    fn only_nine_tabs_have_a_key() {
        let names = vec!["a"; 10];
        let frame = tabs(200, &names, 0);
        let drawn = text(&frame, 1);
        assert!(drawn.contains(&key(9)), "{drawn}");
        assert!(!drawn.contains(&key(10)), "{drawn}");
    }

    #[test]
    fn a_long_label_ends_in_an_ellipsis_inside_its_tab() {
        let long = "a".repeat(24);
        let strip = text(&tabs(40, &[&long, "b"], 1), 1);
        assert!(strip.contains('…'), "{strip}");
        assert!(strip.contains(&key(1)), "{strip}");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abcd", 4), "abcd");
        assert_eq!(fit("漢字漢", 4), "漢…");
        assert_eq!(fit("abc", 0), "");
    }

    #[test]
    fn only_the_title_row_moves_the_window() {
        let padding = Padding::default();
        assert!(on_title(padding, 19.0, 20));
        assert!(!on_title(padding, 20.0, 20));
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
        assert!(bar(40, "x", Some(time)).1.ends_with("Sun 31 Dec  00:05"));
        let bad = LocalTime {
            weekday: 9,
            month: 99,
            ..TIME
        };
        assert!(bar(40, "x", Some(bad)).1.ends_with(" 02   09:41"));
    }

    #[test]
    fn the_pane_moves_down_under_the_bar() {
        let mut frame = pane(2, 10);
        frame.cells[0].character = 'a';
        draw(&mut frame, &shown(&["b"], 0));
        assert_eq!(frame.rows, 4);
        assert_eq!(frame.cells.len(), 40);
        assert_eq!(frame.cells[20].character, 'a');
        assert_eq!(frame.cursor, Some((3, 2)));
    }

    #[test]
    fn a_frame_with_nothing_yet_is_safe() {
        let mut frame = Frame::default();
        draw(
            &mut frame,
            &Bar {
                time: Some(TIME),
                inset: 8,
                ..shown(&["shell"], 0)
            },
        );
        assert!(frame.cells.is_empty());
        draw(
            &mut frame,
            &Bar {
                time: Some(TIME),
                inset: 8,
                ..shown(&[], 3)
            },
        );
        assert!(frame.cells.is_empty());
    }

    #[test]
    fn a_narrow_window_drops_the_date_then_the_time() {
        assert!(bar(26, "folder", Some(TIME)).1.contains("Mon 02 Nov"));
        assert_eq!(bar(25, "folder", Some(TIME)).1, " folder             09:41");
        assert_eq!(bar(14, "folder", Some(TIME)).1, " folder  09:41");
        assert_eq!(bar(13, "folder", Some(TIME)).1, " folder      ");
        assert_eq!(bar(4, "folder", Some(TIME)).1, " fol");
    }

    #[test]
    fn any_width_and_inset_is_safe() {
        for columns in 0..24 {
            for count in 0..12 {
                let names = vec!["漢字 label"; count];
                for active in [0, 1, 99] {
                    let mut frame = pane(1, columns);
                    let added = 2 * usize::from(columns);
                    let before = frame.cells.len();
                    let bar = Bar {
                        activity: u32::MAX,
                        hovered: Some(1),
                        time: Some(TIME),
                        inset: 9,
                        ..shown(&names, active)
                    };
                    draw(&mut frame, &bar);
                    assert_eq!(frame.cells.len(), before + added);
                    for column in 0..columns {
                        hit(column, &names, u32::MAX, columns);
                    }
                }
            }
        }
    }

    #[test]
    fn a_wide_character_never_splits_at_the_edge() {
        assert_eq!(bar(4, "漢字", None).1, " 漢  ");
        assert_eq!(bar(3, "漢字", None).1, " 漢 ");
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
        assert!(drawn.ends_with("Mon 02 Nov  09:41"), "{drawn}");
        assert!(!drawn.contains('◆'));
        let bar = &frame.cells[..160];
        assert!(
            bar[1..1 + text.len()]
                .iter()
                .all(|cell| cell.foreground == TITLE)
        );
        assert!(bar.iter().all(|cell| {
            let colours = [cell.foreground & 0x00ff_ffff, cell.background];
            !STATE_COLOURS.iter().any(|state| colours.contains(state))
        }));
    }

    #[test]
    fn the_inset_is_scaled_points_in_whole_cells() {
        const LEFT_MAC: f64 = 86.0;
        assert_eq!(inset(LEFT_MAC, 2.0, 0, 16), 11);
        assert_eq!(inset(LEFT_MAC, 1.0, 0, 8), 11);
        assert_eq!(inset(0.0, 2.0, 0, 16), 1);
        assert_eq!(inset(0.0, 2.0, 0, 0), 1);
        assert_eq!(inset(LEFT_MAC, 2.0, 40, 16), 9);
        assert_eq!(inset(0.0, 2.0, 28, 16), 1);
    }

    #[test]
    fn the_bar_is_the_first_rows_of_pixels() {
        assert_eq!(locate(0.0, 0.0, CELL, GRID), (true, (0, 0)));
        assert_eq!(locate(35.0, 39.9, CELL, GRID), (true, (0, 3)));
        assert_eq!(locate(35.0, 40.0, CELL, GRID), (false, (0, 3)));
        assert_eq!(locate(35.0, 65.0, CELL, GRID), (false, (1, 3)));
    }

    #[test]
    fn a_pointer_off_the_grid_takes_the_nearest_cell() {
        assert_eq!(locate(-5.0, -5.0, CELL, GRID), (true, (0, 0)));
        assert_eq!(locate(900.0, 900.0, CELL, GRID), (false, (4, 7)));
        assert_eq!(locate(900.0, 900.0, CELL, (0, 0)), (false, (0, 0)));
    }

    #[test]
    fn mouse_reports_name_the_pane_cell_not_the_window_row() {
        let (on_bar, cell) = locate(35.0, 65.0, CELL, GRID);
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
        let (_, (row, column)) = locate(25.0, 85.0, CELL, GRID);
        let at = Position {
            row: 100 + usize::from(row),
            column,
        };
        let mut frame = pane(5, 8);
        let selected = Selection {
            anchor: at,
            focus: at,
        };
        let rect = pinfold_proto::link::Rect {
            x: 0,
            y: 0,
            columns: frame.columns,
            rows: frame.rows,
        };
        selection::overlay(&mut frame, rect, 100, selected);
        draw(&mut frame, &shown(&["x"], 0));
        let inverted: Vec<usize> = (0..frame.cells.len())
            .filter(|&index| frame.cells[index].foreground == 2)
            .collect();
        assert_eq!(inverted, [4 * 8 + 2]);
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
