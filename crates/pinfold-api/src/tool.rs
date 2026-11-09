//! `tool.open`'s parameters, their shape checked (ADR-0045 section 4). Whether `path` exists,
//! and is a file or a directory, is the reactor's to check.

use serde_json::{Value, json};

use crate::spawn::Place;

/// The longest `path` taken, in bytes.
pub const MAX_PATH: usize = 4096;
/// The longest `range` taken, in bytes.
pub const MAX_RANGE: usize = 256;

/// A built-in tool, which runs as `pinfold <name>` in a pane of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `md`: the markdown reader, of a file.
    Md,
    /// `files`: the file manager, of a directory.
    Files,
    /// `diff`: the git diff viewer, of a repository's directory.
    Diff,
}

impl Tool {
    /// The tool's name, which is also its `pinfold` subcommand.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Md => "md",
            Self::Files => "files",
            Self::Diff => "diff",
        }
    }
}

/// What a tool pane runs: a tool and its checked arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Which tool.
    pub tool: Tool,
    /// An absolute path: a file for `md`, a directory for `files` and `diff`.
    pub path: String,
    /// `diff` only: the staged changes.
    pub staged: bool,
    /// `diff` only: a revision range of [`revision`] characters.
    pub range: Option<String>,
}

impl Run {
    /// The arguments after `pinfold`: a fixed list, then `--`, then the caller's values, so
    /// none can be read as an option.
    ///
    /// ```
    /// use pinfold_api::tool::{Run, Tool};
    ///
    /// let run = Run { tool: Tool::Diff, path: "/repo".into(), staged: true, range: Some("main..".into()) };
    /// assert_eq!(run.args(), ["diff", "--staged", "--", "main.."]);
    /// let run = Run { tool: Tool::Md, path: "/repo/README.md".into(), staged: false, range: None };
    /// assert_eq!(run.args(), ["md", "--", "/repo/README.md"]);
    /// ```
    #[must_use]
    pub fn args(&self) -> Vec<&str> {
        let mut args = vec![self.tool.name()];
        if self.staged {
            args.push("--staged");
        }
        args.push("--");
        match self.tool {
            Tool::Md | Tool::Files => args.push(&self.path),
            Tool::Diff => args.extend(self.range.as_deref()),
        }
        args
    }

    /// What `pane.list` shows of it.
    #[must_use]
    pub fn describe(&self) -> Value {
        json!({"name": self.tool.name(), "path": self.path, "staged": self.staged,
               "range": self.range})
    }
}

/// `tool.open`: a tool in a new pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// What the pane runs.
    pub run: Run,
    /// Where the pane goes.
    pub place: Place,
    /// The pane to split, or nothing for the caller's own.
    pub of: Option<usize>,
}

/// Whether `range` is a revision range git takes and never an option: it does not start with
/// `-`, holds only `A-Z a-z 0-9 . _ / ~ ^ @ { } -`, and at most one `..` or `...`.
///
/// ```
/// use pinfold_api::tool::revision;
///
/// assert!(revision("main...HEAD~2") && revision("@{u}..") && revision("v1.2.3"));
/// assert!(!revision("--output=/tmp/x") && !revision("a..b..c") && !revision("a....b"));
/// ```
#[must_use]
pub fn revision(range: &str) -> bool {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || b"._/~^@{}-".contains(&byte);
    if range.is_empty()
        || range.len() > MAX_RANGE
        || range.starts_with('-')
        || !range.bytes().all(allowed)
    {
        return false;
    }
    // Each run of dots longer than one is a `..` or `...`; a longer one is neither.
    let runs = range.split(|character| character != '.');
    let separators: Vec<usize> = runs.map(str::len).filter(|dots| *dots > 1).collect();
    matches!(separators[..], [] | [2 | 3])
}

/// Reads `tool.open`'s parameters, refusing any it does not know, such as a command.
pub(crate) fn params(params: Option<&Value>) -> Result<Open, &'static str> {
    const SHAPE: &str = "tool.open takes tool (md, files or diff), an absolute path, \
                         staged and range for diff, place and of";
    let params = params.and_then(Value::as_object).ok_or(SHAPE)?;
    let known = ["tool", "path", "staged", "range", "place", "of"];
    if params.keys().any(|key| !known.contains(&key.as_str())) {
        return Err(SHAPE);
    }
    let tool = match params.get("tool").and_then(Value::as_str) {
        Some("md") => Tool::Md,
        Some("files") => Tool::Files,
        Some("diff") => Tool::Diff,
        _ => return Err(SHAPE),
    };
    let path = match params.get("path") {
        Some(Value::String(path))
            if path.starts_with('/') && path.len() <= MAX_PATH && !path.contains('\0') =>
        {
            path.clone()
        }
        _ => return Err("tool.open's path must be absolute, at most 4096 bytes"),
    };
    let diff = tool == Tool::Diff;
    let staged = match params.get("staged") {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) if diff => true,
        Some(_) => return Err("only diff takes staged, true or false"),
    };
    let range = match params.get("range") {
        None | Some(Value::Null) => None,
        Some(Value::String(range)) if diff && revision(range) => Some(range.clone()),
        Some(_) => {
            return Err(
                "only diff takes range: revision characters, not starting with -, \
                        and one .. or ... at most",
            );
        }
    };
    let place = match params.get("place").and_then(Value::as_str) {
        None | Some("right") => Place::SplitRight,
        Some("down") => Place::SplitDown,
        Some("tab") => Place::Tab,
        Some(_) => return Err(SHAPE),
    };
    let of = match params.get("of") {
        None | Some(Value::Null) => None,
        Some(of) => Some(
            of.as_u64()
                .and_then(|of| usize::try_from(of).ok())
                .ok_or(SHAPE)?,
        ),
    };
    let run = Run {
        tool,
        path,
        staged,
        range,
    };
    Ok(Open { run, place, of })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(params: &Value) -> Result<Open, &'static str> {
        super::params(Some(params))
    }

    #[test]
    fn a_tool_takes_its_name_an_absolute_path_a_place_and_a_pane() {
        let opened = open(&json!({"tool": "diff", "path": "/r", "staged": true,
                                  "range": "main...HEAD", "place": "tab", "of": 2}));
        let run = Run {
            tool: Tool::Diff,
            path: "/r".to_owned(),
            staged: true,
            range: Some("main...HEAD".to_owned()),
        };
        let expected = Open {
            run,
            place: Place::Tab,
            of: Some(2),
        };
        assert_eq!(opened, Ok(expected));
        let opened = open(&json!({"tool": "md", "path": "/r/README.md"})).unwrap();
        assert_eq!((opened.place, opened.of), (Place::SplitRight, None));
        assert_eq!(opened.run.args(), ["md", "--", "/r/README.md"]);
    }

    #[test]
    fn an_unknown_tool_or_parameter_is_refused() {
        for params in [
            json!(null),
            json!({}),
            json!({"path": "/r"}),
            json!({"tool": "sh", "path": "/r"}),
            json!({"tool": "md", "path": "/r", "command": ["sh"]}),
            json!({"tool": "md", "path": "/r", "args": ["--help"]}),
            json!({"tool": "md", "path": "/r", "place": "split-right"}),
            json!({"tool": "md", "path": "/r", "of": -1}),
        ] {
            assert!(open(&params).is_err(), "{params}");
        }
        assert!(super::params(None).is_err());
    }

    #[test]
    fn a_path_must_be_absolute_and_bounded() {
        let long = format!("/{}", "a".repeat(MAX_PATH));
        for path in [
            json!("README.md"),
            json!("-r"),
            json!(""),
            json!(1),
            json!(long),
        ] {
            assert!(
                open(&json!({"tool": "md", "path": path})).is_err(),
                "{path}"
            );
        }
        assert!(open(&json!({"tool": "files", "path": "/a\u{0}b"})).is_err());
        assert!(open(&json!({"tool": "files"})).is_err());
    }

    #[test]
    fn only_diff_takes_staged_and_range() {
        for tool in ["md", "files"] {
            let staged = json!({"tool": tool, "path": "/r", "staged": true});
            let range = json!({"tool": tool, "path": "/r", "range": "main"});
            assert!(open(&staged).is_err() && open(&range).is_err(), "{tool}");
        }
        assert!(open(&json!({"tool": "diff", "path": "/r", "staged": "yes"})).is_err());
    }

    #[test]
    fn a_range_is_revision_characters_and_one_separator_at_most() {
        for good in [
            "HEAD",
            "main..",
            "..main",
            "a...b",
            "HEAD~3^2",
            "@{upstream}",
            "v1.0-rc",
        ] {
            assert!(revision(good), "{good}");
        }
        let long = "a".repeat(MAX_RANGE + 1);
        for bad in [
            "",
            "-p",
            "--output=/tmp/owned",
            "a..b..c",
            "a....b",
            "a b",
            "a;b",
            "$(id)",
            "a\nb",
            "main:path",
            "ref*",
            long.as_str(),
        ] {
            assert!(!revision(bad), "{bad}");
            let params = json!({"tool": "diff", "path": "/r", "range": bad});
            assert!(open(&params).is_err(), "{bad}");
        }
    }
}
