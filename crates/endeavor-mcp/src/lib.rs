#![cfg_attr(test, allow(clippy::disallowed_methods))]

mod askpass;
mod asks;
pub mod client;
mod core;
mod guard;
mod guide;
mod host_tools;
mod http;
pub mod julia;
mod juliaup;
pub mod r;
mod mcp;
mod notebooks;
mod old_builds;
pub mod paths;
mod release;
mod results;
mod runtime;
mod slurm;
mod standalone;
mod stopped;
mod update;
mod uv;
mod views;
#[cfg(unix)]
mod unixproc;
#[cfg(windows)]
mod winproc;

pub use wire;
pub use core::{INTERFACE as CORE_INTERFACE, serve_unreachable};
pub use guard::{AskRestart, Picture, Pictures, REQUEST_RESTART, RestartAnswer, Snapshot, restart_schema, serve_guarded};
pub use mcp::{NOTEBOOK_TOOLS_JSON, asks_first, changes_notebook, is_tool, runs_code};
pub use standalone::{Lease, lease, unpack};
pub use release::release_helper;

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

const USAGE: &str = "usage: endeavor connect [--state-dir DIR] (--julia JULIA|auto|own | --julia-shell LINE) [--julia-when-needed] [--r RSCRIPT|auto | --r-shell LINE] --runtime RUNTIME_DIR --depot DEPOT [--launcher process|slurm|auto] [--quit-with-client | --own-with-client] [--any-node] [--exit-idle] [--exit-idle-after-client] [--build BUILD]
                        (--state-dir defaults to the folder `serve` and `mcp` use; with --launcher slurm, to one for the cluster;
                         auto is slurm where Slurm's sinfo is, else process)
       endeavor relay --state-dir DIR
       endeavor node-start --state-dir DIR --julia JULIA [--julia-when-needed] [--r RSCRIPT|auto | --r-shell LINE] --runtime RUNTIME_DIR --depot DEPOT [--build BUILD]
       endeavor core --state-dir DIR (--julia JULIA|auto|own | --julia-shell LINE) [--install-julia] [--julia-when-needed] [--r RSCRIPT|auto | --r-shell LINE] [--own-r [--install-r]] [--lifeline FD] --runtime RUNTIME_DIR --depot DEPOT
       endeavor askpass PROMPT
       endeavor serve|mcp|stop [OPTIONS]   (without the app; `endeavor serve --help`)
       endeavor update                      replace this binary with the newest build (Linux, macOS, Windows)
       endeavor --version";
const LOG_TAIL: usize = 40;

struct Args {
    state_dir: PathBuf,
    unnamed: Option<(PathBuf, Option<PathBuf>)>,
    julia: julia::Source,
    julia_when_needed: bool,
    r: r::Source,
    own_r: bool,
    runtime: PathBuf,
    depot: String,
    ember: Option<PathBuf>,
    launcher: Launcher,
    quit_with_client: bool,
    own_with_client: AtomicBool,
    any_node: bool,
    build: Option<String>,
    exit_idle: bool,
    exit_idle_after_client: bool,
    core_env: Vec<(&'static str, Option<String>)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Launcher {
    Process,
    Slurm,
}

impl Launcher {
    fn here() -> Launcher {
        if wire::slurm::has("sinfo") { Launcher::Slurm } else { Launcher::Process }
    }

    fn word(self) -> &'static str {
        match self {
            Launcher::Process => "process",
            Launcher::Slurm => "slurm",
        }
    }
}

struct State {
    launcher: String,
    node: String,
    pid: i32,
    started: Option<u64>,
    boot: Option<String>,
    port: Option<u16>,
    token: String,
    page_token: Option<String>,
    job: Option<String>,
    build: Option<String>,
    interface: Option<u32>,
    folder: Option<String>,
    no_folder: bool,
    exits_when_idle: Option<bool>,
    network: Option<serde_json::Map<String, Value>>,
}

impl State {
    fn usable_as_is(&self) -> bool {
        usable_as_is(self.build.as_deref(), self.interface)
    }
}

pub fn usable_as_is(build: Option<&str>, interface: Option<u32>) -> bool {
    interface == Some(core::INTERFACE) || build == Some(embedded::BUILD_VERSION)
}

pub(crate) fn which_build(build: Option<&str>) -> String {
    build.map_or("an earlier build".to_owned(), |build| format!("build {build}"))
}

pub(crate) fn which_version(theirs: Option<u32>, ours: Option<u32>) -> &'static str {
    match (theirs, ours) {
        (None, Some(_)) => "an older",
        (Some(theirs), Some(ours)) if theirs < ours => "an older",
        (Some(theirs), Some(ours)) if theirs > ours => "a newer",
        _ => "another",
    }
}

pub(crate) fn other_version_text(place: &str, machine: &str, interface: Option<u32>, open: &str) -> String {
    format!(
        "Endeavor on {place} was started by {} version, and it keeps running as it is.{open} Some notebook tools may not work as described, or may be refused. If that happens, don't stop it yourself: tell the user, and ask whether to restart it, which closes the notebooks open in it. Only if the user says yes, call `stop_machine` with machine \"{machine}\", then `use_machine` with machine \"{machine}\" to start this version.",
        which_version(interface, Some(core::INTERFACE))
    )
}

pub(crate) fn no_r_text(place: &str, restart: &str) -> String {
    format!("unsupported::Endeavor on {place} was started by an older version, which has no R notebooks, so it can't make or open this one. This version has them; the older one keeps running until it is restarted. {restart}")
}

pub const OLDER_RUNTIME: &str = "Julia here was started by an older version of Endeavor, which this version can't connect to. Restart Julia to use it.";

enum Event {
    App(ToHelper),
    Eof,
    Exited(i32, String),
    Node(u64, ToApp),
    NodeGone(u64),
}

#[derive(Clone)]
enum Route {
    None,
    Local(u16),
    Node(Arc<slurm::Link>),
}

type Routes = Arc<RwLock<Route>>;

type Answer = Arc<dyn Fn(&wire::files::Request) -> wire::files::Reply + Send + Sync>;

static HELPER_ARGS: OnceLock<&'static [&'static str]> = OnceLock::new();

static LIFELINE: Mutex<Option<std::io::PipeWriter>> = Mutex::new(None);

fn keep_runtime() {
    if let Some(mut lifeline) = LIFELINE.lock().unwrap().take() {
        let _ = lifeline.write_all(b"k");
    }
}

fn own_with_client(args: &Args, on: bool) {
    args.own_with_client.store(on, Ordering::SeqCst);
    if !on {
        keep_runtime();
    }
}

pub fn run(argv: Vec<String>) -> ! {
    run_as(&[], argv)
}

pub fn run_as(helper_args: &'static [&'static str], argv: Vec<String>) -> ! {
    let _ = HELPER_ARGS.set(helper_args);
    let place = old_builds::Place::here(helper_args);
    old_builds::hold_own_folder(place.as_ref());
    if matches!(argv.first().map(String::as_str), Some("connect" | "mcp" | "serve")) {
        old_builds::tidy(place);
    }
    match argv.first().map(String::as_str) {
        Some("askpass") => askpass::run(argv.get(1).map_or("", String::as_str)),
        Some("relay") => slurm::relay_main(&argv[1..]),
        Some("node-start") => slurm::node_start_main(&argv[1..]),
        Some("core") => core::main(&argv[1..]),
        Some("serve" | "mcp" | "stop" | "status" | "open") => standalone::main(&argv),
        Some("hook") => standalone::claude_mode::hook_main(),
        Some("--version" | "-V" | "version") => update::print_version(),
        Some("update") => update::main(&argv[1..]),
        Some(prompt) if prompt != "connect" && [wire::askpass::SOCKET_ENV, wire::askpass::ADDRESS_ENV].iter().any(|v| std::env::var_os(v).is_some()) => askpass::run(prompt),
        _ => {}
    }
    let mut args = match parse_args(argv) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    block_sigusr1();
    let mux = stdout_mux();
    let Err(message) = serve(&mut args, &mux);
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
    let (mut state_dir, mut julia, mut runtime, mut depot, mut r) = (None, None::<julia::Source>, None, None, None);
    let (mut quit_with_client, mut own_with_client, mut any_node, mut launcher, mut build, mut exit_idle, mut julia_when_needed) = (false, false, false, Launcher::Process, None, false, false);
    let (mut own_r, mut exit_idle_after_client) = (false, false);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--state-dir" => state_dir = Some(PathBuf::from(value()?)),
            "--julia" | "--julia-shell" if julia.is_some() => return Err("give one of --julia and --julia-shell".into()),
            "--julia" => julia = Some(value().map(julia::Source::from_value)?),
            "--julia-shell" => julia = Some(julia::Source::Shell(value()?)),
            "--julia-when-needed" => julia_when_needed = true,
            "--r" | "--r-shell" if r.is_some() => return Err("give one of --r and --r-shell".into()),
            "--r" | "--r-shell" => r = Some(r::Source::from_flag(&arg, value()?)),
            "--runtime" => runtime = Some(PathBuf::from(value()?)),
            "--depot" => depot = Some(value()?),
            "--launcher" => {
                launcher = match value()?.as_str() {
                    "process" => Launcher::Process,
                    "slurm" => Launcher::Slurm,
                    "auto" => Launcher::here(),
                    other => return Err(format!("unknown launcher {other}")),
                }
            }
            "--quit-with-client" | "--own-with-client" if quit_with_client || own_with_client => return Err("give one of --quit-with-client and --own-with-client".into()),
            "--quit-with-client" => quit_with_client = true,
            "--own-with-client" => own_with_client = true,
            "--any-node" => any_node = true,
            "--exit-idle" => exit_idle = true,
            "--exit-idle-after-client" => exit_idle_after_client = true,
            "--own-r" => own_r = true,
            "--build" => build = Some(value()?),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let runtime: PathBuf = runtime.ok_or("--runtime is required")?;
    let (depot, ember) = match depot.ok_or("--depot is required")? {
        depot if depot.is_empty() => {
            let folders = paths::server_folders(wire::slurm::scratch().as_deref(), runtime.parent().and_then(Path::parent).unwrap_or(&runtime));
            (folders.depot, Some(folders.ember))
        }
        depot => (depot, None),
    };
    let env = paths::Env::here();
    let unnamed = (state_dir.is_none() && launcher == Launcher::Process).then(|| (env.state_dir(), env.apps_old_state_dir()));
    Ok(Args {
        state_dir: state_dir.unwrap_or_else(|| match &unnamed {
            Some((dir, older)) => runtime::or_older(dir, older.as_deref()),
            None => env.cluster_state_dir(),
        }),
        unnamed,
        julia: julia.ok_or("--julia or --julia-shell is required")?,
        julia_when_needed,
        r: r.unwrap_or_default(),
        runtime,
        depot,
        ember,
        launcher,
        quit_with_client,
        own_with_client: own_with_client.into(),
        any_node,
        build,
        own_r,
        exit_idle,
        exit_idle_after_client,
        core_env: if exit_idle { vec![("ENDEAVOR_EXIT_IDLE", Some("1".into()))] } else { Vec::new() },
    })
}

fn make_state_dir(dir: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))
}

fn owner_only(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    options.mode(0o600);
    options
}

fn serve(args: &mut Args, mux: &Arc<Mux>) -> Result<std::convert::Infallible, String> {
    make_state_dir(&args.state_dir)?;
    let (events, rx) = mpsc::channel();
    let mut inbox = Inbox { rx, later: VecDeque::new() };
    let routes: Routes = Arc::new(RwLock::new(Route::None));
    let parts = Parts::default();
    let attached_dir: Arc<RwLock<Option<PathBuf>>> = Arc::default();
    let answer: Answer = {
        let (attached_dir, named, unnamed, launcher, any_node, parts, julia) = (attached_dir.clone(), args.state_dir.clone(), args.unnamed.clone(), args.launcher, args.any_node, parts.clone(), args.julia.clone());
        Arc::new(move |request| match request {
            wire::files::Request::Julia { download } => match slurm::login_julia(&julia, *download) {
                Ok(julia) => wire::files::Reply::Julia { julia },
                Err(message) => wire::files::Reply::Error { message },
            },
            wire::files::Request::Runtime => {
                let dir = attached_dir.read().unwrap().clone().unwrap_or_else(|| state_dir_now(&unnamed, &named));
                wire::files::Reply::Runtime { runtime: check(&dir, launcher, any_node) }
            }
            wire::files::Request::Write { folder, path, offset, last, .. } => parts.track(folder, path, *offset, *last, || wire::files::answer(request)),
            other => wire::files::answer(other),
        })
    };
    relay_stdin(mux.clone(), routes.clone(), events.clone(), answer, parts.clone());
    let home = wire::files::home().display().to_string();
    let slurm_here = wire::slurm::has("sinfo");
    let hello = ToApp::Hello { protocol: wire::PROTOCOL, version: env!("CARGO_PKG_VERSION").into(), node: hostname(), home, slurm: slurm_here, uploads: true, launcher: args.launcher.word().into() };
    let _ = mux.send(&hello.frame());

    let mut attached: Option<Attached> = None;
    loop {
        *attached_dir.write().unwrap() = attached.as_ref().map(|_| args.state_dir.clone());
        let event = inbox.next();
        match event {
            Event::App(ToHelper::StartRuntime { id, job, engine, install, attach_only }) => {
                if let Some(attached) = &attached {
                    let _ = mux.send(&attached.ready(id, true).frame());
                    continue;
                }
                repick_state_dir(args)?;
                let result = match args.launcher {
                    Launcher::Process => attach(args, mux, &mut inbox, &events, &parts, &engine, install, attach_only),
                    Launcher::Slurm => slurm::attach(args, mux, &mut inbox, &events, &parts, job.unwrap_or_default(), &engine, install, attach_only),
                };
                match result {
                    Ok(now) => {
                        *routes.write().unwrap() = now.route();
                        keep_runtime();
                        if now.reattached {
                            now.take_back();
                        }
                        let _ = mux.send(&now.ready(id, now.reattached).frame());
                        attached = Some(now);
                    }
                    Err(unstarted) => {
                        if let Some((stop, force)) = unstarted.answer(mux, id) {
                            let stopped = stop_recorded(args, &mut inbox, &events, force);
                            answer_stops(mux, &mut inbox, stop, force, &stopped);
                        }
                    }
                }
            }
            Event::App(ToHelper::Stop { unless_shared: true, .. })
                if attached.as_ref().is_some_and(|a| matches!(a.how, How::Process(..)) && a.left_to_plugin()) =>
            {
                parts.discard();
                keep_runtime();
                std::process::exit(0)
            }
            Event::App(ToHelper::Stop { id, force, .. }) => {
                let stopped = match attached.take() {
                    Some(runtime) => runtime.stop(args, &routes, &mut inbox, standalone::stop_lock_limit()).map_err(|failed| {
                        let (runtime, why) = *failed;
                        attached = Some(runtime);
                        why
                    }),
                    None => repick_state_dir(args).and_then(|()| stop_recorded(args, &mut inbox, &events, force)),
                };
                answer_stops(mux, &mut inbox, id, force, &stopped);
            }
            Event::Eof if args.quit_with_client || (args.own_with_client.load(Ordering::SeqCst) && args.launcher == Launcher::Process && attached.as_ref().is_some_and(|a| !a.reattached)) => {
                if let Some(runtime) = attached.take()
                    && let Err(failed) = runtime.stop(args, &routes, &mut inbox, standalone::start_lock_limit())
                {
                    eprintln!("endeavor: {}", failed.1);
                }
                std::process::exit(0);
            }
            Event::App(ToHelper::Detach) => {
                parts.discard();
                keep_runtime();
                left_running(args, attached, &routes, &mut inbox)
            }
            Event::Eof => left_running(args, attached, &routes, &mut inbox),
            Event::App(ToHelper::OwnWithClient { on }) => own_with_client(args, on),
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

fn left_running(args: &Args, attached: Option<Attached>, routes: &Routes, inbox: &mut Inbox) -> ! {
    if args.exit_idle_after_client
        && let Some(attached) = attached
        && matches!(attached.how, How::Process(..))
    {
        if attached.state.interface.is_none_or(|interface| interface < END_WHEN_IDLE_SINCE) {
            if !attached.left_to_plugin()
                && let Err(failed) = attached.stop(args, routes, inbox, standalone::start_lock_limit())
            {
                eprintln!("endeavor: {}", failed.1);
            }
        } else if let Err(e) = attached.end_when_idle() {
            eprintln!("endeavor: couldn't have the runtime end when idle: {e}");
        }
    }
    std::process::exit(0)
}

pub(crate) const CROP_SINCE: u32 = 47;

const END_WHEN_IDLE_SINCE: u32 = 46;

fn state_dir_now(unnamed: &Option<(PathBuf, Option<PathBuf>)>, named: &Path) -> PathBuf {
    match unnamed {
        Some((dir, older)) => runtime::or_older(dir, older.as_deref()),
        None => named.to_path_buf(),
    }
}

fn repick_state_dir(args: &mut Args) -> Result<(), String> {
    let now = state_dir_now(&args.unnamed, &args.state_dir);
    if now != args.state_dir {
        make_state_dir(&now)?;
        args.state_dir = now;
    }
    Ok(())
}

fn answer_stops(mux: &Arc<Mux>, inbox: &mut Inbox, id: u32, force: bool, stopped: &Result<(), String>) {
    for id in std::iter::once(id).chain(inbox.take_stops(force)) {
        let reply = match stopped {
            Ok(()) => ToApp::Stopped { id },
            Err(message) => ToApp::NotStopped { id, message: message.clone() },
        };
        let _ = mux.send(&reply.frame());
    }
}

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

struct Attached {
    how: How,
    state: State,
    reattached: bool,
    quitting: Arc<Mutex<bool>>,
}

enum How {
    Process(Runtime, u16),
    Slurm(slurm::Running),
}

impl Attached {
    fn left_to_plugin(&self) -> bool {
        let How::Process(_, port) = &self.how else { return false };
        *self.quitting.lock().unwrap() = true;
        bridge_rpc(*port, &self.state.token, "endeavor/leave_to_plugin", json!({})).is_ok_and(|reply| reply["result"]["left"] == true)
    }

    fn end_when_idle(&self) -> std::io::Result<()> {
        let How::Process(_, port) = &self.how else { return Ok(()) };
        *self.quitting.lock().unwrap() = true;
        bridge_rpc(*port, &self.state.token, "endeavor/end_when_idle", json!({})).map(drop)
    }

    fn take_back(&self) {
        let How::Process(_, port) = &self.how else { return };
        let (port, token, quitting) = (*port, self.state.token.clone(), self.quitting.clone());
        std::thread::spawn(move || take_back_with(&|| bridge_rpc(port, &token, "endeavor/keep_for_app", json!({})).map(drop), &TAKE_BACK_WAITS, &quitting));
    }

    fn ready(&self, id: u32, reattached: bool) -> ToApp {
        let state = &self.state;
        ToApp::Ready {
            id,
            launcher: state.launcher.clone(),
            node: state.node.clone(),
            pid: state.pid as u32,
            token: state.token.clone(),
            page_token: state.page_token.clone(),
            reattached,
            job: match &self.how {
                How::Process(..) => None,
                How::Slurm(job) => Some(job.info()),
            },
            port: match &self.how {
                How::Process(_, port) => Some(*port),
                How::Slurm(_) => state.port,
            },
            build: state.build.clone(),
            interface: state.interface,
        }
    }

    fn route(&self) -> Route {
        match &self.how {
            How::Process(_, port) => Route::Local(*port),
            How::Slurm(job) => Route::Node(job.link()),
        }
    }

    fn stop(self, args: &Args, routes: &Routes, inbox: &mut Inbox, limit: Duration) -> Result<(), Box<(Attached, String)>> {
        let _starting = match lock_stop(&args.state_dir, inbox, limit) {
            Ok(lock) => lock,
            Err(why) => return Err(Box::new((self, why))),
        };
        *routes.write().unwrap() = Route::None;
        match &self.how {
            How::Process(runtime, _) => {
                if !stop_marked(&args.state_dir, Some(&self.state), runtime, stopped::How::Connection) {
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

const STILL_RUNNING: &str = "Endeavor was not stopped: it is still running.";

fn stop_marked(dir: &Path, state: Option<&State>, runtime: &Runtime, how: stopped::How) -> bool {
    let of = stopped::Of::Runtime(runtime.pid);
    stopped::mark(dir, of, how);
    runtime.stop(state);
    let gone = !runtime.is_it();
    if !gone {
        stopped::unmark(dir, of);
    }
    gone
}

struct Inbox {
    rx: mpsc::Receiver<Event>,
    later: VecDeque<Event>,
}

enum Heard {
    Quiet,
    Stop(u32, bool),
    Detach,
    Eof,
    Event,
    OwnWithClient(bool),
}

impl Inbox {
    fn next(&mut self) -> Event {
        self.later.pop_front().unwrap_or_else(|| self.rx.recv().expect("senders live as long as their threads"))
    }

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
                let _ = mux.send(&ToApp::StartFailed { id, message: "Endeavor is already starting.".into() }.frame());
                Heard::Quiet
            }
            Event::App(ToHelper::Stop { id, force, .. }) => Heard::Stop(id, force),
            Event::App(ToHelper::Detach) => Heard::Detach,
            Event::App(ToHelper::OwnWithClient { on }) => Heard::OwnWithClient(on),
            Event::Eof => Heard::Eof,
            _ => Heard::Event,
        }
    }

    fn defer(&mut self, event: Event) {
        let app = |event: &Event| matches!(event, Event::App(_) | Event::Eof);
        let at = if app(&event) { self.later.len() } else { self.later.iter().position(app).unwrap_or(self.later.len()) };
        self.later.insert(at, event);
    }

    fn take_stops(&mut self, force: bool) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut at = 0;
        while let Some(event) = self.later.get(at) {
            match event {
                Event::App(ToHelper::Stop { force: true, .. }) if !force => break,
                Event::App(ToHelper::Stop { id, .. }) => {
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

enum Unstarted {
    Failed(String),
    NeedsInstall(Vec<wire::Item>),
    Died { status: String, log_tail: Vec<String> },
    Stopped { id: u32, force: bool, other: bool },
    NotRunning,
}

impl Unstarted {
    fn answer(self, mux: &Arc<Mux>, start: u32) -> Option<(u32, bool)> {
        if let Unstarted::Stopped { id, force, other: true } = self {
            let _ = mux.send(&ToApp::StartCancelled { id: start }.frame());
            return Some((id, force));
        }
        let (reply, stop) = match self {
            Unstarted::Failed(message) => (ToApp::StartFailed { id: start, message }, None),
            Unstarted::NeedsInstall(items) => (ToApp::NeedsInstall { id: start, items }, None),
            Unstarted::Died { status, log_tail } => (ToApp::StartDied { id: start, status, log_tail }, None),
            Unstarted::Stopped { id, .. } => (ToApp::StartCancelled { id: start }, Some(id)),
            Unstarted::NotRunning => (ToApp::NotRunning { id: start }, None),
        };
        let _ = mux.send(&reply.frame());
        if let Some(id) = stop {
            let _ = mux.send(&ToApp::Stopped { id }.frame());
        }
        None
    }
}

struct Client<'a> {
    args: &'a Args,
    mux: &'a Arc<Mux>,
    inbox: &'a mut Inbox,
    parts: &'a Parts,
    told: bool,
    stop: Option<(u32, bool, bool)>,
}

impl<'a> Client<'a> {
    fn new(args: &'a Args, mux: &'a Arc<Mux>, inbox: &'a mut Inbox, parts: &'a Parts) -> Client<'a> {
        Client { args, mux, inbox, parts, told: false, stop: None }
    }

    fn hear_kept(&mut self) -> Result<(), Unstarted> {
        while !self.inbox.later.is_empty() {
            if !self.hear(Duration::ZERO, Waiting::Lock) {
                return Err(self.cancelled());
            }
        }
        Ok(())
    }

    fn hear(&mut self, wait: Duration, waiting: Waiting) -> bool {
        match self.inbox.hear_while_starting(self.mux, wait) {
            Heard::Stop(id, force) => {
                self.stop = Some((id, force, waiting != Waiting::Ready));
                return false;
            }
            Heard::Detach => {
                self.parts.discard();
                keep_runtime();
                std::process::exit(0)
            }
            Heard::Eof if self.args.quit_with_client && waiting != Waiting::Lock => return false,
            Heard::Eof if self.args.own_with_client.load(Ordering::SeqCst) && waiting == Waiting::Ready => return false,
            Heard::Eof => std::process::exit(0),
            Heard::OwnWithClient(on) => own_with_client(self.args, on),
            Heard::Quiet | Heard::Event => {}
        }
        true
    }

    fn cancelled(&self) -> Unstarted {
        match self.stop {
            Some((id, force, other)) => Unstarted::Stopped { id, force, other },
            None => std::process::exit(0),
        }
    }

    fn unstarted(&self, outcome: Outcome) -> Unstarted {
        match outcome {
            Outcome::Failed(message) => Unstarted::Failed(message),
            Outcome::NeedsInstall(items) => Unstarted::NeedsInstall(items),
            Outcome::Died { status, log_tail } => Unstarted::Died { status, log_tail },
            Outcome::Unusable(Looked::OtherNode(state)) => Unstarted::Failed(runtime::other_node_text(&state.node)),
            Outcome::Unusable(_) => Unstarted::Failed(OLDER_RUNTIME.into()),
            Outcome::Cancelled => self.cancelled(),
            Outcome::NothingRunning => Unstarted::NotRunning,
            Outcome::Ready(_) => unreachable!("a runtime is not an unstarted one"),
        }
    }

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
        if matches!(waiting, Waiting::Lock | Waiting::Other) && !self.told {
            self.progress("Another connection is starting Julia here; waiting for it.".into());
            self.told = true;
        }
        self.hear(wait, waiting)
    }
}

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

fn attach(args: &Args, mux: &Arc<Mux>, inbox: &mut Inbox, events: &Sender<Event>, parts: &Parts, engine: &str, install: bool, attach_only: bool) -> Result<Attached, Unstarted> {
    let mut client = Client::new(args, mux, inbox, parts);
    client.hear_kept()?;
    let want = Want { args, engine, install, runtime: &|| Ok(args.runtime.clone()), events, attach_only, replace_older: !attach_only };
    match runtime::find_or_start(&want, &mut client) {
        Outcome::Ready(Up { state, port, runtime, started }) => Ok(Attached { how: How::Process(runtime, port), state, reattached: !started, quitting: Default::default() }),
        other => Err(client.unstarted(other)),
    }
}

fn stop_recorded(args: &Args, inbox: &mut Inbox, events: &Sender<Event>, force: bool) -> Result<(), String> {
    let dir = &args.state_dir;
    let _starting = lock_stop(dir, inbox, standalone::stop_lock_limit())?;
    match args.launcher {
        Launcher::Process => match runtime::end(dir, args.any_node, stopped::How::Connection, force, events) {
            Ended::NotRunning | Ended::Stopped(_) | Ended::Cancelled(_) => {}
            Ended::Elsewhere(node) => return Err(format!("Endeavor was not stopped. {}", runtime::other_node_text(&node))),
            Ended::Alive(_) => return Err(STILL_RUNNING.into()),
            Ended::Starting => return Err(runtime::STILL_STARTING.into()),
            Ended::Unidentified => return Err(runtime::START_UNIDENTIFIED.into()),
        },
        Launcher::Slurm => slurm::cancel_recorded(dir),
    }
    Ok(())
}

fn stopped_text(how: stopped::How) -> &'static str {
    match how {
        stopped::How::Connection => "It was stopped from another connection.",
        stopped::How::Stop => "It was stopped with `endeavor stop`.",
    }
}

fn check(dir: &Path, launcher: Launcher, any_node: bool) -> RuntimeState {
    if launcher == Launcher::Slurm {
        return slurm::check(dir);
    }
    let starting = runtime::lock_state(dir).is_held();
    match runtime::look(dir, any_node, true) {
        runtime::Looked::NotRunning | runtime::Looked::Dead(_) | runtime::Looked::Silent(_) if starting => RuntimeState::Starting,
        runtime::Looked::NotRunning | runtime::Looked::Dead(_) | runtime::Looked::Silent(_) => RuntimeState::NotRunning,
        runtime::Looked::OtherNode(state) | runtime::Looked::Older(state) => RuntimeState::Running { node: state.node, notebooks: None, job: None },
        runtime::Looked::Running(state, port) => RuntimeState::Running { notebooks: open_notebooks(port, &state.token), node: state.node, job: None },
    }
}

fn open_notebooks(port: u16, token: &str) -> Option<u32> {
    let params = json!({ "name": "list_notebooks", "arguments": {} });
    let reply = bridge_rpc(port, token, "tools/call", params).ok()?;
    let text = reply["result"]["content"][0]["text"].as_str()?;
    let list: Value = serde_json::from_str(text).ok()?;
    list.as_array().map(|a| a.len() as u32)
}

fn julia_removed(state: &State, port: u16) -> bool {
    if state.node != hostname() {
        return false;
    }
    let Ok(reply) = bridge_rpc(port, &state.token, "endeavor/julia_status", json!({})) else { return false };
    reply["result"]["julia"]["path"].as_str().is_some_and(|path| !Path::new(path).exists())
}

struct Runtime {
    pid: i32,
    started: Option<u64>,
    boot: Option<String>,
    exit: Arc<Exit>,
    state_dir: PathBuf,
}

impl Runtime {
    fn child(child: Child, state_dir: &Path, events: &Sender<Event>) -> Runtime {
        let pid = child.id() as i32;
        #[cfg(windows)]
        let started = winproc::start_time(std::os::windows::io::AsRawHandle::as_raw_handle(&child));
        #[cfg(unix)]
        let started = unixproc::start_time(pid).at();
        Runtime {
            pid,
            started,
            boot: own_boot(),
            exit: Exit::watch_child(child, pid, events.clone()),
            state_dir: state_dir.to_path_buf(),
        }
    }

    fn recorded(state: &State, state_dir: &Path, events: &Sender<Event>) -> Runtime {
        Runtime::of(state.pid, state.started, state.boot.clone(), state_dir, events)
    }

    fn of(pid: i32, started: Option<u64>, boot: Option<String>, state_dir: &Path, events: &Sender<Event>) -> Runtime {
        let exit = Exit::watch_pid(pid, started, boot.clone(), events.clone());
        Runtime { pid, started, boot, exit, state_dir: state_dir.to_path_buf() }
    }

    fn died(&self, status: String) -> (String, Vec<String>) {
        let status = stopped::why(&self.state_dir, stopped::Of::Runtime(self.pid)).map_or(status, |how| stopped_text(how).into());
        let log_tail = log_tail(&self.state_dir.join("runtime.log"));
        stop_workers(self.pid, self.started, self.boot.as_deref());
        remove_state(&self.state_dir, self.pid, self.started);
        (status, log_tail)
    }

    fn is_it(&self) -> bool {
        pid_alive(self.pid, self.started, self.boot.as_deref())
    }

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
            if self.exit.status().is_some() || !self.is_it() {
                break;
            }
            if !unkillable_for_tests() {
                signal_group(self.pid, signal);
            }
            self.exit.wait(Duration::from_secs(5));
        }
        stop_workers(self.pid, self.started, self.boot.as_deref());
        if !self.is_it() {
            remove_state(&self.state_dir, self.pid, self.started);
        }
    }

    #[cfg(windows)]
    fn kill(&self) {
        if self.exit.status().is_none() {
            match winproc::Process::open(self.pid, self.started) {
                Some(core) => core.terminate(),
                None => eprintln!("endeavor: the runtime (pid {}) is gone or isn't the one recorded; not stopping it", self.pid),
            }
            self.exit.wait(Duration::from_secs(5));
        }
        remove_state(&self.state_dir, self.pid, self.started);
    }
}

#[cfg(unix)]
fn unkillable_for_tests() -> bool {
    cfg!(debug_assertions) && std::env::var_os("ENDEAVOR_TEST_UNKILLABLE").is_some()
}

#[cfg(unix)]
fn signal_group(pid: i32, signal: i32) {
    // SAFETY: plain syscalls; a group or process that's gone only returns ESRCH.
    unsafe {
        libc::kill(-pid, signal);
        libc::kill(pid, signal);
    }
}

#[cfg(unix)]
fn stop_workers(pid: i32, started: Option<u64>, boot: Option<&str>) -> bool {
    if pid <= 0 || pid_alive(pid, None, None) || !unixproc::this_boot(started, boot) {
        return false;
    }
    let left = || unixproc::group_running(pid);
    if !left() {
        return false;
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        // SAFETY: plain syscall.
        unsafe { libc::kill(-pid, signal) };
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while left() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
        if !left() {
            break;
        }
    }
    true
}

#[cfg(windows)]
fn stop_workers(_pid: i32, _started: Option<u64>, _boot: Option<&str>) -> bool {
    false
}

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

    fn watch_pid(pid: i32, started: Option<u64>, boot: Option<String>, events: Sender<Event>) -> Arc<Exit> {
        let exit = Arc::new(Exit::default());
        let e = exit.clone();
        std::thread::spawn(move || {
            while pid_alive(pid, started, boot.as_deref()) {
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
fn pid_alive(pid: i32, started: Option<u64>, boot: Option<&str>) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let exists = pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
    if !exists || !unixproc::same_boot(boot) {
        return false;
    }
    match (started, unixproc::start_time(pid)) {
        (Some(started), unixproc::Start::At(now)) => now == started,
        (_, unixproc::Start::Gone) => false,
        _ => true,
    }
}

#[cfg(windows)]
fn pid_alive(pid: i32, started: Option<u64>, _boot: Option<&str>) -> bool {
    winproc::Process::open(pid, started).is_some_and(|process| process.alive())
}

fn own_start_time() -> Option<u64> {
    #[cfg(unix)]
    return unixproc::start_time(std::process::id() as i32).at();
    #[cfg(windows)]
    return winproc::own_start_time();
}

fn own_boot() -> Option<String> {
    #[cfg(unix)]
    return unixproc::boot_id();
    #[cfg(windows)]
    return None;
}

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

#[derive(Clone, Default)]
struct Parts(Arc<Mutex<std::collections::HashSet<PathBuf>>>);

impl Parts {
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

fn set_core_env(command: &mut Command, env: &[(&'static str, Option<String>)]) {
    for (name, value) in env {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
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
        boot: text("boot"),
        port: v["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
        token: text("token")?,
        page_token: text("page_token"),
        job: text("job").filter(|j| !j.is_empty()),
        build: text("build"),
        interface: v["interface"].as_u64().and_then(|n| u32::try_from(n).ok()),
        folder: text("folder"),
        no_folder: v["no_folder"].as_bool().unwrap_or(false),
        exits_when_idle: v["exits_when_idle"].as_bool(),
        network: v["network"].as_object().cloned(),
    })
}

pub(crate) const NETWORK_VARS: [&str; 11] = ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy", "NO_PROXY", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "CURL_CA_BUNDLE"];

pub(crate) fn network_settings() -> serde_json::Map<String, Value> {
    use sha2::{Digest, Sha256};
    NETWORK_VARS
        .iter()
        .filter_map(|name| {
            let value = std::env::var_os(name)?;
            let digest = Sha256::digest(value.as_encoded_bytes());
            Some((name.to_string(), Value::from(digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())))
        })
        .collect()
}

fn remove_state(dir: &Path, pid: i32, started: Option<u64>) {
    let path = dir.join("runtime.json");
    let current = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let same = |v: &Value| v["pid"].as_i64() == Some(pid as i64) && started.is_none_or(|started| v["started"].as_u64().is_none_or(|recorded| recorded == started));
    if current.is_none_or(|v| same(&v)) {
        let _ = std::fs::remove_file(path);
    }
}

fn token(dir: &Path) -> Result<String, String> {
    kept_token(dir, "token")
}

pub(crate) fn page_token(dir: &Path) -> Result<String, String> {
    kept_token(dir, "page_token")
}

fn kept_token(dir: &Path, name: &str) -> Result<String, String> {
    let path = dir.join(name);
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

pub(crate) fn random_hex<const N: usize>() -> Result<String, String> {
    let mut bytes = [0u8; N];
    #[cfg(unix)]
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).map_err(|e| format!("/dev/urandom: {e}"))?;
    #[cfg(windows)]
    getrandom::fill(&mut bytes).map_err(|e| format!("Couldn't get random bytes: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn julia_flags(args: &Args, found: Option<&str>, install: bool) -> Vec<String> {
    let mut flags: Vec<String> = match (found, &args.julia) {
        (Some(path), _) => vec!["--julia".into(), path.into()],
        (None, julia::Source::Path(path)) => vec!["--julia".into(), path.clone()],
        (None, julia::Source::Auto) => vec!["--julia".into(), "auto".into()],
        (None, julia::Source::Own) => vec!["--julia".into(), "own".into()],
        (None, julia::Source::Shell(line)) => vec!["--julia-shell".into(), line.clone()],
    };
    if found.is_none() && install {
        flags.push("--install-julia".into());
    }
    if args.julia_when_needed {
        flags.push("--julia-when-needed".into());
    }
    flags
}

fn runtime_command(julia: &[String], runtime: &Path, depot: &str, ember: Option<&Path>, token: &str, state_dir: &Path, launcher: &str, build: Option<&str>) -> Result<Command, String> {
    let _ = std::fs::remove_file(state_dir.join("runtime.json"));
    let mut command = Command::new(this_program()?);
    #[cfg(unix)]
    command.arg0("endeavor");
    command.args(HELPER_ARGS.get().copied().unwrap_or_default());
    command
        .arg("core")
        .arg("--state-dir")
        .arg(state_dir)
        .args(julia)
        .arg("--runtime")
        .arg(runtime)
        .args(["--depot", depot])
        .args(ember.map(|ember| [Path::new("--ember"), ember]).into_iter().flatten())
        .env("ENDEAVOR_TOKEN", token)
        .env("ENDEAVOR_LAUNCHER", launcher)
        .stdin(Stdio::null());
    if let Some(build) = build {
        command.env("ENDEAVOR_BUILD", build);
    }
    Ok(command)
}

#[cfg(target_os = "linux")]
pub(crate) fn this_program() -> Result<PathBuf, String> {
    Ok(PathBuf::from("/proc/self/exe"))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn this_program() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Couldn't find the helper itself: {e}"))?;
    if HELPER_ARGS.get().is_some_and(|args| !args.is_empty()) {
        return Ok(exe);
    }
    let ours = update::version_line();
    let mut command = Command::new(&exe);
    client::no_window(&mut command);
    let theirs = command.arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().map(|out| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_owned());
    match theirs {
        Ok(theirs) if theirs == ours => Ok(exe),
        Ok(theirs) => Err(format!(
            "{} was replaced after this endeavor started (it is now {}; this is {ours}), so Julia was not started. Start this endeavor again, or reconnect the agent's MCP server, to use the new one.",
            exe.display(),
            if theirs.is_empty() { "a program that doesn't say its version".to_owned() } else { format!("`{theirs}`") }
        )),
        Err(e) => Err(format!("Couldn't run {} to start Julia: {e}. If endeavor was updated or moved, start it again.", exe.display())),
    }
}

fn open_log(path: &Path) -> Result<(File, File), String> {
    let mut options = OpenOptions::new();
    if cfg!(windows) {
        options.write(true);
    } else {
        options.append(true);
    }
    let log = owner_only(options.create(true)).open(path).map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    log.set_len(0).map_err(|e| format!("Couldn't empty {}: {e}", path.display()))?;
    let stderr = log.try_clone().map_err(|e| e.to_string())?;
    Ok((log, stderr))
}

#[cfg(unix)]
fn start(args: &Args, runtime: &Path, julia: &[String], token: &str) -> Result<Child, String> {
    let dir = &args.state_dir;
    let (log, stderr) = open_log(&dir.join("runtime.log"))?;
    let mut command = runtime_command(julia, runtime, &args.depot, args.ember.as_deref(), token, dir, "process", args.build.as_deref())?;
    command.args(args.r.args()).args(args.own_r.then_some("--own-r")).stdout(log).stderr(stderr);
    set_core_env(&mut command, &args.core_env);
    let lifeline = match args.own_with_client.load(Ordering::SeqCst) {
        true => Some(std::io::pipe().map_err(|e| format!("Couldn't start the runtime: {e}"))?),
        false => None,
    };
    let kept = lifeline.as_ref().map(|(reader, _)| reader.as_raw_fd());
    if let Some(fd) = kept {
        command.arg("--lifeline").arg(fd.to_string());
    }
    // SAFETY: setsid, sigprocmask and fcntl are async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if let Some(fd) = kept {
                libc::fcntl(fd, libc::F_SETFD, 0);
            }
            libc::setsid();
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
            Ok(())
        });
    }
    let child = command.spawn().map_err(|e| format!("Couldn't start the runtime: {e}"))?;
    if let Some((_, writer)) = lifeline {
        *LIFELINE.lock().unwrap() = Some(writer);
    }
    Ok(child)
}

#[cfg(windows)]
fn start(args: &Args, runtime: &Path, julia: &[String], token: &str) -> Result<Child, String> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    let dir = &args.state_dir;
    let (log, stderr) = open_log(&dir.join("runtime.log"))?;
    let mut command = runtime_command(julia, runtime, &args.depot, args.ember.as_deref(), token, dir, "process", args.build.as_deref())?;
    command.args(args.r.args()).args(args.own_r.then_some("--own-r")).stdout(log).stderr(stderr);
    set_core_env(&mut command, &args.core_env);
    not_inherited_std_handles();
    let lifeline = match args.own_with_client.load(Ordering::SeqCst) {
        true => Some(std::io::pipe().map_err(|e| format!("Couldn't start the runtime: {e}"))?),
        false => None,
    };
    if let Some((reader, writer)) = &lifeline {
        inherited(writer.as_raw_handle(), false);
        inherited(reader.as_raw_handle(), true);
        command.arg("--lifeline").arg((reader.as_raw_handle() as usize).to_string());
    }
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    let child = match command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            eprintln!("endeavor: the runtime can't leave the job this helper runs in, so it ends with the app");
            command.creation_flags(flags).spawn()
        }
        result => result,
    }
    .map_err(|e| format!("Couldn't start the runtime: {e}"));
    if let Some((reader, writer)) = lifeline {
        drop(reader);
        if child.is_ok() {
            *LIFELINE.lock().unwrap() = Some(writer);
        }
    }
    child
}

#[cfg(windows)]
fn inherited(handle: std::os::windows::io::RawHandle, on: bool) {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    // SAFETY: a handle this process holds; only the flag changes.
    unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, if on { HANDLE_FLAG_INHERIT } else { 0 }) };
}

#[cfg(windows)]
fn not_inherited_std_handles() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle returns a handle we hold, null or INVALID_HANDLE_VALUE; only the flag changes.
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

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

fn log_tail(path: &Path) -> Vec<String> {
    let text = log_end(path);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..].iter().map(|l| redact_secret(l)).collect()
}

fn log_end(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else { return String::new() };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

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
    use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: the call writes at most `len` UTF-16 units into `buf` and sets
    // `len` to the number written, without the terminating zero.
    if unsafe { GetComputerNameExW(ComputerNameDnsHostname, buf.as_mut_ptr(), &mut len) } == 0 {
        return std::env::var("COMPUTERNAME").unwrap_or_default();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

pub(crate) fn is_this_node(node: &str) -> bool {
    let here = hostname();
    if cfg!(windows) { node.eq_ignore_ascii_case(&here) } else { node == here }
}

const CALL: &str = "/endeavor/call";

const TAKE_BACK_WAITS: [Duration; 5] = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)];

fn take_back_with(call: &dyn Fn() -> std::io::Result<()>, waits: &[Duration], quitting: &Mutex<bool>) -> bool {
    let mut waits = waits.iter();
    loop {
        let tried = {
            let quitting = quitting.lock().unwrap();
            if *quitting {
                return false;
            }
            call()
        };
        match tried {
            Ok(()) => return true,
            Err(e) => match waits.next() {
                Some(wait) => std::thread::sleep(*wait),
                None => {
                    eprintln!("[ Warning: Couldn't take the runtime back from the plugin: {e}");
                    return false;
                }
            },
        }
    }
}

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
    let mut response = String::new();
    socket.read_to_string(&mut response)?;
    let (_, payload) = response.split_once("\r\n\r\n").ok_or(std::io::ErrorKind::InvalidData)?;
    serde_json::from_str(payload).map_err(|_| std::io::ErrorKind::InvalidData.into())
}

const BRIDGE_CALL_WAIT: Duration = Duration::from_secs(5);

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
#[path = "../tests/common/exec.rs"]
pub(crate) mod test_exec;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taking_the_runtime_back_is_tried_again_until_it_answers() {
        let tries = std::cell::Cell::new(0);
        let fails_twice = || {
            tries.set(tries.get() + 1);
            if tries.get() < 3 { Err(std::io::ErrorKind::ConnectionRefused.into()) } else { Ok(()) }
        };
        assert!(take_back_with(&fails_twice, &[Duration::ZERO; 5], &Mutex::new(false)));
        assert_eq!(tries.get(), 3, "it stops once it answers");
        tries.set(0);
        let never = || {
            tries.set(tries.get() + 1);
            Err(std::io::ErrorKind::ConnectionRefused.into())
        };
        assert!(!take_back_with(&never, &[Duration::ZERO; 5], &Mutex::new(false)));
        assert_eq!(tries.get(), 6, "one try, then one after each wait");
    }

    #[test]
    fn no_take_back_lands_once_the_app_is_quitting() {
        let quitting = Arc::new(Mutex::new(false));
        let (started, in_try) = (Arc::new(std::sync::Barrier::new(2)), Arc::new(AtomicBool::new(false)));
        let tries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retrying = {
            let (quitting, started, in_try, tries) = (quitting.clone(), started.clone(), in_try.clone(), tries.clone());
            std::thread::spawn(move || {
                let call = || {
                    if tries.fetch_add(1, Ordering::SeqCst) == 0 {
                        in_try.store(true, Ordering::SeqCst);
                        started.wait();
                        std::thread::sleep(Duration::from_millis(200));
                        in_try.store(false, Ordering::SeqCst);
                    }
                    Err(std::io::ErrorKind::ConnectionRefused.into())
                };
                take_back_with(&call, &[Duration::from_millis(10); 5], &quitting)
            })
        };
        started.wait();
        *quitting.lock().unwrap() = true;
        assert!(!in_try.load(Ordering::SeqCst), "the try under way ended before the quit went on");
        assert!(!retrying.join().unwrap());
        assert_eq!(tries.load(Ordering::SeqCst), 1, "no try after the quit");
        assert!(!take_back_with(&|| Ok(()), &[], &Mutex::new(true)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_this_computer_as_julia_does() {
        let said = std::process::Command::new("hostname").output().unwrap();
        assert_eq!(hostname(), String::from_utf8_lossy(&said.stdout).trim());
    }

    #[test]
    fn the_runtime_log_is_emptied_and_both_handles_write_it() {
        let path = client::scratch("runtime-log").join("runtime.log");
        std::fs::write(&path, "the last runtime's log\n").unwrap();
        let (mut out, mut err) = open_log(&path).unwrap();
        out.write_all(b"out\n").unwrap();
        err.write_all(b"err\n").unwrap();
        drop((out, err));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "out\nerr\n");
    }

    #[test]
    fn a_call_gives_up_on_a_server_that_answers_a_little_at_a_time() {
        use std::time::Instant;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
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
        assert!(!a.own_with_client.load(Ordering::SeqCst));
        assert!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --own-with-client").unwrap().own_with_client.load(Ordering::SeqCst));
        assert!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --own-with-client --quit-with-client").is_err());
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d").unwrap().julia, julia::Source::Auto);
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d").unwrap().r, r::Source::Auto);
        assert_eq!(args("connect --state-dir /s --julia auto --r /opt/R/bin/Rscript --runtime /r --depot /d").unwrap().r, r::Source::Path("/opt/R/bin/Rscript".into()));
        assert!(args("connect --state-dir /s --julia auto --r auto --r-shell x --runtime /r --depot /d").is_err());
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --launcher slurm").unwrap().launcher, Launcher::Slurm);
        assert!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --launcher pbs").is_err());
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d --launcher auto").unwrap().launcher, Launcher::here(), "auto is settled as the helper starts");
        let shell = parse_args(["connect", "--julia-shell", "module load julia", "--state-dir", "/s", "--runtime", "/r", "--depot", "/d"].map(String::from).to_vec());
        assert_eq!(shell.unwrap().julia, julia::Source::Shell("module load julia".into()));
        assert!(args("connect --state-dir /s --julia /j --julia-shell x --runtime /r --depot /d").is_err());
        assert!(args("connect --state-dir /s").is_err());
        let defaulted = parse_args(["connect", "--state-dir", "/s", "--julia", "auto", "--runtime", "/c/v1/runtime", "--depot", ""].map(String::from).to_vec()).unwrap();
        let folders = paths::server_folders(wire::slurm::scratch().as_deref(), Path::new("/c"));
        assert_eq!((defaulted.depot, defaulted.ember), (folders.depot, Some(folders.ember)), "an empty depot takes the server's rule, and Ember goes beside it");
        assert_eq!(args("connect --state-dir /s --julia auto --runtime /r --depot /d").unwrap().ember, None, "a depot the client names leaves Ember where it was");
        assert!(args("serve --state-dir /s --julia /j --runtime /r --depot /d").is_err());
        assert!(args("connect --state-dir /s --julia /j --runtime /r --depot /d --bogus").is_err());
    }
}
