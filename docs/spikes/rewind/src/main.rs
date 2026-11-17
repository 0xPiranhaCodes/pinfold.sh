//! ADR-0050 spike. `spike-rewind <dir> <store> <n>` walks `<dir>` without following links and
//! feeds `git fast-import` a commit at `refs/pinfold/spike/<n>` in the bare `<store>`. Caps come
//! from `REWIND_MAX_FILES`, `REWIND_MAX_FILE_MIB`, `REWIND_MAX_NEW_MIB` and
//! `REWIND_INCLUDE_IGNORED`, defaulting to ADR-0050's values. `REWIND_READERS` (4) reader
//! threads each feed their own `git fast-import`, run with `REWIND_FAST_IMPORT_ARGS`
//! (`--depth=0`) in a store whose zlib level is `REWIND_COMPRESSION` (1). Prints one JSON line.
use sha1::{Digest, Sha1};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const BUF: usize = 64 * 1024;
const LISTED: usize = 1000;
/// Records waiting for a reader before the walk blocks: bounds memory whatever the tree's size.
const PENDING: usize = 512;

struct Rec {
    mode: u32,
    id: [u8; 20],
    size: u64,
    mtime: i64,
    ctime: i64,
    ino: u64,
    path: Vec<u8>,
}

struct Pat {
    glob: Vec<u8>,
    neg: bool,
    dir_only: bool,
    anchored: bool,
}

#[derive(Default)]
struct Counts {
    files: u64,
    entries: u64,
    reused: u64,
    read: u64,
    new_bytes: u64,
    uncovered: u64,
    ignored: u64,
    nested: u64,
    incomplete: bool,
}

/// What a reader turns into a blob: a file opened from a duplicated directory descriptor, or a
/// symbolic link's target text.
enum Src {
    File(RawFd, CString),
    Bytes(Vec<u8>),
}

/// A reader's answer for job `seq`: the blob id and the file's stat after reading, or `None`
/// when it could not be read unchanged.
type Done = (u64, Option<([u8; 20], Option<libc::stat>)>);

struct Pending {
    rec: Rec,
    done: bool,
    ok: bool,
}

struct Walk {
    max_files: u64,
    max_file: u64,
    max_new: u64,
    include_ignored: bool,
    c: Counts,
    jobs: Option<SyncSender<(u64, Src)>>,
    results: Receiver<Done>,
    /// Records in walk order, oldest first, until their readers answer.
    pending: VecDeque<Pending>,
    front: u64,
    next: u64,
    stat: BufWriter<File>,
    old: Option<BufReader<File>>,
    cur: Option<Rec>,
    ign: Vec<(usize, Vec<Pat>)>,
    listed: Vec<Vec<u8>>,
    path: Vec<u8>,
    buf: Vec<u8>,
}

fn cstr(b: &[u8]) -> CString {
    CString::new(b).unwrap_or_default()
}

fn fstatat(fd: RawFd, name: &CString) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0)
        .then_some(st)
}

fn fstat(fd: RawFd) -> Option<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(st)
}

fn times(st: &libc::stat) -> (i64, i64) {
    (
        st.st_mtime * 1_000_000_000 + st.st_mtime_nsec,
        st.st_ctime * 1_000_000_000 + st.st_ctime_nsec,
    )
}

/// Sorted names in a directory, `.` and `..` left out.
fn list(fd: RawFd) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    let dir = unsafe { libc::fdopendir(libc::dup(fd)) };
    if dir.is_null() {
        return names;
    }
    loop {
        let e = unsafe { libc::readdir(dir) };
        if e.is_null() {
            break;
        }
        let n = unsafe { std::ffi::CStr::from_ptr((*e).d_name.as_ptr()) }.to_bytes();
        if n != b"." && n != b".." {
            names.push(n.to_vec());
        }
    }
    unsafe { libc::closedir(dir) };
    names.sort();
    names
}

fn is_dotgit(n: &[u8]) -> bool {
    // shortcut: ASCII case only; slice 2 adds git's is_hfs_dotgit code points on macOS.
    n.eq_ignore_ascii_case(b".git")
}

/// Order of a depth-first walk with sorted siblings: compare path components.
fn tree_cmp(a: &[u8], b: &[u8]) -> Ordering {
    a.split(|&c| c == b'/').cmp(b.split(|&c| c == b'/'))
}

fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = &p[2..];
            (rest.first() == Some(&b'/') && glob(&rest[1..], s))
                || (0..=s.len()).any(|i| glob(rest, &s[i..]))
        }
        Some(b'*') => (0..=s.len())
            .take_while(|&i| i == 0 || s[i - 1] != b'/')
            .any(|i| glob(&p[1..], &s[i..])),
        Some(b'?') => s.first().is_some_and(|&c| c != b'/') && glob(&p[1..], &s[1..]),
        Some(b'[') => {
            let Some(&c) = s.first() else { return false };
            let neg = matches!(p.get(1), Some(b'!' | b'^'));
            let start = if neg { 2 } else { 1 };
            let (mut i, mut hit) = (start, false);
            while i < p.len() && (p[i] != b']' || i == start) {
                if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
                    hit |= (p[i]..=p[i + 2]).contains(&c);
                    i += 3;
                } else {
                    hit |= p[i] == c;
                    i += 1;
                }
            }
            if i >= p.len() {
                return c == b'[' && glob(&p[1..], &s[1..]);
            }
            hit != neg && c != b'/' && glob(&p[i + 1..], &s[1..])
        }
        Some(b'\\') if p.len() > 1 => s.first() == Some(&p[1]) && glob(&p[2..], &s[1..]),
        Some(&c) => s.first() == Some(&c) && glob(&p[1..], &s[1..]),
    }
}

fn pattern(line: &[u8]) -> Option<Pat> {
    let mut l = line.strip_suffix(b"\r").unwrap_or(line).trim_ascii_end();
    if l.is_empty() || l[0] == b'#' {
        return None;
    }
    let neg = l[0] == b'!';
    if neg {
        l = &l[1..];
    } else if l.starts_with(b"\\!") || l.starts_with(b"\\#") {
        l = &l[1..];
    }
    let dir_only = l.ends_with(b"/");
    let l = l.strip_suffix(b"/").unwrap_or(l);
    let anchored = l.contains(&b'/');
    let l = l.strip_prefix(b"/").unwrap_or(l);
    Some(Pat {
        glob: l.to_vec(),
        neg,
        dir_only,
        anchored,
    })
}

fn parse(r: &[u8]) -> Option<Rec> {
    let mut f = r.strip_suffix(b"\0")?.splitn(7, |&c| c == b' ');
    let mut num = || std::str::from_utf8(f.next()?).ok();
    let mode = u32::from_str_radix(num()?, 8).ok()?;
    let hex = num()?;
    let mut id = [0u8; 20];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    let (size, mtime, ctime, ino) = (
        num()?.parse().ok()?,
        num()?.parse().ok()?,
        num()?.parse().ok()?,
        num()?.parse().ok()?,
    );
    Some(Rec {
        mode,
        id,
        size,
        mtime,
        ctime,
        ino,
        path: f.next()?.to_vec(),
    })
}

fn hex(id: &[u8; 20]) -> [u8; 40] {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut o = [0; 40];
    for (i, b) in id.iter().enumerate() {
        (o[2 * i], o[2 * i + 1]) = (H[(b >> 4) as usize], H[(b & 15) as usize]);
    }
    o
}

/// A path as fast-import reads it: C-quoted when it needs to be.
fn quoted(p: &[u8]) -> Vec<u8> {
    if !p.iter().any(|&c| c == b'\n' || c == b'"' || c == b'\\') {
        return p.to_vec();
    }
    let mut q = vec![b'"'];
    for &c in p {
        match c {
            b'\n' => q.extend_from_slice(b"\\n"),
            b'"' | b'\\' => q.extend_from_slice(&[b'\\', c]),
            _ => q.push(c),
        }
    }
    q.push(b'"');
    q
}

fn json(s: &[u8]) -> String {
    let mut o = String::from("\"");
    for c in String::from_utf8_lossy(s).chars() {
        match c {
            '"' | '\\' => o.extend(['\\', c]),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o + "\""
}

impl Walk {
    fn skip(&mut self) {
        let p = if self.listed.len() < LISTED {
            self.path.clone()
        } else {
            Vec::new()
        };
        self.skip_path(p);
    }

    fn skip_path(&mut self, p: Vec<u8>) {
        self.c.uncovered += 1;
        if self.listed.len() < LISTED {
            self.listed.push(p);
        }
    }

    /// The previous checkpoint's record for the current path, if it has one.
    fn old(&mut self) -> Option<&Rec> {
        loop {
            let ord = tree_cmp(&self.cur.as_ref()?.path, &self.path);
            match ord {
                Ordering::Less => {
                    let mut line = Vec::new();
                    let r = self.old.as_mut()?;
                    self.cur = (r.read_until(0, &mut line).unwrap_or(0) > 0)
                        .then(|| parse(&line))
                        .flatten();
                }
                Ordering::Equal => return self.cur.as_ref(),
                Ordering::Greater => return None,
            }
        }
    }

    fn ignored(&self, is_dir: bool) -> bool {
        for (base, pats) in self.ign.iter().rev() {
            let rel = &self.path[*base..];
            let name = rel.rsplit(|&c| c == b'/').next().unwrap_or(rel);
            for p in pats.iter().rev() {
                if (!p.dir_only || is_dir) && glob(&p.glob, if p.anchored { rel } else { name }) {
                    return !p.neg;
                }
            }
        }
        false
    }

    fn load_ignore(&mut self, fd: RawFd) {
        let f = unsafe {
            libc::openat(
                fd,
                c".gitignore".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if f < 0 {
            return;
        }
        let mut file = unsafe { File::from_raw_fd(f) };
        let mut text = Vec::new();
        if fstat(f)
            .is_some_and(|st| st.st_mode & libc::S_IFMT == libc::S_IFREG && st.st_size <= 1 << 20)
        {
            let _ = (&mut file).take(1 << 20).read_to_end(&mut text);
        }
        let base = if self.path.is_empty() {
            0
        } else {
            self.path.len() + 1
        };
        self.ign.push((
            base,
            text.split(|&c| c == b'\n').filter_map(pattern).collect(),
        ));
    }

    fn dir(&mut self, fd: RawFd, root: bool) {
        let names = list(fd);
        if !root && names.iter().any(|n| is_dotgit(n)) {
            self.c.nested += 1;
            return self.skip();
        }
        let depth = self.ign.len();
        if names.iter().any(|n| n == b".gitignore") {
            self.load_ignore(fd);
        }
        for name in &names {
            if self.c.incomplete {
                break;
            }
            let len = self.path.len();
            if len > 0 {
                self.path.push(b'/');
            }
            self.path.extend_from_slice(name);
            self.entry(fd, name);
            self.path.truncate(len);
        }
        self.ign.truncate(depth);
    }

    fn entry(&mut self, fd: RawFd, name: &[u8]) {
        let cname = cstr(name);
        let Some(st) = (!is_dotgit(name)).then(|| fstatat(fd, &cname)).flatten() else {
            return self.skip();
        };
        let kind = st.st_mode & libc::S_IFMT;
        let env = kind == libc::S_IFREG && name.starts_with(b".env");
        if !self.include_ignored && !env && self.ignored(kind == libc::S_IFDIR) {
            self.c.ignored += 1;
            return self.skip();
        }
        self.c.entries += 1;
        if self.c.entries > self.max_files {
            self.c.incomplete = true;
            return;
        }
        match kind {
            libc::S_IFDIR => {
                let d = unsafe {
                    libc::openat(
                        fd,
                        cname.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if d < 0 {
                    return self.skip();
                }
                self.dir(d, false);
                unsafe { libc::close(d) };
            }
            libc::S_IFLNK => {
                let n = unsafe {
                    libc::readlinkat(fd, cname.as_ptr(), self.buf.as_mut_ptr().cast(), BUF)
                };
                if n < 0 {
                    return self.skip();
                }
                let target = self.buf[..n as usize].to_vec();
                let rec = self.rec(0o120000, &st);
                self.push(rec, Some(Src::Bytes(target)));
            }
            libc::S_IFREG if st.st_nlink == 1 && (st.st_size as u64) <= self.max_file => {
                self.file(fd, cname, &st)
            }
            _ => self.skip(),
        }
    }

    fn file(&mut self, fd: RawFd, cname: CString, st: &libc::stat) {
        let mode = if st.st_mode & 0o111 != 0 {
            0o100755
        } else {
            0o100644
        };
        let (mtime, ctime) = times(st);
        if let Some(o) = self.old()
            && (o.mode, o.size, o.mtime, o.ctime, o.ino)
                == (mode, st.st_size as u64, mtime, ctime, st.st_ino as u64)
        {
            let id = o.id;
            self.c.reused += 1;
            let rec = Rec {
                id,
                ..self.rec(mode, st)
            };
            return self.push(rec, None);
        }
        if self.c.new_bytes + st.st_size as u64 > self.max_new {
            self.c.incomplete = true;
            return;
        }
        self.c.new_bytes += st.st_size as u64;
        self.c.read += 1;
        let rec = self.rec(mode, st);
        self.push(rec, Some(Src::File(unsafe { libc::dup(fd) }, cname)));
    }

    fn rec(&self, mode: u32, st: &libc::stat) -> Rec {
        let (mtime, ctime) = times(st);
        Rec {
            mode,
            id: [0; 20],
            size: st.st_size as u64,
            mtime,
            ctime,
            ino: st.st_ino as u64,
            path: self.path.clone(),
        }
    }

    /// Queues a record in walk order, handing its content to a reader when it has a `src`.
    fn push(&mut self, rec: Rec, src: Option<Src>) {
        while self.pending.len() >= PENDING {
            self.collect(true);
        }
        self.pending.push_back(Pending {
            rec,
            done: src.is_none(),
            ok: true,
        });
        if let Some(src) = src {
            self.jobs
                .as_ref()
                .expect("jobs open")
                .send((self.next, src))
                .expect("readers");
        }
        self.next += 1;
        self.collect(false);
    }

    /// Takes readers' answers and writes every finished record at the front. False once all
    /// readers are gone.
    fn collect(&mut self, block: bool) -> bool {
        let mut alive = true;
        if block {
            match self.results.recv() {
                Ok(d) => self.apply(d),
                Err(_) => alive = false,
            }
        }
        while let Ok(d) = self.results.try_recv() {
            self.apply(d);
        }
        while self.pending.front().is_some_and(|p| p.done) {
            let p = self.pending.pop_front().expect("front");
            self.front += 1;
            if p.ok {
                self.record(&p.rec);
            } else {
                self.skip_path(p.rec.path);
            }
        }
        alive
    }

    fn apply(&mut self, (seq, r): Done) {
        let p = &mut self.pending[(seq - self.front) as usize];
        p.done = true;
        match r {
            Some((id, st)) => {
                p.rec.id = id;
                if let Some(st) = st {
                    (p.rec.size, (p.rec.mtime, p.rec.ctime), p.rec.ino) =
                        (st.st_size as u64, times(&st), st.st_ino as u64);
                }
            }
            None => p.ok = false,
        }
    }

    fn record(&mut self, r: &Rec) {
        write!(self.stat, "{:o} ", r.mode).expect("stat");
        self.stat.write_all(&hex(&r.id)).expect("stat");
        write!(self.stat, " {} {} {} {} ", r.size, r.mtime, r.ctime, r.ino).expect("stat");
        self.stat.write_all(&r.path).expect("stat");
        self.stat.write_all(b"\0").expect("stat");
        self.c.files += 1;
    }
}

/// Streams exactly `size` bytes to fast-import as a blob, zero-padded if the file shrank (its
/// stat then differs, so the blob is never referenced), and returns git's id for it.
fn blob(git: &mut BufWriter<ChildStdin>, r: &mut dyn Read, size: u64, buf: &mut [u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(format!("blob {size}\0"));
    write!(git, "blob\ndata {size}\n").expect("git stdin");
    let mut left = size;
    while left > 0 {
        let k = left.min(BUF as u64) as usize;
        let n = match r.read(&mut buf[..k]) {
            Ok(n) if n > 0 => n,
            _ => {
                buf[..k].fill(0);
                k
            }
        };
        h.update(&buf[..n]);
        git.write_all(&buf[..n]).expect("git stdin");
        left -= n as u64;
    }
    git.write_all(b"\n").expect("git stdin");
    h.finalize().into()
}

/// Reads a regular file through its own descriptor, checked with `fstat` before and after; a
/// file that changed while read is read once more, then left uncovered.
fn read_file(
    git: &mut BufWriter<ChildStdin>,
    dir: RawFd,
    name: &CString,
    buf: &mut [u8],
) -> Option<([u8; 20], Option<libc::stat>)> {
    for _ in 0..2 {
        let f = unsafe {
            libc::openat(
                dir,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if f < 0 {
            return None;
        }
        let mut file = unsafe { File::from_raw_fd(f) };
        let before =
            fstat(f).filter(|b| b.st_mode & libc::S_IFMT == libc::S_IFREG && b.st_nlink == 1)?;
        let id = blob(git, &mut file, before.st_size as u64, buf);
        if let Some(after) = fstat(f)
            && (after.st_size, times(&after), after.st_ino)
                == (before.st_size, times(&before), before.st_ino)
        {
            return Some((id, Some(after)));
        }
    }
    None
}

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn git(store: &PathBuf) -> Command {
    let mut c = Command::new("git");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_DIR", store)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .current_dir(store);
    c
}

fn fast_import(store: &PathBuf) -> (Child, BufWriter<ChildStdin>) {
    let mut child = git(store)
        .args(["fast-import", "--quiet", "--date-format=raw"])
        .args(
            std::env::var("REWIND_FAST_IMPORT_ARGS")
                .unwrap_or_else(|_| "--depth=0".into())
                .split_whitespace(),
        )
        .stdin(Stdio::piped())
        .spawn()
        .expect("git fast-import");
    let mut w = BufWriter::with_capacity(BUF, child.stdin.take().expect("stdin"));
    w.write_all(b"feature done\n").expect("git stdin");
    (child, w)
}

fn finish((mut child, mut w): (Child, BufWriter<ChildStdin>)) {
    w.write_all(b"done\n").expect("git stdin");
    drop(w);
    assert!(
        child.wait().expect("git").success(),
        "git fast-import failed"
    );
}

/// A reader thread with its own `git fast-import`, taking jobs until the walk closes them.
fn reader(
    store: &PathBuf,
    jobs: Arc<Mutex<Receiver<(u64, Src)>>>,
    done: Sender<Done>,
) -> JoinHandle<()> {
    let mut gi = fast_import(store);
    std::thread::spawn(move || {
        let mut buf = vec![0; BUF];
        while let Some((seq, src)) = jobs.lock().ok().and_then(|r| r.recv().ok()) {
            let r = match src {
                Src::Bytes(b) => {
                    Some((blob(&mut gi.1, &mut &b[..], b.len() as u64, &mut buf), None))
                }
                Src::File(dir, name) => {
                    let r = read_file(&mut gi.1, dir, &name, &mut buf);
                    unsafe { libc::close(dir) };
                    r
                }
            };
            if done.send((seq, r)).is_err() {
                break;
            }
        }
        finish(gi);
    })
}

fn maxrss(who: libc::c_int) -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(who, &mut ru) };
    // ru_maxrss is bytes on macOS and KiB on Linux.
    if cfg!(target_os = "macos") {
        ru.ru_maxrss as u64
    } else {
        ru.ru_maxrss as u64 * 1024
    }
}

fn cpu_ms(who: libc::c_int) -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(who, &mut ru) };
    let ms = |t: libc::timeval| t.tv_sec as u64 * 1000 + t.tv_usec as u64 / 1000;
    ms(ru.ru_utime) + ms(ru.ru_stime)
}

#[cfg(target_os = "macos")]
fn footprint() -> u64 {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let p = (&mut ri as *mut libc::rusage_info_v4).cast();
    unsafe { libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, p) };
    ri.ri_lifetime_max_phys_footprint
}

#[cfg(not(target_os = "macos"))]
fn footprint() -> u64 {
    0
}

/// `gen <dir>`: ADR-0050's generated corpus. 100,000 small text files, 20 files of 20 MB (over
/// the 16 MiB cap) and an ignored `build/` of 200,000 files, plus an ignored `.env.local`.
fn generate(dir: &std::path::Path) {
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    const WORDS: [&str; 16] = [
        "fn", "let", "mut", "self", "match", "Some", "None", "return", "pub", "struct", "impl",
        "use", "if", "else", "for", "while",
    ];
    let write = |p: PathBuf, b: &[u8]| {
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(p, b).expect("write");
    };
    for i in 0..100_000 {
        let mut t = String::new();
        let len = 512 + rnd() % 7680;
        while (t.len() as u64) < len {
            t.push_str(WORDS[(rnd() % 16) as usize]);
            t.push(if rnd() % 8 == 0 { '\n' } else { ' ' });
        }
        write(
            dir.join(format!("src/d{:03}/f{i:06}.rs", i / 1000)),
            t.as_bytes(),
        );
    }
    let mut big = vec![0u8; 20 << 20];
    for i in 0..20 {
        big.iter_mut().for_each(|b| *b = rnd() as u8);
        write(dir.join(format!("big/b{i:02}.bin")), &big);
    }
    for i in 0..200_000 {
        write(
            dir.join(format!("build/d{:03}/o{i:06}", i / 1000)),
            format!("object {i}\n").as_bytes(),
        );
    }
    write(dir.join(".gitignore"), b"build/\n.env*\n");
    write(dir.join(".env.local"), b"TOKEN=not-a-real-secret\n");
}

/// `time <cmd> [args]`: runs a command, then prints its wall time and peak RSS as JSON.
fn time(cmd: &[OsString]) {
    let t0 = Instant::now();
    let ok = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::null())
        .status()
        .expect("spawn");
    assert!(ok.success(), "command failed");
    println!(
        "{{\"ms\":{},\"rss\":{}}}",
        t0.elapsed().as_millis(),
        maxrss(libc::RUSAGE_CHILDREN)
    );
}

/// Current and lifetime-peak memory of `pid`: `phys_footprint` on macOS, RSS and `VmHWM` on
/// Linux. `None` once it has exited.
#[cfg(target_os = "macos")]
fn mem(pid: libc::pid_t) -> Option<(u64, u64)> {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let p = (&mut ri as *mut libc::rusage_info_v4).cast();
    (unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, p) } == 0)
        .then_some((ri.ri_phys_footprint, ri.ri_lifetime_max_phys_footprint))
}

#[cfg(not(target_os = "macos"))]
fn mem(pid: libc::pid_t) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = |k: &str| -> Option<u64> {
        let l = s.lines().find(|l| l.starts_with(k))?;
        l[k.len()..]
            .trim()
            .trim_end_matches(" kB")
            .trim()
            .parse()
            .ok()
    };
    Some((kb("VmRSS:")? * 1024, kb("VmHWM:")? * 1024))
}

#[cfg(target_os = "macos")]
fn group(pgid: libc::pid_t) -> Vec<libc::pid_t> {
    let mut pids = vec![0 as libc::pid_t; 256];
    let size = (pids.len() * size_of::<libc::pid_t>()) as libc::c_int;
    let n = unsafe { libc::proc_listpgrppids(pgid, pids.as_mut_ptr().cast(), size) };
    pids.truncate(n.max(0) as usize);
    pids.retain(|&p| p > 0);
    pids
}

#[cfg(not(target_os = "macos"))]
fn group(pgid: libc::pid_t) -> Vec<libc::pid_t> {
    let mut pids = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        // Field 5 of /proc/<pid>/stat, after the parenthesised command name, is the pgrp.
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let rest = stat.rsplit_once(')').map_or("", |r| r.1);
        if rest.split_whitespace().nth(2).and_then(|g| g.parse().ok()) == Some(pgid) {
            pids.push(pid);
        }
    }
    pids
}

/// `peaks <cmd> [args]`: runs a take (possibly through `taskpolicy` or `nice`) in its own
/// process group, samples every other process in the group every 5 ms, and prints the take's
/// JSON line with `git_sum` (the largest sampled sum of their current memory), `git_peaks` (the
/// largest sum, over the processes alive at one sample, of each one's own lifetime peak so far)
/// and `git_procs` (most at once).
fn peaks(cmd: &[OsString]) {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(&cmd[0])
        .args(&cmd[1..])
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let take = child.id() as libc::pid_t;
    let (mut sum, mut peaks, mut procs) = (0, 0, 0);
    loop {
        let done = child.try_wait().expect("wait").is_some();
        let pids: Vec<_> = group(take).into_iter().filter(|&p| p != take).collect();
        let (mut now, mut now_peaks) = (0, 0);
        for p in &pids {
            if let Some((cur, max)) = mem(*p) {
                now += cur;
                now_peaks += max;
            }
        }
        sum = sum.max(now);
        peaks = peaks.max(now_peaks);
        procs = procs.max(pids.len());
        if done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("stdout");
    let out = out.trim().strip_suffix('}').expect("take JSON");
    println!("{out},\"git_sum\":{sum},\"git_peaks\":{peaks},\"git_procs\":{procs}}}");
}

fn main() {
    let t0 = Instant::now();
    let a: Vec<OsString> = std::env::args_os().collect();
    match a.get(1).and_then(|s| s.to_str()) {
        Some("gen") if a.len() == 3 => return generate(std::path::Path::new(&a[2])),
        Some("time") if a.len() > 2 => return time(&a[2..]),
        Some("peaks") if a.len() > 2 => return peaks(&a[2..]),
        _ => {}
    }
    assert!(
        a.len() == 4,
        "usage: spike-rewind <dir> <store> <n> | gen <dir> | time <cmd> [args] | peaks <cmd> [args]"
    );
    let store = std::path::absolute(&a[2]).expect("store");
    let n: u64 = a[3].to_str().and_then(|s| s.parse().ok()).expect("n");
    if !store.join("HEAD").exists() {
        std::fs::create_dir_all(&store).expect("store");
        std::fs::set_permissions(&store, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .expect("store");
        let ok = git(&store)
            .args(["init", "--bare", "--quiet", "--template="])
            .status()
            .expect("git init");
        assert!(ok.success());
        // The store's own config, written at creation: zlib level only, never a command.
        let level = std::env::var("REWIND_COMPRESSION").unwrap_or_else(|_| "1".into());
        let ok = git(&store)
            .args(["config", "core.compression", &level])
            .status()
            .expect("git config");
        assert!(ok.success());
        std::fs::create_dir_all(store.join("stat")).expect("stat dir");
    }
    let store = store.canonicalize().expect("store");
    let root = unsafe {
        libc::open(
            cstr(a[1].as_bytes()).as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    assert!(root >= 0, "cannot open the directory");

    let (jobs, rx) = sync_channel(PENDING);
    let (done, results) = channel();
    let rx = Arc::new(Mutex::new(rx));
    let readers: Vec<_> = (0..env_u64("REWIND_READERS", 4).max(1))
        .map(|_| reader(&store, rx.clone(), done.clone()))
        .collect();
    drop(done);
    let stat_path = store.join(format!("stat/spike-{n}"));
    let old = n
        .checked_sub(1)
        .and_then(|p| File::open(store.join(format!("stat/spike-{p}"))).ok());
    let mut w = Walk {
        max_files: env_u64("REWIND_MAX_FILES", 100_000),
        max_file: env_u64("REWIND_MAX_FILE_MIB", 16) << 20,
        max_new: env_u64("REWIND_MAX_NEW_MIB", 256) << 20,
        include_ignored: std::env::var_os("REWIND_INCLUDE_IGNORED").is_some(),
        c: Counts::default(),
        jobs: Some(jobs),
        results,
        pending: VecDeque::new(),
        front: 0,
        next: 0,
        stat: BufWriter::with_capacity(BUF, File::create(&stat_path).expect("stat")),
        old: old.map(|f| BufReader::with_capacity(BUF, f)),
        cur: Some(Rec {
            mode: 0,
            id: [0; 20],
            size: 0,
            mtime: 0,
            ctime: 0,
            ino: 0,
            path: Vec::new(),
        }),
        ign: Vec::new(),
        listed: Vec::new(),
        path: Vec::new(),
        buf: vec![0; BUF],
    };
    w.dir(root, true);
    w.jobs = None;
    while !w.pending.is_empty() && w.collect(true) {}
    assert!(w.pending.is_empty(), "a reader stopped early");
    w.stat.flush().expect("stat");
    // The commit names blobs from every reader's pack, so it is written once they are all in.
    for r in readers {
        r.join().expect("reader");
    }

    let walk_ms = t0.elapsed().as_millis();
    let c = std::mem::take(&mut w.c);
    let paths: Vec<String> = w.listed.iter().map(|p| json(p)).collect();
    let msg = format!(
        "{{\"run\":\"spike\",\"n\":{n},\"reason\":\"launch\",\"files\":{},\"new_bytes\":{},\"complete\":{},\"not_covered\":[{}],\"more\":{}}}",
        c.files,
        c.new_bytes,
        !c.incomplete,
        paths.join(","),
        c.uncovered > LISTED as u64
    );
    drop(w);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut gi = fast_import(&store);
    write!(gi.1, "commit refs/pinfold/spike/{n}\ncommitter PinFold <pinfold@localhost> {now} +0000\ndata {}\n{msg}\n", msg.len()).expect("git stdin");
    // On top of the previous checkpoint, only what changed: merge the two stat lists in walk order.
    let prev = n
        .checked_sub(1)
        .map(|p| (p, store.join(format!("stat/spike-{p}"))))
        .filter(|(_, f)| f.exists());
    if let Some((p, _)) = &prev {
        writeln!(gi.1, "from refs/pinfold/spike/{p}").expect("git stdin");
    }
    let next = |r: &mut BufReader<File>| {
        let mut line = Vec::new();
        (r.read_until(0, &mut line).unwrap_or(0) > 0)
            .then(|| parse(&line).expect("own stat record"))
    };
    let mut olds = prev.map(|(_, f)| BufReader::with_capacity(BUF, File::open(f).expect("stat")));
    let mut old = olds.as_mut().and_then(next);
    let mut recs = BufReader::with_capacity(BUF, File::open(&stat_path).expect("stat"));
    let delete = |git: &mut BufWriter<ChildStdin>, p: &[u8]| {
        git.write_all(b"D ")
            .and_then(|_| git.write_all(&quoted(p)))
            .and_then(|_| git.write_all(b"\n"))
            .expect("git stdin")
    };
    while let Some(r) = next(&mut recs) {
        while let Some(o) = old
            .as_ref()
            .filter(|o| tree_cmp(&o.path, &r.path) == Ordering::Less)
        {
            delete(&mut gi.1, &o.path);
            old = olds.as_mut().and_then(next);
        }
        let same = old
            .as_ref()
            .filter(|o| o.path == r.path)
            .map(|o| (o.mode, o.id) == (r.mode, r.id));
        if same.is_some() {
            old = olds.as_mut().and_then(next);
        }
        if same == Some(true) {
            continue;
        }
        write!(gi.1, "M {:o} ", r.mode).expect("git stdin");
        gi.1.write_all(&hex(&r.id)).expect("git stdin");
        gi.1.write_all(b" ").expect("git stdin");
        gi.1.write_all(&quoted(&r.path)).expect("git stdin");
        gi.1.write_all(b"\n").expect("git stdin");
    }
    while let Some(o) = old {
        delete(&mut gi.1, &o.path);
        old = olds.as_mut().and_then(next);
    }
    gi.1.write_all(b"\n").expect("git stdin");
    finish(gi);
    println!(
        "{{\"n\":{n},\"ms\":{},\"walk_ms\":{walk_ms},\"files\":{},\"read\":{},\"reused\":{},\"new_bytes\":{},\"complete\":{},\"not_covered\":{},\"ignored\":{},\"nested\":{},\"cpu_ms\":{},\"rss\":{},\"footprint\":{},\"git_rss\":{},\"git_cpu_ms\":{}}}",
        t0.elapsed().as_millis(),
        c.files,
        c.read,
        c.reused,
        c.new_bytes,
        !c.incomplete,
        c.uncovered,
        c.ignored,
        c.nested,
        cpu_ms(libc::RUSAGE_SELF),
        maxrss(libc::RUSAGE_SELF),
        footprint(),
        maxrss(libc::RUSAGE_CHILDREN),
        cpu_ms(libc::RUSAGE_CHILDREN)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_ids_are_gits() {
        let mut h = Sha1::new();
        h.update(b"blob 6\0hello\n");
        assert_eq!(
            &hex(&h.finalize().into()),
            b"ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn walk_order_compares_components() {
        assert_eq!(tree_cmp(b"a/b", b"a.txt"), Ordering::Less);
        assert_eq!(tree_cmp(b"a", b"a/b"), Ordering::Less);
    }

    #[test]
    fn gitignore_globs() {
        let m = |p: &str, s: &str| glob(&pattern(p.as_bytes()).unwrap().glob, s.as_bytes());
        assert!(m("*.[oa]", "x.o") && !m("*.[oa]", "x.c"));
        assert!(m("**/foo", "foo") && m("**/foo", "a/b/foo"));
        assert!(m("a/**/b", "a/b") && m("a/**/b", "a/x/y/b"));
        assert!(!m("*.o", "d/x.o"), "a star stops at a slash");
        assert!(pattern(b"!keep").unwrap().neg && pattern(b"build/").unwrap().dir_only);
        assert!(pattern(b"# c").is_none() && pattern(b"/x").unwrap().anchored);
    }
}
