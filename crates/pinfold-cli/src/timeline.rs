//! `run.timeline` (ADR-0052 section 7): an agent run's record, read from its ring file when
//! asked and dropped once answered, so the core keeps no entry. Numbers and fixed words only,
//! never pane text (threat model T2). The reply is written as JSON text an entry at a time, one
//! allocation rather than a tree of values a thousand entries long.

use std::fmt::Write;

use pinfold_api::control::{Method, Request};
use pinfold_core::family::Family;
use pinfold_core::io::Reactor;
use pinfold_core::rewind::Rewind;
use pinfold_core::timeline::{Answer, Ended, Entry, METHODS, Outcome, REASONS, What};
use pinfold_proto::link::ApprovalAction;
use serde_json::{Value, json};

/// Answers `run.timeline` for the run in `pane`: the caller's own, a child's it reaches, or any
/// from a terminal outside every pane, which is not asked, as `audit.read` is not: it returns
/// metadata and no way to run anything (ADR-0042). Another pane's is refused, never asked.
///
/// ```
/// # fn answer<T>(log: &mut pinfold_api::Log, reactor: &pinfold_core::io::Reactor,
/// #     family: &mut pinfold_core::family::Family,
/// #     rewind: Option<&pinfold_core::rewind::Rewind<T>>,
/// #     request: pinfold_api::control::Request) {
/// let read = pinfold_cli::timeline::read(reactor, family, rewind, &request);
/// pinfold_cli::audit::reply_json(log, reactor, request, read);
/// # }
/// ```
///
/// # Errors
///
/// A short reason for a caller with no terminal, another pane's run, a pane with none,
/// `[timeline]` off, a file that cannot be read, or a request that is not `run.timeline`.
pub fn read<T>(
    reactor: &Reactor,
    family: &mut Family,
    rewind: Option<&Rewind<T>>,
    request: &Request,
) -> Result<String, String> {
    let &Method::RunTimeline {
        pane,
        count,
        before,
        kinds,
    } = request.method()
    else {
        return Err("not run.timeline".to_owned());
    };
    if request.caller().is_none() {
        return Err("run.timeline needs a caller in a terminal".to_owned());
    }
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    if let Some(own) = own
        && family.reach(reactor, Some(own), pane).is_err()
    {
        return Err("run.timeline reads the caller's own run and its children's only".to_owned());
    }
    let timelines = (reactor.timelines()).ok_or("timelines are off: [timeline] enabled = false")?;
    let run = timelines.newest(pane).ok_or("no agent run in that pane")?;
    let page = timelines.page(pane, before, count, |entry| kinds & 1 << kind(entry) != 0);
    let page = page.map_err(|_| "the run's timeline cannot be read".to_owned())?;
    let summary = run.summary();
    let started = u64::from(summary.started) * 1000;
    let head = json!({
        "run": rewind.and_then(|rewind| Some(rewind.run(rewind.of_pane(pane)?)?.id())),
        "pane": pane,
        "started_ms": started,
        "ended_ms": (!run.is_live()).then(|| started + u64::from(summary.ended)),
        "summary": {
            "commands": summary.commands, "failed": summary.failed,
            "approvals": summary.approvals, "denied": summary.denied, "calls": summary.calls,
            "changed": summary.changed, "errors": summary.errors, "needs_you": summary.needs_you,
        },
        "dropped": run.dropped(),
    })
    .to_string();
    // About 130 bytes an entry; the head's `}` gives way to the entries.
    let mut json = String::with_capacity(head.len() + 16 + page.len() * 160);
    json.push_str(&head[..head.len() - 1]);
    json.push_str(r#","entries":["#);
    for (at, (seq, entry)) in page.into_iter().enumerate() {
        let comma = if at == 0 { "" } else { "," };
        let _ = write!(json, "{comma}{}", fields(reactor, pane, seq, entry));
    }
    json.push_str("]}");
    Ok(json)
}

/// The entry's place in `pinfold_api::control::TIMELINE_KINDS`.
fn kind(entry: &Entry) -> u8 {
    match entry.what {
        What::Command { .. } => 0,
        What::Api { .. } => 1,
        What::Approval { .. } => 2,
        What::State { .. } => 3,
        What::Checkpoint { .. } => 4,
        What::Exit(_) => 5,
    }
}

/// One entry as numbers and fixed words.
fn fields(reactor: &Reactor, pane: usize, seq: u64, entry: Entry) -> Value {
    let pane_number = |pane: Option<u16>| pane.map(u64::from);
    let mut fields = match entry.what {
        What::Command { row, ms, exit } => {
            let row = (row != u32::MAX).then_some(row);
            let held = row.is_some_and(|row| {
                (reactor.engine(pane)).is_some_and(|engine| engine.holds_row(row as usize))
            });
            json!({"row": row, "ms": ms, "exit": exit, "source": "marks", "held": held})
        }
        What::Api {
            seq,
            method,
            outcome,
            target,
        } => json!({
            "audit_seq": seq,
            "method": METHODS.get(usize::from(method)).map(|(name, _)| *name),
            "outcome": match outcome {
                Outcome::Ok => "ok",
                Outcome::Refused => "refused",
                Outcome::Error => "error",
            },
            "target": pane_number(target),
        }),
        What::Approval {
            action,
            answer,
            other,
            grant,
        } => json!({
            "action": match action {
                ApprovalAction::Read => "read",
                ApprovalAction::Send => "send",
                ApprovalAction::Trust => "trust",
                ApprovalAction::Owner => "owner",
                ApprovalAction::Rewind => "rewind",
            },
            "answer": match answer {
                Answer::Once => "once",
                Answer::ThirtyMinutes => "thirty_minutes",
                Answer::Denied => "denied",
                Answer::Expired => "expired",
            },
            "other": pane_number(other),
            "grant": grant,
        }),
        What::State { status, reported } => {
            json!({"status": status.name(), "reported": reported})
        }
        What::Checkpoint {
            n,
            reason,
            complete,
            changed,
        } => json!({
            "n": n,
            "reason": REASONS.get(usize::from(reason)),
            "complete": complete,
            "changed": changed,
        }),
        What::Exit(ended) => match ended {
            Ended::Code(code) => json!({"ended": "code", "exit": code}),
            Ended::Signal(signal) => json!({"ended": "signal", "exit": signal}),
            Ended::Closed => json!({"ended": "closed", "exit": null}),
        },
    };
    fields["seq"] = json!(seq);
    fields["at_ms"] = json!(entry.at);
    fields["kind"] = json!(pinfold_api::control::TIMELINE_KINDS[usize::from(kind(&entry))]);
    fields
}
