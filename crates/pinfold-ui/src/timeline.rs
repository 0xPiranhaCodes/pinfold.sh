//! The timeline: what one agent run did, in time order (ADR-0052 §5).
//!
//! Prefix `t` opens it on the focused pane's run. Like the inbox it is drawn after the panes and
//! takes every key while open, so program output can neither fake nor answer it. It holds one
//! page of at most [`MAX_PAGE`] entries, as the core sent them, and its commands' labels, each
//! cut to [`LABEL`] bytes, and asks the core for the next page as the selection moves past
//! either end. Closing drops the whole value, so nothing of the run is kept once it closes.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use pinfold_config::theme::Theme;
use pinfold_proto::Frame;
use pinfold_proto::link::{ENTRY, MAX_PAGE};
use pinfold_sys::LocalTime;
use winit::keyboard::{Key, NamedKey};

use crate::inbox::{cut, put, write};
use crate::link::RunSummary;
use crate::top_bar::{self, STATES};

/// The widest the overlay draws, border included.
const WIDTH: u16 = 100;
/// The narrowest, unless the window is narrower.
const NARROWEST: u16 = 40;
/// Rows of chrome: the top border, the summary, the rule, two footer lines, the bottom border.
const CHROME: u16 = 6;
/// The most bytes a label is held in: 64 of them stay within the 6.2 KiB the ADR budgets.
pub(crate) const LABEL: usize = 80;
/// Cells between the borders below which rows drop the label, then the time.
const NO_LABEL: usize = 64;
const NO_TIME: usize = 48;
/// Cells of the kind column, `exited` the widest.
const KIND: usize = 6;
/// A label's length byte for a row no longer held.
const EVICTED: u8 = u8::MAX;
/// A `command` entry's row when the shell gave no `B` mark.
const NO_ROW: u32 = u32::MAX;
/// The audit log's method names, as an `api` entry indexes them (`pinfold_core::timeline`).
const METHODS: [&str; 24] = [
    "hello",
    "window.open",
    "pane.list",
    "events.subscribe",
    "pane.read",
    "pane.last_output",
    "pane.send",
    "pane.wait",
    "pane.spawn",
    "agent.launch",
    "tool.open",
    "pane.close",
    "checkpoint.take",
    "checkpoint.list",
    "checkpoint.restore",
    "pane.status",
    "audit.read",
    "session.save",
    "workspace.open",
    "grants.list",
    "grants.revoke",
    "workspace.trust_state",
    "workspace.ask_trust",
    "run.timeline",
];
/// Why a checkpoint was taken, as a `checkpoint` entry indexes it.
const REASONS: [&str; 6] = ["launch", "done", "exit", "api", "view", "restore"];
/// What an approval asked, by `ApprovalAction`'s number, never the prompt's texts.
const ACTIONS: [&str; 5] = ["pane.read", "pane.send", "trust", "owner", "rewind"];

/// What a key did to the timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// It stays open.
    Stay,
    /// It closes.
    Close,
    /// Ask the core for the page that ends before this `seq`, the newest without one.
    Page(Option<u32>),
    /// Scroll the pane to this stored row, and close.
    Reveal(u32),
    /// The command's output has left the scrollback: say so.
    Evicted,
    /// Open the diff viewer at this checkpoint of the run, and close.
    Diff(u32),
    /// No checkpoint at or after the entry is held: say so.
    NoCheckpoint,
}

/// Where the selection goes once the page asked for comes.
#[derive(Clone, Copy, Debug)]
enum Want {
    Newest,
    Oldest,
    Seq(u32),
}

/// What the header says beside the run, from what the window already holds.
pub struct Head<'a> {
    /// The tab's label, already safe.
    pub label: &'a str,
    /// The tab's index.
    pub tab: usize,
    /// The pane's state and whether its agent reported it.
    pub status: Option<(u8, bool)>,
    /// The run's counters.
    pub run: Option<RunSummary>,
}

/// The fields of an entry, as ADR-0052 section 2 lays them out.
struct Fields {
    kind: u8,
    small: u8,
    flag: u16,
    at: u32,
    one: u32,
    two: u32,
}

fn fields(bytes: &[u8; ENTRY]) -> Fields {
    let word =
        |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Fields {
        kind: bytes[0],
        small: bytes[1],
        flag: u16::from_le_bytes([bytes[2], bytes[3]]),
        at: word(4),
        one: word(8),
        two: word(12),
    }
}

/// The overlay, while open.
pub struct Timeline {
    pane: u32,
    /// When the run started, in seconds since the epoch.
    start: i64,
    dropped: u32,
    total: u32,
    /// The held page, oldest first, whose first entry's `seq` is `first`.
    entries: Vec<[u8; ENTRY]>,
    first: u32,
    /// Its commands' labels in their order, each a length byte and the text, or [`EVICTED`].
    /// Both are made at their largest when the overlay opens, so they never grow.
    labels: Vec<u8>,
    /// Index into `entries`, always a shown one when any is.
    selected: usize,
    /// Whether `api` rows that succeeded are shown, which `a` toggles.
    calls: bool,
    /// The page asked for and not come yet: what it ends before, and where the selection goes.
    asked: Option<(Option<u32>, Want)>,
    ready: bool,
    /// Rows the last draw showed, which PgUp and PgDn move by.
    lines: Cell<u16>,
    local: fn(i64) -> Option<LocalTime>,
}

impl Timeline {
    /// Opens on `pane`'s run, and the step that asks for its newest page.
    pub fn open(pane: u32) -> (Self, Step) {
        let timeline = Self {
            pane,
            start: 0,
            dropped: 0,
            total: 0,
            entries: Vec::with_capacity(MAX_PAGE.into()),
            first: 0,
            labels: Vec::with_capacity(usize::from(MAX_PAGE) * (LABEL + 1)),
            selected: 0,
            calls: false,
            asked: Some((None, Want::Newest)),
            ready: false,
            lines: Cell::new(1),
            local: pinfold_sys::local_time,
        };
        (timeline, Step::Page(None))
    }

    /// The pane whose run it shows.
    pub fn pane(&self) -> u32 {
        self.pane
    }

    /// Returns whether a page has come since it opened.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Takes a page of `pane`'s run, `started` seconds after it began, in place of the one held.
    pub fn page(
        &mut self,
        pane: u32,
        started: u32,
        (dropped, total): (u32, u32),
        entries: &[[u8; ENTRY]],
    ) {
        if pane != self.pane {
            return;
        }
        let (before, want) = self.asked.take().unwrap_or((None, Want::Newest));
        let now = SystemTime::now().duration_since(UNIX_EPOCH);
        self.start = now.map_or(0, |now| now.as_secs() as i64) - i64::from(started);
        (self.dropped, self.total, self.ready) = (dropped, total, true);
        let entries = &entries[..entries.len().min(MAX_PAGE.into())];
        self.first = before.unwrap_or(total).saturating_sub(entries.len() as u32);
        self.entries.clear();
        self.entries.extend_from_slice(entries);
        self.labels.clear();
        let last = self.entries.len().saturating_sub(1);
        let index = match want {
            Want::Newest => last,
            Want::Oldest => 0,
            Want::Seq(seq) => (seq.saturating_sub(self.first) as usize).min(last),
        };
        self.settle(index);
    }

    /// Takes an entry the watched run wrote. While the newest page is held it is appended, the
    /// oldest held entry and its label dropped past [`MAX_PAGE`], and a selection on the newest
    /// row follows it; otherwise only the count moves, for the next page down.
    pub fn entry(&mut self, pane: u32, seq: u32, entry: &[u8; ENTRY]) {
        if pane != self.pane || seq < self.total {
            return;
        }
        let end = self.first + self.entries.len() as u32;
        let newest = end == self.total && self.asked.is_none();
        let followed = self
            .visible()
            .last()
            .is_none_or(|last| last == self.selected);
        self.total = seq + 1;
        if !newest || seq != end {
            return;
        }
        if self.entries.len() == usize::from(MAX_PAGE) {
            if self.entries[0][0] == 1 && self.labelled() > 0 {
                let byte = self.labels[0];
                let length = if byte == EVICTED {
                    0
                } else {
                    usize::from(byte)
                };
                self.labels.drain(..1 + length);
            }
            self.entries.remove(0);
            self.first += 1;
            self.selected = self.selected.saturating_sub(1);
        }
        self.entries.push(*entry);
        let index = if followed {
            self.entries.len() - 1
        } else {
            self.selected
        };
        self.settle(index);
    }

    /// Takes the label of the next command in the page that has none, if it names that
    /// command's row: the core sends one after each, in order. `text` is as [`hold`] left it.
    pub fn label(&mut self, pane: u32, row: u32, held: bool, text: &str) {
        let next = self.commands().nth(self.labelled());
        if pane != self.pane || next.map(|index| fields(&self.entries[index]).one) != Some(row) {
            return;
        }
        let text = &text.as_bytes()[..text.len().min(LABEL)];
        if held {
            self.labels.push(text.len() as u8);
            self.labels.extend_from_slice(text);
        } else {
            self.labels.push(EVICTED);
        }
    }

    /// The indexes of the page's commands.
    fn commands(&self) -> impl Iterator<Item = usize> + '_ {
        (self.entries.iter().enumerate())
            .filter(|(_, entry)| entry[0] == 1)
            .map(|(index, _)| index)
    }

    /// Labels held: one for each of the first commands.
    fn labelled(&self) -> usize {
        self.each_label().count()
    }

    /// Each held label: whether its row is still held, and its text.
    fn each_label(&self) -> impl Iterator<Item = (bool, &str)> + '_ {
        let mut rest = self.labels.as_slice();
        std::iter::from_fn(move || {
            let (&byte, after) = rest.split_first()?;
            let length = if byte == EVICTED {
                0
            } else {
                usize::from(byte)
            };
            let (text, after) = after.split_at_checked(length)?;
            rest = after;
            Some((byte != EVICTED, std::str::from_utf8(text).unwrap_or("")))
        })
    }

    /// The label of the entry at `index`, if it is a command whose label came.
    fn label_of(&self, index: usize) -> Option<(bool, &str)> {
        let nth = self.commands().position(|command| command == index)?;
        self.each_label().nth(nth)
    }

    /// Takes a key press. Keys it has no use for are swallowed, as it holds the keyboard.
    pub fn key(&mut self, key: &Key) -> Step {
        let page = i64::from(self.lines.get().max(1));
        match key {
            Key::Named(NamedKey::Escape) => Step::Close,
            Key::Named(NamedKey::Enter) => self.act(),
            Key::Named(NamedKey::ArrowUp) => self.step(-1),
            Key::Named(NamedKey::ArrowDown) => self.step(1),
            Key::Named(NamedKey::PageUp) => self.step(-page),
            Key::Named(NamedKey::PageDown) => self.step(page),
            Key::Named(NamedKey::Home) => {
                let oldest = self.dropped.saturating_add(MAX_PAGE.into());
                self.ask((oldest < self.total).then_some(oldest), Want::Oldest)
            }
            Key::Named(NamedKey::End) => self.ask(None, Want::Newest),
            Key::Character(text) => match text.as_str() {
                "a" => {
                    self.calls = !self.calls;
                    self.settle(self.selected);
                    Step::Stay
                }
                "n" => self.error(1),
                "p" => self.error(-1),
                "d" => self.diff(),
                _ => Step::Stay,
            },
            _ => Step::Stay,
        }
    }

    /// Whether an entry is drawn: every one but an `api` call that succeeded, until `a`.
    fn shown(&self, entry: &[u8; ENTRY]) -> bool {
        self.calls || entry[0] != 2 || fields(entry).flag != 0
    }

    fn visible(&self) -> impl Iterator<Item = usize> + '_ {
        (self.entries.iter().enumerate())
            .filter(|(_, entry)| self.shown(entry))
            .map(|(index, _)| index)
    }

    /// Selects the shown entry nearest `index`, at or before it first.
    fn settle(&mut self, index: usize) {
        let before = self.visible().take_while(|&shown| shown <= index).last();
        self.selected = before.or_else(|| self.visible().next()).unwrap_or(0);
    }

    /// Moves `by` shown rows, asking for the next page past either end.
    fn step(&mut self, by: i64) -> Step {
        let shown: Vec<usize> = self.visible().collect();
        let at = shown.iter().position(|&index| index == self.selected);
        let target = at.map_or(by, |at| at as i64 + by);
        if target < 0 {
            let first = self.first;
            if first > self.dropped {
                return self.ask(Some(first), Want::Seq(first - 1));
            }
        } else if target as usize >= shown.len() {
            let next = self.first + self.entries.len() as u32;
            if next < self.total {
                let before = next.saturating_add(MAX_PAGE.into());
                return self.ask((before < self.total).then_some(before), Want::Seq(next));
            }
        }
        let clamped = target.clamp(0, shown.len() as i64 - 1).max(0) as usize;
        if let Some(&index) = shown.get(clamped) {
            self.selected = index;
        }
        Step::Stay
    }

    fn ask(&mut self, before: Option<u32>, want: Want) -> Step {
        if self.asked.is_some() {
            return Step::Stay;
        }
        self.asked = Some((before, want));
        Step::Page(before)
    }

    /// Enter: a command jumps to its output, and a checkpoint opens the diff viewer at it.
    fn act(&self) -> Step {
        let Some(entry) = self.entries.get(self.selected) else {
            return Step::Stay;
        };
        let entry = fields(entry);
        let held = self.label_of(self.selected).map(|(held, _)| held);
        match entry.kind {
            1 if entry.one == NO_ROW => Step::Stay,
            1 if held == Some(false) => Step::Evicted,
            1 => Step::Reveal(entry.one),
            5 => Step::Diff(entry.flag.into()),
            _ => Step::Stay,
        }
    }

    /// `d`: the diff viewer at the nearest held checkpoint at or after the entry.
    fn diff(&self) -> Step {
        let after = self.entries.iter().skip(self.selected);
        let checkpoint = after.map(fields).find(|entry| entry.kind == 5);
        checkpoint.map_or(Step::NoCheckpoint, |entry| Step::Diff(entry.flag.into()))
    }

    /// `n` and `p`: the next or previous failed command or ✕ error state held.
    fn error(&mut self, by: i64) -> Step {
        let failed = |entry: &[u8; ENTRY]| {
            let entry = fields(entry);
            matches!(
                (entry.kind, entry.flag, entry.small),
                (1, 1, 1..) | (4, _, 3)
            )
        };
        let (shown, selected): (Vec<usize>, _) = (self.visible().collect(), self.selected);
        let found = if by > 0 {
            (shown.iter()).find(|&&index| index > selected && failed(&self.entries[index]))
        } else {
            (shown.iter().rev()).find(|&&index| index < selected && failed(&self.entries[index]))
        };
        if let Some(&index) = found {
            self.selected = index;
        }
        Step::Stay
    }

    /// Bytes the page and its labels hold on the heap.
    #[cfg(test)]
    pub(crate) fn held_bytes(&self) -> usize {
        self.entries.capacity() * ENTRY + self.labels.capacity()
    }

    /// Takes a press at a cell of the panes' grid, `rows` by `columns`: a row is selected, and
    /// a second press on it acts as Enter does; outside the overlay it closes.
    pub fn click(&mut self, rows: u16, columns: u16, (row, column): (u16, u16)) -> Step {
        let shown: Vec<usize> = self.visible().collect();
        let Some((top, left, width, lines)) = self.area(rows, columns, shown.len()) else {
            return Step::Close;
        };
        if !(left..left + width).contains(&column) || !(top..=top + lines + 5).contains(&row) {
            return Step::Close;
        }
        let line = usize::from(row.wrapping_sub(top + 2));
        let first = self.first_shown(&shown, lines);
        match shown
            .get(first + line)
            .filter(|_| line < usize::from(lines))
        {
            Some(&index) if index == self.selected => self.act(),
            Some(&index) => {
                self.selected = index;
                Step::Stay
            }
            None => Step::Stay,
        }
    }

    /// Moves the selection by wheel lines, up for positive.
    pub fn wheel(&mut self, lines: i32) -> Step {
        self.step(-i64::from(lines))
    }

    /// The first of the `shown` rows drawn, so that the selection is in view.
    fn first_shown(&self, shown: &[usize], lines: u16) -> usize {
        (shown.iter().position(|&index| index == self.selected))
            .map_or(0, |at| at.saturating_sub(usize::from(lines) - 1))
    }

    /// Where it goes in a grid of `rows` by `columns`: top, left, width and rows of entries.
    fn area(&self, rows: u16, columns: u16, shown: usize) -> Option<(u16, u16, u16, u16)> {
        let width = columns
            .saturating_sub(4)
            .clamp(NARROWEST, WIDTH)
            .min(columns);
        let tallest = rows.saturating_sub(4).max(CHROME + 1).min(rows);
        if width < 24 || tallest <= CHROME {
            return None;
        }
        let lines = (shown.max(1) as u16).min(tallest - CHROME);
        let height = lines + CHROME;
        Some(((rows - height) / 3, (columns - width) / 2, width, lines))
    }

    /// Draws the overlay over the panes' grid in `frame`.
    pub fn draw(&self, frame: &mut Frame, theme: &Theme, head: &Head) {
        let shown: Vec<usize> = self.visible().collect();
        let area = self.area(frame.rows, frame.columns, shown.len());
        let Some((top, left, width, lines)) = area.filter(|_| self.ready) else {
            return;
        };
        self.lines.set(lines);
        let chrome = theme.chrome;
        let (ground, dim) = (chrome.palette, chrome.dim);
        let right = left + width - 1;
        let rule = top + 2 + lines;
        let bottom = rule + 3;
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
        let end = usize::from(right);
        let start = usize::from(left) + 2;
        // The title sits in the top border, and the close key on its right.
        let mut at = write(
            frame,
            top,
            start,
            " TIMELINE",
            chrome.accent,
            ground,
            end - 6,
        );
        let place = format!(
            " · {} · tab {} · pane {} · ",
            head.label,
            head.tab + 1,
            self.pane
        );
        at = write(frame, top, at, &place, dim, ground, end - 6);
        if let Some((state, _)) = head.status {
            let (glyph, word) = STATES[usize::from(state).min(STATES.len() - 1)];
            let colour = top_bar::state_colour(theme, state).unwrap_or(theme.foreground);
            at = write(
                frame,
                top,
                at,
                &format!("{glyph} {word}"),
                colour,
                ground,
                end - 6,
            );
            write(frame, top, at, " ", dim, ground, end - 6);
        }
        write(frame, top, end - 6, " esc ", dim, ground, end);
        self.draw_summary(frame, theme, (top + 1, start, end - 1), head.run);

        let inner = usize::from(width) - 2;
        let first = self.first_shown(&shown, lines);
        for (line, &index) in shown.iter().skip(first).take(lines.into()).enumerate() {
            self.draw_row(
                frame,
                theme,
                (top + 2 + line as u16, left, right),
                index,
                inner,
            );
        }
        if shown.is_empty() {
            let said = if self.entries.is_empty() {
                "Nothing recorded yet"
            } else {
                "Only api calls that succeeded here · a shows them"
            };
            write(frame, top + 2, start + 1, said, dim, ground, end);
        }

        let keys: [(&str, &str); 5] = [
            ("⏎", " jump · "),
            ("d", " diff at checkpoint · "),
            ("a", " api calls · "),
            ("n/p", " next/previous error · "),
            ("esc", ""),
        ];
        let mut at = start;
        for (key, word) in keys {
            at = write(frame, rule + 1, at, key, theme.foreground, ground, end);
            at = write(frame, rule + 1, at, word, dim, ground, end);
        }
        let said = "commands and exit codes come from the shell's marks, which output can print";
        cut(frame, rule + 2, start, said, dim, ground, end - start);
    }

    /// The run's counters, a zero left out, and how many entries the ring dropped.
    fn draw_summary(
        &self,
        frame: &mut Frame,
        theme: &Theme,
        (row, start, end): (u16, usize, usize),
        run: Option<RunSummary>,
    ) {
        let (ground, dim) = (theme.chrome.palette, theme.chrome.dim);
        let run = run.unwrap_or_default();
        let counts = [
            (run.commands, "command", "commands"),
            (run.approvals, "approval", "approvals"),
            (run.changed, "file changed", "files changed"),
            (run.errors, "error", "errors"),
        ];
        let mut at = start;
        for (count, one, many) in counts.into_iter().filter(|(count, _, _)| *count > 0) {
            let gap = if at == start { "" } else { " · " };
            let word = if count == 1 { one } else { many };
            at = write(
                frame,
                row,
                at,
                &format!("{gap}{count} {word}"),
                theme.foreground,
                ground,
                end,
            );
        }
        if at == start {
            at = write(frame, row, at, "nothing counted yet", dim, ground, end);
        }
        if self.dropped > 0 {
            write(
                frame,
                row,
                at,
                &format!(" · {} older dropped", self.dropped),
                dim,
                ground,
                end,
            );
        }
    }

    fn draw_row(
        &self,
        frame: &mut Frame,
        theme: &Theme,
        (at, left, right): (u16, u16, u16),
        index: usize,
        inner: usize,
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
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        let entry = fields(entry);
        let mut column = usize::from(left) + 3;
        if inner >= NO_TIME {
            let time = (self.local)(self.start + i64::from(entry.at / 1000)).map_or_else(
                || "--:--:--".to_owned(),
                |time| format!("{:02}:{:02}:{:02}", time.hour, time.minute, time.second),
            );
            column = write(frame, at, column, &time, chrome.dim, ground, end) + 2;
        }
        let (fg, red) = (theme.foreground, theme.states.error);
        let (kind, parts): (&str, Vec<(String, u32)>) = match entry.kind {
            1 => {
                let took = took(entry.two);
                let said = match (entry.flag, entry.small) {
                    (1, 0) => (format!("exit 0 · {took}"), fg),
                    (1, code) => (format!("failed · exit {code} · {took}"), red),
                    _ => (format!("no status · {took}"), chrome.dim),
                };
                ("cmd", vec![said])
            }
            2 => {
                let method = METHODS
                    .get(usize::from(entry.small))
                    .copied()
                    .unwrap_or("?");
                let outcome = match entry.flag {
                    0 => ("ok".to_owned(), chrome.dim),
                    1 => ("refused".to_owned(), red),
                    _ => ("error".to_owned(), red),
                };
                let mut parts = vec![(method.to_owned(), fg), outcome];
                if entry.two != u32::MAX {
                    parts.push((format!("→ pane {}", entry.two), chrome.dim));
                }
                ("api", parts)
            }
            3 => {
                let action = ACTIONS
                    .get(usize::from(entry.small))
                    .copied()
                    .unwrap_or("?");
                let action = match entry.two {
                    u32::MAX => action.to_owned(),
                    other => format!("{action} → pane {other}"),
                };
                let answer = match entry.flag {
                    0 => ("allowed once", chrome.accent),
                    1 => ("allowed 30 min", chrome.accent),
                    2 => ("denied", red),
                    _ => ("expired", chrome.dim),
                };
                ("ask", vec![(action, fg), (answer.0.to_owned(), answer.1)])
            }
            4 => {
                let state = entry.small.min(STATES.len() as u8 - 1);
                let (glyph, word) = STATES[usize::from(state)];
                let colour = top_bar::state_colour(theme, state).unwrap_or(fg);
                let source = if entry.flag == 1 {
                    "reported"
                } else {
                    "from output"
                };
                put(frame, at, column as u16, glyph, colour, ground);
                column = write(frame, at, column + 2, word, colour, ground, end) + 2;
                write(frame, at, column, source, chrome.dim, ground, end);
                return;
            }
            5 => {
                let reason = REASONS
                    .get(usize::from(entry.small))
                    .copied()
                    .unwrap_or("?");
                let changed = if entry.small == 5 {
                    format!("{} undone", entry.one)
                } else {
                    format!("{} changed since launch", entry.one)
                };
                let mut parts = vec![(changed, fg), (reason.to_owned(), chrome.dim)];
                if entry.two == 0 {
                    parts.push(("incomplete".to_owned(), theme.yellow));
                }
                ("files", parts)
            }
            _ => {
                let said = match entry.flag {
                    0 => format!("status {}", entry.small),
                    1 => format!("signal {}", entry.small),
                    _ => "pane closed".to_owned(),
                };
                ("exited", vec![(said, fg)])
            }
        };
        write(frame, at, column, kind, chrome.dim, ground, end);
        column += KIND + 1;
        for (text, colour) in &parts {
            column = write(frame, at, column, text, *colour, ground, end) + 2;
        }
        // The command's own line, or that it is gone; nothing until its label comes.
        if entry.kind != 1 || inner < NO_LABEL || column + 4 > end {
            return;
        }
        match self.label_of(index) {
            Some((true, text)) => cut(frame, at, column, text, fg, ground, end - column),
            Some((false, _)) => {
                write(frame, at, column, "evicted", chrome.dim, ground, end);
            }
            None => {}
        }
    }
}

/// A command's run time: `2.3 s`, then `1m20s`, then `2h05m`.
fn took(ms: u32) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..60 => format!("{seconds}.{} s", ms % 1000 / 100),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60),
    }
}

/// A label as held: the characters a message may hold, cut at a character to [`LABEL`] bytes,
/// ending in `…` if cut. CJK still keeps about 26 wide characters, which fills a row.
pub(crate) fn hold(text: &str) -> String {
    let safe = || text.chars().filter(|&character| top_bar::safe(character));
    let fits = safe().map(char::len_utf8).sum::<usize>() <= LABEL;
    let room = if fits {
        LABEL
    } else {
        LABEL - '…'.len_utf8()
    };
    let mut held = String::with_capacity(LABEL);
    for character in safe() {
        if held.len() + character.len_utf8() > room {
            break;
        }
        held.push(character);
    }
    if !fits {
        held.push('…');
    }
    held.shrink_to_fit();
    held
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pinfold_config::theme::GRAPHITE;
    use pinfold_core::status::Status;
    use pinfold_core::timeline::{Answer, Ended, Entry, Outcome, What};
    use pinfold_proto::link::ApprovalAction;

    use crate::snapshots::pane;

    /// A label a program can print: an escape, a bidi override and a state glyph.
    pub(crate) const HOSTILE: &str = "$ echo \u{1b}[31m\u{202e}ok ◆ needs you";

    fn utc(seconds: i64) -> Option<LocalTime> {
        let seconds = seconds.rem_euclid(86_400);
        Some(LocalTime {
            weekday: 0,
            day: 1,
            month: 0,
            hour: (seconds / 3600) as u8,
            minute: (seconds / 60 % 60) as u8,
            second: (seconds % 60) as u8,
        })
    }

    fn entry(at: u32, what: What) -> [u8; ENTRY] {
        Entry { at, what }.encode()
    }

    fn command(at: u32, row: u32, ms: u32, exit: u8) -> [u8; ENTRY] {
        let exit = Some(exit);
        entry(at, What::Command { row, ms, exit })
    }

    fn checkpoint(at: u32, n: u16, reason: u8, complete: bool, changed: u32) -> [u8; ENTRY] {
        entry(
            at,
            What::Checkpoint {
                n,
                reason,
                complete,
                changed,
            },
        )
    }

    /// A run with every kind: commands that passed, failed, were evicted and printed a hostile
    /// label, approvals, calls, states, checkpoints and the exit, 3 older entries dropped.
    pub(crate) fn run() -> (Timeline, Head<'static>) {
        let api = |at, method, outcome, target| What::Api {
            seq: at,
            method,
            outcome,
            target,
        };
        let state = |status, reported| What::State { status, reported };
        let ask = |action, answer, other| What::Approval {
            action,
            answer,
            other,
            grant: 7,
        };
        let entries = [
            checkpoint(0, 0, 0, true, 0),
            entry(400, state(Status::Running, true)),
            command(2_000, 10, 2_300, 0),
            entry(5_000, api(5, 6, Outcome::Ok, Some(3))),
            entry(9_000, ask(ApprovalAction::Send, Answer::Once, Some(3))),
            command(15_000, 20, 400, 2),
            entry(21_000, api(21, 11, Outcome::Refused, Some(4))),
            entry(30_000, ask(ApprovalAction::Read, Answer::Denied, Some(5))),
            entry(42_000, state(Status::NeedsYou, false)),
            command(65_000, 2, 61_000, 0),
            checkpoint(130_000, 1, 1, false, 8),
            entry(140_000, state(Status::Error, true)),
            command(200_000, 30, 1_200, 0),
            checkpoint(260_000, 2, 5, true, 3),
            entry(3_700_000, What::Exit(Ended::Signal(9))),
        ];
        let (mut timeline, _) = Timeline::open(7);
        timeline.page(7, 0, (3, 40), &entries);
        timeline.start = 9 * 3600;
        timeline.local = utc;
        for (row, held, text) in [
            (10, true, "$ cargo build"),
            (20, true, "$ cargo test"),
            (2, false, ""),
            (30, true, HOSTILE),
        ] {
            timeline.label(7, row, held, &hold(text));
        }
        let head = Head {
            label: "api",
            tab: 1,
            status: Some((1, true)),
            run: Some(RunSummary {
                ended: true,
                commands: 4,
                failed: 1,
                approvals: 2,
                denied: 1,
                changed: 8,
                errors: 2,
            }),
        };
        (timeline, head)
    }

    pub(crate) fn text(frame: &Frame) -> String {
        let rows = frame.cells.chunks(usize::from(frame.columns));
        rows.map(|row| row.iter().map(|cell| cell.character).collect::<String>() + "\n")
            .collect()
    }

    fn drawn(timeline: &Timeline, head: &Head, columns: u16) -> Frame {
        let mut frame = pane(' ', 30, columns);
        timeline.draw(&mut frame, &GRAPHITE, head);
        frame
    }

    #[test]
    fn methods_match_the_cores() {
        let names = pinfold_core::timeline::METHODS.map(|(name, _)| name);
        assert_eq!(METHODS, names);
        assert_eq!(REASONS, pinfold_core::timeline::REASONS);
    }

    #[test]
    fn every_state_row_has_its_glyph_colour_and_word() {
        let (timeline, head) = run();
        let frame = drawn(&timeline, &head, 120);
        let all = text(&frame);
        for (state, said) in [
            (1, "● running  reported"),
            (0, "◆ needs you  from output"),
            (3, "✕ error  reported"),
        ] {
            let line = all
                .lines()
                .position(|line| line.contains(said))
                .expect(said);
            let column = all
                .lines()
                .nth(line)
                .unwrap()
                .chars()
                .position(|c| c == said.chars().next().unwrap())
                .unwrap();
            let cell = frame.cells[line * 120 + column];
            assert_eq!(
                Some(cell.foreground),
                top_bar::state_colour(&GRAPHITE, state),
                "{said}"
            );
        }
        assert!(all.contains("TIMELINE · api · tab 2 · pane 7 · ● running"));
        assert!(
            all.contains("4 commands · 2 approvals · 8 files changed · 2 errors · 3 older dropped")
        );
    }

    #[test]
    fn a_label_is_drawn_safe_and_an_evicted_row_says_so() {
        assert_eq!(hold(HOSTILE), "$ echo [31mok  needs you");
        let (timeline, head) = run();
        let all = text(&drawn(&timeline, &head, 120));
        assert!(!all.contains(['\u{1b}', '\u{202e}']));
        assert!(all.contains("$ echo [31mok  needs you"));
        let evicted = all.lines().find(|line| line.contains("1m01s")).unwrap();
        assert!(evicted.contains("evicted"), "{evicted}");
        // A label for a row the page's next command does not name is dropped.
        let (mut timeline, _) = run();
        let labels = timeline.labels.len();
        timeline.label(7, 99, true, "stray");
        timeline.label(8, 30, true, "other pane");
        assert_eq!(timeline.labels.len(), labels);
    }

    #[test]
    fn a_narrow_window_drops_the_label_then_the_time() {
        let (timeline, head) = run();
        let wide = text(&drawn(&timeline, &head, 120));
        assert!(wide.contains("09:00:02") && wide.contains("cargo build"));
        let narrow = text(&drawn(&timeline, &head, 60));
        assert!(narrow.contains("09:00:02") && !narrow.contains("cargo build"));
        let narrower = text(&drawn(&timeline, &head, 44));
        assert!(!narrower.contains("09:00:02") && narrower.contains("exit 0"));
    }

    #[test]
    fn keys_move_toggle_calls_skip_to_errors_and_act() {
        let key = |text: &str| Key::Character(text.into());
        let named = Key::Named;
        let (mut timeline, _) = run();
        // Opened on the newest, the exit; the `ok` call is hidden until `a`.
        assert_eq!(timeline.selected, 14);
        assert_eq!(timeline.visible().count(), 14);
        timeline.key(&key("a"));
        assert_eq!(timeline.visible().count(), 15);
        timeline.key(&key("a"));
        assert_eq!(timeline.key(&key("d")), Step::NoCheckpoint);
        assert_eq!(timeline.key(&named(NamedKey::Enter)), Step::Stay);
        // `p` goes back to the ✕ state, then the failed command.
        timeline.key(&key("p"));
        assert_eq!(timeline.selected, 11);
        timeline.key(&key("p"));
        assert_eq!(timeline.selected, 5);
        assert_eq!(timeline.key(&named(NamedKey::Enter)), Step::Reveal(20));
        assert_eq!(timeline.key(&key("d")), Step::Diff(1));
        timeline.key(&key("n"));
        assert_eq!(timeline.selected, 11);
        // The evicted command flashes, and a checkpoint opens the diff viewer at it.
        timeline.key(&named(NamedKey::ArrowUp));
        timeline.key(&named(NamedKey::ArrowUp));
        assert_eq!(timeline.selected, 9);
        assert_eq!(timeline.key(&named(NamedKey::Enter)), Step::Evicted);
        timeline.key(&named(NamedKey::ArrowDown));
        assert_eq!(timeline.key(&named(NamedKey::Enter)), Step::Diff(1));
        assert_eq!(timeline.key(&named(NamedKey::Escape)), Step::Close);
        assert_eq!(timeline.key(&key("x")), Step::Stay);
    }

    #[test]
    fn moving_past_the_page_asks_for_the_next_and_holds_one_page() {
        let (mut timeline, _) = run();
        let up = Key::Named(NamedKey::ArrowUp);
        // At the top of the page the older page is asked for, once until it comes.
        timeline.selected = 0;
        assert_eq!(timeline.key(&up), Step::Page(Some(25)));
        assert_eq!(timeline.key(&up), Step::Stay);
        timeline.page(
            7,
            0,
            (22, 40),
            &[
                command(0, 1, 5, 0),
                command(1, 1, 5, 0),
                command(2, 1, 5, 0),
            ],
        );
        assert_eq!((timeline.first, timeline.selected), (22, 2));
        assert!(timeline.labels.is_empty(), "labels go with their page");
        // The oldest page asks for nothing older; Down past its end asks for what follows.
        assert_eq!(timeline.key(&up), Step::Stay);
        timeline.selected = 2;
        assert_eq!(
            timeline.key(&Key::Named(NamedKey::ArrowDown)),
            Step::Page(None)
        );
        assert_eq!(timeline.key(&Key::Named(NamedKey::End)), Step::Stay);
        // A full page holds 64, and its labels 80 bytes each at most, within 6.2 KiB.
        let full = vec![command(0, 4, 5, 0); 200];
        timeline.page(7, 0, (0, 200), &full);
        assert_eq!(timeline.entries.len(), usize::from(MAX_PAGE));
        assert_eq!((timeline.first, timeline.selected), (136, 0));
        let wide = "𠀀".repeat(64);
        for _ in 0..200 {
            timeline.label(7, 4, true, &hold(&wide));
        }
        assert_eq!(timeline.labelled(), usize::from(MAX_PAGE));
        assert!(
            timeline
                .each_label()
                .all(|(_, text)| text.len() <= LABEL && text.ends_with('…'))
        );
        assert!(timeline.held_bytes() <= 6_348, "{}", timeline.held_bytes());
        assert_eq!(
            timeline.key(&Key::Named(NamedKey::Home)),
            Step::Page(Some(64))
        );
    }

    #[test]
    fn live_entries_append_on_the_newest_page_and_the_selection_follows_the_newest() {
        let (mut timeline, _) = run();
        assert_eq!(
            (timeline.first, timeline.entries.len(), timeline.selected),
            (25, 15, 14)
        );
        timeline.entry(7, 40, &command(0, 40, 5, 0));
        assert_eq!(
            (timeline.entries.len(), timeline.selected, timeline.total),
            (16, 15, 41)
        );
        timeline.label(7, 40, true, "$ ls");
        assert_eq!(timeline.label_of(15), Some((true, "$ ls")));
        // Off the newest row the selection stays; a repeat, another pane and a gap add nothing.
        timeline.selected = 4;
        timeline.entry(7, 41, &command(0, 41, 5, 0));
        timeline.entry(7, 41, &command(0, 41, 5, 0));
        timeline.entry(8, 42, &command(0, 42, 5, 0));
        assert_eq!((timeline.entries.len(), timeline.selected), (17, 4));
        timeline.entry(7, 50, &command(0, 50, 5, 0));
        assert_eq!((timeline.entries.len(), timeline.total), (17, 51));
        // Not the newest page now, so the next comes with a page down, and nothing appends.
        timeline.entry(7, 51, &command(0, 51, 5, 0));
        assert_eq!(timeline.entries.len(), 17);

        // A full page drops its oldest entry and that entry's label, and grows nothing.
        let full: Vec<_> = (0..64).map(|row| command(0, row, 5, 0)).collect();
        timeline.asked = Some((None, Want::Newest));
        timeline.page(7, 0, (0, 64), &full);
        for row in 0..64 {
            timeline.label(7, row, true, &hold(&"𠀀".repeat(64)));
        }
        let held = timeline.held_bytes();
        timeline.entry(7, 64, &command(0, 64, 5, 0));
        timeline.label(7, 64, true, "$ new");
        assert_eq!(
            (timeline.first, timeline.entries.len(), timeline.selected),
            (1, 64, 63)
        );
        assert_eq!(timeline.labelled(), 64);
        assert_eq!(
            timeline.label_of(0).map(|(_, text)| text.len()),
            Some(76 + 3)
        );
        assert_eq!(timeline.label_of(63), Some((true, "$ new")));
        assert_eq!(timeline.held_bytes(), held);
    }

    #[test]
    fn a_click_selects_a_row_a_second_acts_and_the_wheel_moves() {
        let (mut timeline, _) = run();
        let shown: Vec<usize> = timeline.visible().collect();
        let (top, left, _, _) = timeline.area(30, 120, shown.len()).unwrap();
        // The fifth row shown is the failed command.
        let at = (top + 2 + 4, left + 10);
        assert_eq!(timeline.click(30, 120, at), Step::Stay);
        assert_eq!(timeline.selected, 5);
        assert_eq!(timeline.click(30, 120, at), Step::Reveal(20));
        assert_eq!(timeline.click(30, 120, (0, 0)), Step::Close);
        assert_eq!(timeline.wheel(1), Step::Stay);
        assert_eq!(timeline.selected, 4);
        assert_eq!(timeline.wheel(-2), Step::Stay);
        assert_eq!(timeline.selected, 6);
    }
}
