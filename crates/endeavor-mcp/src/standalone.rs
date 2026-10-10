//! The notebook tools without the app (docs/serve.md). The user starts
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
//! - `open` lets the user's browser in to it, with the link that holds its token.
//! - `status` says what Endeavor has on this computer, and changes nothing.
//!
//! `runtime/` is built into this binary (build.rs) and unpacked to a folder
//! named by its version on first use, so the binary is all a user installs.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};

use crate::http::Head;
use crate::mcp::to_json;
use crate::notebooks::Folder;
use crate::paths::Env;
use crate::runtime::{self, Ended, Hooks, Looked, Outcome, Up, Waiting, Want};
use crate::{Args, Launcher, embedded, julia, stopped};
use machines::Need;
use target::{Local, Target};

mod browser;
mod machines;
mod projects;
mod status;
mod target;


const USAGE: &str = "usage: endeavor serve [OPTIONS]   run Julia here and print how to connect (Ctrl-C stops it)
       endeavor mcp [OPTIONS]     MCP over stdin/stdout for an agent on this machine
       endeavor stop [--force]    stop the Julia that serve or mcp started; --force cancels a start under way
       endeavor status [--json]   show what Endeavor has on this computer; changes nothing
       endeavor open              open the notebooks of the Julia that serve or mcp started in your browser

options:
  --folder DIR         where new notebooks go (default: the current folder)
  --no-folder          the agent is not told a project folder: its notebook paths must be absolute [mcp]
  --port PORT          the port to listen on, on 127.0.0.1 (default: a free one) [serve, mcp]
  --host-tools         give every agent session list_folder, read_file and run_shell here [serve]
  --skills plugin      the agent has Endeavor's skills from its plugin [mcp]
  --julia PATH|auto    the julia to use (default auto: your login shell's, else Endeavor's own download)
  --julia-shell LINE   a shell line that puts julia on the PATH, such as 'module load julia'
  --r RSCRIPT|auto     the R for R notebooks (default auto: your login shell's Rscript) [serve, mcp]
  --r-shell LINE       a shell line that puts Rscript on the PATH, such as 'module load R' [serve, mcp]
  --depot DEPOT        JULIA_DEPOT_PATH (default ~/.cache/endeavor/depot:, or $SCRATCH/endeavor/depot:)
  --idle-stop HOURS    stop notebooks unused this long; 0 never (default 48)
  --state-dir DIR      the runtime's state (default ~/.local/state/endeavor/serve/<host>) [serve, mcp, stop, status, open]
  --json               print the facts as one JSON object [status]
  --force              end Julia while it is still starting, with what it began [stop]
";

/// A notebook this session opened in the browser this recently isn't opened again: agents call
/// `open_notebook` on an open notebook to get its id, sometimes several times in a turn.
const REOPEN_AFTER: Duration = Duration::from_secs(10);

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
    Stop { state_dir: PathBuf, force: bool },
    Status { state_dir: PathBuf, json: bool },
    Open { state_dir: PathBuf },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Options {
    /// The runtime's `runtime.json`, token and log.
    state_dir: PathBuf,
    /// Where `runtime/` is unpacked, a folder per version.
    cache: PathBuf,
    julia: julia::Source,
    /// Julia is found and started only when something needs it, so an agent that only opens R
    /// notebooks never needs Julia. Always so for `serve` and `mcp`, except in tests of a start that
    /// waits for Julia: a debug build reads ENDEAVOR_TEST_JULIA_AT_START.
    julia_when_needed: bool,
    /// The R for R notebooks.
    r: crate::r::Source,
    /// JULIA_DEPOT_PATH.
    depot: String,
    /// Where notebooks are created, and relative paths start; none for `--no-folder`.
    folder: Option<PathBuf>,
    /// The runtime's port; 0 picks a free one.
    port: u16,
    /// Every MCP session gets the host tools (`serve` only).
    host_tools: bool,
    idle_hours: f64,
    /// The agent loads the skills from the plugin (`mcp` only).
    skills_plugin: bool,
}

pub(crate) fn parse(argv: &[String], env: &Env) -> Result<Command, String> {
    let (command, rest) = argv.split_first().ok_or("expected serve, mcp, stop, status or open")?;
    let (mut state_dir, mut julia, mut depot, mut folder, mut port, mut r) = (None, None::<julia::Source>, None, None, 0, None);
    let (mut host_tools, mut idle_hours, mut skills_plugin, mut json, mut force, mut no_folder) = (false, crate::notebooks::IDLE_HOURS, false, false, false, false);
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
            "--r" | "--r-shell" if r.is_some() => return Err("give one of --r and --r-shell".into()),
            "--r" | "--r-shell" => {
                only(&["serve", "mcp"])?;
                r = Some(crate::r::Source::from_flag(arg, value()?))
            }
            "--depot" => {
                only(&["serve", "mcp"])?;
                depot = Some(value()?)
            }
            "--folder" => {
                only(&["serve", "mcp"])?;
                folder = Some(PathBuf::from(value()?))
            }
            "--no-folder" => {
                only(&["mcp"])?;
                no_folder = true
            }
            "--port" => {
                only(&["serve", "mcp"])?;
                port = value()?.parse().map_err(|_| "--port needs a port number".to_owned())?
            }
            "--idle-stop" => {
                only(&["serve", "mcp"])?;
                idle_hours = value()?.parse().ok().filter(|h: &f64| *h >= 0.0).ok_or("--idle-stop needs a number of hours (0: never)")?
            }
            "--json" => {
                only(&["status"])?;
                json = true
            }
            "--force" => {
                only(&["stop"])?;
                force = true
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
    match command.as_str() {
        "stop" => return Ok(Command::Stop { state_dir, force }),
        "status" => return Ok(Command::Status { state_dir, json }),
        "open" => return Ok(Command::Open { state_dir }),
        _ => {}
    }
    let folder = match folder {
        Some(_) if no_folder => return Err("give one of --folder and --no-folder".into()),
        Some(folder) if folder.is_relative() => Some(env.cwd.join(folder)),
        Some(folder) => Some(folder),
        None if no_folder => None,
        None => Some(env.cwd.clone()),
    };
    let options = Options {
        state_dir,
        cache: env.cache(),
        julia: julia.unwrap_or(julia::Source::Auto),
        julia_when_needed: !(cfg!(debug_assertions) && std::env::var_os("ENDEAVOR_TEST_JULIA_AT_START").is_some()),
        r: r.unwrap_or_default(),
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

/// `endeavor serve|mcp|stop|status|open …`.
pub fn main(argv: &[String]) -> ! {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        std::process::exit(0);
    }
    let env = Env::here();
    let command = parse(argv, &env).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    });
    match command {
        Command::Serve(options) => serve(options),
        Command::Mcp(options) => relay(options),
        Command::Stop { state_dir, force } => stop(&state_dir, force),
        Command::Status { state_dir, json } => status::main(&env, &state_dir, json),
        Command::Open { state_dir } => open(&state_dir),
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

fn runtime_args(options: &Options, exit_idle: bool) -> Args {
    Args {
        state_dir: options.state_dir.clone(),
        julia: options.julia.clone(),
        julia_when_needed: options.julia_when_needed,
        r: options.r.clone(),
        // `serve` and `mcp` run on the user's own computer.
        own_r: true,
        runtime: PathBuf::new(),
        depot: options.depot.clone(),
        launcher: Launcher::Process,
        quit_with_client: false,
        own_with_client: false,
        any_node: false,
        build: Some(embedded::BUILD_VERSION.into()),
        exit_idle,
        core_env: core_env(options, exit_idle),
    }
}

/// What `serve` and `mcp` say while a start goes on: its lines to `progress`, and the wait ends when `cancelled` says so.
struct Saying<'a> {
    progress: &'a dyn Fn(&str),
    cancelled: &'a dyn Fn() -> bool,
}

impl Hooks for Saying<'_> {
    fn progress(&mut self, line: String) {
        (self.progress)(&line);
    }

    fn found(&mut self, version: &str, path: &str) {
        (self.progress)(&format!("Starting Julia {version} ({path})"));
    }

    fn wait(&mut self, wait: Duration, _: Waiting) -> bool {
        std::thread::sleep(wait);
        !(self.cancelled)()
    }
}

/// The runtime running from the state folder, or a new one, once it answers (`runtime::find_or_start`).
/// `progress` hears the start's log; `cancelled` ends a start early, and a runtime that was starting with it.
fn start_or_reuse(options: &Options, exit_idle: bool, progress: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<Up, String> {
    let args = runtime_args(options, exit_idle);
    let (events, _) = mpsc::channel();
    let want = Want { args: &args, engine: wire::ENGINE_PLUTO, install: true, runtime: &|| unpack_runtime(&options.cache), events: &events, attach_only: false };
    match runtime::find_or_start(&want, &mut Saying { progress, cancelled }) {
        Outcome::Ready(up) => Ok(up),
        Outcome::Unusable(Looked::OtherNode(state)) => Err(runtime::other_node_text(&state.node)),
        Outcome::Unusable(_) => Err(OLDER_RUNTIME_HERE.into()),
        Outcome::NeedsInstall(items) => Err(items.into_iter().next().map_or_else(String::new, |item| julia::Failure::Missing(item).message())),
        Outcome::Failed(message) => Err(message),
        Outcome::Died { status, log_tail } => {
            let tail = log_tail[log_tail.len().saturating_sub(8)..].join("\n");
            Err(format!("Julia stopped while starting ({status}). The end of {}:\n{tail}", options.state_dir.join("runtime.log").display()))
        }
        Outcome::Cancelled => Err("Stopped before Julia was ready.".into()),
        Outcome::NothingRunning => Err("No Julia is running.".into()),
    }
}

/// Why a runtime from a build before one port per runtime can't be used here.
const OLDER_RUNTIME_HERE: &str = "The Julia running here was started by an older version of Endeavor. Stop it with `endeavor stop`, then try again.";

/// The core's environment for a standalone runtime (see `core::main`).
fn core_env(options: &Options, exit_idle: bool) -> Vec<(&'static str, Option<String>)> {
    let mut env = vec![("ENDEAVOR_IDLE_HOURS", Some(options.idle_hours.to_string()))];
    // The other of the two is cleared: the core lets a folder win over the lack of one.
    match &options.folder {
        Some(folder) => env.extend([("ENDEAVOR_FOLDER", Some(folder.display().to_string())), ("ENDEAVOR_NO_FOLDER", None)]),
        None => env.extend([("ENDEAVOR_NO_FOLDER", Some("1".into())), ("ENDEAVOR_FOLDER", None)]),
    }
    if options.port != 0 {
        env.push(("ENDEAVOR_PORT", Some(options.port.to_string())));
    }
    if options.host_tools {
        env.push(("ENDEAVOR_HOST_TOOLS", Some(crate::hostname())));
    }
    if exit_idle {
        env.push(("ENDEAVOR_EXIT_IDLE", Some("1".into())));
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

/// Hold `DIR/start.lock` to stop the runtime, waiting up to `stop_lock_limit()` for a start under way.
pub(crate) fn stop_lock(dir: &Path) -> Result<std::fs::File, String> {
    let pause = || {
        std::thread::sleep(Duration::from_millis(200));
        Ok::<(), std::convert::Infallible>(())
    };
    wait_for_start_lock(dir, stop_lock_limit(), pause).map_err(|wait| match wait {
        Wait::Failed(message) => message,
        Wait::TimedOut => stop_lock_gave_up(dir),
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

/// How to reach a runtime, for `connection_text`.
pub(crate) struct Connection<'a> {
    pub port: u16,
    pub token: &'a str,
    /// This machine, as `ssh` from the user's computer names it (its host name).
    pub node: &'a str,
    /// Where new notebooks without a path go.
    pub folder: &'a str,
    /// The runtime has a project folder; if not, `folder` is the home folder it works in.
    pub project: bool,
    /// In a Slurm job, the login node it was submitted from (`SLURM_SUBMIT_HOST`).
    pub login: Option<&'a str>,
}

/// What `serve` prints once the runtime answers.
pub(crate) fn connection_text(c: &Connection) -> String {
    let Connection { port, token, node, folder, project, .. } = c;
    let url = format!("http://localhost:{port}/mcp");
    let bearer = format!("Bearer {token}");
    let json = to_json(&json!({ "mcpServers": { "endeavor": { "type": "http", "url": url, "headers": { "Authorization": bearer } } } }));
    let mut ssh = format!("    ssh -L {port}:localhost:{port} {node}\n");
    if let Some(login) = c.login.filter(|login| login != node) {
        ssh = format!("    ssh -J {login} -L {port}:localhost:{port} {node}\n(This is a cluster's compute node: the jump goes through the login node, {login}; use the name you ssh to.)\n");
    }
    let folder = if *project { format!("New notebooks go in {folder}.") } else { format!("This runtime has no project folder: new notebooks without a path go in {folder}, and agents give absolute paths.") };
    format!(
        "Endeavor's notebooks are running on {node}, port {port}. {folder}

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

/// The folder a running standalone runtime recorded for its notebooks, and whether it is a project
/// folder (`false`: it was started without one, and this is the home folder it works in).
fn recorded_folder(dir: &Path) -> Option<(String, bool)> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    Some((state["folder"].as_str()?.to_owned(), state["no_folder"] != true))
}

/// What to tell the user when the runtime recorded in `dir` came from
/// another build than this binary (`embedded::BUILD_VERSION`) and doesn't offer
/// its interface (`core::INTERFACE`); none when it's this build's or offers it,
/// or nothing is recorded. The runtime keeps working as it was started:
/// stopping it is the user's call.
pub(crate) fn other_build(dir: &Path) -> Option<String> {
    other_build_than(dir, embedded::BUILD_VERSION, Some(crate::core::INTERFACE))
}

/// `other_build`, against build `this` and its interface, when that is known.
pub(crate) fn other_build_than(dir: &Path, this: &str, interface: Option<u32>) -> Option<String> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    if interface.is_some() && state["interface"].as_u64() == interface.map(u64::from) {
        return None;
    }
    let which = match state["build"].as_str() {
        Some(build) if build == this => return None,
        Some(build) => format!("build {build}"),
        None => "an earlier build".to_owned(),
    };
    let version = crate::which_version(state["interface"].as_u64().and_then(|n| u32::try_from(n).ok()), interface);
    Some(format!(
        "The Julia running from {} was started by {version} version of endeavor ({which}; this is build {this}). It keeps working as it was started. To use this version, run `endeavor stop`, then start it again.",
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
    let started_with = options.folder.as_ref().map(|folder| folder.display().to_string()).unwrap_or_default();
    let (folder, project) = recorded_folder(dir).unwrap_or_else(|| (started_with.clone(), true));
    if !up.started {
        eprintln!("Endeavor was already running from {} (pid {}); using it as it was started.", dir.display(), up.state.pid);
        if options.port != 0 && options.port != up.port {
            eprintln!("It listens on port {}, not {}. To change that, stop it first (`endeavor stop`).", up.port, options.port);
        }
        if !project {
            eprintln!("It was started without a project folder: new notebooks without a path go in {folder}.");
        } else if folder != started_with {
            eprintln!("Its notebooks folder is {folder}.");
        }
        if let Some(message) = other_build(dir) {
            eprintln!("{message}");
        }
    }
    let login = std::env::var("SLURM_SUBMIT_HOST").ok().filter(|_| std::env::var_os("SLURM_JOB_ID").is_some());
    let node = crate::hostname();
    print!("\n{}", connection_text(&Connection { port: up.port, token: &up.state.token, node: &node, folder: &folder, project, login: login.as_deref() }));
    if up.started {
        println!("Press Ctrl-C to stop Endeavor.");
    } else {
        println!("Ctrl-C leaves Endeavor running; `endeavor stop` ends it.");
    }
    let _ = io::stdout().flush();
    while !stopping() {
        if !crate::pid_alive(up.state.pid, up.state.started, up.state.boot.as_deref()) {
            match crate::stopped::why(dir, crate::stopped::Of::Runtime(up.state.pid)) {
                Some(crate::stopped::How::Stop) => eprintln!("Endeavor was stopped with `endeavor stop`."),
                Some(crate::stopped::How::Connection) => eprintln!("Endeavor was stopped from another connection."),
                None => {
                    eprintln!("Endeavor stopped. Its log is {}.", dir.join("runtime.log").display());
                    std::process::exit(1);
                }
            }
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if up.started {
        eprintln!("Stopping Endeavor…");
        if closing() {
            up.runtime.kill();
        } else {
            up.runtime.stop(Some(&up.state));
        }
        eprintln!("Stopped.");
    }
    std::process::exit(0)
}

fn stop(dir: &Path, force: bool) -> ! {
    let failed = |message: String| -> ! {
        eprintln!("{message}");
        std::process::exit(1)
    };
    let _starting = stop_lock(dir).unwrap_or_else(|why| failed(why));
    let (events, _) = mpsc::channel();
    match runtime::end(dir, false, stopped::How::Stop, force, &events) {
        Ended::NotRunning => println!("No Julia is running from {}.", dir.display()),
        Ended::Elsewhere(node) => failed(format!("The Julia recorded in {} runs on {node}, not here ({}). Stop it there.", dir.display(), crate::hostname())),
        Ended::Alive(pid) => failed(format!("Julia (pid {pid}) is still running.")),
        Ended::Starting => failed(format!("Julia is still starting in {}. `endeavor stop --force` cancels the start.", dir.display())),
        Ended::Unidentified => failed(runtime::START_UNIDENTIFIED.into()),
        Ended::Cancelled(pid) => println!("Cancelled the start of Julia (pid {pid})."),
        Ended::Stopped(pid) => println!("Stopped Julia (pid {pid})."),
    }
    std::process::exit(0)
}

/// `open`: let the user's browser in to the runtime running from `dir`, with the link that holds its
/// token, which tool results don't carry. The browser is opened when this computer shows one;
/// otherwise the link is printed, but only to a terminal: an agent that runs this in its shell would
/// otherwise read the token and could quote it.
fn open(dir: &Path) -> ! {
    let failed = |message: String| -> ! {
        eprintln!("{message}");
        std::process::exit(1)
    };
    match runtime::look(dir, false, true) {
        Looked::Running(state, port) => {
            let link = crate::mcp::entry_link(&crate::mcp::browser_link(port, "/"), &state.token);
            use std::io::IsTerminal;
            if browser::open(&link) {
                println!("Opened Endeavor's notebooks in your browser.");
            } else if io::stdout().is_terminal() {
                println!("Open this link in your browser. It holds the notebooks' key: don't share it.\n    {link}");
            } else {
                failed("No browser could be opened here. Run `endeavor open` in your own terminal: the link it prints holds the notebooks' key, so it isn't printed anywhere else.".into());
            }
        }
        Looked::OtherNode(state) => failed(format!("The Julia recorded in {} runs on {}, not here ({}). Run `endeavor open` there.", dir.display(), state.node, crate::hostname())),
        Looked::Silent(_) => failed("Julia is running but not answering. Try again in a moment.".into()),
        Looked::Older(_) => failed(format!("The Julia running from {} was started by an older Endeavor, which has no page for a browser. `endeavor stop` stops it.", dir.display())),
        Looked::NotRunning | Looked::Dead(_) => failed(format!("No Julia is running from {}.", dir.display())),
    }
    std::process::exit(0)
}

/// `mcp`: the agent's MCP messages, one JSON-RPC message per line on stdin,
/// relayed to the runtime's `/mcp` and the answers written to stdout.
struct Relay {
    options: Options,
    /// This agent session's key (`X-Endeavor-Session`): each runtime gives the
    /// session one notebook under it, and tells sessions apart by it. The same
    /// key goes to every runtime the session uses.
    session: String,
    /// Where this session's notebooks run.
    target: Mutex<Target>,
    /// This computer's runtime, which the target is when it is on this computer.
    local: Arc<Local>,
    /// The connections to the machines this session has used.
    connections: machines::Connections,
    /// One machine tool at a time (they switch, start and stop things).
    ops: Mutex<()>,
    /// Said once, in the first result.
    notice: Mutex<Option<String>>,
    /// The machines' runtimes (machine id and pid) the agent was told came from another build.
    told_other_build: Mutex<std::collections::HashSet<(String, u32)>>,
    machines: crate::client::MachinesFile,
    projects: projects::Projects,
    /// When this session last opened each notebook in the user's browser, by the browser's port and the
    /// notebook's id: an agent that calls `open_notebook` again at once, for its id, opens no second tab.
    opened: Mutex<std::collections::HashMap<(u16, String), Instant>>,
    /// What `initialize` negotiated, for `MCP-Protocol-Version`.
    protocol: Mutex<Option<String>>,
    /// The runtime's `Mcp-Session-Id`, if it gives one.
    mcp_session: Mutex<Option<String>>,
    out: Mutex<Box<dyn Write + Send>>,
}

fn relay(options: Options) -> ! {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let relay = Arc::new(Relay::new(options, format!("stdio-{}-{}", std::process::id(), now.as_millis()), Box::new(io::stdout())));
    relay.target_from_project();
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
    relay.connections.close_all();
    std::process::exit(0)
}

impl Relay {
    fn new(options: Options, session: String, out: Box<dyn Write + Send>) -> Relay {
        Relay {
            target: Mutex::new(Target::local(options.folder.as_deref())),
            local: Arc::new(Local::new(options.clone())),
            options,
            session,
            connections: machines::Connections::default(),
            ops: Mutex::new(()),
            notice: Mutex::new(None),
            told_other_build: Mutex::default(),
            machines: crate::client::MachinesFile::here(),
            projects: projects::Projects::at(Env::here().projects_path()),
            opened: Mutex::default(),
            protocol: Mutex::default(),
            mcp_session: Mutex::default(),
            out: Mutex::new(out),
        }
    }

    fn current(&self) -> Target {
        self.target.lock().unwrap().clone()
    }

    fn write(&self, message: &str) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    /// Tell the runtime something about this session, as the app does for its
    /// sessions with `/endeavor/call`. It fails with `TimedOut` if the exchange isn't done by `deadline`.
    fn tell(port: u16, token: &str, method: &str, params: Value, deadline: Option<Instant>) -> io::Result<(u16, Vec<u8>)> {
        let body = to_json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
        crate::http::post_by(port, crate::CALL, &headers, body.as_bytes(), deadline)
    }

    /// Give the runtime on `port` this session's folder, which is `folder` there: the
    /// runtime may have been started from another folder. With none, the session is told it has no
    /// project folder, so that relative paths are refused. Whether the runtime took it.
    fn tell_session_folder(&self, port: u16, token: &str, folder: Option<&str>) -> bool {
        let params = match folder {
            Some(folder) => json!({ "owner": self.session, "folder": folder }),
            None => json!({ "owner": self.session, "no_folder": true }),
        };
        match Relay::tell(port, token, "endeavor/set_session_folder", params, None) {
            Ok((200, _)) => true,
            Ok((status, _)) => {
                eprintln!("endeavor: couldn't give the runtime this session's folder: HTTP {status}");
                false
            }
            Err(e) => {
                eprintln!("endeavor: couldn't give the runtime this session's folder: {e}");
                false
            }
        }
    }

    /// The session's folder on this computer, when `target` is this computer: its project folder, or
    /// `Unknown` for a front started with `--no-folder`. None on a machine, whose folder the front does
    /// not know until the machine says (`ready`).
    fn local_folder(&self, target: &Target) -> Option<Folder> {
        target.is_local().then(|| self.options.folder.as_ref().map_or(Folder::Unknown, |folder| Folder::In(folder.display().to_string())))
    }

    fn handle(self: &Arc<Self>, line: &str) {
        // The call's whole time, from the moment it arrived: every wait for a runtime comes out of it.
        let deadline = machines::Deadline::after(start_wait());
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return self.write(&to_json(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } })));
        };
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        if let Some(reply) = crate::mcp::answer_locally(&message, self.options.skills_plugin, self.options.folder.is_none()) {
            if message["method"] == "initialize" {
                let negotiated: Value = serde_json::from_str(&reply).unwrap_or_default();
                *self.protocol.lock().unwrap() = negotiated["result"]["protocolVersion"].as_str().map(str::to_owned);
            }
            return self.write(&reply);
        }
        // The runtime has nothing to do with it.
        if message["method"] == "notifications/initialized" {
            return;
        }
        let tool = (message["method"] == "tools/call").then(|| crate::mcp::current_name(message["params"]["name"].as_str().unwrap_or_default()).to_owned());
        if let Some(tool) = tool.as_deref().filter(|tool| crate::mcp::MACHINE_NAMES.contains(tool)) {
            return self.machine_tool(&message, tool, deadline);
        }
        let help = !self.options.skills_plugin;
        let target = self.current();
        // On this computer the front answers what needs no runtime.
        if target.is_local() && let Some(tool) = tool.as_deref().filter(|tool| *tool == crate::guide::TOOL || crate::host_tools::NAMES.contains(tool)) {
            if id.is_none() {
                return;
            }
            // The host-tool refusal comes before the check of the arguments, as the runtime's does.
            let result = crate::mcp::call_parts(&message["params"], help, true).and_then(|(_, arguments)| {
                if tool != crate::guide::TOOL {
                    return Err(crate::mcp::tool_error(&crate::mcp::host_tool_refusal(tool), false));
                }
                crate::mcp::check_arguments(tool, &arguments, help)?;
                crate::guide::read(&arguments).map(|guide| machines::text_result(&guide)).map_err(|error| crate::mcp::tool_error(&error, false))
            });
            return self.answer_call(&message, Some(tool), result.unwrap_or_else(|failed| failed));
        }
        // The front lists the tools of its own build, so it checks every call against that, whether or
        // not a runtime is up; the runtime checks against its own.
        if let Some(tool) = tool.as_deref() {
            let checked = crate::mcp::checked_call(&message["params"], help, true).and_then(|(_, arguments)| {
                // A path the runtime would refuse is refused here, before a runtime is started for it.
                match self.local_folder(&target).and_then(|folder| crate::notebooks::path_refusal(tool, &arguments, &folder)) {
                    Some((refusal, says_what_to_do)) => Err(crate::mcp::tool_error(&refusal, help && !says_what_to_do)),
                    None => Ok(()),
                }
            });
            if let Err(result) = checked {
                if id.is_some() {
                    self.answer_call(&message, Some(tool), result);
                }
                return;
            }
        }
        // Only a call of a tool this build has starts a runtime.
        if !self.held(&target).is_some_and(|provider| matches!(provider.status().state, crate::client::State::Ready(_))) {
            let Some(id) = &id else { return };
            if tool.is_none() {
                if let Some(method) = message["method"].as_str() {
                    self.write(&self.decorate(&message, None, crate::mcp::method_not_found(id, method)));
                }
                return;
            }
        }
        let failed = |why: String| {
            if let Some(id) = &id {
                self.write(&self.decorate(&message, tool.as_deref(), tool_failure(id, &message, &why)));
            }
        };
        // These two use a runtime that is running, and start none.
        let need = match tool.as_deref() {
            Some("session_status") => Need::Peek,
            Some("list_notebooks") => Need::Look,
            _ => Need::Start,
        };
        for attempt in 0..2 {
            let route = match self.route(need, deadline) {
                Ok(route) => route,
                Err(unready) => return self.unready(&message, tool.as_deref(), unready),
            };
            let sink = |reply: String| self.write(&self.decorate(&message, tool.as_deref(), self.in_browser(&message, tool.as_deref(), &route, reply)));
            match self.post(&route, line, &sink) {
                Ok(()) => return,
                Err(Sent::NotConnected(e)) if attempt == 0 => {
                    eprintln!("endeavor: the runtime isn't answering ({e}); looking for it again");
                }
                Err(Sent::NotConnected(e) | Sent::Failed(e)) => return failed(format!("Endeavor's Julia didn't answer: {e}")),
            }
        }
    }

    /// A runtime's reply to the agent's call, with no token in its `browser_url` (a runtime from
    /// before results stopped carrying it still adds one), and the notebook that `new_notebook` or
    /// `open_notebook` made or opened opened in the user's browser, with the link that lets the
    /// browser in. Each such call opens it, so that asking the agent to open it again works, except
    /// within `REOPEN_AFTER` of opening the same one. `opened_in_browser` says whether it is open.
    fn in_browser(&self, message: &Value, tool: Option<&str>, route: &Route, reply: String) -> String {
        let Ok(mut parsed) = serde_json::from_str::<Value>(&reply) else { return reply };
        if parsed["id"] != message["id"] || parsed["result"]["isError"] != false {
            return reply;
        }
        let Some(Value::Object(mut fields)) = parsed["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok()) else { return reply };
        let Some(url) = fields.get("browser_url").and_then(Value::as_str).map(crate::mcp::without_token) else { return reply };
        if matches!(tool, Some("new_notebook" | "open_notebook")) {
            let key = (route.port, fields.get("notebook_id").and_then(Value::as_str).unwrap_or_default().to_owned());
            let just = self.opened.lock().unwrap().get(&key).is_some_and(|at| at.elapsed() < REOPEN_AFTER);
            let opened = just || browser::open(&crate::mcp::entry_link(&url, &route.token));
            if opened && !just {
                self.opened.lock().unwrap().insert(key, Instant::now());
            }
            fields.insert("opened_in_browser".into(), opened.into());
        }
        fields.insert("browser_url".into(), url.into());
        parsed["result"]["content"][0]["text"] = to_json(&Value::Object(fields)).into();
        to_json(&parsed)
    }

    /// POST one message to the runtime and give what it answers to `sink`.
    fn post(&self, route: &Route, body: &str, sink: &dyn Fn(String)) -> Result<(), Sent> {
        let port = route.port;
        let token = &route.token;
        let socket = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).map_err(Sent::NotConnected)?;
        let _ = socket.set_nodelay(true);
        let mut head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nX-Endeavor-Session: {}\r\nContent-Length: {}\r\n",
            self.session,
            body.len()
        );
        if let Some(host) = &route.host {
            head.push_str(&format!("X-Endeavor-Host: {host}\r\nX-Endeavor-Browser-Port: {port}\r\n"));
        }
        if self.options.skills_plugin {
            head.push_str("X-Endeavor-Skills: plugin\r\n");
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

/// Where a call goes: a runtime's port and token, and for a machine's runtime its name.
/// They are taken together, so a call that races a move of the session goes whole to the
/// runtime it was routed to.
pub(crate) struct Route {
    pub port: u16,
    pub token: String,
    /// The machine's name (`X-Endeavor-Host`). With it the runtime also gets
    /// the connection's port as the browser's (`X-Endeavor-Browser-Port`), which is `port`.
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
