use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::{embedded, paths, standalone};

pub(crate) const KEEP_UNUSED: Duration = Duration::from_secs(7 * 24 * 3600);

pub(crate) const KEEP_PART: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Place {
    Server { base: PathBuf, own: String },
    Plugin { base: PathBuf, own: String },
}

impl Place {
    pub(crate) fn here(helper_args: &[&str]) -> Option<Place> {
        if !helper_args.is_empty() {
            return None;
        }
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        Place::of(&exe, &paths::Env::here().plugin_bin(), embedded::BUILD_VERSION)
    }

    pub(crate) fn of(exe: &Path, plugin_bin: &Path, build: &str) -> Option<Place> {
        let folder = exe.parent()?;
        let base = folder.parent()?.to_owned();
        let own = folder.file_name()?.to_str()?.to_owned();
        let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
        if real(&base) == real(plugin_bin) || base.parent().is_some_and(|b| real(b) == real(&paths::plugin_bin_from(plugin_bin))) {
            Some(Place::Plugin { base, own })
        } else if own == build && folder.join("runtime/boot.jl").is_file() {
            Some(Place::Server { base, own })
        } else {
            None
        }
    }

    fn base(&self) -> &Path {
        match self {
            Place::Server { base, .. } | Place::Plugin { base, .. } => base,
        }
    }

    fn own(&self) -> &str {
        match self {
            Place::Server { own, .. } | Place::Plugin { own, .. } => own,
        }
    }

    fn is_build(&self, folder: &Path) -> bool {
        let binary = ["endeavor", "endeavor.exe"].iter().any(|name| folder.join(name).is_file());
        match self {
            Place::Server { .. } => binary && folder.join("runtime/boot.jl").is_file(),
            Place::Plugin { .. } => binary,
        }
    }

    fn kept(&self) -> Vec<String> {
        match self {
            Place::Server { .. } => Vec::new(),
            Place::Plugin { base, .. } => std::fs::read_to_string(base.join(".newest")).ok().and_then(|t| Some(t.lines().next()?.trim().to_owned())).into_iter().collect(),
        }
    }
}

static OWN: OnceLock<Option<standalone::Lease>> = OnceLock::new();

const LOCK_WAIT: Duration = Duration::from_secs(5);

pub(crate) fn hold_own_folder(place: Option<&Place>) {
    OWN.get_or_init(|| {
        let folder = place?.base().join(place?.own());
        let path = folder.join(standalone::IN_USE);
        let marker = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).ok()?;
        let started = std::time::Instant::now();
        loop {
            match marker.try_lock_shared() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        if !same_file(&marker, &path) {
            eprintln!("endeavor: this build ({}) was being removed as it started, as one unused for a week. Run it again, and it is installed again.", folder.display());
            std::process::exit(1);
        }
        Some(standalone::lease_locked(marker))
    });
}

#[cfg(unix)]
fn same_file(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

#[cfg(windows)]
fn same_file(_file: &std::fs::File, _path: &Path) -> bool {
    true
}

pub(crate) fn tidy(place: Option<Place>) {
    let Some(place) = place else { return };
    std::thread::spawn(move || {
        let server = matches!(place, Place::Server { .. });
        remove_unused(&place, SystemTime::now(), &|| in_use_texts(server, place.base(), &record_folders(place.base(), &paths::Env::here())));
    });
}

pub(crate) fn remove_unused(place: &Place, now: SystemTime, in_use: &dyn Fn() -> Option<Vec<String>>) -> Vec<String> {
    let base = place.base();
    let Ok(entries) = std::fs::read_dir(base) else { return Vec::new() };
    let kept = place.kept();
    let age = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok().map(|t| now.duration_since(t).unwrap_or_default());
    let mut texts: Option<Option<Vec<String>>> = None;
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) || name.starts_with('.') {
            continue;
        }
        if name.contains(".removing.") {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        if let Some((build, _)) = name.split_once(".part.") {
            if age(&path).is_some_and(|age| age > KEEP_PART) && !build.is_empty() {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if name == place.own() || kept.contains(&name) || !place.is_build(&path) {
            continue;
        }
        let mut marker = std::fs::OpenOptions::new().read(true).write(true).open(path.join(standalone::IN_USE)).ok();
        let used = match &marker {
            Some(marker) => marker.metadata().and_then(|m| m.modified()).ok().map(|t| now.duration_since(t).unwrap_or_default()),
            None => age(&path),
        };
        if used.is_none_or(|used| used < KEEP_UNUSED) || marker.as_ref().is_some_and(|m| m.try_lock().is_err()) {
            continue;
        }
        let Some(texts) = texts.get_or_insert_with(in_use).as_ref() else { return removed };
        if names(texts, &path) {
            continue;
        }
        let trash = base.join(format!("{name}.removing.{}", std::process::id()));
        if cfg!(windows) {
            marker = None;
        }
        if std::fs::rename(&path, &trash).is_ok() {
            drop(marker);
            let _ = std::fs::remove_dir_all(&trash);
            removed.push(name);
        }
    }
    removed
}

fn names(texts: &[String], folder: &Path) -> bool {
    let folder = folder.display().to_string();
    let real = Path::new(&folder).canonicalize().map(|p| p.display().to_string()).unwrap_or_else(|_| folder.clone());
    texts.iter().any(|text| {
        [&folder, &real].iter().any(|f| {
            text.match_indices(f.as_str()).any(|(at, _)| matches!(text[at + f.len()..].chars().next(), None | Some('/' | '\\' | '\'' | '"' | ' ' | '\0' | '\n')))
        })
    })
}

fn in_use_texts(server: bool, base: &Path, folders: &[PathBuf]) -> Option<Vec<String>> {
    let mut texts = processes()?;
    if server {
        texts.extend(recorded(folders, base, &crate::hostname())?);
        texts.extend(job_scripts()?);
    }
    Some(texts)
}

pub(crate) fn record_folders(base: &Path, env: &paths::Env) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(base).into_iter().flatten().flatten().map(|e| e.path()).collect();
    if let Some(nodes) = env.state_dir().parent() {
        folders.extend(std::fs::read_dir(nodes).into_iter().flatten().flatten().map(|e| e.path()));
    }
    folders.push(env.cluster_state_dir());
    folders
}

pub(crate) fn recorded(folders: &[PathBuf], base: &Path, host: &str) -> Option<Vec<String>> {
    let mut texts = Vec::new();
    for folder in folders {
        let Some(state) = crate::read_state(folder) else { continue };
        if state.node == host && !crate::pid_alive(state.pid, state.started, state.boot.as_deref()) {
            continue;
        }
        texts.push(base.join(state.build?).display().to_string());
    }
    Some(texts)
}

#[cfg(target_os = "linux")]
pub(crate) fn processes() -> Option<Vec<String>> {
    let mut texts = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
            texts.push(exe.display().to_string());
        }
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            texts.push(String::from_utf8_lossy(&cmdline).replace('\0', "\n"));
        }
    }
    Some(texts)
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn processes() -> Option<Vec<String>> {
    let output = std::process::Command::new("ps").args(["-A", "-ww", "-o", "args="]).stdin(std::process::Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

#[cfg(windows)]
pub(crate) fn processes() -> Option<Vec<String>> {
    Some(Vec::new())
}

fn job_scripts() -> Option<Vec<String>> {
    if !wire::slurm::has("squeue") {
        return Some(Vec::new());
    }
    let off_path = wire::slurm::FOLDERS.iter().map(|d| Path::new(d).join("squeue")).find(|p| p.is_file());
    let on_path = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("squeue").is_file()));
    let squeue = if on_path { PathBuf::from("squeue") } else { off_path? };
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty())?;
    let output = std::process::Command::new(squeue)
        .args(["-h", "-n", "endeavor", "-t", "PENDING,RUNNING,CONFIGURING", "-o", "%o", "-u", &user])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).lines().map(str::trim).filter(|l| !l.is_empty()).map(|script| std::fs::read_to_string(script).ok()).collect()
}

#[cfg(test)]
mod tests;
