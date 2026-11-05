//! Time-bounded grants and parked cross-pane requests (threat model, Authorization).
//!
//! A grant is exactly one caller pane, target pane and action. Requests for the same triple
//! share one prompt while it is pending, under a new id each time one joins. The caller's ticket stays here until a real window
//! key answers it, the window goes away, or the prompt reaches its deadline.

use std::time::{Duration, Instant};

use pinfold_proto::link::{ApprovalAction, ApprovalAnswer};

/// How long an approved grant lives.
pub const GRANT_TIME: Duration = Duration::from_secs(30 * 60);
/// How long the person gets to answer a prompt.
pub const PROMPT_TIME: Duration = Duration::from_secs(60);
/// Most requests waiting for approval at once, matching the parked-wait cap.
pub const MAX_PENDING: usize = 64;

/// Supplies time, so expiry tests do not wait on a wall clock.
pub trait Clock {
    /// The current monotonic time.
    fn now(&self) -> Instant;
}

/// The process monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    caller: usize,
    target: usize,
    action: ApprovalAction,
}

/// A prompt to draw in the window. `texts` holds every exact send in this batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// Stable id used by the window's answer and by a lasting grant.
    pub id: u64,
    /// Pane asking.
    pub caller: usize,
    /// Pane being read or sent to.
    pub target: usize,
    /// The requested operation.
    pub action: ApprovalAction,
    /// Exact sends in request order; empty for a read prompt.
    pub texts: Vec<String>,
}

struct Parked<T> {
    id: u64,
    key: Key,
    deadline: Instant,
    tickets: Vec<T>,
    texts: Vec<String>,
}

struct Grant {
    id: u64,
    key: Key,
    expires: Instant,
}

/// One active grant, as `grants.list` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Stable grant id.
    pub id: u64,
    /// Caller pane.
    pub caller: usize,
    /// Target pane.
    pub target: usize,
    /// Granted action.
    pub action: ApprovalAction,
    /// Time left at the instant of the list.
    pub remaining: Duration,
}

/// What checking one request decided.
pub enum Decision<T> {
    /// A live grant covers it.
    Allow(T),
    /// It is parked and the window should draw or update this prompt.
    Ask(Prompt),
    /// It cannot be parked.
    Refuse(T, &'static str),
}

/// Tickets released by an answer, timeout or lost window.
pub struct Released<T> {
    /// The original caller request.
    pub ticket: T,
    /// Whether to carry it out.
    pub allowed: bool,
    /// Refusal text when it was not allowed.
    pub reason: &'static str,
}

/// Active grants and requests parked on prompts.
pub struct Grants<T, C = SystemClock> {
    clock: C,
    next_id: u64,
    grants: Vec<Grant>,
    pending: Vec<Parked<T>>,
}

impl<T> Default for Grants<T, SystemClock> {
    fn default() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl<T, C: Clock> Grants<T, C> {
    /// Makes an empty store using `clock`.
    pub fn with_clock(clock: C) -> Self {
        Self {
            clock,
            next_id: 1,
            grants: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Allows with a live grant, or parks and asks. `text` is present only for sends.
    pub fn decide(
        &mut self,
        ticket: T,
        caller: usize,
        target: usize,
        action: ApprovalAction,
        text: Option<String>,
        window_ready: bool,
    ) -> Decision<T> {
        let now = self.clock.now();
        self.grants.retain(|grant| grant.expires > now);
        let key = Key {
            caller,
            target,
            action,
        };
        if self.grants.iter().any(|grant| grant.key == key) {
            return Decision::Allow(ticket);
        }
        if let Some(refused) = self.full(window_ready) {
            return Decision::Refuse(ticket, refused);
        }
        let id = self.next_id();
        if let Some(pending) = self.pending.iter_mut().find(|pending| pending.key == key) {
            // A fresh id, so a key pressed for the batch as it was cannot approve what joined it.
            pending.id = id;
            pending.tickets.push(ticket);
            pending.texts.extend(text);
            return Decision::Ask(prompt(pending));
        }
        let deadline = now.checked_add(PROMPT_TIME).unwrap_or(now);
        let pending = Parked {
            id,
            key,
            deadline,
            tickets: vec![ticket],
            texts: text.into_iter().collect(),
        };
        let asked = prompt(&pending);
        self.pending.push(pending);
        Decision::Ask(asked)
    }

    /// Parks a workspace trust prompt of its own: `texts` is the canonical directory, then every
    /// `run` line, and `changed` says the directory was trusted with another layout. Allowing it
    /// once is the only allowing answer; it grants nothing lasting.
    pub fn ask_trust(
        &mut self,
        ticket: T,
        texts: Vec<String>,
        changed: bool,
        window_ready: bool,
    ) -> Decision<T> {
        let action = ApprovalAction::Trust;
        self.ask_alone(ticket, usize::from(changed), action, texts, window_ready)
    }

    /// Parks an owner prompt of its own for a caller outside every pane: `texts` is the method,
    /// then what it touches. Like trust, only allowing once answers it.
    pub fn ask_owner(&mut self, ticket: T, texts: Vec<String>, window_ready: bool) -> Decision<T> {
        self.ask_alone(ticket, 0, ApprovalAction::Owner, texts, window_ready)
    }

    fn ask_alone(
        &mut self,
        ticket: T,
        target: usize,
        action: ApprovalAction,
        texts: Vec<String>,
        window_ready: bool,
    ) -> Decision<T> {
        if let Some(refused) = self.full(window_ready) {
            return Decision::Refuse(ticket, refused);
        }
        let now = self.clock.now();
        let id = self.next_id();
        let pending = Parked {
            id,
            // Never matched by `decide`, so no other request joins this prompt.
            key: Key {
                caller: 0,
                target,
                action,
            },
            deadline: now.checked_add(PROMPT_TIME).unwrap_or(now),
            tickets: vec![ticket],
            texts,
        };
        let asked = prompt(&pending);
        self.pending.push(pending);
        Decision::Ask(asked)
    }

    fn full(&self, window_ready: bool) -> Option<&'static str> {
        if !window_ready {
            return Some("approval needs an open PinFold window");
        }
        let parked: usize = self
            .pending
            .iter()
            .map(|pending| pending.tickets.len())
            .sum();
        (parked >= MAX_PENDING).then_some("too many requests are waiting for approval")
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Releases the batch named by a window answer. Unknown and stale ids do nothing.
    pub fn answer(&mut self, id: u64, answer: ApprovalAnswer) -> Vec<Released<T>> {
        let Some(index) = self.pending.iter().position(|pending| pending.id == id) else {
            return Vec::new();
        };
        let pending = self.pending.swap_remove(index);
        let allowed = answer != ApprovalAnswer::Deny;
        let action = pending.key.action;
        if answer == ApprovalAnswer::AllowForThirtyMinutes && !action.is_once() {
            let now = self.clock.now();
            let expires = now.checked_add(GRANT_TIME).unwrap_or(now);
            self.grants.push(Grant {
                id: pending.id,
                key: pending.key,
                expires,
            });
        }
        let reason = match action {
            ApprovalAction::Trust => "the workspace was not trusted",
            ApprovalAction::Owner => "the owner action was denied in the window",
            ApprovalAction::Read | ApprovalAction::Send => "cross-pane action denied",
        };
        release(pending.tickets, allowed, reason)
    }

    /// Releases prompts whose deadline has passed.
    pub fn expire(&mut self) -> Vec<Released<T>> {
        let now = self.clock.now();
        self.grants.retain(|grant| grant.expires > now);
        self.drain_where(
            |pending| pending.deadline <= now,
            "approval timed out after 60 seconds",
        )
    }

    /// Releases every prompt, as when its only trusted window disappears.
    pub fn window_closed(&mut self) -> Vec<Released<T>> {
        self.drain_where(|_| true, "the approval window closed")
    }

    /// Drops parked tickets whose caller disconnected.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        for pending in &mut self.pending {
            let mut index = 0;
            while let Some(ticket) = pending.tickets.get(index) {
                if keep(ticket) {
                    index += 1;
                } else {
                    pending.tickets.remove(index);
                    if pending.key.action == ApprovalAction::Send && index < pending.texts.len() {
                        pending.texts.remove(index);
                    }
                }
            }
        }
        self.pending.retain(|pending| !pending.tickets.is_empty());
    }

    /// Nearest prompt deadline.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.iter().map(|pending| pending.deadline).min()
    }

    /// Lists unexpired lasting grants.
    pub fn list(&mut self) -> Vec<Listed> {
        let now = self.clock.now();
        self.grants.retain(|grant| grant.expires > now);
        self.grants
            .iter()
            .map(|grant| Listed {
                id: grant.id,
                caller: grant.key.caller,
                target: grant.key.target,
                action: grant.key.action,
                remaining: grant.expires.saturating_duration_since(now),
            })
            .collect()
    }

    /// Revokes one lasting grant. Pending prompts are not grants and are not named here.
    pub fn revoke(&mut self, id: u64) -> bool {
        let now = self.clock.now();
        self.grants.retain(|grant| grant.expires > now);
        let before = self.grants.len();
        self.grants.retain(|grant| grant.id != id);
        self.grants.len() != before
    }

    fn drain_where(
        &mut self,
        mut take: impl FnMut(&Parked<T>) -> bool,
        reason: &'static str,
    ) -> Vec<Released<T>> {
        let mut released = Vec::new();
        let mut index = 0;
        while let Some(pending) = self.pending.get(index) {
            if take(pending) {
                let pending = self.pending.swap_remove(index);
                released.extend(release(pending.tickets, false, reason));
            } else {
                index += 1;
            }
        }
        released
    }
}

fn prompt<T>(pending: &Parked<T>) -> Prompt {
    Prompt {
        id: pending.id,
        caller: pending.key.caller,
        target: pending.key.target,
        action: pending.key.action,
        texts: pending.texts.clone(),
    }
}

fn release<T>(tickets: Vec<T>, allowed: bool, reason: &'static str) -> Vec<Released<T>> {
    tickets
        .into_iter()
        .map(|ticket| Released {
            ticket,
            allowed,
            reason,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct TestClock(Cell<Instant>);

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.0.get()
        }
    }

    impl TestClock {
        fn advance(&self, duration: Duration) {
            if let Some(now) = self.0.get().checked_add(duration) {
                self.0.set(now);
            }
        }
    }

    #[test]
    fn a_grant_covers_one_triple_and_expires_after_thirty_minutes() {
        let clock = TestClock(Cell::new(Instant::now()));
        let mut grants = Grants::with_clock(clock);
        let asked = grants.decide(1, 2, 3, ApprovalAction::Read, None, true);
        let Decision::Ask(prompt) = asked else {
            panic!("not asked")
        };
        assert_eq!(
            grants
                .answer(prompt.id, ApprovalAnswer::AllowForThirtyMinutes)
                .len(),
            1
        );
        assert!(matches!(
            grants.decide(2, 2, 3, ApprovalAction::Read, None, true),
            Decision::Allow(2)
        ));
        assert!(matches!(
            grants.decide(3, 2, 4, ApprovalAction::Read, None, true),
            Decision::Ask(_)
        ));
        grants.clock.advance(GRANT_TIME);
        assert!(grants.list().is_empty());
        assert!(matches!(
            grants.decide(4, 2, 3, ApprovalAction::Read, None, true),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn related_requests_batch_and_revoke_removes_a_grant() {
        let mut grants = Grants::default();
        let first = grants.decide(1, 2, 3, ApprovalAction::Send, Some("one".into()), true);
        let Decision::Ask(first) = first else {
            panic!("not asked")
        };
        let second = grants.decide(2, 2, 3, ApprovalAction::Send, Some("two".into()), true);
        let Decision::Ask(second) = second else {
            panic!("not batched")
        };
        assert_ne!(second.id, first.id);
        assert_eq!(second.texts, ["one", "two"]);
        // A key pressed while only "one" was shown approves nothing.
        assert!(
            grants
                .answer(first.id, ApprovalAnswer::AllowOnce)
                .is_empty()
        );
        let released = grants.answer(second.id, ApprovalAnswer::AllowForThirtyMinutes);
        assert_eq!(released.len(), 2);
        assert_eq!(grants.list().len(), 1);
        assert!(grants.revoke(second.id));
        assert!(grants.list().is_empty());
    }

    #[test]
    fn no_window_refuses_instead_of_parking() {
        let mut grants = Grants::default();
        assert!(matches!(
            grants.decide(1, 2, 3, ApprovalAction::Read, None, false),
            Decision::Refuse(1, _)
        ));
        assert_eq!(grants.deadline(), None);
    }

    #[test]
    fn a_trust_prompt_is_its_own_and_grants_nothing_lasting() {
        let mut grants = Grants::default();
        let texts = vec!["/p".to_owned(), "make".to_owned()];
        let Decision::Ask(first) = grants.ask_trust(1, texts.clone(), true, true) else {
            panic!("not asked")
        };
        assert_eq!((first.target, first.action), (1, ApprovalAction::Trust));
        assert_eq!(first.texts, texts);
        let Decision::Ask(second) = grants.ask_trust(2, texts, false, true) else {
            panic!("not asked")
        };
        assert_ne!(first.id, second.id);
        let released = grants.answer(first.id, ApprovalAnswer::AllowForThirtyMinutes);
        assert!(released[0].allowed);
        assert!(grants.list().is_empty());
        let released = grants.answer(second.id, ApprovalAnswer::Deny);
        assert_eq!(released[0].reason, "the workspace was not trusted");
        assert!(matches!(
            grants.ask_trust(3, Vec::new(), false, false),
            Decision::Refuse(3, _)
        ));
    }

    #[test]
    fn an_owner_prompt_is_its_own_and_grants_nothing_lasting() {
        let mut grants = Grants::default();
        let texts = vec!["grants.revoke".to_owned()];
        let Decision::Ask(first) = grants.ask_owner(1, texts.clone(), true) else {
            panic!("not asked")
        };
        assert_eq!(first.action, ApprovalAction::Owner);
        let Decision::Ask(second) = grants.ask_owner(2, texts, true) else {
            panic!("not asked")
        };
        assert_ne!(first.id, second.id);
        let released = grants.answer(first.id, ApprovalAnswer::AllowForThirtyMinutes);
        assert!(released[0].allowed);
        assert!(grants.list().is_empty());
        let released = grants.answer(second.id, ApprovalAnswer::Deny);
        assert!(!released[0].allowed);
        assert!(released[0].reason.contains("denied in the window"));
        assert!(matches!(
            grants.ask_owner(3, Vec::new(), false),
            Decision::Refuse(3, _)
        ));
    }

    #[test]
    fn an_unanswered_prompt_expires_after_sixty_seconds() {
        let clock = TestClock(Cell::new(Instant::now()));
        let mut grants = Grants::with_clock(clock);
        assert!(matches!(
            grants.decide(1, 2, 3, ApprovalAction::Read, None, true),
            Decision::Ask(_)
        ));
        grants.clock.advance(PROMPT_TIME);
        let expired = grants.expire();
        assert_eq!(expired.len(), 1);
        assert!(!expired[0].allowed);
        assert!(expired[0].reason.contains("timed out"));
    }
}
