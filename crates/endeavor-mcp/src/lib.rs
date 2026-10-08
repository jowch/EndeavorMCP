//! `endeavor connect`: the app's end on a machine. It answers questions
//! about the machine's files at once, and when the app asks, becomes the one
//! client of its Julia runtime (Pluto plus EndeavorRuntime): it attaches to the
//! runtime recorded in the state folder or starts one, then relays the app's
//! streams to the runtime's one loopback port over its own stdin/stdout
//! (docs/remote-sessions.md). It runs over `ssh` on a server as the
//! `endeavor` binary; on This Mac the app runs itself as the helper
//! (`endeavor --helper connect …`, calling `run`), so its helper can't go
//! missing or be from another build. It is also ssh's askpass program (see `askpass`).
//!
//! On a cluster (`--launcher slurm`) the runtime runs in a Slurm job instead,
//! and the streams go on through a second helper on the job's node
//! (`endeavor relay`, see `slurm`).

mod askpass;
mod asks;
pub mod client;
mod core;
mod guard;
mod guide;
mod host_tools;
mod http;
mod julia;
mod mcp;
mod notebooks;
pub mod paths;
mod release;
mod results;
mod runtime;
mod slurm;
mod standalone;
mod stopped;
mod update;
#[cfg(windows)]
mod winproc;

/// The wire crate, for the types in this crate's signatures (`wire::Item`, `wire::KIND_RUNTIME`, `wire::ENGINE_PLUTO`).
pub use wire;
pub use core::serve_unreachable;
pub use guard::serve_guarded;
pub use mcp::{NOTEBOOK_TOOLS_JSON, asks_first, changes_notebook, is_tool, runs_code};
pub use standalone::{Lease, lease, unpack};

/// `runtime/` (the Julia side, which Endeavor installs on servers and runs on
/// This Mac) and `plugin/` (the skills Endeavor loads as its Claude Code
/// plugin), built into the binary (build.rs), and the Helpers release's key
/// this build was made for, when the release build recorded it.
pub mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::Duration;

use serde_json::{Value, json};
use wire::relay::Mux;
use wire::files::RuntimeState;
use wire::slurm::JobRequest;
use runtime::{Ended, Hooks, Looked, Outcome, Up, Waiting, Want};
use wire::{Frame, ToApp, ToHelper};

const USAGE: &str = "usage: endeavor connect [--state-dir DIR] (--julia JULIA|auto | --julia-shell LINE) --runtime RUNTIME_DIR --depot DEPOT [--launcher process|slurm] [--quit-with-client] [--any-node] [--exit-idle] [--build BUILD]
                        (--state-dir defaults to the folder `serve` and `mcp` use; with --launcher slurm, to one for the cluster)
       endeavor relay --state-dir DIR
       endeavor node-start --state-dir DIR --julia JULIA --runtime RUNTIME_DIR --depot DEPOT [--build BUILD]
       endeavor core --state-dir DIR --julia JULIA --runtime RUNTIME_DIR --depot DEPOT
       endeavor askpass PROMPT
       endeavor serve|mcp|stop [OPTIONS]   (without the app; `endeavor serve --help`)
       endeavor update                      replace this binary with the newest build (Linux)
       endeavor --version";
const LOG_TAIL: usize = 40;

struct Args {
    state_dir: PathBuf,
    julia: julia::Source,
    runtime: PathBuf,
    depot: String,
    launcher: Launcher,
    /// Stop the runtime when the app goes away without saying Stop or Detach.
    quit_with_client: bool,
    /// The state folder belongs to this one machine, so a different node name
    /// only means the machine was renamed.
    any_node: bool,
    /// The app build this helper and its runtime came from, which a runtime it
    /// starts reports to the app.
    build: Option<String>,
    /// `--exit-idle`: the runtime ends once no notebook has been open for a while
    /// (see `core::main`). The Slurm launcher passes it on to the job.
    exit_idle: bool,
    /// More of the core's environment: a standalone runtime's settings, and
    /// ENDEAVOR_EXIT_IDLE when `exit_idle`. Set on a core this helper starts.
    core_env: Vec<(&'static str, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Launcher {
    /// A detached process on this machine.
    Process,
    /// A Slurm job; this machine is a login node.
    Slurm,
}

/// `runtime.json`, written by the core once the runtime is ready.
struct State {
    launcher: String,
    node: String,
    pid: i32,
    /// When that process started (Windows: its creation time, which tells it
    /// from a later process given the same pid); none on Unix.
    started: Option<u64>,
    /// The runtime's one port (the core's). None for a runtime from a build
    /// before one port per runtime, which this helper can stop but not relay to.
    port: Option<u16>,
    token: String,
    /// The Slurm job it runs in.
    job: Option<String>,
    /// The build that started it (`embedded::BUILD_VERSION`); none for one from before builds were recorded.
    build: Option<String>,
    /// The notebooks folder a standalone runtime recorded.
    folder: Option<String>,
    /// Whether it ends itself when idle; none for a record from before this was written.
    exits_when_idle: Option<bool>,
}

/// Why a runtime from a build before one port per runtime can't be used.
pub const OLDER_RUNTIME: &str = "Julia here was started by an older version of Endeavor, which this version can't connect to. Restart Julia to use it.";

enum Event {
    App(ToHelper),
    /// The app closed our stdin.
    Eof,
    /// The runtime with this pid exited, with this status.
    Exited(i32, String),
    /// A control message from the relay on a job's node (`slurm::Link`).
    Node(u64, ToApp),
    /// That relay's output ended.
    NodeGone(u64),
}

/// Where the app's streams go while a runtime is attached.
#[derive(Clone)]
enum Route {
    None,
    /// The runtime's loopback port here.
    Local(u16),
    /// On to the relay on the job's node.
    Node(Arc<slurm::Link>),
}

type Routes = Arc<RwLock<Route>>;

/// How the helper answers the app's file requests.
type Answer = Arc<dyn Fn(&wire::files::Request) -> wire::files::Reply + Send + Sync>;

/// Arguments this program needs before the helper's own to run as the helper
/// again (for the core): none for the `endeavor` binary, the flag for the app.
static HELPER_ARGS: OnceLock<&'static [&'static str]> = OnceLock::new();

/// The helper's main, given its arguments without the program name. Call it
/// before the process starts any thread: it blocks SIGUSR1 for all of them.
pub fn run(argv: Vec<String>) -> ! {
    run_as(&[], argv)
}

/// `run` in a program that acts as the helper when started with `helper_args`
/// before the helper's arguments (the app: `endeavor --helper …`).
pub fn run_as(helper_args: &'static [&'static str], argv: Vec<String>) -> ! {
    let _ = HELPER_ARGS.set(helper_args);
    match argv.first().map(String::as_str) {
        Some("askpass") => askpass::run(argv.get(1).map_or("", String::as_str)),
        Some("relay") => slurm::relay_main(&argv[1..]),
        Some("node-start") => slurm::node_start_main(&argv[1..]),
        Some("core") => core::main(&argv[1..]),
        Some("serve" | "mcp" | "stop" | "status") => standalone::main(&argv),
        Some("--version" | "-V" | "version") => update::print_version(),
        Some("update") => update::main(&argv[1..]),
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
    block_sigusr1();
    let mux = stdout_mux();
    let Err(message) = serve(&args, &mux);
    eprintln!("endeavor: {message}");
    let _ = mux.send(&ToApp::Error { message }.frame());
    std::process::exit(1);
}

#[cfg(unix)]
fn stdout_mux() -> Arc<Mux> {
    // SAFETY: fd 1 is our stdout and stays open for the life of the process;
    // frames are binary, so skip std's line-buffered Stdout.
    let stdout = unsafe { File::from_raw_fd(1) };
    Mux::new(stdout)
}

#[cfg(windows)]
fn stdout_mux() -> Arc<Mux> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    // SAFETY: our stdout handle stays open for the life of the process;
    // frames are binary, so skip std's line-buffered Stdout.
    let stdout = unsafe { File::from_raw_handle(std::io::stdout().as_raw_handle()) };
    Mux::new(stdout)
}

fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("connect") {
        return Err("expected the `connect` command".into());
    }
    let (mut state_dir, mut julia, mut runtime, mut depot) = (None, None::<julia::Source>, None, None);
    let (mut quit_with_client, mut any_node, mut launcher, mut build, mut exit_idle) = (false, false, Launcher::Process, None, false);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--julia" | "--julia-shell" if julia.is_some() => return Err("give one of --julia and --julia-shell".into()),
            "--julia" => julia = Some(value().map(|v| if v == "auto" { julia::Source::Auto } else { julia::Source::Path(v) })?),
            "--julia-shell" => julia = Some(julia::Source::Shell(value()?)),
            "--runtime" => runtime = Some(PathBuf::from(value()?)),
            "--depot" => depot = Some(value()?),
            "--launcher" => {
                launcher = match value()?.as_str() {
                    "process" => Launcher::Process,
                    "slurm" => Launcher::Slurm,
                    other => return Err(format!("unknown launcher {other}")),
                }
            }
            "--quit-with-client" => quit_with_client = true,
            "--any-node" => any_node = true,
            "--exit-idle" => exit_idle = true,
            "--build" => build = Some(value()?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    Ok(Args {
        state_dir: state_dir.unwrap_or_else(|| match launcher {
            Launcher::Process => paths::Env::here().state_dir(),
            Launcher::Slurm => paths::Env::here().cluster_state_dir(),
        }),
        julia: julia.ok_or("--julia or --julia-shell is required")?,
        runtime: runtime.ok_or("--runtime is required")?,
        depot: depot.ok_or("--depot is required")?,
        launcher,
        quit_with_client,
        any_node,
        build,
        exit_idle,
        core_env: if exit_idle { vec![("ENDEAVOR_EXIT_IDLE", "1".into())] } else { Vec::new() },
    })
}

fn make_state_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))
}

/// `options` for a file only this user may read. On Windows the state folder
/// is under the user's own %LOCALAPPDATA%, whose permissions already do that.
fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    options.mode(0o600);
    options
}

/// Say hello, then serve the app until it detaches or goes away, starting,
/// stopping and relaying to the runtime as it asks. Returns only on failure.
fn serve(args: &Args, mux: &Arc<Mux>) -> Result<std::convert::Infallible, String> {
    make_state_dir(&args.state_dir)?;
    let (events, rx) = mpsc::channel();
    let mut inbox = Inbox { rx, later: VecDeque::new() };
    let routes: Routes = Arc::new(RwLock::new(Route::None));
    let parts = Parts::default();
    let answer: Answer = {
        let (dir, launcher, any_node, parts) = (args.state_dir.clone(), args.launcher, args.any_node, parts.clone());
        Arc::new(move |request| match request {
            wire::files::Request::Runtime => wire::files::Reply::Runtime { runtime: check(&dir, launcher, any_node) },
            wire::files::Request::Write { folder, path, offset, last, .. } => parts.track(folder, path, *offset, *last, || wire::files::answer(request)),
            other => wire::files::answer(other),
        })
    };
    relay_stdin(mux.clone(), routes.clone(), events.clone(), answer, parts.clone());
    let home = wire::files::home().display().to_string();
    let slurm_here = wire::slurm::has("sinfo");
    let hello = ToApp::Hello { protocol: wire::PROTOCOL, version: env!("CARGO_PKG_VERSION").into(), node: hostname(), home, slurm: slurm_here, uploads: true };
    let _ = mux.send(&hello.frame());

    let mut attached: Option<Attached> = None;
    loop {
        let event = inbox.next();
        match event {
            Event::App(ToHelper::StartRuntime { id, job, engine, install }) => {
                if let Some(attached) = &attached {
                    let _ = mux.send(&attached.ready(id, true).frame());
                    continue;
                }
                let result = match args.launcher {
                    Launcher::Process => attach(args, mux, &mut inbox, &events, &parts, &engine, install),
                    Launcher::Slurm => slurm::attach(args, mux, &mut inbox, &events, &parts, job.unwrap_or_default(), &engine, install),
                };
                match result {
                    Ok(now) => {
                        *routes.write().unwrap() = now.route();
                        let _ = mux.send(&now.ready(id, now.reattached).frame());
                        attached = Some(now);
                    }
                    Err(unstarted) => unstarted.answer(mux, id),
                }
            }
            Event::App(ToHelper::Stop { id }) => {
                let stopped = match attached.take() {
                    Some(runtime) => runtime.stop(args, &routes, &mut inbox, standalone::stop_lock_limit()).map_err(|failed| {
                        let (runtime, why) = *failed;
                        attached = Some(runtime);
                        why
                    }),
                    None => stop_recorded(args, &mut inbox, &events),
                };
                // A Stop said during this one has the same outcome: it is not stopped again.
                for id in std::iter::once(id).chain(inbox.take_stops()) {
                    let reply = match &stopped {
                        Ok(()) => ToApp::Stopped { id },
                        Err(message) => ToApp::NotStopped { id, message: message.clone() },
                    };
                    let _ = mux.send(&reply.frame());
                }
            }
            Event::Eof if args.quit_with_client => {
                if let Some(runtime) = attached.take()
                    && let Err(failed) = runtime.stop(args, &routes, &mut inbox, standalone::start_lock_limit())
                {
                    eprintln!("endeavor: {}", failed.1);
                }
                std::process::exit(0);
            }
            // An explicit Detach wins over --quit-with-client: the app decides at
            // quit, and the flag only covers an app that vanishes without saying.
            Event::App(ToHelper::Detach) => {
                parts.discard();
                std::process::exit(0)
            }
            Event::Eof => std::process::exit(0),
            // Answered as they arrive (relay_stdin).
            Event::App(ToHelper::Files { .. }) => {}
            Event::Exited(pid, status) => {
                if attached.as_ref().is_some_and(|a| matches!(&a.how, How::Process(r, _) if r.pid == pid)) {
                    *routes.write().unwrap() = Route::None;
                    let Some(How::Process(runtime, _)) = attached.take().map(|a| a.how) else { unreachable!() };
                    let (status, log_tail) = runtime.died(status);
                    let _ = mux.send(&ToApp::Died { status, log_tail }.frame());
                }
            }
            Event::Node(generation, message) => {
                let ours = attached.as_ref().is_some_and(|a| matches!(&a.how, How::Slurm(j) if j.generation() == generation));
                if ours && let ToApp::Died { status, log_tail } = message {
                    lost_node(args, mux, &routes, &mut attached, &events, Some((status, log_tail)));
                }
            }
            Event::NodeGone(generation) => {
                if attached.as_ref().is_some_and(|a| matches!(&a.how, How::Slurm(j) if j.generation() == generation)) {
                    lost_node(args, mux, &routes, &mut attached, &events, None);
                }
            }
        }
    }
}

/// The relay to the job's node ended: reconnect if the job still runs (the
/// connection inside the cluster dropped), else say how the job ended.
fn lost_node(args: &Args, mux: &Arc<Mux>, routes: &Routes, attached: &mut Option<Attached>, events: &Sender<Event>, said: Option<(String, Vec<String>)>) {
    *routes.write().unwrap() = Route::None;
    let Some(mut now) = attached.take() else { return };
    let How::Slurm(job) = &mut now.how else { return };
    if said.is_none()
        && let Ok(()) = job.reconnect(mux, events)
    {
        *routes.write().unwrap() = now.route();
        *attached = Some(now);
        return;
    }
    let _ = mux.send(&job.ended(&args.state_dir, said).frame());
}

/// The runtime this helper is a client of. Any number of helpers attach to one runtime.
struct Attached {
    how: How,
    state: State,
    reattached: bool,
}

enum How {
    /// A process here, and its port.
    Process(Runtime, u16),
    Slurm(slurm::Running),
}

impl Attached {
    fn ready(&self, id: u32, reattached: bool) -> ToApp {
        let state = &self.state;
        ToApp::Ready {
            id,
            launcher: state.launcher.clone(),
            node: state.node.clone(),
            pid: state.pid as u32,
            token: state.token.clone(),
            reattached,
            job: match &self.how {
                How::Process(..) => None,
                How::Slurm(job) => Some(job.info()),
            },
            port: match &self.how {
                How::Process(_, port) => Some(*port),
                How::Slurm(_) => state.port,
            },
        }
    }

    fn route(&self) -> Route {
        match &self.how {
            How::Process(_, port) => Route::Local(*port),
            How::Slurm(job) => Route::Node(job.link()),
        }
    }

    /// Stop the runtime for every client. The start lock is held throughout, so
    /// a helper that is asked for a runtime meanwhile starts a new one after this
    /// one is gone, and never attaches to one that is on its way out. Without
    /// the lock within `limit` the runtime isn't stopped: it comes back with
    /// why, still routed to. The streams are cut only once the lock is held,
    /// and restored if a process is still alive after its stop.
    fn stop(self, args: &Args, routes: &Routes, inbox: &mut Inbox, limit: Duration) -> Result<(), Box<(Attached, String)>> {
        let _starting = match lock_stop(&args.state_dir, inbox, limit) {
            Ok(lock) => lock,
            Err(why) => return Err(Box::new((self, why))),
        };
        *routes.write().unwrap() = Route::None;
        match &self.how {
            How::Process(runtime, _) => {
                if !stop_marked(&args.state_dir, &self.state, runtime, stopped::How::Connection) {
                    *routes.write().unwrap() = self.route();
                    return Err(Box::new((self, STILL_RUNNING.into())));
                }
            }
            How::Slurm(_) => {
                stopped::mark(&args.state_dir, stopped::Of::Runtime(self.state.pid), stopped::How::Connection);
                let How::Slurm(job) = self.how else { unreachable!() };
                job.stop(inbox);
            }
        }
        Ok(())
    }
}

/// Why a stop that left the process alive is refused.
const STILL_RUNNING: &str = "Julia was not stopped: it is still running.";

/// Stop `runtime`, leaving a note for the other clients of how it was stopped
/// (see `stopped`), and taking the note back if it is still alive. Whether it
/// is gone.
fn stop_marked(dir: &Path, state: &State, runtime: &Runtime, how: stopped::How) -> bool {
    let of = stopped::Of::Runtime(state.pid);
    stopped::mark(dir, of, how);
    runtime.stop(Some(state));
    let gone = !pid_alive(state.pid, state.started);
    if !gone {
        stopped::unmark(dir, of);
    }
    gone
}

/// What the app and the runtime have said that the helper has not yet handled.
struct Inbox {
    rx: mpsc::Receiver<Event>,
    /// Events a stop took off `rx` that are still the loop's to handle.
    later: VecDeque<Event>,
}

/// What a start hears while it waits.
enum Heard {
    /// Nothing, or a `StartRuntime`, which was answered as busy.
    Quiet,
    /// The `Stop` with this id: it cuts the start short.
    Stop(u32),
    Detach,
    Eof,
    /// What happened to a runtime or a relay: nothing a start under way acts on.
    Event,
}

impl Inbox {
    /// The next event, waiting for one.
    fn next(&mut self) -> Event {
        self.later.pop_front().unwrap_or_else(|| self.rx.recv().expect("senders live as long as their threads"))
    }

    /// The next event for a start under way, taking the ones a stop kept first,
    /// and waiting up to `wait` for one. A `StartRuntime` meanwhile is refused,
    /// so that it is answered.
    fn hear_while_starting(&mut self, mux: &Arc<Mux>, wait: Duration) -> Heard {
        let event = match self.later.pop_front() {
            Some(event) => event,
            None => match self.rx.recv_timeout(wait) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => return Heard::Quiet,
                Err(RecvTimeoutError::Disconnected) => unreachable!("the watchers hold senders"),
            },
        };
        match event {
            Event::App(ToHelper::StartRuntime { id, .. }) => {
                let _ = mux.send(&ToApp::StartFailed { id, message: "Julia is already starting.".into() }.frame());
                Heard::Quiet
            }
            Event::App(ToHelper::Stop { id }) => Heard::Stop(id),
            Event::App(ToHelper::Detach) => Heard::Detach,
            Event::Eof => Heard::Eof,
            _ => Heard::Event,
        }
    }

    /// Keep `event` for the main loop once a stop is over. What happened to the
    /// runtime goes before what the app said, so that a start asked for meanwhile
    /// doesn't find a runtime that has since gone still attached.
    fn defer(&mut self, event: Event) {
        let app = |event: &Event| matches!(event, Event::App(_) | Event::Eof);
        let at = if app(&event) { self.later.len() } else { self.later.iter().position(app).unwrap_or(self.later.len()) };
        self.later.insert(at, event);
    }

    /// The ids of the `Stop`s kept that came before any start, a detach or the
    /// end of input, which are no longer kept: a stop that is over answers them.
    fn take_stops(&mut self) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut at = 0;
        while let Some(event) = self.later.get(at) {
            match event {
                Event::App(ToHelper::Stop { id }) => {
                    ids.push(*id);
                    self.later.remove(at);
                }
                Event::App(ToHelper::StartRuntime { .. } | ToHelper::Detach) | Event::Eof => break,
                _ => at += 1,
            }
        }
        ids
    }
}

/// How a start ended without a runtime: what answers it.
enum Unstarted {
    Failed(String),
    /// The start needs these installed and `install` was false.
    NeedsInstall(Vec<wire::Item>),
    Died { status: String, log_tail: Vec<String> },
    /// The `Stop` with this id cut it short; that is answered too.
    Stopped(u32),
}

impl Unstarted {
    /// Answer the `StartRuntime` with id `start`, and the `Stop` that ended it.
    fn answer(self, mux: &Arc<Mux>, start: u32) {
        let (reply, stop) = match self {
            Unstarted::Failed(message) => (ToApp::StartFailed { id: start, message }, None),
            Unstarted::NeedsInstall(items) => (ToApp::NeedsInstall { id: start, items }, None),
            Unstarted::Died { status, log_tail } => (ToApp::StartDied { id: start, status, log_tail }, None),
            Unstarted::Stopped(stop) => (ToApp::StartCancelled { id: start }, Some(stop)),
        };
        let _ = mux.send(&reply.frame());
        if let Some(id) = stop {
            let _ = mux.send(&ToApp::Stopped { id }.frame());
        }
    }
}

/// What the helper says to its client, and hears from it, while a start goes on (`runtime::Hooks`).
struct Client<'a> {
    args: &'a Args,
    mux: &'a Arc<Mux>,
    inbox: &'a mut Inbox,
    parts: &'a Parts,
    /// Whether the client was told that another helper holds the start lock.
    told: bool,
    /// The `Stop` that ended the start.
    stop: Option<u32>,
}

impl<'a> Client<'a> {
    fn new(args: &'a Args, mux: &'a Arc<Mux>, inbox: &'a mut Inbox, parts: &'a Parts) -> Client<'a> {
        Client { args, mux, inbox, parts, told: false, stop: None }
    }

    /// Hear what the client said while a stop waited, before a start for it is made: a client that has
    /// since left gets none. Err when a `Stop` among it ended the start.
    fn hear_kept(&mut self) -> Result<(), Unstarted> {
        while !self.inbox.later.is_empty() {
            if !self.hear(Duration::ZERO, Waiting::Lock) {
                return Err(self.cancelled());
            }
        }
        Ok(())
    }

    /// Hear the client for up to `wait`. Detach and the end of input exit, leaving a runtime that is
    /// starting to finish by itself; but the end of input stops it where `--quit-with-client` says the
    /// runtime goes with the client. A `Stop` ends the start. A `StartRuntime` is refused. False: the start is over.
    fn hear(&mut self, wait: Duration, waiting: Waiting) -> bool {
        match self.inbox.hear_while_starting(self.mux, wait) {
            Heard::Stop(id) => {
                self.stop = Some(id);
                return false;
            }
            Heard::Detach => {
                self.parts.discard();
                std::process::exit(0)
            }
            Heard::Eof if self.args.quit_with_client && waiting != Waiting::Lock => return false,
            Heard::Eof => std::process::exit(0),
            Heard::Quiet | Heard::Event => {}
        }
        true
    }

    /// How a start that `hear` ended is answered; none is when the client has gone, and the process exits.
    fn cancelled(&self) -> Unstarted {
        match self.stop {
            Some(id) => Unstarted::Stopped(id),
            None => std::process::exit(0),
        }
    }

    /// The app's answer to a start that came to nothing.
    fn unstarted(&self, outcome: Outcome) -> Unstarted {
        match outcome {
            Outcome::Failed(message) => Unstarted::Failed(message),
            Outcome::NeedsInstall(items) => Unstarted::NeedsInstall(items),
            Outcome::Died { status, log_tail } => Unstarted::Died { status, log_tail },
            Outcome::Unusable(Looked::OtherNode(state)) => Unstarted::Failed(runtime::other_node_text(&state.node)),
            Outcome::Unusable(_) => Unstarted::Failed(OLDER_RUNTIME.into()),
            Outcome::Cancelled => self.cancelled(),
            Outcome::Ready(_) => unreachable!("a runtime is not an unstarted one"),
        }
    }

    /// Take the start lock, with what the client said while a stop waited heard first. While another
    /// helper holds it the client is told once, and what it sends is still served.
    fn lock_start(&mut self) -> Result<File, Unstarted> {
        self.hear_kept()?;
        runtime::take_start_lock(&self.args.state_dir, self).map_err(|outcome| self.unstarted(outcome))
    }
}

impl Hooks for Client<'_> {
    fn progress(&mut self, line: String) {
        let _ = self.mux.send(&ToApp::Progress { line }.frame());
    }

    fn found(&mut self, version: &str, path: &str) {
        let _ = self.mux.send(&ToApp::Found { name: "Julia".into(), version: version.into(), path: path.into() }.frame());
    }

    fn wait(&mut self, wait: Duration, waiting: Waiting) -> bool {
        if waiting != Waiting::Ready && !self.told {
            self.progress("Another connection is starting Julia here; waiting for it.".into());
            self.told = true;
        }
        self.hear(wait, waiting)
    }
}

/// Take the start lock to stop the runtime, waiting up to `limit`. What the
/// client sends meanwhile doesn't cancel the stop, and none of it is lost: it
/// is kept in `later`.
fn lock_stop(dir: &Path, inbox: &mut Inbox, limit: Duration) -> Result<File, String> {
    let waited = standalone::wait_for_start_lock(dir, limit, || {
        match inbox.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => inbox.defer(event),
            Err(RecvTimeoutError::Disconnected) => unreachable!("the watchers hold senders"),
            Err(RecvTimeoutError::Timeout) => {}
        }
        Ok::<(), std::convert::Infallible>(())
    });
    waited.map_err(|wait| match wait {
        standalone::Wait::Failed(message) => message,
        standalone::Wait::TimedOut => standalone::stop_lock_gave_up(dir),
        standalone::Wait::Interrupted(never) => match never {},
    })
}

/// Attach to the runtime in the state folder, or start one (`runtime::find_or_start`). Helpers asked at
/// once start one runtime and the rest attach to it. The error is the app's answer: why it couldn't
/// start, or that it died while starting.
fn attach(args: &Args, mux: &Arc<Mux>, inbox: &mut Inbox, events: &Sender<Event>, parts: &Parts, engine: &str, install: bool) -> Result<Attached, Unstarted> {
    let mut client = Client::new(args, mux, inbox, parts);
    client.hear_kept()?;
    let want = Want { args, engine, install, runtime: &|| Ok(args.runtime.clone()), events };
    match runtime::find_or_start(&want, &mut client) {
        Outcome::Ready(Up { state, port, runtime, started }) => Ok(Attached { how: How::Process(runtime, port), state, reattached: !started }),
        other => Err(client.unstarted(other)),
    }
}

/// Stop the runtime recorded in the state folder without attaching to it (on
/// a cluster, cancel its job, or the job waiting for a node): the app's Stop
/// for a host it only browsed. Without the start lock, for a runtime this helper
/// may not stop, or for one that is still starting, nothing is stopped: why.
fn stop_recorded(args: &Args, inbox: &mut Inbox, events: &Sender<Event>) -> Result<(), String> {
    let dir = &args.state_dir;
    let _starting = lock_stop(dir, inbox, standalone::stop_lock_limit())?;
    match args.launcher {
        Launcher::Process => match runtime::end(dir, args.any_node, stopped::How::Connection, events) {
            Ended::NotRunning | Ended::Stopped(_) => {}
            Ended::Elsewhere(node) => return Err(format!("Julia was not stopped. {}", runtime::other_node_text(&node))),
            Ended::Alive(_) => return Err(STILL_RUNNING.into()),
            Ended::Starting => return Err(runtime::STILL_STARTING.into()),
        },
        Launcher::Slurm => slurm::cancel_recorded(dir),
    }
    Ok(())
}

/// What a client that finds the runtime gone is told, by how it was stopped.
fn stopped_text(how: stopped::How) -> &'static str {
    match how {
        stopped::How::Connection => "It was stopped from another connection.",
        stopped::How::Stop => "It was stopped with `endeavor stop`.",
    }
}

/// What runs from the state folder, found without taking it over.
fn check(dir: &Path, launcher: Launcher, any_node: bool) -> RuntimeState {
    if launcher == Launcher::Slurm {
        return slurm::check(dir);
    }
    match runtime::look(dir, any_node, true) {
        runtime::Looked::NotRunning | runtime::Looked::Dead(_) | runtime::Looked::Silent(_) => RuntimeState::NotRunning,
        runtime::Looked::OtherNode(state) | runtime::Looked::Older(state) => RuntimeState::Running { node: state.node, notebooks: None, job: None },
        runtime::Looked::Running(state, port) => RuntimeState::Running { notebooks: open_notebooks(port, &state.token), node: state.node, job: None },
    }
}

/// How many notebooks the runtime has open, from its `list_notebooks` tool.
fn open_notebooks(port: u16, token: &str) -> Option<u32> {
    let params = json!({ "name": "list_notebooks", "arguments": {} });
    let reply = bridge_rpc(port, token, "tools/call", params).ok()?;
    let text = reply["result"]["content"][0]["text"].as_str()?;
    let list: Value = serde_json::from_str(text).ok()?;
    list.as_array().map(|a| a.len() as u32)
}

/// A runtime process this helper watches.
struct Runtime {
    pid: i32,
    /// As in `State`: what `kill` checks the pid against before ending it.
    #[cfg(windows)]
    started: Option<u64>,
    exit: Arc<Exit>,
    state_dir: PathBuf,
}

impl Runtime {
    /// The runtime this helper just started as `child`.
    fn child(child: Child, state_dir: &Path, events: &Sender<Event>) -> Runtime {
        let pid = child.id() as i32;
        #[cfg(windows)]
        let started = winproc::start_time(std::os::windows::io::AsRawHandle::as_raw_handle(&child));
        Runtime {
            pid,
            #[cfg(windows)]
            started,
            exit: Exit::watch_child(child, pid, events.clone()),
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// The runtime `state` records, which some earlier helper started.
    fn recorded(state: &State, state_dir: &Path, events: &Sender<Event>) -> Runtime {
        let exit = Exit::watch_pid(state.pid, state.started, events.clone());
        Runtime {
            pid: state.pid,
            #[cfg(windows)]
            started: state.started,
            exit,
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// It exited: clean up after it and say so, and if another connection
    /// stopped it, say that instead of how it exited.
    fn died(&self, status: String) -> (String, Vec<String>) {
        let status = stopped::why(&self.state_dir, stopped::Of::Runtime(self.pid)).map_or(status, |how| stopped_text(how).into());
        let log_tail = log_tail(&self.state_dir.join("runtime.log"));
        // Its notebook workers are no use without it.
        stop_workers(self.pid);
        remove_state(&self.state_dir, self.pid);
        (status, log_tail)
    }

    /// Ask the runtime to shut down (when it's up and from this build), then insist.
    fn stop(&self, state: Option<&State>) {
        if let Some(state) = state
            && let Some(port) = state.port
        {
            let _ = bridge_call(port, CALL, &state.token, "endeavor/shutdown");
            self.exit.wait(Duration::from_secs(10));
        }
        self.kill();
    }

    #[cfg(unix)]
    fn kill(&self) {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            if self.exit.status().is_some() {
                break;
            }
            signal_group(self.pid, signal);
            self.exit.wait(Duration::from_secs(5));
        }
        stop_workers(self.pid);
        // A runtime that survived stays on record for the clients that can still reach it.
        if !pid_alive(self.pid, None) {
            remove_state(&self.state_dir, self.pid);
        }
    }

    /// End the core, which ends its Job Object, and with it Julia and its workers.
    #[cfg(windows)]
    fn kill(&self) {
        if self.exit.status().is_none() {
            match winproc::Process::open(self.pid, self.started) {
                Some(core) => core.terminate(),
                None => eprintln!("endeavor: the runtime (pid {}) is gone or isn't the one recorded; not stopping it", self.pid),
            }
            self.exit.wait(Duration::from_secs(5));
        }
        remove_state(&self.state_dir, self.pid);
    }
}

/// The runtime was started with setsid, so its pid is also its process group's.
#[cfg(unix)]
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
#[cfg(unix)]
fn stop_workers(pid: i32) {
    // SAFETY: plain syscall.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
}

/// Nothing to do on Windows: the core's Job Object ends Julia's workers when
/// the core ends.
#[cfg(windows)]
fn stop_workers(_pid: i32) {}

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
    fn watch_pid(pid: i32, started: Option<u64>, events: Sender<Event>) -> Arc<Exit> {
        let exit = Arc::new(Exit::default());
        let e = exit.clone();
        std::thread::spawn(move || {
            while pid_alive(pid, started) {
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

#[cfg(unix)]
fn pid_alive(pid: i32, _started: Option<u64>) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Windows reuses pids quickly, so the process must also have started when
/// the record says.
#[cfg(windows)]
fn pid_alive(pid: i32, started: Option<u64>) -> bool {
    winproc::Process::open(pid, started).is_some_and(|process| process.alive())
}

/// End the runtime recorded with `pid` and `started` (runtime.json), with
/// Julia and its workers, unless that pid now belongs to another process: the
/// app's Repair runtime, where Unix ends the process group instead.
#[cfg(windows)]
pub fn end_recorded_runtime(pid: i32, started: Option<u64>) {
    if let Some(core) = winproc::Process::open(pid, started) {
        core.terminate();
        core.wait(5_000);
    }
}

#[cfg(unix)]
fn try_lock(file: &File) -> bool {
    // SAFETY: plain syscall on a file we hold open.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(windows)]
fn try_lock(file: &File) -> bool {
    file.try_lock().is_ok()
}

/// Block SIGUSR1 in every thread (call before any starts), so it can't end
/// the process: the helper of an older build sends it to the one named in `DIR/lock`.
fn block_sigusr1() {
    // SAFETY: initializing and applying a signal set on this (still only) thread.
    #[cfg(unix)]
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Read frames from the app: streams go where the attached runtime is (its
/// ports here, or the relay on its job's node), file requests are answered on
/// their own threads with `answer`, other control messages and the end of input
/// become events.
fn relay_stdin(mux: Arc<Mux>, routes: Routes, events: Sender<Event>, answer: Answer, parts: Parts) {
    std::thread::spawn(move || {
        // SAFETY: fd 0 is our stdin; only this thread reads it.
        #[cfg(unix)]
        let stdin = unsafe { File::from_raw_fd(0) };
        // SAFETY: our stdin handle; only this thread reads it.
        #[cfg(windows)]
        let stdin: File = unsafe { std::os::windows::io::FromRawHandle::from_raw_handle(std::os::windows::io::AsRawHandle::as_raw_handle(&std::io::stdin())) };
        let mut stdin = BufReader::new(stdin);
        loop {
            let frame = match Frame::read_from(&mut stdin) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("endeavor: reading from the app: {e}");
                    break;
                }
            };
            let Some(frame) = mux.take(frame) else { continue };
            match frame {
                Frame::Control(json) => match serde_json::from_slice::<ToHelper>(&json) {
                    Ok(ToHelper::Files { id, request }) => {
                        let (mux, answer) = (mux.clone(), answer.clone());
                        std::thread::spawn(move || drop(mux.send(&ToApp::Files { id, reply: answer(&request) }.frame())));
                    }
                    Ok(message) => drop(events.send(Event::App(message))),
                    Err(e) => eprintln!("endeavor: ignoring control message: {e}"),
                },
                Frame::Open { id } => {
                    let route = routes.read().unwrap().clone();
                    match route {
                        Route::Local(port) => dial(&mux, id, port),
                        Route::Node(link) => {
                            if link.open(id).is_err() {
                                let _ = mux.send(&Frame::Close { id });
                            }
                        }
                        Route::None => drop(mux.send(&Frame::Close { id })),
                    }
                }
                frame => {
                    if let Route::Node(link) = &*routes.read().unwrap() {
                        link.forward(frame);
                    }
                }
            }
        }
        mux.close_all();
        parts.discard();
        let _ = events.send(Event::Eof);
    });
}

/// The parts of files the app is sending (`wire::files::write`), so the ones
/// it never finished go when it does.
#[derive(Clone, Default)]
struct Parts(Arc<Mutex<std::collections::HashSet<PathBuf>>>);

impl Parts {
    /// Answer a piece of a file with `write`, noting its part while unfinished.
    fn track(&self, folder: &str, path: &str, offset: u64, last: bool, write: impl FnOnce() -> wire::files::Reply) -> wire::files::Reply {
        let Ok((part, _)) = wire::files::upload_paths(&wire::files::expand(folder), path) else { return write() };
        let mut parts = self.0.lock().unwrap();
        if offset == 0 {
            parts.insert(part.clone());
        }
        let reply = write();
        if last || matches!(reply, wire::files::Reply::Error { .. }) {
            parts.remove(&part);
        }
        reply
    }

    fn discard(&self) {
        for part in self.0.lock().unwrap().drain() {
            let _ = std::fs::remove_file(part);
        }
    }
}

fn dial(mux: &Arc<Mux>, id: u32, port: u16) {
    match TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)) {
        Ok(socket) => {
            let _ = socket.set_nodelay(true);
            let _ = mux.attach(id, socket);
        }
        Err(_) => {
            let _ = mux.send(&Frame::Close { id });
        }
    }
}

fn read_state(dir: &Path) -> Option<State> {
    parse_state(&serde_json::from_str(&std::fs::read_to_string(dir.join("runtime.json")).ok()?).ok()?)
}

fn parse_state(v: &Value) -> Option<State> {
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    Some(State {
        launcher: text("launcher")?,
        node: text("node")?,
        pid: v["pid"].as_i64().and_then(|p| i32::try_from(p).ok())?,
        started: v["started"].as_u64(),
        port: v["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
        token: text("token")?,
        job: text("job").filter(|j| !j.is_empty()),
        build: text("build"),
        folder: text("folder"),
        exits_when_idle: v["exits_when_idle"].as_bool(),
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
    let token = random_hex::<32>()?;
    let mut file = owner_only(OpenOptions::new().write(true).create(true).truncate(true))
        .open(&path)
        .map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
    file.write_all(token.as_bytes()).map_err(|e| e.to_string())?;
    Ok(token)
}

/// `N` random bytes, in hex.
pub(crate) fn random_hex<const N: usize>() -> Result<String, String> {
    let mut bytes = [0u8; N];
    #[cfg(unix)]
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("/dev/urandom: {e}"))?;
    #[cfg(windows)]
    getrandom::fill(&mut bytes).map_err(|e| format!("Couldn't get random bytes: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `endeavor core`, which starts `julia boot.jl` (see core).
fn runtime_command(julia: &str, runtime: &Path, depot: &str, token: &str, state_dir: &Path, launcher: &str, build: Option<&str>) -> Result<Command, String> {
    let _ = std::fs::remove_file(state_dir.join("runtime.json"));
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find the helper itself: {e}"))?;
    let mut command = Command::new(exe);
    // `ps` shows `endeavor core` (`endeavor --helper core` from the app), not the binary's path.
    #[cfg(unix)]
    command.arg0("endeavor");
    command.args(HELPER_ARGS.get().copied().unwrap_or_default());
    command
        .arg("core")
        .arg("--state-dir")
        .arg(state_dir)
        .args(["--julia", julia])
        .arg("--runtime")
        .arg(runtime)
        .args(["--depot", depot])
        // Not argv, which `ps` shows to every user.
        .env("ENDEAVOR_TOKEN", token)
        .env("ENDEAVOR_LAUNCHER", launcher)
        .stdin(Stdio::null());
    if let Some(build) = build {
        command.env("ENDEAVOR_BUILD", build);
    }
    Ok(command)
}

/// Start the runtime detached from us (its own session, no terminal, stdin from
/// /dev/null), logging to `runtime.log`.
#[cfg(unix)]
fn start(args: &Args, runtime: &Path, julia: &str, token: &str) -> Result<Child, String> {
    let dir = &args.state_dir;
    let log_path = dir.join("runtime.log");
    // The log shows Pluto's secret URL.
    let log = owner_only(OpenOptions::new().append(true).create(true))
        .open(&log_path)
        .and_then(|f| f.set_len(0).map(|_| f))
        .map_err(|e| format!("Couldn't open {}: {e}", log_path.display()))?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = runtime_command(julia, runtime, &args.depot, token, dir, "process", args.build.as_deref())?;
    command.stdout(log).stderr(stderr).envs(args.core_env.iter().map(|(k, v)| (k, v)));
    // SAFETY: setsid and sigprocmask are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            // Not the signals the starting process blocked for itself (`standalone` blocks Ctrl-C's).
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    command.spawn().map_err(|e| format!("Couldn't start the runtime: {e}"))
}

/// Start the runtime detached from us (its own process group, a console of its
/// own with no window, stdin from NUL), logging to `runtime.log`. The core
/// then puts itself in a Job Object that takes Julia and its workers along
/// when it ends (see core).
#[cfg(windows)]
fn start(args: &Args, runtime: &Path, julia: &str, token: &str) -> Result<Child, String> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    let dir = &args.state_dir;
    let log_path = dir.join("runtime.log");
    let log = owner_only(OpenOptions::new().append(true).create(true))
        .open(&log_path)
        .and_then(|f| f.set_len(0).map(|_| f))
        .map_err(|e| format!("Couldn't open {}: {e}", log_path.display()))?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = runtime_command(julia, runtime, &args.depot, token, dir, "process", args.build.as_deref())?;
    command.stdout(log).stderr(stderr).envs(args.core_env.iter().map(|(k, v)| (k, v)));
    // Julia and Pluto's workers are console programs: with no console to
    // share, each would open a console window. CREATE_NO_WINDOW gives the core
    // one without a window, which they inherit. Breaking away from a job the
    // app runs in lets the runtime outlive the app, as it does on Unix; a job
    // that doesn't allow that refuses it, and then the runtime ends with the app.
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    match command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            eprintln!("endeavor: the runtime can't leave the job this helper runs in, so it ends with the app");
            command.creation_flags(flags).spawn()
        }
        result => result,
    }
    .map_err(|e| format!("Couldn't start the runtime: {e}"))
}

/// Send the runtime's log lines as `Progress` until it's ready (then what's
/// been written so far) or gone.
fn follow_log(path: PathBuf, mux: Arc<Mux>, ready: Arc<AtomicBool>, exit: Arc<Exit>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut log = runtime::Log::new(path);
        loop {
            let last_pass = ready.load(Ordering::SeqCst) || exit.status().is_some();
            log.drain(&mut |line| drop(mux.send(&ToApp::Progress { line }.frame())));
            if last_pass {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    })
}

/// The end of the runtime's log, secrets masked.
fn log_tail(path: &Path) -> Vec<String> {
    let text = log_end(path);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..].iter().map(|l| redact_secret(l)).collect()
}

/// The last 64 KB of the log at `path`, empty if it can't be read.
fn log_end(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Mask Pluto's `secret=…` URL token.
fn redact_secret(line: &str) -> String {
    let Some(start) = line.find("secret=").map(|i| i + "secret=".len()) else { return line.to_string() };
    let end = line[start..].find(|c: char| !c.is_ascii_alphanumeric()).map_or(line.len(), |i| start + i);
    format!("{}…{}", &line[..start], &line[end..])
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into `buf`.
    unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(windows)]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// The app's calls on the runtime's port.
const CALL: &str = "/endeavor/call";

/// POST one JSON-RPC call to the runtime's calls; its reply.
fn bridge_rpc(port: u16, token: &str, method: &str, params: Value) -> std::io::Result<Value> {
    let mut socket = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(2))?;
    socket.set_read_timeout(Some(Duration::from_secs(5)))?;
    socket.set_write_timeout(Some(Duration::from_secs(3)))?;
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    write!(
        socket,
        "POST {CALL} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    // HTTP/1.0: the body runs to the end of the connection.
    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    let (_, payload) = response.split_once("\r\n\r\n").ok_or(std::io::ErrorKind::InvalidData)?;
    serde_json::from_str(payload).map_err(|_| std::io::ErrorKind::InvalidData.into())
}

/// How long `bridge_call` has, connecting, sending and reading the response's head, all told.
const BRIDGE_CALL_WAIT: Duration = Duration::from_secs(5);

/// POST one JSON-RPC call to `path` on the loopback server at `port`; its HTTP status. A server that
/// answers slowly, or a little at a time, does not keep it past `BRIDGE_CALL_WAIT`.
fn bridge_call(port: u16, path: &str, token: &str, method: &str) -> std::io::Result<u16> {
    bridge_call_within(port, path, token, method, BRIDGE_CALL_WAIT)
}

fn bridge_call_within(port: u16, path: &str, token: &str, method: &str, wait: Duration) -> std::io::Result<u16> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": {} }).to_string();
    let bearer = format!("Bearer {token}");
    let headers = [("Authorization", bearer.as_str()), ("Content-Type", "application/json")];
    http::post_status_by(port, path, &headers, body.as_bytes(), std::time::Instant::now() + wait)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_gives_up_on_a_server_that_answers_a_little_at_a_time() {
        use std::time::Instant;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            // A byte every 50 ms, never a newline: no single read ever times out.
            while !stop.load(Ordering::SeqCst) {
                if socket.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let started = Instant::now();
        let result = bridge_call_within(port, CALL, "t", "ping", Duration::from_millis(400));
        let took = started.elapsed();
        done.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert!(result.is_err(), "{result:?}");
        assert!(took >= Duration::from_millis(350) && took < Duration::from_secs(3), "{took:?}");
    }

    #[test]
    fn redacts_pluto_secret() {
        let line = "│ Go to http://localhost:58250/?secret=wwD760ru in your browser";
        assert_eq!(redact_secret(line), "│ Go to http://localhost:58250/?secret=… in your browser");
        assert_eq!(redact_secret("no token here"), "no token here");
    }

    #[test]
    fn only_unfinished_parts_are_discarded() {
        let folder = std::env::temp_dir().join(format!("endeavor-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(folder.join("data")).unwrap();
        let parts = Parts::default();
        let f = folder.display().to_string();
        let send = |path: &str, offset: u64, bytes: &[u8], last: bool| {
            let request = wire::files::Request::Write { folder: f.clone(), path: path.into(), offset, bytes: bytes.to_vec(), last };
            parts.track(&f, path, offset, last, || wire::files::answer(&request))
        };
        assert_eq!(send("data/done.csv", 0, b"a", false), wire::files::Reply::Written);
        assert_eq!(send("data/done.csv", 1, b"b", true), wire::files::Reply::Written);
        assert_eq!(send("data/open.csv", 0, b"a", false), wire::files::Reply::Written);
        assert_eq!(send("data/failed.csv", 0, b"a", false), wire::files::Reply::Written);
        assert!(matches!(send("data/failed.csv", 5, b"b", false), wire::files::Reply::Error { .. }));
        let open = folder.join("data/.open.csv.part");
        assert_eq!(parts.0.lock().unwrap().iter().collect::<Vec<_>>(), [&open.canonicalize().unwrap()]);
        parts.discard();
        let left: Vec<_> = std::fs::read_dir(folder.join("data")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["done.csv"]);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn parses_connect_arguments() {
        let args = |s: &str| parse_args(s.split(' ').map(String::from).collect());
        let a = args("connect --state-dir /s --julia /j --runtime /r --depot /d: --quit-with-client").unwrap();
        assert_eq!((a.state_dir, a.julia, a.depot.as_str()), (PathBuf::from("/s"), julia::Source::Path("/j".into()), "/d:"));
        assert!(a.quit_with_client && !a.any_node && a.launcher == Launcher::Process);
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d").unwrap().julia, julia::Source::Auto);
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --launcher slurm").unwrap().launcher, Launcher::Slurm);
        assert!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --launcher pbs").is_err());
        let shell = parse_args(["connect", "--julia-shell", "module load julia", "--state-dir", "/s", "--runtime", "/r", "--depot", "/d"].map(String::from).to_vec());
        assert_eq!(shell.unwrap().julia, julia::Source::Shell("module load julia".into()));
        assert!(args("connect --state-dir /s --julia /j --julia-shell x --runtime /r --depot /d").is_err());
        assert!(args("connect --state-dir /s").is_err());
        assert!(args("serve --state-dir /s --julia /j --runtime /r --depot /d").is_err());
        assert!(args("connect --state-dir /s --julia /j --runtime /r --depot /d --bogus").is_err());
    }
}
