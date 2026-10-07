//! The link process (`endeavor link --machine ID`, see `link`): the supervisor
//! that connects, starts the runtime and gets the connection back, and the
//! control interface the fronts call.

use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wire::files::{Reply, Request, RuntimeState};
use wire::slurm::JobRequest;

use super::{FoundInfo, HelloInfo, InstallInfo, JobInfo, QueueInfo, Record, RuntimeInfo, State, Status, valid_id};
use crate::client::{Auth, CLOSED, Cancel, Channel, ConnectError, Event, Listener, Messages, Notice, Options, Server, StartError, StartOptions, Transport, connect, start, this_platform};
use crate::http::{self, Framing, Head};
use crate::standalone::Env;

/// The link ends this long after the last control request.
const IDLE: Duration = Duration::from_secs(8 * 3600);

/// How long a reconnect waits after its first failure, doubling up to `RETRY_LAST`.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LAST: Duration = Duration::from_secs(30);

/// A connection that stays lost this long is given up on (state `failed`).
const RETRY_GIVE_UP: Duration = Duration::from_secs(10 * 60);

/// How often `reattach` asks the helper whether the runtime is there, and how long it waits between.
const REATTACH_TRIES: u32 = 3;
const REATTACH_PAUSE: Duration = Duration::from_secs(1);

/// The largest body a control request may have.
const MAX_BODY: u64 = 64 * 1024;

/// What the supervisor is told.
enum Msg {
    /// A start request while it waits to connect, or after a failure.
    Kick,
    /// The helper of this connection ended by itself.
    Closed(u64),
    /// A runtime start on this connection ended (`Shared::epoch` when it began).
    Started(u64, u64, Result<crate::client::Runtime, StartError>),
    /// The attached runtime went away.
    Notice(u64, Notice),
}

/// What a start asked for.
#[derive(Clone, Debug, Default)]
struct Wish {
    /// On a cluster, what to submit.
    job: Option<JobRequest>,
    /// The helper may install what this start needs: the `install` of the
    /// request, and not what was agreed for an earlier one. Never for a start
    /// that only attaches.
    install: bool,
}

struct Inner {
    id: String,
    name: String,
    state: State,
    step: Option<String>,
    error: Option<String>,
    hello: Option<HelloInfo>,
    runtime: Option<RuntimeInfo>,
    job: Option<JobInfo>,
    queue: Option<QueueInfo>,
    /// The connection, while there is one, and its number.
    channel: Option<Arc<Channel>>,
    conn: u64,
    /// What the runtime was asked for, from a start until a stop or a failure.
    wanted: Option<Wish>,
    /// What an attached or starting runtime had been asked for when the
    /// connection was lost: the next connection gets it back, if it is still there.
    resume: Option<Wish>,
    /// Why the runtime had ended when the connection was lost, which the next
    /// connection still says: it has nothing to bring back.
    ended: Option<String>,
    /// A start is under way on this connection.
    starting: bool,
    /// Counts the stops, so the end of a start that one cut short is ignored.
    /// A stop counts before it asks the helper, which takes a while.
    epoch: u64,
    /// The supervisor is connecting, which a start request needn't wake it for.
    connecting: bool,
    /// A `Kick` is in the supervisor's inbox.
    kick_pending: bool,
    /// The runtime went away before the end of its start was handled.
    gone_early: bool,
    /// The next start only attaches to a runtime that is there.
    check_first: bool,
    /// The last such start found nothing (`Status::nothing_running`).
    nothing_running: bool,
    /// What the machine needs installed, while the state is `needs_install`.
    needs: Option<InstallInfo>,
}

struct Shared {
    dir: PathBuf,
    token: String,
    inbox: Sender<Msg>,
    listener: Arc<Listener>,
    /// The record the link was started with.
    server: Server,
    inner: Mutex<Inner>,
    /// The connect under way, which a quit cancels.
    cancel: Mutex<Arc<Cancel>>,
    activity: Mutex<Instant>,
    /// Control requests in flight.
    busy: AtomicUsize,
    leaving: AtomicBool,
    /// The user agreed to installing the helper on the machine (`POST /link/start`
    /// with `install`, `/link/install`, or `--install` at the link's start). Kept
    /// for as long as the link runs, so that a reconnect to a machine that lost
    /// the helper doesn't need it again. Julia's download is not part of it: it
    /// is asked for by each start (`Wish::download`).
    allow_install: AtomicBool,
    idle: Duration,
    hooks: Hooks,
}

/// Settings for tests (`link`'s docs).
struct Hooks {
    shell: bool,
    root: String,
    state: String,
    depot: String,
    ask: Option<String>,
}

impl Hooks {
    fn read() -> Hooks {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Hooks { shell: std::env::var_os("ENDEAVOR_LINK_SHELL").is_some(), root: var("ENDEAVOR_LINK_ROOT"), state: var("ENDEAVOR_LINK_STATE"), depot: var("ENDEAVOR_LINK_DEPOT"), ask: std::env::var("ENDEAVOR_LINK_ASK").ok() }
    }
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// `endeavor link --machine ID`.
pub(crate) fn main(argv: &[String]) -> ! {
    let fail = |message: String| -> ! {
        eprintln!("endeavor link: {message}");
        exit(1)
    };
    let (id, install) = match argv {
        [flag, id] if flag == "--machine" => (id.clone(), false),
        [flag, id, install] if flag == "--machine" && install == "--install" => (id.clone(), true),
        _ => fail("usage: endeavor link --machine ID [--install]".into()),
    };
    valid_id(&id).unwrap_or_else(|e| fail(e));
    let env = Env::from_vars(&|name| std::env::var(name).ok());
    let dir = env.links_dir().join(&id);
    crate::make_state_dir(&dir).unwrap_or_else(|e| fail(e));
    let server_path = dir.join(super::SERVER_FILE);
    let server: Server = match std::fs::read_to_string(&server_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| fail(format!("{} isn't readable as the machine's settings ({e}). Call the tool again to start the connection afresh.", server_path.display()))),
        Err(e) => fail(format!("Couldn't read {} ({e}). It holds the machine's settings and is written when the connection is started, so call the tool again to start it.", server_path.display())),
    };
    if server.id != id {
        fail(format!("{} holds the settings of {}, not of {id}. Call the tool again to start the connection afresh.", server_path.display(), server.id));
    }
    if let Some(running) = super::running(&dir, &id)
        && running.pid != std::process::id()
    {
        fail(format!("The link to {id} already runs (pid {}).", running.pid));
    }
    catch_stop_signals();
    let idle = std::env::var("ENDEAVOR_LINK_IDLE_SECS").ok().and_then(|s| s.parse::<f64>().ok()).filter(|secs| *secs > 0.0).and_then(|secs| Duration::try_from_secs_f64(secs).ok()).unwrap_or(IDLE);
    let name = display_name(&server);
    let messages = Messages {
        restart_failed: |name| format!("Julia on {name} couldn't start. Call use_machine to try again."),
        not_connected: |name| format!("Endeavor isn't connected to {name}. Call use_machine to use it again."),
    };
    let listener = Listener::new(&name, None, messages).unwrap_or_else(|e| fail(e));
    let control = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| fail(format!("Couldn't open the control port: {e}")));
    let port = control.local_addr().map(|a| a.port()).unwrap_or_else(|e| fail(e.to_string()));
    let token = crate::random_hex::<32>().unwrap_or_else(|e| fail(e));
    let (inbox, messages_in) = mpsc::channel();
    #[cfg(windows)]
    let started = crate::winproc::own_start_time();
    #[cfg(not(windows))]
    let started: Option<u64> = None;
    let shared = Arc::new(Shared {
        dir: dir.clone(),
        token: token.clone(),
        inbox,
        listener,
        server,
        inner: Mutex::new(Inner {
            id: id.clone(),
            name,
            state: State::Connecting,
            step: None,
            error: None,
            hello: None,
            runtime: None,
            job: None,
            queue: None,
            channel: None,
            conn: 0,
            wanted: None,
            resume: None,
            ended: None,
            starting: false,
            epoch: 0,
            connecting: false,
            kick_pending: false,
            gone_early: false,
            check_first: false,
            nothing_running: false,
            needs: None,
        }),
        cancel: Mutex::new(Arc::new(Cancel::default())),
        activity: Mutex::new(Instant::now()),
        busy: AtomicUsize::new(0),
        leaving: AtomicBool::new(false),
        allow_install: AtomicBool::new(install),
        idle,
        hooks: Hooks::read(),
    });
    let record = Record { machine: id.clone(), pid: std::process::id(), started, port, token, build: crate::embedded::BUILD_VERSION.to_owned(), protocol: super::PROTOCOL };
    let text = serde_json::to_string(&record).unwrap_or_default();
    crate::core::write_private(&dir.join("link.json"), text.as_bytes()).unwrap_or_else(|e| fail(e));
    eprintln!("The link to {id} listens for control on 127.0.0.1:{port}, and relays on 127.0.0.1:{}.", shared.listener.port());

    let serving = shared.clone();
    std::thread::spawn(move || serve_control(serving, control));
    let watching = shared.clone();
    std::thread::spawn(move || watch(watching));
    supervise(shared, messages_in)
}

/// The name the agent knows a machine by.
fn display_name(server: &Server) -> String {
    [&server.name, &server.ssh_host, &server.id].into_iter().find(|n| !n.trim().is_empty()).cloned().unwrap_or_default()
}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        f(&mut self.inner.lock().unwrap())
    }

    fn touch(&self) {
        *self.activity.lock().unwrap() = Instant::now();
    }

    fn status(&self) -> Status {
        self.with(|i| Status {
            machine: i.id.clone(),
            name: i.name.clone(),
            state: i.state,
            step: i.step.clone(),
            error: i.error.clone(),
            hello: i.hello.clone(),
            runtime: i.runtime.clone(),
            job: i.job.clone(),
            queue: i.queue.clone(),
            nothing_running: i.nothing_running,
            needs_install: i.needs.clone(),
            pid: std::process::id(),
            build: crate::embedded::BUILD_VERSION.to_owned(),
        })
    }

    /// Hear what connecting and starting say.
    fn on_event(&self, event: Event) {
        self.with(|i| match event {
            Event::Connected { os, arch } => {
                let hello = i.hello.get_or_insert_with(HelloInfo::default);
                (hello.os, hello.arch) = (Some(os), Some(arch));
                i.step = Some(format!("Signed in to {}", i.name));
            }
            Event::Helper { installed } => {
                i.hello.get_or_insert_with(HelloInfo::default).helper_installed = Some(installed);
                i.step = Some(if installed { "Installed Endeavor's helper".into() } else { "Endeavor's helper is installed".into() });
            }
            Event::Found { name, version, path } => {
                i.step = Some(format!("Found {name} {version} ({path})"));
                if let Some(hello) = &mut i.hello {
                    hello.found.retain(|f| f.name != name);
                    hello.found.push(FoundInfo { name, version, path });
                }
            }
            Event::Progress(line) => i.step = Some(line),
            Event::Submitted { job, summary } => {
                i.step = Some(format!("Submitted job {job} ({summary})"));
                i.job = Some(JobInfo { id: job, summary: Some(summary), ..Default::default() });
            }
            Event::Queued { state, reason } => {
                i.step = Some(format!("The job is {} ({reason})", state.to_lowercase()));
                // The helper reports the node, as `reason`, once the job runs.
                if i.starting {
                    i.state = if state == "RUNNING" { State::Starting } else { State::Queued };
                }
                if state == "RUNNING"
                    && let Some(job) = &mut i.job
                {
                    job.node = Some(reason.clone());
                }
                i.queue = Some(QueueInfo { state, reason });
            }
            Event::Started { node, .. } => i.step = Some(format!("Ready on {node}")),
            Event::Slurm(_) | Event::Finished { .. } => {}
        });
    }

    /// Start the runtime on the connection if one is asked for and none is under
    /// way or attached. Never called with the lock held.
    fn begin_start(self: &Arc<Shared>) {
        let begun = self.with(|i| {
            let (Some(channel), Some(wish)) = (i.channel.clone(), i.wanted.clone()) else { return None };
            if i.starting || i.runtime.is_some() {
                return None;
            }
            i.starting = true;
            i.gone_early = false;
            i.ended = None;
            i.state = State::Starting;
            i.error = None;
            i.needs = None;
            i.queue = None;
            i.step = Some(format!("Starting the runtime on {}", i.name));
            Some((channel, wish, i.conn, i.epoch, std::mem::take(&mut i.check_first)))
        });
        let Some((channel, wish, conn, epoch, check)) = begun else { return };
        let shared = self.clone();
        std::thread::spawn(move || {
            if check && !shared.runtime_is_there(&channel, conn, epoch) {
                return;
            }
            let tx = shared.inbox.clone();
            let options = StartOptions { job: wish.job, install: wish.install, ..StartOptions::default() };
            let result = start(&channel, &shared.listener, &options, &|event| shared.on_event(event), move |notice| drop(tx.send(Msg::Notice(conn, notice))));
            let _ = shared.inbox.send(Msg::Started(conn, epoch, result));
        });
    }

    /// For a start that only attaches: ask the helper whether a runtime runs or a
    /// job waits. If not, end the start with nothing started. False then, and when
    /// the answer was no use.
    fn runtime_is_there(&self, channel: &Channel, conn: u64, epoch: u64) -> bool {
        let answer = channel.files(Request::Runtime);
        let current = |i: &Inner| i.conn == conn && i.epoch == epoch;
        match answer {
            Ok(Reply::Runtime { runtime: RuntimeState::Running { .. } | RuntimeState::Queued { .. } }) => true,
            Ok(Reply::Runtime { runtime: RuntimeState::NotRunning }) => {
                self.with(|i| {
                    if current(i) {
                        i.starting = false;
                        i.wanted = None;
                        i.nothing_running = true;
                        i.state = State::Connected;
                        i.step = Some(format!("No runtime is running on {}", i.name));
                    }
                });
                false
            }
            // The connection went: `Closed` follows and takes the wish along to the next one.
            Ok(_) | Err(_) if channel.is_closed() => false,
            other => {
                let trouble = other.map_or_else(|e| e, |reply| format!("The helper answered {reply:?}."));
                self.with(|i| {
                    if current(i) {
                        i.starting = false;
                        i.wanted = None;
                        i.state = State::Failed;
                        i.error = Some(format!("Endeavor couldn't find out whether Julia on {} is running ({trouble}). Call use_machine to try again.", i.name));
                        i.step = i.error.clone();
                    }
                });
                false
            }
        }
    }

    /// `POST /link/install`: the helper may be installed. A machine that lacks it
    /// has no connection, and is connected again.
    fn request_install(&self) {
        self.allow_install.store(true, Ordering::SeqCst);
        self.with(|i| {
            if i.state == State::NeedsInstall && i.channel.is_none() {
                self.reconnect_now(i);
            }
        });
    }

    fn reconnect_now(&self, i: &mut Inner) {
        (i.state, i.error, i.needs) = (State::Connecting, None, None);
        if !i.connecting && !i.kick_pending {
            i.kick_pending = true;
            let _ = self.inbox.send(Msg::Kick);
        }
    }

    /// `POST /link/start`.
    fn request_start(self: &Arc<Shared>, job: Option<JobRequest>, only_running: bool, install: bool) {
        let connected = self.with(|i| {
            // Only a start with no connection can mean the helper: with one, `install` is for what the start needs, and a
            // yes to that mustn't be kept as one for the helper. Set under the lock, as the supervisor checks it.
            if install && i.channel.is_none() {
                self.allow_install.store(true, Ordering::SeqCst);
            }
            if i.runtime.is_some() || i.starting {
                return true;
            }
            i.wanted = Some(Wish { job, install: install && !only_running });
            i.resume = None;
            i.check_first = only_running;
            i.nothing_running = false;
            if i.channel.is_some() {
                return true;
            }
            // No connection: the supervisor starts the runtime once it has one, and
            // tries now if it was waiting. One wake-up is enough, and none while it connects.
            if matches!(i.state, State::Failed | State::NeedsInstall) {
                (i.state, i.error, i.needs) = (State::Connecting, None, None);
            }
            if !i.connecting && !i.kick_pending {
                i.kick_pending = true;
                let _ = self.inbox.send(Msg::Kick);
            }
            false
        });
        if connected {
            self.begin_start();
        }
    }

    /// `POST /link/stop`.
    fn request_stop(&self) -> Result<(), String> {
        // Counted before the helper is asked: it takes a while, and the end of a start it cuts short is no failure.
        let Some((channel, mine)) = self.with(|i| {
            let channel = i.channel.clone()?;
            i.epoch += 1;
            Some((channel, i.epoch))
        }) else {
            return Err(self.with(|i| not_connected_to_stop(i)));
        };
        let stopped = channel.stop();
        self.with(|i| {
            let same = i.channel.as_ref().is_some_and(|now| Arc::ptr_eq(now, &channel));
            match &stopped {
                Ok(()) => {
                    // A connection that comes later mustn't attach to what was stopped.
                    i.resume = None;
                    if same {
                        i.wanted = None;
                        i.starting = false;
                        i.runtime = None;
                        i.job = None;
                        i.queue = None;
                        i.error = None;
                        i.state = State::Connected;
                        i.step = Some("The runtime was stopped".into());
                    }
                }
                Err(message) => {
                    // The runtime is still there, and the start that was under way goes on.
                    if i.epoch == mine {
                        i.epoch -= 1;
                    }
                    if same && i.starting && i.runtime.is_none() {
                        i.starting = false;
                        i.state = State::Failed;
                        i.error = Some(message.clone());
                    }
                }
            }
        });
        if stopped.is_ok() {
            self.listener.disconnected();
        }
        stopped
    }
}

/// Why a stop can't reach the helper, and what to do about it.
fn not_connected_to_stop(i: &Inner) -> String {
    let name = &i.name;
    match i.state {
        State::NeedsInstall => format!("Endeavor isn't connected to {name}: its helper isn't installed there, and stopping the runtime needs it. Ask the user whether Endeavor may install it, then call `stop_machine` again with `install: true`."),
        State::Failed => format!("Endeavor isn't connected to {name}: {} Tell the user, and call `stop_machine` again once that is fixed.", i.error.as_deref().unwrap_or("the connection failed.")),
        _ => format!("Endeavor isn't connected to {name} yet, so it can't stop the runtime. It is connecting: wait a few seconds, then call `stop_machine` again."),
    }
}

/// The connection and what hangs on it, for good: connects, starts what was asked for, and gets it back.
fn supervise(shared: Arc<Shared>, inbox: Receiver<Msg>) -> ! {
    let mut conn = 0;
    let mut reconnecting = false;
    let mut lost_since: Option<Instant> = None;
    let mut delay = RETRY_FIRST;
    loop {
        conn += 1;
        let name = shared.with(|i| {
            i.conn = conn;
            i.channel = None;
            i.state = State::Connecting;
            i.connecting = true;
            i.kick_pending = false;
            i.needs = None;
            i.step = Some(format!("Connecting to {}", i.name));
            i.name.clone()
        });
        // Wake-ups from before this connect are answered by it.
        while inbox.try_recv().is_ok() {}
        match connect_now(&shared) {
            Err(error) => {
                if shared.leaving.load(Ordering::SeqCst) {
                    park();
                }
                if let Some(helper) = error.needs.clone() {
                    // The permission may have come while this connect looked.
                    if shared.allow_install.load(Ordering::SeqCst) {
                        continue;
                    }
                    eprintln!("The helper isn't installed on {name}; waiting for the user's agreement to install it");
                    let parked = shared.with(|i| {
                        // Checked with the lock held, so that a permission is never lost between the two.
                        if shared.allow_install.load(Ordering::SeqCst) {
                            return false;
                        }
                        i.connecting = false;
                        i.state = State::NeedsInstall;
                        i.error = None;
                        i.wanted = None;
                        i.resume = None;
                        i.step = Some(format!("Endeavor's helper isn't installed on {name}"));
                        i.needs = Some(InstallInfo::helper(helper));
                        true
                    });
                    if !parked {
                        continue;
                    }
                    (reconnecting, lost_since, delay) = (false, None, RETRY_FIRST);
                    shared.listener.disconnected();
                    wait_for(&inbox, Duration::MAX);
                    continue;
                }
                let lost = *lost_since.get_or_insert_with(Instant::now);
                let retry = reconnecting && error.retry && lost.elapsed() < RETRY_GIVE_UP;
                eprintln!("The connection to {name} failed{}: {}", if retry { ", trying again" } else { "" }, error.message);
                shared.with(|i| {
                    i.connecting = false;
                    i.state = if retry { State::Connecting } else { State::Failed };
                    i.error = (!retry).then(|| error.message.clone());
                    i.step = Some(if retry { format!("Lost the connection to {name}: {}", error.message) } else { error.message.clone() });
                });
                if retry {
                    wait_for(&inbox, delay);
                    delay = (delay * 2).min(RETRY_LAST);
                } else {
                    reconnecting = false;
                    lost_since = None;
                    delay = RETRY_FIRST;
                    // The listener has been saying that the connection comes back by itself.
                    shared.listener.disconnected();
                    wait_for(&inbox, Duration::MAX);
                }
            }
            Ok(channel) => {
                lost_since = None;
                delay = RETRY_FIRST;
                let watching = (channel.clone(), shared.inbox.clone());
                std::thread::spawn(move || {
                    if watching.0.closed().is_some() {
                        let _ = watching.1.send(Msg::Closed(conn));
                    }
                });
                let resume = shared.with(|i| {
                    i.connecting = false;
                    i.channel = Some(channel.clone());
                    i.state = State::Connected;
                    i.error = None;
                    i.step = Some(format!("Connected to {name}"));
                    if i.resume.is_none()
                        && let Some(why) = i.ended.take()
                    {
                        i.state = State::Failed;
                        i.error = Some(why);
                    }
                    // A start asked for while the connection was away is the newer wish.
                    i.resume.take().filter(|_| i.wanted.is_none())
                });
                if hello_wants_slurm(&shared) {
                    let (shared, channel) = (shared.clone(), channel.clone());
                    std::thread::spawn(move || find_partitions(&shared, &channel, conn));
                }
                if let Some(wish) = resume {
                    reattach(&shared, &channel, wish);
                }
                reconnecting = true;
                shared.begin_start();
                serve_connection(&shared, &inbox, conn);
            }
        }
    }
}

fn hello_wants_slurm(shared: &Shared) -> bool {
    shared.with(|i| i.hello.as_ref().is_some_and(|h| h.slurm))
}

/// Ask a machine with Slurm about its partitions, for `hello`.
fn find_partitions(shared: &Shared, channel: &Channel, conn: u64) {
    let found = match channel.files(Request::Slurm) {
        Ok(Reply::Slurm { scheduler }) => Some(scheduler),
        // The connection ended: the next one asks again.
        Err(_) if channel.is_closed() => return,
        _ => None,
    };
    shared.with(|i| {
        if let Some(hello) = i.hello.as_mut().filter(|_| i.conn == conn) {
            match found {
                Some(scheduler) => (hello.partitions, hello.scratch) = (Some(scheduler.partitions), scheduler.scratch),
                None => (hello.partitions, hello.partitions_failed) = (Some(Vec::new()), true),
            }
        }
    });
}

/// After a reconnect, attach to the runtime that was asked for only if the
/// helper says it is still there (running, or a job that waits): a start would
/// otherwise begin a new runtime, or on a cluster submit a job nobody asked for.
/// An answer that isn't clear is asked for again, and then leaves the link `failed`.
fn reattach(shared: &Shared, channel: &Channel, wish: Wish) {
    let mut trouble = String::new();
    for attempt in 0..REATTACH_TRIES {
        match channel.files(Request::Runtime) {
            Ok(Reply::Runtime { runtime: RuntimeState::Running { .. } | RuntimeState::Queued { .. } }) => {
                shared.with(|i| i.wanted = Some(wish));
                return;
            }
            Ok(Reply::Runtime { runtime: RuntimeState::NotRunning }) => {
                shared.with(|i| {
                    i.state = State::Failed;
                    i.job = None;
                    let said = format!("Julia on {} is not running any more: it ended, or its start was cut short, while Endeavor was disconnected.", i.name);
                    i.step = Some(said.clone());
                    i.error = Some(said);
                });
                shared.listener.disconnected();
                return;
            }
            Ok(other) => trouble = format!("The helper answered {other:?}."),
            Err(message) => trouble = message,
        }
        // The connection went again: the next one tries, with the same wish.
        if channel.is_closed() {
            shared.with(|i| i.resume = Some(wish));
            return;
        }
        if attempt + 1 < REATTACH_TRIES {
            std::thread::sleep(REATTACH_PAUSE);
        }
    }
    eprintln!("Couldn't ask whether the runtime is still there: {trouble}");
    shared.with(|i| {
        i.state = State::Failed;
        i.error = Some(format!("Endeavor couldn't find out whether Julia on {} is still running ({trouble}). Call use_machine to try again.", i.name));
        i.step = i.error.clone();
    });
    shared.listener.disconnected();
}

/// Connect to the machine with the record the link was started with.
fn connect_now(shared: &Arc<Shared>) -> Result<Arc<Channel>, ConnectError> {
    let server = shared.server.clone();
    let helper = |os: &str, arch: &str| -> Result<PathBuf, String> {
        if (os.to_owned(), arch.to_owned()) == this_platform() {
            std::env::current_exe().map_err(|e| format!("Couldn't find the endeavor program itself: {e}"))
        } else {
            crate::release::helper_for(os, arch, &Env::from_vars(&|name| std::env::var(name).ok()).helpers_dir())
        }
    };
    let allowed = shared.allow_install.load(Ordering::SeqCst);
    let options = Options { auth: Auth::Batch, root: shared.hooks.root.clone(), state: shared.hooks.state.clone(), depot: shared.hooks.depot.clone(), exit_idle: true, allow_install: allowed, helper: &helper };
    let transport = if shared.hooks.shell { Transport::Shell { env: Vec::new(), ask: shared.hooks.ask.clone() } } else { Transport::for_server(&server) };
    let cancel = Arc::new(Cancel::default());
    *shared.cancel.lock().unwrap() = cancel.clone();
    if shared.leaving.load(Ordering::SeqCst) {
        cancel.cancel();
    }
    shared.with(|i| i.hello = None);
    let (channel, hello) = connect(&server, &transport, &options, &cancel, &|event| shared.on_event(event))?;
    shared.with(|i| {
        let hello_info = i.hello.get_or_insert_with(HelloInfo::default);
        (hello_info.node, hello_info.home, hello_info.slurm, hello_info.uploads) = (hello.node, hello.home.display().to_string(), hello.slurm, hello.uploads);
    });
    Ok(Arc::new(channel))
}

/// Wait up to `time` for a start request.
fn wait_for(inbox: &Receiver<Msg>, time: Duration) {
    let until = Instant::now().checked_add(time);
    loop {
        let left = until.map_or(Duration::from_secs(3600), |until| until.saturating_duration_since(Instant::now()));
        match inbox.recv_timeout(left) {
            Ok(Msg::Kick) => return,
            // Ends of connections that are over.
            Ok(Msg::Closed(_) | Msg::Started(..) | Msg::Notice(..)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) if until.is_some() => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => park(),
        }
    }
}

/// Follow connection number `conn` until it ends.
fn serve_connection(shared: &Arc<Shared>, inbox: &Receiver<Msg>, conn: u64) {
    loop {
        let Ok(message) = inbox.recv() else { park() };
        match message {
            Msg::Closed(g) if g == conn => {
                if shared.leaving.load(Ordering::SeqCst) {
                    park();
                }
                shared.with(|i| {
                    i.resume = (i.runtime.is_some() || i.starting).then(|| i.wanted.take().or_else(|| i.resume.take()).unwrap_or_default());
                    i.ended = (i.resume.is_none() && i.state == State::Failed).then(|| i.error.take()).flatten();
                    i.channel = None;
                    i.runtime = None;
                    i.starting = false;
                    i.queue = None;
                    i.state = State::Connecting;
                    i.error = None;
                    i.step = Some(format!("Lost the connection to {}; connecting again", i.name));
                });
                return;
            }
            Msg::Started(g, epoch, result) if g == conn => {
                let current = shared.with(|i| i.epoch == epoch);
                if !current {
                    continue;
                }
                match result {
                    // It went before its runtime was heard of, and was handled as gone: it is not ready.
                    Ok(_) if shared.with(|i| std::mem::take(&mut i.gone_early)) => shared.with(|i| i.starting = false),
                    Ok(runtime) => shared.with(|i| {
                        i.starting = false;
                        i.state = State::Ready;
                        i.error = None;
                        i.queue = None;
                        i.step = Some(format!("Ready on {}", runtime.node));
                        if let Some(job) = &runtime.job {
                            let known = i.job.take().unwrap_or_default();
                            i.job = Some(JobInfo { id: job.id.clone(), summary: known.summary, node: Some(job.node.clone()), ends_at: job.ends_at });
                        }
                        i.runtime = Some(RuntimeInfo {
                            port: runtime.port,
                            token: runtime.token,
                            mcp_url: runtime.mcp_url,
                            page_url: runtime.page_url,
                            node: runtime.node,
                            pid: runtime.pid,
                            reattached: runtime.reattached,
                            job: runtime.job,
                        });
                    }),
                    // The connection ended under the start: `Closed` follows and takes the start along to the next one.
                    Err(StartError::Failed(message)) if message == CLOSED => {}
                    Err(_) if shared.with(|i| i.channel.as_ref().is_some_and(|c| c.is_closed())) => {}
                    Err(StartError::NeedsInstall(items)) => {
                        eprintln!("Starting the runtime needs {}, and installing wasn't allowed.", wire::items_text(&items));
                        shared.with(|i| {
                            (i.starting, i.gone_early, i.runtime, i.job, i.queue) = (false, false, None, None, None);
                            i.wanted = None;
                            i.state = State::NeedsInstall;
                            i.error = None;
                            i.step = Some(format!("{} Waiting for the user's yes, which is for this start only.", wire::needs_text(&items, &i.name)));
                            i.needs = Some(InstallInfo { items, helper: None });
                        });
                        shared.listener.restart_failed();
                    }
                    Err(StartError::Failed(message)) => {
                        eprintln!("Starting the runtime failed: {message}");
                        shared.with(|i| {
                            i.starting = false;
                            i.gone_early = false;
                            i.wanted = None;
                            i.runtime = None;
                            i.state = State::Failed;
                            i.job = None;
                            i.queue = None;
                            i.error = Some(message);
                        });
                        shared.listener.restart_failed();
                    }
                }
            }
            Msg::Notice(g, notice) if g == conn => match notice {
                Notice::Died(reason) => {
                    shared.with(|i| {
                        i.gone_early = i.starting;
                        i.runtime = None;
                        i.wanted = None;
                        i.job = None;
                        i.queue = None;
                        i.state = State::Failed;
                        i.error = Some(format!("Julia on {} stopped. {reason}", i.name).trim_end().to_owned());
                    });
                    shared.listener.disconnected();
                }
                Notice::Replaced => {
                    shared.with(|i| {
                        i.gone_early = i.starting;
                        i.runtime = None;
                        i.wanted = None;
                        i.job = None;
                        i.queue = None;
                        i.state = State::Failed;
                        i.error = Some(format!("Another connection took Julia on {} over.", i.name));
                    });
                    shared.listener.disconnected();
                }
                // The helper is gone; `Closed` follows.
                Notice::Lost(message) => eprintln!("The helper said: {message}"),
            },
            _ => {}
        }
    }
}

fn park() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Ends the link when it has been idle, or a stop signal came.
fn watch(shared: Arc<Shared>) {
    let tick = (shared.idle / 4).clamp(Duration::from_millis(20), Duration::from_millis(100));
    loop {
        std::thread::sleep(tick);
        if SIGNALLED.load(Ordering::SeqCst) {
            eprintln!("Stopped by a signal");
            leave(&shared);
        }
        if shared.busy.load(Ordering::SeqCst) == 0 && shared.activity.lock().unwrap().elapsed() >= shared.idle {
            eprintln!("No request for {} s; ending (the runtime keeps running)", shared.idle.as_secs());
            leave(&shared);
        }
    }
}

/// Detach from the runtime, remove the record and exit. Never stops the runtime.
fn leave(shared: &Arc<Shared>) -> ! {
    if !start_leaving(shared) {
        park();
    }
    finish_leaving(shared)
}

/// The first step of leaving, which comes before anything slow: the record goes,
/// so that a front that asks for the link now gets a new one, and the control
/// port stops answering as a link. False when another thread is leaving already.
fn start_leaving(shared: &Shared) -> bool {
    if shared.leaving.swap(true, Ordering::SeqCst) {
        return false;
    }
    let path = shared.dir.join("link.json");
    let ours = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Record>(&text).ok()).is_some_and(|r| r.pid == std::process::id());
    if ours {
        let _ = std::fs::remove_file(&path);
    }
    shared.cancel.lock().unwrap().cancel();
    true
}

fn finish_leaving(shared: &Shared) -> ! {
    if let Some(channel) = shared.with(|i| i.channel.take()) {
        // The helper goes when it has the word; a connection that is dead doesn't hold the exit up.
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || {
            channel.detach();
            let _ = done.send(());
        });
        let _ = waited.recv_timeout(Duration::from_secs(5));
    }
    exit(0)
}

fn serve_control(shared: Arc<Shared>, socket: TcpListener) {
    for connection in socket.incoming() {
        let Ok(connection) = connection else {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let shared = shared.clone();
        std::thread::spawn(move || {
            let _ = control(&shared, connection);
        });
    }
}

/// Counts a control request as in flight, and as activity when it starts and ends.
struct InFlight<'a>(&'a Shared);

impl<'a> InFlight<'a> {
    fn begin(shared: &'a Shared) -> InFlight<'a> {
        shared.busy.fetch_add(1, Ordering::SeqCst);
        shared.touch();
        InFlight(shared)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

fn control(shared: &Arc<Shared>, mut connection: TcpStream) -> std::io::Result<()> {
    connection.set_read_timeout(Some(Duration::from_secs(5)))?;
    connection.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(connection.try_clone()?);
    let Some(head) = Head::read(&mut reader)? else { return Ok(()) };
    let reply = |connection: &mut TcpStream, status: &str, body: &Value| http::respond(connection, status, Some("application/json"), body.to_string().as_bytes(), false);
    if !crate::core::loopback_host(head.header("Host").unwrap_or_default()) {
        return reply(&mut connection, "403 Forbidden", &json!({ "error": "host_not_loopback" }));
    }
    if head.header("Origin").is_some() {
        return reply(&mut connection, "403 Forbidden", &json!({ "error": "browser_origin_refused" }));
    }
    if !crate::core::same(head.header("Authorization").unwrap_or_default(), &format!("Bearer {}", shared.token)) {
        return reply(&mut connection, "401 Unauthorized", &json!({ "error": "unauthorized" }));
    }
    if shared.leaving.load(Ordering::SeqCst) && head.path() != "/link/quit" {
        return reply(&mut connection, "503 Service Unavailable", &json!({ "error": "The link is ending." }));
    }
    let _request = InFlight::begin(shared);
    let body = match head.request_body()? {
        Framing::Length(n) if n <= MAX_BODY => http::read_body(&mut reader, Framing::Length(n))?,
        Framing::Length(_) => return reply(&mut connection, "413 Payload Too Large", &json!({ "error": "too_large" })),
        _ => return reply(&mut connection, "400 Bad Request", &json!({ "error": "a body needs a Content-Length" })),
    };
    match (head.method(), head.path()) {
        ("GET", "/link/status") => reply(&mut connection, "200 OK", &json!(shared.status())),
        ("POST", "/link/start") => {
            let (job, only_running, install) = if body.is_empty() {
                (None, false, false)
            } else {
                let Ok(asked) = serde_json::from_slice::<Value>(&body) else { return reply(&mut connection, "400 Bad Request", &json!({ "error": "the body isn't JSON" })) };
                let Ok(job) = serde_json::from_value::<Option<JobRequest>>(asked.get("job").cloned().unwrap_or(Value::Null)) else {
                    return reply(&mut connection, "400 Bad Request", &json!({ "error": "job isn't a job request" }));
                };
                (job, asked["only_running"] == true, asked["install"] == true)
            };
            shared.request_start(job, only_running, install);
            reply(&mut connection, "200 OK", &json!(shared.status()))
        }
        ("POST", "/link/install") => {
            shared.request_install();
            reply(&mut connection, "200 OK", &json!(shared.status()))
        }
        ("POST", "/link/stop") => match shared.request_stop() {
            Ok(()) => reply(&mut connection, "200 OK", &json!({ "ok": true })),
            Err(message) => reply(&mut connection, "409 Conflict", &json!({ "error": message })),
        },
        ("POST", "/link/quit") => {
            if start_leaving(shared) {
                let _ = reply(&mut connection, "200 OK", &json!({ "ok": true }));
                finish_leaving(shared)
            }
            reply(&mut connection, "200 OK", &json!({ "ok": true }))
        }
        (_, "/link/status" | "/link/start" | "/link/install" | "/link/stop" | "/link/quit") => reply(&mut connection, "405 Method Not Allowed", &json!({ "error": "method_not_allowed" })),
        _ => reply(&mut connection, "404 Not Found", &json!({ "error": "not_found" })),
    }
}

/// A stop signal asks the link to leave (it is read on a thread of its own: `watch`).
/// Handlers, and not a blocked mask, since a mask would pass on to `ssh` and the helper.
#[cfg(unix)]
fn catch_stop_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        SIGNALLED.store(true, Ordering::SeqCst);
    }
    // SAFETY: a handler that only sets an atomic; the mask is cleared of the signals first.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, signal);
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Ctrl-C, Ctrl-Break and the console closing ask the link to leave.
#[cfg(windows)]
fn catch_stop_signals() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn on_console_event(_: u32) -> windows_sys::core::BOOL {
        SIGNALLED.store(true, Ordering::SeqCst);
        // Windows ends the process once this returns for a closing console; `watch` exits first.
        std::thread::sleep(Duration::from_secs(10));
        1
    }
    // SAFETY: a handler that only touches an atomic and sleeps, for the life of the process.
    if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
        eprintln!("endeavor link: couldn't take Ctrl-C ({})", std::io::Error::last_os_error());
    }
}
