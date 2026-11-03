//! What panes print, for `until: "match"` waits and `pane.output` events. Both reach only the
//! caller's own pane, so no streamed text needs redaction. The core taps only the panes a match
//! wait or an output subscriber is on, and hands each its text.

use pinfold_api::control::{Subscriber, Topics};
use pinfold_core::io::Reactor;
use pinfold_core::wait::Waits;
use serde_json::{Value, json};

/// Most bytes of text one `pane.output` event carries.
pub const MAX_CHUNK: usize = 4096;

/// Taps each pane a match wait or an output subscriber is on, and no other. Call it before the
/// reactor's turn, so the text after a wait begins is kept.
///
/// # Examples
///
/// ```
/// use pinfold_core::{io::Reactor, wait::Waits};
///
/// let mut reactor = Reactor::new()?;
/// let pane = reactor.spawn(std::process::Command::new("cat"), 24, 80)?;
/// let waits: Waits<()> = Waits::default();
/// pinfold_cli::output::tap(&mut reactor, &waits, &[]);
/// assert_eq!(reactor.take_text(pane), None);
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn tap<T>(reactor: &mut Reactor, waits: &Waits<T>, subscribers: &[(Subscriber, Topics)]) {
    let panes: Vec<usize> = reactor.list().map(|pane| pane.id).collect();
    for pane in panes {
        let heard = subscribers
            .iter()
            .any(|subscriber| owns(reactor, subscriber, pane));
        reactor.tap(pane, heard || waits.wants_text(pane));
    }
}

/// Hands what each tapped pane printed to its match waits, and as `pane.output` events to the
/// subscribers whose own pane it is.
pub fn hand_out<T>(
    reactor: &mut Reactor,
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
            if owns(reactor, subscriber, pane) {
                events(pane, &text)
                    .into_iter()
                    .for_each(|event| subscriber.0.send(event));
            }
        }
    }
}

/// Whether an output subscriber's own pane is `pane`.
fn owns(reactor: &Reactor, (subscriber, topics): &(Subscriber, Topics), pane: usize) -> bool {
    let own = subscriber
        .caller()
        .and_then(|caller| reactor.pane_of(caller));
    topics.output && own == Some(pane)
}

/// The `pane.output` events for `text` that `pane` printed, at most [`MAX_CHUNK`] bytes of text
/// each, cut on character boundaries.
///
/// # Examples
///
/// ```
/// use pinfold_cli::output::events;
///
/// assert_eq!(events(3, "done\n")[0]["text"], "done\n");
/// ```
#[must_use]
pub fn events(pane: usize, text: &str) -> Vec<Value> {
    let mut rest = text;
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
        let events = events(1, &text);
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
