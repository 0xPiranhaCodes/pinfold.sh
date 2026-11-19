//! The inbox: every agent pane of the session in one view, ◆ needs you first (ADR-0051).
//!
//! Prefix `!` or the palette's "Agents" opens it. Like the palette it is drawn after the panes
//! and takes every key while open, so program output can neither fake nor answer it. Each row
//! is the state's glyph and word in its colour, `tab·pane`, the tab's label, the git branch, the
//! agent's message, the run time and the time in the state, which tick while the inbox is open.
//! With `[inbox] agent_files` on, a second line gives the agent's context use and last steps,
//! and Claude Code sessions running outside PinFold follow PinFold's own rows.
//! Its rows live only while it is open: closing drops the whole value.

use std::time::{Duration, Instant};

use pinfold_config::theme::Theme;
use pinfold_proto::link::Choice;
use pinfold_proto::{Cell, Frame};
use unicode_width::UnicodeWidthChar;
use winit::keyboard::{Key, NamedKey};

use crate::approval::visible;
use crate::link::{Listed, Outside};
use crate::top_bar::{self, STATES};

/// The widest the inbox draws, border included.
const WIDTH: u16 = 100;
/// The narrowest, unless the window is narrower.
const NARROWEST: u16 = 40;
/// Rows of chrome around the list: the top border, the rule, the footer and the bottom border.
const CHROME: u16 = 4;
/// The widest a label column gets, as `top_bar::label` cuts labels to it.
const LABEL: usize = 16;
/// The widest a branch column gets; a longer branch is cut with `…`.
const BRANCH: usize = 24;
/// The fewest cells between the borders that keep a row's second line; narrower windows drop it
/// before the message (ADR-0051 §4).
const SECOND: usize = 60;
/// A message narrower than this is dropped rather than cut to nothing.
const MESSAGE: usize = 8;
/// Cells of the time column: `99h+` and `59:59` fit.
const TIME: usize = 5;
/// The state's word, padded to the longest, "needs you".
const WORD: usize = 9;
/// Each state by its number on the link, in the order rows take: ◆ ✕ ● ✓ ○.
const ORDER: [u8; 5] = [0, 3, 1, 2, 4];
/// How recently an outside agent's file changed for its row to say ● running (ADR-0051 §8).
const RUNNING: Duration = Duration::from_secs(30);
/// What an outside row says in place of `tab·pane`, and the agent's name in PinFold's words.
const OUTSIDE: &str = "outside PinFold";
const CLAUDE: &str = "Claude Code";

/// What a key did to the inbox.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// It stays open, and the window asks the person to confirm Approve or Interrupt for a ◆
    /// row's pane (ADR-0051 §6).
    Answer {
        /// The pane.
        pane: u32,
        /// Its tab's index.
        tab: u8,
        /// Its place in the tab, from 1, as the row shows it.
        place: u8,
        /// The agent profile it was launched from, if any.
        profile: Option<u8>,
        /// The key pressed: `y` approves, `i` interrupts.
        choice: Choice,
    },
    /// It stays open, and the window asks the person to confirm a new tab in an outside agent's
    /// folder (ADR-0051 §8).
    Open {
        /// The folder, as the core's row named it.
        dir: String,
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
    started: Option<Instant>,
    label: String,
    /// Printable ASCII, as the link's decoder refuses anything else.
    branch: String,
    /// The agent's message, already safe to draw.
    message: String,
    profile: Option<u8>,
    /// Context use, a whole percent, for the second line.
    ctx: Option<u8>,
    /// The last steps joined by newlines, as the link's decoder lets only clean ones through.
    steps: String,
    /// An outside agent's folder, a plain absolute path; empty for a pane's row.
    dir: String,
}

impl Row {
    /// The step for Enter or a click: a pane's row jumps, an outside one asks to open a tab.
    fn step(&self) -> Step {
        if self.dir.is_empty() {
            Step::Jump {
                tab: self.tab,
                pane: self.pane,
            }
        } else {
            Step::Open {
                dir: self.dir.clone(),
            }
        }
    }

    /// Its state as drawn: an outside row is ● running while its file changed in the last 30 s,
    /// else ○ idle.
    fn state(&self, now: Instant) -> u8 {
        match self.since {
            Some(since) if !self.dir.is_empty() => {
                if now.saturating_duration_since(since) < RUNNING {
                    1
                } else {
                    4
                }
            }
            _ => self.state,
        }
    }
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
    /// and that tab's label from `labels`, then the agents `outside` PinFold in the core's order.
    /// The selection stays on its row. Row buffers are reused, so a refresh while open allocates
    /// only for longer text.
    pub fn refresh(
        &mut self,
        listed: &[Listed],
        members: &[(u32, u8)],
        labels: &[String],
        outside: &[Outside],
    ) {
        self.ready = true;
        let chosen = self.rows.get(self.selected);
        let chosen = chosen.map(|row| (row.pane, row.dir.clone()));
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
            row.started = Some(listed.started);
            row.branch.clone_from(&listed.branch);
            row.steps.clone_from(&listed.steps);
            (row.profile, row.ctx) = (listed.profile, listed.ctx);
            row.label.clear();
            row.label
                .push_str(labels.get(usize::from(tab)).map_or("", String::as_str));
            row.message.clear();
            // The core cleaned it; this keeps state glyphs and the like out of PinFold's rows.
            row.message
                .extend(listed.message.chars().filter(|&c| top_bar::safe(c)));
            row.dir.clear();
        }
        let rank = |state: u8| {
            ORDER
                .iter()
                .position(|&s| s == state)
                .unwrap_or(ORDER.len())
        };
        self.rows.truncate(count);
        self.rows
            .sort_by_key(|row| (rank(row.state), row.since, row.tab, row.place));
        // Outside rows come after PinFold's own; their pane is none and their state is drawn
        // from the time, so `y` and `i` pass them by.
        for outside in outside {
            let mut row = Row {
                state: 1,
                since: Some(outside.changed),
                ..Row::default()
            };
            row.dir.clone_from(&outside.dir);
            self.rows.push(row);
        }
        let total = self.rows.len();
        let kept = (self.rows.iter()).position(|row| chosen == Some((row.pane, row.dir.clone())));
        self.selected = kept.unwrap_or(self.selected.min(total.saturating_sub(1)));
    }

    /// Returns when a shown time next changes: at most once a minute, as times show no
    /// seconds, so the window redraws for the clock that rarely (ADR-0051 §4).
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        // An outside row turns from ● running to ○ idle 30 s after its file changed.
        let idle = (self.rows.iter())
            .filter(|row| !row.dir.is_empty())
            .filter_map(|row| row.since.map(|since| since + RUNNING))
            .filter(|&idle| idle > now);
        self.rows
            .iter()
            .flat_map(|row| [row.since, row.started])
            .flatten()
            .filter_map(|since| {
                let seconds = now.saturating_duration_since(since).as_secs();
                (seconds < CAPPED).then(|| since + Duration::from_secs((seconds / 60 + 1) * 60))
            })
            .chain(idle)
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
                    return row.step();
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
            // Only on a ◆ row; the window's confirmation, not this key, sends anything.
            Key::Character(key) => {
                let choice = match key.as_str() {
                    "y" => Choice::Approve,
                    "i" => Choice::Interrupt,
                    _ => return Step::Stay,
                };
                let waiting = |row: &&Row| row.state == 0 && row.dir.is_empty();
                if let Some(row) = self.rows.get(self.selected).filter(waiting) {
                    return Step::Answer {
                        pane: row.pane,
                        tab: row.tab,
                        place: row.place,
                        profile: row.profile,
                        choice,
                    };
                }
            }
            _ => {}
        }
        Step::Stay
    }

    /// Takes a press at a cell of the panes' grid, `rows` by `columns`: a row does what Enter
    /// on it does, and anywhere else closes.
    pub fn click(&self, rows: u16, columns: u16, (row, column): (u16, u16)) -> Step {
        let Some(area) = self.area(rows, columns) else {
            return Step::Close;
        };
        let inside = (area.left + 1..area.left + area.width - 1).contains(&column);
        let line = usize::from(row.wrapping_sub(area.top + 1)) / usize::from(area.per);
        match self.rows.get(area.first + line) {
            Some(row) if inside && line < usize::from(area.lines) => row.step(),
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
        let extra = |row: &Row| row.ctx.is_some() || !row.steps.is_empty();
        let second = usize::from(width) - 2 >= SECOND && self.rows.iter().any(extra);
        let per = if second && tallest - CHROME >= 2 {
            2
        } else {
            1
        };
        let lines = (self.rows.len().max(1) as u16).min((tallest - CHROME) / per);
        let height = lines * per + CHROME;
        let first = self.selected.saturating_sub(usize::from(lines) - 1);
        Some(Area {
            top: (rows - height) / 3,
            left: (columns - width) / 2,
            width,
            lines,
            per,
            first,
        })
    }

    /// Draws the inbox over the panes' grid in `frame`, times as of `now`. With `webhook`, whether
    /// its `curl` was found and how many alerts it gave up on, the footer says when it is not
    /// posting (ADR-0051).
    pub fn draw(
        &self,
        frame: &mut Frame,
        theme: &Theme,
        now: Instant,
        webhook: Option<(bool, u32)>,
    ) {
        let Some(area) = self.area(frame.rows, frame.columns).filter(|_| self.ready) else {
            return;
        };
        let chrome = theme.chrome;
        let (ground, dim, text) = (chrome.palette, chrome.dim, theme.foreground);
        let (top, left, right) = (area.top, area.left, area.left + area.width - 1);
        let rule = top + area.lines * area.per + 1;
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
            let at = top + 1 + line as u16 * area.per;
            if row.dir.is_empty() {
                self.draw_row(frame, theme, (at, left, right), &columns, (index, row), now);
            } else {
                self.draw_outside(frame, theme, (at, left, right), (index, row), now);
            }
            if area.per == 2 {
                self.draw_second(frame, theme, (at + 1, left, right), (index, row));
            }
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
            let per = usize::from(area.per);
            for line in start * per..(start + thumb).min(shown) * per {
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
        let mut at = write(
            frame,
            footer,
            at,
            &format!("{waiting} needs you"),
            amber,
            ground,
            end,
        );
        if let Some((sender, failed)) = webhook {
            if failed > 0 {
                let said = format!(" · webhook: {failed} failed");
                at = write(frame, footer, at, &said, theme.states.error, ground, end);
            }
            if !sender {
                at = write(frame, footer, at, " · webhook: no curl", dim, ground, end);
            }
        }
        let said = at.max(usize::from(left) + 28);
        let keys: [(&str, &str); 4] = [
            ("⏎", " jump · "),
            ("y", " approve · "),
            ("i", " interrupt · "),
            ("esc", ""),
        ];
        let width: usize = keys
            .iter()
            .map(|(key, word)| cells(key) + cells(word))
            .sum();
        let mut at = (end - 1).saturating_sub(width);
        // The keys give way to what the footer says.
        if at > said {
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
        if columns.branch > 0 {
            cut(
                frame,
                at,
                column,
                &row.branch,
                chrome.dim,
                ground,
                columns.branch,
            );
            column += columns.branch + 2;
        }
        if columns.message > 0 {
            let (message, colour) = if row.reported {
                (row.message.as_str(), theme.foreground)
            } else {
                ("from output", chrome.dim)
            };
            cut(frame, at, column, message, colour, ground, columns.message);
        }
        let held = |since: Option<Instant>| {
            held_for(since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since)))
        };
        let time = held(row.since);
        let colour = if row.state == 0 {
            theme.states.needs_you
        } else {
            chrome.dim
        };
        write(frame, at, end - cells(&time), &time, colour, ground, end);
        // The run time, right-aligned in its own column left of the time in state.
        let run = held(row.started);
        let right = end - TIME - 1;
        write(
            frame,
            at,
            right - cells(&run),
            &run,
            chrome.dim,
            ground,
            right,
        );
    }
}

impl Inbox {
    /// Draws an outside agent's row at `at`: its glyph and word, `outside PinFold`, the agent's
    /// name, its folder escaped and cut from the left with `…`, and the time since its file
    /// changed (ADR-0051 §8).
    fn draw_outside(
        &self,
        frame: &mut Frame,
        theme: &Theme,
        (at, left, right): (u16, u16, u16),
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
        if chosen {
            put(frame, at, left + 1, '▌', chrome.accent, ground);
        }
        let end = usize::from(right) - 1;
        let state = row.state(now);
        let (glyph, word) = STATES[usize::from(state)];
        let colour = top_bar::state_colour(theme, state).unwrap_or(theme.foreground);
        let mut column = usize::from(left) + 3;
        put(frame, at, column as u16, glyph, colour, ground);
        write(frame, at, column + 2, word, colour, ground, end);
        column += 2 + WORD + 2;
        column = write(frame, at, column, OUTSIDE, chrome.dim, ground, end) + 2;
        column = write(frame, at, column, CLAUDE, theme.foreground, ground, end) + 2;
        let since = row
            .since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        let time = held_for(since);
        let room = (end - TIME - 1).saturating_sub(column);
        // `visible` gives one cell a character, so the folder's end shows when it is cut.
        let dir = visible(&row.dir);
        let count = dir.chars().count();
        if room > 1 && count > room {
            put(frame, at, column as u16, '…', chrome.dim, ground);
            let tail: String = dir.chars().skip(count - (room - 1)).collect();
            write(frame, at, column + 1, &tail, chrome.dim, ground, end);
        } else if room > 1 {
            write(frame, at, column, &dir, chrome.dim, ground, end);
        }
        write(
            frame,
            at,
            end - cells(&time),
            &time,
            chrome.dim,
            ground,
            end,
        );
    }

    /// Draws a row's second line at `at`: its context use as `ctx 64%` and its last steps,
    /// each cut to an even share of the room, in `chrome.dim` (ADR-0051 §4).
    fn draw_second(
        &self,
        frame: &mut Frame,
        theme: &Theme,
        (at, left, right): (u16, u16, u16),
        (index, row): (usize, &Row),
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
        if chosen {
            put(frame, at, left + 1, '▌', chrome.accent, ground);
        }
        let end = usize::from(right) - 1;
        // Under the word, past the glyph.
        let mut column = usize::from(left) + 5;
        if let Some(ctx) = row.ctx {
            column = write(
                frame,
                at,
                column,
                &format!("ctx {ctx}%"),
                chrome.dim,
                ground,
                end,
            );
            column += 2;
        }
        let steps: Vec<&str> = row
            .steps
            .split('\n')
            .filter(|step| !step.is_empty())
            .collect();
        for (done, step) in steps.iter().enumerate() {
            let left_over = steps.len() - done;
            // Each step's share, less the ` · ` before the next.
            let share = (end.saturating_sub(column) / left_over).saturating_sub(3);
            if share == 0 {
                break;
            }
            cut(
                frame,
                at,
                column,
                step,
                chrome.dim,
                ground,
                share.min(cells(step)),
            );
            column += share.min(cells(step));
            if left_over > 1 {
                column = write(frame, at, column, " · ", chrome.dim, ground, end);
            }
        }
    }
}

/// The inbox's place in the grid.
struct Area {
    top: u16,
    left: u16,
    width: u16,
    /// Rows of the list shown.
    lines: u16,
    /// Lines each row takes: 2 when rows show a second line.
    per: u16,
    /// The first row shown.
    first: usize,
}

/// How wide each column of a row is, in cells; 0 drops it.
struct Columns {
    place: usize,
    label: usize,
    branch: usize,
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
        let branch = widest(|row| row.branch.len(), 0).min(BRANCH);
        // The focus bar, a space, the glyph, a space, the word, then the place and two times.
        let fixed = 3 + 2 + WORD + 2 + place + 2 + TIME + 1 + TIME + 1;
        let mut room = inner.saturating_sub(fixed);
        // The label is kept longest, then the branch, then the message.
        let mut take = |width: usize| {
            let fits = width > 0 && room >= width + 2;
            room -= if fits { width + 2 } else { 0 };
            if fits { width } else { 0 }
        };
        let (label, branch) = (take(label), take(branch));
        let message = if room >= MESSAGE + 2 { room - 2 } else { 0 };
        Self {
            place,
            label,
            branch,
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
pub(crate) fn cut(
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

pub(crate) fn put(
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
pub(crate) fn write(
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
            started: now - Duration::from_secs(ago + 600),
            message: message.to_owned(),
            profile: Some(0),
            branch: String::new(),
            ctx: None,
            steps: String::new(),
            run: None,
        }
    }

    /// Five agents over three tabs, as the Agents board draws them.
    pub(crate) fn board(now: Instant) -> Inbox {
        let mut listed = [
            listed(5, 1, 363, "Writing guides/limits.md", now),
            listed(9, 2, 30, "Turn complete · terraform plan clean", now),
            listed(4, 3, 70, "exit 101 · 1 failed", now),
            listed(2, 0, 42, "Allow edit to src/config.rs?", now),
            listed(6, 1, 1335, "npm run dev · watching", now),
        ];
        for (index, branch) in [
            (0, "docs/limits"),
            (2, "fix/flaky-test"),
            (3, "main"),
            (4, "55b0e32"),
        ] {
            listed[index].branch = branch.to_owned();
        }
        let members = [(2, 0), (4, 0), (5, 1), (6, 1), (9, 2), (11, 2)];
        let labels = ["api-refactor", "docs-site", "infra"].map(str::to_owned);
        let mut inbox = Inbox::default();
        inbox.refresh(&listed, &members, &labels, &[]);
        inbox
    }

    /// ADR-0051 §6: `y` and `i` ask for a confirmation on a ◆ row only, naming its pane and
    /// profile; elsewhere they do nothing.
    #[test]
    fn y_and_i_on_a_needs_you_row_ask_and_on_any_other_do_nothing() {
        let mut inbox = board(Instant::now());
        let key = |text: &str| Key::Character(text.into());
        let answer = |choice| Step::Answer {
            pane: 2,
            tab: 0,
            place: 1,
            profile: Some(0),
            choice,
        };
        assert_eq!(inbox.key(&key("y")), answer(Choice::Approve));
        assert_eq!(inbox.key(&key("i")), answer(Choice::Interrupt));
        assert_eq!(inbox.key(&key("Y")), Step::Stay);
        inbox.key(&Key::Named(NamedKey::ArrowDown));
        assert_eq!(inbox.key(&key("y")), Step::Stay, "an error row");
        assert_eq!(inbox.key(&key("i")), Step::Stay);
    }

    #[test]
    fn the_footer_says_when_the_webhook_is_not_posting() {
        let now = Instant::now();
        let inbox = board(now);
        let footer = |webhook| {
            let mut frame = Frame::default();
            crate::panes::blank(&mut frame, 20, 100);
            inbox.draw(&mut frame, &GRAPHITE, now, webhook);
            (0..20)
                .map(|row| text(&frame, row))
                .find(|line| line.contains("agents ·"))
        };
        let plain = footer(None).unwrap();
        assert!(
            !plain.contains("webhook") && plain.contains("⏎ jump · y approve · i interrupt · esc"),
            "{plain}"
        );
        assert!(
            !footer(Some((true, 0))).unwrap().contains("webhook"),
            "posting"
        );
        let failed = footer(Some((true, 3))).unwrap();
        assert!(failed.contains("needs you · webhook: 3 failed"), "{failed}");
        let none = footer(Some((false, 0))).unwrap();
        assert!(none.contains("needs you · webhook: no curl"), "{none}");
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, 20, 100);
        inbox.draw(&mut frame, &GRAPHITE, now, Some((true, 3)));
        let red = frame
            .cells
            .iter()
            .find(|cell| cell.character == 'w' && cell.foreground == GRAPHITE.states.error);
        assert!(red.is_some(), "failed is red");
    }

    pub(crate) fn drawn(inbox: &Inbox, rows: u16, columns: u16, now: Instant) -> Frame {
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, rows, columns);
        inbox.draw(&mut frame, &GRAPHITE, now, None);
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

    /// One pane needing you, then two agents outside PinFold: one whose file changed 5 s ago
    /// and one 10 minutes ago with a long, hostile folder.
    pub(crate) fn with_outside(now: Instant) -> Inbox {
        let outside = [
            Outside {
                dir: "/Users/me/src/api".to_owned(),
                changed: now - Duration::from_secs(5),
            },
            Outside {
                dir: format!("/Users/me/{}x\u{202e}y/end", "deep/".repeat(30)),
                changed: now - Duration::from_secs(600),
            },
        ];
        let mut inbox = Inbox::default();
        let listed = [listed(2, 0, 42, "Allow edit?", now)];
        inbox.refresh(&listed, &[(2, 0)], &["api".into()], &outside);
        inbox
    }

    /// ADR-0051 §8: outside rows follow PinFold's own, with glyph, colour and word, `outside
    /// PinFold`, the agent's name, the folder escaped and cut from the left, and the time since
    /// the file changed; ● running turns ○ idle 30 s after it.
    #[test]
    fn outside_rows_follow_pinfolds_own_and_cut_their_folder_from_the_left() {
        let now = Instant::now();
        let inbox = with_outside(now);
        let shown = lines(&drawn(&inbox, 20, 100, now));
        assert!(shown[0].contains("◆ needs you"), "{shown:?}");
        assert!(
            shown[1].contains("● running    outside PinFold  Claude Code  /Users/me/src/api"),
            "{shown:?}"
        );
        assert!(shown[1].ends_with("<1m"), "{}", shown[1]);
        let long = &shown[2];
        assert!(
            long.contains("○ idle       outside PinFold  Claude Code  …"),
            "{long}"
        );
        assert!(
            long.contains("x<U+202E>y/end") && long.ends_with("10m"),
            "{long}"
        );
        let colour = |frame: &Frame, glyph| {
            frame
                .cells
                .iter()
                .find(|c| c.character == glyph)
                .map(|c| c.foreground)
        };
        let frame = drawn(&inbox, 20, 100, now);
        assert_eq!(colour(&frame, '●'), Some(GRAPHITE.states.running));
        assert!(inbox.next_change(now) <= Some(now + Duration::from_secs(25)));
        let later = drawn(&inbox, 20, 100, now + RUNNING);
        let later = lines(&later);
        assert!(later[1].contains("○ idle"), "{later:?}");
    }

    /// Enter or a click on an outside row asks to open its folder; `y` and `i` pass it by.
    #[test]
    fn enter_on_an_outside_row_asks_to_open_its_folder() {
        let now = Instant::now();
        let mut inbox = with_outside(now);
        inbox.key(&Key::Named(NamedKey::ArrowDown));
        assert_eq!(inbox.key(&Key::Character("y".into())), Step::Stay);
        assert_eq!(inbox.key(&Key::Character("i".into())), Step::Stay);
        let open = Step::Open {
            dir: "/Users/me/src/api".to_owned(),
        };
        assert_eq!(inbox.key(&Key::Named(NamedKey::Enter)), open);
        let frame = drawn(&inbox, 20, 100, now);
        let row = (0..20).find(|&row| text(&frame, row).contains("/Users/me/src/api"));
        assert_eq!(inbox.click(20, 100, (row.unwrap(), 50)), open);
        // The selection stays on its row as the rows come again.
        inbox.refresh(
            &[],
            &[],
            &[],
            &[Outside {
                dir: "/Users/me/src/api".into(),
                changed: now,
            }],
        );
        assert_eq!(inbox.key(&Key::Named(NamedKey::Enter)), open);
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
        inbox.refresh(&six, &members, &["tab".to_owned()], &[]);
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
        inbox.refresh(&[from_output], &[(3, 0)], &["api".to_owned()], &[]);
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
        inbox.refresh(&[listed(3, 1, 2, message, now)], &[(3, 0)], &[label], &[]);
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
        inbox.refresh(&listed, &members, &[], &[]);
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
    fn a_narrow_window_drops_the_message_then_the_branch_then_the_label() {
        let now = Instant::now();
        let inbox = board(now);
        let row = |columns| lines(&drawn(&inbox, 24, columns, now)).remove(0);
        let shows = |columns, label, branch, message| {
            let row = row(columns);
            let shown = (
                row.contains("api-refactor"),
                row.contains("main"),
                row.contains("Allow"),
            );
            assert_eq!(shown, (label, branch, message), "{columns}: {row:?}");
        };
        shows(104, true, true, true);
        shows(72, true, true, false);
        shows(60, true, false, false);
        shows(40, false, false, false);
        let narrow = row(40);
        assert!(
            narrow.contains("<1m") && narrow.contains("10m"),
            "{narrow:?}"
        );
        assert!(narrow.contains("◆ needs you"), "the word always stays");
        let tiny = drawn(&inbox, 4, 20, now);
        assert!(lines(&tiny).is_empty(), "too small to draw");
    }

    #[test]
    fn nothing_listed_says_so_and_enter_does_nothing() {
        let mut inbox = Inbox::default();
        inbox.refresh(&[], &[], &[], &[]);
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
        inbox.refresh(&listed, &members, &[], &[]);
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

    /// The board with its ◆ and ✕ agents' context use and steps, as `[inbox] agent_files`
    /// brings them.
    fn board_with_steps(now: Instant) -> Inbox {
        let mut listed = [
            listed(2, 0, 42, "Allow edit to src/config.rs?", now),
            listed(4, 3, 70, "exit 101 · 1 failed", now),
            listed(5, 1, 363, "Writing guides/limits.md", now),
        ];
        (listed[0].ctx, listed[0].steps) = (Some(64), "Read\nNow the parser.\nEdit".to_owned());
        listed[1].steps = "cargo test --workspace".to_owned();
        let mut inbox = Inbox::default();
        inbox.refresh(
            &listed,
            &[(2, 0), (4, 0), (5, 1)],
            &["api".into(), "docs".into()],
            &[],
        );
        inbox
    }

    /// ADR-0051 §4: with steps, every row takes two lines, the second `ctx 64%` and the steps
    /// under the word; a click on either line jumps; a narrow window drops the second line
    /// before the message.
    #[test]
    fn a_second_line_shows_context_and_steps_and_goes_first_when_narrow() {
        let now = Instant::now();
        let inbox = board_with_steps(now);
        let frame = drawn(&inbox, 24, 100, now);
        let lines: Vec<String> = (0..24).map(|row| text(&frame, row)).collect();
        let first = lines.iter().position(|line| line.contains("◆")).unwrap();
        assert!(
            lines[first + 1].contains("ctx 64%  Read · Now the parser. · Edit"),
            "{}",
            lines[first + 1]
        );
        assert!(lines[first + 2].contains("✕ error"), "{}", lines[first + 2]);
        assert!(lines[first + 3].contains("cargo test --workspace"));
        assert!(!lines[first + 3].contains("ctx"), "no ctx shows none");
        assert!(lines[first + 4].contains("● running"));
        assert!(
            lines[first + 5].trim_matches(['│', ' ']).is_empty(),
            "{}",
            lines[first + 5]
        );
        let first = first as u16;
        assert_eq!(
            inbox.click(24, 100, (first + 3, 50)),
            Step::Jump { tab: 0, pane: 4 }
        );
        assert_eq!(
            inbox.click(24, 100, (first + 2, 50)),
            Step::Jump { tab: 0, pane: 4 }
        );
        assert_eq!(
            inbox.click(24, 100, (first + 1, 50)),
            Step::Jump { tab: 0, pane: 2 }
        );

        // Narrow: no second line, the message still shows.
        let frame = drawn(&inbox, 24, 63, now);
        let lines: Vec<String> = (0..24).map(|row| text(&frame, row)).collect();
        let first = lines.iter().position(|line| line.contains("◆")).unwrap();
        assert!(lines[first + 1].contains("✕"), "{}", lines[first + 1]);
        assert!(!lines.iter().any(|line| line.contains("ctx")));
        assert!(lines[first].contains("Allow"), "{}", lines[first]);
        // Each step is cut to an even share where they do not all fit.
        let mut listed = [listed(2, 0, 42, "Allow", now)];
        listed[0].ctx = Some(64);
        listed[0].steps = "Read\nNow the parser reads the session tail\nEdit".to_owned();
        let mut inbox = Inbox::default();
        inbox.refresh(&listed, &[(2, 0)], &["api".into()], &[]);
        let frame = drawn(&inbox, 24, 66, now);
        let second = (0..24)
            .map(|row| text(&frame, row))
            .find(|line| line.contains("ctx"));
        let second = second.unwrap();
        assert!(
            second.contains("ctx 64%  Read · Now the parser … · Edit"),
            "{second}"
        );
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
        inbox.refresh(&[listed(2, 0, 400_000, "", now)], &[(2, 0)], &[], &[]);
        assert_eq!(inbox.next_change(now), None, "99h+ never changes");
        inbox.refresh(&[], &[], &[], &[]);
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
