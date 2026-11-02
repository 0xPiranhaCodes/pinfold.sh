//! The real `pinfold-core` process.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use pinfold_api::control::socket_path;

fn core(runtime: &Path) -> std::io::Result<Child> {
    Command::new(env!("CARGO_BIN_EXE_pinfold-core"))
        .env("XDG_RUNTIME_DIR", runtime)
        .spawn()
}

/// A short path, as a socket's path has a low limit on macOS.
fn runtime() -> PathBuf {
    let path = std::env::temp_dir().join(format!("pd{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    let _ = std::fs::create_dir(&path);
    path
}

fn connect(path: &Path) -> Option<UnixStream> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(stream) = UnixStream::connect(path) {
            return Some(stream);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn a_core_answers_alone_and_a_killed_one_can_be_replaced() {
    let runtime = runtime();
    let path = socket_path(&runtime.join("pinfold"));
    let mut first = core(&runtime).unwrap();
    let mut stream = connect(&path).unwrap();
    stream
        .write_all(b"{\"id\":1,\"method\":\"hello\",\"params\":{\"version\":1}}\n")
        .unwrap();
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).unwrap();
    assert!(reply.contains("\"version\":1"), "{reply}");

    // Hardened: on Linux another process cannot read its memory map.
    #[cfg(target_os = "linux")]
    assert_eq!(
        std::fs::read(format!("/proc/{}/smaps_rollup", first.id()))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );

    let second = core(&runtime).unwrap().wait_with_output().unwrap();
    assert!(!second.status.success());

    first.kill().unwrap();
    first.wait().unwrap();
    let mut third = core(&runtime).unwrap();
    // The dead core's socket file refuses until the new core replaces it.
    let stream = connect(&path);
    third.kill().unwrap();
    third.wait().unwrap();
    assert!(stream.is_some());
    std::fs::remove_dir_all(&runtime).unwrap();
}
