//! The `pinfold-core` binary.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::time::{Duration, Instant};
use std::{env, thread};

use pinfold_api::control::{self, Control, Method, Request, Subscriber, Topics, Until, Wait};
use pinfold_api::spawn::Place;
use pinfold_core::family::{self, Family, SplitWay};
use pinfold_core::io::{Reactor, Waker};
use pinfold_core::shell;
use pinfold_core::wait::{Outcome, Waits};
use serde_json::{Value, json};

const ROWS: u16 = 24;
const COLUMNS: u16 = 80;
// A read with every byte escaped by JSON still fits in a peer's output queue.
const _: () = assert!(2 * pinfold_core::io::MAX_READ + 4096 < control::MAX_OUTPUT);
const IDLE: Duration = Duration::from_secs(3600);

/// Returns the window binary beside this one. It is resolved once, so nothing later can name
/// another program.
fn window_binary() -> io::Result<PathBuf> {
    Ok(env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-window"))
}

/// Binds the control socket and answers it on the API thread (ADR-0007, ADR-0032). The
/// reactor's thread makes every spawn (see `pinfold_sys::socket::pair`) and holds the panes, so
/// every request crosses to it. The channel holds one request per peer the API thread serves.
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

/// Answers a request that opens no window. A caller with no terminal is unidentified and may
/// not list panes (threat model, caller identity point 3). Any caller with a terminal may, in a
/// pane or not, as a list shows no screen and little a same-user `ps` does not. Reads, sends and
/// closes reach only the caller's own pane and the panes it spawned, the `self` and `children`
/// scopes, until other scopes ask the person. Pane contents go to the caller alone, never to a
/// log.
fn answer(reactor: &mut Reactor, family: &mut Family, request: &Request) -> Result<Value, String> {
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
                    "pane.close",
                    "events.subscribe",
                ],
                (true, None) => &["window.open", "pane.list", "events.subscribe"],
                (false, _) => &["window.open"],
            };
            Ok(json!({"version": control::VERSION, "methods": methods, "pane": own}))
        }
        Method::PaneRead { pane, lines } => {
            let text = reactor.read(family.reach(reactor, own, *pane)?, *lines);
            Ok(json!({"text": text.unwrap_or_default()}))
        }
        Method::PaneLastOutput { pane } => {
            match reactor.last_output(family.reach(reactor, own, *pane)?) {
                Some(text) => Ok(json!({"text": text})),
                None => Err("the pane has no command output marked with OSC 133".to_owned()),
            }
        }
        Method::PaneSend { pane, text } => {
            let sent = reactor.send(family.reach(reactor, own, *pane)?, text, Instant::now());
            sent.map(|()| json!({})).map_err(str::to_owned)
        }
        Method::PaneSpawn(spawn) => {
            let own = own.ok_or("pane.spawn needs a caller in a pane")?;
            let way = match spawn.place {
                Place::SplitRight => Some(SplitWay::Right),
                Place::SplitDown => Some(SplitWay::Down),
                Place::Tab => None,
            };
            let of = spawn.of.unwrap_or(own);
            let pane = family.spawn(reactor, own, of, way, spawn.cwd.as_deref())?;
            Ok(json!({"pane": pane}))
        }
        Method::PaneClose { pane } => {
            let closed = family.close(reactor, own, *pane);
            closed.map(|()| json!({})).map_err(str::to_owned)
        }
        Method::PaneList if identified => {
            let panes = reactor.list().map(|pane| {
                json!({"id": pane.id, "title": pane.title, "cwd": pane.cwd,
                       "command": pane.command, "status": pane.status})
            });
            Ok(json!({"panes": panes.collect::<Vec<_>>()}))
        }
        Method::PaneList => Err("pane.list needs a caller in a terminal".to_owned()),
        // Never sent here: the loop opens windows, parks waits and keeps subscribers.
        Method::WindowOpen | Method::PaneWait(_) | Method::EventsSubscribe(_) => {
            Err("not here".to_owned())
        }
    }
}

/// Parks a wait on the caller's own pane or one it spawned, or refuses it.
fn park(
    reactor: &Reactor,
    family: &mut Family,
    waits: &mut Waits<Request>,
    request: Request,
    wait: Wait,
) {
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let target = wait.pane.or(own).ok_or(family::NOT_YOURS);
    let pane = match target.and_then(|pane| family.reach(reactor, own, pane)) {
        Ok(pane) => pane,
        Err(reason) => return request.answer(Err(reason.to_owned())),
    };
    let until = match wait.until {
        Until::Exit => pinfold_core::wait::Until::Exit,
        Until::Prompt => pinfold_core::wait::Until::Prompt,
        Until::Output => pinfold_core::wait::Until::Output,
    };
    let deadline = Instant::now() + wait.timeout;
    if let Err(request) = waits.park(reactor, request, pane, until, deadline) {
        request.answer(Err("the core is busy".to_owned()));
    }
}

/// The result of a wait that ended.
fn ended(outcome: Outcome) -> Value {
    match outcome {
        Outcome::Exit(exit) => json!({"outcome": "exit", "exit": exit}),
        Outcome::Prompt => json!({"outcome": "prompt"}),
        Outcome::Output => json!({"outcome": "output"}),
        Outcome::Closed => json!({"outcome": "closed"}),
        Outcome::Timeout => json!({"outcome": "timeout"}),
    }
}

/// Each pane's number, status and exit status, which is all an event tells.
type Seen = Vec<(usize, &'static str, Option<u8>)>;

fn seen(reactor: &Reactor) -> Seen {
    let exit = |pane| {
        reactor
            .progress(pane)
            .and_then(|progress| progress.marks.exit)
    };
    let panes = reactor.list();
    panes
        .map(|pane| (pane.id, pane.status, exit(pane.id)))
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
    for &(id, status, exit) in after {
        let event = match before.iter().find(|was| was.0 == id) {
            None => "pane.opened",
            Some(&was) if was != (id, status, exit) => "pane.status",
            Some(_) => continue,
        };
        events.push(json!({"event": event, "pane": id, "status": status, "exit": exit}));
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

/// Runs `panes` shells, a tab each, until every pane has gone and no window is left to tell, or
/// until killed. With a size, it opens a window for `cargo xtask mem-gate` and prints its pid.
fn daemon(panes: usize, measured: Option<&str>) -> io::Result<()> {
    let binary = window_binary()?;
    let mut reactor = Reactor::new()?;
    let asked = listen(reactor.waker())?;
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
    let mut family = Family::default();
    let mut subscribers: Vec<(Subscriber, Topics)> = Vec::new();
    let mut known = Seen::new();
    loop {
        let wait = waits
            .deadline()
            .map(|at| at.saturating_duration_since(Instant::now()));
        reactor.turn(Some(wait.unwrap_or(IDLE)))?;
        waits.retain(|request| !request.is_gone());
        subscribers.retain(|(subscriber, _)| !subscriber.is_gone());
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
                        request.answer(Err(format!("cannot start pinfold-window: {error}")));
                    }
                },
                Method::PaneWait(wait) => park(&reactor, &mut family, &mut waits, request, wait),
                // Every pane of the session, as for pane.list.
                Method::EventsSubscribe(topics) if request.caller().is_some() => {
                    if subscribers.is_empty() {
                        known = seen(&reactor);
                    }
                    subscribers.push((request.subscriber(), topics));
                    request.answer(Ok(json!({})));
                }
                Method::EventsSubscribe(_) => {
                    let reason = "events.subscribe needs a caller in a terminal";
                    request.answer(Err(reason.to_owned()));
                }
                _ => {
                    let answer = answer(&mut reactor, &mut family, &request);
                    request.answer(answer);
                }
            }
        }
        for (request, outcome) in waits.settle(&reactor, Instant::now()) {
            request.answer(Ok(ended(outcome)));
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
            for request in waiting.drain(..) {
                let reason = "the window ended before it was ready";
                request.answer(if ended {
                    Err(reason.to_owned())
                } else {
                    Ok(json!({}))
                });
            }
        }
        if reactor.pane_count() == 0 && !reactor.has_window() {
            return Ok(());
        }
    }
}

/// Hardens, then runs the first pane, whose shell is the user's.
fn serve() -> io::Result<()> {
    pinfold_sys::harden()?;
    daemon(1, None)
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
        [] => serve(),
        // The daemon without hardening, which also leaves a way to attach a debugger
        // (ADR-0020). `cargo xtask mem-gate` measures these two, as /proc refuses a hardened one.
        ["--idle", panes] => daemon(panes.parse().map_err(io::Error::other)?, None),
        ["--window", panes, size] => daemon(panes.parse().map_err(io::Error::other)?, Some(size)),
        ["--replay", file] => replay(file),
        _ => Err(io::Error::other(
            "usage: pinfold-core [--idle <panes> | --window <panes> <width>x<height> | --replay <file>]",
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
