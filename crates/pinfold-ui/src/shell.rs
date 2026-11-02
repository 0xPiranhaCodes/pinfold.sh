//! The command that starts a window's shell.
//!
//! The Dock and Finder start an app in `/` with a bare environment, so on macOS the shell
//! runs as a login shell, which reads the profile that sets `PATH`, as Terminal's does.

use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Used only if no locale is set, so text in UTF-8 is not shown as question marks.
// shortcut: English whatever the system language, read the macOS locale when a user needs another.
const LANG: &str = "en_US.UTF-8";

/// Returns the command for the user's shell.
pub fn command() -> Command {
    let shell = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut command = Command::new(&shell);
    if cfg!(target_os = "macos") {
        command.arg0(login_name(&shell));
        let here = env::current_dir().ok();
        if let Some(directory) = start_directory(here.as_deref(), env::var_os("HOME")) {
            command.current_dir(directory);
        }
        if ["LANG", "LC_ALL", "LC_CTYPE"]
            .iter()
            .all(|name| env::var_os(name).is_none())
        {
            command.env("LANG", LANG);
        }
    }
    command
}

/// Returns a shell's name with the leading dash that makes it a login shell.
fn login_name(shell: &OsStr) -> OsString {
    let mut name = OsString::from("-");
    name.push(Path::new(shell).file_name().unwrap_or(shell));
    name
}

/// Returns where to start instead of `here`: home, if an app launch left us at the root.
fn start_directory(here: Option<&Path>, home: Option<OsString>) -> Option<PathBuf> {
    (here == Some(Path::new("/"))).then_some(home?.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_shell_is_named_with_a_dash() {
        assert_eq!(login_name("/bin/zsh".as_ref()), "-zsh");
        assert_eq!(login_name("fish".as_ref()), "-fish");
        assert_eq!(login_name("/".as_ref()), "-/");
    }

    #[test]
    fn only_a_launch_at_the_root_moves_home() {
        let root = Some(Path::new("/"));
        let home = || Some(OsString::from("/Users/me"));
        assert_eq!(start_directory(root, home()), Some("/Users/me".into()));
        assert_eq!(start_directory(Some(Path::new("/tmp")), home()), None);
        assert_eq!(start_directory(None, home()), None);
        assert_eq!(start_directory(root, None), None);
    }
}
