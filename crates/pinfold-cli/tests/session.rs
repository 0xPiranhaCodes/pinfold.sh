//! The launcher, a real core and a client on the real link, each its own process.
//!
//! No display is needed: each test copies the binaries to a directory of its own, and a script
//! stands in as the `pinfold-window` beside the core there. The core starts that script as it
//! would a window, so it gets the link on descriptor 3 and hands it to this test binary.

use std::error::Error;
use std::fmt::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use pinfold_api::control;
use pinfold_proto::link::{ApprovalAction, ApprovalAnswer, Request, VERSION};
use pinfold_sys::socket;
use pinfold_ui::link::{Disconnect, Link};

/// Names the directory of a run, in a process acting as the window.
const CLIENT: &str = "PINFOLD_TEST_CLIENT";
/// Asks this test binary, in a process of its own, to harden itself twice.
const HARDEN: &str = "PINFOLD_TEST_HARDEN";
/// Set in a process that should panic.
const PANIC: &str = "PINFOLD_TEST_PANIC";
/// Names the directory of a run, in the pane that calls the API.
const API: &str = "PINFOLD_TEST_API";
/// Set in a pane that reports a status.
const STATUS: &str = "PINFOLD_TEST_STATUS";
const PATIENCE: Duration = Duration::from_secs(120);

type Result<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// A runtime and binary directory, whose core dies with the test whatever happens.
struct Session {
    directory: PathBuf,
}

impl Session {
    /// A short path, as a socket's path has a low limit on macOS.
    fn new(name: &str, window: Option<&str>) -> Result<Self> {
        let directory = env::temp_dir().join(format!("ps{}{name}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("run"))?;
        place(env!("CARGO_BIN_EXE_pinfold"), &directory.join("pinfold"))?;
        place(
            env!("CARGO_BIN_EXE_pinfold-core"),
            &directory.join("pinfold-core"),
        )?;
        if let Some(window) = window {
            let script = directory.join("pinfold-window");
            fs::write(&script, format!("#!/bin/sh\n{window}\n"))?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self { directory })
    }

    fn launcher(&self, shell: &str) -> Command {
        let mut command = Command::new(self.directory.join("pinfold"));
        command
            .env("XDG_RUNTIME_DIR", self.directory.join("run"))
            .env("SHELL", shell)
            .env(CLIENT, &self.directory);
        command
    }

    fn launch(&self) -> Result<Output> {
        Ok(self.launcher("/bin/sh").output()?)
    }

    /// Starts a core with `arguments` and a one-line shell, and notes the core.
    fn start_core(&self, arguments: &[&str], shell: &str) -> Result<Child> {
        let script = self.directory.join("shell");
        fs::write(&script, format!("#!/bin/sh\n{shell}\n"))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        let core = Command::new(self.directory.join("pinfold-core"))
            .args(arguments)
            .env("XDG_RUNTIME_DIR", self.directory.join("run"))
            .env("XDG_CONFIG_HOME", self.directory.join("config"))
            .env("SHELL", &script)
            .env(CLIENT, &self.directory)
            .spawn()?;
        fs::write(self.directory.join("core"), core.id().to_string())?;
        Ok(core)
    }

    fn start_launcher(&self) -> Result<Child> {
        Ok(self.launcher("/bin/sh").spawn()?)
    }

    /// The session's log, as a launcher names it.
    fn log(&self) -> PathBuf {
        control::log_path(&self.directory.join("run/pinfold"))
    }

    fn read_log(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.directory.join(name)).unwrap_or_default()
    }

    fn wait_for(&self, name: &str) -> bool {
        let start = Instant::now();
        while start.elapsed() < PATIENCE {
            if !self.read(name).is_empty() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let core = self.read("core");
        if !core.trim().is_empty() {
            let _ = kill(core.trim()).stderr(Stdio::null()).status();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Links a binary in. A copy's descriptor, open for writing, leaks into any child another test
/// is forking, and Linux then refuses to run the copy with ETXTBSY.
fn place(binary: &str, to: &Path) -> std::io::Result<()> {
    fs::hard_link(binary, to).or_else(|_| fs::copy(binary, to).map(drop))
}

/// The shell's builtin, as a slim image has no `kill` program.
fn kill(pid: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &format!("kill -9 {pid}")]);
    command
}

fn shows(link: &Link, text: &str) -> bool {
    let frame = link.screens().first().map(|screen| &screen.frame);
    let cells = frame.iter().flat_map(|frame| &frame.cells);
    let cells: String = cells.map(|cell| cell.character).collect();
    cells.contains(text)
}

fn type_in(link: &mut Link, text: &str) -> bool {
    let (pane, bytes) = (link.focused(), text.as_bytes());
    link.send(Request::Input { pane, bytes })
}

/// Turns `link` until `done`. A link that ends only runs out the clock, as `done` may want that.
fn until(link: &mut Link, mut done: impl FnMut(&mut Link) -> bool) -> Result {
    let start = Instant::now();
    while !done(link) {
        if start.elapsed() > PATIENCE {
            return Err("the client timed out".into());
        }
        let _ = link.turn(Some(Duration::from_millis(20)));
    }
    Ok(())
}

/// The first client types, then leaves when asked to the front. The second sees what is left.
fn client(directory: &Path) -> Result {
    let note = |name: &str, text: &str| fs::write(directory.join(name), text);
    let mut starts = fs::read_to_string(directory.join("starts")).unwrap_or_default();
    writeln!(starts, "start")?;
    note("starts", &starts)?;
    note("core", &std::os::unix::process::parent_id().to_string())?;
    let mut link = Link::new(socket::take_link()?)?;
    assert!(link.resize(24, 80));
    until(&mut link, |link| !link.screens().is_empty())?;
    if starts.lines().count() == 1 {
        assert!(type_in(
            &mut link,
            "echo mark-$((40+2)); echo a-pane >&2; seq 1 100\n"
        ));
        until(&mut link, |link| shows(link, "100"))?;
        note("typed", "yes")?;
        until(&mut link, Link::take_raised)?;
        note("raised", "yes")?;
        return Ok(());
    }
    until(&mut link, |link| shows(link, "100"))?;
    assert!(type_in(&mut link, "echo again-$((1+1))\n"));
    until(&mut link, |link| shows(link, "again-2"))?;
    let pane = link.focused();
    let lines = 10_000;
    assert!(link.send(Request::Scroll { pane, lines }));
    until(&mut link, |link| shows(link, "mark-42"))?;
    note("reattached", "yes")?;
    until(&mut link, |link| link.disconnected().is_some())?;
    assert_eq!(link.disconnected(), Some(Disconnect::Closed));
    assert!(shows(&link, "mark-42"), "the last screen stays");
    note("ended", "yes")?;
    Ok(())
}

/// Not a test: the core runs this binary as its window, through the script in a [`Session`].
#[test]
fn client_process() {
    if let Some(directory) = env::var_os(CLIENT) {
        client(Path::new(&directory)).unwrap();
    }
}

#[test]
fn a_session_outlives_its_client_and_says_when_the_core_dies() {
    let exe = env::current_exe().unwrap();
    let window = format!("exec '{}' --exact client_process", exe.display());
    let session = Session::new("a", Some(&window)).unwrap();

    assert!(session.launch().unwrap().status.success());
    assert!(session.wait_for("typed"), "input did not come back");
    assert!(session.log().exists());
    assert!(
        !session.read_log().contains("a-pane"),
        "a pane reached the log"
    );

    // The launcher has gone, and the core is alive to answer a second request.
    assert!(session.launch().unwrap().status.success());
    assert!(session.wait_for("raised"), "a second open did not raise");
    assert_eq!(session.read("starts").lines().count(), 1);

    // The client has gone, and a new one starts once the core has seen that. Until then a
    // launcher finds the first one open, so it asks again.
    let start = Instant::now();
    while session.read("starts").lines().count() < 2 && start.elapsed() < PATIENCE {
        assert!(session.launch().unwrap().status.success());
        thread::sleep(Duration::from_millis(50));
    }
    assert!(session.wait_for("reattached"), "the screen did not survive");

    let core = session.read("core");
    assert!(kill(core.trim()).status().unwrap().success());
    assert!(
        session.wait_for("ended"),
        "the client missed the core dying"
    );
}

/// Shell that writes the link's hello to descriptor 3.
fn say_hello() -> Result<String> {
    let mut wire = Vec::new();
    let hello = Request::Hello { version: VERSION };
    hello.encode(&mut wire)?;
    let octal: String = wire.iter().map(|byte| format!("\\{byte:03o}")).collect();
    Ok(format!("printf '{octal}' >&3"))
}

/// Shell that notes the core, so a [`Session`] can end it.
const NOTE_CORE: &str = "echo $PPID > \"$PINFOLD_TEST_CLIENT/core\"";
/// Shell that stays until the core has gone.
const STAY: &str = "exec cat <&3 >/dev/null";

/// Launches once, expects that to fail, and returns what the launcher said.
fn launch_failure(session: &Session) -> Result<String> {
    let launched = session.launch()?;
    assert!(!launched.status.success(), "the launcher reported a window");
    Ok(String::from_utf8_lossy(&launched.stderr).into_owned())
}

#[test]
fn two_launchers_at_once_share_one_core_and_one_window() {
    let say_hello = say_hello().unwrap();
    let window = format!(
        "cd \"$PINFOLD_TEST_CLIENT\"\necho start >> starts\n{NOTE_CORE}\n{say_hello}\n{STAY}"
    );
    let session = Session::new("b", Some(&window)).unwrap();
    let (mut first, mut second) = (
        session.start_launcher().unwrap(),
        session.start_launcher().unwrap(),
    );
    assert!(session.wait_for("core"));
    first.wait().unwrap();
    second.wait().unwrap();
    assert_eq!(session.read("starts").lines().count(), 1);

    // A loaded machine may run a launcher past its reply deadline. Once the window has said it
    // is ready, the session must answer and must still have only that window.
    assert!(session.launch().unwrap().status.success());
    assert_eq!(session.read("starts").lines().count(), 1);
}

#[test]
fn a_window_that_exits_during_start_fails_the_launcher() {
    let window = format!("{NOTE_CORE}\necho why-it-ended >&2\nexit 3");
    let session = Session::new("d", Some(&window)).unwrap();
    let error = launch_failure(&session).unwrap();
    assert!(
        error.contains("the window ended before it was ready"),
        "{error}"
    );
    assert!(error.contains(session.log().to_str().unwrap()), "{error}");
    assert!(session.read_log().contains("why-it-ended"));
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(session.log()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_window_that_never_says_hello_fails_the_launcher() {
    let session = Session::new("e", Some(&format!("{NOTE_CORE}\n{STAY}"))).unwrap();
    let start = Instant::now();
    let error = launch_failure(&session).unwrap();
    assert!(error.contains("the window is not ready"), "{error}");
    assert!(start.elapsed() >= control::FIRST_OPEN_WAIT);

    // Only the first window of a core starts cold.
    let start = Instant::now();
    let error = launch_failure(&session).unwrap();
    assert!(error.contains("the window is not ready"), "{error}");
    assert!(start.elapsed() < control::FIRST_OPEN_WAIT);
}

#[test]
fn a_first_window_may_take_longer_than_a_later_one() {
    let wait = control::OPEN_WAIT.as_secs() + 1;
    let hello = say_hello().unwrap();
    let window = format!("{NOTE_CORE}\nsleep {wait}\n{hello}\n{STAY}");
    let session = Session::new("g", Some(&window)).unwrap();
    let launched = session.launch().unwrap();
    let error = String::from_utf8_lossy(&launched.stderr);
    assert!(launched.status.success(), "{error}");
}

#[test]
fn a_new_core_empties_the_log_and_a_running_one_keeps_it() {
    let window = format!(
        "{NOTE_CORE}\necho this-core >&2\n{}\n{STAY}",
        say_hello().unwrap()
    );
    let session = Session::new("h", Some(&window)).unwrap();
    fs::create_dir_all(session.log().parent().unwrap()).unwrap();
    fs::write(session.log(), "an-older-core").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let private = fs::Permissions::from_mode(0o700);
    fs::set_permissions(session.log().parent().unwrap(), private).unwrap();
    for _ in 0..2 {
        assert!(session.launch().unwrap().status.success());
        assert_eq!(session.read_log(), "this-core\n");
    }
}

#[test]
fn a_log_that_will_not_open_stops_nothing_and_a_symlink_is_not_followed() {
    let window = format!("{NOTE_CORE}\n{}\n{STAY}", say_hello().unwrap());
    let session = Session::new("i", Some(&window)).unwrap();
    fs::create_dir_all(session.log().parent().unwrap()).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let private = fs::Permissions::from_mode(0o700);
    fs::set_permissions(session.log().parent().unwrap(), private).unwrap();
    let target = session.directory.join("target");
    fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, session.log()).unwrap();
    let launched = session.launch().unwrap();
    let error = String::from_utf8_lossy(&launched.stderr);
    assert!(launched.status.success(), "{error}");
    assert_eq!(session.read("target"), "keep");
}

/// Not a test: the panic of [`a_panic_prints_its_place_and_not_its_text`].
#[test]
fn panic_process() {
    if env::var_os(PANIC).is_some() {
        pinfold_cli::panics_name_no_text();
        let _ = std::panic::catch_unwind(|| std::panic::panic_any("text from a pane".to_owned()));
    }
}

#[test]
fn a_panic_prints_its_place_and_not_its_text() {
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", "panic_process", "--nocapture"])
        .env(PANIC, "yes")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("session.rs"), "{said}");
    assert!(!said.contains("from a pane"), "{said}");
}

#[test]
fn a_window_that_says_hello_is_a_success() {
    let window = format!("{NOTE_CORE}\n{}\n{STAY}", say_hello().unwrap());
    let session = Session::new("f", Some(&window)).unwrap();
    let launched = session.launch().unwrap();
    let error = String::from_utf8_lossy(&launched.stderr);
    assert!(launched.status.success(), "{error}");
}

#[test]
fn a_launcher_reports_a_core_with_no_window_and_that_core_exits_with_its_shell() {
    let session = Session::new("c", None).unwrap();
    let shell = session.directory.join("shell");
    fs::write(&shell, "#!/bin/sh\nexec sleep 2\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();

    let launched = session.launcher(shell.to_str().unwrap()).output().unwrap();
    assert!(!launched.status.success());
    let error = String::from_utf8_lossy(&launched.stderr);
    assert!(error.contains("no window can open"), "{error}");
    assert!(error.contains("cannot start pinfold-window"), "{error}");

    let path = pinfold_api::control::socket_path(&session.directory.join("run/pinfold"));
    let start = Instant::now();
    while std::os::unix::net::UnixStream::connect(&path).is_ok() {
        assert!(start.elapsed() < PATIENCE, "the core outlived its panes");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Whether a debugger can reach `pid`, as far as hardening decides that. On macOS a plain process
/// is refused whatever the target, so the answer is how.
#[cfg(target_os = "macos")]
fn debuggable(pid: u32) -> Result<bool> {
    let refused = pinfold_sys::probe_attach(pid).err();
    Ok(refused.is_none_or(|error| error.kind() != std::io::ErrorKind::ResourceBusy))
}

/// Whether a debugger can reach `pid`, which on Linux is what lets this read its `/proc` entries.
#[cfg(target_os = "linux")]
fn debuggable(pid: u32) -> Result<bool> {
    match fs::read(format!("/proc/{pid}/smaps_rollup")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Shell that notes itself and stays.
const NOTE_PANE: &str = "echo $$ > \"$PINFOLD_TEST_CLIENT/pane\"\nexec sleep 60";

#[test]
fn hardening_closes_the_core_and_leaves_its_panes_open() {
    let session = Session::new("j", None).unwrap();
    let mut core = session.start_core(&[], NOTE_PANE).unwrap();
    assert!(session.wait_for("pane"), "the pane's shell did not start");
    let pane: u32 = session.read("pane").trim().parse().unwrap();

    assert!(!debuggable(core.id()).unwrap(), "the core is open");
    assert!(
        debuggable(pane).unwrap(),
        "a pane took the core's hardening"
    );

    core.kill().unwrap();
    core.wait().unwrap();

    // The same probe on the same binary without hardening, or the above proves nothing.
    let session = Session::new("k", None).unwrap();
    let mut open = session.start_core(&["--idle", "1"], NOTE_PANE).unwrap();
    assert!(session.wait_for("pane"), "the pane's shell did not start");
    assert!(debuggable(open.id()).unwrap(), "the probe cannot tell");
    open.kill().unwrap();
    open.wait().unwrap();
}

/// Not a test: [`hardening_twice_is_harmless`] runs this, so that this binary stays debuggable.
#[test]
fn harden_process() {
    if env::var_os(HARDEN).is_some() {
        pinfold_sys::harden().unwrap();
        pinfold_sys::harden().unwrap();
    }
}

#[test]
fn ctl_lists_the_sessions_panes_inside_and_outside_a_pane_and_not_without_a_terminal() {
    let session = Session::new("l", None).unwrap();
    let ctl = format!("'{}' ctl list", session.directory.join("pinfold").display());
    let to = |name: &str| format!("> \"$PINFOLD_TEST_CLIENT/{name}\"");
    let inside = format!(
        "{ctl} {}; {ctl} --json {}; exec sleep 30",
        to("in"),
        to("json")
    );
    let mut core = session.start_core(&["--idle", "1"], &inside).unwrap();
    assert!(session.wait_for("json"), "{}", session.read_log());

    // In its pane, the caller is pane 0 and sees itself marked.
    let text = session.read("in");
    assert!(
        text.lines().nth(1).unwrap().starts_with("0*    idle"),
        "{text}"
    );
    let list: serde_json::Value = serde_json::from_str(&session.read("json")).unwrap();
    let pane = &list["panes"][0];
    assert_eq!((&pane["id"], &pane["status"]), (&0.into(), &"idle".into()));
    assert!(
        pane["command"].as_str().unwrap().ends_with("/shell"),
        "{list}"
    );
    assert_eq!(list["panes"].as_array().unwrap().len(), 1);

    let outside = |name: &str| {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &format!("{ctl} {} 2>&1", to(name))])
            .env("XDG_RUNTIME_DIR", session.directory.join("run"))
            .env(CLIENT, &session.directory);
        command
    };
    // In a terminal that is no pane, it sees the same panes, none of them its own.
    let _terminal = pinfold_sys::pty::Pty::spawn(outside("out"), 24, 80).unwrap();
    assert!(session.wait_for("out"));
    let text = session.read("out");
    assert!(
        text.lines().nth(1).unwrap().starts_with("0     idle"),
        "{text}"
    );

    // With no terminal at all it is unidentified and refused.
    let detached = pinfold_sys::spawn_detached(outside("none"), Stdio::null());
    assert!(!detached.unwrap().wait().unwrap().success());
    let refused = session.read("none");
    assert!(
        refused.contains("needs a caller in a terminal"),
        "{refused}"
    );
    core.kill().unwrap();
    core.wait().unwrap();
}

#[test]
fn a_pane_can_send_to_itself_and_cross_pane_send_needs_a_window() {
    let session = Session::new("s", None).unwrap();
    let ctl = format!("'{}' ctl", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    // Both panes send to pane 0: pane 0 reads what it typed, and pane 1 is refused.
    let shell = format!(
        "if {ctl} send 0 'typed\n' 2> {client}/e$$; then read line; echo \"$line\" > {client}/got; \
         else mv {client}/e$$ {client}/refused; fi; exec sleep 30"
    );
    let mut core = session.start_core(&["--idle", "2"], &shell).unwrap();
    assert!(session.wait_for("got"), "{}", session.read_log());
    assert!(session.wait_for("refused"));
    assert_eq!(session.read("got"), "typed\n");
    let refused = session.read("refused");
    assert!(
        refused.contains("needs an open PinFold window"),
        "{refused}"
    );
    core.kill().unwrap();
    core.wait().unwrap();
}

/// Not a test: each pane of [`a_pane_reports_its_own_status_and_not_another_panes`]. It reports
/// pane 0's status, lists the panes, and notes both answers in a file named for its own pane.
#[test]
fn status_process() {
    let (Some(directory), Some(_)) = (env::var_os(CLIENT), env::var_os(STATUS)) else {
        return;
    };
    let report = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "pane.status",
        "params": {"pane": 0, "state": "needs-you", "message": "approve?\u{1b}[2J"}});
    let mut lines = api(&[report]).unwrap();
    let hello = next(&mut lines, || Ok(())).unwrap();
    let reported = next(&mut lines, || Ok(())).unwrap();
    // A refusal ends its connection, so each list takes another, until pane 0's report shows.
    let list = serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "pane.list"});
    let start = Instant::now();
    let listed = loop {
        let mut lines = api(std::slice::from_ref(&list)).unwrap();
        next(&mut lines, || Ok(())).unwrap();
        let listed = next(&mut lines, || Ok(())).unwrap();
        let shown = listed["result"]["panes"][0]["source"] == "reported";
        if shown || start.elapsed() > PATIENCE {
            break listed;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let name = format!("pane{}", hello["result"]["pane"]);
    let heard = format!("{reported}\n{listed}\n");
    fs::write(Path::new(&directory).join(name), heard).unwrap();
}

#[test]
fn a_pane_reports_its_own_status_and_not_another_panes() {
    let session = Session::new("st", None).unwrap();
    let exe = env::current_exe().unwrap();
    let shell = format!(
        "{STATUS}=1 '{}' --exact status_process; exec sleep 30",
        exe.display()
    );
    let mut core = session.start_core(&["--idle", "2"], &shell).unwrap();
    assert!(session.wait_for("pane0"), "{}", session.read_log());
    assert!(session.wait_for("pane1"), "{}", session.read_log());
    let answers = |name| -> Vec<serde_json::Value> {
        let text = session.read(name);
        text.lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let own = answers("pane0");
    assert_eq!(own[0]["result"], serde_json::json!({}));
    let other = answers("pane1");
    let refused = other[0]["error"]["message"].to_string();
    assert!(refused.contains("only the caller's own pane"), "{refused}");
    // Pane 0 lists after its report, which is labelled as one, its message cleaned (T4).
    let listed = &own[1]["result"]["panes"][0];
    let shown = (&listed["status"], &listed["source"], &listed["message"]);
    assert_eq!(
        shown,
        (
            &"needs-you".into(),
            &"reported".into(),
            &"approve?[2J".into()
        )
    );
    // Pane 1 sees pane 0's state and source, and not its message.
    let listed = &other[1]["result"]["panes"][0];
    let shown = (&listed["status"], &listed["source"], &listed["message"]);
    assert_eq!(
        shown,
        (
            &"needs-you".into(),
            &"reported".into(),
            &serde_json::Value::Null
        )
    );
    core.kill().unwrap();
    core.wait().unwrap();
}

#[test]
fn hardening_twice_is_harmless() {
    let status = Command::new(env::current_exe().unwrap())
        .args(["--exact", "harden_process"])
        .env(HARDEN, "1")
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

/// Says hello version 2 on a new connection and sends `lines`.
fn api(lines: &[serde_json::Value]) -> Result<std::io::Lines<std::io::BufReader<UnixStream>>> {
    use std::io::{BufRead, BufReader, Write};
    let path = control::socket_path(&control::runtime_dir());
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let hello = r#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":2}}"#;
    writeln!(stream, "{hello}")?;
    for line in lines {
        writeln!(stream, "{line}")?;
    }
    Ok(BufReader::new(stream).lines())
}

/// The next whole line, waiting out read timeouts, after `tick` on each.
fn next(
    lines: &mut std::io::Lines<std::io::BufReader<UnixStream>>,
    mut tick: impl FnMut() -> Result,
) -> Result<serde_json::Value> {
    let start = Instant::now();
    while start.elapsed() < PATIENCE {
        match lines.next() {
            Some(Ok(line)) => return Ok(serde_json::from_str(&line)?),
            Some(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock => tick()?,
            _ => return Err("the core hung up".into()),
        }
    }
    Err("the core said nothing".into())
}

/// Not a test: the first pane of [`a_caller_waits_on_its_pane_and_hears_panes_open_and_close`].
/// It subscribes, waits on itself, and notes what it hears.
#[test]
fn api_process() {
    let Some(directory) = env::var_os(API) else {
        return;
    };
    let directory = Path::new(&directory);
    let subscribe = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "events.subscribe",
        "params": {"events": ["pane.opened", "pane.closed"]}});
    let mut events = api(&[subscribe]).unwrap();
    for _ in 0..2 {
        next(&mut events, || Ok(())).unwrap();
    }
    fs::write(directory.join("subscribed"), "").unwrap();

    let wait = |id, until, timeout_ms| {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "pane.wait",
            "params": {"pane": "self", "until": until, "timeout_ms": timeout_ms}})
    };
    let mut waits = api(&[wait(2, "prompt", 300), wait(3, "exit", 20_000)]).unwrap();
    let mut heard = String::new();
    for _ in 0..2 {
        writeln!(heard, "{}", next(&mut waits, || Ok(())).unwrap()).unwrap();
    }
    // Marked until heard, as nothing says when the core has parked the wait.
    let mut tty = fs::OpenOptions::new().write(true).open("/dev/tty").unwrap();
    let mut mark = || Ok(std::io::Write::write_all(&mut tty, b"\x1b]133;D;7\x07")?);
    writeln!(heard, "{}", next(&mut waits, &mut mark).unwrap()).unwrap();
    fs::write(directory.join("waited"), heard).unwrap();

    let (mut opened, mut closed) = (None, None);
    while closed.is_none() {
        let event = next(&mut events, || Ok(())).unwrap();
        let params = event["params"].clone();
        match params["event"].as_str() {
            Some("pane.opened") => opened = Some(params),
            _ => closed = Some(params),
        }
    }
    let heard = format!(
        "{}\n{}\n",
        opened.unwrap_or_default(),
        closed.unwrap_or_default()
    );
    fs::write(directory.join("events"), heard).unwrap();
}

/// Not a test: the window of [`a_caller_waits_on_its_pane_and_hears_panes_open_and_close`],
/// which opens a tab once the first pane has subscribed.
#[test]
fn tab_process() {
    let Some(directory) = env::var_os(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    fs::write(
        directory.join("core"),
        std::os::unix::process::parent_id().to_string(),
    )
    .unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    let subscribed = |_: &mut Link| directory.join("subscribed").exists();
    until(&mut link, subscribed).unwrap();
    assert!(link.send(Request::NewTab));
    until(&mut link, |_| directory.join("events").exists()).unwrap();
}

#[test]
fn a_caller_waits_on_its_pane_and_hears_panes_open_and_close() {
    let exe = env::current_exe().unwrap();
    let window = format!("exec '{}' --exact tab_process", exe.display());
    let session = Session::new("w", Some(&window)).unwrap();
    // The first pane is the caller; the tab's pane ends a moment after it opens.
    let shell = session.directory.join("shell");
    let script = format!(
        "#!/bin/sh\nif mkdir \"$PINFOLD_TEST_CLIENT/first\"; then exec '{}' --exact api_process; fi\nsleep 1\n",
        exe.display()
    );
    fs::write(&shell, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let mut launcher = session.launcher(shell.to_str().unwrap());
    let launched = launcher.env(API, &session.directory).output().unwrap();
    assert!(launched.status.success(), "{launched:?}");

    assert!(session.wait_for("waited"), "{}", session.read_log());
    let waited: Vec<serde_json::Value> = session
        .read("waited")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(waited[0]["result"]["version"], 2);
    let methods = waited[0]["result"]["methods"].to_string();
    assert!(methods.contains("pane.wait") && methods.contains("events.subscribe"));
    assert_eq!(
        waited[1]["result"],
        serde_json::json!({"outcome": "timeout"})
    );
    let exited = serde_json::json!({"outcome": "exit", "exit": 7});
    assert_eq!(waited[2]["result"], exited);

    assert!(session.wait_for("events"), "{}", session.read_log());
    let events = session.read("events");
    let mut events = events
        .lines()
        .map(|line| serde_json::from_str(line).unwrap());
    let opened = serde_json::json!({"event": "pane.opened", "pane": 1, "status": "idle",
        "source": "marks", "exit": null});
    assert_eq!(events.next(), Some(opened));
    let closed = serde_json::json!({"event": "pane.closed", "pane": 1});
    assert_eq!(events.next(), Some(closed));
}

/// Not a test: pane 0 asks to send twice. It prints answer-looking bytes and tries a JSON-RPC
/// answer while the first call is parked, then waits for an outside terminal to revoke its grant.
#[test]
fn approval_api_process() {
    let Some(directory) = env::var_os(API) else {
        return;
    };
    let directory = Path::new(&directory);
    while !directory.join("second").exists() {
        thread::sleep(Duration::from_millis(10));
    }
    let send = |id, text| {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "pane.send",
            "params": {"pane": 1, "text": text}})
    };
    let mut first = api(&[send(2, "approved\n")]).unwrap();
    assert_eq!(next(&mut first, || Ok(())).unwrap()["result"]["pane"], 0);

    let mut tty = fs::OpenOptions::new().write(true).open("/dev/tty").unwrap();
    std::io::Write::write_all(&mut tty, b"y a n Escape\n").unwrap();
    let socket = UnixStream::connect(control::socket_path(&control::runtime_dir())).unwrap();
    let api_answer = control::ask(
        &socket,
        "approval.answer",
        &serde_json::json!({"id": 1, "answer": "allow"}),
    );
    assert!(api_answer.is_err());
    let socket = UnixStream::connect(control::socket_path(&control::runtime_dir())).unwrap();
    let pane_admin = control::ask(&socket, "grants.list", &serde_json::json!({}));
    assert!(pane_admin.is_err());
    fs::write(directory.join("attempted"), "yes").unwrap();
    let blocked = matches!(
        first.next(),
        Some(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock
    );
    assert!(blocked, "pane output or the API answered the prompt");
    fs::write(directory.join("still-parked"), "yes").unwrap();

    let allowed = next(&mut first, || Ok(())).unwrap();
    assert_eq!(allowed["result"], serde_json::json!({}));
    fs::write(directory.join("first-allowed"), "yes").unwrap();
    while !directory.join("revoked").exists() {
        thread::sleep(Duration::from_millis(10));
    }
    let mut second = api(&[send(2, "must-not-arrive\n")]).unwrap();
    let _ = next(&mut second, || Ok(())).unwrap();
    let denied = next(&mut second, || Ok(())).unwrap();
    assert!(
        denied["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("denied"))
    );
    fs::write(directory.join("second-denied"), "yes").unwrap();

    let read = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "pane.read",
        "params": {"pane": 1}});
    let mut reading = api(&[read]).unwrap();
    let _ = next(&mut reading, || Ok(())).unwrap();
    let read = next(&mut reading, || Ok(())).unwrap();
    let text = read["result"]["text"].as_str().unwrap();
    assert!(text.contains("«redacted:password»"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    fs::write(directory.join("redacted"), "yes").unwrap();
}

/// Not a test: the trusted inherited link sees both prompts and answers them with key meanings.
#[test]
fn approval_window_process() {
    let Some(directory) = env::var_os(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    fs::write(
        directory.join("core"),
        std::os::unix::process::parent_id().to_string(),
    )
    .unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    until(&mut link, |link| link.is_greeted()).unwrap();
    assert!(link.send(Request::NewTab));
    let mut first = None;
    until(&mut link, |link| {
        first = link.take_approval();
        first.is_some()
    })
    .unwrap();
    let first = first.unwrap();
    assert_eq!(
        (first.caller, first.target, first.action),
        (0, 1, ApprovalAction::Send)
    );
    assert_eq!(first.texts, ["approved\n"]);
    fs::write(directory.join("prompt"), format!("{}", first.id)).unwrap();
    until(&mut link, |_| {
        directory.join("attempted").exists() && directory.join("still-parked").exists()
    })
    .unwrap();
    assert!(link.send(Request::ApprovalAnswer {
        id: first.id,
        answer: ApprovalAnswer::AllowForThirtyMinutes,
    }));

    let mut second = None;
    until(&mut link, |link| {
        second = link.take_approval();
        second.is_some()
    })
    .unwrap();
    let second = second.unwrap();
    assert_eq!(second.texts, ["must-not-arrive\n"]);
    assert!(link.send(Request::ApprovalAnswer {
        id: second.id,
        answer: ApprovalAnswer::Deny,
    }));
    until(&mut link, |_| directory.join("second-denied").exists()).unwrap();

    let mut read = None;
    until(&mut link, |link| {
        read = link.take_approval();
        read.is_some()
    })
    .unwrap();
    let read = read.unwrap();
    assert_eq!(read.action, ApprovalAction::Read);
    assert!(read.texts.is_empty());
    assert!(link.send(Request::ApprovalAnswer {
        id: read.id,
        answer: ApprovalAnswer::AllowOnce,
    }));
    until(&mut link, |_| directory.join("redacted").exists()).unwrap();
}

#[test]
fn only_the_window_answers_cross_pane_actions_and_ctl_revokes_grants() {
    let exe = env::current_exe().unwrap();
    let window = format!("exec '{}' --exact approval_window_process", exe.display());
    let session = Session::new("approval", Some(&window)).unwrap();
    let shell = session.directory.join("approval-shell");
    let script = format!(
        "#!/bin/sh\nif mkdir \"$PINFOLD_TEST_CLIENT/first\" 2>/dev/null; then \
         exec '{}' --exact approval_api_process; else echo yes > \"$PINFOLD_TEST_CLIENT/second\"; \
         echo 'password=hunter2'; \
         read line; echo \"$line\" > \"$PINFOLD_TEST_CLIENT/received\"; exec sleep 30; fi\n",
        exe.display()
    );
    fs::write(&shell, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let launched = session
        .launcher(shell.to_str().unwrap())
        .env(API, &session.directory)
        .output()
        .unwrap();
    assert!(launched.status.success(), "{launched:?}");
    assert!(session.wait_for("first-allowed"), "{}", session.read_log());
    // The target's shell writes this after its own read, a moment after the send returns.
    assert!(session.wait_for("received"), "{}", session.read_log());
    assert_eq!(session.read("received"), "approved\n");

    let id = session.read("prompt").trim().to_owned();
    let pinfold = session.directory.join("pinfold");
    let command = format!(
        "'{}' ctl grants --json > \"$PINFOLD_TEST_CLIENT/grants\"; \
         '{}' ctl grants revoke {id}; \
         '{}' ctl grants --json > \"$PINFOLD_TEST_CLIENT/after\"; \
         echo yes > \"$PINFOLD_TEST_CLIENT/revoked\"",
        pinfold.display(),
        pinfold.display(),
        pinfold.display()
    );
    let mut outside = Command::new("/bin/sh");
    outside
        .args(["-c", &command])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(CLIENT, &session.directory);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    assert!(session.wait_for("revoked"), "{}", session.read_log());
    let listed: serde_json::Value = serde_json::from_str(&session.read("grants")).unwrap();
    assert_eq!(listed["grants"][0]["id"].to_string(), id);
    let after: serde_json::Value = serde_json::from_str(&session.read("after")).unwrap();
    assert_eq!(after["grants"], serde_json::json!([]));
    assert!(session.wait_for("second-denied"), "{}", session.read_log());
    assert!(session.wait_for("redacted"), "{}", session.read_log());
    assert!(session.read("received").contains("approved"));
}

/// Names the directory of a run, in the pane that spawns others.
const SPAWN: &str = "PINFOLD_TEST_SPAWN";

fn call(id: u32, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Asks once on a new connection, as a refusal closes it, and returns the reply.
fn ask(method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let mut lines = api(&[call(2, method, params)])?;
    next(&mut lines, || Ok(()))?;
    next(&mut lines, || Ok(()))
}

/// Not a test: the first pane of [`a_pane_spawns_children_and_reaches_only_them`]. It spawns,
/// sends, reads and closes, and notes each reply.
#[test]
fn spawn_process() {
    let Some(directory) = env::var_os(SPAWN) else {
        return;
    };
    let directory = Path::new(&directory);
    // Started with no terminal, the caller is unidentified.
    if directory.join("spawned").exists() {
        let refused = ask("pane.spawn", serde_json::json!({"where": "tab"})).unwrap();
        fs::write(directory.join("outside"), refused.to_string()).unwrap();
        return;
    }
    let mut lines = api(&[
        call(2, "pane.spawn", serde_json::json!({"where": "split-right"})),
        call(
            3,
            "pane.spawn",
            serde_json::json!({"where": "tab", "cwd": "/"}),
        ),
        call(
            4,
            "pane.send",
            serde_json::json!({"pane": 1, "text": "typed\n"}),
        ),
        call(5, "pane.close", serde_json::json!({"pane": 2})),
    ])
    .unwrap();
    let mut heard = String::new();
    for _ in 0..5 {
        writeln!(heard, "{}", next(&mut lines, || Ok(())).unwrap()).unwrap();
    }
    // A closed child is out of reach, and the caller cannot close itself.
    let read = ask("pane.read", serde_json::json!({"pane": 2})).unwrap();
    let close = ask("pane.close", serde_json::json!({"pane": 0})).unwrap();
    writeln!(heard, "{read}\n{close}").unwrap();
    // The tty echoes the line and `cat` prints it again.
    let start = Instant::now();
    while start.elapsed() < PATIENCE {
        let read = ask("pane.read", serde_json::json!({"pane": 1})).unwrap();
        let text = read["result"]["text"].as_str().unwrap_or_default();
        if text.matches("typed").count() >= 2 {
            writeln!(heard, "read").unwrap();
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    fs::write(directory.join("spawned"), heard).unwrap();
}

#[test]
fn a_pane_spawns_children_and_reaches_only_them() {
    let session = Session::new("p", None).unwrap();
    let exe = env::current_exe().unwrap();
    let shell = format!(
        "if mkdir \"$PINFOLD_TEST_CLIENT/first\"; then {SPAWN}=\"$PINFOLD_TEST_CLIENT\" exec '{}' \
         --exact spawn_process; fi; exec cat",
        exe.display()
    );
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("spawned"), "{}", session.read_log());
    let heard = session.read("spawned");
    let replies: Vec<serde_json::Value> = heard
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let methods = replies[0]["result"]["methods"].to_string();
    assert!(methods.contains("pane.spawn") && methods.contains("pane.close"));
    assert_eq!(replies[1]["result"], serde_json::json!({"pane": 1}));
    assert_eq!(replies[2]["result"], serde_json::json!({"pane": 2}));
    assert_eq!(replies[3]["result"], serde_json::json!({}));
    assert_eq!(replies[4]["result"], serde_json::json!({}));
    assert!(replies[5]["error"].is_object(), "{heard}");
    assert!(replies[6]["error"].is_object(), "{heard}");
    assert!(heard.ends_with("read\n"), "{heard}");

    let mut outside = Command::new(&exe);
    outside
        .args(["--exact", "spawn_process"])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(SPAWN, &session.directory);
    let detached = pinfold_sys::spawn_detached(outside, Stdio::null());
    assert!(detached.unwrap().wait().unwrap().success());
    let refused = session.read("outside");
    assert!(refused.contains("needs a caller in a pane"), "{refused}");
    core.kill().unwrap();
    core.wait().unwrap();
}

#[test]
fn ctl_wait_prints_how_a_wait_ended_and_exits_0_1_or_2() {
    let session = Session::new("cw", None).unwrap();
    let ctl = format!("'{}' ctl wait", session.directory.join("pinfold").display());
    // Each line notes what `ctl wait` printed and its exit status. The pane prints in the
    // background over and over, so a wait parked late still sees a later line or mark.
    let shell = format!(
        r#"exec 3> "$PINFOLD_TEST_CLIENT/x$$"
note() {{ "$@" >&3 2>/dev/null; echo "status $?" >&3; }}
note {ctl} self --until output --timeout 1
note {ctl} self --until idle --idle-ms 100 --timeout 20
(while :; do echo ready-42; sleep 0.2; done) & loud=$!
note {ctl} self --until match --pattern 'ready-\d+$' --timeout 20
kill $loud
(while :; do printf '\033]133;D;3\007'; sleep 0.2; done) & loud=$!
note {ctl} self --until exit --timeout 20
kill $loud
note {ctl} self --until match --pattern '(' --timeout 1
note {ctl} 9 --until exit --timeout 1
note {ctl} self --until exit
mv "$PINFOLD_TEST_CLIENT/x$$" "$PINFOLD_TEST_CLIENT/waited"
exec sleep 30"#
    );
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("waited"), "{}", session.read_log());
    let expected = "timeout\nstatus 1\nidle\nstatus 0\nmatch ready-42\nstatus 0\nexit 3\n\
                    status 0\nstatus 2\nstatus 2\nstatus 2\n";
    assert_eq!(session.read("waited"), expected);
    core.kill().unwrap();
    core.wait().unwrap();
}

/// Names the directory of a run, in the pane that subscribes to output.
const OUTPUT: &str = "PINFOLD_TEST_OUTPUT";

/// Not a test: the first pane of [`output_events_and_match_waits_reach_only_the_callers_pane`].
/// It subscribes to output, spawns a child, and notes what it hears from whom.
#[test]
fn output_process() {
    let Some(directory) = env::var_os(OUTPUT) else {
        return;
    };
    let subscribe = call(
        2,
        "events.subscribe",
        serde_json::json!({"events": ["pane.output"]}),
    );
    let mut events = api(&[subscribe]).unwrap();
    let own = next(&mut events, || Ok(())).unwrap()["result"]["pane"].clone();
    next(&mut events, || Ok(())).unwrap();
    let spawned = ask("pane.spawn", serde_json::json!({"where": "split-right"})).unwrap();
    let child = spawned["result"]["pane"].clone();
    let wait =
        serde_json::json!({"pane": child, "until": "match", "pattern": "x", "timeout_ms": 1});
    let refused = ask("pane.wait", wait).unwrap();
    let text = "child password=hunter2\n";
    let send = serde_json::json!({"pane": child, "text": text});
    ask("pane.send", send).unwrap();
    // The child has printed before the caller does, so its events would come first.
    let start = Instant::now();
    while start.elapsed() < PATIENCE {
        let read = ask("pane.read", serde_json::json!({"pane": child})).unwrap();
        if read["result"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("child")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut tty = fs::OpenOptions::new().write(true).open("/dev/tty").unwrap();
    std::io::Write::write_all(&mut tty, b"own password=hunter2\n").unwrap();
    let mut heard = format!("{own}\n{refused}\n");
    while !heard.contains("own password=hunter2") {
        let event = next(&mut events, || Ok(())).unwrap();
        writeln!(heard, "{}", event["params"]).unwrap();
    }
    fs::write(Path::new(&directory).join("heard"), heard).unwrap();
}

/// Not a test: a terminal outside any pane in
/// [`output_events_and_match_waits_reach_only_the_callers_pane`], which asks for output.
#[test]
fn outside_output_process() {
    let Some(directory) = env::var_os(OUTPUT) else {
        return;
    };
    let events = serde_json::json!({"events": ["pane.output"]});
    let refused = ask("events.subscribe", events).unwrap();
    fs::write(Path::new(&directory).join("outside"), refused.to_string()).unwrap();
}

#[test]
fn output_events_and_match_waits_reach_only_the_callers_pane() {
    let session = Session::new("o", None).unwrap();
    let exe = env::current_exe().unwrap();
    // The second pane is another of the person's, printing all along.
    let shell = format!(
        "if mkdir \"$PINFOLD_TEST_CLIENT/first\"; then {OUTPUT}=\"$PINFOLD_TEST_CLIENT\" exec '{}' \
         --exact output_process; elif mkdir \"$PINFOLD_TEST_CLIENT/second\"; then \
         while :; do echo other password=hunter2; sleep 0.1; done; fi; exec cat",
        exe.display()
    );
    let mut core = session.start_core(&["--idle", "2"], &shell).unwrap();
    assert!(session.wait_for("heard"), "{}", session.read_log());
    let heard = session.read("heard");
    let mut lines = heard.lines();
    let own: u64 = lines.next().unwrap().parse().unwrap();
    let refused = lines.next().unwrap();
    assert!(
        refused.contains("a match wait needs the caller's own pane"),
        "{heard}"
    );
    for line in lines {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["event"], "pane.output", "{heard}");
        assert_eq!(event["pane"], own, "another pane was heard: {heard}");
    }

    // A terminal that is no pane has no own pane to hear, so it is refused.
    let mut outside = Command::new(&exe);
    outside
        .args(["--exact", "outside_output_process"])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(OUTPUT, &session.directory);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    assert!(session.wait_for("outside"), "{}", session.read_log());
    let refused = session.read("outside");
    assert!(
        refused.contains("pane.output needs the caller's own pane"),
        "{refused}"
    );
    core.kill().unwrap();
    core.wait().unwrap();
}

#[test]
fn mcp_lists_tools_calls_the_core_as_its_pane_and_reports_a_refusal_as_a_tool_error() {
    let session = Session::new("m", None).unwrap();
    let call = |id: u32, name: &str, arguments: serde_json::Value| {
        let params = serde_json::json!({"name": name, "arguments": arguments});
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
    };
    let requests = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        call(2, "pane_list_v1", serde_json::json!({})),
        call(3, "pane_read_v1", serde_json::json!({"pane": 1})),
        serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
    ];
    let input: String = requests
        .iter()
        .map(|request| format!("{request}\n"))
        .collect();
    fs::write(session.directory.join("requests"), input).unwrap();
    let mcp = format!("'{}' mcp", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    let shell = format!(
        "{mcp} < {client}/requests > {client}/o && mv {client}/o {client}/out; exec sleep 30"
    );
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("out"), "{}", session.read_log());

    let out = session.read("out");
    let replies: Vec<serde_json::Value> = out
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 4, "{out}");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "pinfold");
    let listed = &replies[1]["result"];
    assert_eq!(listed["isError"], false, "{out}");
    let list: serde_json::Value =
        serde_json::from_str(listed["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(list["caller_pane"], 0, "{list}");
    assert_eq!(list["panes"][0]["id"], 0, "{list}");
    let refused = &replies[2]["result"];
    assert_eq!(refused["isError"], true, "{out}");
    let message = refused["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("no such pane"), "{out}");
    let names: Vec<_> = replies[3]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "pane_list_v1",
            "pane_read_v1",
            "pane_last_output_v1",
            "pane_send_v1",
            "pane_spawn_v1",
            "pane_close_v1",
            "pane_wait_v1",
            "events_next_v1",
        ]
    );
    core.kill().unwrap();
    core.wait().unwrap();
}

#[test]
fn up_opens_a_workspace_as_shells_once_and_then_attaches_to_it() {
    let window = format!("{NOTE_CORE}\n{}\n{STAY}", say_hello().unwrap());
    let session = Session::new("u", Some(&window)).unwrap();
    let project = session.directory.join("project");
    for directory in ["a", "b", ".pinfold"] {
        fs::create_dir_all(project.join(directory)).unwrap();
    }
    let layout = "[[tabs]]\n[[tabs.panes]]\ncwd = 'a'\nrun = 'make'\n\
                  [[tabs]]\n[[tabs.panes]]\ncwd = 'b'\n";
    fs::write(project.join(".pinfold/layout.toml"), layout).unwrap();
    // Each shell says where it is, as OSC 7, and stays.
    let shell = session.directory.join("shell");
    let script = "#!/bin/sh\nprintf '\\033]7;file://%s\\007' \"$(pwd -P)\"\nexec sleep 60\n";
    fs::write(&shell, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let pinfold = session.directory.join("pinfold");
    // Runs `arguments` in a terminal that is no pane, as a person would, and returns its output.
    let in_terminal = |arguments: &str| {
        let out = session.directory.join("out");
        let script = format!(
            "'{}' {arguments} > out.tmp 2>&1; echo \"exit $?\" >> out.tmp; mv out.tmp '{}'",
            pinfold.display(),
            out.display()
        );
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .current_dir(&project)
            .env("XDG_RUNTIME_DIR", session.directory.join("run"))
            .env("SHELL", &shell)
            .env(CLIENT, &session.directory);
        let _terminal = pinfold_sys::pty::Pty::spawn(command, 24, 80).unwrap();
        assert!(session.wait_for("out"), "{}", session.read_log());
        let text = session.read("out");
        fs::remove_file(&out).unwrap();
        text
    };
    let cwds = || {
        let start = Instant::now();
        loop {
            let text = in_terminal("ctl list --json");
            let list: serde_json::Value = serde_json::from_str(text.lines().next().unwrap())
                .unwrap_or_else(|_| panic!("{text}"));
            let panes = list["panes"].as_array().unwrap().iter();
            let cwds: Vec<String> = panes
                .map(|pane| pane["cwd"].as_str().unwrap_or_default().to_owned())
                .collect();
            if cwds.iter().all(|cwd| !cwd.is_empty()) || start.elapsed() > PATIENCE {
                return cwds;
            }
            thread::sleep(Duration::from_millis(50));
        }
    };
    let canonical = |name: &str| project.join(name).canonicalize().unwrap();
    let (a, b) = (canonical("a"), canonical("b"));

    let said = in_terminal("up");
    let waits = "pinfold: 1 command waits for workspace trust (not yet available)\nexit 0\n";
    assert_eq!(said, waits);
    // The core's first shell, then a tab in each cwd.
    let opened = cwds();
    assert_eq!(opened.len(), 3, "{opened:?}");
    assert_eq!(opened[1..], [a.to_str().unwrap(), b.to_str().unwrap()]);

    // A second `pinfold up` attaches and opens nothing.
    assert_eq!(in_terminal("up"), waits);
    assert_eq!(cwds(), opened);
}

#[test]
fn mcp_waits_for_the_next_event_and_times_out_with_none() {
    let session = Session::new("x", None).unwrap();
    let call = |id: u32, name: &str, arguments: serde_json::Value| {
        let params = serde_json::json!({"name": name, "arguments": arguments});
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
    };
    // The spawned tab's shell ends two seconds after it opens, while the next call waits.
    let requests = [
        call(1, "pane_spawn_v1", serde_json::json!({"where": "tab"})),
        call(
            2,
            "events_next_v1",
            serde_json::json!({"events": ["pane.closed"], "timeout_ms": 60_000}),
        ),
        call(
            3,
            "events_next_v1",
            serde_json::json!({"events": ["pane.opened"], "timeout_ms": 100}),
        ),
    ];
    let input: String = requests
        .iter()
        .map(|request| format!("{request}\n"))
        .collect();
    fs::write(session.directory.join("requests"), input).unwrap();
    let mcp = format!("'{}' mcp", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    let shell = format!(
        "if mkdir {client}/first; then {mcp} < {client}/requests > {client}/o && \
         mv {client}/o {client}/out; exec sleep 30; fi; sleep 2"
    );
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("out"), "{}", session.read_log());

    let out = session.read("out");
    let texts: Vec<serde_json::Value> = out
        .lines()
        .map(|line| {
            let reply: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(reply["result"]["isError"], false, "{out}");
            serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(texts.len(), 3, "{out}");
    assert_eq!(texts[0], serde_json::json!({"pane": 1}), "{out}");
    let closed = serde_json::json!({"event": "pane.closed", "pane": 1});
    assert_eq!(texts[1], closed, "{out}");
    assert_eq!(texts[2], serde_json::json!({"timeout": true}), "{out}");
    core.kill().unwrap();
    core.wait().unwrap();
}

/// Runs a core whose first pane spawns a second running `sleep <number>` and sends to it
/// through `pinfold mcp`, with `protect` as `api.protect` if any, and returns the send's reply.
fn send_to_a_sleeping_child(
    name: &str,
    number: &str,
    protect: Option<&str>,
) -> Result<serde_json::Value> {
    let session = Session::new(name, None)?;
    if let Some(protect) = protect {
        fs::create_dir_all(session.directory.join("config/pinfold"))?;
        let toml = format!("[api]\nprotect = [\"{protect}\"]\n");
        fs::write(session.directory.join("config/pinfold/pinfold.toml"), toml)?;
    }
    let call = |id: u32, name: &str, arguments: serde_json::Value| {
        let params = serde_json::json!({"name": name, "arguments": arguments});
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
    };
    let initialize =
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}});
    let spawn = call(2, "pane_spawn_v1", serde_json::json!({"where": "tab"}));
    let send = call(
        3,
        "pane_send_v1",
        serde_json::json!({"pane": 1, "text": "typed"}),
    );
    let mcp = format!("'{}' mcp", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    // The send waits until the child's shell has become `sleep`, so the check sees it.
    let shell = format!(
        "if mkdir {client}/first; then {{ echo '{initialize}'; echo '{spawn}'; \
         until pgrep -x -f 'sleep {number}' > /dev/null; do sleep 0.05; done; \
         echo '{send}'; }} | {mcp} > {client}/o && mv {client}/o {client}/out; \
         exec sleep 30; fi; exec sleep {number}"
    );
    let mut core = session.start_core(&["--idle", "1"], &shell)?;
    assert!(session.wait_for("out"), "{}", session.read_log());
    core.kill()?;
    core.wait()?;
    let out = session.read("out");
    let replies: Vec<serde_json::Value> = out
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(replies.len(), 3, "{out}");
    assert_eq!(replies[1]["result"]["isError"], false, "{out}");
    Ok(replies[2]["result"].clone())
}

#[test]
fn a_configured_protect_pattern_refuses_a_send_to_a_pane_that_runs_it() {
    // A number of this run's own, so another run's `sleep` is not mistaken for the child.
    let pid = std::process::id();
    let allowed = send_to_a_sleeping_child("ua", &format!("{pid}1"), None).unwrap();
    assert_eq!(allowed["isError"], false, "{allowed}");
    let number = format!("{pid}2");
    let refused =
        send_to_a_sleeping_child("ub", &number, Some(&format!("sleep {number}"))).unwrap();
    assert_eq!(refused["isError"], true, "{refused}");
    let message = refused["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("the pane is protected: rule"), "{refused}");
}
