//! The `pinfold` launcher: starts the core if none answers, asks it for a window, and exits.
//! `pinfold ctl` is a thin client of the core's API (ADR-0032).

use std::error::Error;
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pinfold_api::control;
use serde_json::Value;

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

/// `pinfold ctl list`: prints the session's panes, as JSON with `json`. It starts no core.
fn list(json: bool) -> Result<(), Box<dyn Error>> {
    let path = control::socket_path(&control::runtime_dir());
    let stream = connect(&path)?.ok_or("PinFold is not running")?;
    let (hello, list) = control::list_panes(&stream)?;
    let text = if json {
        format!("{list}\n")
    } else {
        table(&hello, &list)
    };
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(())
}

/// The panes as plain text, a line each, with `*` by the caller's own. Every field is from the
/// core, whose OSC 7 parser lets no control character into a directory.
fn table(hello: &Value, list: &Value) -> String {
    let mut text = format!(
        "{:<5} {:<8} {:<16} {:<24} CWD\n",
        "ID", "STATUS", "TITLE", "COMMAND"
    );
    let panes = list["panes"].as_array().map_or(&[][..], Vec::as_slice);
    for pane in panes {
        let field = |name: &str| pane[name].as_str().unwrap_or("-").to_owned();
        let own = if pane["id"] == hello["pane"] { "*" } else { "" };
        let id = format!("{}{own}", pane["id"]);
        let (status, title) = (field("status"), field("title"));
        let (command, cwd) = (field("command"), field("cwd"));
        text += &format!("{id:<5} {status:<8} {title:<16} {command:<24} {cwd}\n");
    }
    text
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => launch(),
        ["ctl", "list"] => list(false),
        ["ctl", "list", "--json"] => list(true),
        _ => {
            eprintln!("usage: pinfold [ctl list [--json]]");
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold: {error}");
            // A core started from a terminal logs there, so this may be an older core's.
            let log = control::log_path(&control::runtime_dir());
            if args.is_empty() && log.exists() {
                eprintln!("pinfold: see {}", log.display());
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
