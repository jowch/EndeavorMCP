//! Executable files for tests' stand-in tools, written so running one can't fail with ETXTBSY
//! ("Text file busy"). Shared by `tests/common` and the crate's own tests (#104).

#![allow(dead_code)]

#[cfg(unix)]
use std::path::Path;

/// Write `contents` to `path` as an executable file. A child process writes it: while this process holds
/// a file open for writing, a fork by any of its threads copies that descriptor until the child execs,
/// and running the file in that time fails with ETXTBSY ("Text file busy"). The tests run on many threads.
#[cfg(unix)]
pub fn write_executable(path: &Path, contents: &str) {
    let status = std::process::Command::new("/bin/sh").args(["-c", r#"printf '%s' "$1" > "$0""#]).arg(path).arg(contents).status().unwrap();
    assert!(status.success(), "couldn't write {}", path.display());
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// Copy `from` to `to` as an executable, in a child process for the same reason.
#[cfg(unix)]
pub fn copy_executable(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(status.success(), "couldn't copy {} to {}", from.display(), to.display());
    std::fs::set_permissions(to, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}
