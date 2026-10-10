//! `endeavor core`: the runtime the helper starts (docs/runtime-core.md).
//! It starts `julia boot.jl` as its child, serves the runtime's one port, and
//! writes `runtime.json` once Julia is ready. On that port it serves the
//! agent's MCP connection at `/mcp` and the app's `/endeavor/call`s itself (see
//! `mcp`), and the app's `/endeavor/events` stream (see `notebooks`), driving
//! Pluto through Julia's adapter; the few calls Julia answers
//! (`endeavor/set_folder`, `endeavor/shutdown`) go on to Julia's own bridge.
//! Every other path is Pluto's page, passed through to Pluto's private port
//! with Pluto's secret added, WebSockets included (docs/one-port.md).
//!
//! Julia shares the core's process group, which the helper created, so the
//! helper's signals to the group reach both. The core exits when Julia does,
//! the same way, and passes a stop signal sent to it alone on to Julia. On
//! Windows the core puts itself in a Job Object before starting Julia, so
//! Julia and its workers end when the core does.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::{self, Head};
use wire::backend::Backend;
use crate::mcp::Bridge;
use crate::{USAGE, bridge_call, owner_only, remove_state};

/// The number for what a core offers the processes that use it, recorded in `runtime.json` as
/// `interface`: the notebook tools' names and arguments at `/mcp`, the app's `/endeavor/call`s and
/// `/endeavor/events`, and the record's own fields. A front or a client of another build uses a
/// runtime with the same number as it is, and treats one with another number (or none) as another
/// build's. Two builds with one number must take each other's calls both ways: a newer front meets
/// an older core as often as the reverse, and lists its own build's tools. So raise it with any
/// change to those, additions included (a tool, an argument, a call, a field), and with a change in
/// what one returns or does while its arguments stay the same. Descriptions and changes inside the
/// core don't count. Tests in `mcp.rs` fail when the notebook tools' names or arguments change, and when
/// a call the app makes or a field of the record is added, removed or renamed; nothing catches a change
/// in what a call or the events stream returns.
pub const INTERFACE: u32 = 1;

/// Where boot.jl writes its state for the core, in the state folder.
const JULIA_STATE: &str = "julia.json";

#[cfg(unix)]
const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

struct Args {
    state_dir: PathBuf,
    julia: String,
    runtime: PathBuf,
    depot: String,
    /// R's `Rscript`, for R notebooks, and the R library Ember is installed in (none: R's own).
    r: String,
    r_library: Option<String>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = argv.iter();
    let (mut state_dir, mut julia, mut runtime, mut depot, mut r, mut r_library) = (None, None, None, None, None, None);
    while let Some(arg) = args.next() {
        let value = args.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value?)),
            "--julia" => julia = Some(value?),
            "--runtime" => runtime = Some(PathBuf::from(value?)),
            "--depot" => depot = Some(value?),
            "--r" => r = Some(value?),
            "--r-library" => r_library = Some(value?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        julia: julia.ok_or("--julia is required")?,
        runtime: runtime.ok_or("--runtime is required")?,
        depot: depot.ok_or("--depot is required")?,
        r: r.unwrap_or_else(|| "Rscript".into()),
        r_library,
    })
}

/// `endeavor core …`, with ENDEAVOR_TOKEN and ENDEAVOR_LAUNCHER in the
/// environment, and ENDEAVOR_BUILD, the app build it came from, which it
/// reports to the app. Its stdout and stderr are the runtime's log, which Julia shares.
///
/// Started without the app (`standalone`), the environment also has
/// ENDEAVOR_FOLDER, the notebooks' folder, which makes it a standalone
/// runtime, or ENDEAVOR_NO_FOLDER, which makes it one with no project folder:
/// it works in the user's home folder, and only a session that says it has none
/// gives absolute paths only;
/// ENDEAVOR_PORT, a fixed port; ENDEAVOR_HOST_TOOLS, the host name
/// under which every session gets the host tools; and ENDEAVOR_IDLE_HOURS, the
/// idle stop (48 hours when not set). ENDEAVOR_EXIT_IDLE, to end the runtime
/// once no notebook has been open for that long, works with or without a
/// folder: a client asks for it on a runtime it starts in the background.
/// `runtime.json` says whether it was set.
pub fn main(argv: &[String]) -> ! {
    let args = parse_args(argv).unwrap_or_else(|e| {
        eprintln!("{e}\n{USAGE}");
        std::process::exit(2);
    });
    let fail = |message: String| -> ! {
        eprintln!("endeavor core: {message}");
        std::process::exit(1);
    };
    let token = std::env::var("ENDEAVOR_TOKEN").unwrap_or_else(|_| fail("ENDEAVOR_TOKEN is not set".into()));
    let launcher = std::env::var("ENDEAVOR_LAUNCHER").unwrap_or_else(|_| "process".into());
    // Held until `runtime.json` is written (`julia_ready`), so that a client can tell a start under way from one that died.
    let mut starting = Some(crate::runtime::hold_starting(&args.state_dir).unwrap_or_else(|e| fail(format!("Couldn't lock {}: {e}", args.state_dir.display()))));
    #[cfg(unix)]
    let (stop_signals, inherited_mask) = block_stop_signals();
    // The runtime is the `runtime/` inside the folder `unpack` made.
    let _lease = args.runtime.parent().and_then(crate::lease);
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let fixed_port: u16 = env("ENDEAVOR_PORT").and_then(|p| p.parse().ok()).unwrap_or(0);
    let folder = env("ENDEAVOR_FOLDER");
    let no_folder = folder.is_none() && env("ENDEAVOR_NO_FOLDER").is_some();
    // Not the client's working folder, which may be one a plugin update removes.
    let folder = folder.or_else(|| {
        no_folder.then(|| {
            let home = crate::paths::Env::here().home;
            if home.as_os_str().is_empty() {
                fail("Couldn't find the home folder, where a runtime without a project folder works".into());
            }
            home.display().to_string()
        })
    });
    if let Some(folder) = &folder {
        // Relative paths in the tools, and Julia's, start in the notebooks' folder.
        std::env::set_current_dir(folder).unwrap_or_else(|e| fail(format!("Couldn't use {folder} as the notebooks' folder: {e}")));
    }
    let idle_hours = env("ENDEAVOR_IDLE_HOURS").and_then(|h| h.parse::<f64>().ok());
    let exit_idle = env("ENDEAVOR_EXIT_IDLE").is_some();
    let listener = TcpListener::bind(("127.0.0.1", fixed_port)).unwrap_or_else(|e| fail(format!("Couldn't open the runtime's port {fixed_port}: {e}")));
    let julia_state = args.state_dir.join(JULIA_STATE);
    let _ = std::fs::remove_file(&julia_state);
    let mut command = julia_command(&args, &token, &launcher, &julia_state).unwrap_or_else(|e| fail(e));
    // Kept, unused, until the process exits: closing it ends the job.
    #[cfg(windows)]
    let _job = crate::winproc::job_ending_with_this_process().unwrap_or_else(|e| fail(format!("Couldn't keep Julia's processes together with this one (Job Object): {e}")));
    // SAFETY: only async-signal-safe calls between fork and exec.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(move || {
            libc::pthread_sigmask(libc::SIG_SETMASK, &inherited_mask, std::ptr::null_mut());
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut julia = command.spawn().unwrap_or_else(|e| fail(format!("Couldn't start {}: {e}", args.julia)));
    #[cfg(unix)]
    pass_on_stop_signals(stop_signals, julia.id() as i32);

    let cookie = cookie_name(&token);
    let port = listener.local_addr().unwrap().port();
    let token_for_r = token.clone();
    let mut bridge = Bridge::new(token, &args.depot);
    bridge.standalone = folder.map(|folder| crate::mcp::Standalone { port, folder, no_folder, host: env("ENDEAVOR_HOST_TOOLS") });
    if let Ok(build) = std::env::var("ENDEAVOR_BUILD") {
        let _ = bridge.notebooks.build.set(build);
    }
    // Before `runtime.json` is written: a client that sees the record reads these.
    if let Some(hours) = idle_hours {
        bridge.notebooks.set_idle_limit(hours);
    }
    Arc::get_mut(&mut bridge.notebooks).expect("nothing else holds the notebooks yet").exits_when_idle = exit_idle;
    let not_let_in = not_let_in(&open_command(&args.state_dir));
    let served = Arc::new(Served { bridge, pluto: OnceLock::new(), ember: Default::default(), cookie, not_let_in });
    let r = Arc::new(RStarter::new(&args, &token_for_r));
    let starting_r = (r.clone(), served.clone());
    let _ = served.bridge.notebooks.starter.set(Box::new(move |backend| match backend {
        Backend::Ember => starting_r.0.start(&starting_r.1),
        Backend::Pluto => Err("Pluto starts with the runtime".into()),
    }));
    accept(listener, served.clone());

    let status = loop {
        if let Some(status) = julia.try_wait().unwrap_or(None) {
            break status;
        }
        if let Some(bridge_port) = julia_ready(&julia_state, &args.state_dir, port, &served, &mut starting) {
            if exit_idle {
                exit_when_idle(served.clone(), bridge_port);
            }
            served.bridge.notebooks.start();
            break julia.wait().unwrap_or_else(|e| fail(format!("waiting for Julia: {e}")));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = std::fs::remove_file(&julia_state);
    r.stop();
    remove_state(&args.state_dir, std::process::id() as i32, None);
    exit_like(status)
}

/// `julia boot.jl` with the environment it reads (see runtime/boot.jl), on
/// private ports free here (Pluto's, and Julia's bridge for the core), writing
/// its state to `julia_state` for the core.
fn julia_command(args: &Args, token: &str, launcher: &str, julia_state: &Path) -> Result<Command, String> {
    let ports = free_ports()?;
    let runtime = args.runtime.display();
    let mut command = Command::new(&args.julia);
    command
        .arg("--color=no")
        .arg(format!("--project={runtime}"))
        .arg(format!("{runtime}/boot.jl"))
        .args(ports.map(|p| p.to_string()))
        .env("JULIA_DEPOT_PATH", &args.depot)
        // Not argv, which `ps` shows to every user.
        .env("ENDEAVOR_TOKEN", token)
        .env("ENDEAVOR_STATE", julia_state)
        .env("ENDEAVOR_LAUNCHER", launcher)
        .stdin(Stdio::null());
    // Julia is a console program. The core may have no console to give it (in
    // the app, the core is the app's own GUI program), and then Julia would open
    // a console window for as long as it runs. Pluto's workers share Julia's.
    crate::client::no_window(&mut command);
    for name in ["ENDEAVOR_PORT", "ENDEAVOR_FOLDER", "ENDEAVOR_NO_FOLDER", "ENDEAVOR_HOST_TOOLS", "ENDEAVOR_IDLE_HOURS", "ENDEAVOR_EXIT_IDLE"] {
        command.env_remove(name);
    }
    Ok(command)
}

fn free_ports() -> Result<[u16; 2], String> {
    // Both held at once so the OS can't hand out the same port twice.
    let pluto = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let mcp = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok([&pluto, &mcp].map(|l| l.local_addr().unwrap().port()))
}

/// Where R's adapter writes its state for the core, in the state folder.
const R_STATE: &str = "r.json";

/// The Ember commit R notebooks use (https://github.com/jowch/Ember), installed
/// from source the first time one is opened (`runtime/r/install.R`).
pub const EMBER_COMMIT: &str = "0176bea6c969d3e6a9dd1d825f9672d62b1959d2";

/// Where installing Ember is.
enum Install {
    Idle,
    Running,
    Failed(String),
}

/// Starts R's adapter (`runtime/r/adapter.R`), with Ember in it, the first time an R notebook is
/// opened, and stops it when the core ends.
struct RStarter {
    rscript: String,
    /// The core's `--r-library`, which has Ember; else Endeavor's own, installed when first needed.
    library: Option<String>,
    own_library: PathBuf,
    install: Arc<std::sync::Mutex<Install>>,
    adapter: PathBuf,
    state: PathBuf,
    token: String,
    /// Runs each start: a process started on Linux ends with the thread that started it
    /// (`PR_SET_PDEATHSIG`), and a request's thread ends with its connection.
    spawn: std::sync::Mutex<std::sync::mpsc::Sender<(Command, std::sync::mpsc::Sender<io::Result<std::process::Child>>)>>,
    child: std::sync::Mutex<Option<std::process::Child>>,
}

impl RStarter {
    fn new(args: &Args, token: &str) -> RStarter {
        let (tx, rx) = std::sync::mpsc::channel::<(Command, std::sync::mpsc::Sender<io::Result<std::process::Child>>)>();
        std::thread::spawn(move || {
            for (mut command, reply) in rx {
                let _ = reply.send(command.spawn());
            }
        });
        RStarter {
            rscript: args.r.clone(),
            library: args.r_library.clone(),
            own_library: crate::paths::Env::here().r_library(EMBER_COMMIT),
            install: Arc::new(std::sync::Mutex::new(Install::Idle)),
            adapter: args.runtime.join("r").join("adapter.R"),
            state: args.state_dir.join(R_STATE),
            token: token.to_owned(),
            spawn: std::sync::Mutex::new(tx),
            child: std::sync::Mutex::default(),
        }
    }

    /// Start the adapter and wait until it's up: what answers for R, and Ember's page on `/ember/`.
    fn start(&self, served: &Served) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        // Ember, its install and R's adapter haven't been tried on Windows.
        if cfg!(windows) {
            return Err("unsupported::R notebooks don't work on Windows yet".into());
        }
        let library = match &self.library {
            Some(library) => PathBuf::from(library),
            None => {
                if !self.own_library.is_dir() {
                    return Err(self.install());
                }
                self.own_library.clone()
            }
        };
        let _ = std::fs::remove_file(&self.state);
        let mut command = Command::new(&self.rscript);
        command.arg("--vanilla").arg(&self.adapter).env("ENDEAVOR_TOKEN", &self.token).env("ENDEAVOR_R_STATE", &self.state).env("R_LIBS", &library).stdin(Stdio::null());
        // SAFETY: only async-signal-safe calls between fork and exec.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.spawn.lock().unwrap().send((command, tx)).map_err(|e| e.to_string())?;
        let mut child = rx.recv().map_err(|e| e.to_string())?.map_err(|e| format!("r_not_found::Couldn't start R ({}): {e}", self.rscript))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let state = loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("r_failed::R stopped while starting ({status}); the runtime's log says why"));
            }
            if let Ok(state) = std::fs::read(&self.state).map_err(|_| ()).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|_| ())) {
                break state;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("r_failed::R's adapter didn't start within 60 seconds".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let port = |key: &str| state[key].as_u64().and_then(|p| u16::try_from(p).ok()).ok_or_else(|| format!("r_failed::R's state has no {key}: {state}"));
        let (bridge, ember) = (port("bridge_port")?, port("ember_port")?);
        let secret = state["ember_secret"].as_str().ok_or("r_failed::R's state has no ember_secret")?.to_owned();
        *served.ember.lock().unwrap() = Some(Page { port: ember, secret });
        if let Some(mut old) = self.child.lock().unwrap().replace(child) {
            let _ = old.kill();
            let _ = old.wait();
        }
        Ok(Arc::new(crate::notebooks::R::new(bridge, self.token.clone())))
    }

    /// Install Ember into Endeavor's own library in the background, the first
    /// time it's needed: why R notebooks can't open yet. Installing takes
    /// minutes (packages build from source), longer than an agent's call may
    /// wait, so the call that starts it returns at once.
    fn install(&self) -> String {
        let mut install = self.install.lock().unwrap();
        match std::mem::replace(&mut *install, Install::Running) {
            Install::Running => {}
            // Said once; the next call tries again.
            Install::Failed(why) => {
                *install = Install::Idle;
                return why;
            }
            Install::Idle => {
                let mut command = Command::new(&self.rscript);
                command.arg("--vanilla").arg(self.adapter.with_file_name("install.R")).arg(&self.own_library).arg(EMBER_COMMIT).stdin(Stdio::null());
                let (state, rscript) = (self.install.clone(), self.rscript.clone());
                std::thread::spawn(move || {
                    let done = match command.status() {
                        Ok(status) if status.success() => Install::Idle,
                        Ok(_) => Install::Failed("r_failed::Couldn't install Ember for R notebooks; the runtime's log says why".into()),
                        Err(e) => Install::Failed(format!("r_not_found::Couldn't start R ({rscript}): {e}")),
                    };
                    *state.lock().unwrap() = done;
                });
            }
        }
        "r_installing::Installing Ember for R notebooks, which takes a few minutes the first time. Try again in a minute.".into()
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.state);
    }
}

/// An engine's page server (Pluto, Ember): its private port and the secret it requires.
#[derive(Clone)]
struct Page {
    port: u16,
    secret: String,
}

/// What `julia boot.jl` reports once it's up.
struct JuliaReady {
    pluto: Page,
    /// Julia's own bridge, for the adapter's calls.
    bridge_port: u16,
}

/// Once Julia has written its state and its bridge answers, write
/// `runtime.json` for the helper: the core's pid and its one `port`, and
/// Julia's launcher, node and job, and whether it ends itself when idle. Pluto's port and secret stay out of it.
/// Gives Julia's bridge port.
fn julia_ready(julia_state: &Path, state_dir: &Path, port: u16, served: &Served, starting: &mut Option<File>) -> Option<u16> {
    let bridge = &served.bridge;
    let token = &bridge.token;
    let julia: Value = serde_json::from_str(&std::fs::read_to_string(julia_state).ok()?).ok()?;
    let port_of = |key: &str| julia[key].as_u64().and_then(|p| u16::try_from(p).ok());
    let ready = JuliaReady {
        pluto: Page { port: port_of("pluto_port")?, secret: julia["pluto_secret"].as_str()?.to_owned() },
        bridge_port: port_of("mcp_port")?,
    };
    if !bridge_call(ready.bridge_port, "/call", token, "ping").is_ok_and(|status| status == 200) {
        return None;
    }
    // Before `runtime.json` says the runtime is ready: whoever finds it then may rely on the folder.
    if let Some(standalone) = &bridge.standalone {
        set_pluto_folder(ready.bridge_port, token, &standalone.folder);
    }
    // With the pid, what tells the core from a later process given its pid.
    let started = crate::own_start_time();
    let boot = crate::own_boot();
    let mut state = json!({
        "launcher": julia["launcher"], "node": julia["node"], "job": julia["job"],
        "pid": std::process::id(), "started": started, "boot": boot, "port": port, "token": token, "exits_when_idle": bridge.notebooks.exits_when_idle,
        "interface": INTERFACE,
    });
    if let Some(standalone) = &bridge.standalone {
        state["folder"] = standalone.folder.clone().into();
        if standalone.no_folder {
            state["no_folder"] = true.into();
        }
    }
    if let Some(build) = bridge.notebooks.build.get() {
        state["build"] = build.clone().into();
    }
    // Before the record: a client that finds it may call at once, so these must be set first.
    let bridge_port = ready.bridge_port;
    let _ = served.pluto.set(ready.pluto);
    let _ = bridge.julia.port.set(bridge_port);
    if let Err(e) = write_private(&state_dir.join("runtime.json"), state.to_string().as_bytes()) {
        eprintln!("endeavor core: {e}");
        return None;
    }
    // Let go after the record is written: whoever sees the lock free and no record knows the start died.
    if let Some(file) = starting.take() {
        crate::runtime::release_starting(file);
    }
    Some(bridge_port)
}

/// Have Pluto's page suggest the notebooks' folder for new notebooks, as the
/// app does for its session's folder.
fn set_pluto_folder(julia_port: u16, token: &str, folder: &str) {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "endeavor/set_folder", "params": { "path": folder } }).to_string();
    let bearer = format!("Bearer {token}");
    let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
    if let Err(e) = http::post(julia_port, "/call", &headers, body.as_bytes()) {
        eprintln!("endeavor core: couldn't give Pluto the notebooks' folder: {e}");
    }
}

/// End the runtime once no notebook has been open for the idle limit (none
/// when it's 0): a runtime the stdio form or a server connection started in the
/// background has no one to stop it.
fn exit_when_idle(served: Arc<Served>, julia_port: u16) {
    std::thread::spawn(move || {
        let notebooks = &served.bridge.notebooks;
        let mut empty_since = std::time::Instant::now();
        loop {
            std::thread::sleep(crate::notebooks::idle_check());
            let hours = notebooks.idle_limit_hours();
            if notebooks.open_count() != Some(0) || hours <= 0.0 {
                empty_since = std::time::Instant::now();
            } else if empty_since.elapsed().as_secs_f64() >= hours * 3600.0 {
                eprintln!("[ Info: No notebook open for {hours} hours; stopping");
                let _ = bridge_call(julia_port, "/call", &served.bridge.token, "endeavor/shutdown");
                return;
            }
        }
    });
}

/// Write `path` whole and readable only by us, so a reader never sees half of
/// it and nobody else sees the token.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    owner_only(OpenOptions::new().write(true).create_new(true))
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

/// Exit the way Julia did, so the helper reports the same status.
fn exit_like(status: ExitStatus) -> ! {
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        // SAFETY: plain syscalls; the default action of `signal` ends this process.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(signal);
        }
    }
    std::process::exit(status.code().unwrap_or(1))
}

/// Block the stop signals (for `pass_on_stop_signals` to take) before any
/// thread starts; the mask we had, for Julia.
#[cfg(unix)]
fn block_stop_signals() -> (libc::sigset_t, libc::sigset_t) {
    // SAFETY: initializing and applying signal sets on this (still only) thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in STOP_SIGNALS {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        (set, old)
    }
}

/// A stop signal sent to the core goes to Julia; the core exits when Julia does.
#[cfg(unix)]
fn pass_on_stop_signals(set: libc::sigset_t, julia_pid: i32) {
    std::thread::spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` holds only signals blocked in every thread.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 {
                // SAFETY: plain syscall.
                unsafe { libc::kill(julia_pid, signal) };
            }
        }
    });
}

/// What the runtime's port serves: the bridge, and once Julia is ready, Pluto.
struct Served {
    bridge: Bridge,
    pluto: OnceLock<Page>,
    /// Ember's, while it runs, for `/ember/`.
    ember: std::sync::Mutex<Option<Page>>,
    /// The cookie that lets a browser into Pluto's page (`cookie_name`).
    cookie: String,
    /// What a browser that hasn't been let in sees (`not_let_in`).
    not_let_in: String,
}

/// Serve the runtime's port: a thread per client, of which there are only a
/// few (the helper's relayed streams).
fn accept(listener: TcpListener, served: Arc<Served>) {
    std::thread::spawn(move || {
        each_connection(listener.incoming(), |client| {
            let served = served.clone();
            std::thread::spawn(move || {
                let _ = serve_client(client, &served);
            });
        })
    });
}

/// Give `serve` each connection `incoming` accepts, for as long as the port is open. A failed accept (a
/// connection reset before it was taken, or no descriptors left) doesn't close the port, and a later one
/// succeeds once descriptors are free; the pause keeps a lasting failure from spinning.
fn each_connection<C>(incoming: impl Iterator<Item = io::Result<C>>, mut serve: impl FnMut(C)) {
    let mut said: Option<std::time::Instant> = None;
    for client in incoming {
        match client {
            Ok(client) => serve(client),
            Err(e) => {
                if said.is_none_or(|said| said.elapsed() > Duration::from_secs(60)) {
                    eprintln!("endeavor core: couldn't accept a connection: {e}");
                    said = Some(std::time::Instant::now());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Where a request on the runtime's port goes, by its path.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    /// The agent's MCP connection.
    Mcp,
    /// The app's notebook state stream.
    Events,
    /// The app's JSON-RPC calls (`endeavor/*`, and tools without a session).
    Call,
    /// Any other path under `/endeavor/`.
    NotFound,
    /// `/ember` and below: Ember's page, its files and its WebSocket.
    Ember,
    /// Everything else: Pluto's page, its files and its WebSocket.
    Pluto,
}

impl Route {
    /// An engine's page, which a browser reaches with the cookie.
    fn is_page(self) -> bool {
        matches!(self, Route::Pluto | Route::Ember)
    }

    fn of(path: &str) -> Route {
        match path {
            "/mcp" => Route::Mcp,
            "/endeavor/events" => Route::Events,
            "/endeavor/call" => Route::Call,
            _ if path.starts_with("/endeavor/") => Route::NotFound,
            "/ember" => Route::Ember,
            _ if path.starts_with("/ember/") => Route::Ember,
            _ => Route::Pluto,
        }
    }
}

/// One client's requests in turn, until either side closes.
fn serve_client(client: TcpStream, served: &Served) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    let bridge = &served.bridge;
    while let Some(mut request) = Head::read(&mut reader)? {
        let route = Route::of(request.path());
        let keep_alive = match access(&request, route, &bridge.token, &served.cookie) {
            Access::Granted => None,
            Access::SetCookie { location } => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", served.cookie, bridge.token);
                let close = if request.keeps_alive() { "" } else { "Connection: close\r\n" };
                write!(client, "HTTP/1.1 303 See Other\r\nLocation: {location}\r\nSet-Cookie: {cookie}\r\nContent-Length: 0\r\n{close}\r\n")?;
                Some(request.keeps_alive())
            }
            Access::Refused(status, error) => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                if error == "unauthorized" && route.is_page() && opens_a_page(&request) {
                    http::respond(&mut client, status, Some("text/html; charset=utf-8"), served.not_let_in.as_bytes(), request.keeps_alive())?;
                } else {
                    let body = json!({ "error": error }).to_string();
                    http::respond(&mut client, status, Some("application/json"), body.as_bytes(), request.keeps_alive())?;
                }
                Some(request.keeps_alive())
            }
        };
        if let Some(keep_alive) = keep_alive {
            if !keep_alive {
                break;
            }
            continue;
        }
        let (Some(pluto), Some(&julia)) = (served.pluto.get(), bridge.julia.port.get()) else {
            return refuse(&mut client, "503 Service Unavailable", "Julia isn't ready yet");
        };
        let keep_alive = match (route, request.method()) {
            (Route::Events, "GET") => return bridge.notebooks.stream_events(&request, client),
            (Route::Mcp, "POST") => bridge.mcp(&request, &mut reader, &mut client)?,
            (Route::Call, "POST") => {
                let body = http::read_body(&mut reader, request.request_body()?)?;
                match bridge.app_call(&body) {
                    Some(reply) => {
                        http::respond(&mut client, "200 OK", Some("application/json"), reply.as_bytes(), request.keeps_alive())?;
                        request.keeps_alive()
                    }
                    None => {
                        request.set_target("/call");
                        request.replace("Host", &format!("127.0.0.1:{julia}"));
                        forward(request, Some(&body), &mut reader, julia, &|_| {})?
                    }
                }
            }
            (Route::Pluto, _) => {
                request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Authorization") && !name.eq_ignore_ascii_case("Cookie"));
                request.headers.push(("Cookie".into(), format!("secret={}", pluto.secret)));
                forward(request, None, &mut reader, pluto.port, &|response| {
                    response.headers.retain(|(name, value)| !(name.eq_ignore_ascii_case("Set-Cookie") && value.trim_start().starts_with("secret=")));
                })?
            }
            (Route::Ember, _) => {
                let ember = served.ember.lock().unwrap().clone();
                match (ember_target(request.target()), ember) {
                    (Err(location), _) => {
                        http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                        let close = if request.keeps_alive() { "" } else { "Connection: close\r\n" };
                        write!(client, "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\n{close}\r\n")?;
                        request.keeps_alive()
                    }
                    (Ok(_), None) => return refuse(&mut client, "503 Service Unavailable", "R notebooks aren't running"),
                    (Ok(target), Some(ember)) => {
                        // Ember takes its secret in the query, the only way its WebSocket does. Host stays the
                        // browser's: Ember refuses a WebSocket whose Origin isn't its Host.
                        request.set_target(&with_secret(&target, &ember.secret));
                        request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Authorization") && !name.eq_ignore_ascii_case("Cookie"));
                        forward(request, None, &mut reader, ember.port, &|response| {
                            response.headers.retain(|(name, value)| !(name.eq_ignore_ascii_case("Set-Cookie") && value.trim_start().starts_with("ember_secret")));
                        })?
                    }
                }
            }
            (Route::NotFound, _) => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                http::respond(&mut client, "404 Not Found", None, b"", request.keeps_alive())?;
                request.keeps_alive()
            }
            // No server-initiated stream on /mcp, and a session lasts as long as the runtime: DELETE doesn't end one.
            _ => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                http::respond(&mut client, "405 Method Not Allowed", None, b"", request.keeps_alive())?;
                request.keeps_alive()
            }
        };
        if !keep_alive {
            break;
        }
    }
    Ok(())
}

/// Answer one request on the app's listener while the app can't reach the
/// runtime (`token` its bearer token), then close. The agent's MCP client
/// gets an answer instead of a reset, which Claude Code counts toward giving
/// up on the server for good: a tool call fails with `why`, and the rest is
/// answered as the core would. Any other request is closed unanswered.
pub fn serve_unreachable(client: TcpStream, token: &str, why: &str) -> io::Result<()> {
    let mut reader = BufReader::new(client.try_clone()?);
    let mut client = client;
    let Some(request) = Head::read(&mut reader)? else { return Ok(()) };
    if Route::of(request.path()) != Route::Mcp {
        return Ok(());
    }
    if let Access::Refused(status, error) = access(&request, Route::Mcp, token, "") {
        let body = json!({ "error": error }).to_string();
        return http::respond(&mut client, status, Some("application/json"), body.as_bytes(), false);
    }
    if request.method() == "POST" {
        crate::mcp::post(&request, &mut reader, &mut client, false, false, |message, _| crate::mcp::answer_unreachable(message, &request, why))?;
    }
    Ok(())
}

/// Whether a request may go where its route sends it.
#[derive(Debug, PartialEq)]
enum Access {
    Granted,
    /// A browser's first visit, with `token=` in its URL: it gets the cookie
    /// and goes to `location`, the same URL without the token.
    SetCookie { location: String },
    /// A status and an error code.
    Refused(&'static str, &'static str),
}

/// Whether a request may go where `route` sends it, given the runtime's
/// `token` and the name of its browser `cookie`. Loopback isn't private on a
/// shared machine, so the token is what keeps other local users out, and a
/// DNS-rebinding page sends a foreign Host.
///
/// Endeavor's own routes are a control API, never a web API: they take the
/// token only as a bearer header, and any request with an Origin came from a
/// browser page (MCP clients never send one). Pluto's page also takes the
/// cookie, as long as the request comes from that page itself: notebook
/// output runs its own JavaScript there, and must not reach the app's calls
/// (approving its own runs) or, through the cookie another runtime set on
/// this loopback address (cookies ignore ports), that runtime's Pluto.
fn access(request: &Head, route: Route, token: &str, cookie: &str) -> Access {
    let host = request.header("Host").unwrap_or_default();
    if !loopback_host(host) {
        return Access::Refused("403 Forbidden", "host_not_loopback");
    }
    let origin = request.header("Origin");
    let bearer = same(request.header("Authorization").unwrap_or_default(), &format!("Bearer {token}"));
    if !route.is_page() {
        return match (origin, bearer) {
            (Some(_), _) => Access::Refused("403 Forbidden", "browser_origin_refused"),
            (None, true) => Access::Granted,
            (None, false) => Access::Refused("401 Unauthorized", "unauthorized"),
        };
    }
    let from_this_page = origin.is_none_or(|origin| origin == format!("http://{host}"))
        && request.header("Sec-Fetch-Site").is_none_or(|site| matches!(site, "same-origin" | "none"));
    if !from_this_page {
        return Access::Refused("403 Forbidden", "browser_origin_refused");
    }
    if bearer || has_cookie(request, cookie, token) {
        return Access::Granted;
    }
    match without_token(request.target()) {
        (Some(given), location) if same(given, token) => Access::SetCookie { location },
        _ => Access::Refused("401 Unauthorized", "unauthorized"),
    }
}

/// What a browser that hasn't been let in sees when it opens one of the pages: the link in a tool
/// result has no token, so the way in is a link the agent never sees. `open` is the command that
/// gives one (`open_command`).
fn not_let_in(open: &str) -> String {
    let open = open.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let terminal = if cfg!(windows) { "PowerShell window" } else { "terminal" };
    format!(
        "<!doctype html><meta charset=utf-8><title>Endeavor</title>
<body style=\"font-family: system-ui, sans-serif; max-width: 40em; margin: 4em auto; line-height: 1.5\">
<h1>This browser can't open Endeavor's notebooks yet</h1>
<p>Endeavor lets a browser in with a link that holds the notebooks' key. The key is kept from your agent, so the link your agent gave you doesn't hold it.</p>
<ul>
<li>If an agent works with the notebooks, ask it to open the notebook again: Endeavor opens it in your browser with the key.</li>
<li>If you started <code>endeavor serve</code>, open the link it printed.</li>
<li>If the notebooks run on this computer, you can also run this in a {terminal}:<br><code>{open}</code></li>
</ul>
</body>
"
    )
}

/// The command that lets a browser in to this runtime (`endeavor open`), as this program and the
/// state folder `dir` are named here: the plugin's program isn't on the PATH. On Windows in
/// PowerShell's form (`& "C:\…\endeavor.exe" …`), since a quoted path alone is a string there.
fn open_command(dir: &Path) -> String {
    let quote = |text: &str| if text.is_empty() || text.contains([' ', '"', '\'', '$', '&']) { format!("\"{text}\"") } else { text.to_owned() };
    let program = std::env::current_exe().map_or_else(|_| "endeavor".to_owned(), |exe| exe.display().to_string());
    let mut words = vec![quote(&program)];
    words.extend(crate::HELPER_ARGS.get().copied().unwrap_or_default().iter().map(|arg| arg.to_string()));
    words.extend(["open".to_owned(), "--state-dir".to_owned(), quote(&dir.display().to_string())]);
    let line = words.join(" ");
    if cfg!(windows) { format!("& {line}") } else { line }
}

/// Whether `request` is a browser opening a page (not a script, an image or a WebSocket).
fn opens_a_page(request: &Head) -> bool {
    request.method() == "GET"
        && match request.header("Sec-Fetch-Dest") {
            Some(dest) => dest == "document",
            None => request.header("Accept").is_some_and(|accept| accept.contains("text/html")),
        }
}

/// The name of the cookie that holds `token` for browsers: one per runtime,
/// so runtimes on the same loopback address don't overwrite each other's.
fn cookie_name(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let id: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("endeavor-{id}")
}

/// Whether the request carries cookie `name` with the value `token`.
fn has_cookie(request: &Head, name: &str, token: &str) -> bool {
    request
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("Cookie"))
        .flat_map(|(_, value)| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(n, value)| n == name && same(value, token))
}

/// A target's `token` query parameter, and the target without it.
fn without_token(target: &str) -> (Option<&str>, String) {
    let Some((path, query)) = target.split_once('?') else { return (None, target.to_owned()) };
    let mut token = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| match pair.strip_prefix("token=") {
            Some(value) => {
                token = Some(value);
                false
            }
            None => !pair.is_empty(),
        })
        .collect();
    let location = if rest.is_empty() { path.to_owned() } else { format!("{path}?{}", rest.join("&")) };
    (token, location)
}

/// Where a request under `/ember` goes on Ember's own port: its target without
/// the prefix, or for bare `/ember`, where to send the browser instead.
fn ember_target(target: &str) -> Result<String, String> {
    let rest = target.strip_prefix("/ember").unwrap_or(target);
    if rest.is_empty() || rest.starts_with('?') {
        return Err(format!("/ember/{rest}"));
    }
    Ok(rest.to_owned())
}

/// `target` with Ember's `secret` as its only `secret` query parameter.
fn with_secret(target: &str, secret: &str) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut pairs: Vec<&str> = query.split('&').filter(|pair| !pair.is_empty() && !pair.starts_with("secret=")).collect();
    let ours = format!("secret={secret}");
    pairs.push(&ours);
    format!("{path}?{}", pairs.join("&"))
}

/// `Host` as clients send it: `127.0.0.1:2346`, `localhost`, `[::1]:2346`.
pub(crate) fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(_) => host.find(']').map_or("", |end| &host[..=end]),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

/// Whether `given` is `expected`, compared in constant time.
pub(crate) fn same(given: &str, expected: &str) -> bool {
    given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Pass one request to the server at `port` (Pluto, or Julia's bridge) on a
/// connection of its own, its body from `client` or already read, and the
/// response back as it comes, through `edit`. Whether the client connection
/// can carry another request.
fn forward(request: Head, body: Option<&[u8]>, client: &mut BufReader<TcpStream>, port: u16, edit: &dyn Fn(&mut Head)) -> io::Result<bool> {
    let upstream = match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)) {
        Ok(upstream) => upstream,
        Err(_) => {
            refuse(client.get_mut(), "502 Bad Gateway", "Julia isn't answering")?;
            return Ok(false);
        }
    };
    let _ = upstream.set_nodelay(true);
    let mut to_upstream = upstream.try_clone()?;
    let mut request = request;
    match body {
        None => {
            request.write_to(&mut to_upstream)?;
            http::copy_body(client, &mut to_upstream, &mut request.request_body()?)?;
        }
        Some(bytes) => {
            request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Transfer-Encoding") && !name.eq_ignore_ascii_case("Content-Length"));
            request.headers.push(("Content-Length".into(), bytes.len().to_string()));
            request.write_to(&mut to_upstream)?;
            to_upstream.write_all(bytes)?;
        }
    }
    http::relay_response(&request, client, &mut BufReader::new(upstream), edit)
}

fn refuse(client: &mut TcpStream, status: &str, why: &str) -> io::Result<()> {
    let body = json!({ "error": why }).to_string();
    write!(client, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_are_served_after_an_accept_that_failed() {
        let incoming = vec![Ok(1), Err(io::Error::from_raw_os_error(24)), Ok(2), Err(io::ErrorKind::ConnectionAborted.into()), Ok(3)];
        let mut served = Vec::new();
        each_connection(incoming.into_iter(), |client| served.push(client));
        assert_eq!(served, [1, 2, 3]);
    }

    fn head(text: &str) -> Head {
        Head::read(&mut text.as_bytes()).unwrap().unwrap()
    }

    const TOKEN: &str = "t0k3n";

    fn access_to(request: &str) -> Access {
        let request = head(request);
        access(&request, Route::of(request.path()), TOKEN, "endeavor-abc")
    }

    #[test]
    fn routes_endeavors_paths_to_the_core_and_the_rest_to_pluto() {
        assert_eq!(Route::of("/mcp"), Route::Mcp);
        assert_eq!(Route::of("/endeavor/events"), Route::Events);
        assert_eq!(Route::of("/endeavor/call"), Route::Call);
        assert_eq!(Route::of("/endeavor/nope"), Route::NotFound);
        for path in ["/ember", "/ember/", "/ember/edit", "/ember/channels"] {
            assert_eq!(Route::of(path), Route::Ember, "{path}");
        }
        for path in ["/", "/edit", "/open", "/static/x.js", "/channels", "/endeavor", "/mcp/x", "/events", "/call", "/embers", "/emberx/edit"] {
            assert_eq!(Route::of(path), Route::Pluto, "{path}");
        }
    }

    #[test]
    fn the_header_opens_every_path_and_the_cookie_only_plutos() {
        let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
        let cookie = format!("Cookie: other=1; endeavor-abc={TOKEN}\r\n");
        for path in ["/mcp", "/endeavor/call", "/endeavor/events", "/edit?id=1"] {
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{bearer}\r\n")), Access::Granted, "{path}");
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")), Access::Refused("401 Unauthorized", "unauthorized"), "{path}");
        }
        assert_eq!(access_to(&format!("GET /edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Granted);
        for path in ["/mcp", "/endeavor/call", "/endeavor/answer_run"] {
            assert_eq!(access_to(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Refused("401 Unauthorized", "unauthorized"), "{path}");
            assert_eq!(
                access_to(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\nOrigin: http://127.0.0.1:9\r\n{cookie}{bearer}\r\n")),
                Access::Refused("403 Forbidden", "browser_origin_refused"),
                "a page's request, even with the token"
            );
        }
        let wrong = |c: &str| access_to(&format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:9\r\nCookie: {c}\r\n\r\n"));
        assert_eq!(wrong(&format!("endeavor-def={TOKEN}")), Access::Refused("401 Unauthorized", "unauthorized"), "another runtime's cookie");
        assert_eq!(wrong("endeavor-abc=t0k3m"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(wrong(&format!("secret={TOKEN}")), Access::Refused("401 Unauthorized", "unauthorized"));
    }

    #[test]
    fn the_cookie_works_only_from_this_page() {
        let cookie = format!("Cookie: endeavor-abc={TOKEN}\r\n");
        let from = |headers: &str| access_to(&format!("GET /channels HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{headers}{cookie}\r\n"));
        assert_eq!(from("Origin: http://127.0.0.1:9\r\nSec-Fetch-Site: same-origin\r\n"), Access::Granted);
        assert_eq!(from("Sec-Fetch-Site: none\r\n"), Access::Granted, "the app loading the page");
        let refused = Access::Refused("403 Forbidden", "browser_origin_refused");
        assert_eq!(from("Origin: http://127.0.0.1:10\r\n"), refused, "another runtime's page");
        assert_eq!(from("Sec-Fetch-Site: same-site\r\n"), refused, "an image on another runtime's page");
        assert_eq!(from("Origin: https://example.com\r\n"), refused);
        assert_eq!(access_to(&format!("GET / HTTP/1.1\r\nHost: evil.example\r\n{cookie}\r\n")), Access::Refused("403 Forbidden", "host_not_loopback"));
    }

    #[test]
    fn the_token_in_a_url_sets_the_cookie_and_leaves_the_url() {
        let visit = |target: &str| access_to(&format!("GET {target} HTTP/1.1\r\nHost: localhost:9\r\n\r\n"));
        assert_eq!(visit(&format!("/?token={TOKEN}")), Access::SetCookie { location: "/".into() });
        assert_eq!(visit(&format!("/edit?id=1&token={TOKEN}&x=2")), Access::SetCookie { location: "/edit?id=1&x=2".into() });
        assert_eq!(visit("/?token=nope"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(
            access_to(&format!("GET /mcp?token={TOKEN} HTTP/1.1\r\nHost: localhost:9\r\n\r\n")),
            Access::Refused("401 Unauthorized", "unauthorized"),
            "only Pluto's paths take it"
        );
    }

    #[test]
    fn embers_page_is_under_its_prefix_with_its_secret_in_the_query() {
        assert_eq!(ember_target("/ember/edit?id=1"), Ok("/edit?id=1".into()));
        assert_eq!(ember_target("/ember/"), Ok("/".into()));
        assert_eq!(ember_target("/ember"), Err("/ember/".into()));
        assert_eq!(ember_target("/ember?x=1"), Err("/ember/?x=1".into()));
        assert_eq!(with_secret("/edit?id=1", "s"), "/edit?id=1&secret=s");
        assert_eq!(with_secret("/", "s"), "/?secret=s");
        assert_eq!(with_secret("/edit?secret=theirs&id=1", "s"), "/edit?id=1&secret=s", "a secret the browser sends is replaced");
    }

    #[test]
    fn embers_page_takes_the_cookie_as_plutos_does() {
        let cookie = format!("Cookie: endeavor-abc={TOKEN}\r\n");
        assert_eq!(access_to(&format!("GET /ember/edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Granted);
        assert_eq!(access_to("GET /ember/edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(
            access_to(&format!("GET /ember/edit?id=1&token={TOKEN} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")),
            Access::SetCookie { location: "/ember/edit?id=1".into() }
        );
        assert_eq!(
            access_to(&format!("GET /ember/channels HTTP/1.1\r\nHost: 127.0.0.1:9\r\nOrigin: http://127.0.0.1:10\r\n{cookie}\r\n")),
            Access::Refused("403 Forbidden", "browser_origin_refused")
        );
    }

    /// The runtime's port with a stand-in Ember on `ember` (none: not running), and its address.
    fn serving(ember: Option<u16>) -> u16 {
        let served = Arc::new(Served { bridge: Bridge::new(TOKEN.into(), ""), pluto: OnceLock::new(), ember: Default::default(), cookie: cookie_name(TOKEN), not_let_in: String::new() });
        let _ = served.pluto.set(Page { port: 1, secret: "p".into() });
        let _ = served.bridge.julia.port.set(1);
        *served.ember.lock().unwrap() = ember.map(|port| Page { port, secret: "s3cret".into() });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        accept(listener, served);
        port
    }

    fn get(port: u16, target: &str, headers: &str) -> String {
        use std::io::Read;
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n{headers}\r\n").unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn a_request_for_embers_page_reaches_ember_with_its_secret_and_the_browsers_host() {
        use std::io::{BufRead, Read};
        let ember = TcpListener::bind("127.0.0.1:0").unwrap();
        let ember_port = ember.local_addr().unwrap().port();
        let seen = std::thread::spawn(move || {
            let (stream, _) = ember.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            while reader.read_line(&mut head).unwrap() > 2 {}
            let body = "hi";
            write!(&stream, "HTTP/1.1 200 OK\r\nSet-Cookie: ember_secret_{ember_port}=s3cret\r\nSet-Cookie: theme=dark\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let _ = reader.read_to_end(&mut Vec::new());
            head
        });
        let port = serving(Some(ember_port));
        let reply = get(port, "/ember/edit?id=1&secret=theirs", "Cookie: a=b\r\n");
        let head = seen.join().unwrap();
        assert!(head.starts_with("GET /edit?id=1&secret=s3cret HTTP/1.1\r\n"), "{head}");
        assert!(head.contains(&format!("Host: 127.0.0.1:{port}\r\n")), "{head}");
        assert!(!head.contains("Cookie") && !head.contains("Authorization"), "{head}");
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("hi"), "{reply}");
        assert!(!reply.contains("s3cret") && reply.contains("theme=dark"), "{reply}");
    }

    #[test]
    fn bare_ember_redirects_and_ember_not_running_is_unavailable() {
        let port = serving(None);
        let reply = get(port, "/ember?x=1", "");
        assert!(reply.starts_with("HTTP/1.1 301") && reply.contains("Location: /ember/?x=1\r\n"), "{reply}");
        assert!(get(port, "/ember/edit?id=1", "").starts_with("HTTP/1.1 503"));
    }

    #[test]
    fn each_runtime_has_its_own_cookie() {
        assert_eq!(cookie_name("a"), "endeavor-ca978112ca1b", "from the token's SHA-256, not the token");
        assert_ne!(cookie_name("b"), cookie_name("a"));
    }
}
