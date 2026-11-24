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
pub mod claude_mode;
mod consent;
mod install_page;
mod signin_page;
mod machines;
mod projects;
mod status;
mod target;

const USAGE: &str = "usage: endeavor serve [OPTIONS]   run Endeavor here and print how to connect (Ctrl-C stops it)
       endeavor mcp [OPTIONS]     MCP over stdin/stdout for an agent on this machine
       endeavor stop [--force]    stop what serve or mcp started; --force cancels a start under way
       endeavor status [--json]   show what Endeavor has on this computer; changes nothing
       endeavor open [--port PORT]  open the notebooks that serve or mcp started in your browser

options:
  --folder DIR         where new notebooks go (default: the current folder)
  --no-folder          the agent is not told a project folder: its notebook paths must be absolute, unless a call names one as Codex's do [mcp]
  --port PORT          the port to listen on, on 127.0.0.1 (default: a free one) [serve, mcp];
                       for open, the port an agent's session forwards to a machine's notebooks
  --host-tools         give every agent session list_folder, read_file and run_shell here [serve]
  --skills plugin      the agent has Endeavor's skills from its plugin [mcp]
  --julia PATH|auto|own  the julia to use (default auto: your login shell's, else Endeavor's own;
                       own: Endeavor's own Julia 1.12.6 only, with juliaup when you have it)
  --julia-shell LINE   a shell line that puts julia on the PATH, such as 'module load julia'
  --r RSCRIPT|auto     the R for R notebooks (default auto: your login shell's Rscript) [serve, mcp]
  --r-shell LINE       a shell line that puts Rscript on the PATH, such as 'module load R' [serve, mcp]
  --depot DEPOT        JULIA_DEPOT_PATH (default ~/.cache/endeavor/depot:, or $SCRATCH/endeavor/depot:)
  --cache DIR          where the runtime's files are unpacked, a folder for Endeavor alone (default ~/.cache/endeavor/serve) [serve, mcp]
  --idle-stop HOURS    stop notebooks unused this long; 0 never (default 48)
  --state-dir DIR      the runtime's state (default ~/.local/state/endeavor/serve/<host>) [serve, mcp, stop, status, open]
  --json               print the facts as one JSON object [status]
  --force              end Julia while it is still starting, with what it began [stop]
";

const REOPEN_AFTER: Duration = Duration::from_secs(10);

fn start_wait() -> Duration {
    lock_limit("ENDEAVOR_START_WAIT_SECS", 45)
}

#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Serve(Options),
    Mcp(Options),
    Stop { state_dir: PathBuf, force: bool },
    Status { state_dir: PathBuf, json: bool },
    Open { state_dir: PathBuf, forwarded: Option<u16> },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Options {
    state_dir: PathBuf,
    older_state_dir: Option<PathBuf>,
    cache: PathBuf,
    julia: julia::Source,
    julia_when_needed: bool,
    r: crate::r::Source,
    depot: String,
    folder: Option<PathBuf>,
    port: u16,
    host_tools: bool,
    idle_hours: f64,
    skills_plugin: bool,
}

pub(crate) fn parse(argv: &[String], env: &Env) -> Result<Command, String> {
    let (command, rest) = argv.split_first().ok_or("expected serve, mcp, stop, status or open")?;
    let (mut state_dir, mut julia, mut depot, mut folder, mut port, mut r) = (None, None::<julia::Source>, None, None, None::<u16>, None);
    let mut cache = None;
    let (mut host_tools, mut idle_hours, mut skills_plugin, mut json, mut force, mut no_folder) = (false, crate::notebooks::IDLE_HOURS, false, false, false, false);
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or(format!("{arg} needs a value"));
        let only = |commands: &[&str]| if commands.contains(&command.as_str()) { Ok(()) } else { Err(format!("{arg} isn't an option of {command}")) };
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--cache" => {
                only(&["serve", "mcp"])?;
                cache = Some(PathBuf::from(value()?))
            }
            "--julia" | "--julia-shell" if julia.is_some() => return Err("give one of --julia and --julia-shell".into()),
            "--julia" => {
                only(&["serve", "mcp"])?;
                julia = Some(value().map(julia::Source::from_value)?)
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
                only(&["serve", "mcp", "open"])?;
                port = Some(value()?.parse().map_err(|_| "--port needs a port number".to_owned())?)
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
    let older_state_dir = state_dir.is_none().then(|| env.apps_old_state_dir()).flatten();
    let state_dir = state_dir.unwrap_or_else(|| env.state_dir());
    let now = || runtime::or_older(&state_dir, older_state_dir.as_deref());
    match command.as_str() {
        "stop" => return Ok(Command::Stop { state_dir: now(), force }),
        "status" => return Ok(Command::Status { state_dir: now(), json }),
        "open" => return Ok(Command::Open { state_dir: now(), forwarded: port }),
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
        older_state_dir,
        cache: cache.map(|c| env.cwd.join(c)).unwrap_or_else(|| env.cache()),
        julia: julia.unwrap_or(julia::Source::Auto),
        julia_when_needed: !(cfg!(debug_assertions) && std::env::var_os("ENDEAVOR_TEST_JULIA_AT_START").is_some()),
        r: r.unwrap_or_default(),
        depot: depot.unwrap_or_else(|| env.depot()),
        folder,
        port: port.unwrap_or(0),
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
        Command::Open { state_dir, forwarded: None } => open(&state_dir),
        Command::Open { state_dir, forwarded: Some(port) } => open_forwarded(&state_dir, port),
    }
}

pub(crate) fn unpack_runtime(cache: &Path) -> Result<PathBuf, String> {
    Ok(unpack(cache, embedded::RUNTIME_VERSION, embedded::RUNTIME_FILES)?.join("runtime"))
}

pub fn unpack(cache: &Path, version: &str, files: &[(&str, &[u8])]) -> Result<PathBuf, String> {
    let dir = cache.join(version);
    if dir.is_dir() {
        let _ = std::fs::OpenOptions::new().write(true).create(true).truncate(false).open(dir.join(IN_USE)).and_then(|f| f.set_modified(SystemTime::now()));
        return Ok(dir);
    }
    let failed = |e: io::Error| format!("Couldn't unpack Endeavor's runtime into {}: {e}", cache.display());
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let part = cache.join(format!("{version}.part.{}.{}", std::process::id(), CALLS.fetch_add(1, Ordering::Relaxed)));
    let written = files.iter().chain([&(IN_USE, &b""[..])]).try_for_each(|(path, contents)| {
        let file = part.join(path);
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(&file, contents)
    });
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(&part);
        return Err(failed(e));
    }
    match std::fs::rename(&part, &dir) {
        Ok(()) => {
            remove_unused(cache, version);
            Ok(dir)
        }
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

pub(crate) const IN_USE: &str = "in-use";

const KEEP_UNUSED: Duration = Duration::from_secs(24 * 3600);

const LEASE_TOUCH: Duration = Duration::from_secs(3600);

fn remove_unused(cache: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(cache) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !named_by_unpack(&name) {
            continue;
        }
        if name.contains(".removing.") {
            let _ = std::fs::remove_dir_all(&path);
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if name.contains(".part.") {
            let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
            if age.is_some_and(|age| age > crate::old_builds::KEEP_PART) {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if name == keep {
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

fn named_by_unpack(name: &str) -> bool {
    let Some((version, _)) = name.split_once('-') else { return false };
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

pub struct Lease {
    _marker: Arc<std::fs::File>,
}

pub fn lease(dir: &Path) -> Option<Lease> {
    let marker = std::fs::OpenOptions::new().read(true).write(true).open(dir.join(IN_USE)).ok()?;
    marker.try_lock_shared().ok()?;
    Some(lease_locked(marker))
}

pub(crate) fn lease_locked(marker: std::fs::File) -> Lease {
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
    Lease { _marker: marker }
}

impl Options {
    pub(crate) fn state_dir(&self) -> PathBuf {
        runtime::or_older(&self.state_dir, self.older_state_dir.as_deref())
    }
}

fn runtime_args(options: &Options, exit_idle: bool) -> Args {
    Args {
        state_dir: options.state_dir(),
        unnamed: None,
        julia: options.julia.clone(),
        julia_when_needed: options.julia_when_needed,
        r: options.r.clone(),
        own_r: true,
        runtime: PathBuf::new(),
        depot: options.depot.clone(),
        launcher: Launcher::Process,
        quit_with_client: false,
        own_with_client: false.into(),
        any_node: false,
        build: Some(embedded::BUILD_VERSION.into()),
        exit_idle,
        exit_idle_after_client: false,
        core_env: core_env(options, exit_idle),
    }
}

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

fn start_or_reuse(options: &Options, exit_idle: bool, progress: &dyn Fn(&str), cancelled: &dyn Fn() -> bool) -> Result<Up, String> {
    let args = runtime_args(options, exit_idle);
    let (events, _) = mpsc::channel();
    let want = Want { args: &args, engine: wire::ENGINE_PLUTO, install: true, runtime: &|| unpack_runtime(&options.cache), events: &events, attach_only: false, replace_older: false };
    match runtime::find_or_start(&want, &mut Saying { progress, cancelled }) {
        Outcome::Ready(up) => Ok(up),
        Outcome::Unusable(Looked::OtherNode(state)) => Err(runtime::other_node_text(&state.node)),
        Outcome::Unusable(_) => Err(OLDER_RUNTIME_HERE.into()),
        Outcome::NeedsInstall(items) => Err(items.into_iter().next().map_or_else(String::new, |item| julia::Failure::Missing(item).message())),
        Outcome::Failed(message) => Err(message),
        Outcome::Died { status, log_tail } => {
            let tail = log_tail[log_tail.len().saturating_sub(8)..].join("\n");
            Err(format!("Endeavor stopped while starting ({status}). The end of {}:\n{tail}", args.state_dir.join("runtime.log").display()))
        }
        Outcome::Cancelled => Err("Stopped before Endeavor was ready.".into()),
        Outcome::NothingRunning => Err("Endeavor isn't running.".into()),
    }
}

const OLDER_RUNTIME_HERE: &str = "The Endeavor running here was started by an older version. Stop it with `endeavor stop`, then try again.";

fn core_env(options: &Options, exit_idle: bool) -> Vec<(&'static str, Option<String>)> {
    let mut env = vec![("ENDEAVOR_IDLE_HOURS", Some(options.idle_hours.to_string()))];
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

pub(crate) fn open_start_lock(dir: &Path) -> Result<std::fs::File, String> {
    let path = dir.join("start.lock");
    crate::owner_only(std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false))
        .open(&path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))
}

pub(crate) fn start_lock_limit() -> Duration {
    lock_limit("ENDEAVOR_START_LOCK_SECS", 30 * 60)
}

pub(crate) fn stop_lock_limit() -> Duration {
    lock_limit("ENDEAVOR_STOP_LOCK_SECS", 20)
}

fn lock_limit(var: &str, default_secs: u64) -> Duration {
    Duration::from_secs(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default_secs))
}

pub(crate) fn start_lock_gave_up(dir: &Path) -> String {
    format!("Gave up waiting for another process that is starting Endeavor in {}. If none is, delete {} and try again.", dir.display(), dir.join("start.lock").display())
}

pub(crate) enum Wait<E> {
    Failed(String),
    TimedOut,
    Interrupted(E),
}

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

pub(crate) fn stop_lock_gave_up(dir: &Path) -> String {
    format!(
        "Endeavor was not stopped: another process has held the start lock in {} for too long. Endeavor is still running. Try again, or delete {} if nothing is starting Endeavor.",
        dir.display(),
        dir.join("start.lock").display()
    )
}

pub(crate) struct Connection<'a> {
    pub port: u16,
    pub token: &'a str,
    pub page_token: &'a str,
    pub node: &'a str,
    pub folder: &'a str,
    pub project: bool,
    pub login: Option<&'a str>,
}

pub(crate) fn connection_text(c: &Connection) -> String {
    let Connection { port, token, page_token, node, folder, project, .. } = c;
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
    http://localhost:{port}/?token={page_token}

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

The link and the header each let anyone who has them run code as you. Keep them to yourself.
",
        gemini = to_json(&json!({ "mcpServers": { "endeavor": { "httpUrl": url, "headers": { "Authorization": bearer } } } })),
    )
}

fn recorded_folder(dir: &Path) -> Option<(String, bool)> {
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    Some((state["folder"].as_str()?.to_owned(), state["no_folder"] != true))
}

pub(crate) fn other_build(dir: &Path) -> Option<String> {
    other_build_than(dir, embedded::BUILD_VERSION, Some(crate::core::INTERFACE))
}

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
        "The Endeavor running from {} was started by {version} version of endeavor ({which}; this is build {this}). It keeps working as it was started. To use this version, run `endeavor stop`, then start it again.",
        dir.display()
    ))
}

static STOP: AtomicBool = AtomicBool::new(false);

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

#[cfg(windows)]
static CLOSING: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn catch_stop_signals() {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
    unsafe extern "system" fn on_console_event(event: u32) -> windows_sys::core::BOOL {
        let closing = event != CTRL_C_EVENT && event != CTRL_BREAK_EVENT;
        CLOSING.fetch_or(closing, Ordering::SeqCst);
        STOP.store(true, Ordering::SeqCst);
        if closing {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        1
    }
    // SAFETY: a handler that only touches atomics and sleeps, for the life of the process.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
        eprintln!("endeavor: couldn't take Ctrl-C ({}); `endeavor stop` stops it", io::Error::last_os_error());
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
    let dir = &options.state_dir();
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
    print!("\n{}", connection_text(&Connection { port: up.port, token: &up.state.token, page_token: up.state.page_token.as_deref().unwrap_or(&up.state.token), node: &node, folder: &folder, project, login: login.as_deref() }));
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
        Ended::NotRunning => println!("Endeavor isn't running from {}.", dir.display()),
        Ended::Elsewhere(node) => failed(format!("The Endeavor recorded in {} runs on {node}, not here ({}). Stop it there.", dir.display(), crate::hostname())),
        Ended::Alive(pid) => failed(format!("Endeavor (pid {pid}) is still running.")),
        Ended::Starting => failed(format!("Endeavor is still starting in {}. `endeavor stop --force` cancels the start.", dir.display())),
        Ended::Unidentified => failed(runtime::START_UNIDENTIFIED.into()),
        Ended::Cancelled(pid) => println!("Cancelled the start of Endeavor (pid {pid})."),
        Ended::Stopped(pid) => println!("Stopped Endeavor (pid {pid})."),
    }
    std::process::exit(0)
}

fn open(dir: &Path) -> ! {
    let failed = |message: String| -> ! {
        eprintln!("{message}");
        std::process::exit(1)
    };
    match runtime::look(dir, false, true) {
        Looked::Running(state, port) => show_entry(port, state.page_token.as_deref().unwrap_or(&state.token), "Endeavor's notebooks"),
        Looked::OtherNode(state) => failed(format!("The Endeavor recorded in {} runs on {}, not here ({}). Run `endeavor open` there.", dir.display(), state.node, crate::hostname())),
        Looked::Silent(_) => failed("Endeavor is running but not answering. Try again in a moment.".into()),
        Looked::Older(_) => failed(format!("The Endeavor running from {} was started by an older version, which has no page for a browser. `endeavor stop` stops it.", dir.display())),
        Looked::NotRunning | Looked::Dead(_) => failed(format!("Endeavor isn't running from {}.", dir.display())),
    }
}

fn open_forwarded(dir: &Path, port: u16) -> ! {
    match browser::forwarded(dir, port) {
        Ok((machine, page_token)) => show_entry(port, &page_token, &format!("the notebooks on {machine}")),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1)
        }
    }
}

fn show_entry(port: u16, page_token: &str, what: &str) -> ! {
    let link = crate::mcp::entry_link(&crate::mcp::browser_link(port, "/"), page_token);
    use std::io::IsTerminal;
    if browser::open(&link) {
        println!("Opened {what} in your browser.");
    } else if io::stdout().is_terminal() {
        println!("Open this link in your browser. It holds the notebooks' key: don't share it.\n    {link}");
    } else {
        eprintln!("No browser could be opened here. Run `endeavor open` in your own terminal: the link it prints holds the notebooks' key, so it isn't printed anywhere else.");
        std::process::exit(1)
    }
    std::process::exit(0)
}

struct Relay {
    options: Options,
    session: String,
    target: Mutex<Target>,
    workspace: Mutex<Option<String>>,
    local: Arc<Local>,
    connections: machines::Connections,
    ops: Mutex<()>,
    notice: Mutex<Option<String>>,
    told_other_build: Mutex<std::collections::HashSet<(String, u32)>>,
    machines: crate::client::MachinesFile,
    projects: projects::Projects,
    opened: Mutex<std::collections::HashMap<(u16, String), Instant>>,
    protocol: Mutex<Option<String>>,
    mcp_session: Mutex<Option<String>>,
    views: std::sync::atomic::AtomicBool,
    meta_logged: Mutex<(Vec<String>, u32)>,
    meta_salt: std::collections::hash_map::RandomState,
    consent: consent::Consent,
    pages: install_page::Pages,
    mode: claude_mode::Files,
    out: Mutex<Box<dyn Write + Send>>,
}

fn relay(options: Options) -> ! {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let fresh = format!("stdio-{}-{}", std::process::id(), now.as_millis());
    let conversation = std::env::var("CLAUDE_CODE_SESSION_ID").ok();
    let child_of_shell = std::env::var_os("CLAUDE_CODE_CHILD_SESSION").is_some();
    let session = session_key(conversation.as_deref(), child_of_shell).unwrap_or(fresh);
    precompile_after_update(&options);
    let mut relay = Relay::new(options, session, Box::new(io::stdout()));
    #[cfg(unix)]
    let parent = Some(std::os::unix::process::parent_id());
    #[cfg(not(unix))]
    let parent = None;
    relay.mode = claude_mode::Files::of(&Env::here().claude_modes_dir(), conversation.as_deref().filter(|_| !child_of_shell), parent);
    relay.mode.register();
    let relay = Arc::new(relay);
    relay.target_from_project();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let relay = relay.clone();
        std::thread::spawn(move || relay.handle(&line));
    }
    relay.connections.close_all();
    std::process::exit(0)
}

fn precompile_after_update(options: &Options) {
    let (cache, source, depot) = (options.cache.clone(), options.julia.clone(), options.depot.clone());
    std::thread::spawn(move || {
        let find = || julia::find(&source, false, &mut |_| {}).ok();
        precompile_if_needed(&depot, &|| unpack_runtime(&cache).ok(), &find, &mut |runtime, key, julia| {
            if let Some(mut command) = precompile_command(julia, runtime, &depot, key) {
                let _ = command.status();
            }
        });
    });
}

const PRECOMPILE_RETRY: Duration = Duration::from_secs(24 * 60 * 60);

fn precompile_if_needed(depot: &str, runtime: &dyn Fn() -> Option<PathBuf>, julia: &dyn Fn() -> Option<(String, String)>, run: &mut dyn FnMut(&Path, &str, &str)) {
    if !julia_ran_here(depot) {
        return;
    }
    let Some(runtime) = runtime() else { return };
    let Some((julia, version)) = julia() else { return };
    let key = format!("{depot}\n{version}\n");
    if std::fs::read_to_string(runtime.join(".precompiled")).is_ok_and(|said| said == key) {
        return;
    }
    let failed = std::fs::metadata(runtime.join(".precompile-failed")).and_then(|m| m.modified()).ok();
    if failed.is_some_and(|at| at.elapsed().is_ok_and(|ago| ago < PRECOMPILE_RETRY)) {
        return;
    }
    run(&runtime, &key, &julia);
}

fn julia_ran_here(depot: &str) -> bool {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let Some(first) = depot.split(sep).next().filter(|p| !p.is_empty()) else { return false };
    std::fs::read_dir(Path::new(first).join("compiled"))
        .map(|versions| versions.flatten().any(|v| v.path().join("EndeavorRuntime").is_dir()))
        .unwrap_or(false)
}

const PRECOMPILE_SCRIPT: &str = r#"
using FileWatching.Pidfile: mkpidlock
const R = ENV["ENDEAVOR_RUNTIME"]
lock = try mkpidlock(joinpath(R, ".precompiling"); wait=false, stale_age=60) catch; exit(0) end
include(joinpath(R, "cache_guard.jl"))
endeavor_drop_unreadable_caches(; log=devnull)
import Pkg
all(d -> d.source === nothing || isdir(d.source), values(Pkg.dependencies())) || exit(0)
ok = try
    Pkg.precompile(["EndeavorRuntime", "EndeavorRuntimeWarm"])
    true
catch
    false
end
endeavor_drop_unreadable_caches(; log=devnull)
isdir(R) || exit(ok ? 0 : 1)
if ok
    write(joinpath(R, ".precompiled"), ENV["ENDEAVOR_PRECOMPILED"])
    rm(joinpath(R, ".precompile-failed"); force=true)
else
    touch(joinpath(R, ".precompile-failed"))
    exit(1)
end
"#;

fn precompile_command(julia: &str, runtime: &Path, depot: &str, key: &str) -> Option<std::process::Command> {
    let lower = if cfg!(windows) {
        "ccall((:SetPriorityClass, \"kernel32\"), stdcall, Cint, (Ptr{Cvoid}, UInt32), ccall((:GetCurrentProcess, \"kernel32\"), stdcall, Ptr{Cvoid}, ()), 0x00004000)\n"
    } else {
        ""
    };
    let script = format!("{lower}{PRECOMPILE_SCRIPT}");
    let nice = (!cfg!(windows)).then(|| ["/usr/bin/nice", "/bin/nice"].into_iter().find(|p| Path::new(p).is_file())).flatten();
    let mut command = match nice {
        Some(nice) => {
            let mut command = std::process::Command::new(nice);
            command.args(["-n", "10", julia]);
            command
        }
        None => std::process::Command::new(julia),
    };
    command
        .arg("--startup-file=no")
        .arg(format!("--project={}", runtime.display()))
        .args(["-e", &script])
        .env("JULIA_DEPOT_PATH", crate::paths::with_user_depot(depot))
        .env("JULIA_LOAD_PATH", if cfg!(windows) { "@;@stdlib" } else { "@:@stdlib" })
        .env("ENDEAVOR_RUNTIME", runtime)
        .env("ENDEAVOR_PRECOMPILED", key)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::client::no_window(&mut command);
    Some(command)
}

fn session_key(conversation: Option<&str>, child_of_shell: bool) -> Option<String> {
    if child_of_shell {
        return None;
    }
    let id = conversation?.trim();
    let ok = !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| format!("claude-{id}"))
}

const META_CALLS_LOGGED: u32 = 5;

fn fingerprint(salt: &std::collections::hash_map::RandomState, text: &str) -> String {
    use std::hash::BuildHasher;
    format!("{:08x}", salt.hash_one(text) as u32)
}

impl Relay {
    fn log_meta(&self, meta: &Value) {
        let keys: Vec<String> = meta.as_object().map(|meta| meta.keys().cloned().collect()).unwrap_or_default();
        let mut logged = self.meta_logged.lock().unwrap();
        let (seen, calls) = &mut *logged;
        if *seen != keys || *calls == 0 {
            eprintln!("[ Info: Tool calls carry _meta keys: {}", if keys.is_empty() { "(none)".to_owned() } else { keys.join(", ") });
            *seen = keys;
        }
        if *calls < META_CALLS_LOGGED {
            *calls += 1;
            let values: Vec<String> = meta.as_object().into_iter().flatten().filter_map(|(key, value)| Some(format!("{key}={}", fingerprint(&self.meta_salt, value.as_str()?)))).collect();
            if !values.is_empty() {
                eprintln!("[ Info: Tool call {calls}'s _meta values, as fingerprints: {}", values.join(", "));
            }
        }
    }

    fn new(options: Options, session: String, out: Box<dyn Write + Send>) -> Relay {
        Relay {
            target: Mutex::new(Target::local(options.folder.as_deref())),
            workspace: Mutex::default(),
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
            views: Default::default(),
            meta_logged: Mutex::default(),
            meta_salt: Default::default(),
            consent: Default::default(),
            pages: Default::default(),
            mode: Default::default(),
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

    fn tell(port: u16, token: &str, method: &str, params: Value, deadline: Option<Instant>) -> io::Result<(u16, Vec<u8>)> {
        let body = to_json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }));
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
        crate::http::post_by(port, crate::CALL, &headers, body.as_bytes(), deadline)
    }

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

    fn local_folder(&self, target: &Target) -> Option<Folder> {
        target.is_local().then(|| match (&self.options.folder, self.workspace.lock().unwrap().as_ref()) {
            (Some(folder), _) => Folder::In(folder.display().to_string()),
            (None, Some(workspace)) => Folder::In(workspace.clone()),
            (None, None) => Folder::Unknown,
        })
    }

    fn learn_workspace(&self, message: &Value) {
        if self.options.folder.is_some() {
            return;
        }
        let metadata = &message["params"]["_meta"]["x-codex-turn-metadata"];
        let parsed = metadata.as_str().and_then(|text| serde_json::from_str::<Value>(text).ok());
        let Some(workspaces) = parsed.as_ref().unwrap_or(metadata)["workspaces"].as_object().cloned() else { return };
        let mut folders = workspaces.keys().filter(|path| Path::new(path).is_absolute() && Path::new(path).is_dir());
        let (Some(folder), None) = (folders.next(), folders.next()) else { return };
        {
            let mut workspace = self.workspace.lock().unwrap();
            if workspace.as_deref() == Some(folder.as_str()) {
                return;
            }
            *workspace = Some(folder.clone());
        }
        eprintln!("endeavor: this session's project folder is {folder}, as Codex names it");
        self.update_target(machines::LOCAL, |t| t.told = None);
    }

    fn handle(self: &Arc<Self>, line: &str) {
        let rewritten: String;
        let mut deadline = machines::Deadline::after(start_wait());
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return self.write(&to_json(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } })));
        };
        if self.consent.take_answer(&message) {
            return;
        }
        if self.consent.take_cancel(&message) {
            return;
        }
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        if message["method"] == "initialize" {
            self.consent.learn_client(&message);
            let draws = crate::views::client_draws_views(&message);
            self.views.store(draws, std::sync::atomic::Ordering::SeqCst);
            let client = &message["params"]["clientInfo"];
            let dialogs = if self.consent.can_ask() { "yes" } else { "no" };
            eprintln!("[ Info: Client {} {}: draws views: {}; shows dialogs: {dialogs} (elicitation: {})", client["name"].as_str().unwrap_or("?"), client["version"].as_str().unwrap_or("?"), if draws { "yes" } else { "no" }, message["params"]["capabilities"]["elicitation"]);
        }
        let views = self.views.load(std::sync::atomic::Ordering::SeqCst);
        if let Some(reply) = crate::mcp::answer_locally(&message, self.options.skills_plugin, self.options.folder.is_none(), views) {
            if message["method"] == "initialize" {
                let negotiated: Value = serde_json::from_str(&reply).unwrap_or_default();
                *self.protocol.lock().unwrap() = negotiated["result"]["protocolVersion"].as_str().map(str::to_owned);
            }
            let reply = if message["method"] == "tools/list" { self.consent.tools_listed(reply, self.page_shows()) } else { reply };
            return self.write(&reply);
        }
        if message["method"] == "notifications/initialized" {
            return;
        }
        let tool = (message["method"] == "tools/call").then(|| crate::mcp::current_name(message["params"]["name"].as_str().unwrap_or_default()).to_owned());
        if tool.is_some() {
            self.log_meta(&message["params"]["_meta"]);
        }
        let mut message = message;
        let mut in_browser = false;
        let stripped;
        let mut line = match message["params"]["arguments"].as_object_mut() {
            Some(arguments) if views && matches!(tool.as_deref(), Some("new_notebook" | "open_notebook")) && arguments.get(crate::views::IN_BROWSER).is_some_and(Value::is_boolean) => {
                in_browser = arguments.remove(crate::views::IN_BROWSER) == Some(Value::Bool(true));
                stripped = to_json(&message);
                stripped.as_str()
            }
            _ => line,
        };
        if tool.is_some() {
            self.learn_workspace(&message);
        }
        if let Some(tool) = tool.as_deref().filter(|tool| crate::mcp::MACHINE_NAMES.contains(tool)) {
            return self.machine_tool(&message, tool, deadline);
        }
        let help = !self.options.skills_plugin;
        let target = self.current();
        if target.is_local() && let Some(tool) = tool.as_deref().filter(|tool| *tool == crate::guide::TOOL || crate::host_tools::NAMES.contains(tool)) {
            if id.is_none() {
                return;
            }
            let result = crate::mcp::call_parts(&message["params"], help, true).and_then(|(_, arguments)| {
                if tool != crate::guide::TOOL {
                    return Err(crate::mcp::tool_error(&crate::mcp::host_tool_refusal(tool), false));
                }
                crate::mcp::check_arguments(tool, &arguments, help)?;
                crate::guide::read(&arguments).map(|guide| machines::text_result(&guide)).map_err(|error| crate::mcp::tool_error(&error, false))
            });
            return self.answer_call(&message, Some(tool), result.unwrap_or_else(|failed| failed));
        }
        if let Some(tool) = tool.as_deref() {
            let checked = crate::mcp::checked_call(&message["params"], help, true).and_then(|(_, arguments)| {
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
        if tool.as_deref() == Some("approve_install") && id.is_some() {
            let began = Instant::now();
            self.consent.looking(&message);
            let waiting = self.waiting_download(&message["params"]["arguments"]["notebook_id"], deadline);
            let seen = consent::Consent::seen_packages(&waiting);
            let page = self.page_shows();
            let held = page && consent::Consent::download_question(&waiting).is_some_and(|question| self.pages.held(&question));
            let mut dialog = self.consent.can_ask() && !held;
            let mut asked = if dialog { self.consent.ask(&message, waiting.clone(), page, &|line| self.write(line)) } else { consent::Asked::NoDialog };
            if matches!(asked, consent::Asked::NoDialog) {
                dialog = false;
                let wait = machines::page_wait().min(machines::PAGE_CALL.saturating_sub(began.elapsed() + Duration::from_secs(10)));
                asked = self.consent.ask_on_page(&message, waiting, page, wait, &self.pages, &machines::open_page, &|line| self.write(line));
            } else if matches!(asked, consent::Asked::Silent) {
                let wait = consent::ANSWER_WAIT.saturating_sub(began.elapsed());
                asked = self.consent.ask_on_page(&message, waiting, page, wait, &self.pages, &machines::open_page, &|line| self.write(line));
            }
            let yes = matches!(asked, consent::Asked::Yes);
            match asked {
                consent::Asked::No(result) => return self.answer_call(&message, tool.as_deref(), result),
                consent::Asked::Dropped => return,
                consent::Asked::Yes if dialog => deadline = machines::Deadline::after(start_wait()),
                consent::Asked::Yes => deadline = machines::Deadline::after(machines::PAGE_CALL.saturating_sub(began.elapsed()).max(machines::MIN_INSTALL)),
                consent::Asked::Unasked => {}
                consent::Asked::NoDialog | consent::Asked::Silent => return self.answer_call(&message, tool.as_deref(), consent::download_cant_ask()),
            }
            let seen = if yes { seen } else { None };
            let mut call = message.clone();
            call["params"]["arguments"]["packages"] = crate::notebooks::seen_packages(message["params"]["arguments"].get("packages"), &seen.unwrap_or_else(|| json!([])));
            rewritten = to_json(&call);
            line = &rewritten;
        }
        let crop_call = tool.as_deref() == Some("notebook_crop") || (tool.as_deref() == Some("view_cell_output") && message["params"]["arguments"].get("crop").is_some_and(|crop| !crop.is_null()));
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
            if crop_call && let Some(older) = self.held(&target).and_then(|provider| match provider.status().state {
                crate::client::State::Ready(info) => info.interface.is_none_or(|interface| interface < crate::CROP_SINCE).then_some(info.pid),
                _ => None,
            }) {
                if id.is_some() {
                    self.answer_call(&message, tool.as_deref(), crate::mcp::tool_error(&format!("unsupported: the Endeavor here (pid {older}) was started by an older version, which can't keep or show a box's picture. Read the box's cells instead. It is replaced by this version once no notebook is open in it, or after `endeavor stop`."), false));
                }
                return;
            }
            let sink = |reply: String| {
                let reply = if tool.as_deref().is_some_and(crate::views::is_view_tool) { reply } else { self.consent.reply_said(reply, self.page_shows()) };
                self.write(&self.decorate(&message, tool.as_deref(), self.in_browser(&message, tool.as_deref(), &route, in_browser, reply)))
            };
            match self.post(&route, line, &sink) {
                Ok(()) => return,
                Err(Sent::NotConnected(e)) if attempt == 0 => {
                    eprintln!("endeavor: the runtime isn't answering ({e}); looking for it again");
                }
                Err(Sent::NotConnected(e) | Sent::Failed(e)) => return failed(format!("Endeavor's runtime didn't answer: {e}")),
            }
        }
    }

    fn waiting_download(&self, notebook: &Value, deadline: machines::Deadline) -> consent::Waiting {
        let deadline = machines::Deadline::after(deadline.left().min(Duration::from_secs(10)));
        let route = match self.route(Need::Look, deadline) {
            Ok(route) => route,
            Err(unready) if unready.nothing_runs() => return consent::Waiting::Nothing,
            Err(_) => return consent::unknown(notebook),
        };
        let id = "endeavor-look";
        let look = to_json(&json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": "list_notebooks", "arguments": {} } }));
        let listed = Mutex::new(None);
        let sink = |reply: String| {
            if serde_json::from_str::<Value>(&reply).is_ok_and(|reply| reply["id"] == id) {
                *listed.lock().unwrap() = Some(reply);
            }
        };
        if self.post(&route, &look, &sink).is_err() {
            return consent::unknown(notebook);
        }
        listed.into_inner().unwrap().map_or_else(|| consent::unknown(notebook), |listed| consent::waiting_in(&listed, notebook))
    }

    fn in_browser(&self, message: &Value, tool: Option<&str>, route: &Route, asked: bool, reply: String) -> String {
        if tool.is_some_and(crate::views::is_view_tool) {
            return reply;
        }
        let Ok(mut parsed) = serde_json::from_str::<Value>(&reply) else { return reply };
        if parsed["id"] != message["id"] || parsed["result"]["isError"] != false {
            return reply;
        }
        let Some(Value::Object(mut fields)) = parsed["result"]["content"][0]["text"].as_str().and_then(|text| serde_json::from_str(text).ok()) else { return reply };
        let Some(url) = fields.get("browser_url").and_then(Value::as_str).map(crate::mcp::without_token) else { return reply };
        let in_view = route.host.is_none() && self.views.load(std::sync::atomic::Ordering::SeqCst) && matches!(tool, Some("new_notebook" | "open_notebook")) && !asked;
        let mut failed = false;
        if in_view {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            fields.insert("opened_in_browser".into(), false.into());
            fields.insert("shown_beside_chat".into(), true.into());
            fields.insert("shown_at".into(), now.into());
        } else if matches!(tool, Some("new_notebook" | "open_notebook")) {
            let key = (route.port, fields.get("notebook_id").and_then(Value::as_str).unwrap_or_default().to_owned());
            let just = self.opened.lock().unwrap().get(&key).is_some_and(|at| at.elapsed() < REOPEN_AFTER);
            let opened = just || browser::open(&crate::mcp::entry_link(&url, &route.page_token));
            if opened && !just {
                self.opened.lock().unwrap().insert(key, Instant::now());
            }
            fields.insert("opened_in_browser".into(), opened.into());
            failed = !opened;
        }
        let var = |name: &str| std::env::var(name).ok();
        if !in_view && (failed || !browser::shows(var)) {
            let dir = self.options.state_dir();
            let forwarded = route.host.as_deref().map(|machine| browser::remember_forwarded(&dir, route.port, machine, &route.page_token).map(|()| route.port));
            if let Some(Err(e)) = &forwarded {
                eprintln!("endeavor: {e}");
            }
            let open = crate::core::open_command(&dir, forwarded.and_then(Result::ok));
            fields.insert("browser_note".into(), browser::note(&crate::hostname(), route.port, &open).into());
        }
        fields.insert("browser_url".into(), url.into());
        parsed["result"]["content"][0]["text"] = to_json(&Value::Object(fields)).into();
        to_json(&parsed)
    }

    fn post(&self, route: &Route, body: &str, sink: &dyn Fn(String)) -> Result<(), Sent> {
        let port = route.port;
        let token = &route.token;
        let socket = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).map_err(Sent::NotConnected)?;
        let _ = socket.set_nodelay(true);
        let mut head = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nX-Endeavor-Session: {}\r\nX-Endeavor-Client: plugin\r\nX-Endeavor-Consent: front\r\nContent-Length: {}\r\n",
            self.session,
            body.len()
        );
        if let Some(host) = &route.host {
            head.push_str(&format!("X-Endeavor-Host: {host}\r\nX-Endeavor-Browser-Port: {port}\r\n"));
        }
        if self.options.skills_plugin {
            head.push_str("X-Endeavor-Skills: plugin\r\n");
        }
        if self.mode.auto() {
            head.push_str("X-Endeavor-Mode: auto\r\n");
        }
        head.push_str(&crate::mcp::Front::own().header());
        if self.views.load(std::sync::atomic::Ordering::SeqCst) {
            head.push_str("X-Endeavor-Notebooks: several\r\n");
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
    NotConnected(io::Error),
    Failed(io::Error),
}

fn tool_failure(id: &Value, message: &Value, why: &str) -> String {
    if message["method"] == "tools/call" {
        return to_json(&json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": why }], "isError": true } }));
    }
    to_json(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": why } }))
}

fn one_line(text: &str) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(value) => to_json(&value),
        Err(_) => text.replace(['\r', '\n'], " "),
    }
}

pub(crate) struct Route {
    pub port: u16,
    pub token: String,
    pub page_token: String,
    pub host: Option<String>,
}

struct Body<R> {
    inner: R,
    framing: crate::http::Framing,
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
