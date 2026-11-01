//! Fonts, memory-mapped and read lazily by `swash` (ADR-0025).

use std::io;
use std::path::{Path, PathBuf};

use pinfold_sys::mapped::Mapped;
use swash::{CacheKey, FontRef};

/// JetBrains Mono, the default font (ADR-0026). See `fonts/README.md`.
const BUNDLED: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");
const BUNDLED_NAME: &str = "the bundled font";

// shortcut: fixed paths, so a font installed elsewhere is missed, use config paths from #21.
/// Japanese, then Simplified Chinese, then Korean.
#[cfg(target_os = "macos")]
const FALLBACKS: &[&str] = &[
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
];
/// Noto CJK where Debian, Arch, Fedora and Alpine install it, then WenQuanYi.
#[cfg(not(target_os = "macos"))]
const FALLBACKS: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-sans-cjk-vf-fonts/NotoSansCJK-VF.ttc",
    "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
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
        // Swash subtracts one from this count, which overflows if `head`, `maxp` or `hhea`
        // leave it 0, and debug builds panic.
        let hhea = face.table(u32::from_be_bytes(*b"hhea"));
        let long_metrics = hhea.and_then(|hhea| hhea.get(34..36));
        if face.metrics(&[]).glyph_count == 0 || long_metrics.is_none_or(|count| count == [0, 0]) {
            return Err(FontError::Invalid(name.to_owned()));
        }
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

    /// Returns the glyph for `character`, or 0 if this font lacks it.
    pub(crate) fn glyph(&self, character: char) -> u16 {
        self.face().charmap().map(character)
    }
}

/// A font, and fallbacks that cost nothing until it lacks a character.
#[derive(Debug)]
pub struct Fonts {
    primary: Font,
    /// Paths not tried yet. Each is tried once, so a bad file cannot cost a retry per glyph.
    untried: &'static [&'static str],
    opened: Vec<Font>,
}

impl From<Font> for Fonts {
    fn from(primary: Font) -> Self {
        Self::with_fallbacks(primary, FALLBACKS)
    }
}

impl Fonts {
    pub(crate) fn with_fallbacks(primary: Font, untried: &'static [&'static str]) -> Self {
        Self {
            primary,
            untried,
            opened: Vec::new(),
        }
    }

    pub(crate) fn primary(&self) -> &Font {
        &self.primary
    }

    /// How many fallbacks have not been tried yet.
    #[cfg(test)]
    pub(crate) fn untried(&self) -> usize {
        self.untried.len()
    }

    /// Finds a fallback with `character`, opening more of them until one has it. A file
    /// that is missing or corrupt is skipped.
    pub(crate) fn fallback(&mut self, character: char) -> Option<(&Font, u16)> {
        let has = |font: &Font| font.glyph(character) != 0;
        let mut found = self.opened.iter().position(has);
        while found.is_none() {
            let (path, rest) = self.untried.split_first()?;
            self.untried = rest;
            if let Ok(font) = Font::open(Path::new(path)) {
                found = has(&font).then_some(self.opened.len());
                self.opened.push(font);
            }
        }
        let font = self.opened.get(found?)?;
        Some((font, font.glyph(character)))
    }
}

/// Font files for tests, so they need no system font.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::{env, fs, io, process};

    use super::{BUNDLED, Font};

    /// A character the bundled font lacks and `kanji_font` has.
    pub const KANJI: char = '日';

    fn table_records(font: &[u8]) -> impl Iterator<Item = usize> + use<> {
        let count = u16::from_be_bytes([font[4], font[5]]);
        (0..usize::from(count)).map(|table| 12 + table * 16)
    }

    fn word(font: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([font[at], font[at + 1], font[at + 2], font[at + 3]])
    }

    fn set_word(font: &mut [u8], at: usize, value: u32) {
        font[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// Where a table starts.
    pub fn table(font: &[u8], tag: &[u8; 4]) -> Option<usize> {
        let record = table_records(font).find(|&at| &font[at..at + 4] == tag)?;
        Some(word(font, record + 8) as usize)
    }

    /// The bundled font with a new `cmap` that maps `KANJI` and the 255 characters after it,
    /// to the glyph for 'H' and those after it.
    pub fn kanji_font() -> Option<Vec<u8>> {
        let mut font = BUNDLED.to_vec();
        let record = table_records(&font).find(|&at| &font[at..at + 4] == b"cmap")?;
        let glyph = Font::bundled().ok()?.glyph('H');
        let code = KANJI as u32;
        // One Windows full-Unicode encoding at offset 12, then a format 12 table of 28 bytes
        // with one group.
        let last = code + 255;
        let cmap = [
            1,
            3 << 16 | 10,
            12,
            12 << 16,
            28,
            0,
            1,
            code,
            last,
            u32::from(glyph),
        ];
        let offset = font.len() as u32;
        font.extend(cmap.iter().flat_map(|value| value.to_be_bytes()));
        set_word(&mut font, record + 8, offset);
        set_word(&mut font, record + 12, 40);
        Some(font)
    }

    /// Wraps a font in a collection of one, as a `.ttc` file holds it.
    pub fn collection(mut font: Vec<u8>) -> Vec<u8> {
        const HEADER: u32 = 16;
        for record in table_records(&font) {
            let offset = word(&font, record + 8);
            set_word(&mut font, record + 8, offset + HEADER);
        }
        let mut file = b"ttcf".to_vec();
        for value in [0x0001_0000, 1, HEADER] {
            file.extend(u32::to_be_bytes(value));
        }
        file.append(&mut font);
        file
    }

    /// A temporary font file, removed on drop. `list` is what `Fonts::with_fallbacks` takes.
    pub struct File {
        path: PathBuf,
        pub list: &'static [&'static str],
    }

    impl File {
        pub fn new(bytes: &[u8]) -> io::Result<Self> {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let number = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("pinfold-font-{}-{number}", process::id());
            let path = env::temp_dir().join(name);
            fs::write(&path, bytes)?;
            let text: &'static str = path.to_string_lossy().into_owned().leak();
            Ok(Self {
                path,
                list: vec![text].leak(),
            })
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{File, KANJI, collection, kanji_font, table};
    use super::*;

    fn bundled() -> Font {
        Font::bundled().unwrap()
    }

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

    #[test]
    fn a_font_with_no_horizontal_metrics_is_rejected() {
        let mut font = kanji_font().unwrap();
        let hhea = table(&font, b"hhea").unwrap();
        font[hhea + 34..hhea + 36].fill(0);
        let file = File::new(&font).unwrap();
        let result = Font::open(Path::new(file.list[0]));
        assert!(matches!(result, Err(FontError::Invalid(_))));
    }

    #[test]
    fn a_collection_loads_through_its_offset() {
        let file = File::new(&collection(kanji_font().unwrap())).unwrap();
        let mut fonts = Fonts::with_fallbacks(bundled(), file.list);
        let (font, glyph) = fonts.fallback(KANJI).unwrap();
        assert!(font.offset > 0);
        assert_ne!(glyph, 0);
    }

    #[test]
    fn bad_files_are_skipped_and_never_retried() {
        let cargo = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let mut fonts = Fonts::with_fallbacks(bundled(), vec!["/does/not/exist", cargo].leak());
        assert!(fonts.fallback(KANJI).is_none());
        assert_eq!(fonts.untried(), 0);
        assert!(fonts.opened.is_empty());
    }

    #[test]
    fn later_fallbacks_open_only_when_earlier_ones_lack_the_character() {
        let file = File::new(&kanji_font().unwrap()).unwrap();
        let list = vec![file.list[0], "/does/not/exist"].leak();
        let mut fonts = Fonts::with_fallbacks(bundled(), list);
        assert!(fonts.fallback(KANJI).is_some());
        assert_eq!(fonts.untried(), 1);
        assert!(fonts.fallback('語').is_none());
        assert_eq!(fonts.untried(), 0);
        assert!(fonts.fallback(KANJI).is_some());
    }
}
