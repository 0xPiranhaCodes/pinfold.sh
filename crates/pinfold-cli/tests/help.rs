//! `pinfold`'s help, version and bad-argument errors. None of these reach a core.

use std::error::Error;
use std::process::{Command, Output};

/// Runs `pinfold` with a runtime directory no core is in, should a case reach for one.
fn pinfold(args: &[&str]) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_pinfold"))
        .args(args)
        .env("XDG_RUNTIME_DIR", "/nonexistent")
        .output()
}

/// What `pinfold` prints for `args`, which must exit 0 and print nothing on stderr.
fn stdout(args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = pinfold(args)?;
    assert_eq!(output.status.code(), Some(0), "{args:?}");
    assert!(output.stderr.is_empty(), "{args:?}");
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn help_lists_every_command_and_option() {
    let text = stdout(&["--help"]).unwrap();
    assert_eq!(stdout(&["-h"]).unwrap(), text);
    assert_eq!(stdout(&["help"]).unwrap(), text);
    for word in [
        "up [dir]",
        "agent <name>",
        "ctl list",
        "ctl send",
        "ctl wait",
        "ctl status",
        "ctl grants",
        "ctl log",
        "ctl session save",
        "mcp",
        "help [command]",
        "-h, --help",
        "-V, --version",
    ] {
        assert!(text.contains(word), "{word}");
    }
}

#[test]
fn each_command_has_help_naming_its_arguments_and_flags() {
    let commands: &[(&[&str], &[&str])] = &[
        (&["up"], &["[dir]"]),
        (&["agent"], &["<name>", "--right", "--down", "--tab"]),
        (
            &["ctl"],
            &[
                "list",
                "send",
                "wait",
                "status",
                "grants",
                "log",
                "session save",
            ],
        ),
        (&["ctl", "list"], &["--json"]),
        (&["ctl", "send"], &["<pane>", "<text>"]),
        (
            &["ctl", "wait"],
            &[
                "<pane>",
                "self",
                "--until",
                "exit",
                "prompt",
                "output",
                "match",
                "idle",
                "--pattern",
                "--idle-ms",
                "--timeout",
            ],
        ),
        (&["ctl", "status"], &["<state>", "[message]", "needs-you"]),
        (&["ctl", "grants"], &["--json", "revoke <id>"]),
        (
            &["ctl", "log"],
            &["--json", "-n <count>", "--pane <pane>", "--verify <file>"],
        ),
        (&["ctl", "session"], &["pinfold ctl session save"]),
        (&["mcp"], &["pinfold mcp"]),
    ];
    for (command, words) in commands {
        let text = stdout(&[command, &["--help"][..]].concat()).unwrap();
        assert_eq!(stdout(&[command, &["-h"][..]].concat()).unwrap(), text);
        assert_eq!(stdout(&[&["help"][..], command].concat()).unwrap(), text);
        assert!(text.contains("Usage: pinfold"), "{command:?}");
        assert!(text.contains("Example"), "{command:?}");
        for word in *words {
            assert!(text.contains(word), "{command:?} {word}");
        }
    }
}

#[test]
fn version_prints_the_workspace_version() {
    let expected = format!("pinfold {}\n", env!("CARGO_PKG_VERSION"));
    assert_eq!(stdout(&["--version"]).unwrap(), expected);
    assert_eq!(stdout(&["-V"]).unwrap(), expected);
}

#[test]
fn a_bad_argument_names_the_problem_in_one_line_and_exits_2() {
    let cases: &[(&[&str], &str)] = &[
        (&["nosuch"], "unknown command 'nosuch'"),
        (&["nosuch", "--help"], "unknown command 'nosuch'"),
        (&["--frob"], "unknown option '--frob'"),
        (&["ctl"], "usage: pinfold ctl <command>"),
        (&["ctl", "nosuch"], "unknown command 'ctl nosuch'"),
        (
            &["ctl", "list", "--jsn"],
            "usage: pinfold ctl list [--json]",
        ),
        (&["agent", "a", "--left"], "usage: pinfold agent <name>"),
        (&["ctl", "send", "x", "y"], "the pane must be a number"),
        (&["ctl", "wait"], "'ctl wait' needs a pane"),
        (&["ctl", "wait", "2", "--until"], "'--until' needs a value"),
        (
            &["ctl", "wait", "2", "--frob", "1"],
            "unknown option '--frob'",
        ),
        (
            &["ctl", "wait", "2", "--until", "exit", "--timeout", "x"],
            "--timeout takes a whole number",
        ),
        (
            &["ctl", "wait", "2", "--until", "exit"],
            "--timeout is required",
        ),
        (&["ctl", "log", "-n", "x"], "-n takes a number"),
        (
            &["ctl", "grants", "revoke", "x"],
            "the grant id must be a number",
        ),
    ];
    for (args, problem) in cases {
        let output = pinfold(args).unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let lines: Vec<&str> = stderr.lines().collect();
        assert_eq!(lines.len(), 2, "{args:?}: {stderr}");
        assert!(lines[0].starts_with("pinfold: "), "{args:?}: {stderr}");
        assert!(lines[0].contains(problem), "{args:?}: {stderr}");
        assert_eq!(lines[1], "Run 'pinfold --help' for usage.");
    }
}
