//! The diff viewer behind `pinfold diff` (ADR-0045 sections 2, 3 and 6): the files on the
//! left and the selected one's unified hunks on the right.
//!
//! Git's output is untrusted: names and lines come from the repository. [`parse`] takes any
//! bytes without panicking, keeps each line as a range into them, and caps each structure;
//! [`Diff::notes`] says which cap cut it, and the viewer shows them above the diff. Every name
//! and line reaches the screen through [`Screen::put`], which escapes it.

use std::iter;
use std::ops::Range;
use std::path::Path;

use crate::keys::Key;
use crate::md::colour;
use crate::screen::{Color, Screen, Style, escape};
use crate::{Flow, Tool, git};

/// Files kept; the rest of the diff is dropped.
const MAX_FILES: usize = 10_000;
/// Hunks kept across every file.
const MAX_HUNKS: usize = 100_000;
/// Lines kept across every file, at 16 bytes a range at most 8 MB.
const MAX_LINES: usize = 500_000;
/// Bytes kept of a line; the rest of it is cut.
const MAX_LINE: usize = 10_000;

/// A parsed `git diff`, which owns the bytes its ranges point into.
pub(crate) struct Diff {
    pub(crate) bytes: Vec<u8>,
    pub(crate) files: Vec<File>,
    /// One sentence per cap that cut the diff.
    pub(crate) notes: Vec<String>,
}

/// One file's part of the diff, from its `diff --git` line.
pub(crate) struct File {
    /// The path before and after, the same unless the file was renamed or copied.
    pub(crate) old: Vec<u8>,
    pub(crate) new: Vec<u8>,
    /// The lines before the first hunk, such as `rename from`, `new mode` and `Binary files`.
    pub(crate) header: Vec<Range<usize>>,
    pub(crate) hunks: Vec<Hunk>,
}

pub(crate) struct Hunk {
    /// The `@@ -1,2 +1,3 @@` line.
    pub(crate) header: Range<usize>,
    /// Lines that start with ` `, `+`, `-` or `\`, or are empty.
    pub(crate) lines: Vec<Range<usize>>,
}

impl File {
    /// The path, or `old → new` for a rename or copy.
    pub(crate) fn label(&self) -> Vec<u8> {
        if self.old == self.new {
            return self.new.clone();
        }
        [&self.old[..], " → ".as_bytes(), &self.new].concat()
    }
}

const KEYS: [&str; 6] = [
    "n p                 the next or previous file",
    "j k  ↓ ↑            a line down or up",
    "space b  PgDn PgUp  a page down or up",
    "g G  Home End       the top or the end",
    "?                   these keys",
    "q                   quit",
];

/// Runs `git diff` in `repo` through [`git::run`] and parses it, or says why it could not.
pub(crate) fn load(repo: &Path, staged: bool, range: Option<&str>) -> Result<Diff, String> {
    // Fixed prefixes, so `diff.noprefix` or `diff.mnemonicPrefix` cannot change the paths.
    let mut args = vec!["diff", "--src-prefix=a/", "--dst-prefix=b/"];
    args.extend(staged.then_some("--staged"));
    args.extend(range);
    args.push("--");
    let output = git::run(repo, args).map_err(|error| format!("cannot run git: {error}"))?;
    // Git cut short at the byte cap dies of SIGPIPE, which is not an error here.
    if !output.cut_short && output.status.is_some_and(|status| !status.success()) {
        let errors = String::from_utf8_lossy(&output.stderr);
        let first = errors.lines().next().unwrap_or_default();
        return Err(format!("git diff failed: {first}"));
    }
    let mut diff = parse(output.stdout);
    if output.cut_short {
        let why = match output.status {
            None => format!("git ran over {} s", git::TIMEOUT.as_secs()),
            Some(_) => format!("git printed over {} MB", git::MAX_OUTPUT / 1_000_000),
        };
        diff.notes
            .insert(0, format!("{why}, so the diff is cut short."));
    }
    Ok(diff)
}

/// Parses the output of `git diff`, which may be cut short anywhere.
pub(crate) fn parse(bytes: Vec<u8>) -> Diff {
    let mut files: Vec<File> = Vec::new();
    let (mut hunks, mut lines, mut long) = (0, 0, false);
    let mut notes = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let end = (bytes[start..].iter().position(|&byte| byte == b'\n'))
            .map_or(bytes.len(), |at| start + at);
        let line = &bytes[start..end];
        let range = start..cut(&bytes, start, end, &mut long);
        start = end + 1;
        if let Some(rest) = line.strip_prefix(b"diff --git ") {
            if files.len() == MAX_FILES {
                notes.push(format!("Only the first {MAX_FILES} files show."));
                break;
            }
            let (old, new) = git_paths(rest);
            files.push(File {
                old,
                new,
                header: Vec::new(),
                hunks: Vec::new(),
            });
            continue;
        }
        // Anything before the first file, such as a warning, is not part of the diff.
        let Some(file) = files.last_mut() else {
            continue;
        };
        if lines == MAX_LINES {
            notes.push(format!("Only the first {MAX_LINES} lines show."));
            break;
        }
        lines += 1;
        if line.starts_with(b"@@") {
            if hunks == MAX_HUNKS {
                notes.push(format!("Only the first {MAX_HUNKS} hunks show."));
                break;
            }
            hunks += 1;
            let lines = Vec::new();
            file.hunks.push(Hunk {
                header: range,
                lines,
            });
            continue;
        }
        if let Some(hunk) = file.hunks.last_mut() {
            hunk.lines.push(range);
            continue;
        }
        if let Some(rest) = line
            .strip_prefix(b"--- ")
            .filter(|rest| rest != b"/dev/null")
        {
            file.old = strip(unquote(trim(rest)).0, b"a/");
        } else if let Some(rest) = line
            .strip_prefix(b"+++ ")
            .filter(|rest| rest != b"/dev/null")
        {
            file.new = strip(unquote(trim(rest)).0, b"b/");
        } else if let Some(rest) =
            (line.strip_prefix(b"rename from ")).or_else(|| line.strip_prefix(b"copy from "))
        {
            file.old = unquote(rest).0;
        } else if let Some(rest) =
            (line.strip_prefix(b"rename to ")).or_else(|| line.strip_prefix(b"copy to "))
        {
            file.new = unquote(rest).0;
        }
        file.header.push(range);
    }
    if long {
        notes.push(format!("Lines over {MAX_LINE} bytes are cut."));
    }
    Diff {
        bytes,
        files,
        notes,
    }
}

/// Where the line from `start` to `end` ends once cut at [`MAX_LINE`] bytes, at the start of
/// a character so a cut one does not show as `\xNN`.
fn cut(bytes: &[u8], start: usize, end: usize, long: &mut bool) -> usize {
    if end - start <= MAX_LINE {
        return end;
    }
    *long = true;
    let mut at = start + MAX_LINE;
    for _ in 0..3 {
        if bytes[at] & 0xC0 == 0x80 {
            at -= 1;
        }
    }
    at
}

/// The two paths of a `diff --git` line. Unquoted ones are split in the middle, which is right
/// unless the file was renamed or copied; then the lines below name both.
fn git_paths(rest: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if rest.starts_with(b"\"") {
        let (old, rest) = unquote(rest);
        let (new, _) = unquote(rest.strip_prefix(b" ").unwrap_or(rest));
        return (strip(old, b"a/"), strip(new, b"b/"));
    }
    let half = rest.len() / 2;
    match (rest.get(..half), rest.get(half), rest.get(half + 1..)) {
        (Some(old), Some(b' '), Some(new)) if old.get(2..) == new.get(2..) => {
            (strip(old.to_vec(), b"a/"), strip(new.to_vec(), b"b/"))
        }
        _ => (rest.to_vec(), rest.to_vec()),
    }
}

/// `path` without `prefix`.
fn strip(path: Vec<u8>, prefix: &[u8]) -> Vec<u8> {
    match path.strip_prefix(prefix) {
        Some(rest) => rest.to_vec(),
        None => path,
    }
}

/// A `---` or `+++` path without the tab git adds after one that holds a space.
fn trim(path: &[u8]) -> &[u8] {
    path.strip_suffix(b"\t").unwrap_or(path)
}

/// A path as git prints it, in double quotes with C escapes if it holds anything unusual, and
/// the bytes after it. An unquoted path is all of `text`.
fn unquote(text: &[u8]) -> (Vec<u8>, &[u8]) {
    let Some(mut rest) = text.strip_prefix(b"\"") else {
        return (text.to_vec(), &[]);
    };
    let mut path = Vec::new();
    while let Some((&byte, after)) = rest.split_first() {
        rest = after;
        if byte == b'"' {
            break;
        }
        if byte != b'\\' {
            path.push(byte);
            continue;
        }
        let Some((&code, after)) = rest.split_first() else {
            break;
        };
        rest = after;
        path.push(match code {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'0'..=b'7' => {
                let mut value = code - b'0';
                for _ in 0..2 {
                    if let Some((&digit @ b'0'..=b'7', after)) = rest.split_first() {
                        value = value.wrapping_mul(8).wrapping_add(digit - b'0');
                        rest = after;
                    }
                }
                value
            }
            other => other,
        });
    }
    (path, rest)
}

/// The diff viewer.
pub(crate) struct Viewer {
    title: Vec<u8>,
    diff: Diff,
    selected: usize,
    top: usize,
    page: usize,
    help: bool,
}

impl Viewer {
    /// A viewer titled `title`, such as `diff --staged`, for `diff`.
    pub(crate) fn new(title: Vec<u8>, diff: Diff) -> Self {
        Self {
            title,
            diff,
            selected: 0,
            top: 0,
            page: 1,
            help: false,
        }
    }

    /// The selected file's header and hunk lines, each with its style.
    fn lines(&self) -> impl Iterator<Item = (&[u8], Style)> {
        let bytes = &self.diff.bytes;
        let file = self.diff.files.get(self.selected);
        let header = (file.into_iter().flat_map(|file| &file.header))
            .map(|range| (range, colour(Color::BrightBlack)));
        let hunks = file
            .into_iter()
            .flat_map(|file| &file.hunks)
            .flat_map(move |hunk| {
                let lines = hunk.lines.iter().map(move |range| {
                    let fg = match bytes[range.clone()].first() {
                        Some(b'+') => Color::Green,
                        Some(b'-') => Color::Red,
                        Some(b'\\') => Color::BrightBlack,
                        _ => Color::Default,
                    };
                    (range, colour(fg))
                });
                iter::once((&hunk.header, colour(Color::Cyan))).chain(lines)
            });
        header
            .chain(hunks)
            .map(|(range, style)| (&bytes[range.clone()], style))
    }

    fn last_top(&self) -> usize {
        let count = self.diff.files.get(self.selected).map_or(0, |file| {
            file.header.len()
                + file
                    .hunks
                    .iter()
                    .map(|hunk| 1 + hunk.lines.len())
                    .sum::<usize>()
        });
        count.saturating_sub(self.page)
    }
}

impl Tool for Viewer {
    fn draw(&mut self, screen: &mut Screen) {
        let (rows, columns) = screen.size();
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        let dim = colour(Color::BrightBlack);
        screen.put(0, 0, &self.title, bold);
        let files = self.diff.files.len();
        let place = format!("{}/{files}  ? keys", (self.selected + 1).min(files));
        let at = columns.saturating_sub(u16::try_from(place.len()).unwrap_or(u16::MAX));
        screen.put(0, at, place, dim);
        if self.help {
            for (row, keys) in (2..rows).zip(KEYS) {
                screen.put(row, 2, keys, Style::default());
            }
            return;
        }
        let mut top = 1;
        for note in &self.diff.notes {
            screen.put(top, 0, note, colour(Color::Yellow));
            top += 1;
        }
        if files == 0 {
            screen.put(top, 0, "No changes.", dim);
            return;
        }
        self.page = usize::from(rows.saturating_sub(top)).max(1);
        self.top = self.top.min(self.last_top());
        let left = columns / 4;
        let width = usize::from(left.saturating_sub(1));
        let first = (self.selected + 1).saturating_sub(self.page);
        for (row, (at, file)) in (top..rows).zip(self.diff.files.iter().enumerate().skip(first)) {
            let label: String = escape(&file.label()).chars().take(width).collect();
            let style = if at == self.selected {
                Style {
                    fg: Color::Black,
                    bg: Color::Blue,
                    bold: false,
                }
            } else {
                Style::default()
            };
            screen.put(row, 0, format!("{label:<width$}"), style);
        }
        for (row, (line, style)) in (top..rows).zip(self.lines().skip(self.top)) {
            let line = line
                .split(|&byte| byte == b'\t')
                .collect::<Vec<_>>()
                .join(&b"    "[..]);
            screen.put(row, left + 1, line, style);
        }
    }

    fn key(&mut self, key: Key) -> Flow {
        if self.help {
            self.help = false;
            return if key == Key::Char('q') {
                Flow::Quit
            } else {
                Flow::Continue
            };
        }
        let page = self.page;
        match key {
            Key::Char('q') => return Flow::Quit,
            Key::Char('?') => self.help = true,
            Key::Char('n') => {
                self.selected = (self.selected + 1).min(self.diff.files.len().saturating_sub(1));
                self.top = 0;
            }
            Key::Char('p') => {
                self.selected = self.selected.saturating_sub(1);
                self.top = 0;
            }
            Key::Char('j') | Key::Down => self.top += 1,
            Key::Char('k') | Key::Up => self.top = self.top.saturating_sub(1),
            Key::Char(' ' | 'f') | Key::PageDown => self.top += page,
            Key::Char('b') | Key::PageUp => self.top = self.top.saturating_sub(page),
            Key::Char('g') | Key::Home => self.top = 0,
            Key::Char('G') | Key::End => self.top = self.last_top(),
            _ => {}
        }
        self.top = self.top.min(self.last_top());
        Flow::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(diff: &Diff, range: &Range<usize>) -> String {
        String::from_utf8_lossy(&diff.bytes[range.clone()]).into_owned()
    }

    fn hunk_lines(diff: &Diff, file: usize) -> Vec<String> {
        let file = &diff.files[file];
        (file.hunks.iter().flat_map(|hunk| &hunk.lines))
            .map(|range| text(diff, range))
            .collect()
    }

    const CHANGE: &str = "diff --git a/src/main.rs b/src/main.rs\n\
        index 1a2b3c4..5d6e7f8 100644\n\
        --- a/src/main.rs\n\
        +++ b/src/main.rs\n\
        @@ -1,3 +1,3 @@ fn main() {\n \
        fn main() {\n\
        -    println!(\"hi\");\n\
        +    println!(\"hello\");\n \
        }\n\
        @@ -9 +9 @@\n\
        --- a/x\n\
        +@@ not a hunk\n";

    /// Every kind of file, with a tab, an escape and a bidi control in the changed lines.
    const FILES: &str = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
        diff --git a/logo.png b/logo.png\nindex 1..2 100644\n\
        Binary files a/logo.png and b/logo.png differ\n\
        diff --git a/old.txt b/new.txt\nsimilarity index 90%\nrename from old.txt\nrename to new.txt\n\
        diff --git \"a/x\\033[2J\" \"b/x\\033[2J\"\nnew file mode 100644\n--- /dev/null\n\
        +++ \"b/x\\033[2J\"\n@@ -0,0 +1 @@\n+\tgnp\u{202e}.exe\x1b]0;x\x07\n";

    /// The screen at 80×24 after `keys`, as rows and style runs.
    fn shown(diff: Diff, keys: &[Key]) -> String {
        let mut viewer = Viewer::new(b"diff main..HEAD".to_vec(), diff);
        for &key in keys {
            viewer.key(key);
        }
        let mut screen = Screen::new(24, 80);
        viewer.draw(&mut screen);
        viewer.draw(&mut screen);
        screen.runs()
    }

    #[test]
    fn a_unified_diff() {
        insta::assert_snapshot!(shown(parse(format!("{CHANGE}{FILES}").into()), &[]));
    }

    #[test]
    fn a_hostile_path_and_line_show_escaped() {
        let keys = [Key::Char('n'); 4];
        insta::assert_snapshot!(shown(parse(format!("{CHANGE}{FILES}").into()), &keys));
    }

    #[test]
    fn a_cut_short_diff() {
        let mut diff = parse(CHANGE.as_bytes()[..CHANGE.len() - 5].to_vec());
        diff.notes
            .push("git printed over 8 MB, so the diff is cut short.".into());
        insta::assert_snapshot!(shown(diff, &[]));
    }

    #[test]
    fn keys_scroll_within_the_diff_and_q_quits() {
        let mut viewer = Viewer::new(Vec::new(), parse(CHANGE.into()));
        let mut screen = Screen::new(5, 80);
        viewer.draw(&mut screen);
        assert_eq!(viewer.key(Key::Char('G')), Flow::Continue);
        // Eleven lines, four rows a page.
        assert_eq!(viewer.top, 7);
        viewer.key(Key::Char('j'));
        assert_eq!(viewer.top, 7);
        viewer.key(Key::Char('n'));
        assert_eq!((viewer.selected, viewer.top), (0, 0));
        assert_eq!(viewer.key(Key::Char('q')), Flow::Quit);
    }

    #[test]
    fn git_diff_runs_through_git_run_and_says_why_it_failed_or_was_cut_short() {
        let repo = std::env::temp_dir().join(format!("pinfold-diff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "diff.noprefix=true",
                ])
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(repo.join("a.txt"), "two\n").unwrap();
        let diff = load(&repo, false, None).unwrap();
        assert_eq!(diff.files[0].label(), b"a.txt");
        assert_eq!(hunk_lines(&diff, 0), ["-one", "+two"]);
        assert!(load(&repo, true, None).unwrap().files.is_empty());
        let problem = load(&repo, false, Some("nope..HEAD")).err().unwrap();
        assert!(problem.starts_with("git diff failed: fatal: "), "{problem}");
        std::fs::write(
            repo.join("big.txt"),
            format!("{}\n", "x".repeat(99)).repeat(100_000),
        )
        .unwrap();
        git(&["add", "-N", "big.txt"]);
        let diff = load(&repo, false, None).unwrap();
        assert_eq!(
            diff.notes,
            ["git printed over 8 MB, so the diff is cut short."]
        );
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn a_change_has_its_path_header_and_hunks() {
        let diff = parse(CHANGE.into());
        let [file] = &diff.files[..] else {
            panic!("one file")
        };
        assert_eq!(file.label(), b"src/main.rs");
        assert_eq!(file.header.len(), 3);
        assert_eq!(file.hunks.len(), 2);
        assert_eq!(
            text(&diff, &file.hunks[0].header),
            "@@ -1,3 +1,3 @@ fn main() {"
        );
        // A removed line that looks like a header stays in its hunk.
        assert_eq!(hunk_lines(&diff, 0)[4..], ["--- a/x", "+@@ not a hunk"]);
        assert!(diff.notes.is_empty());
    }

    #[test]
    fn a_rename_names_both_paths() {
        let diff = parse(
            "diff --git a/old name.txt b/new.txt\nsimilarity index 90%\n\
            rename from old name.txt\nrename to new.txt\n"
                .into(),
        );
        assert_eq!(diff.files[0].label(), "old name.txt → new.txt".as_bytes());
        assert_eq!(diff.files[0].header.len(), 3);
    }

    #[test]
    fn a_binary_file_shows_its_header_and_no_hunks() {
        let diff = parse(
            "diff --git a/logo.png b/logo.png\nindex 1..2 100644\n\
            Binary files a/logo.png and b/logo.png differ\n"
                .into(),
        );
        let file = &diff.files[0];
        assert_eq!(file.label(), b"logo.png");
        assert!(file.hunks.is_empty());
        let last = text(&diff, &file.header[1]);
        assert_eq!(last, "Binary files a/logo.png and b/logo.png differ");
    }

    #[test]
    fn a_mode_change_keeps_both_modes() {
        let diff = parse(
            "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
            diff --git a/gone b/gone\ndeleted file mode 100644\n--- a/gone\n+++ /dev/null\n\
            @@ -1 +0,0 @@\n-x\n"
                .into(),
        );
        let header = &diff.files[0].header;
        let header: Vec<_> = header.iter().map(|range| text(&diff, range)).collect();
        assert_eq!(header, ["old mode 100644", "new mode 100755"]);
        assert_eq!(diff.files[1].label(), b"gone", "/dev/null is not a path");
    }

    #[test]
    fn no_newline_at_end_stays_in_its_hunk() {
        let diff = parse(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+y\n"
                .into(),
        );
        let lines = hunk_lines(&diff, 0);
        assert_eq!(lines, ["-x", "\\ No newline at end of file", "+y"]);
    }

    #[test]
    fn a_diff_cut_anywhere_parses_what_is_there() {
        for at in 0..=CHANGE.len() {
            let diff = parse(CHANGE.as_bytes()[..at].to_vec());
            assert!(diff.files.len() <= 1);
        }
        let diff = parse(CHANGE.as_bytes()[..CHANGE.len() - 5].to_vec());
        assert_eq!(hunk_lines(&diff, 0).last().unwrap(), "+@@ not a ");
        // Mid-way through a quoted path or an escape.
        for cut in [
            &b"diff --git \"a/x\\"[..],
            b"diff --git \"a/\\30",
            b"--- \"a",
        ] {
            parse(cut.to_vec());
        }
    }

    #[test]
    fn quoted_paths_are_unquoted_to_their_bytes() {
        let diff = parse(
            b"diff --git \"a/t\\303\\244b\\tx\\033\" \"b/t\\303\\244b\\tx\\033\"\n\
            --- \"a/t\\303\\244b\\tx\\033\"\n+++ b/with space\t\n"
                .to_vec(),
        );
        let file = &diff.files[0];
        assert_eq!(file.old, "täb\tx\x1b".as_bytes());
        assert_eq!(file.new, b"with space");
    }

    #[test]
    fn past_the_file_cap_the_rest_is_dropped() {
        let diff = parse("diff --git a/a b/a\n".repeat(MAX_FILES + 1).into());
        assert_eq!(diff.files.len(), MAX_FILES);
        assert_eq!(
            diff.notes,
            [format!("Only the first {MAX_FILES} files show.")]
        );
    }

    #[test]
    fn past_the_hunk_cap_the_rest_is_dropped() {
        let diff = parse(
            format!(
                "diff --git a/a b/a\n{}",
                "@@ -1 +1 @@\n".repeat(MAX_HUNKS + 1)
            )
            .into(),
        );
        assert_eq!(diff.files[0].hunks.len(), MAX_HUNKS);
        assert_eq!(
            diff.notes,
            [format!("Only the first {MAX_HUNKS} hunks show.")]
        );
    }

    #[test]
    fn past_the_line_cap_the_rest_is_dropped() {
        let diff = parse(
            format!(
                "diff --git a/a b/a\n@@ -1 +1 @@\n{}",
                "+\n".repeat(MAX_LINES)
            )
            .into(),
        );
        assert_eq!(diff.files[0].hunks[0].lines.len(), MAX_LINES - 1);
        assert_eq!(
            diff.notes,
            [format!("Only the first {MAX_LINES} lines show.")]
        );
    }

    #[test]
    fn a_line_past_the_length_cap_is_cut_at_a_character() {
        let long = format!("+{}", "é".repeat(MAX_LINE));
        let diff = parse(format!("diff --git a/a b/a\n@@ -1 +1 @@\n{long}\n+short\n").into());
        let lines = hunk_lines(&diff, 0);
        assert_eq!(lines[0].len(), MAX_LINE - 1);
        assert!(!lines[0].contains('\u{FFFD}'));
        assert_eq!(lines[1], "+short");
        assert_eq!(
            diff.notes,
            [format!("Lines over {MAX_LINE} bytes are cut.")]
        );
    }
}
