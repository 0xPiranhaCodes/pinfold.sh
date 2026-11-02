//! Watches the config file from a thread of its own, so a slow disk cannot stall the window.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use pinfold_config::{Config, Watch};

/// Costs an idle window one wakeup and one `stat` each period.
const POLL: Duration = Duration::from_secs(1);

/// Polls once. Returns whether to go on, which is until `send` finds the window gone.
fn check(watch: &mut Watch, send: &impl Fn(Config) -> bool) -> bool {
    match watch.poll() {
        None => true,
        Some(Ok(config)) => send(config),
        Some(Err(error)) => {
            // shortcut: only the log hears this, so no one sees it, draw it in the chrome.
            let _ = writeln!(
                io::stderr(),
                "pinfold: keeping the current settings: {error}"
            );
            true
        }
    }
}

/// Calls `send` with the new settings each time the config file is saved well.
///
/// # Errors
///
/// Returns an error if the thread cannot start.
pub fn watch(send: impl Fn(Config) -> bool + Send + 'static) -> io::Result<()> {
    let Some(path) = pinfold_config::path() else {
        return Ok(());
    };
    let mut watch = Watch::new(path);
    thread::Builder::new()
        .name("pinfold-config".into())
        .spawn(move || {
            while check(&mut watch, &send) {
                thread::sleep(POLL);
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::{env, process};

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("pinfold-reload-{}-{name}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_bad_save_sends_nothing_and_a_good_one_is_sent() {
        let dir = dir("bad-save");
        let path = dir.join("pinfold.toml");
        fs::write(&path, "font_size = 20").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        let sent = RefCell::new(Vec::new());
        let send = |config: Config| {
            sent.borrow_mut().push(config.font_size);
            true
        };
        let mut watch = Watch::new(path.clone());
        fs::write(&path, "font_size = oops").unwrap();
        assert!(check(&mut watch, &send));
        assert!(sent.borrow().is_empty());
        let new = path.with_extension("new");
        fs::write(&new, "font_size = 21").unwrap();
        fs::set_permissions(&new, Permissions::from_mode(0o600)).unwrap();
        fs::rename(&new, &path).unwrap();
        assert!(check(&mut watch, &send));
        assert_eq!(*sent.borrow(), [21.0]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_closed_window_stops_the_thread() {
        let dir = dir("closed");
        let path = dir.join("pinfold.toml");
        let mut watch = Watch::new(path.clone());
        fs::write(&path, "").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        assert!(!check(&mut watch, &|_| false));
        fs::remove_dir_all(dir).unwrap();
    }
}
