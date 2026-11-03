//! The `pinfold-core` binary.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TrySendError};
use std::time::{Duration, Instant};
use std::{env, thread};

use pinfold_api::control::{self, Control, Method, Request};
use pinfold_core::io::{Reactor, Waker};
use pinfold_core::shell;
use pinfold_core::snapshot::{self, Saver};
use serde_json::{Value, json};

const ROWS: u16 = 24;
const COLUMNS: u16 = 80;
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
/// pane or not, as a list shows no screen and little a same-user `ps` does not. Reads and sends
/// reach only the caller's own pane, the `self` scope, until other scopes ask the person.
/// Pane contents go to the caller alone, never to a log.
fn answer(reactor: &mut Reactor, request: &Request) -> Result<Value, String> {
    let identified = request.caller().is_some();
    let own = request.caller().and_then(|caller| reactor.pane_of(caller));
    let own_pane = |pane: &usize| {
        if own == Some(*pane) {
            Ok(*pane)
        } else {
            Err("only the caller's own pane can be read or sent to for now".to_owned())
        }
    };
    match request.method() {
        Method::Hello => {
            let methods: &[&str] = match (identified, own) {
                (true, Some(_)) => &[
                    "window.open",
                    "pane.list",
                    "pane.read",
                    "pane.last_output",
                    "pane.send",
                ],
                (true, None) => &["window.open", "pane.list", "session.save"],
                (false, _) => &["window.open"],
            };
            Ok(json!({"version": control::VERSION, "methods": methods, "pane": own}))
        }
        Method::PaneRead { pane, lines } => {
            let text = reactor.read(own_pane(pane)?, *lines);
            Ok(json!({"text": text.unwrap_or_default()}))
        }
        Method::PaneLastOutput { pane } => match reactor.last_output(own_pane(pane)?) {
            Some(text) => Ok(json!({"text": text})),
            None => Err("the pane has no command output marked with OSC 133".to_owned()),
        },
        Method::PaneSend { pane, text } => {
            let sent = reactor.send(own_pane(pane)?, text, Instant::now());
            sent.map(|()| json!({})).map_err(str::to_owned)
        }
        Method::PaneList if identified => {
            let panes = reactor.list().map(|pane| {
                json!({"id": pane.id, "title": pane.title, "cwd": pane.cwd,
                       "command": pane.command, "status": pane.status})
            });
            Ok(json!({"panes": panes.collect::<Vec<_>>()}))
        }
        Method::PaneList => Err("pane.list needs a caller in a terminal".to_owned()),
        // Like audit.read: a guardrail against a pane's program, not proof of a person.
        Method::SessionSave if identified && own.is_none() => {
            let state = pinfold_config::state_dir().ok_or("HOME is not set")?;
            let path = reactor.snapshot().write(&state, control::SESSION);
            path.map(|path| json!({"path": path.to_string_lossy()}))
                .map_err(|error| format!("cannot save the session: {error}"))
        }
        Method::SessionSave => {
            Err("session.save needs a caller in a terminal outside any pane".to_owned())
        }
        // Never sent here: the loop opens windows.
        Method::WindowOpen => Err("no window here".to_owned()),
    }
}

/// Saves the session, logging a failure by its kind alone, which names no path or pane. A
/// failed save stops nothing.
fn save(saver: &mut Saver, reactor: &Reactor) {
    if let Err(error) = saver.save(&reactor.snapshot()) {
        let _ = writeln!(
            io::stderr(),
            "pinfold-core: cannot save the session: {}",
            error.kind()
        );
    }
}

/// Restores the saved session (ADR-0034), and says whether it did. A failure is logged by its
/// kind alone, which names no path or pane, and leaves the core to start fresh.
fn restore_session(reactor: &mut Reactor, state: &Path) -> bool {
    let restored = snapshot::load(state, control::SESSION).and_then(|snapshot| {
        snapshot
            .map(|snapshot| reactor.restore(&snapshot))
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
    if restore && state.is_some_and(|state| restore_session(&mut reactor, &state)) {
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
    let mut window = reactor.has_window();
    loop {
        let wait = saver.as_ref().and_then(|saver| saver.wait(Instant::now()));
        reactor.turn(Some(wait.map_or(IDLE, |wait| wait.min(IDLE))))?;
        while let Ok(request) = asked.try_recv() {
            // Its peer has been told the core is busy, so acting now would surprise a retry.
            if *request.method() != Method::WindowOpen && request.is_late() {
                continue;
            }
            if *request.method() != Method::WindowOpen {
                let answer = answer(&mut reactor, &request);
                request.answer(answer);
                continue;
            }
            match reactor.spawn_window(&binary) {
                Ok(_) => waiting.push(request),
                // The system's words alone, which name no pane, secret or variable.
                Err(error) => request.answer(Err(format!("cannot start pinfold-window: {error}"))),
            }
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
        let ending = reactor.is_terminated() || reactor.pane_count() == 0 && !reactor.has_window();
        if let Some(saver) = saver.as_mut() {
            let now = Instant::now();
            if reactor.was_active() {
                saver.touch(now);
            }
            let closed = window && !reactor.has_window();
            if ending || closed || saver.is_due(now) {
                save(saver, &reactor);
            }
        }
        window = reactor.has_window();
        if ending {
            return Ok(());
        }
    }
}

/// Hardens, then runs the first pane, whose shell is the user's.
fn serve() -> io::Result<()> {
    pinfold_sys::harden()?;
    daemon(1, None, true)
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
        // (ADR-0020). `cargo xtask mem-gate` measures these two, as /proc refuses a hardened one,
        // so they restore nothing, which would change what it measures.
        ["--idle", panes] => daemon(panes.parse().map_err(io::Error::other)?, None, false),
        ["--window", panes, size] => {
            daemon(panes.parse().map_err(io::Error::other)?, Some(size), false)
        }
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
