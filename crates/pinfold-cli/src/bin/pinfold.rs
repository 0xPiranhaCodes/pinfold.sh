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

/// `pinfold ctl wait <pane> --until <what> [--pattern <regex>] [--idle-ms <n>] --timeout <s>`:
/// waits on a pane, `self` or a number, and prints how the wait ended. Returns whether what it
/// waited for happened, rather than a timeout or the pane closing. It starts no core.
fn wait(args: &[&str]) -> Result<bool, Box<dyn Error>> {
    let [pane, flags @ ..] = args else {
        return Err(USAGE.into());
    };
    let pane = match *pane {
        "self" => json!("self"),
        pane => json!(
            pane.parse::<usize>()
                .map_err(|_| "the pane must be a number or self")?
        ),
    };
    let mut params = json!({"pane": pane});
    for pair in flags.chunks(2) {
        let (key, value) = match *pair {
            ["--until", until] => ("until", json!(until)),
            ["--pattern", pattern] => ("pattern", json!(pattern)),
            ["--idle-ms", idle] => ("idle_ms", json!(idle.parse::<u64>()?)),
            ["--timeout", seconds] => (
                "timeout_ms",
                json!(seconds.parse::<u64>()?.saturating_mul(1000)),
            ),
            _ => return Err(USAGE.into()),
        };
        params[key] = value;
    }
    let timeout = params["timeout_ms"]
        .as_u64()
        .ok_or("--timeout is required")?;
    let longer = Duration::from_millis(timeout);
    let (_, result) = control::ask_longer(&running()?, "pane.wait", &params, longer)?;
    let outcome = result["outcome"].as_str().unwrap_or_default();
    let text = match (outcome, &result["exit"], result["line"].as_str()) {
        ("exit", Value::Number(exit), _) => format!("exit {exit}"),
        ("match", _, Some(line)) => format!("match {line}"),
        _ => outcome.to_owned(),
    };
    std::io::stdout().write_all(format!("{}\n", printable(&text)).as_bytes())?;
    Ok(!matches!(outcome, "timeout" | "closed"))
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

const USAGE: &str = "usage: pinfold [ctl list [--json] | ctl send <pane> <text> | ctl wait <pane> \
    --until exit|prompt|output|match|idle [--pattern <regex>] [--idle-ms <n>] --timeout <s>]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => launch(),
        ["ctl", "list"] => list(false),
        ["ctl", "list", "--json"] => list(true),
        ["ctl", "send", pane, text] => send(pane, text),
        // 0 once what it waited for happened, 1 if not, and 2 on an error.
        ["ctl", "wait", ref wait_args @ ..] => {
            return match wait(wait_args) {
                Ok(happened) => ExitCode::from(u8::from(!happened)),
                Err(error) => {
                    eprintln!("pinfold: {error}");
                    ExitCode::from(2)
                }
            };
        }
        _ => {
            eprintln!("{USAGE}");
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
