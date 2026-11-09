//! What `pinfold files` does to files: rename, copy, move and trash (ADR-0045 section 4).
//!
//! Each acts on a link itself, never on what it points at, and never replaces an existing name.
//! Move and trash are renames, so they refuse to cross a file system rather than copy. Messages
//! name files escaped, as the screen shows them.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::screen::escape;

/// Bytes a copy may hold in all; a larger one is refused before anything is written.
pub(crate) const MAX_COPY: u64 = 256 * 1024 * 1024;
/// Entries a copy may hold in all, as a directory lists at most.
const MAX_COPY_ENTRIES: u64 = 10_000;

/// Escaped, as every message shows a name.
fn shown(path: &Path) -> String {
    escape(path.as_os_str().as_bytes())
}

/// Refuses an empty name, `.`, `..`, or one holding `/` or NUL, so a rename stays in its
/// directory.
pub(crate) fn check_name(name: &OsStr) -> Result<(), String> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(format!("{} is not a name", escape(bytes)));
    }
    if bytes.contains(&0) {
        return Err("a name cannot hold NUL".to_owned());
    }
    Ok(())
}

/// Refuses if anything, even a dangling link, is at `to`.
fn vacant(to: &Path) -> Result<(), String> {
    match fs::symlink_metadata(to) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(format!("{} exists; nothing was replaced", shown(to))),
        Err(error) => Err(format!("cannot check {}: {error}", shown(to))),
    }
}

/// Renames `from` to `to`, refusing an existing `to` or another file system.
// shortcut: `to` is checked, then renamed; a name made between the two is replaced. Upgrade to
// renameat2(RENAME_NOREPLACE) and renamex_np(RENAME_EXCL) in pinfold-sys if that matters.
fn rename(from: &Path, to: &Path) -> Result<(), String> {
    vacant(to)?;
    fs::rename(from, to).map_err(|error| match error.kind() {
        io::ErrorKind::CrossesDevices => format!(
            "{} is on another file system; nothing is copied instead",
            shown(from)
        ),
        _ => format!("cannot move {}: {error}", shown(from)),
    })
}

/// Renames `from` to `name` in its own directory.
pub(crate) fn rename_to(from: &Path, name: &OsStr) -> Result<(), String> {
    check_name(name)?;
    let dir = from.parent().ok_or("cannot rename /")?;
    rename(from, &dir.join(name))
}

/// Moves `from` into `dir` under its own name.
pub(crate) fn move_into(from: &Path, dir: &Path) -> Result<(), String> {
    let name = from.file_name().ok_or("cannot move /")?;
    inside(from, dir)?;
    rename(from, &dir.join(name))
}

/// Refuses `dir` if it is `from` or under it.
fn inside(from: &Path, dir: &Path) -> Result<(), String> {
    let (from, dir) = (from.canonicalize(), dir.canonicalize());
    match (from, dir) {
        (Ok(from), Ok(dir)) if dir.starts_with(&from) => {
            Err(format!("{} is inside {}", shown(&dir), shown(&from)))
        }
        _ => Ok(()),
    }
}

/// Copies `from` into `dir` under its own name: a link as a link, a directory with what it
/// holds. Refused past [`MAX_COPY`] bytes or 10,000 entries, before anything is written.
pub(crate) fn copy_into(from: &Path, dir: &Path) -> Result<(), String> {
    let name = from.file_name().ok_or("cannot copy /")?;
    inside(from, dir)?;
    let to = dir.join(name);
    vacant(&to)?;
    let (mut bytes, mut entries) = (0, 0);
    size(from, &mut bytes, &mut entries)
        .map_err(|error| format!("cannot read {}: {error}", shown(from)))?;
    if bytes > MAX_COPY || entries > MAX_COPY_ENTRIES {
        return Err(format!(
            "{} is over the copy cap of 256 MiB or 10,000 entries",
            shown(from)
        ));
    }
    copy(from, &to).map_err(|error| format!("cannot copy {}: {error}", shown(from)))
}

/// Adds up the bytes and entries under `path`, stopping once past the caps.
fn size(path: &Path, bytes: &mut u64, entries: &mut u64) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    *bytes += meta.len();
    *entries += 1;
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            if *bytes > MAX_COPY || *entries > MAX_COPY_ENTRIES {
                break;
            }
            size(&entry?.path(), bytes, entries)?;
        }
    }
    Ok(())
}

fn copy(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)
    } else if kind.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else if kind.is_file() {
        let mut source = File::open(from)?;
        let opened = source.metadata()?;
        // A link swapped in after the check is opened through, so it is caught here.
        if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
            return Err(io::Error::other("it changed while being copied"));
        }
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(meta.mode() & 0o777)
            .open(to)?;
        io::copy(&mut source, &mut target).map(drop)
    } else {
        Err(io::Error::other(
            "only files, directories and links are copied",
        ))
    }
}

/// Where trash goes: `~/.Trash` on macOS, else the XDG home trash, `$XDG_DATA_HOME/Trash` or
/// `~/.local/share/Trash`, and whether it is the XDG kind with `files` and `info`.
pub(crate) fn trash_dir() -> Option<(PathBuf, bool)> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if cfg!(target_os = "macos") {
        return Some((home.join(".Trash"), false));
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|data| data.is_absolute())
        .unwrap_or_else(|| home.join(".local/share"));
    Some((data.join("Trash"), true))
}

/// Renames `path` into the trash at `trash` under a name not yet taken there, writing the XDG
/// `.trashinfo` first if `xdg`. Refuses before anything moves if the trash is on another file
/// system: trash is a rename, never a copy.
pub(crate) fn trash(path: &Path, (trash, xdg): (&Path, bool)) -> Result<(), String> {
    let name = path.file_name().ok_or("cannot trash /")?;
    let files = if xdg {
        trash.join("files")
    } else {
        trash.to_path_buf()
    };
    let made = |dir: &Path| DirBuilder::new().recursive(true).mode(0o700).create(dir);
    made(&files)
        .and_then(|()| {
            if xdg {
                made(&trash.join("info"))
            } else {
                Ok(())
            }
        })
        .map_err(|error| format!("cannot make the trash {}: {error}", shown(trash)))?;
    let device = |path: &Path| fs::symlink_metadata(path).map(|meta| meta.dev());
    match (device(path), device(&files)) {
        (Ok(here), Ok(there)) if here == there => {}
        (Ok(_), Ok(_)) => {
            return Err(format!(
                "{} is on another file system from the trash; nothing was moved",
                shown(path)
            ));
        }
        (Err(error), _) | (_, Err(error)) => {
            return Err(format!("cannot trash {}: {error}", shown(path)));
        }
    }
    for n in 1..1_000 {
        let mut taken = name.to_owned();
        if n > 1 {
            taken.push(format!(".{n}"));
        }
        let to = files.join(&taken);
        if !xdg {
            if vacant(&to).is_ok() {
                return rename(path, &to);
            }
            continue;
        }
        // Creating the info file first claims the name, as the XDG spec asks.
        let mut info = taken.clone();
        info.push(".trashinfo");
        let info = trash.join("info").join(info);
        let open = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info);
        let mut file = match open {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot write {}: {error}", shown(&info))),
        };
        let written = file.write_all(&trash_info(path, SystemTime::now()));
        let moved = written
            .map_err(|error| format!("cannot write {}: {error}", shown(&info)))
            .and_then(|()| rename(path, &to));
        if moved.is_err() {
            let _ = fs::remove_file(&info);
        }
        return moved;
    }
    Err(format!("no free name in the trash for {}", shown(path)))
}

/// An XDG `.trashinfo`: the absolute path, percent-encoded, and the deletion time.
fn trash_info(path: &Path, now: SystemTime) -> Vec<u8> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut info = b"[Trash Info]\nPath=".to_vec();
    for &byte in absolute.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            info.push(byte);
        } else {
            info.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    info.extend_from_slice(format!("\nDeletionDate={}\n", date(seconds)).as_bytes());
    info
}

/// `YYYY-MM-DDThh:mm:ss` for `seconds` since 1970, from Howard Hinnant's `civil_from_days`.
// shortcut: UTC, where the spec asks for local time; std has no time zone, so read TZ through
// pinfold-sys if a trash viewer's dates matter.
fn date(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let (hour, minute, second) = (rest / 3_600, rest / 60 % 60, rest % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pinfold-ops-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("dir/sub")).unwrap();
        fs::write(root.join("dir/sub/a.txt"), "a").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        std::os::unix::fs::symlink("b.txt", root.join("link")).unwrap();
        root
    }

    #[test]
    fn a_rename_stays_in_its_directory_and_replaces_nothing() {
        let root = scratch("rename");
        for name in ["", ".", "..", "a/b", "../x", "a\0b"] {
            assert!(
                rename_to(&root.join("b.txt"), OsStr::new(name)).is_err(),
                "{name:?}"
            );
        }
        let refused = rename_to(&root.join("b.txt"), OsStr::new("dir")).unwrap_err();
        assert!(
            refused.ends_with("dir exists; nothing was replaced"),
            "{refused}"
        );
        // A link is renamed itself, and still points where it did.
        rename_to(&root.join("link"), OsStr::new("renamed")).unwrap();
        assert_eq!(
            fs::read_link(root.join("renamed")).unwrap(),
            Path::new("b.txt")
        );
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"b");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_move_takes_the_link_and_never_goes_into_itself() {
        let root = scratch("move");
        move_into(&root.join("link"), &root.join("dir")).unwrap();
        assert!(
            fs::symlink_metadata(root.join("dir/link"))
                .unwrap()
                .is_symlink()
        );
        assert!(root.join("b.txt").is_file(), "the link's target stayed");
        assert!(move_into(&root.join("dir"), &root.join("dir/sub")).is_err());
        fs::write(root.join("dir/b.txt"), "other").unwrap();
        assert!(move_into(&root.join("b.txt"), &root.join("dir")).is_err());
        assert_eq!(fs::read(root.join("dir/b.txt")).unwrap(), b"other");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_copy_keeps_links_as_links_and_stops_at_its_cap() {
        let root = scratch("copy");
        let into = root.join("into");
        fs::create_dir(&into).unwrap();
        copy_into(&root.join("dir"), &into).unwrap();
        copy_into(&root.join("link"), &into).unwrap();
        assert_eq!(fs::read(into.join("dir/sub/a.txt")).unwrap(), b"a");
        assert_eq!(
            fs::read_link(into.join("link")).unwrap(),
            Path::new("b.txt")
        );
        assert!(
            copy_into(&root.join("b.txt"), &root).is_err(),
            "onto itself"
        );
        assert!(copy_into(&root.join("dir"), &root.join("dir/sub")).is_err());
        let big = root.join("big");
        File::create(&big).unwrap().set_len(MAX_COPY + 1).unwrap();
        let refused = copy_into(&big, &into).unwrap_err();
        assert!(refused.contains("over the copy cap"), "{refused}");
        assert!(!into.join("big").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trash_writes_its_info_first_and_never_takes_a_name_twice() {
        let root = scratch("trash");
        let bin = root.join("Trash");
        for _ in 0..2 {
            fs::write(root.join("b.txt"), "b").unwrap();
            trash(&root.join("b.txt"), (&bin, true)).unwrap();
        }
        assert!(bin.join("files/b.txt").is_file() && bin.join("files/b.txt.2").is_file());
        let info = fs::read_to_string(bin.join("info/b.txt.2.trashinfo")).unwrap();
        let path = root.join("b.txt");
        assert!(info.starts_with(&format!("[Trash Info]\nPath={}\n", path.display())));
        trash(&root.join("link"), (&root.join("mac"), false)).unwrap();
        assert!(
            fs::symlink_metadata(root.join("mac/link"))
                .unwrap()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trash_on_another_file_system_is_refused_and_nothing_moves() {
        let root = scratch("exdev");
        // /dev is devfs or devtmpfs, never the temp directory's file system.
        let refused = trash(&root.join("b.txt"), (Path::new("/dev"), false)).unwrap_err();
        assert!(refused.contains("another file system"), "{refused}");
        assert!(root.join("b.txt").is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trash_info_percent_encodes_the_path_and_dates_in_utc() {
        let at = UNIX_EPOCH + std::time::Duration::from_secs(1_762_689_600);
        let info = trash_info(Path::new("/a b/%\n.txt"), at);
        let expected = "[Trash Info]\nPath=/a%20b/%25%0A.txt\nDeletionDate=2025-11-09T12:00:00\n";
        assert_eq!(String::from_utf8(info).unwrap(), expected);
        assert_eq!(date(951_782_400), "2000-02-29T00:00:00");
    }
}
