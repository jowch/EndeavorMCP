//! Questions about a host's files that the new-session screen asks before any
//! runtime exists: a folder's contents, the Pluto notebooks under a folder, and
//! a notebook's first cells. The helper answers them for a server, the app
//! itself for This Mac, both with [`answer`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::notebooks::{self, Found, Preview};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Request {
    /// Folders and `.jl` files in `path`, hidden ones left out.
    List { path: String },
    /// Pluto notebooks under `path` ([`notebooks::scan`]).
    Notebooks { path: String },
    /// The first cells of the notebook at `path`.
    Preview { path: String },
    /// Slurm's partitions here, and `$SCRATCH`.
    Slurm,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Reply {
    /// `path` is the folder listed, absolute and with `~` expanded.
    List { path: PathBuf, entries: Vec<Entry> },
    Notebooks { found: Vec<Found> },
    Preview { preview: Preview },
    Slurm { scheduler: crate::slurm::Scheduler },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

pub fn answer(request: &Request) -> Reply {
    let result = match request {
        Request::List { path } => list(&expand(path)),
        Request::Notebooks { path } => Ok(Reply::Notebooks { found: notebooks::scan(&expand(path)) }),
        Request::Preview { path } => notebooks::read_preview(&expand(path)).map(|preview| Reply::Preview { preview }),
        Request::Slurm => crate::slurm::probe().map(|scheduler| Reply::Slurm { scheduler }),
    };
    result.unwrap_or_else(|message| Reply::Error { message })
}

/// This machine's home folder.
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~` and `~/…` as the home folder; anything relative is taken from there.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') => home().join(&rest[1..]),
        _ => home().join(path),
    }
}

fn list(dir: &Path) -> Result<Reply, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("Couldn't open {}: {}", dir.display(), plain(&e)))?;
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Links count as what they point to.
            let dir = std::fs::metadata(entry.path()).ok()?.is_dir();
            (!name.starts_with('.') && (dir || name.ends_with(".jl"))).then_some(Entry { name, dir })
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let path = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    Ok(Reply::List { path, entries })
}

fn plain(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => "it doesn't exist".into(),
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_folders_first_then_julia_files() {
        let dir = std::env::temp_dir().join(format!("endeavor-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["b-data", "A-figures", ".git"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for file in ["fit.jl", "notes.txt", ".hidden.jl", "Analysis.jl"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        let Reply::List { path, entries } = answer(&Request::List { path: dir.display().to_string() }) else { panic!() };
        assert_eq!(path, dir.canonicalize().unwrap());
        let names: Vec<(&str, bool)> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("A-figures", true), ("b-data", true), ("Analysis.jl", false), ("fit.jl", false)]);
        let missing = answer(&Request::List { path: dir.join("nope").display().to_string() });
        assert!(matches!(missing, Reply::Error { message } if message.ends_with("it doesn't exist")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn home_relative_paths() {
        assert_eq!(expand("~"), home());
        assert_eq!(expand("~/decay-fits"), home().join("decay-fits"));
        assert_eq!(expand("decay-fits"), home().join("decay-fits"));
        assert_eq!(expand("/srv/data"), PathBuf::from("/srv/data"));
    }

    #[test]
    fn replies_are_tagged_json() {
        let json = serde_json::to_value(Request::Notebooks { path: "~/x".into() }).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "Notebooks", "path": "~/x" }));
        let reply = Reply::Error { message: "no".into() };
        assert_eq!(serde_json::from_value::<Reply>(serde_json::to_value(&reply).unwrap()).unwrap(), reply);
    }
}
