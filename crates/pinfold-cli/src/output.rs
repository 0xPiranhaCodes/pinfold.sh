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
/// Most of an unfinished line held back from another pane's subscribers. Past it, it goes out
/// as a line.
const MAX_HELD_LINE: usize = 4096;
/// Most of a PEM block held back until its end line. Past it, the block is dropped.
const MAX_HELD_BLOCK: usize = 64 * 1024;

/// Redacts a line of a pane that is not the caller's own, for a match wait on it.
#[must_use]
pub fn scrub(line: &str) -> String {
    redact(line).into_owned()
}

/// What each pane printed and has not yet gone to other panes' subscribers, so they get whole
/// lines and whole PEM blocks, which redaction then sees whole however the reads split them.
#[derive(Debug, Default)]
pub struct Held {
    panes: Vec<(usize, Lines)>,
}

#[derive(Debug, Default)]
struct Lines {
    text: String,
    /// Inside a PEM block too long to hold, until its end line.
    dropping: bool,
}

impl Lines {
    /// Adds `text` and returns what may go out: whole lines, with a PEM block only once its end
    /// line has come.
    fn whole(&mut self, text: &str) -> String {
        self.text.push_str(text);
        let (mut out, mut sent, mut at) = (String::new(), 0, 0);
        let mut open = false;
        while let Some(end) = self.text[at..].find('\n').map(|end| at + end + 1) {
            let line = &self.text[at..end];
            if self.dropping {
                self.dropping = !line.contains("-----END ");
                sent = end;
            } else {
                open |= line.contains("-----BEGIN ");
                open &= !line.contains("-----END ");
                if !open {
                    out.push_str(&self.text[sent..end]);
                    sent = end;
                }
            }
            at = end;
        }
        self.text.drain(..sent);
        if open && self.text.len() > MAX_HELD_BLOCK {
            self.text.clear();
            self.dropping = true;
            out.push_str("[redacted]\n");
        } else if !open && self.text.len() > MAX_HELD_LINE {
            if !self.dropping {
                out.push_str(&self.text);
                out.push('\n');
            }
            self.text.clear();
        }
        out
    }
}

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
/// subscriber that reaches it: at once to the caller whose pane it is, and in whole lines,
/// redacted, to the others, holding the rest in `held`.
pub fn hand_out<T>(
    reactor: &mut Reactor,
    family: &mut Family,
    waits: &mut Waits<T>,
    subscribers: &[(Subscriber, Topics)],
    held: &mut Held,
) {
    let panes: Vec<usize> = reactor.list().map(|pane| pane.id).collect();
    held.panes.retain(|(pane, _)| panes.contains(pane));
    for pane in panes {
        let Some(text) = reactor.take_text(pane).filter(|text| !text.is_empty()) else {
            continue;
        };
        waits.feed(pane, &text);
        let reach = |subscriber| Some((subscriber, reaches(reactor, family, subscriber, pane)?));
        let reached: Vec<_> = subscribers.iter().filter_map(reach).collect();
        let index = held.panes.iter().position(|entry| entry.0 == pane);
        let whole = match (reached.iter().any(|(_, own)| !own), index) {
            (false, index) => {
                index.map(|index| held.panes.swap_remove(index));
                String::new()
            }
            (true, Some(index)) => held.panes[index].1.whole(&text),
            (true, None) => {
                let mut lines = Lines::default();
                let whole = lines.whole(&text);
                held.panes.push((pane, lines));
                whole
            }
        };
        for (subscriber, own) in reached {
            let text = if own { &text } else { &whole };
            events(pane, text, own)
                .into_iter()
                .for_each(|event| subscriber.0.send(event));
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
    use std::time::{Duration, Instant};

    use pinfold_core::wait::{Outcome, Until};

    use super::*;

    #[test]
    fn a_secret_split_across_takes_goes_out_whole_and_redacted() {
        let mut lines = Lines::default();
        assert_eq!(lines.whole("ok\npassword=hun"), "ok\n");
        let whole = lines.whole("ter2\nnext");
        assert_eq!(
            events(1, &whole, false)[0]["text"],
            "password=«redacted:password»\n"
        );
        // A line past the cap goes out as one.
        let long = lines.whole(&"x".repeat(MAX_HELD_LINE));
        assert_eq!(long, format!("next{}\n", "x".repeat(MAX_HELD_LINE)));
    }

    #[test]
    fn a_pem_block_split_across_takes_goes_out_whole_and_redacted_or_not_at_all() {
        let mut lines = Lines::default();
        let body = "MIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\n";
        assert_eq!(lines.whole("key:\n-----BEGIN RSA PRIV"), "key:\n");
        assert_eq!(lines.whole("ATE KEY-----\n"), "");
        assert_eq!(lines.whole(body), "");
        let whole = lines.whole("-----END RSA PRIVATE KEY-----\ndone\n");
        let text = events(1, &whole, false)[0]["text"].clone();
        assert_eq!(text, "«redacted:private-key»\ndone\n");

        // Too long to hold, a block is dropped up to its end line.
        assert_eq!(lines.whole("-----BEGIN EC PRIVATE KEY-----\n"), "");
        let dropped = lines.whole(&body.repeat(MAX_HELD_BLOCK / body.len() + 1));
        assert_eq!(dropped, "[redacted]\n");
        assert_eq!(lines.whole(body), "");
        assert_eq!(
            lines.whole("-----END EC PRIVATE KEY-----\nafter\n"),
            "after\n"
        );
    }

    #[test]
    fn a_match_on_another_pane_never_sees_a_secret() {
        let mut reactor = Reactor::new().unwrap();
        let pane = reactor
            .spawn(std::process::Command::new("cat"), 24, 80)
            .unwrap();
        let mut waits = Waits::default();
        let later = Instant::now() + Duration::from_secs(30);
        for (ticket, pattern) in [(0, "ghp_a"), (1, "token=«")] {
            let until = Until::matching(pattern, Some(scrub)).unwrap();
            assert!(waits.park(&reactor, ticket, pane, until, later).is_ok());
        }
        let token = format!("token=ghp_{}", "a".repeat(36));
        waits.feed(pane, &format!("{token}\n{token}"));
        let settled = waits.settle(&reactor, Instant::now());
        let line = "token=«redacted:password»".to_owned();
        assert_eq!(settled, [(1, Outcome::Matched(line))]);
        assert_eq!(waits.settle(&reactor, Instant::now()), []);
    }

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
