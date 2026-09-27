//! `endeavor-remote connect`: the app's end on a machine. It answers questions
//! about the machine's files at once, and when the app asks, becomes the one
//! client of its Julia runtime (Pluto plus EndeavorRuntime): it attaches to the
//! runtime recorded in the state folder or starts one, then relays the app's
//! streams to the runtime's loopback ports over its own stdin/stdout
//! (docs/remote-sessions.md). It runs as a child of the app on This Mac, and
//! over `ssh` on a server. It is also ssh's askpass program there (see `askpass`).

mod askpass;
mod julia;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use serde_json::{Value, json};
use wire::relay::Mux;
use wire::{Frame, Target, ToApp, ToHelper};

const USAGE: &str = "usage: endeavor-remote connect --state-dir DIR (--julia JULIA|auto | --julia-shell LINE) --runtime RUNTIME_DIR --depot DEPOT [--quit-with-client] [--any-node]\n       endeavor-remote askpass PROMPT";
const LOG_TAIL: usize = 40;

struct Args {
    state_dir: PathBuf,
    julia: julia::Source,
    runtime: PathBuf,
    depot: String,
    /// Stop the runtime when the app goes away without saying Stop or Detach.
    quit_with_client: bool,
    /// The state folder belongs to this one machine, so a different node name
    /// only means the machine was renamed.
    any_node: bool,
}

/// `runtime.json`, written by boot.jl once the runtime is ready.
struct State {
    launcher: String,
    node: String,
    pid: i32,
    pluto_port: u16,
    mcp_port: u16,
    token: String,
    pluto_secret: String,
}

enum Event {
    App(ToHelper),
    /// The app closed our stdin.
    Eof,
    /// The runtime with this pid exited, with this status.
    Exited(i32, String),
    /// Another helper wants the runtime.
    Replaced,
}

/// The runtime's two ports while one is attached, for the streams the app opens.
type Ports = Arc<RwLock<Option<[u16; 2]>>>;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("askpass") => askpass::run(argv.get(1).map_or("", String::as_str)),
        // ssh runs `$SSH_ASKPASS PROMPT`, with no room for a mode argument.
        Some(prompt) if prompt != "connect" && std::env::var_os(wire::askpass::SOCKET_ENV).is_some() => askpass::run(prompt),
        _ => {}
    }
    let args = match parse_args(argv) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    // Before any thread starts, so every thread inherits the mask and only the watcher takes it.
    let replace_signal = block_sigusr1();
    // SAFETY: fd 1 is our stdout and stays open for the life of the process;
    // frames are binary, so skip std's line-buffered Stdout.
    let stdout = unsafe { File::from_raw_fd(1) };
    let mux = Mux::new(stdout);
    let Err(message) = serve(&args, &mux, replace_signal);
    eprintln!("endeavor-remote: {message}");
    let _ = mux.send(&ToApp::Error { message }.frame());
    std::process::exit(1);
}

fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("connect") {
        return Err("expected the `connect` command".into());
    }
    let (mut state_dir, mut julia, mut runtime, mut depot) = (None, None::<julia::Source>, None, None);
    let (mut quit_with_client, mut any_node) = (false, false);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--julia" | "--julia-shell" if julia.is_some() => return Err("give one of --julia and --julia-shell".into()),
            "--julia" => julia = Some(value().map(|v| if v == "auto" { julia::Source::Auto } else { julia::Source::Path(v) })?),
            "--julia-shell" => julia = Some(julia::Source::Shell(value()?)),
            "--runtime" => runtime = Some(PathBuf::from(value()?)),
            "--depot" => depot = Some(value()?),
            "--quit-with-client" => quit_with_client = true,
            "--any-node" => any_node = true,
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.ok_or("--state-dir is required")?,
        julia: julia.ok_or("--julia or --julia-shell is required")?,
        runtime: runtime.ok_or("--runtime is required")?,
        depot: depot.ok_or("--depot is required")?,
        quit_with_client,
        any_node,
    })
}

/// Say hello, then serve the app until it detaches or goes away, starting,
/// stopping and relaying to the runtime as it asks. Returns only on failure.
fn serve(args: &Args, mux: &Arc<Mux>, replace_signal: libc::sigset_t) -> Result<std::convert::Infallible, String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&args.state_dir)
        .map_err(|e| format!("Couldn't create {}: {e}", args.state_dir.display()))?;
    let (events, rx) = mpsc::channel();
    watch_replace_signal(replace_signal, events.clone());
    let ports: Ports = Arc::default();
    relay_stdin(mux.clone(), ports.clone(), events.clone());
    let home = wire::files::home().display().to_string();
    let _ = mux.send(&ToApp::Hello { version: env!("CARGO_PKG_VERSION").into(), node: hostname(), home }.frame());

    let mut attached: Option<Attached> = None;
    loop {
        match rx.recv().expect("senders live as long as their threads") {
            Event::App(ToHelper::StartRuntime) => {
                if let Some(attached) = &attached {
                    let _ = mux.send(&attached.ready(true).frame());
                    continue;
                }
                match attach(args, mux, &rx, &events) {
                    Ok(now) => {
                        *ports.write().unwrap() = Some([now.state.pluto_port, now.state.mcp_port]);
                        let _ = mux.send(&now.ready(now.reattached).frame());
                        attached = Some(now);
                    }
                    Err(message) => {
                        let _ = mux.send(&message.frame());
                    }
                }
            }
            Event::App(ToHelper::Stop) => {
                if let Some(runtime) = attached.take() {
                    *ports.write().unwrap() = None;
                    runtime.runtime.stop(Some(&runtime.state));
                }
                let _ = mux.send(&ToApp::Stopped.frame());
            }
            Event::Eof if args.quit_with_client => {
                if let Some(runtime) = attached.take() {
                    runtime.runtime.stop(Some(&runtime.state));
                }
                std::process::exit(0);
            }
            // An explicit Detach wins over --quit-with-client: the app decides at
            // quit, and the flag only covers an app that vanishes without saying.
            Event::App(ToHelper::Detach) | Event::Eof => std::process::exit(0),
            // Answered as they arrive (relay_stdin).
            Event::App(ToHelper::Files { .. }) => {}
            Event::Exited(pid, status) => {
                if attached.as_ref().is_some_and(|a| a.runtime.pid == pid) {
                    *ports.write().unwrap() = None;
                    let runtime = attached.take().expect("checked");
                    let _ = mux.send(&runtime.runtime.died(status).frame());
                }
            }
            Event::Replaced => {
                if attached.is_some() {
                    let _ = mux.send(&ToApp::Replaced.frame());
                    std::process::exit(0);
                }
            }
        }
    }
}

/// The runtime this helper is the client of, and the lock that makes it the only one.
struct Attached {
    runtime: Runtime,
    state: State,
    reattached: bool,
    _lock: File,
}

impl Attached {
    fn ready(&self, reattached: bool) -> ToApp {
        let state = &self.state;
        ToApp::Ready {
            launcher: state.launcher.clone(),
            node: state.node.clone(),
            pid: self.runtime.pid as u32,
            token: state.token.clone(),
            pluto_secret: state.pluto_secret.clone(),
            reattached,
        }
    }
}

/// Take the runtime over, or start one. The error is the app's answer: why it
/// couldn't start, or that it died while starting.
fn attach(args: &Args, mux: &Arc<Mux>, rx: &mpsc::Receiver<Event>, events: &Sender<Event>) -> Result<Attached, ToApp> {
    let failed = |message: String| ToApp::StartFailed { message };
    let lock = lock(&args.state_dir).map_err(failed)?;
    if let Some(state) = existing(args).map_err(failed)? {
        let runtime = Runtime { pid: state.pid, exit: Exit::watch_pid(state.pid, events.clone()), state_dir: args.state_dir.clone() };
        return Ok(Attached { runtime, state, reattached: true, _lock: lock });
    }
    let (julia, version) = julia::find(&args.julia, &|line| drop(mux.send(&ToApp::Progress { line }.frame()))).map_err(failed)?;
    let _ = mux.send(&ToApp::FoundJulia { path: julia.clone(), version }.frame());
    let token = token(&args.state_dir).map_err(failed)?;
    let child = start(args, &julia, &token).map_err(failed)?;
    let pid = child.id() as i32;
    let runtime = Runtime { pid, exit: Exit::watch_child(child, pid, events.clone()), state_dir: args.state_dir.clone() };
    let state = boot(args, mux, &runtime, rx)?;
    Ok(Attached { runtime, state, reattached: false, _lock: lock })
}

/// Wait for a runtime we just started to write its state and answer. A runtime
/// that isn't ready is never left behind: anything but its readiness stops it.
fn boot(args: &Args, mux: &Arc<Mux>, runtime: &Runtime, rx: &mpsc::Receiver<Event>) -> Result<State, ToApp> {
    let ready = Arc::new(AtomicBool::new(false));
    let log = follow_log(args.state_dir.join("runtime.log"), mux.clone(), ready.clone(), runtime.exit.clone());
    let result = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Exited(pid, status)) if pid == runtime.pid => break Err(runtime.died(status)),
            Ok(Event::Exited(..) | Event::App(ToHelper::StartRuntime | ToHelper::Files { .. })) => {}
            Ok(Event::Replaced) => {
                runtime.kill();
                let _ = mux.send(&ToApp::Replaced.frame());
                std::process::exit(0);
            }
            Ok(Event::App(ToHelper::Stop)) => {
                runtime.stop(None);
                break Err(ToApp::Stopped);
            }
            Ok(Event::App(ToHelper::Detach) | Event::Eof) => {
                runtime.stop(None);
                std::process::exit(0);
            }
            Err(RecvTimeoutError::Disconnected) => unreachable!("the watchers hold senders"),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(state) = read_state(&args.state_dir)
                    && state.pid == runtime.pid
                    && bridge_answers(&state)
                {
                    break Ok(state);
                }
            }
        }
    };
    // Its progress lines go out before Ready.
    ready.store(true, Ordering::SeqCst);
    let _ = log.join();
    result
}

/// A runtime process this helper watches.
struct Runtime {
    pid: i32,
    exit: Arc<Exit>,
    state_dir: PathBuf,
}

impl Runtime {
    /// It exited: clean up after it and say so.
    fn died(&self, status: String) -> ToApp {
        let log_tail = log_tail(&self.state_dir.join("runtime.log"));
        // Its notebook workers are no use without it.
        stop_workers(self.pid);
        remove_state(&self.state_dir, self.pid);
        ToApp::Died { status, log_tail }
    }

    /// Ask the runtime to shut down (when its bridge is up), then insist.
    fn stop(&self, state: Option<&State>) {
        if let Some(state) = state {
            let _ = bridge_call(state.mcp_port, &state.token, "endeavor/shutdown");
            self.exit.wait(Duration::from_secs(10));
        }
        self.kill();
    }

    fn kill(&self) {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            if self.exit.status().is_some() {
                break;
            }
            signal_group(self.pid, signal);
            self.exit.wait(Duration::from_secs(5));
        }
        stop_workers(self.pid);
        remove_state(&self.state_dir, self.pid);
    }
}

/// The runtime was started with setsid, so its pid is also its process group's.
fn signal_group(pid: i32, signal: i32) {
    // SAFETY: plain syscalls; a group or process that's gone only returns ESRCH.
    unsafe {
        libc::kill(-pid, signal);
        libc::kill(pid, signal);
    }
}

/// Notebook workers left behind by a runtime that exited without taking them
/// along. Only the group: once the runtime is reaped its pid may be reused, but
/// a group id isn't while any member is left.
fn stop_workers(pid: i32) {
    // SAFETY: plain syscall.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
}

/// Whether and how the runtime exited.
#[derive(Default)]
struct Exit {
    status: Mutex<Option<String>>,
    changed: Condvar,
}

impl Exit {
    fn watch_child(mut child: Child, pid: i32, events: Sender<Event>) -> Arc<Exit> {
        let exit = Arc::new(Exit::default());
        let e = exit.clone();
        std::thread::spawn(move || {
            let status = child.wait().map(|s| s.to_string()).unwrap_or_else(|e| e.to_string());
            e.set(pid, status, &events);
        });
        exit
    }

    /// A runtime some earlier helper started isn't our child: poll it.
    fn watch_pid(pid: i32, events: Sender<Event>) -> Arc<Exit> {
        let exit = Arc::new(Exit::default());
        let e = exit.clone();
        std::thread::spawn(move || {
            while pid_alive(pid) {
                std::thread::sleep(Duration::from_millis(500));
            }
            e.set(pid, "exited".into(), &events);
        });
        exit
    }

    fn set(&self, pid: i32, status: String, events: &Sender<Event>) {
        *self.status.lock().unwrap() = Some(status.clone());
        self.changed.notify_all();
        let _ = events.send(Event::Exited(pid, status));
    }

    fn status(&self) -> Option<String> {
        self.status.lock().unwrap().clone()
    }

    fn wait(&self, timeout: Duration) -> bool {
        let status = self.status.lock().unwrap();
        self.changed.wait_timeout_while(status, timeout, |s| s.is_none()).unwrap().0.is_some()
    }
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Take `DIR/lock`: one client per runtime. A helper already holding it is
/// asked to hand over (it tells its app it was replaced, then exits).
fn lock(dir: &Path) -> Result<File, String> {
    let path = dir.join("lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    let try_lock = |file: &File| unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !try_lock(&file) {
        let mut tries = 0;
        while !try_lock(&file) {
            // Every second, in case the holder hadn't written its pid yet.
            if tries % 10 == 0 {
                let mut text = String::new();
                let _ = (&file).seek(SeekFrom::Start(0)).and_then(|_| (&file).read_to_string(&mut text));
                if let Ok(pid) = text.trim().parse::<i32>()
                    && pid > 0
                    && pid != std::process::id() as i32
                {
                    // SAFETY: plain syscall.
                    unsafe { libc::kill(pid, libc::SIGUSR1) };
                }
            }
            tries += 1;
            if tries > 300 {
                return Err("Another connection to this Julia didn't hand it over.".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    file.set_len(0).and_then(|_| file.seek(SeekFrom::Start(0))).and_then(|_| write!(file, "{}", std::process::id()))
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    Ok(file)
}

fn block_sigusr1() -> libc::sigset_t {
    // SAFETY: initializing and applying a signal set on this (still only) thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    }
}

fn watch_replace_signal(set: libc::sigset_t, events: Sender<Event>) {
    std::thread::spawn(move || {
        loop {
            let mut signal = 0;
            // SAFETY: `set` holds only SIGUSR1, blocked in every thread.
            if unsafe { libc::sigwait(&set, &mut signal) } == 0 && signal == libc::SIGUSR1 {
                let _ = events.send(Event::Replaced);
            }
        }
    });
}

/// Read frames from the app: streams go to the runtime's ports while one is
/// attached, file requests are answered on their own threads, other control
/// messages and the end of input become events.
fn relay_stdin(mux: Arc<Mux>, ports: Ports, events: Sender<Event>) {
    std::thread::spawn(move || {
        // SAFETY: fd 0 is our stdin; only this thread reads it.
        let stdin = BufReader::new(unsafe { File::from_raw_fd(0) });
        let control = events.clone();
        let answering = mux.clone();
        let result = mux.run(
            stdin,
            |mux, id, target| dial(mux, id, target, *ports.read().unwrap()),
            |json| match serde_json::from_slice::<ToHelper>(json) {
                Ok(ToHelper::Files { id, request }) => {
                    let mux = answering.clone();
                    std::thread::spawn(move || drop(mux.send(&ToApp::Files { id, reply: wire::files::answer(&request) }.frame())));
                }
                Ok(message) => drop(control.send(Event::App(message))),
                Err(e) => eprintln!("endeavor-remote: ignoring control message: {e}"),
            },
        );
        if let Err(e) = result {
            eprintln!("endeavor-remote: reading from the app: {e}");
        }
        let _ = events.send(Event::Eof);
    });
}

fn dial(mux: &Arc<Mux>, id: u32, target: Target, ports: Option<[u16; 2]>) {
    let port = ports.map(|p| if target == Target::Pluto { p[0] } else { p[1] });
    let socket = port.ok_or(()).and_then(|port| {
        TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).map_err(drop)
    });
    match socket {
        Ok(socket) => {
            let _ = socket.set_nodelay(true);
            let _ = mux.attach(id, socket);
        }
        Err(()) => {
            let _ = mux.send(&Frame::Close { id });
        }
    }
}

/// The runtime in `runtime.json`, if it's alive and answering here.
fn existing(args: &Args) -> Result<Option<State>, String> {
    let Some(state) = read_state(&args.state_dir) else { return Ok(None) };
    let here = hostname();
    if state.node != here && !args.any_node {
        return Err(format!(
            "Julia for this folder is running on {}, and this is {here}. Connect to {} to use it, or stop it there.",
            state.node, state.node
        ));
    }
    // Twice: a busy runtime can be slow to answer once.
    if pid_alive(state.pid) && (bridge_answers(&state) || bridge_answers(&state)) {
        return Ok(Some(state));
    }
    eprintln!("endeavor-remote: the recorded runtime (pid {}) isn't answering; starting a new one", state.pid);
    Ok(None)
}

fn read_state(dir: &Path) -> Option<State> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?;
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    let port = |k: &str| v[k].as_u64().and_then(|p| u16::try_from(p).ok());
    Some(State {
        launcher: text("launcher")?,
        node: text("node")?,
        pid: v["pid"].as_i64().and_then(|p| i32::try_from(p).ok())?,
        pluto_port: port("pluto_port")?,
        mcp_port: port("mcp_port")?,
        token: text("token")?,
        pluto_secret: text("pluto_secret")?,
    })
}

/// Remove `runtime.json` if it still describes the runtime `pid`.
fn remove_state(dir: &Path, pid: i32) {
    let path = dir.join("runtime.json");
    let current = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    if current.is_none_or(|v| v["pid"].as_i64() == Some(pid as i64)) {
        let _ = std::fs::remove_file(path);
    }
}

/// The bridge's bearer token, kept in the state folder so it stays the same
/// across runtimes (the agent's MCP config carries it).
fn token(dir: &Path) -> Result<String, String> {
    let path = dir.join("token");
    if let Ok(token) = std::fs::read_to_string(&path)
        && token.trim().len() == 64
    {
        return Ok(token.trim().to_owned());
    }
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("/dev/urandom: {e}"))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    file.write_all(token.as_bytes()).map_err(|e| e.to_string())?;
    Ok(token)
}

/// Start `boot.jl` detached from us (its own session, no terminal, stdin from
/// /dev/null), logging to `runtime.log`.
fn start(args: &Args, julia: &str, token: &str) -> Result<Child, String> {
    let ports = free_ports()?;
    let dir = &args.state_dir;
    let _ = std::fs::remove_file(dir.join("runtime.json"));
    let log_path = dir.join("runtime.log");
    // The log shows Pluto's secret URL.
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(&log_path)
        .and_then(|f| f.set_len(0).map(|_| f))
        .map_err(|e| format!("Couldn't open {}: {e}", log_path.display()))?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    let runtime = args.runtime.display();
    let mut command = Command::new(julia);
    command
        .arg("--color=no")
        .arg(format!("--project={runtime}"))
        .arg(format!("{runtime}/boot.jl"))
        .args(ports.map(|p| p.to_string()))
        .env("JULIA_DEPOT_PATH", &args.depot)
        // Not argv, which `ps` shows to every user.
        .env("ENDEAVOR_TOKEN", token)
        .env("ENDEAVOR_STATE", dir.join("runtime.json"))
        .env("ENDEAVOR_LAUNCHER", "process")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn().map_err(|e| format!("Couldn't start {julia}: {e}"))
}

fn free_ports() -> Result<[u16; 2], String> {
    // Both held at once so the OS can't hand out the same port twice.
    let pluto = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let mcp = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    Ok([&pluto, &mcp].map(|l| l.local_addr().unwrap().port()))
}

/// Send the runtime's log lines as `Progress` until it's ready (then what's
/// been written so far) or gone.
fn follow_log(path: PathBuf, mux: Arc<Mux>, ready: Arc<AtomicBool>, exit: Arc<Exit>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(file) = File::open(&path) else { return };
        let mut lines = BufReader::new(file);
        let mut line = String::new();
        loop {
            let last_pass = ready.load(Ordering::SeqCst) || exit.status().is_some();
            match lines.read_line(&mut line) {
                Ok(n) if n > 0 && line.ends_with('\n') => {
                    let text = redact_secret(line.trim_end());
                    let _ = mux.send(&ToApp::Progress { line: text }.frame());
                    line.clear();
                }
                // At the end for now; a partial line waits for the rest.
                Ok(_) if !last_pass => std::thread::sleep(Duration::from_millis(100)),
                _ => return,
            }
        }
    })
}

/// The end of the runtime's log, secrets masked.
fn log_tail(path: &Path) -> Vec<String> {
    let Ok(mut file) = File::open(path) else { return Vec::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..].iter().map(|l| redact_secret(l)).collect()
}

/// Mask Pluto's `secret=…` URL token.
fn redact_secret(line: &str) -> String {
    let Some(start) = line.find("secret=").map(|i| i + "secret=".len()) else { return line.to_string() };
    let end = line[start..].find(|c: char| !c.is_ascii_alphanumeric()).map_or(line.len(), |i| start + i);
    format!("{}…{}", &line[..start], &line[end..])
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into `buf`.
    unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn bridge_answers(state: &State) -> bool {
    bridge_call(state.mcp_port, &state.token, "ping").is_ok_and(|status| status == 200)
}

/// POST one JSON-RPC call to the bridge's `/call`; its HTTP status.
fn bridge_call(port: u16, token: &str, method: &str) -> std::io::Result<u16> {
    let mut socket = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2))?;
    socket.set_read_timeout(Some(Duration::from_secs(3)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} }).to_string();
    write!(
        socket,
        "POST /call HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    let mut status = String::new();
    BufReader::new(socket).read_line(&mut status)?;
    status.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| std::io::ErrorKind::InvalidData.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_pluto_secret() {
        let line = "│ Go to http://localhost:58250/?secret=wwD760ru in your browser";
        assert_eq!(redact_secret(line), "│ Go to http://localhost:58250/?secret=… in your browser");
        assert_eq!(redact_secret("no token here"), "no token here");
    }

    #[test]
    fn parses_connect_arguments() {
        let args = |s: &str| parse_args(s.split(' ').map(String::from).collect());
        let a = args("connect --state-dir /s --julia /j --runtime /r --depot /d: --quit-with-client").unwrap();
        assert_eq!((a.state_dir, a.julia, a.depot.as_str()), (PathBuf::from("/s"), julia::Source::Path("/j".into()), "/d:"));
        assert!(a.quit_with_client && !a.any_node);
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d").unwrap().julia, julia::Source::Auto);
        let shell = parse_args(["connect", "--julia-shell", "module load julia", "--state-dir", "/s", "--runtime", "/r", "--depot", "/d"].map(String::from).to_vec());
        assert_eq!(shell.unwrap().julia, julia::Source::Shell("module load julia".into()));
        assert!(args("connect --state-dir /s --julia /j --julia-shell x --runtime /r --depot /d").is_err());
        assert!(args("connect --state-dir /s").is_err());
        assert!(args("serve --state-dir /s --julia /j --runtime /r --depot /d").is_err());
        assert!(args("connect --state-dir /s --julia /j --runtime /r --depot /d --bogus").is_err());
    }
}
