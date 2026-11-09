//! The one place besides [`git`](crate::git) where a built-in tool starts a process: the user's
//! editor, in the tool's own pane (ADR-0045 section 4).
//!
//! `$VISUAL`, else `$EDITOR`, else `vi`, is split into words as a shell would split it, with its
//! quotes and backslashes, but nothing is expanded and no shell runs. The file goes last, as one
//! absolute argument, so a name starting with `-` cannot be an option and a name holding `;` or
//! a quote is never parsed.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs the user's editor on `path` in this terminal and waits for it to exit.
pub(crate) fn edit(path: &Path) -> Result<(), String> {
    let chosen = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| env::var(name).ok())
        .find(|editor| !editor.trim().is_empty());
    let editor = chosen.unwrap_or_else(|| "vi".to_owned());
    let path = std::path::absolute(path).map_err(|error| error.to_string())?;
    let search = env::var_os("PATH").unwrap_or_default();
    let status = command(&editor, &path, &search)?
        .status()
        .map_err(|error| format!("cannot start the editor: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the editor exited with {status}"))
    }
}

/// The editor's command for the absolute `path`. A program named without a `/` is looked up
/// only on absolute entries of `search`, as git is, so a directory being browsed cannot supply it.
fn command(editor: &str, path: &Path, search: &OsStr) -> Result<Command, String> {
    let words = split(editor)?;
    let (program, args) = words.split_first().ok_or("the editor is empty")?;
    let program = if program.contains('/') {
        PathBuf::from(program)
    } else {
        crate::git::find(search, program).ok_or_else(|| format!("no {program} on PATH"))?
    };
    let mut command = Command::new(program);
    command.args(args).arg(path);
    Ok(command)
}

/// Splits `line` into words at blanks, keeping what single quotes hold as it is, and in double
/// quotes or bare taking a backslash to keep the next character. Refuses an unclosed quote.
fn split(line: &str) -> Result<Vec<String>, String> {
    let unclosed = || "the editor has an unclosed quote".to_owned();
    let (mut words, mut word, mut started) = (Vec::new(), String::new(), false);
    let mut chars = line.chars();
    while let Some(character) = chars.next() {
        match character {
            ' ' | '\t' | '\n' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
                continue;
            }
            '\'' => loop {
                match chars.next().ok_or_else(unclosed)? {
                    '\'' => break,
                    inside => word.push(inside),
                }
            },
            '"' => loop {
                match chars.next().ok_or_else(unclosed)? {
                    '"' => break,
                    '\\' => match chars.next().ok_or_else(unclosed)? {
                        kept @ ('"' | '\\' | '$' | '`') => word.push(kept),
                        other => word.extend(['\\', other]),
                    },
                    inside => word.push(inside),
                }
            },
            '\\' => word.push(chars.next().ok_or_else(unclosed)?),
            other => word.push(other),
        }
        started = true;
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_splits_as_a_shell_would_without_expanding_anything() {
        let words = |line| split(line).unwrap();
        assert_eq!(words("code --wait"), ["code", "--wait"]);
        assert_eq!(
            words("  vim\t-u 'my rc' \"a \\\"b\\\"\" c\\ d ''"),
            ["vim", "-u", "my rc", "a \"b\"", "c d", ""]
        );
        assert_eq!(words("vi $HOME;rm `x`"), ["vi", "$HOME;rm", "`x`"]);
        assert_eq!(words("'a'\"b\"c"), ["abc"]);
        for unclosed in ["vi 'x", "vi \"x", "vi \\"] {
            assert!(split(unclosed).is_err(), "{unclosed}");
        }
    }

    #[test]
    fn the_file_is_one_absolute_argument_after_the_editors_own() {
        let path = Path::new("/w/-rf; touch x 'y'.txt");
        let vim = command("/usr/bin/vim -p", path, OsStr::new("")).unwrap();
        assert_eq!(vim.get_program(), "/usr/bin/vim");
        let args: Vec<&OsStr> = vim.get_args().collect();
        assert_eq!(args, [OsStr::new("-p"), path.as_os_str()]);
        assert!(command("''", path, OsStr::new("")).is_err());
        assert!(command("   ", path, OsStr::new("")).is_err());
    }

    #[test]
    fn an_editor_without_a_slash_is_found_only_on_absolute_path_entries() {
        let found = command("sh -x", Path::new("/a"), OsStr::new(".:bin:/bin")).unwrap();
        assert_eq!(found.get_program(), "/bin/sh");
        assert!(command("sh", Path::new("/a"), OsStr::new(".:bin")).is_err());
    }
}
