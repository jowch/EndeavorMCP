//! The notebook tools without the app (docs/standalone.md). The user starts
//! the runtime themselves, as they would Pluto or Jupyter, and points their
//! agent and browser at its one port:
//!
//! - `serve` starts the runtime in the foreground (or uses the one already
//!   running from its state folder) and prints how to connect: the browser
//!   link, the MCP URL and token, agent configs and the `ssh -L` line.
//!   Ctrl-C (and on Windows, closing the console) stops a runtime it started.
//! - `mcp` is the stdio form for an agent on the same machine (plugin
//!   installs): it starts or reuses the runtime in the background, where it
//!   outlives the agent and ends itself after the idle stop, and relays MCP
//!   between stdin/stdout and the runtime's `/mcp`.
//! - `stop` ends the runtime running from the state folder.
//!
//! `runtime/` is built into this binary (build.rs) and unpacked to a folder
//! named by its version on first use, so the binary is all a user installs.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::http::Head;
use crate::mcp::to_json;
use crate::{Args, Launcher, Runtime, State, embedded, julia};

mod machines;
mod projects;

const USAGE: &str = "usage: endeavor serve [OPTIONS]   run Julia here and print how to connect (Ctrl-C stops it)
       endeavor mcp [OPTIONS]     MCP over stdin/stdout for an agent on this machine
       endeavor stop              stop the Julia that serve or mcp started

options:
  --folder DIR         where new notebooks go (default: the current folder)
  --port PORT          the port to listen on, on 127.0.0.1 (default: a free one) [serve, mcp]
  --host-tools         give every agent session list_folder, read_file and run_shell here [serve]
  --skills plugin      the agent has Endeavor's skills from its plugin [mcp]
  --julia PATH|auto    the julia to use (default auto: your login shell's, else Endeavor's own download)
  --julia-shell LINE   a shell line that puts julia on the PATH, such as 'module load julia'
  --depot DEPOT        JULIA_DEPOT_PATH (default ~/.cache/endeavor/depot:, or $SCRATCH/endeavor/depot:)
  --idle-stop HOURS    stop notebooks unused this long; 0 never (default 48)
  --state-dir DIR      the runtime's state (default ~/.local/state/endeavor/serve/<host>)
";

/// Hours a notebook may sit unused before it stops (and, for a runtime `mcp`
/// started, how long the runtime stays up with no notebook open): the app's default.
const IDLE_HOURS: f64 = 48.0;

/// How long a relayed call, and each machine tool, waits for a runtime that is
/// still starting before it says so, under the time agents give a tool call.
/// `ENDEAVOR_START_WAIT_SECS` sets it (the tests do).
fn start_wait() -> Duration {
    lock_limit("ENDEAVOR_START_WAIT_SECS", 45)
}

#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Serve(Options),
    Mcp(Options),
    Stop { state_dir: PathBuf },
}

#[derive(Debug, PartialEq)]
pub(crate) struct Options {
    /// The runtime's `runtime.json`, token and log.
    state_dir: PathBuf,
    /// Where `runtime/` is unpacked, a folder per version.
    cache: PathBuf,
    julia: julia::Source,
    /// JULIA_DEPOT_PATH.
    depot: String,
    /// Where notebooks are created, and relative paths start.
    folder: PathBuf,
    /// The runtime's port; 0 picks a free one.
    port: u16,
    /// Every MCP session gets the host tools (`serve` only).
    host_tools: bool,
    idle_hours: f64,
    /// The agent loads the skills from the plugin (`mcp` only).
    skills_plugin: bool,
}

/// What the defaults come from.
pub(crate) struct Env {
    pub home: PathBuf,
    pub state_home: Option<PathBuf>,
    pub cache_home: Option<PathBuf>,
    pub scratch: Option<String>,
    pub cwd: PathBuf,
    pub node: String,
}

impl Env {
    fn here() -> Env {
        Env::from_vars(&|name| std::env::var(name).ok())
    }

    /// The defaults for the environment `read` gives, which a process started
    /// with other variables (`link::Spawn`) has.
    pub(crate) fn from_vars(read: &dyn Fn(&str) -> Option<String>) -> Env {
        let var = |name: &str| read(name).filter(|v| !v.is_empty());
        #[cfg(windows)]
        let home = var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("Endeavor");
        #[cfg(not(windows))]
        let home = var("HOME").map(PathBuf::from).or_else(std::env::home_dir).unwrap_or_default();
        Env {
            home,
            state_home: var("XDG_STATE_HOME").map(PathBuf::from),
            cache_home: var("XDG_CACHE_HOME").map(PathBuf::from),
            scratch: var("SCRATCH").filter(|s| s.starts_with('/')),
            cwd: std::env::current_dir().unwrap_or_default(),
            node: crate::hostname(),
        }
    }

    /// Per machine, since a home folder is often shared by a cluster's nodes.
    fn state_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("serve").join(&self.node);
        }
        self.state_home.clone().unwrap_or_else(|| self.home.join(".local/state")).join("endeavor/serve").join(&self.node)
    }

    /// One folder for each machine's link (`link`): its record, lock and log.
    pub(crate) fn links_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("links");
        }
        self.state_home.clone().unwrap_or_else(|| self.home.join(".local/state")).join("endeavor/links")
    }

    /// What projects remember (`projects`).
    pub(crate) fn projects_path(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("projects.json");
        }
        self.state_home.clone().unwrap_or_else(|| self.home.join(".local/state")).join("endeavor/projects.json")
    }

    /// For `connect --launcher slurm`: one for the whole cluster, since a reconnect
    /// through another login node must find the same job.
    fn cluster_state_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("cluster");
        }
        self.state_home.clone().unwrap_or_else(|| self.home.join(".local/state")).join("endeavor/cluster")
    }

    fn cache(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("serve-runtime");
        }
        self.cache_home.clone().unwrap_or_else(|| self.home.join(".cache")).join("endeavor/serve")
    }

    /// Helpers fetched from the release for servers of other platforms (`release::fetch_helper`).
    pub(crate) fn helpers_dir(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("helpers");
        }
        self.cache_home.clone().unwrap_or_else(|| self.home.join(".cache")).join("endeavor/helpers")
    }

    /// The depot the app's server installs use, so packages installed for one
    /// serve the other; the trailing separator stacks the user's own depots
    /// (~/.julia) behind it, read-only.
    fn depot(&self) -> String {
        if cfg!(windows) {
            return format!("{};", self.home.join("serve-depot").display());
        }
        match &self.scratch {
            Some(scratch) => format!("{scratch}/endeavor/depot:"),
            None => format!("{}/.cache/endeavor/depot:", self.home.display()),
        }
    }
}

pub(crate) fn parse(argv: &[String], env: &Env) -> Result<Command, String> {
    let (command, rest) = argv.split_first().ok_or("expected serve, mcp or stop")?;
    let (mut state_dir, mut julia, mut depot, mut folder, mut port) = (None, None::<julia::Source>, None, None, 0);
    let (mut host_tools, mut idle_hours, mut skills_plugin) = (false, IDLE_HOURS, false);
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or(format!("{arg} needs a value"));
        let only = |commands: &[&str]| if commands.contains(&command.as_str()) { Ok(()) } else { Err(format!("{arg} isn't an option of {command}")) };
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--julia" | "--julia-shell" if julia.is_some() => return Err("give one of --julia and --julia-shell".into()),
            "--julia" => {
                only(&["serve", "mcp"])?;
                julia = Some(value().map(|v| if v == "auto" { julia::Source::Auto } else { julia::Source::Path(v) })?)
            }
            "--julia-shell" => {
                only(&["serve", "mcp"])?;
                julia = Some(julia::Source::Shell(value()?))
            }
            "--depot" => {
                only(&["serve", "mcp"])?;
                depot = Some(value()?)
            }
            "--folder" => {
                only(&["serve", "mcp"])?;
                folder = Some(PathBuf::from(value()?))
            }
            "--port" => {
                only(&["serve", "mcp"])?;
                port = value()?.parse().map_err(|_| "--port needs a port number".to_owned())?
            }
            "--idle-stop" => {
                only(&["serve", "mcp"])?;
                idle_hours = value()?.parse().ok().filter(|h: &f64| *h >= 0.0).ok_or("--idle-stop needs a number of hours (0: never)")?
            }
            "--host-tools" => {
                only(&["serve"])?;
                host_tools = true
            }
            "--skills" => {
                only(&["mcp"])?;
                match value()?.as_str() {
                    "plugin" => skills_plugin = true,
                    other => return Err(format!("--skills takes `plugin`, not {other}")),
                }
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let state_dir = state_dir.unwrap_or_else(|| env.state_dir());
    if command == "stop" {
        return Ok(Command::Stop { state_dir });
    }
    let folder = match folder {
        Some(folder) if folder.is_relative() => env.cwd.join(folder),
        Some(folder) => folder,
        None => env.cwd.clone(),
    };
    let options = Options {
        state_dir,
        cache: env.cache(),
        julia: julia.unwrap_or(julia::Source::Auto),
        depot: depot.unwrap_or_else(|| env.depot()),
        folder,
        port,
        host_tools,
        idle_hours,
        skills_plugin,
    };
    match command.as_str() {
        "serve" => Ok(Command::Serve(options)),
        "mcp" => Ok(Command::Mcp(options)),
        other => Err(format!("unknown command {other}")),
    }
}

/// `endeavor serve|mcp|stop …`.
pub fn main(argv: &[String]) -> ! {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    let command = parse(argv, &Env::here()).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    });
    match command {
        Command::Serve(options) => serve(options),
        Command::Mcp(options) => relay(options),
        Command::Stop { state_dir } => stop(&state_dir),
    }
}

/// Unpack the runtime built into this binary, once per version. The folder.
pub(crate) fn unpack_runtime(cache: &Path) -> Result<PathBuf, String> {
    Ok(unpack(cache, embedded::RUNTIME_VERSION, embedded::RUNTIME_FILES)?.join("runtime"))
}

/// `files` in `cache/version/`, put there whole: written to a folder of
/// their own, then moved into place, so a folder by that name is complete.
/// A new version's folder replaces the older ones nothing uses any more
/// (`remove_unused`). That folder.
pub fn unpack(cache: &Path, version: &str, files: &[(&str, &[u8])]) -> Result<PathBuf, String> {
    let dir = cache.join(version);
    if dir.is_dir() {
        let _ = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(dir.join(IN_USE)).and_then(|f| f.set_modified(SystemTime::now()));
        return Ok(dir);
    }
    let failed = |e: io::Error| format!("Couldn't unpack Endeavor's runtime into {}: {e}", cache.display());
    let part = cache.join(format!("{version}.part.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    for (path, contents) in files.iter().chain([&(IN_USE, &b""[..])]) {
        let file = part.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).map_err(failed)?;
        std::fs::write(&file, contents).map_err(failed)?;
    }
    match std::fs::rename(&part, &dir) {
        Ok(()) => {
            remove_unused(cache, version);
            Ok(dir)
        }
        // Another process unpacked the same version first.
        Err(_) if dir.is_dir() => {
            let _ = std::fs::remove_dir_all(&part);
            Ok(dir)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&part);
            Err(failed(e))
        }
    }
}

/// In each unpacked folder: whoever uses the folder holds a shared lock on
/// it (`Lease`), and its time is when the folder was last unpacked or used.
const IN_USE: &str = "in-use";

/// How long an unused folder is kept after its last use. The lock alone may
/// not show: a cache in a home folder shared by a cluster's nodes may have
/// locks that only its own node sees.
const KEEP_UNUSED: Duration = Duration::from_secs(24 * 3600);

/// How often a `Lease` marks its folder as used.
const LEASE_TOUCH: Duration = Duration::from_secs(3600);

/// Remove the folders in `cache` other than `keep`'s that no one holds a
/// `Lease` on and no one has unpacked or used for `KEEP_UNUSED`. A folder
/// with no `IN_USE` is left alone: a build from before leases unpacked it,
/// and a runtime it started may still run from it.
fn remove_unused(cache: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(cache) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name.contains(".removing.") {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        if name == keep || name.contains(".part.") || !path.is_dir() {
            continue;
        }
        let Ok(marker) = std::fs::OpenOptions::new().read(true).write(true).open(path.join(IN_USE)) else { continue };
        let used = marker.metadata().and_then(|m| m.modified()).ok();
        if used.is_none_or(|used| used.elapsed().unwrap_or_default() < KEEP_UNUSED) || marker.try_lock().is_err() {
            continue;
        }
        drop(marker);
        let trash = cache.join(format!("{name}.removing.{}", std::process::id()));
        if std::fs::rename(&path, &trash).is_ok() {
            let _ = std::fs::remove_dir_all(&trash);
        }
    }
}

/// A shared lock on an unpacked folder (`unpack`) that keeps another
/// version's `unpack` from removing it, for as long as this lives.
pub struct Lease {
    _marker: Arc<std::fs::File>,
}

/// Hold `dir`, a folder `unpack` returned, while it is used. None if it
/// wasn't unpacked by `unpack` (such as a server's runtime the app installs).
pub fn lease(dir: &Path) -> Option<Lease> {
    let marker = std::fs::OpenOptions::new().read(true).write(true).open(dir.join(IN_USE)).ok()?;
    marker.try_lock_shared().ok()?;
    let marker = Arc::new(marker);
    let _ = marker.set_modified(SystemTime::now());
    let held = Arc::downgrade(&marker);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(LEASE_TOUCH);
            let Some(marker) = held.upgrade() else { return };
            let _ = marker.set_modified(SystemTime::now());
        }
    });
    Some(Lease { _marker: marker })
}

/// A runtime this process can reach: its state, and the process if this one started it.
struct Up {
    state: State,
    port: u16,
    started: Option<Runtime>,
}

/// The runtime running from the state folder, or a new one, once it answers.
/// `progress` hears the start's log; `cancelled` ends a start early.
fn start_or_reuse(options: &Options, exit_idle: bool, progress: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<Up, String> {
    let dir = &options.state_dir;
    crate::make_state_dir(dir)?;
    // One start at a time: the stdio form runs once per agent session.
    let _starting = start_lock(dir)?;
    let mut args = Args {
        state_dir: dir.clone(),
        julia: options.julia.clone(),
        runtime: PathBuf::new(),
        depot: options.depot.clone(),
        launcher: Launcher::Process,
        quit_with_client: false,
        any_node: false,
        build: Some(embedded::BUILD_VERSION.into()),
        exit_idle,
        core_env: core_env(options, exit_idle),
    };
    if let Some(state) = crate::existing(&args)? {
        let port = state.port.ok_or("The Julia running here was started by an older version of Endeavor. Stop it with `endeavor stop`, then try again.")?;
        return Ok(Up { state, port, started: None });
    }
    crate::stopped::clear(dir);
    args.runtime = unpack_runtime(&options.cache)?;
    let (julia, version) = julia::find(&options.julia, &|line| progress(&line))?;
    progress(&format!("Starting Julia {version} ({julia})"));
    let token = crate::token(dir)?;
    let child = crate::start(&args, &julia, &token)?;
    let (events, _) = mpsc::channel();
    let runtime = Runtime::child(child, dir, &events);
    match wait_ready(&runtime, progress, cancelled) {
        Ok((state, port)) => Ok(Up { state, port, started: Some(runtime) }),
        Err(e) => {
            runtime.kill();
            Err(e)
        }
    }
}

/// The core's environment for a standalone runtime (see `core::main`).
fn core_env(options: &Options, exit_idle: bool) -> Vec<(&'static str, String)> {
    let mut env = vec![("ENDEAVOR_FOLDER", options.folder.display().to_string()), ("ENDEAVOR_IDLE_HOURS", options.idle_hours.to_string())];
    if options.port != 0 {
        env.push(("ENDEAVOR_PORT", options.port.to_string()));
    }
    if options.host_tools {
        env.push(("ENDEAVOR_HOST_TOOLS", crate::hostname()));
    }
    if exit_idle {
        env.push(("ENDEAVOR_EXIT_IDLE", "1".into()));
    }
    env
}

/// `DIR/start.lock`, opened and not yet held. `serve`, `mcp` and `connect` hold
/// it while they find or start a runtime in `dir`.
pub(crate) fn open_start_lock(dir: &Path) -> Result<std::fs::File, String> {
    let path = dir.join("start.lock");
    crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false))
        .open(&path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))
}

/// How long to wait for another process's start before giving up. A first
/// start downloads Julia and precompiles Pluto, which takes minutes, so this is
/// well past that; `ENDEAVOR_START_LOCK_SECS` sets it (the tests do).
pub(crate) fn start_lock_limit() -> Duration {
    lock_limit("ENDEAVOR_START_LOCK_SECS", 30 * 60)
}

/// How long a stop waits for the start lock. The stop itself takes up to about
/// 20 s more, and the client waits 60 s for the answer (`STOP_WAIT` in the
/// client); `ENDEAVOR_STOP_LOCK_SECS` sets it (the tests do).
pub(crate) fn stop_lock_limit() -> Duration {
    lock_limit("ENDEAVOR_STOP_LOCK_SECS", 20)
}

fn lock_limit(var: &str, default_secs: u64) -> Duration {
    Duration::from_secs(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default_secs))
}

/// What to say when `start_lock_limit` passes.
pub(crate) fn start_lock_gave_up(dir: &Path) -> String {
    format!("Gave up waiting for another process that is starting Julia in {}. If none is, delete {} and try again.", dir.display(), dir.join("start.lock").display())
}

/// What `wait_for_start_lock` gave up on.
pub(crate) enum Wait<E> {
    /// The lock file couldn't be opened.
    Failed(String),
    /// The limit passed.
    TimedOut,
    /// `pause` said to stop waiting.
    Interrupted(E),
}

/// Hold `DIR/start.lock` until dropped, waiting up to `limit` for another
/// process to let go. `pause` is called between attempts and does the waiting;
/// an error from it ends the wait.
pub(crate) fn wait_for_start_lock<E>(dir: &Path, limit: Duration, mut pause: impl FnMut() -> Result<(), E>) -> Result<std::fs::File, Wait<E>> {
    let file = open_start_lock(dir).map_err(Wait::Failed)?;
    let started = Instant::now();
    while !crate::try_lock(&file) {
        if started.elapsed() > limit {
            return Err(Wait::TimedOut);
        }
        pause().map_err(Wait::Interrupted)?;
    }
    Ok(file)
}

/// Hold `DIR/start.lock` until dropped, waiting for another process's start.
pub(crate) fn start_lock(dir: &Path) -> Result<std::fs::File, String> {
    let pause = || {
        std::thread::sleep(Duration::from_millis(200));
        Ok::<(), std::convert::Infallible>(())
    };
    wait_for_start_lock(dir, start_lock_limit(), pause).map_err(|wait| match wait {
        Wait::Failed(message) => message,
        Wait::TimedOut => start_lock_gave_up(dir),
        Wait::Interrupted(never) => match never {},
    })
}

/// What to say when a stop gave up waiting for the start lock.
pub(crate) fn stop_lock_gave_up(dir: &Path) -> String {
    format!(
        "Julia was not stopped: another process has held the start lock in {} for too long. Julia is still running. Try again, or delete {} if nothing is starting Julia.",
        dir.display(),
        dir.join("start.lock").display()
    )
}

/// Follow the log of a runtime we started until it writes its state and
/// answers on its port.
fn wait_ready(runtime: &Runtime, progress: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<(State, u16), String> {
    let log_path = runtime.state_dir.join("runtime.log");
    let mut log = None;
    let mut line = String::new();
    loop {
        if log.is_none() {
            log = std::fs::File::open(&log_path).ok().map(BufReader::new);
        }
        while let Some(reader) = &mut log {
            match reader.read_line(&mut line) {
                Ok(n) if n > 0 && line.ends_with('\n') => {
                    progress(&crate::redact_secret(line.trim_end()));
                    line.clear();
                }
                _ => break,
            }
        }
        if let Some(status) = runtime.exit.status() {
            let tail = crate::log_tail(&log_path);
            let tail = tail[tail.len().saturating_sub(8)..].join("\n");
            return Err(format!("Julia stopped while starting ({status}). The end of {}:\n{tail}", log_path.display()));
        }
        if cancelled() {
            return Err("Stopped before Julia was ready.".into());
        }
        if let Some(state) = crate::read_state(&runtime.state_dir)
            && state.pid == runtime.pid
            && let Some(port) = state.port
            && crate::answers(port, &state.token)
        {
            return Ok((state, port));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// How to reach a runtime, for `connection_text`.
pub(crate) struct Connection<'a> {
    pub port: u16,
    pub token: &'a str,
    /// This machine, as `ssh` from the user's computer names it (its host name).
    pub node: &'a str,
    pub folder: &'a str,
    /// In a Slurm job, the login node it was submitted from (`SLURM_SUBMIT_HOST`).
    pub login: Option<&'a str>,
}

/// What `serve` prints once the runtime answers.
pub(crate) fn connection_text(c: &Connection) -> String {
    let Connection { port, token, node, folder, .. } = c;
    let url = format!("http://localhost:{port}/mcp");
    let bearer = format!("Bearer {token}");
    let json = to_json(&json!({ "mcpServers": { "endeavor": { "type": "http", "url": url, "headers": { "Authorization": bearer } } } }));
    let mut ssh = format!("    ssh -L {port}:localhost:{port} {node}\n");
    if let Some(login) = c.login.filter(|login| login != node) {
        ssh = format!("    ssh -J {login} -L {port}:localhost:{port} {node}\n(This is a cluster's compute node: the jump goes through the login node, {login}; use the name you ssh to.)\n");
    }
    format!(
        "Endeavor's notebooks are running on {node}, port {port}. New notebooks go in {folder}.

Open them in a browser:
    http://localhost:{port}/?token={token}

From another computer, forward the port first:
{ssh}
Connect an agent over MCP (Streamable HTTP):
    URL:    {url}
    Header: Authorization: {bearer}

Claude Code:
    claude mcp add --transport http endeavor {url} --header \"Authorization: {bearer}\"

Codex (~/.codex/config.toml):
    [mcp_servers.endeavor]
    url = \"{url}\"
    http_headers = {{ Authorization = \"{bearer}\" }}

Gemini CLI (~/.gemini/settings.json):
    {gemini}

Other agents (JSON):
    {json}

The token lets anyone who has it run code as you. Keep it to yourself.
",
        gemini = to_json(&json!({ "mcpServers": { "endeavor": { "httpUrl": url, "headers": { "Authorization": bearer } } } })),
    )
}

/// The state folder `serve`, `mcp` and `stop` use when not given one.
pub(crate) fn default_state_dir() -> PathBuf {
    Env::here().state_dir()
}

/// The state folder `connect --launcher slurm` uses when not given one.
pub(crate) fn default_cluster_state_dir() -> PathBuf {
    Env::here().cluster_state_dir()
}

/// The folder a running standalone runtime recorded for its notebooks.
fn recorded_folder(dir: &Path) -> Option<String> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    state["folder"].as_str().map(str::to_owned)
}

/// What to tell the user when the runtime recorded in `dir` came from
/// another build than this binary (`embedded::BUILD_VERSION`); none when it's
/// this build's, or nothing is recorded. The runtime keeps working as it was
/// started: stopping it is the user's call.
pub(crate) fn other_build(dir: &Path) -> Option<String> {
    other_build_than(dir, embedded::BUILD_VERSION)
}

/// `other_build`, against build `this`.
pub(crate) fn other_build_than(dir: &Path, this: &str) -> Option<String> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    let which = match state["build"].as_str() {
        Some(build) if build == this => return None,
        Some(build) => format!("build {build}"),
        None => "an earlier build".to_owned(),
    };
    Some(format!(
        "The Julia running from {} was started by another version of endeavor ({which}; this is build {this}). It keeps working as it was started. To use this version, run `endeavor stop`, then start it again.",
        dir.display()
    ))
}

static STOP: AtomicBool = AtomicBool::new(false);

/// Take Ctrl-C, SIGTERM and SIGHUP on a thread of their own, as a request to
/// stop. Called before any other thread starts, so they all inherit the mask.
#[cfg(unix)]
fn catch_stop_signals() {
    // SAFETY: initializing and applying a signal set on this (still only) thread.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    std::thread::spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` holds only signals blocked in every thread.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 {
                STOP.store(true, Ordering::SeqCst);
            }
        }
    });
}

/// The console window was closed (or the user is logging off): Windows ends
/// the process a few seconds after it says so, too soon to ask Julia to shut down.
#[cfg(windows)]
static CLOSING: AtomicBool = AtomicBool::new(false);

/// Take Ctrl-C, Ctrl-Break and the console closing as a request to stop. The
/// runtime has a console of its own (`crate::start`), so none of them reach it.
#[cfg(windows)]
fn catch_stop_signals() {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
    unsafe extern "system" fn on_console_event(event: u32) -> windows_sys::core::BOOL {
        let closing = event != CTRL_C_EVENT && event != CTRL_BREAK_EVENT;
        // Before STOP, so `serve` sees it when it stops.
        CLOSING.fetch_or(closing, Ordering::SeqCst);
        STOP.store(true, Ordering::SeqCst);
        if closing {
            // Windows ends the process once this returns; `serve` exits when Julia is stopped.
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        1
    }
    // SAFETY: a handler that only touches atomics and sleeps, for the life of the process.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
        eprintln!("endeavor: couldn't take Ctrl-C ({}); `endeavor stop` stops Julia", io::Error::last_os_error());
    }
}

#[cfg(windows)]
fn closing() -> bool {
    CLOSING.load(Ordering::SeqCst)
}

#[cfg(unix)]
fn closing() -> bool {
    false
}

fn serve(options: Options) -> ! {
    catch_stop_signals();
    let stopping = || STOP.load(Ordering::SeqCst);
    let up = start_or_reuse(&options, false, &|line| eprintln!("{line}"), &stopping).unwrap_or_else(|e| {
        eprintln!("endeavor: {e}");
        std::process::exit(1)
    });
    let dir = &options.state_dir;
    let folder = recorded_folder(dir).unwrap_or_else(|| options.folder.display().to_string());
    if up.started.is_none() {
        eprintln!("Julia was already running from {} (pid {}); using it as it was started.", dir.display(), up.state.pid);
        if options.port != 0 && options.port != up.port {
            eprintln!("It listens on port {}, not {}. To change that, stop it first (`endeavor stop`).", up.port, options.port);
        }
        if folder != options.folder.display().to_string() {
            eprintln!("Its notebooks folder is {folder}.");
        }
        if let Some(message) = other_build(dir) {
            eprintln!("{message}");
        }
    }
    let login = std::env::var("SLURM_SUBMIT_HOST").ok().filter(|_| std::env::var_os("SLURM_JOB_ID").is_some());
    let node = crate::hostname();
    print!("\n{}", connection_text(&Connection { port: up.port, token: &up.state.token, node: &node, folder: &folder, login: login.as_deref() }));
    match &up.started {
        Some(_) => println!("Press Ctrl-C to stop Julia."),
        None => println!("Ctrl-C leaves this Julia running; `endeavor stop` ends it."),
    }
    let _ = io::stdout().flush();
    while !stopping() {
        if !crate::pid_alive(up.state.pid, up.state.started) {
            match crate::stopped::why(dir, crate::stopped::Of::Runtime(up.state.pid)) {
                Some(crate::stopped::How::Stop) => eprintln!("Julia was stopped with `endeavor stop`."),
                Some(crate::stopped::How::Connection) => eprintln!("Julia was stopped from another connection."),
                None => {
                    eprintln!("Julia stopped. Its log is {}.", dir.join("runtime.log").display());
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if let Some(runtime) = &up.started {
        eprintln!("Stopping Julia…");
        if closing() {
            runtime.kill();
        } else {
            runtime.stop(Some(&up.state));
        }
        eprintln!("Stopped.");
    }
    std::process::exit(0)
}

/// What ending the runtime recorded in a state folder came to.
enum Ended {
    /// None was recorded, or its process is gone.
    NotRunning,
    /// It runs on another machine (its name).
    Elsewhere(String),
    /// Stopped (its pid).
    Stopped(i32),
    /// Still running after the stop (its pid).
    Alive(i32),
}

/// End the runtime recorded in `dir`, as `endeavor stop` does.
fn end_runtime(dir: &Path) -> Ended {
    let Some(state) = crate::read_state(dir) else { return Ended::NotRunning };
    let here = crate::hostname();
    if state.node != here {
        return Ended::Elsewhere(state.node);
    }
    if !crate::pid_alive(state.pid, state.started) {
        crate::remove_state(dir, state.pid);
        return Ended::NotRunning;
    }
    let (events, _) = mpsc::channel();
    if crate::stop_marked(dir, &state, &Runtime::recorded(&state, dir, &events), crate::stopped::How::Stop) { Ended::Stopped(state.pid) } else { Ended::Alive(state.pid) }
}

fn stop(dir: &Path) -> ! {
    match end_runtime(dir) {
        Ended::NotRunning => println!("No Julia is running from {}.", dir.display()),
        Ended::Elsewhere(node) => {
            eprintln!("The Julia recorded in {} runs on {node}, not here ({}). Stop it there.", dir.display(), crate::hostname());
            std::process::exit(1);
        }
        Ended::Alive(pid) => {
            eprintln!("Julia (pid {pid}) is still running.");
            std::process::exit(1);
        }
        Ended::Stopped(pid) => println!("Stopped Julia (pid {pid})."),
    }
    std::process::exit(0)
}

/// Where the stdio relay's runtime stands.
enum Status {
    Idle,
    Starting(String),
    Ready { port: u16, token: String },
    Failed(String),
}

/// `mcp`: the agent's MCP messages, one JSON-RPC message per line on stdin,
/// relayed to the runtime's `/mcp` and the answers written to stdout.
struct Relay {
    options: Options,
    status: Mutex<Status>,
    changed: Condvar,
    /// This agent session's key (`X-Endeavor-Session`): the runtime gives each
    /// session one notebook, and tells sessions apart in its warnings. It is
    /// new when the session moves to another runtime (`switch`): a runtime
    /// that has ended a key ignores it afterwards. It goes with `target`: read
    /// and replaced only with that lock held (`placed`, `switch`), so that a call
    /// never has one runtime's route and another's key.
    session: Mutex<String>,
    /// The first key, which the later ones are made from.
    session_base: String,
    /// How many keys the front has made.
    sessions: std::sync::atomic::AtomicU64,
    /// Where this session's notebooks run.
    target: Mutex<machines::Target>,
    /// One machine tool at a time (they switch, start and stop things).
    ops: Mutex<()>,
    /// Said once, in the first result.
    notice: Mutex<Option<String>>,
    machines: crate::client::MachinesFile,
    projects: projects::Projects,
    /// What `initialize` negotiated, for `MCP-Protocol-Version`.
    protocol: Mutex<Option<String>>,
    /// The runtime's `Mcp-Session-Id`, if it gives one.
    mcp_session: Mutex<Option<String>>,
    /// The agent's name from `initialize`, which the runtime shows to other sessions.
    agent: Mutex<Option<String>>,
    out: Mutex<Box<dyn Write + Send>>,
}

fn relay(options: Options) -> ! {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let relay = Arc::new(Relay::new(options, format!("stdio-{}-{}", std::process::id(), now.as_millis()), Box::new(io::stdout())));
    relay.target_from_project();
    if matches!(*relay.target.lock().unwrap(), machines::Target::Local { .. }) {
        relay.start();
    }
    relay.keep_link_alive();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        // Each on its own thread: a run can take minutes, and a cancellation
        // must get through meanwhile.
        let relay = relay.clone();
        std::thread::spawn(move || relay.handle(&line));
    }
    relay.release();
    std::process::exit(0)
}

impl Relay {
    fn new(options: Options, session: String, out: Box<dyn Write + Send>) -> Relay {
        Relay {
            options,
            status: Mutex::new(Status::Idle),
            changed: Condvar::new(),
            session_base: session.clone(),
            session: Mutex::new(session),
            sessions: std::sync::atomic::AtomicU64::new(0),
            target: Mutex::new(machines::Target::Local { stopped: false }),
            ops: Mutex::new(()),
            notice: Mutex::new(None),
            machines: crate::client::MachinesFile::here(),
            projects: projects::Projects::at(Env::here().projects_path()),
            protocol: Mutex::default(),
            mcp_session: Mutex::default(),
            agent: Mutex::default(),
            out: Mutex::new(out),
        }
    }

    /// The target and the session's key, as they are together.
    fn placed(&self) -> (machines::Target, String) {
        let target = self.target.lock().unwrap();
        (target.clone(), self.session.lock().unwrap().clone())
    }

    fn session(&self) -> String {
        self.placed().1
    }

    fn write(&self, message: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    /// Start or reuse the runtime in the background, unless that's under way.
    fn start(self: &Arc<Self>) {
        {
            let mut status = self.status.lock().unwrap();
            if matches!(*status, Status::Starting(_) | Status::Ready { .. }) {
                return;
            }
            *status = Status::Starting(String::new());
        }
        let relay = self.clone();
        std::thread::spawn(move || {
            let progress = |line: &str| {
                eprintln!("{line}");
                if let Status::Starting(last) = &mut *relay.status.lock().unwrap() {
                    *last = line.to_owned();
                }
            };
            let status = match start_or_reuse(&relay.options, true, &progress, &|| false) {
                Ok(up) => {
                    relay.tell_folder(up.port, &up.state.token);
                    eprintln!("Endeavor's notebooks: http://localhost:{}/?token={}", up.port, up.state.token);
                    if let Some(message) = up.started.is_none().then(|| other_build(&relay.options.state_dir)).flatten() {
                        eprintln!("endeavor: {message}");
                    }
                    Status::Ready { port: up.port, token: up.state.token }
                }
                Err(e) => {
                    eprintln!("endeavor: {e}");
                    Status::Failed(e)
                }
            };
            *relay.status.lock().unwrap() = status;
            relay.changed.notify_all();
        });
    }

    /// Tell the runtime something about this session, as the app does for its
    /// sessions with `/endeavor/call`.
    fn tell(port: u16, token: &str, method: &str, params: Value) -> io::Result<(u16, Vec<u8>)> {
        let body = to_json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
        crate::http::post(port, crate::CALL, &headers, body.as_bytes())
    }

    /// Give the runtime this session's folder: the runtime may have been
    /// started from another folder.
    /// Only while the session is on this computer: it is the key there that is told.
    fn tell_folder(&self, port: u16, token: &str) {
        let (target, session) = self.placed();
        if matches!(target, machines::Target::Local { .. }) {
            self.tell_session_folder(port, token, &session, &self.options.folder.display().to_string());
        }
    }

    /// Give the runtime on `port` the folder of session `session`, which is `folder` there.
    fn tell_session_folder(&self, port: u16, token: &str, session: &str, folder: &str) {
        let params = json!({ "owner": session, "folder": folder });
        if let Err(e) = Relay::tell(port, token, "endeavor/set_session_folder", params) {
            eprintln!("endeavor: couldn't give the runtime this session's folder: {e}");
        }
    }

    /// The agent has gone: let the runtime end this session, so other sessions
    /// don't see it as still working in a notebook, and a call still under way
    /// doesn't bind it again. Best effort, and it doesn't hold up the exit for
    /// more than a moment. The link stays.
    fn release(&self) {
        let Some((port, token, session)) = self.current_runtime() else { return };
        self.end_session(port, &token, &session);
    }

    /// End key `session` on the runtime at `port`, without waiting more than a moment.
    fn end_session(&self, port: u16, token: &str, session: &str) {
        let (token, params) = (token.to_owned(), json!({ "owner": session }));
        let (done, told) = mpsc::channel();
        std::thread::spawn(move || drop(done.send(Relay::tell(port, &token, "endeavor/end_session", params))));
        let _ = told.recv_timeout(Duration::from_millis(500));
    }

    /// The port and token of the runtime this session is using, if it is up, and the session's key there, without starting anything.
    fn current_runtime(&self) -> Option<(u16, String, String)> {
        let (target, session) = self.placed();
        let (port, token) = match &target {
            machines::Target::Machine(machine) => self.machine_runtime(machine)?,
            machines::Target::Local { .. } => match &*self.status.lock().unwrap() {
                Status::Ready { port, token } => (*port, token.clone()),
                _ => return None,
            },
        };
        Some((port, token, session))
    }

    /// The runtime's port and token, waiting up to `start_wait()` for a start;
    /// else why it can't be used yet.
    fn runtime(self: &Arc<Self>) -> Result<(u16, String), String> {
        let deadline = Instant::now() + start_wait();
        let mut status = self.status.lock().unwrap();
        loop {
            match &*status {
                Status::Ready { port, token } => return Ok((*port, token.clone())),
                Status::Failed(e) => {
                    let why = format!("Endeavor's Julia couldn't start: {e}");
                    // The next call tries again.
                    *status = Status::Idle;
                    return Err(why);
                }
                Status::Idle => {
                    drop(status);
                    self.start();
                    status = self.status.lock().unwrap();
                }
                Status::Starting(last) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        let last = if last.is_empty() { String::new() } else { format!(" Last step: {last}") };
                        return Err(format!(
                            "Endeavor's Julia is still starting on this machine. The first start installs packages and takes a few minutes. Try again shortly.{last}"
                        ));
                    }
                    status = self.changed.wait_timeout(status, left).unwrap().0;
                }
            }
        }
    }

    /// The runtime went away: start or find it again.
    fn lost(self: &Arc<Self>) {
        if let machines::Target::Machine(machine) = &mut *self.target.lock().unwrap() {
            // The link is gone, or its runtime: the next call asks the link again.
            machine.link = None;
            machine.asked = None;
            return;
        }
        let mut status = self.status.lock().unwrap();
        if matches!(*status, Status::Ready { .. }) {
            *status = Status::Idle;
        }
    }

    fn handle(self: &Arc<Self>, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return self.write(&to_json(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } })));
        };
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        if let Some(reply) = crate::mcp::answer_locally(&message, self.options.skills_plugin) {
            if message["method"] == "initialize" {
                *self.agent.lock().unwrap() = message["params"]["clientInfo"]["name"].as_str().and_then(crate::mcp::clean_label);
                let negotiated: Value = serde_json::from_str(&reply).unwrap_or_default();
                *self.protocol.lock().unwrap() = negotiated["result"]["protocolVersion"].as_str().map(str::to_owned);
            }
            return self.write(&reply);
        }
        // The runtime has nothing to do with it.
        if message["method"] == "notifications/initialized" {
            return;
        }
        let tool = (message["method"] == "tools/call").then(|| message["params"]["name"].as_str().unwrap_or_default().to_owned());
        if let Some(tool) = tool.as_deref().filter(|tool| crate::mcp::MACHINE_NAMES.contains(tool)) {
            return self.machine_tool(&message, tool);
        }
        let failed = |why: String| {
            if let Some(id) = &id {
                self.write(&self.decorate(&message, tool.as_deref(), tool_failure(id, &message, &why)));
            }
        };
        let sink = |reply: String| self.write(&self.decorate(&message, tool.as_deref(), reply));
        for attempt in 0..2 {
            let route = match self.route(tool.as_deref() != Some("pluto_session_status")) {
                Ok(route) => route,
                Err(unready) => return self.unready(&message, tool.as_deref(), unready),
            };
            match self.post(&route, line, &sink) {
                Ok(()) => return,
                Err(Sent::NotConnected(e)) if attempt == 0 => {
                    eprintln!("endeavor: the runtime isn't answering ({e}); starting it again");
                    self.lost();
                }
                Err(Sent::NotConnected(e) | Sent::Failed(e)) => return failed(format!("Endeavor's Julia didn't answer: {e}")),
            }
        }
    }

    /// POST one message to the runtime and give what it answers to `sink`.
    fn post(&self, route: &Route, body: &str, sink: &dyn Fn(String)) -> Result<(), Sent> {
        self.post_within(route, body, sink, None)
    }

    /// `post`, where reading from the runtime gives up when it says nothing for `quiet`.
    fn post_within(&self, route: &Route, body: &str, sink: &dyn Fn(String), quiet: Option<Duration>) -> Result<(), Sent> {
        let port = route.port;
        let token = &route.token;
        let socket = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).map_err(Sent::NotConnected)?;
        let _ = socket.set_nodelay(true);
        let _ = socket.set_read_timeout(quiet);
        let mut head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nX-Endeavor-Session: {}\r\nContent-Length: {}\r\n",
            route.session,
            body.len()
        );
        if let Some(host) = &route.host {
            head.push_str(&format!("X-Endeavor-Host: {host}\r\nX-Endeavor-Browser-Port: {port}\r\n"));
        }
        if self.options.skills_plugin {
            head.push_str("X-Endeavor-Skills: plugin\r\n");
        }
        let agent = self.agent.lock().unwrap().clone().unwrap_or_else(|| "endeavor mcp".into());
        if let Some(label) = crate::mcp::clean_label(&format!("{agent} on {}", crate::hostname())) {
            head.push_str(&format!("X-Endeavor-Client: {label}\r\n"));
        }
        if let Some(version) = &*self.protocol.lock().unwrap() {
            head.push_str(&format!("MCP-Protocol-Version: {version}\r\n"));
        }
        if let Some(id) = &*self.mcp_session.lock().unwrap() {
            head.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
        }
        head.push_str("\r\n");
        let mut out = &socket;
        out.write_all(head.as_bytes()).and_then(|_| out.write_all(body.as_bytes())).map_err(Sent::NotConnected)?;
        let mut reader = BufReader::new(&socket);
        let response = loop {
            let response = Head::read(&mut reader).map_err(Sent::Failed)?.ok_or_else(|| Sent::Failed(io::ErrorKind::UnexpectedEof.into()))?;
            if !(100..200).contains(&response.status()) {
                break response;
            }
        };
        if let Some(id) = response.header("Mcp-Session-Id") {
            *self.mcp_session.lock().unwrap() = Some(id.to_owned());
        }
        let framing = response.response_body("POST").map_err(Sent::Failed)?;
        let body = Body::new(reader, framing);
        let status = response.status();
        let event_stream = response.header("Content-Type").is_some_and(|t| t.starts_with("text/event-stream"));
        match status {
            202 => Ok(()),
            200 if event_stream => {
                for event in events(body) {
                    sink(one_line(&event.map_err(Sent::Failed)?));
                }
                Ok(())
            }
            200 => {
                let mut text = String::new();
                BufReader::new(body).read_to_string(&mut text).map_err(Sent::Failed)?;
                sink(one_line(&text));
                Ok(())
            }
            _ => {
                let mut text = String::new();
                let _ = BufReader::new(body).read_to_string(&mut text);
                Err(Sent::Failed(io::Error::other(format!("HTTP {status} {}", text.trim()))))
            }
        }
    }
}

enum Sent {
    /// The runtime isn't there: worth starting it again.
    NotConnected(io::Error),
    Failed(io::Error),
}

/// The answer to request `id` (`message`) when the runtime can't give one:
/// for a tool call, a failed result the agent reads; else a JSON-RPC error.
fn tool_failure(id: &Value, message: &Value, why: &str) -> String {
    if message["method"] == "tools/call" {
        return to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": why }], "isError": true } }));
    }
    to_json(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": why } }))
}

/// A JSON-RPC message as one line, which stdio's framing needs.
fn one_line(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => to_json(&value),
        Err(_) => text.replace(['\r', '\n'], " "),
    }
}

/// Where a call goes: a runtime's port and token, and for a machine's runtime its name,
/// with the session's key there. They are taken together, so a call that races a move of
/// the session goes whole to the runtime it was routed to.
pub(crate) struct Route {
    pub port: u16,
    pub token: String,
    /// The session's key (`X-Endeavor-Session`).
    pub session: String,
    /// The machine's name (`X-Endeavor-Host`). With it the runtime also gets
    /// the link's port as the browser's (`X-Endeavor-Browser-Port`), which is `port`.
    pub host: Option<String>,
}

/// A response body as its sender framed it, read without the framing.
struct Body<R> {
    inner: R,
    framing: crate::http::Framing,
    /// Bytes left in the current chunk.
    chunk_left: u64,
    first_chunk: bool,
}

impl<R: BufRead> Body<R> {
    fn new(inner: R, framing: crate::http::Framing) -> Body<R> {
        Body { inner, framing, chunk_left: 0, first_chunk: true }
    }
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use crate::http::Framing;
        match &mut self.framing {
            Framing::UntilClose => self.inner.read(buf),
            Framing::Length(left) => {
                let n = (&mut self.inner).take(*left).read(buf)?;
                *left -= n as u64;
                Ok(n)
            }
            Framing::Chunked(_) => {
                if self.chunk_left == 0 {
                    let mut line = String::new();
                    if !self.first_chunk {
                        // The line break after the last chunk's data.
                        self.inner.read_line(&mut line)?;
                        line.clear();
                    }
                    self.first_chunk = false;
                    if self.inner.read_line(&mut line)? == 0 {
                        return Ok(0);
                    }
                    let digits = line.split([';', '\r', '\n']).next().unwrap_or_default().trim();
                    self.chunk_left = u64::from_str_radix(digits, 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
                    if self.chunk_left == 0 {
                        self.framing = Framing::Length(0);
                        return Ok(0);
                    }
                }
                let n = (&mut self.inner).take(self.chunk_left).read(buf)?;
                if n == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                self.chunk_left -= n as u64;
                Ok(n)
            }
        }
    }
}

/// The data of each event in an event stream, as it arrives.
fn events(body: impl Read) -> impl Iterator<Item = io::Result<String>> {
    let mut lines = BufReader::new(body).lines();
    let mut data: Vec<String> = Vec::new();
    std::iter::from_fn(move || {
        loop {
            match lines.next() {
                None if data.is_empty() => return None,
                None => return Some(Ok(std::mem::take(&mut data).join("\n"))),
                Some(Err(e)) => return Some(Err(e)),
                Some(Ok(line)) if line.is_empty() => {
                    if !data.is_empty() {
                        return Some(Ok(std::mem::take(&mut data).join("\n")));
                    }
                }
                Some(Ok(line)) => {
                    if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.strip_prefix(' ').unwrap_or(value).to_owned());
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests;
