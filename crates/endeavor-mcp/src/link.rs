//! The link (docs/plugins-and-remote.md): one background process for each
//! machine, on the user's computer. It runs the `ssh` to `endeavor connect`
//! there, keeps the one loopback port that relays to the machine's runtime,
//! and reconnects when the connection drops. The fronts (`endeavor mcp`, one
//! for each agent session) share it: `ensure` finds the running link for a
//! machine or starts one, and `Link` has the calls its control interface takes.
//!
//! A link is `endeavor link --machine ID`, started by `ensure` and not by
//! hand. It reads the machine from the machines file
//! (`client::MachinesFile`) and keeps, in `<state home>/endeavor/links/ID/`:
//!
//! - `link.json`: its pid, control port, token and build. Present while it runs.
//! - `link.lock`: held by `ensure` while it looks for a link or starts one.
//! - `link.log`: what the link and its `ssh` said.
//!
//! The control interface is HTTP on a loopback port of its own, with the
//! token from `link.json` as a bearer token. It refuses a Host that isn't
//! loopback and any request with an Origin. Every request counts as activity;
//! the link ends 8 hours after the last one (`ENDEAVOR_LINK_IDLE_SECS`), leaving
//! the runtime running. It never stops a runtime by itself.
//!
//! - `GET /link/status`: a `Status`.
//! - `POST /link/start` `{"job": <JobRequest or null>}`: start the runtime, or
//!   attach to the one running, in the background. The `Status` at once.
//! - `POST /link/stop`: stop the runtime for every client. The link stays connected.
//! - `POST /link/quit`: detach, remove the record and exit. The record goes
//!   first, so a front that asks for a link right after gets a new one.
//!
//! Variables for tests only, read by the link process: `ENDEAVOR_LINK_SHELL`
//! (any value) runs the helper on this computer through `sh`, as
//! `Transport::Shell` does, so no sshd is needed; `ENDEAVOR_LINK_ROOT`,
//! `ENDEAVOR_LINK_STATE` and `ENDEAVOR_LINK_DEPOT` set `Options::root`, `state`
//! and `depot`, which otherwise are the server's own default folders;
//! `ENDEAVOR_LINK_ASK` is a command that runs in the shell before each connect
//! (`Transport::Shell`'s `ask`), and a failure of it fails the connect.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wire::slurm::{JobRequest, Partition};

use crate::standalone::Env;

mod run;
#[cfg(test)]
mod tests;

pub(crate) use run::main;

/// How long `ensure` waits for a new link to answer.
const START_WAIT: Duration = Duration::from_secs(10);

/// How often, and how far apart, `ensure` asks a link whose process lives but doesn't answer.
const SILENT_TRIES: u32 = 5;
const SILENT_PAUSE: Duration = Duration::from_millis(700);

/// How long a call to the link may take, except a stop (`Link::stop`).
const CALL_WAIT: Duration = Duration::from_secs(5);

/// Where a link stands.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Signing in and starting the helper, or getting the connection back.
    Connecting,
    /// The helper is up and no runtime is asked for (or it was stopped).
    Connected,
    /// A runtime is starting or being attached to.
    Starting,
    /// A cluster job waits in the queue.
    Queued,
    /// The runtime is attached and answers through the listener's port.
    Ready,
    /// The last step didn't work: `error` says why. `POST /link/start` tries again.
    Failed,
}

/// What `GET /link/status` answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// The machine's id in the machines file.
    pub machine: String,
    /// The name the agent knows it by.
    pub name: String,
    pub state: State,
    /// The last progress line.
    pub step: Option<String>,
    /// In plain words, when `state` is `failed`.
    pub error: Option<String>,
    /// What the helper reported, once it is connected.
    pub hello: Option<HelloInfo>,
    /// The runtime, once it is attached.
    pub runtime: Option<RuntimeInfo>,
    /// The cluster job, from the time it is submitted.
    pub job: Option<JobInfo>,
    /// Slurm's state and reason while the job waits.
    pub queue: Option<QueueInfo>,
    /// The link process and the build it is from.
    pub pid: u32,
    pub build: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HelloInfo {
    pub node: String,
    pub home: String,
    pub slurm: bool,
    pub uploads: bool,
    /// `uname`'s words for the machine, as `linux` and `x86_64`.
    pub os: Option<String>,
    pub arch: Option<String>,
    /// This connect installed the helper there, or it was already installed.
    pub helper_installed: Option<bool>,
    /// The Julia the helper found, once it has started one.
    pub julia: Option<JuliaInfo>,
    /// Slurm's partitions, on a machine that has Slurm.
    pub partitions: Option<Vec<Partition>>,
    pub scratch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JuliaInfo {
    pub path: String,
    pub version: String,
}

/// How to reach the runtime that is attached.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    /// The listener's port on this computer. It stays the same while the link runs.
    pub port: u16,
    /// For `Authorization: Bearer` on every request to that port.
    pub token: String,
    pub mcp_url: String,
    /// Pluto's start page, with the token that lets a browser in.
    pub page_url: String,
    pub node: String,
    pub pid: u32,
    /// It was running already; this link didn't start it.
    pub reattached: bool,
    pub job: Option<wire::slurm::Job>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobInfo {
    pub id: String,
    /// "8 CPUs · 32 GB · 8 h".
    pub summary: Option<String>,
    pub node: Option<String>,
    /// When Slurm will end it (Unix seconds).
    pub ends_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueInfo {
    pub state: String,
    pub reason: String,
}

/// The file a running link keeps (`link.json`).
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    machine: String,
    pid: u32,
    /// Windows: when the process started, which tells it from a later one with its pid.
    #[serde(default)]
    started: Option<u64>,
    port: u16,
    token: String,
    build: String,
}

/// How `ensure` starts a link: the program, and variables added to the
/// environment it gets (and to where `ensure` looks for the link's folder).
pub struct Spawn {
    pub exe: PathBuf,
    pub env: Vec<(String, String)>,
}

impl Spawn {
    /// This program, in this environment.
    pub fn here() -> Result<Spawn, String> {
        let exe = std::env::current_exe().map_err(|e| format!("Couldn't find the endeavor program itself: {e}"))?;
        Ok(Spawn { exe, env: Vec::new() })
    }

    fn var(&self, name: &str) -> Option<String> {
        self.env.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.clone()).or_else(|| std::env::var(name).ok())
    }

    /// The folder of the machine `id`'s link.
    fn dir(&self, id: &str) -> PathBuf {
        Env::from_vars(&|name| self.var(name)).links_dir().join(id)
    }
}

/// A machine's id becomes a folder's name, and the machines file can be edited by hand.
/// Capitals are out because Windows and macOS give two ids that differ only in
/// case one folder, and names Windows keeps for devices (`nul`, `com1`, even as
/// `nul.txt`) and a trailing dot can't be folders there.
fn valid_id(id: &str) -> Result<(), String> {
    let stem = id.split('.').next().unwrap_or_default();
    let device = matches!(stem, "con" | "prn" | "aux" | "nul") || (stem.len() == 4 && (stem.starts_with("com") || stem.starts_with("lpt")) && stem.ends_with(|c: char| c.is_ascii_digit() && c != '0'));
    let plain = !id.is_empty() && id.len() <= 100 && !id.starts_with('.') && !id.ends_with('.') && !device && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if plain { Ok(()) } else { Err(format!("\"{id}\" isn't a machine id: it has lower-case letters, digits, - _ and . only, doesn't start or end with a dot and isn't a name such as nul or com1.")) }
}

/// A running link, as a front reaches it.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub machine: String,
    /// The control port, on this computer's loopback.
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// The build it is from. `ensure` uses a running link whatever its build; a
    /// front that needs its own can `quit` the old one and `ensure` again,
    /// which gives the browser a new port.
    pub build: String,
}

/// The link for `machine` (an id in the machines file), started if none runs.
pub fn ensure(machine: &str) -> Result<Link, String> {
    ensure_with(&Spawn::here()?, machine)
}

/// `ensure`, starting the link with `spawn`.
pub fn ensure_with(spawn: &Spawn, machine: &str) -> Result<Link, String> {
    valid_id(machine)?;
    let dir = spawn.dir(machine);
    crate::make_state_dir(&dir)?;
    // Two fronts asking at once get one link: the second waits for the first's.
    let lock_path = dir.join("link.lock");
    let lock = crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false))
        .open(&lock_path)
        .map_err(|e| format!("Couldn't open {}: {e}", lock_path.display()))?;
    let waited = Instant::now();
    while !crate::try_lock(&lock) {
        if waited.elapsed() > START_WAIT + Duration::from_secs(10) {
            return Err(format!("Another process is still starting the link to {machine}. Try again in a moment."));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // A link that is alive and slow to answer is waited for, not replaced by a second one.
    let mut silent = 0;
    loop {
        match look(&dir, machine) {
            Found::Link(link) => return Ok(link),
            Found::None => break,
            Found::Silent(pid) if silent >= SILENT_TRIES => return Err(format!("The link to {machine} (pid {pid}) isn't answering. Try again in a moment.")),
            Found::Silent(_) => {
                silent += 1;
                std::thread::sleep(SILENT_PAUSE);
            }
        }
    }
    let _ = std::fs::remove_file(dir.join("link.json"));
    let log_path = dir.join("link.log");
    let log = crate::owner_only(std::fs::OpenOptions::new().write(true).create(true).truncate(true))
        .open(&log_path)
        .map_err(|e| format!("Couldn't open {}: {e}", log_path.display()))?;
    let mut command = Command::new(&spawn.exe);
    // Its own folder as the working directory, so that it doesn't hold the front's (a project's) folder.
    command.args(["link", "--machine", machine]).current_dir(&dir).envs(spawn.env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    let mut child = detached(command)?;
    let started = Instant::now();
    let failed = loop {
        if let Some(link) = running(&dir, machine) {
            // Reaped as soon as it ends.
            std::thread::spawn(move || drop(child.wait()));
            return Ok(link);
        }
        if let Ok(Some(status)) = child.try_wait() {
            break format!("The link to {machine} ended as it started ({status}). {}", log_tail(&log_path));
        }
        if started.elapsed() > START_WAIT {
            end_child(&mut child);
            break format!("The link to {machine} didn't answer in {} s. {}", START_WAIT.as_secs(), log_tail(&log_path));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Err(failed)
}

/// End a link that `ensure` started and gave up on, which has its helper and `ssh`
/// to let go of: ask it to leave, then end it if it doesn't.
fn end_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: plain syscall, on a child this process started and has not reaped.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let until = Instant::now() + Duration::from_secs(6);
        while Instant::now() < until {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The end of the link's log, for an error.
fn log_tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(5)..].join("\n");
    if tail.is_empty() { format!("Its log is {}.", path.display()) } else { format!("Its log ({}) says:\n{tail}", path.display()) }
}

/// Start `command` in a session of its own, with no terminal and the default signal mask.
#[cfg(unix)]
fn detached(mut command: Command) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid and sigprocmask are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            // A front that blocks Ctrl-C's for itself (`mcp`) mustn't pass the block on.
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    command.spawn().map_err(|e| format!("Couldn't start the link: {e}"))
}

/// Start `command` with a console of its own and no window, in a process group
/// of its own, out of the front's Job Object if that allows it (as for the runtime).
#[cfg(windows)]
fn detached(mut command: Command) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    match command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => command.creation_flags(flags).spawn(),
        result => result,
    }
    .map_err(|e| format!("Couldn't start the link: {e}"))
}

/// What the record in a link's folder leads to.
enum Found {
    /// The link, answering with its token.
    Link(Link),
    /// Its process lives (the pid is this) and its port doesn't answer.
    Silent(u32),
    /// No record, an unreadable one, another machine's, or a process that is gone.
    None,
}

fn look(dir: &Path, machine: &str) -> Found {
    let Some(record) = std::fs::read_to_string(dir.join("link.json")).ok().and_then(|text| serde_json::from_str::<Record>(&text).ok()) else { return Found::None };
    if record.machine != machine || !crate::pid_alive(record.pid as i32, record.started) {
        return Found::None;
    }
    let link = Link { machine: machine.to_owned(), port: record.port, token: record.token, pid: record.pid, build: record.build };
    match link.status() {
        Ok(status) if status.machine == machine && status.pid == link.pid => Found::Link(link),
        _ => Found::Silent(link.pid),
    }
}

/// The link whose record is in `dir`, if its process lives and its port answers with its token.
fn running(dir: &Path, machine: &str) -> Option<Link> {
    match look(dir, machine) {
        Found::Link(link) => Some(link),
        _ => None,
    }
}

impl Link {
    /// Where the link stands.
    pub fn status(&self) -> Result<Status, String> {
        self.call("GET", "/link/status", &[], CALL_WAIT)
    }

    /// Start the runtime, or attach to the one running, and return at once: poll
    /// `status` for the rest. A start under way, or a runtime attached, is not an
    /// error. `job` is what to submit on a cluster.
    pub fn start(&self, job: Option<JobRequest>) -> Result<Status, String> {
        self.call("POST", "/link/start", &serde_json::to_vec(&serde_json::json!({ "job": job })).map_err(|e| e.to_string())?, CALL_WAIT)
    }

    /// Stop the runtime for every client, and wait until it is gone (up to a minute
    /// and a bit). The link stays connected, and says why when the runtime didn't stop.
    pub fn stop(&self) -> Result<(), String> {
        self.call::<serde_json::Value>("POST", "/link/stop", &[], Duration::from_secs(75)).map(|_| ())
    }

    /// End the link: it detaches, so the runtime goes on, and removes its record.
    pub fn quit(&self) -> Result<(), String> {
        self.call::<serde_json::Value>("POST", "/link/quit", &[], CALL_WAIT).map(|_| ())
    }

    fn call<T: serde::de::DeserializeOwned>(&self, method: &str, path: &str, body: &[u8], wait: Duration) -> Result<T, String> {
        let bearer = format!("Bearer {}", self.token);
        let (status, reply) = crate::http::call(method, self.port, path, &[("Authorization", &bearer), ("Content-Type", "application/json")], body, Some(wait)).map_err(|e| format!("The link to {} didn't answer: {e}", self.machine))?;
        if status != 200 {
            let said = serde_json::from_slice::<serde_json::Value>(&reply).ok().and_then(|v| v["error"].as_str().map(str::to_owned));
            return Err(said.unwrap_or_else(|| format!("The link to {} answered HTTP {status}.", self.machine)));
        }
        serde_json::from_slice(&reply).map_err(|e| format!("The link to {} sent something unreadable: {e}", self.machine))
    }
}
