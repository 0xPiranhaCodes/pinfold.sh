//! What panes print, for `until: "match"` waits and `pane.output` events. The core taps only
//! the panes a match wait or an output subscriber can reach, and hands each its text.

use std::borrow::Cow;

use pinfold_api::control::{Subscriber, Topics};
use pinfold_api::redact;
use pinfold_core::family::Family;
use pinfold_core::io::Reactor;
use pinfold_core::wait::Waits;
use serde_json::{Value, json};

/// Most bytes of text one `pane.output` event carries.
pub const MAX_CHUNK: usize = 4096;

/// Taps each pane a match wait or an output subscriber can reach, and no other. Call it before
/// the reactor's turn, so the text after a wait begins is kept.
///
/// # Examples
///
/// ```
/// use pinfold_core::{family::Family, io::Reactor, wait::Waits};
///
/// let mut reactor = Reactor::new()?;
/// let pane = reactor.spawn(std::process::Command::new("cat"), 24, 80)?;
/// let waits: Waits<()> = Waits::default();
/// pinfold_cli::output::tap(&mut reactor, &mut Family::default(), &waits, &[]);
/// assert_eq!(reactor.take_text(pane), None);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn tap<T>(
    reactor: &mut Reactor,
    family: &mut Family,
    waits: &Waits<T>,
    subscribers: &[(Subscriber, Topics)],
) {
    let panes: Vec<usize> = reactor.list().map(|pane| pane.id).collect();
    for pane in panes {
        let heard = subscribers
            .iter()
            .any(|subscriber| reaches(reactor, family, subscriber, pane).is_some());
        reactor.tap(pane, heard || waits.wants_text(pane));
    }
}

/// Hands what each tapped pane printed to its match waits, and as `pane.output` events to each
/// subscriber that reaches it.
pub fn hand_out<T>(
    reactor: &mut Reactor,
    family: &mut Family,
    waits: &mut Waits<T>,
    subscribers: &[(Subscriber, Topics)],
) {
    let panes: Vec<usize> = reactor.list().map(|pane| pane.id).collect();
    for pane in panes {
        let Some(text) = reactor.take_text(pane).filter(|text| !text.is_empty()) else {
            continue;
        };
        waits.feed(pane, &text);
        for subscriber in subscribers {
            if let Some(own) = reaches(reactor, family, subscriber, pane) {
                events(pane, &text, own)
                    .into_iter()
                    .for_each(|event| subscriber.0.send(event));
            }
        }
    }
}

/// Whether an output subscriber reaches `pane`, and if so whether it is the caller's own.
fn reaches(
    reactor: &Reactor,
    family: &mut Family,
    (subscriber, topics): &(Subscriber, Topics),
    pane: usize,
) -> Option<bool> {
    let own = subscriber
        .caller()
        .and_then(|caller| reactor.pane_of(caller));
    let reached = topics.output && family.reach(reactor, own, pane).is_ok();
    reached.then_some(own == Some(pane))
}

/// The `pane.output` events for `text` that `pane` printed, at most [`MAX_CHUNK`] bytes of text
/// each, redacted as a whole first unless the pane is the caller's `own`.
///
/// # Examples
///
/// ```
/// use pinfold_cli::output::events;
///
/// let other = events(3, "password=hunter2\n", false);
/// assert_eq!(other[0]["text"], "password=«redacted:password»\n");
/// assert_eq!(events(3, "password=hunter2\n", true)[0]["text"], "password=hunter2\n");
/// ```
#[must_use]
pub fn events(pane: usize, text: &str, own: bool) -> Vec<Value> {
    let text = if own {
        Cow::Borrowed(text)
    } else {
        redact(text)
    };
    let mut rest = &*text;
    let mut events = Vec::new();
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_CHUNK);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (chunk, tail) = rest.split_at(end);
        events.push(json!({"event": "pane.output", "pane": pane, "text": chunk}));
        rest = tail;
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_cut_on_character_boundaries_into_4_kib_events() {
        let text = format!("a{}", "é".repeat(MAX_CHUNK));
        let events = events(1, &text, true);
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|event| event["text"].as_str())
            .collect();
        assert_eq!(texts.concat(), text);
        assert!(texts.iter().all(|chunk| chunk.len() <= MAX_CHUNK));
        assert_eq!(texts.len(), 3);
        assert!(events.iter().all(|event| event["pane"] == 1));
    }
}
