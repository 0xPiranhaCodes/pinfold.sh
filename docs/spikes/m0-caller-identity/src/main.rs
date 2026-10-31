//! M0 spike for bet 4: can a process in pane A act as pane B?
//!
//! `harness` plays the core: it owns two PTY panes and a Unix socket, and identifies each
//! caller from its controlling terminal. Pane B idles. Pane A runs `attacks`, with pane
//! B's environment, and reports what each attempt achieved.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;
use std::{env, fs, io, mem, process, ptr, thread};

const INJECT_MARKER: &str = "PINFOLD_SPIKE_MARKER";

struct Pane {
    name: &'static str,
    leader: libc::pid_t,
    master: RawFd,
    slave_path: String,
    tty_dev: u64,
}

fn errno() -> String {
    io::Error::last_os_error().to_string()
}

fn cstring(text: &str) -> CString {
    CString::new(text).expect("no interior NUL")
}

#[cfg(target_os = "linux")]
fn peer_pid(fd: RawFd) -> libc::pid_t {
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len)
    };
    if result == 0 { cred.pid } else { -1 }
}

#[cfg(target_os = "macos")]
fn peer_pid(fd: RawFd) -> libc::pid_t {
    let mut pid: libc::pid_t = -1;
    let mut len = mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(fd, libc::SOL_LOCAL, libc::LOCAL_PEERPID, (&raw mut pid).cast(), &mut len)
    };
    if result == 0 { pid } else { -1 }
}

/// The controlling terminal and session of `pid`, or `None` if it has gone.
#[cfg(target_os = "linux")]
fn tty_and_session(pid: libc::pid_t) -> Option<(u64, libc::pid_t)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may contain spaces and brackets, so parse after the last ')'.
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let session = fields.nth(3)?.parse().ok()?;
    let tty = fields.next()?.parse().ok()?;
    Some((tty, session))
}

#[cfg(target_os = "macos")]
fn tty_and_session(pid: libc::pid_t) -> Option<(u64, libc::pid_t)> {
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
    };
    let session = unsafe { libc::getsid(pid) };
    (read == size && session > 0).then_some((u64::from(info.e_tdev), session))
}

/// A caller is a pane only if it holds that pane's terminal and is in its session.
fn identify(pid: libc::pid_t, panes: &[Pane]) -> String {
    let Some((tty, session)) = tty_and_session(pid) else {
        return "unidentified (process gone)".into();
    };
    panes
        .iter()
        .find(|pane| pane.tty_dev == tty && pane.leader == session)
        .map_or("unidentified".into(), |pane| format!("pane {}", pane.name))
}

/// Linux only: a pidfd taken at accept time cannot be fooled by pid reuse.
#[cfg(target_os = "linux")]
fn peer_pidfd_exited(fd: RawFd) -> Option<bool> {
    const SO_PEERPIDFD: libc::c_int = 77;
    let mut pidfd: libc::c_int = -1;
    let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, SO_PEERPIDFD, (&raw mut pidfd).cast(), &mut len)
    };
    if result != 0 {
        return None;
    }
    thread::sleep(Duration::from_millis(50));
    let mut poll = libc::pollfd { fd: pidfd, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    unsafe { libc::close(pidfd) };
    Some(ready == 1)
}

#[cfg(target_os = "macos")]
fn peer_pidfd_exited(_fd: RawFd) -> Option<bool> {
    None
}

fn spawn_pane(name: &'static str, program: &str, env_pairs: &[(String, String)]) -> Pane {
    let mut master: RawFd = -1;
    let pid = unsafe {
        libc::forkpty(&mut master, ptr::null_mut(), ptr::null_mut(), ptr::null_mut())
    };
    assert!(pid >= 0, "forkpty: {}", errno());
    if pid == 0 {
        let exe = cstring(&env::current_exe().expect("exe").to_string_lossy());
        let arg = cstring(program);
        let argv = [exe.as_ptr(), arg.as_ptr(), ptr::null()];
        let owned: Vec<CString> =
            env_pairs.iter().map(|(key, value)| cstring(&format!("{key}={value}"))).collect();
        let mut envp: Vec<*const libc::c_char> = owned.iter().map(|item| item.as_ptr()).collect();
        envp.push(ptr::null());
        unsafe {
            libc::execve(exe.as_ptr(), argv.as_ptr().cast(), envp.as_ptr().cast());
            libc::_exit(127);
        }
    }
    // A leaked master would make the attacks below meaningless.
    unsafe { libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC) };
    let slave_path =
        unsafe { CStr::from_ptr(libc::ptsname(master)) }.to_string_lossy().into_owned();
    let mut stat: libc::stat = unsafe { mem::zeroed() };
    let result = unsafe { libc::stat(cstring(&slave_path).as_ptr(), &mut stat) };
    assert_eq!(result, 0, "stat {slave_path}: {}", errno());
    Pane { name, leader: pid, master, slave_path, tty_dev: stat.st_rdev as u64 }
}

fn read_available(fd: RawFd) -> String {
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
    let mut bytes = [0u8; 4096];
    let read = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    String::from_utf8_lossy(&bytes[..read.max(0) as usize]).into_owned()
}

fn harness() -> i32 {
    let dir = env::temp_dir().join(format!("pinfold-spike-{}", process::id()));
    fs::create_dir(&dir).expect("socket directory");
    let socket = dir.join("core.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };

    // Pane B's whole environment, which pane A is also given.
    let mut shared = vec![
        ("PINFOLD_SOCKET".to_owned(), socket.to_string_lossy().into_owned()),
        ("PINFOLD_PANE_ID".to_owned(), "B".to_owned()),
        ("SPIKE_CORE_PID".to_owned(), process::id().to_string()),
    ];
    let pane_b = spawn_pane("B", "idle", &shared);
    shared.push(("SPIKE_B_TTY".into(), pane_b.slave_path.clone()));
    shared.push(("SPIKE_B_PID".into(), pane_b.leader.to_string()));
    shared.push(("SPIKE_B_MASTER_FD".into(), pane_b.master.to_string()));
    let pane_a = spawn_pane("A", "attacks", &shared);
    let panes = [pane_a, pane_b];

    let mut identities = BTreeMap::new();
    let mut reports = BTreeMap::new();
    let mut pidfd_row = None;
    for stream in listener.incoming() {
        let stream = stream.expect("accept");
        let fd = stream.as_raw_fd();
        let pid = peer_pid(fd);
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).expect("read");
        let line = line.trim();
        if line == "done" {
            break;
        }
        if let Some(report) = line.strip_prefix("report ") {
            let (name, text) = report.split_once(' ').expect("report format");
            reports.insert(name.to_owned(), text.to_owned());
        } else if line == "dumpable-off" {
            set_core_dumpable(false);
            (&stream).write_all(b"ok\n").expect("write");
        } else if line == "connect-then-exit" {
            pidfd_row = peer_pidfd_exited(fd);
            thread::sleep(Duration::from_millis(100));
            identities.insert(line.to_owned(), identify(pid, &panes));
        } else {
            let identity = identify(pid, &panes);
            (&stream).write_all(format!("{identity}\n").as_bytes()).expect("write");
            identities.insert(line.to_owned(), identity);
        }
    }

    let tty_output = read_available(panes[1].master);
    let write_reached = tty_output.contains(INJECT_MARKER);
    println!("# Platform: {} {}\n", env::consts::OS, env::consts::ARCH);
    println!("## API identity, called from pane A with pane B's environment\n");
    println!("| Scenario | Core saw | Result |\n| --- | --- | --- |");
    let mut failed = false;
    let expected = [
        ("baseline", "pane A"),
        ("spoofed-env", "pane A"),
        ("forked-child", "pane A"),
        ("new-session", "unidentified"),
        ("steal-b-tty", "unidentified"),
        ("connect-then-exit", "unidentified (process gone)"),
        #[cfg(target_os = "linux")]
        ("pid-namespace", "pane A"),
    ];
    for (scenario, want) in expected {
        let got = identities.get(scenario).map_or("(not run)", String::as_str);
        let pass = got == want;
        failed |= !pass || got.contains("pane B");
        println!("| {scenario} | {got} | {} |", if pass { "pass" } else { "**FAIL**" });
    }
    println!("\n## Attacks that skip the API\n");
    println!("| Attack | Outcome |\n| --- | --- |");
    for (name, text) in &reports {
        println!("| {name} | {text} |");
    }
    let reached = if write_reached { "REACHED pane B's master" } else { "not seen on pane B's master" };
    println!("| tty-bytes-seen-by-core | {reached} |");
    match pidfd_row {
        Some(exited) => println!("| peer-pidfd-detects-exit | {exited} |"),
        None => println!("| peer-pidfd-detects-exit | unavailable on this platform |"),
    }
    println!("\nBet 4 API criterion: {}", if failed { "FAIL" } else { "PASS" });

    for pane in &panes {
        unsafe { libc::kill(pane.leader, libc::SIGKILL) };
    }
    fs::remove_dir_all(dir).ok();
    i32::from(failed)
}

#[cfg(target_os = "linux")]
fn set_core_dumpable(on: bool) {
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, libc::c_ulong::from(on), 0, 0, 0) };
}

#[cfg(target_os = "macos")]
fn set_core_dumpable(_on: bool) {}

fn call(scenario: &str) -> String {
    let path = env::var("PINFOLD_SOCKET").expect("socket");
    let mut stream = UnixStream::connect(path).expect("connect");
    writeln!(stream, "{scenario}").expect("write");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok();
    reply
}

fn report(name: &str, text: &str) {
    let path = env::var("PINFOLD_SOCKET").expect("socket");
    let mut stream = UnixStream::connect(path).expect("connect");
    writeln!(stream, "report {name} {text}").expect("write");
}

/// Runs `body` in a forked child and waits, so that it can change its session safely.
fn in_child(body: impl FnOnce()) {
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        body();
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

fn outcome(result: libc::c_int) -> String {
    if result == 0 { "SUCCEEDED".into() } else { format!("blocked: {}", errno()) }
}

fn steal_tty(path: &str, force: libc::c_int) -> String {
    unsafe { libc::setsid() };
    let fd = unsafe { libc::open(cstring(path).as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return format!("open blocked: {}", errno());
    }
    outcome(unsafe { libc::ioctl(fd, libc::TIOCSCTTY as _, force) })
}

fn inject_tiocsti(path: &str) -> String {
    #[cfg(target_os = "macos")]
    const TIOCSTI: libc::c_ulong = 0x8001_7472;
    #[cfg(target_os = "linux")]
    const TIOCSTI: libc::c_ulong = libc::TIOCSTI as libc::c_ulong;
    let fd = unsafe { libc::open(cstring(path).as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return format!("open blocked: {}", errno());
    }
    let byte = b'x';
    outcome(unsafe { libc::ioctl(fd, TIOCSTI as _, &byte) })
}

#[cfg(target_os = "linux")]
fn ptrace_attach(pid: libc::pid_t) -> String {
    let result = unsafe { libc::ptrace(libc::PTRACE_ATTACH, pid, 0, 0) };
    if result == 0 {
        let mut status = 0;
        unsafe {
            libc::waitpid(pid, &mut status, 0);
            libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0);
        }
        return "SUCCEEDED".into();
    }
    format!("blocked: {}", errno())
}

#[cfg(target_os = "macos")]
fn ptrace_attach(pid: libc::pid_t) -> String {
    let result = unsafe { libc::ptrace(libc::PT_ATTACHEXC, pid, ptr::null_mut(), 0) };
    if result == 0 {
        unsafe { libc::ptrace(libc::PT_DETACH, pid, ptr::null_mut(), 0) };
        return "SUCCEEDED".into();
    }
    format!("blocked: {}", errno())
}

/// Copies pane B's master out of the core, which would let the caller type into pane B.
#[cfg(target_os = "linux")]
fn steal_master(core: libc::pid_t, master: i32) -> String {
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, core, 0) };
    if pidfd < 0 {
        return format!("pidfd_open blocked: {}", errno());
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd as i32, master, 0) };
    if fd < 0 { format!("blocked: {}", errno()) } else { "SUCCEEDED".into() }
}

#[cfg(target_os = "macos")]
fn steal_master(_core: libc::pid_t, _master: i32) -> String {
    "not applicable: no pidfd_getfd".into()
}

/// Models a sandboxed agent: the core must still resolve a caller in a child PID namespace.
#[cfg(target_os = "linux")]
fn pid_namespace_call() {
    in_child(|| {
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) } != 0 {
            report("pid-namespace-setup", &format!("blocked: {}", errno()));
            return;
        }
        in_child(|| {
            call("pid-namespace");
        });
    });
}

#[cfg(target_os = "macos")]
fn pid_namespace_call() {}

fn attacks() {
    let b_tty = env::var("SPIKE_B_TTY").expect("tty");
    let b_pid: libc::pid_t = env::var("SPIKE_B_PID").expect("pid").parse().expect("pid");
    let core: libc::pid_t = env::var("SPIKE_CORE_PID").expect("core").parse().expect("pid");
    let master: i32 = env::var("SPIKE_B_MASTER_FD").expect("fd").parse().expect("fd");

    call("baseline");
    call("spoofed-env");
    in_child(|| {
        call("forked-child");
    });
    in_child(|| {
        unsafe { libc::setsid() };
        call("new-session");
    });
    in_child(|| {
        let adopted = steal_tty(&b_tty, 0);
        report("adopt-b-tty-as-ctty", &adopted);
        call("steal-b-tty");
    });
    in_child(|| {
        report("force-steal-b-tty", &steal_tty(&b_tty, 1));
    });
    in_child(|| {
        let path = env::var("PINFOLD_SOCKET").expect("socket");
        let mut stream = UnixStream::connect(path).expect("connect");
        writeln!(stream, "connect-then-exit").expect("write");
        mem::forget(stream);
    });
    thread::sleep(Duration::from_millis(300));
    pid_namespace_call();

    report("tiocsti-into-b-input", &inject_tiocsti(&b_tty));
    let wrote = fs::OpenOptions::new()
        .write(true)
        .open(Path::new(&b_tty))
        .and_then(|mut tty| tty.write_all(INJECT_MARKER.as_bytes()));
    report("write-b-tty-output", if wrote.is_ok() { "SUCCEEDED" } else { "blocked" });
    report("ptrace-pane-b", &ptrace_attach(b_pid));
    report("ptrace-core", &ptrace_attach(core));
    report("steal-b-master-core-dumpable", &steal_master(core, master));
    call("dumpable-off");
    report("steal-b-master-core-not-dumpable", &steal_master(core, master));
    report("ptrace-core-not-dumpable", &ptrace_attach(core));

    let path = env::var("PINFOLD_SOCKET").expect("socket");
    let mut stream = UnixStream::connect(path).expect("connect");
    writeln!(stream, "done").expect("write");
}

fn main() {
    match env::args().nth(1).as_deref() {
        Some("attacks") => attacks(),
        Some("idle") => loop {
            thread::sleep(Duration::from_secs(60));
        },
        _ => process::exit(harness()),
    }
}
