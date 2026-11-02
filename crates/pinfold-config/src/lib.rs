//! The one TOML config file of ADR-0011: its schema, validation and safe reading.
//!
//! The file is untrusted input, so nothing here panics on what it holds.

use std::env;
use std::ffi::OsString;
use std::fs::{Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, de};

/// The largest file read. A file written by hand is a few KiB, and this bounds startup.
pub const MAX_BYTES: u64 = 64 * 1024;

const MIN_FONT_SIZE: f32 = 6.0;
const MAX_FONT_SIZE: f32 = 72.0;

/// Settings read at startup. Every key is optional.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Whether Option sends Alt on macOS, instead of composing text. Ignored elsewhere.
    pub macos_option_as_alt: bool,
    /// In points, where one point is one logical pixel.
    #[serde(deserialize_with = "font_size")]
    pub font_size: f32,
    /// A font file to use instead of the bundled font. It is checked when it is opened, and
    /// a bad one means the bundled font.
    #[serde(deserialize_with = "font_path")]
    pub font_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            macos_option_as_alt: false,
            font_size: 13.0,
            font_path: None,
        }
    }
}

fn font_size<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    let size = f32::deserialize(deserializer)?;
    // Written so that NaN is refused too.
    if (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
        Ok(size)
    } else {
        Err(de::Error::custom(format!(
            "font_size must be from {MIN_FONT_SIZE} to {MAX_FONT_SIZE} points, not {size}"
        )))
    }
}

fn font_path<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<PathBuf>, D::Error> {
    let text = String::deserialize(deserializer)?;
    // The path may reach the terminal Pinfold started from in a message.
    if Path::new(&text).is_absolute() && !text.contains(char::is_control) {
        Ok(Some(PathBuf::from(text)))
    } else {
        Err(de::Error::custom(format!(
            "font_path must be an absolute path with no control characters, not {text:?}"
        )))
    }
}

/// Why a config file was not used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be opened or read.
    #[error("cannot read it: {0}")]
    Read(#[from] io::Error),
    /// A directory, FIFO or device is in the file's place.
    #[error("it is not a regular file")]
    NotAFile,
    /// The threat model refuses a file another user could have written.
    #[error("it is owned by another user")]
    NotOwned,
    /// The threat model refuses a file another user could write.
    #[error("it is writable by group or others, `chmod go-w` it")]
    Writable,
    /// The file is over [`MAX_BYTES`].
    #[error("it is over the limit of {MAX_BYTES} bytes")]
    TooLarge,
    /// The file is not text.
    #[error("it is not UTF-8")]
    NotUtf8,
    /// The TOML is invalid, or a key is unknown or has a bad value.
    ///
    /// The text names the key and line and quotes the file, without control characters.
    #[error("{0}")]
    Invalid(String),
}

/// Returns where the config file belongs, or nothing if no base directory is set.
///
/// This is `$XDG_CONFIG_HOME/pinfold/pinfold.toml`, or `~/.config/pinfold/pinfold.toml`,
/// on Linux and macOS alike.
pub fn path() -> Option<PathBuf> {
    path_from(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    // The XDG spec says to ignore a relative path, and an empty one counts as unset.
    let absolute = |dir: Option<OsString>| dir.map(PathBuf::from).filter(|dir| dir.is_absolute());
    let base = absolute(xdg).or_else(|| Some(absolute(home)?.join(".config")))?;
    Some(base.join("pinfold/pinfold.toml"))
}

/// Reads the config file, following a symlink, or returns the defaults if there is none.
///
/// # Errors
///
/// Returns an error if the file cannot be read, is refused, or is invalid.
pub fn load(path: &Path) -> Result<Config, Error> {
    // Non-blocking, so a FIFO in the file's place cannot hang startup.
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        file => file?,
    };
    // Asked of the open file, so what is checked is what is read.
    check(&file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    parse(&bytes)
}

fn check(metadata: &Metadata) -> Result<(), Error> {
    if !metadata.is_file() {
        Err(Error::NotAFile)
    } else if metadata.uid() != pinfold_sys::effective_uid() {
        Err(Error::NotOwned)
    } else if metadata.mode() & 0o022 != 0 {
        Err(Error::Writable)
    } else {
        Ok(())
    }
}

/// Parses the contents of a config file.
///
/// # Errors
///
/// Returns an error if `bytes` is not UTF-8 or not a valid config.
///
/// # Examples
///
/// ```
/// let config = pinfold_config::parse(b"font_size = 15")?;
/// assert_eq!(config.font_size, 15.0);
/// assert!(pinfold_config::parse(b"unknown_key = 15").is_err());
/// # Ok::<(), pinfold_config::Error>(())
/// ```
pub fn parse(bytes: &[u8]) -> Result<Config, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    toml::from_str(text).map_err(|error| {
        // The message quotes the file, and it goes to the terminal Pinfold started from.
        let clean = |c: char| c.is_control() && c != '\n';
        Error::Invalid(error.to_string().replace(clean, "\u{fffd}"))
    })
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Command;

    use proptest::prelude::*;
    use tempfile::{TempDir, tempdir};

    use super::*;

    fn message(bytes: &[u8]) -> String {
        parse(bytes)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    }

    fn file(contents: &[u8], mode: u32) -> io::Result<(TempDir, PathBuf)> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, contents)?;
        fs::set_permissions(&path, Permissions::from_mode(mode))?;
        Ok((dir, path))
    }

    #[test]
    fn keys_are_optional() {
        assert_eq!(parse(b"").unwrap(), Config::default());
        assert_eq!(parse(b"# nothing yet\n").unwrap(), Config::default());
    }

    #[test]
    fn both_keys_are_read() {
        let config = parse(b"macos_option_as_alt = true\nfont_size = 14.5\n").unwrap();
        assert!(config.macos_option_as_alt);
        assert_eq!(config.font_size, 14.5);
        assert_eq!(parse(b"font_size = 14").unwrap().font_size, 14.0);
    }

    #[test]
    fn unknown_keys_are_named_with_their_line() {
        let text = message(b"font_size = 14\nmacos_option_as_alr = true\n");
        assert!(text.contains("line 2"), "{text}");
        assert!(text.contains("macos_option_as_alr"), "{text}");
    }

    #[test]
    fn bad_font_sizes_are_named_with_their_line() {
        for value in ["5.9", "72.1", "0", "-13", "nan", "inf", "1e300"] {
            let text = message(format!("\nfont_size = {value}\n").as_bytes());
            assert!(text.contains("line 2"), "{value}: {text}");
            assert!(text.contains("from 6 to 72"), "{value}: {text}");
        }
        assert!(message(b"font_size = \"big\"").contains("font_size"));
        assert_eq!(parse(b"font_size = 6").unwrap().font_size, 6.0);
        assert_eq!(parse(b"font_size = 72").unwrap().font_size, 72.0);
    }

    #[test]
    fn a_font_path_is_read() {
        assert_eq!(parse(b"").unwrap().font_path, None);
        let config = parse(b"font_path = \"/fonts/Mono.ttf\"").unwrap();
        assert_eq!(config.font_path, Some(PathBuf::from("/fonts/Mono.ttf")));
    }

    #[test]
    fn bad_font_paths_are_named_with_their_line() {
        for value in ["\"\"", "\"Mono.ttf\"", "\"~/Mono.ttf\"", "\"/a\\u001bb\""] {
            let text = message(format!("\nfont_path = {value}\n").as_bytes());
            assert!(text.contains("line 2"), "{value}: {text}");
            assert!(text.contains("absolute path"), "{value}: {text}");
            assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text}");
        }
        assert!(message(b"font_path = 1").contains("font_path"));
    }

    #[test]
    fn wrong_types_are_errors() {
        assert!(message(b"macos_option_as_alt = 1").contains("line 1"));
    }

    #[test]
    fn non_utf8_is_an_error() {
        assert!(matches!(parse(b"\xff\xfe"), Err(Error::NotUtf8)));
    }

    #[test]
    fn control_characters_never_reach_the_message() {
        let text = message(b"x = \"\x1b]0;title\x07\"\n");
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text}");
    }

    #[test]
    fn nesting_at_the_size_limit_does_not_overflow_the_stack() {
        let half = MAX_BYTES as usize / 2;
        let cases = [
            format!("x = {}", "[".repeat(half)),
            format!("x = {}", "{a=".repeat(half / 2)),
            format!("{}a=1", "a.".repeat(half - 2)),
            format!("[{}a]", "a.".repeat(half - 2)),
        ];
        for case in cases {
            assert!(parse(case.as_bytes()).is_err());
        }
    }

    #[test]
    fn the_path_follows_xdg_then_home() {
        let some = |dir: &str| Some(OsString::from(dir));
        let expected = PathBuf::from("/xdg/pinfold/pinfold.toml");
        assert_eq!(path_from(some("/xdg"), some("/home/me")), Some(expected));
        let expected = PathBuf::from("/home/me/.config/pinfold/pinfold.toml");
        for xdg in [None, some(""), some("relative")] {
            assert_eq!(path_from(xdg, some("/home/me")).as_ref(), Some(&expected));
        }
        assert_eq!(path_from(None, None), None);
        assert_eq!(path_from(None, some("")), None);
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = tempdir().unwrap();
        let config = load(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn a_file_is_read() {
        let (_dir, path) = file(b"font_size = 20", 0o600).unwrap();
        assert_eq!(load(&path).unwrap().font_size, 20.0);
    }

    #[test]
    fn a_symlink_is_followed_and_its_target_is_checked() {
        let (dir, path) = file(b"font_size = 20", 0o600).unwrap();
        let link = dir.path().join("link.toml");
        symlink(&path, &link).unwrap();
        assert_eq!(load(&link).unwrap().font_size, 20.0);
        fs::set_permissions(&path, Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(load(&link), Err(Error::Writable)));
    }

    #[test]
    fn a_symlink_loop_is_an_error() {
        let dir = tempdir().unwrap();
        let link = dir.path().join("config.toml");
        symlink(&link, &link).unwrap();
        assert!(matches!(load(&link), Err(Error::Read(_))));
    }

    #[test]
    fn a_directory_is_refused() {
        let dir = tempdir().unwrap();
        assert!(matches!(load(dir.path()), Err(Error::NotAFile)));
    }

    #[test]
    fn a_device_is_refused_without_reading_it() {
        assert!(matches!(load(Path::new("/dev/zero")), Err(Error::NotAFile)));
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let status = Command::new("mkfifo").arg(&path).status().unwrap();
        assert!(status.success());
        assert!(matches!(load(&path), Err(Error::NotAFile)));
    }

    #[test]
    fn group_or_other_write_is_refused() {
        for mode in [0o620, 0o602, 0o666] {
            let (_dir, path) = file(b"", mode).unwrap();
            assert!(matches!(load(&path), Err(Error::Writable)), "{mode:o}");
        }
    }

    #[test]
    fn another_users_file_is_refused() {
        if pinfold_sys::effective_uid() != 0 {
            let refused = load(Path::new("/etc/passwd"));
            assert!(matches!(refused, Err(Error::NotOwned)));
        }
    }

    #[test]
    fn an_unreadable_file_is_an_error() {
        if pinfold_sys::effective_uid() != 0 {
            let (_dir, path) = file(b"", 0o000).unwrap();
            assert!(matches!(load(&path), Err(Error::Read(_))));
        }
    }

    #[test]
    fn the_size_limit_is_exact() {
        let limit = MAX_BYTES as usize;
        let (_dir, path) = file(&vec![b'\n'; limit], 0o600).unwrap();
        assert_eq!(load(&path).unwrap(), Config::default());
        let (_dir, path) = file(&vec![b'\n'; limit + 1], 0o600).unwrap();
        assert!(matches!(load(&path), Err(Error::TooLarge)));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = parse(&bytes);
        }

        #[test]
        fn arbitrary_strings_never_panic(text in any::<String>()) {
            let _ = parse(text.as_bytes());
        }

        #[test]
        fn toml_shaped_strings_never_panic(
            text in "(font_size|[a-z_]{1,9}|[\\[\\]{}=.,\"'#\n ]|[0-9e+-]{1,6}){0,64}",
        ) {
            let _ = parse(text.as_bytes());
        }

        #[test]
        fn font_sizes_are_kept_only_in_range(size in any::<f32>()) {
            let result = parse(format!("font_size = {size:?}").as_bytes());
            if (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
                prop_assert_eq!(result.ok().map(|config| config.font_size), Some(size));
            } else {
                prop_assert!(result.is_err());
            }
        }
    }
}
