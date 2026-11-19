//! `pinfold agents peek`: an agent's context use and last three steps, read from its own local
//! session file for an open inbox's second line (ADR-0051 §7). The core starts it with fixed
//! arguments and kills it after 2 s, so a hostile or huge file costs only this process.
//!
//! It also lists Claude Code sessions running outside PinFold, from their files that changed in
//! the last 10 minutes, for the inbox's outside rows (ADR-0051 §8).
//!
//! Claude Code only: OpenCode keeps its sessions in SQLite, which needs a C library to read
//! (ADR-0051, amendment "slice 9, session files as built").

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use unicode_width::UnicodeWidthChar;

/// The most bytes read from the end of a session file.
pub const TAIL: u64 = 64 * 1024;
/// The longest line printed for a directory.
pub const MAX_LINE: usize = 1024;
/// How many of the last steps are kept.
const STEPS: usize = 3;
/// The widest a step is, in cells.
const STEP_CELLS: usize = 80;
/// The most directories one peek reads, as the core holds at most 64 panes.
const MAX_DIRS: usize = 64;
/// The most entries looked at in one project directory for its newest session.
const MAX_ENTRIES: usize = 10_000;
/// The context window a model has unless its name asks for the long one.
const WINDOW: u64 = 200_000;
const LONG_WINDOW: u64 = 1_000_000;
/// How recently a session's file changed for it to be listed as outside PinFold (ADR-0051 §8).
const RECENT: Duration = Duration::from_secs(10 * 60);
/// The most sessions listed as outside PinFold.
pub const MAX_OUTSIDE: usize = 16;

/// Read by `pinfold agents peek` only when `cargo xtask mem-gate` measures it: a last line then
/// gives `ms`, the peek's wall time, `peak`, its own peak footprint in bytes, and `now`, its
/// footprint as it ends: PSS on Linux, which keeps no peak PSS.
const MEASURE: &str = "PINFOLD_MEASURE";

/// What an inbox row's second line shows for one pane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peeked {
    /// Context use as a whole percent, at most 100.
    pub ctx: Option<u8>,
    /// Up to three steps, oldest first: a tool's name or a message's first words, cleaned of
    /// control and bidi characters and at most 80 cells.
    pub steps: Vec<String>,
}

impl Peeked {
    /// The JSON line printed for it, at most [`MAX_LINE`] bytes: the oldest steps go first if
    /// it would be longer.
    ///
    /// ```
    /// use pinfold_tools::agents::Peeked;
    ///
    /// let peeked = Peeked { ctx: Some(64), steps: vec!["Bash".into(), "Edit".into()] };
    /// assert_eq!(peeked.line(), r#"{"ctx":64,"steps":["Bash","Edit"]}"#);
    /// ```
    #[must_use]
    pub fn line(&self) -> String {
        let mut steps = self.steps.as_slice();
        loop {
            let line = serde_json::json!({ "ctx": self.ctx, "steps": steps }).to_string();
            if line.len() <= MAX_LINE || steps.is_empty() {
                return line;
            }
            steps = &steps[1..];
        }
    }
}

/// A Claude Code session running outside PinFold: its directory, from its own file and so only a
/// hint, and how long ago that file changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outside {
    /// The session's directory, absolute, as its file says.
    pub dir: String,
    /// Seconds since its file changed.
    pub secs: u64,
}

impl Outside {
    /// The JSON line printed for it, after the directories' lines.
    ///
    /// ```
    /// use pinfold_tools::agents::Outside;
    ///
    /// let outside = Outside { dir: "/work".into(), secs: 12 };
    /// assert_eq!(outside.line(), r#"{"outside":"/work","secs":12}"#);
    /// ```
    #[must_use]
    pub fn line(&self) -> String {
        serde_json::json!({ "outside": self.dir, "secs": self.secs }).to_string()
    }
}

/// Parses the last [`TAIL`] bytes of a Claude Code session file. `whole` says whether `tail`
/// starts at the file's start; if not, its first line is cut and skipped. Every line is parsed
/// on its own, so a bad, partial or deeply nested one is skipped and the rest still count.
#[must_use]
pub fn parse(tail: &[u8], whole: bool) -> Peeked {
    let mut lines = tail.split(|&byte| byte == b'\n');
    if !whole {
        lines.next();
    }
    let mut peeked = Peeked::default();
    for line in lines {
        // `serde_json` refuses nesting past 128 levels with an error, not a stack overflow.
        let Ok(Value::Object(entry)) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry.get("isSidechain") == Some(&Value::Bool(true))
        {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        if let Some(ctx) = ctx(message, entry.get("requestedModel")) {
            peeked.ctx = Some(ctx);
        }
        let content = message.get("content").and_then(Value::as_array);
        for part in content.into_iter().flatten() {
            let step = match part.get("type").and_then(Value::as_str) {
                Some("tool_use") => part.get("name").and_then(Value::as_str),
                Some("text") => part.get("text").and_then(Value::as_str),
                _ => None,
            };
            if let Some(step) = step.map(cleaned).filter(|step| !step.is_empty()) {
                if peeked.steps.len() == STEPS {
                    peeked.steps.remove(0);
                }
                peeked.steps.push(step);
            }
        }
    }
    peeked
}

/// Context use from a message's `usage`: every input token over the model's window.
fn ctx(message: &Value, requested: Option<&Value>) -> Option<u8> {
    let usage = message.get("usage")?;
    let fields = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ];
    let mut tokens = None::<u64>;
    for field in fields {
        if let Some(count) = usage.get(field).and_then(Value::as_u64) {
            tokens = Some(tokens.unwrap_or(0).saturating_add(count));
        }
    }
    let models = [requested, message.get("model")];
    let long =
        (models.iter().flatten()).any(|model| model.as_str().is_some_and(|m| m.contains("[1m]")));
    let window = if long { LONG_WINDOW } else { WINDOW };
    Some(u8::try_from((tokens?.saturating_mul(100) / window).min(100)).unwrap_or(100))
}

/// A step's first line, without control or bidi characters, cut to [`STEP_CELLS`] with `…`.
fn cleaned(text: &str) -> String {
    let first = text.trim_start().lines().next().unwrap_or_default();
    let mut out = String::new();
    let mut cells = 0;
    for character in first.chars().filter(|&c| !c.is_control() && !bidi(c)) {
        let width = character.width().unwrap_or(0);
        if cells + width > STEP_CELLS - 1 {
            out.push('…');
            break;
        }
        cells += width;
        out.push(character);
    }
    out.trim_end().to_owned()
}

fn bidi(character: char) -> bool {
    matches!(
        character,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Claude Code's project folder name for `dir`: every UTF-16 unit that is not an ASCII letter
/// or digit becomes `-`, as Claude Code writes it.
fn project_name(dir: &Path) -> String {
    let dir = dir.as_os_str().to_string_lossy();
    (dir.encode_utf16())
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect()
}

/// The newest regular `*.jsonl` file in `folder`, never through a link, and when it changed.
fn newest(folder: &Path) -> Option<(SystemTime, PathBuf)> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(folder).ok()?.flatten().take(MAX_ENTRIES) {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let Ok(changed) = meta.modified() else {
            continue;
        };
        if meta.is_file() && best.as_ref().is_none_or(|(time, _)| changed > *time) {
            best = Some((changed, path));
        }
    }
    best
}

/// At most [`TAIL`] bytes from the end of the regular file at `path`, opened read-only without
/// following a last link, and whether they start at its start. A file that grows meanwhile is
/// still read no further than that.
fn tail(path: &Path) -> io::Result<(Vec<u8>, bool)> {
    let mut file: File = pinfold_sys::open_no_follow(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let start = meta.len().saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(TAIL as usize);
    file.take(TAIL).read_to_end(&mut bytes)?;
    Ok((bytes, start == 0))
}

/// What the newest Claude Code session for `dir` shows, under `home`. Nothing for no file.
#[must_use]
pub fn peek(home: &Path, dir: &Path) -> Peeked {
    let folder = home.join(".claude/projects").join(project_name(dir));
    let read = newest(&folder).and_then(|(_, path)| tail(&path).ok());
    read.map(|(bytes, whole)| parse(&bytes, whole))
        .unwrap_or_default()
}

/// The last `cwd` a session's tail names, if that one is absolute. `whole` is as for [`parse`].
fn cwd(tail: &[u8], whole: bool) -> Option<String> {
    let mut lines = tail.split(|&byte| byte == b'\n');
    if !whole {
        lines.next();
    }
    let cwd = lines.rev().find_map(|line| {
        let entry = serde_json::from_slice::<Value>(line).ok()?;
        entry.get("cwd")?.as_str().map(str::to_owned)
    })?;
    cwd.starts_with('/').then_some(cwd)
}

/// Claude Code sessions under `home` whose newest file changed in the last 10 minutes, as of
/// `now`, and whose directory is none of `dirs`: newest first, one a directory, at most
/// [`MAX_OUTSIDE`] (ADR-0051 §8). Project folders and files are never reached through a link.
#[must_use]
pub fn outside(home: &Path, dirs: &[OsString], now: SystemTime) -> Vec<Outside> {
    let projects = home.join(".claude/projects");
    let Ok(entries) = std::fs::read_dir(projects) else {
        return Vec::new();
    };
    let age = |changed: SystemTime| now.duration_since(changed).unwrap_or_default();
    let mut recent: Vec<(SystemTime, PathBuf)> = (entries.flatten().take(MAX_ENTRIES))
        .map(|entry| entry.path())
        .filter(|folder| std::fs::symlink_metadata(folder).is_ok_and(|meta| meta.is_dir()))
        .filter_map(|folder| newest(&folder))
        .filter(|(changed, _)| age(*changed) <= RECENT)
        .collect();
    recent.sort_by_key(|(changed, _)| std::cmp::Reverse(*changed));
    let mut listed: Vec<Outside> = Vec::new();
    for (changed, path) in recent {
        let Some(dir) = tail(&path)
            .ok()
            .and_then(|(bytes, whole)| cwd(&bytes, whole))
        else {
            continue;
        };
        let inside = dirs.iter().any(|pane| Path::new(pane) == Path::new(&dir));
        let seen = listed.iter().any(|row| row.dir == dir);
        let row = Outside {
            dir,
            secs: age(changed).as_secs(),
        };
        if !inside && !seen && row.line().len() <= MAX_LINE {
            listed.push(row);
        }
        if listed.len() == MAX_OUTSIDE {
            break;
        }
    }
    listed
}

/// Runs `pinfold agents peek -- <dir>...`, the hidden command the core starts while the inbox
/// is open (ADR-0051 §7): one [`Peeked::line`] per directory, in order, on standard output, then
/// an [`Outside::line`] for each session in none of them (§8). Returns whether the arguments
/// were right and every line was written.
///
/// ```no_run
/// let args = ["peek", "--", "/path/to/work"].map(Into::into);
/// let ok = pinfold_tools::agents::command(Some("/home/me".as_ref()), &args);
/// ```
pub fn command(home: Option<&Path>, args: &[OsString]) -> bool {
    let start = std::time::Instant::now();
    let (Some(home), [verb, dashes, dirs @ ..]) = (home, args) else {
        return false;
    };
    if verb.as_bytes() != b"peek" || dashes.as_bytes() != b"--" {
        return false;
    }
    let dirs = &dirs[..dirs.len().min(MAX_DIRS)];
    let mut out = io::stdout().lock();
    for dir in dirs {
        if writeln!(out, "{}", peek(home, Path::new(dir)).line()).is_err() {
            return false;
        }
    }
    for row in outside(home, dirs, SystemTime::now()) {
        if writeln!(out, "{}", row.line()).is_err() {
            return false;
        }
    }
    if std::env::var_os(MEASURE).is_some() {
        let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let peak = pinfold_sys::peak_footprint().unwrap_or(0);
        let now = pinfold_sys::footprint(std::process::id()).unwrap_or(0);
        let line = serde_json::json!({ "ms": ms, "peak": peak, "now": now });
        return writeln!(out, "{line}").is_ok();
    }
    true
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    /// A recorded Claude Code session's shape, cut to the fields that matter and a few that
    /// do not.
    const RECORDED: &str = include_str!("agents/claude-session.jsonl");

    fn home(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pinfold-peek-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Writes `body` as the session of `dir` under `home`, and returns its path.
    fn session(home: &Path, dir: &str, body: &[u8]) -> PathBuf {
        let folder = home
            .join(".claude/projects")
            .join(project_name(Path::new(dir)));
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join("s.jsonl");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn a_recorded_session_gives_its_context_and_last_three_steps() {
        let peeked = parse(RECORDED.as_bytes(), true);
        assert_eq!(peeked.ctx, Some(56));
        assert_eq!(peeked.steps, ["Read", "Now the parser.", "Edit"]);
        assert_eq!(
            peeked.line(),
            r#"{"ctx":56,"steps":["Read","Now the parser.","Edit"]}"#
        );
    }

    #[test]
    fn the_project_folder_is_named_as_claude_code_names_it() {
        assert_eq!(
            project_name(Path::new("/Users/me/.claude/my_app")),
            "-Users-me--claude-my-app"
        );
        assert_eq!(project_name(Path::new("/tmp/日本")), "-tmp---");
    }

    #[test]
    fn a_long_context_model_counts_against_a_million() {
        let line = br#"{"type":"assistant","requestedModel":"opus[1m]","message":{"usage":{"input_tokens":500000}}}"#;
        assert_eq!(parse(line, true).ctx, Some(50));
        let over =
            br#"{"type":"assistant","message":{"usage":{"cache_read_input_tokens":900000}}}"#;
        assert_eq!(parse(over, true).ctx, Some(100));
    }

    #[test]
    fn sidechains_and_other_entries_are_not_steps() {
        let lines = concat!(
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"tool_use","name":"Task"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"run rm -rf"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}]}}"#,
        );
        assert_eq!(parse(lines.as_bytes(), true), Peeked::default());
    }

    /// ADR-0051 §7: a bad line is skipped, a cut first line is skipped, and the rest count.
    #[test]
    fn a_truncated_or_garbage_tail_skips_only_its_bad_lines() {
        let good =
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
        let mut tail = b"name\":\"Cut\"}]}}\n\x00\xff garbage\n".to_vec();
        tail.extend_from_slice(good.as_bytes());
        tail.extend_from_slice(b"\n{\"type\":\"assistant\",\"mess");
        let peeked = parse(&tail, false);
        assert_eq!(peeked.steps, ["Bash"]);
        assert_eq!(parse(b"\xff\xfe\x00", true), Peeked::default());
        assert_eq!(parse(b"", true), Peeked::default());
    }

    #[test]
    fn hostile_steps_are_cleaned_and_cut() {
        let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        let long = "界".repeat(500);
        let text = "a\u{1b}]0;pwned\u{7}b\u{202e}c\u{2066}d\nsecond line";
        let lines = [
            deep,
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}).to_string(),
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":long}]}}).to_string(),
        ];
        let peeked = parse(lines.join("\n").as_bytes(), true);
        assert_eq!(peeked.steps[0], "a]0;pwnedbcd");
        let cut = &peeked.steps[1];
        assert!(cut.ends_with('…'));
        assert_eq!(
            cut.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>(),
            79
        );
    }

    #[test]
    fn a_line_is_never_over_one_kibibyte() {
        let steps = vec!["\"\\".repeat(40); 3];
        let line = Peeked {
            ctx: Some(100),
            steps,
        }
        .line();
        assert!(line.len() <= MAX_LINE, "{}", line.len());
        assert!(line.contains("steps"));
    }

    /// A 64 MiB file is read no further than its last 64 KiB, and its last steps still show.
    #[test]
    fn only_the_tail_of_a_huge_session_is_read() {
        let root = home("huge");
        let filler = format!(
            "{}\n",
            serde_json::json!({"type":"user","pad":"x".repeat(1000)})
        );
        let mut body = filler.repeat(64 * 1024 * 1024 / filler.len()).into_bytes();
        body.extend_from_slice(RECORDED.as_bytes());
        let path = session(&root, "/work", &body);
        let (bytes, whole) = tail(&path).unwrap();
        assert_eq!((bytes.len() as u64, whole), (TAIL, false));
        assert_eq!(
            peek(&root, Path::new("/work")).steps,
            ["Read", "Now the parser.", "Edit"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_file_growing_while_read_is_read_to_the_bound_only() {
        let root = home("growing");
        let path = session(&root, "/work", RECORDED.as_bytes());
        let mut file = pinfold_sys::open_no_follow(&path).unwrap();
        let mut appending = fs::OpenOptions::new().append(true).open(&path).unwrap();
        appending.write_all(&vec![b'x'; 200 * 1024]).unwrap();
        let mut bytes = Vec::new();
        (&mut file).take(TAIL).read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.len() as u64, TAIL);
        let (bytes, whole) = tail(&path).unwrap();
        assert_eq!((bytes.len() as u64, whole), (TAIL, false));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_symlinked_session_is_not_followed() {
        let root = home("link");
        let secret = root.join("elsewhere.jsonl");
        fs::write(&secret, RECORDED).unwrap();
        let path = session(&root, "/work", b"");
        fs::remove_file(&path).unwrap();
        symlink(&secret, &path).unwrap();
        assert_eq!(peek(&root, Path::new("/work")), Peeked::default());
        assert!(tail(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_newest_session_wins_and_none_shows_nothing() {
        let root = home("newest");
        assert_eq!(peek(&root, Path::new("/work")), Peeked::default());
        let old = session(&root, "/work", RECORDED.as_bytes());
        let new = old.with_file_name("t.jsonl");
        let step =
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep"}]}}"#;
        fs::write(&new, step).unwrap();
        let earlier = SystemTime::now() - std::time::Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(earlier)
            .unwrap();
        assert_eq!(peek(&root, Path::new("/work")).steps, ["Grep"]);
        fs::remove_dir_all(root).unwrap();
    }

    /// A session of `dir` under `home` whose file names `cwd` and changed `ago` before now.
    fn running(home: &Path, dir: &str, cwd: &str, ago: u64) {
        let line = serde_json::json!({"type": "user", "cwd": cwd}).to_string();
        let path = session(home, dir, format!("{RECORDED}{line}\n").as_bytes());
        let changed = SystemTime::now() - Duration::from_secs(ago);
        let file = File::options().write(true).open(path).unwrap();
        file.set_modified(changed).unwrap();
    }

    /// ADR-0051 §8: a session in a PinFold pane's directory is that pane's row, not listed
    /// again; others changed in the last 10 minutes are, newest first.
    #[test]
    fn a_session_in_a_panes_directory_is_not_listed_twice() {
        let root = home("outside");
        running(&root, "/work", "/work", 5);
        running(&root, "/elsewhere", "/elsewhere", 40);
        running(&root, "/later", "/later", 1);
        running(&root, "/stale", "/stale", 11 * 60);
        let listed = outside(&root, &["/work/".into()], SystemTime::now());
        let dirs: Vec<&str> = listed.iter().map(|row| row.dir.as_str()).collect();
        assert_eq!(dirs, ["/later", "/elsewhere"]);
        assert!((40..45).contains(&listed[1].secs), "{}", listed[1].secs);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn at_most_sixteen_sessions_are_listed_outside() {
        let root = home("sixteen");
        for index in 0..20 {
            let dir = format!("/w{index}");
            running(&root, &dir, &dir, index);
        }
        let listed = outside(&root, &[], SystemTime::now());
        assert_eq!(listed.len(), MAX_OUTSIDE);
        assert_eq!(
            (listed[0].dir.as_str(), listed[15].dir.as_str()),
            ("/w0", "/w15")
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// The directory is the file's word only: a relative one, none, or a linked project folder
    /// gives no row.
    #[test]
    fn a_relative_missing_or_linked_directory_is_not_listed() {
        let root = home("hint");
        running(&root, "/rel", "work/../etc", 1);
        session(&root, "/none", b"{\"type\":\"user\"}\n");
        let elsewhere = root.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("s.jsonl"), "{\"cwd\":\"/linked\"}\n").unwrap();
        symlink(&elsewhere, root.join(".claude/projects/-linked")).unwrap();
        assert_eq!(outside(&root, &[], SystemTime::now()), []);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_command_needs_its_fixed_arguments() {
        let home = Path::new("/nonexistent");
        let args = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(!command(None, &args(&["peek", "--", "/w"])));
        assert!(!command(Some(home), &args(&["peek", "/w"])));
        assert!(!command(Some(home), &args(&["look", "--", "/w"])));
    }
}
