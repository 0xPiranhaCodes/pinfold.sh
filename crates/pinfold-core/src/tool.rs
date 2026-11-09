//! Built-in tool panes (ADR-0045): `pinfold md`, `files` and `diff`, each a program in a pane
//! of its own. The core starts its sibling `pinfold`, never one found through `PATH`, with a
//! fixed argument list and the caller's values after `--`. Only the window's keys reach a tool:
//! [`Reactor::send`](crate::io::Reactor::send) refuses every API caller, so none can confirm a
//! tool's prompt.

use std::path::Path;
use std::process::Command;

use pinfold_api::tool::{Run, Tool};

/// Why `pane.send` to a tool pane is refused.
pub const NO_SEND: &str = "a tool pane takes keys from the window alone, never pane.send";

/// Returns `run` as a command of `binary`, started in the directory it shows: the file's for
/// `md`, the directory itself for `files` and `diff`. Checks first that `path` is a file for
/// `md` and a directory for the others, so a refused call starts nothing.
///
/// # Errors
///
/// Returns why not, naming no path.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use pinfold_api::tool::{Run, Tool};
///
/// let run = Run { tool: Tool::Files, path: "/".into(), staged: false, range: None };
/// let command = pinfold_core::tool::command(Path::new("/opt/pinfold"), &run).unwrap();
/// assert_eq!(command.get_current_dir(), Some(Path::new("/")));
/// let run = Run { tool: Tool::Md, ..run };
/// assert!(pinfold_core::tool::command(Path::new("/opt/pinfold"), &run).is_err());
/// ```
pub fn command(binary: &Path, run: &Run) -> Result<Command, &'static str> {
    let path = Path::new(&run.path);
    let cwd = match run.tool {
        Tool::Md if path.is_file() => path.parent().unwrap_or(path),
        Tool::Md => return Err("md opens an existing file"),
        Tool::Files | Tool::Diff if path.is_dir() => path,
        Tool::Files | Tool::Diff => return Err("files and diff open an existing directory"),
    };
    let mut command = Command::new(binary);
    command.args(run.args()).current_dir(cwd);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::time::Instant;
    use std::{env, fs};

    use super::*;
    use crate::io::Reactor;

    fn run(tool: Tool, path: &Path) -> Run {
        let path = path.to_str().unwrap().to_owned();
        Run {
            tool,
            path,
            staged: false,
            range: None,
        }
    }

    #[test]
    fn a_path_of_the_wrong_kind_starts_nothing() {
        let directory = env::temp_dir().join(format!("pinfold-tool-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("README.md");
        fs::write(&file, "# hi").unwrap();
        let missing = directory.join("missing");
        let binary = Path::new("/nowhere/pinfold");
        for (tool, path) in [
            (Tool::Md, &directory),
            (Tool::Md, &missing),
            (Tool::Files, &file),
            (Tool::Files, &missing),
            (Tool::Diff, &file),
            (Tool::Diff, &missing),
        ] {
            assert!(command(binary, &run(tool, path)).is_err(), "{tool:?}");
        }
        let md = command(binary, &run(Tool::Md, &file)).unwrap();
        assert_eq!(md.get_program(), binary);
        assert_eq!(md.get_current_dir(), Some(directory.as_path()));
        let args: Vec<&OsStr> = md.get_args().collect();
        assert_eq!(args, [OsStr::new("md"), "--".as_ref(), file.as_os_str()]);
        let diff = Run {
            staged: true,
            range: Some("main..".to_owned()),
            ..run(Tool::Diff, &directory)
        };
        let diff = command(binary, &diff).unwrap();
        let args: Vec<&OsStr> = diff.get_args().collect();
        assert_eq!(args, ["diff", "--staged", "--", "main.."]);
        assert_eq!(diff.get_current_dir(), Some(directory.as_path()));
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_tool_pane_is_listed_as_one_and_refuses_every_send() {
        let mut reactor = Reactor::new().unwrap();
        let shell = reactor.spawn(Command::new("/bin/cat"), 24, 80).unwrap();
        let tool = reactor.spawn(Command::new("/bin/cat"), 24, 80).unwrap();
        let files = run(Tool::Files, Path::new("/"));
        reactor.set_tool(tool, files.clone());
        let now = Instant::now();
        assert_eq!(reactor.send(tool, "y\n", now), Err(NO_SEND));
        assert_eq!(reactor.send_lines(tool, "y\n", now), Err(NO_SEND));
        assert_eq!(reactor.send(shell, "y\n", now), Ok(()));
        let listed: Vec<_> = reactor.list().map(|pane| pane.tool).collect();
        assert_eq!(listed, [None, Some(&files)]);
    }
}
