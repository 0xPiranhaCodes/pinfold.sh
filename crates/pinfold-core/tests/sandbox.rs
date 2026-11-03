//! A sandboxed agent, run with the real `sandbox-exec` or bubblewrap, cannot read a hidden
//! directory and keeps the caller identity the API gives its pane.

use std::fs;
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use pinfold_core::sandbox;
use pinfold_sys::pty::Pty;

#[test]
fn a_sandboxed_agent_cannot_read_hidden_paths_and_keeps_its_identity() {
    let home = std::env::temp_dir().join(format!("pinfold-sandbox-{}", std::process::id()));
    let ssh = home.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_test"), "SECRET-KEY\n").unwrap();
    fs::write(home.join("open"), "VISIBLE\n").unwrap();
    let socket = home.join("api.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();

    let path = |path: PathBuf| path.to_str().unwrap().to_owned();
    // The client stays connected until the core has asked who it is.
    let script = format!(
        "cat '{}/id_test'; cat '{}/open'; echo uid=$(id -u); \
         perl -MSocket -e 'socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die; \
         connect($s, pack_sockaddr_un($ARGV[0])) or die; sleep 2' '{}'; \
         echo done",
        path(ssh.clone()),
        path(home.clone()),
        path(socket),
    );
    let program = ["/bin/sh".to_owned(), "-c".to_owned(), script];
    let command = match sandbox::command(&program, &[ssh]) {
        Err(error) if cfg!(target_os = "linux") && error.kind() == ErrorKind::NotFound => {
            eprintln!("skipped: bwrap is not on PATH");
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
    fs::remove_dir_all(&home).unwrap();
    assert!(!output.contains("SECRET-KEY"), "{output}");
    assert!(output.contains("id_test"), "{output}");
    assert!(output.contains("VISIBLE"), "{output}");
    let uid = format!("uid={}", pinfold_sys::effective_uid());
    assert!(output.contains(&uid), "{output}");
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
