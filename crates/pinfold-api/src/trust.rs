//! Workspaces a person trusted: canonical directory and layout SHA-256 pairs in `trust.json`.
//!
//! A same-user program can edit the file, as it can edit `~/.zshrc`, so trust guards against a
//! cloned repository, not against code already running as you (ADR-0034, point 6). The file is
//! read on each question, never held, and written whole through a temporary file and a rename.
//!
//! ```
//! use pinfold_api::trust::{self, State};
//!
//! let path = std::env::temp_dir().join(format!("pinfold-trust-doc-{}.json", std::process::id()));
//! let sha256 = "0".repeat(64);
//! assert_eq!(trust::state(&path, "/home/me/project", &sha256), State::Unknown);
//! trust::record(&path, "/home/me/project", &sha256)?;
//! assert_eq!(trust::state(&path, "/home/me/project", &sha256), State::Trusted);
//! assert_eq!(trust::state(&path, "/home/me/project", &"1".repeat(64)), State::Changed);
//! # std::fs::remove_file(&path)?;
//! # Ok::<(), std::io::Error>(())
//! ```

use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde_json::{Value, json};

/// Most pairs kept; trusting one more drops the oldest.
pub const MAX_ENTRIES: usize = 1000;
/// Largest file read or written: past it, writing drops the oldest pairs until the rest fit.
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// What the store says about a directory and a layout's hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// This directory was trusted with this hash.
    Trusted,
    /// This directory was trusted with another hash: the layout changed since.
    Changed,
    /// This directory was never trusted.
    Unknown,
}

/// The SHA-256 of a layout file's bytes, as the store keys it: 64 lowercase hexadecimal digits.
///
/// ```
/// let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// assert_eq!(pinfold_api::trust::sha256(b""), empty);
/// ```
#[must_use]
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Looks `dir` and `sha256` up in the store at `path`. A missing or unreadable store trusts
/// nothing.
#[must_use]
pub fn state(path: &Path, dir: &str, sha256: &str) -> State {
    match load(path).iter().find(|(known, _)| known == dir) {
        Some((_, hash)) if hash == sha256 => State::Trusted,
        Some(_) => State::Changed,
        None => State::Unknown,
    }
}

/// Records `dir` as trusted with `sha256`, replacing its earlier hash, and writes the store
/// with mode 0600. Past [`MAX_ENTRIES`], or past the bytes the store may hold, the oldest pairs
/// go.
///
/// # Errors
///
/// When the directory or the file cannot be written, or the pair alone is too large to store.
pub fn record(path: &Path, dir: &str, sha256: &str) -> io::Result<()> {
    let mut entries = load(path);
    entries.retain(|(known, _)| known != dir);
    entries.push((dir.to_owned(), sha256.to_owned()));
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    let mut text = serialize(&entries);
    while text.len() as u64 > MAX_BYTES {
        if entries.len() == 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the directory is too long to store",
            ));
        }
        entries.remove(0);
        text = serialize(&entries);
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let temporary = path.with_extension("json.tmp");
    // A leftover temporary file, or a symlink planted there, is removed; `create_new` follows none.
    let _ = fs::remove_file(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    // The rename lasts only once the directory is on disk too.
    File::open(parent)?.sync_all()
}

fn serialize(entries: &[(String, String)]) -> String {
    let trusted: Vec<Value> = entries
        .iter()
        .map(|(dir, sha256)| json!({"dir": dir, "sha256": sha256}))
        .collect();
    json!({"version": 1, "trusted": trusted}).to_string()
}

/// The pairs in the file, oldest first. A store that will not parse is moved aside to
/// `trust.json.bad`, so it is not lost and not trusted, and the store starts empty. [`record`]
/// never writes a directory twice, so a store that names one twice will not parse.
fn load(path: &Path) -> Vec<(String, String)> {
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes));
    match read {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Ok(_) => {
            if let Some(entries) = parse(&bytes) {
                return entries;
            }
        }
        Err(_) => {}
    }
    // Best effort: a store that cannot be moved aside still trusts nothing.
    let _ = fs::rename(path, path.with_extension("json.bad"));
    Vec::new()
}

fn parse(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version")?.as_u64()? != 1 {
        return None;
    }
    let pair = |entry: &Value| {
        let dir = entry.get("dir")?.as_str()?;
        let sha256 = entry.get("sha256")?.as_str()?;
        Some((dir.to_owned(), sha256.to_owned()))
    };
    let entries: Vec<(String, String)> = value
        .get("trusted")?
        .as_array()?
        .iter()
        .map(pair)
        .collect::<Option<_>>()?;
    let mut dirs = HashSet::with_capacity(entries.len());
    entries
        .iter()
        .all(|(dir, _)| dirs.insert(dir.as_str()))
        .then_some(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn path() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "pinfold-trust-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&directory);
        directory.join("state/trust.json")
    }

    fn hash(digit: u32) -> String {
        char::from_digit(digit, 10).unwrap().to_string().repeat(64)
    }

    #[test]
    fn a_trusted_pair_round_trips_in_a_private_file_and_a_new_hash_replaces_it() {
        let path = path();
        assert_eq!(state(&path, "/a", &hash(1)), State::Unknown);
        record(&path, "/a", &hash(1)).unwrap();
        record(&path, "/b", &hash(2)).unwrap();
        assert_eq!(state(&path, "/a", &hash(1)), State::Trusted);
        assert_eq!(state(&path, "/a", &hash(2)), State::Changed);
        assert_eq!(state(&path, "/c", &hash(2)), State::Unknown);
        record(&path, "/a", &hash(3)).unwrap();
        assert_eq!(state(&path, "/a", &hash(1)), State::Changed);
        assert_eq!(load(&path).len(), 2);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(r#"{"trusted":[{"dir":"/b""#), "{text}");
        assert!(text.contains(r#""version":1"#), "{text}");
    }

    #[test]
    fn the_store_keeps_the_newest_thousand() {
        let path = path();
        for index in 0..=MAX_ENTRIES {
            record(&path, &format!("/p{index}"), &hash(1)).unwrap();
        }
        let entries = load(&path);
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert_eq!(state(&path, "/p0", &hash(1)), State::Unknown);
        assert_eq!(
            state(&path, &format!("/p{MAX_ENTRIES}"), &hash(1)),
            State::Trusted
        );
    }

    #[test]
    fn the_store_stays_small_enough_to_read_back() {
        let path = path();
        let long = format!("/{}", "\u{1}".repeat(400_000));
        for index in 0..4 {
            record(&path, &format!("{long}{index}"), &hash(1)).unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES);
        assert_eq!(state(&path, &format!("{long}3"), &hash(1)), State::Trusted);
        assert_eq!(state(&path, &format!("{long}0"), &hash(1)), State::Unknown);
        let huge = "/".repeat(usize::try_from(MAX_BYTES).unwrap());
        assert!(record(&path, &huge, &hash(1)).is_err());
        assert_eq!(state(&path, &format!("{long}3"), &hash(1)), State::Trusted);
    }

    #[test]
    fn a_corrupt_store_is_moved_aside_and_trusts_nothing() {
        for corrupt in [
            &b"{not json"[..],
            br#"{"version":2,"trusted":[]}"#,
            br#"{"version":1,"trusted":[{"dir":"/a"}]}"#,
            br#"{"version":1,"trusted":[{"dir":"/a","sha256":"0"},{"dir":"/a","sha256":"1"}]}"#,
            b"\xff\xfe",
        ] {
            let path = path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, corrupt).unwrap();
            assert_eq!(state(&path, "/a", &hash(1)), State::Unknown);
            assert!(!path.exists());
            assert_eq!(fs::read(path.with_extension("json.bad")).unwrap(), corrupt);
            record(&path, "/a", &hash(1)).unwrap();
            assert_eq!(state(&path, "/a", &hash(1)), State::Trusted);
        }
    }
}
