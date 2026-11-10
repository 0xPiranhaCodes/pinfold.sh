//! Watches the config file and the theme file in use from a thread of their own, so a slow
//! disk cannot stall the window.

use std::io::{self, Write};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use pinfold_config::theme::{self, Theme};
use pinfold_config::{Config, Watch};

/// Costs an idle window one wakeup and two `stat`s each period.
const POLL: Duration = Duration::from_secs(1);

/// What a save changed.
pub enum Saved {
    Config(Config),
    /// The theme the config file now chooses, with its `[colors]` on top.
    Theme(Theme),
}

/// Polls once. Returns whether to go on, which is until `send` finds the window gone.
fn check(watch: &mut Watch, theme: &mut theme::Watch, send: &impl Fn(Saved) -> bool) -> bool {
    let config = watch.poll();
    let saved = config.is_some();
    let open = match config {
        None => true,
        Some(Ok(config)) => send(Saved::Config(config)),
        Some(Err(error)) => {
            // shortcut: only the log hears this, so no one sees it, draw it in the chrome.
            let _ = writeln!(
                io::stderr(),
                "pinfold: keeping the current settings: {error}"
            );
            true
        }
    };
    open && told(theme.poll(saved), send)
}

/// Sends the theme a poll loaded, or says why the one in use stays. Returns whether to go on.
fn told(
    loaded: Option<Result<(Theme, Vec<String>), theme::Error>>,
    send: &impl Fn(Saved) -> bool,
) -> bool {
    match loaded {
        None => true,
        Some(Ok((theme, notes))) => {
            for note in notes {
                let _ = writeln!(io::stderr(), "pinfold: theme: {note}");
            }
            send(Saved::Theme(theme))
        }
        Some(Err(error)) => {
            let _ = writeln!(
                io::stderr(),
                "pinfold: theme: keeping the current theme: {error}"
            );
            true
        }
    }
}

/// Calls `send` with the new settings each time the config file is saved well, and with the
/// new theme each time it or the theme file in use is (ADR-0049, section 2).
///
/// The sender takes the system's appearance, `true` for dark, at start and on each change:
/// the half of `theme = { light, dark }` it chooses is then sent as a saved theme is.
/// Dropping it ends the thread.
///
/// # Errors
///
/// Returns an error if the thread cannot start.
pub fn watch(send: impl Fn(Saved) -> bool + Send + 'static) -> io::Result<Sender<bool>> {
    let (appearance, changes) = mpsc::channel();
    let Some(path) = pinfold_config::path() else {
        return Ok(appearance);
    };
    let mut theme = theme::Watch::new(path.clone());
    let mut watch = Watch::new(path);
    thread::Builder::new()
        .name("pinfold-config".into())
        .spawn(move || {
            while check(&mut watch, &mut theme, &send) {
                // An appearance ends the wait early, so it applies at once.
                match changes.recv_timeout(POLL) {
                    Ok(dark) if !told(theme.follow(dark), &send) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
            }
        })?;
    Ok(appearance)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::{env, process};

    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("pinfold-reload-{}-{name}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Saves by rename, as an editor does.
    fn save(path: &Path, text: &str) {
        let new = path.with_extension("new");
        fs::write(&new, text).unwrap();
        fs::set_permissions(&new, Permissions::from_mode(0o600)).unwrap();
        fs::rename(&new, path).unwrap();
    }

    /// The font size or the background of each thing sent.
    fn sent(path: &Path, saves: &[&str]) -> Vec<String> {
        let sent = RefCell::new(Vec::new());
        let send = |saved: Saved| {
            sent.borrow_mut().push(match saved {
                Saved::Config(config) => format!("font {}", config.font_size),
                Saved::Theme(theme) => format!("ground {:06x}", theme.background),
            });
            true
        };
        let (mut watch, mut theme) = (Watch::new(path.into()), theme::Watch::new(path.into()));
        for text in saves {
            save(path, text);
            assert!(check(&mut watch, &mut theme, &send));
        }
        sent.into_inner()
    }

    #[test]
    fn a_bad_save_sends_nothing_and_a_good_one_is_sent() {
        let dir = dir("bad-save");
        let path = dir.join("pinfold.toml");
        save(&path, "font_size = 20");
        let sent = sent(&path, &["font_size = oops", "font_size = 21"]);
        assert_eq!(sent, ["font 21", "ground 121212"]);
        fs::remove_dir_all(dir).unwrap();
    }

    /// ADR-0049, section 2: a bad theme keeps the one in use, and the other keys still apply.
    #[test]
    fn a_bad_theme_sends_the_other_keys_alone_and_a_good_one_is_sent() {
        let dir = dir("theme");
        let path = dir.join("pinfold.toml");
        save(&path, "");
        let bad = "font_size = 22\n[colors]\nbackground = \"oops\"";
        let good = "font_size = 22\n[colors]\nbackground = \"#0d1117\"";
        let sent = sent(&path, &[bad, good]);
        assert_eq!(sent, ["font 22", "font 22", "ground 0d1117"]);
        fs::remove_dir_all(dir).unwrap();
    }

    /// ADR-0049, section 2: the system's appearance picks a half of `theme = { light, dark }`,
    /// and a half that cannot be used keeps the theme in use.
    #[test]
    fn an_appearance_sends_its_theme_and_a_bad_half_sends_nothing() {
        let dir = dir("appearance");
        let path = dir.join("pinfold.toml");
        save(
            &path,
            "theme = { light = \"GitHub Light\", dark = \"Nowhere\" }",
        );
        let mut theme = theme::Watch::new(path.clone());
        let sent = RefCell::new(Vec::new());
        let send = |saved: Saved| {
            let Saved::Theme(theme) = saved else {
                return false;
            };
            sent.borrow_mut().push(theme.background);
            true
        };
        for dark in [false, false, true, false] {
            assert!(told(theme.follow(dark), &send));
        }
        // Light, nothing for the bad dark half, and light again.
        assert_eq!(sent.into_inner(), [0x00ff_ffff, 0x00ff_ffff]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_closed_window_stops_the_thread() {
        let dir = dir("closed");
        let path = dir.join("pinfold.toml");
        let (mut watch, mut theme) = (Watch::new(path.clone()), theme::Watch::new(path.clone()));
        save(&path, "");
        assert!(!check(&mut watch, &mut theme, &|_| false));
        fs::remove_dir_all(dir).unwrap();
    }
}
