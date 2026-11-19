//! The `pinfold-core` binary.

use std::collections::{BTreeMap, VecDeque};
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
use pinfold_api::spawn::Place;
use pinfold_api::tool::{Run, Tool};
use pinfold_api::trust::{self, State};
use pinfold_api::{KEY_CONTEXT, redact, redact_last_lines};
use pinfold_cli::audit;
use pinfold_cli::output;
use pinfold_cli::timeline;
use pinfold_config::Agent;
use pinfold_config::workspace as layout;
use pinfold_core::family::{self, Family, SplitWay};
use pinfold_core::grants::{Decision, Grants, Released};
use pinfold_core::io::{Reactor, Waker};
use pinfold_core::rewind::{self, Missing, Rewind, Settings};
use pinfold_core::snapshot::{self, Saver, Snapshot};
use pinfold_core::status::Status;
use pinfold_core::timeline::{Answer, Timelines};
use pinfold_core::wait::{Outcome, Waits};
use pinfold_core::workspace::Workspaces;
use pinfold_core::{sandbox, shell, tool, webhook};
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
/// How long a measured window has to say hello. A window is up in a second or two, and
/// `cargo xtask mem-gate` must not read a core whose window never came.
const HELLO_WAIT: Duration = Duration::from_secs(30);

/// [`EMPTY_WAIT`], or in a debug build `PINFOLD_TEST_EMPTY_WAIT_MS`, so its test takes seconds.
fn empty_wait() -> Duration {
    let shorter = env::var("PINFOLD_TEST_EMPTY_WAIT_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions));
    shorter
        .and_then(|ms| ms.parse().ok())
        .map_or(EMPTY_WAIT, Duration::from_millis)
}

/// Returns the binary called `name` beside this one. Each is resolved once, so nothing later
/// can name another program, and never through `PATH`.
fn sibling(name: &str) -> io::Result<PathBuf> {
    Ok(env::current_exe()?.canonicalize()?.with_file_name(name))
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
        // Lines above the cut tell redaction whether it falls inside a key block.
        Method::PaneRead { pane, lines } if cross_pane => {
            let text = reactor
                .read(*pane, lines.map(|lines| lines.saturating_add(KEY_CONTEXT)))
                .ok_or("no such pane")?;
            Ok(json!({"text": redact_last_lines(&text, lines.unwrap_or(usize::MAX))}))
        }
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
/// that change something are asked in the window first, by [`decide`], and refused here.
fn immediate<T, C: pinfold_core::grants::Clock>(
    reactor: &mut Reactor,
    family: &mut Family,
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
                    "tool.open",
                    "checkpoint.take",
                    "checkpoint.list",
                    "checkpoint.restore",
                    "run.timeline",
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
                    "checkpoint.take",
                    "checkpoint.list",
                    "checkpoint.restore",
                    "run.timeline",
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
        // A tool runs as the caller's child, so a sandboxed caller is refused as for any pane.
        Method::ToolOpen(open) => {
            let own = own.ok_or("tool.open needs a caller in a pane")?;
            let of = open.of.unwrap_or(own);
            let binary = reactor.tools().ok_or("the tools' binary is not known")?;
            let command = tool::command(binary, &open.run)?;
            let pane = family.spawn_command(reactor, own, of, way(open.place), None, command)?;
            reactor.set_tool(pane, open.run.clone());
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
                let mut listed = json!({"id": pane.id, "title": pane.title, "cwd": pane.cwd,
                       "command": pane.command, "status": pane.status.name(),
                       "source": source(pane.reported), "message": message,
                       "focused": pane.focused, "since": pane.since, "started": pane.started,
                       "summary": timeline::counters(reactor, pane.id)});
                if let Some(run) = pane.tool {
                    listed["kind"] = json!("tool");
                    listed["tool"] = run.describe();
                }
                listed
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
        | Method::AgentLaunch(_)
        | Method::CheckpointTake { .. }
        | Method::CheckpointList { .. }
        | Method::CheckpointRestore { .. }
        | Method::RunTimeline { .. }
        | Method::AuditRead { .. } => Err("not here".to_owned()),
    }
}

/// A launch whose agent waits for its checkpoint, or a checkpoint call waiting on a worker: a
/// take or list, a restore's plan, the restore the window allowed, or a `tool.open` of a
/// checkpoint waiting on its view checkpoint.
enum Parked {
    Launch(Request, Box<Prepared>),
    Call(Request),
    Plan(Request),
    Restore(Request),
    View(Request),
    /// The window's "Rewind agent changes" on the agent's pane, which no caller asked for, and
    /// the checkpoint its viewer starts from.
    Window(usize, u32),
}

impl Parked {
    fn request(&self) -> Option<&Request> {
        Some(match self {
            Self::Launch(request, _)
            | Self::Call(request)
            | Self::Plan(request)
            | Self::Restore(request)
            | Self::View(request) => request,
            Self::Window(..) => return None,
        })
    }
}

/// An `agent.launch` checked and ready to start: its pane, directory and command are fixed.
struct Prepared {
    own: usize,
    of: usize,
    way: Option<SplitWay>,
    cwd: String,
    command: Command,
    work: Option<sandbox::WorkingDirectory>,
    /// The profile's place in the config's, or `u8::MAX`.
    profile: u8,
    sandbox: bool,
    /// Whether the profile takes checkpoints.
    rewind: bool,
}

impl Prepared {
    /// Where the agent starts, which its checkpoints cover.
    fn dir(&self) -> &Path {
        self.work
            .as_ref()
            .map_or(Path::new(&self.cwd), |work| work.path())
    }
}

/// Checks an `agent.launch` and builds its command. An agent launches only by its profile's
/// name in `agents`, the user's config, and a sandboxed agent and the panes under it start only
/// sandboxed agents in its working directory.
fn prepare(
    reactor: &mut Reactor,
    family: &mut Family,
    agents: &BTreeMap<String, Agent>,
    own: Option<usize>,
    launch: &pinfold_api::spawn::Launch,
) -> Result<Prepared, String> {
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
        let state = pinfold_config::state_dir().ok_or("no state directory, as HOME is not set")?;
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
        let command = sandbox::command(
            &agent.command,
            &hidden,
            &agent.keep_env,
            &work,
            &api,
            shadow,
        );
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
    // The profile's place in the config's, which the core read when it started; past 255
    // profiles the index names none.
    let profile = agents.keys().position(|name| *name == launch.profile);
    let profile = profile
        .and_then(|at| u8::try_from(at).ok())
        .unwrap_or(u8::MAX);
    Ok(Prepared {
        own,
        of,
        way: way(launch.place),
        cwd,
        command,
        work,
        profile,
        sandbox: agent.sandbox,
        rewind: agent.rewind,
    })
}

/// Starts a prepared agent in its pane, whose reach and containment are checked again.
fn start_agent(
    reactor: &mut Reactor,
    family: &mut Family,
    prepared: Prepared,
) -> Result<usize, String> {
    let Prepared {
        own,
        of,
        way,
        cwd,
        command,
        work,
        profile,
        sandbox,
        ..
    } = prepared;
    let pane = match work {
        Some(work) => family.spawn_sandboxed(reactor, own, of, way, work.path(), command)?,
        None => family.spawn_command(reactor, own, of, way, Some(&cwd), command)?,
    };
    reactor.read_agent_signals(pane, profile);
    if sandbox && let Some((count, names)) = reactor.launched_sandboxed() {
        // Names only, never a value (ADR-0047).
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: warn secret_names={count} [{names}]: on macOS sandboxed agents \
             can read these variables in other panes; strip_secrets = true removes them"
        );
    }
    Ok(pane)
}

/// `agent.launch`: starts the agent at once, or with `[rewind]` on for its profile once its
/// launch checkpoint is taken or `wait_s` passed (ADR-0050 section 4). Never waits here.
fn launch_agent(
    reactor: &mut Reactor,
    family: &mut Family,
    agents: &BTreeMap<String, Agent>,
    rewind: Option<&mut Rewind<Parked>>,
    log: &mut Log,
    request: Request,
) {
    let Method::AgentLaunch(launch) = request.method().clone() else {
        return;
    };
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let prepared = match prepare(reactor, family, agents, own, &launch) {
        Ok(prepared) => prepared,
        Err(reason) => return audit::reply(log, reactor, request, Err(reason)),
    };
    let Some(rewind) = rewind.filter(|_| prepared.rewind) else {
        let answer = start_agent(reactor, family, prepared).map(|pane| json!({"pane": pane}));
        return audit::reply(log, reactor, request, answer);
    };
    let dir = prepared.dir().to_owned();
    match rewind.open(prepared.own, &dir) {
        Ok(run) => {
            let parked = Parked::Launch(request, Box::new(prepared));
            if let Err((Parked::Launch(request, prepared), missing)) =
                rewind.launch(run, dir, parked, Instant::now())
            {
                no_checkpoint(rewind.run(run).map(rewind::Run::id), missing);
                launched(
                    reactor,
                    family,
                    rewind,
                    log,
                    (request, *prepared),
                    run,
                    None,
                );
            }
        }
        Err(missing) => {
            no_checkpoint(None, missing);
            let started = start_agent(reactor, family, prepared);
            let answer = started.map(|pane| json!({"pane": pane, "checkpoint": null}));
            audit::reply(log, reactor, request, answer);
        }
    }
}

/// Says why a launch has no checkpoint, naming the run and never a path.
fn no_checkpoint(run: Option<String>, missing: Missing) {
    let run = run.unwrap_or_else(|| "none".to_owned());
    let _ = writeln!(
        io::stderr(),
        "pinfold-core: warn checkpoint=none run={run}: {}",
        missing.word()
    );
}

/// Starts a parked launch's agent and answers with its launch checkpoint, or null.
fn launched(
    reactor: &mut Reactor,
    family: &mut Family,
    rewind: &mut Rewind<Parked>,
    log: &mut Log,
    (request, prepared): (Request, Prepared),
    run: usize,
    checkpoint: Option<String>,
) {
    let started = start_agent(reactor, family, prepared);
    rewind.started(run, started.as_ref().ok().copied());
    let answer = started.map(|pane| json!({"pane": pane, "checkpoint": checkpoint}));
    audit::reply(log, reactor, request, answer);
}

/// `checkpoint.take` and `checkpoint.list` (ADR-0050 section 7) for the run in `pane`, queued
/// for a worker. A caller in a pane reaches its own run, a run in a pane it spawned while that
/// is open, and a run it launched; a terminal outside every pane reaches any run.
fn checkpoint_call(
    reactor: &Reactor,
    family: &mut Family,
    rewind: Option<&mut Rewind<Parked>>,
    log: &mut Log,
    request: Request,
    pane: usize,
) {
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let mut reach = |rewind: &Rewind<Parked>| {
        if request.caller().is_none() {
            return Err("checkpoint calls need a caller in a terminal");
        }
        let run = rewind.of_pane(pane).ok_or("no agent run in that pane")?;
        let launcher = rewind.run(run).map(rewind::Run::launcher);
        match own {
            Some(own)
                if own != pane
                    && Some(own) != launcher
                    && family.reach(reactor, Some(own), pane).is_err() =>
            {
                Err("the run is neither the caller's own nor one it launched")
            }
            _ => Ok(run),
        }
    };
    let Some(rewind) = rewind else {
        let reason = "checkpoints are off: [rewind] enabled = false";
        return audit::reply(log, reactor, request, Err(reason.to_owned()));
    };
    let run = match reach(rewind) {
        Ok(run) => run,
        Err(reason) => return audit::reply(log, reactor, request, Err(reason.to_owned())),
    };
    let queued = match request.method() {
        Method::CheckpointTake { .. } => rewind.take(run, Parked::Call(request), Instant::now()),
        _ => rewind.list(run, Parked::Call(request)),
    };
    if let Err((Parked::Call(request), reason)) = queued {
        audit::reply(log, reactor, request, Err(reason.to_owned()));
    }
}

/// `tool.open` of a run's checkpoint (ADR-0050 section 6): checked as `checkpoint.take` reaches
/// a run and as `tool.open` opens a pane, then the run's `view` checkpoint is queued, and the
/// viewer starts once it is taken, in [`viewed`].
fn view_call(
    reactor: &Reactor,
    family: &mut Family,
    rewind: Option<&mut Rewind<Parked>>,
    log: &mut Log,
    request: Request,
) {
    let Method::ToolOpen(open) = request.method() else {
        return;
    };
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let checked = (|| {
        let rewind = rewind.ok_or("checkpoints are off: [rewind] enabled = false")?;
        let own = own.ok_or("tool.open needs a caller in a pane")?;
        family.reach(reactor, Some(own), open.of.unwrap_or(own))?;
        family.contain(reactor, own, None)?;
        reactor.tools().ok_or("the tools' binary is not known")?;
        if !Path::new(&open.run.path).is_dir() {
            return Err("files and diff open an existing directory");
        }
        let id = open
            .run
            .checkpoint
            .as_deref()
            .and_then(|id| id.rsplit_once('/'));
        let run = id
            .and_then(|(id, _)| rewind.of_id(id))
            .ok_or("no such run")?;
        let record = rewind.run(run).ok_or("no such run")?;
        let pane = record.pane().ok_or("the run's agent never started")?;
        if own != pane
            && own != record.launcher()
            && family.reach(reactor, Some(own), pane).is_err()
        {
            return Err("the run is neither the caller's own nor one it launched");
        }
        Ok((rewind, run))
    })();
    let (rewind, run) = match checked {
        Ok(checked) => checked,
        Err(reason) => return audit::reply(log, reactor, request, Err(reason.to_owned())),
    };
    // No caller takes checkpoints faster this way than with checkpoint.take.
    let queued = rewind.view(run, Parked::View(request), Some(Instant::now()));
    if let Err((Parked::View(request), reason)) = queued {
        audit::reply(log, reactor, request, Err(reason.to_owned()));
    }
}

/// Opens the viewer of a `tool.open` whose view checkpoint `taken` ended, or says why not.
fn viewed(
    reactor: &mut Reactor,
    family: &mut Family,
    rewind: &Rewind<Parked>,
    request: &Request,
    run: usize,
    taken: Result<Value, String>,
) -> Result<Value, String> {
    let Method::ToolOpen(open) = request.method() else {
        return Err("not here".to_owned());
    };
    let shown = taken.map_err(answered_error)?;
    let shown = shown["checkpoint"].as_str().ok_or("no view checkpoint")?;
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let own = own.ok_or("tool.open needs a caller in a pane")?;
    let record = rewind.run(run).ok_or("the run has gone")?;
    let pane = record.pane().ok_or("the run has gone")?;
    let binary = reactor.tools().ok_or("the tools' binary is not known")?;
    let command = tool::rewind(binary, &open.run, &record.key(), shown, pane)?;
    let of = open.of.unwrap_or(own);
    let opened = family.spawn_command(reactor, own, of, way(open.place), None, command)?;
    reactor.set_tool(opened, open.run.clone());
    Ok(json!({"pane": opened}))
}

/// The window's "Rewind agent changes" on `pane` (ADR-0050 slice 7): the run's view
/// checkpoint is queued, and the viewer opens since checkpoint `from` once it is taken, the
/// launch one or one a timeline entry named (ADR-0052 §6). A pane with no run is refused with
/// the window's flash, never an empty viewer.
fn window_view(reactor: &mut Reactor, rewind: Option<&mut Rewind<Parked>>, pane: usize, from: u32) {
    let refused = match rewind.map(|rewind| (rewind.of_pane(pane), rewind)) {
        None => "checkpoints are off",
        Some((None, _)) => "no agent run in this pane",
        Some((Some(run), rewind)) => match rewind.view(run, Parked::Window(pane, from), None) {
            Ok(()) => return,
            Err(_) => "the core is busy taking checkpoints",
        },
    };
    reactor.tell_restored(0, 0, refused);
}

/// Opens the window's viewer of the run in `pane` once its view checkpoint `taken` ended, since
/// the run's checkpoint `from`, or flashes why not.
fn window_viewed(
    reactor: &mut Reactor,
    rewind: &Rewind<Parked>,
    (pane, from): (usize, u32),
    run: usize,
    taken: &Result<Value, String>,
) {
    let opened = (|| {
        let shown = taken.as_ref().ok()?["checkpoint"].as_str()?;
        let record = rewind.run(run)?;
        let since = format!("{}/{from}", record.id());
        reactor.open_rewind(pane, &record.key(), &since, shown).ok()
    })();
    if opened.is_none() {
        reactor.tell_restored(0, 0, "the viewer could not open");
    }
}

/// Whether `request` is a `checkpoint.restore` from `caller`.
fn restore_from(request: &Request, caller: Option<pinfold_sys::Terminal>) -> bool {
    matches!(request.method(), Method::CheckpointRestore { .. }) && request.caller() == caller
}

/// `checkpoint`'s number if it is a checkpoint of `run`, `<run>/<n>`.
fn number_of(run: &rewind::Run, checkpoint: &str) -> Option<u32> {
    let (id, n) = checkpoint.rsplit_once('/')?;
    (id == run.id()).then(|| n.parse().ok())?
}

/// `checkpoint.restore` (ADR-0050 sections 6 and 7). Refused before any prompt unless the
/// caller is a tool pane or a terminal outside every pane, the run is one of this core's and both
/// checkpoints are its own, a window can show the `Rewind` prompt, and the caller has no other
/// restore waiting. A tool pane reaches any run, as a terminal outside every pane does: the
/// window's prompt still guards every restore. Then the plan is queued for a worker.
fn restore_call(
    reactor: &Reactor,
    rewind: Option<&mut Rewind<Parked>>,
    grants: &Grants<Request>,
    log: &mut Log,
    request: Request,
) {
    let Method::CheckpointRestore {
        pane,
        checkpoint,
        shown,
        paths,
    } = request.method().clone()
    else {
        return;
    };
    let caller = request.caller();
    let own = caller.and_then(|caller| reactor.pane_of(caller));
    let tool = |own| {
        reactor
            .list()
            .any(|listed| listed.id == own && listed.tool.is_some())
    };
    let checked = (|| {
        let rewind = rewind.ok_or("checkpoints are off: [rewind] enabled = false")?;
        caller.ok_or("checkpoint calls need a caller in a terminal")?;
        if own.is_some_and(|own| !tool(own)) {
            return Err("checkpoint.restore needs a tool pane or a terminal outside every pane");
        }
        let run = rewind.of_pane(pane).ok_or("no agent run in that pane")?;
        let record = rewind.run(run).ok_or("no agent run in that pane")?;
        let numbers = number_of(record, &checkpoint).zip(number_of(record, &shown));
        let numbers = numbers.ok_or("the checkpoints are not that run's")?;
        if !reactor.can_ask(ApprovalAction::Rewind) {
            return Err("approval needs an open PinFold window");
        }
        let waiting = |parked: &Parked| parked.request().is_some_and(|r| restore_from(r, caller));
        if grants.any(|ticket| restore_from(ticket, caller)) || rewind.any(waiting) {
            return Err("a restore from this caller is already waiting");
        }
        Ok((rewind, run, numbers))
    })();
    let (rewind, run, numbers) = match checked {
        Ok(checked) => checked,
        Err(reason) => return audit::reply(log, reactor, request, Err(reason.to_owned())),
    };
    if let Err((parked, reason)) =
        rewind.plan(run, numbers, paths.as_deref(), Parked::Plan(request))
    {
        let Parked::Plan(request) = parked else {
            return;
        };
        audit::reply(log, reactor, request, Err(reason.to_owned()));
    }
}

/// Asks the window about a restore whose plan ended, with texts from the store and never the
/// caller, or refuses it. A caller that has gone is asked nothing.
fn restore_planned(
    reactor: &mut Reactor,
    grants: &mut Grants<Request>,
    log: &mut Log,
    request: Request,
    plan: Result<serde_json::Value, String>,
) {
    if request.is_gone() {
        return;
    }
    let Method::CheckpointRestore { pane, .. } = request.method() else {
        return;
    };
    let pane = *pane;
    let plan = match plan {
        Ok(plan) => plan,
        Err(why) => return audit::reply(log, reactor, request, Err(answered_error(why))),
    };
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let running = reactor
        .status(pane)
        .is_some_and(|(status, ..)| status == Status::Running);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let texts = rewind::prompt_texts(&plan, own, running, now);
    let ready = reactor.can_ask(ApprovalAction::Rewind);
    match grants.ask_rewind(request, pane, texts, ready) {
        Decision::Ask(prompt) if reactor.ask_approval(&prompt) => {}
        Decision::Ask(prompt) => {
            for released in grants.answer(prompt.id, ApprovalAnswer::Deny) {
                let reason = "approval needs an open PinFold window".to_owned();
                audit::reply(log, reactor, released.ticket, Err(reason));
            }
        }
        Decision::Refuse(request, reason) => {
            audit::reply(log, reactor, request, Err(reason.to_owned()));
        }
        Decision::Allow(request) => {
            audit::reply(log, reactor, request, Err("not asked".to_owned()));
        }
    }
}

/// Queues the restore the window allowed, or answers one it refused, timed out or lost. Only the
/// window's answer gets here.
fn restore_answered(
    reactor: &mut Reactor,
    rewind: Option<&mut Rewind<Parked>>,
    log: &mut Log,
    released: Released<Request>,
) {
    let request = released.ticket;
    if !released.allowed {
        return audit::reply(log, reactor, request, Err(released.reason.to_owned()));
    }
    let Method::CheckpointRestore {
        pane,
        checkpoint,
        shown,
        paths,
    } = request.method().clone()
    else {
        return;
    };
    // The run is found again, so a record reused while the prompt waited is never restored.
    let queued = rewind.ok_or("checkpoints are off").and_then(|rewind| {
        let run = rewind.of_pane(pane).ok_or("the run has gone")?;
        let record = rewind.run(run).ok_or("the run has gone")?;
        let numbers = number_of(record, &checkpoint).zip(number_of(record, &shown));
        let numbers = numbers.ok_or("the run has gone")?;
        Ok((rewind, run, numbers))
    });
    let failed = match queued {
        Ok((rewind, run, numbers)) => {
            match rewind.restore(run, numbers, paths.as_deref(), Parked::Restore(request)) {
                Ok(()) => return,
                Err((Parked::Restore(request), reason)) => (request, reason),
                Err(_) => return,
            }
        }
        Err(reason) => (request, reason),
    };
    let (request, reason) = failed;
    reactor.tell_restored(0, 0, rewind::restore_error(reason));
    audit::reply(log, reactor, request, Err(reason.to_owned()));
}

/// What a caller hears when checkpoint work failed: a path is never named.
fn answered_error(why: String) -> String {
    match why {
        why if why == rewind::KILLED => "the checkpoint took over 60 s".to_owned(),
        why if why.contains('/') && !why.starts_with("checkpoint r") => {
            "the checkpoint was refused or failed".to_owned()
        }
        why => why,
    }
}

/// Writes what the last turn answered into the agent runs' timelines (ADR-0052 slice 2): the
/// calls the audit log recorded, the prompts that ended and the checkpoints taken, each joined
/// to its run by its pane.
fn record_timelines(
    reactor: &mut Reactor,
    log: &Log,
    grants: &mut Grants<Request>,
    rewind: Option<&mut Rewind<Parked>>,
) {
    let answered = grants.answered();
    let taken = rewind.map(|rewind| {
        let taken = rewind.taken().into_iter();
        let pane = |run| rewind.run(run).and_then(rewind::Run::pane);
        taken
            .filter_map(|taken| Some((pane(taken.run)?, taken)))
            .collect::<Vec<_>>()
    });
    let Some(timelines) = reactor.timelines_mut() else {
        return;
    };
    let now = Instant::now();
    // A prompt is answered before the calls it releases are.
    for prompt in answered {
        let answer = match prompt.answer {
            Some(ApprovalAnswer::AllowOnce) => Answer::Once,
            Some(ApprovalAnswer::AllowForThirtyMinutes) => Answer::ThirtyMinutes,
            Some(ApprovalAnswer::Deny) => Answer::Denied,
            None => Answer::Expired,
        };
        let (caller, target) = (prompt.caller, prompt.target);
        timelines.approval(caller, target, prompt.action, answer, prompt.id, now);
    }
    timelines.calls(log, now);
    for (pane, taken) in taken.into_iter().flatten() {
        let (n, reason, complete) = (taken.n, taken.reason, taken.complete);
        timelines.checkpoint(pane, n, reason, complete, taken.changed, now);
    }
}

/// Answers each launch and call whose checkpoint work ended.
fn checkpoints_ended(
    reactor: &mut Reactor,
    family: &mut Family,
    rewind: &mut Rewind<Parked>,
    grants: &mut Grants<Request>,
    log: &mut Log,
) {
    for ended in rewind.poll(Instant::now()) {
        match ended.payload {
            Parked::Plan(request) => restore_planned(reactor, grants, log, request, ended.result),
            Parked::Restore(request) => {
                // The window's flash, whether or not the caller still waits.
                match &ended.result {
                    Ok(line) => {
                        let count = |name: &str| {
                            u32::try_from(line[name].as_u64().unwrap_or(0)).unwrap_or(u32::MAX)
                        };
                        let undid = count("restored").saturating_add(count("deleted"));
                        reactor.tell_restored(undid, count("skipped"), "");
                    }
                    Err(why) => reactor.tell_restored(0, 0, rewind::restore_error(why)),
                }
                audit::reply(log, reactor, request, ended.result.map_err(answered_error));
            }
            Parked::Launch(request, prepared) => {
                let checkpoint = match ended.result {
                    Ok(line) => line["checkpoint"].as_str().map(str::to_owned),
                    Err(_) => {
                        let missing = rewind.run(ended.run).map(rewind::Run::missing);
                        no_checkpoint(
                            rewind.run(ended.run).map(rewind::Run::id),
                            missing.unwrap_or_default(),
                        );
                        None
                    }
                };
                let parked = (request, *prepared);
                launched(reactor, family, rewind, log, parked, ended.run, checkpoint);
            }
            Parked::View(request) => {
                let answer = viewed(reactor, family, rewind, &request, ended.run, ended.result);
                audit::reply(log, reactor, request, answer);
            }
            Parked::Window(pane, from) => {
                window_viewed(reactor, rewind, (pane, from), ended.run, &ended.result);
            }
            Parked::Call(request) => {
                let answer = ended.result.map(|mut line| {
                    if let Some(fields) = line.as_object_mut() {
                        fields.remove("store");
                    }
                    line
                });
                // An error naming a path, such as a refused store, is never logged or answered.
                let answer = answer.map_err(answered_error);
                audit::reply(log, reactor, request, answer);
            }
        }
    }
}

/// The checkpoints of `[rewind]`, run by the sibling `pinfold`, or none if it is off. A debug
/// build takes another program from `PINFOLD_TEST_CHECKPOINT`, so a session test can make a
/// take slow.
fn rewind(
    config: &pinfold_config::Rewind,
    waker: Arc<Waker>,
) -> io::Result<Option<Rewind<Parked>>> {
    if !config.enabled {
        return Ok(None);
    }
    let program = env::var_os("PINFOLD_TEST_CHECKPOINT")
        .filter(|_| cfg!(debug_assertions))
        .map_or_else(|| sibling("pinfold"), |program| Ok(PathBuf::from(program)))?;
    let settings = Settings {
        on_done: config.on_done,
        wait: Duration::from_secs(config.wait_s),
        max_files: config.max_files,
        max_file_mib: config.max_file_mib,
        max_new_mib: config.max_new_mib,
        include_ignored: config.include_ignored,
        max_store_mib: config.max_store_mib,
        keep_days: config.keep_days,
        keep_per_run: config.keep_per_run,
    };
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    Ok(Some(Rewind::new(program, settings, start, Some(waker))))
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
            // A denial writes no session.
            answer_released(reactor, grants, workspaces, log, None, released);
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

/// The saver, if `HOME` is set, and the `session.save` requests waiting on its writes.
type Saving<'a> = Option<(&'a mut Saver, &'a mut VecDeque<Request>)>;

fn answer_released(
    reactor: &mut Reactor,
    grants: &mut Grants<Request>,
    workspaces: &mut Workspaces,
    log: &mut Log,
    mut saving: Saving<'_>,
    released: Vec<Released<Request>>,
) {
    for released in released {
        let answer = match released.ticket.method() {
            // Parked until the save thread's write ends, so a slow disk stalls no pane.
            Method::SessionSave if released.allowed => match saving.as_mut() {
                Some((saver, parked)) => match saver.ask(&saved(reactor, workspaces)) {
                    Ok(()) => {
                        parked.push_back(released.ticket);
                        continue;
                    }
                    Err(error) => Err(format!("cannot save the session: {error}")),
                },
                None => {
                    Err("HOME is not set, or a failed restore's file is still in place".to_owned())
                }
            },
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
        // A window of link version 9 has no owner prompt, so this refuses as with no window.
        let decision = grants.ask_owner(request, texts, reactor.can_ask(ApprovalAction::Owner));
        return ask_alone(reactor, grants, workspaces, log, decision);
    }
    let Some((target, action, text)) = cross_pane(request.method()) else {
        let answer = immediate(reactor, family, grants, &request);
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
    // Refused before any prompt, as no grant lets a caller type into a tool (ADR-0045).
    let tool = reactor
        .list()
        .any(|pane| pane.id == target && pane.tool.is_some());
    if tool && action == ApprovalAction::Send {
        return audit::reply(log, reactor, request, Err(tool::NO_SEND.to_owned()));
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
            // A denial writes no session.
            answer_released(reactor, grants, workspaces, log, None, released);
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

/// Answers each parked `session.save` whose write has ended, in the order they were parked.
fn answer_saved(
    log: &mut Log,
    reactor: &Reactor,
    saver: &mut Saver,
    parked: &mut VecDeque<Request>,
) {
    for answer in saver.answered() {
        let Some(request) = parked.pop_front() else {
            break;
        };
        let answer = answer
            .map(|path| json!({"path": path.to_string_lossy()}))
            .map_err(|error| format!("cannot save the session: {error}"));
        audit::reply(log, reactor, request, answer);
    }
}

/// Hands the session to the save thread, logging a failure by its kind alone, which names no
/// path or pane. A failed save stops nothing.
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
/// and leaves the core to start fresh, with the saved file moved aside so the fresh session's
/// saves do not replace it (#395).
///
/// # Errors
///
/// Returns an error if a restore failed and its file could not be moved aside. The caller
/// must then not save.
fn restore_session(
    reactor: &mut Reactor,
    workspaces: &mut Workspaces,
    state: &Path,
) -> io::Result<bool> {
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
    restored.map(|restored| restored.is_some()).or_else(|error| {
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: cannot restore the session: {}; its file is kept as a .failed or .bad file beside it",
            error.kind()
        );
        // A file that did not parse is already `.bad`, and then there is nothing to move.
        snapshot::set_aside(state, control::SESSION).map(|()| false)
    })
}

/// Turns on [`sandbox::strip_secrets`] if `on`, and logs the secret-looking names, never values,
/// this core still has, as one not started by the launcher does: a sandboxed agent on macOS can
/// read them from the core (ADR-0047), though no pane gets them.
fn strip_secrets(on: bool) {
    sandbox::strip_secrets(on);
    let names: Vec<_> = env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| on && sandbox::secret_name(name))
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if !names.is_empty() {
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: strip_secrets is on, but this core started with {}; no pane gets \
             them, but on macOS a sandboxed agent can read them from the core: start PinFold \
             with `pinfold`, which leaves them out",
            names.join(", ")
        );
    }
}

/// The webhook's sender, when `[alerts] webhook` is a URL the rules allow and `curl` is found,
/// with `reactor` keeping alerts in `webhook_states` for it. Otherwise webhooks are off, and the
/// log says why once, naming the rule or the host, never the URL (ADR-0051).
fn webhook(config: &pinfold_config::Config, reactor: &mut Reactor) -> Option<webhook::Sender> {
    let alerts = &config.alerts;
    let hook = match webhook::Webhook::new(&alerts.webhook) {
        Ok(hook) => hook?,
        Err(refused) => {
            let _ = writeln!(io::stderr(), "pinfold-core: webhooks are off: {refused}");
            return None;
        }
    };
    let Some(curl) = webhook::find_curl() else {
        let place = if cfg!(target_os = "macos") {
            "no /usr/bin/curl"
        } else {
            "no curl on an absolute PATH entry"
        };
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: webhooks to {} are off: {place}",
            hook.host()
        );
        reactor.set_webhook(hook.host(), false);
        return None;
    };
    reactor.post_states(alerts.webhook_states.iter().map(|alert| match alert {
        pinfold_config::Alert::NeedsYou => Status::NeedsYou,
        pinfold_config::Alert::Error => Status::Error,
    }));
    let format = match alerts.webhook_format {
        pinfold_config::WebhookFormat::Json => webhook::Format::Json,
        pinfold_config::WebhookFormat::Text => webhook::Format::Text,
    };
    // By index, as `agent.launch` keeps a pane's profile.
    let agents = config
        .agents
        .keys()
        .map(|name| name.as_str().into())
        .collect();
    reactor.set_webhook(hook.host(), true);
    let waker = Some(reactor.waker());
    Some(webhook::Sender::new(curl, hook, format, agents, waker))
}

/// Hands the sender each alert `reactor` kept, as a record of fixed fields.
fn post(reactor: &mut Reactor, sender: &mut webhook::Sender) {
    let now = Instant::now();
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    while let Some((pane, status)) = reactor.take_post() {
        if let Some(record) = reactor.post_record(pane, status, now, unix) {
            sender.push(record, now);
        }
    }
}

/// Runs the saved session if `restore` and there is one, else `panes` shells, a tab each, until every pane has gone and no window is left to tell, or
/// until SIGTERM or SIGHUP, saving the session on the way out, when the window closes and a
/// minute after a change (ADR-0034). With a size, it opens a window for `cargo xtask mem-gate` and
/// prints its pid once that window says hello, or fails if it ends or `HELLO_WAIT` passes first.
fn daemon(
    mut panes: usize,
    measured: Option<&str>,
    restore: bool,
    unhardened: Option<bool>,
) -> io::Result<()> {
    let binary = sibling("pinfold-window")?;
    // Read once: the core takes no hot reload. Before the first pane, which `strip_secrets`
    // must reach.
    let config = pinfold_cli::config();
    strip_secrets(config.strip_secrets);
    let mut reactor = Reactor::new()?;
    let mut webhook = webhook(&config, &mut reactor);
    // The built-in tools (ADR-0045), resolved before a restore runs them again.
    reactor.set_tools(sibling("pinfold")?);
    reactor.set_agent_files(config.inbox.agent_files);
    if let Some(checked) = unhardened {
        reactor.set_unhardened(checked);
    }
    reactor.watch_signals()?;
    let state = pinfold_config::state_dir();
    // Emptied before any run, as a run's timeline ends with the core (ADR-0052).
    if let Some(state) = state.as_ref().filter(|_| config.timeline.enabled) {
        match Timelines::new(state.join("timeline"), config.timeline.entries) {
            Ok(timelines) => reactor.keep_timelines(timelines),
            Err(error) => {
                let _ = writeln!(
                    io::stderr(),
                    "pinfold-core: warn timeline=off: {error}; agent runs keep no timeline"
                );
            }
        }
    }
    let mut saver = state
        .clone()
        .map(|state| Saver::new(state, control::SESSION, Some(reactor.waker())));
    let mut parked = VecDeque::new();
    let asked = listen(reactor.waker())?;
    let mut workspaces = Workspaces::default();
    if restore && let Some(state) = &state {
        match restore_session(&mut reactor, &mut workspaces, state) {
            Ok(true) => panes = 0,
            Ok(false) => {}
            Err(error) => {
                // Fail safe: the saved session is still in place, so this core never saves.
                let _ = writeln!(
                    io::stderr(),
                    "pinfold-core: cannot move the saved session aside: {}; this session is not saved",
                    error.kind()
                );
                saver = None;
            }
        }
    }
    for _ in 0..panes {
        reactor
            .open_tab(shell::command(), ROWS, COLUMNS)
            .map_err(io::Error::other)?;
    }
    // The measured window's pid and how long it has, until it says hello.
    let mut unready = None;
    if let Some(size) = measured {
        let mut command = Command::new(&binary);
        command.args(["--measure", size]);
        reactor.spawn_window_command(command)?;
        let pid = reactor.window_pid().unwrap_or_default();
        unready = Some((pid, Instant::now() + HELLO_WAIT));
    }
    // Who asked for the window, until it says hello or ends. A caller that timed out has gone
    // and hears nothing.
    let mut waiting = Vec::new();
    let mut waits: Waits<Request> = Waits::default();
    let mut grants: Grants<Request> = Grants::default();
    let mut family = Family::default();
    let mut subscribers: Vec<(Subscriber, Topics)> = Vec::new();
    let mut known = Seen::new();
    // Before any agent runs, and ready by the first sandboxed launch.
    let state = pinfold_config::state_dir();
    if let Some(state) = state.filter(|_| config.agents.values().any(|agent| agent.sandbox)) {
        sandbox::read_login_path(&state);
    }
    if let Some(patterns) = config.api.protect.clone() {
        reactor.set_patterns(patterns);
    }
    // By index, as `agent.launch` keeps a pane's profile, for the inbox's answers (ADR-0051).
    reactor.set_profiles(
        (config.agents.iter()).map(|(name, agent)| (name.as_str(), agent.approve.as_deref())),
    );
    // Panes get the core's own environment, so one check covers both (ADR-0047).
    let names = std::env::vars_os().map(|(name, _)| name);
    reactor.set_exposed(sandbox::exposed(config.strip_secrets, names));
    let mut log = audit::open(&config.api);
    let mut checkpoints = rewind(&config.rewind, reactor.waker())?;
    let mut window = reactor.has_window();
    // A core with no pane waits for its first window, as `pinfold up` asks for one next.
    let mut started = reactor.pane_count() > 0 || window;
    let empty_until = Instant::now() + empty_wait();
    loop {
        let now = Instant::now();
        let deadline = waits.deadline().into_iter().chain(grants.deadline());
        let deadline = deadline
            .chain(checkpoints.as_ref().and_then(Rewind::deadline))
            .min();
        let wait = deadline.map(|at| at.saturating_duration_since(now));
        let wait = wait
            .into_iter()
            .chain(saver.as_ref().and_then(|saver| saver.wait(now)))
            .chain((!started).then(|| empty_until.saturating_duration_since(now)))
            .chain(unready.map(|(_, by)| by.saturating_duration_since(now)));
        output::tap(&mut reactor, &waits, &subscribers);
        // Before the turn, which may drop a pane whose run then ends.
        record_timelines(&mut reactor, &log, &mut grants, checkpoints.as_mut());
        reactor.turn(Some(wait.min().unwrap_or(IDLE)))?;
        if let Some(sender) = webhook.as_mut() {
            post(&mut reactor, sender);
            reactor.set_webhook_failed(sender.failed());
        }
        output::hand_out(&mut reactor, &mut waits, &subscribers);
        waits.retain(|request| !request.is_gone());
        grants.retain(|request| !request.is_gone());
        subscribers.retain(|(subscriber, _)| !subscriber.is_gone());
        let mut released = Vec::new();
        while let Some((id, answer)) = reactor.take_approval_answer() {
            released.extend(grants.answer(id, answer));
        }
        if let Some((pane, from)) = reactor.take_rewind() {
            window_view(&mut reactor, checkpoints.as_mut(), pane, from);
        }
        if !reactor.has_window() && grants.deadline().is_some() {
            released.extend(grants.window_closed());
        }
        released.extend(grants.expire());
        let (restores, released): (Vec<_>, Vec<_>) = released.into_iter().partition(|released| {
            matches!(released.ticket.method(), Method::CheckpointRestore { .. })
        });
        for released in restores {
            restore_answered(&mut reactor, checkpoints.as_mut(), &mut log, released);
        }
        answer_released(
            &mut reactor,
            &mut grants,
            &mut workspaces,
            &mut log,
            saver.as_mut().map(|saver| (saver, &mut parked)),
            released,
        );
        if let (Some(saver), false) = (saver.as_mut(), parked.is_empty()) {
            answer_saved(&mut log, &reactor, saver, &mut parked);
        }
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
                Method::RunTimeline { .. } => {
                    let rewind = checkpoints.as_ref();
                    let read = timeline::read(&reactor, &mut family, rewind, &request);
                    audit::reply_json(&mut log, &reactor, request, read);
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
                Method::AgentLaunch(_) => launch_agent(
                    &mut reactor,
                    &mut family,
                    &config.agents,
                    checkpoints.as_mut(),
                    &mut log,
                    request,
                ),
                Method::CheckpointTake { pane } | Method::CheckpointList { pane } => {
                    let rewind = checkpoints.as_mut();
                    checkpoint_call(&reactor, &mut family, rewind, &mut log, request, pane);
                }
                Method::CheckpointRestore { .. } => {
                    let rewind = checkpoints.as_mut();
                    restore_call(&reactor, rewind, &grants, &mut log, request);
                }
                Method::ToolOpen(open) if open.run.checkpoint.is_some() => {
                    let rewind = checkpoints.as_mut();
                    view_call(&reactor, &mut family, rewind, &mut log, request);
                }
                _ => {
                    decide(
                        &mut reactor,
                        &mut family,
                        &mut grants,
                        &mut workspaces,
                        &mut log,
                        request,
                    );
                }
            }
        }
        if let Some(rewind) = checkpoints.as_mut() {
            // A pane that is gone counts as closed.
            rewind.observe(Instant::now(), |pane| {
                let done = reactor
                    .status(pane)
                    .map(|(status, ..)| status == Status::Done);
                (reactor.is_closed(pane), done == Some(true))
            });
            checkpoints_ended(&mut reactor, &mut family, rewind, &mut grants, &mut log);
            // Each live run's strip, told once to each window as it changes (ADR-0050).
            if !reactor.is_window_ready() {
                rewind.untell();
            }
            while let Some((run, pane, missing)) = rewind.untold() {
                if !reactor.tell_checkpoint(pane, missing as u8) {
                    break;
                }
                rewind.told(run);
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
        if let Some((pid, by)) = unready {
            if reactor.is_window_ready() {
                writeln!(io::stdout(), "{pid}")?;
                unready = None;
            } else if ended || Instant::now() >= by {
                return Err(io::Error::other(
                    "the measured window ended or stalled before it said hello",
                ));
            }
        }
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
            if ending && let Err(error) = saver.finish() {
                let _ = writeln!(
                    io::stderr(),
                    "pinfold-core: cannot save the session: {}",
                    error.kind()
                );
            }
            if ending {
                answer_saved(&mut log, &reactor, saver, &mut parked);
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
    daemon(panes, None, true, unhardened())
}

/// Says in the log when this core lacks the hardened runtime, which only macOS has, or could
/// not tell. Returns whether it could tell, or nothing for a hardened core (ADR-0044).
fn unhardened() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        let sign = pinfold_sys::sign_command();
        let checked = match pinfold_sys::hardened_runtime() {
            Ok(true) => return None,
            Ok(false) => {
                let _ = writeln!(
                    io::stderr(),
                    "pinfold-core: warn hardened_runtime=no: a same-user process can read this \
                     core's memory; sign this build with: {sign}"
                );
                true
            }
            Err(error) => {
                let _ = writeln!(
                    io::stderr(),
                    "pinfold-core: warn hardened_runtime=unknown error={:?}: cannot read this \
                     core's signing flags, so it may lack the hardened runtime; sign this build \
                     with: {sign}",
                    error.kind()
                );
                false
            }
        };
        Some(checked)
    }
    #[cfg(not(target_os = "macos"))]
    None
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

/// Opens `panes` tabs of the built-in tool `name` on `path`, as `tool.open` starts one, says the
/// first one's pid on stdout, then idles until killed. With `close`, it closes them after a
/// second and says the pid of a tool that is gone. `cargo xtask mem-gate` measures this.
fn tool_panes(
    panes: usize,
    name: &str,
    path: &str,
    rewind: Option<[&str; 3]>,
    close: bool,
) -> io::Result<()> {
    let tool = [Tool::Md, Tool::Files, Tool::Diff]
        .into_iter()
        .find(|tool| tool.name() == name)
        .ok_or_else(|| io::Error::other("no such tool"))?;
    let run = Run {
        tool,
        path: path.to_owned(),
        staged: false,
        range: None,
        checkpoint: rewind.map(|[_, checkpoint, _]| checkpoint.to_owned()),
    };
    let binary = sibling("pinfold")?;
    let mut reactor = Reactor::new()?;
    let mut opened = Vec::new();
    for _ in 0..panes {
        let command = match rewind {
            Some([key, _, shown]) => tool::rewind(&binary, &run, key, shown, 0),
            None => tool::command(&binary, &run),
        };
        let command = command.map_err(io::Error::other)?;
        opened.push(
            reactor
                .open_tab(command, ROWS, COLUMNS)
                .map_err(io::Error::other)?,
        );
    }
    let pid = (opened.first().and_then(|&pane| reactor.pid(pane)))
        .ok_or_else(|| io::Error::other("no tool pane"))?;
    if close {
        let until = Instant::now() + Duration::from_secs(1);
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            reactor.turn(Some(left))?;
        }
        for pane in opened {
            reactor.close_pane(pane);
        }
    }
    writeln!(io::stdout(), "{pid}")?;
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
        ["--idle", panes] => daemon(panes.parse().map_err(io::Error::other)?, None, false, None),
        ["--window", panes, size] => daemon(
            panes.parse().map_err(io::Error::other)?,
            Some(size),
            false,
            None,
        ),
        ["--replay", file] => replay(file),
        ["--tool", panes, name, path] => tool_panes(
            panes.parse().map_err(io::Error::other)?,
            name,
            path,
            None,
            false,
        ),
        // The diff viewer on a run's checkpoint, in the store `key` (ADR-0050 slice 7).
        ["--tool", panes, "rewind", path, key, checkpoint, shown] => tool_panes(
            panes.parse().map_err(io::Error::other)?,
            "diff",
            path,
            Some([key, checkpoint, shown]),
            false,
        ),
        ["--tool-closed", name, path] => tool_panes(1, name, path, None, true),
        _ => Err(io::Error::other(
            "usage: pinfold-core [--no-shell | --idle <panes> | --window <panes> <width>x<height> | --replay <file> | --tool <panes> <md|files|diff> <path> | --tool <panes> rewind <dir> <key> <checkpoint> <shown> | --tool-closed <md|files|diff> <path>]",
        )),
    }
}

fn main() -> ExitCode {
    pinfold_sys::one_arena();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_restore_whose_file_cannot_move_is_an_error_and_the_file_stays() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let state = std::env::temp_dir().join(format!("pinfold-stays-{}", std::process::id()));
        let _ = fs::remove_dir_all(&state);
        let sessions = state.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join(format!("{}.json", control::SESSION));
        fs::write(&path, "{").unwrap();
        let mut reactor = Reactor::new().unwrap();
        let mut workspaces = Workspaces::default();
        // No rename works in a directory that cannot be written, except as root.
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o500)).unwrap();
        let stuck = restore_session(&mut reactor, &mut workspaces, &state);
        fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).unwrap();
        if pinfold_sys::effective_uid() != 0 {
            assert!(stuck.is_err(), "the caller must be told not to save");
            assert_eq!(fs::read_to_string(&path).unwrap(), "{");
            // Once the directory can be written, the file is set aside and a fresh core saves.
            assert!(matches!(
                restore_session(&mut reactor, &mut workspaces, &state),
                Ok(false)
            ));
        }
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&sessions).unwrap().count(), 1);
        assert_eq!(reactor.pane_count(), 0);
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn only_reads_and_sends_of_a_pane_are_cross_pane() {
        let read = Method::PaneRead {
            pane: 3,
            lines: None,
        };
        assert_eq!(cross_pane(&read), Some((3, ApprovalAction::Read, None)));
        let last = Method::PaneLastOutput { pane: 3 };
        assert_eq!(cross_pane(&last), Some((3, ApprovalAction::Read, None)));
        let text = "ls\n".to_owned();
        let send = Method::PaneSend {
            pane: 3,
            text: text.clone(),
        };
        assert_eq!(
            cross_pane(&send),
            Some((3, ApprovalAction::Send, Some(text)))
        );
        let status = Method::PaneStatus {
            pane: 3,
            state: "running".to_owned(),
            message: String::new(),
        };
        for method in [Method::PaneList, Method::PaneClose { pane: 3 }, status] {
            assert_eq!(cross_pane(&method), None, "{method:?}");
        }
    }

    #[test]
    fn owner_changes_ask_and_reads_do_not() {
        assert_eq!(
            owner_prompt(&Method::GrantsRevoke { id: 9 }),
            Some(vec!["grants.revoke".to_owned(), "Grant 9".to_owned()])
        );
        assert!(owner_prompt(&Method::SessionSave).is_some());
        let audit = Method::AuditRead {
            count: 1,
            pane: None,
        };
        for method in [Method::GrantsList, audit, Method::PaneList] {
            assert_eq!(owner_prompt(&method), None, "{method:?}");
        }
    }

    #[test]
    fn events_say_what_opened_closed_or_changed_and_a_subscriber_gets_its_topics() {
        let before: Seen = vec![(1, "running", "marks", None), (2, "idle", "marks", None)];
        let after: Seen = vec![(1, "done", "reported", Some(0)), (3, "idle", "marks", None)];
        let events = changes(&before, &after);
        let status = json!({"event": "pane.status", "pane": 1, "status": "done",
                            "source": "reported", "exit": 0});
        let opened = json!({"event": "pane.opened", "pane": 3, "status": "idle",
                            "source": "marks", "exit": null});
        let closed = json!({"event": "pane.closed", "pane": 2});
        assert_eq!(events, [closed, status, opened]);
        assert!(changes(&after, &after).is_empty());

        let topics = Topics {
            opened: true,
            ..Topics::default()
        };
        let wanted: Vec<_> = events.iter().filter(|event| wants(topics, event)).collect();
        assert_eq!(wanted, [&events[2]]);
    }
}
