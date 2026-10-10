//! `endeavor core`: the runtime the helper starts (docs/runtime-core.md).
//! It serves the runtime's one port and writes `runtime.json`. On that port it
//! serves the agent's MCP connection at `/mcp` and the app's `/endeavor/call`s
//! itself (see `mcp`), and the app's `/endeavor/events` stream (see
//! `notebooks`), driving each notebook engine through its adapter. Every other
//! path is Pluto's page, passed through to Pluto's private port with Pluto's
//! secret added, WebSockets included (docs/one-port.md), or under `/ember/`,
//! Ember's.
//!
//! Julia (`julia boot.jl`, with Pluto) is the core's child. With
//! `--julia-when-needed` it starts the first time something needs it: a Julia
//! notebook opened or made, or a browser opening Pluto's page. The core finds
//! Julia only then, so a runtime that only ever opens R notebooks never finds,
//! downloads or starts Julia. Without the flag Julia starts at once, and
//! `runtime.json` is written once it's ready, as the app expects.
//!
//! Julia shares the core's process group, which the helper created, so the
//! helper's signals to the group reach both. A stop signal sent to the core
//! alone goes on to Julia. Once Julia has started, the core exits when Julia
//! does, the same way; before that it ends on a stop signal itself. On
//! Windows the core puts itself in a Job Object as it starts, so Julia and its
//! workers end when the core does.

use std::fs::OpenOptions;
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
pub const INTERFACE: u32 = 7;

/// Where boot.jl writes its state for the core, in the state folder.
const JULIA_STATE: &str = "julia.json";

#[cfg(unix)]
const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

struct Args {
    state_dir: PathBuf,
    /// Where to find Julia: a path, or what `--julia auto`, `--julia own` and `--julia-shell` mean (`julia::find`).
    julia: crate::julia::Source,
    /// `--install-julia`: Endeavor's own Julia may be downloaded when none is found.
    install_julia: bool,
    /// `--julia-when-needed`: Julia starts the first time something needs it, not at once.
    julia_when_needed: bool,
    runtime: PathBuf,
    depot: String,
    /// The R for R notebooks, and the R library Ember is installed in (none: Endeavor's own).
    r: crate::r::Source,
    r_library: Option<String>,
    /// `--own-r`: the core is on the user's own computer, where it may offer Endeavor's own R on a Mac
    /// when none is found; `--install-r`: it may install it without asking again.
    own_r: bool,
    install_r: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = argv.iter();
    let (mut state_dir, mut julia, mut runtime, mut depot, mut r, mut r_library) = (None, None, None, None, None, None);
    let (mut install_julia, mut julia_when_needed, mut own_r, mut install_r) = (false, false, false, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--install-julia" => install_julia = true,
            "--own-r" => own_r = true,
            "--install-r" => install_r = true,
            "--julia-when-needed" => julia_when_needed = true,
            _ => {
                let value = args.next().cloned().ok_or(format!("{arg} needs a value"));
                match arg.as_str() {
                    "--state-dir" => state_dir = Some(PathBuf::from(value?)),
                    "--julia" | "--julia-shell" if julia.is_some() => return Err("give one of --julia and --julia-shell".into()),
                    "--julia" => julia = Some(value.map(crate::julia::Source::from_value)?),
                    "--julia-shell" => julia = Some(crate::julia::Source::Shell(value?)),
                    "--runtime" => runtime = Some(PathBuf::from(value?)),
                    "--depot" => depot = Some(value?),
                    "--r" | "--r-shell" if r.is_some() => return Err("give one of --r and --r-shell".into()),
                    "--r" | "--r-shell" => r = Some(crate::r::Source::from_flag(arg, value?)),
                    "--r-library" => r_library = Some(value?),
                    _ => return Err(format!("unknown argument {arg}")),
                }
            }
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        julia: julia.ok_or("--julia or --julia-shell is required")?,
        install_julia,
        julia_when_needed,
        runtime: runtime.ok_or("--runtime is required")?,
        depot: depot.ok_or("--depot is required")?,
        r: r.unwrap_or_default(),
        r_library,
        own_r,
        install_r,
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
    // Held until `runtime.json` is written (`record`), so that a client can tell a start under way from one that died.
    let starting_lock = crate::runtime::hold_starting(&args.state_dir).unwrap_or_else(|e| fail(format!("Couldn't lock {}: {e}", args.state_dir.display())));
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
    // Kept, unused, until the process exits: closing it ends the job. Before any child starts.
    #[cfg(windows)]
    let _job = crate::winproc::job_ending_with_this_process().unwrap_or_else(|e| fail(format!("Couldn't keep Julia's processes together with this one (Job Object): {e}")));
    let (stops, stopped) = std::sync::mpsc::channel();

    let cookie = cookie_name(&token);
    let port = listener.local_addr().unwrap().port();
    let token_for_r = token.clone();
    let julia = JuliaStarter::new(&args, &token, &launcher, stops.clone());
    #[cfg(unix)]
    let julia = JuliaStarter { mask: Some(inherited_mask), ..julia };
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
    let allow_r_install = Arc::new(std::sync::atomic::AtomicBool::new(args.install_r));
    let served = Arc::new(Served { bridge, pluto: OnceLock::new(), ember: Default::default(), cookie, not_let_in, julia, app_folder: Default::default(), stops, allow_r_install: allow_r_install.clone() });
    #[cfg(unix)]
    pass_on_stop_signals(stop_signals, served.clone());
    let r = Arc::new(RStarter::new(&args, &token_for_r, allow_r_install));
    let starting = (r.clone(), served.clone());
    let _ = served.bridge.notebooks.starter.set(Box::new(move |backend| match backend {
        Backend::Ember => starting.0.start(&starting.1),
        Backend::Pluto => starting.1.julia.wait(&starting.1, JULIA_WAIT).map(|()| starting.1.bridge.julia.clone() as Arc<dyn crate::notebooks::Upstream>),
    }));
    // A folder that has Julia notebooks gets Julia started ahead, so its first notebook doesn't wait. Set before
    // the first connection: the front that started the core sends its folder as soon as it can.
    let weak = Arc::downgrade(&served);
    let _ = served.bridge.on_folder.set(Box::new(move |folder, kind| {
        if let Some(served) = weak.upgrade() {
            warm_for(&served, folder, kind);
        }
    }));
    accept(listener, served.clone());

    if !args.julia_when_needed {
        // As before Julia started only when needed: `runtime.json` once Julia is ready, and the core gone if it can't start.
        served.julia.begin(&served, false);
        if let Err(why) = served.julia.wait(&served, Duration::MAX) {
            match served.julia.exited() {
                Some(status) => {
                    remove_state(&args.state_dir, std::process::id() as i32, None);
                    exit_like(status)
                }
                None => fail(why.split_once("::").map_or(why.clone(), |(_, text)| text.to_owned())),
            }
        }
    }
    if let Err(e) = record(&args.state_dir, port, &launcher, &served.bridge) {
        fail(e);
    }
    // Let go after the record is written: whoever sees the lock free and no record knows the start died.
    crate::runtime::release_starting(starting_lock);
    if exit_idle {
        exit_when_idle(served.clone());
    }
    served.bridge.notebooks.start();
    if let Some(standalone) = served.bridge.standalone.as_ref().filter(|standalone| !standalone.no_folder) {
        warm_for(&served, &standalone.folder, "unknown");
    }

    let stop = stopped.recv().unwrap_or(Stop::Shutdown);
    if !matches!(stop, Stop::Julia(_)) {
        served.julia.kill();
    }
    let _ = std::fs::remove_file(args.state_dir.join(JULIA_STATE));
    r.stop();
    remove_state(&args.state_dir, std::process::id() as i32, None);
    match stop {
        Stop::Julia(status) => exit_like(status),
        #[cfg(unix)]
        Stop::Signal(signal) => exit_like(ExitStatus::from_raw(signal)),
        Stop::Shutdown => std::process::exit(0),
    }
}

/// How long a call that needs Julia waits for it to start before it's told Julia is still starting:
/// well within the 45 s an agent's call waits.
const JULIA_WAIT: Duration = Duration::from_secs(30);

/// What ends the core.
enum Stop {
    /// Julia, once started, ended (on its own, from `endeavor/shutdown`, or from a stop signal passed on).
    Julia(ExitStatus),
    /// A stop signal came while Julia wasn't running.
    #[cfg(unix)]
    Signal(i32),
    /// `endeavor/shutdown`, or idle, while Julia wasn't running.
    Shutdown,
}

/// Write `runtime.json` for the helper: the core's pid and its one `port`, its launcher, node and
/// job, and whether it ends itself when idle.
fn record(state_dir: &Path, port: u16, launcher: &str, bridge: &Bridge) -> Result<(), String> {
    // A Slurm job's id, so a reconnect can find the job with squeue.
    let job = if launcher == "slurm" { std::env::var("SLURM_JOB_ID").unwrap_or_default() } else { String::new() };
    // With the pid, what tells the core from a later process given its pid.
    let started = crate::own_start_time();
    let boot = crate::own_boot();
    let mut state = json!({
        "launcher": launcher, "node": crate::hostname(), "job": job,
        "pid": std::process::id(), "started": started, "boot": boot, "port": port, "token": bridge.token, "exits_when_idle": bridge.notebooks.exits_when_idle,
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
    write_private(&state_dir.join("runtime.json"), state.to_string().as_bytes())
}

/// `julia boot.jl` with the environment it reads (see runtime/boot.jl), on
/// private ports free here (Pluto's, and Julia's bridge for the core), writing
/// its state to `julia_state` for the core.
fn julia_command(julia: &str, runtime: &Path, depot: &str, token: &str, launcher: &str, julia_state: &Path) -> Result<Command, String> {
    let ports = free_ports()?;
    let runtime = runtime.display();
    let mut command = Command::new(julia);
    command
        .arg("--color=no")
        .arg(format!("--project={runtime}"))
        .arg(format!("{runtime}/boot.jl"))
        .args(ports.map(|p| p.to_string()))
        .env("JULIA_DEPOT_PATH", depot)
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

/// Runs each start on one thread that lasts as long as the core: a process started on Linux ends with
/// the thread that started it (`PR_SET_PDEATHSIG`), and a request's thread ends with its connection.
struct Spawner(std::sync::Mutex<std::sync::mpsc::Sender<(Command, std::sync::mpsc::Sender<io::Result<std::process::Child>>)>>);

impl Spawner {
    fn new() -> Spawner {
        let (tx, rx) = std::sync::mpsc::channel::<(Command, std::sync::mpsc::Sender<io::Result<std::process::Child>>)>();
        std::thread::spawn(move || {
            for (mut command, reply) in rx {
                let _ = reply.send(command.spawn());
            }
        });
        Spawner(std::sync::Mutex::new(tx))
    }

    fn spawn(&self, command: Command) -> io::Result<std::process::Child> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.0.lock().unwrap().send((command, tx)).map_err(io::Error::other)?;
        rx.recv().map_err(io::Error::other)?
    }
}

/// Where Julia is.
enum Phase {
    /// Not started yet.
    Idle,
    /// Being found, downloaded or started: what it's doing, in words for the user.
    Starting(String),
    Ready,
    /// The last start failed: why, with its code. Once that has been said, the next call that needs Julia tries again.
    Failed(String),
}

struct JuliaNow {
    phase: Phase,
    /// While starting: the last sign of progress, a new step or new lines in the runtime's log.
    moved: std::time::Instant,
    /// Why the last start failed has been said.
    said: bool,
    /// How Julia ended, if the one started last did.
    exited: Option<ExitStatus>,
}

/// Finds and starts Julia (`julia boot.jl`, with Pluto) when it's first needed, once at a time.
struct JuliaStarter {
    source: crate::julia::Source,
    /// Whether Endeavor's own Julia may be downloaded; `endeavor/allow_julia_install` allows it later.
    install: std::sync::atomic::AtomicBool,
    /// Started when first needed: the client didn't find Julia, so the log says which one starts and why
    /// it couldn't. Started at once, the client said both, and the log is Julia's alone, as it was.
    when_needed: bool,
    runtime: PathBuf,
    depot: String,
    token: String,
    launcher: String,
    /// Where `julia boot.jl` writes its state for the core.
    state: PathBuf,
    log: PathBuf,
    /// The signal mask the core was started with, for Julia.
    #[cfg(unix)]
    mask: Option<libc::sigset_t>,
    spawn: Spawner,
    now: std::sync::Mutex<JuliaNow>,
    changed: std::sync::Condvar,
    /// Julia's pid while it runs, else 0.
    pid: std::sync::atomic::AtomicI32,
    /// The core is stopping: Julia ending now ends it, even mid-start.
    stopping: std::sync::atomic::AtomicBool,
    stops: std::sync::mpsc::Sender<Stop>,
}

impl JuliaStarter {
    fn new(args: &Args, token: &str, launcher: &str, stops: std::sync::mpsc::Sender<Stop>) -> JuliaStarter {
        JuliaStarter {
            source: args.julia.clone(),
            install: args.install_julia.into(),
            when_needed: args.julia_when_needed,
            runtime: args.runtime.clone(),
            depot: args.depot.clone(),
            token: token.to_owned(),
            launcher: launcher.to_owned(),
            state: args.state_dir.join(JULIA_STATE),
            log: args.state_dir.join("runtime.log"),
            #[cfg(unix)]
            mask: None,
            spawn: Spawner::new(),
            now: std::sync::Mutex::new(JuliaNow { phase: Phase::Idle, moved: std::time::Instant::now(), said: false, exited: None }),
            changed: std::sync::Condvar::new(),
            pid: 0.into(),
            stopping: false.into(),
            stops,
        }
    }

    /// Start Julia in the background unless it runs or is starting, or failed and that hasn't been said yet.
    /// A `warm` start is one nothing asked for yet (`warm_for`): it never downloads Julia, and if Julia
    /// would need downloading it leaves Julia not started, for the first call that needs it. Pluto's
    /// packages still install on Julia's first start in a fresh depot, as for any start.
    fn begin(&self, served: &Arc<Served>, warm: bool) {
        let mut now = self.now.lock().unwrap();
        let again = matches!(now.phase, Phase::Failed(_)) && now.said;
        if !(matches!(now.phase, Phase::Idle) || again) || self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        now.phase = Phase::Starting("Julia is starting".into());
        now.moved = std::time::Instant::now();
        now.exited = None;
        drop(now);
        let served = served.clone();
        std::thread::spawn(move || {
            let julia = &served.julia;
            let started = julia.start(&served, warm);
            let mut now = julia.now.lock().unwrap();
            now.said = false;
            now.phase = match (started, now.exited) {
                (Ok(ready), None) => {
                    // Set once: Julia ending after this ends the core, so there is never a second Ready.
                    let _ = served.pluto.set(ready.pluto);
                    let _ = served.bridge.julia.port.set(ready.bridge_port);
                    Phase::Ready
                }
                (Ok(_), Some(status)) => Phase::Failed(format!("julia_failed::Julia stopped as it started ({status}). The runtime's log ({}) says why.", julia.log.display())),
                (Err(why), _) if warm && why.starts_with("julia_not_found::") => {
                    eprintln!("[ Info: Julia isn't here to start ahead; it's looked for again when a Julia notebook needs it");
                    Phase::Idle
                }
                (Err(why), _) => {
                    if julia.when_needed {
                        eprintln!("endeavor core: {}", why.split_once("::").map_or(why.as_str(), |(_, text)| text));
                    }
                    Phase::Failed(why)
                }
            };
            julia.changed.notify_all();
            if matches!(now.phase, Phase::Ready) {
                drop(now);
                // Its notebooks are followed and listed from now on, whatever started it.
                served.bridge.notebooks.add_engine(Backend::Pluto, served.bridge.julia.clone());
            }
        });
    }

    /// Start Julia unless it runs, and wait up to `wait` for it: why it isn't ready, with its code, if it isn't.
    fn wait(&self, served: &Arc<Served>, wait: Duration) -> Result<(), String> {
        self.begin(served, false);
        let deadline = std::time::Instant::now().checked_add(wait);
        let mut now = self.now.lock().unwrap();
        loop {
            let left = deadline.map_or(Duration::from_secs(3600), |deadline| deadline.saturating_duration_since(std::time::Instant::now()));
            match &now.phase {
                Phase::Ready => return Ok(()),
                Phase::Failed(why) => {
                    let why = why.clone();
                    now.said = true;
                    return Err(why);
                }
                // A warm start that left Julia to be found now.
                Phase::Idle if !self.stopping.load(std::sync::atomic::Ordering::SeqCst) => {
                    drop(now);
                    self.begin(served, false);
                    now = self.now.lock().unwrap();
                }
                Phase::Idle => return Err("julia_failed::The runtime is stopping.".into()),
                Phase::Starting(step) if left.is_zero() => {
                    return Err(format!("julia_starting::{step}. The first start can take several minutes (Julia, then Pluto's packages). Try again in a minute."));
                }
                Phase::Starting(_) => now = self.changed.wait_timeout(now, left).unwrap().0,
            }
        }
    }

    /// Nothing has asked for Julia yet.
    fn idle(&self) -> bool {
        matches!(self.now.lock().unwrap().phase, Phase::Idle)
    }

    /// How the Julia started last ended, if it did.
    fn exited(&self) -> Option<ExitStatus> {
        self.now.lock().unwrap().exited
    }

    fn step(&self, words: String) {
        let mut now = self.now.lock().unwrap();
        if let Phase::Starting(step) = &mut now.phase
            && *step != words
        {
            *step = words;
            now.moved = std::time::Instant::now();
        }
    }

    /// The start made progress without a new step: new lines in the runtime's log.
    fn moved(&self) {
        self.now.lock().unwrap().moved = std::time::Instant::now();
    }

    /// Where Julia is, for the app (`endeavor/julia_status`), without starting it: `not_started`,
    /// `starting` with the step in words and how many seconds since the last sign of progress, `ready`,
    /// or `failed` with the error's code and why. Reading a failure here doesn't count as saying it: the
    /// agent's next call that needs Julia still hears why. The app's Retry is `start_now`.
    fn status(&self) -> Value {
        let now = self.now.lock().unwrap();
        match &now.phase {
            Phase::Idle => json!({ "state": "not_started" }),
            Phase::Starting(step) => json!({ "state": "starting", "step": step, "quiet_seconds": now.moved.elapsed().as_secs() }),
            Phase::Ready => json!({ "state": "ready" }),
            Phase::Failed(why) => {
                let (code, message) = why.split_once("::").unwrap_or(("julia_failed", why.as_str()));
                json!({ "state": "failed", "code": code, "message": message })
            }
        }
    }

    /// Start Julia now, for the app (`endeavor/start_julia`): its Retry after a failure, which then counts
    /// as said, or a start before anything needs Julia. Nothing changes while Julia runs or is starting.
    fn start_now(&self, served: &Arc<Served>) {
        {
            let mut now = self.now.lock().unwrap();
            if matches!(now.phase, Phase::Failed(_)) {
                now.said = true;
            }
        }
        self.begin(served, false);
    }

    /// Find Julia, start it, and wait until its bridge answers; then have Pluto suggest the notebooks' folder.
    fn start(&self, served: &Arc<Served>, warm: bool) -> Result<JuliaReady, String> {
        let install = !warm && self.install.load(std::sync::atomic::Ordering::SeqCst);
        let found = crate::julia::find(&self.source, install, &mut |line| {
            eprintln!("{line}");
            self.step(line);
        });
        let (julia, version) = found.map_err(|failure| match failure {
            crate::julia::Failure::Missing(item) => format!(
                "julia_not_found::Julia wasn't found here, and Endeavor may download its own copy only if the user agrees: {item}. Ask the user; only if they agree, call `use_machine` again with `install: true`."
            ),
            crate::julia::Failure::Failed(message) => format!("julia_failed::{message}"),
        })?;
        if self.when_needed {
            eprintln!("[ Info: Starting Julia {version} ({julia})");
        }
        self.step(format!("Julia {version} is starting and loading Pluto"));
        let _ = std::fs::remove_file(&self.state);
        let mut command = julia_command(&julia, &self.runtime, &self.depot, &self.token, &self.launcher, &self.state)?;
        #[cfg(unix)]
        if let Some(mask) = self.mask {
            // SAFETY: only async-signal-safe calls between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                    #[cfg(target_os = "linux")]
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                    Ok(())
                });
            }
        }
        let mut child = self.spawn.spawn(command).map_err(|e| format!("julia_failed::Couldn't start {julia}: {e}"))?;
        let pid = child.id() as i32;
        // To end it if it stalls, once the watcher below has it.
        #[cfg(windows)]
        let started = crate::winproc::start_time(std::os::windows::io::AsRawHandle::as_raw_handle(&child));
        self.pid.store(pid, std::sync::atomic::Ordering::SeqCst);
        // A stop that came before the pid was stored found nothing to end.
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = child.kill();
        }
        let watching = served.clone();
        std::thread::spawn(move || {
            let Ok(status) = child.wait() else { return };
            let julia = &watching.julia;
            julia.pid.store(0, std::sync::atomic::Ordering::SeqCst);
            let mut now = julia.now.lock().unwrap();
            now.exited = Some(status);
            // Once it ran, or once the core is stopping, Julia ending ends the core, as it always has.
            if matches!(now.phase, Phase::Ready) || julia.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = julia.stops.send(Stop::Julia(status));
            }
            julia.changed.notify_all();
        });
        let mut logged = file_size(&self.log);
        let ready = loop {
            if let Some(status) = self.exited() {
                return Err(format!("julia_failed::Julia stopped while starting ({status}). The runtime's log ({}) says why.", self.log.display()));
            }
            if let Some(ready) = julia_ready(&self.state, &self.token) {
                break ready;
            }
            let size = file_size(&self.log);
            if size != logged {
                logged = size;
                self.moved();
            }
            let stalled = self.now.lock().unwrap().moved.elapsed();
            if stalled >= julia_stall() {
                // It may be stuck on anything (a lock, a network share, a hung precompile): end it, so the
                // next call that needs Julia starts it afresh.
                // Through the pid the watcher clears once it has reaped Julia, so a pid reused since isn't hit.
                let pid = self.pid.load(std::sync::atomic::Ordering::SeqCst);
                #[cfg(unix)]
                if pid > 0 {
                    // SAFETY: plain syscall, on the Julia this start began, which hasn't been reaped.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                #[cfg(windows)]
                if let Some(process) = crate::winproc::Process::open(pid, started) {
                    process.terminate();
                }
                return Err(format!(
                    "julia_failed::Julia made no progress for {} minutes while starting (no new step, nothing new in the runtime's log), so it was stopped. The runtime's log ({}) shows where it stopped. Tell the user; trying again starts Julia afresh, so ask them before trying again.",
                    stalled.as_secs() / 60,
                    self.log.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        // Before Julia counts as ready: whoever then opens a notebook may rely on the folder.
        let folder = served.app_folder.lock().unwrap().clone().or_else(|| served.bridge.standalone.as_ref().map(|standalone| standalone.folder.clone()));
        if let Some(folder) = folder {
            set_pluto_folder(ready.bridge_port, &self.token, &folder);
        }
        Ok(ready)
    }

    /// End Julia if it runs: the core is stopping.
    fn kill(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let pid = self.pid.load(std::sync::atomic::Ordering::SeqCst);
        // On Windows the job ends it with the core.
        #[cfg(unix)]
        if pid > 0 {
            // SAFETY: plain syscall.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let _ = pid;
    }
}

/// How long Julia may start with no sign of progress (a new step, or new lines in the runtime's log)
/// before the start fails and Julia is ended. A first start installs and compiles Pluto's packages for
/// minutes, writing a line to the log as each package finishes. The longest quiet stretch is compiling
/// Pluto itself: 75 s on a Linux cloud machine with Julia 1.12.6 (2026-10-10). 30 minutes leaves room
/// for a machine twenty times slower (antivirus scanning each file on Windows, say).
/// ENDEAVOR_TEST_JULIA_STALL_SECS sets it in debug builds.
fn julia_stall() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_JULIA_STALL_SECS").ok()?.parse().ok()).flatten();
    test.map_or(Duration::from_secs(30 * 60), Duration::from_secs)
}

/// The size of a file, 0 when there is none.
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Where R's adapter writes its state for the core, in the state folder.
const R_STATE: &str = "r.json";

/// Ember's page secret, which is the core's to make: R is handed it, not asked for it. It goes into
/// page URLs and a cookie, so it is hex; and it is 32 random bytes, more than Ember's own
/// `random_secret(32)` (32 letters and digits, about 190 bits).
fn ember_secret() -> Result<String, String> {
    crate::random_hex::<32>()
}

/// Ember's r-universe repository, which builds Ember's latest main. R notebooks install Ember from
/// it, and update it when R starts (`runtime/r/install.R`).
pub const EMBER_REPOSITORY: &str = "https://jowch.r-universe.dev";

/// Where Ember's CI publishes its Apple Silicon Mac builds, one release per R version: r-universe has
/// none (Ember #66). `install.R` installs from them first on an Apple Silicon Mac.
pub const EMBER_MAC_ARM64: &str = "https://github.com/jowch/Ember/releases/download";

/// How long the call that starts R waits for the check for a newer Ember, or its install, before
/// answering `r_installing`: as long as Julia's start.
const EMBER_WAIT: Duration = JULIA_WAIT;

/// What `install.R` exits with when Ember's newest build failed and the one before it is used.
const KEPT_PREVIOUS: i32 = 3;

/// The answer that offers Endeavor's own R, `item`, when no R is found on the user's Mac. Through
/// `endeavor mcp` the agent installs it with `use_machine` once the user agrees; the app's agents
/// have no `use_machine`, so there the user installs it in Settings.
fn own_r_offer(item: &wire::Item, in_app: bool) -> String {
    if in_app {
        format!(
            "r_not_found::No R was found on this computer. Endeavor can install its own R: {item}. Tell the user they can install it in the app's Settings, under Notebooks, then R. Once it's installed, open the notebook again."
        )
    } else {
        format!(
            "r_not_found::No R was found here, and Endeavor may install its own R only if the user agrees: {item}. Ask the user; only if they agree, call `use_machine` with `machine` \"local\" and `install: true`, then open the notebook again."
        )
    }
}

/// Why R wasn't found, and what the user can do about it: Endeavor installs its own R only on a Mac.
fn r_not_found(r: &crate::r::Source) -> String {
    format!(
        "r_not_found::Couldn't find R: no {}. R notebooks need R with the tools to build R packages. The user can install it with rig (https://github.com/r-lib/rig) or the system's packages; on a cluster, they can set this machine's R to a shell line such as `module load R`.",
        r.describe()
    )
}

/// Whether this computer can build R packages from source: on a Mac, whether its developer tools are
/// installed (asked without running `cc`, which on a Mac without them opens an installer), elsewhere
/// whether `cc` and `make` are on the PATH.
fn has_compiler() -> bool {
    let quiet = |command: &mut Command| command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if cfg!(target_os = "macos") {
        quiet(Command::new("xcode-select").arg("-p"))
    } else {
        quiet(Command::new("sh").args(["-c", "command -v cc && command -v make"]))
    }
}

/// Why R didn't start when its shell line ended well.
const ENDED_EARLY: &str = "The shell line for R ended without starting R. A line that ends in a comment (#) hides what comes after it.";

/// Why R notebooks use the Ember before the newest, for the agent.
const EMBER_PREVIOUS: &str = "ember_previous::Ember's newest build didn't work here, so R notebooks use the Ember before it. The runtime's log says why. Mention it to the user if R notebooks act up.";

/// Where installing or updating Ember is.
enum Install {
    Idle,
    Running,
    /// Done while no call waited for it: what the next call says.
    Done(Result<bool, String>),
    /// Endeavor's own R didn't install: said once, and the next call tries again.
    Failed(String),
}

/// The Ember builds in Endeavor's own folder (`ember.dcf`, which `install.R` writes): the one in use,
/// the one before it, and the ones that failed here.
#[derive(Debug, Default, PartialEq)]
struct EmberBuilds {
    current: String,
    previous: String,
    failed: String,
}

impl EmberBuilds {
    fn read(folder: &Path) -> EmberBuilds {
        let text = std::fs::read_to_string(folder.join("ember.dcf")).unwrap_or_default();
        let field = |key: &str| text.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix(':')).unwrap_or_default().trim().to_owned();
        EmberBuilds { current: field("Current"), previous: field("Previous"), failed: field("Failed") }
    }

    fn write(&self, folder: &Path) -> std::io::Result<()> {
        let tmp = folder.join(format!("ember.dcf.{}", std::process::id()));
        std::fs::write(&tmp, format!("Current: {}\nPrevious: {}\nFailed: {}\n", self.current, self.previous, self.failed))?;
        std::fs::rename(&tmp, folder.join("ember.dcf"))
    }

    /// Whether the build in use is there, as `install.R`'s `installed` checks.
    fn installed(&self, folder: &Path) -> bool {
        !self.current.is_empty() && folder.join(&self.current).join("ember").is_dir()
    }

    /// R's libraries with build `name`'s Ember, for `R_LIBS`.
    fn libraries(folder: &Path, name: &str) -> String {
        format!("{}:{}", folder.join(name).display(), folder.join("deps").display())
    }

    /// The build in use failed: use the one before it, and don't try this one again.
    fn fall_back(&mut self) {
        self.failed = self.failed.split_whitespace().chain([self.current.as_str()]).collect::<Vec<_>>().join(" ");
        self.current = std::mem::take(&mut self.previous);
    }
}

/// Starts R's adapter (`runtime/r/adapter.R`), with Ember in it, the first time an R notebook is
/// opened, and stops it when the core ends.
struct RStarter {
    r: crate::r::Source,
    /// The core's `--r-library`, which has Ember; else Endeavor's own, installed and updated from r-universe.
    library: Option<String>,
    ember: PathBuf,
    /// `EMBER_REPOSITORY`, or in a debug build `ENDEAVOR_TEST_EMBER_REPOSITORY`.
    repository: String,
    /// `EMBER_MAC_ARM64`, or nothing with a test repository, which is then the only one.
    mac_arm64: String,
    install: Arc<std::sync::Mutex<Install>>,
    /// Whether Endeavor's own R may be installed (`--install-r`, or `endeavor/allow_r_install` later),
    /// and where installing it is.
    allow_own: Arc<std::sync::atomic::AtomicBool>,
    own_install: Arc<std::sync::Mutex<Install>>,
    /// Whether Endeavor's own R may be offered here (`--own-r`): only on the user's own computer.
    offer_own: bool,
    /// Whether the login shell's R was found once: it isn't looked for again.
    shell_has_r: std::sync::atomic::AtomicBool,
    adapter: PathBuf,
    state: PathBuf,
    token: String,
    spawn: Spawner,
    child: std::sync::Mutex<Option<std::process::Child>>,
}

impl RStarter {
    fn new(args: &Args, token: &str, allow_own: Arc<std::sync::atomic::AtomicBool>) -> RStarter {
        let test_repository = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_EMBER_REPOSITORY").ok()).flatten();
        RStarter {
            allow_own,
            offer_own: args.own_r,
            own_install: Arc::new(std::sync::Mutex::new(Install::Idle)),
            shell_has_r: false.into(),
            r: args.r.clone(),
            library: args.r_library.clone(),
            ember: crate::paths::Env::here().ember_folder(),
            repository: test_repository.clone().unwrap_or_else(|| EMBER_REPOSITORY.into()),
            mac_arm64: if test_repository.is_some() { String::new() } else { EMBER_MAC_ARM64.into() },
            install: Arc::new(std::sync::Mutex::new(Install::Idle)),
            adapter: args.runtime.join("r").join("adapter.R"),
            state: args.state_dir.join(R_STATE),
            token: token.to_owned(),
            spawn: Spawner::new(),
            child: std::sync::Mutex::default(),
        }
    }

    /// Start the adapter and wait until it's up: what answers for R, and Ember's page on `/ember/`.
    /// With Endeavor's own Ember, first update it if r-universe has a newer build, and if the
    /// adapter doesn't start with a new build, start it with the one before.
    fn start(&self, served: &Served) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        // Ember, its install and R's adapter haven't been tried on Windows.
        if cfg!(windows) {
            return Err("unsupported::R notebooks don't work on Windows yet".into());
        }
        self.own_r(served.bridge.standalone.is_none())?;
        let folder = &self.ember_folder();
        if let Some(library) = &self.library {
            return self.launch(served, library);
        }
        match self.update(folder) {
            Ok(true) => self.tell_previous(served),
            Ok(false) => {}
            // install.R failed in a way it didn't plan for: the installed Ember still works.
            Err(why) if why.starts_with(INSTALL_FAILED_START) && EmberBuilds::read(folder).installed(folder) => {
                eprintln!("endeavor core: couldn't check for a newer Ember; starting R with the installed one");
            }
            Err(why) => return Err(why),
        }
        let mut builds = EmberBuilds::read(folder);
        if !builds.installed(folder) {
            return Err(INSTALL_FAILED.into());
        }
        match self.launch(served, &EmberBuilds::libraries(folder, &builds.current)) {
            Err(why) if why.starts_with(STOPPED_STARTING) && !builds.previous.is_empty() => {
                eprintln!("endeavor core: R's adapter didn't start with Ember {}; starting it with Ember {}", builds.current, builds.previous);
                // Read again: another runtime's install may have changed it while R started.
                let latest = EmberBuilds::read(folder);
                if latest.current != builds.current && latest.installed(folder) {
                    // It installed a newer build: start with that, and leave the record to it.
                    return self.launch(served, &EmberBuilds::libraries(folder, &latest.current));
                }
                builds = latest;
                let failed = builds.current.clone();
                builds.fall_back();
                if let Err(e) = builds.write(folder) {
                    eprintln!("endeavor core: couldn't record that Ember failed: {e}");
                }
                // Dated from now for install.R's cleanup, which waits a week: another runtime may have it loaded.
                let _ = std::fs::File::open(folder.join(&failed)).and_then(|dir| dir.set_modified(std::time::SystemTime::now()));
                let started = self.launch(served, &EmberBuilds::libraries(folder, &builds.current))?;
                self.tell_previous(served);
                Ok(started)
            }
            started => started,
        }
    }

    /// Tell the agent, with its next notebook, that R notebooks use the Ember before the newest.
    fn tell_previous(&self, served: &Served) {
        served.bridge.notebooks.warn_next(EMBER_PREVIOUS.into());
    }

    fn launch(&self, served: &Served, library: &str) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        let _ = std::fs::remove_file(&self.state);
        let secret = ember_secret().map_err(|e| format!("r_failed::{e}"))?;
        let mut command = self.r.command(&[Path::new("--vanilla"), &self.adapter]);
        command.env("ENDEAVOR_TOKEN", &self.token).env("ENDEAVOR_EMBER_SECRET", &secret).env("ENDEAVOR_R_STATE", &self.state).env("R_LIBS", library).stdin(Stdio::null());
        // SAFETY: only async-signal-safe calls between fork and exec.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = self.spawn.spawn(command).map_err(|e| format!("r_not_found::Couldn't start R ({}): {e}", self.r.describe()))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let state = loop {
            if let Ok(Some(status)) = child.try_wait() {
                if self.r.not_found(status) {
                    return Err(r_not_found(&self.r));
                }
                if self.r.ended_early(status) {
                    return Err(format!("r_failed::{}", ENDED_EARLY));
                }
                return Err(format!("{STOPPED_STARTING} ({status}); the runtime's log says why"));
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
        *served.ember.lock().unwrap() = Some(Page { port: ember, secret });
        if let Some(mut old) = self.child.lock().unwrap().replace(child) {
            let _ = old.kill();
            let _ = old.wait();
        }
        Ok(Arc::new(crate::notebooks::R::new(bridge, self.token.clone())))
    }

    /// The folder Endeavor installs Ember in: one for Endeavor's own R, which is kept apart from the
    /// user's R, and one for any other.
    fn ember_folder(&self) -> PathBuf {
        match self.r.own() {
            Some(own) => own.join("ember"),
            None => self.ember.clone(),
        }
    }

    /// When `--r auto` finds no R on a Mac, Endeavor's own: nothing when R is there, else why R
    /// notebooks can't open yet. Without the user's yes, the answer asks for it; with it, R is
    /// installed in the background (a minute or two), and the call that starts that returns at once.
    /// In the app (`in_app`), whose agents have no `use_machine`, the user installs it from Settings.
    fn own_r(&self, in_app: bool) -> Result<(), String> {
        use std::sync::atomic::Ordering::SeqCst;
        let Some(item) = self.r.own_item().filter(|_| self.offer_own) else { return Ok(()) };
        if self.shell_has_r.load(SeqCst) {
            return Ok(());
        }
        let mut probe = self.r.command(&[Path::new("--version")]);
        if probe.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status| !self.r.not_found(status)) {
            self.shell_has_r.store(true, SeqCst);
            return Ok(());
        }
        let mut install = self.own_install.lock().unwrap();
        match std::mem::replace(&mut *install, Install::Running) {
            Install::Running => {}
            // Said once; the next call tries again.
            Install::Failed(why) => {
                *install = Install::Idle;
                return Err(why);
            }
            Install::Idle | Install::Done(_) if !self.allow_own.load(SeqCst) => {
                *install = Install::Idle;
                return Err(own_r_offer(&item, in_app));
            }
            Install::Idle | Install::Done(_) => {
                let state = self.own_install.clone();
                std::thread::spawn(move || {
                    let done = match crate::r::install_own(&mut |line| eprintln!("{line}")) {
                        Ok(_) => Install::Idle,
                        Err(why) => {
                            eprintln!("endeavor core: {why}");
                            Install::Failed(format!("r_failed::{why}"))
                        }
                    };
                    *state.lock().unwrap() = done;
                });
            }
        }
        Err(format!("r_installing::Installing Endeavor's own R {}, which takes a minute or two. Try again in a minute.", crate::r::OWN_VERSION))
    }

    /// Install Ember into Endeavor's own folder, or update it if r-universe has a newer build
    /// (`install.R`), waiting a little for it: true when the newest build failed and the one
    /// before it is used. Installing the first time takes minutes (packages build from source),
    /// longer than an agent's call may wait, so then the call answers `r_installing` and the
    /// install goes on. An update that takes longer goes on too, and R starts with the Ember
    /// installed; the next start uses the new one.
    fn update(&self, folder: &Path) -> Result<bool, String> {
        let installing = |updating: bool| {
            if updating {
                eprintln!("endeavor core: still updating Ember; starting R with the installed one");
                return Ok(false);
            }
            Err("r_installing::Installing Ember for R notebooks, which takes a few minutes the first time. Try again in a minute.".to_owned())
        };
        let updating = EmberBuilds::read(folder).installed(folder);
        let mut install = self.install.lock().unwrap();
        match std::mem::replace(&mut *install, Install::Running) {
            Install::Running => return installing(updating),
            // Finished after the call that started it stopped waiting; a failure is said once, and the next call tries again.
            Install::Done(done) => {
                *install = Install::Idle;
                return done;
            }
            Install::Idle | Install::Failed(_) => {}
        }
        drop(install);
        let install_r = self.adapter.with_file_name("install.R");
        let mut command = self.r.command(&[Path::new("--vanilla"), &install_r, folder, Path::new(&self.repository), Path::new(&self.mac_arm64)]);
        command.stdin(Stdio::null());
        let (state, r) = (self.install.clone(), self.r.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let done = match command.status() {
                Ok(status) if status.success() => Ok(false),
                Ok(status) if status.code() == Some(KEPT_PREVIOUS) => Ok(true),
                Ok(status) if r.not_found(status) => Err(r_not_found(&r)),
                Ok(status) if r.ended_early(status) => Err(format!("r_failed::{}", ENDED_EARLY)),
                Ok(_) if !has_compiler() => Err(NO_COMPILER.into()),
                Ok(_) => Err(INSTALL_FAILED.into()),
                Err(e) => Err(format!("r_not_found::Couldn't start R ({}): {e}", r.describe())),
            };
            *state.lock().unwrap() = Install::Done(done);
            let _ = tx.send(());
        });
        if rx.recv_timeout(EMBER_WAIT).is_err() {
            return installing(updating);
        }
        match std::mem::replace(&mut *self.install.lock().unwrap(), Install::Idle) {
            Install::Done(done) => done,
            _ => unreachable!("the install thread said it was done"),
        }
    }

    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.state);
    }
}

/// How the errors that say Ember didn't install begin.
const INSTALL_FAILED_START: &str = "r_failed::Couldn't install Ember";

/// Why R notebooks have no Ember.
const INSTALL_FAILED: &str = "r_failed::Couldn't install Ember for R notebooks; the runtime's log says why";

/// Why R notebooks have no Ember, when there's no compiler to build it.
const NO_COMPILER: &str = "r_failed::Couldn't install Ember for R notebooks: it has to be built on this computer, and there is no compiler here to build it. The runtime's log has the details.";

/// How the error that R stopped while its adapter started begins.
const STOPPED_STARTING: &str = "r_failed::R stopped while starting";

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

/// What Julia wrote in its state, once its bridge answers.
fn julia_ready(julia_state: &Path, token: &str) -> Option<JuliaReady> {
    let julia: Value = serde_json::from_str(&std::fs::read_to_string(julia_state).ok()?).ok()?;
    let port_of = |key: &str| julia[key].as_u64().and_then(|p| u16::try_from(p).ok());
    let ready = JuliaReady {
        pluto: Page { port: port_of("pluto_port")?, secret: julia["pluto_secret"].as_str()?.to_owned() },
        bridge_port: port_of("mcp_port")?,
    };
    bridge_call(ready.bridge_port, "/call", token, "ping").is_ok_and(|status| status == 200).then_some(ready)
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

/// Start Julia ahead, in the background, for a session that will use it, so its first notebook
/// doesn't wait. A caller that knows says so with `kind`: `julia` starts it, `r` never does. Otherwise
/// (`unknown`) a folder that has Julia (Pluto) notebooks starts it, and one with only R notebooks, or
/// none, never does. It never downloads Julia (`JuliaStarter::begin`).
fn warm_for(served: &Arc<Served>, folder: &str, kind: &str) {
    if !served.julia.when_needed || !served.julia.idle() || kind == "r" {
        return;
    }
    let (served, folder, said) = (served.clone(), folder.to_owned(), kind == "julia");
    std::thread::spawn(move || {
        if (said || !wire::notebooks::scan(Path::new(&folder), &[Backend::Pluto]).is_empty()) && served.julia.idle() {
            eprintln!("[ Info: Starting Julia ahead for {}", if said { "a Julia session".to_owned() } else { format!("{folder}, which has Julia notebooks") });
            served.julia.begin(&served, true);
        }
    });
}

/// End the runtime once no notebook has been open for the idle limit (none
/// when it's 0): a runtime the stdio form or a server connection started in the
/// background has no one to stop it.
fn exit_when_idle(served: Arc<Served>) {
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
                served.shutdown();
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

/// A stop signal sent to the core goes to Julia, and the core exits when Julia does; with no Julia
/// running, it ends the core.
#[cfg(unix)]
fn pass_on_stop_signals(set: libc::sigset_t, served: Arc<Served>) {
    std::thread::spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` holds only signals blocked in every thread.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 {
                let julia = &served.julia;
                julia.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
                match julia.pid.load(std::sync::atomic::Ordering::SeqCst) {
                    // SAFETY: plain syscall.
                    pid if pid > 0 => unsafe {
                        libc::kill(pid, signal);
                    },
                    _ => {
                        let _ = served.stops.send(Stop::Signal(signal));
                    }
                }
            }
        }
    });
}

/// What the runtime's port serves: the bridge, and once Julia is ready, Pluto.
struct Served {
    bridge: Bridge,
    /// Set once Julia is ready.
    pluto: OnceLock<Page>,
    julia: JuliaStarter,
    /// The folder the app last gave Pluto for new notebooks (`endeavor/set_folder`), for a Julia started later.
    app_folder: std::sync::Mutex<Option<String>>,
    stops: std::sync::mpsc::Sender<Stop>,
    /// Ember's, while it runs, for `/ember/`.
    ember: std::sync::Mutex<Option<Page>>,
    /// Whether Endeavor's own R may be installed (`RStarter::allow_own`).
    allow_r_install: Arc<std::sync::atomic::AtomicBool>,
    /// The cookie that lets a browser into Pluto's page (`cookie_name`).
    cookie: String,
    /// What a browser that hasn't been let in sees (`not_let_in`).
    not_let_in: String,
}

impl Served {
    /// End the runtime: through Julia if it runs (it exits, and the core with it), else at once.
    fn shutdown(&self) {
        let julia = &self.julia;
        julia.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        match self.bridge.julia.port.get() {
            Some(&port) if julia.pid.load(std::sync::atomic::Ordering::SeqCst) > 0 && bridge_call(port, "/call", &self.bridge.token, "endeavor/shutdown").is_ok() => {}
            _ => {
                let _ = self.stops.send(Stop::Shutdown);
            }
        }
    }

    /// One of the app's calls about Julia (`mcp::JULIA_CALLS`), which the core answers here.
    fn julia_call(self: &Arc<Self>, method: &str, body: &[u8]) -> String {
        let message: Value = serde_json::from_slice(body).unwrap_or_default();
        let mut result = json!({});
        match method {
            "endeavor/julia_status" => result = self.julia.status(),
            "endeavor/start_julia" => {
                self.julia.start_now(self);
                result = self.julia.status();
            }
            "endeavor/set_folder" => {
                let folder = message["params"]["path"].as_str().unwrap_or_default().to_owned();
                *self.app_folder.lock().unwrap() = Some(folder.clone());
                if let Some(&port) = self.bridge.julia.port.get() {
                    set_pluto_folder(port, &self.bridge.token, &folder);
                }
            }
            "endeavor/shutdown" => {
                eprintln!("[ Info: Shutting down at the app's request");
                let served = self.clone();
                // After this reply is out.
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(200));
                    served.shutdown();
                });
            }
            "endeavor/allow_julia_install" => self.julia.install.store(true, std::sync::atomic::Ordering::SeqCst),
            "endeavor/allow_r_install" => self.allow_r_install.store(true, std::sync::atomic::Ordering::SeqCst),
            _ => {}
        }
        json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }).to_string()
    }
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
fn serve_client(client: TcpStream, served: &Arc<Served>) -> io::Result<()> {
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
                        let method = serde_json::from_slice::<Value>(&body).ok().and_then(|m| m["method"].as_str().map(str::to_owned)).unwrap_or_default();
                        let reply = served.julia_call(&method, &body);
                        http::respond(&mut client, "200 OK", Some("application/json"), reply.as_bytes(), request.keeps_alive())?;
                        request.keeps_alive()
                    }
                }
            }
            (Route::Pluto, _) if served.pluto.get().is_none() => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                if !opens_a_page(&request) {
                    return refuse(&mut client, "503 Service Unavailable", "Julia isn't running");
                }
                // The runtime's own link, which an R user opens too, doesn't start Julia by itself.
                if request.target() == "/" && served.julia.idle() {
                    http::respond(&mut client, "200 OK", Some("text/html; charset=utf-8"), JULIA_NOT_STARTED_PAGE.as_bytes(), request.keeps_alive())?;
                } else {
                    // A browser opening Pluto's page starts Julia, and sees how that goes until it's ready.
                    let page = julia_starting_page(served.julia.wait(served, Duration::ZERO).err().unwrap_or_default());
                    http::respond(&mut client, "503 Service Unavailable", Some("text/html; charset=utf-8"), page.as_bytes(), request.keeps_alive())?;
                }
                request.keeps_alive()
            }
            (Route::Pluto, _) => {
                let pluto = served.pluto.get().expect("Pluto is ready");
                request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Authorization") && !name.eq_ignore_ascii_case("Cookie"));
                request.headers.push(("Cookie".into(), format!("secret={}", pluto.secret)));
                forward(request, None, &mut reader, pluto.port, "Julia", &|response| {
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
                        forward(request, None, &mut reader, ember.port, "R", &|response| {
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

/// The runtime's own link before anything asked for Julia: starting it can mean a download, so it
/// waits for a click. `/?start-julia` is any other Pluto page as far as Julia goes.
const JULIA_NOT_STARTED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Endeavor</title>
<body style=\"font-family: system-ui, sans-serif; max-width: 40em; margin: 4em auto; line-height: 1.5\">
<h1>Julia isn't running</h1>
<p>Julia starts when a Julia notebook is opened, or when you start it here. R notebooks don't need it: each opens from its own link.</p>
<p><a href=\"/?start-julia\">Start Julia and open Pluto</a></p>
</body>
";

/// What a browser opening Pluto's page sees while Julia isn't ready: that it's starting, and the page
/// reloads itself; or why it couldn't start, and that reloading tries again.
fn julia_starting_page(why: String) -> String {
    let (code, text) = why.split_once("::").unwrap_or(("", why.as_str()));
    let text = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let (refresh, title, more) = match code {
        "julia_starting" | "" => ("<meta http-equiv=refresh content=3>", "Julia is starting", "This page opens Pluto by itself once Julia is ready."),
        _ => ("", "Julia couldn't start", "Reload this page to try again."),
    };
    format!(
        "<!doctype html><meta charset=utf-8>{refresh}<title>Endeavor</title>
<body style=\"font-family: system-ui, sans-serif; max-width: 40em; margin: 4em auto; line-height: 1.5\">
<h1>{title}</h1>
<p>{text}</p>
<p>{more}</p>
</body>
"
    )
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

/// Pass one request to the server at `port` (Pluto, Julia's bridge or Ember), which runs in `language`, on a
/// connection of its own, its body from `client` or already read, and the
/// response back as it comes, through `edit`. Whether the client connection
/// can carry another request.
fn forward(request: Head, body: Option<&[u8]>, client: &mut BufReader<TcpStream>, port: u16, language: &str, edit: &dyn Fn(&mut Head)) -> io::Result<bool> {
    let upstream = match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)) {
        Ok(upstream) => upstream,
        Err(_) => {
            refuse(client.get_mut(), "502 Bad Gateway", &format!("{language} isn't answering"))?;
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
    fn own_r_is_offered_through_use_machine_or_the_apps_settings() {
        let item = wire::Item { kind: wire::KIND_RUNTIME.into(), name: "R 4.6.1".into(), size_mb: Some(165), place: Some("/x/R-4.6.1".into()) };
        let standalone = own_r_offer(&item, false);
        assert!(standalone.starts_with("r_not_found::") && standalone.contains("`use_machine`") && standalone.contains("R 4.6.1 (about 165 MB"), "{standalone}");
        let app = own_r_offer(&item, true);
        assert!(app.starts_with("r_not_found::") && app.contains("Settings, under Notebooks, then R") && !app.contains("use_machine"), "{app}");
    }

    #[test]
    fn ember_builds_read_what_install_r_writes_and_fall_back_to_the_one_before() {
        let folder = std::env::temp_dir().join(format!("endeavor-ember-builds-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(EmberBuilds::read(&folder), EmberBuilds::default(), "nothing installed");
        // As R's write.dcf writes it.
        std::fs::write(folder.join("ember.dcf"), "Current: bbbbbbbbbbbb\nPrevious: aaaaaaaaaaaa\nFailed:\n").unwrap();
        let mut builds = EmberBuilds::read(&folder);
        assert_eq!(builds, EmberBuilds { current: "bbbbbbbbbbbb".into(), previous: "aaaaaaaaaaaa".into(), failed: String::new() });
        assert_eq!(EmberBuilds::libraries(&folder, &builds.current), format!("{}:{}", folder.join("bbbbbbbbbbbb").display(), folder.join("deps").display()));
        builds.fall_back();
        assert_eq!(builds, EmberBuilds { current: "aaaaaaaaaaaa".into(), previous: String::new(), failed: "bbbbbbbbbbbb".into() });
        builds.write(&folder).unwrap();
        assert_eq!(EmberBuilds::read(&folder), builds);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn embers_secret_is_url_safe_and_32_random_bytes() {
        let (a, b) = (ember_secret().unwrap(), ember_secret().unwrap());
        assert_eq!(a.len(), 64, "32 bytes as hex: {a}");
        assert!(a.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "lower-case hex only: {a}");
        assert_ne!(a, b, "a new one each time");
    }

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

    /// What a runtime serves, with no Julia started and none to be found.
    fn stand_in() -> Arc<Served> {
        let (stops, _) = std::sync::mpsc::channel();
        let argv = ["--state-dir", "/nonexistent", "--julia", "/nonexistent/julia", "--runtime", "/nonexistent/runtime", "--depot", "/nonexistent/depot"].map(String::from);
        let julia = JuliaStarter::new(&parse_args(&argv).unwrap(), TOKEN, "process", stops.clone());
        Arc::new(Served { bridge: Bridge::new(TOKEN.into(), ""), pluto: OnceLock::new(), ember: Default::default(), cookie: cookie_name(TOKEN), not_let_in: String::new(), julia, app_folder: Default::default(), stops, allow_r_install: Default::default() })
    }

    /// The runtime's port with a stand-in Ember on `ember` (none: not running), and its address.
    fn serving(ember: Option<u16>) -> u16 {
        let served = stand_in();
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
