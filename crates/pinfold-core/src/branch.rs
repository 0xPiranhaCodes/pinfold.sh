//! The git branch an open inbox's row shows (ADR-0051 §7), read from `HEAD` as data. No git
//! runs, and nothing is kept once it is sent.

use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use pinfold_proto::link::MAX_BRANCH;

/// The most bytes read from a `HEAD`, a worktree's `.git` file or its `gitdir` back link.
const MAX_READ: u64 = 256;

/// The branch of the repository `dir` is in, from the `.git` found walking up from it: the name
/// after `ref: refs/heads/` cut to [`MAX_BRANCH`] bytes of printable ASCII, a detached head's
/// first 7 hex digits, or empty for none. `dir` is the system's, never OSC 7's (T4).
pub(crate) fn branch(dir: &Path) -> String {
    let head = git_dir(dir).and_then(|git| read_small(&git.join("HEAD")));
    head.as_deref().map(shown).unwrap_or_default()
}

/// The nearest `.git` at or above `dir`, as git finds it: a directory, or a worktree's file.
/// Anything else there, such as a link, ends the walk with none.
fn git_dir(dir: &Path) -> Option<PathBuf> {
    for dir in dir.ancestors() {
        let dot = dir.join(".git");
        let Ok(meta) = fs::symlink_metadata(&dot) else {
            continue;
        };
        if meta.is_dir() {
            return Some(dot);
        }
        return meta.is_file().then(|| worktree(&dot)).flatten();
    }
    None
}

/// A worktree's admin directory from its `.git` file's `gitdir:` line, only if it is
/// `<common dir>/worktrees/<name>` and that names this `.git` file back, as `git worktree add`
/// writes it, so a planted file cannot send the read anywhere else in the repository's place.
fn worktree(dot: &Path) -> Option<PathBuf> {
    let text = read_small(dot)?;
    let admin = normal(&dot.parent()?.join(line(text.strip_prefix(b"gitdir: ")?)));
    if admin.parent()?.file_name()? != "worktrees" {
        return None;
    }
    let back = read_small(&admin.join("gitdir"))?;
    (normal(&admin.join(line(&back))) == normal(dot)).then_some(admin)
}

/// The first line of `text` as a path.
fn line(text: &[u8]) -> &Path {
    let end = text.iter().position(|&byte| byte == b'\n');
    Path::new(std::ffi::OsStr::from_bytes(
        &text[..end.unwrap_or(text.len())],
    ))
}

/// `path` with `.` and `..` resolved by name, never by following links.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// At most [`MAX_READ`] bytes of the regular file at `path`, never through a last link and
/// never blocking on a FIFO or device put there.
fn read_small(path: &Path) -> Option<Vec<u8>> {
    let file = pinfold_sys::open_no_follow(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::with_capacity(MAX_READ as usize);
    file.take(MAX_READ).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// What a row shows for `HEAD`'s bytes.
fn shown(head: &[u8]) -> String {
    let line = line(head).as_os_str().as_bytes();
    if let Some(name) = line.strip_prefix(b"ref: refs/heads/") {
        let printable = name.iter().filter(|byte| matches!(byte, b' '..=b'~'));
        return printable
            .take(MAX_BRANCH)
            .map(|&byte| char::from(byte))
            .collect();
    }
    let detached = matches!(line.len(), 40 | 64) && line.iter().all(u8::is_ascii_hexdigit);
    if detached {
        line[..7].iter().map(|&byte| char::from(byte)).collect()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;

    /// An empty directory by its real path, as the system reports a pane's directory.
    fn fresh(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("pinfold-branch-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    /// A fresh directory with `.git/HEAD` holding `head`.
    fn repo(name: &str, head: &[u8]) -> PathBuf {
        let root = fresh(name);
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), head).unwrap();
        root
    }

    #[test]
    fn a_branch_is_read_from_head_in_any_directory_below() {
        let root = repo("plain", b"ref: refs/heads/feat/inbox-8\n");
        let deep = root.join("crates/core/src");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(branch(&root), "feat/inbox-8");
        assert_eq!(branch(&deep), "feat/inbox-8");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_detached_head_shows_its_first_seven_hex_digits() {
        let root = repo("detached", b"55b0e32a1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f\n");
        assert_eq!(branch(&root), "55b0e32");
        fs::write(root.join(".git/HEAD"), [b'a'; 64]).unwrap();
        assert_eq!(branch(&root), "aaaaaaa", "SHA-256");
        fs::write(root.join(".git/HEAD"), b"55b0e32\n").unwrap();
        assert_eq!(branch(&root), "", "not an object name");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_repository_shows_none() {
        let root = fresh("none");
        // The temporary directory is under no repository on any test machine.
        assert_eq!(branch(&root), "");
        fs::create_dir(root.join(".git")).unwrap();
        assert_eq!(branch(&root), "", "a .git with no HEAD");
        fs::remove_dir_all(root).unwrap();
    }

    /// ADR-0051 §7: `HEAD` is data. A long one is cut, control and bidi bytes are dropped, and
    /// binary or a ref outside `refs/heads/` shows nothing.
    #[test]
    fn a_hostile_head_is_cut_cleaned_or_shown_as_none() {
        let mut long = b"ref: refs/heads/".to_vec();
        long.extend([b'x'; 10 * 1024]);
        let root = repo("hostile", &long);
        assert_eq!(branch(&root), "x".repeat(MAX_BRANCH));
        let cases: [(&[u8], &str); 6] = [
            (b"ref: refs/heads/a\x1b]0;pwned\x07b\n", "a]0;pwnedb"),
            ("ref: refs/heads/ma\u{202e}in\u{2066}\n".as_bytes(), "main"),
            (b"ref: refs/heads/one\ntwo\n", "one"),
            (b"ref: refs/heads/\r\n", ""),
            (b"ref: refs/remotes/origin/main\n", ""),
            (b"\x00\xff\xfe\x7fELF\x01\x02", ""),
        ];
        for (head, want) in cases {
            fs::write(root.join(".git/HEAD"), head).unwrap();
            let got = branch(&root);
            assert_eq!(got, want, "{head:?}");
            assert!(got.bytes().all(|byte| matches!(byte, b' '..=b'~')));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_symlinked_head_is_not_followed() {
        let root = repo("link", b"");
        fs::write(root.join("elsewhere"), b"ref: refs/heads/secret\n").unwrap();
        fs::remove_file(root.join(".git/HEAD")).unwrap();
        symlink(root.join("elsewhere"), root.join(".git/HEAD")).unwrap();
        assert_eq!(branch(&root), "");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fifo_named_head_does_not_block() {
        let root = repo("fifo", b"");
        let head = root.join(".git/HEAD");
        fs::remove_file(&head).unwrap();
        let made = Command::new("mkfifo").arg(&head).status().unwrap();
        assert!(made.success());
        let start = Instant::now();
        assert_eq!(branch(&root), "");
        assert!(start.elapsed() < Duration::from_secs(1));
        fs::remove_dir_all(root).unwrap();
    }

    /// A worktree's `.git` file is followed only to `<common dir>/worktrees/<name>` that names
    /// it back.
    #[test]
    fn a_worktree_s_head_is_read_only_through_its_own_admin_directory() {
        let main = repo("worktree", b"ref: refs/heads/main\n");
        let admin = main.join(".git/worktrees/w1");
        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), b"ref: refs/heads/side\n").unwrap();
        let tree = main.join("trees/w1");
        fs::create_dir_all(&tree).unwrap();
        let dot = tree.join(".git");
        fs::write(&dot, format!("gitdir: {}\n", admin.display())).unwrap();
        // Not yet named back.
        assert_eq!(branch(&tree), "");
        fs::write(admin.join("gitdir"), format!("{}\n", dot.display())).unwrap();
        assert_eq!(branch(&tree), "side");
        // Relative, as `worktree.useRelativePaths` writes both.
        fs::write(&dot, b"gitdir: ../../.git/worktrees/w1\n").unwrap();
        fs::write(admin.join("gitdir"), b"../../../trees/w1/.git\n").unwrap();
        assert_eq!(branch(&tree), "side");
        // Anywhere but a `worktrees` directory is refused, even a git dir that names it back.
        let other = main.join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("HEAD"), b"ref: refs/heads/planted\n").unwrap();
        fs::write(other.join("gitdir"), format!("{}\n", dot.display())).unwrap();
        fs::write(&dot, format!("gitdir: {}\n", other.display())).unwrap();
        assert_eq!(branch(&tree), "");
        fs::write(&dot, b"gitdir: /dev/zero\n").unwrap();
        assert_eq!(branch(&tree), "");
        fs::remove_dir_all(main).unwrap();
    }
}
