//! The notebook tools without the app (docs/standalone.md). The user starts
//! the runtime themselves, as they would Pluto or Jupyter, and points their
//! agent and browser at its one port:
//!
//! - `serve` starts the runtime in the foreground (or uses the one already
//!   running from its state folder) and prints how to connect: the browser
//!   link, the MCP URL and token, agent configs and the `ssh -L` line.
//!   Ctrl-C stops a runtime it started.
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
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::http::Head;
use crate::mcp::to_json;
use crate::{Args, Exit, Launcher, Runtime, State, embedded, julia};

const USAGE: &str = "usage: endeavor-remote serve [OPTIONS]   run Julia here and print how to connect (Ctrl-C stops it)
       endeavor-remote mcp [OPTIONS]     MCP over stdin/stdout for an agent on this machine
       endeavor-remote stop              stop the Julia that serve or mcp started

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

/// How long a relayed call waits for a runtime that is still starting before
/// it says so, under the time agents give a tool call.
const START_WAIT: Duration = Duration::from_secs(45);

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
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        #[cfg(windows)]
        let home = var("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default().join("Endeavor");
        #[cfg(not(windows))]
        let home = std::env::home_dir().unwrap_or_default();
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

    fn cache(&self) -> PathBuf {
        if cfg!(windows) {
            return self.home.join("serve-runtime");
        }
        self.cache_home.clone().unwrap_or_else(|| self.home.join(".cache")).join("endeavor/serve")
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

/// `endeavor-remote serve|mcp|stop …`.
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
/// That folder.
pub fn unpack(cache: &Path, version: &str, files: &[(&str, &[u8])]) -> Result<PathBuf, String> {
    let dir = cache.join(version);
    if dir.is_dir() {
        return Ok(dir);
    }
    let failed = |e: io::Error| format!("Couldn't unpack Endeavor's runtime into {}: {e}", cache.display());
    let part = cache.join(format!("{version}.part.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    for (path, contents) in files {
        let file = part.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).map_err(failed)?;
        std::fs::write(&file, contents).map_err(failed)?;
    }
    match std::fs::rename(&part, &dir) {
        Ok(()) => Ok(dir),
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
        build: Some(embedded::RUNTIME_VERSION.into()),
        core_env: core_env(options, exit_idle),
    };
    if let Some(state) = crate::existing(&args)? {
        let port = state.port.ok_or("The Julia running here was started by an older version of Endeavor. Stop it with `endeavor-remote stop`, then try again.")?;
        return Ok(Up { state, port, started: None });
    }
    args.runtime = unpack_runtime(&options.cache)?;
    let (julia, version) = julia::find(&options.julia, &|line| progress(&line))?;
    progress(&format!("Starting Julia {version} ({julia})"));
    let token = crate::token(dir)?;
    let child = crate::start(&args, &julia, &token)?;
    let (pid, started) = (child.id() as i32, crate::child_started(&child));
    let (events, _) = mpsc::channel();
    let runtime = Runtime { pid, started, exit: Exit::watch_child(child, pid, events), state_dir: dir.clone() };
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

/// Hold `DIR/start.lock` until dropped, waiting for another process's start.
fn start_lock(dir: &Path) -> Result<std::fs::File, String> {
    let path = dir.join("start.lock");
    let file = crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false))
        .open(&path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    while !crate::try_lock(&file) {
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(file)
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

/// The folder a running standalone runtime recorded for its notebooks.
fn recorded_folder(dir: &Path) -> Option<String> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    state["folder"].as_str().map(str::to_owned)
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

/// Not ported: on Windows Ctrl-C ends `serve` at once, and the runtime, in a
/// process group of its own, keeps running until `endeavor-remote stop`.
#[cfg(windows)]
fn catch_stop_signals() {}

fn serve(options: Options) -> ! {
    catch_stop_signals();
    let stopping = || STOP.load(Ordering::SeqCst);
    let up = start_or_reuse(&options, false, &|line| eprintln!("{line}"), &stopping).unwrap_or_else(|e| {
        eprintln!("endeavor-remote: {e}");
        std::process::exit(1)
    });
    let dir = &options.state_dir;
    let folder = recorded_folder(dir).unwrap_or_else(|| options.folder.display().to_string());
    if up.started.is_none() {
        eprintln!("Julia was already running from {} (pid {}); using it as it was started.", dir.display(), up.state.pid);
        if options.port != 0 && options.port != up.port {
            eprintln!("It listens on port {}, not {}. To change that, stop it first (`endeavor-remote stop`).", up.port, options.port);
        }
        if folder != options.folder.display().to_string() {
            eprintln!("Its notebooks folder is {folder}.");
        }
    }
    let login = std::env::var("SLURM_SUBMIT_HOST").ok().filter(|_| std::env::var_os("SLURM_JOB_ID").is_some());
    let node = crate::hostname();
    print!("\n{}", connection_text(&Connection { port: up.port, token: &up.state.token, node: &node, folder: &folder, login: login.as_deref() }));
    match &up.started {
        Some(_) => println!("Press Ctrl-C to stop Julia."),
        None => println!("Ctrl-C leaves this Julia running; `endeavor-remote stop` ends it."),
    }
    let _ = io::stdout().flush();
    while !stopping() {
        if !crate::pid_alive(up.state.pid, up.state.started) {
            eprintln!("Julia stopped. Its log is {}.", dir.join("runtime.log").display());
            std::process::exit(1);
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if let Some(runtime) = &up.started {
        eprintln!("Stopping Julia…");
        runtime.stop(Some(&up.state));
        eprintln!("Stopped.");
    }
    std::process::exit(0)
}

fn stop(dir: &Path) -> ! {
    let Some(state) = crate::read_state(dir) else {
        println!("No Julia is running from {}.", dir.display());
        std::process::exit(0)
    };
    let here = crate::hostname();
    if state.node != here {
        eprintln!("The Julia recorded in {} runs on {}, not here ({here}). Stop it there.", dir.display(), state.node);
        std::process::exit(1);
    }
    if !crate::pid_alive(state.pid, state.started) {
        crate::remove_state(dir, state.pid);
        println!("No Julia is running from {}.", dir.display());
        std::process::exit(0);
    }
    let (events, _) = mpsc::channel();
    Runtime::recorded(&state, dir, &events).stop(Some(&state));
    println!("Stopped Julia (pid {}).", state.pid);
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
    /// session one notebook, and tells sessions apart in its warnings.
    session: String,
    /// What `initialize` negotiated, for `MCP-Protocol-Version`.
    protocol: Mutex<Option<String>>,
    /// The runtime's `Mcp-Session-Id`, if it gives one.
    mcp_session: Mutex<Option<String>>,
    out: Mutex<Box<dyn Write + Send>>,
}

fn relay(options: Options) -> ! {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let relay = Arc::new(Relay::new(options, format!("stdio-{}-{}", std::process::id(), now.as_millis()), Box::new(io::stdout())));
    relay.start();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let relay = relay.clone();
        // Each on its own thread: a run can take minutes, and a cancellation
        // must get through meanwhile.
        std::thread::spawn(move || relay.handle(&line));
    }
    std::process::exit(0)
}

impl Relay {
    fn new(options: Options, session: String, out: Box<dyn Write + Send>) -> Relay {
        Relay { options, status: Mutex::new(Status::Idle), changed: Condvar::new(), session, protocol: Mutex::default(), mcp_session: Mutex::default(), out: Mutex::new(out) }
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
                    Status::Ready { port: up.port, token: up.state.token }
                }
                Err(e) => {
                    eprintln!("endeavor-remote: {e}");
                    Status::Failed(e)
                }
            };
            *relay.status.lock().unwrap() = status;
            relay.changed.notify_all();
        });
    }

    /// Give the runtime this session's folder, as the app does for its
    /// sessions: the runtime may have been started from another folder.
    fn tell_folder(&self, port: u16, token: &str) {
        let params = json!({ "owner": self.session, "folder": self.options.folder.display().to_string() });
        let body = to_json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "endeavor/set_session_folder", "params": params }));
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
        if let Err(e) = crate::http::post(port, crate::CALL, &headers, body.as_bytes()) {
            eprintln!("endeavor-remote: couldn't give the runtime this session's folder: {e}");
        }
    }

    /// The runtime's port and token, waiting up to `START_WAIT` for a start;
    /// else why it can't be used yet.
    fn runtime(self: &Arc<Self>) -> Result<(u16, String), String> {
        let deadline = Instant::now() + START_WAIT;
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
                let negotiated: Value = serde_json::from_str(&reply).unwrap_or_default();
                *self.protocol.lock().unwrap() = negotiated["result"]["protocolVersion"].as_str().map(str::to_owned);
            }
            return self.write(&reply);
        }
        // The runtime has nothing to do with it.
        if message["method"] == "notifications/initialized" {
            return;
        }
        let failed = |why: String| {
            if let Some(id) = &id {
                self.write(&tool_failure(id, &message, &why));
            }
        };
        for attempt in 0..2 {
            let (port, token) = match self.runtime() {
                Ok(runtime) => runtime,
                Err(why) => return failed(why),
            };
            match self.post(port, &token, line) {
                Ok(()) => return,
                Err(Sent::NotConnected(e)) if attempt == 0 => {
                    eprintln!("endeavor-remote: the runtime isn't answering ({e}); starting it again");
                    self.lost();
                }
                Err(Sent::NotConnected(e) | Sent::Failed(e)) => return failed(format!("Endeavor's Julia didn't answer: {e}")),
            }
        }
    }

    /// POST one message to the runtime and write what it answers.
    fn post(&self, port: u16, token: &str, body: &str) -> Result<(), Sent> {
        let socket = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).map_err(Sent::NotConnected)?;
        let _ = socket.set_nodelay(true);
        let mut head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nX-Endeavor-Session: {}\r\nContent-Length: {}\r\n",
            self.session,
            body.len()
        );
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
                    self.write(&one_line(&event.map_err(Sent::Failed)?));
                }
                Ok(())
            }
            200 => {
                let mut text = String::new();
                BufReader::new(body).read_to_string(&mut text).map_err(Sent::Failed)?;
                self.write(&one_line(&text));
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
