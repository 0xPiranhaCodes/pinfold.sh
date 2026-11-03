//! A real core audits the requests it answers, and `pinfold ctl log` reads and verifies it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use std::{env, fs, thread};

/// The core's state directory under `home` and `state`, as `pinfold_config::state_dir` finds it.
fn audit_file(home: &Path, state: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/PinFold/audit.log")
    } else {
        state.join("pinfold/audit.log")
    }
}

fn wait_for(done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn sends_are_audited_by_size_and_only_a_terminal_outside_a_pane_reads_the_log() {
    // A short path, as a socket's path has a low limit on macOS.
    let directory = env::temp_dir().join(format!("pa{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    let (home, state, config) = (
        directory.join("h"),
        directory.join("s"),
        directory.join("c"),
    );
    for made in [
        &home,
        &state,
        &config.join("pinfold"),
        &directory.join("run"),
    ] {
        fs::create_dir_all(made).unwrap();
    }
    fs::write(
        config.join("pinfold/pinfold.toml"),
        "[api]\naudit_log = true\n",
    )
    .unwrap();
    let pinfold = env!("CARGO_BIN_EXE_pinfold");
    let at = |name: &str| directory.join(name).display().to_string();
    // Pane 0 sends to itself, pane 1 is refused, and neither may read the log.
    let shell = format!(
        "#!/bin/sh\n'{pinfold}' ctl send 0 'hidden words\n' 2>/dev/null\n\
         '{pinfold}' ctl log 2> '{}'\necho >> '{}'\nexec sleep 30\n",
        at("inside"),
        at("done")
    );
    fs::write(directory.join("shell"), shell).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(directory.join("shell"), fs::Permissions::from_mode(0o700)).unwrap();
    let environment = [
        ("XDG_RUNTIME_DIR", directory.join("run")),
        ("HOME", home.clone()),
        ("XDG_STATE_HOME", state.clone()),
        ("XDG_CONFIG_HOME", config),
        ("SHELL", directory.join("shell")),
    ];
    let mut core = Command::new(env!("CARGO_BIN_EXE_pinfold-core"))
        .args(["--idle", "2"])
        .envs(environment.clone())
        .spawn()
        .unwrap();
    let done = || fs::read_to_string(directory.join("done")).unwrap_or_default();
    assert!(wait_for(|| done().lines().count() == 2));
    let inside = fs::read_to_string(directory.join("inside")).unwrap();
    assert!(inside.contains("outside any pane"), "{inside}");

    let mut outside = Command::new("/bin/sh");
    let script = format!(
        "'{pinfold}' ctl log > '{}' 2>&1; '{pinfold}' ctl log --json -n 1000 > '{}'",
        at("text"),
        at("json")
    );
    outside.args(["-c", &script]).envs(environment);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    let json = || fs::read_to_string(directory.join("json")).unwrap_or_default();
    assert!(wait_for(|| json().ends_with('\n')));
    let text = fs::read_to_string(directory.join("text")).unwrap();
    for expected in [
        "pane 0 pane.send pane 0 ok: allowed, 13 bytes\n",
        // With no window to ask in, a cross-pane send fails closed.
        "pane 1 pane.send pane 0 refused: approval needs an open PinFold window, 13 bytes\n",
        "audit.read - refused: audit.read needs a caller in a terminal outside any pane\n",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    let read: serde_json::Value = serde_json::from_str(&json()).unwrap();
    assert_eq!(read["verified"], true);
    let file = audit_file(&home, &state);
    let written = fs::read_to_string(&file).unwrap();
    for seen in [&text, &json(), &written] {
        assert!(!seen.contains("hidden"), "{seen}");
    }
    core.kill().unwrap();
    core.wait().unwrap();

    let verify = || {
        Command::new(pinfold)
            .args(["ctl", "log", "--verify"])
            .arg(&file)
            .output()
    };
    assert!(verify().unwrap().status.success());
    fs::write(&file, written.replacen("refused", "ok", 1)).unwrap();
    let broken = verify().unwrap();
    assert!(!broken.status.success());
    assert!(String::from_utf8_lossy(&broken.stderr).contains("broken"));
    let _ = fs::remove_dir_all(&directory);
}
