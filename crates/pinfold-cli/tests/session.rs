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
use pinfold_proto::link::{ApprovalAction, ApprovalAnswer, OLDEST, Request, SplitWay, VERSION};
use pinfold_sys::socket;
use pinfold_ui::link::{Approval, Disconnect, Link};

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
/// Names the directory of a run, in the terminal that opens a workspace.
const WORKSPACE: &str = "PINFOLD_TEST_WORKSPACE";
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
            // As in `start_core`: the core it starts saves the session, never to the person's.
            .env("XDG_STATE_HOME", self.directory.join("state"))
            .env("HOME", self.directory.join("state"))
            .env("SHELL", shell)
            .env(CLIENT, &self.directory);
        command
    }

    fn launch(&self) -> Result<Output> {
        Ok(self.launcher("/bin/sh").output()?)
    }

    /// Starts a core with `arguments` and a one-line shell, and notes the core.
    fn start_core(&self, arguments: &[&str], shell: &str) -> Result<Child> {
        self.start_core_at(arguments, shell, &self.directory.join("state"))
    }

    /// As [`Session::start_core`], with the core's HOME at `home`.
    fn start_core_at(&self, arguments: &[&str], shell: &str, home: &Path) -> Result<Child> {
        let script = self.directory.join("shell");
        // The core asks the shell for its PATH with `-i -c`, for sandboxed agents.
        let ask = "case $1 in -i) exec /bin/sh \"$@\";; esac";
        fs::write(&script, format!("#!/bin/sh\n{ask}\n{shell}\n"))?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        let core = Command::new(self.directory.join("pinfold-core"))
            .args(arguments)
            .env("XDG_RUNTIME_DIR", self.directory.join("run"))
            .env("XDG_CONFIG_HOME", self.directory.join("config"))
            // The state directory, on macOS from HOME, so a test never writes the person's.
            .env("XDG_STATE_HOME", self.directory.join("state"))
            .env("HOME", home)
            .env("SHELL", &script)
            .env(CLIENT, &self.directory)
            // A core with no pane need not wait 30 s for a window here.
            .env(
                "PINFOLD_TEST_EMPTY_WAIT_MS",
                EMPTY_WAIT.as_millis().to_string(),
            )
            // As a launcher starts one.
            .stderr(control::start_log(&self.directory.join("run/pinfold"))?)
            .spawn()?;
        note(self.directory.join("core"), core.id().to_string())?;
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

    /// Whether `name` appeared in time. A writer of more than one write renames it into place,
    /// so a non-empty file is a finished one. A helper's panic fails the test at once.
    fn wait_for(&self, name: &str) -> bool {
        self.wait_beside(name, None)
    }

    /// As [`Session::wait_for`], and fails at once if `core` ends first, taking its panes with
    /// it, as when macOS kills a core at exec (#288).
    fn wait_beside(&self, name: &str, mut core: Option<&mut Child>) -> bool {
        let start = Instant::now();
        while start.elapsed() < PATIENCE {
            let failed = self.read("failed");
            assert!(failed.is_empty(), "a helper process failed: {failed}");
            if let Some(core) = core.as_deref_mut() {
                let ended = core.try_wait().ok().flatten();
                assert!(
                    ended.is_none(),
                    "the core ended: {ended:?}\n{}",
                    self.read_log()
                );
            }
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

/// Writes `text` to `path` whole: a reader sees nothing or all of it, never part.
fn note(path: impl AsRef<Path>, text: impl AsRef<[u8]>) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut part = path.as_os_str().to_owned();
    part.push(format!(".part{}", std::process::id()));
    fs::write(&part, text)?;
    fs::rename(part, path)
}

/// The run's directory, in a helper process started with `variable`. The helper's panic is also
/// noted in `failed` there, so [`Session::wait_for`] fails at once with its text.
fn helper(variable: &str) -> Option<PathBuf> {
    let directory = PathBuf::from(env::var_os(variable)?);
    let failed = directory.join("failed");
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = note(&failed, info.to_string());
        previous(info);
    }));
    Some(directory)
}

/// Shell that runs `command` into the run's file `name`, renamed into place once it is whole, and
/// keeps its status.
fn into(command: &str, name: &str) -> String {
    let path = format!("\"$PINFOLD_TEST_CLIENT/{name}\"");
    format!("{{ {command}; }} > {path}.part; status=$?; mv {path}.part {path}; [ $status = 0 ]")
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

/// Answers owner prompts as the file `owner-answers` says, a line each and `allow` past its end,
/// noting each in `owner-asked` first, and returns the first other prompt, or `None` once the link
/// ends or the clock runs out.
fn approval(link: &mut Link, directory: &Path) -> Option<Approval> {
    approval_until(link, directory, |_| false)
}

/// As [`approval`], returning `None` once `done` too.
fn approval_until(
    link: &mut Link,
    directory: &Path,
    mut done: impl FnMut(&mut Link) -> bool,
) -> Option<Approval> {
    let mut other = None;
    let _ = until(link, |link| {
        while let Some(prompt) = link.take_approval() {
            if prompt.action != ApprovalAction::Owner {
                other = Some(prompt);
                return true;
            }
            let asked = directory.join("owner-asked");
            let mut noted = fs::read_to_string(&asked).unwrap_or_default();
            let answers = fs::read_to_string(directory.join("owner-answers")).unwrap_or_default();
            let answer = match answers.lines().nth(noted.lines().count()) {
                Some("deny") => ApprovalAnswer::Deny,
                _ => ApprovalAnswer::AllowOnce,
            };
            let _ = writeln!(noted, "{}", prompt.texts.join(" | "));
            assert!(note(&asked, noted).is_ok());
            let id = prompt.id;
            assert!(link.send(Request::ApprovalAnswer { id, answer }));
        }
        done(link) || link.disconnected().is_some()
    });
    other
}

/// Not a test: a window that answers owner prompts with [`approval`] once it notes `window`.
#[test]
#[ignore = "run by the tests that need an owner window"]
fn owner_window_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    let core = std::os::unix::process::parent_id().to_string();
    note(directory.join("core"), core).unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    until(&mut link, |link| link.is_greeted()).unwrap();
    note(directory.join("window"), "yes").unwrap();
    approval(&mut link, directory);
}

/// The first client types, then leaves when asked to the front. The second sees what is left.
fn client(directory: &Path) -> Result {
    let note = |name: &str, text: &str| note(directory.join(name), text);
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
#[ignore = "run by a_session_outlives_its_client_and_says_when_the_core_dies"]
fn client_process() {
    if let Some(directory) = helper(CLIENT) {
        client(Path::new(&directory)).unwrap();
    }
}

#[test]
fn a_session_outlives_its_client_and_says_when_the_core_dies() {
    let exe = env::current_exe().unwrap();
    let window = format!("exec '{}' --exact --ignored client_process", exe.display());
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
    say_version(VERSION)
}

/// Shell that writes a hello of `version` to descriptor 3.
fn say_version(version: u16) -> Result<String> {
    let mut wire = Vec::new();
    let hello = Request::Hello { version };
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

/// Link version of a window two updates newer, which no longer speaks this core's (ADR-0043).
const NEXT: u16 = VERSION + 2;

/// A window script that notes each core that starts it, and speaks `other` the first time, as
/// the window of a newer install does, and this core's version after.
fn updated_window(other: u16) -> Result<String> {
    let hello = say_hello()?;
    let other = say_version(other)?;
    Ok(format!(
        "cd \"$PINFOLD_TEST_CLIENT\"\n{NOTE_CORE}\necho $PPID >> cores\n\
         if [ -e updated ]; then {hello}; else : > updated; {other}; fi\n{STAY}"
    ))
}

#[test]
fn an_updated_window_restarts_a_core_at_a_prompt_and_opens() {
    let session = Session::new("u", Some(&updated_window(NEXT).unwrap())).unwrap();
    // A login `/bin/sh` runs the profile, whose `path_helper` on macOS a restart would end, so
    // the window could ask before the prompt. This shell is at its prompt from the start.
    let shell = session.directory.join("shell");
    let ask = "case $1 in -i) exec /bin/sh \"$@\";; esac";
    fs::write(&shell, format!("#!/bin/sh\n{ask}\nexec /bin/sh\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let launched = session.launcher(shell.to_str().unwrap()).output().unwrap();
    let error = String::from_utf8_lossy(&launched.stderr);
    assert!(launched.status.success(), "{error}");
    let said = format!("restarting its session, link version {VERSION} to {NEXT}");
    assert!(error.contains(&said), "{error}");
    let cores = session.read("cores");
    let cores: Vec<_> = cores.lines().collect();
    assert!(cores.len() == 2 && cores[0] != cores[1], "{cores:?}");
}

/// ADR-0043: a window one version apart answers at the older version, and the session keeps
/// its core. A window of version N+1 answers this core at its version, so the stub speaks N-1.
#[test]
fn a_window_one_version_apart_keeps_the_core() {
    let session = Session::new("o", Some(&updated_window(OLDEST).unwrap())).unwrap();
    let launched = session.launch().unwrap();
    let error = String::from_utf8_lossy(&launched.stderr);
    assert!(launched.status.success(), "{error}");
    assert!(!error.contains("restarting"), "{error}");
    let cores = session.read("cores");
    assert_eq!(cores.lines().count(), 1, "{cores}");
}

#[test]
fn an_updated_window_never_restarts_a_core_running_a_program_unasked() {
    let session = Session::new("r", Some(&updated_window(NEXT).unwrap())).unwrap();
    let mut core = session
        .start_core(
            &[],
            "sleep 60 &\necho $$ > \"$PINFOLD_TEST_CLIENT/pane\"\nwait",
        )
        .unwrap();
    assert!(session.wait_for("pane"));
    let error = launch_failure(&session).unwrap();
    assert!(error.contains("PinFold was updated"), "{error}");
    assert!(error.contains("pane 0: "), "{error}");
    assert!(error.contains("Not now keeps them running"), "{error}");
    assert!(core.try_wait().unwrap().is_none(), "the core was ended");
    // The core says why, with both versions.
    let logged = format!("the window is link version {NEXT} and this core {VERSION}");
    assert!(
        session.read_log().contains(&logged),
        "{}",
        session.read_log()
    );
}

#[test]
fn a_window_that_exits_during_start_fails_the_launcher() {
    let window = format!("{NOTE_CORE}\necho why-it-ended >&2\nexit 3");
    let session = Session::new("d", Some(&window)).unwrap();
    older_log(&session, "an-older-core").unwrap();
    let error = launch_failure(&session).unwrap();
    assert!(
        error.contains("the window ended before it was ready"),
        "{error}"
    );
    // The hint names the last core's log too.
    let previous = session.log().with_extension("log.1");
    let both = format!("{} and {}", session.log().display(), previous.display());
    assert!(error.contains(&both), "{error}");
    assert!(session.read_log().contains("why-it-ended"));
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(session.log()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// As macOS's Gatekeeper sometimes kills an unsigned build at exec (#288).
#[test]
fn a_core_killed_before_it_answers_fails_the_launcher_at_once() {
    let session = Session::new("kc", None).unwrap();
    let core = session.directory.join("pinfold-core");
    fs::remove_file(&core).unwrap();
    fs::write(&core, "#!/bin/sh\nkill -9 $$\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&core, fs::Permissions::from_mode(0o700)).unwrap();
    let start = Instant::now();
    let error = launch_failure(&session).unwrap();
    assert!(
        error.contains("the core was killed before it answered"),
        "{error}"
    );
    assert!(error.contains("signal: 9"), "{error}");
    // The launcher's start wait, which it used to sit out before saying no core answered.
    assert!(start.elapsed() < Duration::from_secs(5));
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

/// The run directory with a log that `text` stands in for an older core's.
fn older_log(session: &Session, text: &str) -> Result {
    let run = session.directory.join("run/pinfold");
    fs::create_dir_all(&run)?;
    fs::write(session.log(), text)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&run, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[test]
fn a_new_core_keeps_the_last_log_and_a_running_one_keeps_its_own() {
    let window = format!(
        "{NOTE_CORE}\necho this-core >&2\n{}\n{STAY}",
        say_hello().unwrap()
    );
    let session = Session::new("h", Some(&window)).unwrap();
    older_log(&session, "an-older-core").unwrap();
    let previous = || fs::read_to_string(session.log().with_extension("log.1")).unwrap();
    for _ in 0..2 {
        assert!(session.launch().unwrap().status.success());
        assert_eq!(session.read_log(), "this-core\n");
        assert_eq!(previous(), "an-older-core");
    }

    // A core that crashed: its panic line outlives the next launch.
    assert!(
        kill(session.read("core").trim())
            .status()
            .unwrap()
            .success()
    );
    let panicked = "this-core\npinfold: panicked at crates/pinfold-core/src/vt.rs:1:1\n";
    let socket = control::socket_path(&session.directory.join("run/pinfold"));
    let start = Instant::now();
    while UnixStream::connect(&socket).is_ok() {
        assert!(start.elapsed() < PATIENCE, "the killed core still answers");
        thread::sleep(Duration::from_millis(20));
    }
    fs::write(session.log(), panicked).unwrap();
    assert!(session.launch().unwrap().status.success());
    assert_eq!(session.read_log(), "this-core\n");
    assert_eq!(previous(), panicked);
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
#[ignore = "run by a_panic_prints_its_place_and_not_its_text"]
fn panic_process() {
    if env::var_os(PANIC).is_some() {
        pinfold_cli::panics_name_no_text();
        let _ = std::panic::catch_unwind(|| std::panic::panic_any("text from a pane".to_owned()));
    }
}

#[test]
fn a_panic_prints_its_place_and_not_its_text() {
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", "--ignored", "panic_process", "--nocapture"])
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

/// The core's `EMPTY_WAIT`, as [`Session::start_core`] shortens it.
const EMPTY_WAIT: Duration = Duration::from_secs(2);

#[test]
fn a_core_started_with_no_shell_exits_if_no_window_ever_opens() {
    let session = Session::new("ns", None).unwrap();
    let mut core = session.start_core(&["--no-shell"], NOTE_PANE).unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = core.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < PATIENCE, "the empty core never exited");
        thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "{}", session.read_log());
    assert!(
        start.elapsed() >= EMPTY_WAIT,
        "it did not wait for a window"
    );
    assert!(session.read("pane").is_empty(), "it started a shell");
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
#[ignore = "run by hardening_twice_is_harmless"]
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
    let inside = format!(
        "{}; {}; exec sleep 30",
        into(&ctl, "in"),
        into(&format!("{ctl} --json"), "json")
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
            .args(["-c", &into(&format!("{ctl} 2>&1"), name)])
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
#[ignore = "run by a_pane_reports_its_own_status_and_not_another_panes"]
fn status_process() {
    let Some(directory) = env::var_os(STATUS).and_then(|_| helper(CLIENT)) else {
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
    note(Path::new(&directory).join(name), heard).unwrap();
}

#[test]
fn a_pane_reports_its_own_status_and_not_another_panes() {
    let session = Session::new("st", None).unwrap();
    let exe = env::current_exe().unwrap();
    let shell = format!(
        "{STATUS}=1 '{}' --exact --ignored status_process; exec sleep 30",
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
        .args(["--exact", "--ignored", "harden_process"])
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
#[ignore = "run by a_caller_waits_on_its_pane_and_hears_panes_open_and_close"]
fn api_process() {
    let Some(directory) = helper(API) else {
        return;
    };
    let directory = Path::new(&directory);
    let subscribe = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "events.subscribe",
        "params": {"events": ["pane.opened", "pane.closed"]}});
    let mut events = api(&[subscribe]).unwrap();
    for _ in 0..2 {
        next(&mut events, || Ok(())).unwrap();
    }
    note(directory.join("subscribed"), "").unwrap();

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
    note(directory.join("waited"), heard).unwrap();

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
    note(directory.join("events"), heard).unwrap();
}

/// Not a test: the window of [`a_caller_waits_on_its_pane_and_hears_panes_open_and_close`],
/// which opens a tab once the first pane has subscribed.
#[test]
#[ignore = "run by a_caller_waits_on_its_pane_and_hears_panes_open_and_close"]
fn tab_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    note(
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
    let window = format!("exec '{}' --exact --ignored tab_process", exe.display());
    let session = Session::new("w", Some(&window)).unwrap();
    // The first pane is the caller; the tab's pane ends a moment after it opens.
    let shell = session.directory.join("shell");
    let script = format!(
        "#!/bin/sh\nif mkdir \"$PINFOLD_TEST_CLIENT/first\"; then exec '{}' --exact --ignored api_process; fi\nsleep 1\n",
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
#[ignore = "run by only_the_window_answers_cross_pane_actions_and_ctl_revokes_grants"]
fn approval_api_process() {
    let Some(directory) = helper(API) else {
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
    std::io::Write::write_all(&mut tty, b"y a n Escape\nanswer-bytes-written\n").unwrap();
    // Once the marker after them is on screen, the core has read those bytes too.
    let start = Instant::now();
    loop {
        let read = ask("pane.read", serde_json::json!({"pane": 0})).unwrap();
        let text = read["result"]["text"].as_str().unwrap_or_default();
        if text.contains("answer-bytes-written") {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "no marker: {read}");
        thread::sleep(Duration::from_millis(10));
    }
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
    note(directory.join("attempted"), "yes").unwrap();
    let blocked = matches!(
        first.next(),
        Some(Err(error)) if error.kind() == std::io::ErrorKind::WouldBlock
    );
    assert!(blocked, "pane output or the API answered the prompt");
    note(directory.join("still-parked"), "yes").unwrap();

    let allowed = next(&mut first, || Ok(())).unwrap();
    assert_eq!(allowed["result"], serde_json::json!({}));
    note(directory.join("first-allowed"), "yes").unwrap();
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
    note(directory.join("second-denied"), "yes").unwrap();

    // Eight lines cut inside the key block, whether or not the echo of "approved" is in.
    let read = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "pane.read",
        "params": {"pane": 1, "lines": 8}});
    let mut reading = api(&[read]).unwrap();
    let _ = next(&mut reading, || Ok(())).unwrap();
    let read = next(&mut reading, || Ok(())).unwrap();
    let text = read["result"]["text"].as_str().unwrap();
    assert!(text.contains("«redacted:password»"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(text.starts_with("«redacted:private-key»\n"), "{text}");
    assert!(!text.contains("cut-key-row"), "{text}");
    note(directory.join("redacted"), "yes").unwrap();
}

/// Not a test: the trusted inherited link sees both prompts and answers them with key meanings.
#[test]
#[ignore = "run by only_the_window_answers_cross_pane_actions_and_ctl_revokes_grants"]
fn approval_window_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    note(
        directory.join("core"),
        std::os::unix::process::parent_id().to_string(),
    )
    .unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    until(&mut link, |link| link.is_greeted()).unwrap();
    // The tab's shell must not claim `first` before pane 0's has, or the roles swap.
    until(&mut link, |_| directory.join("first").exists()).unwrap();
    assert!(link.send(Request::NewTab));
    let first = approval(&mut link, directory).unwrap();
    assert_eq!(
        (first.caller, first.target, first.action),
        (0, 1, ApprovalAction::Send)
    );
    assert_eq!(first.texts, ["approved\n"]);
    note(directory.join("prompt"), format!("{}", first.id)).unwrap();
    until(&mut link, |_| {
        directory.join("attempted").exists() && directory.join("still-parked").exists()
    })
    .unwrap();
    assert!(link.send(Request::ApprovalAnswer {
        id: first.id,
        answer: ApprovalAnswer::AllowForThirtyMinutes,
    }));

    // The outside terminal's revoke is asked and allowed on the way.
    let second = approval(&mut link, directory).unwrap();
    assert_eq!(second.texts, ["must-not-arrive\n"]);
    assert!(link.send(Request::ApprovalAnswer {
        id: second.id,
        answer: ApprovalAnswer::Deny,
    }));
    until(&mut link, |_| directory.join("second-denied").exists()).unwrap();

    let read = approval(&mut link, directory).unwrap();
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
    let window = format!(
        "exec '{}' --exact --ignored approval_window_process",
        exe.display()
    );
    let session = Session::new("approval", Some(&window)).unwrap();
    let shell = session.directory.join("approval-shell");
    let script = format!(
        "#!/bin/sh\nif mkdir \"$PINFOLD_TEST_CLIENT/first\" 2>/dev/null; then \
         exec '{}' --exact --ignored approval_api_process; else echo yes > \"$PINFOLD_TEST_CLIENT/second\"; \
         echo '-----BEGIN OPENSSH PRIVATE KEY-----'; \
         for row in 0 1 2 3 4 5 6 7 8 9; do echo cut-key-row-$row; done; \
         echo '-----END OPENSSH PRIVATE KEY-----'; echo 'password=hunter2'; \
         while read -r line; do echo \"$line\" >> \"$PINFOLD_TEST_CLIENT/received\"; done; fi\n",
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
        "'{}' ctl grants --json > \"$PINFOLD_TEST_CLIENT/grants\" 2> \"$PINFOLD_TEST_CLIENT/grants-error\"; \
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
    let listed = serde_json::from_str::<serde_json::Value>(&session.read("grants"));
    let listed = listed.unwrap_or_else(|_| panic!("ctl grants: {}", session.read("grants-error")));
    assert_eq!(listed["grants"][0]["id"].to_string(), id);
    let after: serde_json::Value = serde_json::from_str(&session.read("after")).unwrap();
    assert_eq!(after["grants"], serde_json::json!([]));
    // Only the revoke asked: listing grants is a read.
    let asked = session.read("owner-asked");
    assert_eq!(asked, format!("grants.revoke | Grant {id}\n"));
    assert!(session.wait_for("second-denied"), "{}", session.read_log());
    assert!(session.wait_for("redacted"), "{}", session.read_log());
    // Every line the target read: the denied send never reached it.
    assert_eq!(session.read("received"), "approved\n");
}

/// Not a test: pane 0 starts a terminal of its own, as `script -q /dev/null` would, running
/// [`owner_outside_process`], and keeps it until that is done.
#[test]
#[ignore = "run by owner_methods_from_a_new_terminal_in_a_pane_wait_for_the_window"]
fn owner_pane_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    let mut outside = Command::new(env::current_exe().unwrap());
    outside.args(["--exact", "--ignored", "owner_outside_process"]);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    while !directory.join("outside").exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Not a test: in that new terminal, outside every pane, calls each owner method and notes the
/// replies.
#[test]
#[ignore = "run by owner_pane_process"]
fn owner_outside_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    while !directory.join("window").exists() {
        thread::sleep(Duration::from_millis(10));
    }
    let root = fs::canonicalize(directory).unwrap();
    let open = serde_json::json!({"root": root, "layout": {"tabs": [{"panes": [{"cwd": root}]}]}});
    let hello = next(&mut api(&[]).unwrap(), || Ok(())).unwrap();
    let calls = [
        ("session.save", serde_json::json!({})),
        ("session.save", serde_json::json!({})),
        ("grants.list", serde_json::json!({})),
        ("grants.revoke", serde_json::json!({"id": 7})),
        ("workspace.open", open),
    ];
    let asked = calls
        .into_iter()
        .map(|(method, params)| ask(method, params));
    let said: Vec<_> = std::iter::once(hello)
        .chain(asked.map(Result::unwrap))
        .collect();
    let noted = directory.join("outside.tmp");
    note(&noted, serde_json::to_string(&said).unwrap()).unwrap();
    fs::rename(noted, directory.join("outside")).unwrap();
}

#[test]
fn owner_methods_from_a_new_terminal_in_a_pane_wait_for_the_window() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored owner_window_process",
        exe.display()
    );
    let session = Session::new("owner", Some(&window)).unwrap();
    let answers = "deny\nallow\nallow\ndeny\n";
    fs::write(session.directory.join("owner-answers"), answers).unwrap();
    let pane = format!(
        "exec '{}' --exact --ignored owner_pane_process",
        exe.display()
    );
    let mut core = session
        .start_core(&["--window", "1", "80x24"], &pane)
        .unwrap();
    assert!(session.wait_for("outside"), "{}", session.read_log());
    let said: Vec<serde_json::Value> = serde_json::from_str(&session.read("outside")).unwrap();
    let refused = |reply: &serde_json::Value| reply["error"]["message"].to_string();
    // The new terminal is in no pane, so the core takes it for a person's.
    assert_eq!(
        said[0]["result"]["pane"],
        serde_json::Value::Null,
        "{said:?}"
    );
    assert!(
        refused(&said[1]).contains("denied in the window"),
        "{said:?}"
    );
    let path = said[2]["result"]["path"].as_str().unwrap_or_default();
    assert!(Path::new(path).is_file(), "{said:?}");
    // Reading grants is not asked.
    assert_eq!(said[3]["result"], serde_json::json!({"grants": []}));
    assert_eq!(said[4]["result"], serde_json::json!({"revoked": false}));
    assert!(
        refused(&said[5]).contains("denied in the window"),
        "{said:?}"
    );
    let save = "session.save | Writes this session's layout over the saved one";
    let root = fs::canonicalize(&session.directory).unwrap();
    let open = format!(
        "workspace.open | {} | 1 pane(s) in 1 tab(s)",
        root.display()
    );
    let asked = [save, save, "grants.revoke | Grant 7", &open].map(|line| format!("{line}\n"));
    assert_eq!(session.read("owner-asked"), asked.concat());
    core.kill().unwrap();
    core.wait().unwrap();
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
#[ignore = "run by a_pane_spawns_children_and_reaches_only_them"]
fn spawn_process() {
    let Some(directory) = helper(SPAWN) else {
        return;
    };
    let directory = Path::new(&directory);
    // Started with no terminal, the caller is unidentified.
    if directory.join("spawned").exists() {
        let refused = ask("pane.spawn", serde_json::json!({"where": "tab"})).unwrap();
        note(directory.join("outside"), refused.to_string()).unwrap();
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
    note(directory.join("spawned"), heard).unwrap();
}

#[test]
fn a_pane_spawns_children_and_reaches_only_them() {
    let session = Session::new("p", None).unwrap();
    let exe = env::current_exe().unwrap();
    let shell = format!(
        "if mkdir \"$PINFOLD_TEST_CLIENT/first\"; then {SPAWN}=\"$PINFOLD_TEST_CLIENT\" exec '{}' \
         --exact --ignored spawn_process; fi; exec cat",
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
        .args(["--exact", "--ignored", "spawn_process"])
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
#[ignore = "run by output_events_and_match_waits_reach_only_the_callers_pane"]
fn output_process() {
    let Some(directory) = helper(OUTPUT) else {
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
    note(Path::new(&directory).join("heard"), heard).unwrap();
}

/// Not a test: a terminal outside any pane in
/// [`output_events_and_match_waits_reach_only_the_callers_pane`], which asks for output.
#[test]
#[ignore = "run by output_events_and_match_waits_reach_only_the_callers_pane"]
fn outside_output_process() {
    let Some(directory) = helper(OUTPUT) else {
        return;
    };
    let events = serde_json::json!({"events": ["pane.output"]});
    let refused = ask("events.subscribe", events).unwrap();
    note(Path::new(&directory).join("outside"), refused.to_string()).unwrap();
}

#[test]
fn output_events_and_match_waits_reach_only_the_callers_pane() {
    let session = Session::new("o", None).unwrap();
    let exe = env::current_exe().unwrap();
    // The second pane is another of the person's, printing all along.
    let shell = format!(
        "if mkdir \"$PINFOLD_TEST_CLIENT/first\"; then {OUTPUT}=\"$PINFOLD_TEST_CLIENT\" exec '{}' \
         --exact --ignored output_process; elif mkdir \"$PINFOLD_TEST_CLIENT/second\"; then \
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
        .args(["--exact", "--ignored", "outside_output_process"])
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
    let waited = session.wait_beside("out", Some(&mut core));
    assert!(waited, "{}", session.read_log());

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
fn ctl_session_save_writes_a_private_snapshot_only_from_outside_a_pane() {
    use std::os::unix::fs::PermissionsExt;
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored owner_window_process",
        exe.display()
    );
    let session = Session::new("n", Some(&window)).unwrap();
    let ctl = format!(
        "'{}' ctl session save",
        session.directory.join("pinfold").display()
    );
    let to = |name: &str| into(&format!("{ctl} 2>&1"), name);
    let inside = format!("{}; exec sleep 30", to("in"));
    let mut core = session
        .start_core(&["--window", "1", "80x24"], &inside)
        .unwrap();
    assert!(session.wait_for("in"), "{}", session.read_log());
    assert!(session.wait_for("window"), "{}", session.read_log());
    let refused = session.read("in");
    assert!(refused.contains("outside any pane"), "{refused}");

    // The core's state directory, which `start_core` puts in the session's.
    let state = session.directory.join("state");
    let mut outside = Command::new("/bin/sh");
    outside
        .args(["-c", &to("out")])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(CLIENT, &session.directory);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    assert!(session.wait_for("out"));
    let said = session.read("out");
    let path = PathBuf::from(said.trim());
    assert!(path.starts_with(&state), "{said}");
    assert!(path.ends_with("sessions/default.json"), "{said}");
    assert!(session.read("owner-asked").starts_with("session.save | "));
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let snapshot: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(snapshot["version"], 1);
    assert_eq!(snapshot["tabs"][0]["root"]["pane"], 0);
    // The shell, whose temporary path redaction may take for a secret.
    assert!(snapshot["panes"][0]["command"].is_string(), "{snapshot}");
    core.kill().unwrap();
    core.wait().unwrap();
}

#[test]
fn sigterm_saves_the_session_before_the_core_exits() {
    let session = Session::new("t", None).unwrap();
    let mut core = session.start_core(&["--idle", "1"], NOTE_PANE).unwrap();
    assert!(session.wait_for("pane"), "the pane's shell did not start");

    let term = format!("kill -TERM {}", core.id());
    let sent = Command::new("/bin/sh").args(["-c", &term]).status();
    assert!(sent.unwrap().success());
    let start = Instant::now();
    let status = loop {
        if let Some(status) = core.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < PATIENCE, "the core outlived SIGTERM");
        thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{}", session.read_log());

    // `pinfold_config::state_dir` under the HOME and XDG_STATE_HOME that `start_core` sets.
    let state = session
        .directory
        .join("state")
        .join(if cfg!(target_os = "macos") {
            "Library/Application Support/PinFold"
        } else {
            "pinfold"
        });
    let saved = fs::read(state.join("sessions/default.json")).unwrap();
    let snapshot: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(snapshot["tabs"][0]["root"]["pane"], 0);
}

/// Runs `pinfold ctl <arguments>` in a terminal outside any pane and returns what it printed.
fn ctl(session: &Session, arguments: &str, name: &str) -> Result<String> {
    let pinfold = session.directory.join("pinfold");
    let _ = fs::remove_file(session.directory.join(name));
    let line = format!(
        "'{}' {arguments} > \"$PINFOLD_TEST_CLIENT/{name}.tmp\" 2>&1; \
         mv \"$PINFOLD_TEST_CLIENT/{name}.tmp\" \"$PINFOLD_TEST_CLIENT/{name}\"",
        pinfold.display()
    );
    let mut outside = Command::new("/bin/sh");
    outside
        .args(["-c", &line])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(CLIENT, &session.directory);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80)?;
    assert!(session.wait_for(name), "ctl {arguments} said nothing");
    Ok(session.read(name))
}

/// Saves the session until each of its `panes` has a directory under `under`, from OSC 7 or its
/// shell, and returns the snapshot. A shell's own directory is known from its start, so a save
/// can come before the shell's `cd`.
fn save_with_cwds(session: &Session, panes: usize, under: &Path) -> Result<serde_json::Value> {
    let start = Instant::now();
    loop {
        // Until the core answers, ctl says why not, which is no JSON.
        let said = ctl(session, "ctl session save", "saved")?;
        let saved: serde_json::Value = fs::read(said.trim())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let listed = saved["panes"].as_array().map_or(&[][..], Vec::as_slice);
        let moved = |pane: &serde_json::Value| {
            pane["cwd"]
                .as_str()
                .is_some_and(|cwd| Path::new(cwd).starts_with(under))
        };
        if listed.len() == panes && listed.iter().all(moved) {
            return Ok(saved);
        }
        assert!(start.elapsed() < PATIENCE, "{said}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Not a test: the window of [`a_new_core_restores_the_layout_and_cwds_of_a_killed_one`],
/// which splits the focused pane in its first core, then allows each save.
#[test]
#[ignore = "run by a_new_core_restores_the_layout_and_cwds_of_a_killed_one"]
fn split_client_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    until(&mut link, |link| !link.screens().is_empty()).unwrap();
    if fs::create_dir(directory.join("split")).is_ok() {
        let pane = link.focused();
        assert!(link.send(Request::Split {
            pane,
            way: SplitWay::Right
        }));
        until(&mut link, |link| link.screens().len() == 2).unwrap();
    }
    note(directory.join("window"), "yes").unwrap();
    approval(&mut link, directory);
}

#[test]
fn a_new_core_restores_the_layout_and_cwds_of_a_killed_one() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored split_client_process",
        exe.display()
    );
    let session = Session::new("r", Some(&window)).unwrap();
    // Each pane works in a directory of its own, and sends no OSC 7. A restore keeps only a
    // directory no one else can write, which Linux's sticky `/tmp` is not, so these live in the
    // target directory.
    let work =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("restore-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).unwrap();
    let tell = r#"printf '\033]7;file://%s\033\\' "$(pwd -P)""#;
    let first = format!(
        "d='{}/w'$$; mkdir \"$d\" && cd \"$d\"; exec sleep 60",
        work.display()
    );
    let mut core = session
        .start_core(&["--window", "2", "80x24"], &first)
        .unwrap();
    let saved = save_with_cwds(&session, 3, &work).unwrap();
    assert_eq!(saved["tabs"].as_array().unwrap().len(), 2, "{saved}");
    assert!(saved["tabs"][1]["root"]["split"].is_object(), "{saved}");
    core.kill().unwrap();
    core.wait().unwrap();

    // The new core, started as a launcher starts it, rebuilds that layout with a shell in each
    // saved directory.
    let second = format!("pwd -P >> \"$PINFOLD_TEST_CLIENT/pwds\"; {tell}; exec sleep 60");
    let mut core = session.start_core(&[], &second).unwrap();
    // A window to allow the saves, once the core answers, as a launcher would start its own.
    let start = Instant::now();
    while serde_json::from_str::<serde_json::Value>(
        &ctl(&session, "ctl list --json", "list").unwrap(),
    )
    .is_err()
    {
        assert!(start.elapsed() < PATIENCE, "{}", session.read_log());
        thread::sleep(Duration::from_millis(20));
    }
    let launched = session.launch().unwrap();
    assert!(launched.status.success(), "{launched:?}");
    let restored = save_with_cwds(&session, 3, &work).unwrap();
    assert_eq!(restored["tabs"], saved["tabs"]);
    assert_eq!(restored["active"], saved["active"]);
    let cwds = |snapshot: &serde_json::Value| {
        let panes = snapshot["panes"].as_array().unwrap();
        let cwds = panes
            .iter()
            .map(|pane| pane["cwd"].as_str().unwrap().to_owned());
        cwds.collect::<Vec<_>>()
    };
    assert_eq!(cwds(&restored), cwds(&saved));
    // The restored shells say where they started, after the core already knows.
    let start = Instant::now();
    while session.read("pwds").lines().count() < 3 && start.elapsed() < PATIENCE {
        thread::sleep(Duration::from_millis(10));
    }
    let mut pwds: Vec<String> = session.read("pwds").lines().map(str::to_owned).collect();
    let mut expected = cwds(&saved);
    pwds.sort();
    expected.sort();
    assert_eq!(pwds, expected);
    core.kill().unwrap();
    core.wait().unwrap();
    fs::remove_dir_all(&work).unwrap();
}

#[test]
fn up_opens_a_workspace_as_shells_once_and_then_attaches_to_it() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored owner_window_process",
        exe.display()
    );
    let session = Session::new("u", Some(&window)).unwrap();
    let project = session.directory.join("project");
    for directory in ["a", "b", ".pinfold"] {
        fs::create_dir_all(project.join(directory)).unwrap();
    }
    let layout = "[[tabs]]\n[[tabs.panes]]\ncwd = 'a'\n[[tabs]]\n[[tabs.panes]]\ncwd = 'b'\n";
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
            // A state directory of its own, so the core it starts has no session to restore.
            .env("XDG_STATE_HOME", session.directory.join("state"))
            .env("HOME", session.directory.join("state"))
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
    let waits = "exit 0\n";
    assert_eq!(said, waits);
    // A fresh core with no session to restore opens only the layout's two tabs, a pane each.
    let opened = cwds();
    assert_eq!(opened, [a.to_str().unwrap(), b.to_str().unwrap()]);
    let saved = in_terminal("ctl session save");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(saved.lines().next().unwrap()).unwrap()).unwrap();
    assert_eq!(snapshot["tabs"].as_array().unwrap().len(), 2, "{snapshot}");

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

/// Names the directory of a run, in the terminal outside any pane that asks for trust.
const TRUST: &str = "PINFOLD_TEST_TRUST";

/// Not a test: a terminal outside every pane asks for trust twice, and notes what it hears.
#[test]
#[ignore = "run by only_the_window_answers_a_trust_prompt_and_trusting_records_the_layout"]
fn trust_api_process() {
    let Some(directory) = helper(TRUST) else {
        return;
    };
    let directory = Path::new(&directory);
    let root = fs::canonicalize(directory.join("project")).unwrap();
    let root = root.to_str().unwrap();
    let (old, new) = ("a".repeat(64), "b".repeat(64));
    let state = |sha256: &str| {
        ask(
            "workspace.trust_state",
            serde_json::json!({"root": root, "sha256": sha256}),
        )
        .unwrap()["result"]
            .clone()
    };
    let unknown = serde_json::json!({"trusted": false, "changed": false});
    assert_eq!(state(&old), unknown);
    let runs = ["npm run dev", "cargo watch\u{1b}[2J"];
    let trusted = ask(
        "workspace.ask_trust",
        serde_json::json!({"root": root, "sha256": old, "runs": runs}),
    )
    .unwrap();
    assert_eq!(trusted["result"], serde_json::json!({"trusted": true}));
    assert_eq!(state(&old)["trusted"], true);
    assert_eq!(state(&new)["changed"], true);
    let refused = ask(
        "workspace.ask_trust",
        serde_json::json!({"root": root, "sha256": new, "runs": []}),
    )
    .unwrap();
    let reason = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(reason.contains("not trusted"), "{refused}");
    assert_eq!(state(&old)["trusted"], true);
    note(directory.join("asked"), "yes").unwrap();
}

/// Not a test: the window sees each trust prompt with the directory and every run line, then
/// trusts the first and refuses the second, changed one.
#[test]
#[ignore = "run by only_the_window_answers_a_trust_prompt_and_trusting_records_the_layout"]
fn trust_window_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    let core = std::os::unix::process::parent_id().to_string();
    note(directory.join("core"), core).unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    until(&mut link, |link| link.is_greeted()).unwrap();
    let root = fs::canonicalize(directory.join("project")).unwrap();
    for (target, runs, answer) in [
        (
            0,
            &["npm run dev", "cargo watch\u{1b}[2J"][..],
            ApprovalAnswer::AllowOnce,
        ),
        (1, &[][..], ApprovalAnswer::Deny),
    ] {
        let mut prompt = None;
        until(&mut link, |link| {
            prompt = link.take_approval();
            prompt.is_some()
        })
        .unwrap();
        let prompt = prompt.unwrap();
        assert_eq!(
            (prompt.target, prompt.action),
            (target, ApprovalAction::Trust)
        );
        assert_eq!(prompt.texts[0], root.to_str().unwrap());
        assert_eq!(prompt.texts[1..], *runs);
        let id = prompt.id;
        assert!(link.send(Request::ApprovalAnswer { id, answer }));
    }
    until(&mut link, |_| directory.join("asked").exists()).unwrap();
}

#[test]
fn only_the_window_answers_a_trust_prompt_and_trusting_records_the_layout() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored trust_window_process",
        exe.display()
    );
    let session = Session::new("trust", Some(&window)).unwrap();
    fs::create_dir_all(session.directory.join("project")).unwrap();
    let (home, state) = (
        session.directory.join("home"),
        session.directory.join("state"),
    );
    let launched = session
        .launcher("/bin/sh")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", &state)
        .output()
        .unwrap();
    assert!(launched.status.success(), "{launched:?}");
    let mut outside = Command::new(exe);
    outside
        .args(["--exact", "--ignored", "trust_api_process"])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(TRUST, &session.directory);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    assert!(session.wait_for("asked"), "{}", session.read_log());
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

#[test]
fn an_agent_launches_by_profile_as_the_callers_child_and_an_unknown_one_is_refused() {
    let session = Session::new("ag", None).unwrap();
    fs::create_dir_all(session.directory.join("config/pinfold")).unwrap();
    let work = session.directory.join("state/work");
    let pwd = format!(
        "pwd -P > '{}/agent.cwd'; exec sleep 30",
        session.directory.display()
    );
    let mut toml = format!("[agents.sleepy]\ncommand = [\"/bin/sh\", \"-c\", \"{pwd}\"]\n");
    let pinfold = format!("'{}'", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    // The caller's shell moves to a private directory under the core's HOME, as the sandbox
    // refuses HOME and what is above it, and sends no OSC 7, as the default macOS zsh does not.
    let mut shell = format!(
        "mkdir -p -m 700 \"$HOME/work\"; cd \"$HOME/work\"; \
         {pinfold} agent sleepy 2> {client}/e; {pinfold} agent nope 2> {client}/nope; "
    );
    if cfg!(target_os = "macos") {
        toml += "[agents.boxed]\ncommand = [\"sleep\", \"31\"]\nsandbox = true\n";
        shell += &format!("{pinfold} agent boxed --tab 2> {client}/boxed; ");
    }
    fs::write(session.directory.join("config/pinfold/pinfold.toml"), toml).unwrap();
    // A send to pane 1 is allowed only if the caller spawned it.
    shell += &format!(
        "{pinfold} ctl send 1 x 2> {client}/send && \
         {pinfold} ctl list --json > {client}/j; mv {client}/j {client}/json; exec sleep 30"
    );
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("json"), "{}", session.read_log());
    core.kill().unwrap();
    core.wait().unwrap();

    let refused = session.read("nope");
    assert!(
        refused.contains("no agent profile by that name"),
        "{refused}"
    );
    let list: serde_json::Value = serde_json::from_str(&session.read("json")).unwrap();
    let command = |id: u64| {
        let panes = list["panes"].as_array().unwrap();
        let pane = panes.iter().find(|pane| pane["id"] == id);
        pane.map(|pane| pane["command"].clone())
    };
    assert_eq!(
        command(1),
        Some("/bin/sh".into()),
        "{list} {}",
        session.read("e")
    );
    // The agent started in the caller's directory, which no OSC 7 told.
    assert!(session.wait_for("agent.cwd"), "{}", session.read("e"));
    let started = session.read("agent.cwd");
    assert_eq!(Path::new(started.trim()), work.canonicalize().unwrap());
    if cfg!(target_os = "macos") {
        let boxed = session.read("boxed");
        assert_eq!(
            command(2),
            Some("/usr/bin/sandbox-exec".into()),
            "{list} {boxed}"
        );
    }
}

#[test]
fn a_sandboxed_agent_does_not_start_outside_home() {
    let session = Session::new("oh", None).unwrap();
    fs::create_dir_all(session.directory.join("config/pinfold")).unwrap();
    let ran = session.directory.join("agent.ran");
    let toml = format!(
        "[agents.boxed]\ncommand = [\"/bin/sh\", \"-c\", \"echo ran > '{}'; exec sleep 30\"]\n\
         sandbox = true\n",
        ran.display()
    );
    fs::write(session.directory.join("config/pinfold/pinfold.toml"), toml).unwrap();
    // A private directory of the user's outside HOME, as /opt/homebrew is. The shell moves there
    // and reports it with OSC 7, so the pane's directory is that one whichever the core reads.
    let pinfold = format!("'{}'", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    let shell = format!(
        "mkdir -m 700 {client}/outside; cd {client}/outside; \
         printf '\\033]7;file://%s\\007' \"$PWD\"; \
         {pinfold} agent boxed 2> {client}/b; mv {client}/b {client}/boxed; \
         {pinfold} ctl list --json > {client}/j; mv {client}/j {client}/json; exec sleep 30"
    );
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("json"), "{}", session.read_log());
    core.kill().unwrap();
    core.wait().unwrap();

    let boxed = session.read("boxed");
    let list: serde_json::Value = serde_json::from_str(&session.read("json")).unwrap();
    assert_eq!(list["panes"].as_array().unwrap().len(), 1, "{list}");
    assert!(!ran.exists(), "{boxed}");
    assert!(boxed.contains("inside your home folder"), "{boxed}");
}

/// Not a test: the window of
/// [`up_runs_a_trusted_workspace_and_a_restore_runs_it_again_until_its_file_changes`], which
/// refuses a first project, then checks that nothing ran before it trusts the second and waits
/// to see the run line's output in the pane. Once that is shown, a later window types a probe into the focused pane,
/// which runs after any line a restore typed.
#[test]
#[ignore = "run by up_runs_a_trusted_workspace_and_a_restore_runs_it_again_until_its_file_changes"]
fn up_trust_window_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    let core = std::os::unix::process::parent_id().to_string();
    note(directory.join("core"), core).unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    if directory.join("shown").exists() {
        let probes = || probes(&fs::read_to_string(directory.join("ran")).unwrap_or_default());
        let before = probes();
        until(&mut link, |link| !link.screens().is_empty()).unwrap();
        assert!(type_in(
            &mut link,
            "echo probe >> \"$PINFOLD_TEST_CLIENT/ran\"\n"
        ));
        until(&mut link, |_| probes() > before).unwrap();
        // Then allows each `ctl session save` until the core goes.
        approval(&mut link, directory);
        return;
    }
    for (project, answer) in [
        ("/denied", ApprovalAnswer::Deny),
        ("/up-", ApprovalAnswer::AllowOnce),
    ] {
        let prompt = approval(&mut link, directory).unwrap();
        assert_eq!(prompt.action, ApprovalAction::Trust);
        assert!(prompt.texts[0].contains(project), "{:?}", prompt.texts);
        assert!(prompt.texts[1].contains("space-ran"), "{:?}", prompt.texts);
        assert!(!directory.join("ran").exists());
        assert!(link.send(Request::ApprovalAnswer {
            id: prompt.id,
            answer
        }));
    }
    // The typed line shows `work''space-ran`; only its output shows this.
    // Its `workspace.open`s are asked meanwhile, and allowed.
    approval_until(&mut link, directory, |link| shows(link, "workspace-ran"));
    assert!(shows(&link, "workspace-ran"));
    note(directory.join("shown"), "yes").unwrap();
    approval(&mut link, directory);
}

#[test]
fn up_runs_a_trusted_workspace_and_a_restore_runs_it_again_until_its_file_changes() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored up_trust_window_process",
        exe.display()
    );
    let session = Session::new("ut", Some(&window)).unwrap();
    // A restore runs a line only in a directory no one else can write, which Linux's sticky
    // `/tmp` is not, so the project lives in the target directory.
    let project =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("up-{}", std::process::id()));
    let state = session.directory.join("state");
    let _ = fs::remove_dir_all(&project);
    fs::create_dir_all(project.join(".pinfold")).unwrap();
    fs::create_dir_all(&state).unwrap();
    let layout = r#"[[tabs]]
[[tabs.panes]]
run = "echo work''space-ran | tee -a \"$PINFOLD_TEST_CLIENT/ran\""
"#;
    let denied = session.directory.join("denied");
    for dir in [&project, &denied] {
        fs::create_dir_all(dir.join(".pinfold")).unwrap();
        fs::write(dir.join(".pinfold/layout.toml"), layout).unwrap();
    }
    // Runs `pinfold up` in `dir` from a terminal outside any pane, and returns what it said.
    let up = |dir: &Path| {
        let script = format!(
            "'{}' up > up.tmp 2>&1; echo \"exit $?\" >> up.tmp; mv up.tmp '{}'",
            session.directory.join("pinfold").display(),
            session.directory.join("up").display()
        );
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .current_dir(dir)
            .env("XDG_RUNTIME_DIR", session.directory.join("run"))
            .env("XDG_STATE_HOME", &state)
            .env("HOME", &state)
            .env("SHELL", "/bin/sh")
            .env(CLIENT, &session.directory);
        let _terminal = pinfold_sys::pty::Pty::spawn(command, 24, 80).unwrap();
        assert!(session.wait_for("up"), "{}", session.read_log());
        let said = session.read("up");
        fs::remove_file(session.directory.join("up")).unwrap();
        said
    };
    // Refused, the layout opens as shells, says so and exits 1.
    let said = up(&denied);
    assert!(
        said.starts_with("pinfold: 1 run line did not run: "),
        "{said}"
    );
    assert!(said.ends_with("not trusted\nexit 1\n"), "{said}");
    assert_eq!(up(&project), "exit 0\n");
    assert!(session.wait_for("shown"), "{}", session.read_log());
    assert_eq!(session.read("ran"), "workspace-ran\n");

    // The snapshot names the workspace and its hash, never its run line.
    let path = ctl(&session, "ctl session save", "saved").unwrap();
    let saved = fs::read_to_string(path.trim()).unwrap();
    let snapshot: serde_json::Value = serde_json::from_str(&saved).unwrap();
    let root = fs::canonicalize(&project).unwrap();
    // The refused workspace's pane, then this one's, and no shell of the core's own.
    assert_eq!(snapshot["workspaces"][1]["root"], root.to_str().unwrap());
    assert_eq!(snapshot["workspaces"][1]["panes"], serde_json::json!([1]));
    assert_eq!(snapshot["panes"].as_array().unwrap().len(), 2, "{saved}");
    assert!(!saved.contains("space-ran"), "{saved}");
    let core = session.read("core");
    assert!(kill(&core).status().unwrap().success());
    let alive = || {
        let check = format!("kill -0 {core} 2>/dev/null");
        Command::new("/bin/sh")
            .args(["-c", &check])
            .status()
            .unwrap()
            .success()
    };
    let start = Instant::now();
    while alive() && start.elapsed() < PATIENCE {
        thread::sleep(Duration::from_millis(20));
    }

    // Each restore types the trusted line once; a probe typed after it shows when it would have run.
    let restore = |ran: &str| {
        let mut core = session.start_core(&[], "exec /bin/sh").unwrap();
        // Until the core answers, a launcher would start one of its own.
        let start = Instant::now();
        while serde_json::from_str::<serde_json::Value>(
            &ctl(&session, "ctl list --json", "list").unwrap(),
        )
        .is_err()
        {
            assert!(start.elapsed() < PATIENCE, "{}", session.read_log());
            thread::sleep(Duration::from_millis(20));
        }
        let mut launcher = session.launcher("/bin/sh");
        let launched = launcher
            .env("XDG_STATE_HOME", &state)
            .env("HOME", &state)
            .output()
            .unwrap();
        assert!(launched.status.success(), "{launched:?}");
        let start = Instant::now();
        while probes(&session.read("ran")) < probes(ran) {
            assert!(start.elapsed() < PATIENCE, "{}", session.read("ran"));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.read("ran"), ran);
        let _ = ctl(&session, "ctl session save", "saved").unwrap();
        core.kill().unwrap();
        core.wait().unwrap();
    };
    restore("workspace-ran\nworkspace-ran\nprobe\n");
    // A changed file is no longer trusted: its line does not run again.
    fs::write(
        project.join(".pinfold/layout.toml"),
        format!("{layout}# changed\n"),
    )
    .unwrap();
    restore("workspace-ran\nworkspace-ran\nprobe\nprobe\n");
    fs::remove_dir_all(&project).unwrap();
}

#[test]
fn a_trusted_run_line_runs_in_its_layouts_directory_not_the_callers() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored owner_window_process",
        exe.display()
    );
    let session = Session::new("wd", Some(&window)).unwrap();
    let project = session.directory.join("project");
    fs::create_dir_all(project.join(".pinfold")).unwrap();
    fs::create_dir_all(project.join("sub")).unwrap();
    let root = fs::canonicalize(&project).unwrap();
    let layout = "[[tabs]]\n[[tabs.panes]]\nrun = 'pwd -P > \"$PINFOLD_TEST_CLIENT/ran\"'\n";
    fs::write(project.join(".pinfold/layout.toml"), layout).unwrap();
    let sha256 = pinfold_api::trust::sha256(layout.as_bytes());
    // Trusted as `pinfold up`'s prompt would record it, in the core's state directory.
    let state = session
        .directory
        .join("state")
        .join(if cfg!(target_os = "macos") {
            "Library/Application Support/PinFold"
        } else {
            "pinfold"
        });
    let trusted =
        pinfold_api::trust::record(&state.join("trust.json"), root.to_str().unwrap(), &sha256);
    trusted.unwrap();
    let mut core = session
        .start_core(&["--window", "1", "80x24"], "exec /bin/sh")
        .unwrap();
    assert!(session.wait_for("window"), "{}", session.read_log());

    // The caller asks for the pane in `sub`, where the prompt never said the line would run.
    let open = call(
        2,
        "workspace.open",
        serde_json::json!({"root": root, "sha256": sha256,
            "layout": {"tabs": [{"panes": [{"cwd": root.join("sub")}]}]}}),
    );
    let mut outside = Command::new(env::current_exe().unwrap());
    outside
        .args(["--exact", "--ignored", "workspace_open_process"])
        .env("XDG_RUNTIME_DIR", session.directory.join("run"))
        .env(WORKSPACE, &session.directory)
        .env("PINFOLD_TEST_OPEN", open.to_string());
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    assert!(session.wait_for("opened"), "{}", session.read_log());
    let opened = session.read("opened");
    assert!(opened.contains("\"opened\":1"), "{opened}");
    assert!(session.wait_for("ran"), "{}", session.read_log());
    assert_eq!(session.read("ran"), format!("{}\n", root.display()));
    core.kill().unwrap();
    core.wait().unwrap();
}

/// Not a test: the terminal outside any pane of
/// [`a_trusted_run_line_runs_in_its_layouts_directory_not_the_callers`]. It sends the request
/// it is given and notes the answer.
#[test]
#[ignore = "run by a_trusted_run_line_runs_in_its_layouts_directory_not_the_callers"]
fn workspace_open_process() {
    let (Some(directory), Ok(open)) = (helper(WORKSPACE), env::var("PINFOLD_TEST_OPEN")) else {
        return;
    };
    let open: serde_json::Value = serde_json::from_str(&open).unwrap();
    // The core may not be listening yet.
    let start = Instant::now();
    let mut lines = loop {
        match api(std::slice::from_ref(&open)) {
            Ok(lines) => break lines,
            Err(error) if start.elapsed() > PATIENCE => panic!("{error}"),
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    };
    next(&mut lines, || Ok(())).unwrap();
    let opened = next(&mut lines, || Ok(())).unwrap();
    note(Path::new(&directory).join("opened"), opened.to_string()).unwrap();
}

fn probes(ran: &str) -> usize {
    ran.matches("probe").count()
}

/// The directory of the sandbox run in `directory`: under the target, not the temporary
/// directory, which the sandbox lets an agent write on macOS and hides on Linux. It holds
/// `out`, where no agent may write, and `proj/work`, the first agent's working directory.
fn sandbox_base(directory: &Path) -> PathBuf {
    let name = directory.file_name().unwrap_or_default();
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Not a test: each sandboxed agent of
/// [`a_sandboxed_agent_starts_nothing_outside_its_sandbox`], which notes what it heard in its
/// working directory. The first spawns a shell and types a write outside into it, launches an
/// unsandboxed profile, then a sandboxed one, the second, which does the same, and last moves
/// its reported directory up and launches a third there.
#[test]
#[ignore = "run by a_sandboxed_agent_starts_nothing_outside_its_sandbox"]
fn boxed_process() {
    if env::var_os(CLIENT).is_none() {
        return;
    }
    let work = env::current_dir().unwrap();
    // A third agent, which starts above the work directory.
    if !work.ends_with("work") {
        fs::write(work.join("level3"), "").unwrap();
        thread::sleep(Duration::from_secs(30));
        return;
    }
    let wide = work.parent().unwrap().to_path_buf();
    let out = wide.parent().unwrap().join("out");
    let touch = |name: &str| format!("touch '{}'\n", out.join(name).display());
    let mut heard = String::new();
    let spawn_and_type = |heard: &mut String, name: &str| {
        let spawned = ask("pane.spawn", serde_json::json!({"where": "tab"})).unwrap();
        writeln!(heard, "{spawned}").unwrap();
        if let Some(pane) = spawned["result"]["pane"].as_u64() {
            let text = touch(name);
            let sent = ask("pane.send", serde_json::json!({"pane": pane, "text": text}));
            writeln!(heard, "{}", sent.unwrap()).unwrap();
        }
    };
    if fs::create_dir(work.join("b1")).is_ok() {
        spawn_and_type(&mut heard, "child");
        let plain = ask("agent.launch", serde_json::json!({"profile": "plain"})).unwrap();
        let boxed = ask("agent.launch", serde_json::json!({"profile": "boxed"})).unwrap();
        writeln!(heard, "{plain}\n{boxed}").unwrap();
        assert!(wait_file(&work.join("level2")));
        // Its directory, as OSC 7 reports it, moves above the one it was given.
        let wide = wide.to_str().unwrap();
        let mut stdout = std::io::stdout();
        let osc = format!("\x1b]7;file://{wide}\x07");
        std::io::Write::write_all(&mut stdout, osc.as_bytes()).unwrap();
        std::io::Write::flush(&mut stdout).unwrap();
        let mut hello = api(&[call(2, "pane.list", serde_json::json!({}))]).unwrap();
        let own = next(&mut hello, || Ok(())).unwrap()["result"]["pane"].clone();
        let start = Instant::now();
        while start.elapsed() < PATIENCE {
            let list = ask("pane.list", serde_json::json!({})).unwrap();
            let panes = list["result"]["panes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if panes
                .iter()
                .any(|pane| pane["id"] == own && pane["cwd"] == wide)
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let wider = ask("agent.launch", serde_json::json!({"profile": "boxed"})).unwrap();
        writeln!(heard, "{wider}").unwrap();
        fs::write(work.join("level1"), heard).unwrap();
    } else if fs::create_dir(work.join("b2")).is_ok() {
        spawn_and_type(&mut heard, "grandchild");
        fs::write(work.join("level2"), heard).unwrap();
    }
    thread::sleep(Duration::from_secs(30));
}

fn wait_file(path: &Path) -> bool {
    let start = Instant::now();
    while start.elapsed() < PATIENCE && !path.exists() {
        thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

#[test]
fn a_sandboxed_agent_starts_nothing_outside_its_sandbox() {
    let bwrap = ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"];
    if cfg!(target_os = "linux") && !bwrap.iter().any(|path| Path::new(path).exists()) {
        // CI must run the real bwrap (#106), so only a local machine may skip.
        assert!(env::var_os("CI").is_none(), "no bwrap");
        eprintln!("skipped: no bwrap");
        return;
    }
    let session = Session::new("sx", None).unwrap();
    let base = sandbox_base(&session.directory);
    let _ = fs::remove_dir_all(&base);
    let (out, wide) = (base.join("out"), base.join("proj"));
    let work = wide.join("work");
    for dir in [&out, &work] {
        fs::create_dir_all(dir).unwrap();
    }
    for dir in [&base, &wide, &work] {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let (wide, work) = (wide.canonicalize().unwrap(), work.canonicalize().unwrap());
    let exe = env::current_exe().unwrap();
    let toml = format!(
        "[agents.boxed]\ncommand = [\"{}\", \"--exact\", \"--ignored\", \"boxed_process\"]\nsandbox = true\n\
         [agents.plain]\ncommand = [\"/usr/bin/touch\", \"{}\"]\n",
        exe.display(),
        out.join("plain").display()
    );
    fs::create_dir_all(session.directory.join("config/pinfold")).unwrap();
    fs::write(session.directory.join("config/pinfold/pinfold.toml"), toml).unwrap();
    let pinfold = format!("'{}'", session.directory.join("pinfold").display());
    let client = "\"$PINFOLD_TEST_CLIENT\"";
    // The first pane launches the first agent from the work directory; every other is a shell.
    let shell = format!(
        "if mkdir {client}/first; then cd '{}'; {pinfold} agent boxed --tab 2> {client}/e; \
         exec sleep 60; fi; exec /bin/sh",
        work.display()
    );
    // The core's HOME is the base: a sandboxed agent's working directory must be strictly inside
    // home, and this one must also be outside the temporary directory.
    let mut core = session
        .start_core_at(&["--idle", "1"], &shell, &base)
        .unwrap();
    let done = wait_file(&work.join("level1"));
    // Long enough for a shell to run a typed line.
    thread::sleep(Duration::from_secs(2));
    core.kill().unwrap();
    core.wait().unwrap();
    let read = |path: PathBuf| fs::read_to_string(path).unwrap_or_default();
    let (level1, level2) = (read(work.join("level1")), read(work.join("level2")));
    let written: Vec<_> = ["child", "grandchild", "plain"]
        .into_iter()
        .filter(|name| out.join(name).exists())
        .collect();
    let level3 = wide.join("level3").exists();
    let _ = fs::remove_dir_all(&base);
    assert!(done, "{} {}", session.read("e"), session.read_log());
    assert!(written.is_empty(), "{written:?}\n{level1}\n{level2}");
    assert!(!level3, "{level1}");
    let replies: Vec<serde_json::Value> = level1
        .lines()
        .chain(level2.lines())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    // Spawn, plain and wider launches are refused; the sandboxed launch is not.
    let refused = |reply: &serde_json::Value| {
        let message = reply["error"]["message"].as_str().unwrap_or_default();
        message.contains("start only sandboxed agents")
    };
    assert_eq!(replies.len(), 5, "{level1}\n{level2}");
    for index in [0, 1, 3, 4] {
        assert!(refused(&replies[index]), "{index}: {level1}\n{level2}");
    }
    assert!(replies[2]["result"]["pane"].is_u64(), "{level1}");
}

/// Each key whose value changes from run to run or machine to machine. An audit entry's `seq`
/// counts the polls before it.
const VOLATILE: [&str; 8] = [
    "at_ms", "command", "cwd", "hash", "path", "prev", "seq", "title",
];

/// `value` with each [`VOLATILE`] field's value replaced by its name in brackets, at any depth.
fn redacted(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, field) in fields.iter_mut() {
                if VOLATILE.contains(&key.as_str()) && !field.is_null() {
                    *field = format!("[{key}]").into();
                }
                redacted(field);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redacted),
        _ => {}
    }
}

/// Asks each of `calls` once and notes the method beside each reply in `name`.
fn replies(directory: &Path, name: &str, calls: Vec<(&str, serde_json::Value)>) -> Result {
    let hello = next(&mut api(&[])?, || Ok(()))?;
    let mut said = vec![serde_json::json!({"method": "hello", "reply": hello})];
    for (method, params) in calls {
        said.push(serde_json::json!({"method": method, "reply": ask(method, params)?}));
    }
    Ok(note(directory.join(name), serde_json::to_string(&said)?)?)
}

/// Not a test: the window of [`every_api_method_replies_as_its_snapshot`], which allows every
/// prompt until the replies are noted.
#[test]
#[ignore = "run by every_api_method_replies_as_its_snapshot"]
fn allow_window_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    note(
        directory.join("core"),
        std::os::unix::process::parent_id().to_string(),
    )
    .unwrap();
    let mut link = Link::new(socket::take_link().unwrap()).unwrap();
    assert!(link.resize(24, 80));
    until(&mut link, |link| link.is_greeted()).unwrap();
    note(directory.join("window"), "yes").unwrap();
    until(&mut link, |link| {
        while let Some(prompt) = link.take_approval() {
            let (id, answer) = (prompt.id, ApprovalAnswer::AllowOnce);
            assert!(link.send(Request::ApprovalAnswer { id, answer }));
        }
        directory.join("outside").exists() || link.disconnected().is_some()
    })
    .unwrap();
}

/// Not a test: pane 0 of [`every_api_method_replies_as_its_snapshot`]. It calls each method a
/// pane may, then starts a terminal of its own for [`replies_outside_process`].
#[test]
#[ignore = "run by every_api_method_replies_as_its_snapshot"]
fn replies_pane_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let directory = Path::new(&directory);
    while !directory.join("window").exists() {
        thread::sleep(Duration::from_millis(10));
    }
    // Pane 1 prints `out` between OSC 133 marks once, so its reads never change.
    let spawned = ask("pane.spawn", serde_json::json!({"where": "tab"})).unwrap();
    assert_eq!(spawned["result"]["pane"], 1, "{spawned}");
    let start = Instant::now();
    while ask("pane.last_output", serde_json::json!({"pane": 1})).unwrap()["result"].is_null() {
        assert!(start.elapsed() < PATIENCE, "pane 1 printed nothing");
        thread::sleep(Duration::from_millis(10));
    }
    let calls = vec![
        ("window.open", serde_json::json!({})),
        ("pane.list", serde_json::json!({})),
        ("pane.read", serde_json::json!({"pane": 1})),
        ("pane.last_output", serde_json::json!({"pane": 1})),
        ("pane.send", serde_json::json!({"pane": 1, "text": "x"})),
        (
            "pane.wait",
            serde_json::json!({"pane": "self", "until": "prompt", "timeout_ms": 100}),
        ),
        (
            "pane.status",
            serde_json::json!({"pane": 0, "state": "running"}),
        ),
        (
            "events.subscribe",
            serde_json::json!({"events": ["pane.closed"]}),
        ),
        ("agent.launch", serde_json::json!({"profile": "plain"})),
        ("pane.close", serde_json::json!({"pane": 2})),
    ];
    replies(directory, "inside", calls).unwrap();
    let mut outside = Command::new(env::current_exe().unwrap());
    outside.args(["--exact", "--ignored", "replies_outside_process"]);
    let _terminal = pinfold_sys::pty::Pty::spawn(outside, 24, 80).unwrap();
    while !directory.join("outside").exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Not a test: in a terminal outside every pane, calls each owner method.
#[test]
#[ignore = "run by replies_pane_process"]
fn replies_outside_process() {
    let Some(directory) = helper(CLIENT) else {
        return;
    };
    let root = fs::canonicalize(&directory).unwrap();
    let workspace = serde_json::json!({"root": root, "sha256": "a".repeat(64)});
    let mut trust = workspace.clone();
    trust["runs"] = serde_json::json!(["make"]);
    let layout = serde_json::json!({"tabs": [{"panes": [{"cwd": root}]}]});
    let calls = vec![
        ("pane.spawn", serde_json::json!({"where": "tab"})),
        ("grants.list", serde_json::json!({})),
        ("grants.revoke", serde_json::json!({"id": 7})),
        ("session.save", serde_json::json!({})),
        ("workspace.trust_state", workspace),
        ("workspace.ask_trust", trust),
        (
            "workspace.open",
            serde_json::json!({"root": root, "layout": layout}),
        ),
        ("audit.read", serde_json::json!({"count": 2})),
    ];
    replies(&directory, "outside", calls).unwrap();
}

#[test]
fn every_api_method_replies_as_its_snapshot() {
    let exe = env::current_exe().unwrap();
    let window = format!(
        "exec '{}' --exact --ignored allow_window_process",
        exe.display()
    );
    let session = Session::new("rs", Some(&window)).unwrap();
    fs::create_dir_all(session.directory.join("config/pinfold")).unwrap();
    let toml = "[agents.plain]\ncommand = [\"/bin/sh\", \"-c\", \"exec sleep 30\"]\n";
    fs::write(session.directory.join("config/pinfold/pinfold.toml"), toml).unwrap();
    let shell = format!(
        "if mkdir \"$PINFOLD_TEST_CLIENT/first\" 2> /dev/null; then exec '{}' --exact \
         --ignored replies_pane_process; fi; printf '\\033]133;C\\007out\\n\\033]133;D;0\\007'; exec sleep 30",
        exe.display()
    );
    let mut core = session
        .start_core(&["--window", "1", "80x24"], &shell)
        .unwrap();
    assert!(session.wait_for("outside"), "{}", session.read_log());
    core.kill().unwrap();
    core.wait().unwrap();
    for name in ["inside", "outside"] {
        let mut said: serde_json::Value = serde_json::from_str(&session.read(name)).unwrap();
        redacted(&mut said);
        let said = serde_json::to_string_pretty(&said).unwrap();
        insta::assert_snapshot!(format!("replies_{name}"), said);
    }
}

#[test]
fn the_documented_agent_hooks_set_their_panes_status() {
    let session = Session::new("hk", None).unwrap();
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/hooks");
    let claude = fs::read_to_string(docs.join("claude-code.json")).unwrap();
    let readme = fs::read_to_string(docs.join("README.md")).unwrap();
    assert!(
        readme.contains(&claude),
        "the README shows claude-code.json as it is"
    );
    let claude: serde_json::Value = serde_json::from_str(&claude).unwrap();
    // Claude Code runs a hook's command with a shell, its event as JSON on stdin.
    let hook = |event: &str| {
        let command = claude["hooks"][event][0]["hooks"][0]["command"].as_str();
        format!("echo '{{}}' | {{ {}; }}", command.unwrap())
    };
    // Codex runs `notify` as argv, with the event as JSON after it. TOML's array is JSON's here.
    let codex = fs::read_to_string(docs.join("codex.toml")).unwrap();
    let notify = codex.lines().find(|line| line.starts_with("notify = "));
    assert!(
        readme.contains(notify.unwrap()),
        "the README shows codex.toml's line"
    );
    let notify = notify.and_then(|line| line.strip_prefix("notify = "));
    let notify: Vec<String> = serde_json::from_str(notify.unwrap()).unwrap();
    let quoted: Vec<String> = notify.iter().map(|word| format!("'{word}'")).collect();
    let notify = format!(
        "{} '{{\"type\":\"agent-turn-complete\"}}'",
        quoted.join(" ")
    );

    let list = "pinfold ctl list --json";
    let steps = [
        hook("Notification"),
        hook("PostToolUse"),
        hook("Stop"),
        hook("UserPromptSubmit"),
        notify,
    ];
    let mut shell = format!("PATH='{}':$PATH", session.directory.display());
    for (index, step) in steps.iter().enumerate() {
        shell += &format!("; {step} && {}", into(list, &index.to_string()));
    }
    shell += "; exec sleep 30";
    let mut core = session.start_core(&["--idle", "1"], &shell).unwrap();
    assert!(session.wait_for("4"), "{}", session.read_log());

    let shown: Vec<(String, String)> = (0..steps.len())
        .map(|index| {
            let list: serde_json::Value =
                serde_json::from_str(&session.read(&index.to_string())).unwrap();
            let pane = &list["panes"][0];
            (pane["status"].to_string(), pane["source"].to_string())
        })
        .collect();
    let reported = |status: &str| (format!("\"{status}\""), "\"reported\"".to_owned());
    assert_eq!(
        shown,
        ["needs-you", "running", "done", "running", "done"].map(reported)
    );
    core.kill().unwrap();
    core.wait().unwrap();
}
