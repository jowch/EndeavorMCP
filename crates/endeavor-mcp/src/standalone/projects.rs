//! What each project remembers (docs/plugins-and-remote.md): the machine its
//! agent sessions last used and the folder there. One file the binary owns,
//! `<state home>/endeavor/projects.json`: a JSON object from a project's folder
//! (the front's `--folder`, as its canonical path) to a `Remembered`. It is
//! written whole to a temporary file that is then renamed, readable by this
//! user only, under a lock. A file that can't be read or parsed is an error
//! that names it, and is never replaced.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Remembered {
    /// The machine's id in the machines file.
    pub machine: String,
    /// The folder on that machine; none is the server's home folder.
    #[serde(default)]
    pub folder: Option<String>,
}

pub(crate) struct Projects {
    path: PathBuf,
}

impl Projects {
    pub fn at(path: impl Into<PathBuf>) -> Projects {
        Projects { path: path.into() }
    }

    #[cfg(all(test, unix))]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What `folder` remembers, if anything.
    pub fn get(&self, folder: &Path) -> Result<Option<Remembered>, String> {
        Ok(self.load()?.remove(&key(folder)))
    }

    /// Remember `what` for `folder`, or forget what it remembers.
    pub fn set(&self, folder: &Path, what: Option<Remembered>) -> Result<(), String> {
        let key = key(folder);
        self.change(|projects| match what {
            Some(what) => drop(projects.insert(key, what)),
            None => drop(projects.remove(&key)),
        })
    }

    fn load(&self) -> Result<BTreeMap<String, Remembered>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(format!("Couldn't read what projects remember in {}: {e}", self.path.display())),
        };
        serde_json::from_str(&text).map_err(|e| format!("What projects remember in {} isn't valid ({e}). Fix or remove the file; Endeavor won't overwrite it.", self.path.display()))
    }

    fn change(&self, change: impl FnOnce(&mut BTreeMap<String, Remembered>)) -> Result<(), String> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        crate::make_state_dir(dir)?;
        let lock_path = self.path.with_extension("lock");
        let lock = crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false))
            .open(&lock_path)
            .map_err(|e| format!("Couldn't open {}: {e}", lock_path.display()))?;
        let started = Instant::now();
        while !crate::try_lock(&lock) {
            if started.elapsed() > Duration::from_secs(10) {
                return Err(format!("Another Endeavor process has held {} for too long.", lock_path.display()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut projects = self.load()?;
        change(&mut projects);
        let text = serde_json::to_string_pretty(&projects).map_err(|e| e.to_string())?;
        let tmp = self.path.with_extension(format!("json.tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        crate::owner_only(std::fs::OpenOptions::new().write(true).create_new(true))
            .open(&tmp)
            .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|_| f.sync_all()))
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("Couldn't write what projects remember to {}: {e}", self.path.display())
            })
    }
}

/// A project's key: its folder as the system spells it, so that two ways to reach it are one project.
fn key(folder: &Path) -> String {
    std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf()).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp").join(format!("projects-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn a_project_remembers_its_machine_and_forgets_it() {
        let dir = scratch("remember");
        let projects = Projects::at(dir.join("state/endeavor/projects.json"));
        let (one, two) = (dir.join("one"), dir.join("two"));
        std::fs::create_dir_all(&one).unwrap();
        assert_eq!(projects.get(&one).unwrap(), None, "no file yet");
        projects.set(&one, Some(Remembered { machine: "lab".into(), folder: Some("/home/ada/work".into()) })).unwrap();
        projects.set(&two, Some(Remembered { machine: "hpc".into(), folder: None })).unwrap();
        assert_eq!(projects.get(&one).unwrap(), Some(Remembered { machine: "lab".into(), folder: Some("/home/ada/work".into()) }));
        assert_eq!(projects.get(&dir.join("one/../one")).unwrap().map(|r| r.machine), Some("lab".into()), "the same folder by another spelling");
        projects.set(&one, None).unwrap();
        assert_eq!(projects.get(&one).unwrap(), None);
        assert_eq!(projects.get(&two).unwrap().map(|r| r.machine), Some("hpc".into()), "the other project keeps its own");
        projects.set(&one, None).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_whole_and_leaves_nothing_beside_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("private");
        let projects = Projects::at(dir.join("projects.json"));
        projects.set(&dir, Some(Remembered { machine: "lab".into(), folder: None })).unwrap();
        assert_eq!(std::fs::metadata(projects.path()).unwrap().permissions().mode() & 0o777, 0o600);
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["projects.json", "projects.lock"]);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_named_and_kept() {
        let dir = scratch("broken");
        let path = dir.join("projects.json");
        std::fs::write(&path, "{ not json").unwrap();
        let projects = Projects::at(&path);
        let said = projects.get(&dir).unwrap_err();
        assert!(said.contains(&path.display().to_string()) && said.contains("won't overwrite"), "{said}");
        assert!(projects.set(&dir, None).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
