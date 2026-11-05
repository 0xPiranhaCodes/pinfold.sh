//! The macOS menu bar's items (ADR-0033). Its key equivalents are the only home of Command
//! shortcuts on macOS, so the key handler leaves those keys alone there.

use std::cell::Cell;
use std::rc::Rc;

use crate::prefix::Command;

/// What decides which items are greyed out, as of the last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Linux has no menu bar")
)]
pub struct State {
    pub selection: bool,
    pub tabs: usize,
    pub panes: usize,
    /// Whether the text can grow, shrink, or is off the config's size.
    pub bigger: bool,
    pub smaller: bool,
    pub resized: bool,
}

/// Whether `command` can apply in `state`.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "Linux has no menu bar")
)]
fn enabled(command: Command, state: State) -> bool {
    match command {
        Command::Copy => state.selection,
        Command::SelectAll | Command::ClosePane | Command::Zoom => state.panes > 0,
        Command::NextTab | Command::PreviousTab => state.tabs > 1,
        Command::Tab(index) => index < state.tabs,
        Command::FontBigger => state.bigger,
        Command::FontSmaller => state.smaller,
        Command::FontReset => state.resized,
        _ => true,
    }
}

/// The menu bar's item for `command`, under its shared label.
#[cfg(target_os = "macos")]
const fn run(command: Command, key: &'static str) -> pinfold_sys::menu::Item<Command> {
    (
        command.label(),
        key,
        pinfold_sys::menu::Action::Run(command),
    )
}

#[cfg(target_os = "macos")]
const MENUS: &[pinfold_sys::menu::Menu<Command>] = {
    use pinfold_sys::menu::Action::{About, FullScreen, Hide, Minimize, Run, Separator};
    &[
        (
            "PinFold",
            &[
                ("About PinFold", "", About),
                run(Command::OpenConfig, ","),
                ("", "", Separator),
                ("Hide PinFold", "h", Hide),
                ("Quit PinFold", "q", Run(Command::Detach)),
            ],
        ),
        (
            "Shell",
            &[
                run(Command::NewTab, "t"),
                run(Command::SplitRight, "d"),
                run(Command::SplitDown, "D"),
                run(Command::ClosePane, "w"),
                ("", "", Separator),
                run(Command::Detach, ""),
            ],
        ),
        (
            "Edit",
            &[
                run(Command::Copy, "c"),
                run(Command::Paste, "v"),
                run(Command::SelectAll, "a"),
                ("", "", Separator),
                run(Command::Find, "f"),
            ],
        ),
        (
            "View",
            &[
                run(Command::Zoom, ""),
                ("", "", Separator),
                // `=`, not `+`, so the key needs no Shift on most layouts.
                run(Command::FontBigger, "="),
                run(Command::FontSmaller, "-"),
                run(Command::FontReset, "0"),
                ("", "", Separator),
                ("Toggle Full Screen", "", FullScreen),
            ],
        ),
        (
            "Window",
            &[
                ("Minimize", "m", Minimize),
                ("", "", Separator),
                run(Command::NextTab, "}"),
                run(Command::PreviousTab, "{"),
                ("", "", Separator),
                ("Tab 1", "1", Run(Command::Tab(0))),
                ("Tab 2", "2", Run(Command::Tab(1))),
                ("Tab 3", "3", Run(Command::Tab(2))),
                ("Tab 4", "4", Run(Command::Tab(3))),
                ("Tab 5", "5", Run(Command::Tab(4))),
                ("Tab 6", "6", Run(Command::Tab(5))),
                ("Tab 7", "7", Run(Command::Tab(6))),
                ("Tab 8", "8", Run(Command::Tab(7))),
                ("Tab 9", "9", Run(Command::Tab(8))),
            ],
        ),
        ("Help", &[run(Command::OpenDocs, "?")]),
    ]
};

/// The menu bar, and the state its items are greyed by. Nothing on Linux.
#[derive(Default)]
pub struct Bar {
    state: Rc<Cell<State>>,
    #[cfg(target_os = "macos")]
    _menu: Option<pinfold_sys::menu::MenuBar>,
}

impl Bar {
    /// Shows the menu bar, which hands each chosen item's command to `run`.
    #[cfg(target_os = "macos")]
    pub fn install(run: impl Fn(Command) + 'static) -> Self {
        let state = Rc::new(Cell::new(State::default()));
        let shared = state.clone();
        let check = move |command| enabled(command, shared.get());
        let menu = pinfold_sys::menu::install(MENUS, run, check);
        Self { state, _menu: menu }
    }

    /// Shows the menu bar, which Linux does not have.
    #[cfg(not(target_os = "macos"))]
    pub fn install(_run: impl Fn(Command) + 'static) -> Self {
        Self::default()
    }

    /// Records what the items are greyed by.
    pub fn update(&self, state: State) {
        self.state.set(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_that_cannot_apply_are_greyed() {
        let none = State::default();
        for command in [
            Command::Copy,
            Command::SelectAll,
            Command::NextTab,
            Command::Tab(0),
        ] {
            assert!(!enabled(command, none), "{command:?}");
        }
        for command in [
            Command::FontBigger,
            Command::FontSmaller,
            Command::FontReset,
        ] {
            assert!(!enabled(command, none), "{command:?}");
        }
        for command in [
            Command::Paste,
            Command::Find,
            Command::NewTab,
            Command::OpenDocs,
        ] {
            assert!(enabled(command, none), "{command:?}");
        }
        let two = State {
            selection: true,
            tabs: 2,
            panes: 1,
            ..none
        };
        assert!(enabled(Command::Copy, two));
        assert!(enabled(Command::NextTab, two));
        assert!(enabled(Command::Tab(1), two));
        assert!(!enabled(Command::Tab(2), two));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn no_two_items_share_a_key() {
        let mut keys: Vec<&str> = MENUS
            .iter()
            .flat_map(|(_, items)| items.iter())
            .map(|(_, key, _)| *key)
            .filter(|key| !key.is_empty())
            .collect();
        let count = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), count);
    }
}
