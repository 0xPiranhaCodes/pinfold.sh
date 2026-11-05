//! A descriptor numbered past the range a pane's child checks. Alone in its own binary, as it
//! raises the open-file limit, which other tests lower when they start a child.

#![allow(unsafe_code, reason = "opens a descriptor at a chosen number")]

use std::io::{ErrorKind, Read};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use pinfold_sys::pty::Pty;

#[test]
fn a_descriptor_numbered_past_the_checked_range_does_not_reach_the_child() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit touch only `limit`.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = limit.rlim_max.min(1 << 20);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
    // From the first number past today's bound of 10,240 to past the 65,536 checked before #219,
    // which macOS allows on a Mac with 24 GB or more. No close-on-exec, on purpose.
    let tried = [10_240, 10_243, 20_000, 70_000];
    let open: Vec<i32> = tried
        .into_iter()
        // SAFETY: dup2 takes integers and touches no memory.
        .filter(|fd| unsafe { libc::dup2(2, *fd) } == *fd)
        .collect();
    // A kernel that caps this process lower cannot make the leak, so only the limit is checked.
    let cap = Command::new("sysctl")
        .args(["-n", "kern.maxfilesperproc"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    eprintln!(
        "of {tried:?} the kernel allowed {open:?}; kern.maxfilesperproc {cap:?}, hard limit {}",
        limit.rlim_max
    );

    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "ls -1 /dev/fd; echo limit $(ulimit -n); echo end"])
        .env_remove("CLICOLOR")
        .env_remove("CLICOLOR_FORCE");
    let pty = Pty::spawn(command, 24, 80).unwrap();
    let (mut text, mut buffer) = (String::new(), [0u8; 4096]);
    let start = Instant::now();
    while !text.contains("end") && start.elapsed() < Duration::from_secs(5) {
        match pty.master().read(&mut buffer) {
            Ok(count) => text.push_str(&String::from_utf8_lossy(&buffer[..count])),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert!(lines.contains(&"0"), "{text}");
    for fd in open {
        assert!(
            !lines.contains(&fd.to_string().as_str()),
            "{fd} reached the child: {text}"
        );
    }
    // The child gets the limit back as the caller had it.
    let wanted = match limit.rlim_cur {
        libc::RLIM_INFINITY => "unlimited".to_owned(),
        soft => soft.to_string(),
    };
    assert!(
        lines.contains(&format!("limit {wanted}").as_str()),
        "{text}"
    );
}
