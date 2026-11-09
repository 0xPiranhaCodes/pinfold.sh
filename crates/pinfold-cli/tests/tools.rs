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
        // The test runs in this repository, where HEAD always resolves; `--staged` takes one commit.
        "diff",
        "diff HEAD..HEAD",
        "diff --staged HEAD",
        // As the core starts them (#312).
        "md -- Cargo.toml",
        "files -- src",
        "diff --staged -- HEAD",
    ] {
        let pty = tool(args).unwrap();
        let mut text = String::new();
        read_until(&pty, &mut text, "? keys");
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

#[test]
fn e_hands_the_terminal_to_the_editor_as_it_was_then_redraws() {
    let root = std::env::temp_dir().join(format!("pinfold-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("dir")).unwrap();
    // The tool shows the directory as the system resolves it, /private/var on macOS.
    let root = root.canonicalize().unwrap();
    let dir = root.join("dir");
    // A name that a shell would run, and that an editor would take for an option.
    let file = dir.join("-rf; touch ran 'q'.txt");
    std::fs::write(&file, "").unwrap();
    let (editor, out) = (root.join("editor.sh"), root.join("out"));
    std::fs::write(
        &editor,
        "#!/bin/sh\n[ \"$(stty -g)\" = \"$MODE\" ] && echo cooked >> \"$OUT\"\n\
         printf '%s\\n' \"$@\" >> \"$OUT\"\n",
    )
    .unwrap();
    let script = "MODE=$(stty -g); export MODE; \"$PINFOLD\" files -- \"$DIR\"; status=$?; \
                  [ \"$MODE\" = \"$(stty -g)\" ] && echo restored; echo \"exit $status\"";
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .env("PINFOLD", env!("CARGO_BIN_EXE_pinfold"))
        .env("VISUAL", format!("/bin/sh '{}' -x", editor.display()))
        .env("OUT", &out)
        .env("DIR", &dir)
        .current_dir(&root);
    let pty = Pty::spawn(command, 24, 80).unwrap();
    let mut text = String::new();
    read_until(&pty, &mut text, "? keys");
    pty.master().write_all(b"e").unwrap();
    let mut redrawn = String::new();
    read_until(&pty, &mut redrawn, "? keys");
    let edited = std::fs::read_to_string(&out).unwrap();
    assert_eq!(edited, format!("cooked\n-x\n{}\n", file.display()));
    assert!(!root.join("ran").exists() && !dir.join("ran").exists());
    pty.master().write_all(b"q").unwrap();
    read_until(&pty, &mut redrawn, "exit 0");
    assert!(redrawn.contains("restored"), "{redrawn:?}");
    std::fs::remove_dir_all(root).unwrap();
}
