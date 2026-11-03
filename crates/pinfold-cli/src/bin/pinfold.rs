//! The `pinfold` launcher: starts the core if none answers, asks it for a window, and exits.
//! `pinfold ctl` and `pinfold mcp` are thin clients of the core's API (ADR-0032), and `pinfold
//! up` opens a project's workspace through it.

use std::error::Error;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pinfold_api::control;
use pinfold_config::workspace::{self, Layout, Split};
use serde_json::{Value, json};

/// How long a core that was just started gets to answer.
const START_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

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

/// Connects to the core in `directory`, and starts the one beside this binary if none answers.
/// Two launchers can both start one: the second core exits on the lock, and both connect to the
/// first, so its exit status means nothing and only the socket decides. The second also empties
/// the log, moments after the first core started, when there is next to nothing in it.
fn core(directory: &Path) -> Result<UnixStream, Box<dyn Error>> {
    let path = &control::socket_path(directory);
    if let Some(stream) = connect(path)? {
        return Ok(stream);
    }
    let binary = std::env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-core");
    // Windows inherit this and panes do not, as a pane's stderr is its terminal. A log that will
    // not open is no reason to go without a core.
    let log = control::start_log(directory).map_or_else(|_| Stdio::null(), Stdio::from);
    pinfold_sys::spawn_detached(Command::new(&binary), log)
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

fn launch() -> Result<(), Box<dyn Error>> {
    control::open_window(&core(&control::runtime_dir())?)?;
    Ok(())
}

/// `pinfold up [dir]`: opens `dir`'s `.pinfold/layout.toml` as shells in a window, or focuses
/// its tabs if they are open. The file is untrusted, so its `run` lines wait for workspace trust.
fn up(dir: Option<&str>) -> Result<(), Box<dyn Error>> {
    let root = std::fs::canonicalize(dir.unwrap_or("."))?;
    let layout = workspace::load(&root.join(".pinfold/layout.toml"), &root)?;
    let (params, waiting) = request(&root, &layout)?;
    control::open_window(&core(&control::runtime_dir())?)?;
    control::ask(&running()?, "workspace.open", &params)?;
    let commands = if waiting == 1 {
        "command waits"
    } else {
        "commands wait"
    };
    if waiting > 0 {
        eprintln!("pinfold: {waiting} {commands} for workspace trust (not yet available)");
    }
    Ok(())
}

/// `workspace.open`'s parameters for `layout`, with each pane's cwd, or `root` for none, and
/// without its `run` lines, and how many `run` lines there are.
fn request(root: &Path, layout: &Layout) -> Result<(Value, usize), Box<dyn Error>> {
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
    let waiting = panes.filter(|pane| pane.run.is_some()).count();
    let params = json!({"root": text(root)?, "layout": {"tabs": tabs}});
    Ok((params, waiting))
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
    let pane: usize = pane.parse().map_err(|_| "the pane must be a number")?;
    let params = json!({"pane": pane, "text": text});
    control::ask(&running()?, "pane.send", &params)?;
    Ok(())
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
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => launch(),
        ["up"] => up(None),
        ["up", dir] => up(Some(dir)),
        ["ctl", "list"] => list(false),
        ["ctl", "list", "--json"] => list(true),
        ["ctl", "send", pane, text] => send(pane, text),
        ["mcp"] => {
            let socket = control::socket_path(&control::runtime_dir());
            pinfold_cli::mcp::serve(std::io::stdin().lock(), std::io::stdout().lock(), &socket)
                .map_err(Into::into)
        }
        _ => {
            eprintln!(
                "usage: pinfold [up [dir] | ctl list [--json] | ctl send <pane> <text> | mcp]"
            );
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold: {error}");
            // A core started from a terminal logs there, so this may be an older core's.
            let log = control::log_path(&control::runtime_dir());
            if (args.is_empty() || args[0] == "up") && log.exists() {
                eprintln!("pinfold: see {}", log.display());
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_holds_tabs_splits_and_cwds_and_counts_run_lines_without_sending_them() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let toml = b"[[tabs]]\n[[tabs.panes]]\nrun = 'make'\n\
            [[tabs]]\n[[tabs.panes]]\ncwd = '.'\n[[tabs.panes]]\nsplit = 'down'\nrun = 'ls'\n\
            [[tabs.panes]]\n";
        let layout = workspace::parse(toml, &root).unwrap();
        let (params, waiting) = request(&root, &layout).unwrap();
        let cwd = root.to_str().unwrap();
        let expected = json!({"root": cwd, "layout": {"tabs": [
            {"panes": [{"cwd": cwd}]},
            {"panes": [{"cwd": cwd}, {"split": "down", "cwd": cwd}, {"split": "right", "cwd": cwd}]},
        ]}});
        assert_eq!((params, waiting), (expected, 2));
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
    fn a_table_prints_no_control_bidi_or_zero_width_characters() {
        let list = json!({"panes": [{
            "id": 1, "title": "a\u{202e}b", "cwd": "/x\u{1b}]0;y\u{7}", "command": "s\u{200b}h\r", "status": "idle"
        }]});
        let text = table(&json!({"pane": null}), &list);
        assert!(text.contains("a?b") && text.contains("/x?]0;y?") && text.contains("s?h?"));
        assert!(text.lines().count() == 2);
    }
}
