use std::fs::OpenOptions;
use std::io::{self, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::{self, Head};
use wire::backend::Backend;
use crate::mcp::Bridge;
use crate::{USAGE, bridge_call, owner_only, remove_state};

pub const INTERFACE: u32 = 48;

const JULIA_STATE: &str = "julia.json";

#[cfg(unix)]
const STOP_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

struct Args {
    state_dir: PathBuf,
    julia: crate::julia::Source,
    install_julia: bool,
    julia_when_needed: bool,
    runtime: PathBuf,
    depot: String,
    r: crate::r::Source,
    r_library: Option<String>,
    own_r: bool,
    install_r: bool,
    lifeline: Option<usize>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = argv.iter();
    let (mut state_dir, mut julia, mut runtime, mut depot, mut r, mut r_library) = (None, None, None, None, None, None);
    let (mut install_julia, mut julia_when_needed, mut own_r, mut install_r) = (false, false, false, false);
    let mut lifeline = None;
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
                    "--lifeline" => lifeline = Some(value?.parse().map_err(|_| "--lifeline needs a file descriptor or handle")?),
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
        lifeline,
    })
}

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
    let starting_lock = crate::runtime::hold_starting(&args.state_dir).unwrap_or_else(|e| fail(format!("Couldn't lock {}: {e}", args.state_dir.display())));
    #[cfg(unix)]
    let (stop_signals, inherited_mask) = block_stop_signals();
    #[cfg(unix)]
    crate::host_tools::set_command_mask(inherited_mask);
    if let Some(lifeline) = args.lifeline {
        watch_lifeline(lifeline, &args.state_dir);
    }
    let _lease = args.runtime.parent().and_then(crate::lease);
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let fixed_port: u16 = env("ENDEAVOR_PORT").and_then(|p| p.parse().ok()).unwrap_or(0);
    let folder = env("ENDEAVOR_FOLDER");
    let no_folder = folder.is_none() && env("ENDEAVOR_NO_FOLDER").is_some();
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
        std::env::set_current_dir(folder).unwrap_or_else(|e| fail(format!("Couldn't use {folder} as the notebooks' folder: {e}")));
    }
    let idle_hours = env("ENDEAVOR_IDLE_HOURS").and_then(|h| h.parse::<f64>().ok());
    let exit_idle = env("ENDEAVOR_EXIT_IDLE").is_some();
    let listener = TcpListener::bind(("127.0.0.1", fixed_port)).unwrap_or_else(|e| fail(format!("Couldn't open the runtime's port {fixed_port}: {e}")));
    #[cfg(windows)]
    let _job = crate::winproc::job_ending_with_this_process().unwrap_or_else(|e| fail(format!("Couldn't keep Julia's processes together with this one (Job Object): {e}")));
    let (stops, stopped) = std::sync::mpsc::channel();

    let page_token = crate::page_token(&args.state_dir).unwrap_or_else(|e| fail(e));
    let cookie = cookie_name(&page_token);
    let port = listener.local_addr().unwrap().port();
    let engine_token = crate::random_hex::<32>().unwrap_or_else(|e| fail(e));
    let julia = JuliaStarter::new(&args, &engine_token, &launcher, stops.clone());
    #[cfg(unix)]
    let julia = JuliaStarter { mask: Some(inherited_mask), ..julia };
    let mut bridge = Bridge::new(token, engine_token.clone(), &args.depot);
    bridge.standalone = folder.map(|folder| crate::mcp::Standalone { port, folder, no_folder, host: env("ENDEAVOR_HOST_TOOLS") });
    if let Ok(build) = std::env::var("ENDEAVOR_BUILD") {
        let _ = bridge.notebooks.build.set(build);
    }
    if let Some(hours) = idle_hours {
        bridge.notebooks.set_idle_limit(hours);
    }
    bridge.notebooks.exits_when_idle.store(exit_idle, std::sync::atomic::Ordering::SeqCst);
    let not_let_in = not_let_in(&open_command(&args.state_dir));
    let allow_r_install = Arc::new(std::sync::atomic::AtomicBool::new(args.install_r));
    let allow_uv_install = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let served = Arc::new(Served { bridge, page_token, pluto: OnceLock::new(), ember: Default::default(), cookie, not_let_in, julia, app_folder: Default::default(), stops, allow_r_install: allow_r_install.clone(), allow_uv_install: allow_uv_install.clone(), r: OnceLock::new(), marimo: Default::default(), state_dir: args.state_dir.clone(), left: Default::default(), left_generation: Default::default() });
    #[cfg(unix)]
    pass_on_stop_signals(stop_signals, served.clone());
    let r = RStarter::new(&args, &engine_token, allow_r_install);
    #[cfg(unix)]
    let r = RStarter { mask: Some(inherited_mask), ..r };
    let r = Arc::new(r);
    let _ = served.r.set(r.clone());
    let weak = Arc::downgrade(&served);
    let _ = r.after_install.set(Box::new(move || {
        let Some(served) = weak.upgrade() else { return };
        match served.bridge.notebooks.start_engine(Backend::Ember, Duration::ZERO) {
            Err(why) if !why.starts_with("r_installing::") => eprintln!("endeavor core: R didn't start after its install: {why}"),
            _ => {}
        }
    }));
    let marimo = MarimoStarter::new(&args, &engine_token, allow_uv_install);
    #[cfg(unix)]
    let marimo = MarimoStarter { mask: Some(inherited_mask), ..marimo };
    let marimo = Arc::new(marimo);
    let starting = (r.clone(), served.clone(), marimo.clone());
    let _ = served.bridge.notebooks.starter.set(Box::new(move |backend, came, wait| match backend {
        Backend::Ember => starting.0.start_noting(&starting.1, came + wait.min(ember_wait())),
        Backend::Marimo => starting.2.start(&starting.1, came + wait.min(marimo_wait())),
        Backend::Pluto => starting.1.julia.wait(&starting.1, wait.saturating_sub(came.elapsed()).min(julia_wait())).map(|()| starting.1.bridge.julia.clone() as Arc<dyn crate::notebooks::Upstream>),
    }));
    let weak = Arc::downgrade(&served);
    let _ = served.bridge.on_folder.set(Box::new(move |folder, kind| {
        if let Some(served) = weak.upgrade() {
            warm_for(&served, folder, kind);
        }
    }));
    accept(listener, served.clone());

    if !args.julia_when_needed {
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
    if let Err(e) = record(&args.state_dir, port, &launcher, &served.bridge, &served.page_token) {
        fail(e);
    }
    crate::runtime::release_starting(starting_lock);
    if exit_idle {
        exit_when_idle(served.clone());
    }
    served.bridge.notebooks.start();
    if let Some(standalone) = served.bridge.standalone.as_ref().filter(|standalone| !standalone.no_folder) {
        warm_for(&served, &standalone.folder, "unknown");
    }

    let stop = stopped.recv().unwrap_or(Stop::Shutdown);
    served.bridge.notebooks.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
    if !matches!(stop, Stop::Julia(_)) {
        served.julia.kill();
    }
    let _ = std::fs::remove_file(args.state_dir.join(JULIA_STATE));
    r.stop();
    marimo.stop();
    remove_state(&args.state_dir, std::process::id() as i32, None);
    #[cfg(unix)]
    let signalled = served.julia.signalled.load(std::sync::atomic::Ordering::SeqCst);
    match stop {
        #[cfg(unix)]
        Stop::Julia(status) if signalled > 0 && status.success() => exit_like(ExitStatus::from_raw(signalled)),
        Stop::Julia(status) => exit_like(status),
        #[cfg(unix)]
        Stop::Signal(signal) => exit_like(ExitStatus::from_raw(signal)),
        Stop::Shutdown => std::process::exit(0),
    }
}

fn starting_message(step: &str, so_far: Duration, wait: Duration) -> String {
    let words = |d: Duration| if d.as_secs() >= 90 { format!("about {} minutes", (d.as_secs() + 30) / 60) } else { format!("{} seconds", d.as_secs()) };
    let what = format!(
        "julia_starting::{step} ({} so far). The first start on a computer gets Julia and installs Pluto's packages, which can take 10 minutes or more; later starts are much faster.",
        words(so_far)
    );
    if wait.is_zero() {
        return what;
    }
    format!("{what} Tell the user once, then make the same call again right away: each call waits up to {} for the start, so don't pause between calls.", words(wait))
}

const JULIA_WAIT: Duration = Duration::from_secs(30);

fn julia_wait() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_JULIA_WAIT_SECS").ok()?.parse().ok()).flatten();
    test.map_or(JULIA_WAIT, Duration::from_secs)
}

enum Stop {
    Julia(ExitStatus),
    #[cfg(unix)]
    Signal(i32),
    Shutdown,
}

fn record(state_dir: &Path, port: u16, launcher: &str, bridge: &Bridge, page_token: &str) -> Result<(), String> {
    let job = if launcher == "slurm" { std::env::var("SLURM_JOB_ID").unwrap_or_default() } else { String::new() };
    let started = crate::own_start_time();
    let boot = crate::own_boot();
    let mut state = json!({
        "launcher": launcher, "node": crate::hostname(), "job": job,
        "pid": std::process::id(), "started": started, "boot": boot, "port": port, "token": bridge.token, "page_token": page_token, "exits_when_idle": bridge.notebooks.exits_when_idle.load(std::sync::atomic::Ordering::SeqCst),
        "interface": INTERFACE, "network": crate::network_settings(),
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

fn julia_command(julia: &str, runtime: &Path, depot: &str, token: &str, launcher: &str, julia_state: &Path) -> Result<Command, String> {
    let ports = free_ports()?;
    let runtime = runtime.display();
    let mut command = Command::new(julia);
    command
        .arg("--color=no")
        .arg(format!("--project={runtime}"))
        .arg(format!("{runtime}/boot.jl"))
        .args(ports.map(|p| p.to_string()))
        .env("JULIA_DEPOT_PATH", crate::paths::with_user_depot(depot))
        .env("ENDEAVOR_TOKEN", token)
        .env("ENDEAVOR_STATE", julia_state)
        .env("ENDEAVOR_LAUNCHER", launcher)
        .stdin(Stdio::null());
    crate::client::no_window(&mut command);
    for name in ["ENDEAVOR_PORT", "ENDEAVOR_FOLDER", "ENDEAVOR_NO_FOLDER", "ENDEAVOR_HOST_TOOLS", "ENDEAVOR_IDLE_HOURS", "ENDEAVOR_EXIT_IDLE"] {
        command.env_remove(name);
    }
    Ok(command)
}

fn free_ports() -> Result<[u16; 2], String> {
    let pluto = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let mcp = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok([&pluto, &mcp].map(|l| l.local_addr().unwrap().port()))
}

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

enum Phase {
    Idle,
    Starting(String),
    Ready,
    Failed(String),
}

struct JuliaNow {
    phase: Phase,
    moved: std::time::Instant,
    began: std::time::Instant,
    said: bool,
    exited: Option<ExitStatus>,
    started: Option<(String, String, bool)>,
}

struct JuliaStarter {
    source: std::sync::Mutex<crate::julia::Source>,
    install: std::sync::atomic::AtomicBool,
    when_needed: bool,
    runtime: PathBuf,
    depot: String,
    token: String,
    launcher: String,
    state: PathBuf,
    log: PathBuf,
    #[cfg(unix)]
    mask: Option<libc::sigset_t>,
    spawn: Spawner,
    now: std::sync::Mutex<JuliaNow>,
    changed: std::sync::Condvar,
    pid: std::sync::atomic::AtomicI32,
    #[cfg(windows)]
    started: std::sync::atomic::AtomicU64,
    stopping: std::sync::atomic::AtomicBool,
    signalled: std::sync::atomic::AtomicI32,
    stops: std::sync::mpsc::Sender<Stop>,
}

impl JuliaStarter {
    fn new(args: &Args, token: &str, launcher: &str, stops: std::sync::mpsc::Sender<Stop>) -> JuliaStarter {
        JuliaStarter {
            source: std::sync::Mutex::new(args.julia.clone()),
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
            now: std::sync::Mutex::new(JuliaNow { phase: Phase::Idle, moved: std::time::Instant::now(), began: std::time::Instant::now(), said: false, exited: None, started: None }),
            changed: std::sync::Condvar::new(),
            pid: 0.into(),
            #[cfg(windows)]
            started: 0.into(),
            stopping: false.into(),
            signalled: 0.into(),
            stops,
        }
    }

    fn begin(&self, served: &Arc<Served>, warm: bool) {
        let mut now = self.now.lock().unwrap();
        let again = matches!(now.phase, Phase::Failed(_)) && now.said;
        if !(matches!(now.phase, Phase::Idle) || again) || self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        now.phase = Phase::Starting("Julia is starting".into());
        now.moved = std::time::Instant::now();
        now.began = now.moved;
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
                    let _ = served.bridge.views.pluto.set((ready.pluto.port, ready.pluto.secret.clone()));
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
                served.bridge.notebooks.add_engine(Backend::Pluto, served.bridge.julia.clone());
            }
        });
    }

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
                Phase::Idle if !self.stopping.load(std::sync::atomic::Ordering::SeqCst) => {
                    drop(now);
                    self.begin(served, false);
                    now = self.now.lock().unwrap();
                }
                Phase::Idle => return Err("julia_failed::The runtime is stopping.".into()),
                Phase::Starting(step) if left.is_zero() => {
                    return Err(starting_message(step, now.began.elapsed(), wait));
                }
                Phase::Starting(_) => now = self.changed.wait_timeout(now, left).unwrap().0,
            }
        }
    }

    fn idle(&self) -> bool {
        matches!(self.now.lock().unwrap().phase, Phase::Idle)
    }

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

    fn moved(&self) {
        self.now.lock().unwrap().moved = std::time::Instant::now();
    }

    fn status(&self) -> Value {
        let now = self.now.lock().unwrap();
        let mut status = match &now.phase {
            Phase::Idle => json!({ "state": "not_started" }),
            Phase::Starting(step) => json!({ "state": "starting", "step": step, "quiet_seconds": now.moved.elapsed().as_secs() }),
            Phase::Ready => json!({ "state": "ready" }),
            Phase::Failed(why) => {
                let (code, message) = why.split_once("::").unwrap_or(("julia_failed", why.as_str()));
                json!({ "state": "failed", "code": code, "message": message })
            }
        };
        if let (Some((path, version, endeavors)), false) = (&now.started, matches!(now.phase, Phase::Idle)) {
            status["julia"] = json!({ "path": path, "version": version, "endeavors": endeavors });
        }
        status
    }

    fn start_now(&self, served: &Arc<Served>) {
        {
            let mut now = self.now.lock().unwrap();
            if matches!(now.phase, Phase::Failed(_)) {
                now.said = true;
            }
        }
        self.begin(served, false);
    }

    fn start(&self, served: &Arc<Served>, warm: bool) -> Result<JuliaReady, String> {
        let install = !warm && self.install.load(std::sync::atomic::Ordering::SeqCst);
        self.now.lock().unwrap().started = None;
        let source = self.source.lock().unwrap().clone();
        let found = crate::julia::find_whose(&source, install, &mut |line| {
            eprintln!("{line}");
            self.step(line);
        });
        let (julia, version, endeavors) = found.map_err(|failure| match failure {
            crate::julia::Failure::Missing(item) => format!(
                "julia_not_found::Julia wasn't found here, and Endeavor may download its own copy only if the user agrees: {item}. Ask the user; only if they agree, call `use_machine` again with `install: true`."
            ),
            crate::julia::Failure::Failed(message) => format!("julia_failed::{message}"),
        })?;
        if self.when_needed {
            eprintln!("[ Info: Starting Julia {version} ({julia})");
        }
        self.now.lock().unwrap().started = Some((julia.clone(), version.clone(), endeavors));
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
        #[cfg(unix)]
        let watch = watch_for_core_death(pid, self.mask);
        #[cfg(windows)]
        let started = crate::winproc::start_time(std::os::windows::io::AsRawHandle::as_raw_handle(&child));
        #[cfg(windows)]
        self.started.store(started.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
        self.pid.store(pid, std::sync::atomic::Ordering::SeqCst);
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = child.kill();
        }
        let watching = served.clone();
        std::thread::spawn(move || {
            let Ok(status) = child.wait() else { return };
            #[cfg(unix)]
            if let Some((mut watch, mut ended)) = watch {
                let _ = ended.write_all(b"\n");
                drop(ended);
                let _ = watch.wait();
            }
            let julia = &watching.julia;
            julia.pid.store(0, std::sync::atomic::Ordering::SeqCst);
            let mut now = julia.now.lock().unwrap();
            now.exited = Some(status);
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
        let folder = served.app_folder.lock().unwrap().clone().or_else(|| served.bridge.standalone.as_ref().map(|standalone| standalone.folder.clone()));
        if let Some(folder) = folder {
            set_pluto_folder(ready.bridge_port, &self.token, &folder);
        }
        Ok(ready)
    }

    fn kill(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let pid = self.pid.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(unix)]
        if pid > 0 {
            // SAFETY: plain syscall.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let _ = pid;
    }
}

fn julia_stall() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_JULIA_STALL_SECS").ok()?.parse().ok()).flatten();
    test.map_or(Duration::from_secs(30 * 60), Duration::from_secs)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

const R_STATE: &str = "r.json";

fn ember_secret() -> Result<String, String> {
    crate::random_hex::<32>()
}

pub const EMBER_REPOSITORY: &str = "https://jowch.r-universe.dev";

const EMBER_WAIT: Duration = JULIA_WAIT;

fn ember_wait() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_EMBER_WAIT_SECS").ok()?.parse().ok()).flatten();
    test.map_or(EMBER_WAIT, Duration::from_secs)
}

const KEPT_PREVIOUS: i32 = 3;

fn own_r_offer(item: &wire::Item, in_app: bool) -> String {
    own_r_offer_text(&item.to_string(), in_app)
}

const OWN_R_OFFERS: [(&str, &str); 2] = [
    (
        "r_not_found::No R was found on this computer. Endeavor can install its own R if the user agrees: ",
        ". Tell the user to click Install R in the notebook pane, or Install in the app's Settings under Notebooks, then R. Once it's installed, open the notebook again.",
    ),
    (
        "r_not_found::No R was found here, and Endeavor may install its own R only if the user agrees: ",
        ". Ask the user; only if they agree, call `use_machine` with `machine` \"local\" and `install: true`, then open the notebook again.",
    ),
];

fn own_r_offer_text(item: &str, in_app: bool) -> String {
    let (before, after) = OWN_R_OFFERS[usize::from(!in_app)];
    format!("{before}{item}{after}")
}

pub(crate) fn own_r_offer_for(error: &str, app: bool) -> std::borrow::Cow<'_, str> {
    for (before, after) in OWN_R_OFFERS {
        if let Some(item) = error.strip_prefix(before).and_then(|rest| rest.strip_suffix(after)) {
            return own_r_offer_text(item, app).into();
        }
    }
    error.into()
}

fn r_not_found(r: &crate::r::Source) -> String {
    format!(
        "r_not_found::Couldn't find R: no {}. R notebooks need R with the tools to build R packages. The user can install it with rig (https://github.com/r-lib/rig) or the system's packages; on a cluster, they can set this machine's R to a shell line such as `module load R`.",
        r.describe()
    )
}

fn has_compiler() -> bool {
    let quiet = |command: &mut Command| command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if cfg!(target_os = "macos") {
        quiet(Command::new("xcode-select").arg("-p"))
    } else {
        quiet(Command::new("sh").args(["-c", "command -v cc && command -v make"]))
    }
}

const ENDED_EARLY: &str = "The shell line for R ended without starting R. A line that ends in a comment (#) hides what comes after it.";

const EMBER_PREVIOUS: &str = "ember_previous::Ember's newest build didn't work here, so R notebooks use the Ember before it. The runtime's log says why. Mention it to the user if R notebooks act up.";

enum Install {
    Idle,
    Running,
    Done(Result<bool, String>),
    Failed(String, std::time::Instant),
}

fn retry_after() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_INSTALL_RETRY_SECS").ok()?.parse().ok()).flatten();
    test.map_or(Duration::from_secs(60), Duration::from_secs)
}

fn failure_kept(install: &Install) -> Option<String> {
    match install {
        Install::Failed(why, at) if at.elapsed() < retry_after() => {
            let left = retry_after().saturating_sub(at.elapsed()).as_secs().max(1);
            Some(format!("{}. That was the try {} s ago, and this call didn't try again; a call made {left} s from now does.", why.trim_end_matches('.'), at.elapsed().as_secs()))
        }
        _ => None,
    }
}

struct RNow<'a> {
    running: bool,
    own: &'a Install,
    ember: &'a Install,
    ember_installed: bool,
    launching: bool,
    failed: Option<&'a str>,
}

fn r_status(now: &RNow) -> Value {
    let failed = |why: &str| {
        let (code, message) = why.split_once("::").unwrap_or(("r_failed", why));
        json!({ "state": "failed", "code": code, "message": message })
    };
    let ember_running = matches!(now.ember, Install::Running);
    if now.running {
        return if ember_running { json!({ "state": "ready", "updating": true }) } else { json!({ "state": "ready" }) };
    }
    if matches!(now.own, Install::Running) {
        return json!({ "state": "starting", "step": format!("Installing Endeavor's own R {}", crate::r::OWN_VERSION) });
    }
    if ember_running {
        return json!({ "state": "starting", "step": if now.ember_installed { "Updating Ember" } else { "Installing Ember for R notebooks" } });
    }
    if now.launching {
        return json!({ "state": "starting", "step": "Starting R" });
    }
    match (now.own, now.ember, now.failed) {
        (Install::Failed(why, _), ..) | (_, Install::Failed(why, _) | Install::Done(Err(why)), _) => failed(why),
        (.., Some(why)) => failed(why),
        _ => json!({ "state": "not_started" }),
    }
}

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

    fn installed(&self, folder: &Path) -> bool {
        !self.current.is_empty() && folder.join(&self.current).join("ember").is_dir()
    }

    fn libraries(folder: &Path, name: &str) -> String {
        format!("{}:{}", folder.join(name).display(), folder.join("deps").display())
    }

    fn fall_back(&mut self) {
        self.failed = self.failed.split_whitespace().chain([self.current.as_str()]).collect::<Vec<_>>().join(" ");
        self.current = std::mem::take(&mut self.previous);
    }
}

fn user_r_settings(command: &mut Command, home: Option<&Path>, empty: &Path) {
    let named = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(home) = home.filter(|h| !h.as_os_str().is_empty() && !cfg!(windows)) && named("R_ENVIRON_USER").is_none() {
        command.env("R_ENVIRON_USER", home.join(".Renviron"));
    }
    command.env("ENDEAVOR_R_PROFILE", named("R_PROFILE_USER").unwrap_or_default()).env("R_PROFILE_USER", empty);
}

#[derive(Default)]
struct LastWords {
    said: std::sync::Mutex<(std::collections::VecDeque<String>, bool)>,
    ended: std::sync::Condvar,
}

impl LastWords {
    fn after_end(&self, wait: Duration) -> Vec<String> {
        let said = self.said.lock().unwrap();
        let (said, _) = self.ended.wait_timeout_while(said, wait, |(_, ended)| !*ended).unwrap();
        said.0.iter().cloned().collect()
    }
}

fn pass_on_stderr(stderr: std::process::ChildStderr) -> Arc<LastWords> {
    const KEPT: usize = 20;
    let words = Arc::new(LastWords::default());
    let keep = words.clone();
    std::thread::spawn(move || {
        let mut reader = io::BufReader::new(stderr);
        let mut line = Vec::new();
        while matches!(io::BufRead::read_until(&mut reader, b'\n', &mut line), Ok(n) if n > 0) {
            let _ = io::Write::write_all(&mut io::stderr(), &line);
            let text = String::from_utf8_lossy(&line).trim().to_owned();
            line.clear();
            if !text.is_empty() {
                let mut said = keep.said.lock().unwrap();
                said.0.push_back(text);
                if said.0.len() > KEPT {
                    said.0.pop_front();
                }
            }
        }
        keep.said.lock().unwrap().1 = true;
        keep.ended.notify_all();
    });
    words
}

fn why_stopped(who: &str, status: Option<std::process::ExitStatus>, said: &[String]) -> String {
    const LONGEST: usize = 300;
    let how = status.map_or_else(|| "it stopped answering".to_owned(), |status| status.to_string());
    let last: Vec<&str> = said.iter().rev().take(3).rev().map(String::as_str).collect();
    if last.is_empty() {
        return how;
    }
    let mut last = last.join(" ");
    if last.chars().count() > LONGEST {
        last = format!("…{}", last.chars().rev().take(LONGEST).collect::<Vec<_>>().into_iter().rev().collect::<String>());
    }
    format!("{how}; {who}'s last output: {last}")
}

struct RStarter {
    r: crate::r::Source,
    library: Option<String>,
    ember: PathBuf,
    repository: String,
    install: Arc<std::sync::Mutex<Install>>,
    allow_own: Arc<std::sync::atomic::AtomicBool>,
    own_install: Arc<std::sync::Mutex<Install>>,
    offer_own: bool,
    shell_has_r: std::sync::atomic::AtomicBool,
    adapter: PathBuf,
    state: PathBuf,
    token: String,
    #[cfg(unix)]
    mask: Option<libc::sigset_t>,
    spawn: Spawner,
    child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
    launching: std::sync::atomic::AtomicBool,
    failed: std::sync::Mutex<Option<String>>,
    after_install: Arc<OnceLock<Box<dyn Fn() + Send + Sync>>>,
}

impl RStarter {
    fn with_mask(&self, command: &mut Command) {
        #[cfg(unix)]
        if let Some(mask) = self.mask {
            // SAFETY: only async-signal-safe calls between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        let _ = command;
    }

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
            repository: test_repository.unwrap_or_else(|| EMBER_REPOSITORY.into()),
            install: Arc::new(std::sync::Mutex::new(Install::Idle)),
            adapter: args.runtime.join("r").join("adapter.R"),
            state: args.state_dir.join(R_STATE),
            token: token.to_owned(),
            #[cfg(unix)]
            mask: None,
            spawn: Spawner::new(),
            child: Arc::default(),
            launching: false.into(),
            failed: Default::default(),
            after_install: Arc::default(),
        }
    }

    fn status(&self) -> Value {
        let running = self.child.lock().unwrap().as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        let mut status = self.status_of(running);
        if running {
            status["r"] = json!({ "rscript": self.r.rscript(), "endeavors": self.r.own().is_some() });
        }
        status
    }

    fn status_of(&self, running: bool) -> Value {
        let folder = self.ember_folder();
        let own = self.own_install.lock().unwrap();
        let ember = self.install.lock().unwrap();
        let failed = self.failed.lock().unwrap();
        r_status(&RNow {
            running,
            own: &own,
            ember: &ember,
            ember_installed: EmberBuilds::read(&folder).installed(&folder),
            launching: self.launching.load(std::sync::atomic::Ordering::SeqCst),
            failed: failed.as_deref().map(|why| own_r_offer_for(why, true)).as_deref(),
        })
    }

    fn start_noting(&self, served: &Served, until: std::time::Instant) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        *self.failed.lock().unwrap() = None;
        let started = self.start(served, until);
        if let Err(why) = &started
            && !why.starts_with("r_installing::")
        {
            *self.failed.lock().unwrap() = Some(why.clone());
        }
        started
    }

    fn start(&self, served: &Served, until: std::time::Instant) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        if cfg!(windows) {
            return Err("unsupported::R notebooks don't work on Windows yet".into());
        }
        self.own_r(served.bridge.standalone.is_none(), until)?;
        let folder = &self.ember_folder();
        if let Some(library) = &self.library {
            return self.launch(served, library);
        }
        match self.update(folder, until) {
            Ok(true) => self.tell_previous(served),
            Ok(false) => {}
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
                let latest = EmberBuilds::read(folder);
                if latest.current != builds.current && latest.installed(folder) {
                    return self.launch(served, &EmberBuilds::libraries(folder, &latest.current));
                }
                builds = latest;
                let failed = builds.current.clone();
                builds.fall_back();
                if let Err(e) = builds.write(folder) {
                    eprintln!("endeavor core: couldn't record that Ember failed: {e}");
                }
                let _ = std::fs::File::open(folder.join(&failed)).and_then(|dir| dir.set_modified(std::time::SystemTime::now()));
                let started = self.launch(served, &EmberBuilds::libraries(folder, &builds.current))?;
                self.tell_previous(served);
                Ok(started)
            }
            started => started,
        }
    }

    fn tell_previous(&self, served: &Served) {
        served.bridge.notebooks.warn_next(EMBER_PREVIOUS.into());
    }

    fn launch(&self, served: &Served, library: &str) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        self.launching.store(true, std::sync::atomic::Ordering::SeqCst);
        let launched = self.launch_adapter(served, library);
        self.launching.store(false, std::sync::atomic::Ordering::SeqCst);
        launched
    }

    fn launch_adapter(&self, served: &Served, library: &str) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        let _ = std::fs::remove_file(&self.state);
        let secret = ember_secret().map_err(|e| format!("r_failed::{e}"))?;
        let mut command = self.r.command(&[Path::new("--no-save"), Path::new("--no-restore"), &self.adapter]);
        if self.r.own().is_none() {
            let empty = self.state.with_file_name("empty.Rprofile");
            std::fs::write(&empty, "").map_err(|e| format!("r_failed::Couldn't write {}: {e}", empty.display()))?;
            user_r_settings(&mut command, std::env::home_dir().as_deref(), &empty);
        }
        command.env("ENDEAVOR_TOKEN", &self.token).env("ENDEAVOR_EMBER_SECRET", &secret).env("ENDEAVOR_R_STATE", &self.state).env("R_LIBS", library).stdin(Stdio::null()).stdout(Stdio::from(io::stderr())).stderr(Stdio::piped());
        self.with_mask(&mut command);
        // SAFETY: only async-signal-safe calls between fork and exec.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = self.spawn.spawn(command).map_err(|e| format!("r_not_found::Couldn't start R ({}): {e}", self.r.describe()))?;
        let last_words = child.stderr.take().map(pass_on_stderr);
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
        served.bridge.views.set_ember(ember, secret.clone());
        *served.ember.lock().unwrap() = Some(Page { port: ember, secret });
        let (children, pid) = (self.child.clone(), child.id());
        if let Some(mut old) = self.child.lock().unwrap().replace(child) {
            let _ = old.kill();
            let _ = old.wait();
        }
        let why_ended = move || {
            let by = std::time::Instant::now() + Duration::from_secs(1);
            let said = last_words.as_ref().map(|words| words.after_end(Duration::from_secs(1))).unwrap_or_default();
            let status = loop {
                let mut children = children.lock().unwrap();
                let Some(child) = children.as_mut().filter(|child| child.id() == pid) else { break None };
                let status = child.try_wait().ok().flatten();
                if status.is_some() || std::time::Instant::now() >= by {
                    break status;
                }
                drop(children);
                std::thread::sleep(Duration::from_millis(20));
            };
            Some(why_stopped("R", status, &said))
        };
        Ok(Arc::new(crate::notebooks::R::new(bridge, self.token.clone()).telling(Box::new(why_ended))))
    }

    fn ember_folder(&self) -> PathBuf {
        match self.r.own() {
            Some(own) => own.join("ember"),
            None => self.ember.clone(),
        }
    }

    fn own_r(&self, in_app: bool, until: std::time::Instant) -> Result<(), String> {
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
        if matches!(*install, Install::Running) {
            drop(install);
            install = wait_for_install(&self.own_install, until.saturating_duration_since(std::time::Instant::now()));
            if !matches!(*install, Install::Running) {
                drop(install);
                return self.own_r(in_app, until);
            }
        }
        if let Some(why) = failure_kept(&install) {
            return Err(why);
        }
        match std::mem::replace(&mut *install, Install::Running) {
            Install::Running => {}
            Install::Idle | Install::Done(_) | Install::Failed(..) if !self.allow_own.load(SeqCst) => {
                *install = Install::Idle;
                return Err(own_r_offer(&item, in_app));
            }
            Install::Idle | Install::Done(_) | Install::Failed(..) => {
                let (state, after) = (self.own_install.clone(), self.after_install.clone());
                std::thread::spawn(move || {
                    let done = match crate::r::install_own(&mut |line| eprintln!("{line}")) {
                        Ok(_) => Install::Idle,
                        Err(why) => {
                            eprintln!("endeavor core: {why}");
                            Install::Failed(format!("r_failed::{why}"), std::time::Instant::now())
                        }
                    };
                    let installed = matches!(done, Install::Idle);
                    *state.lock().unwrap() = done;
                    if installed && let Some(start) = after.get() {
                        start();
                    }
                });
            }
        }
        Err(format!("r_installing::Installing Endeavor's own R {}, which takes a minute or two. Make the same call again right away: each call waits up to {} seconds.", crate::r::OWN_VERSION, ember_wait().as_secs()))
    }

    fn update(&self, folder: &Path, until: std::time::Instant) -> Result<bool, String> {
        let installing = |updating: bool| {
            if updating {
                eprintln!("endeavor core: still updating Ember; starting R with the installed one");
                return Ok(false);
            }
            Err(format!("r_installing::Installing Ember for R notebooks, which takes a few minutes the first time. Make the same call again right away: each call waits up to {} seconds.", ember_wait().as_secs()))
        };
        let updating = EmberBuilds::read(folder).installed(folder);
        let mut install = self.install.lock().unwrap();
        if matches!(*install, Install::Running) && !updating {
            drop(install);
            install = wait_for_install(&self.install, until.saturating_duration_since(std::time::Instant::now()));
        }
        if let Some(why) = failure_kept(&install) {
            return Err(why);
        }
        match std::mem::replace(&mut *install, Install::Running) {
            Install::Running => return installing(updating),
            Install::Done(done) => {
                *install = Install::Idle;
                return done;
            }
            Install::Idle | Install::Failed(..) => {}
        }
        drop(install);
        let install_r = self.adapter.with_file_name("install.R");
        let mut command = self.r.command(&[Path::new("--vanilla"), &install_r, folder, Path::new(&self.repository)]);
        command.stdin(Stdio::null());
        self.with_mask(&mut command);
        let (state, r, after) = (self.install.clone(), self.r.clone(), self.after_install.clone());
        let waited = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let waiting = waited.clone();
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
            let mut install = state.lock().unwrap();
            let start = !updating && done.is_ok() && !waiting.load(std::sync::atomic::Ordering::SeqCst);
            *install = match done {
                Ok(kept_previous) => Install::Done(Ok(kept_previous)),
                Err(why) => Install::Failed(why, std::time::Instant::now()),
            };
            drop(install);
            let _ = tx.send(());
            if start && let Some(start) = after.get() {
                start();
            }
        });
        let _ = rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now()));
        let mut install = self.install.lock().unwrap();
        if matches!(*install, Install::Running) {
            waited.store(false, std::sync::atomic::Ordering::SeqCst);
            return installing(updating);
        }
        match std::mem::replace(&mut *install, Install::Idle) {
            Install::Done(done) => done,
            Install::Failed(why, at) => {
                *install = Install::Failed(why.clone(), at);
                Err(why)
            }
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

fn wait_for_install(state: &std::sync::Mutex<Install>, wait: Duration) -> std::sync::MutexGuard<'_, Install> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        let install = state.lock().unwrap();
        if !matches!(*install, Install::Running) || std::time::Instant::now() >= deadline {
            return install;
        }
        drop(install);
        std::thread::sleep(Duration::from_millis(250));
    }
}

const INSTALL_FAILED_START: &str = "r_failed::Couldn't install Ember";

const INSTALL_FAILED: &str = "r_failed::Couldn't install Ember for R notebooks; the runtime's log says why";

const NO_COMPILER: &str = "r_failed::Couldn't install Ember for R notebooks: it has to be built on this computer, and there is no compiler here to build it. The runtime's log has the details.";

const MARIMO_PYTHON: &str = "3.12";

struct MarimoStarter {
    adapter: PathBuf,
    requirements: PathBuf,
    state: PathBuf,
    token: String,
    this_computer: bool,
    #[cfg(unix)]
    mask: Option<libc::sigset_t>,
    spawn: Spawner,
    child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
    pending: std::sync::Mutex<Option<MarimoPending>>,
    allow_uv: Arc<std::sync::atomic::AtomicBool>,
    uv_install: Arc<std::sync::Mutex<UvInstall>>,
}

enum UvInstall {
    Idle,
    Running(std::time::Instant),
    Failed(String),
}

struct MarimoPending {
    child: std::process::Child,
    last_words: Option<Arc<LastWords>>,
    secret: String,
    began: std::time::Instant,
}

impl MarimoStarter {
    fn new(args: &Args, token: &str, allow_uv: Arc<std::sync::atomic::AtomicBool>) -> MarimoStarter {
        let folder = args.runtime.join("marimo");
        MarimoStarter {
            adapter: folder.join("adapter.py"),
            requirements: folder.join("requirements.txt"),
            state: args.state_dir.join(MARIMO_STATE),
            token: token.to_owned(),
            this_computer: args.own_r,
            #[cfg(unix)]
            mask: None,
            spawn: Spawner::new(),
            child: Arc::default(),
            pending: Default::default(),
            allow_uv,
            uv_install: Arc::new(std::sync::Mutex::new(UvInstall::Idle)),
        }
    }

    fn start(&self, served: &Served, until: std::time::Instant) -> Result<Arc<dyn crate::notebooks::Upstream>, String> {
        if cfg!(windows) {
            return Err("unsupported::Python notebooks don't work on Windows yet".into());
        }
        if served.bridge.standalone.is_none() && !self.this_computer {
            return Err("unsupported::Python notebooks work with the Endeavor plugin on this computer (`endeavor serve` or `endeavor mcp`), not yet on a server".into());
        }
        let mut pending = self.pending.lock().unwrap();
        if pending.is_none() {
            let uv = self.uv(served, until)?;
            *pending = Some(self.spawn_adapter(&uv)?);
        }
        let deadline = pending.as_ref().unwrap().began + MARIMO_START;
        let state = loop {
            let starting = pending.as_mut().unwrap();
            if let Ok(Some(status)) = starting.child.try_wait() {
                let said = starting.last_words.as_ref().map(|words| words.after_end(Duration::from_secs(1))).unwrap_or_default();
                *pending = None;
                return Err(format!("python_failed::Python notebooks' engine stopped while starting: {}", why_stopped("Python", Some(status), &said)));
            }
            if let Ok(state) = std::fs::read(&self.state).map_err(|_| ()).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|_| ())) {
                break state;
            }
            if served.bridge.notebooks.stopping.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("python_failed::Endeavor is stopping".into());
            }
            let now = std::time::Instant::now();
            if now > deadline {
                let mut child = pending.take().unwrap().child;
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("python_failed::Python notebooks' engine didn't start within {} seconds", MARIMO_START.as_secs()));
            }
            if now > until {
                return Err(format!("python_installing::Setting up Python notebooks: uv is getting Python and marimo, which can take a few minutes the first time ({} seconds so far). Make the same call again right away: each call waits up to {} seconds.", starting.began.elapsed().as_secs(), marimo_wait().as_secs()));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let MarimoPending { mut child, last_words, secret, .. } = pending.take().unwrap();
        let port = |key: &str| state[key].as_u64().and_then(|p| u16::try_from(p).ok()).ok_or_else(|| format!("python_failed::marimo's state has no {key}: {state}"));
        let (bridge, page) = match (port("bridge_port"), port("marimo_port")) {
            (Ok(bridge), Ok(page)) => (bridge, page),
            (Err(why), _) | (_, Err(why)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(why);
            }
        };
        *served.marimo.lock().unwrap() = Some(Page { port: page, secret });
        let (children, pid) = (self.child.clone(), child.id());
        if let Some(mut old) = self.child.lock().unwrap().replace(child) {
            let _ = old.kill();
            let _ = old.wait();
        }
        drop(pending);
        let why_ended = move || {
            let said = last_words.as_ref().map(|words| words.after_end(Duration::from_secs(1))).unwrap_or_default();
            let status = children.lock().unwrap().as_mut().filter(|child| child.id() == pid).and_then(|child| child.try_wait().ok().flatten());
            Some(why_stopped("Python", status, &said))
        };
        Ok(Arc::new(crate::notebooks::R::new(bridge, self.token.clone()).telling(Box::new(why_ended))))
    }

    fn uv(&self, served: &Served, until: std::time::Instant) -> Result<PathBuf, String> {
        use std::sync::atomic::Ordering::SeqCst;
        loop {
            if let Some(uv) = find_uv() {
                return Ok(uv);
            }
            let mut install = self.uv_install.lock().unwrap();
            match std::mem::replace(&mut *install, UvInstall::Idle) {
                UvInstall::Failed(why) => return Err(format!("python_failed::{why}")),
                UvInstall::Running(began) => {
                    *install = UvInstall::Running(began);
                    if std::time::Instant::now() > until {
                        return Err(format!("python_installing::Downloading uv {} for Python notebooks ({} seconds so far). Make the same call again right away: each call waits up to {} seconds.", crate::uv::VERSION, began.elapsed().as_secs(), marimo_wait().as_secs()));
                    }
                }
                UvInstall::Idle => match crate::uv::item() {
                    Some(item) if self.allow_uv.load(SeqCst) => {
                        *install = UvInstall::Running(std::time::Instant::now());
                        let state = self.uv_install.clone();
                        std::thread::spawn(move || {
                            let done = match crate::uv::install(&mut |line| eprintln!("{line}")) {
                                Ok(_) => UvInstall::Idle,
                                Err(why) => {
                                    eprintln!("endeavor core: {why}");
                                    UvInstall::Failed(why)
                                }
                            };
                            *state.lock().unwrap() = done;
                        });
                        eprintln!("endeavor core: downloading {item} for Python notebooks");
                    }
                    Some(item) => return Err(uv_offer(&item)),
                    None => return Err(NO_UV.into()),
                },
            }
            drop(install);
            if served.bridge.notebooks.stopping.load(SeqCst) {
                return Err("python_failed::Endeavor is stopping".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn spawn_adapter(&self, uv: &Path) -> Result<MarimoPending, String> {
        let _ = std::fs::remove_file(&self.state);
        let secret = ember_secret().map_err(|e| format!("python_failed::{e}"))?;
        let mut command = Command::new(uv);
        command.args(["run", "--quiet", "--no-project", "--python", MARIMO_PYTHON, "--with-requirements"]).arg(&self.requirements).arg("python").arg(&self.adapter);
        command.env("ENDEAVOR_TOKEN", &self.token).env("ENDEAVOR_MARIMO_TOKEN", &secret).env("ENDEAVOR_MARIMO_STATE", &self.state).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        if !cfg!(debug_assertions) {
            command.env_remove("ENDEAVOR_TEST_MARIMO_INSTALL_DELAY_SECS");
        }
        #[cfg(unix)]
        if let Some(mask) = self.mask {
            // SAFETY: only async-signal-safe calls between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                    Ok(())
                });
            }
        }
        // SAFETY: only async-signal-safe calls between fork and exec.
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = self.spawn.spawn(command).map_err(|e| format!("python_not_found::Couldn't start uv ({}): {e}", uv.display()))?;
        let last_words = child.stderr.take().map(pass_on_stderr);
        Ok(MarimoPending { child, last_words, secret, began: std::time::Instant::now() })
    }

    fn stop(&self) {
        let pending = self.pending.lock().unwrap().take().map(|pending| pending.child);
        for mut child in pending.into_iter().chain(self.child.lock().unwrap().take()) {
            #[cfg(unix)]
            {
                // SAFETY: a plain kill(2) of our own child.
                unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
                let by = std::time::Instant::now() + Duration::from_secs(3);
                while std::time::Instant::now() < by && matches!(child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.state);
    }
}

const MARIMO_START: Duration = Duration::from_secs(300);

fn marimo_wait() -> Duration {
    let test = cfg!(debug_assertions).then(|| std::env::var("ENDEAVOR_TEST_MARIMO_WAIT_SECS").ok()?.parse().ok()).flatten();
    test.map_or(JULIA_WAIT, Duration::from_secs)
}

const MARIMO_STATE: &str = "marimo.json";

fn find_uv() -> Option<PathBuf> {
    let home = crate::paths::Env::here().home;
    let path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let more = [home.join(".local/bin"), home.join(".cargo/bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")];
    path.into_iter().chain(more).map(|dir| dir.join("uv")).find(|uv| uv.is_file()).or_else(crate::uv::own)
}

const NO_UV: &str = "python_not_found::Python notebooks need uv, which isn't installed. Install it (https://docs.astral.sh/uv/getting-started/installation/), then try again.";

const UV_OFFER: (&str, &str) = (
    "python_not_found::Python notebooks need uv, which isn't installed here, and Endeavor may download its own only if the user agrees: ",
    ". Ask the user; only if they agree, call `use_machine` with `machine` \"local\" and `install: true`, then make this call again. The user can also install uv themselves: https://docs.astral.sh/uv/getting-started/installation/.",
);

const NO_UV_IN_APP: &str = "python_not_found::Python notebooks need uv, which isn't installed on this computer. Tell the user to install it (https://docs.astral.sh/uv/getting-started/installation/), then open the notebook again.";

fn uv_offer(item: &wire::Item) -> String {
    format!("{}{item}{}", UV_OFFER.0, UV_OFFER.1)
}

pub(crate) fn offer_for(error: &str, app: bool) -> std::borrow::Cow<'_, str> {
    if app && error.starts_with(UV_OFFER.0) && error.ends_with(UV_OFFER.1) {
        return NO_UV_IN_APP.into();
    }
    own_r_offer_for(error, app)
}

const STOPPED_STARTING: &str = "r_failed::R stopped while starting";

#[derive(Clone)]
struct Page {
    port: u16,
    secret: String,
}

struct JuliaReady {
    pluto: Page,
    bridge_port: u16,
}

fn julia_ready(julia_state: &Path, token: &str) -> Option<JuliaReady> {
    let julia: Value = serde_json::from_str(&std::fs::read_to_string(julia_state).ok()?).ok()?;
    let port_of = |key: &str| julia[key].as_u64().and_then(|p| u16::try_from(p).ok());
    let ready = JuliaReady {
        pluto: Page { port: port_of("pluto_port")?, secret: julia["pluto_secret"].as_str()?.to_owned() },
        bridge_port: port_of("mcp_port")?,
    };
    bridge_call(ready.bridge_port, "/call", token, "ping").is_ok_and(|status| status == 200).then_some(ready)
}

fn set_pluto_folder(julia_port: u16, token: &str, folder: &str) {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "endeavor/set_folder", "params": { "path": folder } }).to_string();
    let bearer = format!("Bearer {token}");
    let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
    if let Err(e) = http::post(julia_port, "/call", &headers, body.as_bytes()) {
        eprintln!("endeavor core: couldn't give Pluto the notebooks' folder: {e}");
    }
}

fn warm_for(served: &Arc<Served>, folder: &str, kind: &str) {
    if !served.julia.when_needed || !served.julia.idle() || kind == "r" || kind == "python" {
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

fn leave_to_plugin(served: &Arc<Served>, alive: Arc<dyn Fn(&crate::mcp::Front) -> bool + Send + Sync>) -> bool {
    use std::sync::atomic::Ordering::SeqCst;
    if !served.bridge.notebooks.shared(&*alive) {
        return false;
    }
    let mut left = served.left.lock().unwrap();
    if left.is_none() {
        eprintln!("[ Info: The app quit while the plugin works in this runtime; leaving it running for the plugin");
        *left = Some(served.bridge.notebooks.exits_when_idle.swap(true, SeqCst));
        rewrite_exits_when_idle(served, true);
    }
    let generation = served.left_generation.fetch_add(1, SeqCst) + 1;
    watch_left(served.clone(), generation, alive);
    true
}

fn keep_for_app(served: &Served) {
    use std::sync::atomic::Ordering::SeqCst;
    let mut left = served.left.lock().unwrap();
    let Some(before) = left.take() else { return };
    eprintln!("[ Info: The app is back; the runtime is the app's again");
    served.left_generation.fetch_add(1, SeqCst);
    served.bridge.notebooks.exits_when_idle.store(before, SeqCst);
    rewrite_exits_when_idle(served, before);
}

fn end_when_idle(served: &Arc<Served>) {
    use std::sync::atomic::Ordering::SeqCst;
    let mut left = served.left.lock().unwrap();
    if left.is_some() || served.bridge.notebooks.exits_when_idle.load(SeqCst) {
        return;
    }
    eprintln!("[ Info: The app is gone; this runtime goes on until its notebooks are idle, then ends");
    let generation = served.left_generation.fetch_add(1, SeqCst) + 1;
    *left = Some(served.bridge.notebooks.exits_when_idle.swap(true, SeqCst));
    rewrite_exits_when_idle(served, true);
    watch_empty(served.clone(), generation);
}

fn watch_empty(served: Arc<Served>, generation: u64) {
    use std::sync::atomic::Ordering::SeqCst;
    let check = crate::notebooks::idle_check().min(Duration::from_secs(60));
    std::thread::spawn(move || {
        let grace = left_grace();
        let mut empty_since = Instant::now();
        loop {
            std::thread::sleep(check);
            if served.left_generation.load(SeqCst) != generation {
                return;
            }
            if served.bridge.notebooks.open_count() != Some(0) {
                empty_since = Instant::now();
            } else if empty_since.elapsed() >= grace {
                eprintln!("[ Info: No notebook open for {} minutes since the app left; stopping", grace.as_secs() / 60);
                served.end_on_its_own();
                return;
            }
        }
    });
}

fn rewrite_exits_when_idle(served: &Served, value: bool) {
    let path = served.state_dir.join("runtime.json");
    if let Some(mut state) = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok())
        && state["pid"].as_u64() == Some(u64::from(std::process::id()))
    {
        state["exits_when_idle"] = value.into();
        if let Err(e) = write_private(&path, state.to_string().as_bytes()) {
            eprintln!("[ Warning: {e}");
        }
    }
}

const LEFT_GRACE: Duration = Duration::from_secs(10 * 60);

const LEFT_IDLE_HOURS: f64 = 48.0;

fn left_grace() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("ENDEAVOR_TEST_LEFT_GRACE_SECS").ok().and_then(|s| s.parse().ok()) {
        return Duration::from_secs_f64(secs);
    }
    LEFT_GRACE
}

struct LeftWatch {
    grace: Duration,
    out_since: Instant,
    empty_since: Instant,
}

impl LeftWatch {
    fn new(grace: Duration, now: Instant) -> Self {
        LeftWatch { grace, out_since: now, empty_since: now }
    }

    fn tick(&mut self, now: Instant, shared: bool, open: Option<usize>, hours: f64) -> Option<String> {
        if shared {
            self.out_since = now;
        } else if now.duration_since(self.out_since) >= self.grace {
            return Some(format!("The plugin has been out of this runtime for {} minutes", self.grace.as_secs() / 60));
        }
        let hours = if hours <= 0.0 { LEFT_IDLE_HOURS } else { hours };
        if open != Some(0) {
            self.empty_since = now;
        } else if now.duration_since(self.empty_since).as_secs_f64() >= hours * 3600.0 {
            return Some(format!("No notebook open for {}", crate::notebooks::idle_span(hours)));
        }
        None
    }
}

fn watch_left(served: Arc<Served>, generation: u64, alive: Arc<dyn Fn(&crate::mcp::Front) -> bool + Send + Sync>) {
    use std::sync::atomic::Ordering::SeqCst;
    let check = crate::notebooks::idle_check().min(Duration::from_secs(60));
    std::thread::spawn(move || {
        let notebooks = &served.bridge.notebooks;
        let mut watch = LeftWatch::new(left_grace(), Instant::now());
        loop {
            std::thread::sleep(check);
            if served.left_generation.load(SeqCst) != generation {
                return;
            }
            if let Some(why) = watch.tick(Instant::now(), notebooks.shared(&*alive), notebooks.open_count(), notebooks.idle_limit_hours()) {
                if served.left_generation.load(SeqCst) != generation {
                    return;
                }
                eprintln!("[ Info: {why}; stopping");
                served.end_on_its_own();
                return;
            }
        }
    });
}

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
                eprintln!("[ Info: No notebook open for {}; stopping", crate::notebooks::idle_span(hours));
                served.end_on_its_own();
                return;
            }
        }
    });
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    owner_only(OpenOptions::new().write(true).create_new(true))
        .open(&tmp)
        .and_then(|mut f| f.write_all(bytes))
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))
}

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

#[cfg(unix)]
fn watch_lifeline(fd: usize, _: &Path) {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    let fd = fd as i32;
    // SAFETY: the helper passed this end to the core alone, and nothing else here owns it.
    let mut lifeline = unsafe { std::fs::File::from_raw_fd(fd) };
    // SAFETY: plain syscall. Julia and R don't take it along.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    std::thread::spawn(move || {
        let mut said = [0u8; 1];
        if matches!(lifeline.read(&mut said), Ok(1)) {
            return;
        }
        eprintln!("endeavor core: the connection that started Endeavor ended while it started; stopping");
        // SAFETY: plain syscalls on the core's own process group (setsid made it one).
        unsafe { libc::kill(0, libc::SIGTERM) };
        std::thread::sleep(Duration::from_secs(10));
        unsafe { libc::kill(0, libc::SIGKILL) };
    });
}

#[cfg(windows)]
fn watch_lifeline(handle: usize, state_dir: &Path) {
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    // SAFETY: the helper made this handle inheritable for the core alone, and nothing else here owns it.
    // Julia and R don't take it along.
    let mut lifeline = unsafe {
        SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, 0);
        std::fs::File::from_raw_handle(handle as _)
    };
    let state_dir = state_dir.to_owned();
    std::thread::spawn(move || {
        let mut said = [0u8; 1];
        if matches!(lifeline.read(&mut said), Ok(1)) {
            return;
        }
        eprintln!("endeavor core: the connection that started Endeavor ended while it started; stopping");
        remove_state(&state_dir, std::process::id() as i32, None);
        std::process::exit(1);
    });
}

#[cfg(unix)]
fn pass_on_stop_signals(set: libc::sigset_t, served: Arc<Served>) {
    std::thread::spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` holds only signals blocked in every thread.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 {
                let julia = &served.julia;
                julia.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
                served.bridge.notebooks.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
                let first = julia.signalled.compare_exchange(0, signal, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst).is_ok();
                match julia.pid.load(std::sync::atomic::Ordering::SeqCst) {
                    pid if pid > 0 && !first => {
                        // SAFETY: plain syscall.
                        unsafe { libc::kill(pid, signal) };
                    }
                    pid if pid > 0 && served.bridge.julia.port.get().is_some_and(|&port| bridge_call(port, "/call", &julia.token, "endeavor/shutdown").is_ok()) => {
                        signal_unless_julia_ends(served.clone(), pid, signal);
                    }
                    pid if pid > 0 => {
                        // SAFETY: plain syscall.
                        unsafe { libc::kill(pid, signal) };
                        unless_julia_ends(served.clone(), pid);
                    }
                    _ => {
                        let _ = served.stops.send(Stop::Signal(signal));
                    }
                }
            }
        }
    });
}

#[cfg(unix)]
fn watch_for_core_death(pid: i32, mask: Option<libc::sigset_t>) -> Option<(std::process::Child, std::process::ChildStdin)> {
    const WATCH: &str = r#"julia=$1 grace=$2 term=$3
started() {
  if [ -r "/proc/$julia/stat" ]; then s=$(cat "/proc/$julia/stat" 2>/dev/null) || return 1; s=${s##*) }; set -- $s; echo "${20}"
  else ps -o lstart= -p "$julia" 2>/dev/null; fi
}
was=$(started)
[ -n "$was" ] || exit 0
read -r _ && exit 0
[ "$term" = 1 ] && [ "$(started)" = "$was" ] && kill -TERM "$julia" 2>/dev/null
sleep "$grace"
[ "$(started)" = "$was" ] && kill -KILL "$julia" 2>/dev/null
exit 0"#;
    let term = if cfg!(target_os = "linux") { "0" } else { "1" };
    let mut command = Command::new("sh");
    command.args(["-c", WATCH, "sh", &pid.to_string(), &format!("{:.1}", stop_grace().as_secs_f64()), term]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(mask) = mask {
        // SAFETY: only an async-signal-safe call between fork and exec.
        unsafe {
            command.pre_exec(move || {
                libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                Ok(())
            });
        }
    }
    let mut watch = command.spawn().map_err(|e| eprintln!("[ Warning: Couldn't watch for the core's end, to stop Julia with it: {e}")).ok()?;
    let ended = watch.stdin.take()?;
    Some((watch, ended))
}

#[cfg(unix)]
fn signal_unless_julia_ends(served: Arc<Served>, pid: i32, signal: i32) {
    std::thread::spawn(move || {
        std::thread::sleep(stop_grace());
        if served.julia.pid.load(std::sync::atomic::Ordering::SeqCst) != pid {
            return;
        }
        eprintln!("[ Warning: Julia didn't shut down; passing the signal on");
        // SAFETY: plain syscall; the pid is Julia's until the wait thread stores 0 (`kill_unless_shut_down`).
        unsafe { libc::kill(pid, signal) };
        kill_unless_shut_down(&served, pid);
    });
}

fn stop_grace() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("ENDEAVOR_TEST_STOP_GRACE_SECS").ok().and_then(|s| s.parse().ok()) {
        return std::time::Duration::from_secs_f64(secs);
    }
    std::time::Duration::from_secs(10)
}

#[cfg(unix)]
fn unless_julia_ends(served: Arc<Served>, pid: i32) {
    std::thread::spawn(move || {
        let runs = || served.julia.pid.load(std::sync::atomic::Ordering::SeqCst) == pid;
        std::thread::sleep(stop_grace());
        if !runs() {
            return;
        }
        eprintln!("[ Warning: Julia didn't stop on the signal; shutting it down");
        served.shutdown();
        kill_unless_shut_down(&served, pid);
    });
}

fn kill_unless_shut_down(served: &Served, pid: i32) {
    std::thread::sleep(stop_grace());
    if served.julia.pid.load(std::sync::atomic::Ordering::SeqCst) == pid {
        eprintln!("[ Warning: Julia didn't shut down; killing it");
        #[cfg(unix)]
        // SAFETY: plain syscall. The pid is Julia's until the wait thread reaps it and stores 0; a reuse in
        // that brief window is the only way this could hit another process.
        unsafe {
            libc::kill(pid, libc::SIGKILL)
        };
        #[cfg(windows)]
        if let Some(process) = crate::winproc::Process::open(pid, Some(served.julia.started.load(std::sync::atomic::Ordering::SeqCst)).filter(|&t| t != 0)) {
            process.terminate();
        }
    }
}

struct Served {
    bridge: Bridge,
    pluto: OnceLock<Page>,
    julia: JuliaStarter,
    app_folder: std::sync::Mutex<Option<String>>,
    stops: std::sync::mpsc::Sender<Stop>,
    ember: std::sync::Mutex<Option<Page>>,
    marimo: std::sync::Mutex<Option<Page>>,
    allow_r_install: Arc<std::sync::atomic::AtomicBool>,
    allow_uv_install: Arc<std::sync::atomic::AtomicBool>,
    r: OnceLock<Arc<RStarter>>,
    page_token: String,
    cookie: String,
    not_let_in: String,
    state_dir: PathBuf,
    left: Mutex<Option<bool>>,
    left_generation: std::sync::atomic::AtomicU64,
}

impl Served {
    fn shutdown(&self) {
        let julia = &self.julia;
        julia.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        self.bridge.notebooks.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        match self.bridge.julia.port.get() {
            Some(&port) if julia.pid.load(std::sync::atomic::Ordering::SeqCst) > 0 && bridge_call(port, "/call", &self.julia.token, "endeavor/shutdown").is_ok() => {}
            _ => {
                let _ = self.stops.send(Stop::Shutdown);
            }
        }
    }

    fn end_on_its_own(self: &Arc<Self>) {
        let pid = self.julia.pid.load(std::sync::atomic::Ordering::SeqCst);
        self.shutdown();
        if pid > 0 {
            let served = self.clone();
            std::thread::spawn(move || kill_unless_shut_down(&served, pid));
        }
    }

    fn julia_call(self: &Arc<Self>, method: &str, body: &[u8]) -> String {
        let message: Value = serde_json::from_slice(body).unwrap_or_default();
        let mut result = json!({});
        match method {
            "endeavor/julia_status" => result = self.julia.status(),
            "endeavor/r_status" => result = self.r.get().map_or_else(|| json!({ "state": "not_started" }), |r| r.status()),
            "endeavor/start_julia" => {
                self.julia.start_now(self);
                result = self.julia.status();
            }
            "endeavor/set_folder" => {
                let folder = message["params"]["path"].as_str().unwrap_or_default().to_owned();
                *self.app_folder.lock().unwrap() = Some(folder.clone());
                if let Some(&port) = self.bridge.julia.port.get() {
                    set_pluto_folder(port, &self.julia.token, &folder);
                }
            }
            "endeavor/shutdown" => {
                eprintln!("[ Info: Shutting down at the app's request");
                let served = self.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(200));
                    served.shutdown();
                });
            }
            "endeavor/allow_julia_install" => self.julia.install.store(true, std::sync::atomic::Ordering::SeqCst),
            "endeavor/use_julia" => match message["params"]["path"].as_str().filter(|path| !path.is_empty()) {
                Some(path) => *self.julia.source.lock().unwrap() = crate::julia::Source::Path(path.to_owned()),
                None => result = json!({ "error": "use_julia needs a path" }),
            },
            "endeavor/allow_r_install" => self.allow_r_install.store(true, std::sync::atomic::Ordering::SeqCst),
            "endeavor/allow_uv_install" => self.allow_uv_install.store(true, std::sync::atomic::Ordering::SeqCst),
            "endeavor/leave_to_plugin" => result = json!({ "left": leave_to_plugin(self, Arc::new(|front: &crate::mcp::Front| front.alive())) }),
            "endeavor/keep_for_app" => keep_for_app(self),
            "endeavor/end_when_idle" => end_when_idle(self),
            _ => {}
        }
        json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }).to_string()
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    Mcp,
    Events,
    Call,
    NotFound,
    Ember,
    Marimo,
    Pluto,
}

impl Route {
    fn is_page(self) -> bool {
        matches!(self, Route::Pluto | Route::Ember | Route::Marimo)
    }

    fn of(path: &str) -> Route {
        match path {
            "/mcp" => Route::Mcp,
            "/endeavor/events" => Route::Events,
            "/endeavor/call" => Route::Call,
            _ if path.starts_with("/endeavor/") => Route::NotFound,
            "/ember" => Route::Ember,
            _ if path.starts_with("/ember/") => Route::Ember,
            "/marimo" => Route::Marimo,
            _ if path.starts_with("/marimo/") => Route::Marimo,
            _ => Route::Pluto,
        }
    }
}

fn serve_client(client: TcpStream, served: &Arc<Served>) -> io::Result<()> {
    let _ = client.set_nodelay(true);
    let mut client = wire::Shared::new(client);
    let mut reader = BufReader::new(client.clone());
    let bridge = &served.bridge;
    while let Some(mut request) = Head::read(&mut reader)? {
        let route = Route::of(request.path());
        let keep_alive = match access(&request, route, &bridge.token, &served.page_token, &served.cookie) {
            Access::Granted => None,
            Access::SetCookie { location } => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Strict", served.cookie, served.page_token);
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
                if request.target() == "/" && served.julia.idle() {
                    http::respond(&mut client, "200 OK", Some("text/html; charset=utf-8"), JULIA_NOT_STARTED_PAGE.as_bytes(), request.keeps_alive())?;
                } else {
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
                        request.set_target(&with_secret(&target, &ember.secret));
                        request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Authorization") && !name.eq_ignore_ascii_case("Cookie"));
                        forward(request, None, &mut reader, ember.port, "R", &|response| {
                            response.headers.retain(|(name, value)| !(name.eq_ignore_ascii_case("Set-Cookie") && value.trim_start().starts_with("ember_secret")));
                        })?
                    }
                }
            }
            (Route::Marimo, _) => {
                let marimo = served.marimo.lock().unwrap().clone();
                let target = marimo_target(request.target(), |file| served.bridge.notebooks.open_and_allowed(file));
                match (target, marimo) {
                    (MarimoTarget::Moved(location), _) => {
                        http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                        let close = if request.keeps_alive() { "" } else { "Connection: close\r\n" };
                        write!(client, "HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\n{close}\r\n")?;
                        request.keeps_alive()
                    }
                    (MarimoTarget::Refused, _) => {
                        http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                        http::respond(&mut client, "404 Not Found", None, b"", request.keeps_alive())?;
                        request.keeps_alive()
                    }
                    (MarimoTarget::NotAllowed, _) => {
                        http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                        http::respond(&mut client, "403 Forbidden", Some("text/html; charset=utf-8"), MARIMO_NOT_ALLOWED_PAGE.as_bytes(), request.keeps_alive())?;
                        request.keeps_alive()
                    }
                    (MarimoTarget::Forward, None) => return refuse(&mut client, "503 Service Unavailable", "Python notebooks aren't running"),
                    (MarimoTarget::Forward, Some(marimo)) => {
                        request.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Authorization") && !name.eq_ignore_ascii_case("Cookie"));
                        request.headers.push(("Authorization".into(), format!("Bearer {}", marimo.secret)));
                        forward(request, None, &mut reader, marimo.port, "Python", &|response| {
                            response.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("Set-Cookie"));
                        })?
                    }
                }
            }
            (Route::NotFound, _) => {
                http::copy_body(&mut reader, &mut io::sink(), &mut request.request_body()?)?;
                http::respond(&mut client, "404 Not Found", None, b"", request.keeps_alive())?;
                request.keeps_alive()
            }
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

const JULIA_NOT_STARTED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Endeavor</title>
<body style=\"font-family: system-ui, sans-serif; max-width: 40em; margin: 4em auto; line-height: 1.5\">
<h1>Julia isn't running</h1>
<p>Julia starts when a Julia notebook is opened, or when you start it here. R notebooks don't need it: each opens from its own link.</p>
<p><a href=\"/?start-julia\">Start Julia and open Pluto</a></p>
</body>
";

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

pub fn serve_unreachable(client: TcpStream, token: &str, why: &str, restart: Option<(&crate::guard::AskRestart, &str)>) -> io::Result<()> {
    let mut client = wire::Shared::new(client);
    let mut reader = BufReader::new(client.clone());
    let Some(request) = Head::read(&mut reader)? else { return Ok(()) };
    if Route::of(request.path()) != Route::Mcp {
        return Ok(());
    }
    if let Access::Refused(status, error) = access(&request, Route::Mcp, token, "", "") {
        let body = json!({ "error": error }).to_string();
        return http::respond(&mut client, status, Some("application/json"), body.as_bytes(), false);
    }
    if request.method() == "POST" {
        let session = request.header("X-Endeavor-Session").unwrap_or_default().to_owned();
        crate::mcp::post(&request, &mut reader, &mut client, false, false, |message, gone| {
            let Some(restart) = restart else { return crate::mcp::answer_unreachable(message, &request, why) };
            if let Some(reply) = crate::guard::restart_call(message, &session, restart, gone) {
                return Some(crate::mcp::to_json(&reply));
            }
            let reply = crate::mcp::answer_unreachable(message, &request, why)?;
            let listed = (message["method"] == "tools/list").then(|| serde_json::from_str::<Value>(&reply).ok()).flatten();
            match listed.as_ref().and_then(|r| crate::guard::with_restart_tool(&r["result"]).map(|result| (r, result))) {
                Some((r, result)) => Some(crate::mcp::to_json(&json!({ "jsonrpc": "2.0", "id": r["id"], "result": result }))),
                None => Some(reply),
            }
        })?;
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Access {
    Granted,
    SetCookie { location: String },
    Refused(&'static str, &'static str),
}

fn access(request: &Head, route: Route, token: &str, page_token: &str, cookie: &str) -> Access {
    let host = request.header("Host").unwrap_or_default();
    if !loopback_host(host) {
        return Access::Refused("403 Forbidden", "host_not_loopback");
    }
    let origin = request.header("Origin");
    let authorization = request.header("Authorization").unwrap_or_default();
    if !route.is_page() {
        let bearer = same(authorization, &format!("Bearer {token}"));
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
    if same(authorization, &format!("Bearer {page_token}")) || has_cookie(request, cookie, page_token) {
        return Access::Granted;
    }
    match without_token(request.target()) {
        (Some(given), location) if same(given, page_token) => Access::SetCookie { location },
        _ => Access::Refused("401 Unauthorized", "unauthorized"),
    }
}

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

pub(crate) fn open_command(dir: &Path) -> String {
    let quote = |text: &str| if text.is_empty() || text.contains([' ', '"', '\'', '$', '&']) { format!("\"{text}\"") } else { text.to_owned() };
    let program = std::env::current_exe().map_or_else(|_| "endeavor".to_owned(), |exe| exe.display().to_string());
    let mut words = vec![quote(&program)];
    words.extend(crate::HELPER_ARGS.get().copied().unwrap_or_default().iter().map(|arg| arg.to_string()));
    words.extend(["open".to_owned(), "--state-dir".to_owned(), quote(&dir.display().to_string())]);
    let line = words.join(" ");
    if cfg!(windows) { format!("& {line}") } else { line }
}

fn opens_a_page(request: &Head) -> bool {
    request.method() == "GET"
        && match request.header("Sec-Fetch-Dest") {
            Some(dest) => dest == "document",
            None => request.header("Accept").is_some_and(|accept| accept.contains("text/html")),
        }
}

fn cookie_name(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let id: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("endeavor-{id}")
}

fn has_cookie(request: &Head, name: &str, token: &str) -> bool {
    request
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("Cookie"))
        .flat_map(|(_, value)| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(n, value)| n == name && same(value, token))
}

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

fn ember_target(target: &str) -> Result<String, String> {
    let rest = target.strip_prefix("/ember").unwrap_or(target);
    if rest.is_empty() || rest.starts_with('?') {
        return Err(format!("/ember/{rest}"));
    }
    Ok(rest.to_owned())
}

#[derive(Debug, PartialEq)]
enum MarimoTarget {
    Forward,
    Moved(String),
    Refused,
    NotAllowed,
}

fn marimo_target(target: &str, allowed: impl Fn(&str) -> bool) -> MarimoTarget {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some(path) = percent_decoded(path, false) else { return MarimoTarget::Refused };
    let rest = path.strip_prefix("/marimo").unwrap_or(&path);
    if rest.is_empty() {
        return MarimoTarget::Moved(if query.is_empty() { "/marimo/".into() } else { format!("/marimo/?{query}") });
    }
    if rest.contains(['%', '\\']) || rest.contains("//") || rest.split('/').any(|segment| segment == ".." || segment == ".") {
        return MarimoTarget::Refused;
    }
    const REFUSED: [&str; 3] = ["/terminal", "/api/files", "/api/home"];
    if REFUSED.iter().any(|refused| rest == *refused || rest.starts_with(&format!("{refused}/"))) {
        return MarimoTarget::Refused;
    }
    let mut files = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match (percent_decoded(key, true), percent_decoded(value, true)) {
            (Some(key), Some(value)) => if key == "file" { files.push(value) },
            _ => return MarimoTarget::NotAllowed,
        }
    }
    match files.as_slice() {
        [file] if allowed(file) => MarimoTarget::Forward,
        [_, ..] => MarimoTarget::NotAllowed,
        [] if ["/ws", "/ws_sync", "/sse"].iter().any(|socket| rest == *socket || rest.starts_with(&format!("{socket}/"))) => MarimoTarget::NotAllowed,
        [] => MarimoTarget::Forward,
    }
}

pub(crate) fn percent_decoded(text: &str, query: bool) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = |b: Option<&u8>| b.and_then(|b| (*b as char).to_digit(16));
                out.push((hex(bytes.get(i + 1))? * 16 + hex(bytes.get(i + 2))?) as u8);
                i += 3;
            }
            b'+' if query => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

const MARIMO_NOT_ALLOWED_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Endeavor</title>
<body style=\"font-family: system-ui, sans-serif; max-width: 40em; margin: 4em auto; line-height: 1.5\">
<h1>This notebook isn't allowed to run yet</h1>
<p>Opening a Python notebook here installs the packages it names, which runs code. It opens once it is allowed to run: ask your agent to run it, or to open it again once you've allowed it.</p>
</body>
";

fn with_secret(target: &str, secret: &str) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut pairs: Vec<&str> = query.split('&').filter(|pair| !pair.is_empty() && !pair.starts_with("secret=")).collect();
    let ours = format!("secret={secret}");
    pairs.push(&ours);
    format!("{path}?{}", pairs.join("&"))
}

pub(crate) fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(_) => host.find(']').map_or("", |end| &host[..=end]),
        None => host.split(':').next().unwrap_or_default(),
    };
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

pub(crate) fn same(given: &str, expected: &str) -> bool {
    given.len() == expected.len() && given.bytes().zip(expected.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
}

fn forward(request: Head, body: Option<&[u8]>, client: &mut BufReader<wire::Shared>, port: u16, language: &str, edit: &dyn Fn(&mut Head)) -> io::Result<bool> {
    let upstream = match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)) {
        Ok(upstream) => upstream,
        Err(_) => {
            refuse(client.get_mut(), "502 Bad Gateway", &format!("{language} isn't answering"))?;
            return Ok(false);
        }
    };
    let _ = upstream.set_nodelay(true);
    let upstream = wire::Shared::new(upstream);
    let mut to_upstream = upstream.clone();
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

fn refuse(client: &mut impl Write, status: &str, why: &str) -> io::Result<()> {
    let body = json!({ "error": why }).to_string();
    write!(client, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r_reads_the_users_home_settings_not_a_projects() {
        let empty = std::path::Path::new("/state/empty.Rprofile");
        let mut command = std::process::Command::new("Rscript");
        user_r_settings(&mut command, Some(std::path::Path::new("/home/ada")), empty);
        let env: std::collections::HashMap<_, _> = command.get_envs().map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))).collect();
        if std::env::var_os("R_ENVIRON_USER").is_none() && !cfg!(windows) {
            assert_eq!(env["R_ENVIRON_USER"].as_deref(), Some(std::path::Path::new("/home/ada").join(".Renviron").to_string_lossy().as_ref()));
        }
        if cfg!(windows) {
            assert!(!env.contains_key("R_ENVIRON_USER"), "Windows: R's own rules for .Renviron");
        }
        assert_eq!(env["R_PROFILE_USER"].as_deref(), Some(empty.to_string_lossy().as_ref()));
        if std::env::var_os("R_PROFILE_USER").is_none() {
            assert_eq!(env["ENDEAVOR_R_PROFILE"].as_deref(), Some(""));
        }
        let mut none = std::process::Command::new("Rscript");
        user_r_settings(&mut none, None, empty);
        let names: Vec<_> = none.get_envs().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        assert_eq!(names.len(), 2, "no home: no R_ENVIRON_USER, {names:?}");
    }

    #[test]
    fn a_call_made_while_an_install_runs_waits_for_it() {
        let state = Arc::new(std::sync::Mutex::new(Install::Running));
        let began = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(600));
                *state.lock().unwrap() = Install::Done(Ok(false));
            });
            assert!(matches!(*wait_for_install(&state, Duration::from_secs(30)), Install::Done(Ok(false))));
        });
        assert!(began.elapsed() >= Duration::from_millis(600), "waited for it");
        let began = std::time::Instant::now();
        *state.lock().unwrap() = Install::Running;
        assert!(matches!(*wait_for_install(&state, Duration::from_secs(1)), Install::Running));
        assert!(began.elapsed() >= Duration::from_secs(1), "waited as long as it may");
        *state.lock().unwrap() = Install::Idle;
        let began = std::time::Instant::now();
        assert!(matches!(*wait_for_install(&state, Duration::from_secs(30)), Install::Idle) && began.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_julia_start_says_how_long_so_far_and_to_call_again_now() {
        let said = starting_message("Julia 1.12.6 is starting and loading Pluto", Duration::from_secs(250), Duration::from_secs(30));
        for part in ["julia_starting::Julia 1.12.6 is starting and loading Pluto (about 4 minutes so far).", "10 minutes", "much faster", "right away", "waits up to 30 seconds", "don't pause"] {
            assert!(said.contains(part), "{part}: {said}");
        }
        assert!(!said.contains("in a minute"), "{said}");
        assert!(starting_message("Julia is starting", Duration::from_secs(40), Duration::from_secs(5)).contains("(40 seconds so far)") && starting_message("Julia is starting", Duration::from_secs(40), Duration::from_secs(5)).contains("waits up to 5 seconds"));
        assert!(!starting_message("Julia is starting", Duration::from_secs(40), Duration::ZERO).contains("call again"));
    }

    #[test]
    #[cfg(unix)]
    fn why_r_ended_says_how_its_process_ended_and_its_last_output() {
        use std::os::unix::process::ExitStatusExt;
        let said = |lines: &[&str]| lines.iter().map(|line| line.to_string()).collect::<Vec<_>>();
        assert_eq!(why_stopped("R", None, &[]), "it stopped answering");
        assert_eq!(why_stopped("R", Some(ExitStatus::from_raw(9)), &[]), "signal: 9 (SIGKILL)");
        let error = said(&["Listening on 127.0.0.1", "Error in loadNamespace(x) : there is no package called 'ember'", "Calls: loadNamespace -> withRestarts", "Execution halted"]);
        assert_eq!(why_stopped("R", Some(ExitStatus::from_raw(1 << 8)), &error), "exit status: 1; R's last output: Error in loadNamespace(x) : there is no package called 'ember' Calls: loadNamespace -> withRestarts Execution halted");
        let long = why_stopped("R", None, &said(&[&"x".repeat(1000)]));
        assert!(long.ends_with(&format!("…{}", "x".repeat(300))), "the end is kept: {long}");
    }

    #[test]
    #[cfg(unix)]
    fn a_processs_stderr_is_kept_to_its_last_lines_once_it_ends() {
        let mut child = Command::new("sh")
            .args(["-c", "i=0; while [ $i -lt 30 ]; do echo \"line $i\" >&2; i=$((i+1)); done; printf '\\n  \\377 not utf-8\\nError: boom\\n' >&2; exit 3"])
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let words = pass_on_stderr(child.stderr.take().unwrap());
        let status = child.wait().unwrap();
        let said = words.after_end(Duration::from_secs(10));
        assert_eq!(said.len(), 20, "the last 20 lines: {said:?}");
        assert_eq!(said[said.len() - 2..], ["\u{FFFD} not utf-8", "Error: boom"], "blank lines are left out, and bytes that aren't UTF-8 don't stop it");
        assert_eq!(why_stopped("R", Some(status), &said), "exit status: 3; R's last output: line 29 \u{FFFD} not utf-8 Error: boom");
        assert!(why_stopped("Python", Some(status), &said).contains("; Python's last output: "), "Python's adapter is named as itself (#363)");
    }

    #[test]
    fn own_r_is_offered_through_use_machine_or_the_apps_settings() {
        let item = wire::Item { kind: wire::KIND_RUNTIME.into(), name: "R 4.6.1".into(), size_mb: Some(165), place: Some("/x/R-4.6.1".into()) };
        let standalone = own_r_offer(&item, false);
        assert!(standalone.starts_with("r_not_found::") && standalone.contains("`use_machine`") && standalone.contains("R 4.6.1 (about 165 MB"), "{standalone}");
        let app = own_r_offer(&item, true);
        assert!(app.starts_with("r_not_found::") && app.contains("Install R in the notebook pane") && app.contains("Settings under Notebooks, then R") && !app.contains("use_machine"), "{app}");
    }

    #[test]
    fn marimo_runs_on_the_python_its_requirements_were_compiled_for_not_whatever_python_uv_finds_first() {
        let requirements = include_str!("../../../runtime/marimo/requirements.txt");
        let compiled_for = requirements.lines().next().and_then(|header| header.split("--python-version ").nth(1)).and_then(|rest| rest.split_whitespace().next());
        assert_eq!(compiled_for, Some(MARIMO_PYTHON), "a Mac with only its own Python 3.9 can't install requirements compiled for a newer one");
    }

    #[test]
    fn the_app_has_no_way_to_say_yes_to_endeavors_own_uv_so_its_agent_is_told_to_have_the_user_install_uv() {
        let item = wire::Item { kind: wire::KIND_RUNTIME.into(), name: "uv 0.11.32".into(), size_mb: Some(22), place: Some("/x/uv-0.11.32".into()) };
        let offer = uv_offer(&item);
        assert_eq!(offer_for(&offer, false), offer);
        let app = offer_for(&offer, true);
        assert!(app.starts_with("python_not_found::") && !app.contains("use_machine") && app.contains("docs.astral.sh/uv"), "{app}");
        assert_eq!(offer_for(NO_UV, true), NO_UV);
    }

    #[test]
    fn the_own_r_offer_is_worded_for_the_caller_not_the_runtime() {
        let item = wire::Item { kind: wire::KIND_RUNTIME.into(), name: "R 4.6.1".into(), size_mb: Some(165), place: Some("/x/R-4.6.1".into()) };
        for started_by_app in [true, false] {
            let kept = own_r_offer(&item, started_by_app);
            assert_eq!(own_r_offer_for(&kept, true), own_r_offer(&item, true));
            assert_eq!(own_r_offer_for(&kept, false), own_r_offer(&item, false));
        }
        assert_eq!(own_r_offer_for("r_not_found::Couldn't find R", false), "r_not_found::Couldn't find R");
    }

    #[test]
    fn r_status_says_each_step_and_ready_wins_over_an_update() {
        let (idle, running, done_ok, done_err) = (Install::Idle, Install::Running, Install::Done(Ok(false)), Install::Failed("r_failed::Couldn't install Ember".into(), std::time::Instant::now()));
        let own_failed = Install::Failed("r_failed::Couldn't install R: no network".into(), std::time::Instant::now());
        let now = |running_r, own, ember, ember_installed, launching, failed| r_status(&RNow { running: running_r, own, ember, ember_installed, launching, failed });
        let step = |s: &str| json!({ "state": "starting", "step": s });
        assert_eq!(now(false, &idle, &idle, false, false, None), json!({ "state": "not_started" }));
        assert_eq!(now(false, &running, &idle, false, false, None), step(&format!("Installing Endeavor's own R {}", crate::r::OWN_VERSION)));
        assert_eq!(now(false, &idle, &running, false, false, None), step("Installing Ember for R notebooks"));
        assert_eq!(now(false, &idle, &running, true, false, None), step("Updating Ember"));
        assert_eq!(now(false, &idle, &idle, true, true, None), step("Starting R"));
        assert_eq!(now(true, &idle, &idle, true, false, None), json!({ "state": "ready" }));
        assert_eq!(now(true, &idle, &running, true, false, None), json!({ "state": "ready", "updating": true }), "R runs while a newer Ember installs");
        assert_eq!(now(false, &own_failed, &idle, false, false, None), json!({ "state": "failed", "code": "r_failed", "message": "Couldn't install R: no network" }));
        assert_eq!(now(false, &idle, &done_err, false, false, None), json!({ "state": "failed", "code": "r_failed", "message": "Couldn't install Ember" }));
        assert_eq!(now(false, &idle, &idle, false, false, Some("r_not_found::Couldn't find R")), json!({ "state": "failed", "code": "r_not_found", "message": "Couldn't find R" }));
        assert_eq!(now(true, &idle, &idle, true, false, Some("r_failed::old")), json!({ "state": "ready" }), "a running R outranks an old failure");
        assert_eq!(now(false, &idle, &done_ok, true, false, None), json!({ "state": "not_started" }), "installed while no call waited, for the moment before R starts (#148)");
    }

    #[test]
    fn ember_builds_read_what_install_r_writes_and_fall_back_to_the_one_before() {
        let folder = std::env::temp_dir().join(format!("endeavor-ember-builds-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(EmberBuilds::read(&folder), EmberBuilds::default(), "nothing installed");
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
    const PAGES: &str = "p4g3s";

    fn access_to(request: &str) -> Access {
        let request = head(request);
        access(&request, Route::of(request.path()), TOKEN, PAGES, "endeavor-abc")
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
    fn the_bearer_token_opens_endeavors_paths_and_the_pages_token_only_the_pages() {
        let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
        let pages = format!("Authorization: Bearer {PAGES}\r\n");
        let cookie = format!("Cookie: other=1; endeavor-abc={PAGES}\r\n");
        let unauthorized = Access::Refused("401 Unauthorized", "unauthorized");
        for path in ["/mcp", "/endeavor/call", "/endeavor/events"] {
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{bearer}\r\n")), Access::Granted, "{path}");
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{pages}\r\n")), unauthorized, "{path}");
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")), unauthorized, "{path}");
        }
        for path in ["/edit?id=1", "/", "/notebookfile?id=1", "/ember/edit?id=1"] {
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{pages}\r\n")), Access::Granted, "{path}");
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{bearer}\r\n")), unauthorized, "the bearer token doesn't open {path}");
            assert_eq!(access_to(&format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")), unauthorized, "{path}");
        }
        assert_eq!(access_to(&format!("GET /edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Granted);
        assert_eq!(access_to(&format!("GET /edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\nCookie: endeavor-abc={TOKEN}\r\n\r\n")), unauthorized, "a cookie holding the bearer token");
        for path in ["/mcp", "/endeavor/call", "/endeavor/answer_run"] {
            assert_eq!(access_to(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Refused("401 Unauthorized", "unauthorized"), "{path}");
            assert_eq!(
                access_to(&format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:9\r\nOrigin: http://127.0.0.1:9\r\n{cookie}{bearer}\r\n")),
                Access::Refused("403 Forbidden", "browser_origin_refused"),
                "a page's request, even with the token"
            );
        }
        let wrong = |c: &str| access_to(&format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:9\r\nCookie: {c}\r\n\r\n"));
        assert_eq!(wrong(&format!("endeavor-def={PAGES}")), Access::Refused("401 Unauthorized", "unauthorized"), "another runtime's cookie");
        assert_eq!(wrong("endeavor-abc=t0k3m"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(wrong(&format!("secret={PAGES}")), Access::Refused("401 Unauthorized", "unauthorized"));
    }

    #[test]
    fn the_cookie_works_only_from_this_page() {
        let cookie = format!("Cookie: endeavor-abc={PAGES}\r\n");
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
        assert_eq!(visit(&format!("/?token={PAGES}")), Access::SetCookie { location: "/".into() });
        assert_eq!(visit(&format!("/edit?id=1&token={PAGES}&x=2")), Access::SetCookie { location: "/edit?id=1&x=2".into() });
        assert_eq!(visit("/?token=nope"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(visit(&format!("/?token={TOKEN}")), Access::Refused("401 Unauthorized", "unauthorized"), "the bearer token doesn't let a browser in");
        assert_eq!(
            access_to(&format!("GET /mcp?token={PAGES} HTTP/1.1\r\nHost: localhost:9\r\n\r\n")),
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
    }

    #[test]
    fn marimos_page_is_under_its_prefix_without_its_terminal_or_file_listings() {
        let open = |file: &str| file == "/w/a b.py";
        let target = |t: &str| marimo_target(t, open);
        assert_eq!(Route::of("/marimo"), Route::Marimo);
        assert_eq!(Route::of("/marimo/ws"), Route::Marimo);
        assert_eq!(Route::of("/marimoz"), Route::Pluto);
        assert_eq!(target("/marimo"), MarimoTarget::Moved("/marimo/".into()));
        assert_eq!(target("/marimo?file=x"), MarimoTarget::Moved("/marimo/?file=x".into()));
        assert_eq!(target("/marimo/assets/index.js"), MarimoTarget::Forward);
        assert_eq!(target("/marimo/api/document/transaction"), MarimoTarget::Forward);
        for refused in ["/marimo/terminal", "/marimo/terminal/ws", "/marimo/api/files/list_files", "/marimo/api/home/workspace_files", "/marimo/api/home", "/marimo/api/files"] {
            assert_eq!(target(refused), MarimoTarget::Refused, "{refused}");
        }
        for refused in ["/marimo/%74erminal/ws", "/marimo/api/%66iles/list_files", "/marimo/api%2Ffiles/list_files", "/marimo/api/%2566iles/x", "/marimo//api/files/x", "/marimo/x/../api/files/x", "/marimo/api/home%", "/marimo/%ff"] {
            assert_eq!(target(refused), MarimoTarget::Refused, "{refused}");
        }
        assert_eq!(target("/marimo/?file=/w/a%20b.py"), MarimoTarget::Forward);
        assert_eq!(target("/marimo/ws?session_id=s&file=/w/a+b.py"), MarimoTarget::Forward);
        assert_eq!(target("/marimo/?file=/w/other.py"), MarimoTarget::NotAllowed);
        assert_eq!(target("/marimo/ws?file=/etc/x.py&session_id=s"), MarimoTarget::NotAllowed);
        assert_eq!(target("/marimo/ws?session_id=s"), MarimoTarget::NotAllowed);
        assert_eq!(target("/marimo/sse?session_id=s"), MarimoTarget::NotAllowed);
        assert_eq!(target("/marimo/ws_sync?session_id=s"), MarimoTarget::NotAllowed);
        assert_eq!(target("/marimo/?file=%zz"), MarimoTarget::NotAllowed);
        for page in ["/marimo/", "/marimo/ws"] {
            for query in ["file=/w/a%20b.py&file=/w/other.py", "file=/w/a%20b.py&%66ile=/w/other.py", "file=/w/a%20b.py&%66%69%6c%65=/w/other.py", "file=/w/a%20b.py&fil%zz=x", "%66ile=/w/other.py"] {
                assert_eq!(target(&format!("{page}?{query}&session_id=s")), MarimoTarget::NotAllowed, "{page}?{query}");
            }
            assert_eq!(target(&format!("{page}?%66ile=/w/a%20b.py&session_id=s")), MarimoTarget::Forward, "{page}");
        }
    }

    #[test]
    fn embers_page_takes_the_cookie_as_plutos_does() {
        let cookie = format!("Cookie: endeavor-abc={PAGES}\r\n");
        assert_eq!(access_to(&format!("GET /ember/edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n{cookie}\r\n")), Access::Granted);
        assert_eq!(access_to("GET /ember/edit?id=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n"), Access::Refused("401 Unauthorized", "unauthorized"));
        assert_eq!(
            access_to(&format!("GET /ember/edit?id=1&token={PAGES} HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")),
            Access::SetCookie { location: "/ember/edit?id=1".into() }
        );
        assert_eq!(
            access_to(&format!("GET /ember/channels HTTP/1.1\r\nHost: 127.0.0.1:9\r\nOrigin: http://127.0.0.1:10\r\n{cookie}\r\n")),
            Access::Refused("403 Forbidden", "browser_origin_refused")
        );
    }

    fn stand_in() -> Arc<Served> {
        let (stops, _) = std::sync::mpsc::channel();
        let argv = ["--state-dir", "/nonexistent", "--julia", "/nonexistent/julia", "--runtime", "/nonexistent/runtime", "--depot", "/nonexistent/depot"].map(String::from);
        let julia = JuliaStarter::new(&parse_args(&argv).unwrap(), TOKEN, "process", stops.clone());
        Arc::new(Served { bridge: Bridge::new(TOKEN.into(), "e".into(), ""), page_token: PAGES.into(), pluto: OnceLock::new(), ember: Default::default(), cookie: cookie_name(PAGES), not_let_in: String::new(), julia, app_folder: Default::default(), stops, allow_r_install: Default::default(), allow_uv_install: Default::default(), r: OnceLock::new(), marimo: Default::default(), state_dir: PathBuf::from("/nonexistent"), left: Default::default(), left_generation: Default::default() })
    }

    #[test]
    fn the_app_quitting_stops_a_runtime_the_plugin_is_not_in() {
        let served = stand_in();
        assert!(!leave_to_plugin(&served, Arc::new(|_: &crate::mcp::Front| true)));
        assert!(!served.bridge.notebooks.exits_when_idle.load(std::sync::atomic::Ordering::SeqCst), "it isn't switched to end when idle");
        assert!(served.left.lock().unwrap().is_none(), "nor watched");
    }

    #[test]
    fn the_app_taking_a_runtime_back_restores_how_it_ends() {
        use std::sync::atomic::Ordering::SeqCst;
        let served = stand_in();
        keep_for_app(&served);
        assert!(!served.bridge.notebooks.exits_when_idle.load(SeqCst), "one that wasn't left is as it was");
        *served.left.lock().unwrap() = Some(false);
        served.bridge.notebooks.exits_when_idle.store(true, SeqCst);
        let generation = served.left_generation.load(SeqCst);
        keep_for_app(&served);
        assert!(!served.bridge.notebooks.exits_when_idle.load(SeqCst), "no front of another build can take it for the plugin's");
        assert!(served.left.lock().unwrap().is_none());
        assert_ne!(served.left_generation.load(SeqCst), generation, "its watch stops");
        *served.left.lock().unwrap() = Some(true);
        keep_for_app(&served);
        assert!(served.bridge.notebooks.exits_when_idle.load(SeqCst));
    }

    #[test]
    fn a_runtime_the_app_leaves_running_ends_when_idle_until_the_app_is_back() {
        use std::sync::atomic::Ordering::SeqCst;
        let served = stand_in();
        let generation = served.left_generation.load(SeqCst);
        end_when_idle(&served);
        assert!(served.bridge.notebooks.exits_when_idle.load(SeqCst), "it ends once no notebook has been open for the idle limit");
        assert_eq!(*served.left.lock().unwrap(), Some(false));
        let watching = served.left_generation.load(SeqCst);
        assert_ne!(watching, generation);
        end_when_idle(&served);
        assert_eq!(served.left_generation.load(SeqCst), watching, "asked again, nothing more is watched");
        keep_for_app(&served);
        assert!(!served.bridge.notebooks.exits_when_idle.load(SeqCst), "the app has it again");
        assert_ne!(served.left_generation.load(SeqCst), watching, "and the idle watch stops");
        let served = stand_in();
        served.bridge.notebooks.exits_when_idle.store(true, SeqCst);
        end_when_idle(&served);
        assert!(served.left.lock().unwrap().is_none());
    }

    #[test]
    fn a_runtime_left_to_the_plugin_ends_once_the_plugin_is_out_of_it_whatever_the_idle_limit() {
        let start = Instant::now();
        let at = |minutes: u64| start + Duration::from_secs(minutes * 60);
        let mut watch = LeftWatch::new(LEFT_GRACE, start);
        assert_eq!(watch.tick(at(5), true, Some(1), 0.0), None, "the plugin is in it");
        assert_eq!(watch.tick(at(14), false, Some(1), 0.0), None, "out for 9 minutes");
        assert_eq!(watch.tick(at(15), true, Some(1), 0.0), None, "back: a Claude Code restart that opened its notebook again");
        assert_eq!(watch.tick(at(24), false, Some(1), 0.0), None);
        assert!(watch.tick(at(25), false, Some(1), 0.0).is_some_and(|why| why.contains("10 minutes")), "out for 10 minutes, with the limit Never");
        let mut watch = LeftWatch::new(LEFT_GRACE, start);
        assert_eq!(watch.tick(at(47 * 60), true, Some(0), 0.0), None);
        assert!(watch.tick(at(48 * 60), true, Some(0), 0.0).is_some_and(|why| why.contains("48 hours")));
        let mut watch = LeftWatch::new(LEFT_GRACE, start);
        assert!(watch.tick(at(60), true, Some(0), 1.0).is_some_and(|why| why.contains("1 hour")), "the app's limit when it has one");
    }

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
        write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {PAGES}\r\nConnection: close\r\n{headers}\r\n").unwrap();
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
    fn the_bearer_token_gets_no_page_through_the_port() {
        use std::io::Read;
        let port = serving(Some(1));
        for target in ["/", "/edit?id=1", "/ember/edit?id=1"] {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(stream, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n").unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).unwrap();
            assert!(reply.starts_with("HTTP/1.1 401"), "{target}: {reply}");
        }
    }

    fn mcp_post(port: u16, headers: &str, body: &Value) -> Value {
        use std::io::Read;
        let body = crate::mcp::to_json(body);
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nX-Endeavor-Session: s1\r\n{headers}Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let start = reply.find("{\"").unwrap_or_else(|| panic!("{reply}"));
        let end = reply.rfind('}').unwrap();
        serde_json::from_str(&reply[start..=end]).unwrap_or_else(|_| panic!("{reply}"))
    }

    #[test]
    fn a_client_straight_on_the_port_cant_approve_a_download_itself() {
        let port = serving(None);
        let approve = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "approve_install", "arguments": { "notebook_id": "1b2c3d4e-0000-4000-8000-000000000001" } } });
        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let text = |reply: &Value| reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();
        let description = |reply: &Value| reply["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "approve_install").unwrap()["description"].as_str().unwrap().to_owned();
        for straight in ["", "X-Endeavor-Client: plugin\r\n", "X-Endeavor-Client: plugin\r\nX-Endeavor-Consent: yes\r\n"] {
            let refused = mcp_post(port, straight, &approve);
            assert_eq!(refused["result"]["isError"], true, "{refused}");
            assert!(text(&refused).contains("\"download_cant_ask\"") && text(&refused).contains("`endeavor mcp`") && text(&refused).contains("(its browser_url) can't approve it either"), "{refused}");
            let tools = mcp_post(port, straight, &list);
            assert!(description(&tools).contains("can't be approved here") && !description(&tools).contains(crate::mcp::ASK_IN_CHAT), "{tools}");
            assert!(!tools["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "add_machine"), "{tools}");
        }
        for asked in ["X-Endeavor-Client: plugin\r\nX-Endeavor-Consent: front\r\n", "X-Endeavor-Client: app\r\n"] {
            let reply = mcp_post(port, asked, &approve);
            assert!(!text(&reply).contains("download_cant_ask"), "{asked}: {reply}");
            assert!(description(&mcp_post(port, asked, &list)).contains(crate::mcp::ASK_IN_CHAT), "{asked}");
        }
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
