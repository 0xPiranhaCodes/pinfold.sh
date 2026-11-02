//! The `pinfold` launcher: starts the core if none answers, asks it for a window, and exits.

use std::error::Error;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode};
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

/// Connects to the core at `path`, and starts the one beside this binary if none answers. Two
/// launchers can both start one: the second core exits on the lock, and both connect to the
/// first, so its exit status means nothing and only the socket decides.
fn core(path: &Path) -> Result<UnixStream, Box<dyn Error>> {
    if let Some(stream) = connect(path)? {
        return Ok(stream);
    }
    let binary = std::env::current_exe()?
        .canonicalize()?
        .with_file_name("pinfold-core");
    // shortcut: the core's stderr goes nowhere, and a window's with it, so a launcher hears that
    // a window ended and not why, keep a log in the runtime directory once tracing reaches them.
    pinfold_sys::spawn_detached(Command::new(&binary))
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
    let path = control::socket_path(&control::runtime_dir());
    control::open_window(&core(&path)?)?;
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
            ExitCode::FAILURE
        }
    }
}
