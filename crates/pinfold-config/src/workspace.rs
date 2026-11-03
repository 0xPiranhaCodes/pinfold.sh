//! A project's `.pinfold/layout.toml` file.
//!
//! The file comes from an untrusted repository. Reading and parsing are bounded, symlinks are
//! refused, and a working directory cannot escape the project through `..` or another symlink.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The largest workspace layout file accepted.
pub const MAX_BYTES: usize = 64 * 1024;
/// The most tabs in one layout. Keep this equal to `pinfold_proto::link::MAX_TABS`.
pub const MAX_TABS: usize = 32;
/// The most panes across one layout. Keep this equal to `pinfold_proto::link::MAX_PANES`.
pub const MAX_PANES: usize = 64;

/// A workspace layout, ready for a later step to open.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Tabs in their display order.
    pub tabs: Vec<Tab>,
}

/// One tab in a workspace layout.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    /// Panes in the order in which they are split into the tab.
    pub panes: Vec<Pane>,
}

/// One pane in a workspace layout.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pane {
    /// Where to split this pane from the preceding pane. The first pane may omit it.
    pub split: Option<Split>,
    /// An existing directory inside the project, canonicalized when parsed.
    pub cwd: Option<PathBuf>,
    /// A line to type into the pane after the workspace is trusted.
    pub run: Option<String>,
}

/// Where a pane is added relative to the preceding pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    /// Put the new pane to the right.
    Right,
    /// Put the new pane below.
    Down,
}

/// Why a workspace layout was refused.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be opened or read.
    #[error("cannot read the workspace layout: {0}")]
    Read(#[from] io::Error),
    /// The path names a symlink instead of the layout itself.
    #[error("the workspace layout is a symlink")]
    Symlink,
    /// A directory, FIFO or device is in the layout's place.
    #[error("the workspace layout is not a regular file")]
    NotAFile,
    /// The file is over [`MAX_BYTES`].
    #[error("the workspace layout is over the limit of {MAX_BYTES} bytes")]
    TooLarge,
    /// The file is not text.
    #[error("the workspace layout is not UTF-8")]
    NotUtf8,
    /// The TOML is invalid, or a key or value is not part of the format.
    #[error("{0}")]
    Invalid(String),
    /// The layout has no tabs or one of its tabs has no panes.
    #[error("the workspace layout must have at least one tab and one pane per tab")]
    Empty,
    /// The layout exceeds [`MAX_TABS`].
    #[error("the workspace layout has more than {MAX_TABS} tabs")]
    TooManyTabs,
    /// The layout exceeds [`MAX_PANES`].
    #[error("the workspace layout has more than {MAX_PANES} panes")]
    TooManyPanes,
    /// The project directory or a pane working directory cannot be resolved.
    #[error("a workspace directory cannot be resolved: {0}")]
    Cwd(io::Error),
    /// A pane's working directory is not a directory.
    #[error("a pane cwd is not a directory")]
    CwdNotDirectory,
    /// A pane's working directory resolves outside the project.
    #[error("a pane cwd resolves outside the project")]
    CwdOutsideProject,
}

/// Reads and parses a workspace layout without following a symlink at `path`.
///
/// # Errors
///
/// Returns an error when the path is not a regular file, is a symlink, cannot be read, exceeds
/// [`MAX_BYTES`], or its bytes do not form a valid [`Layout`].
pub fn load(path: &Path, project: &Path) -> Result<Layout, Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Symlink);
    }
    // A symlinked `.pinfold` would read a file from outside the project.
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = fs::canonicalize(parent.unwrap_or(Path::new(".")))?;
    if !parent.starts_with(fs::canonicalize(project).map_err(Error::Cwd)?) {
        return Err(Error::Symlink);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                Error::Symlink
            } else {
                Error::Read(error)
            }
        })?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::NotAFile);
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err(Error::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    parse(&bytes, project)
}

/// Parses workspace layout bytes and resolves every supplied `cwd` inside `project`.
///
/// # Errors
///
/// Returns an error for oversized or non-UTF-8 input, invalid TOML, unknown keys, exceeded tab
/// or pane limits, empty tabs, and working directories that are missing, not directories, or
/// resolve outside the project.
pub fn parse(bytes: &[u8], project: &Path) -> Result<Layout, Error> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    let mut layout: Layout = toml::from_str(text).map_err(|error| {
        // Controls, and bidi and zero-width format characters, which could reorder the message.
        let clean = |c: char| {
            (c.is_control() && c != '\n')
                || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        };
        Error::Invalid(error.to_string().replace(clean, "\u{fffd}"))
    })?;
    if layout.tabs.is_empty() || layout.tabs.iter().any(|tab| tab.panes.is_empty()) {
        return Err(Error::Empty);
    }
    if layout.tabs.len() > MAX_TABS {
        return Err(Error::TooManyTabs);
    }
    if layout.tabs.iter().map(|tab| tab.panes.len()).sum::<usize>() > MAX_PANES {
        return Err(Error::TooManyPanes);
    }

    let project = fs::canonicalize(project).map_err(Error::Cwd)?;
    if !project.is_dir() {
        return Err(Error::CwdNotDirectory);
    }
    for cwd in layout
        .tabs
        .iter_mut()
        .flat_map(|tab| &mut tab.panes)
        .filter_map(|pane| pane.cwd.as_mut())
    {
        let candidate = if cwd.is_absolute() {
            cwd.clone()
        } else {
            project.join(&*cwd)
        };
        let resolved = fs::canonicalize(candidate).map_err(Error::Cwd)?;
        if !resolved.is_dir() {
            return Err(Error::CwdNotDirectory);
        }
        if !resolved.starts_with(&project) {
            return Err(Error::CwdOutsideProject);
        }
        *cwd = resolved;
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    const PANE: &[u8] = b"[[tabs]]\n[[tabs.panes]]\n";

    fn error(bytes: &[u8], project: &Path) -> Error {
        parse(bytes, project).unwrap_err()
    }

    #[test]
    fn reads_tabs_panes_and_values() {
        let project = tempdir().unwrap();
        fs::create_dir(project.path().join("app")).unwrap();
        let bytes = b"[[tabs]]\n[[tabs.panes]]\ncwd = \"app\"\nrun = \"cargo test\"\n\
            [[tabs.panes]]\nsplit = \"down\"\n";
        let layout = parse(bytes, project.path()).unwrap();
        assert_eq!(layout.tabs[0].panes.len(), 2);
        assert!(
            layout.tabs[0].panes[0]
                .cwd
                .as_deref()
                .unwrap()
                .ends_with("app")
        );
        assert_eq!(layout.tabs[0].panes[0].run.as_deref(), Some("cargo test"));
        assert_eq!(layout.tabs[0].panes[1].split, Some(Split::Down));
    }

    #[test]
    fn refuses_unknown_keys_with_a_line() {
        let bytes = b"[[tabs]]\n[[tabs.panes]]\nextra = true";
        let error = parse(bytes, Path::new(".")).unwrap_err().to_string();
        assert!(error.contains("unknown field"), "{error}");
        assert!(error.contains("line 3"), "{error}");
    }

    #[test]
    fn refuses_bad_shapes_and_limits() {
        let project = tempdir().unwrap();
        let bad_split = b"[[tabs]]\n[[tabs.panes]]\nsplit = \"left\"";
        assert!(matches!(
            error(bad_split, project.path()),
            Error::Invalid(_)
        ));
        assert!(matches!(error(b"tabs = []", project.path()), Error::Empty));
        let tabs = "[[tabs]]\n[[tabs.panes]]\n".repeat(MAX_TABS + 1);
        assert!(matches!(
            error(tabs.as_bytes(), project.path()),
            Error::TooManyTabs
        ));
        let panes = format!("[[tabs]]\n{}", "[[tabs.panes]]\n".repeat(MAX_PANES + 1));
        assert!(matches!(
            error(panes.as_bytes(), project.path()),
            Error::TooManyPanes
        ));
        assert!(matches!(
            error(&vec![b'#'; MAX_BYTES + 1], project.path()),
            Error::TooLarge
        ));
        assert!(matches!(error(b"\xff", project.path()), Error::NotUtf8));
    }

    #[test]
    fn refuses_invalid_and_escaped_cwds() {
        let root = tempdir().unwrap();
        let project = root.path().join("project");
        let outside = root.path().join("outside");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&outside).unwrap();
        let prefix = "[[tabs]]\n[[tabs.panes]]\ncwd = ";
        assert!(matches!(
            error(format!("{prefix}\"missing\"").as_bytes(), &project),
            Error::Cwd(_)
        ));
        fs::write(project.join("file"), b"").unwrap();
        assert!(matches!(
            error(format!("{prefix}\"file\"").as_bytes(), &project),
            Error::CwdNotDirectory
        ));
        assert!(matches!(
            error(format!("{prefix}\"../outside\"").as_bytes(), &project),
            Error::CwdOutsideProject
        ));
        symlink(&outside, project.join("linked")).unwrap();
        assert!(matches!(
            error(format!("{prefix}\"linked\"").as_bytes(), &project),
            Error::CwdOutsideProject
        ));
    }

    #[test]
    fn load_refuses_symlinks_special_files_and_large_files() {
        let project = tempdir().unwrap();
        let layout = project.path().join("layout.toml");
        fs::write(&layout, PANE).unwrap();
        let link = project.path().join("link.toml");
        symlink(&layout, &link).unwrap();
        assert!(matches!(load(&link, project.path()), Err(Error::Symlink)));
        fs::create_dir(project.path().join("dir")).unwrap();
        assert!(matches!(
            load(&project.path().join("dir"), project.path()),
            Err(Error::NotAFile)
        ));
        // A symlinked `.pinfold` reaching outside the project.
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("layout.toml"), PANE).unwrap();
        symlink(outside.path(), project.path().join(".pinfold")).unwrap();
        let through = project.path().join(".pinfold/layout.toml");
        assert!(matches!(
            load(&through, project.path()),
            Err(Error::Symlink)
        ));
        fs::write(&layout, vec![b'#'; MAX_BYTES + 1]).unwrap();
        assert!(matches!(
            load(&layout, project.path()),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn errors_drop_bidi_and_control_characters() {
        let project = tempdir().unwrap();
        let Err(Error::Invalid(message)) = parse(
            "[[tab]]\n\"a\u{202e}b\u{7}\" = 1\n".as_bytes(),
            project.path(),
        ) else {
            panic!("expected an invalid layout");
        };
        assert!(!message.contains(['\u{202e}', '\u{7}']), "{message}");
    }
}
