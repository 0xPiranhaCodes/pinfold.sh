//! The unified-diff parser behind `pinfold diff` (ADR-0045 sections 2 and 6).
//!
//! Git's output is untrusted: names and lines come from the repository. [`parse`] takes any
//! bytes without panicking, keeps each line as a range into them, and caps each structure;
//! [`Diff::notes`] says which cap cut it. Nothing here is escaped: [`Screen::put`] does that.
//!
//! [`Screen::put`]: crate::screen::Screen::put

use std::ops::Range;

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
