//! Prefix keys: one key arms the window, and the next key is a command.
//!
//! The prefix defaults to Ctrl+a, as `docs/design/architecture.md` sets, and `keys.prefix` in
//! the config changes it. After the prefix:
//!
//! | Keys                | Command                 | From                       |
//! | ------------------- | ----------------------- | -------------------------- |
//! | `c`                 | New tab                 | tmux                       |
//! | `x`                 | Close the pane          | tmux                       |
//! | `n`, `p`            | Next, previous tab      | tmux                       |
//! | `0` to `9`          | Tab at that index       | tmux, so `0` is the first  |
//! | <code>&#124;</code> or `%` | Split right      | the design doc, then tmux  |
//! | `-` or `"`          | Split down              | the design doc, then tmux  |
//! | Arrows              | Move focus              | tmux                       |
//! | Ctrl+arrows         | Resize by 1 cell        | tmux                       |
//! | Alt+arrows          | Resize by 5 cells       | tmux                       |
//! | `z`                 | Zoom                    | tmux                       |
//! | `d`                 | Detach                  | tmux                       |
//!
//! Alt+arrows matter on macOS, where the desktop takes Ctrl+arrows. `h` `j` `k` `l` stay free,
//! because the design keeps `l` for the audit log, as it keeps `u`, `A`, `f` and `g`.
//!
//! The prefix pressed twice passes the second press through, so the pane gets it as it would
//! any key. Any other key after the prefix cancels and is swallowed. There is no timeout: the
//! design names none, and a timer would be idle work.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// How far Alt+arrow moves a border, as tmux does.
const LARGE_RESIZE: u16 = 5;

/// A direction to move focus or a border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Towards column 0.
    Left,
    /// Away from column 0.
    Right,
    /// Towards row 0.
    Up,
    /// Away from row 0.
    Down,
}

/// What the window does for a bound key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open a tab after the others.
    NewTab,
    /// Close the focused pane.
    ClosePane,
    /// Go to the next tab.
    NextTab,
    /// Go to the previous tab.
    PreviousTab,
    /// Go to the tab at this index, counted from 0.
    Tab(usize),
    /// Split the focused pane, with the new pane on the right.
    SplitRight,
    /// Split the focused pane, with the new pane below.
    SplitDown,
    /// Move focus to the pane in this direction.
    Focus(Direction),
    /// Move the focused pane's border in this direction, by this many cells.
    Resize(Direction, u16),
    /// Toggle whether the focused pane fills its tab.
    Zoom,
    /// Close the window and leave the session running.
    Detach,
}

/// What to do with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// It is the pane's: encode and send it as usual.
    Pass,
    /// Drop it. The prefix armed or cancelled, or a modifier was pressed while armed.
    Swallow,
    /// Drop it and run this.
    Run(Command),
}

fn direction(key: NamedKey) -> Option<Direction> {
    Some(match key {
        NamedKey::ArrowLeft => Direction::Left,
        NamedKey::ArrowRight => Direction::Right,
        NamedKey::ArrowUp => Direction::Up,
        NamedKey::ArrowDown => Direction::Down,
        _ => return None,
    })
}

/// Whether this key is only a modifier, which winit reports as a key press of its own.
fn is_modifier(key: &Key) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::CapsLock
                | NamedKey::Control
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::Hyper
                | NamedKey::Meta
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::Shift
                | NamedKey::Super
                | NamedKey::Symbol
                | NamedKey::SymbolLock
        )
    )
}

/// The command for a key after the prefix, or nothing if the key is unbound.
fn command(key: &Key, modifiers: ModifiersState) -> Option<Command> {
    const NONE: ModifiersState = ModifiersState::empty();
    match key {
        Key::Named(named) => {
            let direction = direction(*named)?;
            match modifiers {
                NONE => Some(Command::Focus(direction)),
                ModifiersState::CONTROL => Some(Command::Resize(direction, 1)),
                ModifiersState::ALT => Some(Command::Resize(direction, LARGE_RESIZE)),
                _ => None,
            }
        }
        Key::Character(_) if modifiers.control_key() || modifiers.super_key() => None,
        // Shift and Alt are not checked: some layouts need Option to type `|`, as a German
        // Mac does, and winit has already applied both to the text.
        Key::Character(text) => match text.as_bytes() {
            [digit @ b'0'..=b'9'] => Some(Command::Tab(usize::from(digit - b'0'))),
            b"c" => Some(Command::NewTab),
            b"x" => Some(Command::ClosePane),
            b"n" => Some(Command::NextTab),
            b"p" => Some(Command::PreviousTab),
            b"|" | b"%" => Some(Command::SplitRight),
            b"-" | b"\"" => Some(Command::SplitDown),
            b"z" => Some(Command::Zoom),
            b"d" => Some(Command::Detach),
            _ => None,
        },
        _ => None,
    }
}

/// The prefix key, and whether it is armed.
// shortcut: a prefix is Ctrl and a letter as the layout reports it, so Ctrl+a on a Cyrillic layout
// is not the prefix. Compare physical keys if people on those layouts miss it.
// shortcut: each command needs the prefix again, with no repeat-time as in tmux. Add it if
// resizing a step at a time grates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefix {
    letter: char,
    armed: bool,
    /// Whether the key last taken was the prefix's, so its repeats stay out of the pane.
    consumed: bool,
}

impl Prefix {
    /// Starts unarmed, with Ctrl and `letter` as the prefix, as `keys.prefix` gives it.
    #[must_use]
    pub const fn new(letter: char) -> Self {
        Self {
            letter,
            armed: false,
            consumed: false,
        }
    }

    fn is_prefix(&self, key: &Key, modifiers: ModifiersState) -> bool {
        let mut letter = [0; 4];
        let letter = self.letter.encode_utf8(&mut letter);
        modifiers == ModifiersState::CONTROL
            && matches!(key, Key::Character(text) if text.eq_ignore_ascii_case(letter))
    }

    /// Takes a key press and says what to do with it. Releases must not reach this.
    ///
    /// `repeat` is whether the press is the key's auto-repeat.
    ///
    /// ```
    /// use pinfold_ui::prefix::{Command, Prefix, Step};
    /// use winit::keyboard::{Key, ModifiersState};
    ///
    /// let mut prefix = Prefix::new('a');
    /// let none = ModifiersState::empty();
    /// let key = |text: &str| Key::Character(text.into());
    /// assert_eq!(prefix.key(&key("q"), none, false), Step::Pass);
    /// let armed = prefix.key(&key("a"), ModifiersState::CONTROL, false);
    /// assert_eq!(armed, Step::Swallow);
    /// assert_eq!(prefix.key(&key("z"), none, false), Step::Run(Command::Zoom));
    /// ```
    pub fn key(&mut self, key: &Key, modifiers: ModifiersState, repeat: bool) -> Step {
        let is_prefix = self.is_prefix(key, modifiers);
        // A held prefix would flip between arming and sending, and leave either at random.
        // A held command key would run once and then type into the pane.
        if repeat && (is_prefix || self.consumed) {
            return Step::Swallow;
        }
        // Shift comes as a press of its own on the way to `%` or `|`.
        if is_modifier(key) && (self.armed || self.consumed) {
            return Step::Swallow;
        }
        self.consumed = false;
        if !self.armed {
            self.armed = is_prefix;
            return if is_prefix { Step::Swallow } else { Step::Pass };
        }
        self.armed = false;
        if is_prefix {
            return Step::Pass;
        }
        self.consumed = true;
        command(key, modifiers).map_or(Step::Swallow, Step::Run)
    }

    /// Disarms, for when the window loses focus, so the first key back is not a command.
    pub const fn cancel(&mut self) {
        self.armed = false;
        self.consumed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input;

    const NONE: ModifiersState = ModifiersState::empty();
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const ALT: ModifiersState = ModifiersState::ALT;
    const CONTROL: ModifiersState = ModifiersState::CONTROL;
    const SUPER: ModifiersState = ModifiersState::SUPER;

    fn text(text: &str) -> Key {
        Key::Character(text.into())
    }

    /// Returns a prefix armed with Ctrl+a.
    fn armed() -> Prefix {
        let mut prefix = Prefix::new('a');
        assert_eq!(prefix.key(&text("a"), CONTROL, false), Step::Swallow);
        prefix
    }

    /// Asserts that `key` after the prefix gives `step` and leaves the prefix unarmed.
    fn assert_after_prefix(key: &Key, modifiers: ModifiersState, step: Step) {
        let mut prefix = armed();
        assert_eq!(prefix.key(key, modifiers, false), step, "{key:?}");
        assert_eq!(prefix.key(&text("c"), NONE, false), Step::Pass, "{key:?}");
    }

    #[test]
    fn keys_pass_until_the_prefix() {
        let mut prefix = Prefix::new('a');
        for (key, modifiers) in [
            (text("a"), NONE),
            (text("c"), NONE),
            (text("b"), CONTROL),
            (text("a"), CONTROL | SHIFT),
            (text("a"), CONTROL | ALT),
            (text("a"), CONTROL | SUPER),
            (Key::Named(NamedKey::ArrowUp), NONE),
            (Key::Named(NamedKey::Shift), SHIFT),
        ] {
            assert_eq!(prefix.key(&key, modifiers, false), Step::Pass, "{key:?}");
        }
    }

    #[test]
    fn letters_and_symbols_run_their_commands() {
        for (key, command) in [
            ("c", Command::NewTab),
            ("x", Command::ClosePane),
            ("n", Command::NextTab),
            ("p", Command::PreviousTab),
            ("|", Command::SplitRight),
            ("%", Command::SplitRight),
            ("-", Command::SplitDown),
            ("\"", Command::SplitDown),
            ("z", Command::Zoom),
            ("d", Command::Detach),
        ] {
            assert_after_prefix(&text(key), NONE, Step::Run(command));
        }
    }

    #[test]
    fn digits_pick_a_tab_by_index() {
        for digit in 0..10 {
            let command = Command::Tab(digit);
            assert_after_prefix(&text(&digit.to_string()), NONE, Step::Run(command));
        }
    }

    #[test]
    fn arrows_move_focus_and_resize() {
        for (key, direction) in [
            (NamedKey::ArrowLeft, Direction::Left),
            (NamedKey::ArrowRight, Direction::Right),
            (NamedKey::ArrowUp, Direction::Up),
            (NamedKey::ArrowDown, Direction::Down),
        ] {
            let key = Key::Named(key);
            assert_after_prefix(&key, NONE, Step::Run(Command::Focus(direction)));
            assert_after_prefix(&key, CONTROL, Step::Run(Command::Resize(direction, 1)));
            assert_after_prefix(&key, ALT, Step::Run(Command::Resize(direction, 5)));
        }
    }

    #[test]
    fn the_prefix_twice_sends_it_to_the_pane() {
        let mut prefix = armed();
        let key = text("a");
        assert_eq!(prefix.key(&key, CONTROL, false), Step::Pass);
        assert_eq!(input::encode(&key, None, CONTROL, false, false), [1]);
        assert_eq!(prefix.key(&text("c"), NONE, false), Step::Pass);
    }

    #[test]
    fn unbound_keys_cancel_and_are_swallowed() {
        for (key, modifiers) in [
            (text("q"), NONE),
            (text("C"), SHIFT),
            (text("é"), NONE),
            (text("c"), CONTROL),
            (text("c"), SUPER),
            (Key::Named(NamedKey::Escape), NONE),
            (Key::Named(NamedKey::Enter), NONE),
            (Key::Named(NamedKey::F1), NONE),
            (Key::Named(NamedKey::ArrowUp), SHIFT),
            (Key::Named(NamedKey::ArrowUp), SUPER),
            (Key::Named(NamedKey::ArrowUp), CONTROL | ALT),
            (Key::Dead(Some('`')), NONE),
        ] {
            assert_after_prefix(&key, modifiers, Step::Swallow);
        }
    }

    #[test]
    fn a_modifier_on_the_way_to_a_binding_stays_armed() {
        let mut prefix = armed();
        let shift = Key::Named(NamedKey::Shift);
        assert_eq!(prefix.key(&shift, SHIFT, false), Step::Swallow);
        let split = Step::Run(Command::SplitRight);
        assert_eq!(prefix.key(&text("%"), SHIFT, false), split);
    }

    #[test]
    fn option_can_type_a_binding() {
        assert_after_prefix(&text("|"), ALT, Step::Run(Command::SplitRight));
    }

    #[test]
    fn caps_lock_does_not_hide_the_prefix() {
        let mut prefix = Prefix::new('a');
        assert_eq!(prefix.key(&text("A"), CONTROL, false), Step::Swallow);
        assert_eq!(prefix.key(&text("A"), CONTROL, false), Step::Pass);
    }

    #[test]
    fn the_prefix_letter_comes_from_the_config() {
        let mut prefix = Prefix::new('b');
        assert_eq!(prefix.key(&text("a"), CONTROL, false), Step::Pass);
        assert_eq!(prefix.key(&text("b"), CONTROL, false), Step::Swallow);
        assert_eq!(
            prefix.key(&text("z"), NONE, false),
            Step::Run(Command::Zoom)
        );
    }

    #[test]
    fn a_held_prefix_arms_once() {
        let mut prefix = armed();
        for _ in 0..3 {
            assert_eq!(prefix.key(&text("a"), CONTROL, true), Step::Swallow);
        }
        assert_eq!(
            prefix.key(&text("z"), NONE, false),
            Step::Run(Command::Zoom)
        );
    }

    #[test]
    fn repeats_of_other_keys_pass() {
        let mut prefix = Prefix::new('a');
        assert_eq!(prefix.key(&text("q"), NONE, true), Step::Pass);
    }

    #[test]
    fn repeats_of_a_key_the_prefix_took_stay_out_of_the_pane() {
        for key in [text("c"), text("q"), Key::Named(NamedKey::ArrowUp)] {
            let mut prefix = armed();
            assert_ne!(prefix.key(&key, NONE, false), Step::Pass, "{key:?}");
            assert_eq!(prefix.key(&key, NONE, true), Step::Swallow, "{key:?}");
            assert_eq!(prefix.key(&key, NONE, true), Step::Swallow, "{key:?}");
            assert_eq!(prefix.key(&key, NONE, false), Step::Pass, "{key:?}");
            assert_eq!(prefix.key(&key, NONE, true), Step::Pass, "{key:?}");
        }
    }

    #[test]
    fn cancel_disarms() {
        let mut prefix = armed();
        prefix.cancel();
        assert_eq!(prefix.key(&text("x"), NONE, false), Step::Pass);
    }
}
