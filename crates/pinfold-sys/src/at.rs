//! Descriptor-relative file access that never follows a symbolic link (ADR-0050, slice 1).
//!
//! Every call takes a directory descriptor and one path component, so a name cannot climb out
//! with `..` or pass through a link planted in a parent: the walk and the restore of a
//! checkpoint open each component from its parent's descriptor. A link where a file or
//! directory was asked for is refused as [`Error::Link`], and calls that act on a name
//! (`rename_at`, `unlink_at`, `symlink_at`, `mkdir_at`) act on the link itself.
//!
//! # Examples
//!
//! ```
//! use std::io::{Read, Write};
//! use std::os::fd::AsFd;
//! use pinfold_sys::at;
//!
//! let path = std::env::temp_dir().join(format!("pinfold-at-doc-{}", std::process::id()));
//! std::fs::create_dir_all(&path)?;
//! let dir = at::open_dir(&path)?;
//! at::create_at(dir.as_fd(), "new".as_ref())?.write_all(b"text")?;
//! at::rename_at(dir.as_fd(), "new".as_ref(), dir.as_fd(), "file".as_ref())?;
//! let mut text = String::new();
//! at::open_file_at(dir.as_fd(), "file".as_ref())?.read_to_string(&mut text)?;
//! assert_eq!(text, "text");
//! assert_eq!(at::stat_at(dir.as_fd(), "file".as_ref())?.kind, at::Kind::File);
//! std::fs::remove_dir_all(&path)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::ffi::{CStr, CString, OsStr};
use std::fs::{File, Metadata};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::ptr::NonNull;

/// Why a descriptor-relative call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The name is not one path component: empty, `.`, `..`, or holding `/` or NUL.
    #[error("not a single path component")]
    Name,
    /// A symbolic link is where a file or directory was asked for. It was not followed.
    #[error("a symbolic link, not followed")]
    Link,
    /// A link's target is at least as long as the buffer it was read into.
    #[error("link target does not fit {0} bytes")]
    TooLong(usize),
    /// The system refused the call.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// What a descriptor-relative call returns.
pub type Result<T> = std::result::Result<T, Error>;

/// What a directory entry is, from its own mode, never its link's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Link,
    /// A fifo, socket or device.
    Other,
}

/// The parts of a `stat` a checkpoint records and compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// What the entry is.
    pub kind: Kind,
    /// Permission bits, `st_mode & 0o7777`.
    pub mode: u32,
    /// Size in bytes.
    pub size: u64,
    /// Hard link count.
    pub links: u64,
    /// Device of the file system holding it.
    pub device: u64,
    /// Inode number.
    pub inode: u64,
    /// Modification time, nanoseconds since the Unix epoch.
    pub modified_ns: i64,
    /// Status change time, nanoseconds since the Unix epoch.
    pub changed_ns: i64,
}

#[allow(
    clippy::useless_conversion,
    reason = "S_IFMT is u16 on macOS, u32 on Linux"
)]
fn kind(mode: u32) -> Kind {
    match mode & u32::from(libc::S_IFMT) {
        m if m == u32::from(libc::S_IFREG) => Kind::File,
        m if m == u32::from(libc::S_IFDIR) => Kind::Dir,
        m if m == u32::from(libc::S_IFLNK) => Kind::Link,
        _ => Kind::Other,
    }
}

/// So a file's `fstat` through [`File::metadata`], before and after reading it, compares
/// with the [`stat_at`] that found it.
impl From<&Metadata> for Stat {
    fn from(meta: &Metadata) -> Self {
        let ns = |s: i64, n: i64| s.saturating_mul(1_000_000_000).saturating_add(n);
        Self {
            kind: kind(meta.mode()),
            mode: meta.mode() & 0o7777,
            size: meta.size(),
            links: meta.nlink(),
            device: meta.dev(),
            inode: meta.ino(),
            modified_ns: ns(meta.mtime(), meta.mtime_nsec()),
            changed_ns: ns(meta.ctime(), meta.ctime_nsec()),
        }
    }
}

/// One path component as a C string, refused if it could name anything but an entry of the
/// directory it is relative to.
fn component(name: &OsStr) -> Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(Error::Name);
    }
    CString::new(bytes).map_err(|_| Error::Name)
}

fn check(result: libc::c_int) -> Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(result)
    }
}

/// An open that `O_NOFOLLOW` refused. Linux answers `ENOTDIR` rather than `ELOOP` for a link
/// opened with `O_DIRECTORY`, so a refusal is checked against the entry itself.
fn refused(dir: libc::c_int, name: &CStr, error: Error) -> Error {
    let Error::Io(io) = &error else { return error };
    match io.raw_os_error() {
        Some(libc::ELOOP) => Error::Link,
        Some(libc::ENOTDIR) if lstat(dir, name).is_ok_and(|s| s.kind == Kind::Link) => Error::Link,
        _ => error,
    }
}

fn open(dir: libc::c_int, name: &CStr, flags: libc::c_int, mode: libc::c_uint) -> Result<OwnedFd> {
    let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `name` is a live NUL-terminated string the call does not keep, and the mode is
    // passed as the promoted integer the variadic `openat` reads.
    let fd = check(unsafe { libc::openat(dir, name.as_ptr(), flags, mode) })
        .map_err(|e| refused(dir, name, e))?;
    // SAFETY: `openat` succeeded, so `fd` is a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "the stat field types differ by platform"
)]
fn lstat(dir: libc::c_int, name: &CStr) -> Result<Stat> {
    let mut st = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `name` is NUL-terminated and `st` points to a `stat` the call fills; neither is
    // kept.
    check(unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    // SAFETY: the call succeeded, so it filled in the struct.
    let st = unsafe { st.assume_init() };
    let ns = |s: libc::time_t, n: libc::c_long| {
        i64::from(s)
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(n))
    };
    let mode = u32::from(st.st_mode);
    Ok(Stat {
        kind: kind(mode),
        mode: mode & 0o7777,
        size: u64::try_from(st.st_size).unwrap_or(0),
        links: u64::from(st.st_nlink),
        device: u64::try_from(st.st_dev).unwrap_or(0),
        inode: u64::from(st.st_ino),
        modified_ns: ns(st.st_mtime, st.st_mtime_nsec),
        changed_ns: ns(st.st_ctime, st.st_ctime_nsec),
    })
}

/// Opens the directory at `path`, refused if its last component is a link, as the root a walk
/// or restore opens every other entry from. Links in earlier components are followed: this is
/// the working directory the core recorded at launch.
///
/// # Errors
///
/// [`Error::Link`] if `path` is a link, otherwise the system's error, such as `ENOTDIR`.
pub fn open_dir(path: &Path) -> Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Name)?;
    open(libc::AT_FDCWD, &path, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

/// Opens the directory `name` in `dir`, refused if it is a link.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, [`Error::Link`] for a link, otherwise the
/// system's error.
pub fn open_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd> {
    let name = component(name)?;
    open(
        dir.as_raw_fd(),
        &name,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )
}

/// Opens `name` in `dir` for reading, refused if it is a link. `O_NONBLOCK`, so a fifo cannot
/// hang the open, and `O_NOCTTY`; the caller checks it got a regular file with
/// [`File::metadata`].
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, [`Error::Link`] for a link, otherwise the
/// system's error.
pub fn open_file_at(dir: BorrowedFd<'_>, name: &OsStr) -> Result<File> {
    let name = component(name)?;
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;
    Ok(open(dir.as_raw_fd(), &name, flags, 0)?.into())
}

/// Makes `name` in `dir` as a new file, mode 0600, open for writing. Anything already there,
/// a dangling link included, is refused with `EEXIST`, so nothing a link points to is written.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, otherwise the system's error.
pub fn create_at(dir: BorrowedFd<'_>, name: &OsStr) -> Result<File> {
    let name = component(name)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
    Ok(open(dir.as_raw_fd(), &name, flags, 0o600)?.into())
}

/// Returns the `stat` of `name` in `dir` itself, a link's own and not its target's.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, otherwise the system's error.
pub fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> Result<Stat> {
    lstat(dir.as_raw_fd(), &component(name)?)
}

/// Reads the target of the link `name` in `dir` into `buf`, never following it.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, [`Error::TooLong`] if the target fills
/// `buf`, as it may have been cut, otherwise the system's error, `EINVAL` for a non-link.
pub fn read_link_at<'a>(dir: BorrowedFd<'_>, name: &OsStr, buf: &'a mut [u8]) -> Result<&'a [u8]> {
    let name = component(name)?;
    // SAFETY: `name` is NUL-terminated and the call writes at most `buf.len()` bytes to `buf`.
    let n = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    if n >= buf.len() {
        return Err(Error::TooLong(buf.len()));
    }
    Ok(&buf[..n])
}

/// Removes the file or link `name` from `dir`; a link goes, not what it points to.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, otherwise the system's error.
pub fn unlink_at(dir: BorrowedFd<'_>, name: &OsStr) -> Result<()> {
    let name = component(name)?;
    // SAFETY: `name` is NUL-terminated and not kept.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) }).map(drop)
}

/// Removes the empty directory `name` from `dir`. A link there is refused with `ENOTDIR`.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, otherwise the system's error.
pub fn remove_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> Result<()> {
    let name = component(name)?;
    // SAFETY: `name` is NUL-terminated and not kept.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }).map(drop)
}

/// Makes `name` in `dir` a link to `target`, which is stored as text and may be any path.
///
/// # Errors
///
/// [`Error::Name`] if `name` is not one component or `target` holds NUL, otherwise the
/// system's error, `EEXIST` if anything is at `name`.
pub fn symlink_at(target: &OsStr, dir: BorrowedFd<'_>, name: &OsStr) -> Result<()> {
    let target = CString::new(target.as_bytes()).map_err(|_| Error::Name)?;
    let name = component(name)?;
    // SAFETY: both strings are NUL-terminated and not kept.
    check(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) }).map(drop)
}

/// Renames `from` in `from_dir` to `to` in `to_dir`, replacing whatever entry is at `to` in one
/// step. A link at either name is moved or replaced itself, so what it points to is never
/// written.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, otherwise the system's error.
pub fn rename_at(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
) -> Result<()> {
    let (from, to) = (component(from)?, component(to)?);
    // SAFETY: both names are NUL-terminated and not kept.
    let result = unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
        )
    };
    check(result).map(drop)
}

/// Makes the directory `name` in `dir` with `mode`, less the umask. Anything already there, a
/// link included, is refused with `EEXIST`.
///
/// # Errors
///
/// [`Error::Name`] for anything but one component, otherwise the system's error.
#[allow(
    clippy::unnecessary_fallible_conversions,
    reason = "mode_t is u16 on macOS"
)]
pub fn mkdir_at(dir: BorrowedFd<'_>, name: &OsStr, mode: u32) -> Result<()> {
    let name = component(name)?;
    let mode = libc::mode_t::try_from(mode & 0o7777).map_err(io::Error::other)?;
    // SAFETY: `name` is NUL-terminated and not kept.
    check(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode) }).map(drop)
}

/// The names in a directory, read from its descriptor, `.` and `..` left out, in the order
/// the file system gives. Each name borrows the system's one entry buffer until the next.
#[derive(Debug)]
pub struct Entries {
    dir: NonNull<libc::DIR>,
}

/// Lists `dir` from its start, through a copy of the descriptor so `dir` stays open. The copy
/// shares the descriptor's offset, so lists of the same `dir` must not interleave.
///
/// # Errors
///
/// Returns the system's error if the descriptor cannot be copied or is not a directory.
pub fn read_dir_at(dir: BorrowedFd<'_>) -> Result<Entries> {
    let copy = dir.try_clone_to_owned()?;
    // SAFETY: `fdopendir` takes ownership of the descriptor only when it succeeds.
    let stream = unsafe { libc::fdopendir(copy.as_raw_fd()) };
    let dir = NonNull::new(stream).ok_or_else(io::Error::last_os_error)?;
    let _owned_by_the_stream = copy.into_raw_fd();
    // SAFETY: `dir` is the open stream just made; an earlier list may have moved the offset.
    unsafe { libc::rewinddir(dir.as_ptr()) };
    Ok(Entries { dir })
}

impl Entries {
    /// Returns the next name, or nothing at the end.
    ///
    /// # Errors
    ///
    /// Returns the system's error if the directory cannot be read, so a cut-short list is
    /// never taken for the whole one.
    pub fn next_name(&mut self) -> Result<Option<&OsStr>> {
        loop {
            set_errno(0);
            // SAFETY: `dir` is open until drop, and `&mut self` keeps one reader at a time.
            let entry = unsafe { libc::readdir(self.dir.as_ptr()) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(0) => Ok(None),
                    _ => Err(error.into()),
                };
            }
            // SAFETY: a non-null entry holds a NUL-terminated name valid until the next
            // `readdir` or `closedir`, both of which need `&mut self`, so the borrow is sound.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                return Ok(Some(OsStr::from_bytes(name)));
            }
        }
    }
}

impl Drop for Entries {
    fn drop(&mut self) {
        // SAFETY: the stream is open and dropped once; this also closes its descriptor.
        unsafe { libc::closedir(self.dir.as_ptr()) };
    }
}

fn set_errno(value: libc::c_int) {
    #[cfg(target_os = "macos")]
    let errno = libc::__error;
    #[cfg(target_os = "linux")]
    let errno = libc::__errno_location;
    // SAFETY: both return this thread's errno, a valid int for the thread's life.
    unsafe { *errno() = value };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A scratch directory, with a `secret` file beside it standing for what an agent points a
    /// link at.
    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(test: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("pinfold-at-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("tree")).unwrap();
            fs::create_dir_all(root.join("outside")).unwrap();
            fs::write(root.join("outside/secret"), "secret").unwrap();
            Self { root }
        }

        fn tree(&self) -> PathBuf {
            self.root.join("tree")
        }

        fn secret(&self) -> String {
            fs::read_to_string(self.root.join("outside/secret")).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn os(name: &str) -> &OsStr {
        name.as_ref()
    }

    #[test]
    fn a_name_is_one_component() {
        let s = Scratch::new("names");
        let dir = open_dir(&s.tree()).unwrap();
        for bad in ["", ".", "..", "../outside", "a/b", "/etc", "nul\0"] {
            assert!(
                matches!(open_file_at(dir.as_fd(), os(bad)), Err(Error::Name)),
                "{bad:?}"
            );
            assert!(
                matches!(stat_at(dir.as_fd(), os(bad)), Err(Error::Name)),
                "{bad:?}"
            );
            assert!(
                matches!(create_at(dir.as_fd(), os(bad)), Err(Error::Name)),
                "{bad:?}"
            );
            assert!(
                matches!(mkdir_at(dir.as_fd(), os(bad), 0o700), Err(Error::Name)),
                "{bad:?}"
            );
            assert!(
                matches!(unlink_at(dir.as_fd(), os(bad)), Err(Error::Name)),
                "{bad:?}"
            );
        }
        assert!(matches!(
            symlink_at(os("a\0b"), dir.as_fd(), os("l")),
            Err(Error::Name)
        ));
        assert_eq!(fs::read_dir(s.tree()).unwrap().count(), 0);
    }

    #[test]
    fn a_root_that_is_a_link_is_refused() {
        let s = Scratch::new("root");
        symlink(s.root.join("outside"), s.root.join("link")).unwrap();
        assert!(matches!(open_dir(&s.root.join("link")), Err(Error::Link)));
        assert!(open_dir(&s.tree()).is_ok());
    }

    #[test]
    fn a_component_swapped_for_a_link_is_refused() {
        let s = Scratch::new("swap");
        fs::create_dir(s.tree().join("src")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        let src = open_dir_at(dir.as_fd(), os("src")).unwrap();
        // The agent replaces `src` with a link to the secret's directory.
        fs::rename(s.tree().join("src"), s.tree().join("old")).unwrap();
        symlink(s.root.join("outside"), s.tree().join("src")).unwrap();
        symlink(s.root.join("outside/secret"), s.tree().join("file")).unwrap();
        assert!(matches!(
            open_dir_at(dir.as_fd(), os("src")),
            Err(Error::Link)
        ));
        assert!(matches!(
            open_file_at(dir.as_fd(), os("src")),
            Err(Error::Link)
        ));
        assert!(matches!(
            open_file_at(dir.as_fd(), os("file")),
            Err(Error::Link)
        ));
        // A descriptor opened before the swap still names the real directory.
        create_at(src.as_fd(), os("new")).unwrap();
        assert!(s.tree().join("old/new").exists());
        assert!(!s.root.join("outside/new").exists());
    }

    #[test]
    fn stat_reports_a_link_itself_and_matches_fstat() {
        let s = Scratch::new("stat");
        fs::write(s.tree().join("file"), "four").unwrap();
        symlink(s.root.join("outside/secret"), s.tree().join("link")).unwrap();
        fs::create_dir(s.tree().join("dir")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        let file = stat_at(dir.as_fd(), os("file")).unwrap();
        assert_eq!((file.kind, file.size, file.links), (Kind::File, 4, 1));
        let opened = open_file_at(dir.as_fd(), os("file")).unwrap();
        assert_eq!(Stat::from(&opened.metadata().unwrap()), file);
        assert_eq!(stat_at(dir.as_fd(), os("link")).unwrap().kind, Kind::Link);
        assert_eq!(stat_at(dir.as_fd(), os("dir")).unwrap().kind, Kind::Dir);
        fs::hard_link(s.tree().join("file"), s.tree().join("hard")).unwrap();
        assert_eq!(stat_at(dir.as_fd(), os("file")).unwrap().links, 2);
        let missing = stat_at(dir.as_fd(), os("missing")).unwrap_err();
        assert!(matches!(missing, Error::Io(e) if e.kind() == io::ErrorKind::NotFound));
    }

    #[test]
    fn a_fifo_opens_without_hanging() {
        let s = Scratch::new("fifo");
        let path = CString::new(s.tree().join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path, not kept.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let dir = open_dir(&s.tree()).unwrap();
        assert_eq!(stat_at(dir.as_fd(), os("fifo")).unwrap().kind, Kind::Other);
        let fifo = open_file_at(dir.as_fd(), os("fifo")).unwrap();
        assert!(!fifo.metadata().unwrap().is_file());
    }

    #[test]
    fn read_link_never_follows_and_refuses_a_cut_target() {
        let s = Scratch::new("readlink");
        symlink("../outside/secret", s.tree().join("link")).unwrap();
        fs::write(s.tree().join("file"), "x").unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        let mut buf = [0; 64];
        assert_eq!(
            read_link_at(dir.as_fd(), os("link"), &mut buf).unwrap(),
            b"../outside/secret"
        );
        let mut short = [0; 17];
        assert!(matches!(
            read_link_at(dir.as_fd(), os("link"), &mut short),
            Err(Error::TooLong(17))
        ));
        let not_link = read_link_at(dir.as_fd(), os("file"), &mut buf).unwrap_err();
        assert!(matches!(not_link, Error::Io(e) if e.raw_os_error() == Some(libc::EINVAL)));
    }

    #[test]
    fn create_refuses_a_planted_link_and_writes_nothing_through_it() {
        let s = Scratch::new("create");
        symlink(s.root.join("outside/secret"), s.tree().join("file")).unwrap();
        symlink(s.root.join("outside/absent"), s.tree().join("dangling")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        for name in ["file", "dangling"] {
            let refused = create_at(dir.as_fd(), os(name)).unwrap_err();
            assert!(matches!(refused, Error::Io(e) if e.kind() == io::ErrorKind::AlreadyExists));
        }
        assert!(!s.root.join("outside/absent").exists());
        assert_eq!(s.secret(), "secret");
        let mut new = create_at(dir.as_fd(), os("new")).unwrap();
        new.write_all(b"new").unwrap();
        assert_eq!(new.metadata().unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_rename_over_a_link_replaces_the_link_not_its_target() {
        let s = Scratch::new("rename");
        symlink(s.root.join("outside/secret"), s.tree().join("file")).unwrap();
        symlink(s.root.join("outside"), s.tree().join("dir")).unwrap();
        fs::write(s.root.join("outside/hard"), "hard").unwrap();
        fs::hard_link(s.root.join("outside/hard"), s.tree().join("hard")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        for name in ["file", "dir", "hard"] {
            create_at(dir.as_fd(), os("tmp"))
                .unwrap()
                .write_all(b"restored")
                .unwrap();
            rename_at(dir.as_fd(), os("tmp"), dir.as_fd(), os(name)).unwrap();
            assert_eq!(stat_at(dir.as_fd(), os(name)).unwrap().kind, Kind::File);
            assert_eq!(fs::read_to_string(s.tree().join(name)).unwrap(), "restored");
        }
        assert_eq!(s.secret(), "secret");
        assert_eq!(
            fs::read_to_string(s.root.join("outside/hard")).unwrap(),
            "hard"
        );
        assert!(s.root.join("outside").is_dir());
        // A link made with symlink_at is renamed into place the same way.
        symlink_at(os("target"), dir.as_fd(), os("tmp")).unwrap();
        rename_at(dir.as_fd(), os("tmp"), dir.as_fd(), os("file")).unwrap();
        let mut buf = [0; 16];
        assert_eq!(
            read_link_at(dir.as_fd(), os("file"), &mut buf).unwrap(),
            b"target"
        );
        // Across directories, from descriptors.
        mkdir_at(dir.as_fd(), os("sub"), 0o755).unwrap();
        let sub = open_dir_at(dir.as_fd(), os("sub")).unwrap();
        rename_at(dir.as_fd(), os("file"), sub.as_fd(), os("moved")).unwrap();
        assert!(
            fs::symlink_metadata(s.tree().join("sub/moved"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn unlink_removes_a_link_not_its_target() {
        let s = Scratch::new("unlink");
        symlink(s.root.join("outside/secret"), s.tree().join("file")).unwrap();
        symlink(s.root.join("outside"), s.tree().join("dir")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        unlink_at(dir.as_fd(), os("file")).unwrap();
        // A link to a directory is not a directory to remove_dir_at.
        let refused = remove_dir_at(dir.as_fd(), os("dir")).unwrap_err();
        assert!(matches!(refused, Error::Io(e) if e.raw_os_error() == Some(libc::ENOTDIR)));
        unlink_at(dir.as_fd(), os("dir")).unwrap();
        assert_eq!(s.secret(), "secret");
        assert!(s.root.join("outside").is_dir());
        assert_eq!(fs::read_dir(s.tree()).unwrap().count(), 0);
    }

    #[test]
    fn mkdir_refuses_a_planted_link_and_remove_dir_takes_only_an_empty_one() {
        let s = Scratch::new("mkdir");
        symlink(s.root.join("outside"), s.tree().join("src")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        let refused = mkdir_at(dir.as_fd(), os("src"), 0o755).unwrap_err();
        assert!(matches!(refused, Error::Io(e) if e.kind() == io::ErrorKind::AlreadyExists));
        // As a restore does: the link goes itself, then the directory is made.
        unlink_at(dir.as_fd(), os("src")).unwrap();
        mkdir_at(dir.as_fd(), os("src"), 0o700).unwrap();
        let src = stat_at(dir.as_fd(), os("src")).unwrap();
        assert_eq!(src.kind, Kind::Dir);
        assert_eq!(src.mode & 0o077, 0);
        let opened = open_dir_at(dir.as_fd(), os("src")).unwrap();
        create_at(opened.as_fd(), os("f")).unwrap();
        let full = remove_dir_at(dir.as_fd(), os("src")).unwrap_err();
        assert!(matches!(full, Error::Io(_)));
        unlink_at(opened.as_fd(), os("f")).unwrap();
        remove_dir_at(dir.as_fd(), os("src")).unwrap();
        assert!(s.root.join("outside/secret").exists());
    }

    #[test]
    fn read_dir_lists_names_from_the_descriptor_every_time() {
        let s = Scratch::new("readdir");
        for name in ["a", "b", ".env"] {
            fs::write(s.tree().join(name), name).unwrap();
        }
        symlink(s.root.join("outside"), s.tree().join("link")).unwrap();
        let dir = open_dir(&s.tree()).unwrap();
        let list = |dir: BorrowedFd<'_>| {
            let mut entries = read_dir_at(dir).unwrap();
            let mut names = Vec::new();
            while let Some(name) = entries.next_name().unwrap() {
                names.push(name.to_owned());
            }
            names.sort();
            names
        };
        let expected: Vec<_> = [".env", "a", "b", "link"]
            .map(std::ffi::OsString::from)
            .into();
        assert_eq!(list(dir.as_fd()), expected);
        // The shared offset is rewound, and the descriptor stays open for openat.
        assert_eq!(list(dir.as_fd()), expected);
        let mut text = String::new();
        open_file_at(dir.as_fd(), os("a"))
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "a");
        // The tree, renamed away and replaced by a link, is still what the descriptor lists.
        fs::rename(s.tree(), s.root.join("moved")).unwrap();
        symlink(s.root.join("outside"), s.tree()).unwrap();
        assert_eq!(list(dir.as_fd()), expected);
        let file = File::open(s.root.join("outside/secret")).unwrap();
        assert!(read_dir_at(file.as_fd()).is_err());
    }

    #[test]
    fn a_name_flipping_between_a_directory_and_a_link_never_opens_the_target() {
        let s = Scratch::new("race");
        fs::create_dir(s.tree().join("real")).unwrap();
        let real = fs::metadata(s.tree().join("real")).unwrap().ino();
        let target = fs::metadata(s.root.join("outside")).unwrap().ino();
        symlink(s.root.join("outside"), s.tree().join("link")).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flipper = {
            let (tree, stop) = (s.tree(), Arc::clone(&stop));
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // `src` is in turn the real directory and the link, each swap one rename.
                    fs::rename(tree.join("real"), tree.join("src")).unwrap();
                    fs::rename(tree.join("src"), tree.join("real")).unwrap();
                    fs::rename(tree.join("link"), tree.join("src")).unwrap();
                    fs::rename(tree.join("src"), tree.join("link")).unwrap();
                }
            })
        };
        let dir = open_dir(&s.tree()).unwrap();
        let (mut opened, mut refused) = (0, 0);
        for _ in 0..20_000 {
            match open_dir_at(dir.as_fd(), os("src")) {
                Ok(fd) => {
                    let ino = File::from(fd).metadata().unwrap().ino();
                    assert_eq!(ino, real);
                    assert_ne!(ino, target);
                    opened += 1;
                }
                Err(Error::Link) => refused += 1,
                // Linux's ENOTDIR for a link, when the name flipped back before the check.
                Err(Error::Io(e)) if e.raw_os_error() == Some(libc::ENOTDIR) => refused += 1,
                Err(Error::Io(e)) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => panic!("{e}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        flipper.join().unwrap();
        assert!(
            opened > 0 && refused > 0,
            "opened {opened}, refused {refused}"
        );
    }
}
