//! Markdown reader, file manager and diff viewer: full-screen programs that each run in their
//! own pane (ADR-0045).
//!
//! A tool draws into a [`Screen`], whose [`put`](screen::Screen::put) escapes
//! every string, and reads [`Key`]s. [`run`] owns the terminal around it. Every git call goes
//! through [`git::run`], and the only other process a tool starts is the user's editor.

mod at;
mod diff;
mod editor;
mod files;
pub mod git;
pub mod keys;
mod md;
mod ops;
pub mod screen;

use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::Instant;

use keys::{Key, Keys};
use screen::{Color, Screen, Style};

/// A tool and its arguments, as `pinfold md`, `files` and `diff` take them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `pinfold md <file>`
    Md {
        /// The markdown file.
        file: PathBuf,
    },
    /// `pinfold files [dir]`
    Files {
        /// The directory, by default the current one.
        dir: PathBuf,
    },
    /// `pinfold diff [--staged] [--watch] [range]`
    Diff {
        /// The index against `HEAD`, not the work tree against the index.
        staged: bool,
        /// A revision range, such as `main..HEAD`.
        range: Option<String>,
        /// Run git again every 2 s, or 20 times as long as it took if that is longer.
        watch: bool,
    },
}

impl Command {
    /// Parses the words after `pinfold`, or `None` if they are not a tool's.
    ///
    /// # Examples
    ///
    /// ```
    /// use pinfold_tools::Command;
    ///
    /// let diff = Command::parse(&["diff", "--staged"]);
    /// assert_eq!(diff, Some(Command::Diff { staged: true, range: None, watch: false }));
    /// assert_eq!(Command::parse(&["diff", "--stat"]), None);
    /// ```
    pub fn parse(words: &[&str]) -> Option<Self> {
        let (&tool, rest) = words.split_first()?;
        let (mut staged, mut watch, mut rest) = (false, false, rest);
        loop {
            match (tool, rest) {
                ("diff", ["--staged", more @ ..]) if !staged => (staged, rest) = (true, more),
                ("diff", ["--watch", more @ ..]) if !watch => (watch, rest) = (true, more),
                _ => break,
            }
        }
        // After `--`, as the core passes it, a path that starts with `-` is still a path.
        let argument = match rest {
            [] | ["--"] => None,
            ["--", word] => Some(*word),
            [word] if !word.starts_with('-') => Some(*word),
            _ => return None,
        };
        Some(match (tool, argument) {
            ("md", Some(file)) => Self::Md { file: file.into() },
            ("files", dir) => Self::Files {
                dir: dir.unwrap_or(".").into(),
            },
            // A range is never an option, even after `--` (ADR-0045 section 4).
            ("diff", range) if !range.is_some_and(|range| range.starts_with('-')) => Self::Diff {
                staged,
                range: range.map(str::to_owned),
                watch,
            },
            _ => return None,
        })
    }

    /// Runs the tool in this process's terminal until it quits, and returns whether it started.
    /// One that could not start shows ✕ error with the reason, and quits on the next key.
    ///
    /// # Errors
    ///
    /// Returns an error if standard input or output is not a terminal, or writing to it fails.
    pub fn run(&self) -> io::Result<bool> {
        let (title, problem) = match self {
            Self::Md { file } => {
                let read = problem(file, Path::is_file).map_or_else(
                    || {
                        md::read(file)
                            .map_err(|error| format!("cannot read {}: {error}", file.display()))
                    },
                    Err,
                );
                match read {
                    Ok((bytes, whole)) => {
                        let title = file.as_os_str().as_encoded_bytes().to_vec();
                        let mut reader = md::Reader::new(title, bytes, whole);
                        reader.path.clone_from(file);
                        run(&mut reader)?;
                        return Ok(true);
                    }
                    Err(problem) => (format!("md {}", file.display()), problem),
                }
            }
            Self::Files { dir } => {
                let found = problem(dir, Path::is_dir).map_or_else(
                    || {
                        std::fs::canonicalize(dir)
                            .map_err(|error| format!("cannot open {}: {error}", dir.display()))
                    },
                    Err,
                );
                match found {
                    Ok(path) => {
                        let title = dir.as_os_str().as_encoded_bytes().to_vec();
                        run(&mut files::Files::new(title, path))?;
                        return Ok(true);
                    }
                    Err(problem) => (format!("files {}", dir.display()), problem),
                }
            }
            Self::Diff {
                staged,
                range,
                watch,
            } => {
                let mut title = "diff".to_owned();
                if *staged {
                    title.push_str(" --staged");
                }
                if let Some(range) = range {
                    title = format!("{title} {range}");
                }
                let started = Instant::now();
                match diff::load(Path::new("."), *staged, range.as_deref()) {
                    Ok(diff) => {
                        let source = diff::Source {
                            repo: ".".into(),
                            staged: *staged,
                            range: range.clone(),
                        };
                        let viewer = diff::Viewer::new(title.into_bytes(), diff);
                        run(&mut viewer.watch(source, *watch, started.elapsed()))?;
                        return Ok(true);
                    }
                    Err(problem) => (title, problem),
                }
            }
        };
        run(&mut Failed { title, problem })?;
        Ok(false)
    }
}

/// Lays `markdown` out as `pinfold md` does at `width` columns, and returns each line's
/// already-escaped text. For the `markdown` fuzz target.
#[doc(hidden)]
pub fn fuzz_markdown(markdown: &[u8], width: u16) -> Vec<String> {
    md::lines(markdown, width)
}

/// Parses `diff` as `pinfold diff` does, and returns its notes and each file's label, header and
/// hunk lines, escaped. For the `unified_diff` fuzz target.
#[doc(hidden)]
pub fn fuzz_unified_diff(diff: &[u8]) -> Vec<String> {
    let diff = diff::parse(diff.to_vec());
    let mut shown = diff.notes.clone();
    for file in &diff.files {
        shown.push(screen::escape(&file.label()));
        let hunks =
            (file.hunks.iter()).flat_map(|hunk| std::iter::once(&hunk.header).chain(&hunk.lines));
        let lines = file.header.iter().chain(hunks);
        shown.extend(lines.map(|range| screen::escape(&diff.bytes[range.clone()])));
    }
    shown
}

/// Why `path` cannot be opened, if `kind` is not true of it.
fn problem(path: &Path, kind: fn(&Path) -> bool) -> Option<String> {
    match std::fs::metadata(path) {
        Ok(_) if kind(path) => None,
        Ok(_) => Some(format!("{} is the wrong kind of file", path.display())),
        Err(error) => Some(format!("cannot open {}: {error}", path.display())),
    }
}

/// What a tool does after a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep running.
    Continue,
    /// Leave, restoring the terminal.
    Quit,
}

/// A full-screen tool, which [`run`] draws and sends keys to.
pub trait Tool {
    /// Draws the whole screen; only the rows that changed reach the terminal.
    fn draw(&mut self, screen: &mut Screen);
    /// Handles a key. Every tool quits on `q` when no prompt of its own is open.
    fn key(&mut self, key: Key) -> Flow;
    /// Called before each draw, at least every 100 ms, for a tool that polls.
    fn tick(&mut self) {}
    /// A file to open in the user's editor, asked for by the last key. [`run`] hands the
    /// terminal to the editor until it exits, then redraws the whole screen.
    fn edit(&mut self) -> Option<PathBuf> {
        None
    }
    /// What became of the edit: `Err` says why it failed.
    fn edited(&mut self, _result: Result<(), String>) {}
}

/// The screen of a tool that cannot start.
struct Failed {
    title: String,
    problem: String,
}

impl Tool for Failed {
    fn draw(&mut self, screen: &mut Screen) {
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        screen.put(0, 0, format!("pinfold {}", self.title), bold);
        let red = Style {
            fg: Color::Red,
            ..Style::default()
        };
        let column = screen.put(2, 0, "✕ error", red);
        screen.put(2, column + 1, &self.problem, Style::default());
        screen.put(4, 0, "Press any key to leave.", Style::default());
    }

    fn key(&mut self, _: Key) -> Flow {
        Flow::Quit
    }
}

/// Alternate screen, cursor hidden, no autowrap, SGR mouse reports, bracketed paste (whose text
/// [`Keys`] drops); and their undoing.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?1000h\x1b[?1006h\x1b[?2004h";
const LEAVE: &[u8] = b"\x1b[?2004l\x1b[?1006l\x1b[?1000l\x1b[?7h\x1b[?25h\x1b[?1049l";

/// The terminal in raw mode and the alternate screen, restored when dropped.
struct Terminal<'fd> {
    _raw: pinfold_sys::tty::Raw<'fd>,
}

impl Drop for Terminal<'_> {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(LEAVE).and_then(|()| out.flush());
    }
}

/// Raw mode and the alternate screen on `stdin`'s terminal.
fn enter(stdin: &io::Stdin) -> io::Result<Terminal<'_>> {
    let terminal = Terminal {
        _raw: pinfold_sys::tty::raw(stdin.as_fd())?,
    };
    io::stdout().write_all(ENTER)?;
    Ok(terminal)
}

/// Runs `tool` full screen until it quits, redrawing after each key, read or 100 ms without one.
///
/// # Errors
///
/// Returns an error if standard input or output is not a terminal, or reading or writing fails.
pub fn run(tool: &mut dyn Tool) -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut terminal = enter(&stdin)?;
    let mut screen = Screen::new(0, 0);
    let mut keys = Keys::default();
    let (mut out, mut input) = (Vec::new(), [0; 1024]);
    loop {
        let size = pinfold_sys::tty::size(stdout.as_fd())?;
        if size != screen.size() {
            screen = Screen::new(size.0, size.1);
        }
        screen.clear();
        tool.tick();
        tool.draw(&mut screen);
        out.clear();
        screen.frame(&mut out);
        stdout.write_all(&out)?;
        stdout.flush()?;
        // Raw mode returns from a read after 100 ms with nothing.
        let read = stdin.lock().read(&mut input)?;
        keys.push(&input[..read]);
        while let Some(key) = keys.next(read == 0) {
            if tool.key(key) == Flow::Quit {
                return Ok(());
            }
            if let Some(path) = tool.edit() {
                // The editor gets the terminal as it was; keys read past the one asking are dropped.
                drop(terminal);
                let result = editor::edit(&path);
                terminal = enter(&stdin)?;
                tool.edited(result);
                (keys, screen) = (Keys::default(), Screen::new(0, 0));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        let md = Command::Md {
            file: "README.md".into(),
        };
        assert_eq!(Command::parse(&["md", "README.md"]), Some(md));
        assert_eq!(
            Command::parse(&["files"]),
            Some(Command::Files { dir: ".".into() })
        );
        let range = Some("main..HEAD".to_owned());
        let diff = Command::Diff {
            staged: true,
            range,
            watch: false,
        };
        assert_eq!(
            Command::parse(&["diff", "--staged", "main..HEAD"]),
            Some(diff)
        );
        let dashed = Command::Md {
            file: "-notes.md".into(),
        };
        assert_eq!(Command::parse(&["md", "--", "-notes.md"]), Some(dashed));
        let dashed = Command::Files { dir: "-x".into() };
        assert_eq!(Command::parse(&["files", "--", "-x"]), Some(dashed));
        let staged = Command::Diff {
            staged: true,
            range: Some("a..b".to_owned()),
            watch: false,
        };
        let watched = Command::Diff {
            staged: true,
            range: Some("a..b".to_owned()),
            watch: true,
        };
        assert_eq!(
            Command::parse(&["diff", "--watch", "--staged", "a..b"]),
            Some(watched)
        );
        assert_eq!(
            Command::parse(&["diff", "--staged", "--", "a..b"]),
            Some(staged)
        );
        let plain = Command::Diff {
            staged: false,
            range: None,
            watch: false,
        };
        assert_eq!(Command::parse(&["diff", "--"]), Some(plain));
        for wrong in [
            &["md"][..],
            &["md", "--"],
            &["diff", "--", "--output=x"],
            &["files", "--", "a", "b"],
            &["md", "-x"],
            &["files", "a", "b"],
            &["diff", "--x"],
            &["diff", "--watch", "--watch"],
            &["md", "--watch", "a"],
            &["diff", "a", "b"],
        ] {
            assert_eq!(Command::parse(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn a_tool_that_cannot_start_shows_the_reason_in_red_and_leaves_on_any_key() {
        let mut tool = Failed {
            title: "md gone.md".into(),
            problem: problem(Path::new("gone.md"), Path::is_file).unwrap(),
        };
        let mut screen = Screen::new(6, 60);
        tool.draw(&mut screen);
        assert!(screen.row(2).starts_with("✕ error cannot open gone.md: "));
        assert_eq!(tool.key(Key::Char('j')), Flow::Quit);
    }
}
