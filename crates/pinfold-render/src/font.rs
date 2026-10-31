//! Fonts, memory-mapped and read lazily by `swash` (ADR-0025).

use std::io;
use std::path::{Path, PathBuf};

use pinfold_sys::mapped::Mapped;
use swash::{CacheKey, FontRef};

// shortcut: fixed paths with no config or fallback, replaced by font lookup in #21.
#[cfg(target_os = "macos")]
const SYSTEM_FONTS: [&str; 1] = ["/System/Library/Fonts/Menlo.ttc"];
#[cfg(target_os = "linux")]
const SYSTEM_FONTS: [&str; 4] = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu-sans-mono-fonts/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
];

/// Why a font could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum FontError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The font file.
        path: PathBuf,
        /// What went wrong.
        source: io::Error,
    },
    /// The file is not a usable font.
    #[error("{0} is not a usable font")]
    Invalid(PathBuf),
    /// None of the known system fonts exists.
    #[error("no known monospace font is installed")]
    NoneInstalled,
}

/// A font face in a memory-mapped file.
#[derive(Debug)]
pub struct Font {
    data: Mapped,
    offset: u32,
    key: CacheKey,
}

impl Font {
    /// Opens the first face in a font file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or is not a font.
    pub fn open(path: &Path) -> Result<Self, FontError> {
        let data = Mapped::open(path).map_err(|source| FontError::Read {
            path: path.to_owned(),
            source,
        })?;
        let face =
            FontRef::from_index(&data, 0).ok_or_else(|| FontError::Invalid(path.to_owned()))?;
        let (offset, key) = (face.offset, face.key);
        Ok(Self { data, offset, key })
    }

    /// Opens the first known monospace font that is installed.
    ///
    /// # Errors
    ///
    /// Returns an error if none is installed, or the one found cannot be read.
    pub fn system() -> Result<Self, FontError> {
        let path = SYSTEM_FONTS
            .iter()
            .map(Path::new)
            .find(|path| path.exists())
            .ok_or(FontError::NoneInstalled)?;
        Self::open(path)
    }

    pub(crate) fn face(&self) -> FontRef<'_> {
        FontRef {
            data: &self.data,
            offset: self.offset,
            key: self.key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_is_not_a_font_is_rejected() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(matches!(Font::open(&path), Err(FontError::Invalid(_))));
    }

    #[test]
    fn a_missing_font_is_rejected() {
        let result = Font::open(Path::new("/does/not/exist"));
        assert!(matches!(result, Err(FontError::Read { .. })));
    }
}
