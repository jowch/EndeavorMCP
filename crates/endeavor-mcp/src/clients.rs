//! `DIR/clients/`, which processes are attached to the runtime in a state
//! folder: one file per process, named from its host and pid, which the
//! process keeps locked while it is attached. A process that is gone, killed
//! or not, no longer holds its lock, so a file whose lock can be taken is
//! nobody's and doesn't count. A helper whose client's input ends uses this
//! to leave a runtime that others still use.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How old an unlocked file must be before it is removed: a process that
/// has just made its file may not have locked it yet.
const STALE_AFTER: Duration = Duration::from_secs(10);

/// This process's file in `DIR/clients/`, locked while the value lives.
pub struct Presence {
    _file: File,
    path: PathBuf,
}

/// Record that this process is attached to the runtime in `dir`. None if the
/// file can't be made or locked (this process is already registered there).
pub fn register(dir: &Path) -> Option<Presence> {
    let clients = dir.join("clients");
    std::fs::create_dir_all(&clients).ok()?;
    let path = clients.join(name());
    let file = crate::owner_only(OpenOptions::new().create(true).write(true).truncate(false)).open(&path).ok()?;
    crate::try_lock(&file).then_some(Presence { _file: file, path })
}

/// The file name of this process: host and pid, safe in a path.
fn name() -> String {
    let host: String = crate::hostname().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    format!("{host}-{}", std::process::id())
}

impl Presence {
    /// How many other processes are attached to the same runtime. Files of
    /// processes that are gone are removed on the way.
    pub fn others(&self) -> usize {
        let Some(clients) = self.path.parent() else { return 0 };
        let Ok(entries) = std::fs::read_dir(clients) else { return 0 };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| *path != self.path)
            .filter(|path| {
                let Ok(file) = OpenOptions::new().write(true).open(path) else { return false };
                if !crate::try_lock(&file) {
                    return true;
                }
                let old = file.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age > STALE_AFTER);
                drop(file);
                if old {
                    let _ = std::fs::remove_file(path);
                }
                false
            })
            .count()
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("clients-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A file another process held, and no longer does.
    fn left_behind(dir: &Path, name: &str, age: Duration) -> PathBuf {
        let path = dir.join("clients").join(name);
        std::fs::write(&path, "").unwrap();
        File::options().write(true).open(&path).unwrap().set_modified(SystemTime::now() - age).unwrap();
        path
    }

    #[test]
    fn a_process_sees_the_others_that_hold_their_files() {
        let dir = dir("others");
        let mine = register(&dir).unwrap();
        assert_eq!(mine.others(), 0);
        assert!(register(&dir).is_none(), "one record per process");

        let other = dir.join("clients/elsewhere-1");
        let held = File::create(&other).unwrap();
        assert!(crate::try_lock(&held));
        assert_eq!(mine.others(), 1);
        drop(held);
        // A process forked by another test holds the lock until it execs.
        let mut left = mine.others();
        for _ in 0..50 {
            if left == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            left = mine.others();
        }
        assert_eq!(left, 0, "a file whose holder is gone doesn't count");
    }

    #[test]
    fn files_of_gone_processes_are_removed_once_they_are_old() {
        let dir = dir("stale");
        let mine = register(&dir).unwrap();
        let old = left_behind(&dir, "elsewhere-2", STALE_AFTER * 2);
        let new = left_behind(&dir, "elsewhere-3", Duration::ZERO);
        assert_eq!(mine.others(), 0);
        assert!(!old.exists(), "an old file nobody holds goes");
        assert!(new.exists(), "a new one may be about to be locked");
    }

    #[test]
    fn dropping_the_record_removes_it() {
        let dir = dir("drop");
        let mine = register(&dir).unwrap();
        let path = mine.path.clone();
        assert!(path.exists());
        drop(mine);
        assert!(!path.exists());
        assert!(register(&dir).is_some(), "and it can be made again");
    }
}
