//! The `pinfold` launcher: starts the core if none answers, asks it for a window, and exits.
//! `pinfold ctl` and `pinfold mcp` are thin clients of the core's API (ADR-0032), and `pinfold
//! up` opens a project's workspace through it.

use std::error::Error;
use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pinfold_api::{control, trust};
use pinfold_config::workspace::{self, Layout, Split};
use pinfold_core::sandbox;
use serde_json::{Value, json};

/// How long a core that was just started gets to answer.
const START_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
/// How often `pinfold agent` asks again while the core reads the user's `PATH`.
const AGENT_RETRY: Duration = Duration::from_millis(200);
/// What follows a bad argument's one-line error.
const HINT: &str = "Run 'pinfold --help' for usage.";

const HELP: &str = "\
PinFold: a memory-first terminal for agentic coding.

Usage: pinfold [command]

With no command, opens a PinFold window, and starts PinFold first if it is not running.

Commands:
  up [dir]          Open a project's .pinfold/layout.toml as tabs and panes
  agent <name>      Open a pane running an agent profile from your config
  ctl list          List the session's panes
  ctl send          Type text into a pane
  ctl wait          Wait until a pane's command exits, it prints or it goes quiet
  ctl grants        List or revoke lasting cross-pane grants
  ctl log           Print or verify the audit log of API calls
  ctl session save  Save the session now and print the snapshot's path
  mcp               Serve PinFold's tools to a coding agent over MCP
  help [command]    Print this, or a command's help

Options:
  -h, --help        Print help. 'pinfold <command> --help' prints a command's help
  -V, --version     Print the version

Docs: https://github.com/0xPiranhaCodes/pinfold.sh#readme
";

const UP: &str = "\
pinfold up: open a project's workspace.

Usage: pinfold up [dir]

Reads <dir>/.pinfold/layout.toml and opens its tabs and panes in a PinFold window, starting
PinFold if it is not running. If they are open already, it focuses them. The window asks
first. The layout's run lines run only once you trust that version of the file, which the
window asks too. Refused, the panes open as plain shells and this exits 1.

Arguments:
  [dir]  The project directory (default: the current directory)

Examples:
  pinfold up
  pinfold up ~/src/myapp
";

const AGENT: &str = "\
pinfold agent: open a pane running an agent profile.

Usage: pinfold agent <name> [--right | --down | --tab]

Run it in a PinFold pane. It opens a pane beside that one running the [agents.<name>] profile
from ~/.config/pinfold/pinfold.toml, in the directory the pane's shell is in. PinFold reads
profiles when it starts.

Arguments:
  <name>   The profile's name

Options:
  --right  Split to the right (the default)
  --down   Split below
  --tab    Open a new tab

Examples:
  pinfold agent claude
  pinfold agent codex --tab
";

const CTL: &str = "\
pinfold ctl: control the running PinFold session from a shell or script.

Usage: pinfold ctl <command> [arguments]

These commands talk to the running PinFold and never start it.

Commands:
  list          List the session's panes
  send          Type text into a pane
  wait          Wait until a pane's command exits, it prints or it goes quiet
  grants        List or revoke lasting cross-pane grants
  log           Print or verify the audit log of API calls
  session save  Save the session now and print the snapshot's path

Run 'pinfold ctl <command> --help' for a command's arguments.

Examples:
  pinfold ctl list
  pinfold ctl wait self --until exit --timeout 600
";

const LIST: &str = "\
pinfold ctl list: list the session's panes.

Usage: pinfold ctl list [--json]

Prints a line per pane: ID, STATUS, TITLE, COMMAND and CWD, with * by the pane it runs in.
Programs set their title, directory and status, so read them as hints.

Options:
  --json  Print the API's pane.list result as JSON

Examples:
  pinfold ctl list
  pinfold ctl list --json
";

const SEND: &str = "\
pinfold ctl send: type text into a pane.

Usage: pinfold ctl send <pane> <text>

Run it in a PinFold pane. It types <text> as it is, so add a line end to run a command. The
pane it runs in, and panes that one opened, take the text at once; any other pane asks you in
the PinFold window first. A pane running ssh or sudo, or reading a password, refuses it.

Arguments:
  <pane>  The pane's ID, from 'pinfold ctl list'
  <text>  At most 4096 bytes

Example:
  pinfold ctl send 2 $'make test\\r'
";

const WAIT: &str = "\
pinfold ctl wait: wait for something to happen in a pane.

Usage: pinfold ctl wait <pane> --until <event> [--pattern <regex>] [--idle-ms <ms>] --timeout <seconds>

Prints how the wait ended: exit and the status, match and the line, prompt, output, idle,
timeout or closed. Exits 0 if what it waited for happened, 1 on a timeout or a closed pane,
and 2 on an error.

Arguments:
  <pane>  A pane's ID, or self for the pane it runs in

Options:
  --until <event>      What to wait for:
                         exit    the shell finishes a command
                         prompt  the shell draws its next prompt
                         output  the program prints anything
                         match   a line printed from now on matches --pattern
                         idle    the program prints nothing for --idle-ms
                       exit and prompt need a shell that marks its prompts (OSC 133)
  --pattern <regex>    The regex for match, at most 1024 bytes
  --idle-ms <ms>       The quiet time for idle, from 100 to 600000
  --timeout <seconds>  Required: give up after this long, at most 600

Examples:
  pinfold ctl wait self --until exit --timeout 600
  pinfold ctl wait 2 --until match --pattern 'test result' --timeout 120
";

const GRANTS: &str = "\
pinfold ctl grants: list or revoke lasting cross-pane grants.

Usage: pinfold ctl grants [--json]
       pinfold ctl grants revoke <id>

Pressing a in an approval prompt lets one pane do one action on another for 30 minutes. Run
this in a terminal outside PinFold's panes, such as Terminal.app. Revoking asks in the PinFold
window first.

Options:
  --json       Print the API's grants.list result as JSON

Commands:
  revoke <id>  Revoke the grant with that ID

Examples:
  pinfold ctl grants
  pinfold ctl grants revoke 7
";

const LOG: &str = "\
pinfold ctl log: print or verify the audit log of API calls.

Usage: pinfold ctl log [--json] [-n <count>] [--pane <pane>]
       pinfold ctl log --verify <file>

Prints the newest entries, a line each, and says if the log's chain is broken. Run it in a
terminal outside PinFold's panes, such as Terminal.app.

Options:
  --json           Print the API's audit.read result as JSON
  -n <count>       How many of the newest entries (default 100, at most 1000)
  --pane <pane>    Only calls by or to that pane
  --verify <file>  Check a written log and its rotated .1 instead, with or without PinFold
                   running. PinFold writes one only with api.audit_log set in its config

Examples:
  pinfold ctl log -n 20
  pinfold ctl log --verify \"$HOME/Library/Application Support/PinFold/audit.log\"
";

const SESSION: &str = "\
pinfold ctl session save: save the session now.

Usage: pinfold ctl session save

Writes the session's snapshot, which PinFold restores when it starts, and prints its path. Run
it in a terminal outside PinFold's panes, such as Terminal.app. It asks in the PinFold window
first.

Example:
  pinfold ctl session save
";

const MCP: &str = "\
pinfold mcp: serve PinFold's tools to a coding agent over MCP.

Usage: pinfold mcp

Speaks MCP on stdin and stdout, so an agent in a PinFold pane can list, read, send to, open,
close and wait on panes, with the same approval prompts as 'pinfold ctl'. Add it to your
agent's MCP servers rather than running it by hand.

Example:
  claude mcp add pinfold -- pinfold mcp
";

/// The help for `topic`, a command and its subcommand, if it names one.
fn help(topic: &[&str]) -> Option<&'static str> {
    Some(match topic {
        [] => HELP,
        ["up"] => UP,
        ["agent"] => AGENT,
        ["ctl"] => CTL,
        ["ctl", "list"] => LIST,
        ["ctl", "send"] => SEND,
        ["ctl", "wait"] => WAIT,
        ["ctl", "grants"] => GRANTS,
        ["ctl", "log"] => LOG,
        ["ctl", "session"] | ["ctl", "session", "save"] => SESSION,
        ["mcp"] => MCP,
        _ => return None,
    })
}

/// A bad argument, which exits 2 with [`HINT`].
#[derive(Debug)]
struct Misuse(String);

impl std::fmt::Display for Misuse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for Misuse {}

fn misuse(text: impl Into<String>) -> Box<dyn Error> {
    Box::new(Misuse(text.into()))
}

/// What is wrong with `args`, which no command takes: an unknown command, or a known one's usage.
fn wrong(args: &[&str]) -> String {
    let (topic, name) = match args {
        [first, ..] if first.starts_with('-') => return format!("unknown option '{first}'"),
        ["ctl", sub, ..] => (&args[..2], format!("ctl {sub}")),
        [first, ..] => (&args[..1], (*first).to_owned()),
        [] => return "no command".to_owned(),
    };
    let usage = help(topic).and_then(|text| text.lines().find_map(|l| l.strip_prefix("Usage: ")));
    match usage {
        Some(usage) => format!("wrong arguments to '{name}': usage: {usage}"),
        None => format!("unknown command '{name}'"),
    }
}

/// Connects to a core at `path`. Nothing listening is no error, as that is what starts one.
fn connect(path: &Path) -> std::io::Result<Option<UnixStream>> {
    match UnixStream::connect(path) {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Which install runs a core, against this one's.
#[derive(Debug, PartialEq)]
enum Install {
    /// This one, or one whose program cannot be found, which is served as before: an ejected
    /// disk image's core is hung, and [`open`] ends it when it does not answer.
    Ours,
    /// Another copy of PinFold, still there, such as one on a disk image still mounted.
    Other(PathBuf),
}

/// Which install `theirs`, the program running a core, is, against `ours`, this one's core.
fn install(theirs: std::io::Result<PathBuf>, ours: &Path) -> Install {
    match theirs.and_then(|theirs| theirs.canonicalize()) {
        Ok(theirs) if theirs != ours => Install::Other(theirs),
        _ => Install::Ours,
    }
}

/// The app bundle that holds `binary`, or `binary` outside one, as in a build from source.
fn bundle(binary: &Path) -> &Path {
    let app = binary
        .ancestors()
        .find(|path| path.extension() == Some("app".as_ref()));
    app.unwrap_or(binary)
}

/// Connects to the core in `directory`, and starts the one beside this binary if none answers.
/// Two launchers can both start one: the second core exits on the lock, and both connect to the
/// first, so its exit status means nothing and only the socket decides. The second also starts
/// the log again, moments after the first core started, when there is next to nothing in it.
/// `arguments` go to a core this starts. A core from another copy of PinFold is restarted from
/// this one if the person agrees. On macOS, this refuses to run from a read-only volume, such as
/// a disk image or App Translocation's mount, as a core there breaks when it goes (#279).
fn core(directory: &Path, arguments: &[&str]) -> Result<UnixStream, Box<dyn Error>> {
    let path = &control::socket_path(directory);
    let binary = std::env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-core");
    if cfg!(target_os = "macos") && matches!(pinfold_sys::read_only(&binary), Ok(true)) {
        let text = format!(
            "PinFold cannot run from {}, a disk image or another read-only place. Drag \
             PinFold to the Applications folder, eject the disk image, and open PinFold from \
             Applications.",
            bundle(&binary).display()
        );
        if by_system() {
            dialog(&text, "{\"OK\"} default button \"OK\"")?;
        }
        return Err(text.into());
    }
    if let Some(stream) = connect(path)? {
        let Install::Other(theirs) = install(pinfold_sys::peer_executable(stream.as_fd()), &binary)
        else {
            return Ok(stream);
        };
        let question = format!(
            "PinFold's session is running from {}, not from this copy at {}. A restart opens \
             this copy and restores the session, but ends what runs in its panes. Not now \
             keeps using the running copy.",
            bundle(&theirs).display(),
            bundle(&binary).display()
        );
        if !agreed(&question)? {
            return Ok(stream);
        }
        restart(directory, &stream)?;
    }
    // Windows inherit this and panes do not, as a pane's stderr is its terminal. A log that will
    // not open is no reason to go without a core.
    let log = control::start_log(directory).map_or_else(|_| Stdio::null(), Stdio::from);
    let mut command = Command::new(&binary);
    command.args(arguments);
    pinfold_sys::spawn_detached(command, log)
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
    let start = Instant::now();
    while start.elapsed() < START_WAIT {
        if let Some(stream) = connect(path)? {
            return Ok(stream);
        }
        thread::sleep(POLL);
    }
    Err(format!("no core answered at {}", path.display()).into())
}

/// Asks the core for a window, and starts one with `arguments` if none answers. A core older
/// than the window binary beside it answers with an `update` instead, once PinFold is installed
/// again under it. This then ends that core, which saves the session, and a new core restores
/// it and opens the window. If that would end a running program, it asks the person first.
fn open(arguments: &[&str]) -> Result<(), Box<dyn Error>> {
    let directory = control::runtime_dir();
    let mut stream = core(&directory, arguments)?;
    let opened = match control::open_window(&stream) {
        Err(error) if hung(&error) => {
            eprintln!(
                "pinfold: the running core did not answer, as when its disk image is ejected: \
                 ending it and restoring its session as last saved"
            );
            restart(&directory, &stream)?;
            stream = core(&directory, arguments)?;
            control::open_window(&stream)?
        }
        result => result?,
    };
    let Some(update) = opened.get("update") else {
        return Ok(());
    };
    let running = update["running"].as_array().map_or(&[][..], Vec::as_slice);
    if !running.is_empty() {
        let question = stranded(running);
        if !agreed(&question)? {
            return Err(question.into());
        }
    }
    eprintln!(
        "pinfold: PinFold was updated: restarting its session, link version {} to {}",
        update["core"], update["window"]
    );
    restart(&directory, &stream)?;
    let opened = control::open_window(&core(&directory, arguments)?)?;
    if opened.get("update").is_some() {
        return Err("the new core cannot open its window either: reinstall PinFold".into());
    }
    Ok(())
}

/// What a restart would end and how to keep it, with each field through [`printable`].
fn stranded(running: &[Value]) -> String {
    let mut text = "PinFold was updated while its older session ran. A restart opens the new \
        version and restores the session, but ends what runs in these panes:\n"
        .to_owned();
    for pane in running {
        let field = |name: &str| printable(pane[name].as_str().unwrap_or("?"));
        text += &match pane["title"].as_str() {
            Some(_) => format!(
                "  pane {}: {} in {}\n",
                pane["pane"],
                field("program"),
                field("title")
            ),
            None => format!("  pane {}: {}\n", pane["pane"], field("program")),
        };
    }
    text + "Not now keeps them running, and `pinfold ctl list` in a terminal shows them. \
        Opening PinFold again offers the restart."
}

/// Whether `error` is a core that did not answer in time, rather than one that said no.
fn hung(error: &control::Error) -> bool {
    matches!(error, control::Error::Io(error)
        if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
}

/// Whether the person agrees to restart after reading `question`: in the launcher's terminal,
/// or else in a dialog when the system started it, as the Dock does. With neither, nobody can
/// answer, so no.
fn agreed(question: &str) -> Result<bool, Box<dyn Error>> {
    let input = std::io::stdin();
    if input.is_terminal() {
        eprint!("{question}\nRestart now? [y/N] ");
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        return Ok(matches!(answer.trim(), "y" | "Y" | "yes"));
    }
    if !by_system() {
        return Ok(false);
    }
    let buttons = "{\"Not now\", \"Restart now\"} default button \"Not now\"";
    Ok(dialog(question, buttons)?.starts_with(b"Restart now"))
}

/// Whether the system started this, as the Dock and Finder do on macOS, so only a dialog can
/// reach the person.
fn by_system() -> bool {
    cfg!(target_os = "macos") && std::os::unix::process::parent_id() == 1
}

/// Shows `text` in a dialog with `buttons`, AppleScript for them and the default, and returns
/// the button pressed, or nothing if the dialog was closed.
fn dialog(text: &str, buttons: &str) -> std::io::Result<Vec<u8>> {
    // The text is an argument, never part of the script, so a pane's field cannot run in it.
    let show = format!(
        "display dialog (item 1 of argv) with title \"PinFold\" with icon caution buttons \
         {buttons}"
    );
    let script = ["on run argv", &show, "button returned of result", "end run"];
    let mut command = Command::new("/usr/bin/osascript");
    for line in script {
        command.args(["-e", line]);
    }
    let output = command.arg(text).stdin(Stdio::null()).output()?;
    Ok(if output.status.success() {
        output.stdout
    } else {
        Vec::new()
    })
}

/// Ends the core on `stream`, the one that owns the socket, as only it answered there, and waits
/// until the socket refuses: the core saves the session on SIGTERM, then exits. One too hung for
/// that, as on an ejected disk image, gets SIGKILL and leaves the session it saved last.
fn restart(directory: &Path, stream: &UnixStream) -> Result<(), Box<dyn Error>> {
    let path = control::socket_path(directory);
    let socket = stream.as_fd();
    // Only a process proved PinFold's own is signalled: this user's, at the socket, and
    // pinfold-core by name, which is all an ejected core still has, as its path is gone.
    let user = pinfold_sys::peer_uid(socket)?;
    let name = pinfold_sys::peer_name(socket)?;
    if !own_core(user, pinfold_sys::effective_uid(), &name) {
        return Err(format!("the process at {} is not a PinFold core", path.display()).into());
    }
    pinfold_sys::terminate_peer(socket)?;
    if ended(&path, stream)? {
        return Ok(());
    }
    pinfold_sys::kill_peer(socket)?;
    if ended(&path, stream)? {
        return Ok(());
    }
    Err("the older core did not exit".into())
}

/// Whether a socket's peer, run by `user` and named `name`, is a core this launcher may end.
fn own_core(user: u32, ours: u32, name: &str) -> bool {
    user == ours && name == "pinfold-core"
}

/// Whether the core on `stream` has gone within [`START_WAIT`]: that connection closed and the
/// socket at `path` refuses. Refusal alone is not enough, as a hung core's full backlog refuses.
fn ended(path: &Path, stream: &UnixStream) -> std::io::Result<bool> {
    stream.set_read_timeout(Some(POLL))?;
    let start = Instant::now();
    let mut closed = false;
    while start.elapsed() < START_WAIT {
        if !closed {
            closed = match (&mut &*stream).read(&mut [0; 256]) {
                Ok(0) => true,
                Ok(_) => false,
                Err(error) => !matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            };
        } else if connect(path)?.is_none() {
            return Ok(true);
        } else {
            thread::sleep(POLL);
        }
    }
    Ok(false)
}

fn launch() -> Result<(), Box<dyn Error>> {
    open(&[])
}

/// `pinfold up [dir]`: opens `dir`'s `.pinfold/layout.toml` in a window, or focuses its tabs if
/// they are open. If the file has `run` lines and the directory is not trusted with its hash,
/// the window asks first. The core, not this, reads the lines from the file again and types
/// them, and only once trusted. Refused, the layout opens as shells, and this says so and returns
/// false, so `pinfold up` exits 1 and a script can tell the lines did not run.
fn up(dir: Option<&str>) -> Result<bool, Box<dyn Error>> {
    let root = std::fs::canonicalize(dir.unwrap_or("."))?;
    let bytes = workspace::read(&root.join(".pinfold/layout.toml"), &root)?;
    let layout = workspace::parse(&bytes, &root)?;
    let (mut params, runs) = request(&root, &layout)?;
    let sha256 = trust::sha256(&bytes);
    // A core this starts opens only the workspace, beside a restored session if there is one.
    open(&["--no-shell"])?;
    let asked = json!({"root": params["root"], "sha256": sha256});
    let mut trusted = true;
    if !runs.is_empty()
        && control::ask(&running()?, "workspace.trust_state", &asked)?.1["trusted"] != true
    {
        let asked = json!({"root": params["root"], "sha256": sha256, "runs": runs});
        if let Err(error) = control::ask(&running()?, "workspace.ask_trust", &asked) {
            let count = runs.len();
            let lines = if count == 1 { "line" } else { "lines" };
            eprintln!("pinfold: {count} run {lines} did not run: {error}");
            trusted = false;
        }
    }
    params["sha256"] = sha256.into();
    control::ask(&running()?, "workspace.open", &params)?;
    Ok(trusted)
}

/// `workspace.open`'s parameters for `layout`, with each pane's cwd, or `root` for none, and
/// without its `run` lines, and those lines, for the trust prompt.
fn request<'a>(root: &Path, layout: &'a Layout) -> Result<(Value, Vec<&'a str>), Box<dyn Error>> {
    let text = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or("a path is not UTF-8")
    };
    let mut tabs = Vec::new();
    for tab in &layout.tabs {
        let mut panes = Vec::new();
        for (index, pane) in tab.panes.iter().enumerate() {
            let cwd = text(pane.cwd.as_deref().unwrap_or(root))?;
            panes.push(match (index, pane.split.unwrap_or(Split::Right)) {
                (0, _) => json!({"cwd": cwd}),
                (_, Split::Right) => json!({"split": "right", "cwd": cwd}),
                (_, Split::Down) => json!({"split": "down", "cwd": cwd}),
            });
        }
        tabs.push(json!({"panes": panes}));
    }
    let panes = layout.tabs.iter().flat_map(|tab| &tab.panes);
    let runs = panes.filter_map(|pane| pane.run.as_deref()).collect();
    let params = json!({"root": text(root)?, "layout": {"tabs": tabs}});
    Ok((params, runs))
}

/// `pinfold ctl list`: prints the session's panes, as JSON with `json`. It starts no core.
fn list(json: bool) -> Result<(), Box<dyn Error>> {
    let (hello, list) = control::list_panes(&running()?)?;
    let text = if json {
        format!("{list}\n")
    } else {
        table(&hello, &list)
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

/// `pinfold ctl send <pane> <text>`: types `text` into a pane, which for now must be the
/// caller's own. It starts no core.
fn send(pane: &str, text: &str) -> Result<(), Box<dyn Error>> {
    let pane: usize = pane
        .parse()
        .map_err(|_| misuse("the pane must be a number"))?;
    let params = json!({"pane": pane, "text": text});
    control::ask(&running()?, "pane.send", &params)?;
    Ok(())
}

/// `pinfold agent <name> [--right | --down | --tab]`: opens a pane beside the caller's running
/// the config's `[agents.<name>]` profile. It starts no core, as it needs a caller in a pane.
fn agent(name: &str, place: &str) -> Result<(), Box<dyn Error>> {
    let params = json!({"profile": name, "place": place});
    // Past the core's own wait for the shell, which then asks it again.
    let give_up = Instant::now() + sandbox::LOGIN_WAIT + Duration::from_secs(2);
    loop {
        match control::ask(&running()?, "agent.launch", &params) {
            Err(error) if reading_path(&error) && Instant::now() < give_up => {
                thread::sleep(AGENT_RETRY);
            }
            result => return result.map(drop).map_err(Into::into),
        }
    }
}

/// Whether the core refused a launch only because it has not yet read the user's `PATH`, which
/// it does once, in the first seconds after it starts.
fn reading_path(error: &control::Error) -> bool {
    matches!(error, control::Error::Refused(message) if message.contains(sandbox::READING_PATH))
}

/// `pinfold ctl wait <pane> --until <what> [--pattern <regex>] [--idle-ms <n>] --timeout <s>`:
/// waits on a pane, `self` or a number, and prints how the wait ended. Returns whether what it
/// waited for happened, rather than a timeout or the pane closing. It starts no core.
fn wait(args: &[&str]) -> Result<bool, Box<dyn Error>> {
    let [pane, flags @ ..] = args else {
        return Err(misuse("'ctl wait' needs a pane"));
    };
    let pane = match *pane {
        "self" => json!("self"),
        pane => json!(
            pane.parse::<usize>()
                .map_err(|_| misuse("the pane must be a number or self"))?
        ),
    };
    let mut params = json!({"pane": pane});
    for pair in flags.chunks(2) {
        let (key, value) = match *pair {
            ["--until", until] => ("until", json!(until)),
            ["--pattern", pattern] => ("pattern", json!(pattern)),
            ["--idle-ms", idle] => ("idle_ms", json!(number(idle, "--idle-ms")?)),
            ["--timeout", seconds] => (
                "timeout_ms",
                json!(number(seconds, "--timeout")?.saturating_mul(1000)),
            ),
            [flag] => return Err(misuse(format!("'{flag}' needs a value"))),
            [flag, ..] => return Err(misuse(format!("unknown option '{flag}' for 'ctl wait'"))),
            [] => continue,
        };
        params[key] = value;
    }
    if params["timeout_ms"].is_null() {
        return Err(misuse("--timeout is required"));
    }
    let (_, result) = control::ask(&running()?, "pane.wait", &params)?;
    let outcome = result["outcome"].as_str().unwrap_or_default();
    let text = match (outcome, &result["exit"], result["line"].as_str()) {
        ("exit", Value::Number(exit), _) => format!("exit {exit}"),
        ("match", _, Some(line)) => format!("match {line}"),
        _ => outcome.to_owned(),
    };
    std::io::stdout().write_all(format!("{}\n", printable(&text)).as_bytes())?;
    Ok(!matches!(outcome, "timeout" | "closed"))
}

/// `pinfold ctl session save`: writes the session's snapshot and prints its path. It starts
/// no core.
fn save() -> Result<(), Box<dyn Error>> {
    let (_, saved) = control::ask(&running()?, "session.save", &json!({}))?;
    let path = saved["path"].as_str().ok_or("the core named no file")?;
    std::io::stdout().write_all(format!("{}\n", printable(path)).as_bytes())?;
    Ok(())
}

/// Lists lasting cross-pane grants. The core accepts this only from a terminal outside panes.
fn grants(json_output: bool) -> Result<(), Box<dyn Error>> {
    let (_, listed) = control::ask(&running()?, "grants.list", &json!({}))?;
    let text = if json_output {
        format!("{listed}\n")
    } else {
        grants_table(&listed)
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

fn revoke(id: &str) -> Result<(), Box<dyn Error>> {
    let id: u64 = id
        .parse()
        .map_err(|_| misuse("the grant id must be a number"))?;
    let (_, result) = control::ask(&running()?, "grants.revoke", &json!({"id": id}))?;
    if result["revoked"] == Value::Bool(false) {
        return Err(format!("no active grant {id}").into());
    }
    Ok(())
}

/// `pinfold ctl log [--json] [-n <count>] [--pane <pane>]`: prints the newest audit entries,
/// which only a terminal outside any pane may read. `pinfold ctl log --verify <file>` checks a
/// written audit log instead. Neither starts a core.
fn log(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let mut out = std::io::stdout();
    if let ["--verify", file] = args {
        let count = pinfold_api::verify_file(file)?;
        writeln!(out, "{}: {count} entries, chain intact", printable(file))?;
        return Ok(());
    }
    let (mut params, mut json) = (json!({}), false);
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        let name = match arg {
            "--json" => {
                json = true;
                continue;
            }
            "-n" => "count",
            "--pane" => "pane",
            "--verify" => return Err(misuse("--verify takes a file and no other option")),
            _ => return Err(misuse(format!("unknown option '{arg}' for 'ctl log'"))),
        };
        let value = args.next().and_then(|value| value.parse::<u64>().ok());
        params[name] = value
            .ok_or_else(|| misuse(format!("{arg} takes a number")))?
            .into();
    }
    let (_, read) = control::ask(&running()?, "audit.read", &params)?;
    let text = if json {
        format!("{read}\n")
    } else {
        entries(&read)
    };
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// The audit entries as plain text, a line each, every field through [`printable`].
fn entries(read: &Value) -> String {
    let mut text = String::new();
    for entry in read["entries"].as_array().map_or(&[][..], Vec::as_slice) {
        let field = |name: &str| printable(entry[name].as_str().unwrap_or("-"));
        let caller = match (entry["caller"].as_u64(), entry["terminal"].as_bool()) {
            (Some(pane), _) => format!("pane {pane}"),
            (None, Some(true)) => "outside a pane".to_owned(),
            (None, _) => "unidentified".to_owned(),
        };
        let target = entry["target"].as_u64();
        let target = target.map_or("-".to_owned(), |pane| format!("pane {pane}"));
        let (method, outcome, reason) = (field("method"), field("outcome"), field("reason"));
        let at = utc(entry["at_ms"].as_u64().unwrap_or_default());
        let seq = entry["seq"].as_u64().unwrap_or_default();
        text += &format!("{seq} {at} {caller} {method} {target} {outcome}: {reason}\n");
    }
    if read["verified"] != true {
        text += &format!("chain broken at sequence {}\n", read["broken_at"]);
    }
    text
}

/// Unix milliseconds as `YYYY-MM-DD hh:mm:ss UTC`, by Howard Hinnant's `civil_from_days`.
fn utc(ms: u64) -> String {
    let seconds = ms / 1000;
    let z = seconds / 86_400 + 719_468;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let year = year_of_era + z / 146_097 * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (seconds / 3600 % 24, seconds / 60 % 60, seconds % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// `text` as a whole number, or a misuse naming `flag`.
fn number(text: &str, flag: &str) -> Result<u64, Box<dyn Error>> {
    text.parse()
        .map_err(|_| misuse(format!("{flag} takes a whole number")))
}

/// Connects to the running core, and starts none.
fn running() -> Result<UnixStream, Box<dyn Error>> {
    let path = control::socket_path(&control::runtime_dir());
    Ok(connect(&path)?.ok_or("PinFold is not running")?)
}

/// The panes as plain text, a line each, with `*` by the caller's own. A program chooses its own
/// command and directory, so each field goes through [`printable`] before it reaches a terminal.
fn table(hello: &Value, list: &Value) -> String {
    let mut text = format!(
        "{:<5} {:<8} {:<16} {:<24} CWD\n",
        "ID", "STATUS", "TITLE", "COMMAND"
    );
    let panes = list["panes"].as_array().map_or(&[][..], Vec::as_slice);
    for pane in panes {
        let field = |name: &str| printable(pane[name].as_str().unwrap_or("-"));
        let own = if pane["id"] == hello["pane"] { "*" } else { "" };
        let id = format!("{}{own}", pane["id"]);
        let (status, title) = (field("status"), field("title"));
        let (command, cwd) = (field("command"), field("cwd"));
        text += &format!("{id:<5} {status:<8} {title:<16} {command:<24} {cwd}\n");
    }
    text
}

fn grants_table(list: &Value) -> String {
    let mut text = format!(
        "{:<8} {:<8} {:<8} {:<8} EXPIRES\n",
        "ID", "CALLER", "TARGET", "ACTION"
    );
    let grants = list["grants"].as_array().map_or(&[][..], Vec::as_slice);
    for grant in grants {
        let seconds = grant["expires_ms"].as_u64().unwrap_or_default() / 1000;
        let id = grant["id"].to_string();
        let caller = grant["caller"].to_string();
        let target = grant["target"].to_string();
        let action = grant["action"].as_str().unwrap_or("-");
        text += &format!(
            "{:<8} {:<8} {:<8} {:<8} {seconds}s\n",
            id, caller, target, action
        );
    }
    text
}

/// `text` with each control, bidi or zero-width character as `?`, so a pane's fields cannot
/// move the cursor, recolour or reorder what the person reads (threat T6).
fn printable(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2069}'
            | '\u{061c}'
            | '\u{feff}' => '?',
            _ if character.is_control() => '?',
            _ => character,
        })
        .collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    // Only where no command takes an argument, so `ctl send 3 --help` still types `--help`.
    let topic = match words[..] {
        ["-h" | "--help"] => Some(&words[..0]),
        [_, "-h" | "--help"] | ["ctl", _, "-h" | "--help"] => Some(&words[..words.len() - 1]),
        ["help", ref topic @ ..] => Some(topic),
        _ => None,
    };
    let text = match (topic, &words[..]) {
        (Some(topic), _) => help(topic).map(str::to_owned),
        (None, ["-V" | "--version"]) => Some(format!("pinfold {}\n", env!("CARGO_PKG_VERSION"))),
        _ => None,
    };
    if let Some(text) = text {
        return match std::io::stdout().write_all(text.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    let result = match words[..] {
        _ if topic.is_some() => Err(misuse(wrong(topic.unwrap_or_default()))),
        [] => launch(),
        ["up"] | ["up", _] => match up(args.get(1).map(String::as_str)) {
            Ok(true) => Ok(()),
            Ok(false) => return ExitCode::FAILURE,
            Err(error) => Err(error),
        },
        ["agent", name] | ["agent", name, "--right"] => agent(name, "right"),
        ["agent", name, "--down"] => agent(name, "down"),
        ["agent", name, "--tab"] => agent(name, "tab"),
        ["ctl", "list"] => list(false),
        ["ctl", "list", "--json"] => list(true),
        ["ctl", "send", pane, text] => send(pane, text),
        // 0 once what it waited for happened, 1 if not, and 2 on an error.
        ["ctl", "wait", ref wait_args @ ..] => {
            return match wait(wait_args) {
                Ok(happened) => ExitCode::from(u8::from(!happened)),
                Err(error) => {
                    eprintln!("pinfold: {error}");
                    if error.is::<Misuse>() {
                        eprintln!("{HINT}");
                    }
                    ExitCode::from(2)
                }
            };
        }
        ["ctl", "session", "save"] => save(),
        ["ctl", "log", ref rest @ ..] => log(rest),
        ["mcp"] => {
            let socket = control::socket_path(&control::runtime_dir());
            pinfold_cli::mcp::serve(std::io::stdin().lock(), std::io::stdout().lock(), &socket)
                .map_err(Into::into)
        }
        ["ctl", "grants"] => grants(false),
        ["ctl", "grants", "--json"] => grants(true),
        ["ctl", "grants", "revoke", id] => revoke(id),
        _ => Err(misuse(wrong(&words))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is::<Misuse>() => {
            eprintln!("pinfold: {error}\n{HINT}");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("pinfold: {error}");
            // A core started from a terminal logs there, so this may be an older core's.
            let directory = control::runtime_dir();
            let logs = [
                control::log_path(&directory),
                control::previous_log_path(&directory),
            ];
            let logs: Vec<String> = logs
                .iter()
                .filter(|log| log.exists())
                .map(|log| log.display().to_string())
                .collect();
            if (args.is_empty() || args[0] == "up") && !logs.is_empty() {
                eprintln!("pinfold: see {}", logs.join(" and "));
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_another_copy_that_is_still_there_is_another_install() {
        let dir = std::env::temp_dir().join(format!("pinfold-install-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Other.app/Contents/MacOS")).unwrap();
        let ours = std::env::current_exe().unwrap().canonicalize().unwrap();
        let theirs = dir.join("Other.app/Contents/MacOS/pinfold-core");
        std::fs::write(&theirs, b"").unwrap();
        let theirs = theirs.canonicalize().unwrap();

        assert_eq!(install(Ok(ours.clone()), &ours), Install::Ours);
        assert_eq!(
            install(Ok(theirs.clone()), &ours),
            Install::Other(theirs.clone())
        );
        // An ejected image's core: macOS gives no path, Linux one that no longer exists.
        let gone = std::io::Error::other("proc_pidpath found no path");
        assert_eq!(install(Err(gone), &ours), Install::Ours);
        assert_eq!(
            install(Ok(dir.join("pinfold-core (deleted)")), &ours),
            Install::Ours
        );

        assert_eq!(
            bundle(&theirs),
            dir.join("Other.app").canonicalize().unwrap()
        );
        assert_eq!(
            bundle(Path::new("/target/debug/pinfold-core")),
            Path::new("/target/debug/pinfold-core")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_this_users_pinfold_core_is_ended() {
        assert!(own_core(501, 501, "pinfold-core"));
        assert!(!own_core(0, 501, "pinfold-core"));
        assert!(!own_core(501, 501, "pinfold-window"));
        assert!(!own_core(501, 501, "zsh"));
    }

    #[test]
    fn only_a_core_that_does_not_answer_is_hung() {
        let late = |kind: ErrorKind| control::Error::Io(kind.into());
        assert!(hung(&late(ErrorKind::WouldBlock)));
        assert!(hung(&late(ErrorKind::TimedOut)));
        assert!(!hung(&late(ErrorKind::ConnectionReset)));
        assert!(!hung(&control::Error::Refused("no".to_owned())));
    }

    #[test]
    fn agent_asks_again_only_while_the_core_reads_the_path() {
        let reading = format!("the sandbox will not start: {}", sandbox::READING_PATH);
        assert!(reading_path(&control::Error::Refused(reading)));
        let other = "the sandbox will not start: \"/opt\" is not inside your home folder";
        assert!(!reading_path(&control::Error::Refused(other.to_owned())));
        assert!(!reading_path(&control::Error::Running));
    }

    #[test]
    fn a_request_holds_tabs_splits_and_cwds_and_lists_run_lines_without_sending_them() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let toml = b"[[tabs]]\n[[tabs.panes]]\nrun = 'make'\n\
            [[tabs]]\n[[tabs.panes]]\ncwd = '.'\n[[tabs.panes]]\nsplit = 'down'\nrun = 'ls'\n\
            [[tabs.panes]]\n";
        let layout = workspace::parse(toml, &root).unwrap();
        let (params, runs) = request(&root, &layout).unwrap();
        let cwd = root.to_str().unwrap();
        let expected = json!({"root": cwd, "layout": {"tabs": [
            {"panes": [{"cwd": cwd}]},
            {"panes": [{"cwd": cwd}, {"split": "down", "cwd": cwd}, {"split": "right", "cwd": cwd}]},
        ]}});
        assert_eq!((params, runs), (expected, vec!["make", "ls"]));
    }

    #[test]
    fn a_table_marks_the_callers_pane_and_dashes_what_is_unknown() {
        let list = json!({"panes": [
            {"id": 0, "title": "work", "cwd": "/tmp/work", "command": "/bin/zsh", "status": "running"},
            {"id": 3, "title": null, "cwd": null, "command": "/bin/sh", "status": "idle"},
        ]});
        let expected = "\
ID    STATUS   TITLE            COMMAND                  CWD
0     running  work             /bin/zsh                 /tmp/work
3*    idle     -                /bin/sh                  -
";
        assert_eq!(table(&json!({"pane": 3}), &list), expected);
        // Outside a pane none is the caller's.
        assert!(!table(&json!({"pane": null}), &list).contains('*'));
    }

    #[test]
    fn a_log_prints_a_line_per_entry_with_no_control_characters() {
        let read = json!({"verified": false, "broken_at": 8, "entries": [
            {"seq": 7, "at_ms": 1_700_000_000_123_u64, "caller": 1, "terminal": true,
             "method": "pane.send", "target": 0, "outcome": "refused", "reason": "no\u{1b}[2J, 5 bytes"},
            {"seq": 8, "at_ms": 0, "caller": null, "terminal": false, "method": "pane.list",
             "target": null, "outcome": "ok", "reason": "allowed"},
        ]});
        let expected = "\
7 2023-11-14 22:13:20 UTC pane 1 pane.send pane 0 refused: no?[2J, 5 bytes
8 1970-01-01 00:00:00 UTC unidentified pane.list - ok: allowed
chain broken at sequence 8
";
        assert_eq!(entries(&read), expected);
        assert_eq!(utc(951_825_600_000), "2000-02-29 12:00:00 UTC");
    }

    #[test]
    fn a_table_prints_no_control_bidi_or_zero_width_characters() {
        let list = json!({"panes": [{
            "id": 1, "title": "a\u{202e}b", "cwd": "/x\u{1b}]0;y\u{7}", "command": "s\u{200b}h\r", "status": "idle"
        }]});
        let text = table(&json!({"pane": null}), &list);
        assert!(text.contains("a?b") && text.contains("/x?]0;y?") && text.contains("s?h?"));
        assert!(text.lines().count() == 2);
    }

    #[test]
    fn a_restart_names_each_running_pane_with_no_control_characters() {
        let running = [
            json!({"pane": 2, "title": "work\u{1b}[2J", "program": "vim"}),
            json!({"pane": 5, "title": null, "program": "car\u{202e}go"}),
        ];
        let text = stranded(&running);
        assert!(text.contains("  pane 2: vim in work?[2J\n"), "{text}");
        assert!(text.contains("  pane 5: car?go\n"), "{text}");
        assert!(text.contains("Not now keeps them running"), "{text}");
    }

    #[test]
    fn a_grants_table_names_the_triple_and_time_left() {
        let listed = json!({"grants": [{"id": 7, "caller": 1, "target": 2,
            "action": "send", "expires_ms": 90_000}]});
        let text = grants_table(&listed);
        assert!(text.contains("ID       CALLER   TARGET   ACTION"), "{text}");
        assert!(
            text.contains("7        1        2        send     90s"),
            "{text}"
        );
    }
}
