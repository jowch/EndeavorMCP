//! Questions about a host that the app asks before (or without) attaching to
//! its runtime: a folder's contents, the Pluto notebooks under a folder, a
//! notebook's first cells, Slurm's partitions, and whether a runtime or its job
//! is there. The helper answers them, all but `Runtime` with [`answer`].
//! Also the one write: a file the user attached, sent in pieces into the
//! session's folder (`Place`, then `Write`).

use std::io::{Read, Write as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::notebooks::{self, Found, Preview};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Request {
    /// Folders and `.jl` files in `path`, hidden ones left out.
    List { path: String },
    /// Every file and folder under `path`, a few levels deep ([`walk`]).
    Files { path: String },
    /// Pluto notebooks under `path` ([`notebooks::scan`]).
    Notebooks { path: String },
    /// The first cells of the notebook at `path`.
    Preview { path: String },
    /// Slurm's partitions here, and `$SCRATCH`.
    Slurm,
    /// Whether a runtime (or its job) is there, found without taking it over.
    /// Only the helper knows its state folder, so it answers this one itself.
    Runtime,
    /// Where a file named `name` goes in `folder`'s `data/` ([`place`]).
    Place { folder: String, name: String, size: u64, sha256: String },
    /// A piece of a file being sent to `path` in `folder`, as `Place` answered
    /// ([`write`]). Pieces come in order; the last one puts the file in place.
    Write {
        folder: String,
        path: String,
        offset: u64,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        last: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Reply {
    /// `path` is the folder listed, absolute and with `~` expanded.
    List { path: PathBuf, entries: Vec<Entry> },
    /// Paths relative to the folder asked about; folders end with `/`.
    Files { paths: Vec<String> },
    Notebooks { found: Vec<Found> },
    Preview { preview: Preview },
    Slurm { scheduler: crate::slurm::Scheduler },
    Runtime { runtime: RuntimeState },
    /// `path` is relative to the folder; `have`: a file with the same contents
    /// is already there, so there's nothing to send.
    Place { path: String, have: bool },
    Written,
    Error { message: String },
}

/// What runs from a host's state folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "is")]
pub enum RuntimeState {
    NotRunning,
    /// `notebooks` is how many are open, when the helper could ask (not through
    /// a cluster job's node); `job` is the cluster job it runs in.
    Running { node: String, notebooks: Option<u32>, job: Option<crate::slurm::Job> },
    /// A cluster job for it waits in the queue (`state` PENDING and the like),
    /// or has a node (`state` RUNNING) and Julia is starting there.
    Queued { job: String, state: String, reason: String },
    /// Julia is starting here and has not recorded itself yet: no start is needed, and asking for one
    /// waits for it.
    Starting,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

pub fn answer(request: &Request) -> Reply {
    let result = match request {
        Request::List { path } => list(&expand(path)),
        Request::Files { path } => Ok(Reply::Files { paths: walk(&expand(path), WALK_DEPTH, WALK_LIMIT) }),
        Request::Notebooks { path } => Ok(Reply::Notebooks { found: notebooks::scan(&expand(path)) }),
        Request::Preview { path } => notebooks::read_preview(&expand(path)).map(|preview| Reply::Preview { preview }),
        Request::Slurm => crate::slurm::probe().map(|scheduler| Reply::Slurm { scheduler }),
        Request::Runtime => Err("Only the helper knows about its runtime.".into()),
        Request::Place { folder, name, size, sha256 } => place(&expand(folder), name, *size, sha256),
        Request::Write { folder, path, offset, bytes, last } => write(&expand(folder), path, *offset, bytes, *last).map(|()| Reply::Written),
    };
    result.unwrap_or_else(|message| Reply::Error { message })
}

/// This machine's home folder. `$HOME` on Unix; on Windows, the user profile
/// folder (not an env var: `HOME` is usually unset there).
pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `path` resolved through links, as Julia's `realpath` gives it: on Windows
/// as `C:\…`, where `fs::canonicalize` gives `\\?\C:\…`.
#[cfg(not(windows))]
pub fn real_path(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
}

#[cfg(windows)]
pub fn real_path(path: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// `~` and `~/…` as the home folder; anything relative is taken from there.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') || cfg!(windows) && rest.starts_with('\\') => home().join(&rest[1..]),
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
    let path = real_path(dir).unwrap_or_else(|_| dir.to_path_buf());
    Ok(Reply::List { path, entries })
}

const WALK_DEPTH: usize = 4;
const WALK_LIMIT: usize = 5000;

/// Files and folders under `root`, down to `depth` levels, hidden ones (and
/// what's inside them) left out, at most `limit` of them: shallower first, then
/// by name. Paths are relative to `root`; folders end with `/`.
pub fn walk(root: &Path, depth: usize, limit: usize) -> Vec<String> {
    let mut found = Vec::new();
    let mut level = vec![String::new()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for prefix in level {
            let Ok(read) = std::fs::read_dir(root.join(&prefix)) else { continue };
            let mut entries: Vec<(String, bool)> = read
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Links count as what they point to.
                    let dir = std::fs::metadata(entry.path()).ok()?.is_dir();
                    (!name.starts_with('.')).then_some((name, dir))
                })
                .collect();
            entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
            for (name, dir) in entries {
                if found.len() >= limit {
                    return found;
                }
                let path = format!("{prefix}{name}");
                if dir {
                    found.push(format!("{path}/"));
                    next.push(format!("{path}/"));
                } else {
                    found.push(path);
                }
            }
        }
        level = next;
    }
    found
}

/// Where attached files go, inside a session's folder.
pub const DATA: &str = "data";

/// The name for the `n`th file of this name: `decay.csv`, `decay (2).csv`, …
pub fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_string();
    }
    match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
        None => format!("{name} ({n})"),
    }
}

/// A file's SHA-256, in lowercase hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Where a file named `name` goes in `folder`'s `data/` (made if missing):
/// `data/<name>` if that's free or holds the same contents (size and SHA-256),
/// else the first numbered name that is.
pub fn place(folder: &Path, name: &str, size: u64, sha256: &str) -> Result<Reply, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(format!("{name:?} isn't a file name"));
    }
    if std::fs::symlink_metadata(folder.join(DATA)).is_err() {
        std::fs::create_dir(folder.join(DATA)).map_err(|e| format!("Couldn't make {}: {}", folder.join(DATA).display(), plain(&e)))?;
    }
    let data = inside(folder, Path::new(DATA))?;
    for n in 1.. {
        let candidate = numbered(name, n);
        let path = format!("{DATA}/{candidate}");
        match std::fs::symlink_metadata(data.join(&candidate)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Reply::Place { path, have: false }),
            Err(e) => return Err(format!("Couldn't look at {path}: {}", plain(&e))),
            Ok(meta) if meta.is_file() && meta.len() == size => {
                let theirs = sha256_file(&data.join(&candidate)).map_err(|e| format!("Couldn't read {path}: {}", plain(&e)))?;
                if theirs == sha256 {
                    return Ok(Reply::Place { path, have: true });
                }
            }
            // Different contents, a folder, or a link: taken.
            Ok(_) => {}
        }
    }
    unreachable!("some numbered name is free")
}

/// `rel`, a relative path without `..`, inside `folder` once links are
/// resolved (as the app's `relative_inside` checks); it must exist.
fn inside(folder: &Path, rel: &Path) -> Result<PathBuf, String> {
    let outside = || format!("{} isn't inside the session's folder", rel.display());
    if !rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(outside());
    }
    let root = folder.canonicalize().map_err(|e| format!("Couldn't open {}: {}", folder.display(), plain(&e)))?;
    let resolved = folder.join(rel).canonicalize().map_err(|e| format!("Couldn't open {}: {}", rel.display(), plain(&e)))?;
    if resolved.starts_with(&root) { Ok(resolved) } else { Err(outside()) }
}

/// Where a file sent to `path` (relative to `folder`, in a folder that
/// exists) is written while it comes, a hidden `.<name>.part` beside it, and
/// where it ends up.
pub fn upload_paths(folder: &Path, path: &str) -> Result<(PathBuf, PathBuf), String> {
    let rel = Path::new(path);
    let Some(Component::Normal(name)) = rel.components().next_back() else {
        return Err(format!("{path} isn't inside the session's folder"));
    };
    let name = name.to_string_lossy().into_owned();
    let dir = inside(folder, rel.parent().unwrap_or(Path::new("")))?;
    Ok((dir.join(format!(".{name}.part")), dir.join(name)))
}

/// Write a piece of a file being sent to `path` in `folder`: `bytes` at
/// `offset` of its part (`upload_paths`), which the first piece (offset 0)
/// starts over. The last piece renames the part to `path`, so a file cut short
/// never looks finished. A piece that fails deletes the part.
pub fn write(folder: &Path, path: &str, offset: u64, bytes: &[u8], last: bool) -> Result<(), String> {
    let (part, dest) = upload_paths(folder, path)?;
    let result = write_part(&part, &dest, path, offset, bytes, last);
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

fn write_part(part: &Path, dest: &Path, path: &str, offset: u64, bytes: &[u8], last: bool) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("Couldn't write {path}: {}", plain(&e));
    let mut file = if offset == 0 {
        match std::fs::remove_file(part) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(failed(e)),
            _ => {}
        }
        // create_new doesn't follow a link planted at the part's name.
        std::fs::OpenOptions::new().write(true).create_new(true).open(part).map_err(failed)?
    } else {
        if !std::fs::symlink_metadata(part).is_ok_and(|m| m.is_file() && m.len() == offset) {
            return Err(format!("Sending {path} was cut short; send it again"));
        }
        std::fs::OpenOptions::new().append(true).open(part).map_err(failed)?
    };
    file.write_all(bytes).map_err(failed)?;
    if last {
        file.sync_all().map_err(failed)?;
        if std::fs::symlink_metadata(dest).is_ok() {
            return Err(format!("{path} appeared while it was being sent; send it again"));
        }
        std::fs::rename(part, dest).map_err(failed)?;
    }
    Ok(())
}

/// Bytes as base64 in JSON.
mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        STANDARD.decode(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
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
        assert_eq!(path, real_path(&dir).unwrap());
        let names: Vec<(&str, bool)> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("A-figures", true), ("b-data", true), ("Analysis.jl", false), ("fit.jl", false)]);
        let missing = answer(&Request::List { path: dir.join("nope").display().to_string() });
        assert!(matches!(missing, Reply::Error { message } if message.ends_with("it doesn't exist")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn walks_files_shallow_first_skipping_hidden() {
        let dir = std::env::temp_dir().join(format!("endeavor-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["data/raw/deep/deeper", ".git/objects"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for file in ["fit.jl", "notes.txt", ".env", "data/decay.csv", "data/raw/run1.csv", "data/raw/deep/deeper/x.csv"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        assert_eq!(
            walk(&dir, 3, 100),
            ["data/", "fit.jl", "notes.txt", "data/decay.csv", "data/raw/", "data/raw/deep/", "data/raw/run1.csv"]
        );
        assert_eq!(walk(&dir, 3, 2), ["data/", "fit.jl"]);
        let Reply::Files { paths } = answer(&Request::Files { path: dir.display().to_string() }) else { panic!() };
        assert_eq!(paths.last().map(String::as_str), Some("data/raw/deep/deeper/"), "four levels down");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn session(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("endeavor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("session")).unwrap();
        dir
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Send `bytes` as the app does, in pieces of `piece` bytes.
    fn send(folder: &Path, name: &str, bytes: &[u8], piece: usize) -> Reply {
        let ask = |request| answer(&request);
        let folder = folder.display().to_string();
        let placed = ask(Request::Place { folder: folder.clone(), name: name.into(), size: bytes.len() as u64, sha256: sha(bytes) });
        let Reply::Place { path, have: false } = &placed else { return placed };
        let mut offset = 0;
        loop {
            let chunk = &bytes[offset..(offset + piece).min(bytes.len())];
            let last = chunk.len() < piece;
            let written = ask(Request::Write { folder: folder.clone(), path: path.clone(), offset: offset as u64, bytes: chunk.to_vec(), last });
            assert_eq!(written, Reply::Written);
            offset += chunk.len();
            if last {
                return placed;
            }
        }
    }

    #[test]
    fn sends_a_file_into_data_reusing_the_same_contents_and_numbering_others() {
        let dir = session("upload");
        let folder = dir.join("session");
        let place = |path: &str, have| Reply::Place { path: path.into(), have };
        assert_eq!(send(&folder, "decay.csv", b"t,y\n0,1\n1,0.5\n", 4), place("data/decay.csv", false));
        assert_eq!(std::fs::read(folder.join("data/decay.csv")).unwrap(), b"t,y\n0,1\n1,0.5\n");
        assert_eq!(send(&folder, "decay.csv", b"t,y\n0,1\n1,0.5\n", 4), place("data/decay.csv", true));
        assert_eq!(send(&folder, "decay.csv", b"t,y\n0,1\n1,0.4\n", 4), place("data/decay (2).csv", false));
        assert_eq!(send(&folder, "decay.csv", b"t,y\n0,1\n1,0.4\n", 4), place("data/decay (2).csv", true));
        assert_eq!(send(&folder, "decay.csv", b"other", 4), place("data/decay (3).csv", false));
        assert_eq!(send(&folder, "empty", b"", 4), place("data/empty", false));
        assert_eq!(send(&folder, "even.bin", &[7; 8], 4), place("data/even.bin", false), "a size that's a whole number of pieces");
        assert_eq!(std::fs::read(folder.join("data/even.bin")).unwrap(), [7; 8]);
        let mut names: Vec<String> = std::fs::read_dir(folder.join("data")).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["decay (2).csv", "decay (3).csv", "decay.csv", "empty", "even.bin"], "no parts left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn uploads_stay_inside_the_session_folder() {
        let dir = session("inside");
        let folder = dir.join("session");
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::create_dir_all(folder.join("data")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), folder.join("data/out")).unwrap();
        let refused = |path: &str| write(&folder, path, 0, b"x", true).unwrap_err();
        for path in ["../elsewhere/x", "data/../../elsewhere/x", "/tmp/x", "data/out/x", "data/..", "", "missing/x"] {
            let why = refused(path);
            assert!(why.contains("isn't inside the session's folder") || why.contains("doesn't exist"), "{path}: {why}");
        }
        assert_eq!(std::fs::read_dir(dir.join("elsewhere")).unwrap().count(), 0, "nothing written outside");
        for name in ["", ".", "..", "a/b"] {
            assert!(place(&folder, name, 1, &sha(b"x")).is_err(), "{name:?}");
        }

        // A data folder that is a link out of the session's folder is refused too.
        let linked = dir.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), linked.join("data")).unwrap();
        assert!(place(&linked, "x.csv", 1, &sha(b"x")).unwrap_err().contains("isn't inside"));
        // A link that stays inside is fine, as is the folder itself through /tmp → /private/tmp.
        std::fs::create_dir_all(folder.join("shared")).unwrap();
        std::os::unix::fs::symlink(folder.join("shared"), folder.join("data/shared")).unwrap();
        assert_eq!(write(&folder, "data/shared/y", 0, b"y", true), Ok(()));
        assert_eq!(std::fs::read(folder.join("shared/y")).unwrap(), b"y");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_send_cut_short_leaves_only_a_hidden_part_and_a_bad_piece_removes_it() {
        let dir = session("partial");
        let folder = dir.join("session");
        std::fs::create_dir_all(folder.join("data")).unwrap();
        let (part, dest) = upload_paths(&folder, "data/big.csv").unwrap();
        assert_eq!(part.file_name().unwrap(), ".big.csv.part");
        assert_eq!(write(&folder, "data/big.csv", 0, b"0123", false), Ok(()));
        assert_eq!(write(&folder, "data/big.csv", 4, b"4567", false), Ok(()));
        assert!(part.exists() && !dest.exists(), "unfinished: only the part");
        assert_eq!(walk(&folder, 2, 10), ["data/"], "the part is hidden from @ and the agent's listing");

        // A piece that doesn't follow on (one got lost) fails and discards the part.
        let why = write(&folder, "data/big.csv", 12, b"89", true).unwrap_err();
        assert_eq!(why, "Sending data/big.csv was cut short; send it again");
        assert!(!part.exists() && !dest.exists());

        // Sending again starts over at offset 0, over any stale part.
        std::fs::write(&part, "stale").unwrap();
        assert_eq!(write(&folder, "data/big.csv", 0, b"ab", false), Ok(()));
        assert_eq!(write(&folder, "data/big.csv", 2, b"cd", true), Ok(()));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "abcd");
        assert!(!part.exists());

        // A file that turned up at the name meanwhile isn't overwritten.
        assert_eq!(write(&folder, "data/big.csv", 0, b"new", true).unwrap_err(), "data/big.csv appeared while it was being sent; send it again");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "abcd");
        assert!(!part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn numbers_names_before_the_extension() {
        assert_eq!(numbered("decay.csv", 1), "decay.csv");
        assert_eq!(numbered("decay.csv", 2), "decay (2).csv");
        assert_eq!(numbered("run.tar.gz", 3), "run.tar (3).gz");
        assert_eq!(numbered("README", 2), "README (2)");
        assert_eq!(numbered(".env", 2), ".env (2)");
    }

    #[test]
    fn home_relative_paths() {
        assert_eq!(expand("~"), home());
        assert_eq!(expand("~/decay-fits"), home().join("decay-fits"));
        assert_eq!(expand("decay-fits"), home().join("decay-fits"));
        let absolute = if cfg!(windows) { r"D:\srv\data" } else { "/srv/data" };
        assert_eq!(expand(absolute), PathBuf::from(absolute));
        if cfg!(windows) {
            assert_eq!(expand(r"~\decay-fits"), home().join("decay-fits"));
        }
    }

    #[test]
    fn replies_are_tagged_json() {
        let json = serde_json::to_value(Request::Notebooks { path: "~/x".into() }).unwrap();
        assert_eq!(json, serde_json::json!({ "kind": "Notebooks", "path": "~/x" }));
        let reply = Reply::Error { message: "no".into() };
        assert_eq!(serde_json::from_value::<Reply>(serde_json::to_value(&reply).unwrap()).unwrap(), reply);
        let queued = Reply::Runtime { runtime: RuntimeState::Queued { job: "16".into(), state: "PENDING".into(), reason: "Resources".into() } };
        let json = serde_json::to_value(&queued).unwrap();
        assert_eq!((json["kind"].as_str(), json["runtime"]["is"].as_str()), (Some("Runtime"), Some("Queued")));
        assert_eq!(serde_json::from_value::<Reply>(json).unwrap(), queued);
        let starting = Reply::Runtime { runtime: RuntimeState::Starting };
        let json = serde_json::to_value(&starting).unwrap();
        assert_eq!(json["runtime"], serde_json::json!({ "is": "Starting" }));
        assert_eq!(serde_json::from_value::<Reply>(json).unwrap(), starting);
        let piece = Request::Write { folder: "~/s".into(), path: "data/a.csv".into(), offset: 0, bytes: b"t,y\n".to_vec(), last: true };
        let json = serde_json::to_value(&piece).unwrap();
        assert_eq!(json["bytes"], "dCx5Cg==", "bytes go as base64");
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), piece);
    }
}
