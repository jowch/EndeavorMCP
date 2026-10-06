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

use super::{HelloInfo, JobInfo, JuliaInfo, QueueInfo, Record, RuntimeInfo, State, Status, valid_id};
use crate::client::{Auth, Cancel, Channel, ConnectError, Event, Listener, MachinesFile, Messages, Notice, Options, Server, Transport, connect_checked, start, this_platform};
use crate::http::{self, Framing, Head};
use crate::standalone::Env;

/// The link ends this long after the last control request.
const IDLE: Duration = Duration::from_secs(8 * 3600);

/// How long a reconnect waits after its first failure, doubling up to `RETRY_LAST`.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LAST: Duration = Duration::from_secs(30);

/// A connection that stays lost this long is given up on (state `failed`).
const RETRY_GIVE_UP: Duration = Duration::from_secs(10 * 60);

/// The largest body a control request may have.
const MAX_BODY: u64 = 64 * 1024;

/// What the supervisor is told.
enum Msg {
    /// A start request while it waits to connect, or after a failure.
    Kick,
    /// The helper of this connection ended by itself.
    Closed(u64),
    /// A runtime start on this connection ended (`Shared::epoch` when it began).
    Started(u64, u64, Result<crate::client::Runtime, String>),
    /// The attached runtime went away.
    Notice(u64, Notice),
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
    wanted: Option<Option<JobRequest>>,
    /// What an attached or starting runtime had been asked for when the
    /// connection was lost: the next connection gets it back, if it is still there.
    resume: Option<Option<JobRequest>>,
    /// A start is under way on this connection.
    starting: bool,
    /// Counts the stops, so the end of a start that one cut short is ignored.
    epoch: u64,
}

struct Shared {
    dir: PathBuf,
    token: String,
    inbox: Sender<Msg>,
    listener: Arc<Listener>,
    machines: MachinesFile,
    inner: Mutex<Inner>,
    /// The connect under way, which a quit cancels.
    cancel: Mutex<Arc<Cancel>>,
    activity: Mutex<Instant>,
    /// Control requests in flight.
    busy: AtomicUsize,
    leaving: AtomicBool,
    idle: Duration,
    hooks: Hooks,
}

/// Settings for tests (`link`'s docs).
struct Hooks {
    shell: bool,
    root: String,
    state: String,
    depot: String,
}

impl Hooks {
    fn read() -> Hooks {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Hooks { shell: std::env::var_os("ENDEAVOR_LINK_SHELL").is_some(), root: var("ENDEAVOR_LINK_ROOT"), state: var("ENDEAVOR_LINK_STATE"), depot: var("ENDEAVOR_LINK_DEPOT") }
    }
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// `endeavor link --machine ID`.
pub(crate) fn main(argv: &[String]) -> ! {
    let fail = |message: String| -> ! {
        eprintln!("endeavor link: {message}");
        exit(1)
    };
    let id = match argv {
        [flag, id] if flag == "--machine" => id.clone(),
        _ => fail("usage: endeavor link --machine ID".into()),
    };
    valid_id(&id).unwrap_or_else(|e| fail(e));
    let env = Env::from_vars(&|name| std::env::var(name).ok());
    let dir = env.links_dir().join(&id);
    crate::make_state_dir(&dir).unwrap_or_else(|e| fail(e));
    let machines = MachinesFile::here();
    let server = match machines.find_by_id(&id) {
        Ok(Some(server)) => server,
        Ok(None) => fail(format!("There is no machine {id} in {}.", machines.path().display())),
        Err(e) => fail(e),
    };
    if let Some(running) = super::running(&dir, &id)
        && running.pid != std::process::id()
    {
        fail(format!("The link to {id} already runs (pid {}).", running.pid));
    }
    catch_stop_signals();
    let idle = std::env::var("ENDEAVOR_LINK_IDLE_SECS").ok().and_then(|s| s.parse::<f64>().ok()).map_or(IDLE, Duration::from_secs_f64);
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
        machines,
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
            starting: false,
            epoch: 0,
        }),
        cancel: Mutex::new(Arc::new(Cancel::default())),
        activity: Mutex::new(Instant::now()),
        busy: AtomicUsize::new(0),
        leaving: AtomicBool::new(false),
        idle,
        hooks: Hooks::read(),
    });
    let record = Record { machine: id.clone(), pid: std::process::id(), started, port, token, build: crate::embedded::BUILD_VERSION.to_owned() };
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
            Event::FoundJulia { path, version } => {
                i.step = Some(format!("Found Julia {version} ({path})"));
                if let Some(hello) = &mut i.hello {
                    hello.julia = Some(JuliaInfo { path, version });
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
            let (Some(channel), Some(job)) = (i.channel.clone(), i.wanted.clone()) else { return None };
            if i.starting || i.runtime.is_some() {
                return None;
            }
            i.starting = true;
            i.state = State::Starting;
            i.error = None;
            i.queue = None;
            i.step = Some(format!("Starting the runtime on {}", i.name));
            Some((channel, job, i.conn, i.epoch))
        });
        let Some((channel, job, conn, epoch)) = begun else { return };
        let shared = self.clone();
        std::thread::spawn(move || {
            let tx = shared.inbox.clone();
            let result = start(&channel, &shared.listener, job, &|event| shared.on_event(event), move |notice| drop(tx.send(Msg::Notice(conn, notice))));
            let _ = shared.inbox.send(Msg::Started(conn, epoch, result));
        });
    }

    /// `POST /link/start`.
    fn request_start(self: &Arc<Shared>, job: Option<JobRequest>) {
        let kick = self.with(|i| {
            if i.runtime.is_some() || i.starting {
                return false;
            }
            i.wanted = Some(job);
            i.resume = None;
            if i.channel.is_none() && i.state == State::Failed {
                (i.state, i.error) = (State::Connecting, None);
            }
            i.channel.is_none()
        });
        if kick {
            // No connection: the supervisor starts the runtime once it has one, and tries now if it was waiting.
            let _ = self.inbox.send(Msg::Kick);
        } else {
            self.begin_start();
        }
    }

    /// `POST /link/stop`.
    fn request_stop(&self) -> Result<(), String> {
        let Some(channel) = self.with(|i| i.channel.clone()) else {
            return Err(format!("Endeavor isn't connected to {} right now, so it can't stop the runtime. Try again once it is.", self.with(|i| i.name.clone())));
        };
        channel.stop()?;
        self.with(|i| {
            i.epoch += 1;
            i.wanted = None;
            i.starting = false;
            i.runtime = None;
            i.job = None;
            i.queue = None;
            i.error = None;
            i.state = State::Connected;
            i.step = Some("The runtime was stopped".into());
        });
        self.listener.disconnected();
        Ok(())
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
            i.step = Some(format!("Connecting to {}", i.name));
            i.name.clone()
        });
        match connect_now(&shared) {
            Err(error) => {
                if shared.leaving.load(Ordering::SeqCst) {
                    park();
                }
                let lost = *lost_since.get_or_insert_with(Instant::now);
                let retry = reconnecting && error.retry && lost.elapsed() < RETRY_GIVE_UP;
                eprintln!("The connection to {name} failed{}: {}", if retry { ", trying again" } else { "" }, error.message);
                shared.with(|i| {
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
                    i.channel = Some(channel.clone());
                    i.state = State::Connected;
                    i.error = None;
                    i.step = Some(format!("Connected to {name}"));
                    // A start asked for while the connection was away is the newer wish.
                    i.resume.take().filter(|_| i.wanted.is_none())
                });
                if hello_wants_slurm(&shared) {
                    let (shared, channel) = (shared.clone(), channel.clone());
                    std::thread::spawn(move || find_partitions(&shared, &channel, conn));
                }
                if let Some(job) = resume {
                    reattach(&shared, &channel, job);
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
    if let Ok(Reply::Slurm { scheduler }) = channel.files(Request::Slurm) {
        shared.with(|i| {
            if let Some(hello) = i.hello.as_mut().filter(|_| i.conn == conn) {
                (hello.partitions, hello.scratch) = (Some(scheduler.partitions), scheduler.scratch);
            }
        });
    }
}

/// After a reconnect, attach to the runtime that was asked for only if it is
/// still there: a start would otherwise begin a new one, or on a cluster
/// submit a new job nobody asked for.
fn reattach(shared: &Shared, channel: &Channel, job: Option<JobRequest>) {
    match channel.files(Request::Runtime) {
        Ok(Reply::Runtime { runtime: RuntimeState::NotRunning }) => shared.with(|i| {
            i.step = Some(format!("The runtime on {} ended while Endeavor was disconnected", i.name));
        }),
        _ => shared.with(|i| i.wanted = Some(job)),
    }
}

/// Connect to the machine: its record is read again, so a change in the file is used.
fn connect_now(shared: &Arc<Shared>) -> Result<Arc<Channel>, ConnectError> {
    let wrong = |message: String| ConnectError { message, retry: false };
    let id = shared.with(|i| i.id.clone());
    let server = shared
        .machines
        .find_by_id(&id)
        .map_err(wrong)?
        .ok_or_else(|| wrong(format!("{id} isn't in the list of machines ({}) any more.", shared.machines.path().display())))?;
    shared.with(|i| i.name = display_name(&server));
    let helper = |os: &str, arch: &str| -> Result<PathBuf, String> {
        if (os.to_owned(), arch.to_owned()) == this_platform() {
            std::env::current_exe().map_err(|e| format!("Couldn't find the endeavor program itself: {e}"))
        } else {
            Err(format!("A helper for {os} {arch} servers isn't available yet in this version of Endeavor."))
        }
    };
    let options = Options { auth: Auth::Batch, root: shared.hooks.root.clone(), state: shared.hooks.state.clone(), depot: shared.hooks.depot.clone(), exit_idle: true, helper: &helper };
    let transport = if shared.hooks.shell { Transport::Shell { env: Vec::new(), ask: None } } else { Transport::for_server(&server) };
    let cancel = Arc::new(Cancel::default());
    *shared.cancel.lock().unwrap() = cancel.clone();
    if shared.leaving.load(Ordering::SeqCst) {
        cancel.cancel();
    }
    shared.with(|i| i.hello = None);
    let (channel, hello) = connect_checked(&server, &transport, &options, &cancel, &|event| shared.on_event(event))?;
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
                    i.resume = (i.runtime.is_some() || i.starting).then(|| i.wanted.take().or_else(|| i.resume.take()).unwrap_or(None));
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
                    Err(message) => {
                        eprintln!("Starting the runtime failed: {message}");
                        shared.with(|i| {
                            i.starting = false;
                            i.wanted = None;
                            i.runtime = None;
                            i.state = State::Failed;
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
                        i.runtime = None;
                        i.wanted = None;
                        i.state = State::Failed;
                        i.error = Some(format!("Julia on {} stopped. {reason}", i.name).trim_end().to_owned());
                    });
                    shared.listener.disconnected();
                }
                Notice::Replaced => {
                    shared.with(|i| {
                        i.runtime = None;
                        i.wanted = None;
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
    if shared.leaving.swap(true, Ordering::SeqCst) {
        park();
    }
    shared.cancel.lock().unwrap().cancel();
    if let Some(channel) = shared.with(|i| i.channel.take()) {
        // The helper goes when it has the word; a connection that is dead doesn't hold the exit up.
        let (done, waited) = mpsc::channel();
        std::thread::spawn(move || {
            channel.detach();
            let _ = done.send(());
        });
        let _ = waited.recv_timeout(Duration::from_secs(5));
    }
    let path = shared.dir.join("link.json");
    let ours = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<Record>(&text).ok()).is_some_and(|r| r.pid == std::process::id());
    if ours {
        let _ = std::fs::remove_file(&path);
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
    let _request = InFlight::begin(shared);
    let body = match head.request_body()? {
        Framing::Length(n) if n <= MAX_BODY => http::read_body(&mut reader, Framing::Length(n))?,
        Framing::Length(_) => return reply(&mut connection, "413 Payload Too Large", &json!({ "error": "too_large" })),
        _ => return reply(&mut connection, "400 Bad Request", &json!({ "error": "a body needs a Content-Length" })),
    };
    match (head.method(), head.path()) {
        ("GET", "/link/status") => reply(&mut connection, "200 OK", &json!(shared.status())),
        ("POST", "/link/start") => {
            let job = if body.is_empty() {
                None
            } else {
                match serde_json::from_slice::<Value>(&body).ok().and_then(|v| serde_json::from_value::<Option<JobRequest>>(v.get("job").cloned().unwrap_or(Value::Null)).ok()) {
                    Some(job) => job,
                    None => return reply(&mut connection, "400 Bad Request", &json!({ "error": "job isn't a job request" })),
                }
            };
            shared.request_start(job);
            reply(&mut connection, "200 OK", &json!(shared.status()))
        }
        ("POST", "/link/stop") => match shared.request_stop() {
            Ok(()) => reply(&mut connection, "200 OK", &json!({ "ok": true })),
            Err(message) => reply(&mut connection, "409 Conflict", &json!({ "error": message })),
        },
        ("POST", "/link/quit") => {
            reply(&mut connection, "200 OK", &json!({ "ok": true }))?;
            leave(shared)
        }
        (_, "/link/status" | "/link/start" | "/link/stop" | "/link/quit") => reply(&mut connection, "405 Method Not Allowed", &json!({ "error": "method_not_allowed" })),
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
