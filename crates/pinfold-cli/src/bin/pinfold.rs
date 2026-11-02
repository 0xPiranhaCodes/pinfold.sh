//! The `pinfold` launcher: starts the core if none answers, asks it for a window, and exits.

use std::error::Error;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use pinfold_api::control;

/// How long a core that was just started gets to answer.
const START_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

/// Connects to a core at `path`. Nothing listening is no error, as that is what starts one.
fn connect(path: &Path) -> std::io::Result<Option<UnixStream>> {
    match UnixStream::connect(path) {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Connects to the core in `directory`, and starts the one beside this binary if none answers.
/// Two launchers can both start one: the second core exits on the lock, and both connect to the
/// first, so its exit status means nothing and only the socket decides. The second also empties
/// the log, moments after the first core started, when there is next to nothing in it.
fn core(directory: &Path) -> Result<UnixStream, Box<dyn Error>> {
    let path = &control::socket_path(directory);
    if let Some(stream) = connect(path)? {
        return Ok(stream);
    }
    let binary = std::env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-core");
    // Windows inherit this and panes do not, as a pane's stderr is its terminal. A log that will
    // not open is no reason to go without a core.
    let log = control::start_log(directory).map_or_else(|_| Stdio::null(), Stdio::from);
    pinfold_sys::spawn_detached(Command::new(&binary), log)
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
    let start = Instant::now();
    while start.elapsed() < START_WAIT {
        if let Some(stream) = connect(path)? {
            return Ok(stream);
        }
        thread::sleep(POLL);
    }
    Err(format!("no core answered at {}", path.display()).into())
}

fn launch() -> Result<(), Box<dyn Error>> {
    control::open_window(&core(&control::runtime_dir())?)?;
    Ok(())
}

fn main() -> ExitCode {
    if std::env::args().len() > 1 {
        eprintln!("usage: pinfold");
        return ExitCode::FAILURE;
    }
    match launch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pinfold: {error}");
            // A core started from a terminal logs there, so this may be an older core's.
            let log = control::log_path(&control::runtime_dir());
            if log.exists() {
                eprintln!("pinfold: see {}", log.display());
            }
            ExitCode::FAILURE
        }
    }
}
