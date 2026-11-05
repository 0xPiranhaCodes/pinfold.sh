//! Fonts, memory-mapped and read lazily by `swash` (ADR-0025).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

use pinfold_sys::mapped::Mapped;
use swash::{CacheKey, FontRef};

use crate::glyphs::{can_draw_text, contained};

/// JetBrains Mono, the default font (ADR-0026). See `fonts/README.md`.
const BUNDLED: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");
const BUNDLED_NAME: &str = "the bundled font";

/// Japanese, then Simplified Chinese, then Korean.
#[cfg(target_os = "macos")]
const FALLBACKS: &[&str] = &[
    "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
];

/// Standard installation directories. No font database or native rasterizer is needed.
fn font_directories() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        if cfg!(target_os = "macos") {
            paths.push(home.join("Library/Fonts"));
        } else {
            paths.push(home.join(".fonts"));
            let data = env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"));
            paths.push(data.join("fonts"));
        }
    }
    if cfg!(target_os = "macos") {
        paths.extend(["/Library/Fonts", "/System/Library/Fonts"].map(PathBuf::from));
    } else {
        paths.extend(["/usr/local/share/fonts", "/usr/share/fonts"].map(PathBuf::from));
    }
    paths
}

/// Enumerate once, only after the usual fallbacks miss. Do not follow directory symlinks,
/// open special files, or allow an unbounded directory tree to stall a frame.
fn installed_fonts(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut pending: Vec<_> = roots.into_iter().map(|path| (path, 0)).collect();
    let mut files = Vec::new();
    let mut remaining = 4096;
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if remaining == 0 {
                break;
            }
            remaining -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < 8 {
                pending.push((path, depth + 1));
            } else if path.extension().is_some_and(|ext| {
                ["ttf", "otf", "ttc", "otc"]
                    .iter()
                    .any(|suffix| ext.eq_ignore_ascii_case(suffix))
            }) && path.is_file()
            {
                files.push(path);
            }
        }
        if remaining == 0 {
            break;
        }
    }
    // Icon fonts first: avoid paging through every text family for a private-use symbol.
    files.sort_by_cached_key(|path| {
        let name = path.to_string_lossy().to_ascii_lowercase();
        (!(name.contains("nerd") || name.contains("symbol")), name)
    });
    files.dedup();
    files.reverse(); // pop() visits the preferred font first.
    files
}
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

    /// Opens the font at `path`, the user's choice, or the bundled font without one. The file
    /// is untrusted, so one that is unreadable, damaged or cannot draw text is reported and
    /// replaced by the bundled font.
    ///
    /// # Errors
    ///
    /// Returns an error only if the embedded file is not a font, which a test rules out.
    pub fn chosen(path: Option<&Path>) -> Result<Self, FontError> {
        match path.map(Self::open_checked) {
            Some(Ok(font)) => Ok(font),
            Some(Err(error)) => {
                // shortcut: only the log hears this, so no one sees it, draw it in the chrome.
                let _ = writeln!(io::stderr(), "pinfold: using the bundled font: {error}");
                Self::bundled()
            }
            None => Self::bundled(),
        }
    }

    fn open_checked(path: &Path) -> Result<Self, FontError> {
        let invalid = || FontError::Invalid(path.to_owned());
        let metadata = fs::metadata(path).map_err(|source| FontError::Read {
            path: path.to_owned(),
            source,
        })?;
        // Opening a FIFO would hang startup.
        if !metadata.is_file() {
            return Err(invalid());
        }
        contained(|| Some(Self::open(path).map(|font| can_draw_text(&font).then_some(font))))
            .ok_or_else(invalid)?
            .and_then(|font| font.ok_or_else(invalid))
    }

    /// Whether this is the font embedded in the binary.
    #[cfg(test)]
    pub(crate) fn is_bundled(&self) -> bool {
        matches!(self.data, Data::Bundled)
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

    fn release_pages(&self) {
        if let Data::Mapped(mapped) = &self.data {
            mapped.release_pages();
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
    directories: Vec<PathBuf>,
    installed: Vec<PathBuf>,
    /// Paths not tried yet. Each is tried once, so a bad file cannot cost a retry per glyph.
    untried: &'static [&'static str],
    opened: Vec<Font>,
    /// Whether a glyph was drawn since pages were last released.
    pub(crate) touched: bool,
    /// Whether the bundled font is among `opened` yet.
    bundled: bool,
}

impl From<Font> for Fonts {
    fn from(primary: Font) -> Self {
        Self {
            directories: font_directories(),
            ..Self::with_fallbacks(primary, FALLBACKS)
        }
    }
}

impl Fonts {
    pub(crate) fn with_fallbacks(primary: Font, untried: &'static [&'static str]) -> Self {
        Self {
            primary,
            directories: Vec::new(),
            installed: Vec::new(),
            untried,
            opened: Vec::new(),
            touched: false,
            bundled: false,
        }
    }

    /// Unmaps font pages once a frame's new glyphs are drawn. A CJK font is 20 MB, drawing
    /// one screen touches over half of it, and those pages count against the window.
    pub(crate) fn release_pages(&mut self) {
        if std::mem::take(&mut self.touched) {
            self.primary.release_pages();
            self.opened.iter().for_each(Font::release_pages);
        }
    }

    pub(crate) fn primary(&self) -> &Font {
        &self.primary
    }

    /// How many fallbacks are open.
    #[cfg(test)]
    pub(crate) fn opened_count(&self) -> usize {
        self.opened.len()
    }

    /// How many fallbacks have not been tried yet.
    #[cfg(test)]
    pub(crate) fn untried(&self) -> usize {
        self.untried.len()
    }

    /// Finds a fallback with `character`, opening more of them until one has it. The bundled
    /// font comes first, so a chosen font without `⌘` still draws it the same way. A file that
    /// is missing or corrupt is skipped.
    pub(crate) fn fallback(&mut self, character: char) -> Option<(&Font, u16)> {
        if !std::mem::replace(&mut self.bundled, true) {
            self.opened.extend(Font::bundled());
        }
        let has = |font: &Font| contained(|| Some(font.glyph(character) != 0)).unwrap_or(false);
        let mut found = self.opened.iter().position(has);
        while found.is_none() {
            let path = if let Some((path, rest)) = self.untried.split_first() {
                self.untried = rest;
                PathBuf::from(path)
            } else {
                if !self.directories.is_empty() {
                    self.installed = installed_fonts(std::mem::take(&mut self.directories));
                }
                self.installed.pop()?
            };
            if let Some(font) = contained(|| Font::open(&path).ok()) {
                found = has(&font).then_some(self.opened.len());
                font.release_pages();
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

    /// The bundled font as a file's bytes, which `Font::chosen` accepts.
    pub fn plain_font() -> Vec<u8> {
        BUNDLED.to_vec()
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
    use super::fixture::{File, KANJI, collection, kanji_font, plain_font, table};
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
        let font = Font::open(&path).unwrap();
        assert!(!font.is_bundled());
        assert_eq!(font.glyph('H'), bundled().glyph('H'));
        assert_ne!(font.glyph('H'), 0);
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
    fn no_chosen_font_is_the_bundled_font() {
        assert!(Font::chosen(None).unwrap().is_bundled());
    }

    #[test]
    fn a_chosen_font_is_used() {
        let file = File::new(&collection(plain_font())).unwrap();
        let font = Font::chosen(Some(Path::new(file.list[0]))).unwrap();
        assert!(!font.is_bundled());
    }

    /// A real collection, on machines that have it.
    #[test]
    fn a_system_font_can_be_chosen_if_installed() {
        let path = Path::new("/System/Library/Fonts/Menlo.ttc");
        if path.exists() {
            assert!(!Font::chosen(Some(path)).unwrap().is_bundled());
        }
    }

    #[test]
    fn a_bad_chosen_font_falls_back_to_the_bundled_font() {
        let font = plain_font();
        let mut no_metrics = font.clone();
        let hhea = table(&font, b"hhea").unwrap();
        no_metrics[hhea + 34..hhea + 36].fill(0);
        let cut = table(&font, b"glyf").unwrap() + 64;
        let no_ascii = kanji_font().unwrap();
        let bad = [
            &b"not a font"[..],
            &font[..12],
            &font[..cut],
            &no_metrics,
            &no_ascii,
        ];
        for bytes in bad {
            let file = File::new(bytes).unwrap();
            assert!(
                Font::chosen(Some(Path::new(file.list[0])))
                    .unwrap()
                    .is_bundled()
            );
        }
        for path in ["/does/not/exist", "/", "/dev/zero"] {
            let chosen = Font::chosen(Some(Path::new(path))).unwrap();
            assert!(chosen.is_bundled(), "{path}");
        }
    }

    #[test]
    fn a_fifo_as_the_chosen_font_does_not_hang() {
        let file = File::new(b"").unwrap();
        fs::remove_file(file.list[0]).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(file.list[0])
            .status();
        assert!(made.unwrap().success());
        let chosen = Font::chosen(Some(Path::new(file.list[0]))).unwrap();
        assert!(chosen.is_bundled());
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
        assert_eq!(fonts.opened_count(), 1, "only the bundled font");
    }

    #[test]
    fn installed_icons_are_found_without_replacing_the_primary_font() {
        let mut bytes = kanji_font().unwrap();
        let cmap = table(&bytes, b"cmap").unwrap();
        // Give the fixture the private-use range used by Nerd Font icons.
        bytes[cmap + 28..cmap + 32].copy_from_slice(&0xe600_u32.to_be_bytes());
        bytes[cmap + 32..cmap + 36].copy_from_slice(&0xe6ff_u32.to_be_bytes());
        let file = File::new(&bytes).unwrap();
        let dir = Path::new(file.list[0]).with_extension("fonts");
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("nested/icons.otf"), bytes).unwrap();
        fs::write(dir.join("broken.ttf"), b"not a font").unwrap();
        let mut fonts = Fonts::with_fallbacks(bundled(), &[]);
        fonts.directories = vec![dir.clone()];
        let ascii = fonts.primary().glyph('A');
        assert!(fonts.fallback('\u{e606}').is_some());
        assert_eq!(fonts.primary().glyph('A'), ascii);
        assert!(fonts.primary().is_bundled());
        fs::remove_dir_all(dir).unwrap();
        // Resolved faces remain usable without rescanning files.
        assert!(fonts.fallback('\u{e606}').is_some());
        assert!(fonts.fallback('\u{10ffff}').is_none());
        let mut cache = crate::glyphs::GlyphCache::new(fonts, 16.0, crate::glyphs::CACHE_LIMIT);
        let icon = cache.get('\u{e606}', 0).unwrap().mask.clone();
        assert!(!icon.is_empty());
        assert_ne!(icon, cache.get('\u{10ffff}', 0).unwrap().mask);
    }

    #[test]
    fn discovery_skips_directory_loops_and_special_files() {
        use std::os::unix::fs::symlink;
        let file = File::new(&plain_font()).unwrap();
        let dir = Path::new(file.list[0]).with_extension("scan");
        fs::create_dir(&dir).unwrap();
        symlink(&dir, dir.join("loop")).unwrap();
        symlink("/dev/zero", dir.join("device.ttf")).unwrap();
        symlink(file.list[0], dir.join("linked.TTF")).unwrap();
        assert_eq!(
            installed_fonts(vec![dir.clone()]),
            vec![dir.join("linked.TTF")]
        );
        fs::remove_dir_all(dir).unwrap();
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
