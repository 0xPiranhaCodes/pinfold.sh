//! `pinfold md`, `files` and `diff` run full screen in a terminal and leave it as they found it.

use std::io::{ErrorKind, Read, Write};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use pinfold_sys::pty::Pty;

/// Runs `pinfold <args>` in a shell on a PTY, which prints whether the terminal's mode is
/// restored and how the tool exited.
fn tool(args: &str) -> std::io::Result<Pty> {
    let script = format!(
        "mode=$(stty -g); \"$PINFOLD\" {args}; status=$?; \
         [ \"$mode\" = \"$(stty -g)\" ] && echo restored; echo \"exit $status\""
    );
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", &script])
        .env("PINFOLD", env!("CARGO_BIN_EXE_pinfold"));
    Pty::spawn(command, 24, 80)
}

/// Reads until `wanted` appears or ten seconds pass.
fn read_until(pty: &Pty, text: &mut String, wanted: &str) {
    let (start, mut buffer) = (Instant::now(), [0; 4096]);
    while !text.contains(wanted) && start.elapsed() < Duration::from_secs(10) {
        match pty.master().read(&mut buffer) {
            Ok(count) => text.push_str(&String::from_utf8_lossy(&buffer[..count])),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    assert!(text.contains(wanted), "no {wanted:?} in {text:?}");
}

#[test]
fn each_tool_quits_on_q_and_restores_the_terminal() {
    for args in [
        "md Cargo.toml",
        "files",
        "files src",
        "diff",
        "diff --staged main..HEAD",
    ] {
        let pty = tool(args).unwrap();
        let mut text = String::new();
        read_until(&pty, &mut text, "Press q to leave.");
        assert!(text.contains("\x1b[?1049h"), "{args}: the alternate screen");
        pty.master().write_all(b"jq").unwrap();
        read_until(&pty, &mut text, "exit 0");
        assert!(text.contains("\x1b[?1049l"), "{args}: {text:?}");
        assert!(text.contains("restored"), "{args}: {text:?}");
    }
}

#[test]
fn a_tool_that_cannot_start_shows_the_error_until_a_key_and_exits_1() {
    let pty = tool("md 'gone\x1b[2J.md'").unwrap();
    let mut text = String::new();
    read_until(&pty, &mut text, "Press any key to leave.");
    assert!(text.contains("\x1b[0;31;49m✕ error"), "{text:?}");
    assert!(
        text.contains("gone^[[2J.md"),
        "the name is escaped: {text:?}"
    );
    assert!(!text.contains("\x1b[2J"), "{text:?}");
    pty.master().write_all(b"x").unwrap();
    read_until(&pty, &mut text, "exit 1");
    assert!(text.contains("restored"), "{text:?}");
}
