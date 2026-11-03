//! What the core records of each request it answers (threat model, Audit log): who called, the
//! method, the target pane and the outcome. Never pane contents or sent text: a send is recorded
//! by its size alone.

use std::error::Error;
use std::fs::DirBuilder;
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::time::{SystemTime, UNIX_EPOCH};

use pinfold_api::control::{Method, Request};
use pinfold_api::{Log, Outcome, Record};
use pinfold_core::io::Reactor;
use serde_json::{Value, json};

/// Opens the audit log: in memory only, unless `api.audit_log` is set, which also writes it to
/// `audit.log` in [`pinfold_config::state_dir`]. A file that will not open is told on stderr,
/// the session's log, and leaves the log in memory.
///
/// ```
/// let log = pinfold_cli::audit::open(&pinfold_config::Api::default());
/// assert_eq!(log.iter().count(), 0);
/// ```
#[must_use]
pub fn open(api: &pinfold_config::Api) -> Log {
    if !api.audit_log {
        return Log::new();
    }
    let opened = || -> Result<Log, Box<dyn Error>> {
        let dir = pinfold_config::state_dir().ok_or("HOME is not set")?;
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        Ok(Log::with_file(dir.join("audit.log"))?)
    };
    opened().unwrap_or_else(|error| {
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: audit log kept in memory: {error}"
        );
        Log::new()
    })
}

/// Records `request` with `result` in `log`, then answers it with `result`.
///
/// ```
/// # fn answer(log: &mut pinfold_api::Log, reactor: &pinfold_core::io::Reactor,
/// #     request: pinfold_api::control::Request) {
/// pinfold_cli::audit::reply(log, reactor, request, Ok(serde_json::json!({})));
/// # }
/// ```
pub fn reply(log: &mut Log, reactor: &Reactor, request: Request, result: Result<Value, String>) {
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let record = record(request.method(), request.caller().is_some(), own, &result);
    if let Err(error) = log.append(record) {
        let _ = writeln!(io::stderr(), "pinfold-core: audit log: {error}");
    }
    request.answer(result);
}

/// Answers `audit.read`: the newest entries, oldest first, and whether the chain in memory
/// verifies. Only a caller in a terminal outside any pane may read it, so an agent in a pane
/// cannot read what other agents did.
///
/// ```
/// # fn answer(log: &mut pinfold_api::Log, reactor: &pinfold_core::io::Reactor,
/// #     request: pinfold_api::control::Request) {
/// let read = pinfold_cli::audit::read(log, reactor, &request);
/// pinfold_cli::audit::reply(log, reactor, request, read);
/// # }
/// ```
///
/// # Errors
///
/// A short reason for a caller in a pane or with no terminal, or a request that is not
/// `audit.read`.
pub fn read(log: &Log, reactor: &Reactor, request: &Request) -> Result<Value, String> {
    // shortcut: a terminal outside every pane is a guardrail, not proof of a person: an agent can
    // open a new terminal of its own. Ask in the window once approval prompts land (#100).
    let outside = request
        .caller()
        .is_some_and(|caller| reactor.pane_of(caller).is_none());
    let (true, Method::AuditRead { count, pane }) = (outside, request.method()) else {
        return Err("audit.read needs a caller in a terminal outside any pane".to_owned());
    };
    let chosen: Vec<_> = match pane {
        Some(pane) => log.for_pane(*pane as u64).collect(),
        None => log.iter().collect(),
    };
    let newest = &chosen[chosen.len().saturating_sub(*count)..];
    let broken = log.verify().err().map(|broken| broken.seq);
    let entries: Vec<Value> = newest.iter().map(|entry| entry.to_json()).collect();
    Ok(json!({"entries": entries, "verified": broken.is_none(), "broken_at": broken}))
}

/// The record of one answered request. `own` is the caller's pane, if it is in one.
fn record(
    method: &Method,
    terminal: bool,
    own: Option<usize>,
    result: &Result<Value, String>,
) -> Record {
    let scope = |pane: usize| if own == Some(pane) { "self" } else { "other" };
    let (name, target, scope) = match method {
        Method::Hello => ("hello", None, "any"),
        Method::WindowOpen => ("window.open", None, "any"),
        Method::PaneList => ("pane.list", None, "terminal"),
        Method::EventsSubscribe(_) => ("events.subscribe", None, "terminal"),
        Method::PaneRead { pane, .. } => ("pane.read", Some(*pane), scope(*pane)),
        Method::PaneLastOutput { pane } => ("pane.last_output", Some(*pane), scope(*pane)),
        Method::PaneSend { pane, .. } => ("pane.send", Some(*pane), scope(*pane)),
        Method::PaneWait(wait) => {
            let pane = wait.pane.or(own);
            ("pane.wait", pane, pane.map_or("self", scope))
        }
        Method::PaneSpawn(_) => {
            // The new pane, which the caller spawned.
            let spawned = result
                .as_ref()
                .ok()
                .and_then(|value| value["pane"].as_u64());
            let pane = spawned.and_then(|pane| usize::try_from(pane).ok());
            ("pane.spawn", pane, "children")
        }
        Method::PaneClose { pane } => ("pane.close", Some(*pane), scope(*pane)),
        Method::AuditRead { pane, .. } => ("audit.read", *pane, "outside"),
        // A refused call from inside a pane is the caller's own, not one from outside.
        Method::WorkspaceOpen(_) => ("workspace.open", None, own.map_or("outside", |_| "self")),
    };
    let mut reason = match result {
        Ok(_) => "allowed".to_owned(),
        Err(reason) => reason.clone(),
    };
    // A send's text is never recorded, only its size.
    if let Method::PaneSend { text, .. } = method {
        reason += &format!(", {} bytes", text.len());
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Record {
        seq: 0,
        at_ms: u64::try_from(at.as_millis()).unwrap_or(u64::MAX),
        caller: own.map(|pane| pane as u64),
        terminal,
        method: name.to_owned(),
        target: target.map(|pane| pane as u64),
        scope: scope.to_owned(),
        outcome: match result {
            Ok(_) => Outcome::Ok(reason),
            Err(_) => Outcome::Refused(reason),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_is_recorded_by_its_size_and_never_its_text() {
        let send = Method::PaneSend {
            pane: 2,
            text: "secret token\n".to_owned(),
        };
        let record = record(&send, true, Some(2), &Ok(json!({})));
        assert_eq!(record.outcome, Outcome::Ok("allowed, 13 bytes".to_owned()));
        assert_eq!((record.caller, record.target), (Some(2), Some(2)));
        let names = (record.method.as_str(), record.scope.as_str());
        assert_eq!(names, ("pane.send", "self"));
        assert!(!format!("{record:?}").contains("secret"));
    }

    #[test]
    fn a_refusal_is_recorded_with_its_reason() {
        let read = Method::PaneRead {
            pane: 0,
            lines: None,
        };
        let refused = Err("only the caller's own pane".to_owned());
        let record = record(&read, true, Some(1), &refused);
        let expected = Outcome::Refused("only the caller's own pane".to_owned());
        assert_eq!((record.outcome, record.scope.as_str()), (expected, "other"));
        let unidentified = super::record(&Method::PaneList, false, None, &Ok(json!({})));
        assert_eq!((unidentified.caller, unidentified.terminal), (None, false));
    }

    #[test]
    fn a_workspace_open_is_outside_only_from_no_pane() {
        let open = Method::WorkspaceOpen(pinfold_api::workspace::Open {
            root: "/p".to_owned(),
            tabs: Vec::new(),
        });
        let refused = Err("workspace.open needs a caller outside any pane".to_owned());
        let scope = |own| record(&open, true, own, &refused).scope;
        assert_eq!(
            (scope(None).as_str(), scope(Some(1)).as_str()),
            ("outside", "self")
        );
    }
}
