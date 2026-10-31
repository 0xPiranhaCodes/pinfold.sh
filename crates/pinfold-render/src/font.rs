//! Fonts, memory-mapped and read lazily by `swash` (ADR-0025).

use std::io;
use std::path::{Path, PathBuf};

use pinfold_sys::mapped::Mapped;
use swash::{CacheKey, FontRef};

/// JetBrains Mono, the default font (ADR-0026). See `fonts/README.md`.
const BUNDLED: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");
const BUNDLED_NAME: &str = "the bundled font";

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
}

#[derive(Debug)]
enum Data {
    Bundled,
    Mapped(Mapped),
}

/// A font face, embedded in the binary or in a memory-mapped file.
#[derive(Debug)]
pub struct Font {
    data: Data,
    offset: u32,
    key: CacheKey,
}

impl Font {
    fn parse(data: Data, name: &Path) -> Result<Self, FontError> {
        let bytes = match &data {
            Data::Bundled => BUNDLED,
            Data::Mapped(mapped) => mapped,
        };
        let face =
            FontRef::from_index(bytes, 0).ok_or_else(|| FontError::Invalid(name.to_owned()))?;
        let (offset, key) = (face.offset, face.key);
        Ok(Self { data, offset, key })
    }

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
        Self::parse(Data::Mapped(data), path)
    }

    /// Opens the font embedded in the binary.
    ///
    /// # Errors
    ///
    /// Returns an error only if the embedded file is not a font, which a test rules out.
    pub fn bundled() -> Result<Self, FontError> {
        Self::parse(Data::Bundled, Path::new(BUNDLED_NAME))
    }

    pub(crate) fn face(&self) -> FontRef<'_> {
        FontRef {
            data: match &self.data {
                Data::Bundled => BUNDLED,
                Data::Mapped(mapped) => mapped,
            },
            offset: self.offset,
            key: self.key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_font_loads_and_has_ascii() {
        let font = Font::bundled().unwrap();
        assert!(('!'..='~').all(|character| font.face().charmap().map(character) != 0));
    }

    #[test]
    fn a_font_file_opens() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts/JetBrainsMono-Regular.ttf");
        assert!(Font::open(&path).is_ok());
    }

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
