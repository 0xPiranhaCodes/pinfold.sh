//! Desktop notifications that an agent pane needs you or hit an error (ADR-0046, ADR-0051). The
//! text is PinFold's, "`<tab>` needs you" or "`<tab>` error", never what a program printed, and
//! they are rate-limited per pane and across all of them.

use std::time::{Duration, Instant};

use crate::top_bar::STATES;

/// The least time between two notifications for one pane.
pub const PER_PANE: Duration = Duration::from_secs(60);
/// The least time between any two notifications.
pub const ANY: Duration = Duration::from_secs(10);

/// When notifications were last raised, in all and for each pane within the last minute.
#[derive(Default)]
struct Limit {
    last: Option<Instant>,
    panes: Vec<(u32, Instant)>,
}

impl Limit {
    /// Whether `pane` may notify at `now`, and records it if so. One a pane a minute and one
    /// in 10 s keep at most 6 entries.
    fn allow(&mut self, pane: u32, now: Instant) -> bool {
        let soon = |last: Instant, gap| now.saturating_duration_since(last) < gap;
        let held = self.panes.iter().position(|(at, _)| *at == pane);
        if self.last.is_some_and(|last| soon(last, ANY))
            || held.is_some_and(|held| soon(self.panes[held].1, PER_PANE))
        {
            return false;
        }
        self.last = Some(now);
        self.panes.retain(|(_, last)| soon(*last, PER_PANE));
        self.panes.push((pane, now));
        true
    }
}

/// Raises the notifications, through the platform where it has a way.
pub struct Notifier {
    limit: Limit,
    #[cfg(target_os = "macos")]
    system: Option<pinfold_sys::notify::Notifier>,
}

impl Notifier {
    /// Sets up the platform's notifications. `clicked` gets the pane whose notification was
    /// clicked.
    // shortcut: Linux has no notifications yet, only the tab's mark; `notify-send` comes with
    // ADR-0051's fourth slice.
    pub fn new(clicked: impl Fn(u32) + 'static) -> Self {
        #[cfg(not(target_os = "macos"))]
        drop(clicked);
        #[cfg(target_os = "macos")]
        let clicked = move |pane: usize| {
            if let Ok(pane) = u32::try_from(pane) {
                clicked(pane);
            }
        };
        Self {
            limit: Limit::default(),
            #[cfg(target_os = "macos")]
            system: pinfold_sys::notify::Notifier::new(clicked),
        }
    }

    /// Notifies that `pane`, in the tab at index `tab` labelled `label`, came to `state`, unless
    /// the limits hold it back. The caller has checked that the config asks for this state and
    /// that the person cannot see the pane. `label` is already safe to draw (`top_bar::label`).
    pub fn alert(
        &mut self,
        (pane, state): (u32, u8),
        (tab, label): (Option<usize>, Option<&str>),
        now: Instant,
    ) {
        if self.limit.allow(pane, now) {
            self.show(&text(state, tab, label), pane);
        }
    }

    /// Returns how many panes notified within the last minute.
    #[cfg(test)]
    pub(crate) fn raised(&self) -> usize {
        self.limit.panes.len()
    }

    #[cfg(target_os = "macos")]
    fn show(&self, text: &str, pane: u32) {
        if let Some(system) = &self.system {
            system.notify(text, pane as usize);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[expect(clippy::unused_self, reason = "the same interface as on macOS")]
    fn show(&self, _text: &str, _pane: u32) {}
}

/// A notification's text: the tab's label, or its number without one, and the state's word.
/// Nothing a program printed.
fn text(state: u8, tab: Option<usize>, label: Option<&str>) -> String {
    let word = STATES
        .get(usize::from(state))
        .map_or("needs you", |(_, word)| word);
    match (label.filter(|label| !label.is_empty()), tab) {
        (Some(label), _) => format!("{label} {word}"),
        (None, Some(tab)) => format!("Tab {} {word}", tab + 1),
        (None, None) => format!("An agent {word}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::top_bar::{ERROR, NEEDS_YOU};

    #[test]
    fn the_text_is_pinfolds_own_word_and_label() {
        assert_eq!(text(NEEDS_YOU, Some(0), Some("api")), "api needs you");
        assert_eq!(text(ERROR, Some(0), Some("api")), "api error");
        assert_eq!(text(ERROR, Some(2), Some("")), "Tab 3 error");
        assert_eq!(text(NEEDS_YOU, Some(0), None), "Tab 1 needs you");
        assert_eq!(text(ERROR, None, None), "An agent error");
        assert_eq!(
            text(200, None, Some("api")),
            "api needs you",
            "a state out of range"
        );
    }

    #[test]
    fn a_flood_of_alerts_notifies_once_a_pane_a_minute_and_once_in_ten_seconds() {
        let now = Instant::now();
        let mut limit = Limit::default();
        let mut raised = Vec::new();
        // Ten panes flap needs you and error every millisecond for two minutes.
        for step in 0_u32..120_000 {
            let at = now + Duration::from_millis(u64::from(step));
            if limit.allow(step % 10, at) {
                raised.push(step);
            }
            assert!(limit.panes.len() <= 6);
        }
        // One every 10 s over 120 s, and each pane at most twice.
        assert_eq!(raised.len(), 12);
        for pane in 0..10 {
            assert!(raised.iter().filter(|step| *step % 10 == pane).count() <= 2);
        }
        let mut alone = Limit::default();
        assert!(alone.allow(7, now));
        assert!(!alone.allow(7, now + ANY), "inside the pane's minute");
        assert!(alone.allow(8, now + ANY), "another pane, keyed apart");
        assert!(alone.allow(7, now + PER_PANE));
    }
}
