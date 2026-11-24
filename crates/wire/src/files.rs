use std::io::{Read, Write as _};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::Backend;
use crate::notebooks::{self, Found, Preview};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Request {
    List { path: String },
    Files { path: String },
    Notebooks { path: String },
    Preview { path: String },
    Slurm,
    Julia { download: bool },
    Runtime,
    Folder { path: String },
    Place { folder: String, name: String, size: u64, sha256: String },
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
    List { path: String, entries: Vec<Entry> },
    Files { paths: Vec<String> },
    Notebooks { found: Vec<Found> },
    Preview { preview: Preview },
    Slurm { scheduler: crate::slurm::Scheduler },
    Runtime { runtime: RuntimeState },
    Folder { path: String, created: bool },
    NoFolder { path: String, parent: String },
    Place { path: String, have: bool },
    Written,
    Julia { julia: JuliaState },
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "is", rename_all = "snake_case")]
pub enum JuliaState {
    Ready { path: String, version: String },
    Downloading { step: String },
    Missing { item: crate::Item },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "is")]
pub enum RuntimeState {
    NotRunning,
    Running { node: String, notebooks: Option<u32>, job: Option<crate::slurm::Job> },
    Queued { job: String, state: String, reason: String },
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
        Request::Notebooks { path } => Ok(Reply::Notebooks { found: notebooks::scan(&expand(path), &Backend::ALL) }),
        Request::Preview { path } => notebooks::read_preview(&expand(path)).map(|preview| Reply::Preview { preview }),
        Request::Slurm => crate::slurm::probe().map(|scheduler| Reply::Slurm { scheduler }),
        Request::Runtime => Err("Only the helper knows about its runtime.".into()),
        Request::Julia { .. } => Err("Only the helper knows its Julia.".into()),
        Request::Folder { path } if other_users_home(path) => Err(format!("{path} names another user's home folder, which isn't supported here. Give the full path.")),
        Request::Folder { path } => folder(&expand(path)),
        Request::Place { folder, name, size, sha256 } => place(&expand(folder), name, *size, sha256),
        Request::Write { folder, path, offset, bytes, last } => write(&expand(folder), path, *offset, bytes, *last).map(|()| Reply::Written),
    };
    result.unwrap_or_else(|message| Reply::Error { message })
}

pub fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(not(windows))]
pub fn real_path(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
}

#[cfg(windows)]
pub fn real_path(path: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path)
}

pub fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with('/') || cfg!(windows) && rest.starts_with('\\') => home().join(&rest[1..]),
        _ => home().join(path),
    }
}

fn other_users_home(path: &str) -> bool {
    path.strip_prefix('~').is_some_and(|rest| !(rest.is_empty() || rest.starts_with('/') || cfg!(windows) && rest.starts_with('\\')))
}

fn folder(dir: &Path) -> Result<Reply, String> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => return Ok(Reply::Folder { path: text(dir), created: false }),
        Ok(_) => return Err(format!("{} is a file, not a folder.", dir.display())),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("Couldn't open {}: {}", dir.display(), plain(&e))),
        Err(_) => {}
    }
    let parent = dir.parent().unwrap_or(dir);
    if !parent.is_dir() {
        return Ok(Reply::NoFolder { path: text(dir), parent: text(parent) });
    }
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(Reply::Folder { path: text(dir), created: true }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(Reply::Folder { path: text(dir), created: false }),
        Err(e) => Err(format!("Couldn't make the folder {}: {}", dir.display(), plain(&e))),
    }
}

fn list(dir: &Path) -> Result<Reply, String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("Couldn't open {}: {}", dir.display(), plain(&e)))?;
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = std::fs::metadata(entry.path()).ok()?.is_dir();
            let notebook = || name.ends_with(".jl") || crate::backend::Backend::of_file(&entry.path()).is_some();
            (!name.starts_with('.') && (dir || notebook())).then_some(Entry { name, dir })
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let path = real_path(dir).unwrap_or_else(|_| dir.to_path_buf());
    Ok(Reply::List { path: text(&path), entries })
}

const WALK_DEPTH: usize = 4;
const WALK_LIMIT: usize = 5000;

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

pub const DATA: &str = "data";

pub fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_string();
    }
    match name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()) {
        Some((stem, ext)) => format!("{stem} ({n}).{ext}"),
        None => format!("{name} ({n})"),
    }
}

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

pub fn place(folder: &Path, name: &str, size: u64, sha256: &str) -> Result<Reply, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(format!("{name:?} isn't a file name"));
    }
    if cfg!(windows) && !windows_name(name) {
        return Err(not_on_windows(name));
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
            Ok(_) => {}
        }
    }
    unreachable!("some numbered name is free")
}

fn windows_name(name: &str) -> bool {
    const DEVICES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let device = DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d))
        || stem.len() == 4 && ["COM", "LPT"].iter().any(|d| stem.as_bytes()[..3].eq_ignore_ascii_case(d.as_bytes())) && matches!(stem.as_bytes()[3], b'1'..=b'9');
    !device && !name.ends_with(['.', ' ']) && !name.chars().any(|c| c < ' ' || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
}

fn not_on_windows(name: &str) -> String {
    format!("{name:?} can't be a file's name on Windows; rename it and attach it again")
}

fn inside(folder: &Path, rel: &Path) -> Result<PathBuf, String> {
    let outside = || format!("{} isn't inside the session's folder", rel.display());
    if !rel.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)) {
        return Err(outside());
    }
    let root = folder.canonicalize().map_err(|e| format!("Couldn't open {}: {}", folder.display(), plain(&e)))?;
    let resolved = folder.join(rel).canonicalize().map_err(|e| format!("Couldn't open {}: {}", rel.display(), plain(&e)))?;
    if resolved.starts_with(&root) { Ok(resolved) } else { Err(outside()) }
}

pub fn upload_paths(folder: &Path, path: &str) -> Result<(PathBuf, PathBuf), String> {
    let rel = Path::new(path);
    let Some(Component::Normal(name)) = rel.components().next_back() else {
        return Err(format!("{path} isn't inside the session's folder"));
    };
    let name = name.to_string_lossy().into_owned();
    if cfg!(windows) && !windows_name(&name) {
        return Err(not_on_windows(&name));
    }
    let dir = inside(folder, rel.parent().unwrap_or(Path::new("")))?;
    Ok((dir.join(format!(".{name}.part")), dir.join(name)))
}

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
    fn a_folder_is_used_made_when_only_it_is_missing_and_else_refused() {
        let dir = std::env::temp_dir().join(format!("endeavor-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(folder(&dir), Ok(Reply::Folder { path: text(&dir), created: false }));
        let new = dir.join("new-study");
        assert_eq!(folder(&new), Ok(Reply::Folder { path: text(&new), created: true }));
        assert!(new.is_dir());
        assert_eq!(folder(&new), Ok(Reply::Folder { path: text(&new), created: false }), "made once");
        let deeper = dir.join("missing").join("study");
        assert_eq!(folder(&deeper), Ok(Reply::NoFolder { path: text(&deeper), parent: text(&dir.join("missing")) }));
        assert!(!dir.join("missing").exists(), "nothing is made when the parent is missing");
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        assert!(folder(&dir.join("notes.txt")).unwrap_err().contains("is a file"));
        assert!(matches!(answer(&Request::Folder { path: "~".into() }), Reply::Folder { created: false, .. }));
        assert!(matches!(answer(&Request::Folder { path: "~bob/study".into() }), Reply::Error { message } if message.contains("another user's home")));
        assert!(!home().join("~bob").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_path_on_windows_has_no_verbatim_prefix_as_julias_realpath_has_none() {
        let path = text(&real_path(&std::env::temp_dir()).unwrap());
        assert!(!path.starts_with(r"\\?\"), "{path}");
        assert_eq!(path.chars().nth(1), Some(':'), "{path}");
    }

    #[test]
    fn lists_folders_first_then_julia_files_and_r_and_python_notebooks() {
        let dir = std::env::temp_dir().join(format!("endeavor-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["b-data", "A-figures", ".git"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        for file in ["fit.jl", "notes.txt", ".hidden.jl", "Analysis.jl"] {
            std::fs::write(dir.join(file), "").unwrap();
        }
        std::fs::write(dir.join("growth.py"), "import marimo\n\napp = marimo.App()\n").unwrap();
        std::fs::write(dir.join("script.py"), "print(1)\n").unwrap();
        std::fs::write(dir.join("model.R"), "### An Ember notebook ###\n").unwrap();
        std::fs::write(dir.join("plain.R"), "x <- 1\n").unwrap();
        let Reply::List { path, entries } = answer(&Request::List { path: dir.display().to_string() }) else { panic!() };
        assert_eq!(path, text(&real_path(&dir).unwrap()));
        let names: Vec<(&str, bool)> = entries.iter().map(|e| (e.name.as_str(), e.dir)).collect();
        assert_eq!(names, [("A-figures", true), ("b-data", true), ("Analysis.jl", false), ("fit.jl", false), ("growth.py", false), ("model.R", false)]);
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

        let linked = dir.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), linked.join("data")).unwrap();
        assert!(place(&linked, "x.csv", 1, &sha(b"x")).unwrap_err().contains("isn't inside"));
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

        let why = write(&folder, "data/big.csv", 12, b"89", true).unwrap_err();
        assert_eq!(why, "Sending data/big.csv was cut short; send it again");
        assert!(!part.exists() && !dest.exists());

        std::fs::write(&part, "stale").unwrap();
        assert_eq!(write(&folder, "data/big.csv", 0, b"ab", false), Ok(()));
        assert_eq!(write(&folder, "data/big.csv", 2, b"cd", true), Ok(()));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "abcd");
        assert!(!part.exists());

        assert_eq!(write(&folder, "data/big.csv", 0, b"new", true).unwrap_err(), "data/big.csv appeared while it was being sent; send it again");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "abcd");
        assert!(!part.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_windows_cant_hold() {
        for name in ["run:1.csv", "a\\b.csv", "CON", "con.txt", "Nul", "COM1.csv", "lpt9", "notes.", "notes ", "a*b", "a\u{1}b"] {
            assert!(!windows_name(name), "{name:?}");
        }
        for name in ["run 1.csv", "decay.csv", "console.txt", "COM0.csv", "COM10", "LPT", ".env", "über.csv", "a€.csv", "😀.png", "a中.txt"] {
            assert!(windows_name(name), "{name:?}");
        }
        let dir = session("windows-names");
        let placed = place(&dir, "run:1.csv", 1, &sha(b"x"));
        let sent = write(&dir, "data/run:1.csv", 0, b"x", true);
        if cfg!(windows) {
            assert!(placed.unwrap_err().contains("on Windows"));
            assert!(sent.unwrap_err().contains("on Windows"));
            assert!(std::fs::read_dir(dir.join("data")).map_or(true, |mut d| d.next().is_none()), "nothing written");
        } else {
            assert_eq!(placed, Ok(Reply::Place { path: "data/run:1.csv".into(), have: false }));
            assert_eq!(sent, Ok(()));
        }
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
