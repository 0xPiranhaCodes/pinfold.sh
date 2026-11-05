//! The `pinfold-core` binary.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use std::{env, thread};

use pinfold_api::Log;
use pinfold_api::control::{
    self, Control, Method, Request, Subscriber, Topics, Until, Wait, Workspace,
};
use pinfold_api::redact;
use pinfold_api::spawn::Place;
use pinfold_api::trust::{self, State};
use pinfold_cli::audit;
use pinfold_cli::output;
use pinfold_config::Agent;
use pinfold_config::workspace as layout;
use pinfold_core::family::{self, Family, SplitWay};
use pinfold_core::grants::{Decision, Grants, Released};
use pinfold_core::io::{Reactor, Waker};
use pinfold_core::snapshot::{self, Saver, Snapshot};
use pinfold_core::status::Status;
use pinfold_core::wait::{Outcome, Waits};
use pinfold_core::workspace::Workspaces;
use pinfold_core::{sandbox, shell};
use pinfold_proto::link::{self, ApprovalAction, ApprovalAnswer};
use serde_json::{Value, json};

const ROWS: u16 = 24;
const COLUMNS: u16 = 80;
// A read with every byte escaped by JSON still fits in a peer's output queue.
const _: () = assert!(2 * pinfold_core::io::MAX_READ + 4096 < control::MAX_OUTPUT);
const IDLE: Duration = Duration::from_secs(3600);
/// How long a core that starts with no pane waits for its first pane or window. `pinfold up`
/// asks for the window within its 5 s start wait, so past this one is never coming.
const EMPTY_WAIT: Duration = Duration::from_secs(30);

/// [`EMPTY_WAIT`], or in a debug build `PINFOLD_TEST_EMPTY_WAIT_MS`, so its test takes seconds.
fn empty_wait() -> Duration {
    let shorter = env::var("PINFOLD_TEST_EMPTY_WAIT_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions));
    shorter
        .and_then(|ms| ms.parse().ok())
        .map_or(EMPTY_WAIT, Duration::from_millis)
}

/// Returns the window binary beside this one. It is resolved once, so nothing later can name
/// another program.
fn window_binary() -> io::Result<PathBuf> {
    Ok(env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-window"))
}

/// Binds the control socket and answers it on the API thread (ADR-0007, ADR-0032). The
/// reactor's thread makes every spawn and holds the panes, so every request crosses to it. The channel holds one request per peer the API thread serves.
fn listen(waker: Arc<Waker>) -> io::Result<Receiver<Request>> {
    let control = Control::bind(&control::runtime_dir()).map_err(io::Error::other)?;
    let (ask, asked) = mpsc::sync_channel(control::MAX_PEERS);
    let open = move |request: Request| match ask.try_send(request) {
        Ok(()) => {
            // A lost wake leaves the caller to hear that it waited too long.
            let _ = waker.wake();
        }
        Err(TrySendError::Full(request) | TrySendError::Disconnected(request)) => {
            request.answer(Err("the core is busy".to_owned()));
        }
    };
    let serve = move || control.serve(open);
    thread::Builder::new()
        .name("api".to_owned())
        .stack_size(control::STACK)
        .spawn(serve)?;
    Ok(asked)
}

/// Carries out a pane operation whose scope was already decided. Cross-pane reads are redacted
/// at this last boundary, after the whole result has been collected.
fn pane_action(reactor: &mut Reactor, method: &Method, cross_pane: bool) -> Result<Value, String> {
    let clean = |text: String| {
        if cross_pane {
            redact(&text).into_owned()
        } else {
            text
        }
    };
    match method {
        Method::PaneRead { pane, lines } => {
            let text = reactor.read(*pane, *lines).ok_or("no such pane")?;
            Ok(json!({"text": clean(text)}))
        }
        Method::PaneLastOutput { pane } => match reactor.last_output(*pane) {
            Some(text) => Ok(json!({"text": clean(text)})),
            None if reactor.engine(*pane).is_none() => Err("no such pane".to_owned()),
            None => Err("the pane has no command output marked with OSC 133".to_owned()),
        },
        // Another pane takes a line at a time, each checked for protection before it goes.
        Method::PaneSend { pane, text } if cross_pane => reactor
            .send_lines(*pane, text, Instant::now())
            .map(|_| json!({}))
            .map_err(str::to_owned),
        Method::PaneSend { pane, text } => reactor
            .send(*pane, text, Instant::now())
            .map(|()| json!({}))
            .map_err(str::to_owned),
        _ => Err("not a pane action".to_owned()),
    }
}

fn cross_pane(method: &Method) -> Option<(usize, ApprovalAction, Option<String>)> {
    match method {
        Method::PaneRead { pane, .. } | Method::PaneLastOutput { pane } => {
            Some((*pane, ApprovalAction::Read, None))
        }
        Method::PaneSend { pane, text } => Some((*pane, ApprovalAction::Send, Some(text.clone()))),
        _ => None,
    }
}

fn grants_list<T, C: pinfold_core::grants::Clock>(grants: &mut Grants<T, C>) -> Value {
    let listed = grants.list().into_iter().map(|grant| {
        json!({"id": grant.id, "caller": grant.caller, "target": grant.target,
               "action": grant.action.as_str(), "expires_ms": grant.remaining.as_millis()})
    });
    json!({"grants": listed.collect::<Vec<_>>()})
}

/// Answers a request that opens no window and needs no approval. A caller with no terminal is
/// unidentified and may not list panes (threat model, caller identity point 3). Any caller with
/// a terminal may, in a pane or not, as a list shows no screen and little a same-user `ps` does
/// not. Spawns and closes reach only the caller's own pane and the panes it spawned, the `self`
/// and `children` scopes. Pane contents go to the caller alone, never to a log. Owner methods
/// that change something are asked in the window first, by [`decide`], and refused here. An agent
/// launches only by its profile's name in `agents`, the user's config, and a sandboxed agent and
/// the panes under it start only sandboxed agents in its working directory.
fn immediate<T, C: pinfold_core::grants::Clock>(
    reactor: &mut Reactor,
    family: &mut Family,
    agents: &BTreeMap<String, Agent>,
    grants: &mut Grants<T, C>,
    request: &Request,
) -> Result<Value, String> {
    let identified = request.caller().is_some();
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    match request.method() {
        Method::Hello => {
            let methods: &[&str] = match (identified, own) {
                (true, Some(_)) => &[
                    "window.open",
                    "pane.list",
                    "pane.read",
                    "pane.last_output",
                    "pane.send",
                    "pane.wait",
                    "pane.spawn",
                    "agent.launch",
                    "pane.close",
                    "events.subscribe",
                    "pane.status",
                ],
                (true, None) => &[
                    "window.open",
                    "pane.list",
                    "events.subscribe",
                    "audit.read",
                    "session.save",
                    "grants.list",
                    "grants.revoke",
                    "workspace.open",
                    "workspace.trust_state",
                    "workspace.ask_trust",
                ],
                (false, _) => &["window.open"],
            };
            Ok(
                json!({"version": control::VERSION, "methods": methods, "pane": own,
                      "link": link::VERSION}),
            )
        }
        Method::PaneStatus {
            pane,
            state,
            message,
        } => {
            let status = Status::from_name(state).ok_or("unknown state")?;
            let set = reactor.report_status(
                own.filter(|own| own == pane)
                    .ok_or("only the caller's own pane can report its status")?,
                status,
                message,
                Instant::now(),
            );
            set.map(|()| json!({})).map_err(str::to_owned)
        }
        Method::PaneSpawn(spawn) => {
            let own = own.ok_or("pane.spawn needs a caller in a pane")?;
            let of = spawn.of.unwrap_or(own);
            let pane = family.spawn(reactor, own, of, way(spawn.place), spawn.cwd.as_deref())?;
            Ok(json!({"pane": pane}))
        }
        Method::AgentLaunch(launch) => {
            let own = own.ok_or("agent.launch needs a caller in a pane")?;
            let of = family.reach(reactor, Some(own), launch.of.unwrap_or(own))?;
            // The caller's name is not echoed: the reason goes to the audit log.
            let agent = agents
                .get(&launch.profile)
                .ok_or("no agent profile by that name in the config")?;
            // Where `of` is: its last OSC 7, which any program there can print, or its shell's
            // own directory. Never the core's, which a pane with neither would get.
            let cwd = reactor.cwd(of).filter(|cwd| Path::new(cwd).is_dir());
            let cwd = cwd.ok_or("the pane's directory is not known: its shell has none to read")?;
            let (command, work) = if agent.sandbox {
                // The sandbox needs a directory of the pane's, never one the core chose.
                let refused = |error: io::Error| format!("the sandbox will not start: {error}");
                let hidden = pinfold_config::private_dirs();
                // Never waits: a shell still reading refuses this launch.
                let state =
                    pinfold_config::state_dir().ok_or("no state directory, as HOME is not set")?;
                let path = sandbox::login_path(&state).map_err(refused)?;
                let work = sandbox::working_directory(Path::new(&cwd), &hidden, &path);
                let work = work.map_err(refused)?;
                // Before the shadow is made, so a refused launch writes nothing.
                family.contain(reactor, own, Some(work.path()))?;
                let api = control::socket_path(&control::runtime_dir());
                // A missing sandbox tool is an error, so the agent never runs unsandboxed.
                // The agent's own git directory, one per profile and working directory (ADR-0039).
                let name = Path::new(&launch.profile);
                let named = matches!(
                    name.components().collect::<Vec<_>>()[..],
                    [Component::Normal(_)]
                );
                let shadow = pinfold_config::data_dir().filter(|_| named).map(|data| {
                    let key = trust::sha256(work.path().as_os_str().as_encoded_bytes());
                    data.join("agents").join(name).join(&key[..16])
                });
                let shadow = shadow.as_deref();
                let command = sandbox::command(&agent.command, &hidden, &work, &api, shadow);
                if cfg!(target_os = "linux") {
                    log_abstract_socket_gap();
                }
                (command.map_err(refused)?, Some(work))
            } else {
                let (program, args) = agent.command.split_first().ok_or("empty command")?;
                let mut command = Command::new(program);
                command.args(args);
                (command, None)
            };
            let way = way(launch.place);
            let pane = match work {
                Some(work) => {
                    family.spawn_sandboxed(reactor, own, of, way, work.path(), command)?
                }
                None => family.spawn_command(reactor, own, of, way, Some(&cwd), command)?,
            };
            Ok(json!({"pane": pane}))
        }
        Method::PaneClose { pane } => {
            let closed = family.close(reactor, own, *pane);
            closed.map(|()| json!({})).map_err(str::to_owned)
        }
        Method::PaneList if identified => {
            let panes = reactor.list().map(|pane| {
                // A message is the caller's own business: other panes' stay in the window.
                let message = (pane.reported && own == Some(pane.id)).then_some(pane.message);
                json!({"id": pane.id, "title": pane.title, "cwd": pane.cwd,
                       "command": pane.command, "status": pane.status.name(),
                       "source": source(pane.reported), "message": message})
            });
            Ok(json!({"panes": panes.collect::<Vec<_>>()}))
        }
        Method::PaneList => Err("pane.list needs a caller in a terminal".to_owned()),
        // Asked in the window from a terminal outside every pane, and refused here from anywhere else.
        Method::SessionSave => {
            Err("session.save needs a caller in a terminal outside any pane".to_owned())
        }
        Method::GrantsList if identified && own.is_none() => Ok(grants_list(grants)),
        Method::GrantsList | Method::GrantsRevoke { .. } => {
            Err("grant administration needs a terminal outside every pane".to_owned())
        }
        Method::WorkspaceTrustState(workspace) if identified && own.is_none() => {
            let state = trust::state(&trust_path()?, &canonical(workspace)?, &workspace.sha256);
            Ok(json!({"trusted": state == State::Trusted, "changed": state == State::Changed}))
        }
        Method::WorkspaceTrustState(_) | Method::WorkspaceAskTrust(..) => {
            Err("workspace trust needs a terminal outside every pane".to_owned())
        }
        Method::WorkspaceOpen(_) => {
            Err("workspace.open needs a caller in a terminal outside any pane".to_owned())
        }
        // Never sent here: the loop opens windows, parks waits and keeps subscribers.
        Method::WindowOpen
        | Method::PaneWait(_)
        | Method::EventsSubscribe(_)
        | Method::PaneRead { .. }
        | Method::PaneLastOutput { .. }
        | Method::PaneSend { .. }
        | Method::AuditRead { .. } => Err("not here".to_owned()),
    }
}

/// What the window shows for a method that changes the session or its grants, which a terminal
/// outside every pane runs only once the window allows it: a pane's program can start such a
/// terminal with a new PTY. Reads of grants, audit and trust state are not asked.
fn owner_prompt(method: &Method) -> Option<Vec<String>> {
    let texts = match method {
        Method::SessionSave => vec![
            "session.save".to_owned(),
            "Writes this session's layout over the saved one".to_owned(),
        ],
        Method::GrantsRevoke { id } => vec!["grants.revoke".to_owned(), format!("Grant {id}")],
        Method::WorkspaceOpen(open) => {
            let panes: usize = open.tabs.iter().map(Vec::len).sum();
            let mut texts = vec![
                "workspace.open".to_owned(),
                open.root.clone(),
                format!("{panes} pane(s) in {} tab(s)", open.tabs.len()),
            ];
            if open.sha256.is_some() {
                texts.push("and its run lines, if the layout is trusted".to_owned());
            }
            texts
        }
        _ => return None,
    };
    Some(texts)
}

/// Runs a method of [`owner_prompt`]'s once the window allowed it.
fn owner_action(
    reactor: &mut Reactor,
    grants: &mut Grants<Request>,
    workspaces: &mut Workspaces,
    method: &Method,
) -> Result<Value, String> {
    match method {
        Method::SessionSave => {
            let state = pinfold_config::state_dir().ok_or("HOME is not set")?;
            let path = saved(reactor, workspaces).write(&state, control::SESSION);
            path.map(|path| json!({"path": path.to_string_lossy()}))
                .map_err(|error| format!("cannot save the session: {error}"))
        }
        Method::GrantsRevoke { id } => Ok(json!({"revoked": grants.revoke(*id)})),
        Method::WorkspaceOpen(open) => {
            let sha256 = open.sha256.as_deref();
            let root = Path::new(&open.root);
            let count = open.tabs.iter().map(Vec::len).sum();
            let runs = sha256.and_then(|sha256| trusted_runs(root, sha256, count));
            // A trusted line runs where its file says, never where the caller said.
            let mut cwds = runs.iter().flatten().map(|(cwd, _)| cwd.to_string_lossy());
            let mut pane = |(place, cwd): &(Place, String)| {
                let cwd = cwds
                    .next()
                    .map_or_else(|| cwd.clone(), |cwd| cwd.into_owned());
                (way(*place), cwd)
            };
            let tabs = open.tabs.iter();
            let tabs: Vec<Vec<_>> = tabs
                .map(|tab| tab.iter().map(&mut pane).collect())
                .collect();
            let opened = workspaces.open(reactor, &open.root, sha256, &tabs)?;
            if let (Some(runs), false) = (runs, opened.is_empty()) {
                let panes: Vec<_> = opened.iter().copied().map(Some).collect();
                type_runs(reactor, &panes, &runs);
            }
            Ok(json!({"opened": opened.len()}))
        }
        _ => Err("not an owner action".to_owned()),
    }
}

/// The trust store, `trust.json` in the state directory, once that directory is made and passes
/// [`control::checked`]. A refused directory is logged and trusts nothing, so every layout
/// prompts.
fn trust_path() -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = pinfold_config::state_dir().ok_or("no state directory: HOME is not set")?;
    let made = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir);
    if let Err(error) = made
        .map_err(control::Error::from)
        .and_then(|()| control::checked(&dir))
    {
        let reason = format!("the trust store is refused: {error}");
        let _ = writeln!(io::stderr(), "pinfold-core: {reason}");
        return Err(reason);
    }
    Ok(dir.join("trust.json"))
}

/// `root` itself, once checked to be a canonical directory, so what the prompt shows is what
/// the store records.
fn canonical(workspace: &Workspace) -> Result<String, String> {
    match std::fs::canonicalize(&workspace.root) {
        Ok(path) if path.is_dir() && path.as_os_str() == workspace.root.as_str() => {
            Ok(workspace.root.clone())
        }
        _ => Err("params.root must be a canonical directory".to_owned()),
    }
}

/// Parks a trust prompt for a caller in a terminal outside every pane. It runs nothing.
fn ask_trust(
    reactor: &mut Reactor,
    grants: &mut Grants<Request>,
    workspaces: &mut Workspaces,
    log: &mut Log,
    request: Request,
) {
    let Method::WorkspaceAskTrust(workspace, runs) = request.method() else {
        return;
    };
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    if request.caller().is_none() || own.is_some() {
        let reason = "workspace trust needs a terminal outside every pane";
        return audit::reply(log, reactor, request, Err(reason.to_owned()));
    }
    let checked = trust_path().and_then(|path| Ok((path, canonical(workspace)?)));
    let (path, root) = match checked {
        Ok(checked) => checked,
        Err(reason) => return audit::reply(log, reactor, request, Err(reason)),
    };
    let changed = trust::state(&path, &root, &workspace.sha256) == State::Changed;
    let texts = std::iter::once(root).chain(runs.iter().cloned()).collect();
    let decision = grants.ask_trust(request, texts, changed, reactor.is_window_ready());
    ask_alone(reactor, grants, workspaces, log, decision);
}

/// Draws a trust or owner prompt, which is never allowed before it is asked.
fn ask_alone(
    reactor: &mut Reactor,
    grants: &mut Grants<Request>,
    workspaces: &mut Workspaces,
    log: &mut Log,
    decision: Decision<Request>,
) {
    match decision {
        Decision::Ask(prompt) if reactor.ask_approval(&prompt) => {}
        Decision::Ask(prompt) => {
            let released = grants.answer(prompt.id, ApprovalAnswer::Deny);
            answer_released(reactor, grants, workspaces, log, released);
        }
        Decision::Refuse(request, reason) => {
            audit::reply(log, reactor, request, Err(reason.to_owned()));
        }
        Decision::Allow(request) => {
            audit::reply(log, reactor, request, Err("not asked".to_owned()));
        }
    }
}

/// The way a pane splits, or none for a new tab.
fn way(place: Place) -> Option<SplitWay> {
    match place {
        Place::SplitRight => Some(SplitWay::Right),
        Place::SplitDown => Some(SplitWay::Down),
        Place::Tab => None,
    }
}

fn answer_released(
    reactor: &mut Reactor,
    grants: &mut Grants<Request>,
    workspaces: &mut Workspaces,
    log: &mut Log,
    released: Vec<Released<Request>>,
) {
    for released in released {
        let answer = match released.ticket.method() {
            Method::WorkspaceAskTrust(workspace, _) if released.allowed => {
                trust_path().and_then(|path| {
                    trust::record(&path, &workspace.root, &workspace.sha256)
                        .map(|()| json!({"trusted": true}))
                        .map_err(|error| format!("cannot write the trust store: {error}"))
                })
            }
            method if released.allowed && owner_prompt(method).is_some() => {
                owner_action(reactor, grants, workspaces, method)
            }
            method if released.allowed => pane_action(reactor, method, true),
            _ => Err(released.reason.to_owned()),
        };
        audit::reply(log, reactor, released.ticket, answer);
    }
}

/// Answers a request at once, or parks on a window approval a read or send outside the caller's
/// `self` and `children` scopes, or an owner method from a terminal outside every pane.
fn decide(
    reactor: &mut Reactor,
    family: &mut Family,
    agents: &BTreeMap<String, Agent>,
    grants: &mut Grants<Request>,
    workspaces: &mut Workspaces,
    log: &mut Log,
    request: Request,
) {
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    if let Some(texts) = owner_prompt(request.method())
        && request.caller().is_some()
        && own.is_none()
    {
        let decision = grants.ask_owner(request, texts, reactor.is_window_ready());
        return ask_alone(reactor, grants, workspaces, log, decision);
    }
    let Some((target, action, text)) = cross_pane(request.method()) else {
        let answer = immediate(reactor, family, agents, grants, &request);
        return audit::reply(log, reactor, request, answer);
    };
    let Some(caller) = own else {
        return audit::reply(
            log,
            reactor,
            request,
            Err("cross-pane actions need a caller pane".to_owned()),
        );
    };
    if reactor.engine(target).is_none() {
        return audit::reply(log, reactor, request, Err("no such pane".to_owned()));
    }
    if family.reach(reactor, own, target).is_ok() {
        // Text from any pane but the caller's own is redacted (threat model, Secret redaction).
        let answer = pane_action(reactor, request.method(), caller != target);
        return audit::reply(log, reactor, request, answer);
    }
    match grants.decide(
        request,
        caller,
        target,
        action,
        text,
        reactor.is_window_ready(),
    ) {
        Decision::Allow(request) => {
            let answer = pane_action(reactor, request.method(), true);
            audit::reply(log, reactor, request, answer);
        }
        Decision::Refuse(request, reason) => {
            audit::reply(log, reactor, request, Err(reason.to_owned()));
        }
        Decision::Ask(prompt) if reactor.ask_approval(&prompt) => {}
        // Fail closed for this prompt alone, so one oversized batch cannot drop the others.
        Decision::Ask(prompt) => {
            let released = grants.answer(prompt.id, ApprovalAnswer::Deny);
            answer_released(reactor, grants, workspaces, log, released);
        }
    }
}

/// Parks a wait on the caller's own pane or one it spawned, or refuses it. Match and idle waits
/// take only the caller's own pane.
fn park(
    reactor: &Reactor,
    family: &mut Family,
    waits: &mut Waits<Request>,
    log: &mut Log,
    request: Request,
    wait: Wait,
) {
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let target = wait.pane.or(own).ok_or(family::NOT_YOURS);
    let pane = match target.and_then(|pane| family.reach(reactor, own, pane)) {
        Ok(pane) => pane,
        Err(reason) => return audit::reply(log, reactor, request, Err(reason.to_owned())),
    };
    // shortcut: streamed output and match waits are self-only because redacting a stream needs whole-block state; extend to children with an ADR on streamed redaction.
    let refusal = match wait.until {
        Until::Match(_) => "a match wait needs the caller's own pane",
        Until::Idle(_) => "an idle wait needs the caller's own pane",
        _ => "",
    };
    if !refusal.is_empty() && own != Some(pane) {
        return audit::reply(log, reactor, request, Err(refusal.to_owned()));
    }
    let until = match wait.until {
        Until::Exit => pinfold_core::wait::Until::Exit,
        Until::Prompt => pinfold_core::wait::Until::Prompt,
        Until::Output => pinfold_core::wait::Until::Output,
        Until::Idle(quiet) => pinfold_core::wait::Until::Idle(quiet),
        Until::Match(pattern) => match pinfold_core::wait::Until::matching(&pattern) {
            Ok(until) => until,
            Err(reason) => return audit::reply(log, reactor, request, Err(reason)),
        },
    };
    let deadline = Instant::now() + wait.timeout;
    if let Err(request) = waits.park(reactor, request, pane, until, deadline) {
        audit::reply(log, reactor, request, Err("the core is busy".to_owned()));
    }
}

/// The result of a wait that ended.
fn ended(outcome: Outcome) -> Value {
    match outcome {
        Outcome::Matched(line) => json!({"outcome": "match", "line": line}),
        Outcome::Idle => json!({"outcome": "idle"}),
        Outcome::Exit(exit) => json!({"outcome": "exit", "exit": exit}),
        Outcome::Prompt => json!({"outcome": "prompt"}),
        Outcome::Output => json!({"outcome": "output"}),
        Outcome::Closed => json!({"outcome": "closed"}),
        Outcome::Timeout => json!({"outcome": "timeout"}),
    }
}

/// Where a status came from: output's marks, which it can fake, or its agent's report (T4).
fn source(reported: bool) -> &'static str {
    if reported { "reported" } else { "marks" }
}

/// Each pane's number, status, its source and exit status, which is all an event tells.
type Seen = Vec<(usize, &'static str, &'static str, Option<u8>)>;

fn seen(reactor: &Reactor) -> Seen {
    let exit = |pane| {
        reactor
            .progress(pane)
            .and_then(|progress| progress.marks.exit)
    };
    let panes = reactor.list();
    panes
        .map(|pane| {
            let status = pane.status.name();
            (pane.id, status, source(pane.reported), exit(pane.id))
        })
        .collect()
}

/// The events between two looks at the panes, with nothing from a screen or title.
fn changes(before: &Seen, after: &Seen) -> Vec<Value> {
    let gone = before
        .iter()
        .filter(|was| after.iter().all(|pane| pane.0 != was.0));
    let mut events: Vec<Value> = gone
        .map(|&(id, ..)| json!({"event": "pane.closed", "pane": id}))
        .collect();
    for &(id, status, source, exit) in after {
        let event = match before.iter().find(|was| was.0 == id) {
            None => "pane.opened",
            Some(&was) if was != (id, status, source, exit) => "pane.status",
            Some(_) => continue,
        };
        let event = json!({"event": event, "pane": id, "status": status, "source": source,
                           "exit": exit});
        events.push(event);
    }
    events
}

fn wants(topics: Topics, event: &Value) -> bool {
    match event["event"].as_str() {
        Some("pane.opened") => topics.opened,
        Some("pane.closed") => topics.closed,
        _ => topics.status,
    }
}

/// A trusted layout's panes in the file's order: each one's directory and `run` line.
type Runs = Vec<(PathBuf, Option<String>)>;

/// Types each `run` line into its pane, then a newline, through the pane's input queue as keys
/// go: the core never runs one itself. The pane is a shell the core just started, so the checks
/// that keep an API send out of a program's prompt would only race its start.
fn type_runs(reactor: &mut Reactor, panes: &[Option<usize>], runs: &Runs) {
    for (pane, (_, run)) in panes.iter().zip(runs) {
        // One line each, as the prompt showed it: no control character can end or edit it.
        let run = run
            .as_ref()
            .filter(|run| !run.chars().any(char::is_control));
        if let (Some(pane), Some(run)) = (pane, run) {
            reactor.write(*pane, format!("{run}\n").as_bytes());
        }
    }
}

/// The panes of the layout at `root`, read again, never taken from a caller or a snapshot, if
/// `root` is canonical, the file's bytes hash to `sha256`, the trust store holds `root` with that
/// hash (ADR-0034, points 5 and 6), and it has `panes` panes, as another shape is another file.
fn trusted_runs(root: &Path, sha256: &str, panes: usize) -> Option<Runs> {
    if std::fs::canonicalize(root).ok()? != root {
        return None;
    }
    let bytes = layout::read(&root.join(".pinfold/layout.toml"), root).ok()?;
    let hash = trust::sha256(&bytes);
    let trusted = trust::state(&trust_path().ok()?, root.to_str()?, &hash) == State::Trusted;
    if hash != sha256 || !trusted {
        return None;
    }
    let tabs = layout::parse(&bytes, root).ok()?.tabs;
    let pane = |pane: layout::Pane| (pane.cwd.unwrap_or_else(|| root.to_path_buf()), pane.run);
    let runs: Runs = tabs
        .into_iter()
        .flat_map(|tab| tab.panes)
        .map(pane)
        .collect();
    (runs.len() == panes).then_some(runs)
}

/// Says once that sandboxed agents can reach abstract Unix sockets, if Landlock is below ABI 6.
fn log_abstract_socket_gap() {
    static SAID: Once = Once::new();
    if pinfold_sys::landlock::abi().is_ok_and(|abi| abi < pinfold_sys::landlock::SCOPED) {
        SAID.call_once(|| {
            let _ = writeln!(
                io::stderr(),
                "pinfold-core: Landlock is below ABI 6 here, so sandboxed agents can reach \
                 abstract Unix sockets"
            );
        });
    }
}

/// The session's snapshot, with the workspaces it opened.
fn saved(reactor: &Reactor, workspaces: &Workspaces) -> Snapshot {
    let mut snapshot = reactor.snapshot();
    snapshot.workspaces = workspaces.saved(reactor);
    snapshot
}

/// Saves the session, logging a failure by its kind alone, which names no path or pane. A
/// failed save stops nothing.
fn save(saver: &mut Saver, reactor: &Reactor, workspaces: &Workspaces) {
    if let Err(error) = saver.save(&saved(reactor, workspaces)) {
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: cannot save the session: {}",
            error.kind()
        );
    }
}

/// Restores the saved session (ADR-0034), runs the trusted `run` lines of its workspaces once,
/// and says whether it did. A failure is logged by its kind alone, which names no path or pane,
/// and leaves the core to start fresh.
fn restore_session(reactor: &mut Reactor, workspaces: &mut Workspaces, state: &Path) -> bool {
    let restored = snapshot::load(state, control::SESSION).and_then(|snapshot| {
        snapshot
            .map(|mut snapshot| {
                let trusted = snapshot.workspaces.iter().map(|saved| {
                    trusted_runs(Path::new(&saved.root), &saved.sha256, saved.panes.len())
                });
                let trusted: Vec<_> = trusted.collect();
                // A trusted line runs where its file says, never where output said the pane was.
                for (saved, runs) in snapshot.workspaces.iter().zip(&trusted) {
                    for (index, (cwd, _)) in saved.panes.iter().zip(runs.iter().flatten()) {
                        if let Some(pane) = index.and_then(|index| snapshot.panes.get_mut(index)) {
                            pane.cwd = cwd.to_str().map(str::to_owned);
                        }
                    }
                }
                reactor.restore(&snapshot)?;
                let opened = workspaces.restored(reactor, &snapshot.workspaces);
                for ((_, _, panes), runs) in opened.iter().zip(trusted) {
                    // A directory restore refuses starts a shell in `$HOME`: nothing runs there.
                    let kept = |(cwd, run): (PathBuf, Option<String>)| {
                        let run = run.filter(|_| snapshot::is_private(&cwd));
                        (cwd, run)
                    };
                    let runs: Runs = runs.into_iter().flatten().map(kept).collect();
                    type_runs(reactor, panes, &runs);
                }
                Ok(())
            })
            .transpose()
    });
    restored
        .unwrap_or_else(|error| {
            let _ = writeln!(
                io::stderr(),
                "pinfold-core: cannot restore the session: {}",
                error.kind()
            );
            None
        })
        .is_some()
}

/// Runs the saved session if `restore` and there is one, else `panes` shells, a tab each, until every pane has gone and no window is left to tell, or
/// until SIGTERM or SIGHUP, saving the session on the way out, when the window closes and a
/// minute after a change (ADR-0034). With a size, it opens a window for `cargo xtask mem-gate` and prints its pid.
fn daemon(mut panes: usize, measured: Option<&str>, restore: bool) -> io::Result<()> {
    let binary = window_binary()?;
    let mut reactor = Reactor::new()?;
    reactor.watch_signals()?;
    let state = pinfold_config::state_dir();
    let mut saver = state
        .clone()
        .map(|state| Saver::new(state, control::SESSION));
    let asked = listen(reactor.waker())?;
    let mut workspaces = Workspaces::default();
    if restore && state.is_some_and(|state| restore_session(&mut reactor, &mut workspaces, &state))
    {
        panes = 0;
    }
    for _ in 0..panes {
        reactor
            .open_tab(shell::command(), ROWS, COLUMNS)
            .map_err(io::Error::other)?;
    }
    if let Some(size) = measured {
        let mut command = Command::new(&binary);
        command.args(["--measure", size]);
        reactor.spawn_window_command(command)?;
        let pid = reactor.window_pid().unwrap_or_default();
        writeln!(io::stdout(), "{pid}")?;
    }
    // Who asked for the window, until it says hello or ends. A caller that timed out has gone
    // and hears nothing.
    let mut waiting = Vec::new();
    let mut waits: Waits<Request> = Waits::default();
    let mut grants: Grants<Request> = Grants::default();
    let mut family = Family::default();
    let mut subscribers: Vec<(Subscriber, Topics)> = Vec::new();
    let mut known = Seen::new();
    // Read once: the core takes no hot reload.
    let config = pinfold_cli::config();
    // Before any agent runs, and ready by the first sandboxed launch.
    let state = pinfold_config::state_dir();
    if let Some(state) = state.filter(|_| config.agents.values().any(|agent| agent.sandbox)) {
        sandbox::read_login_path(&state);
    }
    if let Some(patterns) = config.api.protect.clone() {
        reactor.set_patterns(patterns);
    }
    let mut log = audit::open(&config.api);
    let mut window = reactor.has_window();
    // A core with no pane waits for its first window, as `pinfold up` asks for one next.
    let mut started = reactor.pane_count() > 0 || window;
    let empty_until = Instant::now() + empty_wait();
    loop {
        let now = Instant::now();
        let deadline = waits.deadline().into_iter().chain(grants.deadline()).min();
        let wait = deadline.map(|at| at.saturating_duration_since(now));
        let wait = wait
            .into_iter()
            .chain(saver.as_ref().and_then(|saver| saver.wait(now)))
            .chain((!started).then(|| empty_until.saturating_duration_since(now)));
        output::tap(&mut reactor, &waits, &subscribers);
        reactor.turn(Some(wait.min().unwrap_or(IDLE)))?;
        output::hand_out(&mut reactor, &mut waits, &subscribers);
        waits.retain(|request| !request.is_gone());
        grants.retain(|request| !request.is_gone());
        subscribers.retain(|(subscriber, _)| !subscriber.is_gone());
        let mut released = Vec::new();
        while let Some((id, answer)) = reactor.take_approval_answer() {
            released.extend(grants.answer(id, answer));
        }
        if !reactor.has_window() && grants.deadline().is_some() {
            released.extend(grants.window_closed());
        }
        released.extend(grants.expire());
        answer_released(
            &mut reactor,
            &mut grants,
            &mut workspaces,
            &mut log,
            released,
        );
        while let Ok(request) = asked.try_recv() {
            // Its peer has been told the core is busy, so acting now would surprise a retry.
            if *request.method() != Method::WindowOpen && request.is_late() {
                continue;
            }
            match request.method().clone() {
                Method::WindowOpen => match reactor.spawn_window(&binary) {
                    Ok(_) => waiting.push(request),
                    // The system's words alone, which name no pane, secret or variable.
                    Err(error) => {
                        let reason = format!("cannot start pinfold-window: {error}");
                        audit::reply(&mut log, &reactor, request, Err(reason));
                    }
                },
                Method::PaneWait(wait) => {
                    park(&reactor, &mut family, &mut waits, &mut log, request, wait);
                }
                // Every pane of the session, as for pane.list.
                Method::EventsSubscribe(topics)
                    if topics.output
                        && request
                            .caller()
                            .and_then(|caller| reactor.pane_of(caller))
                            .is_none() =>
                {
                    let reason = "pane.output needs the caller's own pane";
                    audit::reply(&mut log, &reactor, request, Err(reason.to_owned()));
                }
                Method::EventsSubscribe(topics) if request.caller().is_some() => {
                    if subscribers.is_empty() {
                        known = seen(&reactor);
                    }
                    subscribers.push((request.subscriber(), topics));
                    audit::reply(&mut log, &reactor, request, Ok(json!({})));
                }
                Method::EventsSubscribe(_) => {
                    let reason = "events.subscribe needs a caller in a terminal";
                    audit::reply(&mut log, &reactor, request, Err(reason.to_owned()));
                }
                Method::AuditRead { .. } => {
                    let read = audit::read(&log, &reactor, &request);
                    audit::reply(&mut log, &reactor, request, read);
                }
                Method::WorkspaceAskTrust(..) => {
                    ask_trust(
                        &mut reactor,
                        &mut grants,
                        &mut workspaces,
                        &mut log,
                        request,
                    );
                }
                _ => {
                    decide(
                        &mut reactor,
                        &mut family,
                        &config.agents,
                        &mut grants,
                        &mut workspaces,
                        &mut log,
                        request,
                    );
                }
            }
        }
        for (request, outcome) in waits.settle(&reactor, Instant::now()) {
            audit::reply(&mut log, &reactor, request, Ok(ended(outcome)));
        }
        if !subscribers.is_empty() {
            let now = seen(&reactor);
            for event in changes(&known, &now) {
                let to = subscribers
                    .iter()
                    .filter(|(_, topics)| wants(*topics, &event));
                to.for_each(|(subscriber, _)| subscriber.send(event.clone()));
            }
            known = now;
        }
        let ended = !reactor.has_window();
        if ended || reactor.is_window_ready() {
            let result = match (ended, reactor.take_other_version()) {
                (false, _) => Ok(json!({})),
                (true, Some(theirs)) => Ok(updated(&reactor, theirs)),
                (true, None) => Err("the window ended before it was ready".to_owned()),
            };
            for request in waiting.drain(..) {
                audit::reply(&mut log, &reactor, request, result.clone());
            }
        }
        started |= reactor.pane_count() > 0 || reactor.has_window();
        let ending = reactor.is_terminated()
            || (started || Instant::now() >= empty_until)
                && reactor.pane_count() == 0
                && !reactor.has_window();
        if let Some(saver) = saver.as_mut() {
            let now = Instant::now();
            if reactor.was_active() {
                saver.touch(now);
            }
            let closed = window && !reactor.has_window();
            if ending || closed || saver.is_due(now) {
                save(saver, &reactor, &workspaces);
            }
        }
        window = reactor.has_window();
        if ending {
            return Ok(());
        }
    }
}

/// Most characters of a title or command in an `update`.
const MAX_NAMED: usize = 40;

/// `window.open`'s answer when the window binary beside this core speaks link version
/// `theirs`: PinFold was installed again under a running session. It names the panes that a
/// restart would end, by their cwd's last part and foreground program, redacted and cut short, so the
/// launcher can ask the person first. A restart that ends none goes ahead without asking.
fn updated(reactor: &Reactor, theirs: u16) -> Value {
    let short = |text: &str| redact(text).chars().take(MAX_NAMED).collect::<String>();
    let running: Vec<Value> = reactor
        .list()
        .filter_map(|pane| {
            let program = reactor.would_end(pane.id)?;
            Some(json!({"pane": pane.id, "title": pane.title.map(short), "program": short(&program)}))
        })
        .collect();
    let _ = writeln!(
        io::stderr(),
        "pinfold-core: the window is link version {theirs} and this core {}: \
         PinFold was updated; panes running a program a restart would end: {}",
        link::VERSION,
        running.len()
    );
    json!({"update": {"window": theirs, "core": link::VERSION, "running": running}})
}

/// Hardens, then runs `panes` shells, the user's, unless it restores a session.
fn serve(panes: usize) -> io::Result<()> {
    pinfold_sys::harden()?;
    daemon(panes, None, true)
}

/// Plays `file` through a real pane, says so on stdout, then idles until killed.
/// `cargo xtask mem-gate` measures this.
fn replay(file: &str) -> io::Result<()> {
    let mut reactor = Reactor::new()?;
    let mut cat = Command::new("cat");
    cat.arg(file);
    let pane = reactor.spawn(cat, 50, 120)?;
    while !reactor.is_closed(pane) {
        reactor.turn(Some(Duration::from_secs(1)))?;
    }
    writeln!(io::stdout(), "drained")?;
    loop {
        reactor.turn(Some(IDLE))?;
    }
}

fn run(args: &[&str]) -> io::Result<()> {
    match *args {
        [] => serve(1),
        // `pinfold up`, whose workspace is all a fresh core should open.
        ["--no-shell"] => serve(0),
        // The daemon without hardening, which also leaves a way to attach a debugger
        // (ADR-0020). `cargo xtask mem-gate` measures these two, as /proc refuses a hardened one,
        // so they restore nothing, which would change what it measures.
        ["--idle", panes] => daemon(panes.parse().map_err(io::Error::other)?, None, false),
        ["--window", panes, size] => {
            daemon(panes.parse().map_err(io::Error::other)?, Some(size), false)
        }
        ["--replay", file] => replay(file),
        _ => Err(io::Error::other(
            "usage: pinfold-core [--no-shell | --idle <panes> | --window <panes> <width>x<height> | --replay <file>]",
        )),
    }
}

fn main() -> ExitCode {
    pinfold_cli::panics_name_no_text();
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold-core: {error}");
            ExitCode::FAILURE
        }
    }
}
