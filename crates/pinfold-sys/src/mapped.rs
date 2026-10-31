//! Read-only file mappings, so a file costs only the pages that are read.

use std::fs::File;
use std::io;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::slice;

/// A file mapped read-only into memory.
///
/// Another process truncating the file while it is mapped raises `SIGBUS`, so this is
/// for the window process, where a crash cannot kill a PTY. The core must not use it.
#[derive(Debug)]
pub struct Mapped {
    data: NonNull<u8>,
    length: usize,
}

// SAFETY: the mapping is read-only and owned, so sharing it across threads is safe.
unsafe impl Send for Mapped {}
// SAFETY: as above, nothing can write through it.
unsafe impl Sync for Mapped {}

impl Mapped {
    /// Maps a whole file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be opened or mapped, or is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// let bytes = pinfold_sys::mapped::Mapped::open("/bin/sh".as_ref())?;
    /// assert!(!bytes.is_empty());
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let length = usize::try_from(file.metadata()?.len()).map_err(io::Error::other)?;
        if length == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty file"));
        }
        // SAFETY: the fd is open, the length is the file's non-zero size, and a null
        // address lets the kernel choose. The mapping outlives closing the fd.
        let data = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if data == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let data = NonNull::new(data.cast()).ok_or_else(io::Error::last_os_error)?;
        Ok(Self { data, length })
    }
}

impl Deref for Mapped {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `data` points to `length` readable bytes until `drop` unmaps them.
        unsafe { slice::from_raw_parts(self.data.as_ptr(), self.length) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: this is the mapping `open` made, and nothing can borrow it past drop.
        unsafe { libc::munmap(self.data.as_ptr().cast(), self.length) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapping_matches_the_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let mapped = Mapped::open(&path).unwrap();
        assert_eq!(&*mapped, std::fs::read(path).unwrap());
    }

    #[test]
    fn a_missing_file_is_an_error() {
        assert!(Mapped::open(Path::new("/does/not/exist")).is_err());
    }

    #[test]
    fn an_empty_file_is_an_error() {
        assert!(Mapped::open(Path::new("/dev/null")).is_err());
    }
}
