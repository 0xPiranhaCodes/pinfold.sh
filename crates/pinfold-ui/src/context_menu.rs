//! The right-click menu, drawn in cells over the pane (ADR-0033, point 4).
//!
//! The window draws it after the panes, and while it is open every key and click goes to it,
//! so program output can neither fake it nor answer it. Each item runs a [`Command`], as a key
//! would, apart from the two a hyperlink adds at the end. Those can join after the menu shows,
//! so it keeps room for them from the start, and no item ever moves under the pointer.

use pinfold_config::theme::Theme;
use pinfold_proto::{Cell, Frame};
use winit::keyboard::{Key, NamedKey};

use crate::prefix::Command;

const ITEMS: [Command; 9] = [
    Command::Copy,
    Command::Paste,
    Command::SelectAll,
    Command::Find,
    Command::SplitRight,
    Command::SplitDown,
    Command::NewTab,
    Command::Zoom,
    Command::ClosePane,
];
const LINK_ITEMS: [&str; 2] = ["Open Link", "Copy Link"];
/// The longest label, a space either side and the border.
const WIDTH: u16 = 11 + 4;

/// What an item does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Run this, as its key would.
    Run(Command),
    /// Open the hyperlink's target.
    OpenLink(String),
    /// Put the hyperlink's target on the clipboard.
    CopyLink(String),
}

/// What a key or click did to the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// It stays open.
    Stay,
    /// It closes with nothing chosen.
    Close,
    /// It closes, and this was chosen.
    Chose(Choice),
}

/// An open menu.
#[derive(Debug)]
pub struct Menu {
    /// The top-left corner of its border, in the panes' grid.
    corner: (u16, u16),
    selected: usize,
    /// The target of the hyperlink right-clicked, if any.
    link: Option<String>,
}

impl Menu {
    /// Opens at the cell `at`, moved up and left as far as it takes to fit a grid of `area`
    /// `(rows, columns)`.
    pub fn open(at: (u16, u16), area: (u16, u16), link: Option<String>) -> Self {
        let height = (ITEMS.len() + LINK_ITEMS.len()) as u16 + 2;
        Self {
            corner: (
                at.0.min(area.0.saturating_sub(height)),
                at.1.min(area.1.saturating_sub(WIDTH)),
            ),
            selected: 0,
            link,
        }
    }

    /// Adds Open Link and Copy Link for `link` after the other items, once. The room for them
    /// was kept when the menu opened, so nothing drawn moves.
    pub fn link(&mut self, link: String) {
        self.link.get_or_insert(link);
    }

    fn len(&self) -> usize {
        ITEMS.len() + self.links()
    }

    /// How many items the hyperlink adds, which come last.
    fn links(&self) -> usize {
        if self.link.is_some() {
            LINK_ITEMS.len()
        } else {
            0
        }
    }

    fn label(&self, index: usize) -> &'static str {
        let label = match index.checked_sub(ITEMS.len()) {
            None => ITEMS.get(index).map(|command| command.label()),
            Some(index) => LINK_ITEMS.get(index).copied(),
        };
        label.unwrap_or_default()
    }

    fn choose(&mut self, index: usize) -> Step {
        if let Some(&command) = ITEMS.get(index) {
            return Step::Chose(Choice::Run(command));
        }
        let choice = match (self.link.take(), index - ITEMS.len()) {
            (Some(link), 0) => Choice::OpenLink(link),
            (Some(link), 1) => Choice::CopyLink(link),
            _ => return Step::Close,
        };
        Step::Chose(choice)
    }

    /// Takes a key press. Keys it has no use for are swallowed, as the menu holds the keyboard.
    pub fn key(&mut self, key: &Key) -> Step {
        let last = self.len() - 1;
        match key {
            Key::Named(NamedKey::ArrowUp) => {
                self.selected = self.selected.checked_sub(1).unwrap_or(last);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.selected = if self.selected == last {
                    0
                } else {
                    self.selected + 1
                };
            }
            Key::Named(NamedKey::Home) => self.selected = 0,
            Key::Named(NamedKey::End) => self.selected = last,
            Key::Named(NamedKey::Enter) => return self.choose(self.selected),
            Key::Named(NamedKey::Escape) => return Step::Close,
            _ => {}
        }
        Step::Stay
    }

    /// The item at the grid's `(row, column)`, if any.
    fn item_at(&self, (row, column): (u16, u16)) -> Option<usize> {
        let (top, left) = self.corner;
        let inside = (left + 1..left + WIDTH - 1).contains(&column);
        let index = usize::from(row.checked_sub(top + 1)?);
        (inside && index < self.len()).then_some(index)
    }

    /// Selects the item under the pointer, and returns whether that changed anything.
    pub fn hover(&mut self, at: (u16, u16)) -> bool {
        match self.item_at(at) {
            Some(index) if index != self.selected => {
                self.selected = index;
                true
            }
            _ => false,
        }
    }

    /// Takes a press at the grid's cell `at`, or off the grid if nothing. A press on an item
    /// chooses it, and anywhere else closes the menu.
    pub fn click(&mut self, at: Option<(u16, u16)>) -> Step {
        match at.and_then(|at| self.item_at(at)) {
            Some(index) => self.choose(index),
            None => Step::Close,
        }
    }

    /// Draws the menu over the panes' grid in `frame`, cut off where the grid ends.
    pub fn draw(&self, frame: &mut Frame, theme: &Theme) {
        let (text, chrome) = (theme.foreground, theme.chrome);
        let (ground, border) = (chrome.tab, chrome.key);
        let (top, left) = self.corner;
        let last = self.len() + 1;
        for line in 0..=last {
            let row = top + line as u16;
            if row >= frame.rows {
                return;
            }
            let item = line.checked_sub(1).filter(|&index| index < self.len());
            let selected = item == Some(self.selected);
            let (foreground, background) = if selected {
                (chrome.accent, chrome.tab_active)
            } else {
                (text, ground)
            };
            let label = item.map_or("", |index| self.label(index));
            let mut label = label.chars();
            for column in 0..WIDTH {
                let edge = column == 0 || column == WIDTH - 1;
                let (character, colour, ground) = match (line, column) {
                    (0, 0) => ('┌', border, ground),
                    (0, c) if c == WIDTH - 1 => ('┐', border, ground),
                    (l, 0) if l == last => ('└', border, ground),
                    (l, c) if l == last && c == WIDTH - 1 => ('┘', border, ground),
                    (0, _) => ('─', border, ground),
                    (l, _) if l == last => ('─', border, ground),
                    _ if edge => ('│', border, ground),
                    (_, 1) => (' ', foreground, background),
                    _ => (label.next().unwrap_or(' '), foreground, background),
                };
                put(frame, row, left + column, character, colour, ground);
            }
        }
    }
}

fn put(
    frame: &mut Frame,
    row: u16,
    column: u16,
    character: char,
    foreground: u32,
    background: u32,
) {
    if column >= frame.columns {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection;
    use pinfold_config::theme::GRAPHITE;
    use pinfold_proto::MouseMode;
    use winit::keyboard::ModifiersState;

    const LINK: &str = "https://pinfold.sh";

    fn named(menu: &mut Menu, key: NamedKey) -> Step {
        menu.key(&Key::Named(key))
    }

    /// The grid's rows after drawing `menu`, as characters.
    fn drawn(menu: &Menu, rows: u16, columns: u16) -> Vec<Vec<char>> {
        let mut frame = Frame::default();
        crate::panes::blank(&mut frame, rows, columns);
        menu.draw(&mut frame, &GRAPHITE);
        frame
            .cells
            .chunks(usize::from(columns))
            .map(|row| row.iter().map(|cell| cell.character).collect())
            .collect()
    }

    fn text(row: &[char], columns: std::ops::Range<usize>) -> String {
        row[columns].iter().collect()
    }

    #[test]
    fn opens_at_the_pointer_with_every_item() {
        let menu = Menu::open((2, 3), (24, 80), None);
        let rows = drawn(&menu, 24, 80);
        assert_eq!(text(&rows[2], 3..18), "┌─────────────┐");
        assert_eq!(text(&rows[3], 3..18), "│ Copy        │");
        assert_eq!(text(&rows[11], 3..18), "│ Close Pane  │");
        assert_eq!(text(&rows[12], 3..18), "└─────────────┘");
        let labels: Vec<_> = (0..menu.len()).map(|index| menu.label(index)).collect();
        assert!(!labels.contains(&"Open Link"));
        assert_eq!(labels.len(), 9);
        assert_eq!(text(&rows[10], 3..18), "│ Zoom Pane   │");
    }

    #[test]
    fn every_label_fits_the_width() {
        let labels = ITEMS.map(Command::label).into_iter().chain(LINK_ITEMS);
        for label in labels {
            assert!(!label.is_empty());
            assert!(label.chars().count() + 4 <= usize::from(WIDTH), "{label}");
        }
    }

    #[test]
    fn moves_in_to_fit_and_survives_a_tiny_grid() {
        let menu = Menu::open((23, 79), (24, 80), None);
        assert_eq!(menu.corner, (11, 65), "room kept for the link items");
        let rows = drawn(&menu, 24, 80);
        assert_eq!(text(&rows[21], 65..80), "└─────────────┘");
        let tiny = Menu::open((1, 1), (3, 4), Some(LINK.into()));
        assert_eq!(tiny.corner, (0, 0));
        let rows: Vec<String> = drawn(&tiny, 3, 4)
            .iter()
            .map(|row| text(row, 0..4))
            .collect();
        assert_eq!(rows, ["┌───", "│ Co", "│ Pa"]);
    }

    #[test]
    fn arrows_move_and_wrap_and_enter_runs_the_item() {
        let mut menu = Menu::open((0, 0), (24, 80), None);
        assert_eq!(named(&mut menu, NamedKey::ArrowUp), Step::Stay);
        let close = Step::Chose(Choice::Run(Command::ClosePane));
        assert_eq!(named(&mut menu, NamedKey::Enter), close);
        let mut menu = Menu::open((0, 0), (24, 80), None);
        named(&mut menu, NamedKey::End);
        named(&mut menu, NamedKey::ArrowDown);
        named(&mut menu, NamedKey::ArrowDown);
        let paste = Step::Chose(Choice::Run(Command::Paste));
        assert_eq!(named(&mut menu, NamedKey::Enter), paste);
    }

    #[test]
    fn escape_closes_and_other_keys_are_swallowed() {
        let mut menu = Menu::open((0, 0), (24, 80), None);
        assert_eq!(menu.key(&Key::Character("c".into())), Step::Stay);
        assert_eq!(named(&mut menu, NamedKey::Tab), Step::Stay);
        assert_eq!(named(&mut menu, NamedKey::Escape), Step::Close);
    }

    #[test]
    fn the_mouse_hovers_and_chooses_and_a_press_elsewhere_closes() {
        let mut menu = Menu::open((5, 10), (24, 80), None);
        assert!(menu.hover((7, 12)));
        assert!(!menu.hover((7, 12)));
        assert!(!menu.hover((5, 12)), "the border is no item");
        let find = Step::Chose(Choice::Run(Command::Find));
        assert_eq!(menu.click(Some((9, 20))), find);
        let mut menu = Menu::open((5, 10), (24, 80), None);
        assert_eq!(menu.click(Some((9, 10))), Step::Close);
        assert_eq!(menu.click(Some((1, 1))), Step::Close);
        assert_eq!(menu.click(None), Step::Close);
    }

    #[test]
    fn a_hyperlink_adds_open_and_copy_link_last() {
        let menu = Menu::open((0, 0), (24, 80), Some(LINK.into()));
        let rows = drawn(&menu, 24, 80);
        assert_eq!(text(&rows[1], 0..15), "│ Copy        │");
        assert_eq!(text(&rows[10], 0..15), "│ Open Link   │");
        assert_eq!(text(&rows[11], 0..15), "│ Copy Link   │");
        assert_eq!(text(&rows[12], 0..15), "└─────────────┘");
        let mut menu = Menu::open((0, 0), (24, 80), Some(LINK.into()));
        named(&mut menu, NamedKey::ArrowUp);
        assert_eq!(
            named(&mut menu, NamedKey::Enter),
            Step::Chose(Choice::CopyLink(LINK.into()))
        );
        let mut menu = Menu::open((0, 0), (24, 80), Some(LINK.into()));
        let open = Step::Chose(Choice::OpenLink(LINK.into()));
        assert_eq!(menu.click(Some((10, 4))), open);
        let mut menu = Menu::open((0, 0), (24, 80), Some(LINK.into()));
        let copy = Step::Chose(Choice::Run(Command::Copy));
        assert_eq!(menu.click(Some((1, 4))), copy);
    }

    /// A link learned after the menu shows must not move what the pointer is on.
    #[test]
    fn a_link_learned_late_moves_nothing_already_drawn() {
        let mut menu = Menu::open((20, 0), (24, 80), None);
        named(&mut menu, NamedKey::ArrowDown);
        let before = drawn(&menu, 24, 80);
        menu.link(LINK.into());
        menu.link("https://other".into());
        let after = drawn(&menu, 24, 80);
        assert_eq!(menu.corner, (11, 0));
        assert_eq!(before[11..21], after[11..21], "the items above stay put");
        assert_eq!(text(&after[21], 0..15), "│ Open Link   │");
        assert_eq!(text(&after[22], 0..15), "│ Copy Link   │");
        let paste = Step::Chose(Choice::Run(Command::Paste));
        assert_eq!(named(&mut menu, NamedKey::Enter), paste);
        let mut menu = Menu::open((0, 0), (24, 80), None);
        menu.link(LINK.into());
        assert_eq!(
            menu.click(Some((10, 4))),
            Step::Chose(Choice::OpenLink(LINK.into()))
        );
    }

    /// The window opens the menu on a right press only when the press is not the program's.
    #[test]
    fn mouse_reporting_takes_the_click_unless_shift_is_held() {
        let none = ModifiersState::empty();
        assert!(selection::owns_press(MouseMode::Off, none));
        for mode in [MouseMode::Click, MouseMode::Drag, MouseMode::Motion] {
            assert!(!selection::owns_press(mode, none), "{mode:?}");
            assert!(
                selection::owns_press(mode, ModifiersState::SHIFT),
                "{mode:?}"
            );
        }
    }
}
