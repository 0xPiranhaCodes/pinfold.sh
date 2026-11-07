//! Notifications that a tab needs you (ADR-0045). The text is PinFold's, "<tab> needs you",
//! never what a program printed, and they are rate-limited per tab and across all of them.

use std::time::{Duration, Instant};

use crate::top_bar::NEEDS_YOU;

/// The least time between two notifications for one tab.
pub const PER_TAB: Duration = Duration::from_secs(60);
/// The least time between any two notifications.
pub const ANY: Duration = Duration::from_secs(10);

/// When notifications were last raised, in all and for each tab's pane.
#[derive(Default)]
struct Limit {
    last: Option<Instant>,
    tabs: Vec<(u32, Instant)>,
}

impl Limit {
    /// Whether the tab whose pane is `pane` may notify at `now`, and records it if so.
    fn allow(&mut self, pane: u32, now: Instant) -> bool {
        let soon = |last: Instant, gap| now.saturating_duration_since(last) < gap;
        let tab = self.tabs.iter().position(|(at, _)| *at == pane);
        if self.last.is_some_and(|last| soon(last, ANY))
            || tab.is_some_and(|tab| soon(self.tabs[tab].1, PER_TAB))
        {
            return false;
        }
        self.last = Some(now);
        self.tabs.retain(|(_, last)| soon(*last, PER_TAB));
        self.tabs.push((pane, now));
        true
    }
}

/// The tabs whose state has just become needs you, and that the person cannot see: every one
/// while the window lacks focus, or else the hidden ones.
fn due<'a>(
    old: &'a [u8],
    new: &'a [u8],
    active: usize,
    focused: bool,
) -> impl Iterator<Item = usize> + 'a {
    (0..new.len()).filter(move |&index| {
        new[index] == NEEDS_YOU
            && old.get(index) != Some(&NEEDS_YOU)
            && (!focused || index != active)
    })
}

/// Raises the notifications, through the platform where it has a way.
pub struct Notifier {
    limit: Limit,
    #[cfg(target_os = "macos")]
    system: Option<pinfold_sys::notify::Notifier>,
}

impl Notifier {
    /// Sets up the platform's notifications. `clicked` gets the index of the tab whose
    /// notification was clicked.
    // shortcut: Linux has no notifications yet, only the tab's mark; a freedesktop one needs a
    // D-Bus client crate, so add one when Linux users ask (ADR-0045).
    pub fn new(clicked: impl Fn(usize) + 'static) -> Self {
        #[cfg(not(target_os = "macos"))]
        drop(clicked);
        Self {
            limit: Limit::default(),
            #[cfg(target_os = "macos")]
            system: pinfold_sys::notify::Notifier::new(clicked),
        }
    }

    /// Notifies for each tab that has just come to need you unseen. `labels` are already safe
    /// to draw (`top_bar::label`), and `pane` gives a tab's pane, which keys the limit.
    // shortcut: a click goes to the tab's index when it was raised, so a tab closed in between
    // shifts it. Key it by pane if that bites.
    pub fn changed(&mut self, tabs: Tabs<'_>, pane: impl Fn(usize) -> u32, now: Instant) {
        for index in due(tabs.old, tabs.new, tabs.active, tabs.focused) {
            if !self.limit.allow(pane(index), now) {
                continue;
            }
            #[cfg(target_os = "macos")]
            if let Some(system) = &self.system {
                system.notify(&text(tabs.labels.get(index), index), index);
            }
        }
    }
}

/// A notification's text, from the tab's label alone, or its number without one.
fn text(label: Option<&String>, index: usize) -> String {
    match label.filter(|label| !label.is_empty()) {
        Some(label) => format!("{label} needs you"),
        None => format!("Tab {} needs you", index + 1),
    }
}

/// The tabs before and after a layout, for [`Notifier::changed`].
pub struct Tabs<'a> {
    /// Each tab's state before, as its pill showed it.
    pub old: &'a [u8],
    /// Each tab's state now.
    pub new: &'a [u8],
    /// The tabs' labels, made safe to draw.
    pub labels: &'a [String],
    /// The active tab.
    pub active: usize,
    /// Whether the window has the keyboard.
    pub focused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_tab_that_just_needs_you_unseen_is_due() {
        let (old, new) = ([1, NEEDS_YOU, 1, 1], [NEEDS_YOU, NEEDS_YOU, NEEDS_YOU, 1]);
        assert_eq!(due(&old, &new, 0, true).collect::<Vec<_>>(), [2]);
        assert_eq!(due(&old, &new, 0, false).collect::<Vec<_>>(), [0, 2]);
        assert_eq!(due(&[], &[NEEDS_YOU], 0, false).count(), 1, "a new tab");
    }

    #[test]
    fn the_text_is_pinfolds_own() {
        assert_eq!(text(Some(&"api".to_owned()), 0), "api needs you");
        assert_eq!(text(Some(&String::new()), 2), "Tab 3 needs you");
        assert_eq!(text(None, 0), "Tab 1 needs you");
    }

    #[test]
    fn a_flood_of_needs_you_notifies_once_a_tab_a_minute_and_once_in_ten_seconds() {
        let now = Instant::now();
        let mut limit = Limit::default();
        let mut raised = Vec::new();
        // Ten panes flap needs you every millisecond for two minutes.
        for step in 0_u32..120_000 {
            let at = now + Duration::from_millis(u64::from(step));
            if limit.allow(step % 10, at) {
                raised.push(step);
            }
        }
        // One every 10 s over 120 s, and each pane at most twice.
        assert_eq!(raised.len(), 12);
        for pane in 0..10 {
            assert!(raised.iter().filter(|step| *step % 10 == pane).count() <= 2);
        }
        let mut alone = Limit::default();
        assert!(alone.allow(7, now));
        assert!(!alone.allow(7, now + ANY), "inside a tab's minute");
        assert!(alone.allow(7, now + PER_TAB));
    }
}
