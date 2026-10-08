//! `endeavor status`: what Endeavor has on this computer, for a user or whoever helps them. It
//! only reads. It makes no folder or file and starts and stops nothing. It asks the runtime
//! recorded in the state folder one local ping, to know whether it answers; there is no ssh and
//! no other network request. It never prints the runtime's token.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::projects::Projects;
use crate::client::MachinesFile;
use crate::paths::Env;
use crate::runtime::{self, Looked};
use crate::{embedded, hostname};

/// Folders with more entries than this are not sized: a size that needs a deep walk is skipped.
const SIZE_LIMIT: usize = 20_000;

#[derive(Serialize)]
struct Report {
    version: &'static str,
    build: &'static str,
    /// The release this build was published as; none for a build that was not.
    release: Option<&'static str>,
    program: Option<PathBuf>,
    runtime: RuntimeReport,
    /// The cluster state folder, only when it holds a record.
    cluster: Option<ClusterReport>,
    machines: MachinesReport,
    projects: ProjectsReport,
    folders: Folders,
}

#[derive(Serialize)]
struct RuntimeReport {
    state_dir: PathBuf,
    /// `not_running`, `starting`, `running`, `stale` (recorded, its process gone) or `other_computer`
    /// (recorded by another computer that shares the home folder, and not asked).
    state: &'static str,
    pid: Option<i32>,
    port: Option<u16>,
    /// The computer that recorded it.
    node: Option<String>,
    /// The build that started it.
    build: Option<String>,
    /// Whether that is not this build.
    other_build: Option<bool>,
    folder: Option<String>,
    exits_when_idle: Option<bool>,
    /// Whether it answered a ping; none when it was not asked.
    answers: Option<bool>,
    /// `runtime.log`, if there is one.
    log: Option<PathBuf>,
}

#[derive(Serialize)]
struct ClusterReport {
    state_dir: PathBuf,
    files: Vec<&'static str>,
}

#[derive(Serialize)]
struct MachinesReport {
    path: PathBuf,
    exists: bool,
    /// Why nothing is listed from a file that is there.
    error: Option<String>,
    machines: Vec<MachineReport>,
}

#[derive(Serialize)]
struct MachineReport {
    id: String,
    name: String,
    host: String,
    /// Julia runs in Slurm jobs.
    slurm: bool,
}

#[derive(Serialize)]
struct ProjectsReport {
    path: PathBuf,
    count: Option<usize>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Folders {
    plugin_binaries: PluginBinaries,
    helpers: Folder,
    serve_runtime: Folder,
    /// Named only: what the app and servers install into is never listed or sized.
    server_root: Folder,
    /// Named only: it can be many GB.
    depot: Folder,
}

#[derive(Serialize)]
struct Folder {
    path: PathBuf,
    exists: bool,
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
struct PluginBinaries {
    #[serde(flatten)]
    folder: Folder,
    builds: Vec<String>,
}

pub(super) fn main(env: &Env, state_dir: &Path, json: bool) -> ! {
    let report = report(env, state_dir);
    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
    } else {
        print!("{}", text(&report));
    }
    std::process::exit(0)
}

fn report(env: &Env, state_dir: &Path) -> Report {
    Report {
        version: env!("CARGO_PKG_VERSION"),
        build: embedded::BUILD_VERSION,
        release: embedded::RELEASE_KEY,
        program: std::env::current_exe().ok(),
        runtime: runtime_report(state_dir),
        cluster: cluster_report(&env.cluster_state_dir()),
        machines: machines_report(&MachinesFile::at(env.machines_file())),
        projects: projects_report(&env.projects_path()),
        folders: Folders {
            plugin_binaries: plugin_binaries(&env.plugin_bin()),
            helpers: folder(env.helpers_dir(), true),
            serve_runtime: folder(env.cache(), true),
            server_root: folder(env.server_root(), false),
            depot: folder(PathBuf::from(env.depot().trim_end_matches([':', ';'])), false),
        },
    }
}

fn runtime_report(dir: &Path) -> RuntimeReport {
    let log = dir.join("runtime.log");
    let mut report = RuntimeReport {
        state_dir: dir.to_owned(),
        state: "not_running",
        pid: None,
        port: None,
        node: None,
        build: None,
        other_build: None,
        folder: None,
        exits_when_idle: None,
        answers: None,
        log: log.is_file().then_some(log),
    };
    let starting = runtime::starting(dir);
    let (state, recorded) = match runtime::look(dir, false, true) {
        Looked::NotRunning => (if starting { "starting" } else { "not_running" }, None),
        Looked::Dead(state) => (if starting { "starting" } else { "stale" }, Some(state)),
        Looked::OtherNode(state) => ("other_computer", Some(state)),
        Looked::Running(state, _) => {
            report.answers = Some(true);
            ("running", Some(state))
        }
        Looked::Silent(state) => {
            report.answers = Some(false);
            ("running", Some(state))
        }
        Looked::Older(state) => ("running", Some(state)),
    };
    report.state = state;
    if let Some(recorded) = recorded {
        report.pid = Some(recorded.pid);
        report.port = recorded.port;
        report.node = Some(recorded.node);
        if state == "running" {
            report.other_build = Some(recorded.build.as_deref() != Some(embedded::BUILD_VERSION));
            report.build = recorded.build;
            report.folder = recorded.folder;
            report.exits_when_idle = recorded.exits_when_idle;
        }
    }
    report
}

fn cluster_report(dir: &Path) -> Option<ClusterReport> {
    let files: Vec<&'static str> = ["runtime.json", "job.json"].into_iter().filter(|name| dir.join(name).is_file()).collect();
    (!files.is_empty()).then(|| ClusterReport { state_dir: dir.to_owned(), files })
}

fn machines_report(file: &MachinesFile) -> MachinesReport {
    let exists = file.path().exists();
    let (machines, error) = match file.load_known() {
        Ok(servers) => (servers.iter().map(|s| MachineReport { id: s.id.clone(), name: s.display_name(), host: s.ssh_target(), slurm: s.cluster.is_some() }).collect(), None),
        Err(e) => (Vec::new(), Some(e)),
    };
    MachinesReport { path: file.path().to_owned(), exists, error, machines }
}

fn projects_report(path: &Path) -> ProjectsReport {
    let (count, error) = match Projects::at(path).count() {
        Ok(count) => (Some(count), None),
        Err(e) => (None, Some(e)),
    };
    ProjectsReport { path: path.to_owned(), count, error }
}

fn folder(path: PathBuf, size: bool) -> Folder {
    let exists = path.exists();
    let size_bytes = if size && exists { size_of(&path) } else { None };
    Folder { path, exists, size_bytes }
}

/// The launcher keeps each build in a folder of its own.
fn plugin_binaries(path: &Path) -> PluginBinaries {
    let mut builds: Vec<String> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    builds.sort();
    PluginBinaries { folder: folder(path.to_owned(), true), builds }
}

/// The size of the files under `path`, links not followed; none if there are more than `SIZE_LIMIT` entries or one can't be read.
fn size_of(path: &Path) -> Option<u64> {
    let (mut total, mut entries, mut todo) = (0, 0, vec![path.to_owned()]);
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).ok()? {
            let entry = entry.ok()?;
            let meta = entry.metadata().ok()?;
            entries += 1;
            if entries > SIZE_LIMIT {
                return None;
            }
            if meta.is_dir() {
                todo.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Some(total)
}

fn size_text(bytes: u64) -> String {
    const MB: u64 = 1 << 20;
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= MB => format!("{:.1} MB", b as f64 / MB as f64),
        b if b >= 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

fn text(report: &Report) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(format!("endeavor {} (build {})", report.version, report.build));
    if let Some(release) = report.release {
        line(format!("Release: {release}"));
    }
    line(format!("Program: {}", report.program.as_ref().map_or("unknown".into(), |p| p.display().to_string())));

    let runtime = &report.runtime;
    line(String::new());
    line("Runtime on this computer".into());
    line(format!("  State folder: {}", runtime.state_dir.display()));
    let pid = runtime.pid.map(|pid| format!("pid {pid}")).unwrap_or_default();
    match runtime.state {
        "starting" => line("  Starting: a runtime is being started and has not recorded itself yet.".into()),
        "running" => {
            let port = runtime.port.map_or("no port recorded (an older build)".into(), |port| format!("port {port}"));
            line(format!("  Running: {pid}, {port}"));
            match (&runtime.build, runtime.other_build) {
                (Some(build), Some(true)) => line(format!("  Started from build {build}, not this build. To use this build, run `endeavor stop`, then start it again.")),
                (None, _) => line("  Started from an earlier build, not this build. To use this build, run `endeavor stop`, then start it again.".into()),
                _ => line("  Started from this build.".into()),
            }
            if let Some(folder) = &runtime.folder {
                line(format!("  Notebooks folder: {folder}"));
            }
            let idle = runtime.exits_when_idle.map_or("not recorded", |idle| if idle { "yes" } else { "no" });
            line(format!("  Ends itself when idle: {idle}"));
            let answers = runtime.answers.map_or("not asked (no port)", |answers| if answers { "yes" } else { "no, though its process is alive" });
            line(format!("  Answers: {answers}"));
        }
        "stale" => line(format!("  Recorded, but its process is gone ({pid}). `endeavor stop` removes the record.")),
        "other_computer" => line(format!("  Recorded by {}, not this computer ({}), so not checked.", runtime.node.as_deref().unwrap_or("another computer"), hostname())),
        _ => line("  Not running.".into()),
    }
    if let Some(log) = &runtime.log {
        line(format!("  Log: {}", log.display()));
    }
    if let Some(cluster) = &report.cluster {
        line(format!("  Cluster state folder: {} (holds {})", cluster.state_dir.display(), cluster.files.join(", ")));
    }

    let machines = &report.machines;
    line(String::new());
    line(format!("Machines file: {}", machines.path.display()));
    match (&machines.error, machines.machines.is_empty()) {
        (Some(error), _) => line(format!("  Not listed: {error}")),
        (None, true) => line("  none".into()),
        (None, false) => {
            for machine in &machines.machines {
                line(format!("  {}: {}, Julia in Slurm jobs: {}", machine.name, machine.host, if machine.slurm { "yes" } else { "no" }));
            }
        }
    }

    let projects = &report.projects;
    line(String::new());
    match (&projects.error, projects.count) {
        (Some(error), _) => line(format!("Projects file: {}\n  Not read: {error}", projects.path.display())),
        (None, count) => line(format!("Projects file: {} ({} remembered)", projects.path.display(), count.unwrap_or(0))),
    }

    let folders = &report.folders;
    line(String::new());
    line("Folders".into());
    let described = |name: &str, f: &Folder, more: String| {
        let detail = match (f.exists, f.size_bytes) {
            (false, _) => "not there".to_owned(),
            (true, Some(bytes)) => format!("{}{more}", size_text(bytes)),
            (true, None) => format!("there{more}"),
        };
        format!("  {name}: {} ({detail})", f.path.display())
    };
    let builds = if folders.plugin_binaries.builds.is_empty() { String::new() } else { format!("; builds: {}", folders.plugin_binaries.builds.join(", ")) };
    line(described("Plugin binaries", &folders.plugin_binaries.folder, builds));
    line(described("Helpers for other platforms", &folders.helpers, String::new()));
    line(described("Unpacked runtime (serve)", &folders.serve_runtime, String::new()));
    line(described("Server root", &folders.server_root, String::new()));
    line(described("Default depot", &folders.depot, String::new()));
    out
}
