//! The command palette, Ctrl+Shift+P, drawn in cells over the panes (`docs/design/ui/`).
//!
//! As with the right-click menu, the window draws it after the panes and every key goes to it
//! while it is open, so program output can neither fake it nor answer it. It filters a list of
//! labels by what is typed and returns the index of the one chosen, so the window decides what
//! that runs.

use pinfold_proto::{Cell, Frame};
use unicode_width::UnicodeWidthChar;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::prefix::Command;

/// The palette's surfaces, from `docs/design/ui/Foundations.dc.html`.
const GROUND: u32 = 0x0015_1920;
const BORDER: u32 = 0x0034_3a44;
const TEXT: u32 = 0x00e2_e5ea;
const HINT: u32 = 0x009a_a1ac;
const SELECTED: u32 = 0x0025_3c3b;
const ACCENT: u32 = 0x005c_d6cb;

/// The widest the palette draws, border included.
const WIDTH: u16 = 48;
/// The longest query, in characters.
const MAX_QUERY: usize = 64;
const CLOSE_HINT: &str = "esc close";

/// The window's commands, as the palette names them. Focus, resize and a tab by number are
/// keys only, as in the menus.
pub const COMMANDS: [(&str, Command); 20] = [
    ("Open Markdown", Command::OpenMarkdown),
    ("Open Files", Command::OpenFiles),
    ("Open Diff", Command::OpenDiff),
    ("New Tab", Command::NewTab),
    ("Split Right", Command::SplitRight),
    ("Split Down", Command::SplitDown),
    ("Close Pane", Command::ClosePane),
    ("Next Tab", Command::NextTab),
    ("Previous Tab", Command::PreviousTab),
    ("Zoom Pane", Command::Zoom),
    ("Find", Command::Find),
    ("Copy", Command::Copy),
    ("Paste", Command::Paste),
    ("Select All", Command::SelectAll),
    ("Bigger Text", Command::FontBigger),
    ("Smaller Text", Command::FontSmaller),
    ("Actual Size", Command::FontReset),
    ("Settings", Command::OpenConfig),
    ("PinFold Help", Command::OpenDocs),
    ("Detach", Command::Detach),
];

/// What a key did to the palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// It stays open.
    Stay,
    /// It closes with nothing chosen.
    Close,
    /// It closes, and the label at this index was chosen.
    Chose(usize),
}

/// An open palette.
#[derive(Debug)]
pub struct Palette {
    labels: Vec<String>,
    query: String,
    /// Indexes into `labels` that match the query, best first.
    shown: Vec<usize>,
    /// Index into `shown`.
    selected: usize,
    /// What it says when it has no labels at all.
    empty: &'static str,
}

impl Palette {
    /// Opens on these labels, all shown in order.
    pub fn open(labels: Vec<String>) -> Self {
        let shown = (0..labels.len()).collect();
        Self {
            labels,
            query: String::new(),
            shown,
            selected: 0,
            empty: "no match",
        }
    }

    /// Opens on `*.md` names from a [`pinfold_proto::link::Notice::Markdown`], one a line.
    pub fn markdown(names: &str) -> Self {
        let names = names.lines().filter(|name| !name.is_empty());
        Self {
            empty: "no .md files here",
            ..Self::open(names.map(str::to_owned).collect())
        }
    }

    /// The label at `index`, as it opened with it.
    pub fn label(&self, index: usize) -> Option<&str> {
        self.labels.get(index).map(String::as_str)
    }

    /// Opens on the window's [`COMMANDS`].
    pub fn commands() -> Self {
        Self::open(
            COMMANDS
                .iter()
                .map(|(label, _)| (*label).to_owned())
                .collect(),
        )
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        let mut ranked: Vec<_> = (self.labels.iter().enumerate())
            .filter_map(|(index, label)| Some((rank(&label.to_lowercase(), &query)?, index)))
            .collect();
        ranked.sort_unstable();
        self.shown = ranked.into_iter().map(|(_, index)| index).collect();
        self.selected = 0;
    }

    /// Takes a key press and the text it typed. Keys it has no use for are swallowed, as the
    /// palette holds the keyboard.
    pub fn key(&mut self, key: &Key, text: Option<&str>, modifiers: ModifiersState) -> Step {
        let last = self.shown.len().saturating_sub(1);
        match key {
            Key::Named(NamedKey::Escape) => return Step::Close,
            Key::Named(NamedKey::Enter) => {
                return self
                    .shown
                    .get(self.selected)
                    .map_or(Step::Stay, |&index| Step::Chose(index));
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
            Key::Named(NamedKey::Backspace) => {
                if self.query.pop().is_some() {
                    self.filter();
                }
            }
            _ if modifiers.control_key() || modifiers.super_key() => {}
            _ => {
                let room = MAX_QUERY.saturating_sub(self.query.chars().count());
                let text = text.unwrap_or_default().chars().filter(|c| !c.is_control());
                let length = self.query.len();
                self.query.extend(text.take(room));
                if self.query.len() != length {
                    self.filter();
                }
            }
        }
        Step::Stay
    }

    /// Draws the palette near the top of the panes' grid in `frame`, centred, with the cursor
    /// after the query. Rows that do not fit are cut off, the selected one kept in view.
    pub fn draw(&self, frame: &mut Frame) {
        let width = WIDTH.min(frame.columns);
        if width < 4 || frame.rows < 3 {
            return;
        }
        let left = (frame.columns - width) / 2;
        let top = u16::from(frame.rows > 6);
        let room = usize::from(frame.rows - top - 3);
        let lines = self.shown.len().clamp(1, room.max(1));
        let first = self.selected.saturating_sub(lines - 1);
        let right = left + width - 1;
        let bottom = top + 2 + lines as u16;
        for row in top..=bottom {
            for column in left..=right {
                let character = match (row == top || row == bottom, column == left, column == right)
                {
                    (true, true, _) if row == top => '┌',
                    (true, _, true) if row == top => '┐',
                    (true, true, _) => '└',
                    (true, _, true) => '┘',
                    (true, ..) => '─',
                    (false, true, _) | (false, _, true) => '│',
                    _ => ' ',
                };
                put(frame, row, column, character, BORDER, GROUND);
            }
        }
        let end = usize::from(right);
        let at = write(
            frame,
            top + 1,
            usize::from(left) + 2,
            "❯ ",
            ACCENT,
            GROUND,
            end,
        );
        let cursor = write(frame, top + 1, at, &self.query, TEXT, GROUND, end);
        let hint = end.saturating_sub(CLOSE_HINT.len() + 1);
        if hint > cursor {
            write(frame, top + 1, hint, CLOSE_HINT, HINT, GROUND, end);
        }
        frame.cursor = Some((top + 1, cursor.min(end - 1) as u16));
        if self.shown.is_empty() {
            write(
                frame,
                top + 2,
                usize::from(left) + 2,
                if self.labels.is_empty() {
                    self.empty
                } else {
                    "no match"
                },
                HINT,
                GROUND,
                end,
            );
        }
        for (line, &index) in self.shown.iter().enumerate().skip(first).take(lines) {
            let row = top + 2 + (line - first) as u16;
            let (colour, ground) = if line == self.selected {
                (ACCENT, SELECTED)
            } else {
                (TEXT, GROUND)
            };
            for column in left + 1..right {
                put(frame, row, column, ' ', colour, ground);
            }
            write(
                frame,
                row,
                usize::from(left) + 2,
                &self.labels[index],
                colour,
                ground,
                end - 1,
            );
        }
    }
}

/// How well `label` matches `query`, both lower case, best first: a prefix, then anywhere,
/// then its letters in order. Nothing if it does not.
fn rank(label: &str, query: &str) -> Option<u8> {
    if label.starts_with(query) {
        return Some(0);
    }
    if label.contains(query) {
        return Some(1);
    }
    let mut letters = label.chars();
    let query = query.chars().filter(|c| !c.is_whitespace());
    query
        .into_iter()
        .all(|c| letters.any(|l| l == c))
        .then_some(2)
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
        column += width;
    }
    column
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ModifiersState = ModifiersState::empty();

    fn typed(palette: &mut Palette, text: &str) -> Step {
        palette.key(&Key::Character(text.into()), Some(text), NONE)
    }

    fn named(palette: &mut Palette, key: NamedKey) -> Step {
        palette.key(&Key::Named(key), None, NONE)
    }

    #[test]
    fn typing_each_label_chooses_its_command() {
        for (index, (label, _)) in COMMANDS.iter().enumerate() {
            let mut palette = Palette::commands();
            assert_eq!(typed(&mut palette, label), Step::Stay);
            assert_eq!(
                named(&mut palette, NamedKey::Enter),
                Step::Chose(index),
                "{label}"
            );
        }
    }

    #[test]
    fn it_ranks_a_prefix_first_then_anywhere_then_letters_in_order() {
        let mut palette = Palette::commands();
        typed(&mut palette, "pa");
        let shown: Vec<_> = palette
            .shown
            .iter()
            .map(|&index| COMMANDS[index].0)
            .collect();
        assert_eq!(
            shown,
            [
                "Paste",
                "Close Pane",
                "Zoom Pane",
                "Open Markdown",
                "Previous Tab"
            ]
        );
        typed(&mut palette, "x");
        assert!(palette.shown.is_empty());
        assert_eq!(named(&mut palette, NamedKey::Enter), Step::Stay);
        named(&mut palette, NamedKey::Backspace);
        // Up from the first wraps to the last.
        named(&mut palette, NamedKey::ArrowUp);
        assert_eq!(named(&mut palette, NamedKey::Enter), Step::Chose(8));
    }

    #[test]
    fn it_keeps_control_characters_and_ctrl_keys_out_and_caps_the_query() {
        let mut palette = Palette::commands();
        typed(&mut palette, "\u{1b}[2J");
        assert_eq!(palette.query, "[2J");
        let ctrl = palette.key(
            &Key::Character("v".into()),
            Some("v"),
            ModifiersState::CONTROL,
        );
        assert_eq!(ctrl, Step::Stay);
        assert_eq!(palette.query, "[2J");
        typed(&mut palette, &"a".repeat(100));
        assert_eq!(palette.query.chars().count(), MAX_QUERY);
        assert_eq!(named(&mut palette, NamedKey::Escape), Step::Close);
    }

    #[test]
    fn it_fits_a_small_grid_and_keeps_the_selection_in_view() {
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, 5, 20);
        let mut palette = Palette::commands();
        named(&mut palette, NamedKey::ArrowUp);
        palette.draw(&mut frame);
        let row = |row: usize| -> String {
            frame.cells[row * 20..(row + 1) * 20]
                .iter()
                .map(|cell| cell.character)
                .collect()
        };
        assert_eq!(row(0), "┌──────────────────┐");
        assert_eq!(row(1), "│ ❯      esc close │");
        assert_eq!(row(2), "│ PinFold Help     │");
        assert_eq!(row(3), "│ Detach           │");
        assert_eq!(row(4), "└──────────────────┘");
        assert_eq!(frame.cursor, Some((1, 4)));
    }
}
