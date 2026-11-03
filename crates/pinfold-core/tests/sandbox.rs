//! A sandboxed agent, run with the real `sandbox-exec` or bubblewrap, cannot read hidden
//! directories or another pane's terminal, writes only to its working directory, and keeps the
//! caller identity the API gives its pane.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use pinfold_core::sandbox;
use pinfold_sys::pty::Pty;

#[test]
fn a_sandboxed_agent_cannot_read_hidden_paths_and_keeps_its_identity() {
    // Not under the temporary directory, which the agent may write to.
    let home = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-sandbox-{}", std::process::id()));
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_test"), "SECRET-KEY\n").unwrap();
    let state = home.join("state");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("state"), "SECRET-STATE\n").unwrap();
    fs::write(home.join("open"), "VISIBLE\n").unwrap();
    let work = home.join("work");
    fs::create_dir_all(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = home.join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();

    // Another pane, whose terminal the agent must not open.
    let mut peer = Command::new("/bin/sh");
    peer.args(["-c", "tty; sleep 30"]);
    let peer = Pty::spawn(peer, 24, 80).unwrap();
    let peer_tty = read_until(&peer, "\n");
    let peer_tty = peer_tty.trim();
    assert!(peer_tty.starts_with("/dev/"), "{peer_tty}");
    let tiocsti = if cfg!(target_os = "macos") {
        0x8001_7472_u32
    } else {
        0x5412
    };

    let path = |path: PathBuf| path.to_str().unwrap().to_owned();
    // The client stays connected until the core has asked who it is.
    let script = format!(
        "cat '{}/id_test'; cat '{}/state'; cat '{2}/open'; echo uid=$(id -u); \
         (echo x >'{2}/.zshrc') 2>/dev/null && echo rc=written || echo rc=refused; \
         echo x >'{2}/work/out' && echo work=written; \
         (exec 3<'{peer_tty}') 2>/dev/null && echo peer=open || echo peer=closed; \
         perl -e 'open(my $t, \"+<\", \"/dev/tty\") or die; my $c = \"x\"; \
         print ioctl($t, {tiocsti}, $c) ? \"sti=allowed\\n\" : \"sti=denied\\n\"'; \
         perl -MSocket -e 'socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         connect($s, pack_sockaddr_un($ARGV[0])) or die; sleep 2' '{3}'; \
         echo done",
        path(ssh.clone()),
        path(state.clone()),
        path(home.clone()),
        path(socket),
    );
    let program = ["/bin/sh".to_owned(), "-c".to_owned(), script];
    let hidden = [ssh, state];
    let work = sandbox::working_directory(&work, &hidden).unwrap();
    let command = match sandbox::command(&program, &hidden, &work) {
        Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::NotFound => {
            eprintln!("skipped: no bwrap");
            return;
        }
        command => command.unwrap(),
    };
    let pty = Pty::spawn(command, 24, 80).unwrap();

    let start = Instant::now();
    let caller = loop {
        match listener.accept() {
            Ok((caller, _)) => break caller,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let late = start.elapsed() > Duration::from_secs(10);
                assert!(!late, "no caller: {}", read_until(&pty, "done"));
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    let identity = pinfold_sys::peer_terminal(caller.as_fd()).unwrap();
    assert_eq!(identity, Some(pty.terminal()));

    let output = read_until(&pty, "done");
    let (rc, out) = (
        home.join(".zshrc").exists(),
        work.path().join("out").exists(),
    );
    fs::remove_dir_all(&home).unwrap();
    assert!(output.contains("rc=refused") && !rc, "{output}");
    assert!(output.contains("work=written") && out, "{output}");
    assert!(!output.contains("SECRET-KEY"), "{output}");
    assert!(output.contains("id_test"), "{output}");
    assert!(!output.contains("SECRET-STATE"), "{output}");
    assert!(output.contains("peer=closed"), "{output}");
    // Linux has no TIOCSTI filter yet; the sandbox wraps the whole pane instead.
    if cfg!(target_os = "macos") {
        assert!(output.contains("sti=denied"), "{output}");
    }
    assert!(output.contains("VISIBLE"), "{output}");
    let uid = format!("uid={}", pinfold_sys::effective_uid());
    assert!(output.contains(&uid), "{output}");
}

#[test]
fn a_bwrap_planted_on_path_is_ignored() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("pinfold-planted-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let planted = dir.join("bwrap");
    fs::write(&planted, "#!/bin/sh\nexec \"$@\"\n").unwrap();
    fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
    // The variable is read at each lookup, so the check runs in a child with its own PATH.
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "--ignored", "planted_child", "--nocapture"])
        .env("PATH", &dir)
        .env("PINFOLD_PLANTED", &planted)
        .status()
        .unwrap();
    fs::remove_dir_all(&dir).unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "run by a_bwrap_planted_on_path_is_ignored"]
fn planted_child() {
    let Some(planted) = std::env::var_os("PINFOLD_PLANTED") else {
        return;
    };
    let work = std::env::temp_dir().join(format!("pinfold-child-{}", std::process::id()));
    fs::create_dir_all(&work).unwrap();
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700)).unwrap();
    let work = sandbox::working_directory(&work, &[]).unwrap();
    let result = sandbox::command(&["/bin/true".to_owned()], &[], &work);
    fs::remove_dir(work.path()).unwrap();
    match result {
        Ok(command) => assert_ne!(command.get_program(), planted),
        Err(error) => assert_eq!(error.kind(), ErrorKind::NotFound),
    }
}

/// Reads until `wanted` appears or ten seconds pass.
fn read_until(pty: &Pty, wanted: &str) -> String {
    let (mut text, mut buffer) = (String::new(), [0u8; 4096]);
    let start = Instant::now();
    while !text.contains(wanted) && start.elapsed() < Duration::from_secs(10) {
        match pty.master().read(&mut buffer) {
            Ok(count) => text.push_str(&String::from_utf8_lossy(&buffer[..count])),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            // Linux reports EIO once the child has closed its end.
            Err(_) => break,
        }
    }
    text
}
