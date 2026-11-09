//! The file manager behind `pinfold files` (ADR-0045 sections 2 and 4).
//!
//! Three columns: the parent directory, the current one and a preview of the selected entry.
//! Names and contents are untrusted, so all of them reach the screen through
//! [`Screen::put`], which escapes them. Enter opens a file in the reader, executables
//! included; nothing is ever run.

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::keys::Key;
use crate::md::{self, Reader, colour};
use crate::screen::{Color, Screen, Style, escape};
use crate::{Flow, Tool};

/// Entries listed per directory; the rest are counted.
const MAX_ENTRIES: usize = 10_000;
/// Bytes of a file the preview reads.
const MAX_PREVIEW: usize = 64 * 1024;
/// Bytes of a binary file the hex preview shows.
const MAX_HEX: usize = 4 * 1024;

const KEYS: [&str; 7] = [
    "j k  ↓ ↑        down or up",
    "h l  ← →        the parent, or into a directory",
    "Enter           into a directory, or a file in the reader",
    "/               filter by name; Enter keeps it, Esc clears it",
    "g G  Home End   the first or the last",
    "?               these keys",
    "q               quit",
];

struct Entry {
    name: OsString,
    dir: bool,
}

/// A directory's entries, directories first and each by name, and how many past the cap.
#[derive(Default)]
struct Listing {
    entries: Vec<Entry>,
    more: usize,
}

fn list(dir: &Path) -> Result<Listing, String> {
    let read = std::fs::read_dir(dir).map_err(|error| format!("cannot read: {error}"))?;
    let mut listing = Listing::default();
    for entry in read.flatten() {
        if listing.entries.len() == MAX_ENTRIES {
            listing.more += 1;
            continue;
        }
        // A link is a directory if it points at one; `is_dir` stats it and opens nothing.
        let dir = entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()));
        let name = entry.file_name();
        listing.entries.push(Entry { name, dir });
    }
    listing
        .entries
        .sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    Ok(listing)
}

enum Preview {
    Text(String),
    Hex(Vec<u8>),
    /// Directory stats, or why there is no preview.
    Note(Vec<String>),
}

fn preview(path: &Path, dir: bool) -> Preview {
    if dir {
        return Preview::Note(match list(path) {
            Ok(listing) => {
                let dirs = listing.entries.iter().filter(|entry| entry.dir).count();
                let files = listing.entries.len() - dirs + listing.more;
                vec![
                    format!("directories  {dirs}"),
                    format!("files        {files}"),
                ]
            }
            Err(problem) => vec![problem],
        });
    }
    match md::read_at_most(path, MAX_PREVIEW) {
        Ok((bytes, _)) if text(&bytes) => {
            Preview::Text(String::from_utf8_lossy(&bytes).replace('\t', "    "))
        }
        Ok((mut bytes, _)) => {
            bytes.truncate(MAX_HEX);
            Preview::Hex(bytes)
        }
        Err(error) => Preview::Note(vec![format!("No preview: {error}")]),
    }
}

/// Text has no NUL and is UTF-8, but for a character the read cut short.
fn text(bytes: &[u8]) -> bool {
    !bytes.contains(&0)
        && std::str::from_utf8(bytes).map_or_else(|e| e.error_len().is_none(), |_| true)
}

/// `name` escaped and cut to `width` characters.
fn clip(name: &[u8], width: usize) -> String {
    escape(name).chars().take(width).collect()
}

/// The file manager: Miller columns over `dir`, and the reader once a file is opened.
pub(crate) struct Files {
    /// The directory as it was given, which stands for `start` in the title.
    title: Vec<u8>,
    start: PathBuf,
    dir: PathBuf,
    listing: Result<Listing, String>,
    parent: Listing,
    /// Indices into the listing of the entries that match `filter`.
    shown: Vec<usize>,
    selected: usize,
    filter: String,
    typing: bool,
    /// The selected entry's preview, made when it is first drawn.
    preview: Option<Preview>,
    pager: Option<Reader>,
    problem: Option<String>,
    help: bool,
}

impl Files {
    /// A file manager titled `title` for the directory `dir`.
    pub(crate) fn new(title: Vec<u8>, dir: PathBuf) -> Self {
        let mut files = Self {
            title,
            start: dir.clone(),
            dir: PathBuf::new(),
            listing: Ok(Listing::default()),
            parent: Listing::default(),
            shown: Vec::new(),
            selected: 0,
            filter: String::new(),
            typing: false,
            preview: None,
            pager: None,
            problem: None,
            help: false,
        };
        files.go(dir, None);
        files
    }

    /// Lists `dir`, with the entry named `select` selected if there is one.
    fn go(&mut self, dir: PathBuf, select: Option<&OsStr>) {
        self.listing = list(&dir);
        self.parent = dir
            .parent()
            .and_then(|up| list(up).ok())
            .unwrap_or_default();
        self.dir = dir;
        self.filter.clear();
        self.refilter();
        let entries = self.entries();
        let selected = self
            .shown
            .iter()
            .position(|&at| Some(entries[at].name.as_os_str()) == select);
        self.selected = selected.unwrap_or(0);
    }

    fn entries(&self) -> &[Entry] {
        self.listing
            .as_ref()
            .map_or(&[], |listing| &listing.entries)
    }

    fn refilter(&mut self) {
        let filter = self.filter.to_lowercase();
        self.shown = (0..self.entries().len())
            .filter(|&at| {
                let name = escape(self.entries()[at].name.as_bytes());
                name.to_lowercase().contains(&filter)
            })
            .collect();
        self.selected = 0;
        self.preview = None;
    }

    fn entry(&self) -> Option<&Entry> {
        let at = *self.shown.get(self.selected)?;
        self.entries().get(at)
    }

    fn select(&mut self, at: usize) {
        self.selected = at.min(self.shown.len().saturating_sub(1));
        self.preview = None;
    }

    /// Into the selected directory, or the selected file in the reader.
    fn open(&mut self) {
        let Some(entry) = self.entry() else { return };
        let (path, dir, name) = (
            self.dir.join(&entry.name),
            entry.dir,
            escape(entry.name.as_bytes()),
        );
        if dir {
            self.go(path, None);
            return;
        }
        // Read, never run: an executable shows its bytes like any other file.
        match md::read(&path) {
            Ok((bytes, whole)) => {
                let title = path.as_os_str().as_bytes().to_vec();
                let mut reader = Reader::new(title, bytes, whole);
                reader.path.clone_from(&path);
                let markdown = path.extension().is_some_and(|extension| extension == "md");
                self.pager = Some(if markdown { reader } else { reader.plain() });
            }
            Err(error) => {
                self.problem = Some(format!("cannot open {name}: {error}"));
            }
        }
    }

    fn title(&self) -> Vec<u8> {
        let mut title = b"files ".to_vec();
        match self.dir.strip_prefix(&self.start) {
            Ok(rest) => {
                title.extend_from_slice(&self.title);
                if !rest.as_os_str().is_empty() {
                    if !title.ends_with(b"/") {
                        title.push(b'/');
                    }
                    title.extend_from_slice(rest.as_os_str().as_bytes());
                }
            }
            Err(_) => title.extend_from_slice(self.dir.as_os_str().as_bytes()),
        }
        title
    }
}

/// Draws `entries` in a column `width` wide from `x`, scrolled to show `selected`.
fn column<'a>(
    screen: &mut Screen,
    (x, width, height): (u16, usize, u16),
    entries: impl Iterator<Item = &'a Entry>,
    selected: Option<(usize, Color)>,
) {
    let top = selected.map_or(0, |(at, _)| (at + 1).saturating_sub(usize::from(height)));
    for (row, (at, entry)) in (1..=height).zip(entries.enumerate().skip(top)) {
        let mut name = entry.name.as_bytes().to_vec();
        if entry.dir {
            name.push(b'/');
        }
        let fg = if entry.dir {
            Color::Blue
        } else {
            Color::Default
        };
        let mut style = Style {
            bold: entry.dir,
            ..colour(fg)
        };
        if let Some((_, bg)) = selected.filter(|&(selected, _)| selected == at) {
            style.fg = Color::Black;
            style.bg = bg;
        }
        let name = clip(&name, width.saturating_sub(1));
        screen.put(
            row,
            x,
            format!("{name:<0$}", width.saturating_sub(1)),
            style,
        );
    }
}

impl Tool for Files {
    fn tick(&mut self) {
        if let Some(pager) = &mut self.pager {
            pager.tick();
        }
    }

    fn draw(&mut self, screen: &mut Screen) {
        if let Some(pager) = &mut self.pager {
            pager.draw(screen);
            return;
        }
        let (rows, columns) = screen.size();
        let (dim, red) = (colour(Color::BrightBlack), colour(Color::Red));
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        screen.put(0, 0, self.title(), bold);
        let place = format!("{}/{}  ? keys", self.selected + 1, self.shown.len());
        let at = columns.saturating_sub(u16::try_from(place.len()).unwrap_or(u16::MAX));
        screen.put(0, at, place, dim);
        if self.help {
            for (row, keys) in (2..rows).zip(KEYS) {
                screen.put(row, 2, keys, Style::default());
            }
            return;
        }
        let height = rows.saturating_sub(2);
        let left = columns / 5;
        let middle = columns * 3 / 10;
        let right = left + middle + 2;
        let width = usize::from(columns.saturating_sub(right));
        let here = self.dir.file_name();
        let up = self
            .parent
            .entries
            .iter()
            .position(|entry| Some(entry.name.as_os_str()) == here);
        let parent = (0, usize::from(left), height);
        column(
            screen,
            parent,
            self.parent.entries.iter(),
            up.map(|at| (at, Color::BrightBlack)),
        );
        match &self.listing {
            Ok(listing) => {
                let shown = self.shown.iter().map(|&at| &listing.entries[at]);
                let current = (left + 1, usize::from(middle), height);
                column(screen, current, shown, Some((self.selected, Color::Blue)));
            }
            Err(problem) => {
                screen.put(
                    1,
                    left + 1,
                    clip(problem.as_bytes(), usize::from(middle)),
                    red,
                );
            }
        }
        if self.preview.is_none() {
            self.preview = self
                .entry()
                .map(|entry| preview(&self.dir.join(&entry.name), entry.dir));
        }
        let lines: Vec<(String, Style)> = match &self.preview {
            None => Vec::new(),
            Some(Preview::Text(text)) => (text.lines())
                .take(usize::from(height))
                .map(|line| (clip(line.as_bytes(), width), Style::default()))
                .collect(),
            Some(Preview::Hex(bytes)) => {
                let per = (width.saturating_sub(6) / 4).clamp(1, 16);
                (bytes.chunks(per).enumerate())
                    .take(usize::from(height))
                    .map(|(at, chunk)| (hex(at * per, per, chunk), Style::default()))
                    .collect()
            }
            Some(Preview::Note(lines)) => lines.iter().map(|line| (line.clone(), dim)).collect(),
        };
        for (row, (line, style)) in (1..=height).zip(lines) {
            screen.put(row, right, line, style);
        }
        let status = rows.saturating_sub(1);
        if self.typing || !self.filter.is_empty() {
            screen.put(status, 0, format!("/{}", self.filter), Style::default());
        } else if let Some(problem) = &self.problem {
            let at = screen.put(status, 0, "✕ error", red);
            screen.put(status, at + 1, problem, Style::default());
        } else if let Ok(Listing {
            more: more @ 1.., ..
        }) = &self.listing
        {
            let note = format!("{more} more entries are not listed.");
            screen.put(status, 0, note, colour(Color::Yellow));
        }
    }

    fn key(&mut self, key: Key) -> Flow {
        if let Some(pager) = &mut self.pager {
            if pager.key(key) == Flow::Quit {
                self.pager = None;
            }
            return Flow::Continue;
        }
        self.problem = None;
        if self.help {
            self.help = false;
            return if key == Key::Char('q') {
                Flow::Quit
            } else {
                Flow::Continue
            };
        }
        if self.typing {
            match key {
                Key::Char(character) => self.filter.push(character),
                Key::Backspace => {
                    self.filter.pop();
                }
                Key::Enter => self.typing = false,
                Key::Esc => {
                    self.typing = false;
                    self.filter.clear();
                }
                _ => return Flow::Continue,
            }
            self.refilter();
            return Flow::Continue;
        }
        match key {
            Key::Char('q') => return Flow::Quit,
            Key::Char('?') => self.help = true,
            Key::Char('/') => self.typing = true,
            Key::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            Key::Char('j') | Key::Down => self.select(self.selected + 1),
            Key::Char('k') | Key::Up => self.select(self.selected.saturating_sub(1)),
            Key::Char('g') | Key::Home => self.select(0),
            Key::Char('G') | Key::End => self.select(usize::MAX),
            Key::Char('h') | Key::Left => {
                if let Some(up) = self.dir.parent() {
                    let (up, here) = (up.to_path_buf(), self.dir.file_name().map(OsStr::to_owned));
                    self.go(up, here.as_deref());
                }
            }
            Key::Char('l') | Key::Right | Key::Enter => self.open(),
            _ => {}
        }
        Flow::Continue
    }
}

/// A row of the hex preview: the offset, each byte in hex, and the printable ones as text.
fn hex(offset: usize, per: usize, bytes: &[u8]) -> String {
    let mut line = format!("{offset:04x}  ");
    for byte in bytes {
        let _ = write!(line, "{byte:02x} ");
    }
    // The text lines up on a short last row.
    line.push_str(&" ".repeat(3 * per - 3 * bytes.len()));
    line.extend(bytes.iter().map(|&byte| {
        if byte.is_ascii_graphic() || byte == b' ' {
            char::from(byte)
        } else {
            '.'
        }
    }));
    line
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A fresh `root/work` in the temp directory, so the parent column holds only `work`.
    fn work(test: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("pinfold-files-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        std::fs::create_dir_all(work.join("docs/old")).unwrap();
        std::fs::write(work.join("docs/a.md"), "# A\n").unwrap();
        std::fs::write(
            work.join("notes.txt"),
            "First line\n\tindented\nred \x1b[31mtext\n",
        )
        .unwrap();
        let binary =
            b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x03\x00>\x00hello\xff";
        std::fs::write(work.join("tool.bin"), binary).unwrap();
        (root, work)
    }

    /// The screen at 80×24 after `keys`, as rows and style runs.
    fn shown(work: &Path, keys: &[Key]) -> String {
        let mut files = Files::new(b"work".to_vec(), work.to_path_buf());
        for &key in keys {
            files.key(key);
        }
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        screen.runs()
    }

    #[test]
    fn columns() {
        let (root, work) = work("columns");
        insta::assert_snapshot!(shown(&work, &[]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_text_preview() {
        let (root, work) = work("text");
        insta::assert_snapshot!(shown(&work, &[Key::Char('j')]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_hex_preview() {
        let (root, work) = work("hex");
        insta::assert_snapshot!(shown(&work, &[Key::Char('G')]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_escaped_name() {
        let (root, work) = work("escaped");
        // APFS refuses invalid UTF-8 in a name, so `\xNN` is left to `escape`'s own tests.
        // Inside a directory with a bidi control and BEL in its name, so the title shows it whole.
        let dir = work.join(OsStr::from_bytes(b"\xe2\x80\xaecod\x07"));
        std::fs::create_dir(&dir).unwrap();
        let name = OsStr::from_bytes(b"a\x1b]0;x\x07\xc2\x85b\x7f");
        std::fs::write(dir.join(name), "").unwrap();
        insta::assert_snapshot!(shown(&work, &[Key::Char('j'), Key::Enter]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_on_an_executable_opens_it_in_the_reader_and_never_runs_it() {
        let (root, work) = work("executable");
        let marker = root.join("ran");
        let script = work.join("run.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut files = Files::new(b"work".to_vec(), work);
        for key in [
            Key::Char('/'),
            Key::Char('r'),
            Key::Char('u'),
            Key::Enter,
            Key::Enter,
        ] {
            assert_eq!(files.key(key), Flow::Continue);
        }
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        assert_eq!(screen.row(1).trim_end(), "#!/bin/sh");
        assert!(screen.row(2).starts_with("touch "), "{}", screen.row(2));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!marker.exists(), "the script ran");
        assert_eq!(
            files.key(Key::Char('q')),
            Flow::Continue,
            "q leaves the reader"
        );
        assert_eq!(files.key(Key::Char('q')), Flow::Quit);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_fifo_is_neither_previewed_nor_opened() {
        let (root, work) = work("fifo");
        let fifo = work.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let mut files = Files::new(b"work".to_vec(), work);
        files.key(Key::Char('j'));
        files.key(Key::Char('j'));
        let mut screen = Screen::new(24, 80);
        files.draw(&mut screen);
        assert!(
            screen.row(1).contains("No preview: not a regular file"),
            "{}",
            screen.row(1)
        );
        files.key(Key::Enter);
        screen.clear();
        files.draw(&mut screen);
        assert!(
            screen.row(23).contains("not a regular file"),
            "{}",
            screen.row(23)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn h_goes_up_with_the_directory_it_left_selected() {
        let (root, work) = work("up");
        let mut files = Files::new(b"work".to_vec(), work.clone());
        files.key(Key::Enter);
        assert_eq!(files.dir, work.join("docs"));
        files.key(Key::Char('h'));
        assert_eq!((files.dir.as_path(), files.selected), (work.as_path(), 0));
        files.key(Key::Char('h'));
        assert_eq!(
            files.entry().map(|entry| entry.name.as_os_str()),
            Some(OsStr::new("work"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
