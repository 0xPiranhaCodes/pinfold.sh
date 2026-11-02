//! The launcher, a real core and a client on the real link, each its own process.
//!
//! No display is needed: each test copies the binaries to a directory of its own, and a script
//! stands in as the `pinfold-window` beside the core there. The core starts that script as it
//! would a window, so it gets the link on descriptor 3 and hands it to this test binary.

use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use pinfold_proto::Frame;
use pinfold_sys::socket;
use pinfold_ui::link::{Disconnect, Link};

/// Names the directory of a run, in a process acting as the window.
const CLIENT: &str = "PINFOLD_TEST_CLIENT";
const PATIENCE: Duration = Duration::from_secs(30);

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
        fs::copy(env!("CARGO_BIN_EXE_pinfold"), directory.join("pinfold"))?;
        fs::copy(
            env!("CARGO_BIN_EXE_pinfold-core"),
            directory.join("pinfold-core"),
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

    fn start_launcher(&self) -> Result<Child> {
        Ok(self.launcher("/bin/sh").spawn()?)
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
            let _ = Command::new("kill")
                .args(["-9", core.trim()])
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn shows(frame: &Frame, text: &str) -> bool {
    let cells: String = frame.cells.iter().map(|cell| cell.character).collect();
    cells.contains(text)
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
    let mut link = Link::new(socket::take_link()?, 0)?;
    if starts.lines().count() == 1 {
        assert!(link.write(b"echo mark-$((40+2)); seq 1 100\n"));
        until(&mut link, |link| shows(link.frame(), "100"))?;
        note("typed", "yes")?;
        until(&mut link, Link::take_raised)?;
        note("raised", "yes")?;
        return Ok(());
    }
    until(&mut link, |link| shows(link.frame(), "100"))?;
    assert!(link.write(b"echo again-$((1+1))\n"));
    until(&mut link, |link| shows(link.frame(), "again-2"))?;
    assert!(link.scroll(10_000));
    until(&mut link, |link| shows(link.frame(), "mark-42"))?;
    note("reattached", "yes")?;
    until(&mut link, |link| link.disconnected().is_some())?;
    assert_eq!(link.disconnected(), Some(Disconnect::Closed));
    assert!(shows(link.frame(), "mark-42"), "the last screen stays");
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
    let killed = Command::new("kill").args(["-9", core.trim()]).status();
    assert!(killed.unwrap().success());
    assert!(
        session.wait_for("ended"),
        "the client missed the core dying"
    );
}

#[test]
fn two_launchers_at_once_share_one_core_and_one_window() {
    let window = "cd \"$PINFOLD_TEST_CLIENT\"\necho $PPID > core\necho start >> starts\nexec cat <&3 >/dev/null";
    let session = Session::new("b", Some(window)).unwrap();
    let (mut first, mut second) = (
        session.start_launcher().unwrap(),
        session.start_launcher().unwrap(),
    );
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert!(session.wait_for("core"));
    assert_eq!(session.read("starts").lines().count(), 1);
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

    let path = pinfold_api::control::socket_path(&session.directory.join("run/pinfold"));
    let start = Instant::now();
    while std::os::unix::net::UnixStream::connect(&path).is_ok() {
        assert!(start.elapsed() < PATIENCE, "the core outlived its panes");
        thread::sleep(Duration::from_millis(20));
    }
}
