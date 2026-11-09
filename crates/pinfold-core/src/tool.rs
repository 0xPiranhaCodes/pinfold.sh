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

/// Returns a tool pane's title: the tool and its argument, the last name of `path` or the
/// range, escaped. A C0 control shows as `^X`, DEL as `^?`, and a C1 or bidi control as
/// `<U+202E>`, as the tools escape them (ADR-0045 section 4).
///
/// ```
/// use pinfold_api::tool::{Run, Tool};
///
/// let run = Run { tool: Tool::Md, path: "/r/evil\u{202e}dm.md".into(), staged: false, range: None };
/// assert_eq!(pinfold_core::tool::title(&run), "md evil<U+202E>dm.md");
/// let run = Run { tool: Tool::Diff, path: "/r".into(), staged: true, range: Some("main..".into()) };
/// assert_eq!(pinfold_core::tool::title(&run), "diff --staged main..");
/// ```
#[must_use]
pub fn title(run: &Run) -> String {
    let name = run.path.rsplit('/').find(|name| !name.is_empty());
    let argument = match run.tool {
        Tool::Diff => run.range.as_deref().or(name),
        Tool::Md | Tool::Files => name,
    };
    let mut title = run.tool.name().to_owned();
    if run.staged {
        title.push_str(" --staged");
    }
    title.push(' ');
    for character in argument.unwrap_or("/").chars() {
        match character {
            '\0'..='\u{1f}' => {
                title.push('^');
                title.push(char::from(character as u8 + 0x40));
            }
            '\u{7f}' => title.push_str("^?"),
            '\u{80}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => {
                title.push_str(&format!("<U+{:04X}>", u32::from(character)));
            }
            _ => title.push(character),
        }
    }
    title
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
        assert_eq!(reactor.tool_title(tool), Some("files /"));
        assert_eq!(reactor.tool_title(shell), None);
    }

    #[test]
    fn a_title_escapes_every_control_and_bidi_character() {
        let named = |name: &str| title(&run(Tool::Files, Path::new(&format!("/r/{name}"))));
        assert_eq!(named("a\u{1b}[2Jb"), "files a^[[2Jb");
        assert_eq!(named("a\u{7f}\u{85}b"), "files a^?<U+0085>b");
        for bidi in [
            '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}',
        ] {
            let shown = named(&format!("a{bidi}b"));
            assert_eq!(shown, format!("files a<U+{:04X}>b", u32::from(bidi)));
        }
        assert_eq!(named("src/"), "files src");
        assert!(!title(&run(Tool::Diff, Path::new("/r"))).starts_with('/'));
    }
}
