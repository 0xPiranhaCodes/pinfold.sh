//! The inbox: every agent pane of the session in one view, ◆ needs you first (ADR-0051).
//!
//! Prefix `!` or the palette's "Agents" opens it. Like the palette it is drawn after the panes
//! and takes every key while open, so program output can neither fake nor answer it. Each row
//! is the state's glyph and word in its colour, `tab·pane`, the tab's label, the agent's message
//! and the time in the state, which ticks while the inbox is open. Its rows live only while it
//! is open: closing drops the whole value.

use std::time::{Duration, Instant};

use pinfold_config::theme::Theme;
use pinfold_proto::{Cell, Frame};
use unicode_width::UnicodeWidthChar;
use winit::keyboard::{Key, NamedKey};

use crate::link::Listed;
use crate::top_bar::{self, STATES};

/// The widest the inbox draws, border included.
const WIDTH: u16 = 100;
/// The narrowest, unless the window is narrower.
const NARROWEST: u16 = 40;
/// Rows of chrome around the list: the top border, the rule, the footer and the bottom border.
const CHROME: u16 = 4;
/// The widest a label column gets, as `top_bar::label` cuts labels to it.
const LABEL: usize = 16;
/// A message narrower than this is dropped rather than cut to nothing.
const MESSAGE: usize = 8;
/// Cells of the time column: `99h+` and `59:59` fit.
const TIME: usize = 5;
/// The state's word, padded to the longest, "needs you".
const WORD: usize = 9;
/// Each state by its number on the link, in the order rows take: ◆ ✕ ● ✓ ○.
const ORDER: [u8; 5] = [0, 3, 1, 2, 4];

/// What a key did to the inbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// It stays open.
    Stay,
    /// It closes.
    Close,
    /// It closes, and the window goes to this pane in the tab at this index.
    Jump {
        /// The tab's index.
        tab: u8,
        /// The pane.
        pane: u32,
    },
}

/// One listed pane, as a row shows it.
#[derive(Debug, Default)]
struct Row {
    pane: u32,
    state: u8,
    reported: bool,
    tab: u8,
    /// The pane's place in its tab, from 1.
    place: u8,
    since: Option<Instant>,
    label: String,
    /// The agent's message, already safe to draw.
    message: String,
}

/// An open inbox.
#[derive(Debug, Default)]
pub struct Inbox {
    rows: Vec<Row>,
    /// Index into `rows`.
    selected: usize,
    /// Whether rows came, so it does not flash "No agent panes" before they do.
    ready: bool,
}

impl Inbox {
    /// Rebuilds the rows from the window's latest: each listed pane, its tab from `members`,
    /// and that tab's label from `labels`. The selection stays on its pane. Row buffers are
    /// reused, so a refresh while open allocates only for longer text.
    pub fn refresh(&mut self, listed: &[Listed], members: &[(u32, u8)], labels: &[String]) {
        self.ready = true;
        let chosen = self.rows.get(self.selected).map(|row| row.pane);
        let placed = listed.iter().filter_map(|listed| {
            let (index, &(_, tab)) =
                (members.iter().enumerate()).find(|(_, (pane, _))| *pane == listed.pane)?;
            let place = members[..index].iter().filter(|(_, t)| *t == tab).count() + 1;
            Some((listed, tab, place))
        });
        let mut count = 0;
        for (listed, tab, place) in placed {
            if count == self.rows.len() {
                self.rows.push(Row::default());
            }
            let row = &mut self.rows[count];
            count += 1;
            (row.pane, row.state, row.reported) = (listed.pane, listed.state, listed.reported);
            (row.tab, row.place, row.since) = (tab, place.min(99) as u8, Some(listed.since));
            row.label.clear();
            row.label
                .push_str(labels.get(usize::from(tab)).map_or("", String::as_str));
            row.message.clear();
            // The core cleaned it; this keeps state glyphs and the like out of PinFold's rows.
            row.message
                .extend(listed.message.chars().filter(|&c| top_bar::safe(c)));
        }
        self.rows.truncate(count);
        let rank = |state: u8| {
            ORDER
                .iter()
                .position(|&s| s == state)
                .unwrap_or(ORDER.len())
        };
        self.rows
            .sort_by_key(|row| (rank(row.state), row.since, row.tab, row.place));
        let kept = self.rows.iter().position(|row| Some(row.pane) == chosen);
        self.selected = kept.unwrap_or(self.selected.min(count.saturating_sub(1)));
    }

    /// Returns when a shown time next changes: at most once a minute, as times show no
    /// seconds, so the window redraws for the clock that rarely (ADR-0051 §4).
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        self.rows
            .iter()
            .filter_map(|row| {
                let since = row.since?;
                let seconds = now.saturating_duration_since(since).as_secs();
                (seconds < CAPPED).then(|| since + Duration::from_secs((seconds / 60 + 1) * 60))
            })
            .min()
    }

    /// Returns whether rows have come since it opened.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Takes a key press. Keys it has no use for are swallowed, as the inbox holds the
    /// keyboard.
    pub fn key(&mut self, key: &Key) -> Step {
        let last = self.rows.len().saturating_sub(1);
        match key {
            Key::Named(NamedKey::Escape) => return Step::Close,
            Key::Named(NamedKey::Enter) => {
                if let Some(row) = self.rows.get(self.selected) {
                    return Step::Jump {
                        tab: row.tab,
                        pane: row.pane,
                    };
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.selected = self.selected.checked_sub(1).unwrap_or(last);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.selected = if self.selected >= last {
                    0
                } else {
                    self.selected + 1
                };
            }
            Key::Named(NamedKey::Home) => self.selected = 0,
            Key::Named(NamedKey::End) => self.selected = last,
            _ => {}
        }
        Step::Stay
    }

    /// Takes a press at a cell of the panes' grid, `rows` by `columns`: a row jumps to its
    /// pane, and anywhere else closes.
    pub fn click(&self, rows: u16, columns: u16, (row, column): (u16, u16)) -> Step {
        let Some(area) = self.area(rows, columns) else {
            return Step::Close;
        };
        let inside = (area.left + 1..area.left + area.width - 1).contains(&column);
        let line = usize::from(row.wrapping_sub(area.top + 1));
        match self.rows.get(area.first + line) {
            Some(row) if inside && line < usize::from(area.lines) => Step::Jump {
                tab: row.tab,
                pane: row.pane,
            },
            _ => Step::Close,
        }
    }

    /// Where the inbox goes in a grid of `rows` by `columns`, or nothing if it cannot fit.
    fn area(&self, rows: u16, columns: u16) -> Option<Area> {
        let width = columns
            .saturating_sub(4)
            .clamp(NARROWEST, WIDTH)
            .min(columns);
        let tallest = rows.saturating_sub(4).max(CHROME + 2).min(rows);
        if width < 24 || tallest <= CHROME {
            return None;
        }
        let lines = (self.rows.len().max(1) as u16).min(tallest - CHROME);
        let height = lines + CHROME;
        let first = self.selected.saturating_sub(usize::from(lines) - 1);
        Some(Area {
            top: (rows - height) / 3,
            left: (columns - width) / 2,
            width,
            lines,
            first,
        })
    }

    /// Draws the inbox over the panes' grid in `frame`, times as of `now`.
    pub fn draw(&self, frame: &mut Frame, theme: &Theme, now: Instant) {
        let Some(area) = self.area(frame.rows, frame.columns).filter(|_| self.ready) else {
            return;
        };
        let chrome = theme.chrome;
        let (ground, dim, text) = (chrome.palette, chrome.dim, theme.foreground);
        let (top, left, right) = (area.top, area.left, area.left + area.width - 1);
        let rule = top + area.lines + 1;
        let bottom = rule + 2;
        for row in top..=bottom {
            let edge = row == top || row == rule || row == bottom;
            for column in left..=right {
                let character = match (column == left, column == right) {
                    (true, _) if row == top => '┌',
                    (_, true) if row == top => '┐',
                    (true, _) if row == rule => '├',
                    (_, true) if row == rule => '┤',
                    (true, _) if row == bottom => '└',
                    (_, true) if row == bottom => '┘',
                    _ if edge => '─',
                    (true, _) | (_, true) => '│',
                    _ => ' ',
                };
                put(frame, row, column, character, chrome.border, ground);
            }
        }
        // The title sits in the top border, and the close key on its right.
        let end = usize::from(right);
        let at = write(
            frame,
            top,
            usize::from(left) + 2,
            " AGENTS",
            chrome.accent,
            ground,
            end,
        );
        write(frame, top, at, " · needs you first ", dim, ground, end);
        write(frame, top, end - 6, " esc ", dim, ground, end);

        let columns = Columns::fit(usize::from(area.width) - 2, &self.rows);
        let visible = self.rows.iter().enumerate().skip(area.first);
        for (line, (index, row)) in visible.take(area.lines.into()).enumerate() {
            let at = top + 1 + line as u16;
            self.draw_row(frame, theme, (at, left, right), &columns, (index, row), now);
        }
        if self.rows.is_empty() {
            write(
                frame,
                top + 1,
                usize::from(left) + 3,
                "No agent panes",
                dim,
                ground,
                end,
            );
        }
        // A thumb on the right border shows where the view is in a list that scrolls.
        let (shown, total) = (usize::from(area.lines), self.rows.len());
        if total > shown {
            let thumb = (shown * shown).div_ceil(total).max(1);
            let start = area.first * shown / total;
            for line in start..(start + thumb).min(shown) {
                put(
                    frame,
                    top + 1 + line as u16,
                    right,
                    '┃',
                    chrome.accent,
                    ground,
                );
            }
        }

        let footer = rule + 1;
        let waiting = self.rows.iter().filter(|row| row.state == 0).count();
        let agents = if total == 1 { "agent" } else { "agents" };
        let at = usize::from(left) + 2;
        let at = write(
            frame,
            footer,
            at,
            &format!("{total} {agents} · "),
            dim,
            ground,
            end,
        );
        let amber = if waiting > 0 {
            theme.states.needs_you
        } else {
            dim
        };
        write(
            frame,
            footer,
            at,
            &format!("{waiting} needs you"),
            amber,
            ground,
            end,
        );
        let keys: [(&str, &str); 3] = [("↑↓", " move  "), ("⏎", " jump  "), ("esc", " close")];
        let width: usize = keys
            .iter()
            .map(|(key, word)| cells(key) + cells(word))
            .sum();
        let mut at = (end - 1).saturating_sub(width);
        if at > usize::from(left) + 28 {
            for (key, word) in keys {
                at = write(frame, footer, at, key, text, ground, end);
                at = write(frame, footer, at, word, dim, ground, end);
            }
        }
    }

    fn draw_row(
        &self,
        frame: &mut Frame,
        theme: &Theme,
        (at, left, right): (u16, u16, u16),
        columns: &Columns,
        (index, row): (usize, &Row),
        now: Instant,
    ) {
        let chrome = theme.chrome;
        let chosen = index == self.selected;
        let ground = if chosen {
            chrome.tab_active
        } else {
            chrome.palette
        };
        for column in left + 1..right {
            put(frame, at, column, ' ', theme.foreground, ground);
        }
        let end = usize::from(right) - 1;
        if chosen {
            put(frame, at, left + 1, '▌', chrome.accent, ground);
        }
        let state = usize::from(row.state).min(STATES.len() - 1);
        let (glyph, word) = STATES[state];
        let colour = top_bar::state_colour(theme, state as u8).unwrap_or(theme.foreground);
        let mut column = usize::from(left) + 3;
        put(frame, at, column as u16, glyph, colour, ground);
        write(frame, at, column + 2, word, colour, ground, end);
        column += 2 + WORD + 2;
        let place = format!("{}·{}", u16::from(row.tab) + 1, row.place);
        write(frame, at, column, &place, chrome.dim, ground, end);
        column += columns.place + 2;
        if columns.label > 0 {
            cut(
                frame,
                at,
                column,
                &row.label,
                theme.foreground,
                ground,
                columns.label,
            );
            column += columns.label + 2;
        }
        if columns.message > 0 {
            let (message, colour) = if row.reported {
                (row.message.as_str(), theme.foreground)
            } else {
                ("from output", chrome.dim)
            };
            cut(frame, at, column, message, colour, ground, columns.message);
        }
        let held = row
            .since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let time = held_for(held);
        let colour = if row.state == 0 {
            theme.states.needs_you
        } else {
            chrome.dim
        };
        write(frame, at, end - cells(&time), &time, colour, ground, end);
    }
}

/// The inbox's place in the grid.
struct Area {
    top: u16,
    left: u16,
    width: u16,
    /// Rows of the list shown.
    lines: u16,
    /// The first row shown.
    first: usize,
}

/// How wide each column of a row is, in cells; 0 drops it.
struct Columns {
    place: usize,
    label: usize,
    message: usize,
}

impl Columns {
    /// Fits the columns to `inner` cells between the borders: narrow windows drop the message,
    /// then the label.
    fn fit(inner: usize, rows: &[Row]) -> Self {
        let widest =
            |text: fn(&Row) -> usize, at_least| rows.iter().map(text).fold(at_least, usize::max);
        let place = widest(
            |row| cells(&format!("{}·{}", u16::from(row.tab) + 1, row.place)),
            3,
        );
        let label = widest(|row| cells(&row.label), 0).min(LABEL);
        // The focus bar, a space, the glyph, a space, the word, then the place and the time.
        let fixed = 3 + 2 + WORD + 2 + place + 2 + TIME + 1;
        let room = inner.saturating_sub(fixed);
        let label = if label > 0 && room >= label + 2 {
            label
        } else {
            0
        };
        let room = room.saturating_sub(if label > 0 { label + 2 } else { 0 });
        let message = if room >= MESSAGE + 2 { room - 2 } else { 0 };
        Self {
            place,
            label,
            message,
        }
    }
}

/// Seconds from which a time reads `99h+` and stops changing.
const CAPPED: u64 = 100 * 3600;

/// Time in a state: `<1m` under a minute, then `6m`, then `3h07`, then `99h+`.
fn held_for(held: Duration) -> String {
    let seconds = held.as_secs();
    let (hours, minutes) = (seconds / 3600, seconds / 60 % 60);
    match hours {
        0 if minutes == 0 => "<1m".to_owned(),
        0 => format!("{minutes}m"),
        1..=99 => format!("{hours}h{minutes:02}"),
        _ => "99h+".to_owned(),
    }
}

/// Cells `text` takes.
fn cells(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Writes `text` in at most `width` cells from `column`, ending in `…` if it is cut.
fn cut(
    frame: &mut Frame,
    row: u16,
    column: usize,
    text: &str,
    colour: u32,
    ground: u32,
    width: usize,
) {
    if cells(text) <= width {
        write(frame, row, column, text, colour, ground, column + width);
        return;
    }
    let at = write(frame, row, column, text, colour, ground, column + width - 1);
    put(frame, row, at as u16, '…', colour, ground);
}

fn put(
    frame: &mut Frame,
    row: u16,
    column: u16,
    character: char,
    foreground: u32,
    background: u32,
) {
    if row >= frame.rows || column >= frame.columns {
        return;
    }
    let index = usize::from(row) * usize::from(frame.columns) + usize::from(column);
    if let Some(cell) = frame.cells.get_mut(index) {
        *cell = Cell {
            character,
            foreground,
            background,
        };
    }
}

/// Writes `text` in row `row` from `column`, stopping before `end`, and returns the column
/// after it. Zero-width characters are dropped, and a wide one takes two cells.
fn write(
    frame: &mut Frame,
    row: u16,
    mut column: usize,
    text: &str,
    colour: u32,
    ground: u32,
    end: usize,
) -> usize {
    for character in text.chars() {
        let width = character.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if column + width > end {
            break;
        }
        put(frame, row, column as u16, character, colour, ground);
        if width == 2 {
            put(frame, row, column as u16 + 1, ' ', colour, ground);
        }
        column += width;
    }
    column
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pinfold_config::theme::GRAPHITE;

    /// A listed pane that came to `state` `ago` seconds before `now`.
    pub(crate) fn listed(pane: u32, state: u8, ago: u64, message: &str, now: Instant) -> Listed {
        Listed {
            pane,
            state,
            reported: true,
            since: now - Duration::from_secs(ago),
            message: message.to_owned(),
        }
    }

    /// Five agents over three tabs, as the Agents board draws them.
    pub(crate) fn board(now: Instant) -> Inbox {
        let listed = [
            listed(5, 1, 363, "Writing guides/limits.md", now),
            listed(9, 2, 30, "Turn complete · terraform plan clean", now),
            listed(4, 3, 70, "exit 101 · 1 failed", now),
            listed(2, 0, 42, "Allow edit to src/config.rs?", now),
            listed(6, 1, 1335, "npm run dev · watching", now),
        ];
        let members = [(2, 0), (4, 0), (5, 1), (6, 1), (9, 2), (11, 2)];
        let labels = ["api-refactor", "docs-site", "infra"].map(str::to_owned);
        let mut inbox = Inbox::default();
        inbox.refresh(&listed, &members, &labels);
        inbox
    }

    pub(crate) fn drawn(inbox: &Inbox, rows: u16, columns: u16, now: Instant) -> Frame {
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, rows, columns);
        inbox.draw(&mut frame, &GRAPHITE, now);
        frame
    }

    fn text(frame: &Frame, row: u16) -> String {
        let columns = usize::from(frame.columns);
        let start = usize::from(row) * columns;
        frame.cells[start..start + columns]
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    /// Each drawn line that holds a row, trimmed.
    fn lines(frame: &Frame) -> Vec<String> {
        (0..frame.rows)
            .map(|row| text(frame, row))
            .filter(|line| STATES.iter().any(|(glyph, _)| line.contains(*glyph)))
            .map(|line| line.trim_matches([' ', '│', '┃']).to_owned())
            .collect()
    }

    #[test]
    fn rows_go_needs_you_error_running_done_idle_and_longest_held_first() {
        let now = Instant::now();
        let inbox = board(now);
        let order: Vec<_> = inbox.rows.iter().map(|row| (row.pane, row.state)).collect();
        assert_eq!(order, [(2, 0), (4, 3), (6, 1), (5, 1), (9, 2)]);
        let places: Vec<_> = inbox.rows.iter().map(|row| (row.tab, row.place)).collect();
        assert_eq!(places, [(0, 1), (0, 2), (1, 2), (1, 1), (2, 1)]);
    }

    #[test]
    fn every_row_has_its_glyph_colour_and_word() {
        let now = Instant::now();
        let mut inbox = board(now);
        let mut six = board_listed(now);
        six.push(listed(1, 4, 5, "", now));
        let members: Vec<_> = six.iter().map(|listed| (listed.pane, 0)).collect();
        inbox.refresh(&six, &members, &["tab".to_owned()]);
        let frame = drawn(&inbox, 24, 100, now);
        let colours = GRAPHITE.states;
        let expected = [
            ('◆', "needs you", colours.needs_you),
            ('✕', "error", colours.error),
            ('●', "running", colours.running),
            ('●', "running", colours.running),
            ('✓', "done", colours.done),
            ('○', "idle", colours.idle),
        ];
        let columns = usize::from(frame.columns);
        let mut found = 0;
        for row in 0..frame.rows {
            let line = &frame.cells[usize::from(row) * columns..][..columns];
            let Some(at) = line
                .iter()
                .position(|cell| STATES.iter().any(|s| s.0 == cell.character))
            else {
                continue;
            };
            let (glyph, word, colour) = expected[found];
            assert_eq!((line[at].character, line[at].foreground), (glyph, colour));
            let drawn: String = line[at + 2..at + 2 + word.len()]
                .iter()
                .map(|c| c.character)
                .collect();
            assert_eq!(drawn, word);
            assert!(
                line[at + 2..at + 2 + word.len()]
                    .iter()
                    .all(|c| c.foreground == colour)
            );
            found += 1;
        }
        assert_eq!(found, expected.len());
    }

    fn board_listed(now: Instant) -> Vec<Listed> {
        vec![
            listed(5, 1, 363, "Writing guides/limits.md", now),
            listed(9, 2, 30, "Turn complete", now),
            listed(4, 3, 70, "exit 101", now),
            listed(2, 0, 42, "Allow edit?", now),
            listed(6, 1, 1335, "npm run dev", now),
        ]
    }

    #[test]
    fn a_state_from_output_says_so_in_place_of_a_message() {
        let now = Instant::now();
        let mut from_output = listed(3, 0, 2, "Allow everything?", now);
        from_output.reported = false;
        let mut inbox = Inbox::default();
        inbox.refresh(&[from_output], &[(3, 0)], &["api".to_owned()]);
        let frame = drawn(&inbox, 12, 80, now);
        let line = &lines(&frame)[0];
        assert!(
            line.contains("from output") && !line.contains("Allow"),
            "{line}"
        );
    }

    #[test]
    fn a_hostile_label_or_message_is_drawn_safe() {
        let now = Instant::now();
        let message = "\u{1b}[31m◆ needs you\u{202e}fake ✓ done\u{7}";
        let mut label = String::new();
        top_bar::label("/home/me/\u{1b}]0;✕ error\u{7}evil", &mut label);
        let mut inbox = Inbox::default();
        inbox.refresh(&[listed(3, 1, 2, message, now)], &[(3, 0)], &[label]);
        let frame = drawn(&inbox, 12, 100, now);
        let line = &lines(&frame)[0];
        // One glyph, the row's own, and no control or bidi character anywhere.
        let glyphs = line
            .chars()
            .filter(|c| STATES.iter().any(|s| s.0 == *c))
            .count();
        assert_eq!(glyphs, 1, "{line}");
        assert!(line.starts_with("▌ ● running"), "{line}");
        assert!(frame.cells.iter().all(|cell| !cell.character.is_control()));
        assert!(!line.contains('\u{202e}'));
    }

    #[test]
    fn sixty_four_rows_scroll_with_the_selection_in_view() {
        let now = Instant::now();
        let listed: Vec<_> = (0..64)
            .map(|pane| listed(pane, 1, u64::from(64 - pane), "", now))
            .collect();
        let members: Vec<_> = (0..64).map(|pane| (pane, (pane % 32) as u8)).collect();
        let mut inbox = Inbox::default();
        inbox.refresh(&listed, &members, &[]);
        let frame = drawn(&inbox, 20, 80, now);
        // The window's rows less 4, less the inbox's chrome.
        assert_eq!(lines(&frame).len(), 12);
        assert!(lines(&frame)[0].contains("1·1"));
        inbox.key(&Key::Named(NamedKey::ArrowUp));
        let frame = drawn(&inbox, 20, 80, now);
        let shown = lines(&frame);
        assert_eq!(shown.len(), 12);
        assert!(
            shown[11].starts_with("▌") && shown[11].contains("32·2"),
            "{shown:?}"
        );
        assert!(text(&frame, 15).contains("64 agents · 0 needs you"));
        assert!(
            frame.cells.iter().any(|cell| cell.character == '┃'),
            "a scroll thumb"
        );
        assert_eq!(
            inbox.key(&Key::Named(NamedKey::Enter)),
            Step::Jump { tab: 31, pane: 63 }
        );
    }

    #[test]
    fn a_narrow_window_drops_the_message_then_the_label() {
        let now = Instant::now();
        let inbox = board(now);
        let wide = lines(&drawn(&inbox, 24, 104, now));
        assert!(
            wide[0].contains("api-refactor") && wide[0].contains("Allow edit"),
            "{wide:?}"
        );
        let middle = lines(&drawn(&inbox, 24, 52, now));
        assert!(
            middle[0].contains("api-refactor") && !middle[0].contains("Allow"),
            "{middle:?}"
        );
        let narrow = lines(&drawn(&inbox, 24, 40, now));
        assert!(
            !narrow[0].contains("api-refactor") && narrow[0].contains("<1m"),
            "{narrow:?}"
        );
        assert!(narrow[0].contains("◆ needs you"), "the word always stays");
        let tiny = drawn(&inbox, 4, 20, now);
        assert!(lines(&tiny).is_empty(), "too small to draw");
    }

    #[test]
    fn nothing_listed_says_so_and_enter_does_nothing() {
        let mut inbox = Inbox::default();
        inbox.refresh(&[], &[], &[]);
        let frame = drawn(&inbox, 12, 60, Instant::now());
        assert!((0..12).any(|row| text(&frame, row).contains("No agent panes")));
        assert!((0..12).any(|row| text(&frame, row).contains("0 agents · 0 needs you")));
        assert_eq!(inbox.key(&Key::Named(NamedKey::Enter)), Step::Stay);
        assert_eq!(inbox.key(&Key::Named(NamedKey::Escape)), Step::Close);
    }

    #[test]
    fn the_selection_stays_on_its_pane_as_rows_move() {
        let now = Instant::now();
        let mut inbox = board(now);
        inbox.key(&Key::Named(NamedKey::ArrowDown));
        assert_eq!(inbox.rows[inbox.selected].pane, 4);
        let mut listed = board_listed(now);
        listed[3].state = 2;
        let members = [(2, 0), (4, 0), (5, 1), (6, 1), (9, 2)];
        inbox.refresh(&listed, &members, &[]);
        assert_eq!(inbox.rows[inbox.selected].pane, 4);
        assert_eq!(
            inbox.key(&Key::Named(NamedKey::Enter)),
            Step::Jump { tab: 0, pane: 4 }
        );
    }

    #[test]
    fn a_click_on_a_row_jumps_and_anywhere_else_closes() {
        let now = Instant::now();
        let inbox = board(now);
        let frame = drawn(&inbox, 24, 100, now);
        let row = (0..24)
            .find(|&row| text(&frame, row).contains("✕"))
            .unwrap_or(0);
        assert_eq!(
            inbox.click(24, 100, (row, 50)),
            Step::Jump { tab: 0, pane: 4 }
        );
        assert_eq!(inbox.click(24, 100, (0, 0)), Step::Close);
        assert_eq!(inbox.click(24, 100, (row, 1)), Step::Close);
    }

    #[test]
    fn times_read_minutes_then_hours_then_cap() {
        let held = |seconds| held_for(Duration::from_secs(seconds));
        assert_eq!(
            [held(0), held(59), held(60), held(363), held(3599)],
            ["<1m", "<1m", "1m", "6m", "59m"]
        );
        assert_eq!(
            [held(3600), held(11_220), held(359_999)],
            ["1h00", "3h07", "99h59"]
        );
        assert_eq!(held(360_000), "99h+");
    }

    #[test]
    fn the_time_ticks_with_the_clock() {
        let now = Instant::now();
        let inbox = board(now);
        let later = now + Duration::from_secs(20);
        assert!(lines(&drawn(&inbox, 24, 100, now))[0].ends_with("<1m"));
        assert!(lines(&drawn(&inbox, 24, 100, later))[0].ends_with("1m"));
    }

    #[test]
    fn the_clock_wakes_the_inbox_only_when_a_shown_time_changes() {
        let now = Instant::now();
        // Held 363, 30, 70, 42 and 1335 s: the 42 s row turns 1m first.
        let mut inbox = board(now);
        let next = inbox
            .next_change(now)
            .map(|at| at.duration_since(now).as_secs());
        assert_eq!(next, Some(18));
        inbox.refresh(&[listed(2, 0, 400_000, "", now)], &[(2, 0)], &[]);
        assert_eq!(inbox.next_change(now), None, "99h+ never changes");
        inbox.refresh(&[], &[], &[]);
        assert_eq!(inbox.next_change(now), None);
    }

    /// CLAUDE.md asks for glyph, colour and word, and the board is the design's view of this.
    #[test]
    fn the_agents_board_shows_the_word_beside_each_glyph() {
        let board = include_str!("../../../docs/design/ui/Agents.dc.html");
        let mut rows = 0;
        for (glyph, word) in STATES {
            for (at, _) in board.match_indices(&format!(">{glyph}</span>")) {
                let after = &board[at..board.len().min(at + 120)];
                assert!(
                    after.contains(&format!(">{word}</span>")),
                    "{glyph}: {after}"
                );
                rows += 1;
            }
        }
        assert_eq!(rows, 5);
    }
}
