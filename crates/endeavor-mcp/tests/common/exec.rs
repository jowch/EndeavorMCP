#![allow(dead_code)]

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
pub fn write_executable(path: &Path, contents: &str) {
    let status = std::process::Command::new("/bin/sh").args(["-c", r#"printf '%s' "$1" > "$0""#]).arg(path).arg(contents).status().unwrap();
    assert!(status.success(), "couldn't write {}", path.display());
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
pub fn copy_executable(from: &Path, to: &Path) {
    let status = std::process::Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(status.success(), "couldn't copy {} to {}", from.display(), to.display());
    std::fs::set_permissions(to, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}
