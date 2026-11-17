//! The checkpoint walk of ADR-0050 (sections 1 to 3): every file and link a checkpoint covers,
//! read without following a link, streamed as `git fast-import` blobs, with the stat list the
//! next checkpoint merges against.
//!
//! The walk is one thread in path order, opening each entry from its parent's descriptor
//! through [`pinfold_sys::at`], so no path is resolved twice and no link is followed. Files to
//! read go through a queue of at most 512 records to one reader thread per importer, each with
//! a 64 KiB buffer, and the stat list is written in walk order as their answers come back, so
//! memory does not grow with the tree.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{self, AtomicUsize};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use pinfold_sys::at::{self, Kind, Stat};
use sha1::{Digest, Sha1};

use crate::gitignore::{self, Pattern};

const BUF: usize = 64 * 1024;
/// Records waiting for a reader before the walk blocks.
const PENDING: usize = 512;
/// Skipped paths listed; the rest are only counted.
pub const LISTED: usize = 1000;
/// Directories below the root that the walk enters. Each holds a descriptor while the walk is
/// inside it, and macOS starts a process with 256.
const MAX_DEPTH: usize = 64;
/// Directory descriptors open at once, the walk's own and those queued files hold; past it the
/// walk waits for readers before opening another.
const MAX_OPEN_DIRS: usize = 128;
/// A `.gitignore` larger than this is not used, nor one past this many bytes of patterns
/// held for the directories the walk is in. Either way the walk covers more, never less.
const IGNORE_FILE: u64 = 64 * 1024;
/// Entries a directory may hold to be entered; its names are held while the walk is in it.
const MAX_NAMES: usize = 1 << 16;
const IGNORE_HELD: usize = 256 * 1024;

/// What a checkpoint covers at most (ADR-0050 section 3, `[rewind]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// Files and links walked, `max_files`; directories are not counted.
    pub max_files: u64,
    /// A larger regular file is not covered, `max_file_mib`.
    pub max_file_bytes: u64,
    /// Bytes read from files whose stat changed, `max_new_mib`.
    pub max_new_bytes: u64,
    /// Whether what the tree's `.gitignore` files ignore is covered too, `include_ignored`.
    pub include_ignored: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_files: 200_000,
            max_file_bytes: 16 << 20,
            max_new_bytes: 2048 << 20,
            include_ignored: false,
        }
    }
}

/// Why a path is not covered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// An entry named `.git`, in any ASCII case, or on macOS an HFS+ alias of it.
    Git,
    /// A directory holding a `.git`: a nested repository or a submodule.
    Nested,
    /// Ignored by a `.gitignore` in the tree; an ignored directory is not entered.
    Ignored,
    /// A regular file over [`Caps::max_file_bytes`], or a directory of more than 65,536
    /// entries.
    Large,
    /// A regular file with more than one hard link.
    HardLink,
    /// A fifo, socket or device.
    Special,
    /// Changed while it was read, twice, or swapped for something else after the walk saw it.
    Changed,
    /// Could not be opened, listed or read.
    Unreadable,
    /// A directory more than 64 levels below the working directory.
    Deep,
}

/// What a walk covered and skipped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Walked {
    /// Files and links in the stat list.
    pub files: u64,
    /// Of them, files and links read because their stat changed.
    pub read: u64,
    /// Of them, files and links whose blob id came from the previous stat list.
    pub reused: u64,
    /// Bytes read.
    pub new_bytes: u64,
    /// False once a cap stopped the walk; the stat list then holds only what came before.
    pub complete: bool,
    /// Paths not covered.
    pub uncovered: u64,
    /// The first [`LISTED`] of them, nested repositories kept over the others.
    pub skipped: Vec<(Vec<u8>, Why)>,
}

/// One covered file or link, as a line of the stat list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    /// `100644`, `100755` or `120000`.
    pub(crate) mode: u32,
    /// Git's blob id.
    pub(crate) id: [u8; 20],
    size: u64,
    modified_ns: i64,
    changed_ns: i64,
    inode: u64,
    /// From the working directory, `/` between components.
    pub(crate) path: Vec<u8>,
}

impl Record {
    fn new(mode: u32, stat: &Stat, path: &[u8]) -> Self {
        Self {
            mode,
            id: [0; 20],
            size: stat.size,
            modified_ns: stat.modified_ns,
            changed_ns: stat.changed_ns,
            inode: stat.inode,
            path: path.to_vec(),
        }
    }

    fn same_stat(&self, other: &Self) -> bool {
        let stat = |r: &Self| (r.mode, r.size, r.modified_ns, r.changed_ns, r.inode);
        stat(self) == stat(other)
    }

    /// `<mode> <id> <size> <mtime> <ctime> <inode> <path>\0`, times in nanoseconds.
    pub(crate) fn write(&self, out: &mut dyn Write) -> io::Result<()> {
        write!(out, "{:o} ", self.mode)?;
        out.write_all(&hex(&self.id))?;
        let (size, modified, changed) = (self.size, self.modified_ns, self.changed_ns);
        write!(out, " {size} {modified} {changed} {} ", self.inode)?;
        out.write_all(&self.path)?;
        out.write_all(b"\0")
    }

    /// The next record, or `None` at the end or at a line that does not parse, so a previous
    /// list cut short by a crash only reuses less.
    pub(crate) fn read(input: &mut dyn BufRead, line: &mut Vec<u8>) -> Option<Self> {
        line.clear();
        input.read_until(0, line).ok()?;
        Self::parse(line)
    }

    /// The next record, or `None` at the end.
    ///
    /// # Errors
    ///
    /// Returns an error if reading fails or a line does not parse, so a commit is never built
    /// from part of a list.
    fn read_whole(input: &mut dyn BufRead, line: &mut Vec<u8>) -> io::Result<Option<Self>> {
        line.clear();
        if input.read_until(0, line)? == 0 {
            return Ok(None);
        }
        let broken = || io::Error::new(io::ErrorKind::InvalidData, "a broken stat list");
        Self::parse(line).map(Some).ok_or_else(broken)
    }

    fn parse(line: &[u8]) -> Option<Self> {
        let mut fields = line.strip_suffix(b"\0")?.splitn(7, |&c| c == b' ');
        let mut text = || std::str::from_utf8(fields.next()?).ok();
        let mode = u32::from_str_radix(text()?, 8).ok()?;
        let hex = text()?.as_bytes();
        let mut id = [0u8; 20];
        for (i, byte) in id.iter_mut().enumerate() {
            let pair = std::str::from_utf8(hex.get(i * 2..i * 2 + 2)?).ok()?;
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self {
            mode,
            id,
            size: text()?.parse().ok()?,
            modified_ns: text()?.parse().ok()?,
            changed_ns: text()?.parse().ok()?,
            inode: text()?.parse().ok()?,
            path: fields.next()?.to_vec(),
        })
    }
}

pub(crate) fn hex(id: &[u8; 20]) -> [u8; 40] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0; 40];
    for (i, byte) in id.iter().enumerate() {
        out[2 * i] = DIGITS[usize::from(byte >> 4)];
        out[2 * i + 1] = DIGITS[usize::from(byte & 15)];
    }
    out
}

/// Writes the commit of a checkpoint to `import`, a `git fast-import` started once every
/// reader's importer has finished, as the commit names their blobs. Its tree is the records of
/// `stat`, the list [`walk`] wrote. Given `parent`, the ref of the checkpoint whose list the walk
/// merged and that list, it names the parent and lists only what changed (`M`) or went (`D`);
/// otherwise every record. `message` is the commit message and `time` its Unix time.
///
/// # Errors
///
/// [`io::ErrorKind::InvalidInput`] if a ref is not under `refs/pinfold/` or holds anything but
/// `A-Z a-z 0-9 / . _ -`, [`io::ErrorKind::InvalidData`] for a broken list, otherwise a failed
/// read or write.
///
/// # Examples
///
/// ```
/// use pinfold_tools::checkpoint::commit;
///
/// let mut import = Vec::new();
/// commit(&mut import, "refs/pinfold/r1-0/0", "{}", 0, &mut &b""[..], None)?;
/// assert!(import.starts_with(b"commit refs/pinfold/r1-0/0\n"));
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn commit(
    import: &mut dyn Write,
    refname: &str,
    message: &str,
    time: u64,
    stat: &mut dyn BufRead,
    parent: Option<(&str, &mut dyn BufRead)>,
) -> io::Result<()> {
    let plain = |name: &str| {
        name.starts_with("refs/pinfold/")
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c))
    };
    if !plain(refname) || parent.as_ref().is_some_and(|(from, _)| !plain(from)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a checkpoint ref",
        ));
    }
    let committer = format!("PinFold <pinfold@localhost> {time} +0000");
    let length = message.len();
    write!(
        import,
        "commit {refname}\ncommitter {committer}\ndata {length}\n{message}\n"
    )?;
    let mut olds = None;
    if let Some((from, list)) = parent {
        writeln!(import, "from {from}")?;
        olds = Some(list);
    }
    let (mut line, mut old_line) = (Vec::new(), Vec::new());
    let mut next_old = |line: &mut Vec<u8>| match olds.as_mut() {
        Some(list) => Record::read_whole(&mut **list, line),
        None => Ok(None),
    };
    let mut old = next_old(&mut old_line)?;
    while let Some(new) = Record::read_whole(stat, &mut line)? {
        while let Some(gone) = old.take_if(|o| tree_cmp(&o.path, &new.path) == Ordering::Less) {
            change(import, &gone, false)?;
            old = next_old(&mut old_line)?;
        }
        let same = old
            .as_ref()
            .filter(|o| o.path == new.path)
            .map(|o| (o.mode, o.id) == (new.mode, new.id));
        if same.is_some() {
            old = next_old(&mut old_line)?;
        }
        if same != Some(true) {
            change(import, &new, true)?;
        }
    }
    while let Some(gone) = old {
        change(import, &gone, false)?;
        old = next_old(&mut old_line)?;
    }
    import.write_all(b"\n")
}

/// `M <mode> <id> <path>` or `D <path>`, the path C-quoted when it holds a newline, a quote or
/// a backslash, as fast-import reads it.
fn change(out: &mut dyn Write, record: &Record, modified: bool) -> io::Result<()> {
    if modified {
        write!(out, "M {:o} ", record.mode)?;
        out.write_all(&hex(&record.id))?;
        out.write_all(b" ")?;
    } else {
        out.write_all(b"D ")?;
    }
    let path = &record.path;
    if !path.iter().any(|&c| matches!(c, b'\n' | b'"' | b'\\')) {
        out.write_all(path)?;
        return out.write_all(b"\n");
    }
    let mut quoted = vec![b'"'];
    for &c in path {
        match c {
            b'\n' => quoted.extend_from_slice(b"\\n"),
            b'"' | b'\\' => quoted.extend_from_slice(&[b'\\', c]),
            _ => quoted.push(c),
        }
    }
    quoted.extend_from_slice(b"\"\n");
    out.write_all(&quoted)
}

/// The walk's order, depth first with siblings sorted by bytes: paths compared by component.
pub(crate) fn tree_cmp(a: &[u8], b: &[u8]) -> Ordering {
    a.split(|&c| c == b'/').cmp(b.split(|&c| c == b'/'))
}

/// Whether git would take `name` for `.git`: any ASCII case, and on macOS with HFS+'s
/// ignorable code points anywhere in it, as git's `is_hfs_dotgit`.
fn is_dotgit(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b".git") || (cfg!(target_os = "macos") && is_hfs_dotgit(name))
}

fn is_hfs_dotgit(name: &[u8]) -> bool {
    let Ok(name) = std::str::from_utf8(name) else {
        return false;
    };
    let ignorable = |c: &char| matches!(c, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}');
    let mut chars = name.chars().filter(|c| !ignorable(c));
    ".git"
        .chars()
        .all(|g| chars.next().is_some_and(|c| c.eq_ignore_ascii_case(&g)))
        && chars.next().is_none()
}

/// A directory descriptor, counted while open.
struct Dir {
    fd: OwnedFd,
    open: Arc<AtomicUsize>,
}

impl Drop for Dir {
    fn drop(&mut self) {
        self.open.fetch_sub(1, atomic::Ordering::Relaxed);
    }
}

/// What a reader turns into a blob.
enum Src {
    /// A file in a directory the walk opened.
    File(Arc<Dir>, OsString),
    /// A link's target text.
    Bytes(Vec<u8>),
}

enum Answer {
    /// The blob id, and the file's stat after reading it.
    Blob([u8; 20], Option<Stat>),
    Skip(Why),
    /// The importer failed; the walk stops with an error.
    Failed,
}

struct Pending {
    record: Record,
    answer: Option<Answer>,
}

/// Walks `root` and covers what [`Caps`] allows. Each blob read goes, as `git fast-import`
/// input (`blob`, `data <n>`), to one of `importers`, one reader thread each, and each covered
/// path's [`Record`] to `stat` in walk order. A record in `previous`, the last checkpoint's
/// stat list, whose stat is unchanged gives its blob id without a read. Returns the counts and
/// the importers, for the caller to finish.
///
/// # Errors
///
/// Returns an error if `root` cannot be opened as a directory (a link included), if
/// `importers` is empty, or if writing to `stat` or an importer fails.
///
/// # Examples
///
/// ```
/// use pinfold_tools::checkpoint::{walk, Caps};
///
/// let dir = std::env::temp_dir().join(format!("pinfold-walk-doc-{}", std::process::id()));
/// std::fs::create_dir_all(&dir)?;
/// std::fs::write(dir.join("a.txt"), "hello\n")?;
/// let mut stat = Vec::new();
/// let (walked, importers) = walk(&dir, &Caps::default(), None, &mut stat, vec![Vec::new()])?;
/// assert!(walked.complete && walked.files == 1);
/// assert!(importers[0].starts_with(b"blob\ndata 6\nhello\n"));
/// std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn walk<W: Write + Send>(
    root: &Path,
    caps: &Caps,
    previous: Option<&mut dyn BufRead>,
    stat: &mut dyn Write,
    importers: Vec<W>,
) -> io::Result<(Walked, Vec<W>)> {
    if importers.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no importer"));
    }
    let root = at::open_dir(root).map_err(io_error)?;
    let (jobs, queue) = mpsc::sync_channel(PENDING);
    let (answers, results) = mpsc::channel();
    let queue = Arc::new(Mutex::new(queue));
    let max_file = caps.max_file_bytes;
    thread::scope(|scope| {
        let readers: Vec<_> = importers
            .into_iter()
            .map(|out| {
                let (queue, answers) = (Arc::clone(&queue), answers.clone());
                scope.spawn(move || reader(out, &queue, &answers, max_file))
            })
            .collect();
        // Once every reader is gone, a send to the queue fails rather than waits.
        drop((queue, answers));
        let mut walk = Walk::new(caps, previous, stat, jobs, results);
        let walked = walk.run(root);
        drop(walk);
        let mut importers = Vec::new();
        let mut failed = None;
        for reader in readers {
            match reader.join() {
                Ok(Ok(out)) => importers.push(out),
                Ok(Err(error)) => failed = failed.or(Some(error)),
                Err(_) => failed = failed.or(Some(io::Error::other("a reader stopped"))),
            }
        }
        match (walked, failed) {
            (Err(error), _) | (Ok(_), Some(error)) => Err(error),
            (Ok(walked), None) => Ok((walked, importers)),
        }
    })
}

fn io_error(error: at::Error) -> io::Error {
    match error {
        at::Error::Io(error) => error,
        other => io::Error::other(other),
    }
}

struct Walk<'s, 'p> {
    caps: Caps,
    walked: Walked,
    /// Files and links walked, against [`Caps::max_files`].
    counted: u64,
    stopped: bool,
    failed: bool,
    jobs: Option<SyncSender<(u64, Src)>>,
    results: Receiver<(u64, Answer)>,
    /// Records in walk order, oldest first, until their readers answer.
    pending: VecDeque<Pending>,
    /// Sequence number of `pending`'s front, and of the next record.
    front: u64,
    next: u64,
    stat: &'s mut dyn Write,
    previous: Option<&'p mut dyn BufRead>,
    /// The previous list's record at or after `path`.
    old: Option<Record>,
    line: Vec<u8>,
    /// For each directory the walk is in that has a `.gitignore`: where paths relative to it
    /// start in `path`, and its patterns.
    ignores: Vec<(usize, Vec<Pattern>)>,
    ignore_held: usize,
    open: Arc<AtomicUsize>,
    path: Vec<u8>,
    buf: Vec<u8>,
}

impl<'s, 'p> Walk<'s, 'p> {
    fn new(
        caps: &Caps,
        mut previous: Option<&'p mut dyn BufRead>,
        stat: &'s mut dyn Write,
        jobs: SyncSender<(u64, Src)>,
        results: Receiver<(u64, Answer)>,
    ) -> Self {
        let mut line = Vec::new();
        let old = previous
            .as_mut()
            .and_then(|input| Record::read(&mut **input, &mut line));
        Self {
            caps: *caps,
            walked: Walked::default(),
            counted: 0,
            stopped: false,
            failed: false,
            jobs: Some(jobs),
            results,
            pending: VecDeque::new(),
            front: 0,
            next: 0,
            stat,
            previous,
            old,
            line,
            ignores: Vec::new(),
            ignore_held: 0,
            open: Arc::new(AtomicUsize::new(0)),
            path: Vec::new(),
            buf: vec![0; BUF],
        }
    }

    fn run(&mut self, root: OwnedFd) -> io::Result<Walked> {
        let root = self.dir_fd(root);
        self.dir(&root, 0)?;
        drop(root);
        self.jobs = None;
        while !self.pending.is_empty() && self.collect(true)? {}
        if self.failed || !self.pending.is_empty() {
            return Err(io::Error::other("an importer stopped"));
        }
        self.stat.flush()?;
        self.walked.complete = !self.stopped;
        Ok(std::mem::take(&mut self.walked))
    }

    fn dir_fd(&self, fd: OwnedFd) -> Arc<Dir> {
        self.open.fetch_add(1, atomic::Ordering::Relaxed);
        Arc::new(Dir {
            fd,
            open: Arc::clone(&self.open),
        })
    }

    /// The names in `dir`, sorted, or why it is not entered.
    fn list(dir: &Dir) -> Result<Vec<OsString>, Why> {
        let mut entries = at::read_dir_at(dir.fd.as_fd()).map_err(|_| Why::Unreadable)?;
        let mut names = Vec::new();
        // shortcut: a directory's names are held while the walk is in it, so one of more than
        // MAX_NAMES is not covered; list in pages if a tree with such a directory needs it.
        while let Some(name) = entries.next_name().map_err(|_| Why::Unreadable)? {
            if names.len() == MAX_NAMES {
                return Err(Why::Large);
            }
            names.push(name.to_owned());
        }
        names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        Ok(names)
    }

    fn dir(&mut self, dir: &Arc<Dir>, depth: usize) -> io::Result<()> {
        let names = match Self::list(dir) {
            Ok(names) => names,
            Err(why) => return self.skip(why),
        };
        if depth > 0 && names.iter().any(|name| is_dotgit(name.as_bytes())) {
            return self.skip(Why::Nested);
        }
        let level = (self.ignores.len(), self.ignore_held);
        if names.iter().any(|name| name == ".gitignore") {
            self.load_ignore(dir);
        }
        for name in &names {
            if self.stopped {
                break;
            }
            let len = self.path.len();
            if len > 0 {
                self.path.push(b'/');
            }
            self.path.extend_from_slice(name.as_bytes());
            let entry = self.entry(dir, name, depth);
            self.path.truncate(len);
            entry?;
        }
        self.ignores.truncate(level.0);
        self.ignore_held = level.1;
        Ok(())
    }

    fn load_ignore(&mut self, dir: &Dir) {
        let Ok(file) = at::open_file_at(dir.fd.as_fd(), ".gitignore".as_ref()) else {
            return;
        };
        let small = file.metadata().is_ok_and(|meta| {
            let stat = Stat::from(&meta);
            stat.kind == Kind::File && stat.size <= IGNORE_FILE
        });
        let mut text = Vec::new();
        if !small || file.take(IGNORE_FILE).read_to_end(&mut text).is_err() {
            return;
        }
        if self.ignore_held + text.len() > IGNORE_HELD {
            return;
        }
        self.ignore_held += text.len();
        let base = if self.path.is_empty() {
            0
        } else {
            self.path.len() + 1
        };
        self.ignores.push((base, gitignore::parse(&text)));
    }

    /// Whether the deepest `.gitignore` with a matching pattern ignores the current path.
    /// A `.gitignore` whose patterns run out of steps is dropped for the rest of the walk.
    fn ignored(&mut self, is_dir: bool) -> bool {
        for (base, patterns) in self.ignores.iter_mut().rev() {
            match gitignore::matches(patterns, &self.path[*base..], is_dir) {
                Ok(Some(ignored)) => return ignored,
                Ok(None) => {}
                Err(gitignore::OverBudget) => patterns.clear(),
            }
        }
        false
    }

    fn entry(&mut self, dir: &Arc<Dir>, name: &OsStr, depth: usize) -> io::Result<()> {
        if is_dotgit(name.as_bytes()) {
            return self.skip(Why::Git);
        }
        let Ok(stat) = at::stat_at(dir.fd.as_fd(), name) else {
            return self.skip(Why::Unreadable);
        };
        let env = stat.kind == Kind::File && name.as_bytes().starts_with(b".env");
        if !self.caps.include_ignored && !env && self.ignored(stat.kind == Kind::Dir) {
            return self.skip(Why::Ignored);
        }
        if matches!(stat.kind, Kind::File | Kind::Link) {
            self.counted += 1;
            if self.counted > self.caps.max_files {
                self.stopped = true;
                return Ok(());
            }
        }
        match stat.kind {
            Kind::Dir if depth + 1 > MAX_DEPTH => self.skip(Why::Deep),
            Kind::Dir => {
                while self.open.load(atomic::Ordering::Relaxed) >= MAX_OPEN_DIRS
                    && !self.pending.is_empty()
                    && self.collect(true)?
                {}
                let Ok(fd) = at::open_dir_at(dir.fd.as_fd(), name) else {
                    return self.skip(Why::Unreadable);
                };
                let child = self.dir_fd(fd);
                self.dir(&child, depth + 1)
            }
            Kind::Link => {
                let Ok(target) = at::read_link_at(dir.fd.as_fd(), name, &mut self.buf) else {
                    return self.skip(Why::Unreadable);
                };
                let target = target.to_vec();
                self.cover(0o120000, &stat, Src::Bytes(target))
            }
            Kind::File if stat.links > 1 => self.skip(Why::HardLink),
            Kind::File if stat.size > self.caps.max_file_bytes => self.skip(Why::Large),
            Kind::File => {
                let mode = if stat.mode & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                };
                self.cover(mode, &stat, Src::File(Arc::clone(dir), name.to_owned()))
            }
            Kind::Other => self.skip(Why::Special),
        }
    }

    /// The previous list's record for the current path, if it has one.
    fn previous(&mut self) -> Option<&Record> {
        loop {
            match tree_cmp(&self.old.as_ref()?.path, &self.path) {
                Ordering::Less => {
                    let input = self.previous.as_mut()?;
                    self.old = Record::read(&mut **input, &mut self.line);
                }
                Ordering::Equal => return self.old.as_ref(),
                Ordering::Greater => return None,
            }
        }
    }

    fn cover(&mut self, mode: u32, stat: &Stat, src: Src) -> io::Result<()> {
        let mut record = Record::new(mode, stat, &self.path);
        if let Some(old) = self.previous()
            && old.same_stat(&record)
        {
            record.id = old.id;
            self.walked.reused += 1;
            return self.push(record, None);
        }
        if self.walked.new_bytes + stat.size > self.caps.max_new_bytes {
            self.stopped = true;
            return Ok(());
        }
        self.walked.new_bytes += stat.size;
        self.walked.read += 1;
        self.push(record, Some(src))
    }

    /// Queues `record` in walk order, handing `src` to a reader if there is one to read.
    fn push(&mut self, record: Record, src: Option<Src>) -> io::Result<()> {
        while self.pending.len() >= PENDING {
            if !self.collect(true)? {
                return Err(io::Error::other("an importer stopped"));
            }
        }
        let answer = if src.is_none() {
            Some(Answer::Blob(record.id, None))
        } else {
            None
        };
        self.pending.push_back(Pending { record, answer });
        if let Some(src) = src {
            let jobs = self
                .jobs
                .as_ref()
                .ok_or_else(|| io::Error::other("walk ended"))?;
            jobs.send((self.next, src))
                .map_err(|_| io::Error::other("an importer stopped"))?;
        }
        self.next += 1;
        self.collect(false).map(drop)
    }

    /// Takes the readers' answers, waiting for one if `block`, and writes every answered
    /// record at the front. False once every reader is gone.
    fn collect(&mut self, block: bool) -> io::Result<bool> {
        let mut alive = true;
        if block {
            match self.results.recv() {
                Ok(answer) => self.answer(answer),
                Err(_) => alive = false,
            }
        }
        while let Ok(answer) = self.results.try_recv() {
            self.answer(answer);
        }
        while self.pending.front().is_some_and(|p| p.answer.is_some()) {
            let Some(Pending { record, answer }) = self.pending.pop_front() else {
                break;
            };
            self.front += 1;
            match answer {
                Some(Answer::Blob(..)) => {
                    record.write(self.stat)?;
                    self.walked.files += 1;
                }
                Some(Answer::Skip(why)) => self.list_skip(&record.path, why),
                Some(Answer::Failed) | None => self.failed = true,
            }
        }
        Ok(alive && !self.failed)
    }

    fn answer(&mut self, (seq, answer): (u64, Answer)) {
        let Some(pending) = (seq.checked_sub(self.front))
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.pending.get_mut(i))
        else {
            return;
        };
        if let Answer::Blob(id, after) = &answer {
            let record = &mut pending.record;
            record.id = *id;
            if let Some(after) = after {
                (
                    record.size,
                    record.modified_ns,
                    record.changed_ns,
                    record.inode,
                ) = (after.size, after.modified_ns, after.changed_ns, after.inode);
            }
        }
        pending.answer = Some(answer);
    }

    fn skip(&mut self, why: Why) -> io::Result<()> {
        let path = std::mem::take(&mut self.path);
        self.list_skip(&path, why);
        self.path = path;
        Ok(())
    }

    fn list_skip(&mut self, path: &[u8], why: Why) {
        self.walked.uncovered += 1;
        let skipped = &mut self.walked.skipped;
        if skipped.len() < LISTED {
            skipped.push((path.to_vec(), why));
        } else if why == Why::Nested
            && let Some(other) = skipped.iter().rposition(|(_, w)| *w != Why::Nested)
        {
            skipped[other] = (path.to_vec(), why);
        }
    }
}

/// A reader thread: blobs for the walk's jobs into `out` until the queue closes.
fn reader<W: Write>(
    mut out: W,
    queue: &Mutex<Receiver<(u64, Src)>>,
    answers: &Sender<(u64, Answer)>,
    max_file: u64,
) -> io::Result<W> {
    let mut buf = vec![0; BUF];
    loop {
        let job = queue
            .lock()
            .map_err(|_| io::Error::other("a reader stopped"))?
            .recv();
        let Ok((seq, src)) = job else {
            return Ok(out);
        };
        // The directory's descriptor closes here, before the answer, once no job holds it.
        let answer = match src {
            Src::Bytes(bytes) => blob(&mut out, &mut &bytes[..], bytes.len() as u64, &mut buf)
                .map(|(id, _)| Answer::Blob(id, None)),
            Src::File(dir, name) => read_file(&mut out, &dir, &name, &mut buf, max_file),
        };
        match answer {
            Ok(answer) => {
                if answers.send((seq, answer)).is_err() {
                    return Ok(out);
                }
            }
            Err(error) => {
                let _ = answers.send((seq, Answer::Failed));
                return Err(error);
            }
        }
    }
}

/// Reads `name` through its own descriptor, checked with `fstat` before and after; one that
/// changed while read is read once more, then left uncovered. A link or anything else swapped
/// in since the walk saw it is never followed or read.
fn read_file(
    out: &mut dyn Write,
    dir: &Dir,
    name: &OsStr,
    buf: &mut [u8],
    max_file: u64,
) -> io::Result<Answer> {
    for _ in 0..2 {
        let mut file = match at::open_file_at(dir.fd.as_fd(), name) {
            Ok(file) => file,
            Err(at::Error::Link) => return Ok(Answer::Skip(Why::Changed)),
            Err(_) => return Ok(Answer::Skip(Why::Unreadable)),
        };
        let Ok(before) = file.metadata().map(|meta| Stat::from(&meta)) else {
            return Ok(Answer::Skip(Why::Unreadable));
        };
        if before.kind != Kind::File || before.links != 1 || before.size > max_file {
            return Ok(Answer::Skip(Why::Changed));
        }
        let (id, whole) = blob(out, &mut file, before.size, buf)?;
        let after = file.metadata().map(|meta| Stat::from(&meta));
        if let Ok(after) = after
            && whole
            && (after.size, after.modified_ns, after.changed_ns, after.inode)
                == (
                    before.size,
                    before.modified_ns,
                    before.changed_ns,
                    before.inode,
                )
        {
            return Ok(Answer::Blob(id, Some(after)));
        }
    }
    Ok(Answer::Skip(Why::Changed))
}

/// Streams exactly `size` bytes of `input` to `out` as a fast-import blob and returns git's id
/// for them, and whether `input` gave them all. A short or failed read is padded with zeros;
/// its blob is then never referenced.
///
/// # Errors
///
/// Only a failed write to `out`.
fn blob(
    out: &mut dyn Write,
    input: &mut dyn Read,
    size: u64,
    buf: &mut [u8],
) -> io::Result<([u8; 20], bool)> {
    let mut hash = Sha1::new();
    hash.update(format!("blob {size}\0"));
    write!(out, "blob\ndata {size}\n")?;
    let (mut left, mut whole) = (size, true);
    while left > 0 {
        let want = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        let got = match input.read(&mut buf[..want]) {
            Ok(got) if got > 0 && whole => got,
            _ => {
                whole = false;
                buf[..want].fill(0);
                want
            }
        };
        hash.update(&buf[..got]);
        out.write_all(&buf[..got])?;
        left -= got as u64;
    }
    out.write_all(b"\n")?;
    Ok((hash.finalize().into(), whole))
}

#[cfg(test)]
mod tests;
