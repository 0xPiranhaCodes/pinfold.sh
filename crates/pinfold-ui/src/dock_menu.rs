//! The macOS Dock menu (ADR-0033): New Tab, Split Right, Split Down, Detach, then one item per
//! tab that goes to it, with the tab's agent state as glyph and word. AppKit asks for it each
//! time the Dock icon is right-clicked, so it is built from the tabs as of the last layout.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use pinfold_sys::menu::Action;

use crate::prefix::Command;
use crate::top_bar::STATES;

const FIXED: [Command; 4] = [
    Command::NewTab,
    Command::SplitRight,
    Command::SplitDown,
    Command::Detach,
];

/// Each tab's label, made safe to draw, and its state from `top_bar::tab_state`.
type Tabs = (Vec<String>, Vec<u8>);

/// The Dock menu's items for `tabs`. A label holds no state glyph (`top_bar::label`), so a
/// program cannot make its tab read as another state.
fn items((labels, states): &Tabs) -> Vec<(String, Action<Command>)> {
    let fixed = FIXED.map(|command| (command.label().to_owned(), Action::Run(command)));
    let tabs = labels
        .iter()
        .zip(states)
        .enumerate()
        .map(|(index, (label, state))| {
            let (glyph, word, _) = STATES[usize::from(*state).min(STATES.len() - 1)];
            (
                format!("{glyph} {label}  {word}"),
                Action::Run(Command::Tab(index)),
            )
        });
    let separator = (!labels.is_empty()).then(|| (String::new(), Action::Separator));
    fixed.into_iter().chain(separator).chain(tabs).collect()
}

/// The tabs the Dock menu shows.
pub struct Dock {
    tabs: Rc<RefCell<Tabs>>,
}

impl Dock {
    /// Gives the app a Dock menu, which hands each chosen item's command to `run`. If winit's
    /// delegate cannot take one, says so and carries on without it.
    pub fn install(run: impl Fn(Command) + 'static) -> Self {
        let tabs = Rc::new(RefCell::new(Tabs::default()));
        let shared = tabs.clone();
        let built = move || {
            shared
                .try_borrow()
                .map(|tabs| items(&tabs))
                .unwrap_or_default()
        };
        if let Err(reason) = pinfold_sys::menu::install_dock(built, run) {
            let _ = writeln!(io::stderr(), "pinfold: no Dock menu: {reason:?}");
        }
        Self { tabs }
    }

    /// Records the tabs, reusing the last copy's memory.
    pub fn update(&self, labels: &[String], states: &[u8]) {
        if let Ok(mut tabs) = self.tabs.try_borrow_mut() {
            let (names, kinds) = &mut *tabs;
            names.resize_with(labels.len(), String::new);
            for (name, label) in names.iter_mut().zip(labels) {
                name.clone_from(label);
            }
            kinds.clear();
            kinds.extend_from_slice(states);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_follow_the_fixed_items_with_glyph_and_word() {
        let tabs = (vec!["api".to_owned(), "docs".to_owned()], vec![1, 0]);
        let titles: Vec<String> = items(&tabs).into_iter().map(|(title, _)| title).collect();
        assert_eq!(
            titles,
            [
                "New Tab",
                "Split Right",
                "Split Down",
                "Detach",
                "",
                "● api  running",
                "◆ docs  needs you"
            ]
        );
        let none = items(&Tabs::default());
        assert_eq!(none.len(), FIXED.len(), "no separator without tabs");
    }

    #[test]
    fn each_item_runs_its_command() {
        let tabs = (
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            vec![2, 3, 9],
        );
        let actions: Vec<Action<Command>> =
            items(&tabs).into_iter().map(|(_, action)| action).collect();
        assert_eq!(
            actions,
            [
                Action::Run(Command::NewTab),
                Action::Run(Command::SplitRight),
                Action::Run(Command::SplitDown),
                Action::Run(Command::Detach),
                Action::Separator,
                Action::Run(Command::Tab(0)),
                Action::Run(Command::Tab(1)),
                Action::Run(Command::Tab(2)),
            ]
        );
        let last = items(&tabs).pop().map(|(title, _)| title);
        assert_eq!(
            last.as_deref(),
            Some("○ c  idle"),
            "an unknown state reads as idle"
        );
    }

    #[test]
    fn update_copies_the_tabs() {
        let dock = Dock {
            tabs: Rc::default(),
        };
        dock.update(&["x".to_owned(), "y".to_owned()], &[1, 2]);
        dock.update(&["z".to_owned()], &[4]);
        assert_eq!(*dock.tabs.borrow(), (vec!["z".to_owned()], vec![4]));
    }
}
