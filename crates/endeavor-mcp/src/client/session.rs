use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wire::files::{Reply, Request, RuntimeState};
use wire::slurm::{JobRequest, Partition};

use super::channel::{CLOSED, Channel, Notice, Runtime, StartError, StartOptions};
use super::listener::{Listener, Messages};
use super::machines::{Launcher, Server};
use super::ssh::{Auth, Cancel, ConnectError, Event, NeedsInstall, Options, Transport, connect, start};

const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LAST: Duration = Duration::from_secs(30);

const RETRY_GIVE_UP: Duration = Duration::from_secs(10 * 60);

const REATTACH_TRIES: u32 = 3;
const REATTACH_PAUSE: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Connecting,
    Connected,
    NothingRunning,
    Starting { queue: Option<QueueInfo> },
    Queued(QueueInfo),
    Ready(RuntimeInfo),
    NeedsInstall(InstallInfo),
    Failed(String),
}

impl State {
    pub fn runtime(&self) -> Option<&RuntimeInfo> {
        if let State::Ready(runtime) = self { Some(runtime) } else { None }
    }

    pub fn queue(&self) -> Option<&QueueInfo> {
        match self {
            State::Queued(queue) | State::Starting { queue: Some(queue) } => Some(queue),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<&str> {
        if let State::Failed(why) = self { Some(why) } else { None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstallInfo {
    pub items: Vec<wire::Item>,
    pub helper: Option<NeedsInstall>,
}

impl InstallInfo {
    pub(crate) fn helper(found: NeedsInstall) -> InstallInfo {
        let item = wire::Item { kind: wire::KIND_HELPER.into(), name: "Endeavor's helper".into(), size_mb: found.bytes.map(|bytes| bytes.div_ceil(1_000_000).max(1)), place: Some(found.folder.clone()) };
        InstallInfo { items: vec![item], helper: Some(found) }
    }

    pub fn needs_helper(&self) -> bool {
        self.helper.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HelloInfo {
    pub node: String,
    pub home: String,
    pub slurm: bool,
    pub uploads: bool,
    pub launcher: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub helper_installed: Option<bool>,
    pub found: Vec<FoundInfo>,
    pub partitions: Option<Vec<Partition>>,
    #[serde(default)]
    pub partitions_failed: bool,
    pub scratch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FoundInfo {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub port: u16,
    pub token: String,
    pub mcp_url: String,
    pub page_url: String,
    #[serde(default)]
    pub page_token: String,
    pub node: String,
    pub pid: u32,
    pub reattached: bool,
    pub job: Option<wire::slurm::Job>,
    pub remote_port: Option<u16>,
    #[serde(default)]
    pub build: Option<String>,
    #[serde(default)]
    pub interface: Option<u32>,
}

impl RuntimeInfo {
    pub fn usable_as_is(&self) -> bool {
        crate::usable_as_is(self.build.as_deref(), self.interface)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobInfo {
    pub id: String,
    pub summary: Option<String>,
    pub node: Option<String>,
    pub ends_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueInfo {
    pub state: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub machine: String,
    pub name: String,
    pub state: State,
    pub step: Option<String>,
    pub hello: Option<HelloInfo>,
    pub job: Option<JobInfo>,
    pub gone: Option<Gone>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Gone {
    Died(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Attach { install: bool },
    Start { job: Option<JobRequest>, install: bool },
}

impl Want {
    fn install(&self) -> bool {
        matches!(self, Want::Attach { install: true } | Want::Start { install: true, .. })
    }

    fn wish(&self) -> Wish {
        match self {
            Want::Attach { .. } => Wish { job: None, attach: true, install: false },
            Want::Start { job, install } => Wish { job: job.clone(), attach: false, install: *install },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ready(RuntimeInfo),
    Queued { job: Option<JobInfo>, queue: QueueInfo },
    NothingRunning,
    NeedsInstall(InstallInfo),
    Failed(String),
    StillWorking(String),
}

pub type HelperFor = Box<dyn Fn(&str, &str) -> Result<PathBuf, String> + Send + Sync>;

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    Step(Event),
    Trouble(String),
}

pub type OnEvent = Box<dyn Fn(SessionEvent) + Send + Sync>;

pub struct Config {
    pub server: Server,
    pub transport: Transport,
    pub root: String,
    pub state: String,
    pub depot: String,
    pub allow_install: bool,
    pub messages: Messages,
    pub helper: HelperFor,
    pub launcher: Option<Launcher>,
    pub auth: Auth,
    pub exit_idle: bool,
    pub julia_when_needed: bool,
    pub on_event: OnEvent,
    pub socket: Option<TcpListener>,
}

impl Config {
    pub fn new(server: Server, helper: impl Fn(&str, &str) -> Result<PathBuf, String> + Send + Sync + 'static) -> Config {
        let transport = Transport::for_server(&server);
        let on_event: OnEvent = Box::new(|event| {
            if let SessionEvent::Trouble(text) = event {
                eprintln!("{text}");
            }
        });
        Config { server, transport, root: String::new(), state: String::new(), depot: String::new(), allow_install: false, messages: Messages::default(), helper: Box::new(helper), launcher: None, auth: Auth::Batch, exit_idle: true, julia_when_needed: false, on_event, socket: None }
    }
}

enum Msg {
    Kick,
    Quit,
    Closed(u64),
    Started(u64, u64, bool, Box<Result<Runtime, StartError>>),
    Notice(u64, Notice),
}

#[derive(Clone, Debug, Default)]
struct Wish {
    job: Option<JobRequest>,
    attach: bool,
    install: bool,
}

#[derive(PartialEq)]
enum Run {
    Idle,
    Starting,
    Gone,
}

impl Inner {
    fn starting(&self) -> bool {
        matches!(self.run, Run::Starting | Run::Gone)
    }

    fn busy(&self) -> bool {
        self.starting() || matches!(self.state, State::Ready(_))
    }
}

struct Inner {
    id: String,
    name: String,
    state: State,
    step: Option<String>,
    hello: Option<HelloInfo>,
    job: Option<JobInfo>,
    channel: Option<Arc<Channel>>,
    conn: u64,
    wanted: Option<Wish>,
    resume: Option<Wish>,
    ended: Option<String>,
    run: Run,
    epoch: u64,
    may_kick: bool,
    settled: Option<Launcher>,
    told_other_build: Option<u32>,
    gone: Option<Gone>,
}

struct Shared {
    config: Config,
    inbox: Sender<Msg>,
    listener: Arc<Listener>,
    inner: Mutex<Inner>,
    changed: Condvar,
    cancel: Mutex<Arc<Cancel>>,
    leaving: AtomicBool,
    allow_install: AtomicBool,
}

pub struct Session {
    shared: Arc<Shared>,
    supervisor: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Session {
    pub fn new(mut config: Config) -> Result<Session, String> {
        let name = config.server.display_name();
        let listener = Listener::on_socket(&name, None, config.messages, config.socket.take())?;
        let (inbox, messages) = mpsc::channel();
        let inner = Inner {
            id: config.server.id.clone(),
            name,
            state: State::Connecting,
            step: None,
            hello: None,
            job: None,
            channel: None,
            conn: 0,
            wanted: None,
            resume: None,
            ended: None,
            run: Run::Idle,
            epoch: 0,
            may_kick: true,
            settled: None,
            told_other_build: None,
            gone: None,
        };
        let shared = Arc::new(Shared {
            allow_install: AtomicBool::new(config.allow_install),
            config,
            inbox,
            listener,
            inner: Mutex::new(inner),
            changed: Condvar::new(),
            cancel: Mutex::new(Arc::new(Cancel::default())),
            leaving: AtomicBool::new(false),
        });
        let supervising = shared.clone();
        let name = format!("session-{}", shared.listener.port());
        let supervisor = std::thread::Builder::new().name(name).spawn(move || supervise(&supervising, &messages)).map_err(|e| {
            shared.listener.close();
            e.to_string()
        })?;
        Ok(Session { shared, supervisor: Mutex::new(Some(supervisor)) })
    }

    pub fn ensure(&self, want: Want, wait: Duration, retry: bool) -> Outcome {
        let begin = self.shared.with(|i| {
            let kept = matches!(i.state, State::Failed(_)) || (matches!(i.state, State::NeedsInstall(_)) && !want.install());
            !(kept && !retry) && self.shared.ask(i, &want)
        });
        if begin {
            self.shared.begin_start();
        }
        self.wait(wait)
    }

    pub fn request(&self, want: &Want) {
        self.shared.request_start(want);
    }

    pub fn status(&self) -> Status {
        status_of(&self.shared.inner())
    }

    pub fn wait_for(&self, wait: Duration, done: impl Fn(&Status) -> bool) -> Status {
        let until = Instant::now().checked_add(wait);
        let mut inner = self.shared.inner();
        loop {
            let status = status_of(&inner);
            let left = until.map_or(Duration::from_secs(3600), |until| until.saturating_duration_since(Instant::now()));
            if done(&status) || (until.is_some() && left.is_zero()) {
                return status;
            }
            inner = self.shared.changed.wait_timeout(inner, left).unwrap().0;
        }
    }

    pub fn cluster(&self) -> bool {
        let settled = self.shared.inner().settled;
        settled.or(self.shared.config.launcher).unwrap_or_else(|| self.shared.config.server.launcher()) != Launcher::Process
    }

    pub fn files(&self, request: Request, wait: Duration) -> Option<Result<Reply, String>> {
        self.wait_for(wait, |status| status.state != State::Connecting);
        let channel = self.shared.inner().channel.clone()?;
        Some(channel.files(request))
    }

    pub fn retry_now(&self) {
        self.shared.retry_now();
    }

    pub fn connected(&self) -> bool {
        self.shared.inner().channel.is_some()
    }

    pub fn stop(&self) -> Result<(), String> {
        self.shared.request_stop(false)
    }

    pub fn force_stop(&self) -> Result<(), String> {
        self.shared.request_stop(true)
    }

    pub fn restart(&self, job: Option<JobRequest>, install: bool, wait: Duration) -> Outcome {
        if let Err(message) = self.shared.request_stop(false) {
            return Outcome::Failed(message);
        }
        self.shared.listener.restarting();
        self.ensure(Want::Start { job, install }, wait, true)
    }

    pub fn allow_install(&self) {
        self.shared.request_install();
    }

    pub fn port(&self) -> u16 {
        self.shared.listener.port()
    }

    pub fn hand_over_port(&self) -> Option<TcpListener> {
        self.shared.listener.hand_over()
    }

    pub fn close(&self) {
        let shared = &self.shared;
        if shared.leaving.swap(true, Ordering::SeqCst) {
            return;
        }
        let held = shared.with(|i| i.channel.take());
        if held.is_none() {
            shared.cancel.lock().unwrap().cancel();
        }
        let _ = shared.inbox.send(Msg::Quit);
        if let Some(channel) = held {
            let_go(channel);
        }
        shared.cancel.lock().unwrap().cancel();
        if let Some(supervisor) = self.supervisor.lock().unwrap().take() {
            let _ = supervisor.join();
        }
        if let Some(channel) = shared.with(|i| i.channel.take()) {
            let_go(channel);
        }
        shared.listener.close();
        shared.with(|i| {
            (i.run, i.wanted, i.resume) = (Run::Idle, None, None);
            (i.conn, i.epoch) = (i.conn + 1, i.epoch + 1);
            i.state = State::Failed(format!("Endeavor's connection to {} was closed.", i.name));
        });
    }

    fn wait(&self, wait: Duration) -> Outcome {
        let until = Instant::now().checked_add(wait);
        let mut inner = self.shared.inner();
        loop {
            if let Some(settled) = outcome(&inner) {
                return settled;
            }
            let left = until.map_or(Duration::from_secs(3600), |until| until.saturating_duration_since(Instant::now()));
            if until.is_some() && left.is_zero() {
                return Outcome::StillWorking(inner.step.clone().unwrap_or_default());
            }
            inner = self.shared.changed.wait_timeout(inner, left).unwrap().0;
        }
    }
}

fn status_of(i: &Inner) -> Status {
    Status {
        machine: i.id.clone(),
        name: i.name.clone(),
        state: i.state.clone(),
        step: i.step.clone(),
        hello: i.hello.clone(),
        job: i.job.clone(),
        gone: i.gone.clone(),
    }
}

fn let_go(channel: Arc<Channel>) {
    let (done, waited) = mpsc::channel();
    std::thread::spawn(move || {
        channel.detach();
        let _ = done.send(());
    });
    let _ = waited.recv_timeout(Duration::from_secs(5));
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

fn outcome(i: &Inner) -> Option<Outcome> {
    match &i.state {
        State::Ready(runtime) => Some(Outcome::Ready(runtime.clone())),
        State::Queued(queue) => Some(Outcome::Queued { job: i.job.clone(), queue: queue.clone() }),
        State::Failed(why) => Some(Outcome::Failed(why.clone())),
        State::NeedsInstall(needs) => Some(Outcome::NeedsInstall(needs.clone())),
        State::NothingRunning => Some(Outcome::NothingRunning),
        State::Connecting | State::Connected | State::Starting { .. } => None,
    }
}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut i = self.inner.lock().unwrap();
        let result = f(&mut i);
        drop(i);
        self.changed.notify_all();
        result
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn trouble(&self, text: String) {
        (self.config.on_event)(SessionEvent::Trouble(text));
    }

    fn on_event(&self, event: Event) {
        let step = SessionEvent::Step(event.clone());
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
            Event::Queued { job, state, reason } => {
                i.step = Some(format!("The job is {} ({reason})", state.to_lowercase()));
                if i.job.as_ref().is_none_or(|known| known.id != job) {
                    i.job = Some(JobInfo { id: job, ..Default::default() });
                }
                if state == "RUNNING"
                    && let Some(job) = &mut i.job
                {
                    job.node = Some(reason.clone());
                }
                if i.starting() {
                    let queue = QueueInfo { state, reason };
                    i.state = if queue.state == "RUNNING" { State::Starting { queue: Some(queue) } } else { State::Queued(queue) };
                }
            }
            Event::Started { node, .. } => i.step = Some(format!("Ready on {node}")),
            Event::Slurm(_) | Event::Finished { .. } => {}
        });
        (self.config.on_event)(step);
    }

    fn begin_start(self: &Arc<Shared>) {
        let begun = self.with(|i| {
            let (Some(channel), Some(wish)) = (i.channel.clone(), i.wanted.clone()) else { return None };
            if i.busy() || self.leaving.load(Ordering::SeqCst) {
                return None;
            }
            i.run = Run::Starting;
            i.ended = None;
            i.state = State::Starting { queue: None };
            i.step = Some(format!("Starting the runtime on {}", i.name));
            Some((channel, wish, i.conn, i.epoch))
        });
        let Some((channel, wish, conn, epoch)) = begun else { return };
        let shared = self.clone();
        std::thread::spawn(move || {
            let wish = if wish.attach {
                let Some(wish) = shared.runtime_is_there(&channel, conn, epoch) else { return };
                wish
            } else {
                wish
            };
            let tx = shared.inbox.clone();
            let options = StartOptions { job: wish.job, install: wish.install, attach_only: wish.attach, ..StartOptions::default() };
            let result = start(&channel, &shared.listener, &options, &|event| shared.on_event(event), move |notice| drop(tx.send(Msg::Notice(conn, notice))));
            let _ = shared.inbox.send(Msg::Started(conn, epoch, options.install, Box::new(result)));
        });
    }

    fn runtime_is_there(&self, channel: &Channel, conn: u64, epoch: u64) -> Option<Wish> {
        let answer = channel.files(Request::Runtime);
        let current = |i: &Inner| i.conn == conn && i.epoch == epoch && !self.leaving.load(Ordering::SeqCst);
        match answer {
            Ok(Reply::Runtime { runtime: RuntimeState::Running { .. } | RuntimeState::Queued { .. } | RuntimeState::Starting }) => {
                let i = self.inner();
                i.wanted.clone().filter(|_| current(&i))
            }
            Ok(Reply::Runtime { runtime: RuntimeState::NotRunning }) => self.with(|i| {
                if !current(i) {
                    return None;
                }
                if let Some(wish) = i.wanted.clone().filter(|w| !w.attach) {
                    return Some(wish);
                }
                i.run = Run::Idle;
                i.wanted = None;
                i.state = State::NothingRunning;
                i.step = Some(format!("No runtime is running on {}", i.name));
                None
            }),
            Ok(_) | Err(_) if channel.is_closed() => None,
            other => {
                let trouble = other.map_or_else(|e| e, |reply| format!("The helper answered {reply:?}."));
                self.with(|i| {
                    if current(i) {
                        let why = format!("Endeavor couldn't find out whether Julia on {} is running ({trouble}). Starting it again tries once more.", i.name);
                        i.run = Run::Idle;
                        i.wanted = None;
                        i.step = Some(why.clone());
                        i.state = State::Failed(why);
                    }
                });
                None
            }
        }
    }

    fn request_install(&self) {
        self.allow_install.store(true, Ordering::SeqCst);
        self.with(|i| {
            if matches!(i.state, State::NeedsInstall(_)) && i.channel.is_none() {
                self.reconnect_now(i);
            }
        });
    }

    fn retry_now(&self) {
        self.with(|i| {
            if self.leaving.load(Ordering::SeqCst) || i.channel.is_some() {
                return;
            }
            match i.state {
                State::Connecting => self.kick(i),
                State::Failed(_) => self.reconnect_now(i),
                _ => {}
            }
        });
    }

    fn reconnect_now(&self, i: &mut Inner) {
        i.state = State::Connecting;
        self.kick(i);
    }

    fn kick(&self, i: &mut Inner) {
        if i.may_kick {
            i.may_kick = false;
            let _ = self.inbox.send(Msg::Kick);
        }
    }

    fn request_start(self: &Arc<Shared>, want: &Want) {
        if self.with(|i| self.ask(i, want)) {
            self.begin_start();
        }
    }

    fn ask(&self, i: &mut Inner, want: &Want) -> bool {
        if self.leaving.load(Ordering::SeqCst) {
            return false;
        }
        if want.install() && i.channel.is_none() {
            self.allow_install.store(true, Ordering::SeqCst);
        }
        let wished = i.wanted.is_some() || i.resume.is_some();
        if i.busy() || (wished && !matches!(i.state, State::Failed(_) | State::NeedsInstall(_))) {
            if let Some(wish) = i.wanted.as_mut().or(i.resume.as_mut()) {
                match want {
                    Want::Start { job, install } if wish.attach => *wish = Wish { job: job.clone(), attach: false, install: *install },
                    _ => wish.install |= want.wish().install,
                }
            }
            return false;
        }
        i.wanted = Some(want.wish());
        i.resume = None;
        i.gone = None;
        if i.state == State::NothingRunning {
            i.state = State::Connected;
        }
        if i.channel.is_some() {
            return true;
        }
        if matches!(i.state, State::Failed(_) | State::NeedsInstall(_)) {
            i.state = State::Connecting;
        }
        self.kick(i);
        false
    }

    fn request_stop(&self, force: bool) -> Result<(), String> {
        let Some((channel, mine)) = self.with(|i| {
            let channel = i.channel.clone()?;
            i.epoch += 1;
            Some((channel, i.epoch))
        }) else {
            return Err(not_connected_to_stop(&self.inner()));
        };
        let stopped = if force { channel.force_stop() } else { channel.stop() };
        self.with(|i| {
            let same = i.channel.as_ref().is_some_and(|now| Arc::ptr_eq(now, &channel));
            match &stopped {
                Ok(()) => {
                    i.resume = None;
                    if same {
                        i.wanted = None;
                        i.run = Run::Idle;
                        i.job = None;
                        i.state = State::Connected;
                        i.gone = None;
                        i.step = Some("The runtime was stopped".into());
                    }
                }
                Err(message) => {
                    if i.epoch == mine {
                        i.epoch -= 1;
                    }
                    if same && i.starting() {
                        i.run = Run::Idle;
                        i.state = State::Failed(message.clone());
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

fn not_connected_to_stop(i: &Inner) -> String {
    let name = &i.name;
    match &i.state {
        State::NeedsInstall(_) => format!("Endeavor isn't connected to {name}: its helper isn't installed there, and stopping the runtime needs it. Installing it wasn't agreed to."),
        State::Failed(why) => format!("Endeavor isn't connected to {name}: {why}"),
        _ => format!("Endeavor isn't connected to {name} yet, so it can't stop the runtime. It is still connecting."),
    }
}

fn supervise(shared: &Arc<Shared>, inbox: &Receiver<Msg>) {
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
            i.may_kick = false;
            i.step = Some(format!("Connecting to {}", i.name));
            i.name.clone()
        });
        while inbox.try_recv().is_ok() {}
        if shared.leaving.load(Ordering::SeqCst) {
            return;
        }
        match connect_now(shared) {
            Err(error) => {
                if shared.leaving.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(helper) = error.needs.clone() {
                    if shared.allow_install.load(Ordering::SeqCst) {
                        continue;
                    }
                    let parked = shared.with(|i| {
                        if shared.allow_install.load(Ordering::SeqCst) {
                            return false;
                        }
                        i.may_kick = true;
                        i.state = State::NeedsInstall(InstallInfo::helper(helper));
                        i.wanted = None;
                        i.resume = None;
                        i.step = Some(format!("Endeavor's helper isn't installed on {name}"));
                        true
                    });
                    if !parked {
                        continue;
                    }
                    shared.trouble(format!("The helper isn't installed on {name}; waiting for the user's agreement to install it"));
                    (reconnecting, lost_since, delay) = (false, None, RETRY_FIRST);
                    shared.listener.disconnected();
                    if !wait_for(inbox, Duration::MAX) {
                        return;
                    }
                    continue;
                }
                let lost = *lost_since.get_or_insert_with(Instant::now);
                let retry = reconnecting && error.retry && lost.elapsed() < RETRY_GIVE_UP;
                shared.with(|i| {
                    i.may_kick = true;
                    i.state = if retry { State::Connecting } else { State::Failed(error.message.clone()) };
                    i.step = Some(if retry { format!("Lost the connection to {name}: {}", error.message) } else { error.message.clone() });
                });
                shared.trouble(format!("The connection to {name} failed{}: {}", if retry { ", trying again" } else { "" }, error.message));
                if retry {
                    if !wait_for(inbox, delay) {
                        return;
                    }
                    delay = (delay * 2).min(RETRY_LAST);
                } else {
                    reconnecting = false;
                    lost_since = None;
                    delay = RETRY_FIRST;
                    shared.listener.disconnected();
                    if !wait_for(inbox, Duration::MAX) {
                        return;
                    }
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
                    i.may_kick = true;
                    i.channel = Some(channel.clone());
                    i.state = State::Connected;
                    i.step = Some(format!("Connected to {name}"));
                    if i.resume.is_none()
                        && let Some(why) = i.ended.take()
                    {
                        i.state = State::Failed(why);
                    }
                    i.resume.take().filter(|_| i.wanted.is_none())
                });
                if shared.leaving.load(Ordering::SeqCst) {
                    return;
                }
                if shared.inner().hello.as_ref().is_some_and(|h| h.slurm) {
                    let (shared, channel) = (shared.clone(), channel.clone());
                    std::thread::spawn(move || find_partitions(&shared, &channel, conn));
                }
                if let Some(wish) = resume {
                    reattach(shared, &channel, wish);
                }
                reconnecting = true;
                shared.begin_start();
                if !serve_connection(shared, inbox, conn) {
                    return;
                }
            }
        }
    }
}

fn find_partitions(shared: &Shared, channel: &Channel, conn: u64) {
    let found = match channel.files(Request::Slurm) {
        Ok(Reply::Slurm { scheduler }) => Some(scheduler),
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

fn reattach(shared: &Shared, channel: &Channel, wish: Wish) {
    let mut trouble = String::new();
    let epoch = shared.inner().epoch;
    let current = |i: &Inner| i.epoch == epoch && !shared.leaving.load(Ordering::SeqCst);
    for attempt in 0..REATTACH_TRIES {
        if shared.leaving.load(Ordering::SeqCst) {
            return;
        }
        match channel.files(Request::Runtime) {
            Ok(Reply::Runtime { runtime: RuntimeState::Running { .. } | RuntimeState::Queued { .. } | RuntimeState::Starting }) => {
                shared.with(|i| {
                    if current(i) {
                        i.wanted = Some(wish);
                    }
                });
                return;
            }
            Ok(Reply::Runtime { runtime: RuntimeState::NotRunning }) => {
                shared.with(|i| {
                    if current(i) {
                        i.job = None;
                        let said = format!("Julia on {} is not running any more: it ended, or its start was cut short, while Endeavor was disconnected.", i.name);
                        i.step = Some(said.clone());
                        i.state = State::Failed(said);
                    }
                });
                shared.listener.disconnected();
                return;
            }
            Ok(other) => trouble = format!("The helper answered {other:?}."),
            Err(message) => trouble = message,
        }
        if channel.is_closed() {
            shared.with(|i| i.resume = Some(wish));
            return;
        }
        if attempt + 1 < REATTACH_TRIES {
            std::thread::sleep(REATTACH_PAUSE);
        }
    }
    let name = shared.with(|i| {
        if current(i) {
            let why = format!("Endeavor couldn't find out whether Julia on {} is still running ({trouble}). Starting it again tries once more.", i.name);
            i.step = Some(why.clone());
            i.state = State::Failed(why);
        }
        i.name.clone()
    });
    shared.trouble(format!("{name}: couldn't ask whether the runtime is still there: {trouble}"));
    shared.listener.disconnected();
}

fn connect_now(shared: &Arc<Shared>) -> Result<Arc<Channel>, ConnectError> {
    let config = &shared.config;
    let allowed = shared.allow_install.load(Ordering::SeqCst);
    let launcher = shared.inner().settled.or(config.launcher);
    let options = Options { auth: config.auth.clone(), root: config.root.clone(), state: config.state.clone(), depot: config.depot.clone(), exit_idle: config.exit_idle, julia_when_needed: config.julia_when_needed, allow_install: allowed, helper: &*config.helper, launcher };
    let cancel = Arc::new(Cancel::default());
    *shared.cancel.lock().unwrap() = cancel.clone();
    if shared.leaving.load(Ordering::SeqCst) {
        cancel.cancel();
    }
    shared.with(|i| i.hello = None);
    let (channel, hello) = connect(&config.server, &config.transport, &options, &cancel, &|event| shared.on_event(event))?;
    shared.with(|i| {
        let hello_info = i.hello.get_or_insert_with(HelloInfo::default);
        (hello_info.node, hello_info.home, hello_info.slurm, hello_info.uploads) = (hello.node, hello.home, hello.slurm, hello.uploads);
        hello_info.launcher = hello.launcher.map(|l| l.word().to_owned());
        i.settled = i.settled.or(hello.launcher);
    });
    Ok(Arc::new(channel))
}

fn wait_for(inbox: &Receiver<Msg>, time: Duration) -> bool {
    let until = Instant::now().checked_add(time);
    loop {
        let left = until.map_or(Duration::from_secs(3600), |until| until.saturating_duration_since(Instant::now()));
        match inbox.recv_timeout(left) {
            Ok(Msg::Kick) => return true,
            Ok(Msg::Quit) => return false,
            Ok(Msg::Closed(_) | Msg::Started(..) | Msg::Notice(..)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) if until.is_some() => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn gone(i: &mut Inner, why: String, gone: Gone) {
    i.gone = Some(gone);
    let starting = i.starting();
    i.run = if starting { Run::Gone } else { Run::Idle };
    (i.wanted, i.job) = (None, None);
    i.step = Some(why.clone());
    i.state = if starting { State::Failed(why) } else { State::NothingRunning };
}

fn serve_connection(shared: &Arc<Shared>, inbox: &Receiver<Msg>, conn: u64) -> bool {
    loop {
        let Ok(message) = inbox.recv() else { return false };
        match message {
            Msg::Quit => return false,
            Msg::Closed(g) if g == conn => {
                if shared.leaving.load(Ordering::SeqCst) {
                    return false;
                }
                shared.with(|i| {
                    i.resume = i.busy().then(|| i.wanted.take().or_else(|| i.resume.take()).unwrap_or_default());
                    let was = std::mem::replace(&mut i.state, State::Connecting);
                    i.ended = if let State::Failed(why) = was { i.resume.is_none().then_some(why) } else { None };
                    i.channel = None;
                    i.run = Run::Idle;
                    i.step = Some(format!("Lost the connection to {}; connecting again", i.name));
                });
                return true;
            }
            Msg::Started(g, epoch, installed, result) if g == conn => {
                let current = shared.with(|i| i.epoch == epoch);
                if !current {
                    continue;
                }
                match *result {
                    Ok(_) if shared.with(|i| i.run == Run::Gone) => shared.with(|i| i.run = Run::Idle),
                    Ok(runtime) => {
                        let untold = shared.with(|i| i.told_other_build.replace(runtime.pid) != Some(runtime.pid));
                        let other = (untold && runtime.reattached && !crate::usable_as_is(runtime.build.as_deref(), runtime.interface)).then(|| {
                            let version = crate::which_version(runtime.interface, Some(crate::core::INTERFACE));
                            format!("{}: Julia there was started by {version} version of Endeavor, and it keeps running as it is. Some calls may not work as expected, or may be refused. Stopping it lets the next start use this version.", runtime.node)
                        });
                        shared.with(|i| {
                        i.step = Some(format!("Ready on {}", runtime.node));
                        if let Some(job) = &runtime.job {
                            let known = i.job.take().unwrap_or_default();
                            i.job = Some(JobInfo { id: job.id.clone(), summary: known.summary, node: Some(job.node.clone()), ends_at: job.ends_at });
                        }
                        i.run = Run::Idle;
                        i.gone = None;
                        i.state = State::Ready(RuntimeInfo {
                            port: runtime.port,
                            token: runtime.token,
                            mcp_url: runtime.mcp_url,
                            page_url: runtime.page_url,
                            page_token: runtime.page_token,
                            node: runtime.node,
                            pid: runtime.pid,
                            reattached: runtime.reattached,
                            job: runtime.job,
                            remote_port: runtime.remote_port,
                            build: runtime.build,
                            interface: runtime.interface,
                        });
                        });
                        if let Some(text) = other {
                            shared.trouble(text);
                        }
                    }
                    Err(StartError::Failed(message)) if message == CLOSED => {}
                    Err(_) if shared.with(|i| i.channel.as_ref().is_some_and(|c| c.is_closed())) => {}
                    Err(StartError::NeedsInstall(items)) => {
                        if !installed && shared.with(|i| i.wanted.as_ref().is_some_and(|w| w.install) && std::mem::replace(&mut i.run, Run::Idle) == Run::Starting) {
                            shared.begin_start();
                            continue;
                        }
                        shared.listener.restart_needs_install(&items);
                        let text = shared.with(|i| {
                            (i.run, i.job) = (Run::Idle, None);
                            i.wanted = None;
                            i.step = Some(format!("{} Waiting for the user's yes, which is for this start only.", wire::needs_text(&items, &i.name)));
                            let text = format!("{}: starting the runtime needs {}, and installing wasn't allowed.", i.name, wire::items_text(&items));
                            i.state = State::NeedsInstall(InstallInfo { items, helper: None });
                            text
                        });
                        shared.trouble(text);
                    }
                    Err(StartError::NotRunning) => {
                        let restart = shared.with(|i| {
                            i.run = Run::Idle;
                            if i.wanted.as_ref().is_some_and(|wish| !wish.attach) {
                                return true;
                            }
                            (i.wanted, i.job) = (None, None);
                            i.state = State::NothingRunning;
                            i.step = Some(format!("No runtime is running on {}", i.name));
                            false
                        });
                        if restart {
                            shared.begin_start();
                        }
                    }
                    Err(StartError::Failed(message)) => {
                        let name = shared.with(|i| {
                            i.run = Run::Idle;
                            i.wanted = None;
                            i.job = None;
                            i.state = State::Failed(message.clone());
                            i.name.clone()
                        });
                        shared.trouble(format!("{name}: starting the runtime failed: {message}"));
                        shared.listener.restart_failed();
                    }
                }
            }
            Msg::Notice(g, notice) if g == conn => match notice {
                Notice::Died(reason) => {
                    let text = shared.with(|i| {
                        let text = format!("Julia on {} stopped. {reason}", i.name).trim_end().to_owned();
                        gone(i, text.clone(), Gone::Died(reason.trim().to_owned()));
                        text
                    });
                    shared.trouble(text);
                    shared.listener.disconnected();
                }
                Notice::Lost(message) => {
                    let name = shared.inner().name.clone();
                    shared.trouble(format!("{name}: the helper said: {message}"));
                }
            },
            _ => {}
        }
    }
}
