//! Helpers shared by the workspace's tests, as dev-dependencies only.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// Writes `text` to `path` as a program with `mode`, ready to run at once (#501).
///
/// A file this process writes leaks its descriptor, open for writing, into any child that
/// another test thread forks before the child's exec, and until then Linux refuses to run the
/// file with ETXTBSY. Renaming alone keeps the same file, so `cp`, a process of its own, writes
/// the program under a temporary name, which is then renamed into place.
///
/// # Errors
///
/// Returns an error if a file cannot be written, copied or renamed.
///
/// # Examples
///
/// ```
/// let path = std::env::temp_dir().join(format!("testkit-doc-{}", std::process::id()));
/// pinfold_testkit::program(&path, "#!/bin/sh\necho hi\n", 0o700)?;
/// let out = std::process::Command::new(&path).output()?;
/// assert_eq!(out.stdout, b"hi\n");
/// std::fs::remove_file(path)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn program(path: impl AsRef<Path>, text: impl AsRef<[u8]>, mode: u32) -> io::Result<()> {
    let path = path.as_ref();
    let temporary = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(format!(".{suffix}{}", std::process::id()));
        name
    };
    let (source, copy) = (temporary("source"), temporary("copy"));
    fs::write(&source, text)?;
    let copied = Command::new("/bin/cp").arg(&source).arg(&copy).status();
    fs::remove_file(&source)?;
    if !copied?.success() {
        return Err(io::Error::other(format!(
            "cp did not copy {}",
            path.display()
        )));
    }
    fs::set_permissions(&copy, fs::Permissions::from_mode(mode))?;
    fs::rename(copy, path)
}
