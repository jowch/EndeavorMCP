//! Removing other builds that nothing uses any more (#97), from the two folders that keep one per
//! build: a server's `~/.cache/endeavor/<build>/` (or the app's install folder there), which the
//! bootstrap script installs into, and the plugin's `bin/<key>/`, which its launcher downloads into.
//!
//! A process run from one of those folders holds a `standalone::Lease` on it for as long as it
//! runs (`hold_own_folder`), which also marks the folder as used every hour. `connect`, `mcp` and
//! `serve` then look at the other builds beside their own, in the background (`tidy`), and remove
//! one only when all of these hold:
//! - no one holds a lease on it, and it hasn't been used for `KEEP_UNUSED` (a build from before
//!   leases counts from when it was installed);
//! - no process here runs from it, or names a file in it (Julia's `--project`);
//! - on a server, no runtime record names it (`runtime.json`, in the state folders `record_folders` lists): a
//!   runtime on another login node, or in a job, runs from the build that started it. A record from
//!   this computer whose process is gone doesn't count, and one from before builds were recorded keeps
//!   them all;
//! - on a server, no Slurm job of the user's that is waiting or running starts from it (the job
//!   script names the build's binary). A server where `squeue` doesn't answer is left alone.
//!
//! The time covers what this computer can't see: a cluster's other login nodes, whose processes
//! and locks don't show here, keep marking a folder they use. A folder is moved aside before it is
//! removed, so a removal cut short leaves `<name>.removing.<pid>`, which the next look removes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use crate::{embedded, paths, standalone};

/// How long a build nothing holds is kept after its last use.
pub(crate) const KEEP_UNUSED: Duration = Duration::from_secs(7 * 24 * 3600);

/// How long a folder an install left half-written (`<name>.part.<pid>`) is kept: its install may
/// still be going on.
const KEEP_PART: Duration = Duration::from_secs(24 * 3600);

/// Which of the two folders a binary runs from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Place {
    /// `<root>/<build>/endeavor` beside `<root>/<build>/runtime`, as the bootstrap script installs it.
    Server { base: PathBuf, own: String },
    /// `<plugin bin>/<key>/endeavor`, or the same under `bin-from/<release>/`.
    Plugin { base: PathBuf, own: String },
}

impl Place {
    /// Where this binary runs from, if it is one of the two. A binary built into the app
    /// (`helper_args`) is neither.
    pub(crate) fn here(helper_args: &[&str]) -> Option<Place> {
        if !helper_args.is_empty() {
            return None;
        }
        let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
        Place::of(&exe, &paths::Env::here().plugin_bin(), embedded::BUILD_VERSION)
    }

    /// `of` for a binary at `exe`, the plugin's folder being `plugin_bin` and this build `build`.
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

    /// Whether `folder`, beside this binary's, holds a whole build of this kind.
    fn is_build(&self, folder: &Path) -> bool {
        let binary = ["endeavor", "endeavor.exe"].iter().any(|name| folder.join(name).is_file());
        match self {
            Place::Server { .. } => binary && folder.join("runtime/boot.jl").is_file(),
            Place::Plugin { .. } => binary,
        }
    }

    /// The other builds that are kept whatever their use: for the plugin, the newest one the
    /// launcher fetched (`.newest`), which an unpinned start runs.
    fn kept(&self) -> Vec<String> {
        match self {
            Place::Server { .. } => Vec::new(),
            Place::Plugin { base, .. } => std::fs::read_to_string(base.join(".newest")).ok().and_then(|t| Some(t.lines().next()?.trim().to_owned())).into_iter().collect(),
        }
    }
}

static OWN: OnceLock<Option<standalone::Lease>> = OnceLock::new();

/// Hold a lease on the folder this binary runs from, for as long as it runs, if it is one of the
/// two kinds (`Place`).
pub(crate) fn hold_own_folder(place: Option<&Place>) {
    OWN.get_or_init(|| {
        let folder = place?.base().join(place?.own());
        let marker = folder.join(standalone::IN_USE);
        if !marker.exists() {
            std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&marker).ok()?;
        }
        standalone::lease(&folder)
    });
}

/// Look for other builds to remove, in a thread of its own, so that nothing waits for it.
pub(crate) fn tidy(place: Option<Place>) {
    let Some(place) = place else { return };
    std::thread::spawn(move || {
        let server = matches!(place, Place::Server { .. });
        remove_unused(&place, SystemTime::now(), &|| in_use_texts(server, place.base(), &record_folders(place.base(), &paths::Env::here())));
    });
}

/// Remove the builds beside `place`'s own that nothing uses (see the module's notes), as of `now`.
/// `in_use` gives what processes and jobs name (their command lines, binaries and job scripts); a
/// build is in use if any of them names a path in it. It is asked only when a build is old enough
/// to go, and None (it can't tell) keeps them all. The names removed.
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
            // A half-written install of a build, left by one that was cut short.
            if age(&path).is_some_and(|age| age > KEEP_PART) && !build.is_empty() {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if name == place.own() || kept.contains(&name) || !place.is_build(&path) {
            continue;
        }
        let marker = std::fs::OpenOptions::new().read(true).write(true).open(path.join(standalone::IN_USE)).ok();
        // Without a marker the build is from before leases, and its folder's time is when it was installed.
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
        // The lock, if taken, is held until the folder has moved, so a process starting from it meanwhile isn't told it may.
        if std::fs::rename(&path, &trash).is_ok() {
            drop(marker);
            let _ = std::fs::remove_dir_all(&trash);
            removed.push(name);
        }
    }
    removed
}

/// Whether any of `texts` names `folder` or a path in it.
fn names(texts: &[String], folder: &Path) -> bool {
    let folder = folder.display().to_string();
    let real = Path::new(&folder).canonicalize().map(|p| p.display().to_string()).unwrap_or_else(|_| folder.clone());
    texts.iter().any(|text| {
        [&folder, &real].iter().any(|f| {
            text.match_indices(f.as_str()).any(|(at, _)| matches!(text[at + f.len()..].chars().next(), None | Some('/' | '\\' | '\'' | '"' | ' ' | '\0' | '\n')))
        })
    })
}

/// What the processes here name, and on a server the runtimes recorded in `folders` and the user's
/// Slurm jobs; None if that can't be told. (The plugin's folders are named by release key, not by
/// the build a record names, and its runtimes run on this computer, where their processes show.)
fn in_use_texts(server: bool, base: &Path, folders: &[PathBuf]) -> Option<Vec<String>> {
    let mut texts = processes()?;
    if server {
        texts.extend(recorded(folders, base, &crate::hostname())?);
        texts.extend(job_scripts()?);
    }
    Some(texts)
}

/// The state folders a runtime from a build in `base` may be recorded in: each folder in `base` (the
/// app names a server's state folder there), each login node's `serve` folder, and the cluster's.
/// A state folder given as a path outside these isn't looked at.
pub(crate) fn record_folders(base: &Path, env: &paths::Env) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(base).into_iter().flatten().flatten().map(|e| e.path()).collect();
    if let Some(nodes) = env.state_dir().parent() {
        folders.extend(std::fs::read_dir(nodes).into_iter().flatten().flatten().map(|e| e.path()));
    }
    folders.push(env.cluster_state_dir());
    folders
}

/// The build folders in `base` that the runtimes recorded in `folders` run from; None if one of them
/// may be running and its record doesn't say which build started it. A record from `host` whose
/// process is gone is left out.
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

/// The command line of each process here, with its binary on Linux.
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

/// The command line of each process here.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn processes() -> Option<Vec<String>> {
    let output = std::process::Command::new("ps").args(["-A", "-ww", "-o", "args="]).stdin(std::process::Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

/// None here: on Windows a running binary can't be moved or removed, so `remove_unused` can't take
/// it from a process that runs it, and the leases cover the rest.
#[cfg(windows)]
pub(crate) fn processes() -> Option<Vec<String>> {
    Some(Vec::new())
}

/// The scripts of the user's Slurm jobs named endeavor that wait or run: what they start. Empty
/// where there's no Slurm; None if `squeue` doesn't answer or a script can't be read.
fn job_scripts() -> Option<Vec<String>> {
    if !wire::slurm::has("squeue") {
        return Some(Vec::new());
    }
    // Where `wire::slurm::has` found it: on the PATH, else in one of its folders.
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
